package agent

import (
	"context"
	"encoding/json"
	"log/slog"
)

// Tool describes an executable tool available to the agent.
type Tool interface {
	// Name returns the unique model-facing tool name.
	Name() string

	// Description returns the model-facing tool description.
	Description() string

	// Schema returns the JSON Schema for the model-facing tool input.
	Schema() json.RawMessage

	// ParallelSafe reports whether the tool can run concurrently with other tools.
	ParallelSafe() bool

	// Execute runs the tool with raw JSON input and call context.
	Execute(ctx context.Context, input json.RawMessage, tc ToolCallContext) (ToolResult, error)
}

// ToolCallContext describes execution context passed to every tool call.
type ToolCallContext struct {
	CallID   string
	Cwd      string
	OnUpdate func(partial json.RawMessage)
	Logger   *slog.Logger
}

// ToolRegistry describes a registry of executable tools.
type ToolRegistry interface {
	// Register adds a tool to the registry.
	Register(Tool) error

	// Get returns a registered tool by name.
	Get(name string) (Tool, bool)

	// All returns every registered tool.
	All() []Tool
}
