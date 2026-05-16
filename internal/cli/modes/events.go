package modes

import (
	"context"
	"encoding/json"
	"fmt"

	"github.com/noeljackson/pi/internal/agent"
	"github.com/noeljackson/pi/internal/cli"
)

type runner interface {
	Prompt(ctx context.Context, text string) error
	Continue(ctx context.Context) error
	WaitForIdle(ctx context.Context) error
	LastError() error
	Subscribe(func(agent.Event)) func()
	SetModel(string) error
	SetThinking(string) error
	ActivateTools([]string) error
	DeactivateTools([]string) error
	Abort()
	State() agent.AgentState
	Queue() []agent.QueuedSubmission
}

type eventEnvelope map[string]any

// ApplyOptions applies one-shot CLI overrides to an agent runner.
func ApplyOptions(runner runner, opts cli.Options) error {
	if opts.Model != "" {
		if err := runner.SetModel(opts.Model); err != nil {
			return err
		}
	}
	if opts.Thinking != "" {
		if err := runner.SetThinking(opts.Thinking); err != nil {
			return err
		}
	}
	if opts.Tools.NoTools {
		return runner.ActivateTools([]string{})
	}
	if len(opts.Tools.Allow) > 0 {
		if err := runner.ActivateTools(opts.Tools.Allow); err != nil {
			return err
		}
	}
	if len(opts.Tools.Deny) > 0 {
		if err := runner.DeactivateTools(opts.Tools.Deny); err != nil {
			return err
		}
	}
	return nil
}

func runPrompt(ctx context.Context, runner runner, opts cli.Options) error {
	if opts.Session.Continue && opts.Prompt == "" {
		if err := runner.Continue(ctx); err != nil {
			return err
		}
	} else if opts.Prompt != "" {
		if err := runner.Prompt(ctx, opts.Prompt); err != nil {
			return err
		}
	} else {
		return fmt.Errorf("prompt is required")
	}
	if err := runner.WaitForIdle(ctx); err != nil {
		return err
	}
	return runner.LastError()
}

// MarshalEvent serializes a Go agent event as a JSONL-compatible TS-style event.
func MarshalEvent(event agent.Event) ([]byte, error) {
	return json.Marshal(eventToEnvelope(event))
}

func eventToEnvelope(event agent.Event) eventEnvelope {
	switch evt := event.(type) {
	case agent.AgentStartEvent:
		return eventEnvelope{"type": "agent_start", "sessionId": evt.SessionID}
	case agent.AgentEndEvent:
		envelope := eventEnvelope{"type": "agent_end", "reason": evt.Reason}
		if evt.Err != nil {
			envelope["error"] = evt.Err.Error()
		}
		return envelope
	case agent.TurnStartEvent:
		return eventEnvelope{"type": "turn_start", "turnId": evt.TurnID}
	case agent.TurnEndEvent:
		return eventEnvelope{
			"type":             "turn_end",
			"turnId":           evt.TurnID,
			"toolCallsPending": evt.ToolCallsPending,
			"toolResults":      toolResultsToJSON(evt.ToolResults),
		}
	case agent.MessageStartEvent:
		return eventEnvelope{
			"type": "message_start",
			"message": eventEnvelope{
				"role":    evt.Role,
				"model":   evt.Model,
				"id":      evt.MessageID,
				"content": []any{},
			},
		}
	case agent.MessageUpdateEvent:
		envelope := eventEnvelope{
			"type":    "message_update",
			"message": eventEnvelope{"id": evt.MessageID, "role": agent.RoleAssistant},
		}
		if evt.Delta.TextDelta != "" {
			envelope["assistantMessageEvent"] = eventEnvelope{"type": "text_delta", "delta": evt.Delta.TextDelta}
		} else if evt.Delta.ThinkingDelta != "" {
			envelope["assistantMessageEvent"] = eventEnvelope{"type": "thinking_delta", "delta": evt.Delta.ThinkingDelta}
		} else if evt.Delta.RedactedThinkingDelta != "" {
			envelope["assistantMessageEvent"] = eventEnvelope{"type": "redacted_thinking_delta", "delta": evt.Delta.RedactedThinkingDelta}
		} else if evt.Delta.ToolUseDelta != nil {
			envelope["assistantMessageEvent"] = eventEnvelope{
				"type":               "tool_use_delta",
				"id":                 evt.Delta.ToolUseDelta.ID,
				"name":               evt.Delta.ToolUseDelta.Name,
				"inputJsonPartial":   evt.Delta.ToolUseDelta.InputJSONPartial,
				"inputJSONPartial":   evt.Delta.ToolUseDelta.InputJSONPartial,
				"input_json_partial": evt.Delta.ToolUseDelta.InputJSONPartial,
			}
		}
		if evt.Delta.Usage != nil {
			envelope["usage"] = usageToJSON(*evt.Delta.Usage)
		}
		return envelope
	case agent.MessageEndEvent:
		return eventEnvelope{
			"type": "message_end",
			"message": eventEnvelope{
				"id":         evt.MessageID,
				"role":       agent.RoleAssistant,
				"content":    contentToJSON(evt.FinalContent),
				"stopReason": evt.StopReason,
				"usage":      usageToJSON(evt.Usage),
			},
		}
	case agent.ToolExecutionStartEvent:
		return eventEnvelope{"type": "tool_execution_start", "toolCallId": evt.CallID, "toolName": evt.Name, "args": rawOrEmpty(evt.Input)}
	case agent.ToolExecutionUpdateEvent:
		return eventEnvelope{"type": "tool_execution_update", "toolCallId": evt.CallID, "partialResult": rawOrEmpty(evt.Partial)}
	case agent.ToolExecutionEndEvent:
		envelope := eventEnvelope{
			"type":       "tool_execution_end",
			"toolCallId": evt.CallID,
			"result":     toolResultToJSON(evt.Result),
			"isError":    evt.Result.IsError,
		}
		if evt.Err != nil {
			envelope["error"] = evt.Err.Error()
			envelope["isError"] = true
		}
		return envelope
	case agent.SessionForkEvent:
		return eventEnvelope{"type": "session_fork", "newLeafId": evt.NewLeafID, "fromLeafId": evt.FromLeafID}
	case agent.SessionMoveEvent:
		return eventEnvelope{"type": "session_move", "fromLeafId": evt.FromLeafID, "toLeafId": evt.ToLeafID}
	case agent.BranchSummaryEvent:
		return eventEnvelope{"type": "branch_summary", "leafId": evt.LeafID, "summary": evt.Summary}
	case agent.ResourcesReloadEvent:
		return eventEnvelope{"type": "resources_reload", "diagnostics": evt.Diagnostics}
	default:
		return eventEnvelope{"type": "unknown"}
	}
}

func contentToJSON(content []agent.Content) []any {
	out := make([]any, 0, len(content))
	for _, block := range content {
		switch value := block.(type) {
		case agent.TextContent:
			out = append(out, eventEnvelope{"type": "text", "text": value.Text})
		case agent.ThinkingContent:
			out = append(out, eventEnvelope{"type": "thinking", "thinking": value.Thinking, "signature": value.Signature})
		case agent.ToolUseContent:
			out = append(out, eventEnvelope{"type": "tool_use", "id": value.ID, "name": value.Name, "input": rawOrEmpty(value.Input)})
		case agent.ToolResultContent:
			out = append(out, eventEnvelope{"type": "tool_result", "toolUseId": value.ToolUseID, "content": contentToJSON(value.Content), "isError": value.IsError})
		case agent.RedactedThinkingContent:
			out = append(out, eventEnvelope{"type": "redacted_thinking", "data": value.Data})
		case agent.ImageContent:
			out = append(out, eventEnvelope{"type": "image", "source": value.Source})
		}
	}
	return out
}

func toolResultsToJSON(results []agent.ToolResult) []any {
	out := make([]any, 0, len(results))
	for _, result := range results {
		out = append(out, toolResultToJSON(result))
	}
	return out
}

func toolResultToJSON(result agent.ToolResult) eventEnvelope {
	return eventEnvelope{
		"toolUseId": result.ToolUseID,
		"content":   contentToJSON(result.Content),
		"details":   rawOrEmpty(result.Details),
		"isError":   result.IsError,
		"terminate": result.Terminate,
	}
}

func usageToJSON(usage agent.Usage) eventEnvelope {
	return eventEnvelope{
		"inputTokens":              usage.InputTokens,
		"outputTokens":             usage.OutputTokens,
		"cacheCreationInputTokens": usage.CacheCreationInputTokens,
		"cacheReadInputTokens":     usage.CacheReadInputTokens,
		"totalTokens":              usage.TotalTokens,
		"cacheWriteTokens":         usage.CacheWriteTokens,
	}
}

func rawOrEmpty(raw json.RawMessage) any {
	if len(raw) == 0 {
		return map[string]any{}
	}
	var value any
	if err := json.Unmarshal(raw, &value); err != nil {
		return string(raw)
	}
	return value
}
