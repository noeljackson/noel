package resources

import "testing"

func TestParseFrontmatterScalarsArraysAndNestedMaps(t *testing.T) {
	frontmatter, body, err := ParseFrontmatter(`---
name: demo
count: 3
ratio: 1.5
enabled: true
tools: [bash, read]
settings:
  mode: strict
  retries: 2
---

Body text
`)
	if err != nil {
		t.Fatal(err)
	}
	if body != "Body text" {
		t.Fatalf("body = %q", body)
	}
	if frontmatter["name"] != "demo" {
		t.Fatalf("name = %#v", frontmatter["name"])
	}
	if frontmatter["count"] != 3 {
		t.Fatalf("count = %#v", frontmatter["count"])
	}
	if frontmatter["ratio"] != 1.5 {
		t.Fatalf("ratio = %#v", frontmatter["ratio"])
	}
	if frontmatter["enabled"] != true {
		t.Fatalf("enabled = %#v", frontmatter["enabled"])
	}
	tools := stringSlice(frontmatter["tools"])
	if len(tools) != 2 || tools[0] != "bash" || tools[1] != "read" {
		t.Fatalf("tools = %#v", tools)
	}
	settings, ok := frontmatter["settings"].(map[string]any)
	if !ok {
		t.Fatalf("settings = %#v", frontmatter["settings"])
	}
	if settings["mode"] != "strict" || settings["retries"] != 2 {
		t.Fatalf("settings = %#v", settings)
	}
}

func TestParseFrontmatterRejectsMalformedHeader(t *testing.T) {
	_, _, err := ParseFrontmatter(`---
name demo
---
body`)
	if err == nil {
		t.Fatal("expected parse error")
	}
	if got := err.Error(); got != "line 1: expected key: value" {
		t.Fatalf("error = %q", got)
	}
}

func TestParseFrontmatterAllowsEmptyHeader(t *testing.T) {
	frontmatter, body, err := ParseFrontmatter("---\n---\nbody")
	if err != nil {
		t.Fatal(err)
	}
	if len(frontmatter) != 0 || body != "body" {
		t.Fatalf("frontmatter = %#v, body = %q", frontmatter, body)
	}
}
