package keys

import tea "github.com/charmbracelet/bubbletea"

func FromTeaKey(msg tea.KeyMsg) Event {
	raw := msg.String()
	mod := ModNone
	if msg.Alt {
		mod |= ModAlt
	}
	if msg.Type == tea.KeyRunes || msg.Type == tea.KeySpace {
		runes := msg.Runes
		if len(runes) == 0 && msg.Type == tea.KeySpace {
			runes = []rune{' '}
		}
		return Event{Type: TypeKey, Key: KeyRune, Runes: runes, Modifiers: mod, Raw: raw}
	}
	switch msg.Type {
	case tea.KeyEnter:
		return Event{Type: TypeKey, Key: KeyEnter, Modifiers: mod, Raw: raw}
	case tea.KeyEsc:
		return Event{Type: TypeKey, Key: KeyEscape, Modifiers: mod, Raw: raw}
	case tea.KeyBackspace, tea.KeyCtrlH:
		return Event{Type: TypeKey, Key: KeyBackspace, Modifiers: mod, Raw: raw}
	case tea.KeyTab:
		return Event{Type: TypeKey, Key: KeyTab, Modifiers: mod, Raw: raw}
	case tea.KeyShiftTab:
		return Event{Type: TypeKey, Key: KeyTab, Modifiers: mod | ModShift, Raw: raw}
	case tea.KeyUp:
		return Event{Type: TypeKey, Key: KeyArrowUp, Modifiers: mod, Raw: raw}
	case tea.KeyDown:
		return Event{Type: TypeKey, Key: KeyArrowDown, Modifiers: mod, Raw: raw}
	case tea.KeyLeft:
		return Event{Type: TypeKey, Key: KeyArrowLeft, Modifiers: mod, Raw: raw}
	case tea.KeyRight:
		return Event{Type: TypeKey, Key: KeyArrowRight, Modifiers: mod, Raw: raw}
	case tea.KeyCtrlUp:
		return Event{Type: TypeKey, Key: KeyArrowUp, Modifiers: mod | ModCtrl, Raw: raw}
	case tea.KeyCtrlDown:
		return Event{Type: TypeKey, Key: KeyArrowDown, Modifiers: mod | ModCtrl, Raw: raw}
	case tea.KeyCtrlLeft:
		return Event{Type: TypeKey, Key: KeyArrowLeft, Modifiers: mod | ModCtrl, Raw: raw}
	case tea.KeyCtrlRight:
		return Event{Type: TypeKey, Key: KeyArrowRight, Modifiers: mod | ModCtrl, Raw: raw}
	case tea.KeyShiftUp:
		return Event{Type: TypeKey, Key: KeyArrowUp, Modifiers: mod | ModShift, Raw: raw}
	case tea.KeyShiftDown:
		return Event{Type: TypeKey, Key: KeyArrowDown, Modifiers: mod | ModShift, Raw: raw}
	case tea.KeyShiftLeft:
		return Event{Type: TypeKey, Key: KeyArrowLeft, Modifiers: mod | ModShift, Raw: raw}
	case tea.KeyShiftRight:
		return Event{Type: TypeKey, Key: KeyArrowRight, Modifiers: mod | ModShift, Raw: raw}
	case tea.KeyHome:
		return Event{Type: TypeKey, Key: KeyHome, Modifiers: mod, Raw: raw}
	case tea.KeyEnd:
		return Event{Type: TypeKey, Key: KeyEnd, Modifiers: mod, Raw: raw}
	case tea.KeyPgUp:
		return Event{Type: TypeKey, Key: KeyPageUp, Modifiers: mod, Raw: raw}
	case tea.KeyPgDown:
		return Event{Type: TypeKey, Key: KeyPageDown, Modifiers: mod, Raw: raw}
	case tea.KeyDelete:
		return Event{Type: TypeKey, Key: KeyDelete, Modifiers: mod, Raw: raw}
	case tea.KeyInsert:
		return Event{Type: TypeKey, Key: KeyInsert, Modifiers: mod, Raw: raw}
	}
	if msg.Type >= tea.KeyCtrlA && msg.Type <= tea.KeyCtrlZ {
		return Event{Type: TypeKey, Key: KeyRune, Runes: []rune{rune('a' + msg.Type - tea.KeyCtrlA)}, Modifiers: mod | ModCtrl, Raw: raw}
	}
	if msg.Type >= tea.KeyF1 && msg.Type <= tea.KeyF20 {
		return Event{Type: TypeKey, Key: Key(int(KeyF1) + int(msg.Type-tea.KeyF1)), Modifiers: mod, Raw: raw}
	}
	if len(msg.Runes) > 0 {
		return Event{Type: TypeKey, Key: KeyRune, Runes: msg.Runes, Modifiers: mod, Raw: raw}
	}
	return Event{Type: TypeKey, Key: KeyUnknown, Raw: raw}
}

func FromTeaMouse(msg tea.MouseMsg) Event {
	mouse := tea.MouseEvent(msg)
	mod := ModNone
	if mouse.Shift {
		mod |= ModShift
	}
	if mouse.Ctrl {
		mod |= ModCtrl
	}
	if mouse.Alt {
		mod |= ModAlt
	}
	return Event{
		Type:      TypeMouse,
		Modifiers: mod,
		Raw:       msg.String(),
		Mouse: Mouse{
			X:       mouse.X,
			Y:       mouse.Y,
			Button:  int(mouse.Button),
			Release: mouse.Action == tea.MouseActionRelease,
		},
	}
}
