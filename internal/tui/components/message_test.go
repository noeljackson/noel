package components

import (
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

func TestMessageViewsRenderNonEmpty(t *testing.T) {
	if got := UserMessageView("hello"); got == "" {
		t.Fatal("expected non-empty user message render")
	}

	got := AssistantMessageView([]agent.Content{
		agent.ThinkingContent{Thinking: "thinking"},
		agent.TextContent{Text: "answer"},
	})
	if got == "" {
		t.Fatal("expected non-empty assistant message render")
	}
}
