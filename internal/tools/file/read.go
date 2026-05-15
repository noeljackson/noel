package file

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"strings"

	"github.com/noeljackson/pi/internal/agent"
	toolcontract "github.com/noeljackson/pi/internal/tools"
)

var readSchema = json.RawMessage(`{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}},"required":["path"],"additionalProperties":false}`)

// ReadTool reads text files and supported images.
type ReadTool struct{}

// NewReadTool returns a read tool.
func NewReadTool() *ReadTool {
	return &ReadTool{}
}

func (ReadTool) Name() string {
	return "read"
}

func (ReadTool) Description() string {
	return "Read a file. Supports text plus jpg, png, gif, and webp images."
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

	path := resolvePath(args.Path, tc.Cwd)
	info, err := os.Stat(path)
	if err != nil {
		return agent.ToolResult{}, err
	}
	if info.IsDir() {
		return agent.ToolResult{}, fmt.Errorf("not a file: %s", path)
	}

	data, err := os.ReadFile(path)
	if err != nil {
		return agent.ToolResult{}, err
	}
	if err := ctx.Err(); err != nil {
		return agent.ToolResult{}, err
	}
	if mime := detectImageMime(path, data); mime != "" {
		if !modelSupportsImages(tc.Model) {
			return textResult(tc.CallID, fmt.Sprintf("[image: %s, %d bytes — current model doesn't accept images]", path, len(data)), toolcontract.ReadDetails{
				Path:  path,
				Bytes: len(data),
			}, false)
		}
		imageData, mediaType, _ := resizeImageIfNeeded(data, mime)
		payload := imageContent(imageData, mediaType)
		rawDetails, err := toolcontract.MarshalDetails(toolcontract.ReadDetails{
			Path:  path,
			Bytes: len(data),
		})
		if err != nil {
			return agent.ToolResult{}, err
		}
		return agent.ToolResult{
			ToolUseID: tc.CallID,
			Content: []agent.Content{agent.ImageContent{Source: agent.ImageSource{
				Type:      "base64",
				MediaType: payload.MediaType,
				Data:      payload.Data,
			}}},
			Details: rawDetails,
		}, nil
	}

	limit := args.Limit
	offset := args.Offset
	if offset <= 0 {
		offset = 1
	}

	text := strings.ReplaceAll(string(data), "\r\n", "\n")
	text = strings.ReplaceAll(text, "\r", "\n")
	lines := strings.Split(text, "\n")
	if offset > len(lines) {
		return agent.ToolResult{}, fmt.Errorf("Offset %d is beyond end of file (%d lines total)", args.Offset, len(lines))
	}
	start := offset - 1
	selected := lines[start:]
	userLimitedLines := 0
	if limit > 0 {
		end := start + limit
		if end > len(lines) {
			end = len(lines)
		}
		selected = lines[start:end]
		userLimitedLines = end - start
	}

	truncation := truncateHead(strings.Join(selected, "\n"), defaultMaxLines, defaultMaxBytes)
	startLineDisplay := start + 1
	output := truncation.Content
	truncated := truncation.Truncated
	if truncation.FirstLineExceedsLimit {
		firstLineSize := formatSize(len([]byte(lines[start])))
		output = fmt.Sprintf("[Line %d is %s, exceeds %s limit. Use bash: sed -n '%dp' %s | head -c %d]", startLineDisplay, firstLineSize, formatSize(defaultMaxBytes), startLineDisplay, args.Path, defaultMaxBytes)
	} else if truncation.Truncated {
		endLineDisplay := startLineDisplay + truncation.OutputLines - 1
		nextOffset := endLineDisplay + 1
		if truncation.TruncatedBy == "lines" {
			output += fmt.Sprintf("\n\n[Showing lines %d-%d of %d. Use offset=%d to continue.]", startLineDisplay, endLineDisplay, len(lines), nextOffset)
		} else {
			output += fmt.Sprintf("\n\n[Showing lines %d-%d of %d (%s limit). Use offset=%d to continue.]", startLineDisplay, endLineDisplay, len(lines), formatSize(defaultMaxBytes), nextOffset)
		}
	} else if userLimitedLines > 0 && start+userLimitedLines < len(lines) {
		remaining := len(lines) - (start + userLimitedLines)
		nextOffset := start + userLimitedLines + 1
		output += fmt.Sprintf("\n\n[%d more lines in file. Use offset=%d to continue.]", remaining, nextOffset)
	}
	return textResult(tc.CallID, output, toolcontract.ReadDetails{
		Path:      path,
		Lines:     truncation.OutputLines,
		Bytes:     len(data),
		Truncated: truncated,
		StartLine: offset,
	}, false)
}

func modelSupportsImages(model string) bool {
	return !strings.Contains(strings.ToLower(model), "text-only")
}

var _ agent.Tool = (*ReadTool)(nil)
