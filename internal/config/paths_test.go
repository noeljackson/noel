package config

import (
	"path/filepath"
	"testing"
)

func TestResolvePathsDefaults(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("PI_AGENT_DIR", "")
	t.Setenv("PI_SESSION_DIR", "")

	paths, err := ResolvePaths()
	if err != nil {
		t.Fatal(err)
	}
	wantAgent := filepath.Join(home, ".pi")
	if paths.AgentDir != wantAgent {
		t.Fatalf("AgentDir = %q, want %q", paths.AgentDir, wantAgent)
	}
	if paths.SessionDir != filepath.Join(wantAgent, "sessions") {
		t.Fatalf("SessionDir = %q", paths.SessionDir)
	}
	if paths.AuthFile != filepath.Join(wantAgent, "auth.json") {
		t.Fatalf("AuthFile = %q", paths.AuthFile)
	}
}

func TestResolvePathsEnvOverrides(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("PI_AGENT_DIR", "~/agent")
	t.Setenv("PI_SESSION_DIR", "~/sessions")

	paths, err := ResolvePaths()
	if err != nil {
		t.Fatal(err)
	}
	if paths.AgentDir != filepath.Join(home, "agent") {
		t.Fatalf("AgentDir = %q", paths.AgentDir)
	}
	if paths.SessionDir != filepath.Join(home, "sessions") {
		t.Fatalf("SessionDir = %q", paths.SessionDir)
	}
}
