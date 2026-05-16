package tui

import (
	"regexp"
	"strings"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

func TestAppRendersSyntheticAssistantEvent(t *testing.T) {
	model := New(Options{Model: "claude"})
	model.width = 80
	model.height = 24
	model.applyEvent(agent.MessageStartEvent{MessageID: "m1", Role: agent.RoleAssistant, Model: "claude"})
	model.applyEvent(agent.MessageUpdateEvent{MessageID: "m1", Delta: agent.MessageDelta{TextDelta: "# Hello"}})
	model.refreshHistory(true)
	if got := stripANSIForTest(model.View()); !strings.Contains(got, "Hello") {
		t.Fatalf("view = %q", got)
	}
}

func TestAppSlashCommandDispatch(t *testing.T) {
	runner := agent.NewAgent(agent.LoopConfig{Model: "old"})
	model := New(Options{Model: "old", Agent: runner})
	model.editor.SetValue("/model new-model")
	if quit := model.submitEditor(); quit {
		t.Fatal("unexpected quit")
	}
	if model.modelName != "new-model" {
		t.Fatalf("modelName = %q", model.modelName)
	}
	model.editor.SetValue("/help")
	model.submitEditor()
	model.refreshHistory(true)
	if got := stripANSIForTest(model.View()); !strings.Contains(got, "/model") {
		t.Fatalf("help view = %q", got)
	}
}

func stripANSIForTest(text string) string {
	return regexp.MustCompile(`\x1b(?:\[[0-9;?]*[A-Za-z]|\][^\x07]*(?:\x07|\x1b\\)|_[^\x07]*(?:\x07|\x1b\\))`).ReplaceAllString(text, "")
}
