// Port of mj-chat/src/chat/feedback.rs.

package chat

import (
	"log/slog"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Conversation feedback stays separate from dashboard-wide notifications.

// conversationNotice is a notice that belongs in the conversation rather
// than the dashboard's notification slot.
type conversationNotice struct {
	ID        string
	Text      string
	Submitted bool
	// SentAt is the journal ordinal this chat had applied when it last sent
	// the notice.
	SentAt uint64
	// Refused says the relay refused the last send.
	Refused      bool
	RecordedAtMs int64
}

// conversationNotice records a conversation notice, skipping a repeat of
// the latest one.
func (c *ChatState) conversationNotice(text string) {
	text = sanitizeTerminalText(text)
	if n := len(c.conversationNotices); n > 0 && c.conversationNotices[n-1].Text == text {
		return
	}
	id, err := client.NewCommandID("chat-notice")
	if err != nil {
		slog.Warn("could not identify conversation notice", "error", err)
		id = ""
	}
	c.conversationNotices = append(c.conversationNotices, conversationNotice{
		Submitted:    id == "",
		ID:           id,
		Text:         text,
		SentAt:       0,
		Refused:      false,
		RecordedAtMs: core.EpochMillis(),
	})
	c.invalidateRenderCache()
}

// reconcileNotices drops notices the session's transcript now carries and
// resends refused ones once the journal has moved past them.
func (c *ChatState) reconcileNotices(session *core.MaterializedSession) {
	kept := c.conversationNotices[:0]
	for _, notice := range c.conversationNotices {
		stableID := "system:notice:" + notice.ID
		recorded := false
		for _, item := range session.Transcript {
			if item.StableID == stableID {
				recorded = true
				break
			}
		}
		if !recorded {
			kept = append(kept, notice)
		}
	}
	c.conversationNotices = kept
	c.resendRefusedNotices(session.AppliedEventOrdinal)
}

// refuseNotice marks a notice refused.
//
// The relay refuses every command while it clears context, notices
// included. A refused notice is sent again once the journal has moved past
// the point where it was sent, so it reaches the conversation when the
// clear ends instead of staying a local row below every later turn.
func (c *ChatState) refuseNotice(id string) {
	for i := range c.conversationNotices {
		if c.conversationNotices[i].ID == id {
			c.conversationNotices[i].Refused = true
			break
		}
	}
	c.resendRefusedNotices(c.latestSeq)
}

func (c *ChatState) resendRefusedNotices(appliedOrdinal uint64) {
	for i := range c.conversationNotices {
		notice := &c.conversationNotices[i]
		if notice.Refused && appliedOrdinal > notice.SentAt {
			notice.Refused = false
			notice.Submitted = false
		}
	}
}

// noticeEntries renders the conversation notices as system transcript
// entries.
func (c *ChatState) noticeEntries() []core.ChatEntry {
	entries := make([]core.ChatEntry, 0, len(c.conversationNotices))
	for _, notice := range c.conversationNotices {
		recordedAt := notice.RecordedAtMs
		entry := core.ChatEntryPlain(c.latestSeq, core.ChatRoleSystem, notice.Text)
		entries = append(entries, entry.WithRecordedAt(&recordedAt))
	}
	return entries
}
