package file

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/noeljackson/pi/internal/agent"
	toolcontract "github.com/noeljackson/pi/internal/tools"
)

var lsSchema = json.RawMessage(`{"type":"object","properties":{"path":{"type":"string"},"ignore":{"type":"array","items":{"type":"string"}},"limit":{"type":"integer"}},"additionalProperties":false}`)

// LsTool lists directory entries.
type LsTool struct{}

// NewLsTool returns an ls tool.
func NewLsTool() *LsTool {
	return &LsTool{}
}

func (LsTool) Name() string {
	return "ls"
}

func (LsTool) Description() string {
	return "List directory entries sorted with directories first."
}

func (LsTool) Schema() json.RawMessage {
	return lsSchema
}

func (LsTool) ParallelSafe() bool {
	return true
}

func (LsTool) Execute(ctx context.Context, input json.RawMessage, tc agent.ToolCallContext) (agent.ToolResult, error) {
	var args struct {
		Path   string   `json:"path"`
		Ignore []string `json:"ignore"`
		Limit  int      `json:"limit"`
	}
	if err := json.Unmarshal(input, &args); err != nil {
		return agent.ToolResult{}, err
	}
	path := args.Path
	if path == "" {
		path = "."
	}
	dir := resolvePath(path, tc.Cwd)
	entries, err := os.ReadDir(dir)
	if err != nil {
		return agent.ToolResult{}, err
	}
	if err := ctx.Err(); err != nil {
		return agent.ToolResult{}, err
	}

	type listedEntry struct {
		name  string
		isDir bool
	}
	listed := make([]listedEntry, 0, len(entries))
	for _, entry := range entries {
		if ignoredEntry(entry.Name(), args.Ignore) {
			continue
		}
		listed = append(listed, listedEntry{name: entry.Name(), isDir: entry.IsDir()})
	}
	sort.Slice(listed, func(i int, j int) bool {
		if listed[i].isDir != listed[j].isDir {
			return listed[i].isDir
		}
		return strings.ToLower(listed[i].name) < strings.ToLower(listed[j].name)
	})

	limit := args.Limit
	if limit <= 0 {
		limit = 1000
	}
	if len(listed) > limit {
		listed = listed[:limit]
	}
	lines := make([]string, 0, len(listed))
	for _, entry := range listed {
		name := entry.name
		if entry.isDir {
			name += "/"
		}
		lines = append(lines, name)
	}
	if len(lines) == 0 {
		lines = append(lines, "(empty directory)")
	}
	details := toolcontract.LsDetails{Path: dir, Entries: len(lines), Limit: limit}
	return textResult(tc.CallID, strings.Join(lines, "\n"), details, false)
}

func ignoredEntry(name string, patterns []string) bool {
	for _, pattern := range patterns {
		if pattern == "" {
			continue
		}
		if ok, err := filepath.Match(pattern, name); err == nil && ok {
			return true
		}
		if pattern == name {
			return true
		}
	}
	return false
}

var _ agent.Tool = (*LsTool)(nil)
