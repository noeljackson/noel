package editor

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/noeljackson/pi/internal/tui/autocomplete"
	"github.com/noeljackson/pi/internal/tui/keys"
)

type EditorCommand string

const (
	CommandNone     EditorCommand = ""
	CommandSubmit   EditorCommand = "submit"
	CommandAbort    EditorCommand = "abort"
	CommandPageUp   EditorCommand = "page-up"
	CommandPageDown EditorCommand = "page-down"
	CommandClear    EditorCommand = "clear"
	CommandExit     EditorCommand = "exit"
)

type Options struct {
	Placeholder    string
	MaxHistorySize int
	LineWrapping   bool
	InitialText    string
	KeyMap         keys.Map
}

type Editor struct {
	placeholder    string
	maxHistorySize int
	lineWrapping   bool
	keyMap         keys.Map
	focused        bool

	lines      []string
	cursorLine int
	cursorCol  int
	lastWidth  int
	stickyCol  *int

	history      []string
	historyIndex int
	historyPath  string

	undo       []snapshot
	redo       []snapshot
	lastAction string
	lastTyped  time.Time
	killRing   killRing

	pastes       map[int]string
	pasteCounter int

	jumpMode string

	autocompleteProvider *autocomplete.CombinedProvider
	suggestions          []autocomplete.Suggestion
	selectedSuggestion   int
}

type snapshot struct {
	lines      []string
	cursorLine int
	cursorCol  int
}

func New(opts Options) *Editor {
	if opts.MaxHistorySize <= 0 {
		opts.MaxHistorySize = 100
	}
	if opts.KeyMap == nil {
		opts.KeyMap = keys.Default()
	}
	e := &Editor{
		placeholder:    opts.Placeholder,
		maxHistorySize: opts.MaxHistorySize,
		lineWrapping:   opts.LineWrapping,
		keyMap:         opts.KeyMap,
		lines:          []string{""},
		historyIndex:   -1,
		lastWidth:      80,
		pastes:         map[int]string{},
		historyPath:    defaultHistoryPath(),
	}
	e.loadHistory()
	if opts.InitialText != "" {
		e.SetValue(opts.InitialText)
	}
	return e
}

func (e *Editor) Focus() {
	e.focused = true
}

func (e *Editor) Blur() {
	e.focused = false
}

func (e *Editor) IsFocused() bool {
	return e.focused
}

func (e *Editor) SetAutocompleteProvider(provider *autocomplete.CombinedProvider) {
	e.autocompleteProvider = provider
	e.clearAutocomplete()
}

func (e *Editor) Value() string {
	return strings.Join(e.lines, "\n")
}

func (e *Editor) ExpandedValue() string {
	text := e.Value()
	for id, content := range e.pastes {
		text = strings.ReplaceAll(text, pasteMarkerPattern(id), content)
		text = replacePasteMarkerVariants(text, id, content)
	}
	return text
}

func (e *Editor) SetValue(value string) {
	e.pushUndo()
	e.lastAction = ""
	e.historyIndex = -1
	e.setValueInternal(normalizeText(value), true)
}

func (e *Editor) Reset() {
	e.lines = []string{""}
	e.cursorLine = 0
	e.cursorCol = 0
	e.historyIndex = -1
	e.undo = nil
	e.redo = nil
	e.lastAction = ""
	e.pastes = map[int]string{}
	e.pasteCounter = 0
	e.clearAutocomplete()
}

func (e *Editor) Cursor() (line, col int) {
	return e.cursorLine, e.cursorCol
}

func (e *Editor) View(width, height int) string {
	if width <= 0 {
		width = 80
	}
	if height <= 0 {
		height = 3
	}
	e.lastWidth = max(1, width-1)
	layout := e.visualLines(e.lastWidth)
	cursorVisual := e.currentVisualLine(layout)
	maxVisible := max(1, height)
	start := 0
	if cursorVisual >= maxVisible {
		start = cursorVisual - maxVisible + 1
	}
	if start+maxVisible > len(layout) {
		start = max(0, len(layout)-maxVisible)
	}
	var rendered []string
	if e.isEmpty() && e.placeholder != "" {
		rendered = append(rendered, dim(e.placeholder))
	} else {
		for i := start; i < len(layout) && len(rendered) < maxVisible; i++ {
			rendered = append(rendered, e.renderVisualLine(layout[i]))
		}
	}
	for len(rendered) < maxVisible {
		rendered = append(rendered, "")
	}
	if len(e.suggestions) > 0 {
		limit := min(5, len(e.suggestions))
		for i := 0; i < limit; i++ {
			prefix := "  "
			if i == e.selectedSuggestion {
				prefix = "> "
			}
			item := e.suggestions[i]
			detail := ""
			if item.Detail != "" {
				detail = dim("  " + item.Detail)
			}
			rendered = append(rendered, prefix+item.Label+detail)
		}
	}
	return strings.Join(rendered, "\n")
}

func (e *Editor) HandleKey(key keys.Event) (bool, EditorCommand) {
	if key.Type == keys.TypePaste {
		e.handlePaste(string(key.Runes))
		return true, CommandNone
	}
	if key.Type != keys.TypeKey {
		return false, CommandNone
	}
	if e.jumpMode != "" {
		if key.Key == keys.KeyRune && len(key.Runes) > 0 {
			e.jumpToChar(string(key.Runes[0]), e.jumpMode)
			e.jumpMode = ""
			return true, CommandNone
		}
		e.jumpMode = ""
	}
	action, ok := e.keyMap.ActionFor(key)
	if !ok && key.Key == keys.KeyRune && key.Modifiers == keys.ModShift && len(key.Runes) == 1 && key.Runes[0] == ' ' {
		e.insertText(" ", true)
		return true, CommandNone
	}
	if ok {
		return e.handleAction(action)
	}
	if key.Key == keys.KeyRune && key.Modifiers&(keys.ModCtrl|keys.ModAlt|keys.ModMeta) == 0 {
		e.insertText(string(key.Runes), true)
		return true, CommandNone
	}
	return false, CommandNone
}

func (e *Editor) PushHistory(entry string) {
	trimmed := strings.TrimSpace(entry)
	if trimmed == "" {
		return
	}
	if len(e.history) > 0 && e.history[0] == trimmed {
		return
	}
	e.history = append([]string{trimmed}, e.history...)
	if len(e.history) > e.maxHistorySize {
		e.history = e.history[:e.maxHistorySize]
	}
	e.persistHistory()
}

func (e *Editor) PrevHistory() (string, bool) {
	if len(e.history) == 0 || e.historyIndex+1 >= len(e.history) {
		return "", false
	}
	if e.historyIndex == -1 {
		e.pushUndo()
	}
	e.historyIndex++
	value := e.history[e.historyIndex]
	e.setValueInternal(value, false)
	return value, true
}

func (e *Editor) NextHistory() (string, bool) {
	if e.historyIndex < 0 {
		return "", false
	}
	e.historyIndex--
	if e.historyIndex < 0 {
		e.setValueInternal("", false)
		return "", true
	}
	value := e.history[e.historyIndex]
	e.setValueInternal(value, false)
	return value, true
}

func (e *Editor) handleAction(action keys.Action) (bool, EditorCommand) {
	if len(e.suggestions) > 0 {
		switch action {
		case keys.ActionAbort:
			e.clearAutocomplete()
			return true, CommandNone
		case keys.ActionCursorUp:
			e.selectedSuggestion = max(0, e.selectedSuggestion-1)
			return true, CommandNone
		case keys.ActionCursorDown:
			e.selectedSuggestion = min(len(e.suggestions)-1, e.selectedSuggestion+1)
			return true, CommandNone
		case keys.ActionTab, keys.ActionSubmit:
			e.applySuggestion()
			if action == keys.ActionSubmit {
				return true, CommandNone
			}
			return true, CommandNone
		}
	}

	switch action {
	case keys.ActionSubmit:
		return true, CommandSubmit
	case keys.ActionNewline:
		e.addNewline()
	case keys.ActionAbort:
		return false, CommandAbort
	case keys.ActionCursorUp:
		if e.isEmpty() || (e.historyIndex >= 0 && e.onFirstVisualLine()) {
			e.PrevHistory()
		} else if e.onFirstVisualLine() {
			e.moveLineStart()
		} else {
			e.moveVertical(-1)
		}
	case keys.ActionCursorDown:
		if e.historyIndex >= 0 && e.onLastVisualLine() {
			e.NextHistory()
		} else if e.onLastVisualLine() {
			e.moveLineEnd()
		} else {
			e.moveVertical(1)
		}
	case keys.ActionCursorLeft:
		e.moveHorizontal(-1)
	case keys.ActionCursorRight:
		e.moveHorizontal(1)
	case keys.ActionCursorWordLeft:
		e.moveWordBackward()
	case keys.ActionCursorWordRight:
		e.moveWordForward()
	case keys.ActionCursorLineStart:
		e.moveLineStart()
	case keys.ActionCursorLineEnd:
		e.moveLineEnd()
	case keys.ActionJumpForward:
		e.jumpMode = "forward"
	case keys.ActionJumpBackward:
		e.jumpMode = "backward"
	case keys.ActionPageUp:
		e.moveVertical(-max(5, e.lastWidth/10))
		return true, CommandPageUp
	case keys.ActionPageDown:
		e.moveVertical(max(5, e.lastWidth/10))
		return true, CommandPageDown
	case keys.ActionDeleteBackward:
		e.deleteBackward()
	case keys.ActionDeleteForward:
		e.deleteForward()
	case keys.ActionDeleteWordBackward:
		e.deleteWordBackward()
	case keys.ActionDeleteWordForward:
		e.deleteWordForward()
	case keys.ActionDeleteToLineStart:
		e.deleteToLineStart()
	case keys.ActionDeleteToLineEnd:
		e.deleteToLineEnd()
	case keys.ActionYank:
		e.yank()
	case keys.ActionYankPop:
		e.yankPop()
	case keys.ActionUndo:
		e.undoOnce()
	case keys.ActionTab:
		e.updateAutocomplete(true)
	case keys.ActionClear:
		return false, CommandClear
	case keys.ActionExit:
		return false, CommandExit
	default:
		return false, CommandNone
	}
	return true, CommandNone
}

func (e *Editor) setValueInternal(value string, resetHistory bool) {
	lines := strings.Split(value, "\n")
	if len(lines) == 0 {
		lines = []string{""}
	}
	e.lines = lines
	e.cursorLine = len(lines) - 1
	e.cursorCol = len(lines[len(lines)-1])
	e.clampCursor()
	if resetHistory {
		e.historyIndex = -1
	}
	e.clearAutocomplete()
}

func (e *Editor) insertText(text string, coalesce bool) {
	if text == "" {
		return
	}
	e.historyIndex = -1
	now := time.Now()
	if !coalesce || e.lastAction != "type" || now.Sub(e.lastTyped) > 500*time.Millisecond || isWhitespaceString(text) {
		e.pushUndo()
	}
	e.lastAction = "type"
	e.lastTyped = now
	e.insertTextInternal(normalizeText(text))
	e.updateAutocomplete(false)
}

func (e *Editor) insertTextInternal(text string) {
	parts := strings.Split(text, "\n")
	line := e.lines[e.cursorLine]
	before := line[:e.cursorCol]
	after := line[e.cursorCol:]
	if len(parts) == 1 {
		e.lines[e.cursorLine] = before + text + after
		e.cursorCol += len(text)
		return
	}
	next := make([]string, 0, len(e.lines)+len(parts)-1)
	next = append(next, e.lines[:e.cursorLine]...)
	next = append(next, before+parts[0])
	next = append(next, parts[1:len(parts)-1]...)
	next = append(next, parts[len(parts)-1]+after)
	next = append(next, e.lines[e.cursorLine+1:]...)
	e.lines = next
	e.cursorLine += len(parts) - 1
	e.cursorCol = len(parts[len(parts)-1])
}

func (e *Editor) addNewline() {
	e.pushUndo()
	e.lastAction = ""
	line := e.lines[e.cursorLine]
	before := line[:e.cursorCol]
	after := line[e.cursorCol:]
	e.lines[e.cursorLine] = before
	e.lines = append(e.lines[:e.cursorLine+1], append([]string{after}, e.lines[e.cursorLine+1:]...)...)
	e.cursorLine++
	e.cursorCol = 0
	e.clearAutocomplete()
}

func (e *Editor) handlePaste(text string) {
	text = strings.Map(func(r rune) rune {
		if r == '\n' || r >= 32 {
			return r
		}
		return -1
	}, normalizeText(text))
	if text == "" {
		return
	}
	e.pushUndo()
	e.lastAction = ""
	lines := strings.Split(text, "\n")
	if len(lines) > 10 || len(text) > 1000 {
		e.pasteCounter++
		id := e.pasteCounter
		e.pastes[id] = text
		marker := fmt.Sprintf("[paste #%d %d chars]", id, len(text))
		if len(lines) > 10 {
			marker = fmt.Sprintf("[paste #%d +%d lines]", id, len(lines))
		}
		e.insertTextInternal(marker)
		return
	}
	e.insertTextInternal(text)
}

func (e *Editor) deleteBackward() {
	e.historyIndex = -1
	e.lastAction = ""
	if e.cursorCol > 0 {
		e.pushUndo()
		line := e.lines[e.cursorLine]
		start := previousGraphemeStart(line, e.cursorCol, e.validPasteIDs())
		e.lines[e.cursorLine] = line[:start] + line[e.cursorCol:]
		e.cursorCol = start
	} else if e.cursorLine > 0 {
		e.pushUndo()
		prevLen := len(e.lines[e.cursorLine-1])
		e.lines[e.cursorLine-1] += e.lines[e.cursorLine]
		e.lines = append(e.lines[:e.cursorLine], e.lines[e.cursorLine+1:]...)
		e.cursorLine--
		e.cursorCol = prevLen
	}
	e.updateAutocomplete(false)
}

func (e *Editor) deleteForward() {
	e.historyIndex = -1
	e.lastAction = ""
	line := e.lines[e.cursorLine]
	if e.cursorCol < len(line) {
		e.pushUndo()
		end := nextGraphemeEnd(line, e.cursorCol, e.validPasteIDs())
		e.lines[e.cursorLine] = line[:e.cursorCol] + line[end:]
	} else if e.cursorLine < len(e.lines)-1 {
		e.pushUndo()
		e.lines[e.cursorLine] += e.lines[e.cursorLine+1]
		e.lines = append(e.lines[:e.cursorLine+1], e.lines[e.cursorLine+2:]...)
	}
	e.updateAutocomplete(false)
}

func (e *Editor) deleteToLineStart() {
	line := e.lines[e.cursorLine]
	if e.cursorCol == 0 && e.cursorLine == 0 {
		return
	}
	e.pushUndo()
	if e.cursorCol > 0 {
		deleted := line[:e.cursorCol]
		e.killRing.push(deleted, true, e.lastAction == "kill")
		e.lines[e.cursorLine] = line[e.cursorCol:]
		e.cursorCol = 0
	} else {
		e.killRing.push("\n", true, e.lastAction == "kill")
		prevLen := len(e.lines[e.cursorLine-1])
		e.lines[e.cursorLine-1] += line
		e.lines = append(e.lines[:e.cursorLine], e.lines[e.cursorLine+1:]...)
		e.cursorLine--
		e.cursorCol = prevLen
	}
	e.lastAction = "kill"
}

func (e *Editor) deleteToLineEnd() {
	line := e.lines[e.cursorLine]
	if e.cursorCol == len(line) && e.cursorLine == len(e.lines)-1 {
		return
	}
	e.pushUndo()
	if e.cursorCol < len(line) {
		deleted := line[e.cursorCol:]
		e.killRing.push(deleted, false, e.lastAction == "kill")
		e.lines[e.cursorLine] = line[:e.cursorCol]
	} else {
		e.killRing.push("\n", false, e.lastAction == "kill")
		e.lines[e.cursorLine] += e.lines[e.cursorLine+1]
		e.lines = append(e.lines[:e.cursorLine+1], e.lines[e.cursorLine+2:]...)
	}
	e.lastAction = "kill"
}

func (e *Editor) deleteWordBackward() {
	if e.cursorCol == 0 {
		e.deleteBackward()
		return
	}
	old := e.cursorCol
	e.moveWordBackward()
	start := e.cursorCol
	e.cursorCol = old
	e.pushUndo()
	line := e.lines[e.cursorLine]
	e.killRing.push(line[start:old], true, e.lastAction == "kill")
	e.lines[e.cursorLine] = line[:start] + line[old:]
	e.cursorCol = start
	e.lastAction = "kill"
}

func (e *Editor) deleteWordForward() {
	line := e.lines[e.cursorLine]
	if e.cursorCol >= len(line) {
		e.deleteForward()
		return
	}
	start := e.cursorCol
	e.moveWordForward()
	end := e.cursorCol
	e.cursorCol = start
	e.pushUndo()
	e.killRing.push(line[start:end], false, e.lastAction == "kill")
	e.lines[e.cursorLine] = line[:start] + line[end:]
	e.lastAction = "kill"
}

func (e *Editor) moveHorizontal(delta int) {
	e.lastAction = ""
	e.stickyCol = nil
	line := e.lines[e.cursorLine]
	if delta > 0 {
		if e.cursorCol < len(line) {
			e.cursorCol = nextGraphemeEnd(line, e.cursorCol, e.validPasteIDs())
		} else if e.cursorLine < len(e.lines)-1 {
			e.cursorLine++
			e.cursorCol = 0
		}
	} else {
		if e.cursorCol > 0 {
			e.cursorCol = previousGraphemeStart(line, e.cursorCol, e.validPasteIDs())
		} else if e.cursorLine > 0 {
			e.cursorLine--
			e.cursorCol = len(e.lines[e.cursorLine])
		}
	}
}

func (e *Editor) moveVertical(delta int) {
	e.lastAction = ""
	visual := e.visualLines(e.lastWidth)
	current := e.currentVisualLine(visual)
	target := clamp(current+delta, 0, len(visual)-1)
	if current == target || len(visual) == 0 {
		return
	}
	source := visual[current]
	col := e.cursorCol - source.startCol
	if e.stickyCol != nil {
		col = *e.stickyCol
	}
	targetLine := visual[target]
	if col > targetLine.length {
		sticky := col
		e.stickyCol = &sticky
		col = targetLine.length
	} else {
		e.stickyCol = nil
	}
	e.cursorLine = targetLine.logicalLine
	e.cursorCol = min(len(e.lines[e.cursorLine]), targetLine.startCol+col)
	e.snapCursorToGrapheme()
}

func (e *Editor) moveLineStart() {
	e.lastAction = ""
	e.stickyCol = nil
	e.cursorCol = 0
}

func (e *Editor) moveLineEnd() {
	e.lastAction = ""
	e.stickyCol = nil
	e.cursorCol = len(e.lines[e.cursorLine])
}

func (e *Editor) moveWordBackward() {
	e.lastAction = ""
	if e.cursorCol == 0 {
		if e.cursorLine > 0 {
			e.cursorLine--
			e.cursorCol = len(e.lines[e.cursorLine])
		}
		return
	}
	line := e.lines[e.cursorLine]
	spans := graphemeSpans(line[:e.cursorCol], e.validPasteIDs())
	i := len(spans) - 1
	for i >= 0 && isWhitespace(spans[i].text) {
		i--
	}
	if i >= 0 && isPunctuation(spans[i].text) {
		for i >= 0 && isPunctuation(spans[i].text) {
			i--
		}
	} else {
		for i >= 0 && !isWhitespace(spans[i].text) && !isPunctuation(spans[i].text) {
			i--
		}
	}
	if i+1 < len(spans) {
		e.cursorCol = spans[i+1].start
	} else {
		e.cursorCol = 0
	}
}

func (e *Editor) moveWordForward() {
	e.lastAction = ""
	line := e.lines[e.cursorLine]
	if e.cursorCol >= len(line) {
		if e.cursorLine < len(e.lines)-1 {
			e.cursorLine++
			e.cursorCol = 0
		}
		return
	}
	spans := graphemeSpans(line[e.cursorCol:], e.validPasteIDs())
	offset := e.cursorCol
	i := 0
	for i < len(spans) && isWhitespace(spans[i].text) {
		i++
	}
	if i < len(spans) && isPunctuation(spans[i].text) {
		for i < len(spans) && isPunctuation(spans[i].text) {
			i++
		}
	} else {
		for i < len(spans) && !isWhitespace(spans[i].text) && !isPunctuation(spans[i].text) {
			i++
		}
	}
	if i == 0 {
		return
	}
	e.cursorCol = offset + spans[i-1].end
}

func (e *Editor) yank() {
	text, ok := e.killRing.peek()
	if !ok {
		return
	}
	e.pushUndo()
	e.insertTextInternal(text)
	e.lastAction = "yank"
}

func (e *Editor) yankPop() {
	if e.lastAction != "yank" || e.killRing.len() <= 1 {
		return
	}
	current, ok := e.killRing.peek()
	if !ok {
		return
	}
	e.deleteTextBackward(current)
	e.killRing.rotate()
	next, _ := e.killRing.peek()
	e.insertTextInternal(next)
	e.lastAction = "yank"
}

func (e *Editor) deleteTextBackward(text string) {
	lines := strings.Split(text, "\n")
	if len(lines) == 1 {
		line := e.lines[e.cursorLine]
		start := max(0, e.cursorCol-len(text))
		e.lines[e.cursorLine] = line[:start] + line[e.cursorCol:]
		e.cursorCol = start
		return
	}
	startLine := e.cursorLine - (len(lines) - 1)
	if startLine < 0 {
		return
	}
	startCol := len(e.lines[startLine]) - len(lines[0])
	after := e.lines[e.cursorLine][e.cursorCol:]
	before := e.lines[startLine][:startCol]
	e.lines = append(e.lines[:startLine], append([]string{before + after}, e.lines[e.cursorLine+1:]...)...)
	e.cursorLine = startLine
	e.cursorCol = startCol
}

func (e *Editor) undoOnce() {
	if len(e.undo) == 0 {
		return
	}
	e.redo = append(e.redo, e.snapshot())
	last := e.undo[len(e.undo)-1]
	e.undo = e.undo[:len(e.undo)-1]
	e.restore(last)
	e.lastAction = ""
}

func (e *Editor) pushUndo() {
	e.undo = append(e.undo, e.snapshot())
	if len(e.undo) > 1000 {
		e.undo = e.undo[len(e.undo)-1000:]
	}
	e.redo = nil
}

func (e *Editor) snapshot() snapshot {
	lines := append([]string(nil), e.lines...)
	return snapshot{lines: lines, cursorLine: e.cursorLine, cursorCol: e.cursorCol}
}

func (e *Editor) restore(s snapshot) {
	e.lines = append([]string(nil), s.lines...)
	e.cursorLine = s.cursorLine
	e.cursorCol = s.cursorCol
	e.clampCursor()
	e.clearAutocomplete()
}

func (e *Editor) jumpToChar(char string, mode string) {
	if char == "" {
		return
	}
	if mode == "forward" {
		for lineIdx := e.cursorLine; lineIdx < len(e.lines); lineIdx++ {
			line := e.lines[lineIdx]
			start := 0
			if lineIdx == e.cursorLine {
				start = min(len(line), e.cursorCol+1)
			}
			if idx := strings.Index(line[start:], char); idx >= 0 {
				e.cursorLine = lineIdx
				e.cursorCol = start + idx
				return
			}
		}
		return
	}
	for lineIdx := e.cursorLine; lineIdx >= 0; lineIdx-- {
		line := e.lines[lineIdx]
		end := len(line)
		if lineIdx == e.cursorLine {
			end = max(0, e.cursorCol-1)
		}
		if idx := strings.LastIndex(line[:end], char); idx >= 0 {
			e.cursorLine = lineIdx
			e.cursorCol = idx
			return
		}
	}
}

func (e *Editor) updateAutocomplete(force bool) {
	if e.autocompleteProvider == nil {
		return
	}
	input := e.Value()
	cursor := e.absoluteCursor()
	if force {
		if len(e.autocompleteProvider.Providers) > 0 {
			for _, provider := range e.autocompleteProvider.Providers {
				if fileProvider, ok := provider.(*autocomplete.FileProvider); ok {
					e.suggestions = fileProvider.ForceSuggestions(input, cursor)
					e.selectedSuggestion = 0
					return
				}
			}
		}
	}
	e.suggestions = e.autocompleteProvider.Suggestions(context.Background(), input, cursor)
	e.selectedSuggestion = 0
}

func (e *Editor) applySuggestion() {
	if len(e.suggestions) == 0 {
		return
	}
	e.pushUndo()
	input, cursor := autocomplete.Apply(e.Value(), e.suggestions[e.selectedSuggestion])
	e.lines = strings.Split(input, "\n")
	e.setAbsoluteCursor(cursor)
	e.clearAutocomplete()
}

func (e *Editor) clearAutocomplete() {
	e.suggestions = nil
	e.selectedSuggestion = 0
}

func (e *Editor) absoluteCursor() int {
	pos := 0
	for i := 0; i < e.cursorLine; i++ {
		pos += len(e.lines[i]) + 1
	}
	return pos + e.cursorCol
}

func (e *Editor) setAbsoluteCursor(pos int) {
	pos = clamp(pos, 0, len(e.Value()))
	for i, line := range e.lines {
		if pos <= len(line) {
			e.cursorLine = i
			e.cursorCol = pos
			return
		}
		pos -= len(line) + 1
	}
	e.cursorLine = len(e.lines) - 1
	e.cursorCol = len(e.lines[e.cursorLine])
}

func (e *Editor) isEmpty() bool {
	return len(e.lines) == 1 && e.lines[0] == ""
}

func (e *Editor) onFirstVisualLine() bool {
	return e.currentVisualLine(e.visualLines(e.lastWidth)) == 0
}

func (e *Editor) onLastVisualLine() bool {
	visual := e.visualLines(e.lastWidth)
	return e.currentVisualLine(visual) == len(visual)-1
}

func (e *Editor) validPasteIDs() map[int]bool {
	ids := map[int]bool{}
	for id := range e.pastes {
		ids[id] = true
	}
	return ids
}

func (e *Editor) clampCursor() {
	if len(e.lines) == 0 {
		e.lines = []string{""}
	}
	e.cursorLine = clamp(e.cursorLine, 0, len(e.lines)-1)
	e.cursorCol = clamp(e.cursorCol, 0, len(e.lines[e.cursorLine]))
}

func (e *Editor) snapCursorToGrapheme() {
	line := e.lines[e.cursorLine]
	for _, span := range graphemeSpans(line, e.validPasteIDs()) {
		if e.cursorCol > span.start && e.cursorCol < span.end {
			e.cursorCol = span.start
			return
		}
	}
}

func normalizeText(text string) string {
	text = strings.ReplaceAll(text, "\r\n", "\n")
	text = strings.ReplaceAll(text, "\r", "\n")
	text = strings.ReplaceAll(text, "\t", "    ")
	return text
}

func isWhitespaceString(text string) bool {
	for _, r := range text {
		if !unicode.IsSpace(r) {
			return false
		}
	}
	return text != ""
}

func defaultHistoryPath() string {
	base := os.Getenv("PI_HOME")
	if base == "" {
		base = os.Getenv("PI_AGENT_DIR")
	}
	if base == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return ""
		}
		base = filepath.Join(home, ".pi")
	}
	return filepath.Join(base, "history")
}

func (e *Editor) loadHistory() {
	if e.historyPath == "" {
		return
	}
	data, err := os.ReadFile(e.historyPath)
	if err != nil {
		return
	}
	for _, line := range strings.Split(string(data), "\n") {
		line = strings.TrimSpace(line)
		if line != "" {
			e.history = append(e.history, line)
		}
	}
	if len(e.history) > e.maxHistorySize {
		e.history = e.history[:e.maxHistorySize]
	}
}

func (e *Editor) persistHistory() {
	if e.historyPath == "" {
		return
	}
	if err := os.MkdirAll(filepath.Dir(e.historyPath), 0o700); err != nil {
		return
	}
	_ = os.WriteFile(e.historyPath, []byte(strings.Join(e.history, "\n")+"\n"), 0o600)
}

func dim(text string) string {
	return "\x1b[2m" + text + "\x1b[0m"
}

func pasteMarkerPattern(id int) string {
	return fmt.Sprintf("[paste #%d]", id)
}

func replacePasteMarkerVariants(text string, id int, content string) string {
	prefix := fmt.Sprintf("[paste #%d ", id)
	for {
		start := strings.Index(text, prefix)
		if start < 0 {
			return text
		}
		end := strings.Index(text[start:], "]")
		if end < 0 {
			return text
		}
		text = text[:start] + content + text[start+end+1:]
	}
}

func max(a, b int) int {
	if a > b {
		return a
	}
	return b
}

func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}

func clamp(value, low, high int) int {
	if high < low {
		return low
	}
	if value < low {
		return low
	}
	if value > high {
		return high
	}
	return value
}

var _ = utf8.RuneSelf
