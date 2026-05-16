package autocomplete

import (
	"context"
	"testing"
)

func TestSlashProviderSuggestions(t *testing.T) {
	provider := NewSlashProvider([]SlashCommand{
		{Name: "model", Description: "Select model"},
		{Name: "login", Description: "Log in"},
	})
	all := provider.Suggestions(context.Background(), "/", 1)
	if len(all) != 2 {
		t.Fatalf("all = %#v", all)
	}
	partial := provider.Suggestions(context.Background(), "/mo", 3)
	if len(partial) != 1 || partial[0].Label != "model" || partial[0].Insert != "/model " {
		t.Fatalf("partial = %#v", partial)
	}
}
