package capabilities

import (
	"image"
	"os"
	"strings"
)

type Caps struct {
	Truecolor  bool
	Hyperlinks bool
	Kitty      bool
	ITerm      bool
	Sixel      bool
	Mouse      bool
	Tmux       bool
	Screen     bool
	CellPixels image.Point
}

func Detect() Caps {
	return DetectEnv(os.Environ())
}

func DetectEnv(env []string) Caps {
	values := map[string]string{}
	for _, item := range env {
		key, value, ok := strings.Cut(item, "=")
		if ok {
			values[key] = value
		}
	}
	term := strings.ToLower(values["TERM"])
	termProgram := strings.ToLower(values["TERM_PROGRAM"])
	colorTerm := strings.ToLower(values["COLORTERM"])
	tmux := values["TMUX"] != "" || strings.Contains(term, "tmux")
	screen := strings.HasPrefix(term, "screen")
	truecolor := colorTerm == "truecolor" || colorTerm == "24bit"
	caps := Caps{
		Truecolor:  truecolor,
		Tmux:       tmux,
		Screen:     screen,
		Mouse:      values["TERM"] != "dumb",
		CellPixels: image.Point{X: 9, Y: 18},
	}
	if tmux || screen {
		return caps
	}
	switch {
	case values["KITTY_WINDOW_ID"] != "", termProgram == "kitty", termProgram == "ghostty", strings.Contains(term, "ghostty"), values["GHOSTTY_RESOURCES_DIR"] != "", values["WEZTERM_PANE"] != "", termProgram == "wezterm":
		caps.Kitty = true
		caps.Truecolor = true
		caps.Hyperlinks = true
	case values["ITERM_SESSION_ID"] != "", termProgram == "iterm.app":
		caps.ITerm = true
		caps.Truecolor = true
		caps.Hyperlinks = true
	case termProgram == "vscode", termProgram == "alacritty":
		caps.Truecolor = true
		caps.Hyperlinks = true
	}
	if strings.Contains(term, "sixel") {
		caps.Sixel = true
	}
	return caps
}

func WrapPassthrough(seq string, caps Caps) string {
	if seq == "" {
		return ""
	}
	if caps.Tmux {
		return "\x1bPtmux;\x1b" + strings.ReplaceAll(seq, "\x1b", "\x1b\x1b") + "\x1b\\"
	}
	if caps.Screen {
		return "\x1bP" + seq + "\x1b\\"
	}
	return seq
}
