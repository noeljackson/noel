package components

import (
	"fmt"
	"strings"

	"github.com/charmbracelet/lipgloss"
)

type FooterState struct {
	Model        string
	InputTokens  int
	OutputTokens int
	Turn         string
	Queued       int
}

var footerStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("244"))

func FooterView(state FooterState) string {
	model := state.Model
	if model == "" {
		model = "unknown"
	}
	turn := state.Turn
	if turn == "" {
		turn = "idle"
	}
	parts := []string{
		fmt.Sprintf("[model: %s]", model),
		fmt.Sprintf("[tokens: %s/%s]", formatTokens(state.InputTokens), formatTokens(state.OutputTokens)),
		fmt.Sprintf("[turn: %s]", turn),
		fmt.Sprintf("[queued: %d]", state.Queued),
	}
	return footerStyle.Render(strings.Join(parts, " "))
}

func formatTokens(count int) string {
	if count < 1000 {
		return fmt.Sprintf("%d", count)
	}
	if count < 10000 {
		return fmt.Sprintf("%.1fk", float64(count)/1000)
	}
	if count < 1000000 {
		return fmt.Sprintf("%dk", count/1000)
	}
	if count < 10000000 {
		return fmt.Sprintf("%.1fM", float64(count)/1000000)
	}
	return fmt.Sprintf("%dM", count/1000000)
}
