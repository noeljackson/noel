package components

import (
	"strings"
	"testing"
)

func TestMarkdownView(t *testing.T) {
	input := strings.Join([]string{
		"# Title",
		"",
		"- item",
		"",
		"```go",
		"fmt.Println(1)",
		"```",
		"",
		"| A | B |",
		"|---|---|",
		"| 1 | [link](https://example.com) |",
	}, "\n")
	got := stripANSI(MarkdownView(input, 80))
	for _, want := range []string{"Title", "- item", "```go", "fmt.Println(1)", "+", "link (https://example.com)"} {
		if !strings.Contains(got, want) {
			t.Fatalf("MarkdownView missing %q in:\n%s", want, got)
		}
	}
}

func BenchmarkMarkdownView4KB(b *testing.B) {
	input := markdownBenchmarkInput(4096)
	for i := 0; i < b.N; i++ {
		_ = MarkdownView(input, 100)
	}
}
