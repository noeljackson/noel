package agent

import (
	"encoding/json"

	"github.com/noeljackson/pi/internal/resources"
)

// Event describes an agent stream event.
type Event interface {
	isEvent()
}

// AgentStartEvent indicates that an agent run has started.
type AgentStartEvent struct {
	SessionID string
}

// AgentEndEvent indicates that an agent run has ended.
type AgentEndEvent struct {
	Reason string
	Err    error
}

// TurnStartEvent indicates that a model turn has started.
type TurnStartEvent struct {
	TurnID string
}

// TurnEndEvent indicates that a model turn has ended.
type TurnEndEvent struct {
	TurnID           string
	ToolCallsPending bool
	ToolResults      []ToolResult
}

// MessageStartEvent indicates that a streamed message has started.
type MessageStartEvent struct {
	TurnID    string
	MessageID string
	Role      Role
	Model     string
}

// MessageUpdateEvent contains an incremental message update.
type MessageUpdateEvent struct {
	MessageID string
	Delta     MessageDelta
}

// MessageDelta describes incremental message content.
type MessageDelta struct {
	TextDelta             string
	ThinkingDelta         string
	SignatureDelta        string
	RedactedThinkingDelta string
	Usage                 *Usage
	ToolUseDelta          *ToolUseDelta
}

// ToolUseDelta describes incremental tool-use content.
type ToolUseDelta struct {
	ID               string
	Name             string
	InputJSONPartial string
}

// MessageEndEvent indicates that a streamed message has ended.
type MessageEndEvent struct {
	MessageID    string
	FinalContent []Content
	StopReason   string
	Usage        Usage
}

// ToolExecutionStartEvent indicates that a tool execution has started.
type ToolExecutionStartEvent struct {
	CallID string
	Name   string
	Input  json.RawMessage
}

// ToolExecutionUpdateEvent contains an incremental tool execution update.
type ToolExecutionUpdateEvent struct {
	CallID  string
	Partial json.RawMessage
}

// ToolExecutionEndEvent indicates that a tool execution has ended.
type ToolExecutionEndEvent struct {
	CallID string
	Result ToolResult
	Err    error
}

type ResourcesReloadEvent struct {
	Diagnostics []resources.Diagnostic
}

func (AgentStartEvent) isEvent()          {}
func (AgentEndEvent) isEvent()            {}
func (TurnStartEvent) isEvent()           {}
func (TurnEndEvent) isEvent()             {}
func (MessageStartEvent) isEvent()        {}
func (MessageUpdateEvent) isEvent()       {}
func (MessageEndEvent) isEvent()          {}
func (ToolExecutionStartEvent) isEvent()  {}
func (ToolExecutionUpdateEvent) isEvent() {}
func (ToolExecutionEndEvent) isEvent()    {}
func (ResourcesReloadEvent) isEvent()     {}
