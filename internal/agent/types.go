// Package agent defines shared agent interfaces and message types.
package agent

import "encoding/json"

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
)

// Content describes a message content block.
type Content interface {
	isContent()
}

// TextContent contains plain text.
type TextContent struct {
	Text string
}

// ThinkingContent contains model reasoning text and an optional signature.
type ThinkingContent struct {
	Thinking  string
	Signature string
}

// ImageContent contains image input content.
type ImageContent struct {
	Source ImageSource
}

// ToolUseContent contains a model-requested tool invocation.
type ToolUseContent struct {
	ID    string
	Name  string
	Input json.RawMessage
}

// ToolResultContent contains the result of a tool invocation.
type ToolResultContent struct {
	ToolUseID string
	Content   []Content
	IsError   bool
}

// ImageSource describes encoded image data.
type ImageSource struct {
	Type      string
	MediaType string
	Data      string
}

// Message describes a conversation message.
type Message interface {
	isMessage()

	// Role returns the sender role for the message.
	Role() Role
}

// UserMessage contains user-authored content.
type UserMessage struct {
	Content []Content
}

// AssistantMessage contains assistant-authored content and response metadata.
type AssistantMessage struct {
	Content    []Content
	StopReason string
	Model      string
	Usage      Usage
}

// ToolResultMessage contains one or more tool results.
type ToolResultMessage struct {
	Results []ToolResult
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
	Content []Content
}

// Usage describes token usage reported by a model provider.
type Usage struct {
	InputTokens              int
	OutputTokens             int
	CacheCreationInputTokens int
	CacheReadInputTokens     int
}

// ToolCall describes a tool invocation requested by the model.
type ToolCall struct {
	ID    string
	Name  string
	Input json.RawMessage
}

func (TextContent) isContent()       {}
func (ThinkingContent) isContent()   {}
func (ImageContent) isContent()      {}
func (ToolUseContent) isContent()    {}
func (ToolResultContent) isContent() {}

func (UserMessage) isMessage()       {}
func (AssistantMessage) isMessage()  {}
func (ToolResultMessage) isMessage() {}
func (SystemMessage) isMessage()     {}

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
