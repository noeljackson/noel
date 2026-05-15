package resources

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/noeljackson/pi/internal/config"
)

func TestResourceLoaderDiscoversContextFilesInNestedProject(t *testing.T) {
	root := t.TempDir()
	agentDir := filepath.Join(root, "home", ".pi", "agent")
	project := filepath.Join(root, "repo")
	nested := filepath.Join(project, "a", "b")
	mkdirAll(t, agentDir, nested)
	writeFile(t, filepath.Join(agentDir, "AGENTS.md"), "user")
	writeFile(t, filepath.Join(project, "AGENTS.md"), "project")
	writeFile(t, filepath.Join(project, "a", "CLAUDE.md"), "nested")

	loaded, err := (&ResourceLoader{
		Paths:       config.Paths{AgentDir: agentDir, SessionDir: filepath.Join(agentDir, "sessions")},
		ProjectRoot: nested,
		UserHome:    filepath.Join(root, "home"),
	}).Load()
	if err != nil {
		t.Fatal(err)
	}
	if len(loaded.ContextFiles) != 3 {
		t.Fatalf("context count = %d, want 3: %#v", len(loaded.ContextFiles), loaded.ContextFiles)
	}
	got := []string{
		loaded.ContextFiles[0].Content,
		loaded.ContextFiles[1].Content,
		loaded.ContextFiles[2].Content,
	}
	want := []string{"user", "project", "nested"}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("context order = %#v, want %#v", got, want)
		}
	}
}

func TestResourceLoaderDiscoversResourcesAndSkillCollisions(t *testing.T) {
	root := t.TempDir()
	agentDir := filepath.Join(root, "home", ".pi", "agent")
	project := filepath.Join(root, "repo")
	mkdirAll(t,
		filepath.Join(agentDir, "skills", "shared"),
		filepath.Join(project, ".pi", "skills", "shared"),
		filepath.Join(project, ".agents", "prompts"),
		filepath.Join(agentDir, "themes"),
	)
	writeFile(t, filepath.Join(agentDir, "skills", "shared", "SKILL.md"), `---
name: shared
description: user skill
tools: [bash, read]
---
body`)
	writeFile(t, filepath.Join(project, ".pi", "skills", "shared", "SKILL.md"), `---
name: shared
description: project skill
---
body`)
	writeFile(t, filepath.Join(project, ".agents", "prompts", "fix.md"), `---
description: Fix prompt
args:
  - name: topic
    required: true
---
Fix $1`)
	writeFile(t, filepath.Join(agentDir, "themes", "custom.json"), `{
  "name": "custom",
  "vars": {"primary": "#010203"},
  "colors": {"accent": "primary"}
}`)

	loaded, err := (&ResourceLoader{
		Paths:       config.Paths{AgentDir: agentDir, SessionDir: filepath.Join(agentDir, "sessions")},
		ProjectRoot: project,
		UserHome:    filepath.Join(root, "home"),
	}).Load()
	if err != nil {
		t.Fatal(err)
	}
	if len(loaded.Skills) != 1 {
		t.Fatalf("skills = %#v", loaded.Skills)
	}
	if loaded.Skills[0].Description != "user skill" {
		t.Fatalf("winning skill = %#v", loaded.Skills[0])
	}
	if len(loaded.PromptTemplates) != 1 || loaded.PromptTemplates[0].Name != "fix" {
		t.Fatalf("prompts = %#v", loaded.PromptTemplates)
	}
	if len(loaded.Themes) != 3 {
		t.Fatalf("themes = %#v", loaded.Themes)
	}
	foundCollision := false
	for _, diagnostic := range loaded.Diagnostics {
		if strings.Contains(diagnostic.Message, `name "shared" collision`) {
			foundCollision = true
		}
	}
	if !foundCollision {
		t.Fatalf("missing collision diagnostic: %#v", loaded.Diagnostics)
	}
}

func mkdirAll(t *testing.T, paths ...string) {
	t.Helper()
	for _, path := range paths {
		if err := os.MkdirAll(path, 0o755); err != nil {
			t.Fatal(err)
		}
	}
}

func writeFile(t *testing.T, path string, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}
