package diagnostics

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	authstore "github.com/noeljackson/pi/internal/auth"
	"github.com/noeljackson/pi/internal/config"
	"github.com/noeljackson/pi/internal/models"
	"github.com/noeljackson/pi/internal/resources"
)

type Level int

const (
	Debug Level = iota
	Info
	Warning
	Error
)

type Diagnostic struct {
	Level   Level
	Source  string
	Message string
	Time    time.Time
	Context map[string]any
}

type Collector struct {
	mu          sync.RWMutex
	diagnostics []Diagnostic
	subscribers map[int]func(Diagnostic)
	nextID      int
}

func New() *Collector {
	return &Collector{subscribers: map[int]func(Diagnostic){}}
}

func (c *Collector) Add(d Diagnostic) {
	if c == nil {
		return
	}
	if d.Time.IsZero() {
		d.Time = time.Now().UTC()
	}

	c.mu.Lock()
	c.diagnostics = append(c.diagnostics, d)
	subscribers := make([]func(Diagnostic), 0, len(c.subscribers))
	for _, subscriber := range c.subscribers {
		subscribers = append(subscribers, subscriber)
	}
	c.mu.Unlock()

	for _, subscriber := range subscribers {
		subscriber(d)
	}
}

func (c *Collector) Recent(n int) []Diagnostic {
	if c == nil || n <= 0 {
		return nil
	}
	c.mu.RLock()
	defer c.mu.RUnlock()
	start := len(c.diagnostics) - n
	if start < 0 {
		start = 0
	}
	out := make([]Diagnostic, len(c.diagnostics[start:]))
	copy(out, c.diagnostics[start:])
	return out
}

func (c *Collector) Subscribe(fn func(Diagnostic)) (unsubscribe func()) {
	if c == nil || fn == nil {
		return func() {}
	}
	c.mu.Lock()
	id := c.nextID
	c.nextID++
	c.subscribers[id] = fn
	c.mu.Unlock()
	return func() {
		c.mu.Lock()
		defer c.mu.Unlock()
		delete(c.subscribers, id)
	}
}

func (c *Collector) Clear() {
	if c == nil {
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	c.diagnostics = nil
}

func StartupDiagnostics(paths config.Paths) []Diagnostic {
	now := time.Now().UTC()
	var out []Diagnostic

	if _, err := config.NewManager(paths.SettingsFile); err != nil {
		out = append(out, Diagnostic{Level: Warning, Source: paths.SettingsFile, Message: err.Error(), Time: now})
	}
	if _, err := models.Load(paths.ModelsFile); err != nil {
		out = append(out, Diagnostic{Level: Warning, Source: paths.ModelsFile, Message: err.Error(), Time: now})
	}
	if _, err := authstore.New(paths.AuthFile).List(); err != nil {
		out = append(out, Diagnostic{Level: Warning, Source: paths.AuthFile, Message: err.Error(), Time: now})
	}
	if os.Getenv("ANTHROPIC_API_KEY") == "" && !fileExists(paths.AuthFile) && !fileExists(claudeCredentialsPath()) {
		out = append(out, Diagnostic{Level: Warning, Source: "auth", Message: "no Anthropic credentials found", Time: now})
	}

	cwd, err := os.Getwd()
	if err == nil {
		loaded, loadErr := (&resources.ResourceLoader{Paths: paths, ProjectRoot: cwd}).Load()
		if loadErr != nil {
			out = append(out, Diagnostic{Level: Error, Source: "resources", Message: loadErr.Error(), Time: now})
		}
		for _, diagnostic := range loaded.Diagnostics {
			out = append(out, fromResourceDiagnostic(diagnostic, now))
		}
	}

	for _, path := range []string{paths.AgentDir, paths.SessionDir, paths.ResourcesDir, paths.ThemesDir} {
		if path == "" {
			continue
		}
		if info, err := os.Stat(path); err == nil && !info.IsDir() {
			out = append(out, Diagnostic{Level: Warning, Source: path, Message: "expected directory", Time: now})
		}
	}
	return out
}

func (l Level) String() string {
	switch l {
	case Debug:
		return "debug"
	case Info:
		return "info"
	case Warning:
		return "warning"
	case Error:
		return "error"
	default:
		return fmt.Sprintf("level(%d)", int(l))
	}
}

func ParseLevel(value string) Level {
	switch strings.ToLower(strings.TrimSpace(value)) {
	case "debug":
		return Debug
	case "info":
		return Info
	case "warning", "warn":
		return Warning
	case "error":
		return Error
	default:
		return Info
	}
}

func fromResourceDiagnostic(d resources.Diagnostic, now time.Time) Diagnostic {
	return Diagnostic{
		Level:   ParseLevel(d.Level),
		Source:  d.Source,
		Message: d.Message,
		Time:    now,
	}
}

func fileExists(path string) bool {
	if path == "" {
		return false
	}
	info, err := os.Stat(path)
	return err == nil && !info.IsDir()
}

func claudeCredentialsPath() string {
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	return filepath.Join(home, ".claude", ".credentials.json")
}
