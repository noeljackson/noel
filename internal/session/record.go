// Package session defines session persistence record types.
package session

import (
	"time"

	"github.com/noeljackson/pi/internal/session/schema"
)

// Record describes one JSONL session record.
type Record = schema.Record

// Leaf identifies a tip of a branch within a session.
type Leaf = schema.Leaf

// BranchSummaryRequest carries the inputs needed to summarize an abandoned
// branch when the user moves away from a leaf.
type BranchSummaryRequest = schema.BranchSummaryRequest

// BranchSummarizer produces a textual summary of an abandoned branch.
type BranchSummarizer = schema.BranchSummarizer

const (
	// RecordTypeSessionHeader identifies a session header record.
	RecordTypeSessionHeader = "session"
	// RecordTypeMessage identifies a message record.
	RecordTypeMessage = "message"
	// RecordTypeToolCall identifies a tool call record.
	RecordTypeToolCall = "tool_call"
	// RecordTypeToolResult identifies a tool result record.
	RecordTypeToolResult = "tool_result"
	// RecordTypeRunState identifies a run state record.
	RecordTypeRunState = "run_state"
	// RecordTypeModelChange identifies a model change record.
	RecordTypeModelChange = "model_change"
	// RecordTypeThinkingChange identifies a thinking-level change record.
	RecordTypeThinkingChange = "thinking_level_change"
	// RecordTypeCompaction identifies a compaction record.
	RecordTypeCompaction = "compaction"
	// RecordTypeBranchSummary identifies a branch summary record.
	RecordTypeBranchSummary = "branch_summary"
	// RecordTypeLabel identifies an entry label record.
	RecordTypeLabel = "label"
	// RecordTypeSessionName identifies a session name record.
	RecordTypeSessionName = "session_info"
	// RecordTypeCustomEntry identifies an app-defined session entry.
	RecordTypeCustomEntry = "custom"
	// RecordTypeCustomMessage identifies an app-defined message entry.
	RecordTypeCustomMessage = "custom_message"
	// RecordTypeBashExecution identifies a shell execution message entry.
	RecordTypeBashExecution = "bash_execution"
	// RecordTypeQueueState identifies a queue state record.
	RecordTypeQueueState = "queue_state"
	// RecordTypeSavePoint identifies a save point record.
	RecordTypeSavePoint = "save_point"
	// RecordTypeLeaf identifies an explicit empty branch leaf.
	RecordTypeLeaf = "leaf"
)

// SessionHeader describes session metadata stored at the start of a session file.
type SessionHeader struct {
	Version         int
	ID              string
	CreatedAt       time.Time
	Cwd             string
	GoVersion       string
	LeafID          string
	ParentSessionID string
	Labels          map[string]string
}
