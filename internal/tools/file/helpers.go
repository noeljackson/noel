package file

import (
	"path/filepath"
	"strings"

	"github.com/noeljackson/pi/internal/agent"
	toolcontract "github.com/noeljackson/pi/internal/tools"
)

const maxContentBytes = 30 * 1024

func resolvePath(path string, cwd string) string {
	if filepath.IsAbs(path) {
		return filepath.Clean(path)
	}
	if cwd == "" {
		cwd = "."
	}
	return filepath.Clean(filepath.Join(cwd, path))
}

func textResult(callID string, text string, details interface{}, isError bool) (agent.ToolResult, error) {
	rawDetails, err := toolcontract.MarshalDetails(details)
	if err != nil {
		return agent.ToolResult{}, err
	}
	return agent.ToolResult{
		ToolUseID: callID,
		Content:   []agent.Content{agent.TextContent{Text: text}},
		Details:   rawDetails,
		IsError:   isError,
	}, nil
}

func truncateText(text string, limit int) string {
	if limit <= 0 || len(text) <= limit {
		return text
	}
	const marker = "\n[truncated]\n"
	if limit <= len(marker) {
		return text[:limit]
	}
	start := len(text) - (limit - len(marker))
	return marker + text[start:]
}

func lineCount(text string) int {
	if text == "" {
		return 0
	}
	count := strings.Count(text, "\n") + 1
	if strings.HasSuffix(text, "\n") {
		count--
	}
	return count
}
