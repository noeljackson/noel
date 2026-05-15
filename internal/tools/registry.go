// Package tools provides built-in tool registry implementations.
package tools

import (
	"fmt"
	"sort"
	"sync"

	"github.com/noeljackson/pi/internal/agent"
)

// Registry is a thread-safe ToolRegistry implementation.
type Registry struct {
	mu    sync.RWMutex
	tools map[string]agent.Tool
}

// NewRegistry returns an empty tool registry.
func NewRegistry() *Registry {
	return &Registry{tools: make(map[string]agent.Tool)}
}

// Register adds a tool to the registry.
func (r *Registry) Register(tool agent.Tool) error {
	if tool == nil {
		return fmt.Errorf("tool is nil")
	}
	name := tool.Name()
	if name == "" {
		return fmt.Errorf("tool name is empty")
	}

	r.mu.Lock()
	defer r.mu.Unlock()
	if r.tools == nil {
		r.tools = make(map[string]agent.Tool)
	}
	if _, exists := r.tools[name]; exists {
		return fmt.Errorf("tool %q is already registered", name)
	}
	r.tools[name] = tool
	return nil
}

// Get returns a registered tool by name.
func (r *Registry) Get(name string) (agent.Tool, bool) {
	r.mu.RLock()
	defer r.mu.RUnlock()
	tool, ok := r.tools[name]
	return tool, ok
}

// All returns every registered tool sorted by name.
func (r *Registry) All() []agent.Tool {
	r.mu.RLock()
	defer r.mu.RUnlock()

	names := make([]string, 0, len(r.tools))
	for name := range r.tools {
		names = append(names, name)
	}
	sort.Strings(names)

	result := make([]agent.Tool, 0, len(names))
	for _, name := range names {
		result = append(result, r.tools[name])
	}
	return result
}

var _ agent.ToolRegistry = (*Registry)(nil)
