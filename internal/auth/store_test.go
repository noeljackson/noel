package auth

import (
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestStoreRoundTripSetGetDeleteList(t *testing.T) {
	path := filepath.Join(t.TempDir(), "auth.json")
	store := New(path)
	expires := time.Now().Add(time.Hour).Truncate(time.Millisecond)

	if err := store.Set("anthropic", ProviderAuth{Type: "api_key", Key: "sk-ant-test"}); err != nil {
		t.Fatal(err)
	}
	if err := store.Set("anthropic-oauth", ProviderAuth{Type: "oauth", AccessToken: "access", RefreshToken: "refresh", ExpiresAt: expires}); err != nil {
		t.Fatal(err)
	}
	got, ok, err := store.Get("anthropic-oauth")
	if err != nil {
		t.Fatal(err)
	}
	if !ok {
		t.Fatal("missing anthropic-oauth")
	}
	if got.AccessToken != "access" || !got.ExpiresAt.Equal(expires) {
		t.Fatalf("auth = %#v", got)
	}
	all, err := store.List()
	if err != nil {
		t.Fatal(err)
	}
	if len(all) != 2 {
		t.Fatalf("len = %d, want 2", len(all))
	}
	if err := store.Delete("anthropic"); err != nil {
		t.Fatal(err)
	}
	if _, ok, err := store.Get("anthropic"); err != nil || ok {
		t.Fatalf("Get after delete ok=%v err=%v", ok, err)
	}
}

func TestStoreMissingFileHandling(t *testing.T) {
	store := New(filepath.Join(t.TempDir(), "auth.json"))
	all, err := store.List()
	if err != nil {
		t.Fatal(err)
	}
	if len(all) != 0 {
		t.Fatalf("len = %d, want 0", len(all))
	}
}

func TestStoreWritesMode0600AndNoTempFiles(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "auth.json")
	store := New(path)
	if err := store.Set("openai", ProviderAuth{Type: "api_key", Key: "sk-test"}); err != nil {
		t.Fatal(err)
	}
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := info.Mode().Perm(); got != 0o600 {
		t.Fatalf("mode = %o, want 600", got)
	}
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range entries {
		if entry.Name() != "auth.json" {
			t.Fatalf("unexpected temp file left behind: %s", entry.Name())
		}
	}
}

func TestStoreReadsTopLevelTSShape(t *testing.T) {
	path := filepath.Join(t.TempDir(), "auth.json")
	if err := os.WriteFile(path, []byte(`{"anthropic":{"type":"api_key","key":"sk-ant-test"}}`), 0o600); err != nil {
		t.Fatal(err)
	}
	got, ok, err := New(path).Get("anthropic")
	if err != nil {
		t.Fatal(err)
	}
	if !ok || got.Key != "sk-ant-test" {
		t.Fatalf("auth ok=%v value=%#v", ok, got)
	}
}
