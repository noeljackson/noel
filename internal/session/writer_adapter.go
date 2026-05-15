package session

import (
	"encoding/json"
	"time"

	"github.com/noeljackson/pi/internal/agent"
)

// AppendEvent persists agent stream events that affect session recovery.
func (s *Session) AppendEvent(event agent.Event) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	switch evt := event.(type) {
	case agent.AgentStartEvent:
		return s.appendRecordLocked(RecordTypeRunState, runStatePayload{Phase: "agent_start", SessionID: evt.SessionID}, s.lastRecordID)
	case agent.AgentEndEvent:
		payload := runStatePayload{Phase: "agent_end", Reason: evt.Reason}
		if evt.Err != nil {
			payload.Error = evt.Err.Error()
		}
		return s.appendRecordLocked(RecordTypeRunState, payload, s.lastRecordID)
	case agent.TurnStartEvent:
		return s.appendRecordLocked(RecordTypeRunState, runStatePayload{Phase: "turn_start", TurnID: evt.TurnID}, s.lastRecordID)
	case agent.TurnEndEvent:
		parentID := s.lastMessageRecordID
		if parentID == "" {
			parentID = s.lastRecordID
		}
		return s.appendRecordLocked(RecordTypeSavePoint, savePointPayload{}, parentID)
	case agent.MessageStartEvent:
		if evt.Model != "" {
			s.activeStreamModels[evt.MessageID] = evt.Model
		}
		return s.appendRecordLocked(RecordTypeRunState, runStatePayload{
			Phase:     "assistant_stream",
			MessageID: evt.MessageID,
			Model:     evt.Model,
		}, s.lastRecordID)
	case agent.MessageUpdateEvent:
		s.currentTurn.PartialText += evt.Delta.TextDelta
		if evt.Delta.ToolUseDelta != nil {
			if s.currentTurn.PartialToolUse != nil && evt.Delta.ToolUseDelta.ID != "" && evt.Delta.ToolUseDelta.ID != s.currentTurn.PartialToolUse.ID {
				s.currentTurn.PartialToolUseInput = ""
			}
			s.currentTurn.PartialToolUseInput += evt.Delta.ToolUseDelta.InputJSONPartial
			id := evt.Delta.ToolUseDelta.ID
			name := evt.Delta.ToolUseDelta.Name
			if s.currentTurn.PartialToolUse != nil {
				if id == "" {
					id = s.currentTurn.PartialToolUse.ID
				}
				if name == "" {
					name = s.currentTurn.PartialToolUse.Name
				}
			}
			input := json.RawMessage("null")
			if json.Valid([]byte(s.currentTurn.PartialToolUseInput)) {
				input = json.RawMessage(s.currentTurn.PartialToolUseInput)
			}
			s.currentTurn.PartialToolUse = &agent.ToolUseContent{
				ID:    id,
				Name:  name,
				Input: input,
			}
		}
		now := time.Now()
		if !s.lastCurrentTurnWrite.IsZero() && now.Sub(s.lastCurrentTurnWrite) < currentTurnWriteInterval {
			return nil
		}
		if err := s.writeCurrentTurnLocked(s.currentTurn); err != nil {
			return err
		}
		s.lastCurrentTurnWrite = now
		return nil
	case agent.MessageEndEvent:
		delete(s.activeStreamModels, evt.MessageID)
		return s.clearCurrentTurnLocked()
	case agent.ToolExecutionStartEvent:
		return s.appendRecordLocked(RecordTypeRunState, runStatePayload{
			Phase:  "tool_call",
			CallID: evt.CallID,
			Name:   evt.Name,
			Args:   evt.Input,
		}, s.lastRecordID)
	case agent.ToolExecutionEndEvent:
		return nil
	case agent.ToolExecutionUpdateEvent:
		return nil
	default:
		return nil
	}
}

type runStatePayload struct {
	Phase     string          `json:"phase"`
	SessionID string          `json:"session_id,omitempty"`
	TurnID    string          `json:"turn_id,omitempty"`
	MessageID string          `json:"message_id,omitempty"`
	Model     string          `json:"model,omitempty"`
	CallID    string          `json:"call_id,omitempty"`
	Name      string          `json:"name,omitempty"`
	Args      json.RawMessage `json:"args,omitempty"`
	Reason    string          `json:"reason,omitempty"`
	Error     string          `json:"error,omitempty"`
}
