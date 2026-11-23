// Port of mj-chat/src/chat/input.rs.

// Composer text editing: insertion, cursor motion, kill and yank, and the
// visual wrapping the terminal cursor follows.
//
// Rust re-exports components::text_layout's wrapped-row helpers here
// (grapheme_offset_for_wrapped_row, input_cursor_visual_position,
// input_visual_rows, set_input_cursor, wrapped_row_for_grapheme_offset); in
// the flat Go package they are called directly. Byte ranges (Rust's
// Range<usize>) are ByteRange values.

package chat

import (
	"strings"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"
)

// HandleSingleLinePromptKey: a single-line host uses the real Prompt editor;
// submission belongs to the host. Vertical transcript navigation has no
// meaning in this field.
func (s *ChatState) HandleSingleLinePromptKey(key tea.KeyPressMsg) EventResult[ChatAction] {
	panic("unported: mj-chat/src/chat/input.rs handle_single_line_prompt_key")
}

// SingleLinePromptText is the plain editor text, without the
// image-preserving saved-draft envelope.
func (s *ChatState) SingleLinePromptText() string {
	return s.input
}

func (s *ChatState) PasteSingleLinePrompt(text string) {
	s.Paste(strings.NewReplacer("\r", " ", "\n", " ").Replace(text))
}

// SetSingleLinePromptText restores literal text and returns the displaced
// draft for the host to retain. Search's preview must not replace its
// original unsent draft.
func (s *ChatState) SetSingleLinePromptText(text string) string {
	s.cancelHistorySearch()
	previous := s.input
	s.setInput(text)
	return previous
}

// drawPromptSlice draws an editor slice with the same mapping for content
// and cursor. Every stored newline occupies a visible cell; submission stays
// literal. (An associated function of ChatState in Rust.)
func drawPromptSlice(frame *Frame, area uv.Rectangle, text string, cursor int, focused bool) {
	panic("unported: mj-chat/src/chat/input.rs draw_prompt_slice")
}

// DrawSingleLinePrompt is the single-row presentation of the real Prompt.
// Search retains both its selected match and the shared status/query instead
// of hiding the match.
func (s *ChatState) DrawSingleLinePrompt(frame *Frame, area uv.Rectangle, focused bool) {
	panic("unported: mj-chat/src/chat/input.rs draw_single_line_prompt")
}

// replaceInputRange replaces the composer bytes in r with inserted
// and returns what it removed.
func (s *ChatState) replaceInputRange(r ByteRange, inserted *PromptPayload) PromptPayload {
	panic("unported: mj-chat/src/chat/input.rs replace_input_range")
}

func (s *ChatState) insertCharacter(character rune) {
	payload := PromptPayloadText(string(character))
	s.replaceInputRange(ByteRange{Start: s.inputCursor, End: s.inputCursor}, &payload)
}

func (s *ChatState) handleTerminalPaste(pasted string) ChatAction {
	if s.earlierReaderHasFocus() {
		return ChatActionNone{}
	}
	if pasted == "" {
		// Terminals signal non-text clipboard content (such as images) with
		// an empty bracketed paste. Respect the same modal routing as Ctrl-V
		// and let the background clipboard reader retrieve it. An agent
		// without image support cannot use that content, so say so instead
		// of reading the clipboard.
		if !(s.standby || s.promptImagesSupported) && s.elicitation == nil {
			if _, composer := s.clipboardTarget().(clipboardTargetComposer); composer {
				s.SetNotice(imagePasteUnsupportedNotice)
				return ChatActionNone{}
			}
		}
		return s.HandleKey(tea.KeyPressMsg{Code: 'v', Mod: tea.ModCtrl})
	}
	return s.pasteOrAttach(pasted)
}

// pasteOrAttach pastes into the composer, or, when the paste is only image
// paths and the agent takes images, asks for those files to be attached.
func (s *ChatState) pasteOrAttach(pasted string) ChatAction {
	if s.elicitation == nil && s.historySearch == nil &&
		(s.standby || s.composerRuntime.acceptingStaged || s.promptImagesSupported) {
		if _, composer := s.clipboardTarget().(clipboardTargetComposer); composer {
			text := sanitizeTerminalText(pasted)
			if images, ok := imagePaths(text); ok {
				return ChatActionAttachPaste{Text: text, Images: images}
			}
		}
	}
	s.handlePaste(pasted)
	return ChatActionNone{}
}

// pastePlain inserts already sanitized text at the cursor, without looking
// for paths.
func (s *ChatState) pastePlain(text string) {
	payload := PromptPayloadText(text)
	s.replaceInputRange(ByteRange{Start: s.inputCursor, End: s.inputCursor}, &payload)
}

// Paste is the host-facing paste: bracketed-paste text lands in the composer
// draft.
func (s *ChatState) Paste(pasted string) {
	s.handlePaste(pasted)
}

func (s *ChatState) handlePaste(pasted string) {
	if s.elicitation != nil {
		s.elicitation.paste(pasted)
		return
	}
	pasted = sanitizeTerminalText(pasted)
	if pasted == "" {
		return
	}
	if s.historySearch != nil {
		s.historySearch.query.PushStr(strings.NewReplacer("\r", " ", "\n", " ").Replace(pasted))
		s.refreshHistorySearch()
		return
	}
	payload := PromptPayloadText(pasted)
	s.replaceInputRange(ByteRange{Start: s.inputCursor, End: s.inputCursor}, &payload)
}

func (s *ChatState) backspace() {
	if s.inputCursor == 0 {
		return
	}
	start := PreviousGrapheme(s.input, s.inputCursor)
	payload := PromptPayloadText("")
	s.replaceInputRange(ByteRange{Start: start, End: s.inputCursor}, &payload)
}

func (s *ChatState) delete() {
	if s.inputCursor >= len(s.input) {
		return
	}
	end := NextGrapheme(s.input, s.inputCursor)
	payload := PromptPayloadText("")
	s.replaceInputRange(ByteRange{Start: s.inputCursor, End: end}, &payload)
}

func (s *ChatState) moveInputCursor(delta int) {
	if delta < 0 {
		s.inputCursor = PreviousGrapheme(s.input, s.inputCursor)
	} else {
		s.inputCursor = NextGrapheme(s.input, s.inputCursor)
	}
	s.inputCursor = snapCursor(s.inputImages, s.inputCursor, delta)
	s.preferredColumn = nil
	s.updateAutocomplete()
}

func (s *ChatState) moveToLineStart(crossBoundary bool) {
	start := LineStart(s.input, s.inputCursor)
	if crossBoundary && s.inputCursor == start && start > 0 {
		s.inputCursor = LineStart(s.input, start-1)
	} else {
		s.inputCursor = start
	}
	s.preferredColumn = nil
	s.updateAutocomplete()
}

func (s *ChatState) moveToLineEnd(crossBoundary bool) {
	end := LineEnd(s.input, s.inputCursor)
	if crossBoundary && s.inputCursor == end && end < len(s.input) {
		s.inputCursor = LineEnd(s.input, end+1)
	} else {
		s.inputCursor = end
	}
	s.preferredColumn = nil
	s.updateAutocomplete()
}

func (s *ChatState) moveVertical(direction int) {
	panic("unported: mj-chat/src/chat/input.rs move_vertical")
}

func (s *ChatState) moveWord(direction int) {
	if direction < 0 {
		s.inputCursor = PreviousWordStart(s.input, s.inputCursor)
	} else {
		s.inputCursor = NextWordEnd(s.input, s.inputCursor)
	}
	s.inputCursor = snapCursor(s.inputImages, s.inputCursor, direction)
	s.preferredColumn = nil
	s.updateAutocomplete()
}

// killRange removes the composer bytes in r into the kill buffer.
func (s *ChatState) killRange(r ByteRange) {
	if r.Start >= r.End {
		return
	}
	payload := PromptPayloadText("")
	removed := s.replaceInputRange(r, &payload)
	s.killBuffer = removed.Text
	s.killImages = removed.Images
}

func (s *ChatState) killToLineStart() {
	start := LineStart(s.input, s.inputCursor)
	if start == s.inputCursor && start > 0 {
		s.killRange(ByteRange{Start: start - 1, End: start})
	} else {
		s.killRange(ByteRange{Start: start, End: s.inputCursor})
	}
}

// killToLineEnd kills to the end of the line. A chained kill appends to the
// kill buffer, in Emacs order, so a later yank restores the whole block.
func (s *ChatState) killToLineEnd(chained bool) {
	panic("unported: mj-chat/src/chat/input.rs kill_to_line_end")
}

func (s *ChatState) yank() {
	if s.killBuffer == "" {
		return
	}
	payload := PromptPayload{Text: s.killBuffer, Images: append([]PromptImage(nil), s.killImages...)}
	s.replaceInputRange(ByteRange{Start: s.inputCursor, End: s.inputCursor}, &payload)
}
