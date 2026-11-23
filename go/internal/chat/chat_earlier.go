// Port of mj-chat/src/chat/earlier.rs.

package chat

import (
	"math"
	"slices"

	tea "charm.land/bubbletea/v2"
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
	if code == tea.KeyEscape {
		s.earlier = nil
		return ChatActionNone{}
	}
	reader := s.earlier
	if reader == nil {
		panic("history reader is open")
	}
	switch code {
	case tea.KeyEnter:
		if !reader.Loading && (!reader.Loaded || reader.Before != nil) {
			reader.Requested = true
		}
	case tea.KeyUp:
		reader.Scroll = max(reader.Scroll-1, 0)
	case tea.KeyPgUp:
		reader.Scroll = max(reader.Scroll-10, 0)
	case tea.KeyDown:
		reader.Scroll = min(reader.Scroll+1, math.MaxUint16)
	case tea.KeyPgDown:
		reader.Scroll = min(reader.Scroll+10, math.MaxUint16)
	case tea.KeyHome:
		reader.Scroll = 0
	}
	return ChatActionNone{}
}

func renderEarlier(frame *Frame, area uv.Rectangle, reader *EarlierMessages) {
	var status string
	switch {
	case reader.Title != nil:
		status = "Up/Down/PgUp/PgDn: scroll | Esc: return"
	case reader.Loading:
		status = "Loading earlier messages..."
	case reader.Error != nil:
		status = *reader.Error
	case reader.Before != nil:
		status = "Enter: earlier page | Up/Down/PgUp/PgDn: scroll | Esc: live"
	default:
		status = "Beginning of conversation | Up/Down/PgUp/PgDn: scroll | Esc: live"
	}
	title := "Earlier messages"
	if reader.Title != nil {
		title = *reader.Title
	}
	block := Panel(true).
		WithTitle(RawLine(" " + title + " ")).
		WithTitleBottom(RawLine(status))
	inner := block.Inner(area)
	paragraph := NewParagraph(TextFrom(slices.Clone(reader.Lines)...)).WithWrap(Wrap{Trim: false})
	maxScroll := max(paragraph.LineCount(inner.Dx())-inner.Dy(), 0)
	reader.Scroll = min(reader.Scroll, min(maxScroll, math.MaxUint16))
	frame.RenderWidget(Clear{}, area)
	frame.RenderWidget(paragraph.WithScroll(reader.Scroll, 0).WithBlock(block), area)
}
