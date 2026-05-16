// Package todo provides the built-in session todo state tool.
package todo

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/noeljackson/pi/internal/agent"
	toolcontract "github.com/noeljackson/pi/internal/tools"
)

var todoSchema = json.RawMessage(`{"type":"object","properties":{"todos":{"type":"array","items":{"type":"object","properties":{"content":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed"]},"activeForm":{"type":"string"}},"required":["content","status"],"additionalProperties":false}}},"required":["todos"],"additionalProperties":false}`)

// Store persists todo state for a session.
type Store interface {
	Get(sessionID string) ([]Item, error)
	Set(sessionID string, items []Item) error
}

// Item describes one todo entry.
type Item struct {
	ID         string    `json:"id"`
	Content    string    `json:"content"`
	Status     Status    `json:"status"`
	ActiveForm string    `json:"activeForm,omitempty"`
	CreatedAt  time.Time `json:"createdAt"`
	UpdatedAt  time.Time `json:"updatedAt"`
}

// Status is a todo lifecycle state.
type Status string

const (
	StatusPending    Status = "pending"
	StatusInProgress Status = "in_progress"
	StatusCompleted  Status = "completed"
)

// Tool replaces the in-session todo list.
type Tool struct {
	store Store
}

// NewTool returns a todo state tool.
func NewTool(store Store) *Tool {
	return &Tool{store: store}
}

func (Tool) Name() string {
	return "todo"
}

func (Tool) Description() string {
	return "Replace the session todo list with the supplied items and statuses."
}

func (Tool) Schema() json.RawMessage {
	return todoSchema
}

func (Tool) ParallelSafe() bool {
	return false
}

func (t *Tool) Execute(_ context.Context, input json.RawMessage, tc agent.ToolCallContext) (agent.ToolResult, error) {
	if t.store == nil {
		return agent.ToolResult{}, fmt.Errorf("todo store is not configured")
	}
	var args struct {
		Todos []struct {
			Content    string `json:"content"`
			Status     Status `json:"status"`
			ActiveForm string `json:"activeForm"`
		} `json:"todos"`
	}
	if err := json.Unmarshal(input, &args); err != nil {
		return agent.ToolResult{}, err
	}

	sessionID := tc.SessionID
	existing, err := t.store.Get(sessionID)
	if err != nil {
		return agent.ToolResult{}, err
	}
	now := time.Now().UTC()
	items := make([]Item, 0, len(args.Todos))
	usedExisting := make(map[int]struct{})
	for i, todo := range args.Todos {
		content := strings.TrimSpace(todo.Content)
		if content == "" {
			return agent.ToolResult{}, fmt.Errorf("todos[%d].content is required", i)
		}
		if !validStatus(todo.Status) {
			return agent.ToolResult{}, fmt.Errorf("todos[%d].status must be pending, in_progress, or completed", i)
		}
		item := Item{
			ID:         fmt.Sprintf("%d", i+1),
			Content:    content,
			Status:     todo.Status,
			ActiveForm: strings.TrimSpace(todo.ActiveForm),
			CreatedAt:  now,
			UpdatedAt:  now,
		}
		if item.ActiveForm == "" {
			item.ActiveForm = content
		}
		if previous, ok := matchExisting(existing, usedExisting, content); ok {
			item.ID = previous.ID
			item.CreatedAt = previous.CreatedAt
			if previous.Status == item.Status && previous.ActiveForm == item.ActiveForm {
				item.UpdatedAt = previous.UpdatedAt
			}
		}
		items = append(items, item)
	}
	if err := t.store.Set(sessionID, items); err != nil {
		return agent.ToolResult{}, err
	}
	return textResult(tc.CallID, formatItems(items), todoDetails{Todos: items}, false)
}

type todoDetails struct {
	Todos []Item `json:"todos"`
}

func validStatus(status Status) bool {
	switch status {
	case StatusPending, StatusInProgress, StatusCompleted:
		return true
	default:
		return false
	}
}

func matchExisting(items []Item, used map[int]struct{}, content string) (Item, bool) {
	for i, item := range items {
		if _, ok := used[i]; ok {
			continue
		}
		if item.Content == content {
			used[i] = struct{}{}
			return item, true
		}
	}
	return Item{}, false
}

func formatItems(items []Item) string {
	if len(items) == 0 {
		return "Todo list is now empty."
	}
	var builder strings.Builder
	builder.WriteString("Todo state updated:")
	for _, item := range items {
		fmt.Fprintf(&builder, "\n- [%s] %s", statusMarker(item.Status), item.Content)
		if item.Status == StatusInProgress && item.ActiveForm != "" {
			fmt.Fprintf(&builder, " (%s)", item.ActiveForm)
		}
	}
	return builder.String()
}

func statusMarker(status Status) string {
	switch status {
	case StatusCompleted:
		return "x"
	case StatusInProgress:
		return ">"
	default:
		return " "
	}
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

var _ agent.Tool = (*Tool)(nil)
