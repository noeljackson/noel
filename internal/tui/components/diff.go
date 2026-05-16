package components

import (
	"strings"
)

func DiffView(unified string, width int) string {
	var out []string
	for _, line := range strings.Split(strings.TrimRight(unified, "\n"), "\n") {
		if width > 0 {
			line = truncateWidth(strings.ReplaceAll(line, "\t", "   "), width)
		}
		switch {
		case strings.HasPrefix(line, "+++") || strings.HasPrefix(line, "---") || strings.HasPrefix(line, "@@"):
			out = append(out, "\x1b[1;36m"+line+"\x1b[0m")
		case strings.HasPrefix(line, "+"):
			out = append(out, "\x1b[32m"+line+"\x1b[0m")
		case strings.HasPrefix(line, "-"):
			out = append(out, "\x1b[31m"+line+"\x1b[0m")
		default:
			out = append(out, "\x1b[2m"+line+"\x1b[0m")
		}
	}
	return strings.Join(out, "\n")
}
