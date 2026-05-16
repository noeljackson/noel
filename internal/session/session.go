package session

import (
	"bytes"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/noeljackson/pi/internal/agent"
)

const currentTurnWriteInterval = 250 * time.Millisecond

// CurrentTurn contains the recoverable state of an in-flight assistant stream.
type CurrentTurn struct {
	PartialText         string                `json:"partial_text,omitempty"`
	PartialToolUse      *agent.ToolUseContent `json:"partial_tool_use,omitempty"`
	PartialToolUseInput string                `json:"partial_tool_use_input,omitempty"`
}

// Session owns one append-only JSONL session file and its current-turn sidecar.
type Session struct {
	mu              sync.Mutex
	id              string
	path            string
	currentTurnPath string
	file            *os.File
	closed          bool

	cwd       string
	createdAt time.Time
	leafID    string
	labels    map[string]string
	name      string

	records             []Record
	byID                map[string]Record
	lastRecordID        string
	lastMessageRecordID string
	messageRecords      []Record

	lastCurrentTurnWrite time.Time
	currentTurn          CurrentTurn
	activeStreamModels   map[string]string
}

func newSession(id string, path string, currentTurnPath string, file *os.File, records []Record) *Session {
	session := &Session{
		id:                 id,
		path:               path,
		currentTurnPath:    currentTurnPath,
		file:               file,
		records:            make([]Record, 0, len(records)),
		byID:               make(map[string]Record, len(records)),
		labels:             make(map[string]string),
		activeStreamModels: make(map[string]string),
	}
	for _, record := range records {
		session.indexRecord(record)
	}
	if len(records) > 0 && records[0].Type == RecordTypeSessionHeader {
		if header, ok := decodeSessionHeader(records[0]); ok && header.LeafID != "" {
			session.leafID = header.LeafID
		}
	}
	return session
}

// ID returns the stable session ID.
func (s *Session) ID() string {
	return s.id
}

// Path returns the JSONL file path for this session.
func (s *Session) Path() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.path
}

// Cwd returns the working directory captured when the session was created.
func (s *Session) Cwd() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.cwd
}

// Records returns a copy of the persisted JSONL records.
func (s *Session) Records() []Record {
	s.mu.Lock()
	defer s.mu.Unlock()
	records := make([]Record, len(s.records))
	copy(records, s.records)
	return records
}

// AppendMessage appends a persisted conversation message.
func (s *Session) AppendMessage(message agent.Message) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.appendMessageLocked(message, s.leafID)
}

// AppendThinkingChange appends a thinking-level change record.
func (s *Session) AppendThinkingChange(level string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.appendRecordLocked(RecordTypeThinkingChange, thinkingChangePayload{ThinkingLevel: level}, s.leafID)
}

// AppendModelChange appends a model change record.
func (s *Session) AppendModelChange(model, provider, api, reason string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.appendRecordLocked(RecordTypeModelChange, modelChangePayload{
		Model:    model,
		ModelID:  model,
		Provider: provider,
		API:      api,
		Reason:   reason,
	}, s.leafID)
}

// AppendLabel appends or clears a label for a session entry.
func (s *Session) AppendLabel(leafID, label string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if leafID != "" {
		if _, ok := s.byID[leafID]; !ok {
			return fmt.Errorf("entry %s not found", leafID)
		}
	}
	return s.appendRecordLocked(RecordTypeLabel, labelPayload{TargetID: leafID, Label: label}, s.leafID)
}

// AppendSessionName appends the session display name.
func (s *Session) AppendSessionName(name string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.appendRecordLocked(RecordTypeSessionName, sessionNamePayload{Name: strings.TrimSpace(name)}, s.leafID)
}

// AppendCustomEntry appends an app-defined session entry.
func (s *Session) AppendCustomEntry(kind string, data json.RawMessage) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.appendRecordLocked(RecordTypeCustomEntry, customEntryPayload{Kind: kind, Data: data}, s.leafID)
}

// AppendCustomMessage appends an app-defined message entry.
func (s *Session) AppendCustomMessage(message agent.CustomMessage) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	payload, err := customMessageToPayload(message)
	if err != nil {
		return err
	}
	return s.appendRawPayloadRecordLocked(RecordTypeCustomMessage, payload, s.leafID)
}

// AppendBashExecution appends a shell execution message entry.
func (s *Session) AppendBashExecution(message agent.BashExecutionMessage) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	payload, err := encodeMessagePayload(message)
	if err != nil {
		return err
	}
	return s.appendRawPayloadRecordLocked(RecordTypeBashExecution, payload, s.leafID)
}

// AppendRunState appends opaque run state.
func (s *Session) AppendRunState(payload any, parentID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.appendRecordLocked(RecordTypeRunState, payload, parentID)
}

// AppendTodoState appends the latest todo tool state.
func (s *Session) AppendTodoState(payload json.RawMessage) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.appendRawPayloadRecordLocked(RecordTypeTodoState, payload, s.leafID)
}

// AppendCompaction appends a compaction summary record.
func (s *Session) AppendCompaction(summary string, parentID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.appendRecordLocked(RecordTypeCompaction, compactionPayload{Summary: summary}, parentID)
}

// AppendCompactionRecord appends a compaction summary with reconstruction data.
func (s *Session) AppendCompactionRecord(summary string, droppedCount int, compactedAt time.Time, parentID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.appendRecordLocked(RecordTypeCompaction, compactionPayload{
		Summary:             summary,
		DroppedMessageCount: droppedCount,
		CompactedAt:         compactedAt,
	}, parentID)
}

// AppendSavePoint appends a save point record.
func (s *Session) AppendSavePoint(parentID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.appendRecordLocked(RecordTypeSavePoint, savePointPayload{}, parentID)
}

// WriteCurrentTurn persists the latest in-flight assistant turn state.
func (s *Session) WriteCurrentTurn(partial CurrentTurn) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	s.currentTurn = partial
	now := time.Now()
	if !s.lastCurrentTurnWrite.IsZero() && now.Sub(s.lastCurrentTurnWrite) < currentTurnWriteInterval {
		return nil
	}
	if err := s.writeCurrentTurnLocked(partial); err != nil {
		return err
	}
	s.lastCurrentTurnWrite = now
	return nil
}

// ClearCurrentTurn removes the in-flight assistant sidecar.
func (s *Session) ClearCurrentTurn() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.clearCurrentTurnLocked()
}

// Messages reconstructs persisted messages from the current branch.
func (s *Session) Messages() ([]agent.Message, error) {
	records, err := s.PathToCurrentLeaf()
	if err != nil {
		return nil, err
	}
	return messagesFromRecords(records)
}

// PathToLeaf returns the record chain from the root to leafID.
func (s *Session) PathToLeaf(leafID string) ([]Record, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.pathToLeafLocked(leafID)
}

// PathToCurrentLeaf returns the record chain for the current branch leaf.
func (s *Session) PathToCurrentLeaf() ([]Record, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.pathToLeafLocked(s.leafID)
}

func (s *Session) pathToLeafLocked(leafID string) ([]Record, error) {
	if leafID == "" {
		return nil, nil
	}
	path := make([]Record, 0)
	seen := make(map[string]struct{})
	currentID := leafID
	for currentID != "" {
		if _, ok := seen[currentID]; ok {
			return nil, fmt.Errorf("cycle in session branch at %s", currentID)
		}
		seen[currentID] = struct{}{}
		record, ok := s.byID[currentID]
		if !ok {
			return nil, fmt.Errorf("entry %s not found", currentID)
		}
		if record.Type != RecordTypeSessionHeader {
			path = append(path, record)
		}
		currentID = record.ParentID
	}
	for i, j := 0, len(path)-1; i < j; i, j = i+1, j-1 {
		path[i], path[j] = path[j], path[i]
	}
	return path, nil
}

func messagesFromRecords(records []Record) ([]agent.Message, error) {
	messages := make([]agent.Message, 0)
	for _, record := range records {
		switch record.Type {
		case RecordTypeMessage:
			message, err := decodeMessagePayload(record.Payload)
			if err != nil {
				return nil, err
			}
			messages = append(messages, message)
		case RecordTypeCustomMessage:
			message, err := payloadToCustomMessage(record.Payload)
			if err != nil {
				return nil, err
			}
			messages = append(messages, message)
		case RecordTypeBashExecution:
			message, err := decodeMessagePayload(record.Payload)
			if err != nil {
				return nil, err
			}
			messages = append(messages, message)
		case RecordTypeBranchSummary:
			message, ok := decodeBranchSummaryPayload(record.Payload, record.Timestamp)
			if !ok || message.Summary == "" {
				continue
			}
			messages = append(messages, message)
		case RecordTypeCompaction:
			payload, ok := decodeCompactionPayload(record.Payload)
			if !ok || payload.DroppedMessageCount <= 0 {
				continue
			}
			droppedCount := payload.DroppedMessageCount
			if droppedCount > len(messages) {
				droppedCount = len(messages)
			}
			summaryMessage := agent.CompactionSummaryMessage{
				Timestamp:    record.Timestamp,
				Summary:      payload.Summary,
				TokensBefore: payload.TokensBefore,
				DroppedCount: droppedCount,
				FileOps:      payload.FileOps,
				Details:      payload.Details,
			}
			compacted := make([]agent.Message, 0, 1+len(messages)-droppedCount)
			compacted = append(compacted, summaryMessage)
			compacted = append(compacted, messages[droppedCount:]...)
			messages = compacted
		default:
			continue
		}
	}
	return messages, nil
}

// LastMessage returns the last persisted message, if any.
func (s *Session) LastMessage() (agent.Message, bool, error) {
	messages, err := s.Messages()
	if err != nil {
		return nil, false, err
	}
	if len(messages) == 0 {
		return nil, false, nil
	}
	return messages[len(messages)-1], true, nil
}

// Close closes the session file.
func (s *Session) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return nil
	}
	s.closed = true
	return s.file.Close()
}

// LastMessageRecordID returns the record ID for the latest persisted message.
func (s *Session) LastMessageRecordID() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.lastMessageRecordID
}

// LeafID returns the current branch leaf record ID.
func (s *Session) LeafID() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.leafID
}

// SetLeafID moves the current branch leaf to an existing record.
func (s *Session) SetLeafID(leafID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if leafID != "" {
		if _, ok := s.byID[leafID]; !ok {
			return fmt.Errorf("entry %s not found", leafID)
		}
	}
	previousLeafID := s.leafID
	s.leafID = leafID
	s.lastRecordID = leafID
	if err := s.persistHeaderLocked(); err != nil {
		s.leafID = previousLeafID
		s.lastRecordID = previousLeafID
		return err
	}
	return nil
}

// Labels returns a copy of the label cache keyed by entry ID.
func (s *Session) Labels() map[string]string {
	s.mu.Lock()
	defer s.mu.Unlock()
	labels := make(map[string]string, len(s.labels))
	for id, label := range s.labels {
		labels[id] = label
	}
	return labels
}

// Name returns the latest session display name.
func (s *Session) Name() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.name
}

// SetName appends and applies the session display name.
func (s *Session) SetName(name string) error {
	return s.AppendSessionName(name)
}

func (s *Session) appendMessageLocked(message agent.Message, parentID string) error {
	payload, err := encodeMessagePayload(message)
	if err != nil {
		return err
	}
	return s.appendRawPayloadRecordLocked(RecordTypeMessage, payload, parentID)
}

func (s *Session) appendRecordLocked(recordType string, payload any, parentID string) error {
	raw, err := json.Marshal(payload)
	if err != nil {
		return err
	}
	return s.appendRawPayloadRecordLocked(recordType, raw, parentID)
}

func (s *Session) appendRawPayloadRecordLocked(recordType string, payload json.RawMessage, parentID string) error {
	if s.closed {
		return errors.New("session is closed")
	}
	if parentID == "" && recordType != RecordTypeSessionHeader && recordType != RecordTypeLeaf && recordType != RecordTypeBranchSummary {
		parentID = s.leafID
	}
	id, err := randomHexID()
	if err != nil {
		return err
	}
	record := Record{
		Type:      recordType,
		ID:        id,
		ParentID:  parentID,
		Timestamp: time.Now().UTC(),
		Payload:   payload,
	}
	if err := writeRecordLine(s.file, record); err != nil {
		return err
	}
	s.indexRecord(record)
	if s.headerHasLeafIDLocked() {
		return s.persistHeaderLocked()
	}
	return nil
}

func (s *Session) indexRecord(record Record) {
	s.records = append(s.records, record)
	s.byID[record.ID] = record
	if record.Type == RecordTypeSessionHeader {
		if header, ok := decodeSessionHeader(record); ok {
			s.cwd = header.Cwd
			s.createdAt = header.CreatedAt
			if header.LeafID != "" {
				s.leafID = header.LeafID
			}
			for id, label := range header.Labels {
				if strings.TrimSpace(label) != "" {
					s.labels[id] = strings.TrimSpace(label)
				}
			}
		}
		return
	}
	s.lastRecordID = record.ID
	s.leafID = record.ID
	if record.Type == RecordTypeMessage {
		s.lastMessageRecordID = record.ID
		s.messageRecords = append(s.messageRecords, record)
	}
	s.indexPayloadRecord(record)
}

func (s *Session) headerHasLeafIDLocked() bool {
	if len(s.records) == 0 || s.records[0].Type != RecordTypeSessionHeader {
		return false
	}
	header, ok := decodeSessionHeader(s.records[0])
	return ok && header.LeafID != ""
}

func (s *Session) indexPayloadRecord(record Record) {
	switch record.Type {
	case RecordTypeLabel:
		var payload labelPayload
		if err := json.Unmarshal(record.Payload, &payload); err != nil {
			return
		}
		label := strings.TrimSpace(payload.Label)
		if label == "" {
			delete(s.labels, payload.TargetID)
		} else {
			s.labels[payload.TargetID] = label
		}
	case RecordTypeSessionName:
		var payload sessionNamePayload
		if err := json.Unmarshal(record.Payload, &payload); err != nil {
			return
		}
		s.name = strings.TrimSpace(payload.Name)
	}
}

func (s *Session) writeCurrentTurnLocked(partial CurrentTurn) error {
	if s.closed {
		return errors.New("session is closed")
	}
	if err := os.MkdirAll(filepath.Dir(s.currentTurnPath), 0o700); err != nil {
		return err
	}
	payload, err := json.Marshal(currentTurnToDisk(partial))
	if err != nil {
		return err
	}
	payload = append(payload, '\n')

	tmpName, err := randomHexID()
	if err != nil {
		return err
	}
	tmpPath := fmt.Sprintf("%s.tmp-%s", s.currentTurnPath, tmpName)
	file, err := os.OpenFile(tmpPath, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	if err := writeFull(file, payload); err != nil {
		_ = file.Close()
		_ = os.Remove(tmpPath)
		return err
	}
	if err := file.Sync(); err != nil {
		_ = file.Close()
		_ = os.Remove(tmpPath)
		return err
	}
	if err := file.Close(); err != nil {
		_ = os.Remove(tmpPath)
		return err
	}
	if err := os.Rename(tmpPath, s.currentTurnPath); err != nil {
		_ = os.Remove(tmpPath)
		return err
	}
	return syncDir(filepath.Dir(s.currentTurnPath))
}

func (s *Session) clearCurrentTurnLocked() error {
	s.currentTurn = CurrentTurn{}
	s.lastCurrentTurnWrite = time.Time{}
	err := os.Remove(s.currentTurnPath)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return err
	}
	return syncDir(filepath.Dir(s.currentTurnPath))
}

func (s *Session) persistHeaderLocked() error {
	if s.closed {
		return errors.New("session is closed")
	}
	if len(s.records) == 0 || s.records[0].Type != RecordTypeSessionHeader {
		return nil
	}
	header, ok := decodeSessionHeader(s.records[0])
	if !ok {
		return errors.New("session file missing session header")
	}
	header.LeafID = s.leafID
	header.Labels = make(map[string]string, len(s.labels))
	for id, label := range s.labels {
		header.Labels[id] = label
	}
	payload, err := json.Marshal(header)
	if err != nil {
		return err
	}
	s.records[0].Payload = payload
	s.byID[s.records[0].ID] = s.records[0]
	return s.rewriteRecordsLocked()
}

func (s *Session) rewriteRecordsLocked() error {
	tmpName, err := randomHexID()
	if err != nil {
		return err
	}
	tmpPath := fmt.Sprintf("%s.tmp-%s", s.path, tmpName)
	file, err := os.OpenFile(tmpPath, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	for _, record := range s.records {
		data, err := json.Marshal(record)
		if err != nil {
			_ = file.Close()
			_ = os.Remove(tmpPath)
			return err
		}
		data = append(data, '\n')
		if err := writeFull(file, data); err != nil {
			_ = file.Close()
			_ = os.Remove(tmpPath)
			return err
		}
	}
	if err := file.Sync(); err != nil {
		_ = file.Close()
		_ = os.Remove(tmpPath)
		return err
	}
	if err := file.Close(); err != nil {
		_ = os.Remove(tmpPath)
		return err
	}
	if err := s.file.Close(); err != nil {
		_ = os.Remove(tmpPath)
		return err
	}
	if err := os.Rename(tmpPath, s.path); err != nil {
		reopened, reopenErr := os.OpenFile(s.path, os.O_RDWR|os.O_APPEND, 0o600)
		if reopenErr == nil {
			s.file = reopened
		}
		_ = os.Remove(tmpPath)
		return err
	}
	reopened, err := os.OpenFile(s.path, os.O_RDWR|os.O_APPEND, 0o600)
	if err != nil {
		return err
	}
	s.file = reopened
	return syncDir(filepath.Dir(s.path))
}

func writeRecordLine(file *os.File, record Record) error {
	data, err := json.Marshal(record)
	if err != nil {
		return err
	}
	data = append(data, '\n')
	if err := writeFull(file, data); err != nil {
		return err
	}
	return file.Sync()
}

func writeFull(file *os.File, data []byte) error {
	for len(data) > 0 {
		n, err := file.Write(data)
		if err != nil {
			return err
		}
		if n == 0 {
			return io.ErrShortWrite
		}
		data = data[n:]
	}
	return nil
}

func randomHexID() (string, error) {
	var bytes [12]byte
	if _, err := rand.Read(bytes[:]); err != nil {
		return "", err
	}
	return hex.EncodeToString(bytes[:]), nil
}

func syncDir(dir string) error {
	file, err := os.Open(dir)
	if err != nil {
		return err
	}
	defer file.Close()
	return file.Sync()
}

func loadRecords(path string) ([]Record, int64, bool, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, 0, false, err
	}
	if len(data) == 0 {
		return nil, 0, false, errors.New("session file is empty")
	}

	chunks := bytes.SplitAfter(data, []byte("\n"))
	records := make([]Record, 0, len(chunks))
	var offset int64
	var validEnd int64
	needsNewline := false

	for _, chunk := range chunks {
		if len(chunk) == 0 {
			continue
		}
		complete := bytes.HasSuffix(chunk, []byte("\n"))
		line := bytes.TrimRight(chunk, "\r\n")
		nextOffset := offset + int64(len(chunk))
		if len(bytes.TrimSpace(line)) == 0 {
			if complete {
				validEnd = nextOffset
				offset = nextOffset
				continue
			}
			break
		}

		var record Record
		if err := json.Unmarshal(line, &record); err != nil {
			if complete {
				return nil, 0, false, fmt.Errorf("invalid JSONL record in %s: %w", path, err)
			}
			break
		}
		records = append(records, record)
		validEnd = nextOffset
		if !complete {
			needsNewline = true
		}
		offset = nextOffset
	}

	return records, validEnd, needsNewline, nil
}

func loadHeaderRecord(path string) (Record, bool) {
	records, _, _, err := loadRecords(path)
	if err != nil || len(records) == 0 || records[0].Type != RecordTypeSessionHeader {
		return Record{}, false
	}
	return records[0], true
}

type compactionPayload struct {
	Summary             string          `json:"summary"`
	DroppedMessageCount int             `json:"dropped_message_count,omitempty"`
	CompactedAt         time.Time       `json:"compacted_at,omitempty"`
	FirstKeptEntryID    string          `json:"firstKeptEntryId,omitempty"`
	TokensBefore        int             `json:"tokensBefore,omitempty"`
	Details             json.RawMessage `json:"details,omitempty"`
	FromHook            bool            `json:"fromHook,omitempty"`
	FileOps             json.RawMessage `json:"fileOps,omitempty"`
}

type savePointPayload struct{}

type thinkingChangePayload struct {
	ThinkingLevel string `json:"thinkingLevel"`
}

type modelChangePayload struct {
	Model    string `json:"model,omitempty"`
	ModelID  string `json:"modelId,omitempty"`
	Provider string `json:"provider"`
	API      string `json:"api,omitempty"`
	Reason   string `json:"reason,omitempty"`
}

type labelPayload struct {
	TargetID string `json:"targetId"`
	Label    string `json:"label,omitempty"`
}

type sessionNamePayload struct {
	Name string `json:"name,omitempty"`
}

type customEntryPayload struct {
	Kind string          `json:"customType"`
	Data json.RawMessage `json:"data,omitempty"`
}

type branchSummaryPayload struct {
	FromID   string          `json:"fromId"`
	Summary  string          `json:"summary"`
	Details  json.RawMessage `json:"details,omitempty"`
	FromHook bool            `json:"fromHook,omitempty"`
}

func decodeCompactionPayload(raw json.RawMessage) (compactionPayload, bool) {
	var payload compactionPayload
	if err := json.Unmarshal(raw, &payload); err != nil {
		return compactionPayload{}, false
	}
	return payload, true
}

func decodeBranchSummaryPayload(raw json.RawMessage, timestamp time.Time) (agent.BranchSummaryMessage, bool) {
	var payload branchSummaryPayload
	if err := json.Unmarshal(raw, &payload); err != nil {
		return agent.BranchSummaryMessage{}, false
	}
	return agent.BranchSummaryMessage{
		Timestamp:    timestamp,
		Summary:      payload.Summary,
		SourceLeafID: payload.FromID,
		Details:      payload.Details,
		FromHook:     payload.FromHook,
	}, true
}
