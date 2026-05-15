package session

import (
	"context"
	"encoding/json"
	"fmt"
	"sort"
	"strings"

	"github.com/noeljackson/pi/internal/agent"
)

const branchSummaryPrompt = `Create a structured summary of this conversation branch for context when returning later.

Use this EXACT format:

## Goal
[What was the user trying to accomplish in this branch?]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Work that was started but not finished]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [What should happen next to continue this work]

Keep each section concise. Preserve exact file paths, function names, and error messages.`

type leafPayload struct{}

type branchSummaryDetails struct {
	ReadFiles     []string `json:"readFiles"`
	ModifiedFiles []string `json:"modifiedFiles"`
}

// Tree returns all branch tips and their parent links.
func (s *Session) Tree() ([]Leaf, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.treeLocked(), nil
}

// ForkAt creates a new leaf branching from the entry with the given ID.
func (s *Session) ForkAt(entryID string) (string, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if entryID == "" {
		return "", fmt.Errorf("entry id is required")
	}
	if _, ok := s.byID[entryID]; !ok {
		return "", fmt.Errorf("entry %s not found", entryID)
	}
	if err := s.appendRecordLocked(RecordTypeLeaf, leafPayload{}, entryID); err != nil {
		return "", err
	}
	return s.leafID, nil
}

// ForkBefore creates a new leaf branching just before the entry with the given
// ID, so that entry is excluded from the new branch.
func (s *Session) ForkBefore(entryID string) (string, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if entryID == "" {
		return "", fmt.Errorf("entry id is required")
	}
	entry, ok := s.byID[entryID]
	if !ok {
		return "", fmt.Errorf("entry %s not found", entryID)
	}
	if err := s.appendRecordLocked(RecordTypeLeaf, leafPayload{}, entry.ParentID); err != nil {
		return "", err
	}
	return s.leafID, nil
}

// MoveTo switches the current leaf to the named leaf.
func (s *Session) MoveTo(leafID string) error {
	return s.SetLeafID(leafID)
}

// SetLeafLabel sets a human-readable label on a leaf.
func (s *Session) SetLeafLabel(leafID, label string) error {
	return s.AppendLabel(leafID, label)
}

// SummarizeAndRecord performs branch summarization and persists the summary as
// a branch summary entry on the previous leaf.
func (s *Session) SummarizeAndRecord(ctx context.Context, summarizer BranchSummarizer, fromLeaf string) error {
	if summarizer == nil {
		return nil
	}
	req, err := s.BuildBranchSummaryRequest(fromLeaf)
	if err != nil {
		return err
	}
	if len(req.Entries) == 0 {
		return nil
	}
	summary, err := summarizer.Summarize(ctx, req)
	if err != nil {
		return err
	}
	summary = strings.TrimSpace(summary)
	if summary == "" {
		summary = "No summary generated"
	}
	return s.RecordBranchSummary(fromLeaf, summary, req.ReadFiles, req.Modified)
}

// RecordBranchSummary persists a completed branch summary on fromLeaf.
func (s *Session) RecordBranchSummary(fromLeaf, summary string, readFiles, modifiedFiles []string) error {
	summary = strings.TrimSpace(summary)
	if summary == "" {
		summary = "No summary generated"
	}
	details, err := json.Marshal(branchSummaryDetails{ReadFiles: readFiles, ModifiedFiles: modifiedFiles})
	if err != nil {
		return err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.appendRecordLocked(RecordTypeBranchSummary, branchSummaryPayload{
		FromID:  fromLeaf,
		Summary: summary,
		Details: details,
	}, fromLeaf)
}

// BuildBranchSummaryRequest collects the branch records and file operations
// needed by a BranchSummarizer.
func (s *Session) BuildBranchSummaryRequest(fromLeaf string) (BranchSummaryRequest, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if fromLeaf == "" {
		return BranchSummaryRequest{LeafID: fromLeaf, Prompt: branchSummaryPrompt}, nil
	}
	entries, err := s.branchRecordsSinceForkLocked(fromLeaf)
	if err != nil {
		return BranchSummaryRequest{}, err
	}
	readFiles, modifiedFiles := extractBranchFileLists(entries)
	return BranchSummaryRequest{
		LeafID:    fromLeaf,
		Entries:   entries,
		ReadFiles: readFiles,
		Modified:  modifiedFiles,
		Prompt:    branchSummaryPrompt,
	}, nil
}

func (s *Session) treeLocked() []Leaf {
	children := s.childCountsLocked()
	labels := s.labels
	leaves := make([]Leaf, 0)
	for _, record := range s.records {
		if !isTreeStructuralRecord(record) {
			continue
		}
		if children[record.ID] != 0 {
			continue
		}
		leaves = append(leaves, Leaf{
			ID:       record.ID,
			ParentID: s.branchParentLocked(record.ID, children),
			Label:    labels[record.ID],
			Created:  record.Timestamp,
		})
	}
	return leaves
}

func (s *Session) branchRecordsSinceForkLocked(leafID string) ([]Record, error) {
	path, err := s.pathToLeafLocked(leafID)
	if err != nil {
		return nil, err
	}
	if len(path) == 0 {
		return nil, nil
	}
	children := s.childCountsLocked()
	start := 0
	for i := len(path) - 2; i >= 0; i-- {
		if children[path[i].ID] > 1 {
			start = i + 1
			break
		}
	}
	entries := make([]Record, len(path[start:]))
	copy(entries, path[start:])
	return entries, nil
}

func (s *Session) childCountsLocked() map[string]int {
	children := make(map[string]int)
	for _, record := range s.records {
		if !isTreeStructuralRecord(record) || record.ParentID == "" {
			continue
		}
		children[record.ParentID]++
	}
	return children
}

func isTreeStructuralRecord(record Record) bool {
	switch record.Type {
	case RecordTypeMessage,
		RecordTypeCustomMessage,
		RecordTypeBashExecution,
		RecordTypeCompaction,
		RecordTypeBranchSummary,
		RecordTypeCustomEntry,
		RecordTypeLeaf:
		return true
	default:
		return false
	}
}

func (s *Session) branchParentLocked(leafID string, children map[string]int) string {
	path, err := s.pathToLeafLocked(leafID)
	if err != nil || len(path) == 0 {
		return ""
	}
	for i := len(path) - 2; i >= 0; i-- {
		if children[path[i].ID] > 1 {
			return path[i].ID
		}
	}
	return ""
}

func extractBranchFileLists(records []Record) ([]string, []string) {
	read := make(map[string]struct{})
	modified := make(map[string]struct{})
	for _, record := range records {
		switch record.Type {
		case RecordTypeMessage:
			message, err := decodeMessagePayload(record.Payload)
			if err == nil {
				extractFileOpsFromMessage(message, read, modified)
			}
		case RecordTypeBranchSummary:
			var payload branchSummaryPayload
			if err := json.Unmarshal(record.Payload, &payload); err != nil || len(payload.Details) == 0 {
				continue
			}
			var details branchSummaryDetails
			if err := json.Unmarshal(payload.Details, &details); err != nil {
				continue
			}
			for _, path := range details.ReadFiles {
				read[path] = struct{}{}
			}
			for _, path := range details.ModifiedFiles {
				modified[path] = struct{}{}
			}
		}
	}
	for path := range modified {
		delete(read, path)
	}
	return sortedKeys(read), sortedKeys(modified)
}

func sortedKeys(values map[string]struct{}) []string {
	keys := make([]string, 0, len(values))
	for key := range values {
		if strings.TrimSpace(key) != "" {
			keys = append(keys, key)
		}
	}
	sort.Strings(keys)
	return keys
}

func extractFileOpsFromMessage(message agent.Message, read map[string]struct{}, modified map[string]struct{}) {
	msg, ok := message.(agent.AssistantMessage)
	if !ok {
		if ptr, ptrOK := message.(*agent.AssistantMessage); ptrOK && ptr != nil {
			msg = *ptr
			ok = true
		}
	}
	if !ok {
		return
	}
	for _, content := range msg.Content {
		toolUse, ok := content.(agent.ToolUseContent)
		if !ok {
			if ptr, ptrOK := content.(*agent.ToolUseContent); ptrOK && ptr != nil {
				toolUse = *ptr
				ok = true
			}
		}
		if !ok {
			continue
		}
		var input struct {
			Path string `json:"path"`
		}
		if err := json.Unmarshal(toolUse.Input, &input); err != nil || input.Path == "" {
			continue
		}
		switch toolUse.Name {
		case "read":
			read[input.Path] = struct{}{}
		case "write", "edit":
			modified[input.Path] = struct{}{}
		}
	}
}
