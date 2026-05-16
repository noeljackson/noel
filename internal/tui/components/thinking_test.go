package components

import (
	"strings"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

func TestThinkingViewCollapsedExpanded(t *testing.T) {
	content := agent.ThinkingContent{Thinking: "private reasoning"}
	if got := stripANSI(ThinkingView(content, 80, false)); got != "[thinking...]" {
		t.Fatalf("collapsed = %q", got)
	}
	if got := stripANSI(ThinkingView(content, 80, true)); !strings.Contains(got, "private reasoning") {
		t.Fatalf("expanded = %q", got)
	}
}
