package agent

import (
	"context"
	"encoding/json"
	"errors"
	"testing"
	"time"
)

func TestLoopPersistsParallelToolResultsInSourceOrder(t *testing.T) {
	first := &fakeTool{
		name:         "first",
		parallelSafe: true,
		delay:        20 * time.Millisecond,
		result:       ToolResult{Content: []Content{TextContent{Text: "first"}}},
	}
	second := &fakeTool{
		name:         "second",
		parallelSafe: true,
		result:       ToolResult{Content: []Content{TextContent{Text: "second"}}},
	}
	registry := &fakeRegistry{}
	mustRegister(t, registry, first, second)
	session := &memorySession{}
	provider := &fakeProvider{responses: []fakeResponse{
		{message: AssistantMessage{
			Content: []Content{
				ToolUseContent{ID: "call-1", Name: "first", Input: json.RawMessage(`{}`)},
				ToolUseContent{ID: "call-2", Name: "second", Input: json.RawMessage(`{}`)},
			},
			StopReason: StopToolUse,
		}},
		{message: AssistantMessage{Content: []Content{TextContent{Text: "done"}}, StopReason: StopEndTurn}},
	}}

	_, err := Continue(context.Background(), LoopConfig{
		Provider:      provider,
		Tools:         registry,
		SessionWriter: session,
		Model:         "m",
	}, []Message{UserMessage{Content: []Content{TextContent{Text: "run tools"}}}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	results := firstToolResultMessage(t, session)
	if len(results.Results) != 2 {
		t.Fatalf("result count = %d, want 2", len(results.Results))
	}
	if results.Results[0].ToolUseID != "call-1" || results.Results[1].ToolUseID != "call-2" {
		t.Fatalf("tool result order = %#v", results.Results)
	}
}

func TestLoopBeforeToolCallBlocksExecution(t *testing.T) {
	tool := &fakeTool{name: "blocked", parallelSafe: true}
	registry := &fakeRegistry{}
	mustRegister(t, registry, tool)
	session := &memorySession{}
	provider := toolUseProvider("blocked", "call-1")

	_, err := Continue(context.Background(), LoopConfig{
		Provider:      provider,
		Tools:         registry,
		SessionWriter: session,
		Model:         "m",
		BeforeToolCall: func(ctx context.Context, call ToolUseContent) error {
			return errors.New("blocked by hook")
		},
	}, []Message{UserMessage{Content: []Content{TextContent{Text: "run"}}}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if tool.calls != 0 {
		t.Fatalf("tool calls = %d, want 0", tool.calls)
	}
	results := firstToolResultMessage(t, session)
	if len(results.Results) != 1 || !results.Results[0].IsError {
		t.Fatalf("expected error tool result, got %#v", results.Results)
	}
}

func TestLoopAfterToolCallFiresForExecutedCalls(t *testing.T) {
	tool := &fakeTool{name: "ok", parallelSafe: true, result: ToolResult{Content: []Content{TextContent{Text: "ok"}}}}
	registry := &fakeRegistry{}
	mustRegister(t, registry, tool)
	provider := toolUseProvider("ok", "call-1")
	calls := 0

	_, err := Continue(context.Background(), LoopConfig{
		Provider: provider,
		Tools:    registry,
		Model:    "m",
		AfterToolCall: func(ctx context.Context, call ToolUseContent, result ToolResult) {
			calls++
		},
	}, []Message{UserMessage{Content: []Content{TextContent{Text: "run"}}}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if calls != 1 {
		t.Fatalf("after hook calls = %d, want 1", calls)
	}
}

func TestLoopTerminateResultExits(t *testing.T) {
	tool := &fakeTool{name: "stop", parallelSafe: true, result: ToolResult{Terminate: true}}
	registry := &fakeRegistry{}
	mustRegister(t, registry, tool)
	provider := &fakeProvider{responses: []fakeResponse{
		{message: AssistantMessage{
			Content:    []Content{ToolUseContent{ID: "call-1", Name: "stop", Input: json.RawMessage(`{}`)}},
			StopReason: StopToolUse,
		}},
		{message: AssistantMessage{Content: []Content{TextContent{Text: "should not run"}}, StopReason: StopEndTurn}},
	}}

	_, err := Continue(context.Background(), LoopConfig{
		Provider: provider,
		Tools:    registry,
		Model:    "m",
	}, []Message{UserMessage{Content: []Content{TextContent{Text: "run"}}}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if got := provider.requestCount(); got != 1 {
		t.Fatalf("request count = %d, want 1", got)
	}
}

func TestLoopPrepareNextTurnStopExits(t *testing.T) {
	provider := &fakeProvider{responses: []fakeResponse{
		{message: AssistantMessage{Content: []Content{TextContent{Text: "one"}}, StopReason: StopEndTurn}},
		{message: AssistantMessage{Content: []Content{TextContent{Text: "two"}}, StopReason: StopEndTurn}},
	}}
	called := false
	_, err := Continue(context.Background(), LoopConfig{
		Provider: provider,
		Model:    "m",
		PrepareNextTurn: func(ctx context.Context, messages []Message) (NextTurnDirective, error) {
			called = true
			return NextTurnDirective{Stop: true}, nil
		},
	}, []Message{UserMessage{Content: []Content{TextContent{Text: "run"}}}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if !called {
		t.Fatal("prepare hook was not called")
	}
	if got := provider.requestCount(); got != 1 {
		t.Fatalf("request count = %d, want 1", got)
	}
}

func TestLoopShouldStopAfterTurnOverridesDefault(t *testing.T) {
	tool := &fakeTool{name: "again", parallelSafe: true}
	registry := &fakeRegistry{}
	mustRegister(t, registry, tool)
	provider := &fakeProvider{responses: []fakeResponse{
		{message: AssistantMessage{
			Content:    []Content{ToolUseContent{ID: "call-1", Name: "again", Input: json.RawMessage(`{}`)}},
			StopReason: StopToolUse,
		}},
		{message: AssistantMessage{Content: []Content{TextContent{Text: "should not run"}}, StopReason: StopEndTurn}},
	}}

	_, err := Continue(context.Background(), LoopConfig{
		Provider: provider,
		Tools:    registry,
		Model:    "m",
		ShouldStopAfterTurn: func(turn int, last *AssistantMessage) bool {
			return true
		},
	}, []Message{UserMessage{Content: []Content{TextContent{Text: "run"}}}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if got := provider.requestCount(); got != 1 {
		t.Fatalf("request count = %d, want 1", got)
	}
}

func toolUseProvider(name string, id string) *fakeProvider {
	return &fakeProvider{responses: []fakeResponse{
		{message: AssistantMessage{
			Content:    []Content{ToolUseContent{ID: id, Name: name, Input: json.RawMessage(`{}`)}},
			StopReason: StopToolUse,
		}},
		{message: AssistantMessage{Content: []Content{TextContent{Text: "done"}}, StopReason: StopEndTurn}},
	}}
}

func firstToolResultMessage(t *testing.T, session *memorySession) ToolResultMessage {
	t.Helper()
	session.mu.Lock()
	defer session.mu.Unlock()
	for _, message := range session.messages {
		if results, ok := message.(ToolResultMessage); ok {
			return results
		}
	}
	t.Fatal("missing tool result message")
	return ToolResultMessage{}
}

func mustRegister(t *testing.T, registry *fakeRegistry, tools ...Tool) {
	t.Helper()
	for _, tool := range tools {
		if err := registry.Register(tool); err != nil {
			t.Fatal(err)
		}
	}
}
