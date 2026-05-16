package editor

import (
	"fmt"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"
)

type graphemeSpan struct {
	start int
	end   int
	text  string
}

// graphemeSpans is a compact boundary detector for prompt editing. It handles
// combining marks, variation selectors, emoji modifiers, ZWJ sequences, and
// regional-indicator pairs. It is not a full UAX #29 implementation.
func graphemeSpans(text string, pasteIDs map[int]bool) []graphemeSpan {
	if text == "" {
		return nil
	}
	var spans []graphemeSpan
	for i := 0; i < len(text); {
		if start, end, ok := pasteMarkerAt(text, i, pasteIDs); ok && start == i {
			spans = append(spans, graphemeSpan{start: start, end: end, text: text[start:end]})
			i = end
			continue
		}
		start := i
		r, size := utf8.DecodeRuneInString(text[i:])
		i += size
		regionalCount := 0
		if isRegionalIndicator(r) {
			regionalCount = 1
		}
		for i < len(text) {
			next, nextSize := utf8.DecodeRuneInString(text[i:])
			if isCombining(next) || isVariationSelector(next) || isEmojiModifier(next) {
				i += nextSize
				continue
			}
			if next == '\u200d' {
				i += nextSize
				if i < len(text) {
					_, joinedSize := utf8.DecodeRuneInString(text[i:])
					i += joinedSize
				}
				continue
			}
			if isRegionalIndicator(next) && regionalCount == 1 {
				i += nextSize
				regionalCount++
				continue
			}
			break
		}
		spans = append(spans, graphemeSpan{start: start, end: i, text: text[start:i]})
	}
	return spans
}

func nextGraphemeEnd(text string, col int, pasteIDs map[int]bool) int {
	for _, span := range graphemeSpans(text, pasteIDs) {
		if span.start >= col {
			return span.end
		}
		if col > span.start && col < span.end {
			return span.end
		}
	}
	return len(text)
}

func previousGraphemeStart(text string, col int, pasteIDs map[int]bool) int {
	last := 0
	for _, span := range graphemeSpans(text, pasteIDs) {
		if span.end >= col {
			if span.end == col {
				return span.start
			}
			return last
		}
		last = span.start
	}
	return last
}

func pasteMarkerAt(text string, offset int, pasteIDs map[int]bool) (int, int, bool) {
	if len(pasteIDs) == 0 || !strings.HasPrefix(text[offset:], "[paste #") {
		return 0, 0, false
	}
	end := strings.IndexByte(text[offset:], ']')
	if end < 0 {
		return 0, 0, false
	}
	end += offset + 1
	inside := text[offset+8 : end-1]
	fields := strings.Fields(inside)
	if len(fields) == 0 {
		return 0, 0, false
	}
	id, err := strconv.Atoi(fields[0])
	if err != nil || !pasteIDs[id] {
		return 0, 0, false
	}
	return offset, end, true
}

func isCombining(r rune) bool {
	return unicode.Is(unicode.Mn, r) || unicode.Is(unicode.Me, r) || unicode.Is(unicode.Mc, r)
}

func isVariationSelector(r rune) bool {
	return r >= 0xfe00 && r <= 0xfe0f || r >= 0xe0100 && r <= 0xe01ef
}

func isEmojiModifier(r rune) bool {
	return r >= 0x1f3fb && r <= 0x1f3ff
}

func isRegionalIndicator(r rune) bool {
	return r >= 0x1f1e6 && r <= 0x1f1ff
}

func visibleWidth(text string) int {
	width := 0
	for _, span := range graphemeSpans(text, nil) {
		width += graphemeWidth(span.text)
	}
	return width
}

func graphemeWidth(text string) int {
	if text == "" {
		return 0
	}
	r, _ := utf8.DecodeRuneInString(text)
	if r == '\n' || r == '\r' || r == '\t' || unicode.IsControl(r) {
		return 0
	}
	if isWideRune(r) {
		return 2
	}
	return 1
}

func isWideRune(r rune) bool {
	return (r >= 0x1100 && r <= 0x115f) ||
		(r >= 0x2329 && r <= 0x232a) ||
		(r >= 0x2e80 && r <= 0xa4cf) ||
		(r >= 0xac00 && r <= 0xd7a3) ||
		(r >= 0xf900 && r <= 0xfaff) ||
		(r >= 0xfe10 && r <= 0xfe19) ||
		(r >= 0xfe30 && r <= 0xfe6f) ||
		(r >= 0xff00 && r <= 0xff60) ||
		(r >= 0xffe0 && r <= 0xffe6) ||
		(r >= 0x1f300 && r <= 0x1faff)
}

func isWhitespace(text string) bool {
	for _, r := range text {
		if !unicode.IsSpace(r) {
			return false
		}
	}
	return text != ""
}

func isPunctuation(text string) bool {
	for _, r := range text {
		if unicode.IsPunct(r) || unicode.IsSymbol(r) {
			return true
		}
	}
	return false
}

func markerText(id int, lineCount int, charCount int) string {
	if lineCount > 10 {
		return fmt.Sprintf("[paste #%d +%d lines]", id, lineCount)
	}
	return fmt.Sprintf("[paste #%d %d chars]", id, charCount)
}
