package task

import (
	"context"
	"encoding/json"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/noeljackson/pi/internal/agent"
)

func TestTaskSingle(t *testing.T) {
	spawner := &mockSpawner{
		results: map[string]Result{"inspect": {Output: "done", SessionID: "s1"}},
	}
	tool := NewTool(spawner)

	result, err := tool.Execute(context.Background(), json.RawMessage(`{"prompt":"inspect","tools":["read"],"model":"m"}`), agent.ToolCallContext{CallID: "call-1"})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("result marked error")
	}
	if got := textContent(t, result); got != "done" {
		t.Fatalf("content = %q, want done", got)
	}
	if len(spawner.requests) != 1 {
		t.Fatalf("requests = %d, want 1", len(spawner.requests))
	}
	req := spawner.requests[0]
	if req.Prompt != "inspect" || req.Model != "m" || len(req.Tools) != 1 || req.Tools[0] != "read" {
		t.Fatalf("request = %#v", req)
	}
}

func TestTaskParallelPreservesOrder(t *testing.T) {
	spawner := &mockSpawner{
		delay: map[string]time.Duration{
			"one": 30 * time.Millisecond,
			"two": 5 * time.Millisecond,
		},
		results: map[string]Result{
			"one": {Output: "first"},
			"two": {Output: "second"},
		},
	}
	tool := NewTool(spawner)

	input := json.RawMessage(`{"prompt":"parent","parallel":[{"prompt":"one"},{"prompt":"two"}],"concurrency":2}`)
	result, err := tool.Execute(context.Background(), input, agent.ToolCallContext{CallID: "call-1"})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	text := textContent(t, result)
	firstIndex := strings.Index(text, "first")
	secondIndex := strings.Index(text, "second")
	if firstIndex < 0 || secondIndex < 0 || firstIndex > secondIndex {
		t.Fatalf("parallel result order not preserved: %q", text)
	}
}

func TestTaskParallelEnforcesConcurrencyLimit(t *testing.T) {
	spawner := &mockSpawner{
		sleep:   20 * time.Millisecond,
		results: map[string]Result{"a": {Output: "a"}, "b": {Output: "b"}, "c": {Output: "c"}},
	}
	tool := NewTool(spawner)

	input := json.RawMessage(`{"prompt":"parent","parallel":[{"prompt":"a"},{"prompt":"b"},{"prompt":"c"}],"concurrency":2}`)
	if _, err := tool.Execute(context.Background(), input, agent.ToolCallContext{CallID: "call-1"}); err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	if spawner.maxRunning > 2 {
		t.Fatalf("max running = %d, want <= 2", spawner.maxRunning)
	}
}

func TestTaskEmitsProgressUpdates(t *testing.T) {
	spawner := &mockSpawner{results: map[string]Result{"work": {Output: "finished"}}}
	tool := NewTool(spawner)
	var updates []map[string]interface{}

	_, err := tool.Execute(context.Background(), json.RawMessage(`{"prompt":"work"}`), agent.ToolCallContext{
		CallID: "call-1",
		OnUpdate: func(partial json.RawMessage) {
			var update map[string]interface{}
			if err := json.Unmarshal(partial, &update); err != nil {
				t.Fatalf("unmarshal update: %v", err)
			}
			updates = append(updates, update)
		},
	})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	if len(updates) != 2 {
		t.Fatalf("updates = %d, want 2", len(updates))
	}
	if updates[0]["status"] != "running" || updates[1]["status"] != "done" {
		t.Fatalf("updates = %#v", updates)
	}
}

type mockSpawner struct {
	mu         sync.Mutex
	requests   []SpawnRequest
	results    map[string]Result
	delay      map[string]time.Duration
	sleep      time.Duration
	running    int
	maxRunning int
}

func (s *mockSpawner) Spawn(ctx context.Context, req SpawnRequest) (Result, error) {
	s.mu.Lock()
	s.requests = append(s.requests, req)
	s.running++
	if s.running > s.maxRunning {
		s.maxRunning = s.running
	}
	sleep := s.sleep
	if delay := s.delay[req.Prompt]; delay > 0 {
		sleep = delay
	}
	s.mu.Unlock()

	if sleep > 0 {
		select {
		case <-time.After(sleep):
		case <-ctx.Done():
			return Result{}, ctx.Err()
		}
	}

	s.mu.Lock()
	s.running--
	result, ok := s.results[req.Prompt]
	s.mu.Unlock()
	if !ok {
		result = Result{Output: req.Prompt}
	}
	return result, nil
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
