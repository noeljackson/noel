package capabilities

import "testing"

func TestDetectEnvKitty(t *testing.T) {
	caps := DetectEnv([]string{"KITTY_WINDOW_ID=1", "COLORTERM=truecolor", "TERM=xterm-kitty"})
	if !caps.Kitty || !caps.Truecolor || !caps.Hyperlinks {
		t.Fatalf("caps = %#v", caps)
	}
}

func TestDetectEnvTmuxDisablesProtocols(t *testing.T) {
	caps := DetectEnv([]string{"KITTY_WINDOW_ID=1", "TMUX=/tmp/tmux", "COLORTERM=truecolor"})
	if caps.Kitty || caps.Hyperlinks || !caps.Tmux || !caps.Truecolor {
		t.Fatalf("caps = %#v", caps)
	}
}
