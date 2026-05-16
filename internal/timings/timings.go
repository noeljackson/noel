package timings

import (
	"sort"
	"sync"
	"time"
)

type Timings struct {
	mu      sync.RWMutex
	records map[string][]time.Duration
}

type Stats struct {
	Count int
	Total time.Duration
	Min   time.Duration
	Max   time.Duration
	Mean  time.Duration
	P50   time.Duration
	P95   time.Duration
	P99   time.Duration
}

func New() *Timings {
	return &Timings{records: map[string][]time.Duration{}}
}

func (t *Timings) Start(name string) func() {
	start := time.Now()
	var once sync.Once
	return func() {
		once.Do(func() {
			t.Record(name, time.Since(start))
		})
	}
}

func (t *Timings) Record(name string, d time.Duration) {
	if t == nil || name == "" || d < 0 {
		return
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	t.records[name] = append(t.records[name], d)
}

func (t *Timings) Summary() map[string]Stats {
	if t == nil {
		return nil
	}
	t.mu.RLock()
	snapshot := make(map[string][]time.Duration, len(t.records))
	for name, values := range t.records {
		snapshot[name] = append([]time.Duration(nil), values...)
	}
	t.mu.RUnlock()

	out := make(map[string]Stats, len(snapshot))
	for name, values := range snapshot {
		out[name] = summarize(values)
	}
	return out
}

func summarize(values []time.Duration) Stats {
	sort.Slice(values, func(i, j int) bool { return values[i] < values[j] })
	stats := Stats{Count: len(values)}
	if len(values) == 0 {
		return stats
	}
	stats.Min = values[0]
	stats.Max = values[len(values)-1]
	for _, value := range values {
		stats.Total += value
	}
	stats.Mean = stats.Total / time.Duration(len(values))
	stats.P50 = percentile(values, 0.50)
	stats.P95 = percentile(values, 0.95)
	stats.P99 = percentile(values, 0.99)
	return stats
}

func percentile(values []time.Duration, p float64) time.Duration {
	if len(values) == 0 {
		return 0
	}
	if len(values) == 1 {
		return values[0]
	}
	index := int(float64(len(values)-1) * p)
	if index < 0 {
		index = 0
	}
	if index >= len(values) {
		index = len(values) - 1
	}
	return values[index]
}
