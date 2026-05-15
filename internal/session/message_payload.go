package session

import (
	"encoding/json"
	"fmt"
	"time"

	"github.com/noeljackson/pi/internal/agent"
)

type messagePayload struct {
	Role               agent.Role                 `json:"role"`
	Content            []contentPayload           `json:"content,omitempty"`
	Results            []toolResultPayload        `json:"results,omitempty"`
	StopReason         agent.StopReason           `json:"stop_reason,omitempty"`
	Model              string                     `json:"model,omitempty"`
	API                string                     `json:"api,omitempty"`
	Provider           string                     `json:"provider,omitempty"`
	ResponseID         string                     `json:"response_id,omitempty"`
	ResponseModel      string                     `json:"response_model,omitempty"`
	ErrorMessage       string                     `json:"error_message,omitempty"`
	Cost               *agent.Cost                `json:"cost,omitempty"`
	Usage              agent.Usage                `json:"usage,omitempty"`
	Timestamp          time.Time                  `json:"timestamp,omitempty"`
	Command            string                     `json:"command,omitempty"`
	Output             string                     `json:"output,omitempty"`
	Stdout             string                     `json:"stdout,omitempty"`
	Stderr             string                     `json:"stderr,omitempty"`
	ExitCode           *int                       `json:"exit_code,omitempty"`
	Cancelled          bool                       `json:"cancelled,omitempty"`
	Truncated          bool                       `json:"truncated,omitempty"`
	FullOutputPath     string                     `json:"full_output_path,omitempty"`
	ExcludeFromContext bool                       `json:"exclude_from_context,omitempty"`
	Kind               string                     `json:"kind,omitempty"`
	Display            bool                       `json:"display,omitempty"`
	Details            json.RawMessage            `json:"details,omitempty"`
	Metadata           map[string]json.RawMessage `json:"metadata,omitempty"`
	Summary            string                     `json:"summary,omitempty"`
	SourceLeafID       string                     `json:"source_leaf_id,omitempty"`
	TokensBefore       int                        `json:"tokens_before,omitempty"`
	DroppedCount       int                        `json:"dropped_count,omitempty"`
	FileOps            json.RawMessage            `json:"file_ops,omitempty"`
	FromHook           bool                       `json:"from_hook,omitempty"`
}

type contentPayload struct {
	Type              string              `json:"type"`
	Text              string              `json:"text,omitempty"`
	TextSignature     string              `json:"textSignature,omitempty"`
	Thinking          string              `json:"thinking,omitempty"`
	Signature         string              `json:"signature,omitempty"`
	ThinkingSignature string              `json:"thinkingSignature,omitempty"`
	Redacted          bool                `json:"redacted,omitempty"`
	Source            *agent.ImageSource  `json:"source,omitempty"`
	ID                string              `json:"id,omitempty"`
	Name              string              `json:"name,omitempty"`
	Input             json.RawMessage     `json:"input,omitempty"`
	Data              string              `json:"data,omitempty"`
	ThoughtSignature  string              `json:"thoughtSignature,omitempty"`
	ToolUseID         string              `json:"tool_use_id,omitempty"`
	Content           []contentPayload    `json:"content,omitempty"`
	IsError           bool                `json:"is_error,omitempty"`
	CacheControl      *agent.CacheControl `json:"cache_control,omitempty"`
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

func customMessageToPayload(message agent.CustomMessage) (json.RawMessage, error) {
	payload, err := messageToPayload(message)
	if err != nil {
		return nil, err
	}
	return json.Marshal(payload)
}

func payloadToCustomMessage(raw json.RawMessage) (agent.CustomMessage, error) {
	message, err := decodeMessagePayload(raw)
	if err != nil {
		return agent.CustomMessage{}, err
	}
	custom, ok := message.(agent.CustomMessage)
	if !ok {
		return agent.CustomMessage{}, fmt.Errorf("custom message payload decoded as %T", message)
	}
	return custom, nil
}

func messageToPayload(message agent.Message) (messagePayload, error) {
	switch msg := message.(type) {
	case agent.UserMessage:
		content, err := contentsToPayload(msg.Content)
		return messagePayload{Role: agent.RoleUser, Content: content, Timestamp: msg.Timestamp}, err
	case *agent.UserMessage:
		content, err := contentsToPayload(msg.Content)
		return messagePayload{Role: agent.RoleUser, Content: content, Timestamp: msg.Timestamp}, err
	case agent.AssistantMessage:
		content, err := contentsToPayload(msg.Content)
		return messagePayload{
			Role:          agent.RoleAssistant,
			Content:       content,
			StopReason:    msg.StopReason,
			Model:         msg.Model,
			API:           msg.API,
			Provider:      msg.Provider,
			ResponseID:    msg.ResponseID,
			ResponseModel: msg.ResponseModel,
			ErrorMessage:  msg.ErrorMessage,
			Cost:          msg.Cost,
			Usage:         msg.Usage,
			Timestamp:     msg.Timestamp,
		}, err
	case *agent.AssistantMessage:
		content, err := contentsToPayload(msg.Content)
		return messagePayload{
			Role:          agent.RoleAssistant,
			Content:       content,
			StopReason:    msg.StopReason,
			Model:         msg.Model,
			API:           msg.API,
			Provider:      msg.Provider,
			ResponseID:    msg.ResponseID,
			ResponseModel: msg.ResponseModel,
			ErrorMessage:  msg.ErrorMessage,
			Cost:          msg.Cost,
			Usage:         msg.Usage,
			Timestamp:     msg.Timestamp,
		}, err
	case agent.ToolResultMessage:
		results, err := toolResultsToPayload(msg.Results)
		return messagePayload{Role: agent.RoleTool, Results: results, Timestamp: msg.Timestamp}, err
	case *agent.ToolResultMessage:
		results, err := toolResultsToPayload(msg.Results)
		return messagePayload{Role: agent.RoleTool, Results: results, Timestamp: msg.Timestamp}, err
	case agent.SystemMessage:
		content, err := contentsToPayload(msg.Content)
		return messagePayload{Role: agent.RoleSystem, Content: content, Timestamp: msg.Timestamp}, err
	case *agent.SystemMessage:
		content, err := contentsToPayload(msg.Content)
		return messagePayload{Role: agent.RoleSystem, Content: content, Timestamp: msg.Timestamp}, err
	case agent.BashExecutionMessage:
		return bashExecutionToPayload(msg), nil
	case *agent.BashExecutionMessage:
		return bashExecutionToPayload(*msg), nil
	case agent.CustomMessage:
		content, err := contentsToPayload(msg.Content)
		return messagePayload{Role: agent.RoleCustom, Content: content, Timestamp: msg.Timestamp, Kind: msg.Kind, Display: msg.Display, Details: msg.Details, Metadata: msg.Metadata}, err
	case *agent.CustomMessage:
		content, err := contentsToPayload(msg.Content)
		return messagePayload{Role: agent.RoleCustom, Content: content, Timestamp: msg.Timestamp, Kind: msg.Kind, Display: msg.Display, Details: msg.Details, Metadata: msg.Metadata}, err
	case agent.BranchSummaryMessage:
		return messagePayload{Role: agent.RoleBranchSummary, Timestamp: msg.Timestamp, Summary: msg.Summary, SourceLeafID: msg.SourceLeafID, Details: msg.Details, FromHook: msg.FromHook}, nil
	case *agent.BranchSummaryMessage:
		return messagePayload{Role: agent.RoleBranchSummary, Timestamp: msg.Timestamp, Summary: msg.Summary, SourceLeafID: msg.SourceLeafID, Details: msg.Details, FromHook: msg.FromHook}, nil
	case agent.CompactionSummaryMessage:
		return messagePayload{Role: agent.RoleCompactionSummary, Timestamp: msg.Timestamp, Summary: msg.Summary, TokensBefore: msg.TokensBefore, DroppedCount: msg.DroppedCount, FileOps: msg.FileOps, Details: msg.Details}, nil
	case *agent.CompactionSummaryMessage:
		return messagePayload{Role: agent.RoleCompactionSummary, Timestamp: msg.Timestamp, Summary: msg.Summary, TokensBefore: msg.TokensBefore, DroppedCount: msg.DroppedCount, FileOps: msg.FileOps, Details: msg.Details}, nil
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
		return agent.UserMessage{Content: content, Timestamp: payload.Timestamp}, nil
	case agent.RoleAssistant:
		content, err := payloadToContents(payload.Content)
		if err != nil {
			return nil, err
		}
		return agent.AssistantMessage{
			Content:       content,
			StopReason:    payload.StopReason,
			Model:         payload.Model,
			API:           payload.API,
			Provider:      payload.Provider,
			ResponseID:    payload.ResponseID,
			ResponseModel: payload.ResponseModel,
			ErrorMessage:  payload.ErrorMessage,
			Cost:          payload.Cost,
			Usage:         payload.Usage,
			Timestamp:     payload.Timestamp,
		}, nil
	case agent.RoleTool:
		results, err := payloadToToolResults(payload.Results)
		if err != nil {
			return nil, err
		}
		return agent.ToolResultMessage{Results: results, Timestamp: payload.Timestamp}, nil
	case agent.RoleSystem:
		content, err := payloadToContents(payload.Content)
		if err != nil {
			return nil, err
		}
		return agent.SystemMessage{Content: content, Timestamp: payload.Timestamp}, nil
	case agent.RoleBashExecution:
		return payloadToBashExecution(payload), nil
	case agent.RoleCustom:
		content, err := payloadToContents(payload.Content)
		if err != nil {
			return nil, err
		}
		return agent.CustomMessage{Timestamp: payload.Timestamp, Kind: payload.Kind, Content: content, Display: payload.Display, Details: payload.Details, Metadata: payload.Metadata}, nil
	case agent.RoleBranchSummary:
		return agent.BranchSummaryMessage{Timestamp: payload.Timestamp, Summary: payload.Summary, SourceLeafID: payload.SourceLeafID, Details: payload.Details, FromHook: payload.FromHook}, nil
	case agent.RoleCompactionSummary:
		return agent.CompactionSummaryMessage{Timestamp: payload.Timestamp, Summary: payload.Summary, TokensBefore: payload.TokensBefore, DroppedCount: payload.DroppedCount, FileOps: payload.FileOps, Details: payload.Details}, nil
	default:
		return nil, fmt.Errorf("unsupported message role %q", payload.Role)
	}
}

func bashExecutionToPayload(msg agent.BashExecutionMessage) messagePayload {
	return messagePayload{
		Role:               agent.RoleBashExecution,
		Timestamp:          msg.Timestamp,
		Command:            msg.Command,
		Output:             msg.Output,
		Stdout:             msg.Stdout,
		Stderr:             msg.Stderr,
		ExitCode:           msg.ExitCode,
		Cancelled:          msg.Cancelled,
		Truncated:          msg.Truncated,
		FullOutputPath:     msg.FullOutputPath,
		ExcludeFromContext: msg.ExcludeFromContext,
	}
}

func payloadToBashExecution(payload messagePayload) agent.BashExecutionMessage {
	return agent.BashExecutionMessage{
		Timestamp:          payload.Timestamp,
		Command:            payload.Command,
		Output:             payload.Output,
		Stdout:             payload.Stdout,
		Stderr:             payload.Stderr,
		ExitCode:           payload.ExitCode,
		Cancelled:          payload.Cancelled,
		Truncated:          payload.Truncated,
		FullOutputPath:     payload.FullOutputPath,
		ExcludeFromContext: payload.ExcludeFromContext,
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
		return contentPayload{Type: "text", Text: block.Text, TextSignature: block.TextSignature, CacheControl: block.CacheControl}, nil
	case *agent.TextContent:
		return contentPayload{Type: "text", Text: block.Text, TextSignature: block.TextSignature, CacheControl: block.CacheControl}, nil
	case agent.ThinkingContent:
		return contentPayload{Type: "thinking", Thinking: block.Thinking, Signature: block.Signature, ThinkingSignature: block.ThinkingSignature, Redacted: block.Redacted}, nil
	case *agent.ThinkingContent:
		return contentPayload{Type: "thinking", Thinking: block.Thinking, Signature: block.Signature, ThinkingSignature: block.ThinkingSignature, Redacted: block.Redacted}, nil
	case agent.ImageContent:
		source := block.Source
		return contentPayload{Type: "image", Source: &source, CacheControl: block.CacheControl}, nil
	case *agent.ImageContent:
		source := block.Source
		return contentPayload{Type: "image", Source: &source, CacheControl: block.CacheControl}, nil
	case agent.ToolUseContent:
		return contentPayload{Type: "tool_use", ID: block.ID, Name: block.Name, Input: block.Input, ThoughtSignature: block.ThoughtSignature}, nil
	case *agent.ToolUseContent:
		return contentPayload{Type: "tool_use", ID: block.ID, Name: block.Name, Input: block.Input, ThoughtSignature: block.ThoughtSignature}, nil
	case agent.ToolResultContent:
		content, err := contentsToPayload(block.Content)
		return contentPayload{Type: "tool_result", ToolUseID: block.ToolUseID, Content: content, IsError: block.IsError}, err
	case *agent.ToolResultContent:
		content, err := contentsToPayload(block.Content)
		return contentPayload{Type: "tool_result", ToolUseID: block.ToolUseID, Content: content, IsError: block.IsError}, err
	case agent.RedactedThinkingContent:
		return contentPayload{Type: "redacted_thinking", Data: block.Data}, nil
	case *agent.RedactedThinkingContent:
		return contentPayload{Type: "redacted_thinking", Data: block.Data}, nil
	case agent.ServerToolUseContent:
		return contentPayload{Type: "server_tool_use", ID: block.ID, Name: block.Name, Input: block.Input}, nil
	case *agent.ServerToolUseContent:
		return contentPayload{Type: "server_tool_use", ID: block.ID, Name: block.Name, Input: block.Input}, nil
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
		return agent.TextContent{Text: payload.Text, TextSignature: payload.TextSignature, CacheControl: payload.CacheControl}, nil
	case "thinking":
		return agent.ThinkingContent{Thinking: payload.Thinking, Signature: payload.Signature, ThinkingSignature: payload.ThinkingSignature, Redacted: payload.Redacted}, nil
	case "image":
		if payload.Source == nil {
			return nil, fmt.Errorf("image content missing source")
		}
		return agent.ImageContent{Source: *payload.Source, CacheControl: payload.CacheControl}, nil
	case "tool_use":
		return agent.ToolUseContent{ID: payload.ID, Name: payload.Name, Input: payload.Input, ThoughtSignature: payload.ThoughtSignature}, nil
	case "tool_result":
		content, err := payloadToContents(payload.Content)
		if err != nil {
			return nil, err
		}
		return agent.ToolResultContent{ToolUseID: payload.ToolUseID, Content: content, IsError: payload.IsError}, nil
	case "redacted_thinking":
		return agent.RedactedThinkingContent{Data: payload.Data}, nil
	case "server_tool_use":
		return agent.ServerToolUseContent{ID: payload.ID, Name: payload.Name, Input: payload.Input}, nil
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
