package agent

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"sync"
)

const defaultMaxTurns = 100

type Provider interface {
	Stream(ctx context.Context, req StreamRequest, emit func(Event)) (*AssistantMessage, error)
}

type StreamRequest struct {
	Model     string
	Messages  []Message
	System    string
	Tools     []Tool
	MaxTokens int
}

type LoopConfig struct {
	Provider      Provider
	Tools         ToolRegistry
	Model         string
	System        string
	MaxTokens     int
	MaxTurns      int
	SessionWriter SessionWriter
	Compactor     Compactor
}

type SessionWriter interface {
	AppendMessage(Message) error
	AppendEvent(Event) error
}

type Compactor func(ctx context.Context, messages []Message, system string) ([]Message, error)

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
	maxTurns := cfg.MaxTurns
	if maxTurns == 0 {
		maxTurns = defaultMaxTurns
	}

	if appendInitial && cfg.SessionWriter != nil {
		if err := cfg.SessionWriter.AppendMessage(messages[0]); err != nil {
			return nil, err
		}
	}

	for turn := 1; turn <= maxTurns; turn++ {
		turnID := fmt.Sprintf("turn-%d", turn)
		emit(TurnStartEvent{TurnID: turnID})
		if cfg.Compactor != nil {
			compacted, err := cfg.Compactor(ctx, messages, cfg.System)
			if err != nil {
				return nil, err
			}
			messages = compacted
		}
		assistant, err := cfg.Provider.Stream(ctx, StreamRequest{
			Model:     cfg.Model,
			Messages:  messages,
			System:    cfg.System,
			Tools:     toolsFromRegistry(cfg.Tools),
			MaxTokens: cfg.MaxTokens,
		}, func(event Event) {
			if start, ok := event.(MessageStartEvent); ok {
				start.TurnID = turnID
				event = start
			}
			emit(event)
			if cfg.SessionWriter != nil {
				_ = cfg.SessionWriter.AppendEvent(event)
			}
		})
		if err != nil {
			return nil, err
		}

		messages = append(messages, *assistant)
		if cfg.SessionWriter != nil {
			if err := cfg.SessionWriter.AppendMessage(*assistant); err != nil {
				return nil, err
			}
		}

		toolCalls := toolUses(assistant.Content)
		hasPendingToolCalls := assistant.StopReason == "tool_use" && len(toolCalls) > 0
		emit(TurnEndEvent{TurnID: turnID, ToolCallsPending: hasPendingToolCalls})
		if !hasPendingToolCalls {
			return assistant, nil
		}

		toolResultMessage := ToolResultMessage{Results: executeToolCalls(ctx, cfg.Tools, toolCalls, emit)}
		messages = append(messages, toolResultMessage)
		if cfg.SessionWriter != nil {
			if err := cfg.SessionWriter.AppendMessage(toolResultMessage); err != nil {
				return nil, err
			}
		}
	}

	return nil, fmt.Errorf("agent exceeded max turns: %d", maxTurns)
}

func toolsFromRegistry(registry ToolRegistry) []Tool {
	if registry == nil {
		return nil
	}
	return registry.All()
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

func executeToolCalls(ctx context.Context, registry ToolRegistry, calls []ToolUseContent, emit func(Event)) []ToolResult {
	results := make([]ToolResult, len(calls))

	for i, call := range calls {
		tool, ok := lookupTool(registry, call.Name)
		if !ok {
			results[i] = missingToolResult(call)
			continue
		}
		if !tool.ParallelSafe() {
			results[i] = executeToolCall(ctx, tool, call, emit)
		}
	}

	var parallel sync.WaitGroup
	for i, call := range calls {
		tool, ok := lookupTool(registry, call.Name)
		if !ok || !tool.ParallelSafe() {
			continue
		}
		parallel.Add(1)
		go func(index int, currentTool Tool, currentCall ToolUseContent) {
			defer parallel.Done()
			results[index] = executeToolCall(ctx, currentTool, currentCall, emit)
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

func executeToolCall(ctx context.Context, tool Tool, call ToolUseContent, emit func(Event)) ToolResult {
	emit(ToolExecutionStartEvent{CallID: call.ID, Name: call.Name, Input: call.Input})
	result, err := tool.Execute(ctx, call.Input, ToolCallContext{
		CallID: call.ID,
		Cwd:    currentWorkingDirectory(),
		OnUpdate: func(partial json.RawMessage) {
			emit(ToolExecutionUpdateEvent{CallID: call.ID, Partial: partial})
		},
		Logger: slog.Default(),
	})
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
	emit(ToolExecutionEndEvent{CallID: call.ID, Result: result, Err: err})
	return result
}

func missingToolResult(call ToolUseContent) ToolResult {
	return ToolResult{
		ToolUseID: call.ID,
		Content:   []Content{TextContent{Text: fmt.Sprintf("Tool %s not found", call.Name)}},
		IsError:   true,
	}
}

func currentWorkingDirectory() string {
	cwd, err := os.Getwd()
	if err != nil {
		return ""
	}
	return cwd
}
