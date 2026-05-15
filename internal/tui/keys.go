package tui

import "github.com/charmbracelet/bubbles/key"

type keyMap struct {
	submit      key.Binding
	newline     key.Binding
	abortOrQuit key.Binding
	pageUp      key.Binding
	pageDown    key.Binding
	clear       key.Binding
}

func defaultKeyMap() keyMap {
	return keyMap{
		submit: key.NewBinding(
			key.WithKeys("enter"),
			key.WithHelp("enter", "submit"),
		),
		newline: key.NewBinding(
			key.WithKeys("shift+enter", "alt+enter"),
			key.WithHelp("shift+enter", "newline"),
		),
		abortOrQuit: key.NewBinding(
			key.WithKeys("ctrl+c"),
			key.WithHelp("ctrl+c", "abort/quit"),
		),
		pageUp: key.NewBinding(
			key.WithKeys("pgup"),
			key.WithHelp("pgup", "scroll up"),
		),
		pageDown: key.NewBinding(
			key.WithKeys("pgdown"),
			key.WithHelp("pgdn", "scroll down"),
		),
		clear: key.NewBinding(
			key.WithKeys("ctrl+l"),
			key.WithHelp("ctrl+l", "clear"),
		),
	}
}
