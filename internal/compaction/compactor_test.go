package compaction

import (
	"context"
	"reflect"
	"strings"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

type mockSummaryProvider struct {
	received []agent.Message
	summary  string
}

func (p *mockSummaryProvider) Summarize(ctx context.Context, messages []agent.Message) (string, error) {
	p.received = messages
	return p.summary, nil
}

func TestCompactProducesSummaryAndPreservedSuffix(t *testing.T) {
	provider := &mockSummaryProvider{summary: "compact summary"}
	compactor := New(Settings{
		MaxTokens:       200,
		TargetTokens:    20,
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
	summary, ok := got[0].(agent.SystemMessage)
	if !ok {
		t.Fatalf("expected first message to be system summary, got %T", got[0])
	}
	if text := summary.Content[0].(agent.TextContent).Text; text != "compact summary" {
		t.Fatalf("unexpected summary %q", text)
	}
	if !reflect.DeepEqual(got[1], messages[2]) || !reflect.DeepEqual(got[2], messages[3]) {
		t.Fatalf("preserved suffix mismatch: %#v", got[1:])
	}
	if len(provider.received) != 2 {
		t.Fatalf("expected summarization prompt messages, got %d", len(provider.received))
	}
	prompt := provider.received[1].(agent.UserMessage).Content[0].(agent.TextContent).Text
	if !strings.Contains(prompt, "[User]: older request") {
		t.Fatalf("summary prompt did not include serialized conversation: %q", prompt)
	}
}
