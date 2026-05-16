package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/noeljackson/pi/internal/agent"
	anthropicprovider "github.com/noeljackson/pi/internal/anthropic"
	authstore "github.com/noeljackson/pi/internal/auth"
	"github.com/noeljackson/pi/internal/cli"
	"github.com/noeljackson/pi/internal/cli/modes"
	rpcmode "github.com/noeljackson/pi/internal/cli/modes/rpc"
	"github.com/noeljackson/pi/internal/config"
	"github.com/noeljackson/pi/internal/models"
	"github.com/noeljackson/pi/internal/resources"
	"github.com/noeljackson/pi/internal/session"
	"github.com/noeljackson/pi/internal/tools"
	"github.com/noeljackson/pi/internal/tools/bash"
	filetools "github.com/noeljackson/pi/internal/tools/file"
	"github.com/noeljackson/pi/internal/tui"
)

const (
	defaultModel     = "claude-sonnet-4-6"
	defaultMaxTokens = 4096
	version          = "dev"
)

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func run() error {
	opts, err := cli.Parse(os.Args[1:])
	if err != nil {
		return err
	}
	if opts.Help {
		fmt.Fprint(os.Stdout, cli.HelpText("pi"))
		return nil
	}
	if opts.Version {
		fmt.Fprintln(os.Stdout, version)
		return nil
	}
	if opts.Offline {
		if err := os.Setenv("PI_OFFLINE", "1"); err != nil {
			return err
		}
		if err := os.Setenv("PI_SKIP_VERSION_CHECK", "1"); err != nil {
			return err
		}
	}
	if opts.Login != "" {
		return runLogin(opts.Login)
	}
	if opts.Logout != "" {
		return runLogout(opts.Logout)
	}
	if opts.ListModels {
		return runListModels()
	}

	if opts.Mode != cli.ModeRPC {
		if err := preparePrompt(&opts); err != nil {
			return err
		}
	}

	switch opts.Mode {
	case cli.ModePrint:
		return runPrint(opts)
	case cli.ModeJSON:
		return runJSON(opts)
	case cli.ModeRPC:
		return runRPC(opts)
	case cli.ModeInteractive:
		return runTUI(opts)
	default:
		return fmt.Errorf("unsupported mode %q", opts.Mode)
	}
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

func runPrint(opts cli.Options) error {
	runner, closer, err := newAgent(opts)
	if err != nil {
		return err
	}
	if closer != nil {
		defer closer.Close()
	}
	return modes.RunPrintWithAgent(context.Background(), opts, runner, os.Stdout)
}

func runJSON(opts cli.Options) error {
	runner, closer, err := newAgent(opts)
	if err != nil {
		return err
	}
	if closer != nil {
		defer closer.Close()
	}
	return modes.RunJSONWithAgent(context.Background(), opts, runner, os.Stdout)
}

func runRPC(opts cli.Options) error {
	runner, closer, err := newAgent(opts)
	if err != nil {
		return err
	}
	if closer != nil {
		defer closer.Close()
	}
	return rpcmode.Run(context.Background(), opts, runner, os.Stdin, os.Stdout)
}

func runTUI(opts cli.Options) error {
	rootCtx, cancel := context.WithCancel(context.Background())
	defer cancel()

	auth, err := authSource(opts)
	if err != nil {
		return err
	}
	sess, cfg, err := newSessionConfig(opts, auth)
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
		if opts.Prompt != "" {
			select {
			case submitCh <- opts.Prompt:
			case <-rootCtx.Done():
			}
		}
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

func preparePrompt(opts *cli.Options) error {
	parts := make([]string, 0, len(opts.Files)+2)
	if piped, err := readPipedStdin(); err != nil {
		return err
	} else if piped != "" {
		parts = append(parts, piped)
		if opts.Mode == cli.ModeInteractive {
			opts.Mode = cli.ModePrint
			opts.Print = true
		}
	}
	for _, name := range opts.Files {
		content, err := readPromptFile(name)
		if err != nil {
			return err
		}
		if content != "" {
			parts = append(parts, content)
		}
	}
	if opts.Prompt != "" {
		parts = append(parts, opts.Prompt)
	}
	opts.Prompt = strings.Join(parts, "")
	return nil
}

func readPipedStdin() (string, error) {
	stat, err := os.Stdin.Stat()
	if err != nil {
		return "", err
	}
	if stat.Mode()&os.ModeCharDevice != 0 {
		return "", nil
	}
	data, err := io.ReadAll(os.Stdin)
	if err != nil {
		return "", err
	}
	return strings.TrimSpace(string(data)), nil
}

func readPromptFile(name string) (string, error) {
	path, err := expandPath(name)
	if err != nil {
		return "", err
	}
	if !filepath.IsAbs(path) {
		abs, err := filepath.Abs(path)
		if err != nil {
			return "", err
		}
		path = abs
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return "", fmt.Errorf("read %s: %w", path, err)
	}
	if len(data) == 0 {
		return "", nil
	}
	return fmt.Sprintf("<file name=\"%s\">\n%s\n</file>\n", path, string(data)), nil
}

func newAgent(opts cli.Options) (*agent.Agent, io.Closer, error) {
	auth, err := authSource(opts)
	if err != nil {
		return nil, nil, err
	}
	sess, cfg, err := newSessionConfig(opts, auth)
	if err != nil {
		return nil, nil, err
	}
	return agent.NewAgent(cfg), sess, nil
}

func authSource(opts cli.Options) (anthropicprovider.AuthSource, error) {
	if opts.APIKey != "" {
		return anthropicprovider.APIKeyAuth{Key: opts.APIKey}, nil
	}
	return anthropicprovider.PickAuth()
}

func newSessionConfig(opts cli.Options, auth anthropicprovider.AuthSource) (*session.Session, agent.LoopConfig, error) {
	paths, err := config.ResolvePaths()
	if err != nil {
		return nil, agent.LoopConfig{}, err
	}
	if opts.Session.SessionDir != "" {
		sessionDir, err := expandPath(opts.Session.SessionDir)
		if err != nil {
			return nil, agent.LoopConfig{}, err
		}
		paths.SessionDir = sessionDir
	}
	store, err := newSessionStore(paths)
	if err != nil {
		return nil, agent.LoopConfig{}, err
	}

	var sess *session.Session
	sessionID := opts.Session.SessionID
	if sessionID == "" {
		sessionID = opts.Session.Resume
	}
	if opts.Session.Continue {
		infos, err := store.List()
		if err != nil {
			return nil, agent.LoopConfig{}, err
		}
		if len(infos) == 0 {
			return nil, agent.LoopConfig{}, errors.New("no previous session to continue")
		}
		sessionID = infos[0].ID
	}
	if sessionID == "" {
		cwd, err := os.Getwd()
		if err != nil {
			return nil, agent.LoopConfig{}, err
		}
		sess, err = store.Create(cwd)
		if err != nil {
			return nil, agent.LoopConfig{}, err
		}
	} else {
		sess, err = openSessionArg(store, sessionID)
		if err != nil {
			return nil, agent.LoopConfig{}, err
		}
	}
	if opts.Session.Fork != "" {
		if _, err := sess.ForkAt(opts.Session.Fork); err != nil {
			_ = sess.Close()
			return nil, agent.LoopConfig{}, err
		}
	}

	registry, err := builtinRegistry()
	if err != nil {
		_ = sess.Close()
		return nil, agent.LoopConfig{}, err
	}
	if opts.Tools.NoTools || opts.Tools.NoBuiltins {
		if err := registry.Activate(nil); err != nil {
			_ = sess.Close()
			return nil, agent.LoopConfig{}, err
		}
	}
	if len(opts.Tools.Allow) > 0 {
		if err := registry.Activate(opts.Tools.Allow); err != nil {
			_ = sess.Close()
			return nil, agent.LoopConfig{}, err
		}
	}
	if len(opts.Tools.Deny) > 0 {
		if err := registry.Deactivate(opts.Tools.Deny); err != nil {
			_ = sess.Close()
			return nil, agent.LoopConfig{}, err
		}
	}
	cwd, err := os.Getwd()
	if err != nil {
		_ = sess.Close()
		return nil, agent.LoopConfig{}, err
	}
	resourceLoader := &resources.ResourceLoader{
		Paths:       paths,
		ProjectRoot: cwd,
	}
	loadedResources, err := resourceLoader.Load()
	if err != nil {
		_ = sess.Close()
		return nil, agent.LoopConfig{}, err
	}
	for _, diagnostic := range loadedResources.Diagnostics {
		if diagnostic.Level == "error" {
			fmt.Fprintf(os.Stderr, "resource error: %s: %s\n", diagnostic.Source, diagnostic.Message)
		}
	}

	cfg := agent.LoopConfig{
		Provider:       anthropicprovider.NewClient(auth),
		Tools:          registry,
		Model:          modelName(opts),
		Thinking:       thinkingLevel(opts),
		Resources:      loadedResources,
		ResourceLoader: resourceLoader,
		MaxTokens:      defaultMaxTokens,
		SessionWriter:  sess,
	}
	if paths, err := config.ResolvePaths(); err == nil {
		cfg.AuthStore = authstore.New(paths.AuthFile)
	}
	return sess, cfg, nil
}

func newSessionStore(paths config.Paths) (*session.JSONLStore, error) {
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

func modelName(opts cli.Options) string {
	if opts.Model != "" {
		model, _ := splitModelThinking(opts.Model)
		return model
	}
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

func thinkingLevel(opts cli.Options) string {
	if opts.Thinking != "" {
		return opts.Thinking
	}
	if opts.Model != "" {
		_, thinking := splitModelThinking(opts.Model)
		if thinking != "" {
			return thinking
		}
	}
	paths, err := config.ResolvePaths()
	if err == nil {
		manager, err := config.NewManager(paths.SettingsFile)
		if err == nil {
			settings := manager.Get()
			if settings.Thinking != "" {
				return settings.Thinking
			}
		}
	}
	return "auto"
}

func splitModelThinking(value string) (string, string) {
	model, thinking, ok := strings.Cut(value, ":")
	if !ok || model == "" || thinking == "" {
		return value, ""
	}
	switch thinking {
	case "off", "minimal", "low", "medium", "high", "xhigh":
		return model, thinking
	default:
		return value, ""
	}
}

func sessionIDFromArg(value string) string {
	base := filepath.Base(value)
	return strings.TrimSuffix(base, ".jsonl")
}

func openSessionArg(store *session.JSONLStore, value string) (*session.Session, error) {
	if strings.Contains(value, "/") || strings.Contains(value, "\\") || strings.HasSuffix(value, ".jsonl") {
		path, err := expandPath(value)
		if err != nil {
			return nil, err
		}
		return store.OpenPath(path)
	}
	return store.Open(sessionIDFromArg(value))
}

func expandPath(path string) (string, error) {
	if path == "~" {
		return os.UserHomeDir()
	}
	if strings.HasPrefix(path, "~/") {
		home, err := os.UserHomeDir()
		if err != nil {
			return "", err
		}
		return filepath.Join(home, path[2:]), nil
	}
	return path, nil
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
