package components

import (
	"strings"
	"testing"
)

func TestDiffViewColorsUnifiedDiff(t *testing.T) {
	got := DiffView("--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new", 80)
	if !strings.Contains(got, "\x1b[") || !strings.Contains(stripANSI(got), "+new") {
		t.Fatalf("DiffView = %q", got)
	}
}
