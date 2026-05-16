package modes

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"strings"
	"sync"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
	"github.com/noeljackson/pi/internal/cli"
)

type fakeProvider struct {
	mu        sync.Mutex
	responses []agent.AssistantMessage
}

func (p *fakeProvider) Stream(ctx context.Context, req agent.StreamRequest, emit func(agent.Event)) (*agent.AssistantMessage, error) {
	p.mu.Lock()
	if len(p.responses) == 0 {
		p.mu.Unlock()
		return nil, errors.New("missing fake response")
	}
	response := p.responses[0]
	p.responses = p.responses[1:]
	p.mu.Unlock()

	emit(agent.MessageStartEvent{MessageID: "msg", Role: agent.RoleAssistant, Model: req.Model})
	for _, content := range response.Content {
		if text, ok := content.(agent.TextContent); ok {
			emit(agent.MessageUpdateEvent{MessageID: "msg", Delta: agent.MessageDelta{TextDelta: text.Text}})
		}
	}
	emit(agent.MessageEndEvent{
		MessageID:    "msg",
		FinalContent: response.Content,
		StopReason:   response.StopReason.String(),
		Usage:        response.Usage,
	})
	response.Model = req.Model
	return &response, nil
}

func TestRunPrintWithAgentWritesAssistantTextOnly(t *testing.T) {
	runner := agent.NewAgent(agent.LoopConfig{
		Provider: &fakeProvider{responses: []agent.AssistantMessage{{
			Content:    []agent.Content{agent.TextContent{Text: "fixed message"}},
			StopReason: agent.StopEndTurn,
		}}},
		Model: "fake-model",
	})
	var out bytes.Buffer
	err := RunPrintWithAgent(context.Background(), cli.Options{Prompt: "hello"}, runner, &out)
	if err != nil {
		t.Fatal(err)
	}
	if got := out.String(); got != "fixed message\n" {
		t.Fatalf("output = %q", got)
	}
}

func TestRunJSONWithAgentWritesJSONLEvents(t *testing.T) {
	runner := agent.NewAgent(agent.LoopConfig{
		Provider: &fakeProvider{responses: []agent.AssistantMessage{{
			Content:    []agent.Content{agent.TextContent{Text: "hello"}},
			StopReason: agent.StopEndTurn,
			Usage:      agent.Usage{InputTokens: 1, OutputTokens: 2, TotalTokens: 3},
		}}},
		Model: "fake-model",
	})
	var out bytes.Buffer
	err := RunJSONWithAgent(context.Background(), cli.Options{Prompt: "hello"}, runner, &out)
	if err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimSpace(out.String()), "\n")
	if len(lines) == 0 {
		t.Fatal("no JSONL output")
	}
	seenDelta := false
	for _, line := range lines {
		var event map[string]any
		if err := json.Unmarshal([]byte(line), &event); err != nil {
			t.Fatalf("invalid JSONL line %q: %v", line, err)
		}
		if event["type"] == "message_update" {
			delta := event["assistantMessageEvent"].(map[string]any)
			seenDelta = delta["type"] == "text_delta" && delta["delta"] == "hello"
		}
	}
	if !seenDelta {
		t.Fatalf("text delta not found in JSONL:\n%s", out.String())
	}
}
