package cli

import (
	"reflect"
	"testing"
)

func TestParseCoreFlags(t *testing.T) {
	opts, err := Parse([]string{
		"--continue",
		"--tools", "bash,read",
		"--tools=grep",
		"--thinking", "high",
		"--model", "claude-sonnet-4-6",
		"--session-dir", "/tmp/sessions",
		"--no-builtin-tools",
		"hello",
	})
	if err != nil {
		t.Fatal(err)
	}
	if opts.Session.Continue != true {
		t.Fatal("continue flag was not set")
	}
	if opts.Model != "claude-sonnet-4-6" || opts.Thinking != "high" {
		t.Fatalf("model/thinking mismatch: %#v", opts)
	}
	if opts.Session.SessionDir != "/tmp/sessions" {
		t.Fatalf("session dir = %q", opts.Session.SessionDir)
	}
	if !opts.Tools.NoBuiltins {
		t.Fatal("no builtin tools flag was not set")
	}
	if !reflect.DeepEqual(opts.Tools.Allow, []string{"bash", "read", "grep"}) {
		t.Fatalf("tools = %#v", opts.Tools.Allow)
	}
	if opts.Prompt != "hello" {
		t.Fatalf("prompt = %q", opts.Prompt)
	}
}

func TestParseModePrintAliases(t *testing.T) {
	for _, args := range [][]string{
		{"--print", "hello"},
		{"--mode", "print", "hello"},
	} {
		opts, err := Parse(args)
		if err != nil {
			t.Fatalf("Parse(%v): %v", args, err)
		}
		if opts.Mode != ModePrint {
			t.Fatalf("Parse(%v) mode = %s", args, opts.Mode)
		}
	}
}

func TestParseNoToolsFlags(t *testing.T) {
	opts, err := Parse([]string{"--no-tools", "--no-builtin-tools"})
	if err != nil {
		t.Fatal(err)
	}
	if !opts.Tools.NoTools || !opts.Tools.NoBuiltins {
		t.Fatalf("tool flags = %#v", opts.Tools)
	}
}

func TestParseToolsRepeatedAndCommaSeparated(t *testing.T) {
	opts, err := Parse([]string{"--tools", "bash,read", "--tools", "grep"})
	if err != nil {
		t.Fatal(err)
	}
	want := []string{"bash", "read", "grep"}
	if !reflect.DeepEqual(opts.Tools.Allow, want) {
		t.Fatalf("tools = %#v, want %#v", opts.Tools.Allow, want)
	}
}

func TestParseInvalidCombinations(t *testing.T) {
	for _, args := range [][]string{
		{"--print", "--mode", "json"},
		{"--continue", "--resume", "abc"},
	} {
		if _, err := Parse(args); err == nil {
			t.Fatalf("Parse(%v) succeeded, want error", args)
		}
	}
}

func TestParseExtraArgsAfterDoubleDash(t *testing.T) {
	opts, err := Parse([]string{"--print", "hello", "--", "--not-a-pi-flag", "value"})
	if err != nil {
		t.Fatal(err)
	}
	want := []string{"--not-a-pi-flag", "value"}
	if !reflect.DeepEqual(opts.ExtraArgs, want) {
		t.Fatalf("extra args = %#v, want %#v", opts.ExtraArgs, want)
	}
}
