package anthropic

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"

	anthropicsdk "github.com/anthropics/anthropic-sdk-go"
	"github.com/anthropics/anthropic-sdk-go/option"

	"github.com/noeljackson/pi/internal/agent"
)

type Client struct {
	auth AuthSource
}

func NewClient(auth AuthSource) *Client {
	return &Client{auth: auth}
}

func (c *Client) Stream(ctx context.Context, req agent.StreamRequest, emit func(agent.Event)) (*agent.AssistantMessage, error) {
	params, err := buildMessageParams(req)
	if err != nil {
		return nil, err
	}

	opts, err := c.requestOptions(ctx)
	if err != nil {
		return nil, err
	}
	client := anthropicsdk.NewClient(opts...)
	stream := client.Messages.NewStreaming(ctx, params)
	accumulated := anthropicsdk.Message{}
	messageID := ""

	for stream.Next() {
		event := stream.Current()
		if err := accumulated.Accumulate(event); err != nil {
			return nil, err
		}

		switch event := event.AsAny().(type) {
		case anthropicsdk.MessageStartEvent:
			messageID = event.Message.ID
			emit(agent.MessageStartEvent{
				MessageID: messageID,
				Role:      agent.RoleAssistant,
				Model:     string(event.Message.Model),
			})
		case anthropicsdk.ContentBlockStartEvent:
			if toolUse, ok := event.ContentBlock.AsAny().(anthropicsdk.ToolUseBlock); ok {
				emit(agent.MessageUpdateEvent{
					MessageID: messageID,
					Delta: agent.MessageDelta{ToolUseDelta: &agent.ToolUseDelta{
						ID:   toolUse.ID,
						Name: toolUse.Name,
					}},
				})
			}
		case anthropicsdk.ContentBlockDeltaEvent:
			emitContentDelta(messageID, event, emit)
		case anthropicsdk.MessageDeltaEvent:
			accumulated.StopReason = event.Delta.StopReason
			accumulated.Usage.OutputTokens = event.Usage.OutputTokens
		case anthropicsdk.MessageStopEvent:
			message := convertAssistantMessage(accumulated)
			emit(agent.MessageEndEvent{
				MessageID:    messageID,
				FinalContent: message.Content,
				StopReason:   message.StopReason,
				Usage:        message.Usage,
			})
		case anthropicsdk.ContentBlockStopEvent:
		}
	}
	if err := stream.Err(); err != nil {
		return nil, err
	}

	message := convertAssistantMessage(accumulated)
	return &message, nil
}

func (c *Client) requestOptions(ctx context.Context) ([]option.RequestOption, error) {
	if c.auth == nil {
		return nil, errors.New("anthropic: missing auth source")
	}
	headers, err := c.auth.Headers(ctx)
	if err != nil {
		return nil, err
	}

	opts := []option.RequestOption{option.WithoutEnvironmentDefaults()}
	if key, ok := apiKeyFromAuth(c.auth); ok {
		return append(opts, option.WithAPIKey(key)), nil
	}

	for key, value := range headers {
		if strings.EqualFold(key, "anthropic-beta") {
			opts = append(opts, option.WithHeaderAdd(key, value))
			continue
		}
		opts = append(opts, option.WithHeader(key, value))
	}
	return opts, nil
}

func apiKeyFromAuth(auth AuthSource) (string, bool) {
	switch a := auth.(type) {
	case APIKeyAuth:
		return a.Key, true
	case *APIKeyAuth:
		return a.Key, true
	default:
		return "", false
	}
}

func emitContentDelta(messageID string, event anthropicsdk.ContentBlockDeltaEvent, emit func(agent.Event)) {
	switch delta := event.Delta.AsAny().(type) {
	case anthropicsdk.TextDelta:
		emit(agent.MessageUpdateEvent{
			MessageID: messageID,
			Delta:     agent.MessageDelta{TextDelta: delta.Text},
		})
	case anthropicsdk.InputJSONDelta:
		emit(agent.MessageUpdateEvent{
			MessageID: messageID,
			Delta: agent.MessageDelta{ToolUseDelta: &agent.ToolUseDelta{
				InputJSONPartial: delta.PartialJSON,
			}},
		})
	case anthropicsdk.ThinkingDelta:
		emit(agent.MessageUpdateEvent{
			MessageID: messageID,
			Delta:     agent.MessageDelta{ThinkingDelta: delta.Thinking},
		})
	}
}

func buildMessageParams(req agent.StreamRequest) (anthropicsdk.MessageNewParams, error) {
	maxTokens := req.MaxTokens
	if maxTokens == 0 {
		maxTokens = 4096
	}

	messages, err := convertMessages(req.Messages)
	if err != nil {
		return anthropicsdk.MessageNewParams{}, err
	}

	params := anthropicsdk.MessageNewParams{
		Model:     anthropicsdk.Model(req.Model),
		MaxTokens: int64(maxTokens),
		Messages:  messages,
		Tools:     convertTools(req.Tools),
	}
	if req.System != "" {
		params.System = []anthropicsdk.TextBlockParam{{Text: req.System}}
	}
	return params, nil
}

func convertMessages(messages []agent.Message) ([]anthropicsdk.MessageParam, error) {
	params := make([]anthropicsdk.MessageParam, 0, len(messages))
	for _, message := range messages {
		switch message := message.(type) {
		case agent.UserMessage:
			blocks, err := convertContentBlocks(message.Content)
			if err != nil {
				return nil, err
			}
			params = append(params, anthropicsdk.NewUserMessage(blocks...))
		case agent.AssistantMessage:
			blocks, err := convertContentBlocks(message.Content)
			if err != nil {
				return nil, err
			}
			params = append(params, anthropicsdk.NewAssistantMessage(blocks...))
		case agent.ToolResultMessage:
			params = append(params, anthropicsdk.NewUserMessage(convertToolResults(message.Results)...))
		case agent.SystemMessage:
		default:
			return nil, fmt.Errorf("unsupported message type %T", message)
		}
	}
	return params, nil
}

func convertContentBlocks(content []agent.Content) ([]anthropicsdk.ContentBlockParamUnion, error) {
	blocks := make([]anthropicsdk.ContentBlockParamUnion, 0, len(content))
	for _, block := range content {
		switch block := block.(type) {
		case agent.TextContent:
			blocks = append(blocks, anthropicsdk.ContentBlockParamUnion{
				OfText: &anthropicsdk.TextBlockParam{Text: block.Text},
			})
		case agent.ThinkingContent:
			blocks = append(blocks, anthropicsdk.ContentBlockParamUnion{
				OfThinking: &anthropicsdk.ThinkingBlockParam{
					Thinking:  block.Thinking,
					Signature: block.Signature,
				},
			})
		case agent.ToolUseContent:
			input, err := decodeObject(block.Input)
			if err != nil {
				return nil, err
			}
			blocks = append(blocks, anthropicsdk.ContentBlockParamUnion{
				OfToolUse: &anthropicsdk.ToolUseBlockParam{
					ID:    block.ID,
					Name:  block.Name,
					Input: input,
				},
			})
		default:
			return nil, fmt.Errorf("unsupported content type %T", block)
		}
	}
	return blocks, nil
}

func convertToolResults(results []agent.ToolResult) []anthropicsdk.ContentBlockParamUnion {
	blocks := make([]anthropicsdk.ContentBlockParamUnion, 0, len(results))
	for _, result := range results {
		toolResult := anthropicsdk.ToolResultBlockParam{
			ToolUseID: result.ToolUseID,
			IsError:   anthropicsdk.Bool(result.IsError),
			Content:   convertToolResultContent(result.Content),
		}
		blocks = append(blocks, anthropicsdk.ContentBlockParamUnion{OfToolResult: &toolResult})
	}
	return blocks
}

func convertToolResultContent(content []agent.Content) []anthropicsdk.ToolResultBlockParamContentUnion {
	blocks := make([]anthropicsdk.ToolResultBlockParamContentUnion, 0, len(content))
	for _, block := range content {
		if text, ok := block.(agent.TextContent); ok {
			blocks = append(blocks, anthropicsdk.ToolResultBlockParamContentUnion{
				OfText: &anthropicsdk.TextBlockParam{Text: text.Text},
			})
		}
	}
	if len(blocks) == 0 {
		blocks = append(blocks, anthropicsdk.ToolResultBlockParamContentUnion{
			OfText: &anthropicsdk.TextBlockParam{Text: ""},
		})
	}
	return blocks
}

func convertTools(tools []agent.Tool) []anthropicsdk.ToolUnionParam {
	if len(tools) == 0 {
		return nil
	}
	params := make([]anthropicsdk.ToolUnionParam, 0, len(tools))
	for _, tool := range tools {
		toolParam := anthropicsdk.ToolParam{
			Name:        tool.Name(),
			Description: anthropicsdk.String(tool.Description()),
			InputSchema: convertToolSchema(tool.Schema()),
		}
		params = append(params, anthropicsdk.ToolUnionParam{OfTool: &toolParam})
	}
	return params
}

func convertToolSchema(schema json.RawMessage) anthropicsdk.ToolInputSchemaParam {
	var decoded map[string]any
	if len(schema) > 0 {
		_ = json.Unmarshal(schema, &decoded)
	}
	if decoded == nil {
		decoded = map[string]any{}
	}

	param := anthropicsdk.ToolInputSchemaParam{
		Properties:  decoded["properties"],
		ExtraFields: map[string]any{},
	}
	if required, ok := decoded["required"].([]any); ok {
		param.Required = make([]string, 0, len(required))
		for _, item := range required {
			if value, ok := item.(string); ok {
				param.Required = append(param.Required, value)
			}
		}
	}
	for key, value := range decoded {
		if key != "type" && key != "properties" && key != "required" {
			param.ExtraFields[key] = value
		}
	}
	return param
}

func convertAssistantMessage(message anthropicsdk.Message) agent.AssistantMessage {
	return agent.AssistantMessage{
		Content:    convertResponseContent(message.Content),
		StopReason: string(message.StopReason),
		Model:      string(message.Model),
		Usage:      convertUsage(message.Usage),
	}
}

func convertResponseContent(content []anthropicsdk.ContentBlockUnion) []agent.Content {
	blocks := make([]agent.Content, 0, len(content))
	for _, block := range content {
		switch block := block.AsAny().(type) {
		case anthropicsdk.TextBlock:
			blocks = append(blocks, agent.TextContent{Text: block.Text})
		case anthropicsdk.ThinkingBlock:
			blocks = append(blocks, agent.ThinkingContent{
				Thinking:  block.Thinking,
				Signature: block.Signature,
			})
		case anthropicsdk.ToolUseBlock:
			blocks = append(blocks, agent.ToolUseContent{
				ID:    block.ID,
				Name:  block.Name,
				Input: block.Input,
			})
		}
	}
	return blocks
}

func convertUsage(usage anthropicsdk.Usage) agent.Usage {
	return agent.Usage{
		InputTokens:              int(usage.InputTokens),
		OutputTokens:             int(usage.OutputTokens),
		CacheCreationInputTokens: int(usage.CacheCreationInputTokens),
		CacheReadInputTokens:     int(usage.CacheReadInputTokens),
	}
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
