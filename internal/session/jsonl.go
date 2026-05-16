package session

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"strings"
	"time"
)

// SessionInfo describes a persisted session found by JSONLStore.List.
type SessionInfo struct {
	ID        string
	CreatedAt time.Time
	Cwd       string
	Path      string
}

// JSONLStore creates and opens append-only JSONL sessions.
type JSONLStore struct {
	dir string
}

// NewJSONLStore returns a store rooted at dir.
func NewJSONLStore(dir string) *JSONLStore {
	return &JSONLStore{dir: dir}
}

// Create creates a new session file in the store root.
func (s *JSONLStore) Create(cwd string) (*Session, error) {
	if err := os.MkdirAll(s.dir, 0o700); err != nil {
		return nil, err
	}

	id, err := randomHexID()
	if err != nil {
		return nil, err
	}
	recordID, err := randomHexID()
	if err != nil {
		return nil, err
	}

	path := s.sessionPath(id)
	file, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_RDWR|os.O_APPEND, 0o600)
	if err != nil {
		return nil, err
	}

	createdAt := time.Now().UTC()
	header := SessionHeader{
		Version:   3,
		ID:        id,
		CreatedAt: createdAt,
		Cwd:       cwd,
		GoVersion: runtime.Version(),
	}
	payload, err := json.Marshal(header)
	if err != nil {
		_ = file.Close()
		return nil, err
	}
	record := Record{
		Type:      RecordTypeSessionHeader,
		ID:        recordID,
		Timestamp: createdAt,
		Payload:   payload,
	}
	if err := writeRecordLine(file, record); err != nil {
		_ = file.Close()
		return nil, err
	}

	session := newSession(id, path, s.currentTurnPath(id), file, []Record{record})
	session.cwd = cwd
	session.createdAt = createdAt
	return session, nil
}

// Open opens an existing session by id.
func (s *JSONLStore) Open(id string) (*Session, error) {
	return s.OpenPath(s.sessionPath(id))
}

// OpenPath opens an existing session by file path.
func (s *JSONLStore) OpenPath(path string) (*Session, error) {
	records, validEnd, needsNewline, err := loadRecords(path)
	if err != nil {
		return nil, err
	}
	if len(records) == 0 || records[0].Type != RecordTypeSessionHeader {
		return nil, errors.New("session file missing session header")
	}

	file, err := os.OpenFile(path, os.O_RDWR|os.O_APPEND, 0o600)
	if err != nil {
		return nil, err
	}
	if err := file.Truncate(validEnd); err != nil {
		_ = file.Close()
		return nil, err
	}
	if _, err := file.Seek(0, 2); err != nil {
		_ = file.Close()
		return nil, err
	}
	if needsNewline {
		if _, err := file.Write([]byte("\n")); err != nil {
			_ = file.Close()
			return nil, err
		}
		if err := file.Sync(); err != nil {
			_ = file.Close()
			return nil, err
		}
	}

	header, _ := decodeSessionHeader(records[0])
	session := newSession(header.ID, path, s.currentTurnPath(header.ID), file, records)
	if header, ok := decodeSessionHeader(records[0]); ok {
		session.cwd = header.Cwd
		session.createdAt = header.CreatedAt
	}
	return session, nil
}

// List returns all valid sessions in newest-first order.
func (s *JSONLStore) List() ([]SessionInfo, error) {
	entries, err := os.ReadDir(s.dir)
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}

	infos := make([]SessionInfo, 0, len(entries))
	for _, entry := range entries {
		if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".jsonl") {
			continue
		}
		path := filepath.Join(s.dir, entry.Name())
		record, ok := loadHeaderRecord(path)
		if !ok {
			continue
		}
		header, ok := decodeSessionHeader(record)
		if !ok {
			continue
		}
		createdAt := header.CreatedAt
		if createdAt.IsZero() {
			createdAt = record.Timestamp
		}
		infos = append(infos, SessionInfo{
			ID:        header.ID,
			CreatedAt: createdAt,
			Cwd:       header.Cwd,
			Path:      path,
		})
	}
	sort.Slice(infos, func(i, j int) bool {
		return infos[i].CreatedAt.After(infos[j].CreatedAt)
	})
	return infos, nil
}

func (s *JSONLStore) sessionPath(id string) string {
	return filepath.Join(s.dir, id+".jsonl")
}

func (s *JSONLStore) currentTurnPath(id string) string {
	return filepath.Join(s.dir, id+".current-turn.json")
}

func decodeSessionHeader(record Record) (SessionHeader, bool) {
	var header SessionHeader
	if err := json.Unmarshal(record.Payload, &header); err != nil {
		return SessionHeader{}, false
	}
	return header, header.ID != ""
}
