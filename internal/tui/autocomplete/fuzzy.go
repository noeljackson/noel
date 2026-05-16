package autocomplete

import (
	"regexp"
	"sort"
	"strings"
)

type FuzzyMatch struct {
	Matches bool
	Score   float64
}

var (
	alphaNumericQuery = regexp.MustCompile(`^([a-z]+)([0-9]+)$`)
	numericAlphaQuery = regexp.MustCompile(`^([0-9]+)([a-z]+)$`)
)

func FuzzyMatchText(query string, text string) FuzzyMatch {
	queryLower := strings.ToLower(query)
	textLower := strings.ToLower(text)
	primary := fuzzyMatchNormalized(queryLower, textLower)
	if primary.Matches {
		return primary
	}

	swapped := ""
	if parts := alphaNumericQuery.FindStringSubmatch(queryLower); parts != nil {
		swapped = parts[2] + parts[1]
	} else if parts := numericAlphaQuery.FindStringSubmatch(queryLower); parts != nil {
		swapped = parts[2] + parts[1]
	}
	if swapped == "" {
		return primary
	}
	match := fuzzyMatchNormalized(swapped, textLower)
	if !match.Matches {
		return primary
	}
	match.Score += 5
	return match
}

func fuzzyMatchNormalized(query string, text string) FuzzyMatch {
	if query == "" {
		return FuzzyMatch{Matches: true}
	}
	if len(query) > len(text) {
		return FuzzyMatch{}
	}
	queryRunes := []rune(query)
	textRunes := []rune(text)
	queryIndex := 0
	score := 0.0
	lastMatchIndex := -1
	consecutiveMatches := 0

	for i, r := range textRunes {
		if queryIndex >= len(queryRunes) {
			break
		}
		if r != queryRunes[queryIndex] {
			continue
		}
		isBoundary := i == 0 || strings.ContainsRune(" \t-_./:", textRunes[i-1])
		if lastMatchIndex == i-1 {
			consecutiveMatches++
			score -= float64(consecutiveMatches * 5)
		} else {
			consecutiveMatches = 0
			if lastMatchIndex >= 0 {
				score += float64(i-lastMatchIndex-1) * 2
			}
		}
		if isBoundary {
			score -= 10
		}
		score += float64(i) * 0.1
		lastMatchIndex = i
		queryIndex++
	}
	if queryIndex < len(queryRunes) {
		return FuzzyMatch{}
	}
	if query == text {
		score -= 100
	}
	return FuzzyMatch{Matches: true, Score: score}
}

func FuzzyFilter[T interface{}](items []T, query string, getText func(T) string) []T {
	if strings.TrimSpace(query) == "" {
		return append([]T(nil), items...)
	}
	tokens := strings.Fields(query)
	type scored struct {
		item  T
		score float64
	}
	var results []scored
	for _, item := range items {
		total := 0.0
		ok := true
		text := getText(item)
		for _, token := range tokens {
			match := FuzzyMatchText(token, text)
			if !match.Matches {
				ok = false
				break
			}
			total += match.Score
		}
		if ok {
			results = append(results, scored{item: item, score: total})
		}
	}
	sort.SliceStable(results, func(i, j int) bool {
		return results[i].score < results[j].score
	})
	out := make([]T, 0, len(results))
	for _, result := range results {
		out = append(out, result.item)
	}
	return out
}
