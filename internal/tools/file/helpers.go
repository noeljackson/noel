package file

import (
	"fmt"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/noeljackson/pi/internal/agent"
	toolcontract "github.com/noeljackson/pi/internal/tools"
)

const maxContentBytes = 30 * 1024
const defaultMaxLines = 2000
const defaultMaxBytes = 50 * 1024

type truncationResult struct {
	Content               string
	Truncated             bool
	TruncatedBy           string
	TotalLines            int
	TotalBytes            int
	OutputLines           int
	OutputBytes           int
	FirstLineExceedsLimit bool
}

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

func truncateHead(text string, maxLines int, maxBytes int) truncationResult {
	if maxLines <= 0 {
		maxLines = defaultMaxLines
	}
	if maxBytes <= 0 {
		maxBytes = defaultMaxBytes
	}
	totalBytes := len([]byte(text))
	lines := strings.Split(text, "\n")
	totalLines := len(lines)
	if totalLines <= maxLines && totalBytes <= maxBytes {
		return truncationResult{
			Content:     text,
			TotalLines:  totalLines,
			TotalBytes:  totalBytes,
			OutputLines: totalLines,
			OutputBytes: totalBytes,
		}
	}
	if len([]byte(lines[0])) > maxBytes {
		return truncationResult{
			Truncated:             true,
			TruncatedBy:           "bytes",
			TotalLines:            totalLines,
			TotalBytes:            totalBytes,
			FirstLineExceedsLimit: true,
		}
	}

	output := make([]string, 0, minInt(len(lines), maxLines))
	outputBytes := 0
	truncatedBy := "lines"
	for i := 0; i < len(lines) && i < maxLines; i++ {
		lineBytes := len([]byte(lines[i]))
		if i > 0 {
			lineBytes++
		}
		if outputBytes+lineBytes > maxBytes {
			truncatedBy = "bytes"
			break
		}
		output = append(output, lines[i])
		outputBytes += lineBytes
	}
	if len(output) >= maxLines && outputBytes <= maxBytes {
		truncatedBy = "lines"
	}
	content := strings.Join(output, "\n")
	return truncationResult{
		Content:     content,
		Truncated:   true,
		TruncatedBy: truncatedBy,
		TotalLines:  totalLines,
		TotalBytes:  totalBytes,
		OutputLines: len(output),
		OutputBytes: len([]byte(content)),
	}
}

func formatSize(bytes int) string {
	if bytes < 1024 {
		return strconv.Itoa(bytes) + "B"
	}
	if bytes < 1024*1024 {
		return fmt.Sprintf("%.1fKB", float64(bytes)/1024)
	}
	return fmt.Sprintf("%.1fMB", float64(bytes)/(1024*1024))
}

func minInt(a int, b int) int {
	if a < b {
		return a
	}
	return b
}
