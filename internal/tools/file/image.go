package file

import (
	"bytes"
	"encoding/base64"
	"encoding/binary"
	"image"
	"image/color"
	"image/gif"
	"image/jpeg"
	"image/png"
	"math"
	"net/http"
	"path/filepath"
	"strings"
)

const maxImageSide = 2048

func detectImageMime(path string, data []byte) string {
	sniff := data
	if len(sniff) > 512 {
		sniff = sniff[:512]
	}
	mime := http.DetectContentType(sniff)
	switch mime {
	case "image/jpeg", "image/png", "image/gif", "image/webp":
		return mime
	}
	switch strings.ToLower(filepath.Ext(path)) {
	case ".jpg", ".jpeg":
		if len(data) >= 3 && data[0] == 0xff && data[1] == 0xd8 && data[2] == 0xff {
			return "image/jpeg"
		}
	case ".png":
		if len(data) >= 8 && bytes.Equal(data[:8], []byte{0x89, 'P', 'N', 'G', '\r', '\n', 0x1a, '\n'}) {
			return "image/png"
		}
	case ".gif":
		if len(data) >= 3 && string(data[:3]) == "GIF" {
			return "image/gif"
		}
	case ".webp":
		if len(data) >= 12 && string(data[:4]) == "RIFF" && string(data[8:12]) == "WEBP" {
			return "image/webp"
		}
	}
	return ""
}

func imageContent(data []byte, mime string) agentImagePayload {
	return agentImagePayload{
		MediaType: mime,
		Data:      base64.StdEncoding.EncodeToString(data),
	}
}

type agentImagePayload struct {
	MediaType string
	Data      string
}

func resizeImageIfNeeded(data []byte, mime string) ([]byte, string, bool) {
	var img image.Image
	var err error
	switch mime {
	case "image/jpeg":
		img, err = jpeg.Decode(bytes.NewReader(data))
		if err != nil {
			return data, mime, false
		}
		img = applyOrientation(img, jpegOrientation(data))
	case "image/png":
		img, err = png.Decode(bytes.NewReader(data))
		if err != nil {
			return data, mime, false
		}
	default:
		return data, mime, false
	}

	bounds := img.Bounds()
	width := bounds.Dx()
	height := bounds.Dy()
	if width <= maxImageSide && height <= maxImageSide {
		if mime == "image/jpeg" && jpegOrientation(data) != 1 {
			encoded, ok := encodeImage(img, mime)
			if ok {
				return encoded, mime, true
			}
		}
		return data, mime, false
	}

	scale := float64(maxImageSide) / float64(width)
	if height > width {
		scale = float64(maxImageSide) / float64(height)
	}
	targetWidth := int(math.Round(float64(width) * scale))
	targetHeight := int(math.Round(float64(height) * scale))
	if targetWidth < 1 {
		targetWidth = 1
	}
	if targetHeight < 1 {
		targetHeight = 1
	}
	resized := bilinearResize(img, targetWidth, targetHeight)
	encoded, ok := encodeImage(resized, mime)
	if !ok {
		return data, mime, false
	}
	return encoded, mime, true
}

func encodeImage(img image.Image, mime string) ([]byte, bool) {
	var out bytes.Buffer
	switch mime {
	case "image/jpeg":
		if err := jpeg.Encode(&out, img, &jpeg.Options{Quality: 80}); err != nil {
			return nil, false
		}
	case "image/png":
		if err := png.Encode(&out, img); err != nil {
			return nil, false
		}
	default:
		return nil, false
	}
	return out.Bytes(), true
}

func bilinearResize(src image.Image, width int, height int) *image.RGBA {
	dst := image.NewRGBA(image.Rect(0, 0, width, height))
	sb := src.Bounds()
	srcW := sb.Dx()
	srcH := sb.Dy()
	if width == 1 || height == 1 || srcW == 1 || srcH == 1 {
		for y := 0; y < height; y++ {
			sy := sb.Min.Y + int(float64(y)*float64(srcH)/float64(height))
			for x := 0; x < width; x++ {
				sx := sb.Min.X + int(float64(x)*float64(srcW)/float64(width))
				dst.Set(x, y, src.At(sx, sy))
			}
		}
		return dst
	}
	for y := 0; y < height; y++ {
		fy := float64(y) * float64(srcH-1) / float64(height-1)
		y0 := int(math.Floor(fy))
		y1 := minInt(y0+1, srcH-1)
		wy := fy - float64(y0)
		for x := 0; x < width; x++ {
			fx := float64(x) * float64(srcW-1) / float64(width-1)
			x0 := int(math.Floor(fx))
			x1 := minInt(x0+1, srcW-1)
			wx := fx - float64(x0)
			c00 := color.RGBAModel.Convert(src.At(sb.Min.X+x0, sb.Min.Y+y0)).(color.RGBA)
			c10 := color.RGBAModel.Convert(src.At(sb.Min.X+x1, sb.Min.Y+y0)).(color.RGBA)
			c01 := color.RGBAModel.Convert(src.At(sb.Min.X+x0, sb.Min.Y+y1)).(color.RGBA)
			c11 := color.RGBAModel.Convert(src.At(sb.Min.X+x1, sb.Min.Y+y1)).(color.RGBA)
			dst.SetRGBA(x, y, blendRGBA(c00, c10, c01, c11, wx, wy))
		}
	}
	return dst
}

func blendRGBA(c00 color.RGBA, c10 color.RGBA, c01 color.RGBA, c11 color.RGBA, wx float64, wy float64) color.RGBA {
	return color.RGBA{
		R: blendChannel(c00.R, c10.R, c01.R, c11.R, wx, wy),
		G: blendChannel(c00.G, c10.G, c01.G, c11.G, wx, wy),
		B: blendChannel(c00.B, c10.B, c01.B, c11.B, wx, wy),
		A: blendChannel(c00.A, c10.A, c01.A, c11.A, wx, wy),
	}
}

func blendChannel(v00 byte, v10 byte, v01 byte, v11 byte, wx float64, wy float64) byte {
	top := float64(v00)*(1-wx) + float64(v10)*wx
	bottom := float64(v01)*(1-wx) + float64(v11)*wx
	value := top*(1-wy) + bottom*wy
	if value < 0 {
		return 0
	}
	if value > 255 {
		return 255
	}
	return byte(math.Round(value))
}

func applyOrientation(img image.Image, orientation int) image.Image {
	switch orientation {
	case 2:
		return flipHorizontal(img)
	case 3:
		return flipVertical(flipHorizontal(img))
	case 4:
		return flipVertical(img)
	case 5:
		return flipHorizontal(rotate90CW(img))
	case 6:
		return rotate90CW(img)
	case 7:
		return flipHorizontal(rotate90CCW(img))
	case 8:
		return rotate90CCW(img)
	default:
		return img
	}
}

func rotate90CW(src image.Image) *image.RGBA {
	sb := src.Bounds()
	dst := image.NewRGBA(image.Rect(0, 0, sb.Dy(), sb.Dx()))
	for y := 0; y < sb.Dy(); y++ {
		for x := 0; x < sb.Dx(); x++ {
			dst.Set(sb.Dy()-1-y, x, src.At(sb.Min.X+x, sb.Min.Y+y))
		}
	}
	return dst
}

func rotate90CCW(src image.Image) *image.RGBA {
	sb := src.Bounds()
	dst := image.NewRGBA(image.Rect(0, 0, sb.Dy(), sb.Dx()))
	for y := 0; y < sb.Dy(); y++ {
		for x := 0; x < sb.Dx(); x++ {
			dst.Set(y, sb.Dx()-1-x, src.At(sb.Min.X+x, sb.Min.Y+y))
		}
	}
	return dst
}

func flipHorizontal(src image.Image) *image.RGBA {
	sb := src.Bounds()
	dst := image.NewRGBA(image.Rect(0, 0, sb.Dx(), sb.Dy()))
	for y := 0; y < sb.Dy(); y++ {
		for x := 0; x < sb.Dx(); x++ {
			dst.Set(sb.Dx()-1-x, y, src.At(sb.Min.X+x, sb.Min.Y+y))
		}
	}
	return dst
}

func flipVertical(src image.Image) *image.RGBA {
	sb := src.Bounds()
	dst := image.NewRGBA(image.Rect(0, 0, sb.Dx(), sb.Dy()))
	for y := 0; y < sb.Dy(); y++ {
		for x := 0; x < sb.Dx(); x++ {
			dst.Set(x, sb.Dy()-1-y, src.At(sb.Min.X+x, sb.Min.Y+y))
		}
	}
	return dst
}

func jpegOrientation(data []byte) int {
	if len(data) < 4 || data[0] != 0xff || data[1] != 0xd8 {
		return 1
	}
	offset := 2
	for offset+4 <= len(data) {
		if data[offset] != 0xff {
			return 1
		}
		marker := data[offset+1]
		if marker == 0xff {
			offset++
			continue
		}
		if marker == 0xd9 || marker == 0xda {
			return 1
		}
		length := int(binary.BigEndian.Uint16(data[offset+2 : offset+4]))
		if length < 2 || offset+2+length > len(data) {
			return 1
		}
		segment := data[offset+4 : offset+2+length]
		if marker == 0xe1 && len(segment) >= 6 && bytes.Equal(segment[:6], []byte{'E', 'x', 'i', 'f', 0, 0}) {
			return tiffOrientation(segment[6:])
		}
		offset += 2 + length
	}
	return 1
}

func tiffOrientation(data []byte) int {
	if len(data) < 8 {
		return 1
	}
	var order binary.ByteOrder
	switch string(data[:2]) {
	case "II":
		order = binary.LittleEndian
	case "MM":
		order = binary.BigEndian
	default:
		return 1
	}
	if order.Uint16(data[2:4]) != 42 {
		return 1
	}
	ifdOffset := int(order.Uint32(data[4:8]))
	if ifdOffset < 0 || ifdOffset+2 > len(data) {
		return 1
	}
	count := int(order.Uint16(data[ifdOffset : ifdOffset+2]))
	pos := ifdOffset + 2
	for i := 0; i < count; i++ {
		if pos+12 > len(data) {
			return 1
		}
		tag := order.Uint16(data[pos : pos+2])
		if tag == 0x0112 {
			value := int(order.Uint16(data[pos+8 : pos+10]))
			if value >= 1 && value <= 8 {
				return value
			}
			return 1
		}
		pos += 12
	}
	return 1
}

func imageDimensions(data []byte, mime string) (int, int, bool) {
	var cfg image.Config
	var err error
	switch mime {
	case "image/jpeg":
		cfg, err = jpeg.DecodeConfig(bytes.NewReader(data))
		if err == nil {
			orientation := jpegOrientation(data)
			if orientation >= 5 && orientation <= 8 {
				return cfg.Height, cfg.Width, true
			}
		}
	case "image/png":
		cfg, err = png.DecodeConfig(bytes.NewReader(data))
	case "image/gif":
		cfg, err = gif.DecodeConfig(bytes.NewReader(data))
	default:
		return 0, 0, false
	}
	if err != nil {
		return 0, 0, false
	}
	return cfg.Width, cfg.Height, true
}
