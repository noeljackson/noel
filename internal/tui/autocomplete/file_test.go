package autocomplete

import (
	"context"
	"os"
	"path/filepath"
	"testing"
)

func TestFileProviderAtPathCompletion(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "package.json"), []byte("{}"), 0o600); err != nil {
		t.Fatal(err)
	}
	provider := NewFileProvider(dir)
	got := provider.Suggestions(context.Background(), "@./pa", len("@./pa"))
	if len(got) != 1 {
		t.Fatalf("got %#v", got)
	}
	if got[0].Insert != "@./package.json " {
		t.Fatalf("insert = %q", got[0].Insert)
	}
}

func TestFileProviderTildeCompletion(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	if err := os.WriteFile(filepath.Join(home, "notes.txt"), []byte("x"), 0o600); err != nil {
		t.Fatal(err)
	}
	provider := NewFileProvider(t.TempDir())
	got := provider.ForceSuggestions("~/no", len("~/no"))
	if len(got) != 1 || got[0].Insert != "~/notes.txt" {
		t.Fatalf("got %#v", got)
	}
}
