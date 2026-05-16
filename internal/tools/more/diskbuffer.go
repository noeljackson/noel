package more

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
)

// DiskBuffer stores full tool output in one file per call ID.
type DiskBuffer struct {
	dir string
}

// NewDiskBuffer returns a disk-backed output buffer rooted at dir.
func NewDiskBuffer(dir string) *DiskBuffer {
	return &DiskBuffer{dir: dir}
}

func (b *DiskBuffer) Get(callID string) (string, bool, error) {
	path := b.path(callID)
	if path == "" {
		return "", false, nil
	}
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return "", false, nil
	}
	if err != nil {
		return "", false, err
	}
	return string(data), true, nil
}

func (b *DiskBuffer) Put(callID string, text string) error {
	path := b.path(callID)
	if path == "" {
		return nil
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	return os.WriteFile(path, []byte(text), 0o600)
}

// Cleanup removes every buffered output under this buffer root.
func (b *DiskBuffer) Cleanup() error {
	if b == nil || b.dir == "" {
		return nil
	}
	err := os.RemoveAll(b.dir)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	return err
}

func (b *DiskBuffer) path(callID string) string {
	if b == nil || b.dir == "" {
		return ""
	}
	safe := sanitizeCallID(callID)
	if safe == "" {
		return ""
	}
	return filepath.Join(b.dir, safe+".txt")
}

func sanitizeCallID(callID string) string {
	callID = strings.TrimSpace(callID)
	var builder strings.Builder
	for _, r := range callID {
		switch {
		case r >= 'a' && r <= 'z':
			builder.WriteRune(r)
		case r >= 'A' && r <= 'Z':
			builder.WriteRune(r)
		case r >= '0' && r <= '9':
			builder.WriteRune(r)
		case r == '-' || r == '_' || r == '.':
			builder.WriteRune(r)
		default:
			builder.WriteByte('_')
		}
	}
	return builder.String()
}

var _ Buffer = (*DiskBuffer)(nil)
