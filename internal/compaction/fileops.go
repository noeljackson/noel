package compaction

import (
	"encoding/json"
	"sort"
	"strings"

	"github.com/noeljackson/pi/internal/agent"
	"github.com/noeljackson/pi/internal/tools"
)

// FileOperation describes a file-affecting tool call extracted from a message.
type FileOperation struct {
	Path   string
	Tool   string
	Action string
	Bytes  int
}

// ExtractFileOps walks a message's tool calls and results and returns file
// operations they performed.
func ExtractFileOps(message agent.Message) []FileOperation {
	ops := make([]FileOperation, 0)
	switch msg := message.(type) {
	case agent.AssistantMessage:
		ops = append(ops, extractAssistantFileOps(msg.Content)...)
	case *agent.AssistantMessage:
		if msg != nil {
			ops = append(ops, extractAssistantFileOps(msg.Content)...)
		}
	case agent.ToolResultMessage:
		ops = append(ops, extractToolResultFileOps(msg.Results)...)
	case *agent.ToolResultMessage:
		if msg != nil {
			ops = append(ops, extractToolResultFileOps(msg.Results)...)
		}
	}
	return ops
}

// ComputeFileLists groups file operations into read-only and modified lists.
// When maxFiles is positive, the most recent paths in each group are retained.
func ComputeFileLists(ops []FileOperation, maxFiles int) (readOnly, modified []string) {
	modifiedSet := make(map[string]int)
	readSet := make(map[string]int)
	for i, op := range ops {
		path := strings.TrimSpace(op.Path)
		if path == "" {
			continue
		}
		switch op.Action {
		case "write", "modify":
			modifiedSet[path] = i
		case "read":
			readSet[path] = i
		}
	}
	for path := range modifiedSet {
		delete(readSet, path)
	}
	return cappedSortedPaths(readSet, maxFiles), cappedSortedPaths(modifiedSet, maxFiles)
}

// FormatFileOperations formats file operation context for appending to a
// compaction summary.
func FormatFileOperations(readOnly, modified []string) string {
	sections := make([]string, 0, 2)
	if len(readOnly) > 0 {
		sections = append(sections, "<read-files>\n"+strings.Join(readOnly, "\n")+"\n</read-files>")
	}
	if len(modified) > 0 {
		sections = append(sections, "<modified-files>\n"+strings.Join(modified, "\n")+"\n</modified-files>")
	}
	if len(sections) == 0 {
		return ""
	}
	return "\n\n" + strings.Join(sections, "\n\n")
}

func extractAssistantFileOps(contents []agent.Content) []FileOperation {
	ops := make([]FileOperation, 0)
	for _, content := range contents {
		switch block := content.(type) {
		case agent.ToolUseContent:
			if op, ok := fileOperationFromToolUse(block.Name, block.Input); ok {
				ops = append(ops, op)
			}
		case *agent.ToolUseContent:
			if block != nil {
				if op, ok := fileOperationFromToolUse(block.Name, block.Input); ok {
					ops = append(ops, op)
				}
			}
		}
	}
	return ops
}

func fileOperationFromToolUse(name string, input json.RawMessage) (FileOperation, bool) {
	var args struct {
		Path    string `json:"path"`
		Command string `json:"command"`
	}
	if err := json.Unmarshal(input, &args); err != nil {
		return FileOperation{}, false
	}
	switch name {
	case "read":
		if args.Path == "" {
			return FileOperation{}, false
		}
		return FileOperation{Path: args.Path, Tool: "read", Action: "read"}, true
	case "write":
		if args.Path == "" {
			return FileOperation{}, false
		}
		return FileOperation{Path: args.Path, Tool: "write", Action: "write"}, true
	case "edit":
		if args.Path == "" {
			return FileOperation{}, false
		}
		return FileOperation{Path: args.Path, Tool: "edit", Action: "modify"}, true
	case "bash":
		if args.Command == "" {
			return FileOperation{}, false
		}
		return FileOperation{Path: args.Command, Tool: "bash", Action: "modify"}, true
	default:
		return FileOperation{}, false
	}
}

func extractToolResultFileOps(results []agent.ToolResult) []FileOperation {
	ops := make([]FileOperation, 0, len(results))
	for _, result := range results {
		if len(result.Details) == 0 {
			continue
		}
		if op, ok := editDetailsOperation(result.Details); ok {
			ops = append(ops, op)
			continue
		}
		if op, ok := bashDetailsOperation(result.Details); ok {
			ops = append(ops, op)
			continue
		}
		if op, ok := readDetailsOperation(result.Details); ok {
			ops = append(ops, op)
			continue
		}
		if op, ok := writeDetailsOperation(result.Details); ok {
			ops = append(ops, op)
		}
	}
	return ops
}

func readDetailsOperation(raw json.RawMessage) (FileOperation, bool) {
	if !jsonObjectHasKey(raw, "truncated") && !jsonObjectHasKey(raw, "startLine") {
		return FileOperation{}, false
	}
	var details tools.ReadDetails
	if err := json.Unmarshal(raw, &details); err != nil || details.Path == "" {
		return FileOperation{}, false
	}
	if details.Bytes == 0 && details.Lines == 0 && !details.Truncated && details.StartLine == 0 {
		return FileOperation{}, false
	}
	return FileOperation{Path: details.Path, Tool: "read", Action: "read", Bytes: details.Bytes}, true
}

func writeDetailsOperation(raw json.RawMessage) (FileOperation, bool) {
	if jsonObjectHasKey(raw, "truncated") || jsonObjectHasKey(raw, "startLine") ||
		jsonObjectHasKey(raw, "editsApplied") || jsonObjectHasKey(raw, "command") {
		return FileOperation{}, false
	}
	var details tools.WriteDetails
	if err := json.Unmarshal(raw, &details); err != nil || details.Path == "" {
		return FileOperation{}, false
	}
	if details.Bytes == 0 && details.Lines == 0 {
		return FileOperation{}, false
	}
	return FileOperation{Path: details.Path, Tool: "write", Action: "write", Bytes: details.Bytes}, true
}

func editDetailsOperation(raw json.RawMessage) (FileOperation, bool) {
	var details tools.EditDetails
	if err := json.Unmarshal(raw, &details); err != nil || details.Path == "" {
		return FileOperation{}, false
	}
	if details.EditsApplied == 0 && details.BeforeLines == 0 && details.AfterLines == 0 && len(details.Hunks) == 0 {
		return FileOperation{}, false
	}
	return FileOperation{Path: details.Path, Tool: "edit", Action: "modify"}, true
}

func bashDetailsOperation(raw json.RawMessage) (FileOperation, bool) {
	if !jsonObjectHasKey(raw, "command") {
		return FileOperation{}, false
	}
	var details tools.BashDetails
	if err := json.Unmarshal(raw, &details); err != nil || details.Command == "" {
		return FileOperation{}, false
	}
	return FileOperation{
		Path:   details.Command,
		Tool:   "bash",
		Action: "modify",
		Bytes:  details.StdoutBytes + details.StderrBytes,
	}, true
}

func jsonObjectHasKey(raw json.RawMessage, key string) bool {
	var object map[string]json.RawMessage
	if err := json.Unmarshal(raw, &object); err != nil {
		return false
	}
	_, ok := object[key]
	return ok
}

func cappedSortedPaths(paths map[string]int, maxFiles int) []string {
	type pathSeen struct {
		path string
		seen int
	}
	values := make([]pathSeen, 0, len(paths))
	for path, seen := range paths {
		values = append(values, pathSeen{path: path, seen: seen})
	}
	if maxFiles > 0 && len(values) > maxFiles {
		sort.Slice(values, func(i, j int) bool {
			return values[i].seen > values[j].seen
		})
		values = values[:maxFiles]
	}
	result := make([]string, 0, len(values))
	for _, value := range values {
		result = append(result, value.path)
	}
	sort.Strings(result)
	return result
}
