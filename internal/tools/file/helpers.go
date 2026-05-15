package file

import (
	"encoding/json"
	"path/filepath"

	"github.com/noeljackson/pi/internal/agent"
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

func textResult(callID string, text string, details map[string]interface{}, isError bool) (agent.ToolResult, error) {
	var rawDetails json.RawMessage
	if details != nil {
		encoded, err := json.Marshal(details)
		if err != nil {
			return agent.ToolResult{}, err
		}
		rawDetails = encoded
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
