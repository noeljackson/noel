package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/noeljackson/pi/internal/agent"
	anthropicprovider "github.com/noeljackson/pi/internal/anthropic"
	authstore "github.com/noeljackson/pi/internal/auth"
	"github.com/noeljackson/pi/internal/config"
	"github.com/noeljackson/pi/internal/models"
	"github.com/noeljackson/pi/internal/resources"
	"github.com/noeljackson/pi/internal/session"
	"github.com/noeljackson/pi/internal/tools"
	"github.com/noeljackson/pi/internal/tools/bash"
	filetools "github.com/noeljackson/pi/internal/tools/file"
	"github.com/noeljackson/pi/internal/tools/more"
	"github.com/noeljackson/pi/internal/tools/task"
	"github.com/noeljackson/pi/internal/tools/todo"
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

	sess, cfg, cleanup, err := newSessionConfig("", auth)
	if err != nil {
		return err
	}
	defer cleanup()
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

	sess, cfg, cleanup, err := newSessionConfig(resumeID, auth)
	if err != nil {
		return err
	}
	defer cleanup()
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
		Resources:   cfg.Resources,
		Agent:       runner,
		OpenBrowser: openBrowser,
		Submit: func(text string) {
			select {
			case submitCh <- text:
			case <-rootCtx.Done():
			}
		},
		Abort:  runner.Abort,
		Logout: runLogout,
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

func newSessionConfig(resumeID string, auth anthropicprovider.AuthSource) (*session.Session, agent.LoopConfig, func(), error) {
	paths, err := config.ResolvePaths()
	if err != nil {
		return nil, agent.LoopConfig{}, nil, err
	}
	store, err := newSessionStore(paths)
	if err != nil {
		return nil, agent.LoopConfig{}, nil, err
	}

	var sess *session.Session
	if resumeID == "" {
		cwd, err := os.Getwd()
		if err != nil {
			return nil, agent.LoopConfig{}, nil, err
		}
		sess, err = store.Create(cwd)
		if err != nil {
			return nil, agent.LoopConfig{}, nil, err
		}
	} else {
		sess, err = store.Open(resumeID)
		if err != nil {
			return nil, agent.LoopConfig{}, nil, err
		}
	}

	cwd, err := os.Getwd()
	if err != nil {
		_ = sess.Close()
		return nil, agent.LoopConfig{}, nil, err
	}
	cfg, cleanup, err := loopConfigForSession(sess, paths, store, auth, cwd)
	if err != nil {
		cleanup()
		_ = sess.Close()
		return nil, agent.LoopConfig{}, nil, err
	}
	return sess, cfg, cleanup, nil
}

func loopConfigForSession(sess *session.Session, paths config.Paths, store *session.JSONLStore, auth anthropicprovider.AuthSource, cwd string) (agent.LoopConfig, func(), error) {
	moreBuffer := more.NewDiskBuffer(toolOutputDir(paths, sess.ID()))
	cleanup := func() {
		_ = moreBuffer.Cleanup()
	}
	spawner := &cliTaskSpawner{
		paths: paths,
		store: store,
		auth:  auth,
		cwd:   cwd,
	}
	registry, err := builtinRegistry(spawner, todo.NewSessionStore(sess), moreBuffer)
	if err != nil {
		return agent.LoopConfig{}, cleanup, err
	}
	resourceLoader := &resources.ResourceLoader{
		Paths:       paths,
		ProjectRoot: cwd,
	}
	loadedResources, err := resourceLoader.Load()
	if err != nil {
		return agent.LoopConfig{}, cleanup, err
	}
	for _, diagnostic := range loadedResources.Diagnostics {
		if diagnostic.Level == "error" {
			fmt.Fprintf(os.Stderr, "resource error: %s: %s\n", diagnostic.Source, diagnostic.Message)
		}
	}

	cfg := agent.LoopConfig{
		Provider:       anthropicprovider.NewClient(auth),
		Tools:          registry,
		Model:          modelName(),
		SessionID:      sess.ID(),
		Resources:      loadedResources,
		ResourceLoader: resourceLoader,
		MaxTokens:      defaultMaxTokens,
		SessionWriter:  sess,
		AfterToolCall: func(_ context.Context, call agent.ToolUseContent, result agent.ToolResult) {
			storeToolOutput(moreBuffer, call, result)
		},
	}
	if paths, err := config.ResolvePaths(); err == nil {
		cfg.AuthStore = authstore.New(paths.AuthFile)
	}
	return cfg, cleanup, nil
}

func newSessionStore(paths config.Paths) (*session.JSONLStore, error) {
	return session.NewJSONLStore(paths.SessionDir), nil
}

func builtinRegistry(taskSpawner task.Spawner, todoStore todo.Store, moreBuffer more.Buffer) (*tools.Registry, error) {
	registry := tools.NewRegistry()
	registrations := []struct {
		tool agent.Tool
		sets []tools.ToolSet
	}{
		{tool: bash.NewTool(), sets: []tools.ToolSet{tools.ToolSetCoding}},
		{tool: task.NewTool(taskSpawner), sets: []tools.ToolSet{tools.ToolSetCoding}},
		{tool: todo.NewTool(todoStore), sets: []tools.ToolSet{tools.ToolSetCoding, tools.ToolSetReadOnly}},
		{tool: more.NewTool(moreBuffer), sets: []tools.ToolSet{tools.ToolSetCoding, tools.ToolSetReadOnly}},
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

type cliTaskSpawner struct {
	paths config.Paths
	store *session.JSONLStore
	auth  anthropicprovider.AuthSource
	cwd   string
}

func (s *cliTaskSpawner) Spawn(ctx context.Context, req task.SpawnRequest) (task.Result, error) {
	if s == nil || s.store == nil {
		return task.Result{}, errors.New("task spawner is not configured")
	}
	cwd := s.cwd
	if cwd == "" {
		var err error
		cwd, err = os.Getwd()
		if err != nil {
			return task.Result{}, err
		}
	}
	start := time.Now()
	sess, err := s.store.Create(cwd)
	if err != nil {
		return task.Result{}, err
	}
	defer sess.Close()

	cfg, cleanup, err := loopConfigForSession(sess, s.paths, s.store, s.auth, cwd)
	if err != nil {
		cleanup()
		return task.Result{SessionID: sess.ID()}, err
	}
	defer cleanup()
	if req.Model != "" {
		cfg.Model = req.Model
	}
	if req.SystemPrompt != "" {
		cfg.SystemPrompt = req.SystemPrompt
	}
	if req.MaxTurns > 0 {
		cfg.MaxTurns = req.MaxTurns
	}
	if len(req.Tools) > 0 {
		if err := validateTools(cfg.Tools, req.Tools); err != nil {
			return task.Result{SessionID: sess.ID()}, err
		}
		cfg.ActiveTools = append([]string(nil), req.Tools...)
	}

	runner := agent.NewAgent(cfg)
	if err := runner.Prompt(ctx, req.Prompt); err != nil {
		return task.Result{SessionID: sess.ID()}, err
	}
	if err := runner.WaitForIdle(ctx); err != nil {
		return task.Result{SessionID: sess.ID()}, err
	}
	if err := runner.LastError(); err != nil {
		return task.Result{SessionID: sess.ID()}, err
	}
	output, inputTokens, outputTokens, err := finalAssistantOutput(sess)
	if err != nil {
		return task.Result{SessionID: sess.ID()}, err
	}
	return task.Result{
		Output:       output,
		SessionID:    sess.ID(),
		DurationMS:   int(time.Since(start).Milliseconds()),
		InputTokens:  inputTokens,
		OutputTokens: outputTokens,
	}, nil
}

func validateTools(registry agent.ToolRegistry, names []string) error {
	for _, name := range names {
		if _, ok := registry.Get(name); !ok {
			return fmt.Errorf("tool %q is not registered", name)
		}
	}
	return nil
}

func finalAssistantOutput(sess *session.Session) (string, int, int, error) {
	messages, err := sess.Messages()
	if err != nil {
		return "", 0, 0, err
	}
	for i := len(messages) - 1; i >= 0; i-- {
		var msg agent.AssistantMessage
		switch typed := messages[i].(type) {
		case agent.AssistantMessage:
			msg = typed
		case *agent.AssistantMessage:
			msg = *typed
		default:
			continue
		}
		return textFromContent(msg.Content), msg.Usage.InputTokens, msg.Usage.OutputTokens, nil
	}
	return "", 0, 0, nil
}

func storeToolOutput(buffer more.Buffer, call agent.ToolUseContent, result agent.ToolResult) {
	if buffer == nil || call.ID == "" {
		return
	}
	text := fullOutputFromDetails(result.Details)
	if text == "" {
		text = textFromContent(result.Content)
	}
	if text == "" {
		return
	}
	_ = buffer.Put(call.ID, text)
}

func fullOutputFromDetails(raw json.RawMessage) string {
	if len(raw) == 0 {
		return ""
	}
	var details struct {
		OutputFile string `json:"outputFile"`
	}
	if err := json.Unmarshal(raw, &details); err != nil || details.OutputFile == "" {
		return ""
	}
	data, err := os.ReadFile(details.OutputFile)
	if err != nil {
		return ""
	}
	return string(data)
}

func textFromContent(content []agent.Content) string {
	var builder strings.Builder
	for _, block := range content {
		var value string
		switch typed := block.(type) {
		case agent.TextContent:
			value = typed.Text
		case *agent.TextContent:
			value = typed.Text
		default:
			continue
		}
		if builder.Len() > 0 {
			builder.WriteString("\n")
		}
		builder.WriteString(value)
	}
	return builder.String()
}

func toolOutputDir(paths config.Paths, sessionID string) string {
	return filepath.Join(paths.AgentDir, "tool-outputs", sessionID)
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
