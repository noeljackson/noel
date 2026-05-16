// Package task provides the built-in subagent delegation tool.
package task

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"sync"
	"time"

	"github.com/noeljackson/pi/internal/agent"
	toolcontract "github.com/noeljackson/pi/internal/tools"
)

const (
	defaultConcurrency = 4
	maxParallelTasks   = 8
	previewLimit       = 160
)

var taskSchema = json.RawMessage(`{"type":"object","properties":{"prompt":{"type":"string","description":"Task instructions for a focused sub-agent."},"tools":{"type":"array","items":{"type":"string"},"description":"Allowed tools for the sub-agent."},"model":{"type":"string"},"system_prompt":{"type":"string"},"max_turns":{"type":"integer"},"concurrency":{"type":"integer","default":4},"parallel":{"type":"array","items":{"type":"object","properties":{"prompt":{"type":"string"},"tools":{"type":"array","items":{"type":"string"}},"model":{"type":"string"},"system_prompt":{"type":"string"},"max_turns":{"type":"integer"}},"required":["prompt"],"additionalProperties":false},"description":"Run multiple subtasks in parallel."}},"required":["prompt"],"additionalProperties":false}`)

// Spawner starts isolated sub-agents for delegated work.
type Spawner interface {
	Spawn(ctx context.Context, req SpawnRequest) (Result, error)
}

// SpawnRequest describes one sub-agent run.
type SpawnRequest struct {
	Prompt       string
	Tools        []string
	Model        string
	SystemPrompt string
	MaxTurns     int
	Concurrency  int
}

// Result is the final output of a sub-agent run.
type Result struct {
	Output       string
	SessionID    string
	DurationMS   int
	InputTokens  int
	OutputTokens int
}

// Tool delegates focused work to fresh agent sessions.
type Tool struct {
	spawner Spawner
}

// NewTool returns a task delegation tool.
func NewTool(spawner Spawner) *Tool {
	return &Tool{spawner: spawner}
}

func (Tool) Name() string {
	return "task"
}

func (Tool) Description() string {
	return "Delegate a focused task to an isolated sub-agent. Supports a single task or parallel subtasks with an optional tool allow-list and model override."
}

func (Tool) Schema() json.RawMessage {
	return taskSchema
}

func (Tool) ParallelSafe() bool {
	return false
}

func (t *Tool) Execute(ctx context.Context, input json.RawMessage, tc agent.ToolCallContext) (agent.ToolResult, error) {
	if t.spawner == nil {
		return agent.ToolResult{}, fmt.Errorf("task spawner is not configured")
	}

	var args toolArgs
	if err := json.Unmarshal(input, &args); err != nil {
		return agent.ToolResult{}, err
	}
	if strings.TrimSpace(args.Prompt) == "" {
		return agent.ToolResult{}, fmt.Errorf("prompt is required")
	}
	if len(args.Parallel) > maxParallelTasks {
		return textResult(tc.CallID, fmt.Sprintf("Too many parallel tasks (%d). Max is %d.", len(args.Parallel), maxParallelTasks), nil, true)
	}

	if len(args.Parallel) == 0 {
		return t.runSingle(ctx, args.toRequest(), tc)
	}
	return t.runParallel(ctx, args, tc)
}

func (t *Tool) runSingle(ctx context.Context, req SpawnRequest, tc agent.ToolCallContext) (agent.ToolResult, error) {
	emitProgress(tc, 0, "running", "")
	start := time.Now()
	result, err := t.spawner.Spawn(ctx, req)
	if result.DurationMS == 0 {
		result.DurationMS = int(time.Since(start).Milliseconds())
	}
	if err != nil {
		text := fmt.Sprintf("Task failed: %s", err)
		emitProgress(tc, 0, "done", text)
		return textResult(tc.CallID, text, detailsFromResults([]Result{result}), true)
	}
	emitProgress(tc, 0, "done", result.Output)
	return textResult(tc.CallID, formatSingleResult(result), detailsFromResults([]Result{result}), false)
}

func (t *Tool) runParallel(ctx context.Context, args toolArgs, tc agent.ToolCallContext) (agent.ToolResult, error) {
	concurrency := args.Concurrency
	if concurrency <= 0 {
		concurrency = defaultConcurrency
	}
	if concurrency > len(args.Parallel) {
		concurrency = len(args.Parallel)
	}
	if concurrency < 1 {
		concurrency = 1
	}

	results := make([]Result, len(args.Parallel))
	errorsByIndex := make([]error, len(args.Parallel))
	jobs := make(chan int)
	var wg sync.WaitGroup

	for i := 0; i < concurrency; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for index := range jobs {
				task := args.Parallel[index]
				req := task.toRequest(args)
				emitProgress(tc, index, "running", "")
				start := time.Now()
				result, err := t.spawner.Spawn(ctx, req)
				if result.DurationMS == 0 {
					result.DurationMS = int(time.Since(start).Milliseconds())
				}
				results[index] = result
				errorsByIndex[index] = err
				if err != nil {
					emitProgress(tc, index, "done", err.Error())
				} else {
					emitProgress(tc, index, "done", result.Output)
				}
			}
		}()
	}

	for index := range args.Parallel {
		select {
		case jobs <- index:
		case <-ctx.Done():
			close(jobs)
			wg.Wait()
			return agent.ToolResult{}, ctx.Err()
		}
	}
	close(jobs)
	wg.Wait()

	isError := false
	for _, err := range errorsByIndex {
		if err != nil {
			isError = true
			break
		}
	}
	return textResult(tc.CallID, formatParallelResults(results, errorsByIndex), detailsFromResults(results), isError)
}

type toolArgs struct {
	Prompt       string         `json:"prompt"`
	Tools        []string       `json:"tools"`
	Model        string         `json:"model"`
	SystemPrompt string         `json:"system_prompt"`
	MaxTurns     int            `json:"max_turns"`
	Concurrency  int            `json:"concurrency"`
	Parallel     []parallelTask `json:"parallel"`
}

func (a toolArgs) toRequest() SpawnRequest {
	return SpawnRequest{
		Prompt:       a.Prompt,
		Tools:        append([]string(nil), a.Tools...),
		Model:        a.Model,
		SystemPrompt: a.SystemPrompt,
		MaxTurns:     a.MaxTurns,
		Concurrency:  a.Concurrency,
	}
}

type parallelTask struct {
	Prompt       string   `json:"prompt"`
	Tools        []string `json:"tools"`
	Model        string   `json:"model"`
	SystemPrompt string   `json:"system_prompt"`
	MaxTurns     int      `json:"max_turns"`
}

func (p parallelTask) toRequest(parent toolArgs) SpawnRequest {
	tools := p.Tools
	if len(tools) == 0 {
		tools = parent.Tools
	}
	model := p.Model
	if model == "" {
		model = parent.Model
	}
	systemPrompt := p.SystemPrompt
	if systemPrompt == "" {
		systemPrompt = parent.SystemPrompt
	}
	maxTurns := p.MaxTurns
	if maxTurns == 0 {
		maxTurns = parent.MaxTurns
	}
	return SpawnRequest{
		Prompt:       p.Prompt,
		Tools:        append([]string(nil), tools...),
		Model:        model,
		SystemPrompt: systemPrompt,
		MaxTurns:     maxTurns,
		Concurrency:  parent.Concurrency,
	}
}

type taskDetails struct {
	Results []Result `json:"results"`
}

func detailsFromResults(results []Result) taskDetails {
	return taskDetails{Results: results}
}

func emitProgress(tc agent.ToolCallContext, index int, status string, output string) {
	if tc.OnUpdate == nil {
		return
	}
	update := map[string]interface{}{
		"type":   "task_progress",
		"index":  index,
		"status": status,
	}
	if output != "" {
		update["output_preview"] = truncatePreview(output)
	}
	if encoded, err := json.Marshal(update); err == nil {
		tc.OnUpdate(encoded)
	}
}

func formatSingleResult(result Result) string {
	if strings.TrimSpace(result.Output) == "" {
		return "(no output)"
	}
	return result.Output
}

func formatParallelResults(results []Result, errorsByIndex []error) string {
	var builder strings.Builder
	for i, result := range results {
		if i > 0 {
			builder.WriteString("\n\n")
		}
		fmt.Fprintf(&builder, "Task %d:\n", i+1)
		if errorsByIndex[i] != nil {
			fmt.Fprintf(&builder, "Error: %s", errorsByIndex[i])
			continue
		}
		output := strings.TrimSpace(result.Output)
		if output == "" {
			output = "(no output)"
		}
		builder.WriteString(output)
	}
	return builder.String()
}

func truncatePreview(text string) string {
	text = strings.TrimSpace(text)
	if len(text) <= previewLimit {
		return text
	}
	return text[:previewLimit] + "..."
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
