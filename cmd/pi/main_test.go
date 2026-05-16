package main

import (
	"context"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"sync"
	"testing"
	"time"

	"github.com/noeljackson/pi/internal/agent"
	anthropicprovider "github.com/noeljackson/pi/internal/anthropic"
	"github.com/noeljackson/pi/internal/cli"
	"github.com/noeljackson/pi/internal/cli/modes"
	"github.com/noeljackson/pi/internal/config"
	"github.com/noeljackson/pi/internal/session"
)

type testAuth struct{}

func (testAuth) Headers(context.Context) (map[string]string, error) {
	return map[string]string{"x-api-key": "test"}, nil
}

type captureProvider struct {
	mu    sync.Mutex
	tools []string
}

func (p *captureProvider) Stream(_ context.Context, req agent.StreamRequest, emit func(agent.Event)) (*agent.AssistantMessage, error) {
	names := make([]string, 0, len(req.Tools))
	for _, tool := range req.Tools {
		names = append(names, tool.Name())
	}
	sort.Strings(names)
	p.mu.Lock()
	p.tools = names
	p.mu.Unlock()
	emit(agent.MessageStartEvent{MessageID: "msg", Role: agent.RoleAssistant, Model: req.Model})
	emit(agent.MessageEndEvent{
		MessageID:    "msg",
		FinalContent: []agent.Content{agent.TextContent{Text: "ok"}},
		StopReason:   agent.StopEndTurn.String(),
	})
	return &agent.AssistantMessage{
		Content:    []agent.Content{agent.TextContent{Text: "ok"}},
		StopReason: agent.StopEndTurn,
		Model:      req.Model,
	}, nil
}

func resetMainHooks(t *testing.T) {
	t.Helper()
	oldPickAuth := pickAuthFn
	oldNewAgent := newAgentFn
	oldListModels := runListModelsFn
	oldLogin := runLoginFn
	oldLogout := runLogoutFn
	oldPrint := runPrintModeFn
	oldJSON := runJSONModeFn
	oldRPC := runRPCModeFn
	oldTUI := runTUIModeFn
	t.Cleanup(func() {
		pickAuthFn = oldPickAuth
		newAgentFn = oldNewAgent
		runListModelsFn = oldListModels
		runLoginFn = oldLogin
		runLogoutFn = oldLogout
		runPrintModeFn = oldPrint
		runJSONModeFn = oldJSON
		runRPCModeFn = oldRPC
		runTUIModeFn = oldTUI
	})
}

func TestRunWithArgsDispatchesModes(t *testing.T) {
	for _, tc := range []struct {
		name string
		args []string
		want string
	}{
		{name: "print", args: []string{"--print", "hello", "--quiet-startup"}, want: "print"},
		{name: "headless", args: []string{"--headless", "hello", "--quiet-startup"}, want: "print"},
		{name: "json", args: []string{"--mode", "json", "hello", "--quiet-startup"}, want: "json"},
		{name: "rpc", args: []string{"--mode", "rpc", "--quiet-startup"}, want: "rpc"},
		{name: "interactive", args: []string{"--quiet-startup"}, want: "interactive"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			resetMainHooks(t)
			t.Setenv("PI_AGENT_DIR", t.TempDir())
			pickAuthFn = func() (anthropicprovider.AuthSource, error) {
				return testAuth{}, nil
			}
			var got string
			runPrintModeFn = func(_ context.Context, opts cli.Options, _ *agent.Agent, _ io.Writer) error {
				got = "print"
				if opts.Prompt != "hello" {
					t.Fatalf("print prompt = %q", opts.Prompt)
				}
				return nil
			}
			runJSONModeFn = func(_ context.Context, opts cli.Options, _ *agent.Agent, _ io.Writer) error {
				got = "json"
				if opts.Prompt != "hello" {
					t.Fatalf("json prompt = %q", opts.Prompt)
				}
				return nil
			}
			runRPCModeFn = func(_ context.Context, _ cli.Options, _ *agent.Agent, _ io.Reader, _ io.Writer) error {
				got = "rpc"
				return nil
			}
			runTUIModeFn = func(_ cli.Options, _ *agent.Agent, _ agent.LoopConfig, _ *session.Session, _ config.Paths) error {
				got = "interactive"
				return nil
			}
			if err := runWithArgs(tc.args); err != nil {
				t.Fatal(err)
			}
			if got != tc.want {
				t.Fatalf("dispatched %q, want %q", got, tc.want)
			}
		})
	}
}

func TestActionFlagsShortCircuitBeforeAgentConstruction(t *testing.T) {
	for _, tc := range []struct {
		name string
		args []string
	}{
		{name: "list models", args: []string{"--list-models", "--quiet-startup"}},
		{name: "login", args: []string{"--login", "anthropic", "--quiet-startup"}},
		{name: "logout", args: []string{"--logout", "anthropic", "--quiet-startup"}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			resetMainHooks(t)
			t.Setenv("PI_AGENT_DIR", t.TempDir())
			pickAuthFn = func() (anthropicprovider.AuthSource, error) {
				t.Fatal("auth should not be picked for action flags")
				return nil, nil
			}
			newAgentFn = func(agent.LoopConfig) *agent.Agent {
				t.Fatal("agent should not be constructed for action flags")
				return nil
			}
			runListModelsFn = func() error { return nil }
			runLoginFn = func(string) error { return nil }
			runLogoutFn = func(string) error { return nil }
			if err := runWithArgs(tc.args); err != nil {
				t.Fatal(err)
			}
		})
	}
}

func TestNewSessionConfigResolvesContinueResumeAndFork(t *testing.T) {
	dir := t.TempDir()
	paths := config.Paths{
		AgentDir:     t.TempDir(),
		SessionDir:   dir,
		AuthFile:     t.TempDir() + "/auth.json",
		SettingsFile: t.TempDir() + "/settings.json",
		ModelsFile:   t.TempDir() + "/models.json",
		ResourcesDir: t.TempDir(),
		ThemesDir:    t.TempDir(),
	}
	store := session.NewJSONLStore(dir)
	oldSess, err := store.Create("/tmp/old")
	if err != nil {
		t.Fatal(err)
	}
	oldID := oldSess.ID()
	if err := oldSess.Close(); err != nil {
		t.Fatal(err)
	}
	newSess, err := store.Create("/tmp/new")
	if err != nil {
		t.Fatal(err)
	}
	if err := newSess.AppendMessage(agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "fork point"}}}); err != nil {
		t.Fatal(err)
	}
	forkPoint := newSess.LeafID()
	newID := newSess.ID()
	if err := newSess.Close(); err != nil {
		t.Fatal(err)
	}
	oldTime := time.Now().Add(-time.Hour)
	if err := os.Chtimes(sessionPath(dir, oldID), oldTime, oldTime); err != nil {
		t.Fatal(err)
	}
	newTime := time.Now()
	if err := os.Chtimes(sessionPath(dir, newID), newTime, newTime); err != nil {
		t.Fatal(err)
	}

	sess, _, cleanup, err := newSessionConfig(cli.Options{Session: cli.SessionFlag{Continue: true}}, testAuth{}, paths)
	if err != nil {
		t.Fatal(err)
	}
	if sess.ID() != newID {
		t.Fatalf("--continue opened %s, want %s", sess.ID(), newID)
	}
	cleanup()
	_ = sess.Close()

	sess, _, cleanup, err = newSessionConfig(cli.Options{Session: cli.SessionFlag{Resume: newID[:8]}}, testAuth{}, paths)
	if err != nil {
		t.Fatal(err)
	}
	if sess.ID() != newID {
		t.Fatalf("--resume prefix opened %s, want %s", sess.ID(), newID)
	}
	cleanup()
	_ = sess.Close()

	sess, _, cleanup, err = newSessionConfig(cli.Options{Session: cli.SessionFlag{Resume: newID, Fork: forkPoint}}, testAuth{}, paths)
	if err != nil {
		t.Fatal(err)
	}
	if sess.LeafID() == forkPoint {
		t.Fatal("--fork did not move to a new leaf")
	}
	cleanup()
	_ = sess.Close()
}

func TestNewSessionConfigToolFiltering(t *testing.T) {
	paths := config.Paths{
		AgentDir:     t.TempDir(),
		SessionDir:   t.TempDir(),
		AuthFile:     t.TempDir() + "/auth.json",
		SettingsFile: t.TempDir() + "/settings.json",
		ModelsFile:   t.TempDir() + "/models.json",
		ResourcesDir: t.TempDir(),
		ThemesDir:    t.TempDir(),
	}
	sess, cfg, cleanup, err := newSessionConfig(cli.Options{Session: cli.SessionFlag{NoSession: true}, Tools: cli.ToolsFlag{NoBuiltins: true}}, testAuth{}, paths)
	if err != nil {
		t.Fatal(err)
	}
	if got := cfg.Tools.All(); len(got) != 0 {
		t.Fatalf("--no-builtin-tools registered %d tools, want 0", len(got))
	}
	cleanup()
	_ = sess.Close()

	provider := &captureProvider{}
	sess, cfg, cleanup, err = newSessionConfig(cli.Options{Session: cli.SessionFlag{NoSession: true}}, testAuth{}, paths)
	if err != nil {
		t.Fatal(err)
	}
	cfg.Provider = provider
	runner := agent.NewAgent(cfg)
	if err := modes.ApplyOptions(runner, cli.Options{Tools: cli.ToolsFlag{Allow: []string{"bash"}}}); err != nil {
		t.Fatal(err)
	}
	if err := runner.Prompt(context.Background(), "hello"); err != nil {
		t.Fatal(err)
	}
	if err := runner.WaitForIdle(context.Background()); err != nil {
		t.Fatal(err)
	}
	if err := runner.LastError(); err != nil {
		t.Fatal(err)
	}
	provider.mu.Lock()
	got := append([]string(nil), provider.tools...)
	provider.mu.Unlock()
	if !reflect.DeepEqual(got, []string{"bash"}) {
		t.Fatalf("--tools bash sent tools %#v, want [bash]", got)
	}
	cleanup()
	_ = sess.Close()
}

func sessionPath(dir, id string) string {
	return filepath.Join(dir, id+".jsonl")
}
