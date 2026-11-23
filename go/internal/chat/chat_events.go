// Port of mj-chat/src/chat/events.rs.

package chat

import (
	"encoding/json"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// eventConsumed reports whether an event belonged to the chat even when its
// handler had no action. This keeps a clamped cursor, an empty composer, and
// a modal's no-op key distinguishable from an event the host should route
// on.
//
// Events are Bubble Tea messages: tea.KeyPressMsg (IsRepeat for crossterm's
// Repeat kind), tea.KeyReleaseMsg, tea.MouseMsg and tea.PasteMsg.
func (s *ChatState) eventConsumed(event tea.Msg, action ChatAction) bool {
	panic("unported: mj-chat/src/chat/events.rs event_consumed")
}

func (s *ChatState) setAnchor(anchor TranscriptAnchor) {
	// Explicit navigation owns follow intent, even when the chosen row is
	// the same row the automatic opening reveal selected.
	s.revealLatestAgentOnDraw = false
	s.revealedAnchor = nil
	s.anchor = anchor
}

func (s *ChatState) finishReviewerElicitationResponse(form ElicitationDraft, response core.ElicitationResponse) ChatAction {
	role := s.elicitationRole
	s.elicitationRole = nil
	return ChatActionRespondReviewerElicitation{
		Draft: ChatElicitationDraft{
			form:         form,
			reviewer:     true,
			reviewerRole: role,
		},
		Response: response,
	}
}

func (s *ChatState) applyEvent(event *core.SequencedEvent) {
	panic("unported: mj-chat/src/chat/events.rs apply_event")
}

func (s *ChatState) applyAdapter(seq uint64, recordedAtMs *int64, payload json.RawMessage) {
	panic("unported: mj-chat/src/chat/events.rs apply_adapter")
}

// applySessionUpdateAt projects one typed ACP update into stable transcript
// items. The runtime keeps JSON at the persistence boundary so old event
// logs remain wire compatible; rendering never guesses at arbitrary JSON
// shapes. (The test-only apply_session_update is in chat_events_test.go.)
func (s *ChatState) applySessionUpdateAt(seq uint64, recordedAtMs *int64, update json.RawMessage) {
	panic("unported: mj-chat/src/chat/events.rs apply_session_update_at")
}
