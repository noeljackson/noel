package anthropic

import (
	"strings"

	"github.com/noeljackson/pi/internal/agent"
)

var claudeCodeToolNames = map[string]string{
	"read":              "Read",
	"write":             "Write",
	"edit":              "Edit",
	"bash":              "Bash",
	"grep":              "Grep",
	"glob":              "Glob",
	"find":              "Glob",
	"ask_user_question": "AskUserQuestion",
	"enter_plan_mode":   "EnterPlanMode",
	"exit_plan_mode":    "ExitPlanMode",
	"kill_shell":        "KillShell",
	"notebook_edit":     "NotebookEdit",
	"skill":             "Skill",
	"task":              "Task",
	"task_output":       "TaskOutput",
	"todo_write":        "TodoWrite",
	"web_fetch":         "WebFetch",
	"web_search":        "WebSearch",
}

var claudeCodeReverseToolNames = reverseToolNameMap(claudeCodeToolNames)

func reverseToolNameMap(input map[string]string) map[string]string {
	out := make(map[string]string, len(input))
	for local, remote := range input {
		if _, exists := out[strings.ToLower(remote)]; !exists {
			out[strings.ToLower(remote)] = local
		}
	}
	return out
}

func toClaudeCodeName(name string) string {
	if mapped, ok := claudeCodeToolNames[strings.ToLower(name)]; ok {
		return mapped
	}
	return name
}

func fromClaudeCodeName(name string, tools []agent.Tool) string {
	lowerName := strings.ToLower(name)
	for _, tool := range tools {
		if strings.ToLower(toClaudeCodeName(tool.Name())) == lowerName || strings.ToLower(tool.Name()) == lowerName {
			return tool.Name()
		}
	}
	if mapped, ok := claudeCodeReverseToolNames[lowerName]; ok {
		return mapped
	}
	return name
}
