package mock

import (
	"testing"
	"time"

	"github.com/noeljackson/pi/internal/agent"
)

func TestSourceEmitsExpectedEventTypes(t *testing.T) {
	seen := make(map[string]bool)
	for event := range Source(time.Duration(0)) {
		switch event.(type) {
		case agent.AgentStartEvent:
			seen["agent_start"] = true
		case agent.TurnStartEvent:
			seen["turn_start"] = true
		case agent.MessageStartEvent:
			seen["message_start"] = true
		case agent.MessageUpdateEvent:
			seen["message_update"] = true
		case agent.ToolExecutionStartEvent:
			seen["tool_execution_start"] = true
		case agent.ToolExecutionUpdateEvent:
			seen["tool_execution_update"] = true
		case agent.ToolExecutionEndEvent:
			seen["tool_execution_end"] = true
		case agent.MessageEndEvent:
			seen["message_end"] = true
		case agent.TurnEndEvent:
			seen["turn_end"] = true
		case agent.AgentEndEvent:
			seen["agent_end"] = true
		}
	}

	for _, name := range []string{
		"agent_start",
		"turn_start",
		"message_start",
		"message_update",
		"tool_execution_start",
		"tool_execution_update",
		"tool_execution_end",
		"message_end",
		"turn_end",
		"agent_end",
	} {
		if !seen[name] {
			t.Fatalf("missing event type %s", name)
		}
	}
}
