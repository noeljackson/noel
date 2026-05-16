package keys

import "testing"

func TestParseChordRoundTrip(t *testing.T) {
	tests := []string{
		"ctrl+c",
		"alt+enter",
		"ctrl+alt+]",
		"ctrl+x ctrl+c",
		"shift+tab",
		"pageUp",
	}
	for _, test := range tests {
		chord, err := ParseChord(test)
		if err != nil {
			t.Fatalf("ParseChord(%q): %v", test, err)
		}
		roundTrip, err := ParseChord(chord.String())
		if err != nil {
			t.Fatalf("ParseChord(%q): %v", chord.String(), err)
		}
		if roundTrip.String() != chord.String() {
			t.Fatalf("round trip = %q, want %q", roundTrip.String(), chord.String())
		}
	}
}

func TestMapActionFor(t *testing.T) {
	action, ok := Default().ActionFor(Event{Type: TypeKey, Key: KeyRune, Runes: []rune{'c'}, Modifiers: ModCtrl})
	if !ok || action != ActionAbort {
		t.Fatalf("action = %q, %v", action, ok)
	}
}
