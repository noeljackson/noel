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
	mu        sync.RWMutex
	tools     map[string]agent.Tool
	sets      map[ToolSet]map[string]struct{}
	active    map[string]struct{}
	activeSet bool
}

// ToolSet identifies a curated subset of registered tools.
type ToolSet string

const (
	// ToolSetCoding includes tools for coding tasks.
	ToolSetCoding ToolSet = "coding"
	// ToolSetReadOnly includes tools that inspect files without mutating state.
	ToolSetReadOnly ToolSet = "read_only"
)

// NewRegistry returns an empty tool registry.
func NewRegistry() *Registry {
	return &Registry{
		tools: make(map[string]agent.Tool),
		sets:  make(map[ToolSet]map[string]struct{}),
	}
}

// Register adds a tool to the registry.
func (r *Registry) Register(tool agent.Tool) error {
	return r.RegisterInSet(tool)
}

// RegisterInSet adds a tool to the registry and records its tool-set memberships.
func (r *Registry) RegisterInSet(tool agent.Tool, sets ...ToolSet) error {
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
	if r.sets == nil {
		r.sets = make(map[ToolSet]map[string]struct{})
	}
	if _, exists := r.tools[name]; exists {
		return fmt.Errorf("tool %q is already registered", name)
	}
	r.tools[name] = tool
	for _, set := range sets {
		if r.sets[set] == nil {
			r.sets[set] = make(map[string]struct{})
		}
		r.sets[set][name] = struct{}{}
	}
	return nil
}

// Get returns a registered tool by name.
func (r *Registry) Get(name string) (agent.Tool, bool) {
	r.mu.RLock()
	defer r.mu.RUnlock()
	if r.activeSet {
		if _, ok := r.active[name]; !ok {
			return nil, false
		}
	}
	tool, ok := r.tools[name]
	return tool, ok
}

// All returns every active registered tool sorted by name.
func (r *Registry) All() []agent.Tool {
	r.mu.RLock()
	defer r.mu.RUnlock()

	return r.toolsByNamesLocked(r.activeNamesLocked())
}

// Registered returns every registered tool sorted by name, regardless of activation state.
func (r *Registry) Registered() []agent.Tool {
	r.mu.RLock()
	defer r.mu.RUnlock()

	names := make([]string, 0, len(r.tools))
	for name := range r.tools {
		names = append(names, name)
	}
	sort.Strings(names)
	return r.toolsByNamesLocked(names)
}

// Set returns registered tools in the requested curated set, sorted by name.
func (r *Registry) Set(name ToolSet) []agent.Tool {
	r.mu.RLock()
	defer r.mu.RUnlock()

	members := r.sets[name]
	names := make([]string, 0, len(members))
	for toolName := range members {
		if _, ok := r.tools[toolName]; ok {
			names = append(names, toolName)
		}
	}
	sort.Strings(names)
	return r.toolsByNamesLocked(names)
}

// Activate restricts which tools are available to the agent.
func (r *Registry) Activate(names []string) error {
	r.mu.Lock()
	defer r.mu.Unlock()

	active := make(map[string]struct{}, len(names))
	for _, name := range names {
		if _, ok := r.tools[name]; !ok {
			return fmt.Errorf("tool %q is not registered", name)
		}
		active[name] = struct{}{}
	}
	r.active = active
	r.activeSet = true
	return nil
}

// Deactivate removes tools from the active set.
func (r *Registry) Deactivate(names []string) error {
	r.mu.Lock()
	defer r.mu.Unlock()

	for _, name := range names {
		if _, ok := r.tools[name]; !ok {
			return fmt.Errorf("tool %q is not registered", name)
		}
	}
	if !r.activeSet {
		r.active = make(map[string]struct{}, len(r.tools))
		for name := range r.tools {
			r.active[name] = struct{}{}
		}
		r.activeSet = true
	}
	for _, name := range names {
		delete(r.active, name)
	}
	return nil
}

// Active returns the currently active tools.
func (r *Registry) Active() []agent.Tool {
	return r.All()
}

func (r *Registry) activeNamesLocked() []string {
	if !r.activeSet {
		names := make([]string, 0, len(r.tools))
		for name := range r.tools {
			names = append(names, name)
		}
		sort.Strings(names)
		return names
	}
	names := make([]string, 0, len(r.active))
	for name := range r.active {
		if _, ok := r.tools[name]; ok {
			names = append(names, name)
		}
	}
	sort.Strings(names)
	return names
}

func (r *Registry) toolsByNamesLocked(names []string) []agent.Tool {
	result := make([]agent.Tool, 0, len(names))
	for _, name := range names {
		result = append(result, r.tools[name])
	}
	return result
}

var _ agent.ToolRegistry = (*Registry)(nil)
