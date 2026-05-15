package resources

type Resources struct {
	ContextFiles    []ContextFile
	Skills          []Skill
	PromptTemplates []PromptTemplate
	Themes          []Theme
	Diagnostics     []Diagnostic
}

type ContextFile struct {
	Path    string
	Scope   string
	Content string
}

type Skill struct {
	Name        string
	Path        string
	Description string
	Tools       []string
	Frontmatter map[string]any
	Body        string
}

type PromptTemplate struct {
	Name        string
	Path        string
	Args        []TemplateArg
	Body        string
	Description string
}

type TemplateArg struct {
	Name     string
	Required bool
	Default  string
}

type Theme struct {
	Name   string
	Path   string
	Colors map[string]string
}

type Diagnostic struct {
	Level   string
	Source  string
	Message string
}
