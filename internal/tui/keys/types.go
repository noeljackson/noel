package keys

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

type Type int

const (
	TypeKey Type = iota
	TypeMouse
	TypePaste
	TypeResize
	TypeFocus
	TypeBracketedPasteStart
	TypeBracketedPasteEnd
)

type Key int

const (
	KeyUnknown Key = iota
	KeyEnter
	KeyEscape
	KeyBackspace
	KeyTab
	KeyArrowUp
	KeyArrowDown
	KeyArrowLeft
	KeyArrowRight
	KeyHome
	KeyEnd
	KeyPageUp
	KeyPageDown
	KeyDelete
	KeyInsert
	KeyF1
	KeyF2
	KeyF3
	KeyF4
	KeyF5
	KeyF6
	KeyF7
	KeyF8
	KeyF9
	KeyF10
	KeyF11
	KeyF12
	KeyF13
	KeyF14
	KeyF15
	KeyF16
	KeyF17
	KeyF18
	KeyF19
	KeyF20
	KeyF21
	KeyF22
	KeyF23
	KeyF24
	KeyRune
)

type Modifier uint8

const (
	ModNone  Modifier = 0
	ModShift Modifier = 1 << 0
	ModCtrl  Modifier = 1 << 1
	ModAlt   Modifier = 1 << 2
	ModMeta  Modifier = 1 << 3
)

type Mouse struct {
	X       int
	Y       int
	Button  int
	Release bool
}

type Event struct {
	Type      Type
	Key       Key
	Runes     []rune
	Modifiers Modifier
	Raw       string
	Mouse     Mouse
}

type Action string

const (
	ActionSubmit             Action = "submit"
	ActionNewline            Action = "newline"
	ActionAbort              Action = "abort"
	ActionHistoryPrev        Action = "history-prev"
	ActionHistoryNext        Action = "history-next"
	ActionCursorUp           Action = "cursor-up"
	ActionCursorDown         Action = "cursor-down"
	ActionCursorLeft         Action = "cursor-left"
	ActionCursorRight        Action = "cursor-right"
	ActionCursorWordLeft     Action = "cursor-word-left"
	ActionCursorWordRight    Action = "cursor-word-right"
	ActionCursorLineStart    Action = "cursor-line-start"
	ActionCursorLineEnd      Action = "cursor-line-end"
	ActionJumpForward        Action = "jump-forward"
	ActionJumpBackward       Action = "jump-backward"
	ActionPageUp             Action = "page-up"
	ActionPageDown           Action = "page-down"
	ActionDeleteBackward     Action = "delete-backward"
	ActionDeleteForward      Action = "delete-forward"
	ActionDeleteWordBackward Action = "delete-word-backward"
	ActionDeleteWordForward  Action = "delete-word-forward"
	ActionDeleteToLineStart  Action = "delete-to-line-start"
	ActionDeleteToLineEnd    Action = "delete-to-line-end"
	ActionYank               Action = "yank"
	ActionYankPop            Action = "yank-pop"
	ActionUndo               Action = "undo"
	ActionTab                Action = "tab"
	ActionCopy               Action = "copy"
	ActionClear              Action = "clear"
	ActionExit               Action = "exit"
)

type Map map[string]Action

func Default() Map {
	return Map{
		"enter":         ActionSubmit,
		"shift+enter":   ActionNewline,
		"alt+enter":     ActionNewline,
		"ctrl+c":        ActionAbort,
		"up":            ActionCursorUp,
		"down":          ActionCursorDown,
		"left":          ActionCursorLeft,
		"ctrl+b":        ActionCursorLeft,
		"right":         ActionCursorRight,
		"ctrl+f":        ActionCursorRight,
		"alt+left":      ActionCursorWordLeft,
		"ctrl+left":     ActionCursorWordLeft,
		"alt+b":         ActionCursorWordLeft,
		"alt+right":     ActionCursorWordRight,
		"ctrl+right":    ActionCursorWordRight,
		"alt+f":         ActionCursorWordRight,
		"home":          ActionCursorLineStart,
		"ctrl+a":        ActionCursorLineStart,
		"end":           ActionCursorLineEnd,
		"ctrl+e":        ActionCursorLineEnd,
		"ctrl+]":        ActionJumpForward,
		"ctrl+alt+]":    ActionJumpBackward,
		"pageup":        ActionPageUp,
		"pagedown":      ActionPageDown,
		"backspace":     ActionDeleteBackward,
		"delete":        ActionDeleteForward,
		"ctrl+d":        ActionDeleteForward,
		"ctrl+w":        ActionDeleteWordBackward,
		"alt+backspace": ActionDeleteWordBackward,
		"alt+d":         ActionDeleteWordForward,
		"alt+delete":    ActionDeleteWordForward,
		"ctrl+u":        ActionDeleteToLineStart,
		"ctrl+k":        ActionDeleteToLineEnd,
		"ctrl+y":        ActionYank,
		"alt+y":         ActionYankPop,
		"ctrl+-":        ActionUndo,
		"tab":           ActionTab,
		"pgup":          ActionPageUp,
		"pgdown":        ActionPageDown,
		"ctrl+l":        ActionClear,
	}
}

func (m Map) ActionFor(event Event) (Action, bool) {
	for raw, action := range m {
		chord, err := ParseChord(raw)
		if err != nil || len(chord.Strokes) != 1 {
			continue
		}
		if chord.Strokes[0].Matches(event) {
			return action, true
		}
	}
	return "", false
}

func LoadFromFile(path string) (Map, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	loaded := Default()
	switch strings.ToLower(filepath.Ext(path)) {
	case ".json":
		var raw map[string]string
		if err := json.Unmarshal(data, &raw); err != nil {
			return nil, err
		}
		for chord, action := range raw {
			loaded[canonicalChordString(chord)] = Action(action)
		}
	case ".toml":
		raw, err := parseSimpleTOML(data)
		if err != nil {
			return nil, err
		}
		for chord, action := range raw {
			loaded[canonicalChordString(chord)] = Action(action)
		}
	default:
		return nil, fmt.Errorf("unsupported keybinding file %q", path)
	}
	return loaded, nil
}

func parseSimpleTOML(data []byte) (map[string]string, error) {
	out := map[string]string{}
	for lineNo, line := range strings.Split(string(data), "\n") {
		line = strings.TrimSpace(line)
		if line == "" || strings.HasPrefix(line, "#") || strings.HasPrefix(line, "[") {
			continue
		}
		key, value, ok := strings.Cut(line, "=")
		if !ok {
			return nil, fmt.Errorf("line %d: expected key = value", lineNo+1)
		}
		key = strings.Trim(strings.TrimSpace(key), `"'`)
		value = strings.Trim(strings.TrimSpace(value), `"'`)
		out[key] = value
	}
	return out, nil
}
