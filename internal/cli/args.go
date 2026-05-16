// Package cli parses pi command-line options.
package cli

import (
	"errors"
	"fmt"
	"strings"
)

// Mode selects the CLI runtime mode.
type Mode string

const (
	ModeInteractive Mode = "interactive"
	ModePrint       Mode = "print"
	ModeJSON        Mode = "json"
	ModeRPC         Mode = "rpc"
)

// Options contains parsed command-line options.
type Options struct {
	Mode         Mode
	Prompt       string
	Model        string
	Provider     string
	APIKey       string
	Thinking     string
	Tools        ToolsFlag
	Session      SessionFlag
	Print        bool
	Verbose      bool
	ListModels   bool
	Login        string
	Logout       string
	Offline      bool
	OffsetTokens int
	Files        []string
	ExtraArgs    []string
	Help         bool
	Version      bool
	Headless     bool
	QuietStartup bool
	StartupProbe bool
}

// ToolsFlag contains CLI tool-selection flags.
type ToolsFlag struct {
	Allow      []string
	Deny       []string
	NoTools    bool
	NoBuiltins bool
}

// SessionFlag contains CLI session-selection flags.
type SessionFlag struct {
	Continue   bool
	Resume     string
	Fork       string
	SessionDir string
	SessionID  string
	NoSession  bool
}

var validThinkingLevels = map[string]struct{}{
	"off":     {},
	"minimal": {},
	"low":     {},
	"medium":  {},
	"high":    {},
	"xhigh":   {},
	"auto":    {},
}

// Parse parses pi CLI arguments.
func Parse(args []string) (Options, error) {
	opts := Options{Mode: ModeInteractive}
	var messages []string
	var explicitMode string

	for i := 0; i < len(args); i++ {
		arg := args[i]
		if arg == "--" {
			opts.ExtraArgs = append(opts.ExtraArgs, args[i+1:]...)
			break
		}
		if arg == "" {
			continue
		}
		if strings.HasPrefix(arg, "@") && len(arg) > 1 {
			opts.Files = append(opts.Files, arg[1:])
			continue
		}
		if strings.HasPrefix(arg, "--") {
			name, value, hasValue := splitLongFlag(arg)
			switch name {
			case "help":
				opts.Help = true
			case "version":
				opts.Version = true
			case "mode":
				next, err := flagValue(args, &i, name, value, hasValue)
				if err != nil {
					return Options{}, err
				}
				mode, err := parseMode(next)
				if err != nil {
					return Options{}, err
				}
				opts.Mode = mode
				explicitMode = next
			case "print":
				opts.Print = true
				opts.Mode = ModePrint
				if !hasValue && i+1 < len(args) && printConsumes(args[i+1]) {
					messages = append(messages, args[i+1])
					i++
				}
			case "headless":
				opts.Headless = true
				opts.Print = true
				opts.Mode = ModePrint
			case "continue":
				opts.Session.Continue = true
			case "resume":
				next, err := flagValue(args, &i, name, value, hasValue)
				if err != nil {
					return Options{}, err
				}
				opts.Session.Resume = next
			case "session":
				next, err := flagValue(args, &i, name, value, hasValue)
				if err != nil {
					return Options{}, err
				}
				opts.Session.SessionID = next
			case "fork":
				next, err := flagValue(args, &i, name, value, hasValue)
				if err != nil {
					return Options{}, err
				}
				opts.Session.Fork = next
			case "session-dir":
				next, err := flagValue(args, &i, name, value, hasValue)
				if err != nil {
					return Options{}, err
				}
				opts.Session.SessionDir = next
			case "no-session":
				opts.Session.NoSession = true
			case "provider":
				next, err := flagValue(args, &i, name, value, hasValue)
				if err != nil {
					return Options{}, err
				}
				opts.Provider = next
			case "model":
				next, err := flagValue(args, &i, name, value, hasValue)
				if err != nil {
					return Options{}, err
				}
				opts.Model = next
			case "api-key":
				next, err := flagValue(args, &i, name, value, hasValue)
				if err != nil {
					return Options{}, err
				}
				opts.APIKey = next
			case "thinking":
				next, err := flagValue(args, &i, name, value, hasValue)
				if err != nil {
					return Options{}, err
				}
				if _, ok := validThinkingLevels[next]; !ok {
					return Options{}, fmt.Errorf("invalid thinking level %q: valid values are off, minimal, low, medium, high, xhigh", next)
				}
				opts.Thinking = next
			case "no-tools":
				opts.Tools.NoTools = true
			case "no-builtin-tools":
				opts.Tools.NoBuiltins = true
			case "tools":
				next, err := flagValue(args, &i, name, value, hasValue)
				if err != nil {
					return Options{}, err
				}
				opts.Tools.Allow = append(opts.Tools.Allow, splitList(next)...)
			case "deny-tools":
				next, err := flagValue(args, &i, name, value, hasValue)
				if err != nil {
					return Options{}, err
				}
				opts.Tools.Deny = append(opts.Tools.Deny, splitList(next)...)
			case "file":
				next, err := flagValue(args, &i, name, value, hasValue)
				if err != nil {
					return Options{}, err
				}
				opts.Files = append(opts.Files, next)
			case "login":
				next, err := flagValue(args, &i, name, value, hasValue)
				if err != nil {
					return Options{}, err
				}
				opts.Login = next
			case "logout":
				next, err := flagValue(args, &i, name, value, hasValue)
				if err != nil {
					return Options{}, err
				}
				opts.Logout = next
			case "list-models":
				opts.ListModels = true
				if !hasValue && i+1 < len(args) && args[i+1] != "" && !strings.HasPrefix(args[i+1], "-") && !strings.HasPrefix(args[i+1], "@") {
					i++
				}
			case "verbose":
				opts.Verbose = true
			case "quiet-startup":
				opts.QuietStartup = true
			case "startup-probe":
				opts.StartupProbe = true
			case "offline":
				opts.Offline = true
			default:
				return Options{}, fmt.Errorf("unknown option: --%s", name)
			}
			continue
		}
		if strings.HasPrefix(arg, "-") {
			switch arg {
			case "-h":
				opts.Help = true
			case "-v":
				opts.Version = true
			case "-p":
				opts.Print = true
				opts.Mode = ModePrint
				if i+1 < len(args) && printConsumes(args[i+1]) {
					messages = append(messages, args[i+1])
					i++
				}
			case "-c":
				opts.Session.Continue = true
			case "-r":
				next, err := flagValue(args, &i, "resume", "", false)
				if err != nil {
					return Options{}, err
				}
				opts.Session.Resume = next
			case "-t":
				next, err := flagValue(args, &i, "tools", "", false)
				if err != nil {
					return Options{}, err
				}
				opts.Tools.Allow = append(opts.Tools.Allow, splitList(next)...)
			case "-nt":
				opts.Tools.NoTools = true
			case "-nbt":
				opts.Tools.NoBuiltins = true
			default:
				return Options{}, fmt.Errorf("unknown option: %s", arg)
			}
			continue
		}
		messages = append(messages, arg)
	}

	opts.Prompt = strings.Join(messages, "\n")
	if err := validate(opts, explicitMode); err != nil {
		return Options{}, err
	}
	return opts, nil
}

func splitLongFlag(arg string) (name string, value string, hasValue bool) {
	trimmed := strings.TrimPrefix(arg, "--")
	if before, after, ok := strings.Cut(trimmed, "="); ok {
		return before, after, true
	}
	return trimmed, "", false
}

func flagValue(args []string, index *int, name, inline string, hasInline bool) (string, error) {
	if hasInline {
		if inline == "" {
			return "", fmt.Errorf("--%s requires a value", name)
		}
		return inline, nil
	}
	if *index+1 >= len(args) {
		return "", fmt.Errorf("--%s requires a value", name)
	}
	next := args[*index+1]
	if next == "" || next == "--" || (strings.HasPrefix(next, "-") && !strings.HasPrefix(next, "---")) {
		return "", fmt.Errorf("--%s requires a value", name)
	}
	*index = *index + 1
	return next, nil
}

func parseMode(value string) (Mode, error) {
	switch value {
	case "text", "print":
		return ModePrint, nil
	case "json":
		return ModeJSON, nil
	case "rpc":
		return ModeRPC, nil
	default:
		return "", fmt.Errorf("invalid mode %q: expected text, print, json, or rpc", value)
	}
}

func printConsumes(next string) bool {
	return next != "" && !strings.HasPrefix(next, "@") && (!strings.HasPrefix(next, "-") || strings.HasPrefix(next, "---"))
}

func splitList(value string) []string {
	parts := strings.Split(value, ",")
	out := make([]string, 0, len(parts))
	for _, part := range parts {
		part = strings.TrimSpace(part)
		if part != "" {
			out = append(out, part)
		}
	}
	return out
}

func validate(opts Options, explicitMode string) error {
	actionCount := 0
	for _, active := range []bool{opts.Login != "", opts.Logout != "", opts.ListModels, opts.Help, opts.Version} {
		if active {
			actionCount++
		}
	}
	if actionCount > 1 {
		return errors.New("only one of --login, --logout, --list-models, --help, or --version can be used")
	}
	if actionCount == 1 && (opts.Print || opts.Headless || opts.Mode == ModeJSON || opts.Mode == ModeRPC || opts.Session.Continue || opts.Session.Resume != "" || opts.Session.SessionID != "" || opts.Prompt != "" || len(opts.Files) > 0 || len(opts.ExtraArgs) > 0) {
		return errors.New("action flags cannot be combined with prompts, modes, or session selection")
	}
	if opts.Print && explicitMode != "" && explicitMode != "text" && explicitMode != "print" {
		return fmt.Errorf("--print cannot be combined with --mode %s", explicitMode)
	}
	sessionSelections := 0
	if opts.Session.Continue {
		sessionSelections++
	}
	if opts.Session.Resume != "" {
		sessionSelections++
	}
	if opts.Session.SessionID != "" {
		sessionSelections++
	}
	if sessionSelections > 1 {
		return errors.New("--continue, --resume, and --session are mutually exclusive")
	}
	if opts.Mode == ModeRPC && len(opts.Files) > 0 {
		return errors.New("@file and --file arguments are not supported in RPC mode")
	}
	if opts.StartupProbe && (opts.Print || opts.Headless || opts.Mode == ModeJSON || opts.Mode == ModeRPC || opts.Session.Continue || opts.Session.Resume != "" || opts.Session.SessionID != "" || opts.Prompt != "" || len(opts.Files) > 0 || len(opts.ExtraArgs) > 0) {
		return errors.New("usage: pi --startup-probe [--quiet-startup]")
	}
	return nil
}

// PrintHelp writes CLI help text for the pi executable.
func PrintHelp(out interface{ Write([]byte) (int, error) }) {
	_, _ = fmt.Fprint(out, HelpText("pi"))
}

// HelpText returns CLI help text.
func HelpText(appName string) string {
	return fmt.Sprintf(`%s - AI coding assistant with read, bash, edit, write tools

Usage:
  %s [options] [@files...] [messages...]

Options:
  --provider <name>              Provider name (default: anthropic)
  --model <pattern>              Model pattern or ID
  --api-key <key>                API key (defaults to env vars)
  --mode <mode>                  Output mode: text (default), json, or rpc
  --print, -p                    Non-interactive mode: process prompt and exit
  --continue, -c                 Continue previous session
  --resume, -r <id>              Resume a session by id
  --session <path|id>            Use specific session file or partial UUID
  --fork <path|id>               Fork specific session leaf into a new session
  --session-dir <dir>            Directory for session storage and lookup
  --no-session                   Don't save session (ephemeral)
  --no-tools, -nt                Disable all tools by default (built-in and extension)
  --no-builtin-tools, -nbt       Disable built-in tools by default but keep extension/custom tools enabled
  --tools, -t <tools>            Comma-separated allowlist of tool names to enable
  --thinking <level>             Set thinking level: off, minimal, low, medium, high, xhigh
  --file <path>                  Include a file in the initial prompt
  --list-models [search]         List available models
  --login <provider>             Login to a provider
  --logout <provider>            Logout from a provider
  --verbose                      Force verbose startup
  --offline                      Disable startup network operations
  --help, -h                     Show this help
  --version, -v                  Show version number

Examples:
  %s
  %s -p "List all .go files in internal/"
  %s @prompt.md "Apply this to the codebase"
  %s --continue "What did we discuss?"
  %s --tools read,grep,find,ls -p "Review the code"
`, appName, appName, appName, appName, appName, appName, appName)
}
