package autocomplete

import (
	"context"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/noeljackson/pi/internal/resources"
)

type Kind int

const (
	KindSlashCommand Kind = iota
	KindFile
	KindSkill
	KindPrompt
)

type Range struct {
	Start int
	End   int
}

type Suggestion struct {
	Label  string
	Insert string
	Detail string
	Kind   Kind
	Range  Range
}

type Provider interface {
	Suggestions(ctx context.Context, input string, cursor int) []Suggestion
}

type CombinedProvider struct {
	Providers []Provider
}

func NewCombinedProvider(providers ...Provider) *CombinedProvider {
	return &CombinedProvider{Providers: providers}
}

func (p *CombinedProvider) Suggestions(ctx context.Context, input string, cursor int) []Suggestion {
	if p == nil {
		return nil
	}
	var out []Suggestion
	for _, provider := range p.Providers {
		if provider == nil {
			continue
		}
		out = append(out, provider.Suggestions(ctx, input, cursor)...)
	}
	return out
}

type SlashCommand struct {
	Name        string
	Description string
	Args        []ArgSpec
}

type ArgSpec struct {
	Name        string
	Description string
}

type SlashProvider struct {
	Commands []SlashCommand
}

func NewSlashProvider(commands []SlashCommand) *SlashProvider {
	return &SlashProvider{Commands: commands}
}

func (p *SlashProvider) Suggestions(_ context.Context, input string, cursor int) []Suggestion {
	if cursor < 0 || cursor > len(input) {
		cursor = len(input)
	}
	start := lineStart(input, cursor)
	before := input[start:cursor]
	trimmed := strings.TrimLeft(before, " \t")
	if !strings.HasPrefix(trimmed, "/") || strings.Contains(trimmed, " ") || strings.Contains(trimmed, "\t") {
		return nil
	}
	prefix := strings.TrimPrefix(trimmed, "/")
	rangeStart := cursor - len(trimmed)
	items := FuzzyFilter(p.Commands, prefix, func(cmd SlashCommand) string { return cmd.Name })
	out := make([]Suggestion, 0, len(items))
	for _, cmd := range items {
		out = append(out, Suggestion{
			Label:  cmd.Name,
			Insert: "/" + cmd.Name + " ",
			Detail: cmd.Description,
			Kind:   KindSlashCommand,
			Range:  Range{Start: rangeStart, End: cursor},
		})
	}
	return out
}

type FileProvider struct {
	BaseDir string
	Limit   int
}

func NewFileProvider(baseDir string) *FileProvider {
	if baseDir == "" {
		baseDir, _ = os.Getwd()
	}
	return &FileProvider{BaseDir: baseDir, Limit: 50}
}

func (p *FileProvider) Suggestions(_ context.Context, input string, cursor int) []Suggestion {
	if cursor < 0 || cursor > len(input) {
		cursor = len(input)
	}
	prefix, start, ok := extractPathPrefix(input[:cursor], false)
	if !ok {
		return nil
	}
	return p.fileSuggestions(prefix, Range{Start: start, End: cursor})
}

func (p *FileProvider) ForceSuggestions(input string, cursor int) []Suggestion {
	if cursor < 0 || cursor > len(input) {
		cursor = len(input)
	}
	prefix, start, ok := extractPathPrefix(input[:cursor], true)
	if !ok {
		return nil
	}
	return p.fileSuggestions(prefix, Range{Start: start, End: cursor})
}

func (p *FileProvider) fileSuggestions(prefix string, replace Range) []Suggestion {
	rawPrefix, isAt, isQuoted := parsePathPrefix(prefix)
	expanded := expandHome(rawPrefix)
	searchDir, searchPrefix, displayBase := splitSearchPath(p.BaseDir, rawPrefix, expanded)
	entries, err := os.ReadDir(searchDir)
	if err != nil {
		return nil
	}
	var suggestions []Suggestion
	for _, entry := range entries {
		if strings.HasPrefix(entry.Name(), ".") && !strings.HasPrefix(searchPrefix, ".") {
			continue
		}
		if !strings.HasPrefix(strings.ToLower(entry.Name()), strings.ToLower(searchPrefix)) {
			continue
		}
		isDir := entry.IsDir()
		if !isDir && entry.Type()&os.ModeSymlink != 0 {
			if info, err := os.Stat(filepath.Join(searchDir, entry.Name())); err == nil {
				isDir = info.IsDir()
			}
		}
		display := filepath.ToSlash(filepath.Join(displayBase, entry.Name()))
		if displayBase == "" || displayBase == "." {
			display = entry.Name()
		}
		if strings.HasPrefix(rawPrefix, "./") && !strings.HasPrefix(display, "./") {
			display = "./" + display
		}
		if isDir {
			display += "/"
		}
		insert := buildCompletionValue(display, isAt, isQuoted)
		label := entry.Name()
		if isDir {
			label += "/"
		}
		if isAt && !isDir {
			insert += " "
		}
		suggestions = append(suggestions, Suggestion{
			Label:  label,
			Insert: insert,
			Kind:   KindFile,
			Detail: display,
			Range:  replace,
		})
	}
	sort.SliceStable(suggestions, func(i, j int) bool {
		iDir := strings.HasSuffix(suggestions[i].Label, "/")
		jDir := strings.HasSuffix(suggestions[j].Label, "/")
		if iDir != jDir {
			return iDir
		}
		return suggestions[i].Label < suggestions[j].Label
	})
	limit := p.Limit
	if limit <= 0 {
		limit = 50
	}
	if len(suggestions) > limit {
		suggestions = suggestions[:limit]
	}
	return suggestions
}

type SkillProvider struct {
	Skills []resources.Skill
}

func (p SkillProvider) Suggestions(_ context.Context, input string, cursor int) []Suggestion {
	token, start, ok := extractSymbolPrefix(input, cursor, "#")
	if !ok {
		return nil
	}
	items := FuzzyFilter(p.Skills, token, func(skill resources.Skill) string { return skill.Name })
	out := make([]Suggestion, 0, len(items))
	for _, skill := range items {
		out = append(out, Suggestion{
			Label:  skill.Name,
			Insert: "#" + skill.Name,
			Detail: skill.Description,
			Kind:   KindSkill,
			Range:  Range{Start: start, End: cursor},
		})
	}
	return out
}

type PromptTemplateProvider struct {
	Templates []resources.PromptTemplate
}

func (p PromptTemplateProvider) Suggestions(_ context.Context, input string, cursor int) []Suggestion {
	if cursor < 0 || cursor > len(input) {
		cursor = len(input)
	}
	start := lineStart(input, cursor)
	before := input[start:cursor]
	if strings.ContainsAny(before, " \t") || !strings.HasPrefix(before, "/") {
		return nil
	}
	prefix := strings.TrimPrefix(before, "/")
	items := FuzzyFilter(p.Templates, prefix, func(t resources.PromptTemplate) string { return t.Name })
	out := make([]Suggestion, 0, len(items))
	for _, template := range items {
		out = append(out, Suggestion{
			Label:  template.Name,
			Insert: "/" + template.Name + " ",
			Detail: template.Description,
			Kind:   KindPrompt,
			Range:  Range{Start: start, End: cursor},
		})
	}
	return out
}

func Apply(input string, suggestion Suggestion) (string, int) {
	next := input[:suggestion.Range.Start] + suggestion.Insert + input[suggestion.Range.End:]
	return next, suggestion.Range.Start + len(suggestion.Insert)
}

func extractSymbolPrefix(input string, cursor int, symbol string) (string, int, bool) {
	if cursor < 0 || cursor > len(input) {
		cursor = len(input)
	}
	last := strings.LastIndexAny(input[:cursor], " \t\n\"'=")
	start := last + 1
	if !strings.HasPrefix(input[start:cursor], symbol) {
		return "", 0, false
	}
	return strings.TrimPrefix(input[start:cursor], symbol), start, true
}

func extractPathPrefix(before string, force bool) (string, int, bool) {
	if quoted, start, ok := extractQuotedPrefix(before); ok {
		return quoted, start, true
	}
	last := strings.LastIndexAny(before, " \t\"'=")
	start := last + 1
	prefix := before[start:]
	if strings.HasPrefix(prefix, "@") {
		return prefix, start, true
	}
	if force {
		return prefix, start, true
	}
	if strings.Contains(prefix, "/") || strings.HasPrefix(prefix, ".") || strings.HasPrefix(prefix, "~/") {
		return prefix, start, true
	}
	if prefix == "" && strings.HasSuffix(before, " ") {
		return prefix, start, true
	}
	return "", 0, false
}

func extractQuotedPrefix(before string) (string, int, bool) {
	inQuote := false
	start := -1
	for i, r := range before {
		if r == '"' {
			inQuote = !inQuote
			if inQuote {
				start = i
			}
		}
	}
	if !inQuote || start < 0 {
		return "", 0, false
	}
	if start > 0 && before[start-1] == '@' {
		return before[start-1:], start - 1, true
	}
	return before[start:], start, true
}

func parsePathPrefix(prefix string) (raw string, isAt bool, isQuoted bool) {
	switch {
	case strings.HasPrefix(prefix, `@"`):
		return strings.TrimPrefix(prefix, `@"`), true, true
	case strings.HasPrefix(prefix, `"`):
		return strings.TrimPrefix(prefix, `"`), false, true
	case strings.HasPrefix(prefix, "@"):
		return strings.TrimPrefix(prefix, "@"), true, false
	default:
		return prefix, false, false
	}
}

func splitSearchPath(baseDir string, raw string, expanded string) (searchDir string, searchPrefix string, displayBase string) {
	if baseDir == "" {
		baseDir, _ = os.Getwd()
	}
	switch {
	case raw == "", raw == "./", raw == "../", raw == "~", raw == "~/", raw == "/":
		searchDir = expanded
		if searchDir == "" || raw == "./" || raw == "../" {
			searchDir = filepath.Join(baseDir, expanded)
		}
		displayBase = raw
	case strings.HasSuffix(raw, "/"):
		if filepath.IsAbs(expanded) || strings.HasPrefix(raw, "~") {
			searchDir = expanded
		} else {
			searchDir = filepath.Join(baseDir, expanded)
		}
		displayBase = raw
	default:
		dir := filepath.Dir(expanded)
		searchPrefix = filepath.Base(expanded)
		displayBase = filepath.Dir(raw)
		if displayBase == "." {
			displayBase = ""
		}
		if filepath.IsAbs(expanded) || strings.HasPrefix(raw, "~") {
			searchDir = dir
		} else {
			searchDir = filepath.Join(baseDir, dir)
		}
	}
	return searchDir, searchPrefix, displayBase
}

func expandHome(path string) string {
	if path == "~" || strings.HasPrefix(path, "~/") {
		home, _ := os.UserHomeDir()
		if path == "~" {
			return home
		}
		return filepath.Join(home, path[2:])
	}
	return path
}

func buildCompletionValue(path string, isAt bool, isQuoted bool) string {
	prefix := ""
	if isAt {
		prefix = "@"
	}
	if !isQuoted && !strings.Contains(path, " ") {
		return prefix + path
	}
	return prefix + `"` + path + `"`
}

func lineStart(input string, cursor int) int {
	if cursor > len(input) {
		cursor = len(input)
	}
	if idx := strings.LastIndex(input[:cursor], "\n"); idx >= 0 {
		return idx + 1
	}
	return 0
}
