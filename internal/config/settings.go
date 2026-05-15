package config

import (
	"encoding/json"
	"errors"
	"os"
	"sync"
)

// ToolsSettings controls tool allow/deny lists.
type ToolsSettings struct {
	Allow []string `json:"allow,omitempty"`
	Deny  []string `json:"deny,omitempty"`
}

// Settings is pi's persisted user settings surface.
type Settings struct {
	Model        string        `json:"model,omitempty"`
	Thinking     string        `json:"thinking,omitempty"`
	AutoCompact  bool          `json:"autoCompact"`
	QuietStartup bool          `json:"quietStartup"`
	Tools        ToolsSettings `json:"tools"`
	Telemetry    bool          `json:"telemetry"`
	Theme        string        `json:"theme,omitempty"`
}

// Manager provides synchronized access to settings.json.
type Manager struct {
	path     string
	mu       sync.Mutex
	settings Settings
}

// NewManager loads settings from path, applying defaults for missing fields.
func NewManager(path string) (*Manager, error) {
	m := &Manager{path: path}
	if err := m.Reload(); err != nil {
		return nil, err
	}
	return m, nil
}

// Get returns a copy of the current settings.
func (m *Manager) Get() Settings {
	m.mu.Lock()
	defer m.mu.Unlock()
	return cloneSettings(m.settings)
}

// Update applies fn to a copy of settings and persists the result atomically.
func (m *Manager) Update(fn func(*Settings)) error {
	m.mu.Lock()
	defer m.mu.Unlock()

	next := cloneSettings(m.settings)
	fn(&next)
	next = applyRuntimeDefaults(next)
	data, err := json.MarshalIndent(next, "", "  ")
	if err != nil {
		return err
	}
	data = append(data, '\n')
	if err := writeFileAtomic(m.path, data, 0o600); err != nil {
		return err
	}
	m.settings = next
	return nil
}

// Reload re-reads settings from disk.
func (m *Manager) Reload() error {
	m.mu.Lock()
	defer m.mu.Unlock()

	data, err := os.ReadFile(m.path)
	if errors.Is(err, os.ErrNotExist) {
		m.settings = defaultSettings()
		return nil
	}
	if err != nil {
		return err
	}
	if len(data) == 0 {
		m.settings = defaultSettings()
		return nil
	}
	settings := defaultSettings()
	if err := json.Unmarshal(data, &settings); err != nil {
		return err
	}
	m.settings = applyRuntimeDefaults(settings)
	return nil
}

func defaultSettings() Settings {
	return Settings{
		Model:       "claude-sonnet-4-6",
		Thinking:    "auto",
		AutoCompact: true,
		Tools: ToolsSettings{
			Allow: []string{},
			Deny:  []string{},
		},
		Theme: "default",
	}
}

func applyRuntimeDefaults(settings Settings) Settings {
	if settings.Model == "" {
		settings.Model = "claude-sonnet-4-6"
	}
	if settings.Thinking == "" {
		settings.Thinking = "auto"
	}
	if settings.Tools.Allow == nil {
		settings.Tools.Allow = []string{}
	}
	if settings.Tools.Deny == nil {
		settings.Tools.Deny = []string{}
	}
	if settings.Theme == "" {
		settings.Theme = "default"
	}
	return settings
}

func cloneSettings(settings Settings) Settings {
	settings.Tools.Allow = append([]string(nil), settings.Tools.Allow...)
	settings.Tools.Deny = append([]string(nil), settings.Tools.Deny...)
	return settings
}
