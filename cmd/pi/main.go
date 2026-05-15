package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/noeljackson/pi/internal/agent"
	anthropicprovider "github.com/noeljackson/pi/internal/anthropic"
	"github.com/noeljackson/pi/internal/session"
	"github.com/noeljackson/pi/internal/tools"
	"github.com/noeljackson/pi/internal/tools/bash"
	filetools "github.com/noeljackson/pi/internal/tools/file"
	"github.com/noeljackson/pi/internal/tui"
)

const (
	defaultModel     = "claude-sonnet-4-6"
	defaultMaxTokens = 4096
)

type cliOptions struct {
	headless bool
	resumeID string
	prompt   string
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func run() error {
	opts, err := parseOptions(os.Args[1:])
	if err != nil {
		return err
	}
	if opts.headless {
		return runHeadless(opts.prompt)
	}
	return runTUI(opts.resumeID)
}

func parseOptions(args []string) (cliOptions, error) {
	flags := flag.NewFlagSet("pi", flag.ContinueOnError)
	flags.SetOutput(os.Stderr)
	headless := flags.Bool("headless", false, "run one prompt without the TUI")
	resumeID := flags.String("resume", "", "resume a session by id")
	if err := flags.Parse(args); err != nil {
		return cliOptions{}, err
	}

	remaining := flags.Args()
	if *headless {
		if *resumeID != "" {
			return cliOptions{}, errors.New("--headless cannot be combined with --resume")
		}
		if len(remaining) == 0 {
			return cliOptions{}, errors.New("usage: pi --headless \"prompt\"")
		}
		return cliOptions{headless: true, prompt: strings.Join(remaining, " ")}, nil
	}
	if len(remaining) != 0 {
		return cliOptions{}, errors.New("usage: pi [--resume <session-id>] or pi --headless \"prompt\"")
	}
	return cliOptions{resumeID: *resumeID}, nil
}

func runHeadless(prompt string) error {
	auth, err := anthropicprovider.PickAuth()
	if err != nil {
		return err
	}

	sess, cfg, err := newSessionConfig("", auth)
	if err != nil {
		return err
	}
	defer sess.Close()

	emit := func(event agent.Event) {
		if update, ok := event.(agent.MessageUpdateEvent); ok && update.Delta.TextDelta != "" {
			fmt.Fprint(os.Stdout, update.Delta.TextDelta)
		}
	}
	_, err = runTurn(context.Background(), sess, cfg, prompt, emit)
	if err == nil {
		fmt.Fprintln(os.Stdout)
	}
	return err
}

func runTUI(resumeID string) error {
	rootCtx, cancel := context.WithCancel(context.Background())
	defer cancel()

	auth, err := anthropicprovider.PickAuth()
	if err != nil {
		return err
	}

	sess, cfg, err := newSessionConfig(resumeID, auth)
	if err != nil {
		return err
	}
	defer sess.Close()

	messages, err := sess.Messages()
	if err != nil {
		return err
	}

	eventCh := make(chan agent.Event, 256)
	submitCh := make(chan string, 32)

	var turnMu sync.Mutex
	var turnCancel context.CancelFunc
	setTurnCancel := func(cancel context.CancelFunc) {
		turnMu.Lock()
		defer turnMu.Unlock()
		turnCancel = cancel
	}
	abort := func() {
		turnMu.Lock()
		cancelTurn := turnCancel
		turnMu.Unlock()
		if cancelTurn != nil {
			cancelTurn()
		}
	}

	model := tui.New(tui.Options{
		EventSource: eventCh,
		Messages:    messages,
		Model:       cfg.Model,
		Submit: func(text string) {
			select {
			case submitCh <- text:
			case <-rootCtx.Done():
			}
		},
		Abort: abort,
	})

	programDone := make(chan error, 1)
	program := tea.NewProgram(model, tea.WithContext(rootCtx))
	go func() {
		_, err := program.Run()
		programDone <- err
	}()

	for {
		select {
		case err := <-programDone:
			cancel()
			return err
		case text := <-submitCh:
			turnCtx, cancelTurn := context.WithCancel(rootCtx)
			setTurnCancel(cancelTurn)
			err := runTUITurn(turnCtx, sess, cfg, text, eventCh)
			cancelTurn()
			setTurnCancel(nil)
			if err != nil && !errors.Is(err, context.Canceled) {
				sendEvent(rootCtx, eventCh, agent.AgentEndEvent{Reason: "error", Err: err})
			}
		}
	}
}

func runTUITurn(ctx context.Context, sess *session.Session, cfg agent.LoopConfig, prompt string, eventCh chan<- agent.Event) error {
	emit := func(event agent.Event) {
		sendEvent(ctx, eventCh, event)
	}
	_, err := runTurn(ctx, sess, cfg, prompt, emit)
	return err
}

func sendEvent(ctx context.Context, eventCh chan<- agent.Event, event agent.Event) {
	select {
	case eventCh <- event:
	case <-ctx.Done():
	}
}

func runTurn(ctx context.Context, sess *session.Session, cfg agent.LoopConfig, prompt string, emit func(agent.Event)) (*agent.AssistantMessage, error) {
	user := agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: prompt}}}
	messages, err := sess.Messages()
	if err != nil {
		return nil, err
	}
	if len(messages) == 0 {
		return agent.Run(ctx, cfg, user, emit)
	}
	messages = append(messages, user)
	if err := sess.AppendMessage(user); err != nil {
		return nil, err
	}
	return agent.Continue(ctx, cfg, messages, emit)
}

func newSessionConfig(resumeID string, auth anthropicprovider.AuthSource) (*session.Session, agent.LoopConfig, error) {
	store, err := newSessionStore()
	if err != nil {
		return nil, agent.LoopConfig{}, err
	}

	var sess *session.Session
	if resumeID == "" {
		cwd, err := os.Getwd()
		if err != nil {
			return nil, agent.LoopConfig{}, err
		}
		sess, err = store.Create(cwd)
		if err != nil {
			return nil, agent.LoopConfig{}, err
		}
	} else {
		sess, err = store.Open(resumeID)
		if err != nil {
			return nil, agent.LoopConfig{}, err
		}
	}

	registry, err := builtinRegistry()
	if err != nil {
		_ = sess.Close()
		return nil, agent.LoopConfig{}, err
	}

	cfg := agent.LoopConfig{
		Provider:      anthropicprovider.NewClient(auth),
		Tools:         registry,
		Model:         modelName(),
		MaxTokens:     defaultMaxTokens,
		SessionWriter: sess,
	}
	return sess, cfg, nil
}

func newSessionStore() (*session.JSONLStore, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return nil, err
	}
	return session.NewJSONLStore(filepath.Join(home, ".pi", "sessions")), nil
}

func builtinRegistry() (*tools.Registry, error) {
	registry := tools.NewRegistry()
	registrations := []struct {
		tool agent.Tool
		sets []tools.ToolSet
	}{
		{tool: bash.NewTool(), sets: []tools.ToolSet{tools.ToolSetCoding}},
		{tool: filetools.NewReadTool(), sets: []tools.ToolSet{tools.ToolSetReadOnly, tools.ToolSetCoding}},
		{tool: filetools.NewWriteTool(), sets: []tools.ToolSet{tools.ToolSetCoding}},
		{tool: filetools.NewEditTool(), sets: []tools.ToolSet{tools.ToolSetCoding}},
		{tool: filetools.NewGrepTool(), sets: []tools.ToolSet{tools.ToolSetReadOnly, tools.ToolSetCoding}},
		{tool: filetools.NewFindTool(), sets: []tools.ToolSet{tools.ToolSetReadOnly, tools.ToolSetCoding}},
		{tool: filetools.NewLsTool(), sets: []tools.ToolSet{tools.ToolSetReadOnly, tools.ToolSetCoding}},
	}
	for _, registration := range registrations {
		if err := registry.RegisterInSet(registration.tool, registration.sets...); err != nil {
			return nil, err
		}
	}
	return registry, nil
}

func modelName() string {
	model := os.Getenv("PI_MODEL")
	if model == "" {
		return defaultModel
	}
	return model
}
