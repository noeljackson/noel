package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sort"
	"strings"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/noeljackson/pi/internal/agent"
	anthropicprovider "github.com/noeljackson/pi/internal/anthropic"
	authstore "github.com/noeljackson/pi/internal/auth"
	"github.com/noeljackson/pi/internal/cli"
	"github.com/noeljackson/pi/internal/cli/modes"
	"github.com/noeljackson/pi/internal/cli/modes/rpc"
	"github.com/noeljackson/pi/internal/config"
	"github.com/noeljackson/pi/internal/diagnostics"
	"github.com/noeljackson/pi/internal/exporter"
	"github.com/noeljackson/pi/internal/models"
	"github.com/noeljackson/pi/internal/observability"
	"github.com/noeljackson/pi/internal/resources"
	"github.com/noeljackson/pi/internal/session"
	"github.com/noeljackson/pi/internal/timings"
	"github.com/noeljackson/pi/internal/tools"
	"github.com/noeljackson/pi/internal/tools/bash"
	filetools "github.com/noeljackson/pi/internal/tools/file"
	"github.com/noeljackson/pi/internal/tools/more"
	"github.com/noeljackson/pi/internal/tools/task"
	"github.com/noeljackson/pi/internal/tools/todo"
	"github.com/noeljackson/pi/internal/tui"
	"github.com/noeljackson/pi/internal/tui/slash"
	"github.com/noeljackson/pi/internal/version"
)

const (
	defaultModel     = "claude-sonnet-4-6"
	defaultMaxTokens = 4096
)

var (
	pickAuthFn      = anthropicprovider.PickAuth
	newAgentFn      = agent.NewAgent
	runListModelsFn = runListModels
	runLoginFn      = runLogin
	runLogoutFn     = runLogout
	runPrintModeFn  = func(ctx context.Context, opts cli.Options, runner *agent.Agent, out io.Writer) error {
		return modes.RunPrintWithAgent(ctx, opts, runner, out)
	}
	runJSONModeFn = func(ctx context.Context, opts cli.Options, runner *agent.Agent, out io.Writer) error {
		return modes.RunJSONWithAgent(ctx, opts, runner, out)
	}
	runRPCModeFn    = rpc.Run
	runTUIModeFn    = runTUI
	activeCollector *diagnostics.Collector
	activeTimings   *timings.Timings
	activeLogger    *slog.Logger
)

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func run() error {
	return runWithArgs(os.Args[1:])
}

func runWithArgs(args []string) error {
	opts, err := cli.Parse(args)
	if err != nil {
		return err
	}
	if opts.Help {
		cli.PrintHelp(os.Stdout)
		return nil
	}
	if opts.Version {
		printVersion()
		return nil
	}

	paths, err := config.ResolvePaths()
	if err != nil {
		return err
	}
	if opts.Session.SessionDir != "" {
		paths.SessionDir = opts.Session.SessionDir
	}
	collector := diagnostics.New()
	timingCollector := timings.New()
	logger := observability.NewLogger(observability.Options{
		LogDir:    filepath.Join(paths.AgentDir, "logs"),
		Level:     slog.LevelInfo,
		Collector: collector,
	})
	defer logger.Close()
	slog.SetDefault(logger.Slog())
	activeCollector = collector
	activeTimings = timingCollector
	activeLogger = logger.Slog()
	defer func() {
		activeCollector = nil
		activeTimings = nil
		activeLogger = nil
	}()

	quietStartup := opts.QuietStartup || quietStartupFromSettings(paths)
	opts.QuietStartup = quietStartup
	for _, diagnostic := range diagnostics.StartupDiagnostics(paths) {
		collector.Add(diagnostic)
	}
	if !quietStartup {
		printStartupDiagnostics(os.Stderr, collector.Recent(100))
	}
	if opts.StartupProbe {
		fmt.Fprintln(os.Stdout, "prompt-ready")
		return nil
	}
	if opts.ListModels {
		return runListModelsFn()
	}
	if opts.Login != "" {
		return runLoginFn(opts.Login)
	}
	if opts.Logout != "" {
		return runLogoutFn(opts.Logout)
	}

	if err := prependFilesToPrompt(&opts); err != nil {
		return err
	}

	auth, err := pickAuthFn()
	if err != nil {
		return err
	}

	sess, cfg, cleanup, err := newSessionConfig(opts, auth, paths)
	if err != nil {
		return err
	}
	defer cleanup()
	defer sess.Close()

	runner := newAgentFn(cfg)
	if err := modes.ApplyOptions(runner, opts); err != nil {
		return err
	}

	switch opts.Mode {
	case cli.ModePrint:
		return runPrintModeFn(context.Background(), opts, runner, os.Stdout)
	case cli.ModeJSON:
		return runJSONModeFn(context.Background(), opts, runner, os.Stdout)
	case cli.ModeRPC:
		return runRPCModeFn(context.Background(), opts, runner, os.Stdin, os.Stdout)
	case cli.ModeInteractive, "":
		return runTUIModeFn(opts, runner, cfg, sess, paths)
	default:
		return fmt.Errorf("unknown mode: %s", opts.Mode)
	}
}

func printVersion() {
	fmt.Fprintln(os.Stdout, version.BuildVersion)
}

func prependFilesToPrompt(opts *cli.Options) error {
	if opts == nil || len(opts.Files) == 0 {
		return nil
	}
	var builder strings.Builder
	for _, file := range opts.Files {
		path, err := resolveInputFile(file)
		if err != nil {
			return err
		}
		info, err := os.Stat(path)
		if err != nil {
			return fmt.Errorf("file %s: %w", path, err)
		}
		if info.IsDir() {
			return fmt.Errorf("file %s is a directory", path)
		}
		if info.Size() == 0 {
			continue
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return fmt.Errorf("read file %s: %w", path, err)
		}
		fmt.Fprintf(&builder, "<file name=\"%s\">\n%s\n</file>\n", path, string(data))
	}
	if builder.Len() == 0 {
		return nil
	}
	if opts.Prompt != "" {
		builder.WriteString(opts.Prompt)
	}
	opts.Prompt = builder.String()
	return nil
}

func resolveInputFile(path string) (string, error) {
	if strings.HasPrefix(path, "~/") {
		home, err := os.UserHomeDir()
		if err != nil {
			return "", err
		}
		path = filepath.Join(home, path[2:])
	}
	return filepath.Abs(path)
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

func runTUI(opts cli.Options, runner *agent.Agent, cfg agent.LoopConfig, sess *session.Session, paths config.Paths) error {
	rootCtx, cancel := context.WithCancel(context.Background())
	defer cancel()

	messages, err := sess.Messages()
	if err != nil {
		return err
	}

	collector := activeCollector
	if collector == nil {
		collector = diagnostics.New()
	}
	timingCollector := cfg.Timings
	if timingCollector == nil {
		timingCollector = activeTimings
	}
	if timingCollector == nil {
		timingCollector = timings.New()
	}
	eventCh := make(chan agent.Event, 256)
	submitCh := make(chan string, 32)
	runner.Subscribe(func(event agent.Event) {
		sendEvent(rootCtx, eventCh, event)
	})
	commands := slash.New()
	registerSlashCommands(commands, collector, timingCollector, sess, session.NewJSONLStore(paths.SessionDir))
	if !opts.QuietStartup && telemetryEnabled(paths) {
		go checkVersionNotice(rootCtx, eventCh)
	}

	model := tui.New(tui.Options{
		EventSource: eventCh,
		Messages:    messages,
		Model:       cfg.Model,
		Resources:   cfg.Resources,
		Agent:       runner,
		OpenBrowser: openBrowser,
		Slash:       commands,
		Timings:     timingCollector,
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

func printStartupDiagnostics(file *os.File, items []diagnostics.Diagnostic) {
	for _, diagnostic := range items {
		if diagnostic.Level < diagnostics.Warning {
			continue
		}
		source := diagnostic.Source
		if source == "" {
			source = "startup"
		}
		fmt.Fprintf(file, "%s: %s: %s\n", diagnostic.Level.String(), source, diagnostic.Message)
	}
}

func quietStartupFromSettings(paths config.Paths) bool {
	manager, err := config.NewManager(paths.SettingsFile)
	if err != nil {
		return false
	}
	return manager.Get().QuietStartup
}

func telemetryEnabled(paths config.Paths) bool {
	manager, err := config.NewManager(paths.SettingsFile)
	if err != nil {
		return false
	}
	return manager.Get().Telemetry
}

func checkVersionNotice(ctx context.Context, eventCh chan<- agent.Event) {
	if version.BuildVersion == "dev" {
		return
	}
	checkCtx, cancel := context.WithTimeout(ctx, 2*time.Second)
	defer cancel()
	status, err := version.Check(checkCtx, "")
	if err != nil || status == nil || !status.Available {
		return
	}
	messageID := fmt.Sprintf("version-%d", time.Now().UnixNano())
	sendEvent(ctx, eventCh, agent.MessageStartEvent{MessageID: messageID, Role: agent.RoleAssistant, Model: "system"})
	sendEvent(ctx, eventCh, agent.MessageEndEvent{
		MessageID:    messageID,
		FinalContent: []agent.Content{agent.TextContent{Text: fmt.Sprintf("Update available: %s (current %s)", status.Latest, status.Current)}},
	})
}

func registerSlashCommands(registry *slash.Registry, collector *diagnostics.Collector, timingCollector *timings.Timings, sess *session.Session, store *session.JSONLStore) {
	registry.Register(slash.Command{
		Name:        "diagnostics",
		Description: "show recent diagnostics",
		Handler: func(_ context.Context, _ string, _ *agent.Agent) (string, error) {
			return formatDiagnostics(collector.Recent(50)), nil
		},
	})
	registry.Register(slash.Command{
		Name:        "timings",
		Description: "show timing summary",
		Handler: func(_ context.Context, _ string, _ *agent.Agent) (string, error) {
			return formatTimings(timingCollector.Summary()), nil
		},
	})
	registry.Register(slash.Command{
		Name:        "export",
		Description: "export current session as JSONL",
		Args:        []slash.ArgSpec{{Name: "path", Description: "output path"}},
		Handler: func(_ context.Context, args string, _ *agent.Agent) (string, error) {
			path := strings.TrimSpace(args)
			if path == "" {
				path = fmt.Sprintf("pi-session-%s.jsonl", sess.ID())
			}
			if err := exporter.ExportPath(sess, path); err != nil {
				return "", err
			}
			return "Exported session to " + path, nil
		},
	})
	registry.Register(slash.Command{
		Name:        "import",
		Description: "import a JSONL session",
		Args:        []slash.ArgSpec{{Name: "path", Description: "input path", Required: true}},
		Handler: func(_ context.Context, args string, _ *agent.Agent) (string, error) {
			path := strings.TrimSpace(args)
			if path == "" {
				return "", errors.New("usage: /import <path>")
			}
			id, err := exporter.ImportPath(store, path)
			if err != nil {
				return "", err
			}
			return "Imported session " + id, nil
		},
	})
	registry.Register(slash.Command{
		Name:        "export-html",
		Description: "export current session as HTML",
		Handler: func(_ context.Context, _ string, _ *agent.Agent) (string, error) {
			return "HTML export is deferred; see EXPORT_HTML_FOLLOWUP.md", nil
		},
	})
}

func formatDiagnostics(items []diagnostics.Diagnostic) string {
	if len(items) == 0 {
		return "No diagnostics recorded."
	}
	lines := make([]string, 0, len(items))
	for _, item := range items {
		source := item.Source
		if source == "" {
			source = "unknown"
		}
		lines = append(lines, fmt.Sprintf("%s %s: %s", item.Level.String(), source, item.Message))
	}
	return strings.Join(lines, "\n")
}

func formatTimings(summary map[string]timings.Stats) string {
	if len(summary) == 0 {
		return "No timings recorded."
	}
	names := make([]string, 0, len(summary))
	for name := range summary {
		names = append(names, name)
	}
	sort.Strings(names)
	lines := []string{"name count total mean p95 max"}
	for _, name := range names {
		stats := summary[name]
		lines = append(lines, fmt.Sprintf("%s %d %s %s %s %s",
			name,
			stats.Count,
			stats.Total.Round(time.Millisecond),
			stats.Mean.Round(time.Millisecond),
			stats.P95.Round(time.Millisecond),
			stats.Max.Round(time.Millisecond),
		))
	}
	return strings.Join(lines, "\n")
}

func newSessionConfig(opts cli.Options, auth anthropicprovider.AuthSource, paths config.Paths) (*session.Session, agent.LoopConfig, func(), error) {
	store, err := newSessionStore(paths)
	if err != nil {
		return nil, agent.LoopConfig{}, nil, err
	}
	cwd, err := os.Getwd()
	if err != nil {
		return nil, agent.LoopConfig{}, nil, err
	}

	var sess *session.Session
	opened := false
	switch {
	case opts.Session.NoSession:
		sess = session.NewInMemorySession(cwd)
	case opts.Session.Continue:
		info, ok, err := store.MostRecent()
		if err != nil {
			return nil, agent.LoopConfig{}, nil, err
		}
		if ok {
			sess, err = store.Open(info.ID)
			opened = err == nil
		} else {
			sess, err = store.Create(cwd)
		}
		if err != nil {
			return nil, agent.LoopConfig{}, nil, err
		}
	case opts.Session.Resume != "":
		sess, err = openSessionByIDOrPrefix(store, opts.Session.Resume)
		if err != nil {
			return nil, agent.LoopConfig{}, nil, err
		}
		opened = true
	case opts.Session.SessionID != "":
		sess, err = store.Open(opts.Session.SessionID)
		if err != nil {
			return nil, agent.LoopConfig{}, nil, err
		}
		opened = true
	default:
		sess, err = store.Create(cwd)
		if err != nil {
			return nil, agent.LoopConfig{}, nil, err
		}
	}
	if opts.Session.Fork != "" {
		if !opened {
			_ = sess.Close()
			return nil, agent.LoopConfig{}, nil, errors.New("--fork requires an existing session selected by --continue, --resume, or --session")
		}
		_, err := sess.ForkAt(opts.Session.Fork)
		if err != nil {
			_ = sess.Close()
			return nil, agent.LoopConfig{}, nil, err
		}
	}

	cfg, cleanup, err := loopConfigForSession(sess, paths, store, auth, cwd, activeCollector, activeTimings, activeLogger, opts.Tools.NoBuiltins)
	if err != nil {
		cleanup()
		_ = sess.Close()
		return nil, agent.LoopConfig{}, nil, err
	}
	return sess, cfg, cleanup, nil
}

func openSessionByIDOrPrefix(store *session.JSONLStore, id string) (*session.Session, error) {
	sess, err := store.Open(id)
	if err == nil {
		return sess, nil
	}
	infos, listErr := store.List()
	if listErr != nil {
		return nil, listErr
	}
	matches := make([]session.SessionInfo, 0, 1)
	for _, info := range infos {
		if strings.HasPrefix(info.ID, id) {
			matches = append(matches, info)
		}
	}
	switch len(matches) {
	case 0:
		return nil, err
	case 1:
		return store.Open(matches[0].ID)
	default:
		return nil, fmt.Errorf("session id prefix %q is ambiguous", id)
	}
}

func loopConfigForSession(sess *session.Session, paths config.Paths, store *session.JSONLStore, auth anthropicprovider.AuthSource, cwd string, collector *diagnostics.Collector, timingCollector *timings.Timings, logger *slog.Logger, noBuiltins bool) (agent.LoopConfig, func(), error) {
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
	registry := tools.NewRegistry()
	if !noBuiltins {
		var err error
		registry, err = builtinRegistry(spawner, todo.NewSessionStore(sess), moreBuffer)
		if err != nil {
			return agent.LoopConfig{}, cleanup, err
		}
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
		if collector != nil {
			collector.Add(diagnostics.Diagnostic{
				Level:   diagnostics.ParseLevel(diagnostic.Level),
				Source:  diagnostic.Source,
				Message: diagnostic.Message,
				Time:    time.Now().UTC(),
			})
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
		Timings:        timingCollector,
		Logger:         logger,
		AfterToolCall: func(_ context.Context, call agent.ToolUseContent, result agent.ToolResult) {
			storeToolOutput(moreBuffer, call, result)
		},
	}
	cfg.AuthStore = authstore.New(paths.AuthFile)
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

	cfg, cleanup, err := loopConfigForSession(sess, s.paths, s.store, s.auth, cwd, nil, nil, nil, false)
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
