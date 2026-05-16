package resources

import (
	"fmt"
	"strconv"
	"strings"
)

func ParseFrontmatter(content string) (map[string]any, string, error) {
	normalized := normalizeNewlines(content)
	if !strings.HasPrefix(normalized, "---") {
		return map[string]any{}, normalized, nil
	}
	end := strings.Index(normalized[3:], "\n---")
	if end < 0 {
		return map[string]any{}, normalized, nil
	}
	end += 3
	header := ""
	if end >= 4 {
		header = normalized[4:end]
	}
	body := strings.TrimSpace(normalized[end+4:])
	values, err := parseYAMLSubset(header)
	if err != nil {
		return nil, "", err
	}
	return values, body, nil
}

func normalizeNewlines(value string) string {
	value = strings.ReplaceAll(value, "\r\n", "\n")
	return strings.ReplaceAll(value, "\r", "\n")
}

type parsedLine struct {
	indent int
	text   string
	line   int
}

func parseYAMLSubset(input string) (map[string]any, error) {
	lines := significantLines(input)
	out := map[string]any{}
	for i := 0; i < len(lines); {
		line := lines[i]
		if line.indent != 0 {
			return nil, fmt.Errorf("line %d: unexpected indentation", line.line)
		}
		key, raw, ok := splitKeyValue(line.text)
		if !ok {
			return nil, fmt.Errorf("line %d: expected key: value", line.line)
		}
		if raw != "" {
			value, err := parseScalar(raw)
			if err != nil {
				return nil, fmt.Errorf("line %d: %w", line.line, err)
			}
			out[key] = value
			i++
			continue
		}
		value, next, err := parseNested(lines, i+1, line.indent)
		if err != nil {
			return nil, err
		}
		out[key] = value
		i = next
	}
	return out, nil
}

func significantLines(input string) []parsedLine {
	rawLines := strings.Split(input, "\n")
	lines := make([]parsedLine, 0, len(rawLines))
	for i, raw := range rawLines {
		if strings.TrimSpace(raw) == "" {
			continue
		}
		trimmedLeft := strings.TrimLeft(raw, " ")
		if strings.HasPrefix(strings.TrimSpace(raw), "#") {
			continue
		}
		lines = append(lines, parsedLine{
			indent: len(raw) - len(trimmedLeft),
			text:   strings.TrimSpace(raw),
			line:   i + 1,
		})
	}
	return lines
}

func parseNested(lines []parsedLine, start int, parentIndent int) (any, int, error) {
	if start >= len(lines) || lines[start].indent <= parentIndent {
		return map[string]any{}, start, nil
	}
	if strings.HasPrefix(lines[start].text, "- ") {
		return parseList(lines, start, parentIndent)
	}
	return parseMap(lines, start, parentIndent)
}

func parseMap(lines []parsedLine, start int, parentIndent int) (map[string]any, int, error) {
	out := map[string]any{}
	i := start
	for i < len(lines) {
		line := lines[i]
		if line.indent <= parentIndent {
			break
		}
		key, raw, ok := splitKeyValue(line.text)
		if !ok {
			return nil, 0, fmt.Errorf("line %d: expected key: value", line.line)
		}
		if raw != "" {
			value, err := parseScalar(raw)
			if err != nil {
				return nil, 0, fmt.Errorf("line %d: %w", line.line, err)
			}
			out[key] = value
			i++
			continue
		}
		value, next, err := parseNested(lines, i+1, line.indent)
		if err != nil {
			return nil, 0, err
		}
		out[key] = value
		i = next
	}
	return out, i, nil
}

func parseList(lines []parsedLine, start int, parentIndent int) ([]any, int, error) {
	out := []any{}
	i := start
	for i < len(lines) {
		line := lines[i]
		if line.indent <= parentIndent {
			break
		}
		if !strings.HasPrefix(line.text, "- ") {
			break
		}
		raw := strings.TrimSpace(strings.TrimPrefix(line.text, "- "))
		if raw == "" {
			value, next, err := parseNested(lines, i+1, line.indent)
			if err != nil {
				return nil, 0, err
			}
			out = append(out, value)
			i = next
			continue
		}
		if key, valueText, ok := splitKeyValue(raw); ok {
			item := map[string]any{}
			if valueText == "" {
				value, next, err := parseNested(lines, i+1, line.indent)
				if err != nil {
					return nil, 0, err
				}
				item[key] = value
				i = next
			} else {
				value, err := parseScalar(valueText)
				if err != nil {
					return nil, 0, fmt.Errorf("line %d: %w", line.line, err)
				}
				item[key] = value
				i++
			}
			for i < len(lines) && lines[i].indent > line.indent && !strings.HasPrefix(lines[i].text, "- ") {
				child := lines[i]
				childKey, childRaw, ok := splitKeyValue(child.text)
				if !ok {
					return nil, 0, fmt.Errorf("line %d: expected key: value", child.line)
				}
				if childRaw != "" {
					value, err := parseScalar(childRaw)
					if err != nil {
						return nil, 0, fmt.Errorf("line %d: %w", child.line, err)
					}
					item[childKey] = value
					i++
					continue
				}
				value, next, err := parseNested(lines, i+1, child.indent)
				if err != nil {
					return nil, 0, err
				}
				item[childKey] = value
				i = next
			}
			out = append(out, item)
			continue
		}
		value, err := parseScalar(raw)
		if err != nil {
			return nil, 0, fmt.Errorf("line %d: %w", line.line, err)
		}
		out = append(out, value)
		i++
	}
	return out, i, nil
}

func splitKeyValue(line string) (string, string, bool) {
	idx := strings.Index(line, ":")
	if idx <= 0 {
		return "", "", false
	}
	key := strings.TrimSpace(line[:idx])
	if key == "" || strings.ContainsAny(key, "\t[]{}") {
		return "", "", false
	}
	return key, strings.TrimSpace(line[idx+1:]), true
}

func parseScalar(raw string) (any, error) {
	raw = strings.TrimSpace(raw)
	if raw == "" {
		return "", nil
	}
	if strings.HasPrefix(raw, "[") {
		return parseInlineArray(raw)
	}
	if isQuoted(raw) {
		if raw[0] == '\'' {
			return strings.ReplaceAll(raw[1:len(raw)-1], `\'`, `'`), nil
		}
		unquoted, err := strconv.Unquote(raw)
		if err != nil {
			return "", err
		}
		return unquoted, nil
	}
	switch raw {
	case "true":
		return true, nil
	case "false":
		return false, nil
	case "null", "~":
		return nil, nil
	}
	if i, err := strconv.Atoi(raw); err == nil {
		return i, nil
	}
	if f, err := strconv.ParseFloat(raw, 64); err == nil && strings.Contains(raw, ".") {
		return f, nil
	}
	if strings.Contains(raw, ": ") {
		return nil, fmt.Errorf("malformed scalar %q", raw)
	}
	return strings.Trim(raw, `"`), nil
}

func parseInlineArray(raw string) ([]any, error) {
	if !strings.HasSuffix(raw, "]") {
		return nil, fmt.Errorf("unterminated array")
	}
	inner := strings.TrimSpace(raw[1 : len(raw)-1])
	if inner == "" {
		return []any{}, nil
	}
	parts, err := splitCommaAware(inner)
	if err != nil {
		return nil, err
	}
	out := make([]any, 0, len(parts))
	for _, part := range parts {
		value, err := parseScalar(strings.TrimSpace(part))
		if err != nil {
			return nil, err
		}
		out = append(out, value)
	}
	return out, nil
}

func splitCommaAware(input string) ([]string, error) {
	var parts []string
	var current strings.Builder
	var quote rune
	for _, r := range input {
		if quote != 0 {
			current.WriteRune(r)
			if r == quote {
				quote = 0
			}
			continue
		}
		if r == '\'' || r == '"' {
			quote = r
			current.WriteRune(r)
			continue
		}
		if r == ',' {
			parts = append(parts, current.String())
			current.Reset()
			continue
		}
		current.WriteRune(r)
	}
	if quote != 0 {
		return nil, fmt.Errorf("unterminated quoted string")
	}
	parts = append(parts, current.String())
	return parts, nil
}

func isQuoted(raw string) bool {
	return len(raw) >= 2 && ((raw[0] == '"' && raw[len(raw)-1] == '"') || (raw[0] == '\'' && raw[len(raw)-1] == '\''))
}
