package auth

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"sync"
	"time"
)

// Store persists provider credentials in auth.json.
type Store struct {
	path string
	mu   sync.Mutex
}

// New returns a Store backed by path.
func New(path string) *Store {
	return &Store{path: path}
}

// ProviderAuth is one provider's persisted credential.
type ProviderAuth struct {
	Type         string
	Key          string
	AccessToken  string
	RefreshToken string
	ExpiresAt    time.Time
	Metadata     map[string]any
}

type storeFile struct {
	Providers map[string]ProviderAuth `json:"providers"`
}

// Get returns auth for provider.
func (s *Store) Get(provider string) (ProviderAuth, bool, error) {
	providers, err := s.List()
	if err != nil {
		return ProviderAuth{}, false, err
	}
	auth, ok := providers[provider]
	return auth, ok, nil
}

// Set stores auth for provider.
func (s *Store) Set(provider string, auth ProviderAuth) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	providers, err := s.readLocked()
	if err != nil {
		return err
	}
	providers[provider] = auth
	return s.writeLocked(providers)
}

// Delete removes auth for provider.
func (s *Store) Delete(provider string) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	providers, err := s.readLocked()
	if err != nil {
		return err
	}
	delete(providers, provider)
	return s.writeLocked(providers)
}

// List returns all stored provider credentials.
func (s *Store) List() (map[string]ProviderAuth, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.readLocked()
}

func (s *Store) readLocked() (map[string]ProviderAuth, error) {
	data, err := os.ReadFile(s.path)
	if errors.Is(err, os.ErrNotExist) {
		return map[string]ProviderAuth{}, nil
	}
	if err != nil {
		return nil, err
	}
	if len(data) == 0 {
		return map[string]ProviderAuth{}, nil
	}

	var wrapped storeFile
	if err := json.Unmarshal(data, &wrapped); err == nil && wrapped.Providers != nil {
		return cloneProviders(wrapped.Providers), nil
	}

	var topLevel map[string]ProviderAuth
	if err := json.Unmarshal(data, &topLevel); err != nil {
		return nil, err
	}
	if topLevel == nil {
		topLevel = map[string]ProviderAuth{}
	}
	return cloneProviders(topLevel), nil
}

func (s *Store) writeLocked(providers map[string]ProviderAuth) error {
	data, err := json.MarshalIndent(storeFile{Providers: providers}, "", "  ")
	if err != nil {
		return err
	}
	data = append(data, '\n')
	return writeFileAtomic(s.path, data, 0o600)
}

func cloneProviders(input map[string]ProviderAuth) map[string]ProviderAuth {
	out := make(map[string]ProviderAuth, len(input))
	for key, value := range input {
		if value.Metadata != nil {
			metadata := make(map[string]any, len(value.Metadata))
			for mk, mv := range value.Metadata {
				metadata[mk] = mv
			}
			value.Metadata = metadata
		}
		out[key] = value
	}
	return out
}

type providerAuthJSON struct {
	Type         string         `json:"type"`
	Key          string         `json:"key,omitempty"`
	AccessToken  string         `json:"accessToken,omitempty"`
	Access       string         `json:"access,omitempty"`
	RefreshToken string         `json:"refreshToken,omitempty"`
	Refresh      string         `json:"refresh,omitempty"`
	ExpiresAt    any            `json:"expiresAt,omitempty"`
	Expires      any            `json:"expires,omitempty"`
	Metadata     map[string]any `json:"metadata,omitempty"`
}

// UnmarshalJSON accepts both pi's auth.json field names and the TS OAuth names.
func (a *ProviderAuth) UnmarshalJSON(data []byte) error {
	var raw providerAuthJSON
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	a.Type = raw.Type
	a.Key = raw.Key
	a.AccessToken = raw.AccessToken
	if a.AccessToken == "" {
		a.AccessToken = raw.Access
	}
	a.RefreshToken = raw.RefreshToken
	if a.RefreshToken == "" {
		a.RefreshToken = raw.Refresh
	}
	a.Metadata = raw.Metadata
	exp, err := parseExpiry(raw.ExpiresAt)
	if err != nil {
		return err
	}
	if exp.IsZero() {
		exp, err = parseExpiry(raw.Expires)
		if err != nil {
			return err
		}
	}
	a.ExpiresAt = exp
	return nil
}

// MarshalJSON writes the requested auth.json field names.
func (a ProviderAuth) MarshalJSON() ([]byte, error) {
	raw := providerAuthJSON{
		Type:         a.Type,
		Key:          a.Key,
		AccessToken:  a.AccessToken,
		RefreshToken: a.RefreshToken,
		Metadata:     a.Metadata,
	}
	if !a.ExpiresAt.IsZero() {
		raw.ExpiresAt = a.ExpiresAt.UnixMilli()
	}
	return json.Marshal(raw)
}

func parseExpiry(value any) (time.Time, error) {
	switch v := value.(type) {
	case nil:
		return time.Time{}, nil
	case float64:
		return expiryFromNumber(int64(v)), nil
	case string:
		if v == "" {
			return time.Time{}, nil
		}
		t, err := time.Parse(time.RFC3339, v)
		if err != nil {
			return time.Time{}, err
		}
		return t, nil
	default:
		return time.Time{}, nil
	}
}

func expiryFromNumber(value int64) time.Time {
	if value <= 0 {
		return time.Time{}
	}
	if value > 1_000_000_000_000 {
		return time.UnixMilli(value)
	}
	return time.Unix(value, 0)
}

func writeFileAtomic(path string, data []byte, mode os.FileMode) error {
	dir := filepath.Dir(path)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return err
	}
	tmp, err := os.CreateTemp(dir, "."+filepath.Base(path)+".*.tmp")
	if err != nil {
		return err
	}
	tmpName := tmp.Name()
	removeTmp := true
	defer func() {
		if removeTmp {
			_ = os.Remove(tmpName)
		}
	}()
	if err := tmp.Chmod(mode); err != nil {
		_ = tmp.Close()
		return err
	}
	if _, err := tmp.Write(data); err != nil {
		_ = tmp.Close()
		return err
	}
	if err := tmp.Sync(); err != nil {
		_ = tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	if err := os.Rename(tmpName, path); err != nil {
		return err
	}
	removeTmp = false
	if err := os.Chmod(path, mode); err != nil {
		return err
	}
	return syncDir(dir)
}

func syncDir(dir string) error {
	f, err := os.Open(dir)
	if err != nil {
		return err
	}
	defer f.Close()
	return f.Sync()
}
