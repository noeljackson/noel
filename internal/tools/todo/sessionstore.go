package todo

import (
	"encoding/json"

	"github.com/noeljackson/pi/internal/session"
)

// SessionStore persists todo state in the active session branch.
type SessionStore struct {
	session *session.Session
}

// NewSessionStore returns a session-backed todo store.
func NewSessionStore(session *session.Session) *SessionStore {
	return &SessionStore{session: session}
}

func (s *SessionStore) Get(_ string) ([]Item, error) {
	if s.session == nil {
		return nil, nil
	}
	records, err := s.session.PathToCurrentLeaf()
	if err != nil {
		return nil, err
	}
	for i := len(records) - 1; i >= 0; i-- {
		if records[i].Type != session.RecordTypeTodoState {
			continue
		}
		var payload todoPayload
		if err := json.Unmarshal(records[i].Payload, &payload); err != nil {
			return nil, err
		}
		return append([]Item(nil), payload.Todos...), nil
	}
	return nil, nil
}

func (s *SessionStore) Set(_ string, items []Item) error {
	if s.session == nil {
		return nil
	}
	payload, err := json.Marshal(todoPayload{Todos: append([]Item(nil), items...)})
	if err != nil {
		return err
	}
	return s.session.AppendTodoState(payload)
}

type todoPayload struct {
	Todos []Item `json:"todos"`
}

var _ Store = (*SessionStore)(nil)
