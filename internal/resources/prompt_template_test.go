package resources

import (
	"strings"
	"testing"
)

func TestPromptTemplateRenderRequiredArgsAndDefaults(t *testing.T) {
	template := PromptTemplate{
		Args: []TemplateArg{
			{Name: "topic", Required: true},
			{Name: "tone", Default: "direct"},
		},
		Body: "Write about {{topic}} in ${tone} tone. First: $1. All: $ARGUMENTS.",
	}
	rendered, err := template.Render(RenderArgs{"topic": "resources"})
	if err != nil {
		t.Fatal(err)
	}
	want := "Write about resources in direct tone. First: resources. All: resources direct."
	if rendered != want {
		t.Fatalf("rendered = %q, want %q", rendered, want)
	}
}

func TestPromptTemplateRenderMissingRequiredArg(t *testing.T) {
	template := PromptTemplate{
		Args: []TemplateArg{{Name: "topic", Required: true}},
		Body: "{{topic}}",
	}
	_, err := template.Render(RenderArgs{})
	if err == nil {
		t.Fatal("expected missing argument error")
	}
	if !strings.Contains(err.Error(), "missing required argument: topic") {
		t.Fatalf("error = %q", err)
	}
}

func TestPromptTemplateRenderBashStyleSlicing(t *testing.T) {
	template := PromptTemplate{
		Args: []TemplateArg{
			{Name: "one"},
			{Name: "two"},
			{Name: "three"},
		},
		Body: "$1|$2|$@|${@:2}|${@:2:1}",
	}
	rendered, err := template.Render(RenderArgs{"one": "a", "two": "b", "three": "c"})
	if err != nil {
		t.Fatal(err)
	}
	if rendered != "a|b|a b c|b c|b" {
		t.Fatalf("rendered = %q", rendered)
	}
}
