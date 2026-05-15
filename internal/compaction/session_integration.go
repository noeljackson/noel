package compaction

import (
	"time"

	"github.com/noeljackson/pi/internal/session"
)

// Record persists a compaction event on the session.
func Record(s *session.Session, parentMessageID string, summary string, droppedCount int) error {
	return s.AppendCompactionRecord(summary, droppedCount, time.Now().UTC(), parentMessageID)
}
