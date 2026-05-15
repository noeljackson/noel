package anthropic

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/noeljackson/pi/internal/agent"
)

func TestStreamTextOnlyMessage(t *testing.T) {
	stream := sse(
		"message_start", `{"type":"message_start","message":{"id":"msg-1","model":"claude-sonnet-4-6","role":"assistant","usage":{"input_tokens":10,"output_tokens":0}}}`,
		"content_block_start", `{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}`,
		"content_block_delta", `{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}`,
		"content_block_stop", `{"type":"content_block_stop","index":0}`,
		"message_delta", `{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}`,
		"message_stop", `{"type":"message_stop"}`,
	)
	_, _, events, message, err := runMockStream(t, APIKeyAuth{Key: "sk-ant-test"}, agent.StreamRequest{
		Model:    "claude-sonnet-4-6",
		System:   "system",
		Messages: []agent.Message{agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "hello"}}}},
	}, stream)
	if err != nil {
		t.Fatal(err)
	}
	if message.ResponseID != "msg-1" || message.ResponseModel != "claude-sonnet-4-6" {
		t.Fatalf("response metadata = %#v", message)
	}
	if message.API != apiName || message.Provider != anthropicProvider {
		t.Fatalf("provider metadata = %s/%s", message.API, message.Provider)
	}
	if message.StopReason != agent.StopEndTurn {
		t.Fatalf("stop reason = %q", message.StopReason)
	}
	if message.Usage.InputTokens != 10 || message.Usage.OutputTokens != 2 || message.Usage.TotalTokens != 12 {
		t.Fatalf("usage = %#v", message.Usage)
	}
	if message.Cost == nil || message.Cost.Total == 0 {
		t.Fatalf("cost = %#v, want populated", message.Cost)
	}
	text, ok := message.Content[0].(agent.TextContent)
	if !ok || text.Text != "hi" {
		t.Fatalf("content = %#v", message.Content)
	}
	if len(events) < 4 {
		t.Fatalf("events = %#v", events)
	}
	if update, ok := events[1].(agent.MessageUpdateEvent); !ok || update.Delta.TextDelta != "hi" {
		t.Fatalf("text delta event = %#v", events[1])
	}
}

func TestStreamToolUseRepairsMalformedInputJSON(t *testing.T) {
	stream := sse(
		"message_start", `{"type":"message_start","message":{"id":"msg-1","model":"claude-sonnet-4-6","usage":{"input_tokens":1}}}`,
		"content_block_start", `{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu-1","name":"bash","input":{}}}`,
		"content_block_delta", `{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"echo"}}`,
		"content_block_stop", `{"type":"content_block_stop","index":0}`,
		"message_delta", `{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}`,
		"message_stop", `{"type":"message_stop"}`,
	)
	_, _, events, message, err := runMockStream(t, APIKeyAuth{Key: "sk-ant-test"}, agent.StreamRequest{
		Model:    "claude-sonnet-4-6",
		Messages: []agent.Message{agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "run"}}}},
	}, stream)
	if err != nil {
		t.Fatal(err)
	}
	toolUse, ok := message.Content[0].(agent.ToolUseContent)
	if !ok {
		t.Fatalf("content = %#v", message.Content)
	}
	var input map[string]string
	if err := json.Unmarshal(toolUse.Input, &input); err != nil {
		t.Fatalf("tool input JSON invalid: %v; %s", err, toolUse.Input)
	}
	if input["cmd"] != "echo" {
		t.Fatalf("cmd = %q, want echo", input["cmd"])
	}
	if message.StopReason != agent.StopToolUse {
		t.Fatalf("stop reason = %q", message.StopReason)
	}
	if update, ok := events[1].(agent.MessageUpdateEvent); !ok || update.Delta.ToolUseDelta == nil || update.Delta.ToolUseDelta.Name != "bash" {
		t.Fatalf("tool start event = %#v", events[1])
	}
}

func TestStreamThinkingSignatureAndRedactedThinking(t *testing.T) {
	stream := sse(
		"message_start", `{"type":"message_start","message":{"id":"msg-1","model":"claude-sonnet-4-6","usage":{}}}`,
		"content_block_start", `{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}`,
		"content_block_delta", `{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason"}}`,
		"content_block_delta", `{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}`,
		"content_block_stop", `{"type":"content_block_stop","index":0}`,
		"content_block_start", `{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque"}}`,
		"content_block_stop", `{"type":"content_block_stop","index":1}`,
		"message_delta", `{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{}}`,
		"message_stop", `{"type":"message_stop"}`,
	)
	_, _, events, message, err := runMockStream(t, APIKeyAuth{Key: "sk-ant-test"}, agent.StreamRequest{
		Model:    "claude-sonnet-4-6",
		Messages: []agent.Message{agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "think"}}}},
	}, stream)
	if err != nil {
		t.Fatal(err)
	}
	thinking, ok := message.Content[0].(agent.ThinkingContent)
	if !ok || thinking.Thinking != "reason" || thinking.ThinkingSignature != "sig" {
		t.Fatalf("thinking = %#v", message.Content[0])
	}
	redacted, ok := message.Content[1].(agent.RedactedThinkingContent)
	if !ok || redacted.Data != "opaque" {
		t.Fatalf("redacted = %#v", message.Content[1])
	}
	var sawSignature bool
	var sawRedacted bool
	for _, event := range events {
		update, ok := event.(agent.MessageUpdateEvent)
		if !ok {
			continue
		}
		sawSignature = sawSignature || update.Delta.SignatureDelta == "sig"
		sawRedacted = sawRedacted || update.Delta.RedactedThinkingDelta == "opaque"
	}
	if !sawSignature || !sawRedacted {
		t.Fatalf("events missing signature/redacted deltas: %#v", events)
	}
}

func TestStreamMultipleToolCalls(t *testing.T) {
	stream := sse(
		"message_start", `{"type":"message_start","message":{"id":"msg-1","model":"claude-sonnet-4-6","usage":{}}}`,
		"content_block_start", `{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu-1","name":"bash","input":{}}}`,
		"content_block_delta", `{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"pwd\"}"}}`,
		"content_block_stop", `{"type":"content_block_stop","index":0}`,
		"content_block_start", `{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu-2","name":"read","input":{}}}`,
		"content_block_delta", `{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"README.md\"}"}}`,
		"content_block_stop", `{"type":"content_block_stop","index":1}`,
		"message_delta", `{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{}}`,
		"message_stop", `{"type":"message_stop"}`,
	)
	_, _, _, message, err := runMockStream(t, APIKeyAuth{Key: "sk-ant-test"}, agent.StreamRequest{
		Model:    "claude-sonnet-4-6",
		Messages: []agent.Message{agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "tools"}}}},
	}, stream)
	if err != nil {
		t.Fatal(err)
	}
	if len(message.Content) != 2 {
		t.Fatalf("content len = %d, want 2", len(message.Content))
	}
	first := message.Content[0].(agent.ToolUseContent)
	second := message.Content[1].(agent.ToolUseContent)
	if first.ID != "toolu-1" || second.ID != "toolu-2" {
		t.Fatalf("tool calls = %#v", message.Content)
	}
}

func TestStreamMidStreamUsageUpdate(t *testing.T) {
	stream := sse(
		"message_start", `{"type":"message_start","message":{"id":"msg-1","model":"claude-sonnet-4-6","usage":{"input_tokens":3}}}`,
		"message_delta", `{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4,"cache_read_input_tokens":5,"cache_creation_input_tokens":6}}`,
		"message_stop", `{"type":"message_stop"}`,
	)
	_, _, events, message, err := runMockStream(t, APIKeyAuth{Key: "sk-ant-test"}, agent.StreamRequest{
		Model:    "claude-sonnet-4-6",
		Messages: []agent.Message{agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "usage"}}}},
	}, stream)
	if err != nil {
		t.Fatal(err)
	}
	if message.Usage.TotalTokens != 18 {
		t.Fatalf("usage = %#v", message.Usage)
	}
	var sawUsage bool
	for _, event := range events {
		update, ok := event.(agent.MessageUpdateEvent)
		if ok && update.Delta.Usage != nil && update.Delta.Usage.TotalTokens == 18 {
			sawUsage = true
		}
	}
	if !sawUsage {
		t.Fatalf("missing usage update event: %#v", events)
	}
}

func TestStreamErrorEvent(t *testing.T) {
	stream := sse("error", `{"type":"error","error":{"message":"bad"}}`)
	_, _, events, message, err := runMockStream(t, APIKeyAuth{Key: "sk-ant-test"}, agent.StreamRequest{
		Model:    "claude-sonnet-4-6",
		Messages: []agent.Message{agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "error"}}}},
	}, stream)
	if err == nil {
		t.Fatal("expected error")
	}
	if message == nil || message.StopReason != agent.StopError {
		t.Fatalf("message = %#v", message)
	}
	end, ok := events[len(events)-1].(agent.AgentEndEvent)
	if !ok || end.Reason != "error" || end.Err == nil {
		t.Fatalf("last event = %#v", events[len(events)-1])
	}
}

func TestOAuthHeadersAndToolNameCasing(t *testing.T) {
	stream := sse(
		"message_start", `{"type":"message_start","message":{"id":"msg-1","model":"claude-sonnet-4-6","usage":{}}}`,
		"content_block_start", `{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu-1","name":"Bash","input":{}}}`,
		"message_delta", `{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{}}`,
		"message_stop", `{"type":"message_stop"}`,
	)
	payload, headers, _, message, err := runMockStream(t, ClaudeCodeOAuth{Path: writeOAuthCredentials(t)}, agent.StreamRequest{
		Model:    "claude-sonnet-4-6",
		Messages: []agent.Message{agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "run"}}}},
		Tools:    []agent.Tool{testTool{name: "bash"}},
	}, stream)
	if err != nil {
		t.Fatal(err)
	}
	if got := headers.Get("Authorization"); got != "Bearer sk-ant-oat-test" {
		t.Fatalf("Authorization = %q", got)
	}
	if got := headers.Get("x-api-key"); got != "" {
		t.Fatalf("x-api-key = %q, want empty", got)
	}
	if got := headers.Get("x-app"); got != "claude-code" {
		t.Fatalf("x-app = %q", got)
	}
	beta := headers.Get("anthropic-beta")
	if !strings.Contains(beta, claudeCodeBeta) || !strings.Contains(beta, oauthBeta) {
		t.Fatalf("anthropic-beta = %q", beta)
	}
	tools := payload["tools"].([]any)
	tool := tools[0].(map[string]any)
	if tool["name"] != "Bash" {
		t.Fatalf("outgoing tool name = %q", tool["name"])
	}
	toolUse := message.Content[0].(agent.ToolUseContent)
	if toolUse.Name != "bash" {
		t.Fatalf("incoming tool name = %q", toolUse.Name)
	}
}

func TestAPIKeyHeaders(t *testing.T) {
	stream := sse(
		"message_start", `{"type":"message_start","message":{"id":"msg-1","model":"claude-sonnet-4-6","usage":{}}}`,
		"message_delta", `{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{}}`,
		"message_stop", `{"type":"message_stop"}`,
	)
	_, headers, _, _, err := runMockStream(t, APIKeyAuth{Key: "sk-ant-test"}, agent.StreamRequest{
		Model:    "claude-sonnet-4-6",
		Messages: []agent.Message{agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "hello"}}}},
	}, stream)
	if err != nil {
		t.Fatal(err)
	}
	if got := headers.Get("x-api-key"); got != "sk-ant-test" {
		t.Fatalf("x-api-key = %q", got)
	}
	if got := headers.Get("Authorization"); got != "" {
		t.Fatalf("Authorization = %q, want empty", got)
	}
}

func TestConsecutiveToolResultGrouping(t *testing.T) {
	stream := sse(
		"message_start", `{"type":"message_start","message":{"id":"msg-1","model":"claude-sonnet-4-6","usage":{}}}`,
		"message_delta", `{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{}}`,
		"message_stop", `{"type":"message_stop"}`,
	)
	payload, _, _, _, err := runMockStream(t, APIKeyAuth{Key: "sk-ant-test"}, agent.StreamRequest{
		Model: "claude-sonnet-4-6",
		Messages: []agent.Message{
			agent.ToolResultMessage{Results: []agent.ToolResult{{ToolUseID: "toolu-1", Content: []agent.Content{agent.TextContent{Text: "one"}}}}},
			agent.ToolResultMessage{Results: []agent.ToolResult{{ToolUseID: "toolu-2", Content: []agent.Content{agent.TextContent{Text: "two"}}}}},
		},
	}, stream)
	if err != nil {
		t.Fatal(err)
	}
	messages := payload["messages"].([]any)
	if len(messages) != 1 {
		t.Fatalf("messages len = %d, want 1", len(messages))
	}
	content := messages[0].(map[string]any)["content"].([]any)
	if len(content) != 2 {
		t.Fatalf("tool result blocks len = %d, want 2", len(content))
	}
}

func TestPayloadSystemToolImageThinkingMetadataAndToolChoice(t *testing.T) {
	stream := sse(
		"message_start", `{"type":"message_start","message":{"id":"msg-1","model":"claude-3-7-sonnet-20250219","usage":{}}}`,
		"message_delta", `{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{}}`,
		"message_stop", `{"type":"message_stop"}`,
	)
	payload, headers, _, _, err := runMockStream(t, APIKeyAuth{Key: "sk-ant-test"}, agent.StreamRequest{
		Model:  "claude-3-7-sonnet-20250219",
		System: "system",
		Messages: []agent.Message{agent.UserMessage{Content: []agent.Content{
			agent.TextContent{Text: "look"},
			agent.ImageContent{Source: agent.ImageSource{Type: "base64", MediaType: "image/png", Data: "aW1n"}},
		}}},
		Tools:      []agent.Tool{testTool{name: "bash"}},
		Thinking:   &agent.ThinkingConfig{Enabled: true, BudgetTokens: 2048},
		Metadata:   map[string]string{"user_id": "user-1"},
		ToolChoice: &agent.ToolChoice{Type: "tool", Name: "bash"},
	}, stream)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(headers.Get("anthropic-beta"), interleavedThinkingBeta) {
		t.Fatalf("anthropic-beta = %q, want interleaved thinking", headers.Get("anthropic-beta"))
	}
	system := payload["system"].([]any)[0].(map[string]any)
	if system["cache_control"].(map[string]any)["type"] != "ephemeral" {
		t.Fatalf("system cache_control = %#v", system["cache_control"])
	}
	messages := payload["messages"].([]any)
	content := messages[0].(map[string]any)["content"].([]any)
	if content[1].(map[string]any)["type"] != "image" {
		t.Fatalf("image block = %#v", content[1])
	}
	tool := payload["tools"].([]any)[0].(map[string]any)
	if tool["eager_input_streaming"] != true {
		t.Fatalf("tool eager_input_streaming = %#v", tool["eager_input_streaming"])
	}
	if tool["cache_control"].(map[string]any)["type"] != "ephemeral" {
		t.Fatalf("tool cache_control = %#v", tool["cache_control"])
	}
	thinking := payload["thinking"].(map[string]any)
	if thinking["type"] != "enabled" || thinking["budget_tokens"] != float64(2048) {
		t.Fatalf("thinking = %#v", thinking)
	}
	metadata := payload["metadata"].(map[string]any)
	if metadata["user_id"] != "user-1" {
		t.Fatalf("metadata = %#v", metadata)
	}
	toolChoice := payload["tool_choice"].(map[string]any)
	if toolChoice["type"] != "tool" || toolChoice["name"] != "bash" {
		t.Fatalf("tool_choice = %#v", toolChoice)
	}
}

func runMockStream(t *testing.T, auth AuthSource, req agent.StreamRequest, stream string) (map[string]any, http.Header, []agent.Event, *agent.AssistantMessage, error) {
	t.Helper()
	var payload map[string]any
	var headers http.Header
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != messagesPath {
			t.Errorf("path = %s, want %s", r.URL.Path, messagesPath)
		}
		headers = r.Header.Clone()
		if err := json.NewDecoder(r.Body).Decode(&payload); err != nil {
			t.Errorf("decode payload: %v", err)
		}
		w.Header().Set("content-type", "text/event-stream")
		_, _ = w.Write([]byte(stream))
	}))
	defer server.Close()

	client := &Client{auth: auth, baseURL: server.URL, httpClient: server.Client()}
	var events []agent.Event
	message, err := client.Stream(context.Background(), req, func(event agent.Event) {
		events = append(events, event)
	})
	return payload, headers, events, message, err
}

func sse(parts ...string) string {
	var builder strings.Builder
	for i := 0; i < len(parts); i += 2 {
		builder.WriteString("event: ")
		builder.WriteString(parts[i])
		builder.WriteString("\n")
		builder.WriteString("data: ")
		builder.WriteString(parts[i+1])
		builder.WriteString("\n\n")
	}
	return builder.String()
}

func writeOAuthCredentials(t *testing.T) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), ".credentials.json")
	data := `{"claudeAiOauth":{"accessToken":"sk-ant-oat-test","expiresAt":` + strconv.FormatInt(time.Now().Add(time.Hour).UnixMilli(), 10) + `}}`
	if err := os.WriteFile(path, []byte(data), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

type testTool struct {
	name string
}

func (t testTool) Name() string {
	return t.name
}

func (t testTool) Description() string {
	return "test tool"
}

func (t testTool) Schema() json.RawMessage {
	return json.RawMessage(`{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}`)
}

func (t testTool) ParallelSafe() bool {
	return true
}

func (t testTool) Execute(context.Context, json.RawMessage, agent.ToolCallContext) (agent.ToolResult, error) {
	return agent.ToolResult{}, nil
}
