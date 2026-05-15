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
		activeStreamModels: make(map[string]string),
	}
	for _, record := range records {
		session.indexRecord(record)
	}
	return session
}

// ID returns the stable session ID.
func (s *Session) ID() string {
	return s.id
}

// AppendMessage appends a persisted conversation message.
func (s *Session) AppendMessage(message agent.Message) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.appendMessageLocked(message, s.lastRecordID)
}

// AppendRunState appends opaque run state.
func (s *Session) AppendRunState(payload any, parentID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.appendRecordLocked(RecordTypeRunState, payload, parentID)
}

// AppendCompaction appends a compaction summary record.
func (s *Session) AppendCompaction(summary string, parentID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.appendRecordLocked(RecordTypeCompaction, compactionPayload{Summary: summary}, parentID)
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

// Messages reconstructs persisted messages from the JSONL record stream.
func (s *Session) Messages() ([]agent.Message, error) {
	records, _, _, err := loadRecords(s.path)
	if err != nil {
		return nil, err
	}
	messages := make([]agent.Message, 0)
	for _, record := range records {
		if record.Type != RecordTypeMessage {
			continue
		}
		message, err := decodeMessagePayload(record.Payload)
		if err != nil {
			return nil, err
		}
		messages = append(messages, message)
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
	return nil
}

func (s *Session) indexRecord(record Record) {
	s.records = append(s.records, record)
	s.byID[record.ID] = record
	s.lastRecordID = record.ID
	if record.Type == RecordTypeMessage {
		s.lastMessageRecordID = record.ID
		s.messageRecords = append(s.messageRecords, record)
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
	Summary string `json:"summary"`
}

type savePointPayload struct{}
