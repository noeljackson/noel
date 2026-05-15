package anthropic

import (
	"bufio"
	"context"
	"io"
	"strings"
)

type serverSentEvent struct {
	Event string
	Data  string
	Raw   []string
}

type sseDecoderState struct {
	event string
	data  []string
	raw   []string
}

func iterateSSE(ctx context.Context, reader io.Reader, handle func(serverSentEvent) error) error {
	state := &sseDecoderState{}
	buffered := bufio.NewReader(reader)
	var buffer string

	for {
		if err := ctx.Err(); err != nil {
			return err
		}
		chunk, err := buffered.ReadString('\n')
		if len(chunk) > 0 {
			buffer += chunk
			for {
				line, rest, ok := consumeLine(buffer)
				if !ok {
					break
				}
				buffer = rest
				event := decodeSSELine(line, state)
				if event != nil {
					if err := handle(*event); err != nil {
						return err
					}
				}
			}
		}
		if err == io.EOF {
			break
		}
		if err != nil {
			return err
		}
	}

	if buffer != "" {
		event := decodeSSELine(buffer, state)
		if event != nil {
			if err := handle(*event); err != nil {
				return err
			}
		}
	}
	if event := flushSSEEvent(state); event != nil {
		return handle(*event)
	}
	return nil
}

func decodeSSELine(line string, state *sseDecoderState) *serverSentEvent {
	if line == "" {
		return flushSSEEvent(state)
	}

	state.raw = append(state.raw, line)
	if strings.HasPrefix(line, ":") {
		return nil
	}

	fieldName := line
	value := ""
	if delimiter := strings.Index(line, ":"); delimiter >= 0 {
		fieldName = line[:delimiter]
		value = line[delimiter+1:]
		if strings.HasPrefix(value, " ") {
			value = value[1:]
		}
	}

	switch fieldName {
	case "event":
		state.event = value
	case "data":
		state.data = append(state.data, value)
	}
	return nil
}

func flushSSEEvent(state *sseDecoderState) *serverSentEvent {
	if state.event == "" && len(state.data) == 0 {
		return nil
	}
	event := &serverSentEvent{
		Event: state.event,
		Data:  strings.Join(state.data, "\n"),
		Raw:   append([]string(nil), state.raw...),
	}
	state.event = ""
	state.data = nil
	state.raw = nil
	return event
}

func consumeLine(text string) (line string, rest string, ok bool) {
	index := nextLineBreakIndex(text)
	if index == -1 {
		return "", text, false
	}
	next := index + 1
	if text[index] == '\r' && next < len(text) && text[next] == '\n' {
		next++
	}
	return text[:index], text[next:], true
}

func nextLineBreakIndex(text string) int {
	carriage := strings.IndexByte(text, '\r')
	newline := strings.IndexByte(text, '\n')
	if carriage == -1 {
		return newline
	}
	if newline == -1 {
		return carriage
	}
	if carriage < newline {
		return carriage
	}
	return newline
}
