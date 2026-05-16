package agent

import (
	"context"
	"errors"
	"fmt"
	"sync"
	"time"

	"github.com/noeljackson/pi/internal/resources"
)

type AgentState string

const (
	AgentIdle          AgentState = "Idle"
	AgentStreaming     AgentState = "Streaming"
	AgentToolExecution AgentState = "ToolExecution"
	AgentAborted       AgentState = "Aborted"
	AgentError         AgentState = "Error"
)

type QueueMode string

const (
	QueueAppend  QueueMode = "append"
	QueueSteer   QueueMode = "steer"
	QueueReplace QueueMode = "replace"
)

type QueuedSubmission struct {
	Mode    QueueMode
	Message Message
	Text    string
}

type queuedKind int

const (
	queuedPrompt queuedKind = iota
	queuedContinue
)

type queuedSubmission struct {
	QueuedSubmission
	kind queuedKind
}

type Agent struct {
	mu          sync.Mutex
	cond        *sync.Cond
	cfg         LoopConfig
	messages    []Message
	state       AgentState
	streaming   *AssistantMessage
	pending     map[string]ToolUseContent
	queue       []queuedSubmission
	lastError   error
	subscribers map[int]func(Event)
	nextSubID   int
	active      context.CancelFunc
	running     bool
	closed      bool
}

type messageReader interface {
	Messages() ([]Message, error)
}

type modelChangeWriter interface {
	AppendModelChange(model, provider, api, reason string) error
}

type thinkingChangeWriter interface {
	AppendThinkingChange(level string) error
}

type runStateWriter interface {
	AppendRunState(payload any, parentID string) error
}

type compactionRecordWriter interface {
	AppendCompactionRecord(summary string, droppedCount int, compactedAt time.Time, parentID string) error
	LastMessageRecordID() string
}

func NewAgent(cfg LoopConfig) *Agent {
	a := &Agent{
		cfg:         cfg,
		state:       AgentIdle,
		pending:     make(map[string]ToolUseContent),
		subscribers: make(map[int]func(Event)),
	}
	a.cond = sync.NewCond(&a.mu)
	if reader, ok := cfg.SessionWriter.(messageReader); ok {
		messages, err := reader.Messages()
		if err == nil {
			a.messages = append([]Message(nil), messages...)
		} else {
			a.state = AgentError
			a.lastError = err
		}
	}
	go a.processQueue()
	return a
}

func (a *Agent) Prompt(ctx context.Context, text string) error {
	return a.PromptMessage(ctx, UserMessage{
		Content:   []Content{TextContent{Text: text}},
		Timestamp: time.Now(),
	})
}

func (a *Agent) PromptMessage(ctx context.Context, msg UserMessage) error {
	return a.enqueue(ctx, queuedSubmission{
		QueuedSubmission: QueuedSubmission{Mode: QueueAppend, Message: msg, Text: userMessageText(msg)},
		kind:             queuedPrompt,
	})
}

func (a *Agent) Continue(ctx context.Context) error {
	return a.enqueue(ctx, queuedSubmission{
		QueuedSubmission: QueuedSubmission{Mode: QueueAppend, Text: "continue"},
		kind:             queuedContinue,
	})
}

// CompactNow runs compaction immediately and updates the in-memory session
// context before returning.
func (a *Agent) CompactNow(ctx context.Context) error {
	a.Abort()
	if err := a.WaitForIdle(ctx); err != nil {
		return err
	}

	a.mu.Lock()
	cfg := a.cfg
	messages := append([]Message(nil), a.messages...)
	a.mu.Unlock()
	if cfg.Compactor == nil {
		return errors.New("compactor is not configured")
	}

	compacted, err := cfg.Compactor.AfterOverflowRetry(ctx, messages, effectiveSystemPrompt(cfg))
	if err != nil {
		return err
	}
	if len(compacted) == len(messages) {
		return nil
	}

	if writer, ok := cfg.SessionWriter.(compactionRecordWriter); ok {
		parentID := writer.LastMessageRecordID()
		dropped := len(messages) - (len(compacted) - 1)
		if err := writer.AppendCompactionRecord(compactionSummaryText(compacted[0]), dropped, time.Now().UTC(), parentID); err != nil {
			return err
		}
	}

	a.mu.Lock()
	a.messages = append([]Message(nil), compacted...)
	a.mu.Unlock()
	return nil
}

// QueueDuringCompaction enqueues a user submission to run after current work.
func (a *Agent) QueueDuringCompaction(ctx context.Context, text string) error {
	return a.FollowUp(ctx, text)
}

func (a *Agent) Retry(ctx context.Context) error {
	a.mu.Lock()
	var message UserMessage
	found := false
	for i := len(a.messages) - 1; i >= 0; i-- {
		candidate, ok := a.messages[i].(UserMessage)
		if ok {
			message = candidate
			found = true
			break
		}
	}
	a.mu.Unlock()
	if !found {
		return errors.New("no user message to retry")
	}
	message.Timestamp = time.Now()
	return a.enqueue(ctx, queuedSubmission{
		QueuedSubmission: QueuedSubmission{Mode: QueueReplace, Message: message, Text: userMessageText(message)},
		kind:             queuedPrompt,
	})
}

func (a *Agent) Steer(ctx context.Context, text string) error {
	msg := UserMessage{Content: []Content{TextContent{Text: text}}, Timestamp: time.Now()}
	if err := a.enqueue(ctx, queuedSubmission{
		QueuedSubmission: QueuedSubmission{Mode: QueueSteer, Message: msg, Text: text},
		kind:             queuedPrompt,
	}); err != nil {
		return err
	}
	a.Abort()
	return nil
}

func (a *Agent) FollowUp(ctx context.Context, text string) error {
	msg := UserMessage{Content: []Content{TextContent{Text: text}}, Timestamp: time.Now()}
	return a.enqueue(ctx, queuedSubmission{
		QueuedSubmission: QueuedSubmission{Mode: QueueAppend, Message: msg, Text: text},
		kind:             queuedPrompt,
	})
}

func (a *Agent) State() AgentState {
	a.mu.Lock()
	defer a.mu.Unlock()
	return a.state
}

func (a *Agent) StreamingMessage() *AssistantMessage {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.streaming == nil {
		return nil
	}
	copy := *a.streaming
	copy.Content = append([]Content(nil), a.streaming.Content...)
	return &copy
}

func (a *Agent) PendingToolCalls() []ToolUseContent {
	a.mu.Lock()
	defer a.mu.Unlock()
	pending := make([]ToolUseContent, 0, len(a.pending))
	for _, call := range a.pending {
		pending = append(pending, call)
	}
	return pending
}

func (a *Agent) Queue() []QueuedSubmission {
	a.mu.Lock()
	defer a.mu.Unlock()
	queue := make([]QueuedSubmission, 0, len(a.queue))
	for _, item := range a.queue {
		queue = append(queue, item.QueuedSubmission)
	}
	return queue
}

func (a *Agent) LastError() error {
	a.mu.Lock()
	defer a.mu.Unlock()
	return a.lastError
}

func (a *Agent) Resources() resources.Resources {
	a.mu.Lock()
	defer a.mu.Unlock()
	return a.cfg.Resources
}

func (a *Agent) ReloadResources() error {
	a.mu.Lock()
	loader := a.cfg.ResourceLoader
	a.mu.Unlock()
	if loader == nil {
		return errors.New("resource loader is not configured")
	}
	loaded, err := loader.Load()
	if err != nil {
		return err
	}
	a.mu.Lock()
	a.cfg.Resources = loaded
	a.mu.Unlock()
	a.publish(ResourcesReloadEvent{Diagnostics: loaded.Diagnostics})
	return nil
}

func (a *Agent) Abort() {
	a.mu.Lock()
	cancel := a.active
	a.mu.Unlock()
	if cancel != nil {
		cancel()
	}
	if writer, ok := a.cfg.SessionWriter.(runStateWriter); ok {
		_ = writer.AppendRunState(map[string]string{"phase": "abort"}, "")
	}
}

func (a *Agent) WaitForIdle(ctx context.Context) error {
	done := make(chan struct{})
	go func() {
		select {
		case <-ctx.Done():
			a.cond.Broadcast()
		case <-done:
		}
	}()
	a.mu.Lock()
	defer a.mu.Unlock()
	defer close(done)
	for a.running || len(a.queue) > 0 {
		if err := ctx.Err(); err != nil {
			return err
		}
		a.cond.Wait()
	}
	return nil
}

func (a *Agent) Subscribe(handler func(Event)) func() {
	a.mu.Lock()
	defer a.mu.Unlock()
	id := a.nextSubID
	a.nextSubID++
	a.subscribers[id] = handler
	return func() {
		a.mu.Lock()
		defer a.mu.Unlock()
		delete(a.subscribers, id)
	}
}

func (a *Agent) SetModel(model string) error {
	a.mu.Lock()
	a.cfg.Model = model
	a.mu.Unlock()
	if writer, ok := a.cfg.SessionWriter.(modelChangeWriter); ok {
		return writer.AppendModelChange(model, "", "", "user")
	}
	return nil
}

func (a *Agent) SetThinking(level string) error {
	a.mu.Lock()
	a.cfg.Thinking = level
	a.mu.Unlock()
	if writer, ok := a.cfg.SessionWriter.(thinkingChangeWriter); ok {
		return writer.AppendThinkingChange(level)
	}
	return nil
}

func (a *Agent) ActivateTools(names []string) error {
	a.mu.Lock()
	defer a.mu.Unlock()
	active := make(map[string]struct{}, len(a.cfg.ActiveTools)+len(names))
	for _, name := range a.cfg.ActiveTools {
		active[name] = struct{}{}
	}
	if len(a.cfg.ActiveTools) == 0 {
		for _, tool := range toolsFromConfig(a.cfg) {
			active[tool.Name()] = struct{}{}
		}
	}
	for _, name := range names {
		if _, ok := lookupTool(a.cfg.Tools, name); !ok {
			return fmt.Errorf("unknown tool: %s", name)
		}
		active[name] = struct{}{}
	}
	a.cfg.ActiveTools = mapKeys(active)
	return nil
}

func (a *Agent) DeactivateTools(names []string) error {
	a.mu.Lock()
	defer a.mu.Unlock()
	inactive := make(map[string]struct{}, len(names))
	for _, name := range names {
		inactive[name] = struct{}{}
	}
	active := a.cfg.ActiveTools
	if len(active) == 0 {
		for _, tool := range toolsFromConfig(a.cfg) {
			active = append(active, tool.Name())
		}
	}
	next := make([]string, 0, len(active))
	for _, name := range active {
		if _, ok := inactive[name]; !ok {
			next = append(next, name)
		}
	}
	a.cfg.ActiveTools = next
	return nil
}

func (a *Agent) enqueue(ctx context.Context, item queuedSubmission) error {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.closed {
		return errors.New("agent is closed")
	}
	if err := ctx.Err(); err != nil {
		return err
	}
	if item.Mode == QueueSteer || item.Mode == QueueReplace {
		a.queue = append([]queuedSubmission{item}, a.queue...)
	} else {
		a.queue = append(a.queue, item)
	}
	a.cond.Broadcast()
	return nil
}

func (a *Agent) processQueue() {
	for {
		a.mu.Lock()
		for len(a.queue) == 0 && !a.closed {
			a.cond.Wait()
		}
		if a.closed {
			a.mu.Unlock()
			return
		}
		item := a.queue[0]
		a.queue = a.queue[1:]
		ctx, cancel := context.WithCancel(context.Background())
		a.active = cancel
		a.running = true
		a.state = AgentStreaming
		a.streaming = nil
		a.pending = make(map[string]ToolUseContent)
		a.lastError = nil
		a.cond.Broadcast()
		a.mu.Unlock()

		err := a.runSubmission(ctx, item)
		cancel()

		a.mu.Lock()
		a.active = nil
		a.running = false
		a.streaming = nil
		a.pending = make(map[string]ToolUseContent)
		if err != nil {
			a.lastError = err
			if errors.Is(err, context.Canceled) || ctx.Err() != nil {
				a.state = AgentAborted
			} else {
				a.state = AgentError
			}
		} else if len(a.queue) == 0 {
			a.state = AgentIdle
		}
		a.cond.Broadcast()
		a.mu.Unlock()
	}
}

func (a *Agent) runSubmission(ctx context.Context, item queuedSubmission) error {
	a.mu.Lock()
	cfg := a.cfg
	messages := append([]Message(nil), a.messages...)
	a.mu.Unlock()

	if item.kind == queuedPrompt {
		if item.Message == nil {
			return errors.New("queued prompt is missing a message")
		}
		if cfg.SessionWriter != nil {
			if err := cfg.SessionWriter.AppendMessage(item.Message); err != nil {
				return err
			}
		}
		a.mu.Lock()
		a.messages = append(a.messages, item.Message)
		messages = append([]Message(nil), a.messages...)
		a.mu.Unlock()
	}

	if len(messages) == 0 {
		return errors.New("cannot continue empty session")
	}
	_, err := Continue(ctx, cfg, messages, a.processEvent)
	return err
}

func (a *Agent) processEvent(event Event) {
	a.mu.Lock()
	switch evt := event.(type) {
	case MessageStartEvent:
		if evt.Role == RoleAssistant {
			a.state = AgentStreaming
			a.streaming = &AssistantMessage{Model: evt.Model, Timestamp: time.Now()}
		}
	case MessageUpdateEvent:
		a.applyMessageUpdateLocked(evt)
	case MessageEndEvent:
		a.applyMessageEndLocked(evt)
	case ToolExecutionStartEvent:
		a.state = AgentToolExecution
		a.pending[evt.CallID] = ToolUseContent{ID: evt.CallID, Name: evt.Name, Input: evt.Input}
	case ToolExecutionEndEvent:
		delete(a.pending, evt.CallID)
		if len(a.pending) == 0 {
			a.state = AgentStreaming
		}
	case TurnEndEvent:
		if len(evt.ToolResults) > 0 {
			a.messages = append(a.messages, ToolResultMessage{Results: evt.ToolResults, Timestamp: time.Now()})
		}
	case AgentEndEvent:
		if evt.Err != nil {
			a.lastError = evt.Err
		}
		a.streaming = nil
	}
	subscribers := make([]func(Event), 0, len(a.subscribers))
	for _, subscriber := range a.subscribers {
		subscribers = append(subscribers, subscriber)
	}
	a.cond.Broadcast()
	a.mu.Unlock()

	publishTo(subscribers, event)
}

func (a *Agent) publish(event Event) {
	a.mu.Lock()
	subscribers := make([]func(Event), 0, len(a.subscribers))
	for _, subscriber := range a.subscribers {
		subscribers = append(subscribers, subscriber)
	}
	a.cond.Broadcast()
	a.mu.Unlock()
	publishTo(subscribers, event)
}

func publishTo(subscribers []func(Event), event Event) {
	for _, subscriber := range subscribers {
		subscriber(event)
	}
}

func (a *Agent) applyMessageUpdateLocked(event MessageUpdateEvent) {
	if a.streaming == nil {
		a.streaming = &AssistantMessage{Timestamp: time.Now()}
	}
	if event.Delta.TextDelta != "" {
		appendTextContent(&a.streaming.Content, event.Delta.TextDelta)
	}
	if event.Delta.ThinkingDelta != "" {
		appendThinkingContent(&a.streaming.Content, event.Delta.ThinkingDelta)
	}
	if event.Delta.ToolUseDelta != nil {
		a.applyToolUseDeltaLocked(*event.Delta.ToolUseDelta)
	}
}

func (a *Agent) applyMessageEndLocked(event MessageEndEvent) {
	message := AssistantMessage{
		Content:    append([]Content(nil), event.FinalContent...),
		StopReason: StopReason(event.StopReason),
		Usage:      event.Usage,
		Timestamp:  time.Now(),
	}
	if a.streaming != nil {
		message.Model = a.streaming.Model
	}
	a.messages = append(a.messages, message)
	a.streaming = nil
}

func (a *Agent) applyToolUseDeltaLocked(delta ToolUseDelta) {
	for i := range a.streaming.Content {
		toolUse, ok := a.streaming.Content[i].(ToolUseContent)
		if !ok {
			continue
		}
		if delta.ID != "" && toolUse.ID != "" && delta.ID != toolUse.ID {
			continue
		}
		if delta.ID != "" {
			toolUse.ID = delta.ID
		}
		if delta.Name != "" {
			toolUse.Name = delta.Name
		}
		if delta.InputJSONPartial != "" {
			toolUse.Input = append(toolUse.Input, []byte(delta.InputJSONPartial)...)
		}
		a.streaming.Content[i] = toolUse
		return
	}
	a.streaming.Content = append(a.streaming.Content, ToolUseContent{
		ID:    delta.ID,
		Name:  delta.Name,
		Input: []byte(delta.InputJSONPartial),
	})
}

func appendTextContent(content *[]Content, delta string) {
	last := len(*content) - 1
	if last >= 0 {
		if text, ok := (*content)[last].(TextContent); ok {
			text.Text += delta
			(*content)[last] = text
			return
		}
	}
	*content = append(*content, TextContent{Text: delta})
}

func appendThinkingContent(content *[]Content, delta string) {
	last := len(*content) - 1
	if last >= 0 {
		if thinking, ok := (*content)[last].(ThinkingContent); ok {
			thinking.Thinking += delta
			(*content)[last] = thinking
			return
		}
	}
	*content = append(*content, ThinkingContent{Thinking: delta})
}

func userMessageText(message UserMessage) string {
	text := ""
	for _, content := range message.Content {
		if block, ok := content.(TextContent); ok {
			text += block.Text
		}
	}
	return text
}

func mapKeys(values map[string]struct{}) []string {
	keys := make([]string, 0, len(values))
	for key := range values {
		keys = append(keys, key)
	}
	return keys
}

func compactionSummaryText(message Message) string {
	switch msg := message.(type) {
	case CompactionSummaryMessage:
		return msg.Summary
	case *CompactionSummaryMessage:
		if msg != nil {
			return msg.Summary
		}
	case SystemMessage:
		return userMessageText(UserMessage{Content: msg.Content})
	case *SystemMessage:
		if msg != nil {
			return userMessageText(UserMessage{Content: msg.Content})
		}
	}
	return ""
}
