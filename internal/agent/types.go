// Package agent defines shared agent interfaces and message types.
package agent

import (
	"encoding/json"
	"strconv"
	"strings"
	"time"
)

// Role identifies the sender role for a message.
type Role string

const (
	// RoleUser identifies user-authored messages.
	RoleUser Role = "user"
	// RoleAssistant identifies assistant-authored messages.
	RoleAssistant Role = "assistant"
	// RoleTool identifies tool-result messages.
	RoleTool Role = "tool"
	// RoleSystem identifies system messages.
	RoleSystem Role = "system"
	// RoleBashExecution identifies shell execution transcript messages.
	RoleBashExecution Role = "bashExecution"
	// RoleCustom identifies app-defined messages.
	RoleCustom Role = "custom"
	// RoleBranchSummary identifies branch summary messages.
	RoleBranchSummary Role = "branchSummary"
	// RoleCompactionSummary identifies compaction summary messages.
	RoleCompactionSummary Role = "compactionSummary"
)

// StopReason identifies why a model response stopped.
type StopReason string

const (
	// StopEndTurn indicates the model completed its turn.
	StopEndTurn StopReason = "end_turn"
	// StopMaxTokens indicates generation hit a token limit.
	StopMaxTokens StopReason = "max_tokens"
	// StopToolUse indicates the model requested tool execution.
	StopToolUse StopReason = "tool_use"
	// StopStopSequence indicates generation stopped at a configured sequence.
	StopStopSequence StopReason = "stop_sequence"
	// StopAbort indicates the request was aborted.
	StopAbort StopReason = "abort"
	// StopError indicates generation failed.
	StopError StopReason = "error"
	// StopRefusal indicates the model refused the request.
	StopRefusal StopReason = "refusal"
	// StopPauseTurn indicates the provider paused the turn.
	StopPauseTurn StopReason = "pause_turn"
	// StopOverloaded indicates the provider was overloaded.
	StopOverloaded StopReason = "overloaded"
)

// Content describes a message content block.
type Content interface {
	isContent()
}

// CacheControl describes provider prompt-cache hints.
type CacheControl struct {
	Type string `json:"type"`
}

// TextContent contains plain text.
type TextContent struct {
	Text          string        `json:"text"`
	TextSignature string        `json:"textSignature,omitempty"`
	CacheControl  *CacheControl `json:"cache_control,omitempty"`
}

// ThinkingContent contains model reasoning text and an optional signature.
type ThinkingContent struct {
	Thinking          string `json:"thinking"`
	Signature         string `json:"signature,omitempty"`
	ThinkingSignature string `json:"thinkingSignature,omitempty"`
	Redacted          bool   `json:"redacted,omitempty"`
}

// ImageContent contains image input content.
type ImageContent struct {
	Source       ImageSource   `json:"source"`
	CacheControl *CacheControl `json:"cache_control,omitempty"`
}

// ToolUseContent contains a model-requested tool invocation.
type ToolUseContent struct {
	ID               string          `json:"id"`
	Name             string          `json:"name"`
	Input            json.RawMessage `json:"input"`
	ThoughtSignature string          `json:"thoughtSignature,omitempty"`
}

// ToolResultContent contains the result of a tool invocation.
type ToolResultContent struct {
	ToolUseID string    `json:"tool_use_id"`
	Content   []Content `json:"content,omitempty"`
	IsError   bool      `json:"is_error,omitempty"`
}

// RedactedThinkingContent contains an opaque redacted reasoning payload.
type RedactedThinkingContent struct {
	Data string `json:"data"`
}

// ServerToolUseContent contains a provider-side tool invocation.
type ServerToolUseContent struct {
	ID    string          `json:"id"`
	Name  string          `json:"name"`
	Input json.RawMessage `json:"input"`
}

// ImageSource describes encoded image data.
type ImageSource struct {
	Type      string `json:"type"`
	MediaType string `json:"media_type"`
	Data      string `json:"data"`
}

// Message describes a conversation message.
type Message interface {
	isMessage()

	// Role returns the sender role for the message.
	Role() Role
}

// UserMessage contains user-authored content.
type UserMessage struct {
	Content   []Content
	Timestamp time.Time
}

// AssistantMessage contains assistant-authored content and response metadata.
type AssistantMessage struct {
	Content       []Content
	StopReason    StopReason
	Model         string
	API           string
	Provider      string
	ResponseID    string
	ResponseModel string
	ErrorMessage  string
	Cost          *Cost
	Usage         Usage
	Timestamp     time.Time
}

// ToolResultMessage contains one or more tool results.
type ToolResultMessage struct {
	Results   []ToolResult
	Timestamp time.Time
}

// ToolResult describes a tool execution result.
type ToolResult struct {
	ToolUseID string
	Content   []Content
	Details   json.RawMessage
	IsError   bool
}

// SystemMessage contains system-level instructions.
type SystemMessage struct {
	Content   []Content
	Timestamp time.Time
}

// BashExecutionMessage contains shell execution output injected by the harness.
type BashExecutionMessage struct {
	Timestamp          time.Time
	Command            string
	Output             string
	Stdout             string
	Stderr             string
	ExitCode           *int
	Cancelled          bool
	Truncated          bool
	FullOutputPath     string
	ExcludeFromContext bool
}

// CustomMessage contains app-defined content that can be converted to LLM input.
type CustomMessage struct {
	Timestamp time.Time
	Kind      string
	Content   []Content
	Display   bool
	Details   json.RawMessage
	Metadata  map[string]json.RawMessage
}

// BranchSummaryMessage contains a summary of a branch the session returned from.
type BranchSummaryMessage struct {
	Timestamp    time.Time
	Summary      string
	SourceLeafID string
	Details      json.RawMessage
	FromHook     bool
}

// CompactionSummaryMessage contains a summary of compacted conversation history.
type CompactionSummaryMessage struct {
	Timestamp    time.Time
	Summary      string
	TokensBefore int
	DroppedCount int
	FileOps      json.RawMessage
	Details      json.RawMessage
}

// Usage describes token usage reported by a model provider.
type Usage struct {
	InputTokens              int
	OutputTokens             int
	CacheCreationInputTokens int
	CacheReadInputTokens     int
	TotalTokens              int
	CacheWriteTokens         int
}

// Cost describes provider-reported request cost.
type Cost struct {
	Input      float64 `json:"input"`
	Output     float64 `json:"output"`
	CacheRead  float64 `json:"cacheRead"`
	CacheWrite float64 `json:"cacheWrite"`
	Total      float64 `json:"total"`
}

// ToolCall describes a tool invocation requested by the model.
type ToolCall struct {
	ID    string
	Name  string
	Input json.RawMessage
}

func (TextContent) isContent()             {}
func (ThinkingContent) isContent()         {}
func (ImageContent) isContent()            {}
func (ToolUseContent) isContent()          {}
func (ToolResultContent) isContent()       {}
func (RedactedThinkingContent) isContent() {}
func (ServerToolUseContent) isContent()    {}

func (UserMessage) isMessage()              {}
func (AssistantMessage) isMessage()         {}
func (ToolResultMessage) isMessage()        {}
func (SystemMessage) isMessage()            {}
func (BashExecutionMessage) isMessage()     {}
func (CustomMessage) isMessage()            {}
func (BranchSummaryMessage) isMessage()     {}
func (CompactionSummaryMessage) isMessage() {}

// Role returns the user role.
func (UserMessage) Role() Role {
	return RoleUser
}

// Role returns the assistant role.
func (AssistantMessage) Role() Role {
	return RoleAssistant
}

// Role returns the tool role.
func (ToolResultMessage) Role() Role {
	return RoleTool
}

// Role returns the system role.
func (SystemMessage) Role() Role {
	return RoleSystem
}

// Role returns the bash execution role.
func (BashExecutionMessage) Role() Role {
	return RoleBashExecution
}

// Role returns the custom role.
func (CustomMessage) Role() Role {
	return RoleCustom
}

// Role returns the branch summary role.
func (BranchSummaryMessage) Role() Role {
	return RoleBranchSummary
}

// Role returns the compaction summary role.
func (CompactionSummaryMessage) Role() Role {
	return RoleCompactionSummary
}

// String returns the stop reason string.
func (s StopReason) String() string {
	return string(s)
}

// MessageTimestamp returns a message's timestamp.
func MessageTimestamp(message Message) time.Time {
	switch msg := message.(type) {
	case UserMessage:
		return msg.Timestamp
	case *UserMessage:
		return msg.Timestamp
	case AssistantMessage:
		return msg.Timestamp
	case *AssistantMessage:
		return msg.Timestamp
	case ToolResultMessage:
		return msg.Timestamp
	case *ToolResultMessage:
		return msg.Timestamp
	case SystemMessage:
		return msg.Timestamp
	case *SystemMessage:
		return msg.Timestamp
	case BashExecutionMessage:
		return msg.Timestamp
	case *BashExecutionMessage:
		return msg.Timestamp
	case CustomMessage:
		return msg.Timestamp
	case *CustomMessage:
		return msg.Timestamp
	case BranchSummaryMessage:
		return msg.Timestamp
	case *BranchSummaryMessage:
		return msg.Timestamp
	case CompactionSummaryMessage:
		return msg.Timestamp
	case *CompactionSummaryMessage:
		return msg.Timestamp
	default:
		return time.Time{}
	}
}

// BashExecutionToText converts shell execution output to LLM-visible text.
func BashExecutionToText(msg BashExecutionMessage) string {
	text := "Ran `" + msg.Command + "`\n"
	output := msg.Output
	if output == "" {
		output = strings.TrimRight(strings.Join(nonEmptyStrings(msg.Stdout, msg.Stderr), "\n"), "\n")
	}
	if output != "" {
		text += "```\n" + output + "\n```"
	} else {
		text += "(no output)"
	}
	if msg.Cancelled {
		text += "\n\n(command cancelled)"
	} else if msg.ExitCode != nil && *msg.ExitCode != 0 {
		text += "\n\nCommand exited with code " + strconv.Itoa(*msg.ExitCode)
	}
	if msg.Truncated && msg.FullOutputPath != "" {
		text += "\n\n[Output truncated. Full output: " + msg.FullOutputPath + "]"
	}
	return text
}

// ConvertToLLM converts harness messages into provider-compatible messages.
func ConvertToLLM(messages []Message) []Message {
	converted := make([]Message, 0, len(messages))
	for _, message := range messages {
		switch msg := message.(type) {
		case BashExecutionMessage:
			if !msg.ExcludeFromContext {
				converted = append(converted, UserMessage{
					Content:   []Content{TextContent{Text: BashExecutionToText(msg)}},
					Timestamp: msg.Timestamp,
				})
			}
		case *BashExecutionMessage:
			if msg != nil && !msg.ExcludeFromContext {
				converted = append(converted, UserMessage{
					Content:   []Content{TextContent{Text: BashExecutionToText(*msg)}},
					Timestamp: msg.Timestamp,
				})
			}
		case CustomMessage:
			converted = append(converted, UserMessage{Content: msg.Content, Timestamp: msg.Timestamp})
		case *CustomMessage:
			if msg != nil {
				converted = append(converted, UserMessage{Content: msg.Content, Timestamp: msg.Timestamp})
			}
		case BranchSummaryMessage:
			converted = append(converted, UserMessage{
				Content:   []Content{TextContent{Text: branchSummaryPrefix + msg.Summary + branchSummarySuffix}},
				Timestamp: msg.Timestamp,
			})
		case *BranchSummaryMessage:
			if msg != nil {
				converted = append(converted, UserMessage{
					Content:   []Content{TextContent{Text: branchSummaryPrefix + msg.Summary + branchSummarySuffix}},
					Timestamp: msg.Timestamp,
				})
			}
		case CompactionSummaryMessage:
			converted = append(converted, UserMessage{
				Content:   []Content{TextContent{Text: compactionSummaryPrefix + msg.Summary + compactionSummarySuffix}},
				Timestamp: msg.Timestamp,
			})
		case *CompactionSummaryMessage:
			if msg != nil {
				converted = append(converted, UserMessage{
					Content:   []Content{TextContent{Text: compactionSummaryPrefix + msg.Summary + compactionSummarySuffix}},
					Timestamp: msg.Timestamp,
				})
			}
		case UserMessage, *UserMessage, AssistantMessage, *AssistantMessage, ToolResultMessage, *ToolResultMessage, SystemMessage, *SystemMessage:
			converted = append(converted, message)
		}
	}
	return converted
}

const compactionSummaryPrefix = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n"
const compactionSummarySuffix = "\n</summary>"
const branchSummaryPrefix = "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n"
const branchSummarySuffix = "</summary>"

func nonEmptyStrings(values ...string) []string {
	out := make([]string, 0, len(values))
	for _, value := range values {
		if value != "" {
			out = append(out, value)
		}
	}
	return out
}
