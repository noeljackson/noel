package resources

import (
	"embed"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"

	"github.com/noeljackson/pi/internal/config"
)

//go:embed themes/*.json
var builtinThemeFiles embed.FS

type ResourceLoader struct {
	Paths       config.Paths
	ProjectRoot string
	UserHome    string
}

func (l *ResourceLoader) Load() (Resources, error) {
	loader, err := l.withDefaults()
	if err != nil {
		return Resources{}, err
	}

	var out Resources
	out.ContextFiles = loader.loadContextFiles(&out.Diagnostics)
	out.Skills = loader.loadSkills(&out.Diagnostics)
	out.PromptTemplates = loader.loadPromptTemplates(&out.Diagnostics)
	out.Themes = loader.loadThemes(&out.Diagnostics)
	return out, nil
}

func (l ResourceLoader) withDefaults() (ResourceLoader, error) {
	if l.Paths.AgentDir == "" || l.Paths.SessionDir == "" {
		paths, err := config.DefaultPaths()
		if err != nil {
			return ResourceLoader{}, err
		}
		if l.Paths.AgentDir == "" {
			l.Paths.AgentDir = paths.AgentDir
		}
		if l.Paths.SessionDir == "" {
			l.Paths.SessionDir = paths.SessionDir
		}
	}
	if l.ProjectRoot == "" {
		cwd, err := os.Getwd()
		if err != nil {
			return ResourceLoader{}, err
		}
		l.ProjectRoot = cwd
	}
	if l.UserHome == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return ResourceLoader{}, err
		}
		l.UserHome = home
	}
	if abs, err := filepath.Abs(l.ProjectRoot); err == nil {
		l.ProjectRoot = abs
	}
	if abs, err := filepath.Abs(l.Paths.AgentDir); err == nil {
		l.Paths.AgentDir = abs
	}
	return l, nil
}

func (l ResourceLoader) loadContextFiles(diagnostics *[]Diagnostic) []ContextFile {
	var files []ContextFile
	seen := map[string]struct{}{}
	if file, ok := loadContextFileFromDir(l.Paths.AgentDir, "user", diagnostics); ok {
		files = append(files, file)
		seen[file.Path] = struct{}{}
	}

	var projectFiles []ContextFile
	for dir := filepath.Clean(l.ProjectRoot); ; dir = filepath.Dir(dir) {
		file, ok := loadContextFileFromDir(dir, "project", diagnostics)
		if ok {
			if _, exists := seen[file.Path]; !exists {
				projectFiles = append([]ContextFile{file}, projectFiles...)
				seen[file.Path] = struct{}{}
			}
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			break
		}
	}
	files = append(files, projectFiles...)
	return files
}

func loadContextFileFromDir(dir string, scope string, diagnostics *[]Diagnostic) (ContextFile, bool) {
	for _, name := range []string{"AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"} {
		path := filepath.Join(dir, name)
		info, err := os.Stat(path)
		if err != nil || info.IsDir() {
			continue
		}
		content, err := os.ReadFile(path)
		if err != nil {
			*diagnostics = append(*diagnostics, Diagnostic{Level: "warning", Source: path, Message: err.Error()})
			return ContextFile{}, false
		}
		abs, _ := filepath.Abs(path)
		return ContextFile{Path: abs, Scope: scope, Content: string(content)}, true
	}
	return ContextFile{}, false
}

func (l ResourceLoader) loadSkills(diagnostics *[]Diagnostic) []Skill {
	roots := []resourceRoot{
		{path: filepath.Join(l.Paths.AgentDir, "skills"), scope: "user"},
		{path: filepath.Join(l.ProjectRoot, config.ConfigDirName, "skills"), scope: "project"},
		{path: filepath.Join(l.ProjectRoot, ".agents", "skills"), scope: "project"},
	}
	seenNames := map[string]Skill{}
	seenPaths := map[string]struct{}{}
	var skills []Skill
	for _, root := range roots {
		for _, skill := range l.loadSkillsFromDir(root, diagnostics) {
			canonical := canonicalPath(skill.Path)
			if _, ok := seenPaths[canonical]; ok {
				continue
			}
			if existing, ok := seenNames[skill.Name]; ok {
				*diagnostics = append(*diagnostics, Diagnostic{
					Level:   "warning",
					Source:  skill.Path,
					Message: fmt.Sprintf(`name "%s" collision: %s wins over %s`, skill.Name, existing.Path, skill.Path),
				})
				continue
			}
			seenPaths[canonical] = struct{}{}
			seenNames[skill.Name] = skill
			skills = append(skills, skill)
		}
	}
	return skills
}

type resourceRoot struct {
	path  string
	scope string
}

func (l ResourceLoader) loadSkillsFromDir(root resourceRoot, diagnostics *[]Diagnostic) []Skill {
	if !isDir(root.path) {
		return nil
	}
	var skills []Skill
	ignoreRules := loadIgnoreRules(root.path, root.path)
	var walk func(string, bool)
	walk = func(dir string, includeRootFiles bool) {
		ignoreRules = append(ignoreRules, loadIgnoreRules(dir, root.path)...)
		entries, err := os.ReadDir(dir)
		if err != nil {
			*diagnostics = append(*diagnostics, Diagnostic{Level: "warning", Source: dir, Message: err.Error()})
			return
		}
		sort.Slice(entries, func(i, j int) bool { return entries[i].Name() < entries[j].Name() })
		for _, entry := range entries {
			if entry.Name() != "SKILL.md" {
				continue
			}
			path := filepath.Join(dir, entry.Name())
			if ignored(path, false, root.path, ignoreRules) {
				*diagnostics = append(*diagnostics, Diagnostic{Level: "info", Source: path, Message: "skill ignored by ignore rules"})
				continue
			}
			if skill, ok := loadSkillFile(path, diagnostics); ok {
				skills = append(skills, skill)
			}
			return
		}
		for _, entry := range entries {
			name := entry.Name()
			if strings.HasPrefix(name, ".") || name == "node_modules" {
				continue
			}
			path := filepath.Join(dir, name)
			info, err := entry.Info()
			if err != nil {
				continue
			}
			isDirectory := info.IsDir()
			isFile := info.Mode().IsRegular()
			if info.Mode()&os.ModeSymlink != 0 {
				target, statErr := os.Stat(path)
				if statErr != nil {
					continue
				}
				isDirectory = target.IsDir()
				isFile = target.Mode().IsRegular()
			}
			if ignored(path, isDirectory, root.path, ignoreRules) {
				if isFile && strings.HasSuffix(name, ".md") {
					*diagnostics = append(*diagnostics, Diagnostic{Level: "info", Source: path, Message: "skill ignored by ignore rules"})
				}
				continue
			}
			if isDirectory {
				walk(path, false)
				continue
			}
			if includeRootFiles && isFile && strings.HasSuffix(name, ".md") {
				if skill, ok := loadSkillFile(path, diagnostics); ok {
					skills = append(skills, skill)
				}
			}
		}
	}
	walk(root.path, true)
	return skills
}

func loadSkillFile(path string, diagnostics *[]Diagnostic) (Skill, bool) {
	content, err := os.ReadFile(path)
	if err != nil {
		*diagnostics = append(*diagnostics, Diagnostic{Level: "warning", Source: path, Message: err.Error()})
		return Skill{}, false
	}
	frontmatter, body, err := ParseFrontmatter(string(content))
	if err != nil {
		*diagnostics = append(*diagnostics, Diagnostic{Level: "warning", Source: path, Message: err.Error()})
		return Skill{}, false
	}
	parent := filepath.Base(filepath.Dir(path))
	name := stringValue(frontmatter["name"])
	if name == "" {
		name = parent
	}
	description := stringValue(frontmatter["description"])
	for _, message := range validateSkill(name, parent, description) {
		*diagnostics = append(*diagnostics, Diagnostic{Level: "warning", Source: path, Message: message})
	}
	if strings.TrimSpace(description) == "" {
		return Skill{}, false
	}
	abs, _ := filepath.Abs(path)
	return Skill{
		Name:        name,
		Path:        abs,
		Description: description,
		Tools:       stringSlice(frontmatter["tools"]),
		Frontmatter: frontmatter,
		Body:        body,
	}, true
}

func validateSkill(name string, parent string, description string) []string {
	var errors []string
	if name != parent && filepath.Base(parent) != "skills" {
		errors = append(errors, fmt.Sprintf(`name "%s" does not match parent directory "%s"`, name, parent))
	}
	if len(name) > 64 {
		errors = append(errors, fmt.Sprintf("name exceeds 64 characters (%d)", len(name)))
	}
	validName := regexp.MustCompile(`^[a-z0-9-]+$`)
	if !validName.MatchString(name) {
		errors = append(errors, "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)")
	}
	if strings.HasPrefix(name, "-") || strings.HasSuffix(name, "-") {
		errors = append(errors, "name must not start or end with a hyphen")
	}
	if strings.Contains(name, "--") {
		errors = append(errors, "name must not contain consecutive hyphens")
	}
	if strings.TrimSpace(description) == "" {
		errors = append(errors, "description is required")
	} else if len(description) > 1024 {
		errors = append(errors, fmt.Sprintf("description exceeds 1024 characters (%d)", len(description)))
	}
	return errors
}

func (l ResourceLoader) loadPromptTemplates(diagnostics *[]Diagnostic) []PromptTemplate {
	roots := []resourceRoot{
		{path: filepath.Join(l.Paths.AgentDir, "prompts"), scope: "user"},
		{path: filepath.Join(l.ProjectRoot, config.ConfigDirName, "prompts"), scope: "project"},
		{path: filepath.Join(l.ProjectRoot, ".agents", "prompts"), scope: "project"},
	}
	seen := map[string]PromptTemplate{}
	var prompts []PromptTemplate
	for _, root := range roots {
		if !isDir(root.path) {
			continue
		}
		entries, err := os.ReadDir(root.path)
		if err != nil {
			*diagnostics = append(*diagnostics, Diagnostic{Level: "warning", Source: root.path, Message: err.Error()})
			continue
		}
		sort.Slice(entries, func(i, j int) bool { return entries[i].Name() < entries[j].Name() })
		for _, entry := range entries {
			if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".md") {
				continue
			}
			path := filepath.Join(root.path, entry.Name())
			prompt, ok := loadPromptTemplateFile(path, diagnostics)
			if !ok {
				continue
			}
			if existing, exists := seen[prompt.Name]; exists {
				*diagnostics = append(*diagnostics, Diagnostic{
					Level:   "warning",
					Source:  prompt.Path,
					Message: fmt.Sprintf(`name "/%s" collision: %s wins over %s`, prompt.Name, existing.Path, prompt.Path),
				})
				continue
			}
			seen[prompt.Name] = prompt
			prompts = append(prompts, prompt)
		}
	}
	return prompts
}

func loadPromptTemplateFile(path string, diagnostics *[]Diagnostic) (PromptTemplate, bool) {
	content, err := os.ReadFile(path)
	if err != nil {
		*diagnostics = append(*diagnostics, Diagnostic{Level: "warning", Source: path, Message: err.Error()})
		return PromptTemplate{}, false
	}
	frontmatter, body, err := ParseFrontmatter(string(content))
	if err != nil {
		*diagnostics = append(*diagnostics, Diagnostic{Level: "warning", Source: path, Message: err.Error()})
		return PromptTemplate{}, false
	}
	abs, _ := filepath.Abs(path)
	name := strings.TrimSuffix(filepath.Base(path), ".md")
	return PromptTemplate{
		Name:        name,
		Path:        abs,
		Args:        parseTemplateArgs(frontmatter["args"]),
		Body:        body,
		Description: promptDescription(frontmatter, body),
	}, true
}

func promptDescription(frontmatter map[string]any, body string) string {
	if description := stringValue(frontmatter["description"]); description != "" {
		return description
	}
	for _, line := range strings.Split(body, "\n") {
		if strings.TrimSpace(line) == "" {
			continue
		}
		if len(line) > 60 {
			return line[:60] + "..."
		}
		return line
	}
	return ""
}

func (l ResourceLoader) loadThemes(diagnostics *[]Diagnostic) []Theme {
	var themes []Theme
	loadBuiltin := func(path string) {
		content, err := builtinThemeFiles.ReadFile(path)
		if err != nil {
			*diagnostics = append(*diagnostics, Diagnostic{Level: "warning", Source: path, Message: err.Error()})
			return
		}
		if theme, ok := parseTheme(path, content, diagnostics); ok {
			theme.Path = "<builtin:" + strings.TrimSuffix(filepath.Base(path), ".json") + ">"
			themes = append(themes, theme)
		}
	}
	loadBuiltin("themes/dark.json")
	loadBuiltin("themes/light.json")

	userDir := filepath.Join(l.Paths.AgentDir, "themes")
	entries, err := os.ReadDir(userDir)
	if err == nil {
		sort.Slice(entries, func(i, j int) bool { return entries[i].Name() < entries[j].Name() })
		for _, entry := range entries {
			if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".json") {
				continue
			}
			path := filepath.Join(userDir, entry.Name())
			content, readErr := os.ReadFile(path)
			if readErr != nil {
				*diagnostics = append(*diagnostics, Diagnostic{Level: "warning", Source: path, Message: readErr.Error()})
				continue
			}
			if theme, ok := parseTheme(path, content, diagnostics); ok {
				themes = append(themes, theme)
			}
		}
	} else if !os.IsNotExist(err) {
		*diagnostics = append(*diagnostics, Diagnostic{Level: "warning", Source: userDir, Message: err.Error()})
	}

	seen := map[string]Theme{}
	var deduped []Theme
	for _, theme := range themes {
		if existing, ok := seen[theme.Name]; ok {
			*diagnostics = append(*diagnostics, Diagnostic{
				Level:   "warning",
				Source:  theme.Path,
				Message: fmt.Sprintf(`name "%s" collision: %s wins over %s`, theme.Name, existing.Path, theme.Path),
			})
			continue
		}
		seen[theme.Name] = theme
		deduped = append(deduped, theme)
	}
	return deduped
}

func parseTheme(path string, content []byte, diagnostics *[]Diagnostic) (Theme, bool) {
	var raw struct {
		Name   string         `json:"name"`
		Vars   map[string]any `json:"vars"`
		Colors map[string]any `json:"colors"`
	}
	if err := json.Unmarshal(content, &raw); err != nil {
		*diagnostics = append(*diagnostics, Diagnostic{Level: "warning", Source: path, Message: err.Error()})
		return Theme{}, false
	}
	if raw.Name == "" || raw.Colors == nil {
		*diagnostics = append(*diagnostics, Diagnostic{Level: "warning", Source: path, Message: "theme requires name and colors"})
		return Theme{}, false
	}
	colors := map[string]string{}
	for key, value := range raw.Colors {
		resolved, err := resolveThemeValue(value, raw.Vars, map[string]struct{}{})
		if err != nil {
			*diagnostics = append(*diagnostics, Diagnostic{Level: "warning", Source: path, Message: err.Error()})
			return Theme{}, false
		}
		colors[key] = resolved
	}
	abs, _ := filepath.Abs(path)
	return Theme{Name: raw.Name, Path: abs, Colors: colors}, true
}

func resolveThemeValue(value any, vars map[string]any, seen map[string]struct{}) (string, error) {
	switch typed := value.(type) {
	case string:
		if typed == "" || strings.HasPrefix(typed, "#") {
			return typed, nil
		}
		if _, ok := seen[typed]; ok {
			return "", fmt.Errorf("circular variable reference detected: %s", typed)
		}
		next, ok := vars[typed]
		if !ok {
			return "", fmt.Errorf("variable reference not found: %s", typed)
		}
		seen[typed] = struct{}{}
		return resolveThemeValue(next, vars, seen)
	case float64:
		return fmt.Sprintf("%.0f", typed), nil
	default:
		return "", fmt.Errorf("invalid color value %v", value)
	}
}

func loadIgnoreRules(dir string, root string) []ignoreRule {
	var rules []ignoreRule
	for _, name := range []string{".gitignore", ".ignore", ".fdignore"} {
		content, err := os.ReadFile(filepath.Join(dir, name))
		if err != nil {
			continue
		}
		relDir, _ := filepath.Rel(root, dir)
		prefix := filepath.ToSlash(relDir)
		if prefix == "." {
			prefix = ""
		} else if prefix != "" {
			prefix += "/"
		}
		for _, line := range strings.Split(normalizeNewlines(string(content)), "\n") {
			line = strings.TrimSpace(line)
			if line == "" || strings.HasPrefix(line, "#") || strings.HasPrefix(line, "!") {
				continue
			}
			line = strings.TrimPrefix(line, "/")
			rules = append(rules, ignoreRule{pattern: prefix + line})
		}
	}
	return rules
}

type ignoreRule struct {
	pattern string
}

func ignored(path string, isDir bool, root string, rules []ignoreRule) bool {
	rel, err := filepath.Rel(root, path)
	if err != nil {
		return false
	}
	rel = filepath.ToSlash(rel)
	if isDir {
		rel += "/"
	}
	for _, rule := range rules {
		pattern := rule.pattern
		if strings.HasSuffix(pattern, "/") {
			if strings.HasPrefix(rel, pattern) {
				return true
			}
			continue
		}
		if ok, _ := filepath.Match(pattern, rel); ok {
			return true
		}
		if ok, _ := filepath.Match(pattern, filepath.Base(rel)); ok {
			return true
		}
		if rel == pattern || strings.HasPrefix(rel, pattern+"/") {
			return true
		}
	}
	return false
}

func isDir(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.IsDir()
}

func canonicalPath(path string) string {
	real, err := filepath.EvalSymlinks(path)
	if err == nil {
		path = real
	}
	abs, err := filepath.Abs(path)
	if err != nil {
		return filepath.Clean(path)
	}
	return filepath.Clean(abs)
}

func stringValue(value any) string {
	if s, ok := value.(string); ok {
		return s
	}
	return ""
}

func stringSlice(value any) []string {
	items, ok := value.([]any)
	if !ok {
		return nil
	}
	out := make([]string, 0, len(items))
	for _, item := range items {
		if s, ok := item.(string); ok {
			out = append(out, s)
		}
	}
	return out
}
