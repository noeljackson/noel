package components

import (
	"strings"
	"testing"
)

func TestSelectorViewHighlightsSelected(t *testing.T) {
	got := SelectorView(SelectorOpts{
		Items:    []SelectorItem{{Label: "one"}, {Label: "two", Description: "second"}},
		Selected: 1,
		Width:    40,
	})
	plain := stripANSI(got)
	if !strings.Contains(plain, "> two") {
		t.Fatalf("selector = %q", plain)
	}
}
