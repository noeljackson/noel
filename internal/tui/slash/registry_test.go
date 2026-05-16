package slash

import "testing"

func TestRegistryRegisterLookupNames(t *testing.T) {
	registry := New()
	registry.Register(Command{Name: "model", Description: "select model"})
	registry.Register(Command{Name: "login", Description: "login"})
	if command, ok := registry.Lookup("model"); !ok || command.Description != "select model" {
		t.Fatalf("lookup = %#v, %v", command, ok)
	}
	names := registry.Names()
	if len(names) != 2 || names[0] != "login" || names[1] != "model" {
		t.Fatalf("names = %#v", names)
	}
}
