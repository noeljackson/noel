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
	if got < 15 || got > 20 {
		t.Fatalf("expected estimate near 17 tokens, got %d", got)
	}
}
