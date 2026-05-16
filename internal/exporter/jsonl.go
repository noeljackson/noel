package exporter

import (
	"bufio"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"time"

	"github.com/noeljackson/pi/internal/session"
)

func Export(sess *session.Session, w io.Writer) error {
	if sess == nil {
		return errors.New("session is required")
	}
	if w == nil {
		return errors.New("writer is required")
	}
	for _, record := range sess.Records() {
		data, err := json.Marshal(record)
		if err != nil {
			return err
		}
		if _, err := w.Write(append(data, '\n')); err != nil {
			return err
		}
	}
	return nil
}

func ExportPath(sess *session.Session, path string) error {
	if path == "" {
		return errors.New("path is required")
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	file, err := os.OpenFile(path, os.O_CREATE|os.O_TRUNC|os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	defer file.Close()
	return Export(sess, file)
}

func Import(store *session.JSONLStore, r io.Reader) (string, error) {
	if store == nil {
		return "", errors.New("session store is required")
	}
	if r == nil {
		return "", errors.New("reader is required")
	}
	records, err := readRecords(r)
	if err != nil {
		return "", err
	}
	if len(records) == 0 || records[0].Type != session.RecordTypeSessionHeader {
		return "", errors.New("import file missing session header")
	}

	var importedHeader session.SessionHeader
	if err := json.Unmarshal(records[0].Payload, &importedHeader); err != nil {
		return "", err
	}
	id, err := randomHexID()
	if err != nil {
		return "", err
	}
	headerID, err := randomHexID()
	if err != nil {
		return "", err
	}
	if importedHeader.LeafID == "" {
		importedHeader.LeafID = lastRecordID(records)
	}
	now := time.Now().UTC()
	header := session.SessionHeader{
		Version:         3,
		ID:              id,
		CreatedAt:       now,
		Cwd:             importedHeader.Cwd,
		GoVersion:       runtime.Version(),
		LeafID:          importedHeader.LeafID,
		ParentSessionID: importedHeader.ID,
		Labels:          importedHeader.Labels,
	}
	payload, err := json.Marshal(header)
	if err != nil {
		return "", err
	}
	records[0] = session.Record{
		Type:      session.RecordTypeSessionHeader,
		ID:        headerID,
		Timestamp: now,
		Payload:   payload,
	}

	path := filepath.Join(store.Dir(), id+".jsonl")
	if err := os.MkdirAll(store.Dir(), 0o700); err != nil {
		return "", err
	}
	file, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
	if err != nil {
		return "", err
	}
	remove := true
	defer func() {
		_ = file.Close()
		if remove {
			_ = os.Remove(path)
		}
	}()
	for _, record := range records {
		data, err := json.Marshal(record)
		if err != nil {
			return "", err
		}
		if _, err := file.Write(append(data, '\n')); err != nil {
			return "", err
		}
	}
	if err := file.Sync(); err != nil {
		return "", err
	}
	if err := file.Close(); err != nil {
		return "", err
	}
	remove = false
	return id, nil
}

func ImportPath(store *session.JSONLStore, path string) (string, error) {
	file, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer file.Close()
	return Import(store, file)
}

func readRecords(r io.Reader) ([]session.Record, error) {
	scanner := bufio.NewScanner(r)
	scanner.Buffer(make([]byte, 0, 64*1024), 8*1024*1024)
	var records []session.Record
	lineNumber := 0
	for scanner.Scan() {
		lineNumber++
		line := scanner.Bytes()
		if len(line) == 0 {
			continue
		}
		var record session.Record
		if err := json.Unmarshal(line, &record); err != nil {
			return nil, fmt.Errorf("invalid JSONL record at line %d: %w", lineNumber, err)
		}
		records = append(records, record)
	}
	if err := scanner.Err(); err != nil {
		return nil, err
	}
	return records, nil
}

func lastRecordID(records []session.Record) string {
	for i := len(records) - 1; i >= 0; i-- {
		if records[i].Type != session.RecordTypeSessionHeader {
			return records[i].ID
		}
	}
	return ""
}

func randomHexID() (string, error) {
	var bytes [12]byte
	if _, err := rand.Read(bytes[:]); err != nil {
		return "", err
	}
	return hex.EncodeToString(bytes[:]), nil
}
