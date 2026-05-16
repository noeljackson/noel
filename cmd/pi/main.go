package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"runtime"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/noeljackson/pi/internal/agent"
	anthropicprovider "github.com/noeljackson/pi/internal/anthropic"
	authstore "github.com/noeljackson/pi/internal/auth"
	"github.com/noeljackson/pi/internal/config"
	"github.com/noeljackson/pi/internal/models"
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
	headless   bool
	resumeID   string
	prompt     string
	login      string
	logout     string
	listModels bool
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
	if opts.login != "" {
		return runLogin(opts.login)
	}
	if opts.logout != "" {
		return runLogout(opts.logout)
	}
	if opts.listModels {
		return runListModels()
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
	login := flags.String("login", "", "login to a provider")
	logout := flags.String("logout", "", "logout from a provider")
	listModels := flags.Bool("list-models", false, "list available models")
	if err := flags.Parse(args); err != nil {
		return cliOptions{}, err
	}

	remaining := flags.Args()
	actionCount := 0
	for _, active := range []bool{*login != "", *logout != "", *listModels} {
		if active {
			actionCount++
		}
	}
	if actionCount > 0 {
		if actionCount > 1 || *headless || *resumeID != "" || len(remaining) != 0 {
			return cliOptions{}, errors.New("usage: pi [--login <provider> | --logout <provider> | --list-models]")
		}
		return cliOptions{login: *login, logout: *logout, listModels: *listModels}, nil
	}
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

func runLogin(provider string) error {
	if provider != "anthropic" {
		return fmt.Errorf("unsupported login provider %q", provider)
	}
	paths, err := config.ResolvePaths()
	if err != nil {
		return err
	}
	store := authstore.New(paths.AuthFile)
	result, err := authstore.LoginAnthropic(context.Background(), openBrowser)
	if err != nil {
		return err
	}
	metadata := map[string]any{}
	if len(result.Scopes) > 0 {
		metadata["scopes"] = result.Scopes
	}
	if result.Subscription != "" {
		metadata["subscription"] = result.Subscription
	}
	if err := store.Set("anthropic-oauth", authstore.ProviderAuth{
		Type:         "oauth",
		AccessToken:  result.AccessToken,
		RefreshToken: result.RefreshToken,
		ExpiresAt:    result.ExpiresAt,
		Metadata:     metadata,
	}); err != nil {
		return err
	}
	fmt.Fprintln(os.Stdout, "Logged in to Anthropic")
	return nil
}

func runLogout(provider string) error {
	if provider != "anthropic" {
		return fmt.Errorf("unsupported logout provider %q", provider)
	}
	paths, err := config.ResolvePaths()
	if err != nil {
		return err
	}
	store := authstore.New(paths.AuthFile)
	if existing, ok, err := store.Get("anthropic-oauth"); err != nil {
		return err
	} else if ok {
		_ = authstore.LogoutAnthropic(context.Background(), existing.RefreshToken)
	}
	if err := store.Delete("anthropic-oauth"); err != nil {
		return err
	}
	fmt.Fprintln(os.Stdout, "Logged out from Anthropic")
	return nil
}

func runListModels() error {
	paths, err := config.ResolvePaths()
	if err != nil {
		return err
	}
	registry, err := models.Load(paths.ModelsFile)
	if err != nil {
		return err
	}
	for _, model := range registry.All() {
		thinking := ""
		if model.Thinking {
			thinking = " thinking"
		}
		fmt.Fprintf(os.Stdout, "%s/%s\t%s\tcontext=%d max_output=%d%s\n",
			model.Provider, model.ID, model.Display, model.ContextWindow, model.MaxOutput, thinking)
	}
	return nil
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

	runner := agent.NewAgent(cfg)
	runner.Subscribe(func(event agent.Event) {
		if update, ok := event.(agent.MessageUpdateEvent); ok && update.Delta.TextDelta != "" {
			fmt.Fprint(os.Stdout, update.Delta.TextDelta)
		}
	})
	if err := runner.Prompt(context.Background(), prompt); err != nil {
		return err
	}
	if err := runner.WaitForIdle(context.Background()); err != nil {
		return err
	}
	if err := runner.LastError(); err != nil {
		return err
	}
	fmt.Fprintln(os.Stdout)
	return nil
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
	runner := agent.NewAgent(cfg)
	runner.Subscribe(func(event agent.Event) {
		sendEvent(rootCtx, eventCh, event)
	})

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
		Abort: runner.Abort,
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
			if err := runner.Prompt(rootCtx, text); err != nil && !errors.Is(err, context.Canceled) {
				sendEvent(rootCtx, eventCh, agent.AgentEndEvent{Reason: "error", Err: err})
			}
		}
	}
}

func sendEvent(ctx context.Context, eventCh chan<- agent.Event, event agent.Event) {
	select {
	case eventCh <- event:
	case <-ctx.Done():
	}
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
	if paths, err := config.ResolvePaths(); err == nil {
		cfg.AuthStore = authstore.New(paths.AuthFile)
	}
	return sess, cfg, nil
}

func newSessionStore() (*session.JSONLStore, error) {
	paths, err := config.ResolvePaths()
	if err != nil {
		return nil, err
	}
	return session.NewJSONLStore(paths.SessionDir), nil
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
	if model != "" {
		return model
	}
	paths, err := config.ResolvePaths()
	if err == nil {
		manager, err := config.NewManager(paths.SettingsFile)
		if err == nil {
			settings := manager.Get()
			if settings.Model != "" {
				return settings.Model
			}
		}
	}
	return defaultModel
}

func openBrowser(url string) error {
	var cmd *exec.Cmd
	switch runtime.GOOS {
	case "darwin":
		cmd = exec.Command("open", url)
	case "windows":
		cmd = exec.Command("rundll32", "url.dll,FileProtocolHandler", url)
	default:
		cmd = exec.Command("xdg-open", url)
	}
	if err := cmd.Start(); err != nil {
		fmt.Fprintf(os.Stdout, "Open this URL: %s\n", url)
		return nil
	}
	return nil
}
