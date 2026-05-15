package auth

import (
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"net/url"
	"testing"
	"time"
)

func TestGeneratePKCECorrectness(t *testing.T) {
	pkce, err := generatePKCE()
	if err != nil {
		t.Fatal(err)
	}
	if len(pkce.Verifier) < 43 {
		t.Fatalf("verifier length = %d", len(pkce.Verifier))
	}
	sum := sha256.Sum256([]byte(pkce.Verifier))
	want := base64.RawURLEncoding.EncodeToString(sum[:])
	if pkce.Challenge != want {
		t.Fatalf("challenge = %q, want %q", pkce.Challenge, want)
	}
}

func TestLoginAnthropicCallbackWiring(t *testing.T) {
	oldAuthorizeURL := anthropicAuthorizeURL
	oldTokenURL := anthropicTokenURL
	oldPort := anthropicCallbackPort
	defer func() {
		anthropicAuthorizeURL = oldAuthorizeURL
		anthropicTokenURL = oldTokenURL
		anthropicCallbackPort = oldPort
	}()

	tokenServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var payload map[string]string
		if err := json.NewDecoder(r.Body).Decode(&payload); err != nil {
			t.Error(err)
		}
		if payload["grant_type"] != "authorization_code" {
			t.Errorf("grant_type = %q", payload["grant_type"])
		}
		if payload["code_verifier"] == "" {
			t.Error("missing code_verifier")
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"access_token":"access","refresh_token":"refresh","expires_in":3600,"scope":"user:profile","subscription_type":"max"}`))
	}))
	defer tokenServer.Close()

	anthropicAuthorizeURL = "https://claude.ai/oauth/authorize"
	anthropicTokenURL = tokenServer.URL
	anthropicCallbackPort = 0

	result, err := LoginAnthropic(context.Background(), func(authURL string) error {
		u, err := url.Parse(authURL)
		if err != nil {
			return err
		}
		redirectURI := u.Query().Get("redirect_uri")
		state := u.Query().Get("state")
		go func() {
			time.Sleep(20 * time.Millisecond)
			callbackURL, parseErr := url.Parse(redirectURI)
			if parseErr != nil {
				t.Error(parseErr)
				return
			}
			q := callbackURL.Query()
			q.Set("code", "code")
			q.Set("state", state)
			callbackURL.RawQuery = q.Encode()
			resp, getErr := http.Get(callbackURL.String())
			if getErr != nil {
				t.Error(getErr)
				return
			}
			_ = resp.Body.Close()
		}()
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
	if result.AccessToken != "access" || result.RefreshToken != "refresh" {
		t.Fatalf("result = %#v", result)
	}
	if len(result.Scopes) != 1 || result.Scopes[0] != "user:profile" {
		t.Fatalf("scopes = %#v", result.Scopes)
	}
}
