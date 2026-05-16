package compaction

import (
	"context"
	"encoding/json"
	"reflect"
	"strings"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

type mockSummaryProvider struct {
	received  [][]agent.Message
	summaries []string
	summary   string
}

func (p *mockSummaryProvider) Summarize(ctx context.Context, messages []agent.Message) (string, error) {
	p.received = append(p.received, messages)
	if len(p.summaries) > 0 {
		summary := p.summaries[0]
		p.summaries = p.summaries[1:]
		return summary, nil
	}
	return p.summary, nil
}

func TestCompactProducesSummaryAndPreservedSuffix(t *testing.T) {
	provider := &mockSummaryProvider{summary: "compact summary"}
	compactor := New(Settings{
		MaxTokens:       200,
		TargetTokens:    10,
		SummaryProvider: provider,
	})
	messages := []agent.Message{
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "older request"}}},
		agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "older answer"}}},
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "recent request with detail"}}},
		agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "recent answer with detail"}}},
	}

	got, err := compactor.Compact(context.Background(), messages, "")
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 3 {
		t.Fatalf("expected summary plus two preserved messages, got %d", len(got))
	}
	summary, ok := got[0].(agent.CompactionSummaryMessage)
	if !ok {
		t.Fatalf("expected first message to be compaction summary, got %T", got[0])
	}
	if text := summary.Summary; text != "compact summary" {
		t.Fatalf("unexpected summary %q", text)
	}
	if !reflect.DeepEqual(got[1], messages[2]) || !reflect.DeepEqual(got[2], messages[3]) {
		t.Fatalf("preserved suffix mismatch: %#v", got[1:])
	}
	if len(provider.received[0]) != 2 {
		t.Fatalf("expected summarization prompt messages, got %d", len(provider.received[0]))
	}
	prompt := provider.received[0][1].(agent.UserMessage).Content[0].(agent.TextContent).Text
	if !strings.Contains(prompt, "[User]: older request") {
		t.Fatalf("summary prompt did not include serialized conversation: %q", prompt)
	}
}

func TestPrepareCompactionReturnsPlan(t *testing.T) {
	compactor := New(Settings{TargetTokens: 6})
	messages := []agent.Message{
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "older request"}}},
		agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "older answer"}}},
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "recent request"}}},
		agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "recent answer"}}},
	}

	plan, err := compactor.PrepareCompaction(messages, "")
	if err != nil {
		t.Fatal(err)
	}
	if plan.CutIndex != 2 {
		t.Fatalf("cut index = %d, want 2", plan.CutIndex)
	}
	if len(plan.SummarizedKept) != 2 || len(plan.PreservedSuffix) != 2 {
		t.Fatalf("unexpected plan lengths: %#v", plan)
	}
	if plan.EstimatedTokensBefore == 0 || plan.EstimatedTokensAfter == 0 {
		t.Fatalf("expected token estimates in plan: %#v", plan)
	}
}

func TestSplitTurnReturnsPrefixMessages(t *testing.T) {
	compactor := New(Settings{TargetTokens: 8})
	messages := []agent.Message{
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "do work"}}},
		agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "early work"}}},
		agent.AssistantMessage{Content: []agent.Content{
			agent.TextContent{Text: "now read"},
			agent.ToolUseContent{ID: "call-1", Name: "read", Input: json.RawMessage(`{"path":"a.txt"}`)},
		}},
		agent.ToolResultMessage{Results: []agent.ToolResult{{ToolUseID: "call-1"}}},
	}

	prefix, err := compactor.SplitTurn(messages, 2)
	if err != nil {
		t.Fatal(err)
	}
	if len(prefix) != 2 {
		t.Fatalf("prefix length = %d, want 2", len(prefix))
	}
	if _, ok := prefix[0].(agent.UserMessage); !ok {
		t.Fatalf("expected prefix to start with user message, got %T", prefix[0])
	}
}

func TestUpdatePreviousSummaryUsesUpdatePrompt(t *testing.T) {
	provider := &mockSummaryProvider{summary: "updated summary"}
	compactor := New(Settings{SummaryProvider: provider})
	updated, err := compactor.UpdatePreviousSummary(context.Background(), "previous summary", []agent.Message{
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "new work"}}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if updated != "updated summary" {
		t.Fatalf("updated summary = %q", updated)
	}
	prompt := provider.received[0][1].(agent.UserMessage).Content[0].(agent.TextContent).Text
	if !strings.Contains(prompt, "<previous-summary>\nprevious summary\n</previous-summary>") {
		t.Fatalf("prompt missing previous summary: %q", prompt)
	}
	if !strings.Contains(prompt, "The messages above are NEW conversation messages") {
		t.Fatalf("prompt missing update instructions: %q", prompt)
	}
}
