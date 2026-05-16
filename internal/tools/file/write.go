package file

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"

	"github.com/noeljackson/pi/internal/agent"
	toolcontract "github.com/noeljackson/pi/internal/tools"
)

var writeSchema = json.RawMessage(`{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false}`)

// WriteTool atomically writes file content.
type WriteTool struct{}

// NewWriteTool returns a write tool.
func NewWriteTool() *WriteTool {
	return &WriteTool{}
}

func (WriteTool) Name() string {
	return "write"
}

func (WriteTool) Description() string {
	return "Write content to a file atomically, creating parent directories as needed."
}

func (WriteTool) Schema() json.RawMessage {
	return writeSchema
}

func (WriteTool) ParallelSafe() bool {
	return false
}

func (WriteTool) Execute(ctx context.Context, input json.RawMessage, tc agent.ToolCallContext) (agent.ToolResult, error) {
	var args struct {
		Path    string `json:"path"`
		Content string `json:"content"`
	}
	if err := json.Unmarshal(input, &args); err != nil {
		return agent.ToolResult{}, err
	}
	if args.Path == "" {
		return agent.ToolResult{}, fmt.Errorf("path is required")
	}
	path := resolvePath(args.Path, tc.Cwd)
	bytes := []byte(args.Content)

	var result agent.ToolResult
	err := WithLock(path, func() error {
		if err := ctx.Err(); err != nil {
			return err
		}
		if err := atomicWrite(path, bytes, 0o666); err != nil {
			return err
		}
		var err error
		result, err = textResult(tc.CallID, fmt.Sprintf("Successfully wrote %d bytes to %s", len(bytes), args.Path), toolcontract.WriteDetails{
			Path:  path,
			Bytes: len(bytes),
			Lines: lineCount(args.Content),
		}, false)
		return err
	})
	if err != nil {
		return agent.ToolResult{}, err
	}
	return result, nil
}

func atomicWrite(path string, content []byte, perm os.FileMode) error {
	dir := filepath.Dir(path)
	if err := os.MkdirAll(dir, 0o777); err != nil {
		return err
	}

	suffix, err := randomSuffix()
	if err != nil {
		return err
	}
	tmpPath := fmt.Sprintf("%s.tmp-%s", path, suffix)
	file, err := os.OpenFile(tmpPath, os.O_CREATE|os.O_EXCL|os.O_WRONLY, perm)
	if err != nil {
		return err
	}

	removeTmp := true
	defer func() {
		if removeTmp {
			_ = os.Remove(tmpPath)
		}
	}()

	if _, err := file.Write(content); err != nil {
		_ = file.Close()
		return err
	}
	if err := file.Sync(); err != nil {
		_ = file.Close()
		return err
	}
	if err := file.Close(); err != nil {
		return err
	}
	if err := os.Rename(tmpPath, path); err != nil {
		return err
	}
	removeTmp = false

	if dirFile, err := os.Open(dir); err == nil {
		_ = dirFile.Sync()
		_ = dirFile.Close()
	}
	return nil
}

func randomSuffix() (string, error) {
	var bytes [8]byte
	if _, err := rand.Read(bytes[:]); err != nil {
		return "", err
	}
	return hex.EncodeToString(bytes[:]), nil
}

var _ agent.Tool = (*WriteTool)(nil)
