package anthropic

import (
	"encoding/json"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/noeljackson/pi/internal/agent"
)

const (
	nonVisionUserImagePlaceholder = "(image omitted: model does not support images)"
	nonVisionToolImagePlaceholder = "(tool image omitted: model does not support images)"
)

// SanitizeUnicode removes lone surrogate code points that JSON providers reject.
func SanitizeUnicode(s string) string {
	if s == "" {
		return s
	}
	var builder strings.Builder
	builder.Grow(len(s))
	for len(s) > 0 {
		r, size := utf8.DecodeRuneInString(s)
		if r == utf8.RuneError && size == 1 {
			s = s[size:]
			continue
		}
		if r < 0xD800 || r > 0xDFFF {
			builder.WriteRune(r)
		}
		s = s[size:]
	}
	return builder.String()
}

// RepairJSON performs the same best-effort cleanup needed for streamed tool JSON:
// escape invalid string contents and close unfinished strings, arrays, and objects.
func RepairJSON(input string) string {
	repaired := repairJSONString(input)
	var stack []rune
	inString := false
	escaped := false

	for _, r := range repaired {
		if inString {
			if escaped {
				escaped = false
				continue
			}
			if r == '\\' {
				escaped = true
				continue
			}
			if r == '"' {
				inString = false
			}
			continue
		}

		switch r {
		case '"':
			inString = true
		case '{':
			stack = append(stack, '}')
		case '[':
			stack = append(stack, ']')
		case '}', ']':
			if len(stack) > 0 && stack[len(stack)-1] == r {
				stack = stack[:len(stack)-1]
			}
		}
	}

	if inString {
		repaired += "\""
	}
	for i := len(stack) - 1; i >= 0; i-- {
		repaired += string(stack[i])
	}
	return repaired
}

func repairJSONString(input string) string {
	const validEscapes = `"\/bfnrtu`
	var builder strings.Builder
	builder.Grow(len(input))
	inString := false

	for i := 0; i < len(input); {
		r, size := utf8.DecodeRuneInString(input[i:])
		if !inString {
			builder.WriteRune(r)
			if r == '"' {
				inString = true
			}
			i += size
			continue
		}

		switch r {
		case '"':
			builder.WriteRune(r)
			inString = false
		case '\\':
			if i+size >= len(input) {
				builder.WriteString(`\\`)
				i += size
				continue
			}
			next, nextSize := utf8.DecodeRuneInString(input[i+size:])
			if next == 'u' {
				digitStart := i + size + nextSize
				digitEnd := digitStart + 4
				if digitEnd <= len(input) && isHex4(input[digitStart:digitEnd]) {
					builder.WriteString(input[i:digitEnd])
					i = digitEnd
					continue
				}
			}
			if strings.ContainsRune(validEscapes, next) {
				builder.WriteRune(r)
				builder.WriteRune(next)
				i += size + nextSize
				continue
			}
			builder.WriteString(`\\`)
		case '\b':
			builder.WriteString(`\b`)
		case '\f':
			builder.WriteString(`\f`)
		case '\n':
			builder.WriteString(`\n`)
		case '\r':
			builder.WriteString(`\r`)
		case '\t':
			builder.WriteString(`\t`)
		default:
			if r >= 0 && r <= 0x1f {
				builder.WriteString(`\u00`)
				const hex = "0123456789abcdef"
				builder.WriteByte(hex[byte(r)>>4])
				builder.WriteByte(hex[byte(r)&0x0f])
			} else {
				builder.WriteRune(r)
			}
		}
		i += size
	}
	return builder.String()
}

func isHex4(s string) bool {
	if len(s) != 4 {
		return false
	}
	for _, r := range s {
		if (r < '0' || r > '9') && (r < 'a' || r > 'f') && (r < 'A' || r > 'F') {
			return false
		}
	}
	return true
}

func parseStreamingJSON(partial string) json.RawMessage {
	if strings.TrimSpace(partial) == "" {
		return json.RawMessage(`{}`)
	}
	candidates := []string{partial, repairJSONString(partial), RepairJSON(partial)}
	for _, candidate := range candidates {
		var decoded any
		if err := json.Unmarshal([]byte(candidate), &decoded); err == nil && decoded != nil {
			encoded, err := json.Marshal(decoded)
			if err == nil {
				return encoded
			}
		}
	}
	return json.RawMessage(`{}`)
}

func TransformMessages(messages []agent.Message, model string) []agent.Message {
	imageAware := DowngradeImagesForModel(messages, model)
	normalized := normalizeToolCallIDs(imageAware, model)
	return EnsureSyntheticToolResults(normalized)
}

func DowngradeImagesForModel(messages []agent.Message, model string) []agent.Message {
	if !strings.Contains(strings.ToLower(model), "text-only") {
		return messages
	}
	result := make([]agent.Message, 0, len(messages))
	for _, message := range messages {
		switch msg := message.(type) {
		case agent.UserMessage:
			msg.Content = replaceImagesWithPlaceholder(msg.Content, nonVisionUserImagePlaceholder)
			result = append(result, msg)
		case *agent.UserMessage:
			if msg == nil {
				continue
			}
			copyMsg := *msg
			copyMsg.Content = replaceImagesWithPlaceholder(copyMsg.Content, nonVisionUserImagePlaceholder)
			result = append(result, copyMsg)
		case agent.ToolResultMessage:
			msg.Results = replaceToolResultImages(msg.Results)
			result = append(result, msg)
		case *agent.ToolResultMessage:
			if msg == nil {
				continue
			}
			copyMsg := *msg
			copyMsg.Results = replaceToolResultImages(copyMsg.Results)
			result = append(result, copyMsg)
		default:
			result = append(result, message)
		}
	}
	return result
}

func replaceToolResultImages(results []agent.ToolResult) []agent.ToolResult {
	out := make([]agent.ToolResult, len(results))
	for i, result := range results {
		out[i] = result
		out[i].Content = replaceImagesWithPlaceholder(result.Content, nonVisionToolImagePlaceholder)
	}
	return out
}

func replaceImagesWithPlaceholder(content []agent.Content, placeholder string) []agent.Content {
	result := make([]agent.Content, 0, len(content))
	previousWasPlaceholder := false
	for _, block := range content {
		if _, ok := block.(agent.ImageContent); ok {
			if !previousWasPlaceholder {
				result = append(result, agent.TextContent{Text: placeholder})
			}
			previousWasPlaceholder = true
			continue
		}
		result = append(result, block)
		if text, ok := block.(agent.TextContent); ok {
			previousWasPlaceholder = text.Text == placeholder
		} else {
			previousWasPlaceholder = false
		}
	}
	return result
}

func normalizeToolCallIDs(messages []agent.Message, model string) []agent.Message {
	toolCallIDMap := map[string]string{}
	result := make([]agent.Message, 0, len(messages))
	for _, message := range messages {
		switch msg := message.(type) {
		case agent.AssistantMessage:
			result = append(result, normalizeAssistantToolIDs(msg, model, toolCallIDMap))
		case *agent.AssistantMessage:
			if msg != nil {
				result = append(result, normalizeAssistantToolIDs(*msg, model, toolCallIDMap))
			}
		case agent.ToolResultMessage:
			msg.Results = normalizeToolResultIDs(msg.Results, toolCallIDMap)
			result = append(result, msg)
		case *agent.ToolResultMessage:
			if msg != nil {
				copyMsg := *msg
				copyMsg.Results = normalizeToolResultIDs(copyMsg.Results, toolCallIDMap)
				result = append(result, copyMsg)
			}
		default:
			result = append(result, message)
		}
	}
	return result
}

func normalizeAssistantToolIDs(msg agent.AssistantMessage, model string, idMap map[string]string) agent.AssistantMessage {
	isSameModel := msg.API == "anthropic-messages" && msg.Model == model && msg.StopReason != agent.StopError && msg.StopReason != agent.StopAbort
	content := make([]agent.Content, 0, len(msg.Content))
	for _, block := range msg.Content {
		switch value := block.(type) {
		case agent.ToolUseContent:
			if !isSameModel {
				normalized := normalizeToolCallID(value.ID)
				if normalized != value.ID {
					idMap[value.ID] = normalized
					value.ID = normalized
				}
				value.ThoughtSignature = ""
			}
			content = append(content, value)
		case agent.ThinkingContent:
			if value.Redacted && !isSameModel {
				continue
			}
			if !isSameModel {
				if strings.TrimSpace(value.Thinking) != "" {
					content = append(content, agent.TextContent{Text: value.Thinking})
				}
				continue
			}
			content = append(content, value)
		default:
			content = append(content, block)
		}
	}
	msg.Content = content
	return msg
}

func normalizeToolResultIDs(results []agent.ToolResult, idMap map[string]string) []agent.ToolResult {
	out := make([]agent.ToolResult, len(results))
	for i, result := range results {
		out[i] = result
		if normalized, ok := idMap[result.ToolUseID]; ok {
			out[i].ToolUseID = normalized
		}
	}
	return out
}

func normalizeToolCallID(id string) string {
	var builder strings.Builder
	for _, r := range id {
		if (r >= 'a' && r <= 'z') || (r >= 'A' && r <= 'Z') || (r >= '0' && r <= '9') || r == '_' || r == '-' {
			builder.WriteRune(r)
		} else {
			builder.WriteByte('_')
		}
		if builder.Len() >= 64 {
			break
		}
	}
	return builder.String()
}

func EnsureSyntheticToolResults(messages []agent.Message) []agent.Message {
	result := make([]agent.Message, 0, len(messages))
	var pending []agent.ToolUseContent
	existing := map[string]bool{}

	insertSynthetic := func() {
		for _, call := range pending {
			if !existing[call.ID] {
				result = append(result, agent.ToolResultMessage{
					Results: []agent.ToolResult{{
						ToolUseID: call.ID,
						Content:   []agent.Content{agent.TextContent{Text: "No result provided"}},
						IsError:   true,
					}},
					Timestamp: time.Now(),
				})
			}
		}
		pending = nil
		existing = map[string]bool{}
	}

	for _, message := range messages {
		switch msg := message.(type) {
		case agent.AssistantMessage:
			insertSynthetic()
			if msg.StopReason == agent.StopError || msg.StopReason == agent.StopAbort {
				continue
			}
			pending = collectToolUses(msg.Content)
			result = append(result, msg)
		case *agent.AssistantMessage:
			if msg == nil {
				continue
			}
			insertSynthetic()
			if msg.StopReason == agent.StopError || msg.StopReason == agent.StopAbort {
				continue
			}
			pending = collectToolUses(msg.Content)
			result = append(result, *msg)
		case agent.ToolResultMessage:
			for _, toolResult := range msg.Results {
				existing[toolResult.ToolUseID] = true
			}
			result = append(result, msg)
		case *agent.ToolResultMessage:
			if msg == nil {
				continue
			}
			for _, toolResult := range msg.Results {
				existing[toolResult.ToolUseID] = true
			}
			result = append(result, *msg)
		case agent.UserMessage, *agent.UserMessage:
			insertSynthetic()
			result = append(result, message)
		default:
			result = append(result, message)
		}
	}
	insertSynthetic()
	return result
}

func collectToolUses(content []agent.Content) []agent.ToolUseContent {
	calls := make([]agent.ToolUseContent, 0)
	for _, block := range content {
		if call, ok := block.(agent.ToolUseContent); ok {
			calls = append(calls, call)
		}
	}
	return calls
}
