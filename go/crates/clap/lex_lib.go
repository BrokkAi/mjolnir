// Port of clap_lex 1.1.0 src/lib.rs

package clap

import (
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// lexRawArgs is clap_lex::RawArgs: the command-line arguments.
//
// An OsString is a Go string here: any bytes, UTF-8 or not.
type lexRawArgs struct {
	items []string
}

// lexRawArgsNew is RawArgs::new.
func lexRawArgsNew(iter []string) *lexRawArgs {
	items := make([]string, len(iter))
	copy(items, iter)
	return &lexRawArgs{items: items}
}

// cursor is RawArgs::cursor: a position before the first argument.
func (r *lexRawArgs) cursor() lexArgCursor {
	return lexArgCursor{cursor: 0}
}

// next is RawArgs::next: advance the cursor, returning the next ParsedArg.
func (r *lexRawArgs) next(cursor *lexArgCursor) rs.Option[lexParsedArg] {
	next, ok := r.nextOs(cursor).Get()
	if !ok {
		return rs.None[lexParsedArg]()
	}
	return rs.Some(lexParsedArg{inner: next})
}

// nextOs is RawArgs::next_os: advance the cursor, returning a raw argument
// value.
func (r *lexRawArgs) nextOs(cursor *lexArgCursor) rs.Option[string] {
	next := r.peekOs(cursor)
	if cursor.cursor < rs.UsizeMax {
		cursor.cursor++
	}
	return next
}

// peek is RawArgs::peek: return the next ParsedArg.
func (r *lexRawArgs) peek(cursor *lexArgCursor) rs.Option[lexParsedArg] {
	next, ok := r.peekOs(cursor).Get()
	if !ok {
		return rs.None[lexParsedArg]()
	}
	return rs.Some(lexParsedArg{inner: next})
}

// peekOs is RawArgs::peek_os: return a raw argument value.
func (r *lexRawArgs) peekOs(cursor *lexArgCursor) rs.Option[string] {
	if cursor.cursor < len(r.items) {
		return rs.Some(r.items[cursor.cursor])
	}
	return rs.None[string]()
}

// remaining is RawArgs::remaining: return all remaining raw arguments,
// advancing the cursor to the end.
func (r *lexRawArgs) remaining(cursor *lexArgCursor) []string {
	start := cursor.cursor
	if start > len(r.items) {
		// Rust's slice index would panic here; the cursor only passes the end
		// by one, after next_os found nothing.
		start = len(r.items)
	}
	remaining := r.items[start:]
	cursor.cursor = len(r.items)
	return remaining
}

// lexSeekFrom is std::io::SeekFrom, as clap_lex re-exports it.
type lexSeekFrom struct {
	kind   uint8 // 0 Start, 1 End, 2 Current
	offset int64
}

func lexSeekFromStart(pos uint64) lexSeekFrom  { return lexSeekFrom{kind: 0, offset: int64(pos)} }
func lexSeekFromEnd(pos int64) lexSeekFrom     { return lexSeekFrom{kind: 1, offset: pos} }
func lexSeekFromCurrent(pos int64) lexSeekFrom { return lexSeekFrom{kind: 2, offset: pos} }

func saturatingAddI64(a, b int64) int64 {
	sum := a + b
	if a >= 0 && b >= 0 && sum < 0 {
		return 1<<63 - 1
	}
	if a < 0 && b < 0 && sum >= 0 {
		return -1 << 63
	}
	return sum
}

// seek is RawArgs::seek: adjust the cursor's position.
func (r *lexRawArgs) seek(cursor *lexArgCursor, pos lexSeekFrom) {
	var target int64
	switch pos.kind {
	case 0:
		target = pos.offset
	case 1:
		target = max(saturatingAddI64(int64(len(r.items)), pos.offset), 0)
	default:
		target = max(saturatingAddI64(int64(cursor.cursor), pos.offset), 0)
	}
	if target > int64(len(r.items)) || target < 0 {
		target = int64(len(r.items))
	}
	cursor.cursor = int(target)
}

// insert is RawArgs::insert: inject arguments before the RawArgs::next.
func (r *lexRawArgs) insert(cursor *lexArgCursor, insertItems []string) {
	items := make([]string, 0, len(r.items)+len(insertItems))
	items = append(items, r.items[:cursor.cursor]...)
	items = append(items, insertItems...)
	items = append(items, r.items[cursor.cursor:]...)
	r.items = items
}

// isEnd is RawArgs::is_end: any remaining args?
func (r *lexRawArgs) isEnd(cursor *lexArgCursor) bool {
	return r.peekOs(cursor).IsNone()
}

// lexArgCursor is clap_lex::ArgCursor: a position within RawArgs.
type lexArgCursor struct {
	cursor int
}

// lexParsedArg is clap_lex::ParsedArg: a command-line argument.
type lexParsedArg struct {
	inner string
}

// isEmpty is ParsedArg::is_empty: argument is length of 0.
func (p lexParsedArg) isEmpty() bool { return len(p.inner) == 0 }

// isStdio is ParsedArg::is_stdio: does the argument look like a stdio
// argument (`-`).
func (p lexParsedArg) isStdio() bool { return p.inner == "-" }

// isEscape is ParsedArg::is_escape: does the argument look like an argument
// escape (`--`).
func (p lexParsedArg) isEscape() bool { return p.inner == "--" }

// isNegativeNumber is ParsedArg::is_negative_number: does the argument look
// like a negative number? This won't parse the number in full but attempts to
// see if this looks like a number.
func (p lexParsedArg) isNegativeNumber() bool {
	s, ok := p.toValue()
	if !ok {
		return false
	}
	if len(s) == 0 || s[0] != '-' {
		return false
	}
	return lexIsNumber(s[1:])
}

// lexLong is the pair ParsedArg::to_long returns: the flag, as text when it
// is UTF-8, and the value after `=` when there is one.
type lexLong struct {
	flag      string
	flagValid bool
	value     rs.Option[string]
}

// toLong is ParsedArg::to_long: treat as a long-flag.
func (p lexParsedArg) toLong() rs.Option[lexLong] {
	raw := p.inner
	remainder, ok := lexStripPrefix(raw, "--")
	if !ok {
		return rs.None[lexLong]()
	}
	if len(remainder) == 0 {
		return rs.None[lexLong]()
	}

	flag := remainder
	value := rs.None[string]()
	if p0, p1, found := lexSplitOnce(remainder, "="); found {
		flag = p0
		value = rs.Some(p1)
	}
	return rs.Some(lexLong{flag: flag, flagValid: utf8.ValidString(flag), value: value})
}

// isLong is ParsedArg::is_long: can treat as a long-flag.
func (p lexParsedArg) isLong() bool {
	return lexStartsWith(p.inner, "--") && !p.isEscape()
}

// toShort is ParsedArg::to_short: treat as a short-flag.
func (p lexParsedArg) toShort() rs.Option[lexShortFlags] {
	remainderOs, ok := lexStripPrefix(p.inner, "-")
	if !ok {
		return rs.None[lexShortFlags]()
	}
	if lexStartsWith(remainderOs, "-") {
		return rs.None[lexShortFlags]()
	}
	if len(remainderOs) == 0 {
		return rs.None[lexShortFlags]()
	}
	return rs.Some(lexShortFlagsNew(remainderOs))
}

// isShort is ParsedArg::is_short: can treat as a short-flag.
func (p lexParsedArg) isShort() bool {
	return lexStartsWith(p.inner, "-") && !p.isStdio() && !lexStartsWith(p.inner, "--")
}

// toValueOs is ParsedArg::to_value_os: treat as a value.
//
// NOTE: May return a flag or an escape.
func (p lexParsedArg) toValueOs() string { return p.inner }

// toValue is ParsedArg::to_value: treat as a value. The second result is
// false when the argument is not UTF-8 (Rust's Err(&OsStr)).
//
// NOTE: May return a flag or an escape.
func (p lexParsedArg) toValue() (string, bool) {
	return p.inner, utf8.ValidString(p.inner)
}

// display is ParsedArg::display: safely print an argument that may contain
// non-UTF8 content. This may perform lossy conversion, depending on the
// platform.
func (p lexParsedArg) display() string {
	return toStringLossy(p.inner)
}

// lexShortFlags is clap_lex::ShortFlags: walk through short flags within a
// ParsedArg.
//
// It is a value: copying it is Rust's clone().
type lexShortFlags struct {
	inner string
	// pos is the offset of the next char of the UTF-8 prefix; validLen is the
	// length of that prefix.
	pos      int
	validLen int
	// invalidSuffix reports whether the bytes after the UTF-8 prefix are
	// still to be returned.
	invalidSuffix bool
}

func lexShortFlagsNew(inner string) lexShortFlags {
	validLen := utf8ValidUpTo(inner)
	return lexShortFlags{
		inner:         inner,
		pos:           0,
		validLen:      validLen,
		invalidSuffix: validLen < len(inner),
	}
}

// advanceBy is ShortFlags::advance_by: move the iterator forward by n short
// flags. The result is the number of flags it could move, and whether all n
// were there.
func (s *lexShortFlags) advanceBy(n int) (int, bool) {
	for i := 0; i < n; i++ {
		flag, ok := s.nextFlag().Get()
		if !ok {
			return i, false
		}
		if !flag.valid {
			return i, false
		}
	}
	return n, true
}

// isEmpty is ShortFlags::is_empty: no short flags left.
func (s *lexShortFlags) isEmpty() bool {
	return !s.invalidSuffix && s.pos >= s.validLen
}

// isNegativeNumber is ShortFlags::is_negative_number: does the short flag
// look like a number?
//
// Ideally call this before doing any iterator.
func (s *lexShortFlags) isNegativeNumber() bool {
	return !s.invalidSuffix && lexIsNumber(s.inner[s.pos:s.validLen])
}

// lexShortFlag is the Result<char, &OsStr> ShortFlags yields: a flag, or the
// bytes that are not UTF-8.
type lexShortFlag struct {
	valid bool
	flag  rune
	rest  string
}

// nextFlag is ShortFlags::next_flag: advance the iterator, returning the next
// short flag on success.
//
// On error, returns the invalid-UTF8 value.
func (s *lexShortFlags) nextFlag() rs.Option[lexShortFlag] {
	if s.pos < s.validLen {
		flag, size := utf8.DecodeRuneInString(s.inner[s.pos:s.validLen])
		s.pos += size
		return rs.Some(lexShortFlag{valid: true, flag: flag})
	}

	if s.invalidSuffix {
		s.invalidSuffix = false
		return rs.Some(lexShortFlag{valid: false, rest: s.inner[s.validLen:]})
	}

	return rs.None[lexShortFlag]()
}

// nextValueOs is ShortFlags::next_value_os: advance the iterator, returning
// everything left as a value.
func (s *lexShortFlags) nextValueOs() rs.Option[string] {
	if s.pos < s.validLen {
		index := s.pos
		s.pos = s.validLen
		s.invalidSuffix = false
		return rs.Some(s.inner[index:])
	}

	if s.invalidSuffix {
		s.invalidSuffix = false
		return rs.Some(s.inner[s.validLen:])
	}

	return rs.None[string]()
}

func lexIsNumber(arg string) bool {
	// Return true if this looks like an integer or a float where it's all
	// digits plus an optional single dot after some digits.
	//
	// For floats allow forms such as `1.`, `1.2`, `1.2e10`, etc.
	seenDot := false
	positionOfE := -1
	for i := 0; i < len(arg); i++ {
		c := arg[i]
		switch {
		// Digits are always valid
		case c >= '0' && c <= '9':

		// Allow a `.`, but only one, only if it comes before an
		// optional exponent, and only if it's not the first character.
		case c == '.' && !seenDot && positionOfE < 0 && i > 0:
			seenDot = true

		// Allow an exponent `e`/`E` but only at most one after the first
		// character.
		case (c == 'e' || c == 'E') && positionOfE < 0 && i > 0:
			positionOfE = i

		default:
			return false
		}
	}

	// Disallow `-1e` which isn't a valid float since it doesn't actually have
	// an exponent.
	if positionOfE >= 0 {
		return positionOfE != len(arg)-1
	}
	return true
}
