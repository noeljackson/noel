package session

import (
	"bufio"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"sync"
	"time"

	"github.com/noeljackson/pi/internal/agent"
)

type Writer struct {
	id      string
	file    *os.File
	encoder *json.Encoder
	mu      sync.Mutex
}

func NewWriter() (*Writer, error) {
	id, err := newSessionID()
	if err != nil {
		return nil, err
	}
	writer, err := openWriter(id, false)
	if err != nil {
		return nil, err
	}
	if err := writer.appendSessionHeader(); err != nil {
		_ = writer.Close()
		return nil, err
	}
	return writer, nil
}

func OpenWriter(id string) (*Writer, error) {
	return openWriter(id, true)
}

func (w *Writer) ID() string {
	return w.id
}

func (w *Writer) Close() error {
	return w.file.Close()
}

func (w *Writer) AppendMessage(message agent.Message) error {
	payload, err := json.Marshal(message)
	if err != nil {
		return err
	}
	return w.appendRecord(Record{
		Type:      RecordTypeMessage,
		ID:        newRecordID(),
		Timestamp: time.Now().UTC(),
		Payload:   payload,
	})
}

func (w *Writer) AppendEvent(agent.Event) error {
	return nil
}

func LoadMessages(id string) ([]agent.Message, error) {
	path, err := sessionPath(id)
	if err != nil {
		return nil, err
	}
	file, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer file.Close()

	messages := []agent.Message{}
	scanner := bufio.NewScanner(file)
	for scanner.Scan() {
		var record Record
		if err := json.Unmarshal(scanner.Bytes(), &record); err != nil {
			return nil, err
		}
		if record.Type != RecordTypeMessage {
			continue
		}
		message, err := unmarshalMessage(record.Payload)
		if err != nil {
			return nil, err
		}
		messages = append(messages, message)
	}
	if err := scanner.Err(); err != nil {
		return nil, err
	}
	return messages, nil
}

func openWriter(id string, appendExisting bool) (*Writer, error) {
	path, err := sessionPath(id)
	if err != nil {
		return nil, err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return nil, err
	}

	flags := os.O_WRONLY | os.O_CREATE
	if appendExisting {
		flags |= os.O_APPEND
	} else {
		flags |= os.O_EXCL
	}
	file, err := os.OpenFile(path, flags, 0o600)
	if err != nil {
		return nil, err
	}
	return &Writer{
		id:      id,
		file:    file,
		encoder: json.NewEncoder(file),
	}, nil
}

func (w *Writer) appendSessionHeader() error {
	cwd, err := os.Getwd()
	if err != nil {
		return err
	}
	payload, err := json.Marshal(SessionHeader{
		Version:   1,
		ID:        w.id,
		CreatedAt: time.Now().UTC(),
		Cwd:       cwd,
		GoVersion: runtime.Version(),
	})
	if err != nil {
		return err
	}
	return w.appendRecord(Record{
		Type:      RecordTypeSessionHeader,
		ID:        w.id,
		Timestamp: time.Now().UTC(),
		Payload:   payload,
	})
}

func (w *Writer) appendRecord(record Record) error {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.encoder.Encode(record)
}

func newSessionID() (string, error) {
	var bytes [8]byte
	if _, err := rand.Read(bytes[:]); err != nil {
		return "", err
	}
	return hex.EncodeToString(bytes[:]), nil
}

func newRecordID() string {
	var bytes [8]byte
	if _, err := rand.Read(bytes[:]); err != nil {
		return fmt.Sprintf("%d", time.Now().UnixNano())
	}
	return hex.EncodeToString(bytes[:])
}

func sessionPath(id string) (string, error) {
	if id == "" {
		return "", errors.New("session id is required")
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(home, ".pi", "sessions", id+".jsonl"), nil
}

func unmarshalMessage(payload json.RawMessage) (agent.Message, error) {
	var probe struct {
		Content    []json.RawMessage
		Results    []json.RawMessage
		StopReason string
		Model      string
		Usage      agent.Usage
	}
	if err := json.Unmarshal(payload, &probe); err != nil {
		return nil, err
	}
	if probe.Results != nil {
		results, err := unmarshalToolResults(probe.Results)
		if err != nil {
			return nil, err
		}
		return agent.ToolResultMessage{Results: results}, nil
	}
	if probe.StopReason != "" || probe.Model != "" {
		content, err := unmarshalContent(probe.Content)
		if err != nil {
			return nil, err
		}
		return agent.AssistantMessage{
			Content:    content,
			StopReason: probe.StopReason,
			Model:      probe.Model,
			Usage:      probe.Usage,
		}, nil
	}
	content, err := unmarshalContent(probe.Content)
	if err != nil {
		return nil, err
	}
	return agent.UserMessage{Content: content}, nil
}

func unmarshalToolResults(rawResults []json.RawMessage) ([]agent.ToolResult, error) {
	results := make([]agent.ToolResult, 0, len(rawResults))
	for _, rawResult := range rawResults {
		var result struct {
			ToolUseID string
			Content   []json.RawMessage
			Details   json.RawMessage
			IsError   bool
		}
		if err := json.Unmarshal(rawResult, &result); err != nil {
			return nil, err
		}
		content, err := unmarshalContent(result.Content)
		if err != nil {
			return nil, err
		}
		results = append(results, agent.ToolResult{
			ToolUseID: result.ToolUseID,
			Content:   content,
			Details:   result.Details,
			IsError:   result.IsError,
		})
	}
	return results, nil
}

func unmarshalContent(rawBlocks []json.RawMessage) ([]agent.Content, error) {
	content := make([]agent.Content, 0, len(rawBlocks))
	for _, rawBlock := range rawBlocks {
		block, err := unmarshalContentBlock(rawBlock)
		if err != nil {
			return nil, err
		}
		content = append(content, block)
	}
	return content, nil
}

func unmarshalContentBlock(rawBlock json.RawMessage) (agent.Content, error) {
	var probe struct {
		Text      *string
		Thinking  *string
		Signature string
		Source    *agent.ImageSource
		ID        string
		Name      string
		Input     json.RawMessage
		ToolUseID string
		Content   []json.RawMessage
		IsError   bool
	}
	if err := json.Unmarshal(rawBlock, &probe); err != nil {
		return nil, err
	}
	if probe.Text != nil {
		return agent.TextContent{Text: *probe.Text}, nil
	}
	if probe.Thinking != nil {
		return agent.ThinkingContent{Thinking: *probe.Thinking, Signature: probe.Signature}, nil
	}
	if probe.Source != nil {
		return agent.ImageContent{Source: *probe.Source}, nil
	}
	if probe.ID != "" && probe.Name != "" {
		return agent.ToolUseContent{ID: probe.ID, Name: probe.Name, Input: probe.Input}, nil
	}
	if probe.ToolUseID != "" {
		content, err := unmarshalContent(probe.Content)
		if err != nil {
			return nil, err
		}
		return agent.ToolResultContent{
			ToolUseID: probe.ToolUseID,
			Content:   content,
			IsError:   probe.IsError,
		}, nil
	}
	return nil, fmt.Errorf("unknown content block: %s", string(rawBlock))
}
