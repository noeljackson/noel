package config

import (
	"os"
	"path/filepath"
)

const (
	ConfigDirName       = ".pi"
	EnvAgentDir         = "PI_CODING_AGENT_DIR"
	EnvSessionDir       = "PI_CODING_AGENT_SESSION_DIR"
	EnvAgentDirLegacy   = "PI_AGENT_DIR"
	EnvSessionDirLegacy = "PI_SESSION_DIR"
)

type Paths struct {
	AgentDir   string
	SessionDir string
}

func DefaultPaths() (Paths, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return Paths{}, err
	}

	agentDir := os.Getenv(EnvAgentDir)
	if agentDir == "" {
		agentDir = os.Getenv(EnvAgentDirLegacy)
	}
	if agentDir == "" {
		agentDir = filepath.Join(home, ConfigDirName, "agent")
	} else {
		agentDir = expandHome(agentDir, home)
	}

	sessionDir := os.Getenv(EnvSessionDir)
	if sessionDir == "" {
		sessionDir = os.Getenv(EnvSessionDirLegacy)
	}
	if sessionDir == "" {
		sessionDir = filepath.Join(agentDir, "sessions")
	} else {
		sessionDir = expandHome(sessionDir, home)
	}

	return Paths{
		AgentDir:   filepath.Clean(agentDir),
		SessionDir: filepath.Clean(sessionDir),
	}, nil
}

func expandHome(path string, home string) string {
	if path == "~" {
		return home
	}
	if len(path) >= 2 && path[:2] == "~/" {
		return filepath.Join(home, path[2:])
	}
	return path
}
