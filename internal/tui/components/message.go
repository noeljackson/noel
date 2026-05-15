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
)

// UserMessageView renders a user-authored prompt.
func UserMessageView(text string) string {
	text = strings.TrimSpace(text)
	if text == "" {
		return ""
	}
	return messageUserStyle.Render(text)
}

// AssistantMessageView renders assistant content blocks.
func AssistantMessageView(content []agent.Content) string {
	var parts []string
	for _, block := range content {
		switch value := block.(type) {
		case agent.TextContent:
			if strings.TrimSpace(value.Text) != "" {
				parts = append(parts, messageAssistantStyle.Render(strings.TrimSpace(value.Text)))
			}
		case agent.ThinkingContent:
			if strings.TrimSpace(value.Thinking) != "" {
				parts = append(parts, messageThinkingStyle.Render(strings.TrimSpace(value.Thinking)))
			}
		case agent.ToolUseContent:
			continue
		}
	}
	return strings.Join(parts, "\n\n")
}
