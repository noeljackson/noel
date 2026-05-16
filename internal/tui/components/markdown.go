package components

import (
	"fmt"
	"regexp"
	"strconv"
	"strings"

	"github.com/charmbracelet/lipgloss"
)

var (
	mdHeadingStyle    = lipgloss.NewStyle().Foreground(lipgloss.Color("75")).Bold(true)
	mdCodeStyle       = lipgloss.NewStyle().Foreground(lipgloss.Color("228"))
	mdCodeBlockStyle  = lipgloss.NewStyle().Foreground(lipgloss.Color("252")).Background(lipgloss.Color("236"))
	mdQuoteStyle      = lipgloss.NewStyle().Foreground(lipgloss.Color("244")).Italic(true)
	mdLinkStyle       = lipgloss.NewStyle().Foreground(lipgloss.Color("81")).Underline(true)
	mdTableLineStyle  = lipgloss.NewStyle().Foreground(lipgloss.Color("244"))
	mdStrongStyle     = lipgloss.NewStyle().Bold(true)
	mdEmphasisStyle   = lipgloss.NewStyle().Italic(true)
	mdInlineCodeRegex = regexp.MustCompile("`([^`]+)`")
	mdStrongRegex     = regexp.MustCompile(`\*\*([^*]+)\*\*`)
	mdEmRegex         = regexp.MustCompile(`(^|[^*])\*([^*]+)\*`)
	mdLinkRegex       = regexp.MustCompile(`\[([^\]]+)\]\(([^)]+)\)`)
)

func MarkdownView(text string, width int) string {
	text = strings.TrimSpace(strings.ReplaceAll(text, "\t", "   "))
	if text == "" {
		return ""
	}
	if width <= 0 {
		width = 80
	}
	lines := renderMarkdownLines(text, width)
	return strings.Join(lines, "\n")
}

func renderMarkdownLines(text string, width int) []string {
	raw := strings.Split(text, "\n")
	var out []string
	inCode := false
	codeLang := ""
	var paragraph []string
	flushParagraph := func() {
		if len(paragraph) == 0 {
			return
		}
		rendered := renderInline(strings.Join(paragraph, " "))
		out = append(out, wrapLines(rendered, width)...)
		paragraph = nil
	}

	for i := 0; i < len(raw); i++ {
		line := raw[i]
		trimmed := strings.TrimSpace(line)
		if strings.HasPrefix(trimmed, "```") {
			flushParagraph()
			if !inCode {
				inCode = true
				codeLang = strings.TrimSpace(strings.TrimPrefix(trimmed, "```"))
				label := "```"
				if codeLang != "" {
					label += codeLang
				}
				out = append(out, mdTableLineStyle.Render(label))
			} else {
				inCode = false
				codeLang = ""
				out = append(out, mdTableLineStyle.Render("```"))
			}
			continue
		}
		if inCode {
			out = append(out, mdCodeBlockStyle.Render("  "+line))
			continue
		}
		if trimmed == "" {
			flushParagraph()
			if len(out) > 0 && out[len(out)-1] != "" {
				out = append(out, "")
			}
			continue
		}
		if isTableStart(raw, i) {
			flushParagraph()
			tableLines, consumed := renderTable(raw[i:], width)
			out = append(out, tableLines...)
			i += consumed - 1
			continue
		}
		if heading, ok := renderHeading(trimmed); ok {
			flushParagraph()
			out = append(out, wrapLines(heading, width)...)
			continue
		}
		if strings.HasPrefix(trimmed, ">") {
			flushParagraph()
			quote := strings.TrimSpace(strings.TrimPrefix(trimmed, ">"))
			for _, qline := range wrapLines(mdQuoteStyle.Render(quote), max(1, width-2)) {
				out = append(out, mdTableLineStyle.Render("| ")+qline)
			}
			continue
		}
		if rendered, ok := renderListItem(trimmed); ok {
			flushParagraph()
			out = append(out, wrapLines(rendered, width)...)
			continue
		}
		if isHorizontalRule(trimmed) {
			flushParagraph()
			out = append(out, mdTableLineStyle.Render(strings.Repeat("-", min(width, 80))))
			continue
		}
		paragraph = append(paragraph, trimmed)
	}
	flushParagraph()
	for len(out) > 0 && out[len(out)-1] == "" {
		out = out[:len(out)-1]
	}
	return out
}

func renderHeading(line string) (string, bool) {
	level := 0
	for level < len(line) && line[level] == '#' {
		level++
	}
	if level == 0 || level > 6 || level >= len(line) || line[level] != ' ' {
		return "", false
	}
	text := strings.TrimSpace(line[level:])
	if level >= 3 {
		text = strings.Repeat("#", level) + " " + text
	}
	return mdHeadingStyle.Render(text), true
}

func renderListItem(line string) (string, bool) {
	if strings.HasPrefix(line, "- ") || strings.HasPrefix(line, "* ") {
		return mdTableLineStyle.Render("- ") + renderInline(strings.TrimSpace(line[2:])), true
	}
	dot := strings.Index(line, ". ")
	if dot > 0 {
		if _, err := strconv.Atoi(line[:dot]); err == nil {
			return mdTableLineStyle.Render(line[:dot+2]) + renderInline(strings.TrimSpace(line[dot+2:])), true
		}
	}
	return "", false
}

func renderInline(text string) string {
	text = mdLinkRegex.ReplaceAllStringFunc(text, func(match string) string {
		parts := mdLinkRegex.FindStringSubmatch(match)
		if len(parts) != 3 {
			return match
		}
		return mdLinkStyle.Render(parts[1]) + lipgloss.NewStyle().Foreground(lipgloss.Color("244")).Render(" ("+parts[2]+")")
	})
	text = mdInlineCodeRegex.ReplaceAllStringFunc(text, func(match string) string {
		parts := mdInlineCodeRegex.FindStringSubmatch(match)
		if len(parts) != 2 {
			return match
		}
		return mdCodeStyle.Render(parts[1])
	})
	text = mdStrongRegex.ReplaceAllStringFunc(text, func(match string) string {
		parts := mdStrongRegex.FindStringSubmatch(match)
		if len(parts) != 2 {
			return match
		}
		return mdStrongStyle.Render(parts[1])
	})
	text = mdEmRegex.ReplaceAllStringFunc(text, func(match string) string {
		parts := mdEmRegex.FindStringSubmatch(match)
		if len(parts) != 3 {
			return match
		}
		return parts[1] + mdEmphasisStyle.Render(parts[2])
	})
	return text
}

func isTableStart(lines []string, index int) bool {
	if index+1 >= len(lines) {
		return false
	}
	header := strings.TrimSpace(lines[index])
	separator := strings.TrimSpace(lines[index+1])
	return strings.Contains(header, "|") && strings.Contains(separator, "|") && strings.Trim(separator, "| :-") == ""
}

func renderTable(lines []string, width int) ([]string, int) {
	var rows [][]string
	consumed := 0
	for _, line := range lines {
		trimmed := strings.TrimSpace(line)
		if trimmed == "" || !strings.Contains(trimmed, "|") {
			break
		}
		consumed++
		if consumed == 2 {
			continue
		}
		rows = append(rows, splitTableRow(trimmed))
	}
	if len(rows) == 0 {
		return nil, consumed
	}
	cols := 0
	for _, row := range rows {
		cols = max(cols, len(row))
	}
	widths := make([]int, cols)
	for _, row := range rows {
		for i, cell := range row {
			widths[i] = max(widths[i], visibleWidth(renderInline(cell)))
		}
	}
	maxCell := max(1, (width-(cols*3+1))/max(1, cols))
	for i := range widths {
		widths[i] = min(max(widths[i], 3), maxCell)
	}
	border := func(left, mid, right string) string {
		parts := make([]string, len(widths))
		for i, w := range widths {
			parts[i] = strings.Repeat("-", w+2)
		}
		return mdTableLineStyle.Render(left + strings.Join(parts, mid) + right)
	}
	var out []string
	out = append(out, border("+", "+", "+"))
	for r, row := range rows {
		cells := make([]string, len(widths))
		for i := range widths {
			cell := ""
			if i < len(row) {
				cell = renderInline(row[i])
			}
			cells[i] = " " + padWidth(cell, widths[i]) + " "
		}
		line := "|" + strings.Join(cells, "|") + "|"
		if r == 0 {
			line = mdStrongStyle.Render(line)
		}
		out = append(out, line)
		if r == 0 {
			out = append(out, border("+", "+", "+"))
		}
	}
	out = append(out, border("+", "+", "+"))
	return out, consumed
}

func splitTableRow(line string) []string {
	line = strings.Trim(line, "|")
	parts := strings.Split(line, "|")
	for i := range parts {
		parts[i] = strings.TrimSpace(parts[i])
	}
	return parts
}

func isHorizontalRule(line string) bool {
	if len(line) < 3 {
		return false
	}
	for _, r := range line {
		if r != '-' && r != '_' && r != '*' {
			return false
		}
	}
	return true
}

func markdownBenchmarkInput(size int) string {
	var b strings.Builder
	for b.Len() < size {
		b.WriteString(fmt.Sprintf("## Heading %d\n\nParagraph with **bold**, *italic*, `code`, and [link](https://example.com).\n\n- one\n- two\n\n", b.Len()))
	}
	return b.String()
}
