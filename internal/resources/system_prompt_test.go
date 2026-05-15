package resources

import (
	"strings"
	"testing"
)

func TestSystemPromptBuilderIncludesContextAndSkills(t *testing.T) {
	prompt := (&SystemPromptBuilder{
		BasePrompt: "base",
		Context: []ContextFile{
			{Path: "/repo/AGENTS.md", Scope: "project", Content: "project rules"},
			{Path: "/home/user/.pi/agent/CLAUDE.md", Scope: "user", Content: "user rules"},
		},
		Skills: []Skill{
			{Name: "review-code", Path: "/skills/review-code/SKILL.md", Description: "Review <code> & report", Frontmatter: map[string]any{}},
			{Name: "hidden", Path: "/skills/hidden/SKILL.md", Description: "Hidden", Frontmatter: map[string]any{"disable-model-invocation": true}},
		},
	}).Build()

	for _, want := range []string{
		"base\n\n# Project Context\n\nProject-specific instructions and guidelines:",
		"## /repo/AGENTS.md\n\nproject rules",
		"## /home/user/.pi/agent/CLAUDE.md\n\nuser rules",
		"The following skills provide specialized instructions for specific tasks.",
		"<available_skills>",
		"<name>review-code</name>",
		"<description>Review &lt;code&gt; &amp; report</description>",
		"<location>/skills/review-code/SKILL.md</location>",
	} {
		if !strings.Contains(prompt, want) {
			t.Fatalf("prompt missing %q:\n%s", want, prompt)
		}
	}
	if strings.Contains(prompt, "hidden") {
		t.Fatalf("disabled skill was included:\n%s", prompt)
	}
}
