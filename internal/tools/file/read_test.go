package file

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"image"
	"image/color"
	"image/jpeg"
	"image/png"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

func TestReadOffsetLimit(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "file.txt"), []byte("a\nb\nc\nd\n"), 0o666); err != nil {
		t.Fatal(err)
	}

	result, err := NewReadTool().Execute(context.Background(), json.RawMessage(`{"path":"file.txt","offset":2,"limit":2}`), agent.ToolCallContext{Cwd: dir})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	got := toolText(t, result)
	want := "b\nc\n\n[2 more lines in file. Use offset=4 to continue.]"
	if got != want {
		t.Fatalf("content = %q, want %q", got, want)
	}
}

func TestReadTruncationContinuation(t *testing.T) {
	dir := t.TempDir()
	content := strings.Repeat("x\n", defaultMaxLines+1)
	if err := os.WriteFile(filepath.Join(dir, "large.txt"), []byte(content), 0o666); err != nil {
		t.Fatal(err)
	}

	result, err := NewReadTool().Execute(context.Background(), json.RawMessage(`{"path":"large.txt"}`), agent.ToolCallContext{Cwd: dir})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	if !strings.Contains(toolText(t, result), "Use offset=2001 to continue.") {
		t.Fatalf("content missing continuation hint: %q", toolText(t, result))
	}
}

func TestReadImageReturnsImageContent(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "image.png")
	writePNG(t, path, 10, 8)

	result, err := NewReadTool().Execute(context.Background(), json.RawMessage(`{"path":"image.png"}`), agent.ToolCallContext{Cwd: dir, Model: "claude-sonnet-4-6"})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	imageContent, ok := result.Content[0].(agent.ImageContent)
	if !ok {
		t.Fatalf("content type = %T, want agent.ImageContent", result.Content[0])
	}
	if imageContent.Source.Type != "base64" || imageContent.Source.MediaType != "image/png" || imageContent.Source.Data == "" {
		t.Fatalf("image source = %#v", imageContent.Source)
	}
}

func TestReadImageResize(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "large.png")
	writePNG(t, path, 2200, 100)

	result, err := NewReadTool().Execute(context.Background(), json.RawMessage(`{"path":"large.png"}`), agent.ToolCallContext{Cwd: dir, Model: "claude-sonnet-4-6"})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	imageContent := result.Content[0].(agent.ImageContent)
	data, err := base64.StdEncoding.DecodeString(imageContent.Source.Data)
	if err != nil {
		t.Fatal(err)
	}
	cfg, err := png.DecodeConfig(bytes.NewReader(data))
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Width != maxImageSide {
		t.Fatalf("width = %d, want %d", cfg.Width, maxImageSide)
	}
}

func TestReadJPEGEXIFOrientation(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "oriented.jpg")
	writeOrientedJPEG(t, path, 6)

	result, err := NewReadTool().Execute(context.Background(), json.RawMessage(`{"path":"oriented.jpg"}`), agent.ToolCallContext{Cwd: dir, Model: "claude-sonnet-4-6"})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	imageContent := result.Content[0].(agent.ImageContent)
	data, err := base64.StdEncoding.DecodeString(imageContent.Source.Data)
	if err != nil {
		t.Fatal(err)
	}
	cfg, err := jpeg.DecodeConfig(bytes.NewReader(data))
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Width != 1 || cfg.Height != 2 {
		t.Fatalf("dimensions = %dx%d, want 1x2", cfg.Width, cfg.Height)
	}
}

func TestReadImageNonVisionModelNote(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "image.png")
	writePNG(t, path, 2, 2)

	result, err := NewReadTool().Execute(context.Background(), json.RawMessage(`{"path":"image.png"}`), agent.ToolCallContext{Cwd: dir, Model: "text-only-model"})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	got := toolText(t, result)
	if !strings.Contains(got, "current model doesn't accept images") {
		t.Fatalf("content = %q, want non-vision note", got)
	}
}

func toolText(t *testing.T, result agent.ToolResult) string {
	t.Helper()
	text, ok := result.Content[0].(agent.TextContent)
	if !ok {
		t.Fatalf("content type = %T, want agent.TextContent", result.Content[0])
	}
	return text.Text
}

func writePNG(t *testing.T, path string, width int, height int) {
	t.Helper()
	img := image.NewRGBA(image.Rect(0, 0, width, height))
	for y := 0; y < height; y++ {
		for x := 0; x < width; x++ {
			img.SetRGBA(x, y, color.RGBA{R: uint8(x % 255), G: uint8(y % 255), B: 80, A: 255})
		}
	}
	file, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	if err := png.Encode(file, img); err != nil {
		t.Fatal(err)
	}
}

func writeOrientedJPEG(t *testing.T, path string, orientation uint16) {
	t.Helper()
	img := image.NewRGBA(image.Rect(0, 0, 2, 1))
	img.SetRGBA(0, 0, color.RGBA{R: 255, A: 255})
	img.SetRGBA(1, 0, color.RGBA{B: 255, A: 255})
	var jpegData bytes.Buffer
	if err := jpeg.Encode(&jpegData, img, &jpeg.Options{Quality: 90}); err != nil {
		t.Fatal(err)
	}
	data := jpegData.Bytes()
	exif := buildOrientationEXIF(orientation)
	withEXIF := append([]byte{}, data[:2]...)
	withEXIF = append(withEXIF, exif...)
	withEXIF = append(withEXIF, data[2:]...)
	if err := os.WriteFile(path, withEXIF, 0o666); err != nil {
		t.Fatal(err)
	}
}

func buildOrientationEXIF(orientation uint16) []byte {
	tiff := make([]byte, 8+2+12+4)
	copy(tiff[0:2], []byte("MM"))
	binary.BigEndian.PutUint16(tiff[2:4], 42)
	binary.BigEndian.PutUint32(tiff[4:8], 8)
	binary.BigEndian.PutUint16(tiff[8:10], 1)
	entry := tiff[10:22]
	binary.BigEndian.PutUint16(entry[0:2], 0x0112)
	binary.BigEndian.PutUint16(entry[2:4], 3)
	binary.BigEndian.PutUint32(entry[4:8], 1)
	binary.BigEndian.PutUint16(entry[8:10], orientation)
	payload := append([]byte("Exif\x00\x00"), tiff...)
	segment := []byte{0xff, 0xe1, 0, 0}
	binary.BigEndian.PutUint16(segment[2:4], uint16(len(payload)+2))
	return append(segment, payload...)
}
