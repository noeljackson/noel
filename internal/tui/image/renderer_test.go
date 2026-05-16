package image

import (
	"image"
	"image/color"
	"strings"
	"testing"
)

func testImage() image.Image {
	img := image.NewRGBA(image.Rect(0, 0, 2, 2))
	img.Set(0, 0, color.RGBA{R: 255, A: 255})
	return img
}

func TestRendererKittyITermFallback(t *testing.T) {
	img := testImage()
	kitty, rows, err := NewRenderer(Capabilities{Kitty: true}).Render(img, 10, 4)
	if err != nil {
		t.Fatal(err)
	}
	if rows != 4 || !strings.Contains(kitty, "\x1b_G") {
		t.Fatalf("kitty = %q rows=%d", kitty, rows)
	}
	iterm, _, err := NewRenderer(Capabilities{ITerm: true}).Render(img, 10, 4)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(iterm, "\x1b]1337;File=") {
		t.Fatalf("iterm = %q", iterm)
	}
	fallback, _, err := NewRenderer(Capabilities{}).Render(img, 10, 4)
	if err != nil {
		t.Fatal(err)
	}
	if fallback != "[image: 2x2]" {
		t.Fatalf("fallback = %q", fallback)
	}
}
