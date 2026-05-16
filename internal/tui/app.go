package tui

import (
	"context"
	"encoding/json"
	"fmt"
	"strconv"
	"strings"
	"time"

	"github.com/charmbracelet/bubbles/viewport"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/noeljackson/pi/internal/agent"
	"github.com/noeljackson/pi/internal/resources"
	"github.com/noeljackson/pi/internal/timings"
	"github.com/noeljackson/pi/internal/tui/autocomplete"
	"github.com/noeljackson/pi/internal/tui/components"
	tuieditor "github.com/noeljackson/pi/internal/tui/editor"
	"github.com/noeljackson/pi/internal/tui/keys"
	"github.com/noeljackson/pi/internal/tui/slash"
)

type Options struct {
	EventSource <-chan agent.Event
	Messages    []agent.Message
	Model       string
	Resources   resources.Resources
	Slash       *slash.Registry
	Agent       *agent.Agent
	Timings     *timings.Timings
	Submit      func(text string)
	Abort       func()
	OpenBrowser func(string) error
	Logout      func(provider string) error
}

type Model struct {
	opts     Options
	viewport viewport.Model
	editor   *tuieditor.Editor

	width  int
	height int

	entries          []chatEntry
	assistantEntries map[string]int
	tools            map[string]components.ToolCardState

	modelName string
	usage     agent.Usage
	turn      string
	queued    []string
	status    string
	thinking  string

	slashRegistry *slash.Registry
	overlay       overlayState
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
	entryBash
	entryBranchSummary
	entryCompactionSummary
	entryCustom
)

type overlayState struct {
	kind     string
	title    string
	items    []components.SelectorItem
	selected int
}

type eventMsg struct {
	event agent.Event
	ok    bool
}

func New(opts Options) Model {
	keyMap := keys.Default()
	editor := tuieditor.New(tuieditor.Options{
		Placeholder:    "Message pi...",
		MaxHistorySize: 100,
		LineWrapping:   true,
		KeyMap:         keyMap,
	})
	editor.Focus()
	editor.SetAutocompleteProvider(newAutocompleteProvider(opts.Resources, opts.Slash))

	vp := viewport.New(80, 20)

	initialModel := opts.Model
	if initialModel == "" {
		initialModel = "unknown"
	}
	model := Model{
		opts:             opts,
		viewport:         vp,
		editor:           editor,
		assistantEntries: make(map[string]int),
		tools:            make(map[string]components.ToolCardState),
		modelName:        initialModel,
		turn:             "idle",
		thinking:         "off",
		slashRegistry:    slash.Builtins(),
	}
	model.loadMessages(opts.Messages)
	return model
}

func (m Model) Init() tea.Cmd {
	return waitForEvent(m.opts.EventSource)
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
		if m.overlay.kind != "" {
			handled, next := m.handleOverlayKey(value)
			if handled {
				return next, nil
			}
		}
		event := keys.FromTeaKey(value)
		consumed, command := m.editor.HandleKey(event)
		switch command {
		case tuieditor.CommandAbort:
			if m.turn != "idle" {
				m.turn = "aborting"
				if m.opts.Abort != nil {
					m.opts.Abort()
				}
				return m, nil
			}
			return m, tea.Quit
		case tuieditor.CommandSubmit:
			if m.submitEditor() {
				return m, tea.Quit
			}
			m.resize()
			m.refreshHistory(true)
			return m, nil
		case tuieditor.CommandPageUp:
			m.viewport.PageUp()
			return m, nil
		case tuieditor.CommandPageDown:
			m.viewport.PageDown()
			return m, nil
		case tuieditor.CommandClear:
			m.refreshHistory(true)
			return m, tea.ClearScreen
		}
		if consumed {
			return m, nil
		}
	case tea.MouseMsg:
		_ = keys.FromTeaMouse(value)
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
	m.viewport, cmd = m.viewport.Update(msg)
	cmds = append(cmds, cmd)

	return m, tea.Batch(cmds...)
}

func (m Model) View() string {
	stopRender := m.opts.Timings.Start("tui.render")
	defer stopRender()

	queue := m.queueView()
	footer := components.FooterView(components.FooterState{
		Model:            m.modelName,
		InputTokens:      m.usage.InputTokens,
		OutputTokens:     m.usage.OutputTokens,
		CacheReadTokens:  m.usage.CacheReadInputTokens,
		CacheWriteTokens: m.usage.CacheCreationInputTokens + m.usage.CacheWriteTokens,
		Turn:             m.turn,
		Queued:           len(m.queued),
		Mode:             "interactive",
		Thinking:         m.thinking,
		Status:           m.status,
		Width:            m.width,
	})

	parts := []string{m.viewport.View()}
	if queue != "" {
		parts = append(parts, queue)
	}
	parts = append(parts, defaultTheme.panel.Render(m.editor.View(max(1, m.width-4), m.editorVisibleHeight())), footer)
	view := strings.Join(parts, "\n")
	if m.overlay.kind != "" {
		view += "\n" + m.renderOverlay()
	}
	return view
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

func (m *Model) submitEditor() bool {
	text := strings.TrimSpace(m.editor.ExpandedValue())
	if text == "" {
		return false
	}
	m.editor.PushHistory(text)
	m.editor.Reset()

	if strings.HasPrefix(text, "/") {
		return m.handleSlash(text)
	}
	if m.turn != "idle" {
		m.queued = append(m.queued, text)
		return false
	}
	m.entries = append(m.entries, chatEntry{kind: entryUser, text: text})
	if m.opts.Submit != nil {
		m.opts.Submit(text)
	}
	return false
}

func (m *Model) runSlashCommand(text string) bool {
	registry := m.opts.Slash
	if registry == nil {
		return false
	}
	name, args := splitSlashCommand(text)
	command, ok := registry.Lookup(name)
	if !ok || command.Handler == nil {
		return false
	}
	output, err := command.Handler(context.Background(), args, m.opts.Agent)
	if err != nil {
		m.entries = append(m.entries, chatEntry{kind: entryError, text: err.Error()})
		return true
	}
	if strings.TrimSpace(output) != "" {
		m.entries = append(m.entries, chatEntry{
			kind:    entryAssistant,
			content: []agent.Content{agent.TextContent{Text: output}},
		})
	}
	return true
}

func splitSlashCommand(text string) (string, string) {
	text = strings.TrimSpace(strings.TrimPrefix(text, "/"))
	name, args, ok := strings.Cut(text, " ")
	if !ok {
		return name, ""
	}
	return name, strings.TrimSpace(args)
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
		case agent.BashExecutionMessage:
			m.entries = append(m.entries, chatEntry{kind: entryBash, text: msg.Command, content: []agent.Content{agent.TextContent{Text: msg.Output}}})
		case agent.BranchSummaryMessage:
			m.entries = append(m.entries, chatEntry{kind: entryBranchSummary, text: msg.Summary})
		case agent.CompactionSummaryMessage:
			m.entries = append(m.entries, chatEntry{kind: entryCompactionSummary, text: msg.Summary})
		case agent.CustomMessage:
			if msg.Display {
				m.entries = append(m.entries, chatEntry{kind: entryCustom, text: messageText(msg.Content)})
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
		m.turn = "streaming"
		m.status = ""
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
	case agent.ResourcesReloadEvent:
		m.status = fmt.Sprintf("reloaded resources (%d diagnostics)", len(value.Diagnostics))
	case agent.SessionForkEvent:
		m.status = "forked to " + value.NewLeafID
	case agent.SessionMoveEvent:
		m.status = "moved to " + value.ToLeafID
	case agent.BranchSummaryEvent:
		m.entries = append(m.entries, chatEntry{kind: entryBranchSummary, text: value.Summary})
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
	footerHeight := 1
	editorHeight := m.editorVisibleHeight() + 2
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

func (m Model) editorVisibleHeight() int {
	if m.height <= 0 {
		return 3
	}
	return max(3, min(8, m.height/4))
}

func newAutocompleteProvider(loaded resources.Resources, registry *slash.Registry) *autocomplete.CombinedProvider {
	if registry == nil {
		registry = slash.Builtins()
	}
	commands := make([]autocomplete.SlashCommand, 0, len(registry.Commands()))
	for _, command := range registry.Commands() {
		args := make([]autocomplete.ArgSpec, 0, len(command.Args))
		for _, arg := range command.Args {
			args = append(args, autocomplete.ArgSpec{Name: arg.Name, Description: arg.Description})
		}
		commands = append(commands, autocomplete.SlashCommand{
			Name:        command.Name,
			Description: command.Description,
			Args:        args,
		})
	}
	return autocomplete.NewCombinedProvider(
		autocomplete.NewSlashProvider(commands),
		autocomplete.NewFileProvider(""),
		autocomplete.SkillProvider{Skills: loaded.Skills},
		autocomplete.PromptTemplateProvider{Templates: loaded.PromptTemplates},
	)
}

func (m *Model) refreshHistory(scrollBottom bool) {
	var rendered []string
	for _, entry := range m.entries {
		var text string
		switch entry.kind {
		case entryUser:
			text = components.UserMessageViewWidth(entry.text, max(1, m.width-2))
		case entryAssistant:
			text = components.AssistantMessageViewWithOptions(entry.content, components.AssistantMessageOptions{
				Width:            max(1, m.width-2),
				ThinkingExpanded: false,
			})
		case entryTool:
			card, ok := m.tools[entry.toolID]
			if ok {
				text = components.ToolCard(card, m.width)
			}
		case entryError:
			text = components.ErrorMessageView(entry.text)
		case entryBash:
			msg := agent.BashExecutionMessage{Command: entry.text, Output: messageText(entry.content)}
			text = components.BashExecutionView(msg, m.width, false)
		case entryBranchSummary:
			text = components.BranchSummaryView(agent.BranchSummaryMessage{Summary: entry.text}, m.width, false)
		case entryCompactionSummary:
			text = components.CompactionSummaryView(agent.CompactionSummaryMessage{Summary: entry.text}, m.width, false)
		case entryCustom:
			text = components.MarkdownView(entry.text, max(1, m.width-2))
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
		case agent.ImageContent:
			label := "[image]"
			if value.Source.MediaType != "" {
				label = "[image: " + value.Source.MediaType + "]"
			}
			body = append(body, label)
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
	card.Details = append(card.Details[:0], raw...)
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

func (m *Model) handleSlash(text string) bool {
	name, args := parseSlash(text)
	if name == "" {
		return false
	}
	switch name {
	case "quit":
		return true
	case "help":
		m.showHelp()
		return false
	case "settings":
		m.overlay = overlayState{
			kind:  "settings",
			title: "Settings",
			items: []components.SelectorItem{
				{Label: "model", Description: m.modelName},
				{Label: "thinking", Description: m.thinking},
				{Label: "queued", Description: fmt.Sprintf("%d", len(m.queued))},
			},
		}
		return false
	case "session":
		m.entries = append(m.entries, chatEntry{kind: entryCustom, text: m.sessionInfo()})
		return false
	case "tree":
		m.showTree()
		return false
	case "new":
		m.entries = nil
		m.tools = make(map[string]components.ToolCardState)
		m.assistantEntries = make(map[string]int)
		m.status = "new session view started"
		return false
	case "login":
		provider := strings.TrimSpace(args)
		if provider == "" {
			provider = "anthropic"
		}
		if m.opts.Agent == nil {
			m.status = "login unavailable: agent is not configured"
			return false
		}
		if err := m.opts.Agent.Login(provider, m.opts.OpenBrowser); err != nil {
			m.entries = append(m.entries, chatEntry{kind: entryError, text: err.Error()})
		} else {
			m.status = "logged in to " + provider
		}
		return false
	case "logout":
		provider := strings.TrimSpace(args)
		if provider == "" {
			provider = "anthropic"
		}
		if m.opts.Logout == nil {
			m.status = "logout unavailable"
			return false
		}
		if err := m.opts.Logout(provider); err != nil {
			m.entries = append(m.entries, chatEntry{kind: entryError, text: err.Error()})
		} else {
			m.status = "logged out from " + provider
		}
		return false
	}
	command, ok := m.slashRegistry.Lookup(name)
	if !ok {
		m.entries = append(m.entries, chatEntry{kind: entryError, text: "unknown command: /" + name})
		return false
	}
	if command.Handler == nil {
		m.status = "/" + name + " is not available in this build"
		return false
	}
	msg, err := command.Handler(context.Background(), args, m.opts.Agent)
	if err != nil {
		m.entries = append(m.entries, chatEntry{kind: entryError, text: err.Error()})
	} else {
		if msg != "" {
			m.status = msg
		}
		m.status = "/" + name + " complete"
		if name == "model" {
			m.modelName = args
		}
		if name == "thinking" {
			m.thinking = args
		}
	}
	return false
}

func parseSlash(text string) (string, string) {
	text = strings.TrimSpace(strings.TrimPrefix(text, "/"))
	if text == "" {
		return "", ""
	}
	name, args, ok := strings.Cut(text, " ")
	if !ok {
		return text, ""
	}
	return name, strings.TrimSpace(args)
}

func (m *Model) showHelp() {
	var lines []string
	lines = append(lines, "## Slash commands")
	for _, command := range m.slashRegistry.Commands() {
		lines = append(lines, fmt.Sprintf("- `/%s` - %s", command.Name, command.Description))
	}
	m.entries = append(m.entries, chatEntry{kind: entryCustom, text: strings.Join(lines, "\n")})
}

func (m *Model) showTree() {
	if m.opts.Agent == nil {
		m.status = "tree unavailable: agent is not configured"
		return
	}
	leaves, err := m.opts.Agent.Tree()
	if err != nil {
		m.entries = append(m.entries, chatEntry{kind: entryError, text: err.Error()})
		return
	}
	items := make([]components.SelectorItem, 0, len(leaves))
	for _, leaf := range leaves {
		desc := leaf.Label
		items = append(items, components.SelectorItem{Label: leaf.ID, Description: desc})
	}
	m.overlay = overlayState{kind: "tree", title: "Session Tree", items: items}
}

func (m Model) sessionInfo() string {
	return fmt.Sprintf("## Session\n\n- Model: `%s`\n- Turn: `%s`\n- Messages: `%d`\n- Queued: `%d`\n- Tokens: `%s` input, `%s` output",
		m.modelName,
		m.turn,
		len(m.entries),
		len(m.queued),
		formatComponentTokens(m.usage.InputTokens),
		formatComponentTokens(m.usage.OutputTokens),
	)
}

func formatComponentTokens(count int) string {
	if count < 1000 {
		return strconv.Itoa(count)
	}
	if count < 10000 {
		return fmt.Sprintf("%.1fk", float64(count)/1000)
	}
	if count < 1000000 {
		return fmt.Sprintf("%dk", count/1000)
	}
	if count < 10000000 {
		return fmt.Sprintf("%.1fM", float64(count)/1000000)
	}
	return fmt.Sprintf("%dM", count/1000000)
}

func (m Model) renderOverlay() string {
	return defaultTheme.panel.Render(components.SelectorView(components.SelectorOpts{
		Title:    m.overlay.title,
		Items:    m.overlay.items,
		Selected: m.overlay.selected,
		Width:    max(20, m.width-4),
		MaxRows:  min(10, max(3, m.height/2)),
	}))
}

func (m Model) handleOverlayKey(key tea.KeyMsg) (bool, Model) {
	switch key.String() {
	case "esc", "ctrl+c":
		m.overlay = overlayState{}
		return true, m
	case "up", "k":
		if len(m.overlay.items) > 0 {
			m.overlay.selected--
			if m.overlay.selected < 0 {
				m.overlay.selected = len(m.overlay.items) - 1
			}
		}
		return true, m
	case "down", "j":
		if len(m.overlay.items) > 0 {
			m.overlay.selected = (m.overlay.selected + 1) % len(m.overlay.items)
		}
		return true, m
	case "enter":
		if m.overlay.kind == "tree" && m.opts.Agent != nil && len(m.overlay.items) > 0 {
			leafID := m.overlay.items[m.overlay.selected].Label
			if err := m.opts.Agent.Move(context.Background(), leafID); err != nil {
				m.entries = append(m.entries, chatEntry{kind: entryError, text: err.Error()})
			}
		}
		m.overlay = overlayState{}
		return true, m
	default:
		return false, m
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
