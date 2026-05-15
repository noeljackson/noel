package agent

import (
	"context"
	"encoding/json"
	"errors"
	"sync"
	"testing"
	"time"
)

type fakeProvider struct {
	mu        sync.Mutex
	responses []fakeResponse
	requests  []StreamRequest
}

type fakeResponse struct {
	message    AssistantMessage
	block      chan struct{}
	waitForCtx bool
}

func (p *fakeProvider) Stream(ctx context.Context, req StreamRequest, emit func(Event)) (*AssistantMessage, error) {
	p.mu.Lock()
	p.requests = append(p.requests, req)
	if len(p.responses) == 0 {
		p.mu.Unlock()
		return nil, errors.New("missing fake response")
	}
	response := p.responses[0]
	p.responses = p.responses[1:]
	p.mu.Unlock()

	if response.waitForCtx {
		<-ctx.Done()
		return nil, ctx.Err()
	}
	emit(MessageStartEvent{MessageID: "msg", Role: RoleAssistant, Model: req.Model})
	for _, content := range response.message.Content {
		if text, ok := content.(TextContent); ok && text.Text != "" {
			emit(MessageUpdateEvent{MessageID: "msg", Delta: MessageDelta{TextDelta: text.Text}})
		}
	}
	if response.block != nil {
		select {
		case <-response.block:
		case <-ctx.Done():
			return nil, ctx.Err()
		}
	}
	emit(MessageEndEvent{
		MessageID:    "msg",
		FinalContent: response.message.Content,
		StopReason:   response.message.StopReason.String(),
		Usage:        response.message.Usage,
	})
	message := response.message
	message.Model = req.Model
	return &message, nil
}

func (p *fakeProvider) requestCount() int {
	p.mu.Lock()
	defer p.mu.Unlock()
	return len(p.requests)
}

func (p *fakeProvider) lastRequest() StreamRequest {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.requests[len(p.requests)-1]
}

type memorySession struct {
	mu              sync.Mutex
	messages        []Message
	events          []Event
	modelChanges    []string
	thinkingChanges []string
	runStates       []any
}

func (s *memorySession) AppendMessage(message Message) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.messages = append(s.messages, message)
	return nil
}

func (s *memorySession) AppendEvent(event Event) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.events = append(s.events, event)
	return nil
}

func (s *memorySession) Messages() ([]Message, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return append([]Message(nil), s.messages...), nil
}

func (s *memorySession) AppendModelChange(model, provider, api, reason string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.modelChanges = append(s.modelChanges, model)
	return nil
}

func (s *memorySession) AppendThinkingChange(level string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.thinkingChanges = append(s.thinkingChanges, level)
	return nil
}

func (s *memorySession) AppendRunState(payload any, parentID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.runStates = append(s.runStates, payload)
	return nil
}

func TestAgentPromptStateTransitions(t *testing.T) {
	release := make(chan struct{})
	provider := &fakeProvider{responses: []fakeResponse{{
		message: AssistantMessage{Content: []Content{TextContent{Text: "pong"}}, StopReason: StopEndTurn},
		block:   release,
	}}}
	agent := NewAgent(LoopConfig{Provider: provider, SessionWriter: &memorySession{}, Model: "m"})

	if err := agent.Prompt(context.Background(), "ping"); err != nil {
		t.Fatal(err)
	}
	waitFor(t, func() bool { return agent.State() == AgentStreaming })
	close(release)
	if err := agent.WaitForIdle(context.Background()); err != nil {
		t.Fatal(err)
	}
	if got := agent.State(); got != AgentIdle {
		t.Fatalf("state = %s, want %s", got, AgentIdle)
	}
}

func TestAgentQueueDrainsAfterTurn(t *testing.T) {
	release := make(chan struct{})
	provider := &fakeProvider{responses: []fakeResponse{
		{message: AssistantMessage{Content: []Content{TextContent{Text: "one"}}, StopReason: StopEndTurn}, block: release},
		{message: AssistantMessage{Content: []Content{TextContent{Text: "two"}}, StopReason: StopEndTurn}},
	}}
	agent := NewAgent(LoopConfig{Provider: provider, SessionWriter: &memorySession{}, Model: "m"})

	if err := agent.Prompt(context.Background(), "first"); err != nil {
		t.Fatal(err)
	}
	waitFor(t, func() bool { return agent.State() == AgentStreaming })
	if err := agent.Prompt(context.Background(), "second"); err != nil {
		t.Fatal(err)
	}
	waitFor(t, func() bool { return len(agent.Queue()) == 1 })
	close(release)
	if err := agent.WaitForIdle(context.Background()); err != nil {
		t.Fatal(err)
	}
	if got := provider.requestCount(); got != 2 {
		t.Fatalf("request count = %d, want 2", got)
	}
}

func TestAgentSteerAbortsAndResubmits(t *testing.T) {
	provider := &fakeProvider{responses: []fakeResponse{
		{waitForCtx: true},
		{message: AssistantMessage{Content: []Content{TextContent{Text: "steered"}}, StopReason: StopEndTurn}},
	}}
	agent := NewAgent(LoopConfig{Provider: provider, SessionWriter: &memorySession{}, Model: "m"})

	if err := agent.Prompt(context.Background(), "first"); err != nil {
		t.Fatal(err)
	}
	waitFor(t, func() bool { return agent.State() == AgentStreaming })
	if err := agent.Steer(context.Background(), "replacement"); err != nil {
		t.Fatal(err)
	}
	if err := agent.WaitForIdle(context.Background()); err != nil {
		t.Fatal(err)
	}
	if got := provider.requestCount(); got != 2 {
		t.Fatalf("request count = %d, want 2", got)
	}
	if got := requestUserText(provider.lastRequest()); got != "firstreplacement" {
		t.Fatalf("last request user text = %q", got)
	}
}

func TestAgentRetryRerunsPreviousUserMessage(t *testing.T) {
	provider := &fakeProvider{responses: []fakeResponse{
		{message: AssistantMessage{Content: []Content{TextContent{Text: "one"}}, StopReason: StopEndTurn}},
		{message: AssistantMessage{Content: []Content{TextContent{Text: "two"}}, StopReason: StopEndTurn}},
	}}
	agent := NewAgent(LoopConfig{Provider: provider, SessionWriter: &memorySession{}, Model: "m"})

	if err := agent.Prompt(context.Background(), "retry me"); err != nil {
		t.Fatal(err)
	}
	if err := agent.WaitForIdle(context.Background()); err != nil {
		t.Fatal(err)
	}
	if err := agent.Retry(context.Background()); err != nil {
		t.Fatal(err)
	}
	if err := agent.WaitForIdle(context.Background()); err != nil {
		t.Fatal(err)
	}
	if got := provider.requestCount(); got != 2 {
		t.Fatalf("request count = %d, want 2", got)
	}
	if got := requestUserText(provider.lastRequest()); got != "retry meretry me" {
		t.Fatalf("last request user text = %q", got)
	}
}

func TestAgentFollowUpAndWaitForIdle(t *testing.T) {
	release := make(chan struct{})
	provider := &fakeProvider{responses: []fakeResponse{
		{message: AssistantMessage{Content: []Content{TextContent{Text: "one"}}, StopReason: StopEndTurn}, block: release},
		{message: AssistantMessage{Content: []Content{TextContent{Text: "two"}}, StopReason: StopEndTurn}},
	}}
	agent := NewAgent(LoopConfig{Provider: provider, SessionWriter: &memorySession{}, Model: "m"})
	if err := agent.Prompt(context.Background(), "first"); err != nil {
		t.Fatal(err)
	}
	waitFor(t, func() bool { return agent.State() == AgentStreaming })
	if err := agent.FollowUp(context.Background(), "after"); err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() { done <- agent.WaitForIdle(context.Background()) }()
	select {
	case <-done:
		t.Fatal("WaitForIdle returned before queued follow-up drained")
	case <-time.After(25 * time.Millisecond):
	}
	close(release)
	if err := <-done; err != nil {
		t.Fatal(err)
	}
}

func TestAgentAbortLeavesAbortedState(t *testing.T) {
	provider := &fakeProvider{responses: []fakeResponse{{waitForCtx: true}}}
	session := &memorySession{}
	agent := NewAgent(LoopConfig{Provider: provider, SessionWriter: session, Model: "m"})
	if err := agent.Prompt(context.Background(), "stop"); err != nil {
		t.Fatal(err)
	}
	waitFor(t, func() bool { return agent.State() == AgentStreaming })
	agent.Abort()
	if err := agent.WaitForIdle(context.Background()); err != nil {
		t.Fatal(err)
	}
	if got := agent.State(); got != AgentAborted {
		t.Fatalf("state = %s, want %s", got, AgentAborted)
	}
	if len(session.runStates) == 0 {
		t.Fatal("expected abort run state")
	}
}

func TestAgentSetModelAndThinkingPersistAndApply(t *testing.T) {
	session := &memorySession{}
	provider := &fakeProvider{responses: []fakeResponse{{
		message: AssistantMessage{Content: []Content{TextContent{Text: "ok"}}, StopReason: StopEndTurn},
	}}}
	agent := NewAgent(LoopConfig{Provider: provider, SessionWriter: session, Model: "old"})
	if err := agent.SetModel("new"); err != nil {
		t.Fatal(err)
	}
	if err := agent.SetThinking("high"); err != nil {
		t.Fatal(err)
	}
	if err := agent.Prompt(context.Background(), "ping"); err != nil {
		t.Fatal(err)
	}
	if err := agent.WaitForIdle(context.Background()); err != nil {
		t.Fatal(err)
	}
	if got := provider.lastRequest().Model; got != "new" {
		t.Fatalf("model = %q, want new", got)
	}
	if len(session.modelChanges) != 1 || session.modelChanges[0] != "new" {
		t.Fatalf("model changes = %#v", session.modelChanges)
	}
	if len(session.thinkingChanges) != 1 || session.thinkingChanges[0] != "high" {
		t.Fatalf("thinking changes = %#v", session.thinkingChanges)
	}
}

func TestAgentSubscriberReceivesEventsInOrder(t *testing.T) {
	provider := &fakeProvider{responses: []fakeResponse{{
		message: AssistantMessage{Content: []Content{TextContent{Text: "ok"}}, StopReason: StopEndTurn},
	}}}
	agent := NewAgent(LoopConfig{Provider: provider, SessionWriter: &memorySession{}, Model: "m"})
	var events []string
	agent.Subscribe(func(event Event) {
		events = append(events, eventName(event))
	})
	if err := agent.Prompt(context.Background(), "ping"); err != nil {
		t.Fatal(err)
	}
	if err := agent.WaitForIdle(context.Background()); err != nil {
		t.Fatal(err)
	}
	want := []string{"agent_start", "turn_start", "message_start", "message_update", "message_end", "turn_end", "agent_end"}
	if len(events) != len(want) {
		t.Fatalf("events = %#v, want %#v", events, want)
	}
	for i := range want {
		if events[i] != want[i] {
			t.Fatalf("events = %#v, want %#v", events, want)
		}
	}
}

type fakeTool struct {
	name         string
	parallelSafe bool
	delay        time.Duration
	result       ToolResult
	calls        int
}

func (t *fakeTool) Name() string            { return t.name }
func (t *fakeTool) Description() string     { return t.name }
func (t *fakeTool) Schema() json.RawMessage { return json.RawMessage(`{"type":"object"}`) }
func (t *fakeTool) ParallelSafe() bool      { return t.parallelSafe }
func (t *fakeTool) Execute(ctx context.Context, input json.RawMessage, tc ToolCallContext) (ToolResult, error) {
	t.calls++
	if t.delay > 0 {
		select {
		case <-time.After(t.delay):
		case <-ctx.Done():
			return ToolResult{}, ctx.Err()
		}
	}
	result := t.result
	if result.ToolUseID == "" {
		result.ToolUseID = tc.CallID
	}
	return result, nil
}

type fakeRegistry struct {
	tools map[string]Tool
}

func (r *fakeRegistry) Register(tool Tool) error {
	if r.tools == nil {
		r.tools = make(map[string]Tool)
	}
	r.tools[tool.Name()] = tool
	return nil
}

func (r *fakeRegistry) Get(name string) (Tool, bool) {
	tool, ok := r.tools[name]
	return tool, ok
}

func (r *fakeRegistry) All() []Tool {
	tools := make([]Tool, 0, len(r.tools))
	for _, tool := range r.tools {
		tools = append(tools, tool)
	}
	return tools
}

func waitFor(t *testing.T, condition func() bool) {
	t.Helper()
	deadline := time.Now().Add(time.Second)
	for time.Now().Before(deadline) {
		if condition() {
			return
		}
		time.Sleep(time.Millisecond)
	}
	t.Fatal("condition was not met")
}

func requestUserText(req StreamRequest) string {
	text := ""
	for _, message := range req.Messages {
		user, ok := message.(UserMessage)
		if !ok {
			continue
		}
		for _, content := range user.Content {
			if block, ok := content.(TextContent); ok {
				text += block.Text
			}
		}
	}
	return text
}

func eventName(event Event) string {
	switch event.(type) {
	case AgentStartEvent:
		return "agent_start"
	case TurnStartEvent:
		return "turn_start"
	case MessageStartEvent:
		return "message_start"
	case MessageUpdateEvent:
		return "message_update"
	case MessageEndEvent:
		return "message_end"
	case ToolExecutionStartEvent:
		return "tool_execution_start"
	case ToolExecutionUpdateEvent:
		return "tool_execution_update"
	case ToolExecutionEndEvent:
		return "tool_execution_end"
	case TurnEndEvent:
		return "turn_end"
	case AgentEndEvent:
		return "agent_end"
	default:
		return "unknown"
	}
}
