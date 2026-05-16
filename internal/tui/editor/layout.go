package editor

import "strings"

type visualLine struct {
	logicalLine int
	startCol    int
	length      int
	text        string
}

func (e *Editor) visualLines(width int) []visualLine {
	if width <= 0 {
		width = 1
	}
	var out []visualLine
	for i, line := range e.lines {
		if line == "" {
			out = append(out, visualLine{logicalLine: i})
			continue
		}
		if !e.lineWrapping || visibleWidth(line) <= width {
			out = append(out, visualLine{logicalLine: i, startCol: 0, length: len(line), text: line})
			continue
		}
		out = append(out, wrapLine(i, line, width, e.validPasteIDs())...)
	}
	if len(out) == 0 {
		out = append(out, visualLine{})
	}
	return out
}

func wrapLine(logical int, line string, width int, pasteIDs map[int]bool) []visualLine {
	var lines []visualLine
	spans := graphemeSpans(line, pasteIDs)
	chunkStart := 0
	chunkWidth := 0
	wrapAt := -1
	widthAtWrap := 0
	for _, span := range spans {
		w := graphemeWidth(span.text)
		if chunkWidth+w > width {
			if wrapAt > chunkStart {
				text := line[chunkStart:wrapAt]
				lines = append(lines, visualLine{logicalLine: logical, startCol: chunkStart, length: len(text), text: text})
				chunkStart = wrapAt
				chunkWidth -= widthAtWrap
			} else if span.start > chunkStart {
				text := line[chunkStart:span.start]
				lines = append(lines, visualLine{logicalLine: logical, startCol: chunkStart, length: len(text), text: text})
				chunkStart = span.start
				chunkWidth = 0
			}
			wrapAt = -1
		}
		chunkWidth += w
		if isWhitespace(span.text) {
			wrapAt = span.end
			widthAtWrap = chunkWidth
		}
	}
	text := line[chunkStart:]
	lines = append(lines, visualLine{logicalLine: logical, startCol: chunkStart, length: len(text), text: text})
	return lines
}

func (e *Editor) currentVisualLine(lines []visualLine) int {
	for i, line := range lines {
		if line.logicalLine != e.cursorLine {
			continue
		}
		isLastForLogical := i == len(lines)-1 || lines[i+1].logicalLine != line.logicalLine
		offset := e.cursorCol - line.startCol
		if offset >= 0 && (offset < line.length || isLastForLogical && offset <= line.length) {
			return i
		}
	}
	return max(0, len(lines)-1)
}

func (e *Editor) renderVisualLine(line visualLine) string {
	text := line.text
	if line.logicalLine != e.cursorLine {
		return text
	}
	cursor := e.cursorCol - line.startCol
	cursor = clamp(cursor, 0, len(text))
	if !e.focused {
		return text
	}
	before := text[:cursor]
	after := text[cursor:]
	if after == "" {
		return before + "\x1b[7m \x1b[0m"
	}
	spans := graphemeSpans(after, e.validPasteIDs())
	if len(spans) == 0 {
		return before + "\x1b[7m \x1b[0m"
	}
	first := spans[0]
	return before + "\x1b[7m" + after[first.start:first.end] + "\x1b[0m" + strings.TrimPrefix(after[first.end:], "")
}
