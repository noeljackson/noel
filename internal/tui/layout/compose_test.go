package layout

import (
	"strings"
	"testing"
)

func TestComposeAnchorPositioning(t *testing.T) {
	got := Compose(10, 5, []Box{{Width: Fixed(3), Height: Fixed(1), Anchor: AnchorBottomRight, Content: "abc"}})
	if !strings.Contains(got, "\x1b[5;1H") || !strings.Contains(got, "       abc") {
		t.Fatalf("compose = %q", got)
	}
}

func TestComposePercentWidthAndMargins(t *testing.T) {
	got := Compose(20, 4, []Box{{Width: Percent(50), Anchor: AnchorTopLeft, MarginLeft: 2, Content: "x"}})
	if !strings.Contains(got, "\x1b[1;1H  x") {
		t.Fatalf("compose = %q", got)
	}
}

func TestComposeOverlayZOrder(t *testing.T) {
	got := Compose(5, 1, []Box{
		{Width: Fixed(5), Content: "aaaaa", Anchor: AnchorTopLeft},
		{Width: Fixed(3), Content: "bbb", Anchor: AnchorTopLeft, MarginLeft: 1},
	})
	if !strings.Contains(got, "abbba") {
		t.Fatalf("compose = %q", got)
	}
}
