// Package rpc implements pi's JSONL RPC mode.
package rpc

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"sync"

	"github.com/noeljackson/pi/internal/agent"
	"github.com/noeljackson/pi/internal/cli"
	"github.com/noeljackson/pi/internal/cli/modes"
)

// Run starts the JSONL RPC server.
func Run(ctx context.Context, opts cli.Options, runner *agent.Agent, in io.Reader, out io.Writer) error {
	if err := modes.ApplyOptions(runner, opts); err != nil {
		return err
	}

	writer := &jsonlWriter{out: out}
	unsubscribe := runner.Subscribe(func(event agent.Event) {
		data, err := modes.MarshalEvent(event)
		if err == nil {
			_ = writer.writeRaw(data)
		}
	})
	defer unsubscribe()

	scanner := bufio.NewScanner(in)
	scanner.Buffer(make([]byte, 0, 64*1024), 10*1024*1024)
	for scanner.Scan() {
		line := scanner.Bytes()
		if len(line) == 0 {
			continue
		}
		var command rpcCommand
		if err := json.Unmarshal(line, &command); err != nil {
			_ = writer.write(response("", "parse", false, nil, fmt.Sprintf("Failed to parse command: %s", err)))
			continue
		}
		if command.Type == "" {
			_ = writer.write(response(command.ID, "unknown", false, nil, "command type is required"))
			continue
		}
		if command.Type == "quit" {
			_ = writer.write(response(command.ID, "quit", true, nil, ""))
			return nil
		}
		if err := handleCommand(ctx, runner, writer, command); err != nil {
			return err
		}
	}
	if err := scanner.Err(); err != nil {
		return err
	}
	return nil
}

type rpcCommand struct {
	ID                string          `json:"id,omitempty"`
	Type              string          `json:"type"`
	Message           string          `json:"message,omitempty"`
	Provider          string          `json:"provider,omitempty"`
	ModelID           string          `json:"modelId,omitempty"`
	Model             string          `json:"model,omitempty"`
	Level             string          `json:"level,omitempty"`
	EntryID           string          `json:"entryId,omitempty"`
	SessionPath       string          `json:"sessionPath,omitempty"`
	Name              string          `json:"name,omitempty"`
	StreamingBehavior string          `json:"streamingBehavior,omitempty"`
	Raw               json.RawMessage `json:"-"`
}

func handleCommand(ctx context.Context, runner *agent.Agent, writer *jsonlWriter, command rpcCommand) error {
	switch command.Type {
	case "prompt":
		if command.Message == "" {
			return writer.write(response(command.ID, command.Type, false, nil, "message is required"))
		}
		if err := runner.Prompt(ctx, command.Message); err != nil {
			return writer.write(response(command.ID, command.Type, false, nil, err.Error()))
		}
		if err := writer.write(response(command.ID, command.Type, true, nil, "")); err != nil {
			return err
		}
		if err := runner.WaitForIdle(ctx); err != nil {
			return writer.write(response(command.ID, command.Type, false, nil, err.Error()))
		}
		if err := runner.LastError(); err != nil {
			return writer.write(response(command.ID, command.Type, false, nil, err.Error()))
		}
		return nil
	case "steer":
		if command.Message == "" {
			return writer.write(response(command.ID, command.Type, false, nil, "message is required"))
		}
		if err := runner.Steer(ctx, command.Message); err != nil {
			return writer.write(response(command.ID, command.Type, false, nil, err.Error()))
		}
		return writer.write(response(command.ID, command.Type, true, nil, ""))
	case "follow_up":
		if command.Message == "" {
			return writer.write(response(command.ID, command.Type, false, nil, "message is required"))
		}
		if err := runner.FollowUp(ctx, command.Message); err != nil {
			return writer.write(response(command.ID, command.Type, false, nil, err.Error()))
		}
		return writer.write(response(command.ID, command.Type, true, nil, ""))
	case "abort":
		runner.Abort()
		return writer.write(response(command.ID, command.Type, true, nil, ""))
	case "set_model":
		model := command.Model
		if model == "" {
			model = command.ModelID
		}
		if command.Provider != "" && model != "" {
			model = command.Provider + "/" + model
		}
		if model == "" {
			return writer.write(response(command.ID, command.Type, false, nil, "model is required"))
		}
		if err := runner.SetModel(model); err != nil {
			return writer.write(response(command.ID, command.Type, false, nil, err.Error()))
		}
		return writer.write(response(command.ID, command.Type, true, map[string]any{"id": model}, ""))
	case "set_thinking", "set_thinking_level":
		if command.Level == "" {
			return writer.write(response(command.ID, command.Type, false, nil, "level is required"))
		}
		if err := runner.SetThinking(command.Level); err != nil {
			return writer.write(response(command.ID, command.Type, false, nil, err.Error()))
		}
		return writer.write(response(command.ID, command.Type, true, nil, ""))
	case "get_state":
		data := map[string]any{
			"isStreaming":           runner.State() == agent.AgentStreaming || runner.State() == agent.AgentToolExecution,
			"isCompacting":          false,
			"steeringMode":          "all",
			"followUpMode":          "all",
			"autoCompactionEnabled": false,
			"pendingMessageCount":   len(runner.Queue()),
		}
		return writer.write(response(command.ID, command.Type, true, data, ""))
	case "get_messages":
		return writer.write(response(command.ID, command.Type, true, map[string]any{"messages": []any{}}, ""))
	case "get_last_assistant_text":
		streaming := runner.StreamingMessage()
		return writer.write(response(command.ID, command.Type, true, map[string]any{"text": assistantText(streaming)}, ""))
	case "fork", "session_fork", "session_move", "switch_session", "new_session", "clone":
		return writer.write(response(command.ID, command.Type, false, nil, "session tree RPC commands are not implemented in the Go CLI yet"))
	default:
		return writer.write(response(command.ID, command.Type, false, nil, fmt.Sprintf("Unknown command: %s", command.Type)))
	}
}

func assistantText(message *agent.AssistantMessage) *string {
	if message == nil {
		return nil
	}
	text := ""
	for _, content := range message.Content {
		if block, ok := content.(agent.TextContent); ok {
			text += block.Text
		}
	}
	return &text
}

type jsonlWriter struct {
	mu  sync.Mutex
	out io.Writer
}

func (w *jsonlWriter) write(value map[string]any) error {
	data, err := json.Marshal(value)
	if err != nil {
		return err
	}
	return w.writeRaw(data)
}

func (w *jsonlWriter) writeRaw(data []byte) error {
	w.mu.Lock()
	defer w.mu.Unlock()
	if len(data) == 0 {
		return errors.New("empty JSONL record")
	}
	_, err := fmt.Fprintf(w.out, "%s\n", data)
	return err
}

func response(id, command string, success bool, data any, message string) map[string]any {
	out := map[string]any{
		"type":    "response",
		"command": command,
		"success": success,
	}
	if id != "" {
		out["id"] = id
	}
	if success {
		if data != nil {
			out["data"] = data
		}
	} else {
		out["error"] = message
	}
	return out
}
