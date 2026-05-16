package timings

import (
	"testing"
	"time"
)

func TestStartStopRecordsDuration(t *testing.T) {
	timings := New()
	stop := timings.Start("turn")
	time.Sleep(time.Millisecond)
	stop()
	stop()

	stats := timings.Summary()["turn"]
	if stats.Count != 1 {
		t.Fatalf("count = %d, want 1", stats.Count)
	}
	if stats.Total <= 0 {
		t.Fatalf("total = %s, want positive duration", stats.Total)
	}
}

func TestSummaryAggregates(t *testing.T) {
	timings := New()
	timings.Record("tool", time.Millisecond)
	timings.Record("tool", 3*time.Millisecond)
	timings.Record("tool", 5*time.Millisecond)

	stats := timings.Summary()["tool"]
	if stats.Count != 3 {
		t.Fatalf("count = %d, want 3", stats.Count)
	}
	if stats.Total != 9*time.Millisecond {
		t.Fatalf("total = %s, want 9ms", stats.Total)
	}
	if stats.Min != time.Millisecond || stats.Max != 5*time.Millisecond {
		t.Fatalf("min/max = %s/%s, want 1ms/5ms", stats.Min, stats.Max)
	}
	if stats.Mean != 3*time.Millisecond {
		t.Fatalf("mean = %s, want 3ms", stats.Mean)
	}
	if stats.P50 != 3*time.Millisecond {
		t.Fatalf("p50 = %s, want 3ms", stats.P50)
	}
}
