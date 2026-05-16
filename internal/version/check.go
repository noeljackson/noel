package version

import (
	"context"
	"encoding/json"
	"net/http"
	"os"
	"strings"
	"time"
)

var BuildVersion = "dev"

type Status struct {
	Current   string
	Latest    string
	Available bool
	CheckedAt time.Time
}

func Check(ctx context.Context, upstream string) (*Status, error) {
	if disabled() {
		return nil, nil
	}
	if upstream == "" {
		upstream = "github.com/noeljackson/pi"
	}
	url := latestReleaseURL(upstream)
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "pi/"+BuildVersion)

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return &Status{Current: BuildVersion, CheckedAt: time.Now().UTC()}, nil
	}

	var payload struct {
		TagName string `json:"tag_name"`
		Name    string `json:"name"`
		Version string `json:"version"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&payload); err != nil {
		return nil, err
	}
	latest := firstNonEmpty(payload.TagName, payload.Version, payload.Name)
	status := &Status{
		Current:   BuildVersion,
		Latest:    latest,
		CheckedAt: time.Now().UTC(),
	}
	status.Available = latest != "" && isNewer(latest, BuildVersion)
	return status, nil
}

func latestReleaseURL(upstream string) string {
	if strings.HasPrefix(upstream, "http://") || strings.HasPrefix(upstream, "https://") {
		return upstream
	}
	upstream = strings.TrimPrefix(upstream, "github.com/")
	upstream = strings.TrimSuffix(upstream, ".git")
	return "https://api.github.com/repos/" + upstream + "/releases/latest"
}

func disabled() bool {
	return truthyEnv("PI_SKIP_VERSION_CHECK") || truthyEnv("PI_OFFLINE")
}

func truthyEnv(name string) bool {
	value := strings.ToLower(strings.TrimSpace(getenv(name)))
	return value == "1" || value == "true" || value == "yes"
}

var getenv = os.Getenv

func firstNonEmpty(values ...string) string {
	for _, value := range values {
		if trimmed := strings.TrimSpace(value); trimmed != "" {
			return trimmed
		}
	}
	return ""
}

func isNewer(candidateVersion string, currentVersion string) bool {
	candidate := parseVersion(candidateVersion)
	current := parseVersion(currentVersion)
	if len(candidate) == 0 || len(current) == 0 {
		return strings.TrimSpace(candidateVersion) != strings.TrimSpace(currentVersion)
	}
	for i := 0; i < 3; i++ {
		if candidate[i] != current[i] {
			return candidate[i] > current[i]
		}
	}
	return false
}

func parseVersion(value string) []int {
	value = strings.TrimPrefix(strings.TrimSpace(value), "v")
	main := strings.SplitN(value, "-", 2)[0]
	parts := strings.Split(main, ".")
	if len(parts) < 3 {
		return nil
	}
	out := make([]int, 3)
	for i := 0; i < 3; i++ {
		for _, ch := range parts[i] {
			if ch < '0' || ch > '9' {
				return nil
			}
			out[i] = out[i]*10 + int(ch-'0')
		}
	}
	return out
}
