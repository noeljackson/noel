package render

import (
	"fmt"
	"io"
	"strings"
)

type Surface struct {
	Cells         [][]Cell
	Width, Height int
}

type Cell struct {
	Rune  rune
	Width int
	Style Style
}

type Style struct {
	Foreground Color
	Background Color
	Bold       bool
	Italic     bool
	Underline  bool
	Hyperlink  string
}

type Color struct {
	R, G, B uint8
	Set     bool
}

type Renderer struct {
	out      io.Writer
	previous Surface
	hasPrev  bool
}

func NewRenderer(out io.Writer) *Renderer {
	return &Renderer{out: out}
}

func (r *Renderer) Render(surface Surface) error {
	if r == nil || r.out == nil {
		return nil
	}
	surface = normalize(surface)
	if surface.Width == 0 || surface.Height == 0 {
		if !r.hasPrev {
			return nil
		}
		r.previous = surface
		r.hasPrev = true
		return nil
	}

	if !r.hasPrev || r.previous.Width != surface.Width || r.previous.Height != surface.Height {
		if err := r.writeFull(surface); err != nil {
			return err
		}
		r.previous = clone(surface)
		r.hasPrev = true
		return nil
	}

	var b strings.Builder
	b.WriteString("\x1b[?2026h")
	for row := 0; row < surface.Height; row++ {
		if rowEqual(r.previous.Cells[row], surface.Cells[row]) {
			continue
		}
		b.WriteString(fmt.Sprintf("\x1b[%d;%dH\x1b[2K", row+1, 1))
		b.WriteString(renderRow(surface.Cells[row]))
	}
	b.WriteString("\x1b[?2026l")
	if b.Len() == len("\x1b[?2026h\x1b[?2026l") {
		return nil
	}
	if _, err := io.WriteString(r.out, b.String()); err != nil {
		return err
	}
	r.previous = clone(surface)
	r.hasPrev = true
	return nil
}

func (r *Renderer) Clear() error {
	if r == nil || r.out == nil {
		return nil
	}
	r.previous = Surface{}
	r.hasPrev = false
	_, err := io.WriteString(r.out, "\x1b[2J\x1b[H\x1b[3J")
	return err
}

func (r *Renderer) Resize(width, height int) {
	if width < 0 {
		width = 0
	}
	if height < 0 {
		height = 0
	}
	r.previous = Surface{Width: width, Height: height, Cells: make([][]Cell, height)}
	for row := range r.previous.Cells {
		r.previous.Cells[row] = blankRow(width)
	}
	r.hasPrev = true
}

func (r *Renderer) writeFull(surface Surface) error {
	var b strings.Builder
	b.WriteString("\x1b[?2026h\x1b[H")
	for row := 0; row < surface.Height; row++ {
		if row > 0 {
			b.WriteString("\r\n")
		}
		b.WriteString("\x1b[2K")
		b.WriteString(renderRow(surface.Cells[row]))
	}
	b.WriteString("\x1b[?2026l")
	_, err := io.WriteString(r.out, b.String())
	return err
}

func normalize(surface Surface) Surface {
	if surface.Width < 0 {
		surface.Width = 0
	}
	if surface.Height < 0 {
		surface.Height = 0
	}
	if surface.Height == 0 && len(surface.Cells) > 0 {
		surface.Height = len(surface.Cells)
	}
	if surface.Width == 0 {
		for _, row := range surface.Cells {
			if len(row) > surface.Width {
				surface.Width = len(row)
			}
		}
	}
	cells := make([][]Cell, surface.Height)
	for row := 0; row < surface.Height; row++ {
		cells[row] = blankRow(surface.Width)
		if row >= len(surface.Cells) {
			continue
		}
		copy(cells[row], surface.Cells[row])
		for col := range cells[row] {
			if cells[row][col].Width == 0 {
				cells[row][col].Width = 1
			}
		}
	}
	surface.Cells = cells
	return surface
}

func blankRow(width int) []Cell {
	row := make([]Cell, width)
	for i := range row {
		row[i] = Cell{Rune: ' ', Width: 1}
	}
	return row
}

func rowEqual(a, b []Cell) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

func renderRow(row []Cell) string {
	var b strings.Builder
	current := Style{}
	for _, cell := range row {
		if cell.Rune == 0 {
			cell.Rune = ' '
		}
		if cell.Style != current {
			b.WriteString(styleReset(current))
			b.WriteString(styleOpen(cell.Style))
			current = cell.Style
		}
		b.WriteRune(cell.Rune)
	}
	b.WriteString(styleReset(current))
	return b.String()
}

func styleOpen(style Style) string {
	var codes []string
	if style.Bold {
		codes = append(codes, "1")
	}
	if style.Italic {
		codes = append(codes, "3")
	}
	if style.Underline {
		codes = append(codes, "4")
	}
	if style.Foreground.Set {
		codes = append(codes, fmt.Sprintf("38;2;%d;%d;%d", style.Foreground.R, style.Foreground.G, style.Foreground.B))
	}
	if style.Background.Set {
		codes = append(codes, fmt.Sprintf("48;2;%d;%d;%d", style.Background.R, style.Background.G, style.Background.B))
	}
	out := ""
	if len(codes) > 0 {
		out += "\x1b[" + strings.Join(codes, ";") + "m"
	}
	if style.Hyperlink != "" {
		out += "\x1b]8;;" + style.Hyperlink + "\x1b\\"
	}
	return out
}

func styleReset(style Style) string {
	if style == (Style{}) {
		return ""
	}
	if style.Hyperlink != "" {
		return "\x1b]8;;\x1b\\\x1b[0m"
	}
	return "\x1b[0m"
}

func clone(surface Surface) Surface {
	next := Surface{Width: surface.Width, Height: surface.Height, Cells: make([][]Cell, len(surface.Cells))}
	for i := range surface.Cells {
		next.Cells[i] = append([]Cell(nil), surface.Cells[i]...)
	}
	return next
}
