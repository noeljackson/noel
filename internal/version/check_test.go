package version

import (
	"context"
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestCheckParsesLatestRelease(t *testing.T) {
	previousVersion := BuildVersion
	previousGetenv := getenv
	BuildVersion = "1.2.3"
	getenv = func(string) string { return "" }
	t.Cleanup(func() {
		BuildVersion = previousVersion
		getenv = previousGetenv
	})

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"tag_name":"v1.2.4"}`))
	}))
	defer server.Close()

	status, err := Check(context.Background(), server.URL)
	if err != nil {
		t.Fatal(err)
	}
	if status == nil || status.Latest != "v1.2.4" || !status.Available {
		t.Fatalf("status = %#v", status)
	}
}

func TestCheckDisabled(t *testing.T) {
	previousGetenv := getenv
	getenv = func(name string) string {
		if name == "PI_SKIP_VERSION_CHECK" {
			return "1"
		}
		return ""
	}
	t.Cleanup(func() { getenv = previousGetenv })

	status, err := Check(context.Background(), "http://example.invalid")
	if err != nil {
		t.Fatal(err)
	}
	if status != nil {
		t.Fatalf("status = %#v, want nil", status)
	}
}
