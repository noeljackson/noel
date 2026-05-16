package auth

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"strings"
	"time"
)

const (
	anthropicClientID     = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
	anthropicCallbackPath = "/callback"
	anthropicScopes       = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"
)

var (
	anthropicAuthorizeURL = "https://claude.ai/oauth/authorize"
	anthropicTokenURL     = "https://platform.claude.com/v1/oauth/token"
	anthropicCallbackHost = "127.0.0.1"
	anthropicCallbackPort = 53692
	httpClient            = http.DefaultClient
)

// LoginResult is an Anthropic OAuth login or refresh result.
type LoginResult struct {
	AccessToken  string
	RefreshToken string
	ExpiresAt    time.Time
	Scopes       []string
	Subscription string
}

// LoginAnthropic runs the Anthropic OAuth authorization-code PKCE flow.
func LoginAnthropic(ctx context.Context, openBrowser func(string) error) (LoginResult, error) {
	pkce, err := generatePKCE()
	if err != nil {
		return LoginResult{}, err
	}
	callback, err := startCallbackServer(ctx, pkce.Verifier)
	if err != nil {
		return LoginResult{}, err
	}
	defer callback.server.Close()

	authURL, err := buildAnthropicAuthorizeURL(callback.redirectURI, pkce)
	if err != nil {
		return LoginResult{}, err
	}
	if openBrowser != nil {
		if err := openBrowser(authURL); err != nil {
			return LoginResult{}, err
		}
	}

	result, err := callback.wait(ctx)
	if err != nil {
		return LoginResult{}, err
	}
	return exchangeAnthropicCode(ctx, result.code, result.state, pkce.Verifier, callback.redirectURI)
}

// RefreshAnthropic uses a refresh token to get a fresh access token.
func RefreshAnthropic(ctx context.Context, refreshToken string) (LoginResult, error) {
	if refreshToken == "" {
		return LoginResult{}, errors.New("anthropic oauth: empty refresh token")
	}
	return postAnthropicToken(ctx, map[string]string{
		"grant_type":    "refresh_token",
		"client_id":     anthropicClientID,
		"refresh_token": refreshToken,
	})
}

// LogoutAnthropic invalidates tokens server-side when an endpoint exists.
func LogoutAnthropic(_ context.Context, _ string) error {
	return nil
}

type pkcePair struct {
	Verifier  string
	Challenge string
}

func generatePKCE() (pkcePair, error) {
	bytes := make([]byte, 32)
	if _, err := rand.Read(bytes); err != nil {
		return pkcePair{}, err
	}
	verifier := base64.RawURLEncoding.EncodeToString(bytes)
	sum := sha256.Sum256([]byte(verifier))
	return pkcePair{
		Verifier:  verifier,
		Challenge: base64.RawURLEncoding.EncodeToString(sum[:]),
	}, nil
}

func buildAnthropicAuthorizeURL(redirectURI string, pkce pkcePair) (string, error) {
	u, err := url.Parse(anthropicAuthorizeURL)
	if err != nil {
		return "", err
	}
	q := u.Query()
	q.Set("code", "true")
	q.Set("client_id", anthropicClientID)
	q.Set("response_type", "code")
	q.Set("redirect_uri", redirectURI)
	q.Set("scope", anthropicScopes)
	q.Set("code_challenge", pkce.Challenge)
	q.Set("code_challenge_method", "S256")
	q.Set("state", pkce.Verifier)
	u.RawQuery = q.Encode()
	return u.String(), nil
}

type callbackResult struct {
	code  string
	state string
}

type callbackServer struct {
	server      *http.Server
	redirectURI string
	resultCh    <-chan callbackServerResult
}

type callbackServerResult struct {
	result callbackResult
	err    error
}

func (s callbackServer) wait(ctx context.Context) (callbackResult, error) {
	select {
	case <-ctx.Done():
		return callbackResult{}, ctx.Err()
	case result := <-s.resultCh:
		if result.err != nil {
			return callbackResult{}, result.err
		}
		return result.result, nil
	}
}

func startCallbackServer(ctx context.Context, expectedState string) (callbackServer, error) {
	resultCh := make(chan callbackServerResult, 1)
	mux := http.NewServeMux()
	server := &http.Server{Handler: mux, BaseContext: func(net.Listener) context.Context { return ctx }}
	mux.HandleFunc(anthropicCallbackPath, func(w http.ResponseWriter, req *http.Request) {
		query := req.URL.Query()
		if oauthErr := query.Get("error"); oauthErr != "" {
			http.Error(w, "Anthropic authentication did not complete.", http.StatusBadRequest)
			resultCh <- callbackServerResult{err: fmt.Errorf("anthropic oauth: %s", oauthErr)}
			return
		}
		code := query.Get("code")
		state := query.Get("state")
		if code == "" || state == "" {
			http.Error(w, "Missing code or state parameter.", http.StatusBadRequest)
			return
		}
		if state != expectedState {
			http.Error(w, "State mismatch.", http.StatusBadRequest)
			return
		}
		w.Header().Set("Content-Type", "text/html; charset=utf-8")
		_, _ = io.WriteString(w, "Anthropic authentication completed. You can close this window.")
		select {
		case resultCh <- callbackServerResult{result: callbackResult{code: code, state: state}}:
		default:
		}
	})

	host := anthropicCallbackHost
	if envHost := os.Getenv("PI_OAUTH_CALLBACK_HOST"); envHost != "" {
		host = envHost
	}
	listener, err := net.Listen("tcp", fmt.Sprintf("%s:%d", host, anthropicCallbackPort))
	if err != nil {
		return callbackServer{}, err
	}
	host, port, err := net.SplitHostPort(listener.Addr().String())
	if err != nil {
		_ = listener.Close()
		return callbackServer{}, err
	}
	if host == "127.0.0.1" {
		host = "localhost"
	}
	redirectURI := fmt.Sprintf("http://%s:%s%s", host, port, anthropicCallbackPath)
	go func() {
		err := server.Serve(listener)
		if err != nil && !errors.Is(err, http.ErrServerClosed) {
			select {
			case resultCh <- callbackServerResult{err: err}:
			default:
			}
		}
	}()
	return callbackServer{server: server, redirectURI: redirectURI, resultCh: resultCh}, nil
}

func exchangeAnthropicCode(ctx context.Context, code, state, verifier, redirectURI string) (LoginResult, error) {
	return postAnthropicToken(ctx, map[string]string{
		"grant_type":    "authorization_code",
		"client_id":     anthropicClientID,
		"code":          code,
		"state":         state,
		"redirect_uri":  redirectURI,
		"code_verifier": verifier,
	})
}

type tokenResponse struct {
	AccessToken      string `json:"access_token"`
	RefreshToken     string `json:"refresh_token"`
	ExpiresIn        int64  `json:"expires_in"`
	Scope            string `json:"scope"`
	SubscriptionType string `json:"subscription_type"`
}

func postAnthropicToken(ctx context.Context, payload map[string]string) (LoginResult, error) {
	body, err := json.Marshal(payload)
	if err != nil {
		return LoginResult{}, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, anthropicTokenURL, bytes.NewReader(body))
	if err != nil {
		return LoginResult{}, err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json")
	resp, err := httpClient.Do(req)
	if err != nil {
		return LoginResult{}, err
	}
	defer resp.Body.Close()
	respBody, err := io.ReadAll(resp.Body)
	if err != nil {
		return LoginResult{}, err
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return LoginResult{}, fmt.Errorf("anthropic oauth: token request failed: status=%d body=%s", resp.StatusCode, string(respBody))
	}
	var token tokenResponse
	if err := json.Unmarshal(respBody, &token); err != nil {
		return LoginResult{}, err
	}
	if token.AccessToken == "" {
		return LoginResult{}, errors.New("anthropic oauth: token response missing access_token")
	}
	return LoginResult{
		AccessToken:  token.AccessToken,
		RefreshToken: token.RefreshToken,
		ExpiresAt:    time.Now().Add(time.Duration(token.ExpiresIn)*time.Second - 5*time.Minute),
		Scopes:       strings.Fields(token.Scope),
		Subscription: token.SubscriptionType,
	}, nil
}
