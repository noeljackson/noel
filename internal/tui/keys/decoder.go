package keys

import (
	"strconv"
	"strings"
	"unicode/utf8"
)

const (
	esc               = "\x1b"
	bracketPasteStart = "\x1b[200~"
	bracketPasteEnd   = "\x1b[201~"
	lockModifierMask  = 64 + 128
	xtermModShift     = 1
	xtermModAlt       = 2
	xtermModCtrl      = 4
	xtermModMeta      = 8
)

// The app maps Bubble Tea KeyMsg/MouseMsg to Event because Bubble Tea already
// owns terminal input on the main program path. Decoder remains for tests and
// raw-input callers; it mirrors the TS stdin buffer for CSI, mouse, Kitty,
// modifyOtherKeys, bracketed paste, and high-bit Alt bytes.
type Decoder struct {
	buffer      string
	pasteMode   bool
	pasteBuffer string
}

func NewDecoder() *Decoder {
	return &Decoder{}
}

func (d *Decoder) Feed(b []byte) []Event {
	if len(b) == 1 && b[0] > 127 {
		b = []byte{0x1b, b[0] - 128}
	}
	d.buffer += string(b)
	var out []Event
	for {
		if d.pasteMode {
			end := strings.Index(d.buffer, bracketPasteEnd)
			if end < 0 {
				d.pasteBuffer += d.buffer
				d.buffer = ""
				return out
			}
			d.pasteBuffer += d.buffer[:end]
			out = append(out, Event{Type: TypePaste, Key: KeyRune, Runes: []rune(d.pasteBuffer), Raw: d.pasteBuffer})
			d.buffer = d.buffer[end+len(bracketPasteEnd):]
			d.pasteMode = false
			d.pasteBuffer = ""
			continue
		}

		start := strings.Index(d.buffer, bracketPasteStart)
		if start == 0 {
			out = append(out, Event{Type: TypeBracketedPasteStart, Raw: bracketPasteStart})
			d.buffer = d.buffer[len(bracketPasteStart):]
			d.pasteMode = true
			continue
		}
		if start > 0 {
			before := d.buffer[:start]
			d.buffer = d.buffer[start:]
			out = append(out, decodePlainRunes(before)...)
			continue
		}

		if d.buffer == "" {
			return out
		}
		seq, ok := takeSequence(d.buffer)
		if !ok {
			return out
		}
		d.buffer = d.buffer[len(seq):]
		out = append(out, parseSequence(seq)...)
	}
}

func decodePlainRunes(s string) []Event {
	var events []Event
	for s != "" {
		r, size := utf8.DecodeRuneInString(s)
		if r == utf8.RuneError && size == 0 {
			break
		}
		events = append(events, parseSequence(s[:size])...)
		s = s[size:]
	}
	return events
}

func takeSequence(buffer string) (string, bool) {
	if !strings.HasPrefix(buffer, esc) {
		_, size := utf8.DecodeRuneInString(buffer)
		if size == 0 {
			return "", false
		}
		return buffer[:size], true
	}
	if len(buffer) == 1 {
		return "", false
	}
	after := buffer[1:]
	switch {
	case strings.HasPrefix(after, "[M"):
		if len(buffer) < 6 {
			return "", false
		}
		return buffer[:6], true
	case strings.HasPrefix(after, "["):
		for i := 2; i < len(buffer); i++ {
			c := buffer[i]
			if c >= 0x40 && c <= 0x7e {
				return buffer[:i+1], true
			}
		}
		return "", false
	case strings.HasPrefix(after, "]"):
		return takeSTTerminated(buffer)
	case strings.HasPrefix(after, "P"), strings.HasPrefix(after, "_"):
		return takeEscBackslashTerminated(buffer)
	case strings.HasPrefix(after, "O"):
		if len(buffer) < 3 {
			return "", false
		}
		return buffer[:3], true
	default:
		_, size := utf8.DecodeRuneInString(after)
		if size == 0 {
			return "", false
		}
		return buffer[:1+size], true
	}
}

func takeSTTerminated(buffer string) (string, bool) {
	if idx := strings.Index(buffer, "\x07"); idx >= 0 {
		return buffer[:idx+1], true
	}
	return takeEscBackslashTerminated(buffer)
}

func takeEscBackslashTerminated(buffer string) (string, bool) {
	if idx := strings.Index(buffer, "\x1b\\"); idx >= 0 {
		return buffer[:idx+2], true
	}
	return "", false
}

func parseSequence(seq string) []Event {
	if seq == "" {
		return nil
	}
	if strings.HasPrefix(seq, "\x1b[M") && len(seq) >= 6 {
		return []Event{oldMouseEvent(seq)}
	}
	if strings.HasPrefix(seq, "\x1b[<") {
		if ev, ok := sgrMouseEvent(seq); ok {
			return []Event{ev}
		}
	}
	if ev, ok := parseKittyOrCSI(seq); ok {
		return []Event{ev}
	}
	if ev, ok := parseLegacy(seq); ok {
		return []Event{ev}
	}
	if strings.HasPrefix(seq, esc) && len(seq) > 1 {
		events := parseSequence(seq[1:])
		for i := range events {
			events[i].Modifiers |= ModAlt
			events[i].Raw = seq
		}
		return events
	}
	runes := []rune(seq)
	if len(runes) == 1 && runes[0] >= 32 {
		return []Event{{Type: TypeKey, Key: KeyRune, Runes: runes, Raw: seq}}
	}
	return []Event{{Type: TypeKey, Raw: seq}}
}

func parseLegacy(seq string) (Event, bool) {
	key := KeyRune
	var mods Modifier
	switch seq {
	case "\x1b":
		key = KeyEscape
	case "\r", "\n", "\x1bOM":
		key = KeyEnter
	case "\t":
		key = KeyTab
	case "\x7f", "\b":
		key = KeyBackspace
	case "\x1b[Z":
		key = KeyTab
		mods = ModShift
	case "\x1b[A", "\x1bOA":
		key = KeyArrowUp
	case "\x1b[B", "\x1bOB":
		key = KeyArrowDown
	case "\x1b[C", "\x1bOC":
		key = KeyArrowRight
	case "\x1b[D", "\x1bOD":
		key = KeyArrowLeft
	case "\x1b[H", "\x1bOH", "\x1b[1~", "\x1b[7~":
		key = KeyHome
	case "\x1b[F", "\x1bOF", "\x1b[4~", "\x1b[8~":
		key = KeyEnd
	case "\x1b[2~":
		key = KeyInsert
	case "\x1b[3~":
		key = KeyDelete
	case "\x1b[5~", "\x1b[[5~":
		key = KeyPageUp
	case "\x1b[6~", "\x1b[[6~":
		key = KeyPageDown
	case "\x1bOP", "\x1b[11~", "\x1b[[A":
		key = KeyF1
	case "\x1bOQ", "\x1b[12~", "\x1b[[B":
		key = KeyF2
	case "\x1bOR", "\x1b[13~", "\x1b[[C":
		key = KeyF3
	case "\x1bOS", "\x1b[14~", "\x1b[[D":
		key = KeyF4
	default:
		if len(seq) == 1 {
			c := seq[0]
			if c >= 1 && c <= 26 {
				return Event{Type: TypeKey, Key: KeyRune, Runes: []rune{rune('a' + c - 1)}, Modifiers: ModCtrl, Raw: seq}, true
			}
		}
		return Event{}, false
	}
	return Event{Type: TypeKey, Key: key, Modifiers: mods, Raw: seq}, true
}

func parseKittyOrCSI(seq string) (Event, bool) {
	if strings.HasPrefix(seq, "\x1b[27;") && strings.HasSuffix(seq, "~") {
		body := strings.TrimSuffix(strings.TrimPrefix(seq, "\x1b["), "~")
		parts := strings.Split(body, ";")
		if len(parts) == 3 && parts[0] == "27" {
			modValue, err1 := strconv.Atoi(parts[1])
			cp, err2 := strconv.Atoi(parts[2])
			if err1 == nil && err2 == nil {
				return eventFromCodepoint(cp, modifierFromXterm(modValue-1), seq), true
			}
		}
	}
	if strings.HasSuffix(seq, "u") && strings.HasPrefix(seq, "\x1b[") {
		body := strings.TrimSuffix(strings.TrimPrefix(seq, "\x1b["), "u")
		main, modValue := splitModifier(body)
		segments := strings.Split(main, ":")
		cp, err := strconv.Atoi(segments[0])
		if err != nil {
			return Event{}, false
		}
		shifted := 0
		if len(segments) > 1 && segments[1] != "" {
			shifted, _ = strconv.Atoi(segments[1])
		}
		mod := modifierFromXterm(modValue - 1)
		if mod&ModShift != 0 && shifted >= 32 {
			cp = shifted
		}
		return eventFromCodepoint(normalizeKittyCodepoint(cp), mod, seq), true
	}
	if strings.HasPrefix(seq, "\x1b[1;") {
		final := seq[len(seq)-1]
		if strings.Contains("ABCDHF", string(final)) {
			body := seq[len("\x1b[1;") : len(seq)-1]
			body = strings.Split(body, ":")[0]
			modValue, err := strconv.Atoi(body)
			if err != nil {
				return Event{}, false
			}
			ev := Event{Type: TypeKey, Modifiers: modifierFromXterm(modValue - 1), Raw: seq}
			switch final {
			case 'A':
				ev.Key = KeyArrowUp
			case 'B':
				ev.Key = KeyArrowDown
			case 'C':
				ev.Key = KeyArrowRight
			case 'D':
				ev.Key = KeyArrowLeft
			case 'H':
				ev.Key = KeyHome
			case 'F':
				ev.Key = KeyEnd
			}
			return ev, true
		}
	}
	if strings.HasPrefix(seq, "\x1b[") && strings.HasSuffix(seq, "~") {
		body := strings.TrimSuffix(strings.TrimPrefix(seq, "\x1b["), "~")
		parts := strings.Split(body, ";")
		num, err := strconv.Atoi(parts[0])
		if err != nil {
			return Event{}, false
		}
		mod := ModNone
		if len(parts) > 1 {
			eventParts := strings.Split(parts[1], ":")
			modValue, _ := strconv.Atoi(eventParts[0])
			mod = modifierFromXterm(modValue - 1)
		}
		ev := Event{Type: TypeKey, Modifiers: mod, Raw: seq}
		switch num {
		case 2:
			ev.Key = KeyInsert
		case 3:
			ev.Key = KeyDelete
		case 5:
			ev.Key = KeyPageUp
		case 6:
			ev.Key = KeyPageDown
		case 7:
			ev.Key = KeyHome
		case 8:
			ev.Key = KeyEnd
		case 15:
			ev.Key = KeyF5
		case 17:
			ev.Key = KeyF6
		case 18:
			ev.Key = KeyF7
		case 19:
			ev.Key = KeyF8
		case 20:
			ev.Key = KeyF9
		case 21:
			ev.Key = KeyF10
		case 23:
			ev.Key = KeyF11
		case 24:
			ev.Key = KeyF12
		default:
			return Event{}, false
		}
		return ev, true
	}
	return Event{}, false
}

func splitModifier(body string) (string, int) {
	if i := strings.LastIndex(body, ";"); i >= 0 {
		modPart := strings.Split(body[i+1:], ":")[0]
		modValue, err := strconv.Atoi(modPart)
		if err == nil {
			return body[:i], modValue
		}
	}
	return body, 1
}

func eventFromCodepoint(cp int, mod Modifier, raw string) Event {
	ev := Event{Type: TypeKey, Modifiers: mod, Raw: raw}
	switch cp {
	case 9:
		ev.Key = KeyTab
	case 13, 57414:
		ev.Key = KeyEnter
	case 27:
		ev.Key = KeyEscape
	case 32:
		ev.Key = KeyRune
		ev.Runes = []rune{' '}
	case 127:
		ev.Key = KeyBackspace
	case -1:
		ev.Key = KeyArrowUp
	case -2:
		ev.Key = KeyArrowDown
	case -3:
		ev.Key = KeyArrowRight
	case -4:
		ev.Key = KeyArrowLeft
	case -10:
		ev.Key = KeyDelete
	case -11:
		ev.Key = KeyInsert
	case -12:
		ev.Key = KeyPageUp
	case -13:
		ev.Key = KeyPageDown
	case -14:
		ev.Key = KeyHome
	case -15:
		ev.Key = KeyEnd
	default:
		ev.Key = KeyRune
		ev.Runes = []rune{rune(cp)}
	}
	if ev.Key == KeyRune && len(ev.Runes) == 1 && ev.Runes[0] >= 'A' && ev.Runes[0] <= 'Z' && mod&ModShift != 0 {
		ev.Runes[0] += 'a' - 'A'
	}
	return ev
}

func normalizeKittyCodepoint(cp int) int {
	switch cp {
	case 57417:
		return -4
	case 57418:
		return -3
	case 57419:
		return -1
	case 57420:
		return -2
	case 57421:
		return -12
	case 57422:
		return -13
	case 57423:
		return -14
	case 57424:
		return -15
	case 57425:
		return -11
	case 57426:
		return -10
	}
	if cp >= 57399 && cp <= 57408 {
		return cp - 57399 + '0'
	}
	return cp
}

func modifierFromXterm(mask int) Modifier {
	mask &^= lockModifierMask
	var mod Modifier
	if mask&xtermModShift != 0 {
		mod |= ModShift
	}
	if mask&xtermModCtrl != 0 {
		mod |= ModCtrl
	}
	if mask&xtermModAlt != 0 {
		mod |= ModAlt
	}
	if mask&xtermModMeta != 0 {
		mod |= ModMeta
	}
	return mod
}

func oldMouseEvent(seq string) Event {
	return Event{
		Type: TypeMouse,
		Raw:  seq,
		Mouse: Mouse{
			Button:  int(seq[3]-32) & 3,
			X:       int(seq[4]) - 33,
			Y:       int(seq[5]) - 33,
			Release: int(seq[3]-32)&3 == 3,
		},
	}
}

func sgrMouseEvent(seq string) (Event, bool) {
	final := seq[len(seq)-1]
	body := strings.TrimSuffix(strings.TrimPrefix(seq, "\x1b[<"), string(final))
	parts := strings.Split(body, ";")
	if len(parts) != 3 {
		return Event{}, false
	}
	b, err1 := strconv.Atoi(parts[0])
	x, err2 := strconv.Atoi(parts[1])
	y, err3 := strconv.Atoi(parts[2])
	if err1 != nil || err2 != nil || err3 != nil {
		return Event{}, false
	}
	return Event{
		Type: TypeMouse,
		Raw:  seq,
		Mouse: Mouse{
			Button:  b,
			X:       x - 1,
			Y:       y - 1,
			Release: final == 'm',
		},
	}, true
}
