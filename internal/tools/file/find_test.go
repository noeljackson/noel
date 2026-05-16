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

func TestFindLimitHonored(t *testing.T) {
	dir := t.TempDir()
	for _, name := range []string{"a.txt", "b.txt", "c.txt"} {
		if err := os.WriteFile(filepath.Join(dir, name), []byte("x"), 0o666); err != nil {
			t.Fatal(err)
		}
	}
	result, err := NewFindTool().Execute(context.Background(), json.RawMessage(`{"pattern":"*.txt","limit":2}`), agent.ToolCallContext{Cwd: dir})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	lines := strings.Split(toolText(t, result), "\n")
	if len(lines) < 2 || strings.HasPrefix(lines[0], "./") {
		t.Fatalf("content = %q", toolText(t, result))
	}
	if !strings.Contains(toolText(t, result), "2 results limit reached") {
		t.Fatalf("content = %q, want limit notice", toolText(t, result))
	}
}

func TestFindTypeFiltering(t *testing.T) {
	dir := t.TempDir()
	if err := os.Mkdir(filepath.Join(dir, "adir"), 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "afile"), []byte("x"), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("afile", filepath.Join(dir, "alink")); err != nil {
		t.Fatal(err)
	}
	tests := []struct {
		name string
		typ  string
		want string
	}{
		{name: "file", typ: "f", want: "afile"},
		{name: "dir", typ: "d", want: "adir/"},
		{name: "link", typ: "l", want: "alink"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			result, err := NewFindTool().Execute(context.Background(), json.RawMessage(`{"pattern":"a*","type":"`+tt.typ+`"}`), agent.ToolCallContext{Cwd: dir})
			if err != nil {
				t.Fatalf("Execute returned error: %v", err)
			}
			if !strings.Contains(toolText(t, result), tt.want) {
				t.Fatalf("content = %q, want %q", toolText(t, result), tt.want)
			}
		})
	}
}
