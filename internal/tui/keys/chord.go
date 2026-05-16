package keys

import (
	"fmt"
	"strings"
	"unicode/utf8"
)

type Chord struct {
	Strokes []Stroke
}

type Stroke struct {
	Key       Key
	Runes     []rune
	Modifiers Modifier
}

func ParseChord(s string) (Chord, error) {
	fields := strings.Fields(strings.TrimSpace(s))
	if len(fields) == 0 {
		return Chord{}, fmt.Errorf("empty chord")
	}
	strokes := make([]Stroke, 0, len(fields))
	for _, field := range fields {
		stroke, err := parseStroke(field)
		if err != nil {
			return Chord{}, err
		}
		strokes = append(strokes, stroke)
	}
	return Chord{Strokes: strokes}, nil
}

func parseStroke(s string) (Stroke, error) {
	parts := strings.Split(strings.ToLower(strings.TrimSpace(s)), "+")
	if len(parts) == 0 {
		return Stroke{}, fmt.Errorf("empty stroke")
	}
	var mods Modifier
	keyName := parts[len(parts)-1]
	for _, part := range parts[:len(parts)-1] {
		switch part {
		case "shift":
			mods |= ModShift
		case "ctrl", "control":
			mods |= ModCtrl
		case "alt", "option":
			mods |= ModAlt
		case "meta", "super", "cmd", "command":
			mods |= ModMeta
		default:
			return Stroke{}, fmt.Errorf("unknown modifier %q", part)
		}
	}
	key, runes, ok := parseKeyName(keyName)
	if !ok {
		return Stroke{}, fmt.Errorf("unknown key %q", keyName)
	}
	return Stroke{Key: key, Runes: runes, Modifiers: mods}, nil
}

func parseKeyName(name string) (Key, []rune, bool) {
	switch name {
	case "enter", "return":
		return KeyEnter, nil, true
	case "escape", "esc":
		return KeyEscape, nil, true
	case "backspace", "bs":
		return KeyBackspace, nil, true
	case "tab":
		return KeyTab, nil, true
	case "up":
		return KeyArrowUp, nil, true
	case "down":
		return KeyArrowDown, nil, true
	case "left":
		return KeyArrowLeft, nil, true
	case "right":
		return KeyArrowRight, nil, true
	case "home":
		return KeyHome, nil, true
	case "end":
		return KeyEnd, nil, true
	case "pageup", "pgup":
		return KeyPageUp, nil, true
	case "pagedown", "pgdown":
		return KeyPageDown, nil, true
	case "delete", "del":
		return KeyDelete, nil, true
	case "insert", "ins":
		return KeyInsert, nil, true
	case "space":
		return KeyRune, []rune{' '}, true
	}
	if strings.HasPrefix(name, "f") && len(name) > 1 {
		n := 0
		for _, r := range name[1:] {
			if r < '0' || r > '9' {
				return 0, nil, false
			}
			n = n*10 + int(r-'0')
		}
		if n >= 1 && n <= 24 {
			return Key(int(KeyF1) + n - 1), nil, true
		}
	}
	if utf8.RuneCountInString(name) == 1 {
		return KeyRune, []rune(name), true
	}
	return 0, nil, false
}

func (c Chord) String() string {
	parts := make([]string, 0, len(c.Strokes))
	for _, stroke := range c.Strokes {
		parts = append(parts, stroke.String())
	}
	return strings.Join(parts, " ")
}

func (s Stroke) String() string {
	parts := make([]string, 0, 5)
	if s.Modifiers&ModCtrl != 0 {
		parts = append(parts, "ctrl")
	}
	if s.Modifiers&ModAlt != 0 {
		parts = append(parts, "alt")
	}
	if s.Modifiers&ModShift != 0 {
		parts = append(parts, "shift")
	}
	if s.Modifiers&ModMeta != 0 {
		parts = append(parts, "meta")
	}
	parts = append(parts, keyName(s.Key, s.Runes))
	return strings.Join(parts, "+")
}

func keyName(key Key, runes []rune) string {
	switch key {
	case KeyEnter:
		return "enter"
	case KeyEscape:
		return "escape"
	case KeyBackspace:
		return "backspace"
	case KeyTab:
		return "tab"
	case KeyArrowUp:
		return "up"
	case KeyArrowDown:
		return "down"
	case KeyArrowLeft:
		return "left"
	case KeyArrowRight:
		return "right"
	case KeyHome:
		return "home"
	case KeyEnd:
		return "end"
	case KeyPageUp:
		return "pageup"
	case KeyPageDown:
		return "pagedown"
	case KeyDelete:
		return "delete"
	case KeyInsert:
		return "insert"
	case KeyRune:
		if len(runes) == 1 && runes[0] == ' ' {
			return "space"
		}
		return string(runes)
	default:
		if key >= KeyF1 && key <= KeyF24 {
			return fmt.Sprintf("f%d", int(key-KeyF1)+1)
		}
	}
	return ""
}

func (s Stroke) Matches(event Event) bool {
	if event.Type != TypeKey {
		return false
	}
	if event.Key != s.Key {
		return false
	}
	if event.Modifiers != s.Modifiers {
		return false
	}
	if s.Key == KeyRune {
		return string(event.Runes) == string(s.Runes)
	}
	return true
}

func canonicalChordString(s string) string {
	chord, err := ParseChord(s)
	if err != nil {
		return strings.ToLower(strings.TrimSpace(s))
	}
	return chord.String()
}
