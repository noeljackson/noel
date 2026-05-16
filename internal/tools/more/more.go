// Package more provides the built-in tool output continuation tool.
package more

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/noeljackson/pi/internal/agent"
	toolcontract "github.com/noeljackson/pi/internal/tools"
)

const defaultLimit = 2000

var moreSchema = json.RawMessage(`{"type":"object","properties":{"call_id":{"type":"string","description":"The tool call ID whose full output is wanted."},"offset":{"type":"integer","default":0},"limit":{"type":"integer","default":2000}},"required":["call_id"],"additionalProperties":false}`)

// Buffer stores untruncated tool output keyed by tool call ID.
type Buffer interface {
	Get(callID string) (text string, ok bool, err error)
	Put(callID string, text string) error
}

// Tool reveals slices of previous tool output.
type Tool struct {
	buffer Buffer
}

// NewTool returns a more tool.
func NewTool(buffer Buffer) *Tool {
	return &Tool{buffer: buffer}
}

func (Tool) Name() string {
	return "more"
}

func (Tool) Description() string {
	return "Retrieve more lines from a previous tool call's full output by call_id."
}

func (Tool) Schema() json.RawMessage {
	return moreSchema
}

func (Tool) ParallelSafe() bool {
	return true
}

func (t *Tool) Execute(_ context.Context, input json.RawMessage, tc agent.ToolCallContext) (agent.ToolResult, error) {
	if t.buffer == nil {
		return agent.ToolResult{}, fmt.Errorf("more buffer is not configured")
	}
	var args struct {
		CallID string `json:"call_id"`
		Offset int    `json:"offset"`
		Limit  int    `json:"limit"`
	}
	if err := json.Unmarshal(input, &args); err != nil {
		return agent.ToolResult{}, err
	}
	if strings.TrimSpace(args.CallID) == "" {
		return agent.ToolResult{}, fmt.Errorf("call_id is required")
	}
	if args.Offset < 0 {
		return agent.ToolResult{}, fmt.Errorf("offset must be >= 0")
	}
	if args.Limit <= 0 {
		args.Limit = defaultLimit
	}

	text, ok, err := t.buffer.Get(args.CallID)
	if err != nil {
		return agent.ToolResult{}, err
	}
	if !ok {
		return textResult(tc.CallID, fmt.Sprintf("No buffered output found for call_id %q.", args.CallID), moreDetails{CallID: args.CallID}, true)
	}
	lines := splitLines(text)
	if args.Offset >= len(lines) {
		return textResult(tc.CallID, fmt.Sprintf("Offset %d is beyond buffered output (%d lines).", args.Offset, len(lines)), moreDetails{CallID: args.CallID, Offset: args.Offset, Limit: args.Limit, TotalLines: len(lines)}, true)
	}
	end := args.Offset + args.Limit
	if end > len(lines) {
		end = len(lines)
	}
	output := strings.Join(lines[args.Offset:end], "\n")
	return textResult(tc.CallID, output, moreDetails{
		CallID:     args.CallID,
		Offset:     args.Offset,
		Limit:      args.Limit,
		TotalLines: len(lines),
		NextOffset: nextOffset(end, len(lines)),
	}, false)
}

type moreDetails struct {
	CallID     string `json:"call_id"`
	Offset     int    `json:"offset,omitempty"`
	Limit      int    `json:"limit,omitempty"`
	TotalLines int    `json:"total_lines,omitempty"`
	NextOffset int    `json:"next_offset,omitempty"`
}

func splitLines(text string) []string {
	if text == "" {
		return []string{""}
	}
	lines := strings.Split(text, "\n")
	if strings.HasSuffix(text, "\n") {
		lines = lines[:len(lines)-1]
	}
	return lines
}

func nextOffset(end int, total int) int {
	if end >= total {
		return 0
	}
	return end
}

func textResult(callID string, text string, details interface{}, isError bool) (agent.ToolResult, error) {
	rawDetails, err := toolcontract.MarshalDetails(details)
	if err != nil {
		return agent.ToolResult{}, err
	}
	return agent.ToolResult{
		ToolUseID: callID,
		Content:   []agent.Content{agent.TextContent{Text: text}},
		Details:   rawDetails,
		IsError:   isError,
	}, nil
}

var _ agent.Tool = (*Tool)(nil)
