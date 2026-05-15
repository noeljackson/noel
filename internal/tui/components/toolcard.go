package components

import (
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/charmbracelet/lipgloss"
)

const maxToolBodyLines = 10

type ToolStatus string

const (
	ToolPending ToolStatus = "pending"
	ToolRunning ToolStatus = "running"
	ToolDone    ToolStatus = "done"
	ToolError   ToolStatus = "error"
)

type ToolCardState struct {
	ID          string
	Name        string
	ArgsSummary string
	Status      ToolStatus
	Body        []string
	StartedAt   time.Time
	EndedAt     time.Time
	ExitCode    *int
	Bytes       *int
	Err         string
}

var (
	toolBorderStyle  = lipgloss.NewStyle().Border(lipgloss.RoundedBorder()).BorderForeground(lipgloss.Color("240")).Padding(0, 1)
	toolTitleStyle   = lipgloss.NewStyle().Foreground(lipgloss.Color("39")).Bold(true)
	toolDimStyle     = lipgloss.NewStyle().Foreground(lipgloss.Color("244"))
	toolSuccessStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("42"))
	toolDangerStyle  = lipgloss.NewStyle().Foreground(lipgloss.Color("196"))
)

func ToolCard(card ToolCardState, width int) string {
	if card.Status == "" {
		card.Status = ToolPending
	}

	header := toolTitleStyle.Render(card.Name)
	if card.ArgsSummary != "" {
		header += " " + toolDimStyle.Render(truncateSingleLine(card.ArgsSummary, max(8, width-12-len(card.Name))))
	}

	status := string(card.Status)
	switch card.Status {
	case ToolDone:
		status = toolSuccessStyle.Render(status)
	case ToolError:
		status = toolDangerStyle.Render(status)
	default:
		status = toolDimStyle.Render(status)
	}

	var lines []string
	lines = append(lines, fmt.Sprintf("%s [%s]", header, status))

	body := lastLines(card.Body, maxToolBodyLines)
	if len(body) > 0 {
		if len(card.Body) > len(body) {
			lines = append(lines, toolDimStyle.Render("... show more"))
		}
		lines = append(lines, body...)
	}

	footer := toolFooter(card)
	if footer != "" {
		lines = append(lines, toolDimStyle.Render(footer))
	}

	style := toolBorderStyle
	if width > 4 {
		style = style.Width(width - 4)
	}
	return style.Render(strings.Join(lines, "\n"))
}

func CompactJSON(raw json.RawMessage) string {
	if len(raw) == 0 {
		return ""
	}

	var value interface{}
	if err := json.Unmarshal(raw, &value); err != nil {
		return strings.TrimSpace(string(raw))
	}

	compact, err := json.Marshal(value)
	if err != nil {
		return strings.TrimSpace(string(raw))
	}
	return string(compact)
}

func AppendRawBody(body []string, raw json.RawMessage) []string {
	text := RawText(raw)
	if text == "" {
		return body
	}
	return append(body, strings.Split(strings.TrimRight(text, "\n"), "\n")...)
}

func RawText(raw json.RawMessage) string {
	if len(raw) == 0 {
		return ""
	}

	var text string
	if err := json.Unmarshal(raw, &text); err == nil {
		return text
	}

	var object map[string]interface{}
	if err := json.Unmarshal(raw, &object); err == nil {
		var parts []string
		for _, key := range []string{"text", "output", "stdout", "stderr"} {
			if value, ok := object[key].(string); ok && value != "" {
				parts = append(parts, value)
			}
		}
		if len(parts) > 0 {
			return strings.Join(parts, "\n")
		}
	}

	return CompactJSON(raw)
}

func toolFooter(card ToolCardState) string {
	var parts []string
	if !card.StartedAt.IsZero() {
		end := card.EndedAt
		if end.IsZero() {
			end = time.Now()
		}
		parts = append(parts, end.Sub(card.StartedAt).Round(time.Millisecond).String())
	}
	if card.ExitCode != nil {
		parts = append(parts, fmt.Sprintf("exit %d", *card.ExitCode))
	}
	if card.Bytes != nil {
		parts = append(parts, fmt.Sprintf("%d bytes", *card.Bytes))
	}
	if card.Err != "" {
		parts = append(parts, card.Err)
	}
	return strings.Join(parts, " | ")
}

func lastLines(lines []string, count int) []string {
	if len(lines) <= count {
		return lines
	}
	return lines[len(lines)-count:]
}

func truncateSingleLine(text string, width int) string {
	text = strings.Join(strings.Fields(text), " ")
	if width <= 0 || len(text) <= width {
		return text
	}
	if width <= 1 {
		return "..."
	}
	return text[:width-1] + "..."
}
