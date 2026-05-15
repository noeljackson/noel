package session

import (
	"context"
	"reflect"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

func TestForkAtCreatesLeafWithParent(t *testing.T) {
	sess := newTreeTestSession(t)
	first := appendTreeText(t, sess, "first")
	oldLeaf := appendTreeText(t, sess, "second")

	newLeaf, err := sess.ForkAt(first)
	if err != nil {
		t.Fatal(err)
	}
	if newLeaf == oldLeaf || newLeaf == first {
		t.Fatalf("new leaf id %q should be distinct from existing entries", newLeaf)
	}

	leaves := mustTree(t, sess)
	got := leafParents(leaves)
	want := map[string]string{oldLeaf: first, newLeaf: first}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("leaf parents mismatch\n got: %#v\nwant: %#v", got, want)
	}
}

func TestForkBeforeExcludesNamedEntry(t *testing.T) {
	sess := newTreeTestSession(t)
	appendTreeText(t, sess, "first")
	second := appendTreeText(t, sess, "second")

	newLeaf, err := sess.ForkBefore(second)
	if err != nil {
		t.Fatal(err)
	}
	if sess.LeafID() != newLeaf {
		t.Fatalf("current leaf = %q, want %q", sess.LeafID(), newLeaf)
	}
	path, err := sess.PathToCurrentLeaf()
	if err != nil {
		t.Fatal(err)
	}
	assertPathTexts(t, path, []string{"first"})
}

func TestMoveToSwitchesCurrentPath(t *testing.T) {
	sess := newTreeTestSession(t)
	first := appendTreeText(t, sess, "first")
	mainLeaf := appendTreeText(t, sess, "main")
	if _, err := sess.ForkAt(first); err != nil {
		t.Fatal(err)
	}
	branchLeaf := appendTreeText(t, sess, "branch")

	if err := sess.MoveTo(mainLeaf); err != nil {
		t.Fatal(err)
	}
	path, err := sess.PathToCurrentLeaf()
	if err != nil {
		t.Fatal(err)
	}
	assertPathTexts(t, path, []string{"first", "main"})

	if err := sess.MoveTo(branchLeaf); err != nil {
		t.Fatal(err)
	}
	path, err = sess.PathToCurrentLeaf()
	if err != nil {
		t.Fatal(err)
	}
	assertPathTexts(t, path, []string{"first", "branch"})
}

func TestTreeTopologicalOrder(t *testing.T) {
	sess := newTreeTestSession(t)
	first := appendTreeText(t, sess, "first")
	mainLeaf := appendTreeText(t, sess, "main")
	forkLeaf, err := sess.ForkAt(first)
	if err != nil {
		t.Fatal(err)
	}

	leaves := mustTree(t, sess)
	gotIDs := []string{leaves[0].ID, leaves[1].ID}
	wantIDs := []string{mainLeaf, forkLeaf}
	if !reflect.DeepEqual(gotIDs, wantIDs) {
		t.Fatalf("leaf order mismatch\n got: %#v\nwant: %#v", gotIDs, wantIDs)
	}
	for _, leaf := range leaves {
		if leaf.ParentID != first {
			t.Fatalf("leaf %s parent = %q, want %q", leaf.ID, leaf.ParentID, first)
		}
	}
}

func TestSetLeafLabelRoundTrips(t *testing.T) {
	dir := t.TempDir()
	store := NewJSONLStore(dir)
	sess, err := store.Create("/tmp/project")
	if err != nil {
		t.Fatal(err)
	}
	leaf := appendTreeText(t, sess, "target")
	if err := sess.SetLeafLabel(leaf, "work branch"); err != nil {
		t.Fatal(err)
	}
	if err := sess.Close(); err != nil {
		t.Fatal(err)
	}

	reopened, err := store.Open(sess.ID())
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	leaves := mustTree(t, reopened)
	if len(leaves) != 1 {
		t.Fatalf("leaf count = %d, want 1", len(leaves))
	}
	if leaves[0].ID != leaf || leaves[0].Label != "work branch" {
		t.Fatalf("leaf after reopen = %#v", leaves[0])
	}
}

func TestForkFromDeepHistorySharesMessagesBeforeFork(t *testing.T) {
	sess := newTreeTestSession(t)
	appendTreeText(t, sess, "one")
	two := appendTreeText(t, sess, "two")
	mainLeaf := appendTreeText(t, sess, "three")
	if _, err := sess.ForkAt(two); err != nil {
		t.Fatal(err)
	}
	branchLeaf := appendTreeText(t, sess, "branch")

	mainPath, err := sess.PathToLeaf(mainLeaf)
	if err != nil {
		t.Fatal(err)
	}
	assertPathTexts(t, mainPath, []string{"one", "two", "three"})

	branchPath, err := sess.PathToLeaf(branchLeaf)
	if err != nil {
		t.Fatal(err)
	}
	assertPathTexts(t, branchPath, []string{"one", "two", "branch"})
}

func TestSummarizeAndRecordPersistsBranchSummary(t *testing.T) {
	sess := newTreeTestSession(t)
	first := appendTreeText(t, sess, "first")
	assistantLeaf := appendTreeAssistantTool(t, sess, "read", `{"path":"README.md"}`)
	if _, err := sess.ForkAt(first); err != nil {
		t.Fatal(err)
	}

	summarizer := &capturingSummarizer{summary: "branch summary"}
	if err := sess.SummarizeAndRecord(context.Background(), summarizer, assistantLeaf); err != nil {
		t.Fatal(err)
	}
	if summarizer.req.LeafID != assistantLeaf {
		t.Fatalf("request leaf = %q, want %q", summarizer.req.LeafID, assistantLeaf)
	}
	if !reflect.DeepEqual(summarizer.req.ReadFiles, []string{"README.md"}) {
		t.Fatalf("read files = %#v", summarizer.req.ReadFiles)
	}
	path, err := sess.PathToCurrentLeaf()
	if err != nil {
		t.Fatal(err)
	}
	messages, err := messagesFromRecords(path)
	if err != nil {
		t.Fatal(err)
	}
	got, ok := messages[len(messages)-1].(agent.BranchSummaryMessage)
	if !ok || got.Summary != "branch summary" || got.SourceLeafID != assistantLeaf {
		t.Fatalf("last message = %#v", messages[len(messages)-1])
	}
}

type capturingSummarizer struct {
	req     BranchSummaryRequest
	summary string
}

func (s *capturingSummarizer) Summarize(_ context.Context, req BranchSummaryRequest) (string, error) {
	s.req = req
	return s.summary, nil
}

func newTreeTestSession(t *testing.T) *Session {
	t.Helper()
	store := NewJSONLStore(t.TempDir())
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

func appendTreeText(t *testing.T, sess *Session, text string) string {
	t.Helper()
	if err := sess.AppendMessage(agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: text}}}); err != nil {
		t.Fatal(err)
	}
	return sess.LeafID()
}

func appendTreeAssistantTool(t *testing.T, sess *Session, name string, input string) string {
	t.Helper()
	if err := sess.AppendMessage(agent.AssistantMessage{Content: []agent.Content{
		agent.ToolUseContent{ID: "call-1", Name: name, Input: []byte(input)},
	}}); err != nil {
		t.Fatal(err)
	}
	return sess.LeafID()
}

func mustTree(t *testing.T, sess *Session) []Leaf {
	t.Helper()
	leaves, err := sess.Tree()
	if err != nil {
		t.Fatal(err)
	}
	return leaves
}

func leafParents(leaves []Leaf) map[string]string {
	parents := make(map[string]string, len(leaves))
	for _, leaf := range leaves {
		parents[leaf.ID] = leaf.ParentID
	}
	return parents
}
