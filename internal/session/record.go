// Package session defines session persistence record types.
package session

import (
	"encoding/json"
	"time"
)

// Record describes one JSONL session record.
type Record struct {
	Type      string          `json:"type"`
	ID        string          `json:"id"`
	ParentID  string          `json:"parentId,omitempty"`
	Timestamp time.Time       `json:"timestamp"`
	Payload   json.RawMessage `json:"payload"`
}

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
	// RecordTypeCompaction identifies a compaction record.
	RecordTypeCompaction = "compaction"
	// RecordTypeBranchSummary identifies a branch summary record.
	RecordTypeBranchSummary = "branch_summary"
	// RecordTypeQueueState identifies a queue state record.
	RecordTypeQueueState = "queue_state"
	// RecordTypeSavePoint identifies a save point record.
	RecordTypeSavePoint = "save_point"
)

// SessionHeader describes session metadata stored at the start of a session file.
type SessionHeader struct {
	Version   int
	ID        string
	CreatedAt time.Time
	Cwd       string
	GoVersion string
}
