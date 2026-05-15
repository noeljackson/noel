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

var editSchema = json.RawMessage(`{"type":"object","properties":{"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["path","old_string","new_string"],"additionalProperties":false}`)

// EditTool edits a file using exact string replacement.
type EditTool struct{}

// NewEditTool returns an edit tool.
func NewEditTool() *EditTool {
	return &EditTool{}
}

func (EditTool) Name() string {
	return "edit"
}

func (EditTool) Description() string {
	return "Edit a file by replacing exact text. The old string must be unique unless replace_all is true."
}

func (EditTool) Schema() json.RawMessage {
	return editSchema
}

func (EditTool) ParallelSafe() bool {
	return false
}

func (EditTool) Execute(ctx context.Context, input json.RawMessage, tc agent.ToolCallContext) (agent.ToolResult, error) {
	var args struct {
		Path       string `json:"path"`
		OldString  string `json:"old_string"`
		NewString  string `json:"new_string"`
		ReplaceAll bool   `json:"replace_all"`
	}
	if err := json.Unmarshal(input, &args); err != nil {
		return agent.ToolResult{}, err
	}
	if args.Path == "" {
		return agent.ToolResult{}, fmt.Errorf("path is required")
	}
	if args.OldString == "" {
		return agent.ToolResult{}, fmt.Errorf("old_string must not be empty")
	}

	path := resolvePath(args.Path, tc.Cwd)
	var result agent.ToolResult
	err := WithLock(path, func() error {
		if err := ctx.Err(); err != nil {
			return err
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		source := string(data)
		bom, text := stripBOM(source)
		ending := detectLineEnding(text)
		normalized := normalizeLineEndings(text)
		oldString := normalizeLineEndings(args.OldString)
		newString := normalizeLineEndings(args.NewString)

		count := strings.Count(normalized, oldString)
		if count == 0 {
			return fmt.Errorf("old_string was not found in %s", args.Path)
		}
		if !args.ReplaceAll && count > 1 {
			return fmt.Errorf("old_string appears %d times in %s; set replace_all to true or provide more context", count, args.Path)
		}

		replaceCount := 1
		editsApplied := 1
		if args.ReplaceAll {
			replaceCount = -1
			editsApplied = count
		}
		edited := strings.Replace(normalized, oldString, newString, replaceCount)
		if edited == normalized {
			return fmt.Errorf("replacement produced no changes in %s", args.Path)
		}
		finalText := bom + restoreLineEndings(edited, ending)
		if err := atomicWrite(path, []byte(finalText), 0o666); err != nil {
			return err
		}

		diff := summarizeReplacement(oldString, newString, count, args.ReplaceAll)
		details := toolcontract.EditDetails{
			Path:         path,
			EditsApplied: editsApplied,
			BeforeLines:  lineCount(normalized),
			AfterLines:   lineCount(edited),
		}
		result, err = textResult(tc.CallID, fmt.Sprintf("replaced %d occurrence(s) in %s\n%s", count, args.Path, diff), details, false)
		return err
	})
	if err != nil {
		return agent.ToolResult{}, err
	}
	return result, nil
}

func stripBOM(text string) (string, string) {
	if strings.HasPrefix(text, "\uFEFF") {
		return "\uFEFF", strings.TrimPrefix(text, "\uFEFF")
	}
	return "", text
}

func detectLineEnding(text string) string {
	crlf := strings.Index(text, "\r\n")
	lf := strings.Index(text, "\n")
	if lf == -1 {
		return "\n"
	}
	if crlf != -1 && crlf == lf-1 {
		return "\r\n"
	}
	return "\n"
}

func normalizeLineEndings(text string) string {
	text = strings.ReplaceAll(text, "\r\n", "\n")
	return strings.ReplaceAll(text, "\r", "\n")
}

func restoreLineEndings(text string, ending string) string {
	if ending == "\r\n" {
		return strings.ReplaceAll(text, "\n", "\r\n")
	}
	return text
}

func summarizeReplacement(oldString string, newString string, count int, replaceAll bool) string {
	if !replaceAll {
		count = 1
	}
	oldSummary := truncateText(oldString, 1024)
	newSummary := truncateText(newString, 1024)
	return fmt.Sprintf("--- old (%d occurrence(s))\n%s\n+++ new\n%s", count, oldSummary, newSummary)
}

var _ agent.Tool = (*EditTool)(nil)
