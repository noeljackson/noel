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

var editSchema = json.RawMessage(`{"type":"object","properties":{"path":{"type":"string"},"edits":{"type":"array","items":{"type":"object","properties":{"oldText":{"type":"string"},"newText":{"type":"string"},"replaceAll":{"type":"boolean"}},"required":["oldText","newText"],"additionalProperties":false}},"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean"},"oldText":{"type":"string"},"newText":{"type":"string"},"replaceAll":{"type":"boolean"}},"required":["path"],"additionalProperties":false}`)

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
	args, err := parseEditArgs(input)
	if err != nil {
		return agent.ToolResult{}, err
	}
	if args.Path == "" {
		return agent.ToolResult{}, fmt.Errorf("path is required")
	}
	if len(args.Edits) == 0 {
		return agent.ToolResult{}, fmt.Errorf("Edit tool input is invalid. edits must contain at least one replacement.")
	}

	path := resolvePath(args.Path, tc.Cwd)
	var result agent.ToolResult
	err = WithLock(path, func() error {
		if err := ctx.Err(); err != nil {
			return err
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return fmt.Errorf("Could not edit file: %s. Error code: %s.", args.Path, osErrorCode(err))
		}
		source := string(data)
		bom, text := stripBOM(source)
		ending := detectLineEnding(text)
		normalized := normalizeLineEndings(text)
		edited, matches, err := applyBatchEdits(normalized, args.Edits, args.Path)
		if err != nil {
			return err
		}
		finalText := bom + restoreLineEndings(edited, ending)
		if err := atomicWrite(path, []byte(finalText), 0o666); err != nil {
			return err
		}

		diff := unifiedDiff(normalized, edited, 3)
		details := toolcontract.EditDetails{
			Path:         path,
			EditsApplied: len(matches),
			BeforeLines:  lineCount(normalized),
			AfterLines:   lineCount(edited),
			Hunks: []toolcontract.EditHunk{{
				Method: diffMethod(matches),
				Diff:   diff,
			}},
		}
		result, err = textResult(tc.CallID, fmt.Sprintf("Successfully replaced %d block(s) in %s.", len(args.Edits), args.Path), details, false)
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

type editArgs struct {
	Path  string
	Edits []editInput
}

type editInput struct {
	OldText    string
	NewText    string
	ReplaceAll bool
}

type editMatch struct {
	Index      int
	Start      int
	End        int
	NewText    string
	UsedFuzzy  bool
	ReplaceAll bool
}

func parseEditArgs(input json.RawMessage) (editArgs, error) {
	var raw struct {
		Path            string          `json:"path"`
		Edits           json.RawMessage `json:"edits"`
		OldString       *string         `json:"old_string"`
		NewString       *string         `json:"new_string"`
		ReplaceAll      bool            `json:"replace_all"`
		OldText         *string         `json:"oldText"`
		NewText         *string         `json:"newText"`
		ReplaceAllCamel bool            `json:"replaceAll"`
	}
	if err := json.Unmarshal(input, &raw); err != nil {
		return editArgs{}, err
	}
	args := editArgs{Path: raw.Path}
	if len(raw.Edits) > 0 && string(raw.Edits) != "null" {
		if len(raw.Edits) > 0 && raw.Edits[0] == '"' {
			var encoded string
			if err := json.Unmarshal(raw.Edits, &encoded); err == nil {
				raw.Edits = json.RawMessage(encoded)
			}
		}
		if err := json.Unmarshal(raw.Edits, &args.Edits); err != nil {
			return editArgs{}, err
		}
	}
	switch {
	case raw.OldString != nil && raw.NewString != nil:
		args.Edits = append(args.Edits, editInput{OldText: *raw.OldString, NewText: *raw.NewString, ReplaceAll: raw.ReplaceAll})
	case raw.OldText != nil && raw.NewText != nil:
		args.Edits = append(args.Edits, editInput{OldText: *raw.OldText, NewText: *raw.NewText, ReplaceAll: raw.ReplaceAllCamel})
	}
	return args, nil
}

func applyBatchEdits(content string, edits []editInput, path string) (string, []editMatch, error) {
	normalizedEdits := make([]editInput, len(edits))
	for i, edit := range edits {
		normalizedEdits[i] = editInput{
			OldText:    normalizeLineEndings(edit.OldText),
			NewText:    normalizeLineEndings(edit.NewText),
			ReplaceAll: edit.ReplaceAll,
		}
		if normalizedEdits[i].OldText == "" {
			if len(edits) == 1 {
				return "", nil, fmt.Errorf("oldText must not be empty in %s.", path)
			}
			return "", nil, fmt.Errorf("edits[%d].oldText must not be empty in %s.", i, path)
		}
	}

	matches := []editMatch{}
	for i, edit := range normalizedEdits {
		exact := findAll(content, edit.OldText)
		if len(exact) > 0 {
			if len(exact) > 1 && !edit.ReplaceAll {
				return "", nil, duplicateEditError(path, i, len(edits), len(exact))
			}
			for _, start := range exact {
				matches = append(matches, editMatch{Index: i, Start: start, End: start + len(edit.OldText), NewText: edit.NewText, ReplaceAll: edit.ReplaceAll})
				if !edit.ReplaceAll {
					break
				}
			}
			continue
		}
		fuzzy := findAllFuzzy(content, edit.OldText)
		if len(fuzzy) == 0 {
			return "", nil, notFoundEditError(path, i, len(edits))
		}
		if len(fuzzy) > 1 && !edit.ReplaceAll {
			return "", nil, duplicateEditError(path, i, len(edits), len(fuzzy))
		}
		for _, match := range fuzzy {
			matches = append(matches, editMatch{Index: i, Start: match[0], End: match[1], NewText: edit.NewText, UsedFuzzy: true, ReplaceAll: edit.ReplaceAll})
			if !edit.ReplaceAll {
				break
			}
		}
	}
	sortMatches(matches)
	for i := 1; i < len(matches); i++ {
		previous := matches[i-1]
		current := matches[i]
		if previous.End > current.Start {
			return "", nil, fmt.Errorf("edits[%d] and edits[%d] overlap in %s. Merge them into one edit or target disjoint regions.", previous.Index, current.Index, path)
		}
	}
	edited := content
	for i := len(matches) - 1; i >= 0; i-- {
		match := matches[i]
		edited = edited[:match.Start] + match.NewText + edited[match.End:]
	}
	if edited == content {
		if len(edits) == 1 {
			return "", nil, fmt.Errorf("No changes made to %s. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected.", path)
		}
		return "", nil, fmt.Errorf("No changes made to %s. The replacements produced identical content.", path)
	}
	return edited, matches, nil
}

func findAll(content string, needle string) []int {
	result := []int{}
	offset := 0
	for {
		index := strings.Index(content[offset:], needle)
		if index == -1 {
			return result
		}
		start := offset + index
		result = append(result, start)
		offset = start + len(needle)
	}
}

func findAllFuzzy(content string, needle string) [][2]int {
	normalizedContent, positions := collapseWhitespaceWithPositions(content)
	normalizedNeedle, _ := collapseWhitespaceWithPositions(needle)
	starts := findAll(normalizedContent, normalizedNeedle)
	result := make([][2]int, 0, len(starts))
	for _, start := range starts {
		if start >= len(positions) {
			continue
		}
		endIndex := start + len(normalizedNeedle) - 1
		if endIndex >= len(positions) {
			continue
		}
		result = append(result, [2]int{positions[start][0], positions[endIndex][1]})
	}
	return result
}

func collapseWhitespaceWithPositions(text string) (string, [][2]int) {
	var builder strings.Builder
	positions := make([][2]int, 0, len(text))
	inWhitespace := false
	whitespaceStart := 0
	for index, r := range text {
		size := len(string(r))
		if isFuzzyWhitespace(r) {
			if !inWhitespace {
				inWhitespace = true
				whitespaceStart = index
				builder.WriteByte(' ')
				positions = append(positions, [2]int{index, index + size})
			} else {
				positions[len(positions)-1][1] = index + size
			}
			continue
		}
		inWhitespace = false
		_ = whitespaceStart
		builder.WriteRune(r)
		positions = append(positions, [2]int{index, index + size})
	}
	return builder.String(), positions
}

func isFuzzyWhitespace(r rune) bool {
	return r == ' ' || r == '\t' || r == '\n' || r == '\r'
}

func sortMatches(matches []editMatch) {
	for i := 1; i < len(matches); i++ {
		for j := i; j > 0 && matches[j-1].Start > matches[j].Start; j-- {
			matches[j-1], matches[j] = matches[j], matches[j-1]
		}
	}
}

func notFoundEditError(path string, editIndex int, totalEdits int) error {
	if totalEdits == 1 {
		return fmt.Errorf("Could not find the exact text in %s. The old text must match exactly including all whitespace and newlines.", path)
	}
	return fmt.Errorf("Could not find edits[%d] in %s. The oldText must match exactly including all whitespace and newlines.", editIndex, path)
}

func duplicateEditError(path string, editIndex int, totalEdits int, occurrences int) error {
	if totalEdits == 1 {
		return fmt.Errorf("Found %d occurrences of the text in %s. The text must be unique. Please provide more context to make it unique.", occurrences, path)
	}
	return fmt.Errorf("Found %d occurrences of edits[%d] in %s. Each oldText must be unique. Please provide more context to make it unique.", occurrences, editIndex, path)
}

func diffMethod(matches []editMatch) string {
	for _, match := range matches {
		if match.UsedFuzzy {
			return "fuzzy"
		}
	}
	return "exact"
}

func osErrorCode(err error) string {
	if os.IsNotExist(err) {
		return "ENOENT"
	}
	if os.IsPermission(err) {
		return "EACCES"
	}
	return "UNKNOWN"
}

func unifiedDiff(oldContent string, newContent string, contextLines int) string {
	oldLines := strings.Split(oldContent, "\n")
	newLines := strings.Split(newContent, "\n")
	start := 0
	for start < len(oldLines) && start < len(newLines) && oldLines[start] == newLines[start] {
		start++
	}
	endOld := len(oldLines) - 1
	endNew := len(newLines) - 1
	for endOld >= start && endNew >= start && oldLines[endOld] == newLines[endNew] {
		endOld--
		endNew--
	}
	hunkStart := start - contextLines
	if hunkStart < 0 {
		hunkStart = 0
	}
	hunkEndOld := endOld + contextLines
	if hunkEndOld >= len(oldLines) {
		hunkEndOld = len(oldLines) - 1
	}
	hunkEndNew := endNew + contextLines
	if hunkEndNew >= len(newLines) {
		hunkEndNew = len(newLines) - 1
	}
	oldCount := hunkEndOld - hunkStart + 1
	newCount := hunkEndNew - hunkStart + 1
	if oldCount < 0 {
		oldCount = 0
	}
	if newCount < 0 {
		newCount = 0
	}
	var builder strings.Builder
	fmt.Fprintf(&builder, "@@ -%d,%d +%d,%d @@\n", hunkStart+1, oldCount, hunkStart+1, newCount)
	for i := hunkStart; i < start && i < len(oldLines) && i < len(newLines); i++ {
		fmt.Fprintf(&builder, " %s\n", oldLines[i])
	}
	for i := start; i <= endOld && i < len(oldLines); i++ {
		fmt.Fprintf(&builder, "-%s\n", oldLines[i])
	}
	for i := start; i <= endNew && i < len(newLines); i++ {
		fmt.Fprintf(&builder, "+%s\n", newLines[i])
	}
	for i := endOld + 1; i <= hunkEndOld && i < len(oldLines); i++ {
		fmt.Fprintf(&builder, " %s\n", oldLines[i])
	}
	return strings.TrimRight(builder.String(), "\n")
}

var _ agent.Tool = (*EditTool)(nil)
