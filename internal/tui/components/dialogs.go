package components

import (
	"fmt"
	"strings"

	"github.com/charmbracelet/lipgloss"
)

type SelectorItem struct {
	Label       string
	Description string
}

type SelectorOpts struct {
	Title    string
	Items    []SelectorItem
	Selected int
	Width    int
	MaxRows  int
}

func SelectorView(opts SelectorOpts) string {
	width := opts.Width
	if width <= 0 {
		width = 80
	}
	maxRows := opts.MaxRows
	if maxRows <= 0 {
		maxRows = 8
	}
	selected := opts.Selected
	if selected < 0 {
		selected = 0
	}
	if selected >= len(opts.Items) {
		selected = len(opts.Items) - 1
	}
	start := 0
	if selected >= maxRows {
		start = selected - maxRows + 1
	}
	end := min(len(opts.Items), start+maxRows)
	var lines []string
	if opts.Title != "" {
		lines = append(lines, mdHeadingStyle.Render(truncateWidth(opts.Title, width)))
	}
	if len(opts.Items) == 0 {
		lines = append(lines, messageThinkingStyle.Render("  No matches"))
		return strings.Join(lines, "\n")
	}
	for i := start; i < end; i++ {
		item := opts.Items[i]
		prefix := "  "
		style := lipgloss.NewStyle()
		if i == selected {
			prefix = "> "
			style = lipgloss.NewStyle().Foreground(lipgloss.Color("231")).Background(lipgloss.Color("238"))
		}
		labelWidth := width - 2
		desc := normalizeSingleLine(item.Description)
		if desc != "" && width > 42 {
			labelWidth = min(32, width/2)
			label := padWidth(truncateWidth(item.Label, labelWidth), labelWidth)
			remaining := width - visibleWidth(prefix) - visibleWidth(label) - 2
			lines = append(lines, style.Render(prefix+label+"  "+truncateWidth(desc, remaining)))
			continue
		}
		lines = append(lines, style.Render(prefix+truncateWidth(item.Label, labelWidth)))
	}
	if start > 0 || end < len(opts.Items) {
		lines = append(lines, messageThinkingStyle.Render(fmt.Sprintf("  (%d/%d)", selected+1, len(opts.Items))))
	}
	return strings.Join(lines, "\n")
}

type InputDialogOpts struct {
	Title       string
	Placeholder string
	Value       string
	Width       int
	Focused     bool
}

func InputDialogView(opts InputDialogOpts) string {
	width := opts.Width
	if width <= 0 {
		width = 80
	}
	value := opts.Value
	if value == "" {
		value = messageThinkingStyle.Render(opts.Placeholder)
	}
	cursor := " "
	if opts.Focused {
		cursor = "\x1b[7m \x1b[27m"
	}
	lines := []string{}
	if opts.Title != "" {
		lines = append(lines, mdHeadingStyle.Render(truncateWidth(opts.Title, width)))
	}
	lines = append(lines, "> "+truncateWidth(value, max(1, width-4))+cursor)
	return strings.Join(lines, "\n")
}

func LoaderView(text string, frame int) string {
	frames := []string{"-", "\\", "|", "/"}
	if frame < 0 {
		frame = 0
	}
	return messageThinkingStyle.Render(frames[frame%len(frames)] + " " + text)
}

func CancellableLoaderView(text string, frame int) string {
	return LoaderView(text+" (press Esc to cancel)", frame)
}
