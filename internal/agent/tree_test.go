package agent_test

import (
	"context"
	"encoding/json"
	"testing"
	"time"

	"github.com/noeljackson/pi/internal/agent"
	"github.com/noeljackson/pi/internal/session"
)

type treeProvider struct {
	blockUntilCancel bool
}

func (p treeProvider) Stream(ctx context.Context, req agent.StreamRequest, emit func(agent.Event)) (*agent.AssistantMessage, error) {
	if p.blockUntilCancel {
		<-ctx.Done()
		return nil, ctx.Err()
	}
	msg := &agent.AssistantMessage{
		Content:    []agent.Content{agent.TextContent{Text: "ok"}},
		StopReason: agent.StopEndTurn,
		Model:      req.Model,
	}
	emit(agent.MessageStartEvent{MessageID: "msg", Role: agent.RoleAssistant, Model: req.Model})
	emit(agent.MessageEndEvent{MessageID: "msg", FinalContent: msg.Content, StopReason: msg.StopReason.String()})
	return msg, nil
}

func TestAgentForkEmitsSessionForkEvent(t *testing.T) {
	sess := newAgentTreeSession(t)
	entryID := appendAgentTreeText(t, sess, "base")
	a := agent.NewAgent(agent.LoopConfig{Provider: treeProvider{}, SessionWriter: sess, Model: "m"})

	var forkEvent agent.SessionForkEvent
	a.Subscribe(func(event agent.Event) {
		if evt, ok := event.(agent.SessionForkEvent); ok {
			forkEvent = evt
		}
	})

	newLeaf, err := a.Fork(context.Background(), entryID)
	if err != nil {
		t.Fatal(err)
	}
	if forkEvent.NewLeafID != newLeaf || forkEvent.FromLeafID != entryID {
		t.Fatalf("fork event = %#v, new leaf %q", forkEvent, newLeaf)
	}
	if sess.LeafID() != newLeaf {
		t.Fatalf("session leaf = %q, want %q", sess.LeafID(), newLeaf)
	}
}

func TestAgentMoveSummarizesAbandonedBranch(t *testing.T) {
	sess := newAgentTreeSession(t)
	first := appendAgentTreeText(t, sess, "first")
	mainLeaf := appendAgentTreeText(t, sess, "main")
	if _, err := sess.ForkAt(first); err != nil {
		t.Fatal(err)
	}
	branchLeaf := appendAgentTreeText(t, sess, "branch")
	summarizer := &agentTreeSummarizer{summary: "abandoned summary"}
	a := agent.NewAgent(agent.LoopConfig{
		Provider:         treeProvider{},
		SessionWriter:    sess,
		Model:            "m",
		BranchSummarizer: summarizer,
	})
	var summaryEvent agent.BranchSummaryEvent
	a.Subscribe(func(event agent.Event) {
		if evt, ok := event.(agent.BranchSummaryEvent); ok {
			summaryEvent = evt
		}
	})

	if err := a.Move(context.Background(), mainLeaf); err != nil {
		t.Fatal(err)
	}
	if summarizer.req.LeafID != branchLeaf {
		t.Fatalf("summarized leaf = %q, want %q", summarizer.req.LeafID, branchLeaf)
	}
	leaves, err := sess.Tree()
	if err != nil {
		t.Fatal(err)
	}
	foundSummary := false
	for _, leaf := range leaves {
		path, err := sess.PathToLeaf(leaf.ID)
		if err != nil {
			t.Fatal(err)
		}
		for _, record := range path {
			if record.Type != session.RecordTypeBranchSummary {
				continue
			}
			var payload struct {
				Summary string `json:"summary"`
			}
			if err := json.Unmarshal(record.Payload, &payload); err != nil {
				t.Fatal(err)
			}
			if payload.Summary == "abandoned summary" {
				foundSummary = true
			}
		}
	}
	if !foundSummary {
		t.Fatal("branch summary was not recorded on abandoned branch")
	}
	if summaryEvent.LeafID != branchLeaf || summaryEvent.Summary != "abandoned summary" {
		t.Fatalf("summary event = %#v", summaryEvent)
	}
	if sess.LeafID() != mainLeaf {
		t.Fatalf("session leaf = %q, want %q", sess.LeafID(), mainLeaf)
	}
}

func TestAgentMoveClearsQueue(t *testing.T) {
	sess := newAgentTreeSession(t)
	targetLeaf := appendAgentTreeText(t, sess, "base")
	a := agent.NewAgent(agent.LoopConfig{Provider: treeProvider{blockUntilCancel: true}, SessionWriter: sess, Model: "m"})
	if err := a.Prompt(context.Background(), "running"); err != nil {
		t.Fatal(err)
	}
	waitForAgentTree(t, func() bool { return a.State() == agent.AgentStreaming })
	if err := a.Prompt(context.Background(), "queued"); err != nil {
		t.Fatal(err)
	}
	waitForAgentTree(t, func() bool { return len(a.Queue()) == 1 })

	if err := a.Move(context.Background(), targetLeaf); err != nil {
		t.Fatal(err)
	}
	if got := len(a.Queue()); got != 0 {
		t.Fatalf("queue length = %d, want 0", got)
	}
	if got := a.StreamingMessage(); got != nil {
		t.Fatalf("streaming message = %#v, want nil", got)
	}
}

type agentTreeSummarizer struct {
	req     session.BranchSummaryRequest
	summary string
}

func (s *agentTreeSummarizer) Summarize(_ context.Context, req session.BranchSummaryRequest) (string, error) {
	s.req = req
	return s.summary, nil
}

func newAgentTreeSession(t *testing.T) *session.Session {
	t.Helper()
	store := session.NewJSONLStore(t.TempDir())
	sess, err := store.Create("/tmp/project")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := sess.Close(); err != nil {
			t.Fatal(err)
		}
	})
	return sess
}

func appendAgentTreeText(t *testing.T, sess *session.Session, text string) string {
	t.Helper()
	if err := sess.AppendMessage(agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: text}}}); err != nil {
		t.Fatal(err)
	}
	return sess.LeafID()
}

func waitForAgentTree(t *testing.T, pred func() bool) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if pred() {
			return
		}
		time.Sleep(time.Millisecond)
	}
	t.Fatal("condition not met")
}
