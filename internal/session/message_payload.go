package session

import (
	"encoding/json"
	"fmt"

	"github.com/noeljackson/pi/internal/agent"
)

type messagePayload struct {
	Role       agent.Role          `json:"role"`
	Content    []contentPayload    `json:"content,omitempty"`
	Results    []toolResultPayload `json:"results,omitempty"`
	StopReason string              `json:"stop_reason,omitempty"`
	Model      string              `json:"model,omitempty"`
	Usage      agent.Usage         `json:"usage,omitempty"`
}

type contentPayload struct {
	Type      string             `json:"type"`
	Text      string             `json:"text,omitempty"`
	Thinking  string             `json:"thinking,omitempty"`
	Signature string             `json:"signature,omitempty"`
	Source    *agent.ImageSource `json:"source,omitempty"`
	ID        string             `json:"id,omitempty"`
	Name      string             `json:"name,omitempty"`
	Input     json.RawMessage    `json:"input,omitempty"`
	ToolUseID string             `json:"tool_use_id,omitempty"`
	Content   []contentPayload   `json:"content,omitempty"`
	IsError   bool               `json:"is_error,omitempty"`
}

type toolResultPayload struct {
	ToolUseID string           `json:"tool_use_id"`
	Content   []contentPayload `json:"content,omitempty"`
	Details   json.RawMessage  `json:"details,omitempty"`
	IsError   bool             `json:"is_error,omitempty"`
}

func encodeMessagePayload(message agent.Message) (json.RawMessage, error) {
	payload, err := messageToPayload(message)
	if err != nil {
		return nil, err
	}
	raw, err := json.Marshal(payload)
	if err != nil {
		return nil, err
	}
	return raw, nil
}

func decodeMessagePayload(raw json.RawMessage) (agent.Message, error) {
	var payload messagePayload
	if err := json.Unmarshal(raw, &payload); err != nil {
		return nil, err
	}
	return payloadToMessage(payload)
}

func messageToPayload(message agent.Message) (messagePayload, error) {
	switch msg := message.(type) {
	case agent.UserMessage:
		content, err := contentsToPayload(msg.Content)
		return messagePayload{Role: agent.RoleUser, Content: content}, err
	case *agent.UserMessage:
		content, err := contentsToPayload(msg.Content)
		return messagePayload{Role: agent.RoleUser, Content: content}, err
	case agent.AssistantMessage:
		content, err := contentsToPayload(msg.Content)
		return messagePayload{
			Role:       agent.RoleAssistant,
			Content:    content,
			StopReason: msg.StopReason,
			Model:      msg.Model,
			Usage:      msg.Usage,
		}, err
	case *agent.AssistantMessage:
		content, err := contentsToPayload(msg.Content)
		return messagePayload{
			Role:       agent.RoleAssistant,
			Content:    content,
			StopReason: msg.StopReason,
			Model:      msg.Model,
			Usage:      msg.Usage,
		}, err
	case agent.ToolResultMessage:
		results, err := toolResultsToPayload(msg.Results)
		return messagePayload{Role: agent.RoleTool, Results: results}, err
	case *agent.ToolResultMessage:
		results, err := toolResultsToPayload(msg.Results)
		return messagePayload{Role: agent.RoleTool, Results: results}, err
	case agent.SystemMessage:
		content, err := contentsToPayload(msg.Content)
		return messagePayload{Role: agent.RoleSystem, Content: content}, err
	case *agent.SystemMessage:
		content, err := contentsToPayload(msg.Content)
		return messagePayload{Role: agent.RoleSystem, Content: content}, err
	default:
		return messagePayload{}, fmt.Errorf("unsupported message type %T", message)
	}
}

func payloadToMessage(payload messagePayload) (agent.Message, error) {
	switch payload.Role {
	case agent.RoleUser:
		content, err := payloadToContents(payload.Content)
		if err != nil {
			return nil, err
		}
		return agent.UserMessage{Content: content}, nil
	case agent.RoleAssistant:
		content, err := payloadToContents(payload.Content)
		if err != nil {
			return nil, err
		}
		return agent.AssistantMessage{
			Content:    content,
			StopReason: payload.StopReason,
			Model:      payload.Model,
			Usage:      payload.Usage,
		}, nil
	case agent.RoleTool:
		results, err := payloadToToolResults(payload.Results)
		if err != nil {
			return nil, err
		}
		return agent.ToolResultMessage{Results: results}, nil
	case agent.RoleSystem:
		content, err := payloadToContents(payload.Content)
		if err != nil {
			return nil, err
		}
		return agent.SystemMessage{Content: content}, nil
	default:
		return nil, fmt.Errorf("unsupported message role %q", payload.Role)
	}
}

func contentsToPayload(contents []agent.Content) ([]contentPayload, error) {
	payloads := make([]contentPayload, 0, len(contents))
	for _, content := range contents {
		payload, err := contentToPayload(content)
		if err != nil {
			return nil, err
		}
		payloads = append(payloads, payload)
	}
	return payloads, nil
}

func contentToPayload(content agent.Content) (contentPayload, error) {
	switch block := content.(type) {
	case agent.TextContent:
		return contentPayload{Type: "text", Text: block.Text}, nil
	case *agent.TextContent:
		return contentPayload{Type: "text", Text: block.Text}, nil
	case agent.ThinkingContent:
		return contentPayload{Type: "thinking", Thinking: block.Thinking, Signature: block.Signature}, nil
	case *agent.ThinkingContent:
		return contentPayload{Type: "thinking", Thinking: block.Thinking, Signature: block.Signature}, nil
	case agent.ImageContent:
		source := block.Source
		return contentPayload{Type: "image", Source: &source}, nil
	case *agent.ImageContent:
		source := block.Source
		return contentPayload{Type: "image", Source: &source}, nil
	case agent.ToolUseContent:
		return contentPayload{Type: "tool_use", ID: block.ID, Name: block.Name, Input: block.Input}, nil
	case *agent.ToolUseContent:
		return contentPayload{Type: "tool_use", ID: block.ID, Name: block.Name, Input: block.Input}, nil
	case agent.ToolResultContent:
		content, err := contentsToPayload(block.Content)
		return contentPayload{Type: "tool_result", ToolUseID: block.ToolUseID, Content: content, IsError: block.IsError}, err
	case *agent.ToolResultContent:
		content, err := contentsToPayload(block.Content)
		return contentPayload{Type: "tool_result", ToolUseID: block.ToolUseID, Content: content, IsError: block.IsError}, err
	default:
		return contentPayload{}, fmt.Errorf("unsupported content type %T", content)
	}
}

func payloadToContents(payloads []contentPayload) ([]agent.Content, error) {
	contents := make([]agent.Content, 0, len(payloads))
	for _, payload := range payloads {
		content, err := payloadToContent(payload)
		if err != nil {
			return nil, err
		}
		contents = append(contents, content)
	}
	return contents, nil
}

func payloadToContent(payload contentPayload) (agent.Content, error) {
	switch payload.Type {
	case "text":
		return agent.TextContent{Text: payload.Text}, nil
	case "thinking":
		return agent.ThinkingContent{Thinking: payload.Thinking, Signature: payload.Signature}, nil
	case "image":
		if payload.Source == nil {
			return nil, fmt.Errorf("image content missing source")
		}
		return agent.ImageContent{Source: *payload.Source}, nil
	case "tool_use":
		return agent.ToolUseContent{ID: payload.ID, Name: payload.Name, Input: payload.Input}, nil
	case "tool_result":
		content, err := payloadToContents(payload.Content)
		if err != nil {
			return nil, err
		}
		return agent.ToolResultContent{ToolUseID: payload.ToolUseID, Content: content, IsError: payload.IsError}, nil
	default:
		return nil, fmt.Errorf("unsupported content type %q", payload.Type)
	}
}

func toolResultsToPayload(results []agent.ToolResult) ([]toolResultPayload, error) {
	payloads := make([]toolResultPayload, 0, len(results))
	for _, result := range results {
		content, err := contentsToPayload(result.Content)
		if err != nil {
			return nil, err
		}
		payloads = append(payloads, toolResultPayload{
			ToolUseID: result.ToolUseID,
			Content:   content,
			Details:   result.Details,
			IsError:   result.IsError,
		})
	}
	return payloads, nil
}

func payloadToToolResults(payloads []toolResultPayload) ([]agent.ToolResult, error) {
	results := make([]agent.ToolResult, 0, len(payloads))
	for _, payload := range payloads {
		content, err := payloadToContents(payload.Content)
		if err != nil {
			return nil, err
		}
		results = append(results, agent.ToolResult{
			ToolUseID: payload.ToolUseID,
			Content:   content,
			Details:   payload.Details,
			IsError:   payload.IsError,
		})
	}
	return results, nil
}

type currentTurnDisk struct {
	PartialText         string          `json:"partial_text,omitempty"`
	PartialToolUse      *toolUsePayload `json:"partial_tool_use,omitempty"`
	PartialToolUseInput string          `json:"partial_tool_use_input,omitempty"`
}

type toolUsePayload struct {
	ID    string          `json:"id,omitempty"`
	Name  string          `json:"name,omitempty"`
	Input json.RawMessage `json:"input,omitempty"`
}

func currentTurnToDisk(turn CurrentTurn) currentTurnDisk {
	var toolUse *toolUsePayload
	if turn.PartialToolUse != nil {
		toolUse = &toolUsePayload{
			ID:    turn.PartialToolUse.ID,
			Name:  turn.PartialToolUse.Name,
			Input: turn.PartialToolUse.Input,
		}
	}
	return currentTurnDisk{
		PartialText:         turn.PartialText,
		PartialToolUse:      toolUse,
		PartialToolUseInput: turn.PartialToolUseInput,
	}
}

func currentTurnFromDisk(disk currentTurnDisk) CurrentTurn {
	var toolUse *agent.ToolUseContent
	if disk.PartialToolUse != nil {
		toolUse = &agent.ToolUseContent{
			ID:    disk.PartialToolUse.ID,
			Name:  disk.PartialToolUse.Name,
			Input: disk.PartialToolUse.Input,
		}
	}
	return CurrentTurn{
		PartialText:         disk.PartialText,
		PartialToolUse:      toolUse,
		PartialToolUseInput: disk.PartialToolUseInput,
	}
}
