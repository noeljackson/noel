package main

import (
	"fmt"
	"os"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/noeljackson/pi/internal/tui"
	"github.com/noeljackson/pi/internal/tui/mock"
)

func main() {
	model := tui.New(tui.Options{
		EventSource: mock.Source(250 * time.Millisecond),
		Submit: func(text string) {
			fmt.Fprintf(os.Stderr, "submitted: %s\n", text)
		},
		Abort: func() {
			fmt.Fprintln(os.Stderr, "abort requested")
		},
	})

	program := tea.NewProgram(model, tea.WithAltScreen())
	if _, err := program.Run(); err != nil {
		fmt.Fprintf(os.Stderr, "pi-tui-demo: %v\n", err)
		os.Exit(1)
	}
}
