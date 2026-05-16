package layout

import (
	"fmt"
	"strconv"
	"strings"
)

type Anchor string

const (
	AnchorCenter       Anchor = "center"
	AnchorTopLeft      Anchor = "top-left"
	AnchorTopRight     Anchor = "top-right"
	AnchorBottomLeft   Anchor = "bottom-left"
	AnchorBottomRight  Anchor = "bottom-right"
	AnchorTopCenter    Anchor = "top-center"
	AnchorBottomCenter Anchor = "bottom-center"
	AnchorLeftCenter   Anchor = "left-center"
	AnchorRightCenter  Anchor = "right-center"
)

type Dimension struct {
	Value   int
	Percent float64
	Auto    bool
}

func Auto() Dimension                 { return Dimension{Auto: true} }
func Fixed(value int) Dimension       { return Dimension{Value: value} }
func Percent(value float64) Dimension { return Dimension{Percent: value} }

type BorderStyle string

const (
	BorderNone   BorderStyle = ""
	BorderSingle BorderStyle = "single"
)

type Box struct {
	Width, Height             Dimension
	Anchor                    Anchor
	MarginTop, MarginLeft     int
	MarginRight, MarginBottom int
	Row, Col                  int
	HasRow, HasCol            bool
	Content                   string
	Border                    BorderStyle
}

func Compose(width, height int, boxes []Box) string {
	if width <= 0 || height <= 0 || len(boxes) == 0 {
		return ""
	}
	canvas := make([][]rune, height)
	for row := range canvas {
		canvas[row] = []rune(strings.Repeat(" ", width))
	}
	for _, box := range boxes {
		lines, boxWidth, boxHeight := renderBox(box, width, height)
		if boxWidth <= 0 || boxHeight <= 0 {
			continue
		}
		row := box.Row
		if !box.HasRow {
			row = ResolveAnchorRow(anchorOrDefault(box.Anchor), boxHeight, height-box.MarginTop-box.MarginBottom, box.MarginTop)
		}
		col := box.Col
		if !box.HasCol {
			col = ResolveAnchorCol(anchorOrDefault(box.Anchor), boxWidth, width-box.MarginLeft-box.MarginRight, box.MarginLeft)
		}
		row = clamp(row, 0, height-1)
		col = clamp(col, 0, width-1)
		for i, line := range lines {
			targetRow := row + i
			if targetRow < 0 || targetRow >= height {
				continue
			}
			runes := []rune(line)
			for j := 0; j < len(runes) && col+j < width; j++ {
				if col+j >= 0 {
					canvas[targetRow][col+j] = runes[j]
				}
			}
		}
	}
	var b strings.Builder
	for row, line := range canvas {
		b.WriteString(fmt.Sprintf("\x1b[%d;%dH", row+1, 1))
		b.WriteString(string(line))
	}
	return b.String()
}

func ResolveAnchorRow(anchor Anchor, boxHeight, availableHeight, marginTop int) int {
	if availableHeight < 1 {
		availableHeight = 1
	}
	switch anchor {
	case AnchorTopLeft, AnchorTopCenter, AnchorTopRight:
		return marginTop
	case AnchorBottomLeft, AnchorBottomCenter, AnchorBottomRight:
		return marginTop + availableHeight - boxHeight
	default:
		return marginTop + (availableHeight-boxHeight)/2
	}
}

func ResolveAnchorCol(anchor Anchor, boxWidth, availableWidth, marginLeft int) int {
	if availableWidth < 1 {
		availableWidth = 1
	}
	switch anchor {
	case AnchorTopLeft, AnchorLeftCenter, AnchorBottomLeft:
		return marginLeft
	case AnchorTopRight, AnchorRightCenter, AnchorBottomRight:
		return marginLeft + availableWidth - boxWidth
	default:
		return marginLeft + (availableWidth-boxWidth)/2
	}
}

func ParseDimension(value string) Dimension {
	value = strings.TrimSpace(value)
	if value == "" || value == "auto" {
		return Auto()
	}
	if strings.HasSuffix(value, "%") {
		number, err := strconv.ParseFloat(strings.TrimSuffix(value, "%"), 64)
		if err == nil {
			return Percent(number)
		}
	}
	number, err := strconv.Atoi(value)
	if err == nil {
		return Fixed(number)
	}
	return Auto()
}

func renderBox(box Box, screenWidth, screenHeight int) ([]string, int, int) {
	contentLines := strings.Split(strings.TrimRight(box.Content, "\n"), "\n")
	if len(contentLines) == 1 && contentLines[0] == "" {
		contentLines = []string{}
	}
	contentWidth := 0
	for _, line := range contentLines {
		contentWidth = max(contentWidth, runeWidth(line))
	}
	boxWidth := resolveDimension(box.Width, screenWidth, contentWidth)
	boxHeight := resolveDimension(box.Height, screenHeight, len(contentLines))
	if box.Border != BorderNone {
		boxWidth = max(boxWidth, contentWidth+2)
		boxHeight = max(boxHeight, len(contentLines)+2)
	} else {
		boxWidth = max(boxWidth, contentWidth)
		boxHeight = max(boxHeight, len(contentLines))
	}
	boxWidth = max(1, min(boxWidth, screenWidth))
	boxHeight = max(1, min(boxHeight, screenHeight))

	var lines []string
	if box.Border == BorderNone {
		for i := 0; i < boxHeight; i++ {
			line := ""
			if i < len(contentLines) {
				line = contentLines[i]
			}
			lines = append(lines, padRight(line, boxWidth))
		}
		return lines, boxWidth, boxHeight
	}
	innerWidth := max(0, boxWidth-2)
	lines = append(lines, "┌"+strings.Repeat("─", innerWidth)+"┐")
	for i := 0; i < boxHeight-2; i++ {
		line := ""
		if i < len(contentLines) {
			line = contentLines[i]
		}
		lines = append(lines, "│"+padRight(line, innerWidth)+"│")
	}
	lines = append(lines, "└"+strings.Repeat("─", innerWidth)+"┘")
	return lines, boxWidth, boxHeight
}

func resolveDimension(dim Dimension, ref, autoValue int) int {
	if dim.Auto || (dim.Value == 0 && dim.Percent == 0) {
		return autoValue
	}
	if dim.Percent > 0 {
		return int(float64(ref) * dim.Percent / 100)
	}
	return dim.Value
}

func anchorOrDefault(anchor Anchor) Anchor {
	if anchor == "" {
		return AnchorCenter
	}
	return anchor
}

func padRight(text string, width int) string {
	runes := []rune(text)
	if len(runes) > width {
		return string(runes[:width])
	}
	return text + strings.Repeat(" ", width-len(runes))
}

func runeWidth(text string) int { return len([]rune(text)) }
func clamp(value, low, high int) int {
	if value < low {
		return low
	}
	if value > high {
		return high
	}
	return value
}
func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}
func max(a, b int) int {
	if a > b {
		return a
	}
	return b
}
