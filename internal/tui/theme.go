package tui

import "github.com/charmbracelet/lipgloss"

type theme struct {
	accent  lipgloss.Style
	dim     lipgloss.Style
	danger  lipgloss.Style
	success lipgloss.Style
	panel   lipgloss.Style
}

var defaultTheme = theme{
	accent:  lipgloss.NewStyle().Foreground(lipgloss.Color("39")),
	dim:     lipgloss.NewStyle().Foreground(lipgloss.Color("244")),
	danger:  lipgloss.NewStyle().Foreground(lipgloss.Color("196")),
	success: lipgloss.NewStyle().Foreground(lipgloss.Color("42")),
	panel:   lipgloss.NewStyle().Border(lipgloss.NormalBorder()).BorderForeground(lipgloss.Color("240")),
}
