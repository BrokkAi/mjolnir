// Port of mj-chat/src/chat/turn_control.rs (tests).

package chat

import (
	"reflect"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func turnControlTestChat() *ChatState {
	session := core.MaterializedSessionEmpty("owner")
	return ChatStateFromMaterializedTail(&session, nil, nil)
}

func turnControlTestState() core.RelayOperationalState {
	snapshot := core.NewRelaySnapshot("owner")
	return snapshot.OperationalState()
}

// Hard-won: d3fde1066: automatic steering left Esc a no-op behind "Esc interrupts".
func TestReconnectRestoresPendingSteeringAndEscapeInterruptsUntilDeliveryIsUncertain(t *testing.T) {
	chat := turnControlTestChat()
	state := turnControlTestState()
	state.ActivePrompt = &core.ActiveRelayPrompt{CommandID: "active", CreatedAtMs: 1, StartedAtMs: 1}
	state.Steering = &core.SteeringOperation{
		CommandID:      "steer",
		ActivePromptID: "active",
		QueuedPromptID: "queued",
		Status:         core.SteeringStatusPending,
	}
	interrupt := core.RelayCommandCancelTurnFor{ActivePromptID: "active"}
	message := "Steering…"
	state.TurnControls = &core.TurnControls{
		Status:    core.TurnControlStatusPending,
		Primary:   nil,
		Interrupt: interrupt,
		Message:   &message,
	}
	chat.syncTurnControl(&state)
	escape := tea.KeyPressMsg{Code: tea.KeyEscape}
	if got, want := chat.HandleKey(escape), ChatAction(ChatActionTurnControl{Command: interrupt}); !reflect.DeepEqual(got, want) {
		t.Fatalf("Esc = %#v, want %#v", got, want)
	}
	if feedback := chat.operationFeedback["turn-control"]; feedback != "Steering…" {
		t.Fatalf("feedback = %q", feedback)
	}
	if got := chat.HandleKey(tea.KeyPressMsg{Code: tea.KeyEscape, IsRepeat: true}); !reflect.DeepEqual(got, ChatAction(ChatActionNone{})) {
		t.Fatalf("holding Esc must not interrupt: %#v", got)
	}
	state.Steering.Status = core.SteeringStatusUnconfirmed
	chat.syncTurnControl(&state)
	if !chat.turnControlDialogOpen {
		t.Fatal("uncertain delivery did not open the review")
	}
	if got := chat.HandleKey(escape); !reflect.DeepEqual(got, ChatAction(ChatActionNone{})) {
		t.Fatalf("Esc on the review = %#v", got)
	}
	if chat.turnControlDialogOpen {
		t.Fatal("Esc did not dismiss the review")
	}
	chat.syncTurnControl(&state)
	if chat.turnControlDialogOpen {
		t.Fatal("ordinary snapshots must not reopen a dismissed offer")
	}
	if feedback := chat.operationFeedback["turn-control"]; !strings.Contains(feedback, "unconfirmed") {
		t.Fatalf("feedback = %q", feedback)
	}
	state.Steering.Status = core.SteeringStatusApplied
	chat.syncTurnControl(&state)
	if feedback, ok := chat.operationFeedback["turn-control"]; ok {
		t.Fatalf("feedback stayed after delivery: %q", feedback)
	}
}

func TestExplicitCancellationKeepsFeedbackUntilTheTurnSettles(t *testing.T) {
	chat := turnControlTestChat()
	state := turnControlTestState()
	cancelling := "active"
	state.CancellingPromptID = &cancelling
	chat.syncTurnControl(&state)
	if feedback := chat.operationFeedback["turn-control"]; feedback != "Interrupting turn…" {
		t.Fatalf("feedback = %q", feedback)
	}
	if got := chat.HandleKey(tea.KeyPressMsg{Code: tea.KeyEscape}); !reflect.DeepEqual(got, ChatAction(ChatActionNone{})) {
		t.Fatalf("Esc while cancelling = %#v", got)
	}
	state.CancellingPromptID = nil
	chat.syncTurnControl(&state)
	if feedback, ok := chat.operationFeedback["turn-control"]; ok {
		t.Fatalf("feedback stayed after the turn settled: %q", feedback)
	}
}
