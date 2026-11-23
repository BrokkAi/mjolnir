// Port of mj-chat/src/chat/pointer.rs.

// Pointer routing for the conversation. Mouse events are Bubble Tea mouse
// messages: crossterm's Down(Left) is a tea.MouseClickMsg with MouseLeft,
// ScrollUp/ScrollDown a tea.MouseWheelMsg with MouseWheelUp/MouseWheelDown,
// Drag and Moved a tea.MouseMotionMsg, Up a tea.MouseReleaseMsg.

package chat

import (
	"maps"
	"slices"
	"strings"
	"time"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func (s *ChatState) setActiveUserShells(shells []core.ActiveUserShell) {
	next := make([]string, 0, len(shells))
	for _, shell := range shells {
		next = append(next, shell.CommandID)
	}
	if !slices.Equal(s.activeUserShells, next) {
		s.activeUserShells = next
	}
}

func (s *ChatState) setActiveAgentTerminals(terminals []core.ActiveAgentTerminal, session *core.MaterializedSession) {
	claims := map[string]int64{}
	unresolved := make(map[string]int64, len(terminals))
	for _, terminal := range terminals {
		unresolved[terminal.TerminalID] = terminal.StartedAtMs
	}
	// Claims are normally on the newest item, so walking backward stops
	// immediately. The full-history path is reserved for the uncommon
	// unclaimed fallback this state exists to cover.
	for index := len(session.Transcript) - 1; index >= 0; index-- {
		item := session.Transcript[index]
		tool, ok := item.Body.(core.TranscriptBodyTool)
		if !ok {
			continue
		}
		for _, terminalID := range tool.TerminalRefs {
			startedAtMs, ok := unresolved[terminalID]
			if !ok {
				continue
			}
			if item.LastChangedAtMs >= startedAtMs {
				claims[terminalID] = item.LastChangedAtMs
				delete(unresolved, terminalID)
			}
		}
		if len(unresolved) == 0 {
			break
		}
	}
	if !slices.Equal(s.activeAgentTerminals, terminals) || !maps.Equal(s.claimedAgentTerminals, claims) {
		s.activeAgentTerminals = slices.Clone(terminals)
		s.claimedAgentTerminals = claims
	}
}

func (s *ChatState) activeUserShellIDs() []string {
	return slices.Clone(s.activeUserShells)
}

// ToggleRenderMode switches the transcript between rendered Markdown and raw
// source.
//
// The host owns the key that runs this, so it is called from the dashboard's
// command registry as well as from the transcript control.
func (s *ChatState) ToggleRenderMode() {
	s.renderMode = s.renderMode.Toggled()
	if s.renderMode == client.TranscriptRenderModeRich {
		s.SetNotice("Rich transcript rendering enabled")
	} else {
		s.SetNotice("Raw transcript source enabled")
	}
}

// DictationToggleAction is what a dictation toggle should do right now.
//
// A recording remains stoppable even if the helper becomes unavailable while
// it is running, so an active recording answers even when voice is no longer
// available. An unavailable idle microphone is inert.
func (s *ChatState) DictationToggleAction() ChatAction {
	if s.voiceAvailable || s.voiceActive {
		return ChatActionToggleVoice{}
	}
	return ChatActionNone{}
}

// FrameSurfaces are the surfaces the last frame registered, for the
// selection engine.
func (s *ChatState) FrameSurfaces() *FrameSurfaces {
	return &s.frameSurfaces
}

// ComponentHandlesMouse reports whether a shared component should receive
// this pointer event before text selection or the host surface. Captured
// presses remain owned even after the pointer leaves the control's hitbox.
func (s *ChatState) ComponentHandlesMouse(mouse tea.MouseMsg) bool {
	if s.earlier != nil {
		return true
	}
	point := mouse.Mouse()
	position := uv.Pos(point.X, point.Y)
	// An elicitation is the visible modal. Check its captured controls before
	// retained task/config/review geometry so those hidden states cannot
	// steal transcript selection or pointer events above it.
	if s.elicitation != nil {
		return s.elicitation.componentHandlesMouseAt(point.X, point.Y)
	}
	if s.turnControlDialogOpen {
		return s.turnControlDialog.CapturesPointer() || s.turnControlDialog.Contains(point.X, point.Y)
	}
	// The background-task dialog answers for the rectangle it drew, border
	// included, and for nothing else: outside it the pointer belongs to
	// whatever the host drew beside this conversation.
	if s.taskDialogOpen &&
		(s.taskDialogForm.CapturesPointer() ||
			(s.taskDialogArea != nil && position.In(pointerOuterRect(*s.taskDialogArea)))) {
		return true
	}
	if pointerAreaContains(s.taskControlArea, position) {
		return true
	}
	if pointerAreaContains(s.subagentControlArea, position) {
		return true
	}
	if pointerAreaContains(s.reviewerControlArea, position) {
		return true
	}
	if s.voiceForm.CapturesPointer() || pointerAreaContains(s.voiceButtonArea, position) {
		return true
	}
	if _, ok := s.promptConfigChipAt(point.X, point.Y); ok {
		return true
	}
	// An open dropdown captures outside presses to dismiss without
	// click-through.
	return s.configPickerActive()
}

// pointerAreaContains reports whether a last-frame hitbox, when there is
// one, covers position.
func pointerAreaContains(area *uv.Rectangle, position uv.Position) bool {
	return area != nil && position.In(*area)
}

// pointerOuterRect is ratatui's Rect::outer with a one-cell margin: area
// grown by one cell on every side, clamped at the screen's origin.
func pointerOuterRect(area uv.Rectangle) uv.Rectangle {
	return uv.Rectangle{
		Min: uv.Pos(max(area.Min.X-1, 0), max(area.Min.Y-1, 0)),
		Max: uv.Pos(area.Max.X+1, area.Max.Y+1),
	}
}

// pointerWheel reports a wheel turn in direction (tea.MouseWheelUp or
// tea.MouseWheelDown), crossterm's ScrollUp and ScrollDown.
func pointerWheel(mouse tea.MouseMsg, direction tea.MouseButton) bool {
	wheel, ok := mouse.(tea.MouseWheelMsg)
	return ok && wheel.Button == direction
}

// pointerLeftPress reports crossterm's Down(Left).
func pointerLeftPress(mouse tea.MouseMsg) bool {
	click, ok := mouse.(tea.MouseClickMsg)
	return ok && click.Button == tea.MouseLeft
}

// composerReplaced reports whether a dialog of this conversation's own
// currently stands in for the ordinary composer.
func (s *ChatState) composerReplaced() bool {
	return s.earlier != nil ||
		s.elicitation != nil ||
		s.configPicker != nil ||
		s.taskDialogOpen
}

// ConversationPointerCaptured: captured controls and scrollbars keep their
// conversation as owner even when the pointer crosses another session's
// notification form.
func (s *ChatState) ConversationPointerCaptured() bool {
	if s.earlier != nil {
		return false
	}
	scrollbar := s.TranscriptScrollbarDragging()
	if s.elicitation != nil {
		surface, ok := s.elicitation.pointerSurface()
		return scrollbar || (ok && surface == ElicitationSurfaceConversation)
	}
	if s.turnControlDialogOpen {
		return scrollbar || s.turnControlDialog.CapturesPointer()
	}
	return scrollbar ||
		(s.taskDialogOpen && s.taskDialogForm.CapturesPointer()) ||
		s.voiceForm.CapturesPointer() ||
		s.configPickerPointerCaptured()
}

// CancelConversationPointer cancels any pointer gesture owned by a chat
// component.
func (s *ChatState) CancelConversationPointer() {
	s.cancelTranscriptScrollbarPointer()
	s.turnControlDialog.CancelPointer()
	s.taskDialogForm.CancelPointer()
	s.voiceForm.CancelPointer()
	s.cancelConfigPickerPointer()
	if s.elicitation != nil {
		s.elicitation.cancelPointerIn(ElicitationSurfaceConversation)
	}
}

func (s *ChatState) CancelComponentPointer() {
	s.CancelConversationPointer()
	if s.elicitation != nil {
		s.elicitation.cancelPointerIn(ElicitationSurfaceNotification)
	}
}

// ResetComponentGeometry clears rendered component geometry before a host
// redraw. Persistent focus and pointer ownership survive a normal resize;
// only hitboxes are invalidated until the next registration pass.
func (s *ChatState) ResetComponentGeometry() {
	s.taskDialogForm.ResetGeometry()
	s.voiceForm.ResetGeometry()
	s.voiceButtonArea = nil
	s.configChipAreas = s.configChipAreas[:0]
	s.taskControlArea = nil
	s.subagentControlArea = nil
	s.taskDialogArea = nil
	s.taskDialogControlIDs = s.taskDialogControlIDs[:0]
	s.resetConfigPickerGeometry()
	if s.elicitation != nil {
		s.elicitation.resetComponentGeometry()
	}
}

// scrollElicitationMessage scrolls the elicitation message pane, for a drag
// held at its edge.
func (s *ChatState) scrollElicitationMessage(rows int) {
	if s.elicitation != nil {
		s.elicitation.scrollMessage(rows)
	}
}

// ElicitationSelectionText is the message text a selection in the
// elicitation pane covers.
func (s *ChatState) ElicitationSelectionText(selection *SelectionRange) (string, bool) {
	if s.elicitation == nil {
		return "", false
	}
	area, ok := s.elicitation.messageArea()
	if !ok {
		return "", false
	}
	return s.elicitation.selectionText(selection, area.Dx()), true
}

// HandleMouse routes a pointer event. Capture is on for every surface, so
// the app owns the wheel: terminal scrollback repaints whole TUI frames and
// is unusably slow on long sessions.
func (s *ChatState) HandleMouse(mouse tea.MouseMsg) ChatAction {
	point := mouse.Mouse()
	position := uv.Pos(point.X, point.Y)
	wheelUp := pointerWheel(mouse, tea.MouseWheelUp)
	wheelDown := pointerWheel(mouse, tea.MouseWheelDown)
	leftPress := pointerLeftPress(mouse)
	if s.earlierReaderHasFocus() && s.earlier != nil {
		reader := s.earlier
		switch {
		case wheelUp:
			reader.Scroll = max(reader.Scroll-3, 0)
		case wheelDown:
			reader.Scroll = min(reader.Scroll+3, 0xFFFF)
		}
		return ChatActionNone{}
	}
	if leftPress {
		s.notices.Dismiss(time.Now())
	}
	// The topmost form receives the gesture before selection, scrollbars, or
	// review controls. Reviewer elicitations keep priority while a stale
	// scrollbar is being redrawn.
	if dialog := s.elicitation; dialog != nil {
		overForm := dialog.componentHandlesMouseAt(point.X, point.Y)
		messageArea, hasMessage := dialog.messageArea()
		overMessage := hasMessage && position.In(messageArea)
		overQuestionChrome := dialog.renderedAreaContains(point.X, point.Y)
		if overForm || overMessage {
			request := *dialog.request()
			response := dialog.handleMouse(mouse)
			if response != nil {
				reviewer := s.elicitationIsReviewers
				var reviewerDraft ElicitationDraft
				if reviewer {
					reviewerDraft = dialog.draft()
				}
				s.elicitation = nil
				if reviewer {
					s.elicitationIsReviewers = false
					return s.finishReviewerElicitationResponse(reviewerDraft, *response)
				}
				return ChatActionRespondElicitation{Request: request, Response: *response}
			}
			return ChatActionNone{}
		}
		// A transcript thumb can be released after the pointer crosses the
		// question. Preserve that already-captured transcript gesture, while
		// the form-capture branch above still has priority for question
		// controls dragged upward.
		if s.TranscriptScrollbarDragging() && s.handleTranscriptScrollbarMouse(mouse) {
			return ChatActionNone{}
		}
		// The question owns its border and footer, but those cells are not
		// controls. Keep a wheel or click there from falling through to the
		// transcript; the upper transcript remains the only region that
		// should scroll while the question is open.
		if overQuestionChrome {
			return ChatActionNone{}
		}
		// Once a question is visible, stale task/config/review state is
		// retained for later restoration but must not intercept the
		// transcript above it. Route every other mouse event directly to the
		// transcript and stop before those modal handlers below.
		if s.handleTranscriptScrollbarMouse(mouse) {
			return ChatActionNone{}
		}
		if surface := s.frameSurfaces.Surface(SurfaceIdTranscript{}); surface != nil && position.In(surface.Rect) {
			switch {
			case wheelUp:
				s.scrollHistoryUp(mouseScrollRows)
			case wheelDown:
				s.scrollHistoryDown(mouseScrollRows)
			}
		}
		return ChatActionNone{}
	}
	if s.turnControlDialogOpen {
		return s.handleTurnControlDialog(mouse)
	}
	if s.taskDialogOpen {
		result := s.taskDialogForm.Handle(mouse)
		switch action := result.Action.(type) {
		case InteractionActivate[BackgroundTaskControl]:
			return s.activateBackgroundTaskControl(action.ID)
		case InteractionCancel[BackgroundTaskControl]:
			s.closeTaskDialog()
			return ChatActionNone{}
		}
		if result.Consumed {
			return ChatActionNone{}
		}
		switch {
		case wheelUp:
			s.setTaskDialogScroll(max(s.taskDialogScroll-3, 0))
		case wheelDown:
			s.setTaskDialogScroll(s.taskDialogScroll + 3)
		case leftPress && pointerAreaContains(s.taskDialogArea, position):
			// The list is read-only; clicking it keeps the dialog open.
		}
		return ChatActionNone{}
	}
	if s.configPickerActive() {
		_, action := s.handleConfigPickerMouse(mouse)
		return action
	}
	if leftPress && pointerAreaContains(s.taskControlArea, position) {
		s.openTaskDialog()
		return ChatActionNone{}
	}
	if leftPress && pointerAreaContains(s.subagentControlArea, position) {
		return ChatActionOpenSubagents{}
	}
	if leftPress && pointerAreaContains(s.reviewerControlArea, position) {
		return ChatActionOpenReviewers{}
	}
	if s.handleTranscriptScrollbarMouse(mouse) {
		return ChatActionNone{}
	}
	// A plain transcript click reaches here only after the selection router
	// has confirmed that the press never became a drag. Keeping this after
	// scrollbar hit testing prevents a thumb click from opening a link or
	// toggling a tool underneath it. A link inside a tool's output opens
	// rather than toggling the tool.
	if leftPress {
		if url, ok := s.transcriptLinkAt(point.X, point.Y); ok {
			// Agent text chooses these destinations; other schemes can start
			// arbitrary local handlers, so only web pages open on a click.
			if isWebURL(url) {
				return ChatActionOpenLink{URL: url}
			}
			return ChatActionCopyLink{URL: url}
		}
	}
	if leftPress && s.toggleTranscriptAt(point.X, point.Y) {
		return ChatActionNone{}
	}
	// The model and effort chips sit on the prompt border next to the
	// microphone; a click opens that key's value selector.
	if leftPress {
		if key, ok := s.promptConfigChipAt(point.X, point.Y); ok {
			s.openPromptConfigPicker(key)
			return ChatActionNone{}
		}
	}
	voiceResult := s.voiceForm.Handle(mouse)
	if activate, ok := voiceResult.Action.(InteractionActivate[VoiceControl]); ok && activate.ID == VoiceControlMicrophone {
		return s.DictationToggleAction()
	}
	if voiceResult.Consumed {
		return ChatActionNone{}
	}
	// Keep the legacy hitbox usable for callers that have not rendered a
	// frame yet. Once the shared form has registered its button, the branch
	// above owns the complete press/release gesture.
	if leftPress && pointerAreaContains(s.voiceButtonArea, position) && (s.voiceAvailable || s.voiceActive) {
		return ChatActionToggleVoice{}
	}
	// The host routes a wheel event here only when the pointer is over the
	// conversation, so it always drives the transcript.
	switch {
	case wheelUp:
		s.scrollHistoryUp(mouseScrollRows)
	case wheelDown:
		s.scrollHistoryDown(mouseScrollRows)
	}
	return ChatActionNone{}
}

// isWebURL reports an http or https URL with an authority; agent text
// chooses these destinations, and other schemes can start arbitrary local
// handlers.
func isWebURL(url string) bool {
	scheme, rest, ok := strings.Cut(url, ":")
	if !ok {
		return false
	}
	// ASCII-only folding, as Rust's eq_ignore_ascii_case: Unicode folding
	// would accept "httpſ".
	scheme = strings.Map(func(r rune) rune {
		if 'A' <= r && r <= 'Z' {
			return r + ('a' - 'A')
		}
		return r
	}, scheme)
	return (scheme == "http" || scheme == "https") && strings.HasPrefix(rest, "//")
}
