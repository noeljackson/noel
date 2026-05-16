package components

import (
	"strings"

	"github.com/charmbracelet/lipgloss"
	"github.com/noeljackson/pi/internal/agent"
)

var (
	messageUserStyle      = lipgloss.NewStyle().Foreground(lipgloss.Color("231")).Background(lipgloss.Color("238")).Padding(0, 1)
	messageAssistantStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("252"))
	messageThinkingStyle  = lipgloss.NewStyle().Foreground(lipgloss.Color("244")).Italic(true)
	messageErrorStyle     = lipgloss.NewStyle().Foreground(lipgloss.Color("203")).Bold(true)
)

// ErrorMessageView renders an error that occurred during the agent loop.
func ErrorMessageView(text string) string {
	text = strings.TrimSpace(text)
	if text == "" {
		return ""
	}
	return messageErrorStyle.Render("Error: " + text)
}

// UserMessageView renders a user-authored prompt.
func UserMessageView(text string) string {
	text = strings.TrimSpace(text)
	if text == "" {
		return ""
	}
	return messageUserStyle.Render(MarkdownView(text, 80))
}

type AssistantMessageOptions struct {
	Width            int
	ThinkingExpanded bool
	HideThinking     bool
}

func UserMessageViewWidth(text string, width int) string {
	text = strings.TrimSpace(text)
	if text == "" {
		return ""
	}
	return messageUserStyle.Width(max(0, width-2)).Render(MarkdownView(text, max(1, width-4)))
}

// AssistantMessageView renders assistant content blocks.
func AssistantMessageView(content []agent.Content, width ...int) string {
	w := 80
	if len(width) > 0 && width[0] > 0 {
		w = width[0]
	}
	return AssistantMessageViewWithOptions(content, AssistantMessageOptions{
		Width:            w,
		ThinkingExpanded: true,
	})
}

func AssistantMessageViewWithOptions(content []agent.Content, opts AssistantMessageOptions) string {
	width := opts.Width
	if width <= 0 {
		width = 80
	}
	var parts []string
	for _, block := range content {
		switch value := block.(type) {
		case agent.TextContent:
			if strings.TrimSpace(value.Text) != "" {
				parts = append(parts, messageAssistantStyle.Render(MarkdownView(value.Text, width)))
			}
		case agent.ThinkingContent:
			if strings.TrimSpace(value.Thinking) != "" {
				if opts.HideThinking {
					parts = append(parts, messageThinkingStyle.Render("[thinking...]"))
				} else {
					parts = append(parts, ThinkingView(value, width, opts.ThinkingExpanded))
				}
			}
		case agent.ToolUseContent:
			continue
		}
	}
	return strings.Join(parts, "\n\n")
}
