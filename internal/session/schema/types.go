package schema

import (
	"context"
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

// Leaf identifies a tip of a branch within a session.
type Leaf struct {
	ID       string
	ParentID string
	Label    string
	Created  time.Time
}

// BranchSummaryRequest carries the inputs needed to summarize an abandoned
// branch when the user moves away from a leaf.
type BranchSummaryRequest struct {
	LeafID    string
	Entries   []Record
	ReadFiles []string
	Modified  []string
	Prompt    string
}

// BranchSummarizer produces a textual summary of an abandoned branch.
type BranchSummarizer interface {
	Summarize(ctx context.Context, req BranchSummaryRequest) (summary string, err error)
}
