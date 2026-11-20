// Port of alloc 1.99.1 src/string.rs, and core 1.99.1 src/char/decode.rs for
// the UTF-16 decoding it uses

package strs

import (
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// A Rust `String` is a Go string. The methods that change a String in place
// take a pointer to the variable that holds it: `s.truncate(3)` is
// `strs.Truncate(&s, 3)`.

// FromUtf8Error is string::FromUtf8Error: the error of StringFromUtf8. It
// holds the bytes that were not UTF-8.
type FromUtf8Error struct {
	bytes []byte
	error Utf8Error
}

// AsBytes is FromUtf8Error::as_bytes: the bytes that were converted.
func (e *FromUtf8Error) AsBytes() []byte { return e.bytes }

// IntoUtf8Lossy is FromUtf8Error::into_utf8_lossy: the bytes as a string,
// with each invalid sequence replaced by U+FFFD.
func (e *FromUtf8Error) IntoUtf8Lossy() string {
	return FromUtf8Lossy(e.bytes)
}

// IntoBytes is FromUtf8Error::into_bytes: the bytes that were converted.
func (e *FromUtf8Error) IntoBytes() []byte { return e.bytes }

// Utf8Error is FromUtf8Error::utf8_error: where and why the conversion
// failed.
func (e *FromUtf8Error) Utf8Error() Utf8Error { return e.error }

// String is the error's Display text, which is its Utf8Error's.
func (e *FromUtf8Error) String() string { return e.error.String() }

// Error makes a FromUtf8Error a Go error.
func (e *FromUtf8Error) Error() string { return e.String() }

// FmtDisplay is `impl Display for FromUtf8Error`.
func (e *FromUtf8Error) FmtDisplay(f *rs.Formatter) error { return rs.DisplayFmt(e.error, f) }

// FmtDebug is the derived `Debug`:
// `FromUtf8Error { bytes: [255], error: Utf8Error { valid_up_to: 0, error_len: Some(1) } }`.
func (e *FromUtf8Error) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("FromUtf8Error").Field("bytes", e.bytes).Field("error", e.error).Finish()
}

func (e *FromUtf8Error) debugString() string {
	var text strings.Builder
	text.WriteString("FromUtf8Error { bytes: [")
	for i, b := range e.bytes {
		if i > 0 {
			text.WriteString(", ")
		}
		text.WriteString(strconv.Itoa(int(b)))
	}
	text.WriteString("], error: ")
	text.WriteString(e.error.debugString())
	text.WriteString(" }")
	return text.String()
}

// FromUtf16Error is string::FromUtf16Error: the error of FromUtf16.
type FromUtf16Error struct {
	kind fromUtf16ErrorKind
}

type fromUtf16ErrorKind uint8

const (
	fromUtf16ErrorKind_LoneSurrogate fromUtf16ErrorKind = iota
	fromUtf16ErrorKind_OddBytes
)

var _ = rs.RegisterEnum[fromUtf16ErrorKind]("FromUtf16ErrorKind",
	rs.Const(fromUtf16ErrorKind_LoneSurrogate, "LoneSurrogate"),
	rs.Const(fromUtf16ErrorKind_OddBytes, "OddBytes"),
)

// String is the error's Display text.
func (e *FromUtf16Error) String() string {
	switch e.kind {
	case fromUtf16ErrorKind_LoneSurrogate:
		return "invalid utf-16: lone surrogate found"
	default:
		return "invalid utf-16: odd number of bytes"
	}
}

// Error makes a FromUtf16Error a Go error.
func (e *FromUtf16Error) Error() string { return e.String() }

// FmtDisplay is `impl Display for FromUtf16Error`.
func (e *FromUtf16Error) FmtDisplay(f *rs.Formatter) error { return f.Pad(e.String()) }

// FmtDebug is the derived `Debug`: `FromUtf16Error { kind: LoneSurrogate }`.
func (e *FromUtf16Error) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("FromUtf16Error").Field("kind", e.kind).Finish()
}

func (e *FromUtf16Error) debugString() string {
	if e.kind == fromUtf16ErrorKind_LoneSurrogate {
		return "FromUtf16Error { kind: LoneSurrogate }"
	}
	return "FromUtf16Error { kind: OddBytes }"
}

// StringFromUtf8 is String::from_utf8: v as a string when it is valid UTF-8,
// and otherwise a *FromUtf8Error, which prints as FromUtf8's Utf8Error does
// with `{}` but holds the bytes and shows them with `{:?}`.
func StringFromUtf8(v []byte) (string, error) {
	if err, ok := validateUtf8(v); !ok {
		return "", &FromUtf8Error{bytes: v, error: err}
	}
	return string(v), nil
}

// FromUtf8Lossy is String::from_utf8_lossy: v as a string, with each invalid
// sequence replaced by one U+FFFD. What counts as one sequence is Rust's rule
// (the maximal prefix of a valid sequence, as Utf8Chunks cuts it): three
// bytes that begin a four-byte char are one replacement, where Go's own
// conversion writes three.
func FromUtf8Lossy(v []byte) string {
	if utf8.Valid(v) {
		return string(v)
	}

	const replacement = "�"

	var res strings.Builder
	res.Grow(len(v))
	for source := v; len(source) > 0; {
		chunk, rest := nextUtf8Chunk(source)
		source = rest
		res.WriteString(chunk.valid)
		if len(chunk.invalid) > 0 {
			res.WriteString(replacement)
		}
	}
	return res.String()
}

// decodeUtf16 is char::decode_utf16: the chars of v, with ok false in place
// of each unpaired surrogate.
func decodeUtf16(v []uint16, yield func(c rune, ok bool) bool) {
	for i := 0; i < len(v); {
		u := v[i]
		i++

		if u&0xF800 != 0xD800 {
			// not a surrogate
			if !yield(rune(u), true) {
				return
			}
		} else if u >= 0xDC00 {
			// a trailing surrogate
			if !yield(rune(u), false) {
				return
			}
		} else {
			if i >= len(v) {
				// eof
				yield(rune(u), false)
				return
			}
			u2 := v[i]
			if u2 < 0xDC00 || u2 > 0xDFFF {
				// not a trailing surrogate so we're not a valid
				// surrogate pair, so rewind to redecode u2 next time.
				if !yield(rune(u), false) {
					return
				}
				continue
			}
			i++

			// all ok, so lets decode it.
			c := (uint32(u&0x3ff)<<10 | uint32(u2&0x3ff)) + 0x1_0000
			if !yield(rune(c), true) {
				return
			}
		}
	}
}

// FromUtf16 is String::from_utf16: the string v encodes, or a *FromUtf16Error
// when v holds an unpaired surrogate.
func FromUtf16(v []uint16) (string, error) {
	ret := make([]byte, 0, len(v))
	valid := true
	var buffer [CharMaxLenUtf8]byte
	decodeUtf16(v, func(c rune, ok bool) bool {
		if !ok {
			valid = false
			return false
		}
		ret = append(ret, buffer[:encodeUtf8Raw(c, buffer[:])]...)
		return true
	})
	if !valid {
		return "", &FromUtf16Error{kind: fromUtf16ErrorKind_LoneSurrogate}
	}
	return string(ret), nil
}

// FromUtf16Lossy is String::from_utf16_lossy: the string v encodes, with each
// unpaired surrogate replaced by U+FFFD.
func FromUtf16Lossy(v []uint16) string {
	ret := make([]byte, 0, len(v))
	var buffer [CharMaxLenUtf8]byte
	decodeUtf16(v, func(c rune, ok bool) bool {
		if !ok {
			c = CharReplacementCharacter
		}
		ret = append(ret, buffer[:encodeUtf8Raw(c, buffer[:])]...)
		return true
	})
	return string(ret)
}

// Truncate is String::truncate: it shortens *s to newLen bytes, and does
// nothing when newLen is greater than the length. It panics when newLen is
// inside a char.
func Truncate(s *string, newLen int) {
	if uint(newLen) <= uint(len(*s)) {
		if !IsCharBoundary(*s, newLen) {
			rs.PanicAt(1, "assertion failed: self.is_char_boundary(new_len)")
		}
		*s = (*s)[:newLen]
	}
}

// Pop is String::pop: it removes the last char of *s and returns it, or
// returns None when *s is empty.
func Pop(s *string) rs.Option[rune] {
	if *s == "" {
		return rs.None[rune]()
	}
	ch, size := nextCodePointReverse(*s)
	*s = (*s)[:len(*s)-size]
	return rs.Some(ch)
}

// Remove is String::remove: it removes the char at byte idx of *s and returns
// it. It panics when idx is at or beyond the end, or inside a char.
func Remove(s *string, idx int) rune {
	if !IsCharBoundary(*s, idx) {
		rs.PanicAt(1, sliceErrorFail(*s, idx, len(*s)))
	}
	if idx == len(*s) {
		rs.PanicAt(1, "cannot remove a char from the end of a string")
	}
	ch, size := nextCodePoint((*s)[idx:])
	*s = (*s)[:idx] + (*s)[idx+size:]
	return ch
}

// Retain is String::retain: it keeps the chars of *s that f accepts, in
// order, calling f once for each char.
func Retain(s *string, f func(rune) bool) {
	old := *s
	var kept []byte
	for idx := 0; idx < len(old); {
		ch, size := nextCodePoint(old[idx:])
		if !f(ch) {
			if kept == nil {
				kept = make([]byte, idx, len(old))
				copy(kept, old[:idx])
			}
		} else if kept != nil {
			kept = append(kept, old[idx:idx+size]...)
		}
		idx += size
	}
	if kept != nil {
		*s = string(kept)
	}
}

// Insert is String::insert: it inserts ch at byte idx of *s. It panics when
// idx is beyond the end or inside a char.
func Insert(s *string, idx int, ch rune) {
	if !IsCharBoundary(*s, idx) {
		rs.PanicAt(1, "assertion failed: self.is_char_boundary(idx)")
	}
	*s = (*s)[:idx] + CharEncodeUtf8(ch) + (*s)[idx:]
}

// InsertStr is String::insert_str: it inserts text at byte idx of *s. It
// panics when idx is beyond the end or inside a char.
func InsertStr(s *string, idx int, text string) {
	if !IsCharBoundary(*s, idx) {
		rs.PanicAt(1, "assertion failed: self.is_char_boundary(idx)")
	}
	*s = (*s)[:idx] + text + (*s)[idx:]
}

// SplitOff is String::split_off: it cuts *s at byte at, keeps the first part
// in *s and returns the rest. It panics when at is beyond the end or inside a
// char.
func SplitOff(s *string, at int) string {
	if !IsCharBoundary(*s, at) {
		rs.PanicAt(1, "assertion failed: self.is_char_boundary(at)")
	}
	other := (*s)[at:]
	*s = (*s)[:at]
	return other
}

// sliceIndexFail is slice_index_fail: the message of the panic for a range
// start..end that a slice of the given length cannot be indexed with.
func sliceIndexFail(start, end, length int) string {
	if uint(start) > uint(length) {
		return "range start index " + usizeText(start) + " out of range for slice of length " + usizeText(length)
	}

	if uint(end) > uint(length) {
		return "range end index " + usizeText(end) + " out of range for slice of length " + usizeText(length)
	}

	if uint(start) > uint(end) {
		return "slice index starts at " + usizeText(start) + " but ends at " + usizeText(end)
	}

	// Only reachable if the range was a `RangeInclusive` or a
	// `RangeToInclusive`, with `end == len`.
	return "range end index " + usizeText(end) + " out of range for slice of length " + usizeText(length)
}

// sliceRange is slice::range (into_slice_range) over a slice of the given
// length, for the range `start..end`, or `start..=end` when inclusive: the
// end of the same range written `start..end`, or the message of the panic
// when the range does not fit.
func sliceRange(start, end, length int, inclusive bool) (int, string) {
	if inclusive {
		if uint(end) >= uint(length) {
			return 0, sliceIndexFail(0, end, length)
		}
		// Cannot overflow because `end < len` implies `end < usize::MAX`.
		end++
	} else if uint(end) > uint(length) {
		return 0, sliceIndexFail(0, end, length)
	}

	if uint(start) > uint(end) {
		return 0, sliceIndexFail(start, end, length)
	}
	return end, ""
}

// Drain is String::drain for the range `start..end`: it removes those bytes
// from *s and returns them. (`s.drain(..n)` is Drain(&s, 0, n), `s.drain(n..)`
// is Drain(&s, n, len(s)), and `s.drain(a..=b)` is DrainInclusive.) Rust
// returns an iterator over the removed chars and removes them when it is
// dropped; here they are removed at once and returned as a string. It panics
// when the range is out of bounds, backwards or cuts a char.
func Drain(s *string, start, end int) string {
	return drain(s, start, end, false)
}

// DrainInclusive is String::drain for the range `start..=end`
// (`s.drain(..=end)` is DrainInclusive(&s, 0, end)). It panics as Drain does;
// here an end at or beyond the length is out of bounds.
func DrainInclusive(s *string, start, end int) string {
	return drain(s, start, end, true)
}

func drain(s *string, start, end int, inclusive bool) string {
	end, message := sliceRange(start, end, len(*s), inclusive)
	if message != "" {
		rs.PanicAt(2, message)
	}
	if !IsCharBoundary(*s, start) {
		rs.PanicAt(2, "assertion failed: self.is_char_boundary(start)")
	}
	if !IsCharBoundary(*s, end) {
		rs.PanicAt(2, "assertion failed: self.is_char_boundary(end)")
	}
	drained := (*s)[start:end]
	*s = (*s)[:start] + (*s)[end:]
	return drained
}

// ReplaceRange is String::replace_range for the range `start..end`: it
// replaces those bytes of *s with replaceWith, which may be of another
// length. (`..n` is 0, n, `n..` is n, len(s), and `a..=b` is
// ReplaceRangeInclusive.) It panics when the range is out of bounds,
// backwards or cuts a char.
func ReplaceRange(s *string, start, end int, replaceWith string) {
	replaceRange(s, start, end, false, replaceWith)
}

// ReplaceRangeInclusive is String::replace_range for the range `start..=end`
// (`..=end` is 0, end). It panics as ReplaceRange does; here an end at or
// beyond the length is out of bounds.
func ReplaceRangeInclusive(s *string, start, end int, replaceWith string) {
	replaceRange(s, start, end, true, replaceWith)
}

func replaceRange(s *string, start, end int, inclusive bool, replaceWith string) {
	end, message := sliceRange(start, end, len(*s), inclusive)
	if message != "" {
		rs.PanicAt(2, message)
	}
	if !IsCharBoundary(*s, start) {
		rs.PanicAt(2, "start of range should be a character boundary")
	}
	if !IsCharBoundary(*s, end) {
		rs.PanicAt(2, "end of range should be a character boundary")
	}
	*s = (*s)[:start] + replaceWith + (*s)[end:]
}
