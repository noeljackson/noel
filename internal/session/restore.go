package session

import (
	"encoding/json"
	"errors"
	"os"

	"github.com/noeljackson/pi/internal/agent"
)

// InterruptedTurn describes a persisted turn that did not reach a save point.
type InterruptedTurn struct {
	LastAssistantMessage *agent.AssistantMessage
	PendingToolCalls     []agent.ToolUseContent
	PartialText          string
	PartialToolUse       *agent.ToolUseContent
}

// InterruptedTurn reports recoverable interrupted assistant or tool state.
func (s *Session) InterruptedTurn() (*InterruptedTurn, error) {
	messages, err := s.Messages()
	if err != nil {
		return nil, err
	}

	var lastAssistant *agent.AssistantMessage
	lastAssistantIndex := -1
	for i, message := range messages {
		switch msg := message.(type) {
		case agent.AssistantMessage:
			copy := msg
			lastAssistant = &copy
			lastAssistantIndex = i
		case *agent.AssistantMessage:
			copy := *msg
			lastAssistant = &copy
			lastAssistantIndex = i
		}
	}

	pending := make([]agent.ToolUseContent, 0)
	if lastAssistant != nil {
		completed := make(map[string]struct{})
		for _, message := range messages[lastAssistantIndex+1:] {
			switch msg := message.(type) {
			case agent.ToolResultMessage:
				for _, result := range msg.Results {
					completed[result.ToolUseID] = struct{}{}
				}
			case *agent.ToolResultMessage:
				for _, result := range msg.Results {
					completed[result.ToolUseID] = struct{}{}
				}
			}
		}
		for _, content := range lastAssistant.Content {
			switch block := content.(type) {
			case agent.ToolUseContent:
				if _, ok := completed[block.ID]; !ok {
					pending = append(pending, block)
				}
			case *agent.ToolUseContent:
				if _, ok := completed[block.ID]; !ok {
					pending = append(pending, *block)
				}
			}
		}
	}

	currentTurn, hasCurrentTurn, err := s.readCurrentTurn()
	if err != nil {
		return nil, err
	}
	if !hasCurrentTurn && len(pending) == 0 {
		return nil, nil
	}
	return &InterruptedTurn{
		LastAssistantMessage: lastAssistant,
		PendingToolCalls:     pending,
		PartialText:          currentTurn.PartialText,
		PartialToolUse:       currentTurn.PartialToolUse,
	}, nil
}

func (s *Session) readCurrentTurn() (CurrentTurn, bool, error) {
	data, err := os.ReadFile(s.currentTurnPath)
	if errors.Is(err, os.ErrNotExist) {
		return CurrentTurn{}, false, nil
	}
	if err != nil {
		return CurrentTurn{}, false, err
	}
	var disk currentTurnDisk
	if err := json.Unmarshal(data, &disk); err != nil {
		return CurrentTurn{}, false, err
	}
	return currentTurnFromDisk(disk), true, nil
}
