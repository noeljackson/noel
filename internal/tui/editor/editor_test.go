package editor

import (
	"strings"
	"testing"

	"github.com/noeljackson/pi/internal/tui/keys"
)

func TestEditorTypingAndGraphemeCursorMotion(t *testing.T) {
	t.Setenv("PI_HOME", t.TempDir())
	e := New(Options{LineWrapping: true})
	e.Focus()
	e.HandleKey(runeEvent('a'))
	e.HandleKey(keys.Event{Type: keys.TypeKey, Key: keys.KeyRune, Runes: []rune("e\u0301")})
	e.HandleKey(runeEvent('b'))
	e.HandleKey(keyEvent(keys.KeyArrowLeft))
	_, col := e.Cursor()
	if col != len("ae\u0301") {
		t.Fatalf("col = %d", col)
	}
	e.HandleKey(keyEvent(keys.KeyArrowLeft))
	_, col = e.Cursor()
	if col != len("a") {
		t.Fatalf("combining grapheme col = %d", col)
	}
}

func TestEditorUndoCoalescesTyping(t *testing.T) {
	t.Setenv("PI_HOME", t.TempDir())
	e := New(Options{})
	for _, r := range "hello" {
		e.HandleKey(runeEvent(r))
	}
	e.HandleKey(keys.Event{Type: keys.TypeKey, Key: keys.KeyRune, Runes: []rune{'-'}, Modifiers: keys.ModCtrl})
	if e.Value() != "" {
		t.Fatalf("value = %q", e.Value())
	}
}

func TestEditorKillRingRotation(t *testing.T) {
	t.Setenv("PI_HOME", t.TempDir())
	e := New(Options{InitialText: "one two three"})
	e.HandleKey(keyEvent(keys.KeyHome))
	e.HandleKey(keys.Event{Type: keys.TypeKey, Key: keys.KeyRune, Runes: []rune{'k'}, Modifiers: keys.ModCtrl})
	e.SetValue("prefix ")
	e.HandleKey(keys.Event{Type: keys.TypeKey, Key: keys.KeyRune, Runes: []rune{'u'}, Modifiers: keys.ModCtrl})
	e.HandleKey(keys.Event{Type: keys.TypeKey, Key: keys.KeyRune, Runes: []rune{'k'}, Modifiers: keys.ModCtrl})
	e.HandleKey(keys.Event{Type: keys.TypeKey, Key: keys.KeyRune, Runes: []rune{'y'}, Modifiers: keys.ModCtrl})
	if e.Value() != "prefix " {
		t.Fatalf("first yank = %q", e.Value())
	}
	e.HandleKey(keys.Event{Type: keys.TypeKey, Key: keys.KeyRune, Runes: []rune{'y'}, Modifiers: keys.ModAlt})
	if !strings.Contains(e.Value(), "one two three") {
		t.Fatalf("yank-pop = %q", e.Value())
	}
}

func TestEditorHistoryNavigation(t *testing.T) {
	t.Setenv("PI_HOME", t.TempDir())
	e := New(Options{})
	e.PushHistory("first")
	e.PushHistory("second")
	if value, ok := e.PrevHistory(); !ok || value != "second" {
		t.Fatalf("prev = %q, %v", value, ok)
	}
	if value, ok := e.PrevHistory(); !ok || value != "first" {
		t.Fatalf("prev2 = %q, %v", value, ok)
	}
	if value, ok := e.NextHistory(); !ok || value != "second" {
		t.Fatalf("next = %q, %v", value, ok)
	}
}

func TestEditorBracketedPasteExpansionOnSubmit(t *testing.T) {
	t.Setenv("PI_HOME", t.TempDir())
	e := New(Options{})
	large := strings.Repeat("x", 1001)
	e.HandleKey(keys.Event{Type: keys.TypePaste, Runes: []rune(large)})
	if !strings.Contains(e.Value(), "[paste #1 1001 chars]") {
		t.Fatalf("marker = %q", e.Value())
	}
	if e.ExpandedValue() != large {
		t.Fatalf("expanded length = %d", len(e.ExpandedValue()))
	}
	_, cmd := e.HandleKey(keyEvent(keys.KeyEnter))
	if cmd != CommandSubmit {
		t.Fatalf("cmd = %q", cmd)
	}
}

func runeEvent(r rune) keys.Event {
	return keys.Event{Type: keys.TypeKey, Key: keys.KeyRune, Runes: []rune{r}}
}

func keyEvent(key keys.Key) keys.Event {
	return keys.Event{Type: keys.TypeKey, Key: key}
}
