package slash

import (
	"context"
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

func (r *Registry) Register(command Command) {
	if r == nil || command.Name == "" {
		return
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	r.commands[command.Name] = command
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
