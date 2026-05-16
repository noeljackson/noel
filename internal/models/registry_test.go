package models

import (
	"os"
	"path/filepath"
	"testing"
)

func TestDefaultModelsLoadViaEmbed(t *testing.T) {
	registry, err := Load("")
	if err != nil {
		t.Fatal(err)
	}
	all := registry.All()
	if len(all) == 0 {
		t.Fatal("no default models")
	}
	if _, ok := registry.Resolve("sonnet"); !ok {
		t.Fatal("default sonnet alias did not resolve")
	}
}

func TestUserOverrideMerge(t *testing.T) {
	path := filepath.Join(t.TempDir(), "models.json")
	data := `{
		"models": [
			{"id":"claude-sonnet-4-6","provider":"anthropic","display":"Custom Sonnet","contextWindow":123,"maxOutput":456,"thinking":true,"pricing":{"input":1,"output":2,"cacheRead":3,"cacheWrite":4},"aliases":["custom-sonnet"]},
			{"id":"local","provider":"openai","display":"Local","aliases":["fast"]}
		]
	}`
	if err := os.WriteFile(path, []byte(data), 0o600); err != nil {
		t.Fatal(err)
	}
	registry, err := Load(path)
	if err != nil {
		t.Fatal(err)
	}
	model, ok := registry.Resolve("custom-sonnet")
	if !ok {
		t.Fatal("custom alias did not resolve")
	}
	if model.Display != "Custom Sonnet" || model.ContextWindow != 123 {
		t.Fatalf("model = %#v", model)
	}
	if _, ok := registry.Resolve("fast"); !ok {
		t.Fatal("custom model did not resolve")
	}
}

func TestRegistryAliasAndScopedResolution(t *testing.T) {
	registry, err := Load("")
	if err != nil {
		t.Fatal(err)
	}
	model, ok := registry.Resolve("claude:sonnet")
	if !ok {
		t.Fatal("scoped sonnet did not resolve")
	}
	if model.Provider != "anthropic" || model.ID != "claude-sonnet-4-6" {
		t.Fatalf("model = %#v", model)
	}
	model, ok = registry.Resolve("anthropic/claude-haiku-4-5")
	if !ok {
		t.Fatal("provider/id did not resolve")
	}
	if model.ID != "claude-haiku-4-5" {
		t.Fatalf("ID = %q", model.ID)
	}
}

func TestRegistryPricing(t *testing.T) {
	registry, err := Load("")
	if err != nil {
		t.Fatal(err)
	}
	model, ok := registry.Resolve("sonnet")
	if !ok {
		t.Fatal("sonnet did not resolve")
	}
	if model.Pricing.Input != 3.0 || model.Pricing.Output != 15.0 || model.Pricing.CacheRead != 0.3 || model.Pricing.CacheWrite != 3.75 {
		t.Fatalf("pricing = %#v", model.Pricing)
	}
}
