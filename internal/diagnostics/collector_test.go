package diagnostics

import "testing"

func TestCollectorAddSubscribeRecentClear(t *testing.T) {
	collector := New()
	var seen []Diagnostic
	unsubscribe := collector.Subscribe(func(d Diagnostic) {
		seen = append(seen, d)
	})

	collector.Add(Diagnostic{Level: Info, Source: "one", Message: "first"})
	collector.Add(Diagnostic{Level: Warning, Source: "two", Message: "second"})
	if len(seen) != 2 {
		t.Fatalf("subscriber saw %d diagnostics, want 2", len(seen))
	}

	recent := collector.Recent(1)
	if len(recent) != 1 || recent[0].Source != "two" {
		t.Fatalf("recent = %#v, want latest diagnostic", recent)
	}

	unsubscribe()
	collector.Add(Diagnostic{Level: Error, Source: "three", Message: "third"})
	if len(seen) != 2 {
		t.Fatalf("subscriber was called after unsubscribe")
	}

	collector.Clear()
	if got := collector.Recent(10); len(got) != 0 {
		t.Fatalf("recent after clear = %#v, want empty", got)
	}
}
