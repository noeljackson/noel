package anthropic

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/noeljackson/pi/internal/agent"
)

const (
	defaultBaseURL                   = "https://api.anthropic.com"
	messagesPath                     = "/v1/messages"
	apiName                          = "anthropic-messages"
	anthropicProvider                = "anthropic"
	anthropicOAuthProvider           = "anthropic-oauth"
	anthropicVersion                 = "2023-06-01"
	claudeCodeVersion                = "2.1.75"
	claudeCodeBeta                   = "claude-code-20250219"
	oauthBeta                        = "oauth-2025-04-20"
	fineGrainedToolStreamingBeta     = "fine-grained-tool-streaming-2025-05-14"
	interleavedThinkingBeta          = "interleaved-thinking-2025-05-14"
	defaultThinkingDisplay           = "summarized"
	defaultThinkingBudgetTokens      = 1024
	redactedThinkingPlaceholder      = "[Reasoning redacted]"
	defaultAnthropicRequestMaxTokens = 4096
)

type Client struct {
	auth       AuthSource
	baseURL    string
	httpClient *http.Client
}

func NewClient(auth AuthSource) *Client {
	return &Client{auth: auth}
}

func (c *Client) Stream(ctx context.Context, req agent.StreamRequest, emit func(agent.Event)) (*agent.AssistantMessage, error) {
	// The Go SDK v1.43 exposes signature_delta and redacted_thinking, but the TS
	// reference also repairs SSE line/event JSON and streams tool input through a
	// custom decoder. Keep this adapter on direct HTTP so fidelity is controlled
	// here instead of by SDK accumulator behavior.
	if emit == nil {
		emit = func(agent.Event) {}
	}
	isOAuth := isClaudeCodeOAuth(c.auth)
	payload, err := buildAnthropicRequest(req, isOAuth)
	if err != nil {
		return nil, err
	}

	response, err := c.doStreamRequest(ctx, payload, isOAuth, requestBetaFeatures(req))
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()

	if response.StatusCode < http.StatusOK || response.StatusCode >= http.StatusMultipleChoices {
		body, _ := io.ReadAll(response.Body)
		return nil, fmt.Errorf("anthropic: HTTP %d: %s", response.StatusCode, strings.TrimSpace(string(body)))
	}

	message, err := translateStream(ctx, response.Body, req, isOAuth, emit)
	if err != nil {
		emit(agent.AgentEndEvent{Reason: agent.StopError.String(), Err: err})
		return message, err
	}
	return message, nil
}

func (c *Client) doStreamRequest(ctx context.Context, payload anthropicRequest, isOAuth bool, betaFeatures []string) (*http.Response, error) {
	if c.auth == nil {
		return nil, errors.New("anthropic: missing auth source")
	}
	body, err := json.Marshal(payload)
	if err != nil {
		return nil, err
	}
	request, err := http.NewRequestWithContext(ctx, http.MethodPost, c.messagesURL(), bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	request.Header.Set("accept", "text/event-stream")
	request.Header.Set("content-type", "application/json")
	request.Header.Set("anthropic-version", anthropicVersion)

	headers, err := c.auth.Headers(ctx)
	if err != nil {
		return nil, err
	}
	for key, value := range composeHeaders(headers, isOAuth, betaFeatures) {
		if value == "" {
			continue
		}
		request.Header.Set(key, value)
	}

	client := c.httpClient
	if client == nil {
		client = http.DefaultClient
	}
	return client.Do(request)
}

func (c *Client) messagesURL() string {
	base := c.baseURL
	if base == "" {
		base = defaultBaseURL
	}
	parsed, err := url.Parse(base)
	if err != nil {
		return strings.TrimRight(defaultBaseURL, "/") + messagesPath
	}
	parsed.Path = strings.TrimRight(parsed.Path, "/") + messagesPath
	return parsed.String()
}

func requestBetaFeatures(req agent.StreamRequest) []string {
	var betas []string
	if req.FineGrainedToolStreaming {
		betas = append(betas, fineGrainedToolStreamingBeta)
	}
	if !supportsAdaptiveThinking(req.Model) {
		betas = append(betas, interleavedThinkingBeta)
	}
	return betas
}

func composeHeaders(headers map[string]string, isOAuth bool, betaFeatures []string) map[string]string {
	result := make(map[string]string, len(headers)+4)
	for key, value := range headers {
		if strings.EqualFold(key, "anthropic-beta") {
			continue
		}
		result[key] = value
	}
	betas := splitBetaHeader(headers["anthropic-beta"])
	if isOAuth {
		betas = append([]string{claudeCodeBeta, oauthBeta}, betas...)
		result["user-agent"] = "claude-cli/" + claudeCodeVersion
		result["x-app"] = "claude-code"
	}
	betas = append(betas, betaFeatures...)
	if len(betas) > 0 {
		result["anthropic-beta"] = joinUniqueBetas(betas)
	}
	return result
}

func splitBetaHeader(header string) []string {
	if header == "" {
		return nil
	}
	parts := strings.Split(header, ",")
	result := make([]string, 0, len(parts))
	for _, part := range parts {
		trimmed := strings.TrimSpace(part)
		if trimmed != "" {
			result = append(result, trimmed)
		}
	}
	return result
}

func joinUniqueBetas(values []string) string {
	seen := map[string]bool{}
	result := make([]string, 0, len(values))
	for _, value := range values {
		if value == "" || seen[value] {
			continue
		}
		seen[value] = true
		result = append(result, value)
	}
	return strings.Join(result, ",")
}

func isClaudeCodeOAuth(auth AuthSource) bool {
	switch auth.(type) {
	case ClaudeCodeOAuth, *ClaudeCodeOAuth:
		return true
	default:
		return false
	}
}

type anthropicRequest struct {
	Model        string                  `json:"model"`
	Messages     []anthropicMessageParam `json:"messages"`
	MaxTokens    int                     `json:"max_tokens"`
	Stream       bool                    `json:"stream"`
	System       []anthropicContentParam `json:"system,omitempty"`
	Tools        []anthropicToolParam    `json:"tools,omitempty"`
	Thinking     *anthropicThinkingParam `json:"thinking,omitempty"`
	OutputConfig map[string]string       `json:"output_config,omitempty"`
	Metadata     map[string]string       `json:"metadata,omitempty"`
	ToolChoice   *anthropicToolChoice    `json:"tool_choice,omitempty"`
	Temperature  *float64                `json:"temperature,omitempty"`
}

type anthropicMessageParam struct {
	Role    string `json:"role"`
	Content any    `json:"content"`
}

type anthropicContentParam struct {
	Type         string              `json:"type"`
	Text         string              `json:"text,omitempty"`
	Thinking     string              `json:"thinking,omitempty"`
	Signature    string              `json:"signature,omitempty"`
	Data         string              `json:"data,omitempty"`
	Source       *agent.ImageSource  `json:"source,omitempty"`
	ID           string              `json:"id,omitempty"`
	Name         string              `json:"name,omitempty"`
	Input        any                 `json:"input,omitempty"`
	ToolUseID    string              `json:"tool_use_id,omitempty"`
	Content      any                 `json:"content,omitempty"`
	IsError      *bool               `json:"is_error,omitempty"`
	CacheControl *agent.CacheControl `json:"cache_control,omitempty"`
}

type anthropicToolParam struct {
	Name                string              `json:"name"`
	Description         string              `json:"description,omitempty"`
	InputSchema         toolInputSchema     `json:"input_schema"`
	EagerInputStreaming bool                `json:"eager_input_streaming,omitempty"`
	CacheControl        *agent.CacheControl `json:"cache_control,omitempty"`
}

type toolInputSchema struct {
	Type       string         `json:"type"`
	Properties any            `json:"properties"`
	Required   []string       `json:"required,omitempty"`
	Extra      map[string]any `json:"-"`
}

func (s toolInputSchema) MarshalJSON() ([]byte, error) {
	out := map[string]any{"type": "object", "properties": s.Properties}
	if len(s.Required) > 0 {
		out["required"] = s.Required
	}
	for key, value := range s.Extra {
		if key != "type" && key != "properties" && key != "required" {
			out[key] = value
		}
	}
	return json.Marshal(out)
}

type anthropicThinkingParam struct {
	Type         string `json:"type"`
	BudgetTokens int    `json:"budget_tokens,omitempty"`
	Display      string `json:"display,omitempty"`
}

type anthropicToolChoice struct {
	Type string `json:"type"`
	Name string `json:"name,omitempty"`
}

func buildAnthropicRequest(req agent.StreamRequest, isOAuth bool) (anthropicRequest, error) {
	maxTokens := req.MaxTokens
	if maxTokens == 0 {
		maxTokens = defaultAnthropicRequestMaxTokens
	}
	cacheControl := &agent.CacheControl{Type: "ephemeral"}
	messages, err := convertMessages(req.Messages, req.Model, isOAuth, cacheControl)
	if err != nil {
		return anthropicRequest{}, err
	}
	payload := anthropicRequest{
		Model:     req.Model,
		Messages:  messages,
		MaxTokens: maxTokens,
		Stream:    true,
	}

	if isOAuth {
		payload.System = append(payload.System, anthropicContentParam{
			Type:         "text",
			Text:         "You are Claude Code, Anthropic's official CLI for Claude.",
			CacheControl: cacheControl,
		})
	}
	if req.System != "" {
		payload.System = append(payload.System, anthropicContentParam{
			Type:         "text",
			Text:         SanitizeUnicode(req.System),
			CacheControl: cacheControl,
		})
	}
	if len(req.Tools) > 0 {
		payload.Tools = convertTools(req.Tools, isOAuth, cacheControl)
	}
	if req.Thinking != nil {
		payload.Thinking = convertThinking(req.Thinking, req.Model)
		if payload.Thinking != nil && payload.Thinking.Type != "disabled" {
			payload.Temperature = nil
		}
	}
	if req.Temperature != nil && (payload.Thinking == nil || payload.Thinking.Type == "disabled") {
		payload.Temperature = req.Temperature
	}
	if req.Thinking != nil && req.Thinking.Effort != "" && payload.Thinking != nil && payload.Thinking.Type == "adaptive" {
		payload.OutputConfig = map[string]string{"effort": req.Thinking.Effort}
	}
	if userID := req.Metadata["user_id"]; userID != "" {
		payload.Metadata = map[string]string{"user_id": userID}
	}
	if req.ToolChoice != nil && req.ToolChoice.Type != "" {
		payload.ToolChoice = &anthropicToolChoice{Type: req.ToolChoice.Type, Name: req.ToolChoice.Name}
		if isOAuth && payload.ToolChoice.Type == "tool" {
			payload.ToolChoice.Name = toClaudeCodeName(payload.ToolChoice.Name)
		}
	}
	return payload, nil
}

func convertThinking(thinking *agent.ThinkingConfig, model string) *anthropicThinkingParam {
	if !thinking.Enabled {
		return &anthropicThinkingParam{Type: "disabled"}
	}
	display := thinking.Display
	if display == "" {
		display = defaultThinkingDisplay
	}
	if thinking.Adaptive || supportsAdaptiveThinking(model) {
		return &anthropicThinkingParam{Type: "adaptive", Display: display}
	}
	budget := thinking.BudgetTokens
	if budget == 0 {
		budget = defaultThinkingBudgetTokens
	}
	return &anthropicThinkingParam{Type: "enabled", BudgetTokens: budget, Display: display}
}

func supportsAdaptiveThinking(model string) bool {
	return strings.Contains(model, "opus-4-6") ||
		strings.Contains(model, "opus-4.6") ||
		strings.Contains(model, "opus-4-7") ||
		strings.Contains(model, "opus-4.7") ||
		strings.Contains(model, "sonnet-4-6") ||
		strings.Contains(model, "sonnet-4.6")
}

func convertMessages(messages []agent.Message, model string, isOAuth bool, cacheControl *agent.CacheControl) ([]anthropicMessageParam, error) {
	transformed := TransformMessages(messages, model)
	params := make([]anthropicMessageParam, 0, len(transformed))
	for i := 0; i < len(transformed); i++ {
		switch msg := transformed[i].(type) {
		case agent.UserMessage:
			content, ok, err := convertUserContent(msg.Content)
			if err != nil {
				return nil, err
			}
			if ok {
				params = append(params, anthropicMessageParam{Role: "user", Content: content})
			}
		case *agent.UserMessage:
			if msg == nil {
				continue
			}
			content, ok, err := convertUserContent(msg.Content)
			if err != nil {
				return nil, err
			}
			if ok {
				params = append(params, anthropicMessageParam{Role: "user", Content: content})
			}
		case agent.AssistantMessage:
			blocks, err := convertAssistantContent(msg.Content, isOAuth)
			if err != nil {
				return nil, err
			}
			if len(blocks) > 0 {
				params = append(params, anthropicMessageParam{Role: "assistant", Content: blocks})
			}
		case *agent.AssistantMessage:
			if msg == nil {
				continue
			}
			blocks, err := convertAssistantContent(msg.Content, isOAuth)
			if err != nil {
				return nil, err
			}
			if len(blocks) > 0 {
				params = append(params, anthropicMessageParam{Role: "assistant", Content: blocks})
			}
		case agent.ToolResultMessage:
			blocks := collectConsecutiveToolResults(transformed, &i)
			params = append(params, anthropicMessageParam{Role: "user", Content: blocks})
		case *agent.ToolResultMessage:
			if msg == nil {
				continue
			}
			blocks := collectConsecutiveToolResults(transformed, &i)
			params = append(params, anthropicMessageParam{Role: "user", Content: blocks})
		case agent.SystemMessage, *agent.SystemMessage:
		default:
			return nil, fmt.Errorf("unsupported message type %T", transformed[i])
		}
	}
	addCacheControlToLastUser(params, cacheControl)
	return params, nil
}

func convertUserContent(content []agent.Content) (any, bool, error) {
	blocks, hasImages, err := convertTextImageBlocks(content)
	if err != nil {
		return nil, false, err
	}
	if !hasImages {
		texts := make([]string, 0, len(blocks))
		for _, block := range blocks {
			if block.Type == "text" && strings.TrimSpace(block.Text) != "" {
				texts = append(texts, block.Text)
			}
		}
		if len(texts) == 0 {
			return nil, false, nil
		}
		return strings.Join(texts, "\n"), true, nil
	}
	filtered := make([]anthropicContentParam, 0, len(blocks))
	for _, block := range blocks {
		if block.Type != "text" || strings.TrimSpace(block.Text) != "" {
			filtered = append(filtered, block)
		}
	}
	return filtered, len(filtered) > 0, nil
}

func convertTextImageBlocks(content []agent.Content) ([]anthropicContentParam, bool, error) {
	blocks := make([]anthropicContentParam, 0, len(content))
	hasImages := false
	for _, block := range content {
		switch value := block.(type) {
		case agent.TextContent:
			blocks = append(blocks, anthropicContentParam{Type: "text", Text: SanitizeUnicode(value.Text)})
		case agent.ImageContent:
			hasImages = true
			source := value.Source
			if source.Type == "" {
				source.Type = "base64"
			}
			blocks = append(blocks, anthropicContentParam{Type: "image", Source: &source})
		default:
			return nil, false, fmt.Errorf("unsupported user content type %T", block)
		}
	}
	if hasImages {
		hasText := false
		for _, block := range blocks {
			if block.Type == "text" {
				hasText = true
				break
			}
		}
		if !hasText {
			blocks = append([]anthropicContentParam{{Type: "text", Text: "(see attached image)"}}, blocks...)
		}
	}
	return blocks, hasImages, nil
}

func convertAssistantContent(content []agent.Content, isOAuth bool) ([]anthropicContentParam, error) {
	blocks := make([]anthropicContentParam, 0, len(content))
	for _, block := range content {
		switch value := block.(type) {
		case agent.TextContent:
			if strings.TrimSpace(value.Text) != "" {
				blocks = append(blocks, anthropicContentParam{Type: "text", Text: SanitizeUnicode(value.Text)})
			}
		case agent.ThinkingContent:
			signature := value.ThinkingSignature
			if signature == "" {
				signature = value.Signature
			}
			if value.Redacted {
				if signature != "" {
					blocks = append(blocks, anthropicContentParam{Type: "redacted_thinking", Data: signature})
				}
				continue
			}
			if strings.TrimSpace(value.Thinking) == "" {
				continue
			}
			if signature == "" {
				blocks = append(blocks, anthropicContentParam{Type: "text", Text: SanitizeUnicode(value.Thinking)})
			} else {
				blocks = append(blocks, anthropicContentParam{Type: "thinking", Thinking: SanitizeUnicode(value.Thinking), Signature: signature})
			}
		case agent.RedactedThinkingContent:
			blocks = append(blocks, anthropicContentParam{Type: "redacted_thinking", Data: value.Data})
		case agent.ToolUseContent:
			input, err := decodeObject(value.Input)
			if err != nil {
				return nil, err
			}
			name := value.Name
			if isOAuth {
				name = toClaudeCodeName(name)
			}
			blocks = append(blocks, anthropicContentParam{Type: "tool_use", ID: value.ID, Name: name, Input: input})
		default:
			return nil, fmt.Errorf("unsupported assistant content type %T", block)
		}
	}
	return blocks, nil
}

func collectConsecutiveToolResults(messages []agent.Message, index *int) []anthropicContentParam {
	blocks := make([]anthropicContentParam, 0)
	for *index < len(messages) {
		results, ok := toolResultsFromMessage(messages[*index])
		if !ok {
			break
		}
		for _, result := range results {
			isError := result.IsError
			blocks = append(blocks, anthropicContentParam{
				Type:      "tool_result",
				ToolUseID: result.ToolUseID,
				Content:   convertToolResultContent(result.Content),
				IsError:   &isError,
			})
		}
		(*index)++
	}
	(*index)--
	return blocks
}

func toolResultsFromMessage(message agent.Message) ([]agent.ToolResult, bool) {
	switch msg := message.(type) {
	case agent.ToolResultMessage:
		return msg.Results, true
	case *agent.ToolResultMessage:
		if msg == nil {
			return nil, false
		}
		return msg.Results, true
	default:
		return nil, false
	}
}

func convertToolResultContent(content []agent.Content) any {
	blocks, hasImages, err := convertTextImageBlocks(content)
	if err != nil || len(blocks) == 0 {
		return ""
	}
	if !hasImages {
		texts := make([]string, 0, len(blocks))
		for _, block := range blocks {
			if block.Type == "text" {
				texts = append(texts, block.Text)
			}
		}
		return strings.Join(texts, "\n")
	}
	return blocks
}

func addCacheControlToLastUser(params []anthropicMessageParam, cacheControl *agent.CacheControl) {
	if cacheControl == nil || len(params) == 0 {
		return
	}
	last := &params[len(params)-1]
	if last.Role != "user" {
		return
	}
	switch content := last.Content.(type) {
	case string:
		last.Content = []anthropicContentParam{{Type: "text", Text: content, CacheControl: cacheControl}}
	case []anthropicContentParam:
		if len(content) == 0 {
			return
		}
		content[len(content)-1].CacheControl = cacheControl
		last.Content = content
	}
}

func convertTools(tools []agent.Tool, isOAuth bool, cacheControl *agent.CacheControl) []anthropicToolParam {
	params := make([]anthropicToolParam, 0, len(tools))
	for i, tool := range tools {
		name := tool.Name()
		if isOAuth {
			name = toClaudeCodeName(name)
		}
		param := anthropicToolParam{
			Name:                name,
			Description:         tool.Description(),
			InputSchema:         convertToolSchema(tool.Schema()),
			EagerInputStreaming: true,
		}
		if cacheControl != nil && i == len(tools)-1 {
			param.CacheControl = cacheControl
		}
		params = append(params, param)
	}
	return params
}

func convertToolSchema(schema json.RawMessage) toolInputSchema {
	var decoded map[string]any
	if len(schema) > 0 {
		_ = json.Unmarshal(schema, &decoded)
	}
	if decoded == nil {
		decoded = map[string]any{}
	}
	result := toolInputSchema{
		Type:       "object",
		Properties: decoded["properties"],
		Extra:      map[string]any{},
	}
	if result.Properties == nil {
		result.Properties = map[string]any{}
	}
	if required, ok := decoded["required"].([]any); ok {
		result.Required = make([]string, 0, len(required))
		for _, item := range required {
			if value, ok := item.(string); ok {
				result.Required = append(result.Required, value)
			}
		}
	}
	for key, value := range decoded {
		result.Extra[key] = value
	}
	return result
}

type rawAnthropicEvent struct {
	Type         string          `json:"type"`
	Index        int             `json:"index"`
	Message      rawMessage      `json:"message"`
	ContentBlock rawContentBlock `json:"content_block"`
	Delta        rawDelta        `json:"delta"`
	Usage        rawUsage        `json:"usage"`
}

type rawMessage struct {
	ID         string   `json:"id"`
	Model      string   `json:"model"`
	Role       string   `json:"role"`
	StopReason string   `json:"stop_reason"`
	Usage      rawUsage `json:"usage"`
}

type rawContentBlock struct {
	Type      string          `json:"type"`
	ID        string          `json:"id"`
	Name      string          `json:"name"`
	Input     json.RawMessage `json:"input"`
	Text      string          `json:"text"`
	Thinking  string          `json:"thinking"`
	Signature string          `json:"signature"`
	Data      string          `json:"data"`
}

type rawDelta struct {
	Type        string `json:"type"`
	Text        string `json:"text"`
	Thinking    string `json:"thinking"`
	PartialJSON string `json:"partial_json"`
	Signature   string `json:"signature"`
	StopReason  string `json:"stop_reason"`
}

type rawUsage struct {
	InputTokens              *int `json:"input_tokens"`
	OutputTokens             *int `json:"output_tokens"`
	CacheReadInputTokens     *int `json:"cache_read_input_tokens"`
	CacheCreationInputTokens *int `json:"cache_creation_input_tokens"`
}

type streamBlock struct {
	providerIndex int
	contentIndex  int
	kind          string
	partialJSON   string
}

func translateStream(ctx context.Context, body io.Reader, req agent.StreamRequest, isOAuth bool, emit func(agent.Event)) (*agent.AssistantMessage, error) {
	message := &agent.AssistantMessage{
		Model:      req.Model,
		API:        apiName,
		Provider:   anthropicProvider,
		StopReason: agent.StopEndTurn,
		Timestamp:  time.Now(),
	}
	if isOAuth {
		message.Provider = anthropicOAuthProvider
	}
	messageID := ""
	responseModel := ""
	blocks := map[int]*streamBlock{}
	sawMessageStart := false
	sawMessageStop := false

	err := iterateSSE(ctx, body, func(sse serverSentEvent) error {
		if sse.Event == "error" {
			return fmt.Errorf("anthropic stream error: %s", strings.TrimSpace(sse.Data))
		}
		if !isAnthropicMessageEvent(sse.Event) {
			return nil
		}
		event, err := parseAnthropicEvent(sse)
		if err != nil {
			return err
		}
		switch event.Type {
		case "message_start":
			sawMessageStart = true
			messageID = event.Message.ID
			message.ResponseID = event.Message.ID
			responseModel = event.Message.Model
			message.ResponseModel = responseModel
			updateUsage(&message.Usage, event.Message.Usage)
			message.Cost = calculateCost(req.Model, message.Usage)
			emit(agent.MessageStartEvent{MessageID: messageID, Role: agent.RoleAssistant, Model: responseModel})
		case "content_block_start":
			handleContentBlockStart(event, req.Tools, isOAuth, messageID, message, blocks, emit)
		case "content_block_delta":
			handleContentBlockDelta(event, messageID, message, blocks, emit)
		case "content_block_stop":
			handleContentBlockStop(event, message, blocks)
		case "message_delta":
			if event.Delta.StopReason != "" {
				message.StopReason = mapStopReason(event.Delta.StopReason)
			}
			updateUsage(&message.Usage, event.Usage)
			message.Cost = calculateCost(req.Model, message.Usage)
			usage := message.Usage
			emit(agent.MessageUpdateEvent{MessageID: messageID, Delta: agent.MessageDelta{Usage: &usage}})
		case "message_stop":
			sawMessageStop = true
			message.Timestamp = time.Now()
			emit(agent.MessageEndEvent{
				MessageID:    messageID,
				FinalContent: message.Content,
				StopReason:   message.StopReason.String(),
				Usage:        message.Usage,
			})
		}
		return nil
	})
	if err != nil {
		message.StopReason = agent.StopError
		message.ErrorMessage = err.Error()
		message.Timestamp = time.Now()
		return message, err
	}
	if sawMessageStart && !sawMessageStop {
		err := errors.New("anthropic stream ended before message_stop")
		message.StopReason = agent.StopError
		message.ErrorMessage = err.Error()
		return message, err
	}
	if responseModel == "" {
		message.ResponseModel = req.Model
	}
	return message, nil
}

func isAnthropicMessageEvent(event string) bool {
	switch event {
	case "message_start", "message_delta", "message_stop", "content_block_start", "content_block_delta", "content_block_stop":
		return true
	default:
		return false
	}
}

func parseAnthropicEvent(sse serverSentEvent) (rawAnthropicEvent, error) {
	var event rawAnthropicEvent
	if err := json.Unmarshal([]byte(sse.Data), &event); err == nil {
		return event, nil
	}
	repaired := RepairJSON(sse.Data)
	if err := json.Unmarshal([]byte(repaired), &event); err != nil {
		return rawAnthropicEvent{}, fmt.Errorf("could not parse Anthropic SSE event %s: %w; data=%s; raw=%s", sse.Event, err, sse.Data, strings.Join(sse.Raw, "\n"))
	}
	return event, nil
}

func handleContentBlockStart(event rawAnthropicEvent, tools []agent.Tool, isOAuth bool, messageID string, message *agent.AssistantMessage, blocks map[int]*streamBlock, emit func(agent.Event)) {
	switch event.ContentBlock.Type {
	case "text":
		index := len(message.Content)
		message.Content = append(message.Content, agent.TextContent{Text: event.ContentBlock.Text})
		blocks[event.Index] = &streamBlock{providerIndex: event.Index, contentIndex: index, kind: "text"}
	case "thinking":
		index := len(message.Content)
		message.Content = append(message.Content, agent.ThinkingContent{Thinking: event.ContentBlock.Thinking, ThinkingSignature: event.ContentBlock.Signature})
		blocks[event.Index] = &streamBlock{providerIndex: event.Index, contentIndex: index, kind: "thinking"}
	case "redacted_thinking":
		index := len(message.Content)
		message.Content = append(message.Content, agent.RedactedThinkingContent{Data: event.ContentBlock.Data})
		blocks[event.Index] = &streamBlock{providerIndex: event.Index, contentIndex: index, kind: "redacted_thinking"}
		emit(agent.MessageUpdateEvent{MessageID: messageID, Delta: agent.MessageDelta{RedactedThinkingDelta: event.ContentBlock.Data, ThinkingDelta: redactedThinkingPlaceholder}})
	case "tool_use":
		name := event.ContentBlock.Name
		if isOAuth {
			name = fromClaudeCodeName(name, tools)
		}
		input := event.ContentBlock.Input
		if len(input) == 0 {
			input = json.RawMessage(`{}`)
		}
		index := len(message.Content)
		message.Content = append(message.Content, agent.ToolUseContent{ID: event.ContentBlock.ID, Name: name, Input: input})
		blocks[event.Index] = &streamBlock{providerIndex: event.Index, contentIndex: index, kind: "tool_use"}
		emit(agent.MessageUpdateEvent{MessageID: messageID, Delta: agent.MessageDelta{ToolUseDelta: &agent.ToolUseDelta{ID: event.ContentBlock.ID, Name: name}}})
	}
}

func handleContentBlockDelta(event rawAnthropicEvent, messageID string, message *agent.AssistantMessage, blocks map[int]*streamBlock, emit func(agent.Event)) {
	block := blocks[event.Index]
	if block == nil || block.contentIndex >= len(message.Content) {
		return
	}
	switch event.Delta.Type {
	case "text_delta":
		if text, ok := message.Content[block.contentIndex].(agent.TextContent); ok {
			text.Text += event.Delta.Text
			message.Content[block.contentIndex] = text
		}
		emit(agent.MessageUpdateEvent{MessageID: messageID, Delta: agent.MessageDelta{TextDelta: event.Delta.Text}})
	case "thinking_delta":
		if thinking, ok := message.Content[block.contentIndex].(agent.ThinkingContent); ok {
			thinking.Thinking += event.Delta.Thinking
			message.Content[block.contentIndex] = thinking
		}
		emit(agent.MessageUpdateEvent{MessageID: messageID, Delta: agent.MessageDelta{ThinkingDelta: event.Delta.Thinking}})
	case "signature_delta":
		if thinking, ok := message.Content[block.contentIndex].(agent.ThinkingContent); ok {
			thinking.ThinkingSignature += event.Delta.Signature
			thinking.Signature = thinking.ThinkingSignature
			message.Content[block.contentIndex] = thinking
		}
		emit(agent.MessageUpdateEvent{MessageID: messageID, Delta: agent.MessageDelta{SignatureDelta: event.Delta.Signature}})
	case "input_json_delta":
		block.partialJSON += event.Delta.PartialJSON
		if toolUse, ok := message.Content[block.contentIndex].(agent.ToolUseContent); ok {
			toolUse.Input = parseStreamingJSON(block.partialJSON)
			message.Content[block.contentIndex] = toolUse
		}
		emit(agent.MessageUpdateEvent{MessageID: messageID, Delta: agent.MessageDelta{ToolUseDelta: &agent.ToolUseDelta{InputJSONPartial: event.Delta.PartialJSON}}})
	}
}

func handleContentBlockStop(event rawAnthropicEvent, message *agent.AssistantMessage, blocks map[int]*streamBlock) {
	block := blocks[event.Index]
	if block == nil || block.kind != "tool_use" || block.contentIndex >= len(message.Content) {
		return
	}
	if toolUse, ok := message.Content[block.contentIndex].(agent.ToolUseContent); ok && block.partialJSON != "" {
		toolUse.Input = parseStreamingJSON(block.partialJSON)
		message.Content[block.contentIndex] = toolUse
	}
}

func updateUsage(usage *agent.Usage, raw rawUsage) {
	if raw.InputTokens != nil {
		usage.InputTokens = *raw.InputTokens
	}
	if raw.OutputTokens != nil {
		usage.OutputTokens = *raw.OutputTokens
	}
	if raw.CacheCreationInputTokens != nil {
		usage.CacheCreationInputTokens = *raw.CacheCreationInputTokens
		usage.CacheWriteTokens = *raw.CacheCreationInputTokens
	}
	if raw.CacheReadInputTokens != nil {
		usage.CacheReadInputTokens = *raw.CacheReadInputTokens
	}
	usage.TotalTokens = usage.InputTokens + usage.OutputTokens + usage.CacheCreationInputTokens + usage.CacheReadInputTokens
}

func mapStopReason(reason string) agent.StopReason {
	switch reason {
	case "end_turn":
		return agent.StopEndTurn
	case "max_tokens":
		return agent.StopMaxTokens
	case "tool_use":
		return agent.StopToolUse
	case "stop_sequence":
		return agent.StopStopSequence
	case "pause_turn":
		return agent.StopPauseTurn
	case "refusal":
		return agent.StopRefusal
	case "overloaded":
		return agent.StopOverloaded
	default:
		return agent.StopError
	}
}

type modelPricing struct {
	input      float64
	output     float64
	cacheRead  float64
	cacheWrite float64
}

var anthropicModelPricing = map[string]modelPricing{
	"claude-3-haiku-20240307":    {input: 0.25, output: 1.25, cacheRead: 0.03, cacheWrite: 0.3},
	"claude-3-sonnet-20240229":   {input: 3, output: 15, cacheRead: 0.3, cacheWrite: 0.3},
	"claude-3-opus-20240229":     {input: 15, output: 75, cacheRead: 1.5, cacheWrite: 18.75},
	"claude-3-5-haiku-20241022":  {input: 0.8, output: 4, cacheRead: 0.08, cacheWrite: 1},
	"claude-3-5-haiku-latest":    {input: 0.8, output: 4, cacheRead: 0.08, cacheWrite: 1},
	"claude-3-5-sonnet-20240620": {input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75},
	"claude-3-5-sonnet-20241022": {input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75},
	"claude-3-7-sonnet-20250219": {input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75},
	"claude-haiku-4-5":           {input: 1, output: 5, cacheRead: 0.1, cacheWrite: 1.25},
	"claude-haiku-4-5-20251001":  {input: 1, output: 5, cacheRead: 0.1, cacheWrite: 1.25},
	"claude-opus-4-0":            {input: 15, output: 75, cacheRead: 1.5, cacheWrite: 18.75},
	"claude-opus-4-1":            {input: 15, output: 75, cacheRead: 1.5, cacheWrite: 18.75},
	"claude-opus-4-1-20250805":   {input: 15, output: 75, cacheRead: 1.5, cacheWrite: 18.75},
	"claude-opus-4-20250514":     {input: 15, output: 75, cacheRead: 1.5, cacheWrite: 18.75},
	"claude-opus-4-5":            {input: 5, output: 25, cacheRead: 0.5, cacheWrite: 6.25},
	"claude-opus-4-5-20251101":   {input: 5, output: 25, cacheRead: 0.5, cacheWrite: 6.25},
	"claude-opus-4-6":            {input: 5, output: 25, cacheRead: 0.5, cacheWrite: 6.25},
	"claude-opus-4-7":            {input: 5, output: 25, cacheRead: 0.5, cacheWrite: 6.25},
	"claude-sonnet-4-0":          {input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75},
	"claude-sonnet-4-20250514":   {input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75},
	"claude-sonnet-4-5":          {input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75},
	"claude-sonnet-4-5-20250929": {input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75},
	"claude-sonnet-4-6":          {input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75},
}

func calculateCost(model string, usage agent.Usage) *agent.Cost {
	pricing, ok := anthropicModelPricing[model]
	if !ok {
		return nil
	}
	cost := &agent.Cost{
		Input:      pricing.input / 1_000_000 * float64(usage.InputTokens),
		Output:     pricing.output / 1_000_000 * float64(usage.OutputTokens),
		CacheRead:  pricing.cacheRead / 1_000_000 * float64(usage.CacheReadInputTokens),
		CacheWrite: pricing.cacheWrite / 1_000_000 * float64(usage.CacheCreationInputTokens),
	}
	cost.Total = cost.Input + cost.Output + cost.CacheRead + cost.CacheWrite
	return cost
}

func decodeObject(raw json.RawMessage) (any, error) {
	if len(raw) == 0 {
		return map[string]any{}, nil
	}
	var decoded any
	if err := json.Unmarshal(raw, &decoded); err != nil {
		return nil, err
	}
	if decoded == nil {
		return map[string]any{}, nil
	}
	return decoded, nil
}
