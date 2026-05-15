package file

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"

	"github.com/noeljackson/pi/internal/agent"
	toolcontract "github.com/noeljackson/pi/internal/tools"
)

var findSchema = json.RawMessage(`{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"type":{"type":"string","enum":["f","d"]},"limit":{"type":"integer"}},"required":["pattern"],"additionalProperties":false}`)

// FindTool finds files and directories by name.
type FindTool struct{}

// NewFindTool returns a find tool.
func NewFindTool() *FindTool {
	return &FindTool{}
}

func (FindTool) Name() string {
	return "find"
}

func (FindTool) Description() string {
	return "Find files or directories by pattern."
}

func (FindTool) Schema() json.RawMessage {
	return findSchema
}

func (FindTool) ParallelSafe() bool {
	return true
}

func (FindTool) Execute(ctx context.Context, input json.RawMessage, tc agent.ToolCallContext) (agent.ToolResult, error) {
	var args struct {
		Pattern string `json:"pattern"`
		Path    string `json:"path"`
		Type    string `json:"type"`
		Limit   int    `json:"limit"`
	}
	if err := json.Unmarshal(input, &args); err != nil {
		return agent.ToolResult{}, err
	}
	if args.Pattern == "" {
		return agent.ToolResult{}, fmt.Errorf("pattern is required")
	}
	if args.Type != "" && args.Type != "f" && args.Type != "d" {
		return agent.ToolResult{}, fmt.Errorf("type must be f or d")
	}
	limit := args.Limit
	if limit <= 0 {
		limit = 1000
	}
	rootInput := args.Path
	if rootInput == "" {
		rootInput = "."
	}
	root := resolvePath(rootInput, tc.Cwd)

	results, err := runFind(ctx, root, args.Pattern, args.Type)
	if err != nil {
		return agent.ToolResult{}, err
	}
	if len(results) == 0 {
		return textResult(tc.CallID, "No files found matching pattern", toolcontract.FindDetails{
			Pattern: args.Pattern,
			Hits:    0,
			Limit:   limit,
		}, false)
	}
	sort.Strings(results)
	return textResult(tc.CallID, strings.Join(results, "\n"), toolcontract.FindDetails{
		Pattern: args.Pattern,
		Hits:    len(results),
		Limit:   limit,
	}, false)
}

func runFind(ctx context.Context, root string, pattern string, entryType string) ([]string, error) {
	fdPath, err := exec.LookPath("fd")
	if err == nil {
		args := []string{"--color=never", "--hidden"}
		if entryType != "" {
			args = append(args, "--type", entryType)
		}
		args = append(args, "--", pattern, root)
		cmd := exec.CommandContext(ctx, fdPath, args...)
		var stdout bytes.Buffer
		var stderr bytes.Buffer
		cmd.Stdout = &stdout
		cmd.Stderr = &stderr
		if err := cmd.Run(); err != nil {
			if ctx.Err() != nil {
				return nil, ctx.Err()
			}
			if exitErr, ok := err.(*exec.ExitError); !ok || exitErr.ExitCode() != 1 {
				message := strings.TrimSpace(stderr.String())
				if message == "" {
					message = err.Error()
				}
				return nil, fmt.Errorf("fd failed: %s", message)
			}
		}
		return parseFindOutput(root, stdout.Bytes()), nil
	}
	return walkFind(ctx, root, pattern, entryType)
}

func parseFindOutput(root string, output []byte) []string {
	scanner := bufio.NewScanner(bytes.NewReader(output))
	results := []string{}
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if line == "" {
			continue
		}
		line = strings.TrimSuffix(line, string(filepath.Separator))
		if rel, err := filepath.Rel(root, line); err == nil && rel != "." && !strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
			line = rel
		}
		results = append(results, filepath.ToSlash(line))
	}
	return results
}

func walkFind(ctx context.Context, root string, pattern string, entryType string) ([]string, error) {
	results := []string{}
	err := filepath.WalkDir(root, func(path string, entry os.DirEntry, err error) error {
		if err != nil {
			return nil
		}
		if err := ctx.Err(); err != nil {
			return err
		}
		if path == root {
			return nil
		}
		if entry.Name() == ".git" && entry.IsDir() {
			return filepath.SkipDir
		}
		if entryType == "f" && entry.IsDir() {
			return nil
		}
		if entryType == "d" && !entry.IsDir() {
			return nil
		}
		if !nameMatches(pattern, entry.Name()) {
			return nil
		}
		rel, err := filepath.Rel(root, path)
		if err != nil {
			return nil
		}
		if entry.IsDir() {
			rel += string(filepath.Separator)
		}
		results = append(results, filepath.ToSlash(rel))
		return nil
	})
	return results, err
}

func nameMatches(pattern string, name string) bool {
	if ok, err := filepath.Match(pattern, name); err == nil && ok {
		return true
	}
	return strings.Contains(name, pattern)
}

var _ agent.Tool = (*FindTool)(nil)
