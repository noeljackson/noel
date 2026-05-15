package file

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/noeljackson/pi/internal/agent"
	toolcontract "github.com/noeljackson/pi/internal/tools"
)

const maxReadBytes = 10 * 1024 * 1024

var readSchema = json.RawMessage(`{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}},"required":["path"],"additionalProperties":false}`)

// ReadTool reads text files with line-numbered output.
type ReadTool struct{}

// NewReadTool returns a read tool.
func NewReadTool() *ReadTool {
	return &ReadTool{}
}

func (ReadTool) Name() string {
	return "read"
}

func (ReadTool) Description() string {
	return "Read a text file with line numbers. Supports offset and limit for line ranges."
}

func (ReadTool) Schema() json.RawMessage {
	return readSchema
}

func (ReadTool) ParallelSafe() bool {
	return true
}

func (ReadTool) Execute(ctx context.Context, input json.RawMessage, tc agent.ToolCallContext) (agent.ToolResult, error) {
	var args struct {
		Path   string `json:"path"`
		Offset int    `json:"offset"`
		Limit  int    `json:"limit"`
	}
	if err := json.Unmarshal(input, &args); err != nil {
		return agent.ToolResult{}, err
	}
	if args.Path == "" {
		return agent.ToolResult{}, fmt.Errorf("path is required")
	}
	if err := ctx.Err(); err != nil {
		return agent.ToolResult{}, err
	}

	if isImagePath(args.Path) {
		return agent.ToolResult{}, fmt.Errorf("image files are not supported by read")
	}

	path := resolvePath(args.Path, tc.Cwd)
	info, err := os.Stat(path)
	if err != nil {
		return agent.ToolResult{}, err
	}
	if info.IsDir() {
		return agent.ToolResult{}, fmt.Errorf("not a file: %s", path)
	}
	if info.Size() > maxReadBytes {
		return agent.ToolResult{}, fmt.Errorf("file is too large: %d bytes exceeds %d bytes", info.Size(), maxReadBytes)
	}

	data, err := os.ReadFile(path)
	if err != nil {
		return agent.ToolResult{}, err
	}
	if err := ctx.Err(); err != nil {
		return agent.ToolResult{}, err
	}

	limit := args.Limit
	if limit <= 0 {
		limit = 2000
	}
	offset := args.Offset
	if offset <= 0 {
		offset = 1
	}

	text := strings.ReplaceAll(string(data), "\r\n", "\n")
	text = strings.ReplaceAll(text, "\r", "\n")
	lines := strings.Split(text, "\n")
	if len(lines) > 0 && lines[len(lines)-1] == "" {
		lines = lines[:len(lines)-1]
	}
	if offset > len(lines) && len(lines) > 0 {
		return agent.ToolResult{}, fmt.Errorf("offset %d is beyond end of file (%d lines total)", offset, len(lines))
	}
	if len(lines) == 0 {
		return textResult(tc.CallID, "", toolcontract.ReadDetails{
			Path:      path,
			Lines:     0,
			Bytes:     len(data),
			Truncated: false,
			StartLine: offset,
		}, false)
	}

	start := offset - 1
	end := start + limit
	if end > len(lines) {
		end = len(lines)
	}

	var builder strings.Builder
	for i := start; i < end; i++ {
		if builder.Len() > 0 {
			builder.WriteByte('\n')
		}
		fmt.Fprintf(&builder, "%6d\t%s", i+1, lines[i])
	}
	return textResult(tc.CallID, builder.String(), toolcontract.ReadDetails{
		Path:      path,
		Lines:     end - start,
		Bytes:     len(data),
		Truncated: end < len(lines),
		StartLine: offset,
	}, false)
}

func isImagePath(path string) bool {
	switch strings.ToLower(filepath.Ext(path)) {
	case ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".tiff", ".tif", ".ico", ".avif":
		return true
	default:
		return false
	}
}

var _ agent.Tool = (*ReadTool)(nil)
