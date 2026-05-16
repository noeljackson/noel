package todo

import (
	"context"
	"encoding/json"
	"strings"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

func TestTodoReplacesState(t *testing.T) {
	store := &mockStore{items: []Item{{ID: "1", Content: "old", Status: StatusPending}}}
	tool := NewTool(store)

	result, err := tool.Execute(context.Background(), json.RawMessage(`{"todos":[{"content":"new","status":"pending","activeForm":"working new"}]}`), agent.ToolCallContext{CallID: "call-1", SessionID: "s1"})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("result marked error")
	}
	if len(store.items) != 1 || store.items[0].Content != "new" || store.items[0].Status != StatusPending {
		t.Fatalf("items = %#v", store.items)
	}
	if !strings.Contains(textContent(t, result), "new") {
		t.Fatalf("result content = %q", textContent(t, result))
	}
}

func TestTodoStatusTransitionsPreserveID(t *testing.T) {
	store := &mockStore{}
	tool := NewTool(store)

	if _, err := tool.Execute(context.Background(), json.RawMessage(`{"todos":[{"content":"ship","status":"pending"}]}`), agent.ToolCallContext{SessionID: "s1"}); err != nil {
		t.Fatalf("initial Execute returned error: %v", err)
	}
	id := store.items[0].ID
	createdAt := store.items[0].CreatedAt

	if _, err := tool.Execute(context.Background(), json.RawMessage(`{"todos":[{"content":"ship","status":"in_progress","activeForm":"shipping"}]}`), agent.ToolCallContext{SessionID: "s1"}); err != nil {
		t.Fatalf("transition Execute returned error: %v", err)
	}
	if store.items[0].ID != id {
		t.Fatalf("id = %q, want %q", store.items[0].ID, id)
	}
	if !store.items[0].CreatedAt.Equal(createdAt) {
		t.Fatalf("createdAt changed")
	}
	if store.items[0].Status != StatusInProgress || store.items[0].ActiveForm != "shipping" {
		t.Fatalf("item = %#v", store.items[0])
	}
}

func TestTodoPersistenceRoundTripViaMockStore(t *testing.T) {
	store := &mockStore{}
	tool := NewTool(store)

	if _, err := tool.Execute(context.Background(), json.RawMessage(`{"todos":[{"content":"a","status":"completed"},{"content":"b","status":"pending"}]}`), agent.ToolCallContext{SessionID: "s1"}); err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	got, err := store.Get("s1")
	if err != nil {
		t.Fatalf("Get returned error: %v", err)
	}
	if len(got) != 2 || got[0].Status != StatusCompleted || got[1].Content != "b" {
		t.Fatalf("got = %#v", got)
	}
}

func TestTodoRejectsInvalidStatus(t *testing.T) {
	tool := NewTool(&mockStore{})
	if _, err := tool.Execute(context.Background(), json.RawMessage(`{"todos":[{"content":"a","status":"blocked"}]}`), agent.ToolCallContext{}); err == nil {
		t.Fatalf("expected invalid status error")
	}
}

type mockStore struct {
	items []Item
}

func (s *mockStore) Get(_ string) ([]Item, error) {
	return append([]Item(nil), s.items...), nil
}

func (s *mockStore) Set(_ string, items []Item) error {
	s.items = append([]Item(nil), items...)
	return nil
}

func textContent(t *testing.T, result agent.ToolResult) string {
	t.Helper()
	if len(result.Content) != 1 {
		t.Fatalf("content length = %d, want 1", len(result.Content))
	}
	text, ok := result.Content[0].(agent.TextContent)
	if !ok {
		t.Fatalf("content type = %T, want agent.TextContent", result.Content[0])
	}
	return text.Text
}
