package compaction

import (
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

func TestEstimateTokensKnownString(t *testing.T) {
	estimator := ContextEstimator{}
	messages := []agent.Message{
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "1234567890123456"}}},
	}

	got := estimator.EstimateTokens(messages, "abcd")
	if got != 4 {
		t.Fatalf("expected TS-compatible char estimate of 4 tokens, got %d", got)
	}
}

func TestEstimateContextTokensUsesUsageAnchorAndTrailingEstimate(t *testing.T) {
	estimator := ContextEstimator{}
	messages := []agent.Message{
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "older ignored by usage"}}},
		agent.AssistantMessage{
			Content:    []agent.Content{agent.TextContent{Text: "assistant"}},
			Usage:      agent.Usage{InputTokens: 100, OutputTokens: 25, CacheReadInputTokens: 7, CacheCreationInputTokens: 3},
			StopReason: agent.StopEndTurn,
		},
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "12345678"}}},
	}

	got := estimator.EstimateContextTokens(messages)
	if got.Tokens != 137 {
		t.Fatalf("tokens = %d, want 137", got.Tokens)
	}
	if got.UsageTokens != 135 || got.TrailingTokens != 2 {
		t.Fatalf("unexpected split estimate: %#v", got)
	}
	if got.LastUsageIndex == nil || *got.LastUsageIndex != 1 {
		t.Fatalf("last usage index = %#v, want 1", got.LastUsageIndex)
	}
}

func TestEstimateContextTokensIgnoresErroredUsage(t *testing.T) {
	estimator := ContextEstimator{}
	messages := []agent.Message{
		agent.AssistantMessage{
			Content:      []agent.Content{agent.TextContent{Text: "12345678"}},
			Usage:        agent.Usage{InputTokens: 1000},
			StopReason:   agent.StopError,
			ErrorMessage: "failed",
		},
	}

	got := estimator.EstimateContextTokens(messages)
	if got.Tokens != 2 || got.UsageTokens != 0 || got.LastUsageIndex != nil {
		t.Fatalf("unexpected estimate for errored usage: %#v", got)
	}
}
