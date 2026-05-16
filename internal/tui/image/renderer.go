package image

import (
	"bytes"
	"encoding/base64"
	"fmt"
	stdimage "image"
	"image/jpeg"
	"image/png"
	"strings"
	"sync/atomic"

	"github.com/noeljackson/pi/internal/tui/capabilities"
)

type Renderer interface {
	Render(img stdimage.Image, width, height int) (string, int, error)
	Erase(id string) error
}

type Capabilities struct {
	Kitty      bool
	ITerm      bool
	Sixel      bool
	Truecolor  bool
	Hyperlinks bool
	Tmux       bool
	Screen     bool
}

type renderer struct {
	caps Capabilities
}

var imageID uint32

func DetectCapabilities() Capabilities {
	caps := capabilities.Detect()
	return Capabilities{
		Kitty:      caps.Kitty,
		ITerm:      caps.ITerm,
		Sixel:      caps.Sixel,
		Truecolor:  caps.Truecolor,
		Hyperlinks: caps.Hyperlinks,
		Tmux:       caps.Tmux,
		Screen:     caps.Screen,
	}
}

func NewRenderer(caps Capabilities) Renderer {
	return renderer{caps: caps}
}

func (r renderer) Render(img stdimage.Image, width, height int) (string, int, error) {
	if img == nil {
		return "[image: unknown]", 1, nil
	}
	if width <= 0 {
		width = 80
	}
	if height <= 0 {
		height = rowsFor(img.Bounds().Dx(), img.Bounds().Dy(), width)
	}
	if r.caps.Kitty {
		data, err := encodePNG(img)
		if err != nil {
			return "", 0, err
		}
		id := atomic.AddUint32(&imageID, 1)
		seq := encodeKitty(base64.StdEncoding.EncodeToString(data), id, width, height)
		return capabilities.WrapPassthrough(seq, capabilities.Caps{Tmux: r.caps.Tmux, Screen: r.caps.Screen}), height, nil
	}
	if r.caps.ITerm {
		data, err := encodeImage(img)
		if err != nil {
			return "", 0, err
		}
		seq := encodeITerm(base64.StdEncoding.EncodeToString(data), width, height)
		return capabilities.WrapPassthrough(seq, capabilities.Caps{Tmux: r.caps.Tmux, Screen: r.caps.Screen}), height, nil
	}
	bounds := img.Bounds()
	return fmt.Sprintf("[image: %dx%d]", bounds.Dx(), bounds.Dy()), 1, nil
}

func (r renderer) Erase(id string) error {
	_ = r
	_ = id
	return nil
}

func encodePNG(img stdimage.Image) ([]byte, error) {
	var buf bytes.Buffer
	if err := png.Encode(&buf, img); err != nil {
		return nil, err
	}
	return buf.Bytes(), nil
}

func encodeImage(img stdimage.Image) ([]byte, error) {
	var buf bytes.Buffer
	if err := jpeg.Encode(&buf, img, &jpeg.Options{Quality: 90}); err == nil {
		return buf.Bytes(), nil
	}
	return encodePNG(img)
}

func encodeKitty(data string, id uint32, cols, rows int) string {
	params := fmt.Sprintf("a=T,f=100,q=2,C=1,c=%d,r=%d,i=%d", cols, rows, id)
	const chunkSize = 4096
	if len(data) <= chunkSize {
		return "\x1b_G" + params + ";" + data + "\x1b\\"
	}
	var b strings.Builder
	for offset := 0; offset < len(data); offset += chunkSize {
		end := offset + chunkSize
		if end > len(data) {
			end = len(data)
		}
		chunk := data[offset:end]
		switch {
		case offset == 0:
			b.WriteString("\x1b_G" + params + ",m=1;" + chunk + "\x1b\\")
		case end == len(data):
			b.WriteString("\x1b_Gm=0;" + chunk + "\x1b\\")
		default:
			b.WriteString("\x1b_Gm=1;" + chunk + "\x1b\\")
		}
	}
	return b.String()
}

func encodeITerm(data string, width, height int) string {
	return fmt.Sprintf("\x1b]1337;File=inline=1;width=%d;height=%d;preserveAspectRatio=1:%s\x07", width, height, data)
}

func rowsFor(pixelWidth, pixelHeight, cols int) int {
	if pixelWidth <= 0 || pixelHeight <= 0 || cols <= 0 {
		return 1
	}
	return max(1, (pixelHeight*cols+pixelWidth-1)/pixelWidth)
}

func max(a, b int) int {
	if a > b {
		return a
	}
	return b
}
