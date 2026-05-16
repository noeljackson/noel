package autocomplete

import "testing"
import "math"

func TestFuzzyMatchScores(t *testing.T) {
	tests := []struct {
		query string
		text  string
		score float64
	}{
		{query: "", text: "model", score: 0},
		{query: "mod", text: "model", score: -39.7},
		{query: "mo", text: "model", score: -24.9},
		{query: "abc", text: "abc", score: -139.7},
		{query: "a1", text: "1a", score: -119.9},
	}
	for _, test := range tests {
		match := FuzzyMatchText(test.query, test.text)
		if !match.Matches {
			t.Fatalf("%q/%q did not match", test.query, test.text)
		}
		if math.Abs(match.Score-test.score) > 0.0001 {
			t.Fatalf("%q/%q score = %v, want %v", test.query, test.text, match.Score, test.score)
		}
	}
}

func TestFuzzyFilterSorts(t *testing.T) {
	items := []string{"model", "session", "mod-list"}
	got := FuzzyFilter(items, "mo", func(item string) string { return item })
	if len(got) < 2 || got[0] != "model" || got[1] != "mod-list" {
		t.Fatalf("got %#v", got)
	}
}
