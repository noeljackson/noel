package anthropic

import (
	"context"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"

	authstore "github.com/noeljackson/pi/internal/auth"
)

func TestAPIKeyAuthHeaders(t *testing.T) {
	headers, err := APIKeyAuth{Key: "sk-ant-test"}.Headers(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if got := headers["x-api-key"]; got != "sk-ant-test" {
		t.Fatalf("x-api-key = %q, want %q", got, "sk-ant-test")
	}
	if len(headers) != 1 {
		t.Fatalf("headers length = %d, want 1", len(headers))
	}
}

func TestAPIKeyAuthEmptyKey(t *testing.T) {
	_, err := APIKeyAuth{}.Headers(context.Background())
	if err == nil {
		t.Fatal("expected error")
	}
}

func TestClaudeCodeOAuthHeaders(t *testing.T) {
	path := writeCredentials(t, time.Now().Add(time.Hour), "sk-ant-oat-test")

	headers, err := ClaudeCodeOAuth{Path: path}.Headers(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if got := headers["Authorization"]; got != "Bearer sk-ant-oat-test" {
		t.Fatalf("Authorization = %q, want %q", got, "Bearer sk-ant-oat-test")
	}
	if got := headers["anthropic-beta"]; got != "oauth-2025-04-20" {
		t.Fatalf("anthropic-beta = %q, want %q", got, "oauth-2025-04-20")
	}
}

func TestClaudeCodeOAuthExpiredToken(t *testing.T) {
	path := writeCredentials(t, time.Now().Add(-time.Hour), "sk-ant-oat-test")

	_, err := ClaudeCodeOAuth{Path: path}.Headers(context.Background())
	if err == nil {
		t.Fatal("expected error")
	}
	if !strings.Contains(err.Error(), "run `claude login` to refresh") {
		t.Fatalf("error = %q, want claude login guidance", err)
	}
}

func TestClaudeCodeOAuthMissingAccessToken(t *testing.T) {
	path := writeCredentials(t, time.Now().Add(time.Hour), "")

	_, err := ClaudeCodeOAuth{Path: path}.Headers(context.Background())
	if err == nil {
		t.Fatal("expected error")
	}
	if !strings.Contains(err.Error(), "claudeAiOauth.accessToken") {
		t.Fatalf("error = %q, want missing access token error", err)
	}
}

func TestPickAuthUsesEnvAPIKey(t *testing.T) {
	t.Setenv("ANTHROPIC_API_KEY", "sk-ant-env")
	t.Setenv("HOME", t.TempDir())

	auth, err := PickAuth()
	if err != nil {
		t.Fatal(err)
	}
	apiKeyAuth, ok := auth.(APIKeyAuth)
	if !ok {
		t.Fatalf("auth type = %T, want APIKeyAuth", auth)
	}
	if apiKeyAuth.Key != "sk-ant-env" {
		t.Fatalf("key = %q, want %q", apiKeyAuth.Key, "sk-ant-env")
	}
}

func TestPickAuthWithStorePrefersStoredAPIKeyOverEnv(t *testing.T) {
	t.Setenv("ANTHROPIC_API_KEY", "sk-ant-env")
	store := authstore.New(filepath.Join(t.TempDir(), "auth.json"))
	if err := store.Set("anthropic", authstore.ProviderAuth{Type: "api_key", Key: "sk-ant-stored"}); err != nil {
		t.Fatal(err)
	}

	auth, err := PickAuthWithStore(store)
	if err != nil {
		t.Fatal(err)
	}
	apiKeyAuth, ok := auth.(APIKeyAuth)
	if !ok {
		t.Fatalf("auth type = %T, want APIKeyAuth", auth)
	}
	if apiKeyAuth.Key != "sk-ant-stored" {
		t.Fatalf("key = %q, want stored key", apiKeyAuth.Key)
	}
}

func TestPickAuthWithStoreUsesStoredOAuthAfterEnv(t *testing.T) {
	t.Setenv("ANTHROPIC_API_KEY", "")
	t.Setenv("HOME", t.TempDir())
	store := authstore.New(filepath.Join(t.TempDir(), "auth.json"))
	if err := store.Set("anthropic-oauth", authstore.ProviderAuth{
		Type:        "oauth",
		AccessToken: "stored-access",
		ExpiresAt:   time.Now().Add(time.Hour),
	}); err != nil {
		t.Fatal(err)
	}

	auth, err := PickAuthWithStore(store)
	if err != nil {
		t.Fatal(err)
	}
	stored, ok := auth.(StoredOAuth)
	if !ok {
		t.Fatalf("auth type = %T, want StoredOAuth", auth)
	}
	if stored.AccessToken != "stored-access" {
		t.Fatalf("access token = %q", stored.AccessToken)
	}
}

func TestPickAuthUsesClaudeCodeCredentials(t *testing.T) {
	t.Setenv("ANTHROPIC_API_KEY", "")
	home := t.TempDir()
	t.Setenv("HOME", home)
	claudeDir := filepath.Join(home, ".claude")
	if err := os.MkdirAll(claudeDir, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(claudeDir, ".credentials.json"), []byte(`{"claudeAiOauth":{"accessToken":"sk-ant-oat-test"}}`), 0o600); err != nil {
		t.Fatal(err)
	}

	auth, err := PickAuth()
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := auth.(ClaudeCodeOAuth); !ok {
		t.Fatalf("auth type = %T, want ClaudeCodeOAuth", auth)
	}
}

func TestPickAuthMissingCredentials(t *testing.T) {
	t.Setenv("ANTHROPIC_API_KEY", "")
	t.Setenv("HOME", t.TempDir())

	_, err := PickAuth()
	if err == nil {
		t.Fatal("expected error")
	}
	if !strings.Contains(err.Error(), "no ANTHROPIC_API_KEY and no ~/.claude/.credentials.json") {
		t.Fatalf("error = %q, want missing credentials guidance", err)
	}
}

func writeCredentials(t *testing.T, expiresAt time.Time, accessToken string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), ".credentials.json")
	data := `{
		"claudeAiOauth": {
			"accessToken": "` + accessToken + `",
			"refreshToken": "sk-ant-ort-test",
			"expiresAt": ` + strconv.FormatInt(expiresAt.UnixMilli(), 10) + `,
			"rateLimitTier": "test",
			"scopes": ["user"],
			"subscriptionType": "max"
		}
	}`
	if err := os.WriteFile(path, []byte(data), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}
