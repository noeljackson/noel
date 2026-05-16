package render

import (
	"strings"
	"testing"
)

func TestRendererEmptySurfaceOutputsNothing(t *testing.T) {
	var out strings.Builder
	renderer := NewRenderer(&out)
	if err := renderer.Render(Surface{}); err != nil {
		t.Fatal(err)
	}
	if out.String() != "" {
		t.Fatalf("output = %q", out.String())
	}
}

func TestRendererIdenticalSurfaceOutputsNoDiff(t *testing.T) {
	var out strings.Builder
	renderer := NewRenderer(&out)
	surface := Surface{Width: 1, Height: 1, Cells: [][]Cell{{{Rune: 'a', Width: 1}}}}
	if err := renderer.Render(surface); err != nil {
		t.Fatal(err)
	}
	out.Reset()
	if err := renderer.Render(surface); err != nil {
		t.Fatal(err)
	}
	if out.String() != "" {
		t.Fatalf("output = %q", out.String())
	}
}

func TestRendererOneCellChangeUsesCursorMove(t *testing.T) {
	var out strings.Builder
	renderer := NewRenderer(&out)
	first := Surface{Width: 2, Height: 1, Cells: [][]Cell{{{Rune: 'a', Width: 1}, {Rune: 'b', Width: 1}}}}
	second := Surface{Width: 2, Height: 1, Cells: [][]Cell{{{Rune: 'a', Width: 1}, {Rune: 'c', Width: 1}}}}
	if err := renderer.Render(first); err != nil {
		t.Fatal(err)
	}
	out.Reset()
	if err := renderer.Render(second); err != nil {
		t.Fatal(err)
	}
	got := out.String()
	if !strings.Contains(got, "\x1b[1;1H") || !strings.Contains(got, "ac") {
		t.Fatalf("diff output = %q", got)
	}
}
