package file

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strings"

	"github.com/noeljackson/pi/internal/agent"
	toolcontract "github.com/noeljackson/pi/internal/tools"
)

var grepSchema = json.RawMessage(`{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"glob":{"type":"string"},"type":{"type":"string"},"output_mode":{"type":"string","enum":["content","files_with_matches","count"]},"head_limit":{"type":"integer"},"ignoreCase":{"type":"boolean"},"literal":{"type":"boolean"},"context":{"type":"integer"},"limit":{"type":"integer"}},"required":["pattern"],"additionalProperties":false}`)

// GrepTool searches file contents.
type GrepTool struct{}

// NewGrepTool returns a grep tool.
func NewGrepTool() *GrepTool {
	return &GrepTool{}
}

func (GrepTool) Name() string {
	return "grep"
}

func (GrepTool) Description() string {
	return "Search file contents using ripgrep when available. The Go fallback skips .git but does not parse gitignore files."
}

func (GrepTool) Schema() json.RawMessage {
	return grepSchema
}

func (GrepTool) ParallelSafe() bool {
	return true
}

func (GrepTool) Execute(ctx context.Context, input json.RawMessage, tc agent.ToolCallContext) (agent.ToolResult, error) {
	var args struct {
		Pattern    string `json:"pattern"`
		Path       string `json:"path"`
		Glob       string `json:"glob"`
		Type       string `json:"type"`
		OutputMode string `json:"output_mode"`
		HeadLimit  int    `json:"head_limit"`
		IgnoreCase bool   `json:"ignoreCase"`
		Literal    bool   `json:"literal"`
		Context    int    `json:"context"`
		Limit      int    `json:"limit"`
	}
	if err := json.Unmarshal(input, &args); err != nil {
		return agent.ToolResult{}, err
	}
	if args.Pattern == "" {
		return agent.ToolResult{}, fmt.Errorf("pattern is required")
	}
	if args.OutputMode == "" {
		args.OutputMode = "content"
	}
	if args.OutputMode != "content" && args.OutputMode != "files_with_matches" && args.OutputMode != "count" {
		return agent.ToolResult{}, fmt.Errorf("output_mode must be content, files_with_matches, or count")
	}
	limit := args.Limit
	if limit <= 0 {
		limit = args.HeadLimit
	}
	if limit <= 0 {
		limit = 100
	}
	rootInput := args.Path
	if rootInput == "" {
		rootInput = "."
	}
	root := resolvePath(rootInput, tc.Cwd)

	run, err := runGrep(ctx, root, grepOptions{
		pattern:    args.Pattern,
		glob:       args.Glob,
		fileType:   args.Type,
		outputMode: args.OutputMode,
		limit:      limit,
		ignoreCase: args.IgnoreCase,
		literal:    args.Literal,
		context:    args.Context,
	})
	if err != nil {
		return agent.ToolResult{}, err
	}
	output := run.output
	if output == "" {
		output = "No matches found"
	}
	truncatedOutput := truncateText(output, maxContentBytes)
	truncated := run.truncated || truncatedOutput != output
	return textResult(tc.CallID, truncatedOutput, toolcontract.GrepDetails{
		Pattern:    args.Pattern,
		Files:      run.files,
		Matches:    run.matches,
		Truncated:  truncated,
		OutputMode: args.OutputMode,
	}, false)
}

type grepOptions struct {
	pattern    string
	glob       string
	fileType   string
	outputMode string
	limit      int
	ignoreCase bool
	literal    bool
	context    int
}

type grepRunResult struct {
	output    string
	files     []string
	matches   int
	truncated bool
}

func runGrep(ctx context.Context, root string, options grepOptions) (grepRunResult, error) {
	rgPath, err := exec.LookPath("rg")
	if err == nil {
		return runRipgrep(ctx, rgPath, root, options)
	}
	return walkGrep(ctx, root, options)
}

func runRipgrep(ctx context.Context, rgPath string, root string, options grepOptions) (grepRunResult, error) {
	args := []string{"--json", "--line-number", "--color=never", "--hidden"}
	if options.glob != "" {
		args = append(args, "--glob", options.glob)
	}
	if options.fileType != "" {
		args = append(args, "--type", options.fileType)
	}
	if options.ignoreCase {
		args = append(args, "--ignore-case")
	}
	if options.literal {
		args = append(args, "--fixed-strings")
	}
	args = append(args, "--", options.pattern, root)

	cmd := exec.CommandContext(ctx, rgPath, args...)
	var stdout bytes.Buffer
	var stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		if ctx.Err() != nil {
			return grepRunResult{}, ctx.Err()
		}
		if exitErr, ok := err.(*exec.ExitError); !ok || exitErr.ExitCode() != 1 {
			message := strings.TrimSpace(stderr.String())
			if message == "" {
				message = err.Error()
			}
			return grepRunResult{}, fmt.Errorf("rg failed: %s", message)
		}
	}
	return formatRipgrepJSON(root, stdout.String(), options)
}

func formatRipgrepJSON(root string, output string, options grepOptions) (grepRunResult, error) {
	scanner := bufio.NewScanner(strings.NewReader(output))
	matches := []grepMatch{}
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if line == "" {
			continue
		}
		var event struct {
			Type string `json:"type"`
			Data struct {
				Path struct {
					Text string `json:"text"`
				} `json:"path"`
				LineNumber int `json:"line_number"`
				Lines      struct {
					Text string `json:"text"`
				} `json:"lines"`
			} `json:"data"`
		}
		if err := json.Unmarshal([]byte(line), &event); err != nil || event.Type != "match" {
			continue
		}
		matches = append(matches, grepMatch{
			path: event.Data.Path.Text,
			line: event.Data.LineNumber,
			text: strings.TrimSuffix(strings.ReplaceAll(event.Data.Lines.Text, "\r", ""), "\n"),
			rel:  displayGrepPath(root, event.Data.Path.Text),
		})
		if len(matches) >= options.limit {
			break
		}
	}
	return formatGrepMatches(root, matches, options), nil
}

type grepMatch struct {
	path string
	rel  string
	line int
	text string
}

func walkGrep(ctx context.Context, root string, options grepOptions) (grepRunResult, error) {
	patternText := options.pattern
	if options.ignoreCase {
		patternText = "(?i)" + patternText
	}
	pattern, err := regexp.Compile(patternText)
	if err != nil {
		return grepRunResult{}, err
	}
	matches := []grepMatch{}
	err = filepath.WalkDir(root, func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			return nil
		}
		if err := ctx.Err(); err != nil {
			return err
		}
		if entry.IsDir() {
			if entry.Name() == ".git" {
				return filepath.SkipDir
			}
			return nil
		}
		if options.glob != "" && !pathGlobMatches(root, path, options.glob) {
			return nil
		}
		if options.fileType != "" && !typeMatches(path, options.fileType) {
			return nil
		}
		file, err := os.Open(path)
		if err != nil {
			return nil
		}
		defer file.Close()
		rel, ok := relativeUnder(root, path)
		if !ok {
			rel = path
		}
		displayPath := filepath.ToSlash(rel)
		scanner := bufio.NewScanner(file)
		lineNumber := 0
		for scanner.Scan() {
			lineNumber++
			line := scanner.Text()
			found := false
			if options.literal {
				haystack := line
				needle := options.pattern
				if options.ignoreCase {
					haystack = strings.ToLower(haystack)
					needle = strings.ToLower(needle)
				}
				found = strings.Contains(haystack, needle)
			} else {
				found = pattern.MatchString(line)
			}
			if !found {
				continue
			}
			matches = append(matches, grepMatch{path: path, rel: displayPath, line: lineNumber, text: line})
			if len(matches) >= options.limit {
				return filepath.SkipAll
			}
		}
		return nil
	})
	if err != nil {
		return grepRunResult{}, err
	}
	return formatGrepMatches(root, matches, options), nil
}

func relativeUnder(root string, path string) (string, bool) {
	rel, err := filepath.Rel(root, path)
	if err != nil || rel == "." || strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
		return "", false
	}
	return rel, true
}

func pathGlobMatches(root string, path string, pattern string) bool {
	rel, ok := relativeUnder(root, path)
	if !ok {
		rel = path
	}
	rel = filepath.ToSlash(rel)
	pattern = filepath.ToSlash(pattern)
	if ok, err := filepath.Match(pattern, rel); err == nil && ok {
		return true
	}
	if strings.HasPrefix(pattern, "**/") {
		if ok, err := filepath.Match(strings.TrimPrefix(pattern, "**/"), filepath.Base(rel)); err == nil && ok {
			return true
		}
	}
	return false
}

func typeMatches(path string, fileType string) bool {
	ext := strings.TrimPrefix(strings.ToLower(filepath.Ext(path)), ".")
	switch fileType {
	case "":
		return true
	case "go":
		return ext == "go"
	case "js":
		return ext == "js" || ext == "jsx"
	case "ts":
		return ext == "ts" || ext == "tsx"
	case "json":
		return ext == "json"
	case "md":
		return ext == "md" || ext == "markdown"
	default:
		return ext == strings.ToLower(fileType)
	}
}

func formatGrepMatches(root string, matches []grepMatch, options grepOptions) grepRunResult {
	fileSet := map[string]struct{}{}
	counts := map[string]int{}
	for _, match := range matches {
		fileSet[match.rel] = struct{}{}
		counts[match.rel]++
	}
	files := make([]string, 0, len(fileSet))
	for file := range fileSet {
		files = append(files, file)
	}
	sort.Strings(files)

	linesTruncated := false
	outputLines := []string{}
	switch options.outputMode {
	case "files_with_matches":
		outputLines = append(outputLines, files...)
	case "count":
		for _, file := range files {
			outputLines = append(outputLines, fmt.Sprintf("%s:%d", file, counts[file]))
		}
	default:
		for _, match := range matches {
			if options.context <= 0 {
				text, truncated := truncateGrepLine(match.text)
				linesTruncated = linesTruncated || truncated
				outputLines = append(outputLines, fmt.Sprintf("%s:%d: %s", match.rel, match.line, text))
				continue
			}
			block, truncated := grepContextBlock(match, options.context)
			linesTruncated = linesTruncated || truncated
			outputLines = append(outputLines, block...)
		}
	}
	output := strings.Join(outputLines, "\n")
	truncation := truncateHead(output, 1<<30, defaultMaxBytes)
	truncated := len(matches) >= options.limit || truncation.Truncated || linesTruncated
	if truncation.Truncated {
		output = truncation.Content
	}
	notices := []string{}
	if len(matches) >= options.limit && len(matches) > 0 {
		notices = append(notices, fmt.Sprintf("%d matches limit reached. Use limit=%d for more, or refine pattern", options.limit, options.limit*2))
	}
	if truncation.Truncated {
		notices = append(notices, fmt.Sprintf("%s limit reached", formatSize(defaultMaxBytes)))
	}
	if linesTruncated {
		notices = append(notices, "Some lines truncated to 500 chars. Use read tool to see full lines")
	}
	if len(notices) > 0 && output != "" {
		output += "\n\n[" + strings.Join(notices, ". ") + "]"
	}
	return grepRunResult{output: output, files: files, matches: len(matches), truncated: truncated}
}

func displayGrepPath(root string, path string) string {
	if rel, ok := relativeUnder(root, path); ok {
		return filepath.ToSlash(rel)
	}
	return filepath.ToSlash(filepath.Base(path))
}

func grepContextBlock(match grepMatch, contextLines int) ([]string, bool) {
	data, err := os.ReadFile(match.path)
	if err != nil {
		return []string{fmt.Sprintf("%s:%d: (unable to read file)", match.rel, match.line)}, false
	}
	lines := strings.Split(strings.ReplaceAll(strings.ReplaceAll(string(data), "\r\n", "\n"), "\r", "\n"), "\n")
	start := match.line - contextLines
	if start < 1 {
		start = 1
	}
	end := match.line + contextLines
	if end > len(lines) {
		end = len(lines)
	}
	output := make([]string, 0, end-start+1)
	truncatedAny := false
	for current := start; current <= end; current++ {
		text, truncated := truncateGrepLine(lines[current-1])
		truncatedAny = truncatedAny || truncated
		if current == match.line {
			output = append(output, fmt.Sprintf("%s:%d: %s", match.rel, current, text))
		} else {
			output = append(output, fmt.Sprintf("%s-%d- %s", match.rel, current, text))
		}
	}
	return output, truncatedAny
}

func truncateGrepLine(line string) (string, bool) {
	const limit = 500
	if len(line) <= limit {
		return line, false
	}
	return line[:limit] + "... [truncated]", true
}

var _ agent.Tool = (*GrepTool)(nil)
