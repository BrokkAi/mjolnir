// Port of mj-chat/src/text_input.rs.

// Nonblocking readline-style editing for text fields embedded in Hel's TUIs.

package chat

import (
	"strings"

	tea "charm.land/bubbletea/v2"
)

type EditOutcome int

const (
	EditOutcomeUnhandled EditOutcome = iota
	EditOutcomeHandled
	EditOutcomeChanged
)

func (o EditOutcome) Changed() bool {
	return o == EditOutcomeChanged
}

type InputFilter int

const (
	InputFilterAny InputFilter = iota
	InputFilterAsciiAlphabeticUppercase
	// InputFilterAsciiHexLowercase: session ids are lowercase hex; typing one
	// to confirm a destructive action must not survive stray characters or
	// capitals.
	InputFilterAsciiHexLowercase
)

// TextInput is a single- or multi-line readline-style editor. Offsets are
// byte offsets into the value, always on grapheme boundaries.
type TextInput struct {
	value      string
	cursor     int
	killBuffer string
	chainKill  bool
	// maxChars is the character limit, when one is set.
	maxChars  *int
	filter    InputFilter
	multiline bool
	// preferredColumn is the column a vertical motion aims for, so a short
	// line in between does not pull the caret left. Every other cursor change
	// clears it.
	preferredColumn *int
	// history is shared between copies of a TextInput, as Go copies the
	// pointer: never mutate it in place, replace it with an updated copy.
	history *inputHistory
}

type inputHistory struct {
	entries []string
	index   *int
	draft   string
}

func NewTextInput() TextInput {
	return TextInput{}
}

// TextInputMultiline is Rust's `TextInput::multiline()`.
func TextInputMultiline() TextInput {
	return TextInput{multiline: true}
}

func TextInputFromValue(value string) TextInput {
	input := NewTextInput()
	input.SetValue(value)
	return input
}

func (t TextInput) WithMaxChars(maxChars int) TextInput {
	t.maxChars = &maxChars
	t.enforceLimit()
	return t
}

func (t TextInput) WithFilter(filter InputFilter) TextInput {
	t.filter = filter
	value := t.value
	t.value = ""
	t.SetValue(value)
	return t
}

// String is the value, as Rust's Display.
func (t TextInput) String() string {
	return t.value
}

// EqualStr is Rust's `PartialEq<str>`.
func (t *TextInput) EqualStr(other string) bool {
	return t.value == other
}

// Equal is Rust's derived PartialEq.
func (t *TextInput) Equal(other *TextInput) bool {
	panic("unported: mj-chat/src/text_input.rs Equal")
}

// Extend inserts each character, as Rust's `Extend<char>`.
func (t *TextInput) Extend(characters string) {
	for _, character := range characters {
		t.InsertStr(string(character))
	}
}

// UsesVerticalKeys reports whether Up/Down belong to this editor rather than
// form navigation.
func (t *TextInput) UsesVerticalKeys() bool {
	return t.multiline || t.history != nil
}

func (t *TextInput) SetHistory(history []string) {
	if len(history) == 0 {
		t.history = nil
		return
	}
	t.history = &inputHistory{entries: history}
}

func (t *TextInput) Value() string {
	return t.value
}

func (t *TextInput) Cursor() int {
	return t.cursor
}

// SetCursor places the caret at a grapheme boundary, clamping offsets from a
// drawn field.
func (t *TextInput) SetCursor(offset int) {
	panic("unported: mj-chat/src/text_input.rs SetCursor")
}

func (t *TextInput) IsEmpty() bool {
	return t.value == ""
}

func (t *TextInput) SetValue(value string) {
	t.value = ""
	t.cursor = 0
	t.insertFiltered(value)
	t.cursor = len(t.value)
	t.leaveHistory()
}

func (t *TextInput) Clear() {
	t.value = ""
	t.cursor = 0
	t.preferredColumn = nil
	t.leaveHistory()
}

func (t *TextInput) WithCursorMarker(marker string) string {
	return t.value[:t.cursor] + marker + t.value[t.cursor:]
}

func (t *TextInput) Push(character rune) {
	t.InsertStr(string(character))
}

func (t *TextInput) PushStr(text string) {
	t.InsertStr(text)
}

func (t *TextInput) Pop() (rune, bool) {
	panic("unported: mj-chat/src/text_input.rs Pop")
}

func (t TextInput) IntoValue() string {
	return t.value
}

func (t *TextInput) InsertStr(text string) bool {
	before := t.value
	t.insertFiltered(text)
	t.leaveHistory()
	return t.value != before
}

func (t *TextInput) HandleKey(key tea.KeyPressMsg) EditOutcome {
	panic("unported: mj-chat/src/text_input.rs HandleKey")
}

// handleLineStart is Ctrl-A and Home. A single-line field has one line, so
// only a multiline field distinguishes the line start from the start of the
// value.
func (t *TextInput) handleLineStart(crossBoundary bool) EditOutcome {
	if !t.multiline {
		return t.moveTo(0)
	}
	t.MoveToLineStart(crossBoundary)
	return EditOutcomeHandled
}

// handleLineEnd is Ctrl-E and End; the mirror of handleLineStart.
func (t *TextInput) handleLineEnd(crossBoundary bool) EditOutcome {
	if !t.multiline {
		return t.moveTo(len(t.value))
	}
	t.MoveToLineEnd(crossBoundary)
	return EditOutcomeHandled
}

// handleVertical is Up and Down. A multiline field walks its own lines; a
// field with a history walks the history instead, because that is the only
// way to reach it from the keyboard.
func (t *TextInput) handleVertical(direction int) EditOutcome {
	if !t.multiline || t.history != nil {
		return t.moveHistory(direction)
	}
	t.MoveVertical(direction)
	return EditOutcomeHandled
}

// killLineBackward is Ctrl-U: a single-line field kills back to offset zero,
// which is what KillToLineStart computes for a value without newlines anyway.
func (t *TextInput) killLineBackward() bool {
	if !t.multiline {
		return t.kill(0, t.cursor, false)
	}
	return t.KillToLineStart()
}

// killLineForward is Ctrl-K; the mirror of killLineBackward.
func (t *TextInput) killLineForward(chained bool) bool {
	if !t.multiline {
		return t.kill(t.cursor, len(t.value), chained)
	}
	return t.KillToLineEnd(chained)
}

// MoveToLineStart moves to the start of the current line. With
// crossBoundary, a press that is already at the line start moves to the
// previous line's start.
func (t *TextInput) MoveToLineStart(crossBoundary bool) {
	start := LineStart(t.value, t.cursor)
	if crossBoundary && t.cursor == start && start > 0 {
		t.cursor = LineStart(t.value, start-1)
	} else {
		t.cursor = start
	}
	t.preferredColumn = nil
}

// MoveToLineEnd moves to the end of the current line; the mirror of
// MoveToLineStart.
func (t *TextInput) MoveToLineEnd(crossBoundary bool) {
	end := LineEnd(t.value, t.cursor)
	if crossBoundary && t.cursor == end && end < len(t.value) {
		t.cursor = LineEnd(t.value, end+1)
	} else {
		t.cursor = end
	}
	t.preferredColumn = nil
}

// MoveVertical moves one line up (negative direction) or down, aiming for
// the column the first vertical move started from so a short line in between
// does not pull the caret permanently left.
func (t *TextInput) MoveVertical(direction int) {
	panic("unported: mj-chat/src/text_input.rs MoveVertical")
}

// KillToLineStart kills back to the start of the line, or the preceding
// newline when the caret already sits there. Reports whether anything was
// killed.
func (t *TextInput) KillToLineStart() bool {
	start := LineStart(t.value, t.cursor)
	if start == t.cursor && start > 0 {
		return t.kill(start-1, start, false)
	}
	return t.kill(start, t.cursor, false)
}

// KillToLineEnd kills to the end of the line, or the newline itself when the
// caret already sits there. A chained kill appends to the kill buffer, in
// Emacs order, so a later yank restores the whole block.
func (t *TextInput) KillToLineEnd(chained bool) bool {
	end := LineEnd(t.value, t.cursor)
	if end == t.cursor && end < len(t.value) {
		return t.kill(end, end+1, chained)
	}
	return t.kill(t.cursor, end, chained)
}

// textInputChanged is Rust's `TextInput::changed`.
func textInputChanged(changed bool) EditOutcome {
	if changed {
		return EditOutcomeChanged
	}
	return EditOutcomeHandled
}

func (t *TextInput) moveTo(cursor int) EditOutcome {
	t.cursor = cursor
	t.preferredColumn = nil
	return EditOutcomeHandled
}

func (t *TextInput) backspace() bool {
	if t.cursor == 0 {
		return false
	}
	start := PreviousGrapheme(t.value, t.cursor)
	t.value = t.value[:start] + t.value[t.cursor:]
	t.cursor = start
	t.preferredColumn = nil
	t.leaveHistory()
	return true
}

func (t *TextInput) delete() bool {
	if t.cursor == len(t.value) {
		return false
	}
	end := NextGrapheme(t.value, t.cursor)
	t.value = t.value[:t.cursor] + t.value[end:]
	t.preferredColumn = nil
	t.leaveHistory()
	return true
}

// kill removes value[start:end] into the kill buffer, appending when
// appendKill is set.
func (t *TextInput) kill(start, end int, appendKill bool) bool {
	panic("unported: mj-chat/src/text_input.rs kill")
}

func (t *TextInput) yank() bool {
	if t.killBuffer == "" {
		return false
	}
	return t.InsertStr(t.killBuffer)
}

func (t *TextInput) previousWordStart() int {
	return PreviousWordStart(t.value, t.cursor)
}

func (t *TextInput) nextWordEnd() int {
	return NextWordEnd(t.value, t.cursor)
}

func (t *TextInput) moveHistory(delta int) EditOutcome {
	panic("unported: mj-chat/src/text_input.rs moveHistory")
}

func (t *TextInput) leaveHistory() {
	if t.history != nil && t.history.index != nil {
		updated := *t.history
		updated.index = nil
		t.history = &updated
	}
}

func (t *TextInput) insertFiltered(text string) {
	panic("unported: mj-chat/src/text_input.rs insertFiltered")
}

func (t *TextInput) enforceLimit() {
	panic("unported: mj-chat/src/text_input.rs enforceLimit")
}

// NormalizeKey folds terminal control characters and capitals into the
// Control and Shift modifiers the editors match on.
func NormalizeKey(code rune, modifiers tea.KeyMod) (rune, tea.KeyMod) {
	panic("unported: mj-chat/src/text_input.rs NormalizeKey")
}

func PreviousGrapheme(input string, cursor int) int {
	panic("unported: mj-chat/src/text_input.rs PreviousGrapheme")
}

func NextGrapheme(input string, cursor int) int {
	panic("unported: mj-chat/src/text_input.rs NextGrapheme")
}

// PreviousWordStart is the offset of the start of the word before cursor,
// readline style: a run of whitespace is skipped, then the run of same-class
// characters that follows it. "Class" is word characters versus the
// punctuation in the separator set.
func PreviousWordStart(text string, cursor int) int {
	panic("unported: mj-chat/src/text_input.rs PreviousWordStart")
}

// NextWordEnd is the offset of the end of the word after cursor; the mirror
// of PreviousWordStart.
func NextWordEnd(text string, cursor int) int {
	panic("unported: mj-chat/src/text_input.rs NextWordEnd")
}

// LineStart is the offset just after the newline that precedes cursor, or
// zero.
func LineStart(text string, cursor int) int {
	return strings.LastIndexByte(text[:cursor], '\n') + 1
}

// LineEnd is the offset of the newline that follows cursor, or the end of
// text.
func LineEnd(text string, cursor int) int {
	index := strings.IndexByte(text[cursor:], '\n')
	if index < 0 {
		return len(text)
	}
	return cursor + index
}

func wordClass(character rune) bool {
	const separators = "`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?"
	return strings.ContainsRune(separators, character)
}

// SingleLinePaste normalizes a terminal paste for a single-line field
// without collapsing spaces.
func SingleLinePaste(pasted string) string {
	trimmed := strings.Trim(pasted, "\r\n")
	return strings.Map(func(character rune) rune {
		if character == '\r' || character == '\n' {
			return ' '
		}
		return character
	}, trimmed)
}
