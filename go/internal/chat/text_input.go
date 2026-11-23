// Port of mj-chat/src/text_input.rs.

// Nonblocking readline-style editing for text fields embedded in Hel's TUIs.

package chat

import (
	"slices"
	"strings"
	"unicode"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"
	"github.com/clipperhouse/uax29/v2/graphemes"
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

// GoString is Rust's derived Debug.
func (o EditOutcome) GoString() string {
	switch o {
	case EditOutcomeHandled:
		return "Handled"
	case EditOutcomeChanged:
		return "Changed"
	}
	return "Unhandled"
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
	if t.value != other.value || t.cursor != other.cursor || t.killBuffer != other.killBuffer ||
		t.chainKill != other.chainKill || t.filter != other.filter || t.multiline != other.multiline ||
		!optionalIntsEqual(t.maxChars, other.maxChars) ||
		!optionalIntsEqual(t.preferredColumn, other.preferredColumn) {
		return false
	}
	if t.history == nil || other.history == nil {
		return t.history == nil && other.history == nil
	}
	return slices.Equal(t.history.entries, other.history.entries) &&
		optionalIntsEqual(t.history.index, other.history.index) &&
		t.history.draft == other.history.draft
}

// optionalIntsEqual compares two Rust `Option<usize>` values.
func optionalIntsEqual(a, b *int) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return *a == *b
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
	cursor := 0
	for _, index := range textInputGraphemeStarts(t.value) {
		if index > offset {
			break
		}
		cursor = index
	}
	if len(t.value) <= offset {
		cursor = len(t.value)
	}
	t.cursor = cursor
	t.chainKill = false
	t.preferredColumn = nil
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
	if t.cursor != len(t.value) || t.value == "" {
		return 0, false
	}
	character, size := utf8.DecodeLastRuneInString(t.value)
	t.value = t.value[:len(t.value)-size]
	t.cursor = len(t.value)
	t.preferredColumn = nil
	return character, true
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
	code, modifiers := NormalizeKey(KeyPressCode(key), key.Mod)
	chained := t.chainKill
	t.chainKill = false
	if modifiers.Contains(tea.ModCtrl) {
		var changed bool
		switch code {
		case 'a':
			return t.handleLineStart(true)
		case 'e':
			return t.handleLineEnd(true)
		case 'b':
			return t.moveTo(PreviousGrapheme(t.value, t.cursor))
		case 'f':
			return t.moveTo(NextGrapheme(t.value, t.cursor))
		case 'h':
			changed = t.backspace()
		case 'd':
			changed = t.delete()
		case 'u':
			changed = t.killLineBackward()
		case 'k':
			changed = t.killLineForward(chained)
			t.chainKill = true
		case 'w', tea.KeyBackspace:
			changed = t.kill(t.previousWordStart(), t.cursor, false)
		case 'y':
			changed = t.yank()
		case tea.KeyLeft:
			return t.moveTo(t.previousWordStart())
		case tea.KeyRight:
			return t.moveTo(t.nextWordEnd())
		case tea.KeyDelete:
			changed = t.kill(t.cursor, t.nextWordEnd(), false)
		case 'p', tea.KeyUp:
			return t.handleVertical(-1)
		case 'n', tea.KeyDown:
			return t.handleVertical(1)
		default:
			return EditOutcomeUnhandled
		}
		return textInputChanged(changed)
	}
	if modifiers.Contains(tea.ModAlt) {
		switch code {
		case 'b', tea.KeyLeft:
			return t.moveTo(t.previousWordStart())
		case 'f', tea.KeyRight:
			return t.moveTo(t.nextWordEnd())
		case 'd', tea.KeyDelete:
			return textInputChanged(t.kill(t.cursor, t.nextWordEnd(), false))
		case tea.KeyBackspace:
			return textInputChanged(t.kill(t.previousWordStart(), t.cursor, false))
		}
		return EditOutcomeUnhandled
	}
	switch code {
	case tea.KeyLeft:
		return t.moveTo(PreviousGrapheme(t.value, t.cursor))
	case tea.KeyRight:
		return t.moveTo(NextGrapheme(t.value, t.cursor))
	case tea.KeyHome:
		return t.handleLineStart(false)
	case tea.KeyEnd:
		return t.handleLineEnd(false)
	case tea.KeyBackspace:
		return textInputChanged(t.backspace())
	case tea.KeyDelete:
		return textInputChanged(t.delete())
	case tea.KeyUp:
		return t.handleVertical(-1)
	case tea.KeyDown:
		return t.handleVertical(1)
	}
	if IsCharKeyCode(code) && !modifiers.Contains(tea.ModSuper) && !unicode.IsControl(code) {
		// A terminal reports composed text (an IME commit, a shifted letter)
		// as the press's text; insert that rather than the bare code.
		text := string(code)
		if key.Text != "" {
			text = key.Text
		}
		return textInputChanged(t.InsertStr(text))
	}
	return EditOutcomeUnhandled
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
	start := LineStart(t.value, t.cursor)
	var column int
	if t.preferredColumn != nil {
		column = *t.preferredColumn
	} else {
		column = len(textInputGraphemeStarts(t.value[start:t.cursor]))
	}
	var targetStart int
	if direction < 0 {
		if start == 0 {
			t.cursor = 0
			t.preferredColumn = nil
			return
		}
		targetStart = LineStart(t.value, start-1)
	} else {
		end := LineEnd(t.value, t.cursor)
		if end == len(t.value) {
			t.cursor = len(t.value)
			t.preferredColumn = nil
			return
		}
		targetStart = end + 1
	}
	targetEnd := LineEnd(t.value, targetStart)
	t.cursor = targetEnd
	if starts := textInputGraphemeStarts(t.value[targetStart:targetEnd]); column < len(starts) {
		t.cursor = targetStart + starts[column]
	}
	t.preferredColumn = &column
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
	if start >= end {
		return false
	}
	killed := t.value[start:end]
	if appendKill {
		t.killBuffer += killed
	} else {
		t.killBuffer = killed
	}
	t.value = t.value[:start] + t.value[end:]
	t.cursor = start
	t.preferredColumn = nil
	t.leaveHistory()
	return true
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
	if t.history == nil {
		return EditOutcomeUnhandled
	}
	history := *t.history
	var next *int
	switch {
	case history.index == nil && delta < 0:
		history.draft = t.value
		last := len(history.entries) - 1
		next = &last
	case history.index == nil:
		next = nil
	case delta < 0:
		previous := max(*history.index-1, 0)
		next = &previous
	case *history.index+1 < len(history.entries):
		following := *history.index + 1
		next = &following
	default:
		next = nil
	}
	value := history.draft
	if next != nil && *next >= 0 && *next < len(history.entries) {
		value = history.entries[*next]
	}
	t.history = &history
	t.SetValue(value)
	updated := *t.history
	updated.index = next
	t.history = &updated
	return EditOutcomeChanged
}

func (t *TextInput) leaveHistory() {
	if t.history != nil && t.history.index != nil {
		updated := *t.history
		updated.index = nil
		t.history = &updated
	}
}

func (t *TextInput) insertFiltered(text string) {
	t.preferredColumn = nil
	for _, character := range text {
		if unicode.IsControl(character) && !(t.multiline && (character == '\n' || character == '\t')) {
			continue
		}
		switch t.filter {
		case InputFilterAsciiAlphabeticUppercase:
			if !isASCIIAlphabetic(character) {
				continue
			}
			character = asciiToUpper(character)
		case InputFilterAsciiHexLowercase:
			if !isASCIIHexDigit(character) {
				continue
			}
			character = asciiToLower(character)
		}
		if t.maxChars != nil && utf8.RuneCountInString(t.value) >= *t.maxChars {
			break
		}
		encoded := string(character)
		t.value = t.value[:t.cursor] + encoded + t.value[t.cursor:]
		t.cursor += len(encoded)
	}
}

func (t *TextInput) enforceLimit() {
	if t.maxChars == nil {
		return
	}
	count := 0
	for index := range t.value {
		if count == *t.maxChars {
			t.value = t.value[:index]
			break
		}
		count++
	}
	t.cursor = min(t.cursor, len(t.value))
	t.preferredColumn = nil
}

func isASCIIAlphabetic(character rune) bool {
	return ('a' <= character && character <= 'z') || ('A' <= character && character <= 'Z')
}

func isASCIIHexDigit(character rune) bool {
	return ('0' <= character && character <= '9') ||
		('a' <= character && character <= 'f') || ('A' <= character && character <= 'F')
}

func asciiToUpper(character rune) rune {
	if 'a' <= character && character <= 'z' {
		return character - 'a' + 'A'
	}
	return character
}

func asciiToLower(character rune) rune {
	if 'A' <= character && character <= 'Z' {
		return character - 'A' + 'a'
	}
	return character
}

// textASCIILower is Rust's `str::to_ascii_lowercase`.
func textASCIILower(text string) string {
	return strings.Map(asciiToLower, text)
}

// textASCIIEqualFold is Rust's `str::eq_ignore_ascii_case`.
func textASCIIEqualFold(left, right string) bool {
	return len(left) == len(right) && textASCIILower(left) == textASCIILower(right)
}

// keyModifierMask is the part of a Bubble Tea KeyMod that crossterm reports
// as KeyModifiers. Bubble Tea also carries lock states there, which crossterm
// keeps apart.
const keyModifierMask = tea.ModShift | tea.ModAlt | tea.ModCtrl | tea.ModMeta | tea.ModHyper | tea.ModSuper

// IsCharKeyCode reports whether code is what crossterm calls
// `KeyCode::Char`: a character rather than a named key. Bubble Tea gives Tab,
// Enter, Escape and Backspace character codes; crossterm names them.
func IsCharKeyCode(code rune) bool {
	return code < tea.KeyExtended && code != tea.KeyTab && code != tea.KeyEnter &&
		code != tea.KeyEscape && code != tea.KeyBackspace
}

// KeyPressCode is the key code crossterm would report for key: the typed
// character for a printable key (Bubble Tea reports a shifted letter as its
// lowercase code plus the text it produced), otherwise Bubble Tea's code,
// with the keypad's navigation keys folded onto the main ones as crossterm
// reports them.
func KeyPressCode(key tea.KeyPressMsg) rune {
	if character, size := utf8.DecodeRuneInString(key.Text); size > 0 && size == len(key.Text) &&
		character != utf8.RuneError {
		return character
	}
	switch key.Code {
	case tea.KeyKpUp:
		return tea.KeyUp
	case tea.KeyKpDown:
		return tea.KeyDown
	case tea.KeyKpLeft:
		return tea.KeyLeft
	case tea.KeyKpRight:
		return tea.KeyRight
	case tea.KeyKpHome:
		return tea.KeyHome
	case tea.KeyKpEnd:
		return tea.KeyEnd
	case tea.KeyKpDelete:
		return tea.KeyDelete
	case tea.KeyKpPgUp:
		return tea.KeyPgUp
	case tea.KeyKpPgDown:
		return tea.KeyPgDown
	case tea.KeyKpInsert:
		return tea.KeyInsert
	case tea.KeyKpEnter:
		return tea.KeyEnter
	}
	return key.Code
}

// NormalizeKey folds terminal control characters and capitals into the
// Control and Shift modifiers the editors match on.
//
// code is a crossterm-style key code (see KeyPressCode); the lock states
// Bubble Tea carries in modifiers are dropped, as crossterm keeps them out of
// its modifiers.
func NormalizeKey(code rune, modifiers tea.KeyMod) (rune, tea.KeyMod) {
	modifiers &= keyModifierMask
	if !IsCharKeyCode(code) {
		return code, modifiers
	}
	if modifiers == 0 && code >= 1 && code <= 26 {
		return code - 1 + 'a', modifiers | tea.ModCtrl
	}
	if 'A' <= code && code <= 'Z' {
		modifiers |= tea.ModShift
		if modifiers&(tea.ModCtrl|tea.ModAlt|tea.ModSuper) != 0 {
			return asciiToLower(code), modifiers
		}
	}
	return code, modifiers
}

// textInputGraphemeStarts is the byte offset of every extended grapheme
// cluster in text (unicode-segmentation's `grapheme_indices(true)`).
func textInputGraphemeStarts(text string) []int {
	var starts []int
	iterator := graphemes.FromString(text)
	for iterator.Next() {
		starts = append(starts, iterator.Start())
	}
	return starts
}

func PreviousGrapheme(input string, cursor int) int {
	starts := textInputGraphemeStarts(input[:cursor])
	if len(starts) == 0 {
		return 0
	}
	return starts[len(starts)-1]
}

func NextGrapheme(input string, cursor int) int {
	iterator := graphemes.FromString(input[cursor:])
	if !iterator.Next() {
		return len(input)
	}
	return cursor + iterator.End()
}

// PreviousWordStart is the offset of the start of the word before cursor,
// readline style: a run of whitespace is skipped, then the run of same-class
// characters that follows it. "Class" is word characters versus the
// punctuation in the separator set.
func PreviousWordStart(text string, cursor int) int {
	trimmed := strings.TrimRightFunc(text[:cursor], unicode.IsSpace)
	if trimmed == "" {
		return 0
	}
	runStart := 0
	if index := strings.LastIndexFunc(trimmed, unicode.IsSpace); index >= 0 {
		_, size := utf8.DecodeRuneInString(trimmed[index:])
		runStart = index + size
	}
	run := trimmed[runStart:]
	start := runStart + len(run)
	known, class := false, false
	for index := len(run); index > 0; {
		character, size := utf8.DecodeLastRuneInString(run[:index])
		index -= size
		next := wordClass(character)
		if known && class != next {
			break
		}
		known, class = true, next
		start = runStart + index
	}
	return start
}

// NextWordEnd is the offset of the end of the word after cursor; the mirror
// of PreviousWordStart.
func NextWordEnd(text string, cursor int) int {
	suffix := text[cursor:]
	nonSpace := strings.IndexFunc(suffix, func(character rune) bool { return !unicode.IsSpace(character) })
	if nonSpace < 0 {
		return len(text)
	}
	run := suffix[nonSpace:]
	end := 0
	known, class := false, false
	for index, character := range run {
		if unicode.IsSpace(character) {
			break
		}
		next := wordClass(character)
		if known && class != next {
			break
		}
		known, class = true, next
		end = index + utf8.RuneLen(character)
	}
	return cursor + nonSpace + end
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
