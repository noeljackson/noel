package models

import (
	"embed"
	"encoding/json"
	"errors"
	"os"
	"sort"
	"strings"
)

//go:embed defaults.json
var defaultsFS embed.FS

// Model describes an LLM model available to pi.
type Model struct {
	ID            string   `json:"id"`
	Provider      string   `json:"provider"`
	Display       string   `json:"display,omitempty"`
	ContextWindow int      `json:"contextWindow,omitempty"`
	MaxOutput     int      `json:"maxOutput,omitempty"`
	Thinking      bool     `json:"thinking,omitempty"`
	Pricing       Pricing  `json:"pricing,omitempty"`
	Aliases       []string `json:"aliases,omitempty"`
}

// Pricing is per 1M tokens.
type Pricing struct {
	Input      float64 `json:"input,omitempty"`
	Output     float64 `json:"output,omitempty"`
	CacheRead  float64 `json:"cacheRead,omitempty"`
	CacheWrite float64 `json:"cacheWrite,omitempty"`
}

// Registry resolves and enumerates configured models.
type Registry struct {
	models []Model
}

type registryFile struct {
	Models []Model `json:"models"`
}

var providerPreference = []string{
	"anthropic",
	"openai",
	"google",
	"openrouter",
}

var providerAliases = map[string]string{
	"claude": "anthropic",
}

// Load loads embedded defaults and merges optional user models from path.
func Load(path string) (*Registry, error) {
	defaults, err := loadDefaults()
	if err != nil {
		return nil, err
	}
	models := defaults
	if path != "" {
		userModels, err := loadUserModels(path)
		if err != nil {
			return nil, err
		}
		models = mergeModels(models, userModels)
	}
	return &Registry{models: models}, nil
}

// Resolve resolves name as an alias, ID, provider/model, or provider:short scope.
func (r *Registry) Resolve(name string) (Model, bool) {
	name = strings.TrimSpace(name)
	if name == "" {
		return Model{}, false
	}
	if model, ok := r.resolveExact(name, r.models); ok {
		return model, true
	}

	if provider, pattern, ok := splitScopedName(name); ok {
		candidates := r.ByProvider(provider)
		if model, ok := r.resolveExact(pattern, candidates); ok {
			return model, true
		}
		return resolvePattern(pattern, candidates)
	}

	return resolvePattern(name, r.models)
}

// All returns all models in registry order.
func (r *Registry) All() []Model {
	return cloneModels(r.models)
}

// ByProvider returns models for provider.
func (r *Registry) ByProvider(provider string) []Model {
	provider = canonicalProvider(provider)
	var out []Model
	for _, model := range r.models {
		if strings.EqualFold(model.Provider, provider) {
			out = append(out, cloneModel(model))
		}
	}
	return out
}

func loadDefaults() ([]Model, error) {
	data, err := defaultsFS.ReadFile("defaults.json")
	if err != nil {
		return nil, err
	}
	return parseModels(data)
}

func loadUserModels(path string) ([]Model, error) {
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	return parseModels(data)
}

func parseModels(data []byte) ([]Model, error) {
	var file registryFile
	if err := json.Unmarshal(data, &file); err != nil {
		return nil, err
	}
	for i := range file.Models {
		file.Models[i] = normalizeModel(file.Models[i])
	}
	return file.Models, nil
}

func mergeModels(defaults, overrides []Model) []Model {
	merged := cloneModels(defaults)
	for _, override := range overrides {
		override = normalizeModel(override)
		index := -1
		for i, existing := range merged {
			if existing.Provider == override.Provider && existing.ID == override.ID {
				index = i
				break
			}
		}
		if index >= 0 {
			merged[index] = mergeModel(merged[index], override)
		} else {
			merged = append(merged, override)
		}
	}
	return merged
}

func mergeModel(base, override Model) Model {
	if override.ID != "" {
		base.ID = override.ID
	}
	if override.Provider != "" {
		base.Provider = override.Provider
	}
	if override.Display != "" {
		base.Display = override.Display
	}
	if override.ContextWindow != 0 {
		base.ContextWindow = override.ContextWindow
	}
	if override.MaxOutput != 0 {
		base.MaxOutput = override.MaxOutput
	}
	if override.Thinking {
		base.Thinking = true
	}
	if override.Pricing != (Pricing{}) {
		base.Pricing = override.Pricing
	}
	if override.Aliases != nil {
		base.Aliases = append([]string(nil), override.Aliases...)
	}
	return base
}

func normalizeModel(model Model) Model {
	model.Provider = canonicalProvider(model.Provider)
	if model.Display == "" {
		model.Display = model.ID
	}
	if model.Aliases == nil {
		model.Aliases = []string{}
	}
	return model
}

func canonicalProvider(provider string) string {
	provider = strings.ToLower(strings.TrimSpace(provider))
	if mapped, ok := providerAliases[provider]; ok {
		return mapped
	}
	return provider
}

func (r *Registry) resolveExact(name string, candidates []Model) (Model, bool) {
	normalized := strings.ToLower(name)
	if slash := strings.Index(name, "/"); slash >= 0 {
		provider := canonicalProvider(name[:slash])
		id := strings.TrimSpace(name[slash+1:])
		for _, model := range candidates {
			if model.Provider == provider && strings.EqualFold(model.ID, id) {
				return cloneModel(model), true
			}
		}
	}

	var matches []Model
	for _, model := range candidates {
		if strings.EqualFold(model.ID, normalized) || strings.EqualFold(model.Display, name) || hasAlias(model, name) {
			matches = append(matches, model)
		}
	}
	return choosePreferred(matches)
}

func splitScopedName(name string) (string, string, bool) {
	colon := strings.Index(name, ":")
	if colon <= 0 || colon == len(name)-1 {
		return "", "", false
	}
	provider := canonicalProvider(name[:colon])
	pattern := strings.TrimSpace(name[colon+1:])
	return provider, pattern, true
}

func resolvePattern(pattern string, candidates []Model) (Model, bool) {
	pattern = strings.ToLower(strings.TrimSpace(pattern))
	var matches []Model
	for _, model := range candidates {
		if strings.Contains(strings.ToLower(model.ID), pattern) ||
			strings.Contains(strings.ToLower(model.Display), pattern) ||
			hasAlias(model, pattern) {
			matches = append(matches, model)
		}
	}
	return choosePreferred(matches)
}

func hasAlias(model Model, name string) bool {
	for _, alias := range model.Aliases {
		if strings.EqualFold(alias, name) {
			return true
		}
	}
	return false
}

func choosePreferred(matches []Model) (Model, bool) {
	if len(matches) == 0 {
		return Model{}, false
	}
	sort.SliceStable(matches, func(i, j int) bool {
		leftRank := providerRank(matches[i].Provider)
		rightRank := providerRank(matches[j].Provider)
		if leftRank != rightRank {
			return leftRank < rightRank
		}
		return matches[i].ID > matches[j].ID
	})
	return cloneModel(matches[0]), true
}

func providerRank(provider string) int {
	for i, preferred := range providerPreference {
		if provider == preferred {
			return i
		}
	}
	return len(providerPreference)
}

func cloneModels(models []Model) []Model {
	out := make([]Model, len(models))
	for i, model := range models {
		out[i] = cloneModel(model)
	}
	return out
}

func cloneModel(model Model) Model {
	model.Aliases = append([]string(nil), model.Aliases...)
	return model
}
