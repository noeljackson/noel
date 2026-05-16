package rpc

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
	})
	response.Model = req.Model
	return &response, nil
}

func TestRunPromptCommandWritesResponseAndEvents(t *testing.T) {
	runner := agent.NewAgent(agent.LoopConfig{
		Provider: &fakeProvider{responses: []agent.AssistantMessage{{
			Content:    []agent.Content{agent.TextContent{Text: "rpc response"}},
			StopReason: agent.StopEndTurn,
		}}},
		Model: "fake-model",
	})
	input := strings.NewReader(`{"id":"1","type":"prompt","message":"hello"}` + "\n")
	var out bytes.Buffer

	if err := Run(context.Background(), cli.Options{}, runner, input, &out); err != nil {
		t.Fatal(err)
	}

	lines := strings.Split(strings.TrimSpace(out.String()), "\n")
	if len(lines) < 2 {
		t.Fatalf("expected response and events, got:\n%s", out.String())
	}
	var first map[string]any
	if err := json.Unmarshal([]byte(lines[0]), &first); err != nil {
		t.Fatal(err)
	}
	if first["type"] != "response" || first["command"] != "prompt" || first["success"] != true {
		t.Fatalf("first response = %#v", first)
	}
	seenDelta := false
	for _, line := range lines[1:] {
		var event map[string]any
		if err := json.Unmarshal([]byte(line), &event); err != nil {
			t.Fatal(err)
		}
		if event["type"] == "message_update" {
			delta := event["assistantMessageEvent"].(map[string]any)
			seenDelta = delta["delta"] == "rpc response"
		}
	}
	if !seenDelta {
		t.Fatalf("message update not found:\n%s", out.String())
	}
}
