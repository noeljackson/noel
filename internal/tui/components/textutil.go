package components

import (
	"regexp"
	"strings"

	"github.com/charmbracelet/lipgloss"
)

var ansiRegexp = regexp.MustCompile(`\x1b(?:\[[0-9;?]*[A-Za-z]|\][^\x07]*(?:\x07|\x1b\\)|_[^\x07]*(?:\x07|\x1b\\))`)

func visibleWidth(text string) int {
	return lipgloss.Width(text)
}

func stripANSI(text string) string {
	return ansiRegexp.ReplaceAllString(text, "")
}

func wrapLines(text string, width int) []string {
	if width <= 0 {
		width = 80
	}
	var out []string
	for _, rawLine := range strings.Split(text, "\n") {
		if rawLine == "" {
			out = append(out, "")
			continue
		}
		line := strings.ReplaceAll(rawLine, "\t", "   ")
		for visibleWidth(line) > width {
			cut := cutIndex(line, width)
			out = append(out, strings.TrimRight(line[:cut], " "))
			line = strings.TrimLeft(line[cut:], " ")
			if line == "" {
				break
			}
		}
		if line != "" {
			out = append(out, line)
		}
	}
	if len(out) == 0 {
		return []string{""}
	}
	return out
}

func cutIndex(text string, width int) int {
	if width <= 0 {
		return 0
	}
	lastSpace := -1
	used := 0
	for idx, r := range text {
		if r == ' ' {
			lastSpace = idx
		}
		used += runeCellWidth(r)
		if used > width {
			if lastSpace > 0 {
				return lastSpace
			}
			return idx
		}
	}
	return len(text)
}

func truncateWidth(text string, width int) string {
	if width <= 0 {
		return ""
	}
	if visibleWidth(text) <= width {
		return text
	}
	ellipsis := "..."
	if width <= len(ellipsis) {
		return ellipsis[:width]
	}
	target := width - len(ellipsis)
	used := 0
	for idx, r := range text {
		used += runeCellWidth(r)
		if used > target {
			return text[:idx] + ellipsis
		}
	}
	return text
}

func padWidth(text string, width int) string {
	if width <= 0 {
		return ""
	}
	visible := visibleWidth(text)
	if visible >= width {
		return truncateWidth(text, width)
	}
	return text + strings.Repeat(" ", width-visible)
}

func runeCellWidth(r rune) int {
	if r == 0 {
		return 0
	}
	if r < 0x20 || r == 0x7f {
		return 0
	}
	if r >= 0x1100 &&
		(r <= 0x115f ||
			r == 0x2329 || r == 0x232a ||
			(r >= 0x2e80 && r <= 0xa4cf) ||
			(r >= 0xac00 && r <= 0xd7a3) ||
			(r >= 0xf900 && r <= 0xfaff) ||
			(r >= 0xfe10 && r <= 0xfe19) ||
			(r >= 0xfe30 && r <= 0xfe6f) ||
			(r >= 0xff00 && r <= 0xff60) ||
			(r >= 0xffe0 && r <= 0xffe6) ||
			(r >= 0x1f300 && r <= 0x1faff)) {
		return 2
	}
	return 1
}

func normalizeSingleLine(text string) string {
	return strings.Join(strings.Fields(strings.ReplaceAll(text, "\n", " ")), " ")
}
