// Port of mj-chat/src/chat/active/surfaces.rs.

package chat

import (
	"log/slog"
	"slices"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"
)

// cancelDictation stops any dictation thread. The thread reports Finished,
// which clears the view's voice state, so this only asks it to stop.
func (c *ActiveChat) cancelDictation() {
	cancel := c.voiceCancel
	c.voiceCancel = nil
	if cancel == nil {
		return
	}
	if err := cancel.Send(VoiceCommandCancel); err != nil {
		slog.Debug("dictation worker already stopped", "error", err)
	}
}

// DrawIn draws the transcript and the composer into regions, for a host that
// owns the rest of the frame.
//
// promptFocused says whether the composer owns the keyboard; only then does
// it draw a cursor and an accent border. transcriptSelected says the
// selection engine still owns a selection on the transcript, so its row space
// has to stay frozen for this frame.
func (c *ActiveChat) DrawIn(frame *Frame, regions ChatRegions, promptFocused bool, transcriptSelected bool) {
	c.selectElicitationSurface(ElicitationSurfaceConversation)
	renderIn(frame, c.ChatState, regions, promptFocused, transcriptSelected)
	c.dispatchToolDetails()
}

func (c *ActiveChat) selectElicitationSurface(surface ElicitationSurface) {
	if c.elicitation != nil {
		c.elicitation.selectSurface(surface)
	}
}

// HasPendingElicitationInput: accepted form input and answer delivery retain
// their result owner.
func (c *ActiveChat) HasPendingElicitationInput() bool {
	return (c.elicitation != nil && c.composerRuntime.pasteInFlight) || c.hasPendingElicitationAnswers()
}

func (c *ActiveChat) HasElicitation() bool {
	return c.elicitation != nil
}

// ElicitationInputAvailableIn: eligibility follows the dialog's rendered
// interior, across surfaces.
func (c *ActiveChat) ElicitationInputAvailableIn(surface ElicitationSurface) bool {
	return c.elicitation != nil && c.elicitation.wasRenderedIn(surface)
}

func (c *ActiveChat) ElicitationComponentHandlesMouseIn(surface ElicitationSurface, mouse tea.Mouse) bool {
	return c.elicitation != nil && c.elicitation.componentHandlesMouseIn(surface, mouse.X, mouse.Y)
}

// CancelElicitationPointerIn releases a presentation that navigation stopped
// displaying, while leaving capture in the other presentation untouched.
func (c *ActiveChat) CancelElicitationPointerIn(surface ElicitationSurface) {
	if c.elicitation != nil {
		c.elicitation.cancelPointerIn(surface)
	}
}

func (c *ActiveChat) ElicitationPointerSurface() (ElicitationSurface, bool) {
	if c.elicitation == nil {
		var none ElicitationSurface
		return none, false
	}
	return c.elicitation.pointerSurface()
}

// ElicitationHeight: the chat, including reviewer routing, owns which form
// is current.
func (c *ActiveChat) ElicitationHeight(width int) (int, bool) {
	if c.elicitation == nil {
		return 0, false
	}
	return c.elicitation.naturalHeight(width), true
}

// DrawElicitationIn draws the current form without making another draft or
// response path.
func (c *ActiveChat) DrawElicitationIn(frame *Frame, area uv.Rectangle, focused bool) FrameSurfaces {
	c.selectElicitationSurface(ElicitationSurfaceNotification)
	surfaces := NewFrameSurfaces()
	if c.elicitation != nil {
		renderElicitationIn(frame, c.elicitation, &surfaces, area, focused)
	}
	return surfaces
}

// ElicitationMessageText is the logical text whose row space this form
// exposes for selection.
func (c *ActiveChat) ElicitationMessageText() (string, bool) {
	if c.elicitation == nil {
		return "", false
	}
	return c.elicitation.promptText(), true
}

func (c *ActiveChat) ElicitationSelectionTextIn(surface ElicitationSurface, selection *SelectionRange) (string, bool) {
	c.selectElicitationSurface(surface)
	return c.ElicitationSelectionText(selection)
}

func (c *ActiveChat) AutoscrollElicitationIn(surface ElicitationSurface, direction int) {
	c.selectElicitationSurface(surface)
	c.AutoscrollSelection(SurfaceIdElicitationMessage{}, direction)
}

func (c *ActiveChat) ElicitationMinimumHeight(width int) (int, bool) {
	if c.elicitation == nil {
		return 0, false
	}
	return c.elicitation.minimumHeight(width), true
}

// FooterCommandAreas is the visible host footer commands, indexed through
// the supplied chords then functions, with the text drawn in each area. The
// text can differ from the supplied hint: the first surviving chord carries
// the chord prefix.
func (c *ActiveChat) FooterCommandAreas() []FooterCommandArea {
	return slices.Clone(c.footerCommandAreas)
}

// AutoscrollSelection scrolls the surface a drag is holding against one of
// its edges. direction is negative for up and positive for down.
func (c *ActiveChat) AutoscrollSelection(surface SurfaceId, direction int) {
	rows := mouseScrollRows
	switch surface.(type) {
	case SurfaceIdTranscript:
		if direction < 0 {
			c.scrollHistoryUp(mouseScrollRows)
		} else {
			c.scrollHistoryDown(mouseScrollRows)
		}
	case SurfaceIdElicitationMessage:
		if direction < 0 {
			c.scrollElicitationMessage(-rows)
		} else {
			c.scrollElicitationMessage(rows)
		}
	}
}
