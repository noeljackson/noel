package agent

import (
	"context"
	"errors"

	authstore "github.com/noeljackson/pi/internal/auth"
)

// Login runs a provider login flow and persists credentials in the agent's auth store.
func (a *Agent) Login(provider string, openBrowser func(string) error) error {
	return a.LoginContext(context.Background(), provider, openBrowser)
}

// LoginContext runs a provider login flow and persists credentials in the agent's auth store.
func (a *Agent) LoginContext(ctx context.Context, provider string, openBrowser func(string) error) error {
	if provider != "anthropic" {
		return errors.New("unsupported login provider")
	}
	if a.cfg.AuthStore == nil {
		return errors.New("auth store is not configured")
	}
	result, err := authstore.LoginAnthropic(ctx, openBrowser)
	if err != nil {
		return err
	}
	metadata := map[string]any{}
	if len(result.Scopes) > 0 {
		metadata["scopes"] = result.Scopes
	}
	if result.Subscription != "" {
		metadata["subscription"] = result.Subscription
	}
	return a.cfg.AuthStore.Set("anthropic-oauth", authstore.ProviderAuth{
		Type:         "oauth",
		AccessToken:  result.AccessToken,
		RefreshToken: result.RefreshToken,
		ExpiresAt:    result.ExpiresAt,
		Metadata:     metadata,
	})
}
