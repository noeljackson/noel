package config

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sync"
	"testing"
)

func TestSettingsUpdateRoundTrip(t *testing.T) {
	path := filepath.Join(t.TempDir(), "settings.json")
	manager, err := NewManager(path)
	if err != nil {
		t.Fatal(err)
	}
	if err := manager.Update(func(settings *Settings) {
		settings.Model = "claude-opus-4-7"
		settings.Tools.Allow = []string{"bash", "read"}
		settings.Telemetry = true
	}); err != nil {
		t.Fatal(err)
	}

	reloaded, err := NewManager(path)
	if err != nil {
		t.Fatal(err)
	}
	settings := reloaded.Get()
	if settings.Model != "claude-opus-4-7" {
		t.Fatalf("Model = %q", settings.Model)
	}
	if len(settings.Tools.Allow) != 2 || settings.Tools.Allow[0] != "bash" {
		t.Fatalf("Tools.Allow = %#v", settings.Tools.Allow)
	}
	if !settings.Telemetry {
		t.Fatal("Telemetry = false, want true")
	}
}

func TestSettingsDefaultsAppliedWhenFieldsMissing(t *testing.T) {
	path := filepath.Join(t.TempDir(), "settings.json")
	if err := os.WriteFile(path, []byte(`{"tools":{"allow":["read"]}}`), 0o600); err != nil {
		t.Fatal(err)
	}
	manager, err := NewManager(path)
	if err != nil {
		t.Fatal(err)
	}
	settings := manager.Get()
	if settings.Model != "claude-sonnet-4-6" {
		t.Fatalf("Model = %q", settings.Model)
	}
	if settings.Thinking != "auto" {
		t.Fatalf("Thinking = %q", settings.Thinking)
	}
	if !settings.AutoCompact {
		t.Fatal("AutoCompact = false, want true")
	}
	if settings.Theme != "default" {
		t.Fatalf("Theme = %q", settings.Theme)
	}
}

func TestSettingsPreservesExplicitFalseAutoCompact(t *testing.T) {
	path := filepath.Join(t.TempDir(), "settings.json")
	if err := os.WriteFile(path, []byte(`{"autoCompact":false}`), 0o600); err != nil {
		t.Fatal(err)
	}
	manager, err := NewManager(path)
	if err != nil {
		t.Fatal(err)
	}
	if manager.Get().AutoCompact {
		t.Fatal("AutoCompact = true, want explicit false")
	}
}

func TestSettingsConcurrentUpdate(t *testing.T) {
	path := filepath.Join(t.TempDir(), "settings.json")
	manager, err := NewManager(path)
	if err != nil {
		t.Fatal(err)
	}

	var wg sync.WaitGroup
	for i := 0; i < 20; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			if err := manager.Update(func(settings *Settings) {
				settings.Tools.Allow = append(settings.Tools.Allow, "bash")
			}); err != nil {
				t.Errorf("Update: %v", err)
			}
		}()
	}
	wg.Wait()

	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var settings Settings
	if err := json.Unmarshal(data, &settings); err != nil {
		t.Fatal(err)
	}
	if len(settings.Tools.Allow) != 20 {
		t.Fatalf("allow length = %d, want 20", len(settings.Tools.Allow))
	}
}
