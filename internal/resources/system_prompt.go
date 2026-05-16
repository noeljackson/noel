package resources

import "strings"

type SystemPromptBuilder struct {
	BasePrompt string
	Context    []ContextFile
	Skills     []Skill
}

func (b *SystemPromptBuilder) Build() string {
	prompt := b.BasePrompt
	if len(b.Context) > 0 {
		if prompt != "" {
			prompt += "\n\n"
		}
		prompt += "# Project Context\n\n"
		prompt += "Project-specific instructions and guidelines:\n\n"
		for _, file := range b.Context {
			prompt += "## " + file.Path + "\n\n" + file.Content + "\n\n"
		}
	}
	prompt += FormatSkillsForPrompt(b.Skills)
	return prompt
}

func FormatSkillsForPrompt(skills []Skill) string {
	var visible []Skill
	for _, skill := range skills {
		if disabled, ok := skill.Frontmatter["disable-model-invocation"].(bool); ok && disabled {
			continue
		}
		visible = append(visible, skill)
	}
	if len(visible) == 0 {
		return ""
	}

	lines := []string{
		"\n\nThe following skills provide specialized instructions for specific tasks.",
		"Use the read tool to load a skill's file when the task matches its description.",
		"When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.",
		"",
		"<available_skills>",
	}
	for _, skill := range visible {
		lines = append(lines,
			"  <skill>",
			"    <name>"+escapeXML(skill.Name)+"</name>",
			"    <description>"+escapeXML(skill.Description)+"</description>",
			"    <location>"+escapeXML(skill.Path)+"</location>",
			"  </skill>",
		)
	}
	lines = append(lines, "</available_skills>")
	return strings.Join(lines, "\n")
}

func escapeXML(value string) string {
	replacer := strings.NewReplacer(
		"&", "&amp;",
		"<", "&lt;",
		">", "&gt;",
		`"`, "&quot;",
		"'", "&apos;",
	)
	return replacer.Replace(value)
}
