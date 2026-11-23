// Port of mj-chat/src/chat/earlier.rs.

package chat

import (
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// EarlierMessages is a bounded page, separate from the live feed and the
// person's draft.
type EarlierMessages struct {
	// Title: static text uses the same reader without requesting history
	// pages.
	Title      *string
	Generation uint64
	Before     *core.TranscriptCursor
	Requested  bool
	Loading    bool
	Loaded     bool
	Lines      []Line
	// Scroll is a row offset, kept within 0..=65535 as Rust's u16 is.
	Scroll int
	Error  *string
}

// earlierReaderHasFocus: the earlier-message reader belongs to the
// conversation presentation. A notification form still edits the same dialog
// while that reader is open.
func (s *ChatState) earlierReaderHasFocus() bool {
	return s.earlier != nil && !(s.elicitation != nil && s.elicitation.isNotificationSurface())
}

func (s *ChatState) openEarlierMessages() {
	s.CancelConversationPointer()
	s.earlierGeneration++
	s.earlier = &EarlierMessages{
		Generation: s.earlierGeneration,
		Requested:  true,
	}
}

// earlierKey handles a key while the reader is open. code is the Bubble Tea
// key code (tea.KeyEsc, tea.KeyEnter, tea.KeyUp, ...).
func (s *ChatState) earlierKey(code rune) ChatAction {
	panic("unported: mj-chat/src/chat/earlier.rs earlierKey")
}

func renderEarlier(frame *Frame, area uv.Rectangle, reader *EarlierMessages) {
	panic("unported: mj-chat/src/chat/earlier.rs renderEarlier")
}
