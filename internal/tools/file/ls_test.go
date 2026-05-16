package file

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

func TestLsLimitAndNotice(t *testing.T) {
	dir := t.TempDir()
	for _, name := range []string{"b", "a", "c"} {
		if err := os.WriteFile(filepath.Join(dir, name), []byte("x"), 0o666); err != nil {
			t.Fatal(err)
		}
	}
	result, err := NewLsTool().Execute(context.Background(), json.RawMessage(`{"limit":2}`), agent.ToolCallContext{Cwd: dir})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	got := toolText(t, result)
	if !strings.HasPrefix(got, "a\nb\n") {
		t.Fatalf("content = %q, want alpha ordering", got)
	}
	if !strings.Contains(got, "(showing 2 of 3 entries)") {
		t.Fatalf("content = %q, want truncation notice", got)
	}
}
