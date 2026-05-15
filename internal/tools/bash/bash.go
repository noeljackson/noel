// Package bash provides the built-in bash tool.
package bash

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"os/exec"
	"syscall"
	"time"

	"github.com/noeljackson/pi/internal/agent"
)

const (
	defaultTimeout = 120000
	maxOutputBytes = 30 * 1024
)

var bashSchema = json.RawMessage(`{"type":"object","properties":{"command":{"type":"string"},"timeout_ms":{"type":"integer"},"cwd":{"type":"string"}},"required":["command"],"additionalProperties":false}`)

// Tool executes bash commands.
type Tool struct{}

// NewTool returns a bash tool.
func NewTool() *Tool {
	return &Tool{}
}

func (Tool) Name() string {
	return "bash"
}

func (Tool) Description() string {
	return "Execute a bash command and return combined stdout and stderr."
}

func (Tool) Schema() json.RawMessage {
	return bashSchema
}

func (Tool) ParallelSafe() bool {
	return true
}

func (Tool) Execute(ctx context.Context, input json.RawMessage, tc agent.ToolCallContext) (agent.ToolResult, error) {
	var args struct {
		Command   string `json:"command"`
		TimeoutMS int    `json:"timeout_ms"`
		Cwd       string `json:"cwd"`
	}
	if err := json.Unmarshal(input, &args); err != nil {
		return agent.ToolResult{}, err
	}
	if args.Command == "" {
		return agent.ToolResult{}, fmt.Errorf("command is required")
	}
	timeout := args.TimeoutMS
	if timeout <= 0 {
		timeout = defaultTimeout
	}
	cwd := tc.Cwd
	if args.Cwd != "" {
		cwd = args.Cwd
	}

	start := time.Now()
	cmd := exec.Command("bash", "-c", args.Command)
	if cwd != "" {
		cmd.Dir = cwd
	}
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}

	chunks := make(chan outputChunk, 128)
	cmd.Stdout = streamWriter{stream: "stdout", chunks: chunks}
	cmd.Stderr = streamWriter{stream: "stderr", chunks: chunks}

	if err := cmd.Start(); err != nil {
		return agent.ToolResult{}, err
	}

	waitCh := make(chan error, 1)
	go func() {
		waitCh <- cmd.Wait()
	}()

	timer := time.NewTimer(time.Duration(timeout) * time.Millisecond)
	defer timer.Stop()
	ticker := time.NewTicker(250 * time.Millisecond)
	defer ticker.Stop()

	var combined bytes.Buffer
	var pendingStdout bytes.Buffer
	var pendingStderr bytes.Buffer
	stdoutBytes := 0
	stderrBytes := 0
	timedOut := false
	cancelled := false

	emit := func(exitCode *int) {
		if tc.OnUpdate == nil {
			pendingStdout.Reset()
			pendingStderr.Reset()
			return
		}
		if pendingStdout.Len() == 0 && pendingStderr.Len() == 0 && exitCode == nil {
			return
		}
		update := map[string]interface{}{
			"stdout":    pendingStdout.String(),
			"stderr":    pendingStderr.String(),
			"exit_code": exitCode,
		}
		encoded, err := json.Marshal(update)
		if err == nil {
			tc.OnUpdate(encoded)
		}
		pendingStdout.Reset()
		pendingStderr.Reset()
	}

	record := func(chunk outputChunk) {
		combined.Write(chunk.data)
		if chunk.stream == "stdout" {
			stdoutBytes += len(chunk.data)
			pendingStdout.Write(chunk.data)
		} else {
			stderrBytes += len(chunk.data)
			pendingStderr.Write(chunk.data)
		}
	}

	var waitErr error
	running := true
	for running {
		select {
		case chunk := <-chunks:
			record(chunk)
		case <-ticker.C:
			emit(nil)
		case <-timer.C:
			timedOut = true
			killProcessGroup(cmd)
		case <-ctx.Done():
			cancelled = true
			killProcessGroup(cmd)
		case waitErr = <-waitCh:
			running = false
		}
	}
	for {
		select {
		case chunk := <-chunks:
			record(chunk)
		default:
			goto drained
		}
	}

drained:
	exitCode := commandExitCode(waitErr)
	if timedOut || cancelled {
		exitCode = -1
	}
	emit(&exitCode)

	output := truncateOutput(combined.String())
	if timedOut {
		output = appendStatus(output, fmt.Sprintf("Command timed out after %dms", timeout))
	} else if cancelled {
		output = appendStatus(output, "Command cancelled")
	}
	duration := time.Since(start).Milliseconds()
	details, err := json.Marshal(map[string]interface{}{
		"exit_code":    exitCode,
		"stdout_bytes": stdoutBytes,
		"stderr_bytes": stderrBytes,
		"command":      args.Command,
		"duration_ms":  duration,
	})
	if err != nil {
		return agent.ToolResult{}, err
	}
	return agent.ToolResult{
		ToolUseID: tc.CallID,
		Content:   []agent.Content{agent.TextContent{Text: output}},
		Details:   details,
		IsError:   exitCode != 0,
	}, nil
}

type outputChunk struct {
	stream string
	data   []byte
}

type streamWriter struct {
	stream string
	chunks chan<- outputChunk
}

func (w streamWriter) Write(data []byte) (int, error) {
	copied := append([]byte(nil), data...)
	w.chunks <- outputChunk{stream: w.stream, data: copied}
	return len(data), nil
}

func killProcessGroup(cmd *exec.Cmd) {
	if cmd.Process == nil {
		return
	}
	pid := cmd.Process.Pid
	if pid <= 0 {
		return
	}
	if err := syscall.Kill(-pid, syscall.SIGKILL); err != nil {
		_ = cmd.Process.Kill()
	}
}

func commandExitCode(err error) int {
	if err == nil {
		return 0
	}
	if exitErr, ok := err.(*exec.ExitError); ok {
		return exitErr.ExitCode()
	}
	return -1
}

func truncateOutput(text string) string {
	if len(text) <= maxOutputBytes {
		return text
	}
	const marker = "\n[truncated]\n"
	if maxOutputBytes <= len(marker) {
		return text[:maxOutputBytes]
	}
	start := len(text) - (maxOutputBytes - len(marker))
	return marker + text[start:]
}

func appendStatus(text string, status string) string {
	if text == "" {
		return status
	}
	return text + "\n\n" + status
}

var _ agent.Tool = (*Tool)(nil)
