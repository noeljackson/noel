package tui

import (
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/charmbracelet/bubbles/key"
	"github.com/charmbracelet/bubbles/textarea"
	"github.com/charmbracelet/bubbles/viewport"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/noeljackson/pi/internal/agent"
	"github.com/noeljackson/pi/internal/tui/components"
)

type Options struct {
	EventSource <-chan agent.Event
	Messages    []agent.Message
	Model       string
	Submit      func(text string)
	Abort       func()
}

type Model struct {
	opts     Options
	keys     keyMap
	viewport viewport.Model
	editor   textarea.Model

	width  int
	height int

	entries          []chatEntry
	assistantEntries map[string]int
	tools            map[string]components.ToolCardState

	modelName string
	usage     agent.Usage
	turn      string
	queued    []string
}

type chatEntry struct {
	kind      entryKind
	text      string
	messageID string
	content   []agent.Content
	toolID    string
}

type entryKind int

const (
	entryUser entryKind = iota
	entryAssistant
	entryTool
	entryError
)

type eventMsg struct {
	event agent.Event
	ok    bool
}

func New(opts Options) Model {
	editor := textarea.New()
	editor.Placeholder = "Message pi..."
	editor.Prompt = "> "
	editor.ShowLineNumbers = false
	editor.CharLimit = 0
	editor.SetHeight(3)
	editor.Focus()

	vp := viewport.New(80, 20)

	initialModel := opts.Model
	if initialModel == "" {
		initialModel = "unknown"
	}
	model := Model{
		opts:             opts,
		keys:             defaultKeyMap(),
		viewport:         vp,
		editor:           editor,
		assistantEntries: make(map[string]int),
		tools:            make(map[string]components.ToolCardState),
		modelName:        initialModel,
		turn:             "idle",
	}
	model.loadMessages(opts.Messages)
	return model
}

func (m Model) Init() tea.Cmd {
	return tea.Batch(textarea.Blink, waitForEvent(m.opts.EventSource))
}

func (m Model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmds []tea.Cmd

	switch value := msg.(type) {
	case tea.WindowSizeMsg:
		m.width = value.Width
		m.height = value.Height
		m.resize()
		m.refreshHistory(true)
	case tea.KeyMsg:
		switch {
		case key.Matches(value, m.keys.abortOrQuit):
			if m.turn != "idle" {
				m.turn = "aborting"
				if m.opts.Abort != nil {
					m.opts.Abort()
				}
				return m, nil
			}
			return m, tea.Quit
		case key.Matches(value, m.keys.submit):
			m.submitEditor()
			m.resize()
			m.refreshHistory(true)
			return m, nil
		case key.Matches(value, m.keys.newline):
			m.editor.InsertString("\n")
			return m, nil
		case key.Matches(value, m.keys.pageUp):
			m.viewport.PageUp()
			return m, nil
		case key.Matches(value, m.keys.pageDown):
			m.viewport.PageDown()
			return m, nil
		case key.Matches(value, m.keys.clear):
			m.refreshHistory(true)
			return m, tea.ClearScreen
		}
	case eventMsg:
		if !value.ok {
			return m, tea.Quit
		}
		m.applyEvent(value.event)
		m.resize()
		m.refreshHistory(true)
		return m, waitForEvent(m.opts.EventSource)
	}

	var cmd tea.Cmd
	m.editor, cmd = m.editor.Update(msg)
	cmds = append(cmds, cmd)

	m.viewport, cmd = m.viewport.Update(msg)
	cmds = append(cmds, cmd)

	return m, tea.Batch(cmds...)
}

func (m Model) View() string {
	queue := m.queueView()
	footer := components.FooterView(components.FooterState{
		Model:        m.modelName,
		InputTokens:  m.usage.InputTokens,
		OutputTokens: m.usage.OutputTokens,
		Turn:         m.turn,
		Queued:       len(m.queued),
	})

	parts := []string{m.viewport.View()}
	if queue != "" {
		parts = append(parts, queue)
	}
	parts = append(parts, defaultTheme.panel.Render(m.editor.View()), footer)
	return strings.Join(parts, "\n")
}

func waitForEvent(source <-chan agent.Event) tea.Cmd {
	if source == nil {
		return nil
	}
	return func() tea.Msg {
		event, ok := <-source
		return eventMsg{event: event, ok: ok}
	}
}

func (m *Model) submitEditor() {
	text := strings.TrimSpace(m.editor.Value())
	if text == "" {
		return
	}
	m.editor.Reset()

	if m.turn != "idle" {
		m.queued = append(m.queued, text)
		return
	}
	m.entries = append(m.entries, chatEntry{kind: entryUser, text: text})
	if m.opts.Submit != nil {
		m.opts.Submit(text)
	}
}

func (m *Model) loadMessages(messages []agent.Message) {
	for _, message := range messages {
		switch msg := message.(type) {
		case agent.UserMessage:
			m.entries = append(m.entries, chatEntry{kind: entryUser, text: messageText(msg.Content)})
		case agent.AssistantMessage:
			m.entries = append(m.entries, chatEntry{kind: entryAssistant, content: msg.Content})
			if msg.Model != "" {
				m.modelName = msg.Model
			}
			if msg.Usage != (agent.Usage{}) {
				m.usage = msg.Usage
			}
			for _, content := range msg.Content {
				toolUse, ok := content.(agent.ToolUseContent)
				if !ok {
					continue
				}
				card := m.ensureTool(toolUse.ID, toolUse.Name)
				card.ArgsSummary = components.CompactJSON(toolUse.Input)
				m.tools[toolUse.ID] = card
			}
		case agent.ToolResultMessage:
			for _, result := range msg.Results {
				card := m.ensureTool(result.ToolUseID, "")
				card.Status = components.ToolDone
				if result.IsError {
					card.Status = components.ToolError
				}
				card.Body = appendResultContent(card.Body, result.Content)
				applyResultDetails(&card, result.Details)
				m.tools[result.ToolUseID] = card
			}
		}
	}
}

func (m *Model) applyEvent(event agent.Event) {
	switch value := event.(type) {
	case agent.AgentStartEvent:
		m.turn = "idle"
	case agent.TurnStartEvent:
		m.turn = "streaming"
	case agent.MessageStartEvent:
		if value.Role != agent.RoleAssistant {
			return
		}
		m.modelName = value.Model
		index := len(m.entries)
		m.entries = append(m.entries, chatEntry{kind: entryAssistant, messageID: value.MessageID})
		m.assistantEntries[value.MessageID] = index
	case agent.MessageUpdateEvent:
		m.applyMessageUpdate(value)
	case agent.MessageEndEvent:
		m.applyMessageEnd(value)
	case agent.ToolExecutionStartEvent:
		card := m.ensureTool(value.CallID, value.Name)
		card.Status = components.ToolRunning
		card.StartedAt = time.Now()
		card.ArgsSummary = components.CompactJSON(value.Input)
		m.tools[value.CallID] = card
	case agent.ToolExecutionUpdateEvent:
		card := m.ensureTool(value.CallID, "")
		card.Status = components.ToolRunning
		card.Body = components.AppendRawBody(card.Body, value.Partial)
		m.tools[value.CallID] = card
	case agent.ToolExecutionEndEvent:
		card := m.ensureTool(value.CallID, "")
		card.EndedAt = time.Now()
		card.Status = components.ToolDone
		if value.Err != nil || value.Result.IsError {
			card.Status = components.ToolError
		}
		if value.Err != nil {
			card.Err = value.Err.Error()
		}
		card.Body = appendResultContent(card.Body, value.Result.Content)
		applyResultDetails(&card, value.Result.Details)
		m.tools[value.CallID] = card
	case agent.TurnEndEvent:
		m.turn = "idle"
		m.flushQueued()
	case agent.AgentEndEvent:
		m.turn = "idle"
		if value.Err != nil {
			m.entries = append(m.entries, chatEntry{kind: entryError, text: value.Err.Error()})
		}
	}
}

func (m *Model) applyMessageUpdate(event agent.MessageUpdateEvent) {
	index, ok := m.assistantEntries[event.MessageID]
	if !ok {
		index = len(m.entries)
		m.entries = append(m.entries, chatEntry{kind: entryAssistant, messageID: event.MessageID})
		m.assistantEntries[event.MessageID] = index
	}

	entry := &m.entries[index]
	if event.Delta.TextDelta != "" {
		appendContent(&entry.content, agent.TextContent{Text: event.Delta.TextDelta})
	}
	if event.Delta.ThinkingDelta != "" {
		appendContent(&entry.content, agent.ThinkingContent{Thinking: event.Delta.ThinkingDelta})
	}
	if event.Delta.ToolUseDelta != nil {
		delta := event.Delta.ToolUseDelta
		card := m.ensureTool(delta.ID, delta.Name)
		if delta.InputJSONPartial != "" {
			card.ArgsSummary += delta.InputJSONPartial
		}
		m.tools[delta.ID] = card
	}
}

func (m *Model) applyMessageEnd(event agent.MessageEndEvent) {
	if index, ok := m.assistantEntries[event.MessageID]; ok && len(event.FinalContent) > 0 {
		m.entries[index].content = event.FinalContent
		for _, content := range event.FinalContent {
			toolUse, ok := content.(agent.ToolUseContent)
			if !ok {
				continue
			}
			card := m.ensureTool(toolUse.ID, toolUse.Name)
			card.ArgsSummary = components.CompactJSON(toolUse.Input)
			m.tools[toolUse.ID] = card
		}
	}
	m.usage = event.Usage
}

func (m *Model) ensureTool(id string, name string) components.ToolCardState {
	card, ok := m.tools[id]
	if !ok {
		card = components.ToolCardState{ID: id, Name: name, Status: components.ToolPending}
		m.tools[id] = card
		m.entries = append(m.entries, chatEntry{kind: entryTool, toolID: id})
	}
	if name != "" {
		card.Name = name
	}
	if card.Name == "" {
		card.Name = "tool"
	}
	return card
}

func (m *Model) flushQueued() {
	if len(m.queued) == 0 {
		return
	}
	text := m.queued[0]
	m.queued = m.queued[1:]
	m.entries = append(m.entries, chatEntry{kind: entryUser, text: text})
	if m.opts.Submit != nil {
		m.opts.Submit(text)
	}
}

func (m *Model) resize() {
	if m.width <= 0 {
		return
	}
	editorWidth := max(1, m.width-4)
	m.editor.SetWidth(editorWidth)

	footerHeight := 1
	editorHeight := m.editor.Height() + 2
	queueHeight := 0
	if len(m.queued) > 0 {
		queueHeight = min(3, len(m.queued)+1)
	}
	viewportHeight := m.height - footerHeight - editorHeight - queueHeight
	if viewportHeight < 1 {
		viewportHeight = 1
	}
	m.viewport.Width = m.width
	m.viewport.Height = viewportHeight
}

func (m *Model) refreshHistory(scrollBottom bool) {
	var rendered []string
	for _, entry := range m.entries {
		var text string
		switch entry.kind {
		case entryUser:
			text = components.UserMessageView(entry.text)
		case entryAssistant:
			text = components.AssistantMessageView(entry.content)
		case entryTool:
			card, ok := m.tools[entry.toolID]
			if ok {
				text = components.ToolCard(card, m.width)
			}
		case entryError:
			text = components.ErrorMessageView(entry.text)
		}
		if strings.TrimSpace(text) != "" {
			rendered = append(rendered, text)
		}
	}
	m.viewport.SetContent(strings.Join(rendered, "\n\n"))
	if scrollBottom {
		m.viewport.GotoBottom()
	}
}

func (m Model) queueView() string {
	if len(m.queued) == 0 {
		return ""
	}
	var lines []string
	lines = append(lines, defaultTheme.dim.Render(fmt.Sprintf("queued: %d", len(m.queued))))
	for i, text := range m.queued {
		if i >= 2 {
			lines = append(lines, defaultTheme.dim.Render("..."))
			break
		}
		lines = append(lines, defaultTheme.dim.Render("- "+truncateQueueText(text, max(8, m.width-4))))
	}
	return strings.Join(lines, "\n")
}

func appendContent(content *[]agent.Content, next agent.Content) {
	if len(*content) == 0 {
		*content = append(*content, next)
		return
	}

	last := (*content)[len(*content)-1]
	switch value := next.(type) {
	case agent.TextContent:
		if previous, ok := last.(agent.TextContent); ok {
			(*content)[len(*content)-1] = agent.TextContent{Text: previous.Text + value.Text}
			return
		}
	case agent.ThinkingContent:
		if previous, ok := last.(agent.ThinkingContent); ok {
			(*content)[len(*content)-1] = agent.ThinkingContent{Thinking: previous.Thinking + value.Thinking}
			return
		}
	}
	*content = append(*content, next)
}

func appendResultContent(body []string, content []agent.Content) []string {
	for _, block := range content {
		switch value := block.(type) {
		case agent.TextContent:
			if strings.TrimSpace(value.Text) != "" {
				body = append(body, strings.Split(strings.TrimRight(value.Text, "\n"), "\n")...)
			}
		case agent.ThinkingContent:
			if strings.TrimSpace(value.Thinking) != "" {
				body = append(body, strings.Split(strings.TrimRight(value.Thinking, "\n"), "\n")...)
			}
		}
	}
	return body
}

func messageText(content []agent.Content) string {
	var parts []string
	for _, block := range content {
		switch value := block.(type) {
		case agent.TextContent:
			if strings.TrimSpace(value.Text) != "" {
				parts = append(parts, value.Text)
			}
		case agent.ThinkingContent:
			if strings.TrimSpace(value.Thinking) != "" {
				parts = append(parts, value.Thinking)
			}
		}
	}
	return strings.Join(parts, "\n")
}

func applyResultDetails(card *components.ToolCardState, raw json.RawMessage) {
	if len(raw) == 0 {
		return
	}
	var details map[string]interface{}
	if err := json.Unmarshal(raw, &details); err != nil {
		return
	}
	for _, key := range []string{"exitCode", "exit_code"} {
		if value, ok := numberAsInt(details[key]); ok {
			card.ExitCode = &value
			break
		}
	}
	for _, key := range []string{"bytesWritten", "bytes"} {
		if value, ok := numberAsInt(details[key]); ok {
			card.Bytes = &value
			break
		}
	}
}

func numberAsInt(value interface{}) (int, bool) {
	switch typed := value.(type) {
	case float64:
		return int(typed), true
	case int:
		return typed, true
	default:
		return 0, false
	}
}

func truncateQueueText(text string, width int) string {
	text = strings.Join(strings.Fields(text), " ")
	if len(text) <= width {
		return text
	}
	return text[:max(0, width-1)] + "..."
}
