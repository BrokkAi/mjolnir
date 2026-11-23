// Port of mj-chat/src/chat/keys.rs.
//
// The composer's keyboard handling: one key press against the chat's
// dialogs, prompt controls, history search, and prompt editor.
//
// Key events are Bubble Tea's: crossterm's KeyEvent becomes tea.KeyMsg
// (a tea.KeyPressMsg, repeats included through Key().IsRepeat, or a
// tea.KeyReleaseMsg), and keypad keys arrive as their own codes
// (tea.KeyKpHome) where crossterm reported KeyEventState::KEYPAD.
package chat

import (
	tea "charm.land/bubbletea/v2"
)

// HandleKey applies one key event to the chat and reports what the host
// must do about it.
func (s *ChatState) HandleKey(key tea.KeyMsg) ChatAction {
	if _, release := key.(tea.KeyReleaseMsg); release {
		return ChatActionNone{}
	}
	pressed := key.Key()
	repeat := pressed.IsRepeat
	// Any key breaks a Ctrl-K chain; only the Ctrl-K arm sets it again.
	chained := s.chainKill
	s.chainKill = false
	code, keypad := plainKeypadCode(typedKeyCode(pressed))
	code, modifiers := normalizeKey(code, pressed.Mod)
	modifiers = withoutLockModifiers(modifiers)
	normalized := pressed
	normalized.Code, normalized.Mod = code, modifiers
	if s.earlierReaderHasFocus() {
		return s.earlierKey(code)
	}
	if code == tea.KeyPgUp && modifiers.Contains(tea.ModCtrl) &&
		!(s.elicitation != nil && s.elicitation.isNotificationSurface()) {
		s.openEarlierMessages()
		return ChatActionNone{}
	}

	// Leaving the view is never an answer to the agent, so these two come
	// before the elicitation dialog. A pending elicitation is durable
	// projection state: it is rebuilt from pendingElicitations the next time
	// the session is opened, so stepping out loses nothing but field text
	// that was typed and not submitted.
	// The pane dial, detach, and the web viewer are host bindings behind the
	// prefix key. The host catches them before the composer sees them, so
	// the composer has no escape hatch of its own left.

	if dialog := s.elicitation; dialog != nil {
		if code == 'v' && modifiers&(tea.ModCtrl|tea.ModSuper) != 0 {
			return ChatActionPasteFromClipboard{}
		}
		request := *dialog.request()
		response := dialog.handleKeyEvent(key)
		if response != nil {
			var reviewerDraft *ElicitationDraft
			if s.elicitationIsReviewers {
				draft := dialog.draft()
				reviewerDraft = &draft
			}
			s.elicitation = nil
			if reviewerDraft != nil {
				s.elicitationIsReviewers = false
				return s.finishReviewerElicitationResponse(*reviewerDraft, *response)
			}
			return ChatActionRespondElicitation{Request: request, Response: *response}
		}
		return ChatActionNone{}
	}

	if s.turnControlDialogOpen {
		return s.handleTurnControlDialog(key)
	}
	if s.taskDialogOpen {
		var event tea.Msg = tea.KeyPressMsg(normalized)
		result := s.taskDialogForm.Handle(event)
		if activate, ok := result.Action.(InteractionActivate[BackgroundTaskControl]); ok {
			return s.activateBackgroundTaskControl(activate.ID)
		}
		if _, ok := result.Action.(InteractionCancel[BackgroundTaskControl]); ok {
			s.closeTaskDialog()
			return ChatActionNone{}
		}
		if result.Consumed {
			return ChatActionNone{}
		}
		switch code {
		case tea.KeyUp:
			s.setTaskDialogScroll(max(s.taskDialogScroll-1, 0))
		case tea.KeyDown:
			s.setTaskDialogScroll(s.taskDialogScroll + 1)
		case tea.KeyPgUp:
			s.setTaskDialogScroll(max(s.taskDialogScroll-5, 0))
		case tea.KeyPgDown:
			s.setTaskDialogScroll(s.taskDialogScroll + 5)
		}
		return ChatActionNone{}
	}

	// The value selector owns the keyboard while it is up; it is checked
	// after the elicitation dialog because the dialog draws on top of it.
	if s.configPickerActive() {
		return s.handleConfigPickerEvent(tea.KeyPressMsg(pressed))
	}

	if s.taskControlFocused() {
		switch {
		case code == tea.KeyEscape || code == tea.KeyUp:
			s.promptControlFocus = nil
		case code == tea.KeyEnter:
			s.openTaskDialog()
			return ChatActionNone{}
		case code == tea.KeyRight && s.subagentCount > 0:
			s.focusSubagentControl()
			return ChatActionNone{}
		case code == tea.KeyRight && s.reviewers.Total > 0:
			s.focusReviewerControl()
			return ChatActionNone{}
		case code == tea.KeyDown:
			return ChatActionNone{}
		default:
			s.promptControlFocus = nil
		}
		if code == tea.KeyEscape || code == tea.KeyUp {
			return ChatActionNone{}
		}
	}

	if s.subagentControlFocused() {
		switch {
		case code == tea.KeyEscape || code == tea.KeyUp:
			s.promptControlFocus = nil
		case code == tea.KeyEnter:
			return ChatActionOpenSubagents{}
		case code == tea.KeyLeft && s.backgroundTaskCount() > 0:
			s.promptControlFocus = nil
			s.focusTaskControl()
			return ChatActionNone{}
		case code == tea.KeyRight && s.reviewers.Total > 0:
			s.focusReviewerControl()
			return ChatActionNone{}
		case code == tea.KeyDown:
			return ChatActionNone{}
		default:
			s.promptControlFocus = nil
		}
		if code == tea.KeyEscape || code == tea.KeyUp {
			return ChatActionNone{}
		}
	}

	if s.promptControlFocus != nil && *s.promptControlFocus == PromptControlReviewers {
		switch {
		case code == tea.KeyEnter:
			return ChatActionOpenReviewers{}
		case code == tea.KeyLeft && s.subagentCount > 0:
			s.focusSubagentControl()
			return ChatActionNone{}
		case code == tea.KeyLeft && s.backgroundTaskCount() > 0:
			s.focusTaskControl()
			return ChatActionNone{}
		case code == tea.KeyDown || code == tea.KeyRight:
			return ChatActionNone{}
		case code == tea.KeyEscape || code == tea.KeyUp:
			s.promptControlFocus = nil
			return ChatActionNone{}
		default:
			s.promptControlFocus = nil
		}
	}
	if code == 'v' && modifiers&(tea.ModCtrl|tea.ModSuper) != 0 {
		return ChatActionPasteFromClipboard{}
	}

	if s.historySearch != nil {
		s.handleHistorySearchKey(code, modifiers)
		return ChatActionNone{}
	}

	if code == tea.KeyEscape {
		if repeat || s.turnControlSubmitting || s.turnControlAwaitingState != nil || s.cancellingPromptID != nil {
			return ChatActionNone{}
		}
		if modifiers.Contains(tea.ModCtrl) {
			if s.turnControls != nil && s.turnControls.Interrupt != nil {
				return ChatActionTurnControl{Command: s.turnControls.Interrupt}
			}
			return ChatActionNone{}
		}
		return s.interruptAction()
	}
	// With Num Lock off, the keypad's corner keys are transcript navigation.
	// Enhanced keyboard reporting distinguishes them from the dedicated Home
	// and End keys, which keep editing the prompt line.
	if keypad {
		switch code {
		case tea.KeyHome:
			s.setAnchor(TranscriptAnchorRow{Entry: 0, Row: 0})
			return ChatActionNone{}
		case tea.KeyEnd:
			s.setAnchor(TranscriptAnchorBottom{})
			return ChatActionNone{}
		}
	}
	if !s.standby && modifiers == tea.ModCtrl|tea.ModAlt {
		if repeat && (code == 'r' || code == 'd') {
			return ChatActionNone{}
		}
		if id, ok := s.recoverableSubmissionID(); ok {
			switch code {
			case 'r':
				return ChatActionResendSubmission{CommandID: id}
			case 'd':
				return ChatActionDismissSubmission{CommandID: id}
			}
		}
	}

	if modifiers.Contains(tea.ModCtrl) {
		switch code {
		// Reverse-i-search stays on readline's key. The block above hands
		// every key to an open search first, which is what lets Ctrl-R step
		// to the previous match and Alt-R cycle the search's scope once it
		// is open.
		case 'r':
			s.beginHistorySearch()
		case 'a':
			s.moveToLineStart(true)
		case 'e':
			s.moveToLineEnd(true)
		case 'b':
			s.moveInputCursor(-1)
		case 'f':
			s.moveInputCursor(1)
		case 'h':
			s.backspace()
		case 'd':
			s.delete()
		case 'u':
			s.killToLineStart()
		case 'k':
			s.killToLineEnd(chained)
			s.chainKill = true
		case 'w':
			start := PreviousWordStart(s.input, s.inputCursor)
			s.killRange(ByteRange{Start: start, End: s.inputCursor})
		case 'c':
			// Stash the abandoned prompt so history can recall it.
			if s.input != "" {
				stashed := s.input
				s.input = ""
				s.RecordPromptHistory(stashed)
				if s.bundleID != nil {
					s.composerRequests = append(s.composerRequests, composerRequestDaemon{
						request: ChatDaemonRequestRecordAbandonedPrompt{
							SessionID: s.sessionID,
							BundleID:  *s.bundleID,
							Text:      stashed,
						},
					})
				}
				s.clearInput()
			}
		case 'y':
			s.yank()
		case 'j', 'm':
			s.insertCharacter('\n')
		case 'p':
			switch {
			case s.input == "" && len(s.queuedPrompts) > 0:
				return s.editLatestQueuedPrompt()
			case s.input == "" || s.historyIndex != nil:
				s.moveHistory(-1)
			default:
				s.moveVertical(-1)
			}
		case 'n':
			if s.historyIndex != nil {
				s.moveHistory(1)
			} else {
				s.moveVertical(1)
			}
		case tea.KeyLeft:
			s.moveWord(-1)
		case tea.KeyRight:
			s.moveWord(1)
		case tea.KeyBackspace:
			start := PreviousWordStart(s.input, s.inputCursor)
			s.killRange(ByteRange{Start: start, End: s.inputCursor})
		case tea.KeyDelete:
			end := NextWordEnd(s.input, s.inputCursor)
			s.killRange(ByteRange{Start: s.inputCursor, End: end})
		case tea.KeyHome:
			s.setAnchor(TranscriptAnchorRow{Entry: 0, Row: 0})
		case tea.KeyEnd:
			s.setAnchor(TranscriptAnchorBottom{})
		}
		return ChatActionNone{}
	}
	if modifiers.Contains(tea.ModAlt) {
		switch {
		// One message at a time sits between PgUp's page and Ctrl+Home's
		// whole conversation. Readline leaves the paging keys alone, so
		// these never collide with prompt editing.
		case code == tea.KeyPgUp:
			s.jumpToUserMessage(false)
		case code == tea.KeyPgDown:
			s.jumpToUserMessage(true)
		case code == 'b' || code == tea.KeyLeft:
			s.moveWord(-1)
		case code == 'f' || code == tea.KeyRight:
			s.moveWord(1)
		case code == 'd' || code == tea.KeyDelete:
			end := NextWordEnd(s.input, s.inputCursor)
			s.killRange(ByteRange{Start: s.inputCursor, End: end})
		case code == tea.KeyBackspace:
			start := PreviousWordStart(s.input, s.inputCursor)
			s.killRange(ByteRange{Start: start, End: s.inputCursor})
		case code == tea.KeyEnter:
			s.insertCharacter('\n')
		case code == tea.KeyUp && len(s.queuedPrompts) > 0:
			return s.editLatestQueuedPrompt()
		}
		return ChatActionNone{}
	}
	switch {
	case code == tea.KeyEnter && modifiers.Contains(tea.ModShift):
		s.insertCharacter('\n')
		return ChatActionNone{}
	case code == tea.KeyEnter:
		if s.acceptAutocomplete() {
			return ChatActionNone{}
		}
		if s.voiceActive {
			// Voice updates append to the current draft. Keep Enter from
			// submitting that draft while the recorder still owns the
			// input, but leave ordinary editing available.
			return ChatActionNone{}
		}
		return s.submitInput()
	case code == tea.KeyBackspace:
		s.backspace()
		return ChatActionNone{}
	case code == tea.KeyDelete:
		s.delete()
		return ChatActionNone{}
	// Shift-Tab is crossterm's BackTab.
	case code == tea.KeyTab && modifiers.Contains(tea.ModShift):
		return ChatActionCycleFocus{Reverse: true}
	// Tab completes an open popup first; with none open it hands the
	// keyboard to the next pane of the combined surface.
	case code == tea.KeyTab:
		if s.acceptAutocomplete() {
			return ChatActionNone{}
		}
		return ChatActionCycleFocus{Reverse: false}
	case isCharacterKey(code) && modifiers&(tea.ModCtrl|tea.ModAlt) == 0:
		s.insertCharacter(code)
		return ChatActionNone{}
	case code == tea.KeyUp && s.autocomplete != nil:
		s.moveAutocomplete(-1)
		return ChatActionNone{}
	case code == tea.KeyDown && s.autocomplete != nil:
		s.moveAutocomplete(1)
		return ChatActionNone{}
	case code == tea.KeyUp:
		switch {
		case s.input == "" && len(s.queuedPrompts) > 0:
			return s.editLatestQueuedPrompt()
		case s.input == "" || s.historyIndex != nil:
			s.moveHistory(-1)
		default:
			s.moveVertical(-1)
		}
		return ChatActionNone{}
	case code == tea.KeyDown:
		switch {
		case s.historyIndex != nil:
			s.moveHistory(1)
		case s.cursorIsOnLastPromptLine() &&
			(s.backgroundTaskCount() > 0 || s.subagentCount > 0 || s.reviewers.Total > 0):
			switch {
			case s.backgroundTaskCount() > 0:
				s.focusTaskControl()
			case s.subagentCount > 0:
				s.focusSubagentControl()
			default:
				s.focusReviewerControl()
			}
		default:
			s.moveVertical(1)
		}
		return ChatActionNone{}
	case code == tea.KeyLeft && modifiers.Contains(tea.ModShift) && len(s.queuedPrompts) > 0:
		return s.editLatestQueuedPrompt()
	case code == tea.KeyLeft:
		s.moveInputCursor(-1)
		return ChatActionNone{}
	case code == tea.KeyRight:
		s.moveInputCursor(1)
		return ChatActionNone{}
	case code == tea.KeyPgUp:
		s.scrollHistoryUp(max(s.lastViewportHeight, 1))
		return ChatActionNone{}
	case code == tea.KeyPgDown:
		s.scrollHistoryDown(max(s.lastViewportHeight, 1))
		return ChatActionNone{}
	case code == tea.KeyHome:
		s.moveToLineStart(false)
		return ChatActionNone{}
	case code == tea.KeyEnd:
		s.moveToLineEnd(false)
		return ChatActionNone{}
	}
	return ChatActionNone{}
}

// isCharacterKey reports whether code is a character (crossterm's
// KeyCode::Char) rather than one of Bubble Tea's named keys. Enter, Tab,
// Backspace and Escape carry their control characters as codes, and the
// other named keys sit above tea.KeyExtended.
func isCharacterKey(code rune) bool {
	switch code {
	case tea.KeyEnter, tea.KeyTab, tea.KeyBackspace, tea.KeyEscape:
		return false
	}
	return code < tea.KeyExtended
}

// typedKeyCode is the code crossterm reports for key: the character as
// typed for a shifted character (`A`, `!`), which Bubble Tea reports as the
// unshifted code with a ShiftedCode beside it.
func typedKeyCode(key tea.Key) rune {
	if key.ShiftedCode != 0 {
		return key.ShiftedCode
	}
	return key.Code
}

// withoutLockModifiers drops Caps Lock, Num Lock and Scroll Lock, which
// crossterm reports as key state rather than modifiers, so modifier tests
// read as Rust's do.
func withoutLockModifiers(modifiers tea.KeyMod) tea.KeyMod {
	return modifiers &^ (tea.ModCapsLock | tea.ModNumLock | tea.ModScrollLock)
}

// plainKeypadCode is the key code crossterm reports for a keypad key, with
// whether it came from the keypad: crossterm gives keypad keys their plain
// codes and marks them with KeyEventState::KEYPAD, where Bubble Tea gives
// them codes of their own.
func plainKeypadCode(code rune) (rune, bool) {
	switch code {
	case tea.KeyKpEnter:
		return tea.KeyEnter, true
	case tea.KeyKpEqual:
		return '=', true
	case tea.KeyKpMultiply:
		return '*', true
	case tea.KeyKpPlus:
		return '+', true
	case tea.KeyKpComma, tea.KeyKpSep:
		return ',', true
	case tea.KeyKpMinus:
		return '-', true
	case tea.KeyKpDecimal:
		return '.', true
	case tea.KeyKpDivide:
		return '/', true
	case tea.KeyKpUp:
		return tea.KeyUp, true
	case tea.KeyKpDown:
		return tea.KeyDown, true
	case tea.KeyKpLeft:
		return tea.KeyLeft, true
	case tea.KeyKpRight:
		return tea.KeyRight, true
	case tea.KeyKpPgUp:
		return tea.KeyPgUp, true
	case tea.KeyKpPgDown:
		return tea.KeyPgDown, true
	case tea.KeyKpHome:
		return tea.KeyHome, true
	case tea.KeyKpEnd:
		return tea.KeyEnd, true
	case tea.KeyKpInsert:
		return tea.KeyInsert, true
	case tea.KeyKpDelete:
		return tea.KeyDelete, true
	case tea.KeyKpBegin:
		return code, true
	}
	if code >= tea.KeyKp0 && code <= tea.KeyKp9 {
		return '0' + (code - tea.KeyKp0), true
	}
	return code, false
}
