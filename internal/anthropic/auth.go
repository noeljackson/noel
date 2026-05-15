package anthropic

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"time"
)

// AuthSource produces the headers needed to authenticate an Anthropic API
// request. Implementations may cache results; callers must not assume
// thread-safety unless documented.
type AuthSource interface {
	// Headers returns the auth-related headers for one request. Implementations
	// may refresh state on every call.
	Headers(ctx context.Context) (map[string]string, error)
}

// APIKeyAuth uses an x-api-key header. This is the conventional Anthropic API
// authentication path.
type APIKeyAuth struct {
	Key string
}

func (a APIKeyAuth) Headers(_ context.Context) (map[string]string, error) {
	if a.Key == "" {
		return nil, errors.New("anthropic: empty API key")
	}
	return map[string]string{"x-api-key": a.Key}, nil
}

// ClaudeCodeOAuth reads OAuth credentials from Claude Code's local credentials
// file and authenticates via the OAuth bearer flow. Refresh is not implemented;
// when the token expires Headers returns an error and the user must re-run
// `claude login` to refresh the file.
type ClaudeCodeOAuth struct {
	// Path overrides the default credentials path. Empty means use the default
	// location (~/.claude/.credentials.json).
	Path string
}

func (a ClaudeCodeOAuth) credsPath() (string, error) {
	if a.Path != "" {
		return a.Path, nil
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", fmt.Errorf("anthropic: cannot resolve home dir: %w", err)
	}
	return filepath.Join(home, ".claude", ".credentials.json"), nil
}

type credsFile struct {
	ClaudeAIOAuth struct {
		AccessToken      string   `json:"accessToken"`
		RefreshToken     string   `json:"refreshToken"`
		ExpiresAt        int64    `json:"expiresAt"` // milliseconds since epoch
		Scopes           []string `json:"scopes"`
		SubscriptionType string   `json:"subscriptionType"`
	} `json:"claudeAiOauth"`
}

func (a ClaudeCodeOAuth) Headers(_ context.Context) (map[string]string, error) {
	path, err := a.credsPath()
	if err != nil {
		return nil, err
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("anthropic: read Claude Code credentials at %s: %w", path, err)
	}
	var c credsFile
	if err := json.Unmarshal(data, &c); err != nil {
		return nil, fmt.Errorf("anthropic: parse Claude Code credentials: %w", err)
	}
	if c.ClaudeAIOAuth.AccessToken == "" {
		return nil, fmt.Errorf("anthropic: credentials file %s has no claudeAiOauth.accessToken", path)
	}
	if c.ClaudeAIOAuth.ExpiresAt > 0 {
		exp := time.UnixMilli(c.ClaudeAIOAuth.ExpiresAt)
		if time.Now().After(exp) {
			return nil, fmt.Errorf("anthropic: Claude Code access token expired at %s - run `claude login` to refresh", exp.Format(time.RFC3339))
		}
	}
	return map[string]string{
		"Authorization":  "Bearer " + c.ClaudeAIOAuth.AccessToken,
		"anthropic-beta": "oauth-2025-04-20",
	}, nil
}

// PickAuth returns the appropriate AuthSource based on environment and file
// presence. Order:
//  1. ANTHROPIC_API_KEY env var (returns APIKeyAuth).
//  2. ~/.claude/.credentials.json present (returns ClaudeCodeOAuth).
//  3. Error.
func PickAuth() (AuthSource, error) {
	if k := os.Getenv("ANTHROPIC_API_KEY"); k != "" {
		return APIKeyAuth{Key: k}, nil
	}
	home, err := os.UserHomeDir()
	if err == nil {
		path := filepath.Join(home, ".claude", ".credentials.json")
		if _, statErr := os.Stat(path); statErr == nil {
			return ClaudeCodeOAuth{}, nil
		}
	}
	return nil, errors.New("anthropic: no ANTHROPIC_API_KEY and no ~/.claude/.credentials.json - set the env var or run `claude login`")
}
