package observability

import (
	"bytes"
	"context"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/noeljackson/pi/internal/diagnostics"
)

func TestLoggerWritesJSONAndRedacts(t *testing.T) {
	dir := t.TempDir()
	logger := NewLogger(Options{LogDir: dir, Level: slog.LevelDebug})
	defer logger.Close()

	logger.Slog().Info("request Bearer secret-token", "access_token", "raw-token", "safe", "ok")

	data := readLogFile(t, dir, "2026-01-01")
	if len(data) == 0 {
		data = readOnlyLog(t, dir)
	}
	text := string(data)
	if !strings.Contains(text, `"safe":"ok"`) {
		t.Fatalf("log line does not contain JSON attr: %s", text)
	}
	for _, secret := range []string{"secret-token", "raw-token"} {
		if strings.Contains(text, secret) {
			t.Fatalf("log contains unredacted secret %q: %s", secret, text)
		}
	}
}

func TestLoggerRotatesByDay(t *testing.T) {
	dir := t.TempDir()
	handler := &rotatingHandler{dir: dir, level: slog.LevelDebug}
	for _, day := range []string{"2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z"} {
		ts, err := time.Parse(time.RFC3339, day)
		if err != nil {
			t.Fatal(err)
		}
		if err := handler.Handle(context.Background(), slog.Record{Time: ts, Level: slog.LevelInfo, Message: "hello"}); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := os.Stat(filepath.Join(dir, "pi-2026-01-01.log")); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(dir, "pi-2026-01-02.log")); err != nil {
		t.Fatal(err)
	}
}

func TestLoggerBridgesDiagnostics(t *testing.T) {
	var buf bytes.Buffer
	collector := diagnostics.New()
	handler := &rotatingHandler{writer: &buf, level: slog.LevelDebug, collector: collector}
	record := slog.Record{Time: time.Now(), Level: slog.LevelWarn, Message: "failed"}
	record.AddAttrs(slog.String("source", "test"))
	if err := handler.Handle(context.Background(), record); err != nil {
		t.Fatal(err)
	}
	recent := collector.Recent(1)
	if len(recent) != 1 || recent[0].Level != diagnostics.Warning || recent[0].Source != "test" {
		t.Fatalf("diagnostics = %#v", recent)
	}
}

func readOnlyLog(t *testing.T, dir string) []byte {
	t.Helper()
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 1 {
		t.Fatalf("log files = %d, want 1", len(entries))
	}
	data, err := os.ReadFile(filepath.Join(dir, entries[0].Name()))
	if err != nil {
		t.Fatal(err)
	}
	return data
}

func readLogFile(t *testing.T, dir string, day string) []byte {
	t.Helper()
	data, _ := os.ReadFile(filepath.Join(dir, "pi-"+day+".log"))
	return data
}
