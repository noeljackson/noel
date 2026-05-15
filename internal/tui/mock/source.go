package mock

import (
	"encoding/json"
	"time"

	"github.com/noeljackson/pi/internal/agent"
)

func Events() []agent.Event {
	return []agent.Event{
		agent.AgentStartEvent{SessionID: "demo-session"},
		agent.TurnStartEvent{TurnID: "demo-turn"},
		agent.MessageStartEvent{
			TurnID:    "demo-turn",
			MessageID: "demo-message",
			Role:      agent.RoleAssistant,
			Model:     "claude-sonnet-4-6",
		},
		agent.MessageUpdateEvent{
			MessageID: "demo-message",
			Delta:     agent.MessageDelta{TextDelta: "I will inspect the workspace and run a tiny shell command.\n\n"},
		},
		agent.MessageUpdateEvent{
			MessageID: "demo-message",
			Delta: agent.MessageDelta{
				ToolUseDelta: &agent.ToolUseDelta{
					ID:               "tool-bash-1",
					Name:             "bash",
					InputJSONPartial: `{"cmd":"printf 'hello from pi\n'"}`,
				},
			},
		},
		agent.ToolExecutionStartEvent{
			CallID: "tool-bash-1",
			Name:   "bash",
			Input:  json.RawMessage(`{"cmd":"printf 'hello from pi\n'"}`),
		},
		agent.ToolExecutionUpdateEvent{
			CallID:  "tool-bash-1",
			Partial: json.RawMessage(`{"stdout":"hello from pi\n"}`),
		},
		agent.ToolExecutionUpdateEvent{
			CallID:  "tool-bash-1",
			Partial: json.RawMessage(`{"stdout":"done\n"}`),
		},
		agent.ToolExecutionEndEvent{
			CallID: "tool-bash-1",
			Result: agent.ToolResult{
				ToolUseID: "tool-bash-1",
				Content: []agent.Content{
					agent.TextContent{Text: "hello from pi\ndone\n"},
				},
				Details: json.RawMessage(`{"exitCode":0}`),
			},
		},
		agent.MessageEndEvent{
			MessageID: "demo-message",
			FinalContent: []agent.Content{
				agent.TextContent{Text: "I will inspect the workspace and run a tiny shell command.\n\nThe command completed successfully."},
				agent.ToolUseContent{
					ID:    "tool-bash-1",
					Name:  "bash",
					Input: json.RawMessage(`{"cmd":"printf 'hello from pi\n'"}`),
				},
			},
			StopReason: "end_turn",
			Usage: agent.Usage{
				InputTokens:  1200,
				OutputTokens: 3400,
			},
		},
		agent.TurnEndEvent{TurnID: "demo-turn"},
		agent.AgentEndEvent{Reason: "complete"},
	}
}

func Source(delay time.Duration) <-chan agent.Event {
	events := Events()
	ch := make(chan agent.Event)
	go func() {
		defer close(ch)
		for _, event := range events {
			if delay > 0 {
				time.Sleep(delay)
			}
			ch <- event
		}
	}()
	return ch
}
