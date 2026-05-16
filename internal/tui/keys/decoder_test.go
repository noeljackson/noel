package keys

import "testing"

func TestDecoderRegularCSIKeys(t *testing.T) {
	events := NewDecoder().Feed([]byte("\x1b[A\x1b[3~"))
	if len(events) != 2 {
		t.Fatalf("events = %#v", events)
	}
	if events[0].Key != KeyArrowUp || events[1].Key != KeyDelete {
		t.Fatalf("events = %#v", events)
	}
}

func TestDecoderBracketedPaste(t *testing.T) {
	events := NewDecoder().Feed([]byte("\x1b[200~hello\nworld\x1b[201~"))
	if len(events) != 2 {
		t.Fatalf("events = %#v", events)
	}
	if events[0].Type != TypeBracketedPasteStart {
		t.Fatalf("start = %#v", events[0])
	}
	if events[1].Type != TypePaste || string(events[1].Runes) != "hello\nworld" {
		t.Fatalf("paste = %#v", events[1])
	}
}

func TestDecoderSGRMouse(t *testing.T) {
	events := NewDecoder().Feed([]byte("\x1b[<35;20;5m"))
	if len(events) != 1 {
		t.Fatalf("events = %#v", events)
	}
	if events[0].Type != TypeMouse || !events[0].Mouse.Release || events[0].Mouse.X != 19 || events[0].Mouse.Y != 4 {
		t.Fatalf("mouse = %#v", events[0])
	}
}

func TestDecoderKittyProtocol(t *testing.T) {
	events := NewDecoder().Feed([]byte("\x1b[97;5u"))
	if len(events) != 1 {
		t.Fatalf("events = %#v", events)
	}
	if events[0].Key != KeyRune || string(events[0].Runes) != "a" || events[0].Modifiers != ModCtrl {
		t.Fatalf("kitty = %#v", events[0])
	}
}

func TestDecoderModifyOtherKeys(t *testing.T) {
	events := NewDecoder().Feed([]byte("\x1b[27;3;13~"))
	if len(events) != 1 {
		t.Fatalf("events = %#v", events)
	}
	if events[0].Key != KeyEnter || events[0].Modifiers != ModAlt {
		t.Fatalf("modifyOtherKeys = %#v", events[0])
	}
}
