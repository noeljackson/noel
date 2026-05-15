package tools

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

type testTool struct {
	name string
}

func (t testTool) Name() string {
	return t.name
}

func (testTool) Description() string {
	return "test tool"
}

func (testTool) Schema() json.RawMessage {
	return json.RawMessage(`{"type":"object"}`)
}

func (testTool) ParallelSafe() bool {
	return true
}

func (t testTool) Execute(context.Context, json.RawMessage, agent.ToolCallContext) (agent.ToolResult, error) {
	return agent.ToolResult{Content: []agent.Content{agent.TextContent{Text: t.name}}}, nil
}

func TestRegistryRegisterInSetAndSet(t *testing.T) {
	registry := NewRegistry()
	if err := registry.RegisterInSet(testTool{name: "read"}, ToolSetReadOnly, ToolSetCoding); err != nil {
		t.Fatal(err)
	}
	if err := registry.RegisterInSet(testTool{name: "bash"}, ToolSetCoding); err != nil {
		t.Fatal(err)
	}

	if got := toolNames(registry.Set(ToolSetReadOnly)); !sameNames(got, []string{"read"}) {
		t.Fatalf("read-only set = %v", got)
	}
	if got := toolNames(registry.Set(ToolSetCoding)); !sameNames(got, []string{"bash", "read"}) {
		t.Fatalf("coding set = %v", got)
	}
}

func TestRegistryActivateAndAll(t *testing.T) {
	registry := NewRegistry()
	registerTestTools(t, registry, "bash", "read", "write")

	if got := toolNames(registry.All()); !sameNames(got, []string{"bash", "read", "write"}) {
		t.Fatalf("all before activation = %v", got)
	}
	if err := registry.Activate([]string{"read", "write"}); err != nil {
		t.Fatal(err)
	}
	if got := toolNames(registry.All()); !sameNames(got, []string{"read", "write"}) {
		t.Fatalf("all after activation = %v", got)
	}
	if _, ok := registry.Get("bash"); ok {
		t.Fatal("inactive tool should not be returned by Get")
	}
	if got := toolNames(registry.Registered()); !sameNames(got, []string{"bash", "read", "write"}) {
		t.Fatalf("registered tools = %v", got)
	}
}

func TestRegistryDeactivate(t *testing.T) {
	registry := NewRegistry()
	registerTestTools(t, registry, "bash", "read", "write")

	if err := registry.Deactivate([]string{"bash"}); err != nil {
		t.Fatal(err)
	}
	if got := toolNames(registry.Active()); !sameNames(got, []string{"read", "write"}) {
		t.Fatalf("active tools = %v", got)
	}
}

func TestRegistryBuiltinSetMembership(t *testing.T) {
	registry := NewRegistry()
	for _, registration := range []struct {
		name string
		sets []ToolSet
	}{
		{name: "read", sets: []ToolSet{ToolSetReadOnly, ToolSetCoding}},
		{name: "grep", sets: []ToolSet{ToolSetReadOnly, ToolSetCoding}},
		{name: "find", sets: []ToolSet{ToolSetReadOnly, ToolSetCoding}},
		{name: "ls", sets: []ToolSet{ToolSetReadOnly, ToolSetCoding}},
		{name: "write", sets: []ToolSet{ToolSetCoding}},
		{name: "edit", sets: []ToolSet{ToolSetCoding}},
		{name: "bash", sets: []ToolSet{ToolSetCoding}},
	} {
		if err := registry.RegisterInSet(testTool{name: registration.name}, registration.sets...); err != nil {
			t.Fatal(err)
		}
	}

	if got := toolNames(registry.Set(ToolSetReadOnly)); !sameNames(got, []string{"find", "grep", "ls", "read"}) {
		t.Fatalf("read-only tools = %v", got)
	}
	if got := toolNames(registry.Set(ToolSetCoding)); !sameNames(got, []string{"bash", "edit", "find", "grep", "ls", "read", "write"}) {
		t.Fatalf("coding tools = %v", got)
	}
}

func registerTestTools(t *testing.T, registry *Registry, names ...string) {
	t.Helper()
	for _, name := range names {
		if err := registry.Register(testTool{name: name}); err != nil {
			t.Fatal(err)
		}
	}
}

func toolNames(tools []agent.Tool) []string {
	names := make([]string, 0, len(tools))
	for _, tool := range tools {
		names = append(names, tool.Name())
	}
	return names
}

func sameNames(got []string, want []string) bool {
	if len(got) != len(want) {
		return false
	}
	for index := range got {
		if got[index] != want[index] {
			return false
		}
	}
	return true
}
