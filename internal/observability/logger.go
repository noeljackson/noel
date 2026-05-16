package observability

import (
	"context"
	"fmt"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"time"

	"github.com/noeljackson/pi/internal/diagnostics"
)

type Logger struct {
	handler *rotatingHandler
	logger  *slog.Logger
}

type Options struct {
	LogDir    string
	Level     slog.Level
	Collector *diagnostics.Collector
}

func NewLogger(opts Options) *Logger {
	handler := &rotatingHandler{
		dir:       opts.LogDir,
		level:     opts.Level,
		collector: opts.Collector,
	}
	return &Logger{handler: handler, logger: slog.New(handler)}
}

func (l *Logger) Slog() *slog.Logger {
	if l == nil || l.logger == nil {
		return slog.Default()
	}
	return l.logger
}

func (l *Logger) Close() error {
	if l == nil || l.handler == nil {
		return nil
	}
	return l.handler.close()
}

type rotatingHandler struct {
	mu        sync.Mutex
	dir       string
	level     slog.Level
	collector *diagnostics.Collector
	attrs     []slog.Attr
	groups    []string
	day       string
	file      *os.File
	handler   slog.Handler
	writer    io.Writer
}

func (h *rotatingHandler) Enabled(_ context.Context, level slog.Level) bool {
	return level >= h.level
}

func (h *rotatingHandler) Handle(ctx context.Context, record slog.Record) error {
	if err := h.ensureHandler(record.Time); err != nil {
		return err
	}
	record.Message = RedactString(record.Message)
	h.bridgeDiagnostic(record)
	return h.handler.Handle(ctx, record)
}

func (h *rotatingHandler) WithAttrs(attrs []slog.Attr) slog.Handler {
	clone := h.clone()
	clone.attrs = append(clone.attrs, attrs...)
	return clone
}

func (h *rotatingHandler) WithGroup(name string) slog.Handler {
	clone := h.clone()
	if name != "" {
		clone.groups = append(clone.groups, name)
	}
	return clone
}

func (h *rotatingHandler) clone() *rotatingHandler {
	h.mu.Lock()
	defer h.mu.Unlock()
	return &rotatingHandler{
		dir:       h.dir,
		level:     h.level,
		collector: h.collector,
		attrs:     append([]slog.Attr(nil), h.attrs...),
		groups:    append([]string(nil), h.groups...),
		writer:    h.writer,
	}
}

func (h *rotatingHandler) ensureHandler(t time.Time) error {
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.writer != nil {
		if h.handler == nil {
			h.handler = h.newJSONHandler(h.writer)
		}
		return nil
	}
	if h.dir == "" {
		h.dir = "."
	}
	if t.IsZero() {
		t = time.Now()
	}
	day := t.Format("2006-01-02")
	if h.handler != nil && h.day == day {
		return nil
	}
	if err := os.MkdirAll(h.dir, 0o700); err != nil {
		return err
	}
	if h.file != nil {
		_ = h.file.Close()
		h.file = nil
	}
	path := filepath.Join(h.dir, fmt.Sprintf("pi-%s.log", day))
	file, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
	if err != nil {
		return err
	}
	h.file = file
	h.day = day
	h.handler = h.newJSONHandler(file)
	return nil
}

func (h *rotatingHandler) newJSONHandler(w io.Writer) slog.Handler {
	var handler slog.Handler = slog.NewJSONHandler(w, &slog.HandlerOptions{
		Level:       h.level,
		ReplaceAttr: redactAttr,
	})
	for _, group := range h.groups {
		handler = handler.WithGroup(group)
	}
	if len(h.attrs) > 0 {
		handler = handler.WithAttrs(h.attrs)
	}
	return handler
}

func (h *rotatingHandler) bridgeDiagnostic(record slog.Record) {
	if h.collector == nil {
		return
	}
	isDiagnostic := record.Level >= slog.LevelWarn
	source := ""
	contextValues := map[string]any{}
	record.Attrs(func(attr slog.Attr) bool {
		key := strings.ToLower(attr.Key)
		if key == "diagnostic" && attr.Value.Kind() == slog.KindBool && attr.Value.Bool() {
			isDiagnostic = true
		}
		if key == "source" {
			source = attr.Value.String()
		}
		contextValues[attr.Key] = redactedValue(attr)
		return true
	})
	if !isDiagnostic {
		return
	}
	level := diagnostics.Info
	if record.Level <= slog.LevelDebug {
		level = diagnostics.Debug
	} else if record.Level >= slog.LevelError {
		level = diagnostics.Error
	} else if record.Level >= slog.LevelWarn {
		level = diagnostics.Warning
	}
	h.collector.Add(diagnostics.Diagnostic{
		Level:   level,
		Source:  source,
		Message: RedactString(record.Message),
		Time:    record.Time,
		Context: contextValues,
	})
}

func (h *rotatingHandler) close() error {
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.file == nil {
		return nil
	}
	err := h.file.Close()
	h.file = nil
	h.handler = nil
	return err
}

func redactAttr(groups []string, attr slog.Attr) slog.Attr {
	_ = groups
	if isSensitiveKey(attr.Key) {
		attr.Value = slog.StringValue("[REDACTED]")
		return attr
	}
	if attr.Value.Kind() == slog.KindString {
		attr.Value = slog.StringValue(RedactString(attr.Value.String()))
	}
	return attr
}

var sensitiveKeys = []string{
	"access_token",
	"accesstoken",
	"refresh_token",
	"refreshtoken",
	"api_key",
	"apikey",
	"authorization",
	"bearer",
}

func isSensitiveKey(key string) bool {
	normalized := strings.ToLower(strings.ReplaceAll(key, "-", "_"))
	for _, sensitive := range sensitiveKeys {
		if normalized == sensitive || strings.Contains(normalized, sensitive) {
			return true
		}
	}
	return false
}

var redactionPatterns = []*regexp.Regexp{
	regexp.MustCompile(`(?i)bearer\s+[A-Za-z0-9._~+/=-]+`),
	regexp.MustCompile(`(?i)(access_token|refresh_token|api_key|authorization)=([^\s&]+)`),
	regexp.MustCompile(`(?i)("?(access_token|refresh_token|api_key|authorization)"?\s*:\s*")([^"]+)(")`),
}

func RedactString(value string) string {
	out := value
	out = redactionPatterns[0].ReplaceAllString(out, "Bearer [REDACTED]")
	out = redactionPatterns[1].ReplaceAllString(out, "${1}=[REDACTED]")
	out = redactionPatterns[2].ReplaceAllString(out, "${1}[REDACTED]${4}")
	return out
}

func redactedValue(attr slog.Attr) any {
	if isSensitiveKey(attr.Key) {
		return "[REDACTED]"
	}
	if attr.Value.Kind() == slog.KindString {
		return RedactString(attr.Value.String())
	}
	return attr.Value.Any()
}
