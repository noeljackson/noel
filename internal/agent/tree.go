package agent

import (
	"context"
	"errors"

	"github.com/noeljackson/pi/internal/session/schema"
)

type treeSessionWriter interface {
	LeafID() string
	ForkAt(entryID string) (string, error)
	ForkBefore(entryID string) (string, error)
	MoveTo(leafID string) error
	SetLeafLabel(leafID, label string) error
	Tree() ([]schema.Leaf, error)
	Messages() ([]Message, error)
	SummarizeAndRecord(ctx context.Context, summarizer schema.BranchSummarizer, fromLeaf string) error
}

type branchSummarySessionWriter interface {
	BuildBranchSummaryRequest(fromLeaf string) (schema.BranchSummaryRequest, error)
	RecordBranchSummary(fromLeaf, summary string, readFiles, modifiedFiles []string) error
}

// Fork creates a new session branch at entryID and makes it current.
func (a *Agent) Fork(ctx context.Context, entryID string) (string, error) {
	writer, err := a.treeSession()
	if err != nil {
		return "", err
	}
	if err := a.cancelAndClearQueue(ctx); err != nil {
		return "", err
	}
	fromLeaf := writer.LeafID()
	newLeafID, err := writer.ForkAt(entryID)
	if err != nil {
		return "", err
	}
	if err := a.reloadMessages(writer); err != nil {
		return "", err
	}
	a.processEvent(SessionForkEvent{NewLeafID: newLeafID, FromLeafID: fromLeaf})
	return newLeafID, nil
}

// ForkBefore creates a new session branch before entryID and makes it current.
func (a *Agent) ForkBefore(ctx context.Context, entryID string) (string, error) {
	writer, err := a.treeSession()
	if err != nil {
		return "", err
	}
	if err := a.cancelAndClearQueue(ctx); err != nil {
		return "", err
	}
	fromLeaf := writer.LeafID()
	newLeafID, err := writer.ForkBefore(entryID)
	if err != nil {
		return "", err
	}
	if err := a.reloadMessages(writer); err != nil {
		return "", err
	}
	a.processEvent(SessionForkEvent{NewLeafID: newLeafID, FromLeafID: fromLeaf})
	return newLeafID, nil
}

// Move switches the agent to another session leaf.
func (a *Agent) Move(ctx context.Context, leafID string) error {
	writer, err := a.treeSession()
	if err != nil {
		return err
	}
	if err := a.cancelAndClearQueue(ctx); err != nil {
		return err
	}
	fromLeaf := writer.LeafID()
	if fromLeaf == leafID {
		return nil
	}
	a.mu.Lock()
	summarizer := a.cfg.BranchSummarizer
	a.mu.Unlock()
	if summarizer != nil && fromLeaf != "" {
		summary, err := a.summarizeAndRecordBranch(ctx, writer, summarizer, fromLeaf)
		if err != nil {
			return err
		}
		if summary != "" {
			a.processEvent(BranchSummaryEvent{LeafID: fromLeaf, Summary: summary})
		}
	}
	if err := writer.MoveTo(leafID); err != nil {
		return err
	}
	if err := a.reloadMessages(writer); err != nil {
		return err
	}
	a.processEvent(SessionMoveEvent{FromLeafID: fromLeaf, ToLeafID: leafID})
	return nil
}

func (a *Agent) summarizeAndRecordBranch(ctx context.Context, writer treeSessionWriter, summarizer schema.BranchSummarizer, fromLeaf string) (string, error) {
	if branchWriter, ok := writer.(branchSummarySessionWriter); ok {
		req, err := branchWriter.BuildBranchSummaryRequest(fromLeaf)
		if err != nil {
			return "", err
		}
		if len(req.Entries) == 0 {
			return "", nil
		}
		summary, err := summarizer.Summarize(ctx, req)
		if err != nil {
			return "", err
		}
		if err := branchWriter.RecordBranchSummary(fromLeaf, summary, req.ReadFiles, req.Modified); err != nil {
			return "", err
		}
		return summary, nil
	}
	if err := writer.SummarizeAndRecord(ctx, summarizer, fromLeaf); err != nil {
		return "", err
	}
	return "", nil
}

// LeafLabel sets or clears a label on a session leaf.
func (a *Agent) LeafLabel(leafID, label string) error {
	writer, err := a.treeSession()
	if err != nil {
		return err
	}
	return writer.SetLeafLabel(leafID, label)
}

// Tree returns all session leaves.
func (a *Agent) Tree() ([]schema.Leaf, error) {
	writer, err := a.treeSession()
	if err != nil {
		return nil, err
	}
	return writer.Tree()
}

func (a *Agent) treeSession() (treeSessionWriter, error) {
	a.mu.Lock()
	writer, ok := a.cfg.SessionWriter.(treeSessionWriter)
	a.mu.Unlock()
	if !ok || writer == nil {
		return nil, errors.New("agent session does not support tree navigation")
	}
	return writer, nil
}

func (a *Agent) cancelAndClearQueue(ctx context.Context) error {
	a.mu.Lock()
	cancel := a.active
	a.queue = nil
	a.streaming = nil
	a.pending = make(map[string]ToolUseContent)
	a.cond.Broadcast()
	a.mu.Unlock()
	if cancel != nil {
		cancel()
		if err := a.WaitForIdle(ctx); err != nil {
			return err
		}
	}
	return ctx.Err()
}

func (a *Agent) reloadMessages(writer treeSessionWriter) error {
	messages, err := writer.Messages()
	if err != nil {
		return err
	}
	a.mu.Lock()
	a.messages = append([]Message(nil), messages...)
	a.queue = nil
	a.streaming = nil
	a.pending = make(map[string]ToolUseContent)
	if !a.running {
		a.state = AgentIdle
	}
	a.cond.Broadcast()
	a.mu.Unlock()
	return nil
}
