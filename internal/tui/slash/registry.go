package slash

import (
	"context"
	"errors"
	"sort"
	"sync"

	"github.com/noeljackson/pi/internal/agent"
)

type ArgSpec struct {
	Name        string
	Description string
	Required    bool
}

type Command struct {
	Name        string
	Description string
	Args        []ArgSpec
	Handler     func(ctx context.Context, args string, agent *agent.Agent) (string, error)
}

type Registry struct {
	mu       sync.RWMutex
	commands map[string]Command
}

func New() *Registry {
	return &Registry{commands: map[string]Command{}}
}

func Builtins() *Registry {
	r := New()
	for _, command := range builtinCommands() {
		r.Register(command)
	}
	return r
}

func (r *Registry) Register(command Command) {
	if r == nil || command.Name == "" {
		return
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	r.commands[command.Name] = command
}

func builtinCommands() []Command {
	return []Command{
		{Name: "model", Description: "switch model", Args: []ArgSpec{{Name: "name", Description: "model id", Required: true}}, Handler: func(_ context.Context, args string, a *agent.Agent) (string, error) {
			if a == nil {
				return "", errors.New("agent is not configured")
			}
			if args == "" {
				return "", errors.New("usage: /model <name>")
			}
			return "", a.SetModel(args)
		}},
		{Name: "thinking", Description: "set thinking level", Args: []ArgSpec{{Name: "level", Description: "off, low, medium, high", Required: true}}, Handler: func(_ context.Context, args string, a *agent.Agent) (string, error) {
			if a == nil {
				return "", errors.New("agent is not configured")
			}
			if args == "" {
				return "", errors.New("usage: /thinking <level>")
			}
			return "", a.SetThinking(args)
		}},
		{Name: "compact", Description: "compact context", Handler: func(ctx context.Context, _ string, a *agent.Agent) (string, error) {
			if a == nil {
				return "", errors.New("agent is not configured")
			}
			return "", a.CompactNow(ctx)
		}},
		{Name: "fork", Description: "fork at a leaf or entry", Args: []ArgSpec{{Name: "leaf-id", Description: "entry id"}}, Handler: func(ctx context.Context, args string, a *agent.Agent) (string, error) {
			if a == nil {
				return "", errors.New("agent is not configured")
			}
			if args == "" {
				return "", errors.New("usage: /fork <entry-id>")
			}
			_, err := a.Fork(ctx, args)
			return "", err
		}},
		{Name: "clone", Description: "clone current leaf"},
		{Name: "tree", Description: "show session tree"},
		{Name: "new", Description: "start a new session"},
		{Name: "resume", Description: "resume a session", Args: []ArgSpec{{Name: "id", Description: "session id"}}},
		{Name: "reload", Description: "reload resources", Handler: func(_ context.Context, _ string, a *agent.Agent) (string, error) {
			if a == nil {
				return "", errors.New("agent is not configured")
			}
			return "", a.ReloadResources()
		}},
		{Name: "quit", Description: "quit"},
		{Name: "help", Description: "show command list"},
		{Name: "login", Description: "login to provider", Args: []ArgSpec{{Name: "provider", Description: "provider id"}}},
		{Name: "logout", Description: "logout from provider", Args: []ArgSpec{{Name: "provider", Description: "provider id"}}},
		{Name: "settings", Description: "show settings"},
		{Name: "session", Description: "show session info"},
	}
}

func (r *Registry) Lookup(name string) (Command, bool) {
	if r == nil {
		return Command{}, false
	}
	r.mu.RLock()
	defer r.mu.RUnlock()
	command, ok := r.commands[name]
	return command, ok
}

func (r *Registry) Names() []string {
	if r == nil {
		return nil
	}
	r.mu.RLock()
	defer r.mu.RUnlock()
	names := make([]string, 0, len(r.commands))
	for name := range r.commands {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}

func (r *Registry) Commands() []Command {
	if r == nil {
		return nil
	}
	r.mu.RLock()
	defer r.mu.RUnlock()
	commands := make([]Command, 0, len(r.commands))
	for _, command := range r.commands {
		commands = append(commands, command)
	}
	sort.Slice(commands, func(i, j int) bool {
		return commands[i].Name < commands[j].Name
	})
	return commands
}
