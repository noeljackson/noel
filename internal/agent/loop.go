package agent

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"sync"

	authstore "github.com/noeljackson/pi/internal/auth"
	"github.com/noeljackson/pi/internal/resources"
	"github.com/noeljackson/pi/internal/session/schema"
)

const defaultMaxTurns = 100

type Provider interface {
	Stream(ctx context.Context, req StreamRequest, emit func(Event)) (*AssistantMessage, error)
}

type StreamRequest struct {
	Model       string
	Messages    []Message
	System      string
	Tools       []Tool
	MaxTokens   int
	Thinking    *ThinkingConfig
	Metadata    map[string]string
	ToolChoice  *ToolChoice
	Temperature *float64
	// FineGrainedToolStreaming asks Anthropic for legacy fine-grained tool input
	// deltas when eager_input_streaming is not available.
	FineGrainedToolStreaming bool
}

// ThinkingConfig describes provider reasoning controls.
type ThinkingConfig struct {
	Enabled      bool
	Adaptive     bool
	BudgetTokens int
	Display      string
	Effort       string
}

// ToolChoice describes provider tool selection controls.
type ToolChoice struct {
	Type string
	Name string
}

type Compactor interface {
	MaybeCompact(ctx context.Context, messages []Message, system string) ([]Message, error)
}

type ResourceLoader interface {
	Load() (resources.Resources, error)
}

type LoopConfig struct {
	Provider         Provider
	Tools            ToolRegistry
	Model            string
	Thinking         string
	System           string
	SystemPrompt     string
	Resources        resources.Resources
	ResourceLoader   ResourceLoader
	MaxTokens        int
	MaxTurns         int
	SessionWriter    SessionWriter
	SessionID        string
	Compactor        Compactor
	BranchSummarizer schema.BranchSummarizer
	AuthStore        *authstore.Store

	PrepareNextTurn     func(ctx context.Context, messages []Message) (NextTurnDirective, error)
	ShouldStopAfterTurn func(turn int, last *AssistantMessage) bool
	BeforeToolCall      func(ctx context.Context, call ToolUseContent) error
	AfterToolCall       func(ctx context.Context, call ToolUseContent, result ToolResult)
	ActiveTools         []string
}

type NextTurnDirective struct {
	InsertMessages []Message
	NewModel       string
	NewThinking    string
	Stop           bool
}

type SessionWriter interface {
	AppendMessage(Message) error
	AppendEvent(Event) error
}

func Run(ctx context.Context, cfg LoopConfig, initial UserMessage, emit func(Event)) (*AssistantMessage, error) {
	return run(ctx, cfg, []Message{initial}, true, emit)
}

func Continue(ctx context.Context, cfg LoopConfig, messages []Message, emit func(Event)) (*AssistantMessage, error) {
	if len(messages) == 0 {
		return nil, errors.New("cannot continue empty session")
	}
	if messages[len(messages)-1].Role() == RoleAssistant {
		return nil, errors.New("cannot continue from assistant message")
	}
	return run(ctx, cfg, messages, false, emit)
}

func run(ctx context.Context, cfg LoopConfig, messages []Message, appendInitial bool, emit func(Event)) (*AssistantMessage, error) {
	if cfg.Provider == nil {
		return nil, errors.New("agent provider is required")
	}
	if emit == nil {
		emit = func(Event) {}
	}
	emitEvent := func(event Event) error {
		emit(event)
		if cfg.SessionWriter != nil {
			return cfg.SessionWriter.AppendEvent(event)
		}
		return nil
	}
	maxTurns := cfg.MaxTurns
	if maxTurns == 0 {
		maxTurns = defaultMaxTurns
	}

	if err := emitEvent(AgentStartEvent{}); err != nil {
		return nil, err
	}
	var finalAssistant *AssistantMessage
	var finalErr error
	defer func() {
		reason := "complete"
		if finalErr != nil {
			reason = "error"
		}
		_ = emitEvent(AgentEndEvent{Reason: reason, Err: finalErr})
	}()

	if appendInitial && cfg.SessionWriter != nil {
		if err := cfg.SessionWriter.AppendMessage(messages[0]); err != nil {
			finalErr = err
			return nil, err
		}
	}

	for turn := 1; turn <= maxTurns; turn++ {
		turnID := fmt.Sprintf("turn-%d", turn)
		if err := emitEvent(TurnStartEvent{TurnID: turnID}); err != nil {
			finalErr = err
			return nil, err
		}
		streamMessages := messages
		if cfg.Compactor != nil {
			compacted, err := cfg.Compactor.MaybeCompact(ctx, messages, effectiveSystemPrompt(cfg))
			if err != nil {
				finalErr = err
				return nil, err
			}
			streamMessages = compacted
		}
		assistant, err := cfg.Provider.Stream(ctx, StreamRequest{
			Model:     cfg.Model,
			Messages:  ConvertToLLM(streamMessages),
			System:    effectiveSystemPrompt(cfg),
			Tools:     toolsFromConfig(cfg),
			MaxTokens: cfg.MaxTokens,
		}, func(event Event) {
			if start, ok := event.(MessageStartEvent); ok {
				start.TurnID = turnID
				event = start
			}
			_ = emitEvent(event)
		})
		if err != nil {
			finalErr = err
			return nil, err
		}

		messages = append(messages, *assistant)
		if cfg.SessionWriter != nil {
			if err := cfg.SessionWriter.AppendMessage(*assistant); err != nil {
				finalErr = err
				return nil, err
			}
		}

		toolCalls := toolUses(assistant.Content)
		hasPendingToolCalls := assistant.StopReason == StopToolUse && len(toolCalls) > 0
		var toolResults []ToolResult
		terminate := false
		if hasPendingToolCalls {
			toolResults = executeToolCalls(ctx, cfg, toolCalls, emitEvent)
			toolResultMessage := ToolResultMessage{Results: toolResults}
			messages = append(messages, toolResultMessage)
			if cfg.SessionWriter != nil {
				if err := cfg.SessionWriter.AppendMessage(toolResultMessage); err != nil {
					finalErr = err
					return nil, err
				}
			}
			terminate = hasTerminatingResult(toolResults)
		}

		if err := emitEvent(TurnEndEvent{TurnID: turnID, ToolCallsPending: hasPendingToolCalls && !terminate, ToolResults: toolResults}); err != nil {
			finalErr = err
			return nil, err
		}

		if cfg.PrepareNextTurn != nil {
			directive, err := cfg.PrepareNextTurn(ctx, append([]Message(nil), messages...))
			if err != nil {
				finalErr = err
				return nil, err
			}
			if len(directive.InsertMessages) > 0 {
				for _, message := range directive.InsertMessages {
					messages = append(messages, message)
					if cfg.SessionWriter != nil {
						if err := cfg.SessionWriter.AppendMessage(message); err != nil {
							finalErr = err
							return nil, err
						}
					}
				}
			}
			if directive.NewModel != "" {
				cfg.Model = directive.NewModel
			}
			if directive.NewThinking != "" {
				cfg.Thinking = directive.NewThinking
			}
			if directive.Stop {
				finalAssistant = assistant
				return assistant, nil
			}
		}

		if cfg.ShouldStopAfterTurn != nil && cfg.ShouldStopAfterTurn(turn, assistant) {
			finalAssistant = assistant
			return assistant, nil
		}
		if terminate || !hasPendingToolCalls {
			finalAssistant = assistant
			return assistant, nil
		}
		if hasTerminatingToolResult(toolResults) {
			return assistant, nil
		}
	}

	err := fmt.Errorf("agent exceeded max turns: %d", maxTurns)
	finalErr = err
	return finalAssistant, err
}

func effectiveSystemPrompt(cfg LoopConfig) string {
	base := cfg.SystemPrompt
	if base == "" {
		base = cfg.System
	}
	if len(cfg.Resources.ContextFiles) == 0 && len(cfg.Resources.Skills) == 0 {
		return base
	}
	return (&resources.SystemPromptBuilder{
		BasePrompt: base,
		Context:    cfg.Resources.ContextFiles,
		Skills:     cfg.Resources.Skills,
	}).Build()
}

func toolsFromConfig(cfg LoopConfig) []Tool {
	if cfg.Tools == nil {
		return nil
	}
	all := cfg.Tools.All()
	if len(cfg.ActiveTools) == 0 {
		return all
	}
	active := make(map[string]struct{}, len(cfg.ActiveTools))
	for _, name := range cfg.ActiveTools {
		active[name] = struct{}{}
	}
	tools := make([]Tool, 0, len(all))
	for _, tool := range all {
		if _, ok := active[tool.Name()]; ok {
			tools = append(tools, tool)
		}
	}
	return tools
}

func toolUses(content []Content) []ToolUseContent {
	uses := make([]ToolUseContent, 0)
	for _, block := range content {
		if toolUse, ok := block.(ToolUseContent); ok {
			uses = append(uses, toolUse)
		}
	}
	return uses
}

func executeToolCalls(ctx context.Context, cfg LoopConfig, calls []ToolUseContent, emit func(Event) error) []ToolResult {
	results := make([]ToolResult, len(calls))

	for i, call := range calls {
		tool, ok := lookupTool(cfg.Tools, call.Name)
		if !ok {
			results[i] = missingToolResult(call)
			continue
		}
		if !tool.ParallelSafe() {
			results[i] = executeToolCall(ctx, cfg, tool, call, emit)
		}
	}

	var parallel sync.WaitGroup
	for i, call := range calls {
		tool, ok := lookupTool(cfg.Tools, call.Name)
		if !ok || !tool.ParallelSafe() {
			continue
		}
		parallel.Add(1)
		go func(index int, currentTool Tool, currentCall ToolUseContent) {
			defer parallel.Done()
			results[index] = executeToolCall(ctx, cfg, currentTool, currentCall, emit)
		}(i, tool, call)
	}
	parallel.Wait()
	return results
}

func lookupTool(registry ToolRegistry, name string) (Tool, bool) {
	if registry == nil {
		return nil, false
	}
	return registry.Get(name)
}

func executeToolCall(ctx context.Context, cfg LoopConfig, tool Tool, call ToolUseContent, emit func(Event) error) ToolResult {
	_ = emit(ToolExecutionStartEvent{CallID: call.ID, Name: call.Name, Input: call.Input})
	if cfg.BeforeToolCall != nil {
		if err := cfg.BeforeToolCall(ctx, call); err != nil {
			result := ToolResult{
				ToolUseID: call.ID,
				Content:   []Content{TextContent{Text: err.Error()}},
				IsError:   true,
			}
			_ = emit(ToolExecutionEndEvent{CallID: call.ID, Result: result, Err: err})
			return result
		}
	}
	input, err := ValidateAndCoerceToolArguments(tool.Schema(), call.Input)
	var result ToolResult
	if err == nil {
		result, err = tool.Execute(ctx, input, ToolCallContext{
			CallID:    call.ID,
			SessionID: cfg.SessionID,
			Cwd:       currentWorkingDirectory(),
			Model:     cfg.Model,
			OnUpdate: func(partial json.RawMessage) {
				_ = emit(ToolExecutionUpdateEvent{CallID: call.ID, Partial: partial})
			},
			Logger: slog.Default(),
		})
	}
	if err != nil {
		result = ToolResult{
			ToolUseID: call.ID,
			Content:   []Content{TextContent{Text: err.Error()}},
			IsError:   true,
		}
	}
	if result.ToolUseID == "" {
		result.ToolUseID = call.ID
	}
	if cfg.AfterToolCall != nil {
		cfg.AfterToolCall(ctx, call, result)
	}
	_ = emit(ToolExecutionEndEvent{CallID: call.ID, Result: result, Err: err})
	return result
}

func hasTerminatingToolResult(results []ToolResult) bool {
	for _, result := range results {
		if result.Terminate {
			return true
		}
	}
	return false
}

func missingToolResult(call ToolUseContent) ToolResult {
	return ToolResult{
		ToolUseID: call.ID,
		Content:   []Content{TextContent{Text: fmt.Sprintf("Tool %s not found", call.Name)}},
		IsError:   true,
	}
}

func hasTerminatingResult(results []ToolResult) bool {
	for _, result := range results {
		if result.Terminate {
			return true
		}
	}
	return false
}

func currentWorkingDirectory() string {
	cwd, err := os.Getwd()
	if err != nil {
		return ""
	}
	return cwd
}
