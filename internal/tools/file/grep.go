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
)

var grepSchema = json.RawMessage(`{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"glob":{"type":"string"},"type":{"type":"string"},"output_mode":{"type":"string","enum":["content","files_with_matches","count"]},"head_limit":{"type":"integer"}},"required":["pattern"],"additionalProperties":false}`)

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
	if args.HeadLimit <= 0 {
		args.HeadLimit = 100
	}
	rootInput := args.Path
	if rootInput == "" {
		rootInput = "."
	}
	root := resolvePath(rootInput, tc.Cwd)

	output, err := runGrep(ctx, root, grepOptions{
		pattern:    args.Pattern,
		glob:       args.Glob,
		fileType:   args.Type,
		outputMode: args.OutputMode,
		headLimit:  args.HeadLimit,
	})
	if err != nil {
		return agent.ToolResult{}, err
	}
	if output == "" {
		output = "No matches found"
	}
	return textResult(tc.CallID, truncateText(output, maxContentBytes), map[string]interface{}{
		"path":        root,
		"output_mode": args.OutputMode,
	}, false)
}

type grepOptions struct {
	pattern    string
	glob       string
	fileType   string
	outputMode string
	headLimit  int
}

func runGrep(ctx context.Context, root string, options grepOptions) (string, error) {
	rgPath, err := exec.LookPath("rg")
	if err == nil {
		return runRipgrep(ctx, rgPath, root, options)
	}
	return walkGrep(ctx, root, options)
}

func runRipgrep(ctx context.Context, rgPath string, root string, options grepOptions) (string, error) {
	args := []string{"--color=never", "--line-number"}
	if options.glob != "" {
		args = append(args, "--glob", options.glob)
	}
	if options.fileType != "" {
		args = append(args, "--type", options.fileType)
	}
	switch options.outputMode {
	case "files_with_matches":
		args = append(args, "--files-with-matches")
	case "count":
		args = append(args, "--count")
	}
	args = append(args, "--", options.pattern, root)

	cmd := exec.CommandContext(ctx, rgPath, args...)
	var stdout bytes.Buffer
	var stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		if ctx.Err() != nil {
			return "", ctx.Err()
		}
		if exitErr, ok := err.(*exec.ExitError); !ok || exitErr.ExitCode() != 1 {
			message := strings.TrimSpace(stderr.String())
			if message == "" {
				message = err.Error()
			}
			return "", fmt.Errorf("rg failed: %s", message)
		}
	}
	return limitLines(relativizeGrepOutput(root, stdout.String(), options.outputMode), options.headLimit), nil
}

func relativizeGrepOutput(root string, output string, outputMode string) string {
	scanner := bufio.NewScanner(strings.NewReader(output))
	lines := []string{}
	for scanner.Scan() {
		line := strings.TrimRight(scanner.Text(), "\r")
		if line == "" {
			continue
		}
		if outputMode == "content" || outputMode == "count" {
			index := strings.Index(line, ":")
			if index > 0 {
				prefix := line[:index]
				if rel, ok := relativeUnder(root, prefix); ok {
					line = filepath.ToSlash(rel) + line[index:]
				}
			}
		} else if rel, ok := relativeUnder(root, line); ok {
			line = filepath.ToSlash(rel)
		}
		lines = append(lines, line)
	}
	return strings.Join(lines, "\n")
}

func walkGrep(ctx context.Context, root string, options grepOptions) (string, error) {
	pattern, err := regexp.Compile(options.pattern)
	if err != nil {
		return "", err
	}
	matchesByFile := map[string]int{}
	lines := []string{}
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
			if !pattern.MatchString(line) {
				continue
			}
			matchesByFile[displayPath]++
			if options.outputMode == "content" {
				lines = append(lines, fmt.Sprintf("%s:%d:%s", displayPath, lineNumber, line))
				if len(lines) >= options.headLimit {
					return filepath.SkipAll
				}
			}
		}
		return nil
	})
	if err != nil {
		return "", err
	}

	switch options.outputMode {
	case "content":
		return strings.Join(lines, "\n"), nil
	case "files_with_matches":
		files := make([]string, 0, len(matchesByFile))
		for file := range matchesByFile {
			files = append(files, file)
		}
		sort.Strings(files)
		return limitLines(strings.Join(files, "\n"), options.headLimit), nil
	case "count":
		files := make([]string, 0, len(matchesByFile))
		for file := range matchesByFile {
			files = append(files, file)
		}
		sort.Strings(files)
		counts := make([]string, 0, len(files))
		for _, file := range files {
			counts = append(counts, fmt.Sprintf("%s:%d", file, matchesByFile[file]))
		}
		return limitLines(strings.Join(counts, "\n"), options.headLimit), nil
	default:
		return "", fmt.Errorf("unsupported output mode: %s", options.outputMode)
	}
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

func limitLines(text string, limit int) string {
	if limit <= 0 || text == "" {
		return text
	}
	lines := strings.Split(strings.TrimRight(text, "\n"), "\n")
	if len(lines) <= limit {
		return strings.Join(lines, "\n")
	}
	return strings.Join(lines[:limit], "\n")
}

var _ agent.Tool = (*GrepTool)(nil)
