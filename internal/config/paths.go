package config

import (
	"os"
	"path/filepath"
)

// Paths contains pi's persistent configuration and session paths.
type Paths struct {
	AgentDir     string
	SessionDir   string
	AuthFile     string
	SettingsFile string
	ModelsFile   string
	ResourcesDir string
	ThemesDir    string
}

// ResolvePaths resolves pi's configuration paths from environment variables.
func ResolvePaths() (Paths, error) {
	agentDir := os.Getenv("PI_AGENT_DIR")
	if agentDir == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return Paths{}, err
		}
		agentDir = filepath.Join(home, ".pi")
	} else {
		expanded, err := expandTilde(agentDir)
		if err != nil {
			return Paths{}, err
		}
		agentDir = expanded
	}

	sessionDir := os.Getenv("PI_SESSION_DIR")
	if sessionDir == "" {
		sessionDir = filepath.Join(agentDir, "sessions")
	} else {
		expanded, err := expandTilde(sessionDir)
		if err != nil {
			return Paths{}, err
		}
		sessionDir = expanded
	}

	return Paths{
		AgentDir:     agentDir,
		SessionDir:   sessionDir,
		AuthFile:     filepath.Join(agentDir, "auth.json"),
		SettingsFile: filepath.Join(agentDir, "settings.json"),
		ModelsFile:   filepath.Join(agentDir, "models.json"),
		ResourcesDir: filepath.Join(agentDir, "resources"),
		ThemesDir:    filepath.Join(agentDir, "themes"),
	}, nil
}

func expandTilde(path string) (string, error) {
	if path != "~" && len(path) < 2 || len(path) >= 2 && path[:2] != "~/" {
		return path, nil
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	if path == "~" {
		return home, nil
	}
	return filepath.Join(home, path[2:]), nil
}
