package components

import (
	"fmt"
	"strings"

	"github.com/charmbracelet/lipgloss"
)

type FooterState struct {
	Model            string
	InputTokens      int
	OutputTokens     int
	CacheReadTokens  int
	CacheWriteTokens int
	Turn             string
	Queued           int
	Mode             string
	Thinking         string
	Status           string
	Width            int
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
		fmt.Sprintf("model: %s", model),
		fmt.Sprintf("tokens: ↑%s ↓%s", formatTokens(state.InputTokens), formatTokens(state.OutputTokens)),
		fmt.Sprintf("turn: %s", turn),
	}
	if state.CacheReadTokens > 0 {
		parts = append(parts, "R"+formatTokens(state.CacheReadTokens))
	}
	if state.CacheWriteTokens > 0 {
		parts = append(parts, "W"+formatTokens(state.CacheWriteTokens))
	}
	if state.Mode != "" {
		parts = append(parts, "mode: "+state.Mode)
	}
	if state.Thinking != "" {
		parts = append(parts, "thinking: "+state.Thinking)
	}
	if state.Queued > 0 {
		parts = append(parts, fmt.Sprintf("queued: %d", state.Queued))
	}
	if state.Status != "" {
		parts = append(parts, state.Status)
	}
	line := strings.Join(parts, "  ")
	if state.Width > 0 {
		line = truncateWidth(line, state.Width)
	}
	return footerStyle.Render(line)
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
