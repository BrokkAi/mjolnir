// Port of core 1.99.1 src/str/error.rs

package strs

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Utf8Error is str::Utf8Error: why a byte string is not UTF-8. It is the
// error of FromUtf8, and is a value, as the Rust type is `Copy`.
type Utf8Error struct {
	validUpTo int
	errorLen  rs.Option[uint8]
}

// ValidUpTo is Utf8Error::valid_up_to: the index in the given bytes up to
// which valid UTF-8 was verified.
func (e Utf8Error) ValidUpTo() int { return e.validUpTo }

// ErrorLen is Utf8Error::error_len: None when the input ended in the middle
// of a char, otherwise the length (1 to 3) of the invalid sequence that
// starts at ValidUpTo.
func (e Utf8Error) ErrorLen() rs.Option[int] {
	if length, ok := e.errorLen.Get(); ok {
		return rs.Some(int(length))
	}
	return rs.None[int]()
}

// String is the error's Display text.
func (e Utf8Error) String() string {
	if errorLen, ok := e.errorLen.Get(); ok {
		return "invalid utf-8 sequence of " + strconv.Itoa(int(errorLen)) + " bytes from index " + strconv.Itoa(e.validUpTo)
	}
	return "incomplete utf-8 byte sequence from index " + strconv.Itoa(e.validUpTo)
}

// Error makes a Utf8Error a Go error.
func (e Utf8Error) Error() string { return e.String() }

// FmtDisplay is `impl Display for Utf8Error`.
func (e Utf8Error) FmtDisplay(f *rs.Formatter) error { return f.Pad(e.String()) }

// FmtDebug is the derived `Debug`:
// `Utf8Error { valid_up_to: 1, error_len: Some(1) }`.
func (e Utf8Error) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Utf8Error").Field("valid_up_to", e.validUpTo).Field("error_len", e.errorLen).Finish()
}

func (e Utf8Error) debugString() string {
	errorLen := "None"
	if length, ok := e.errorLen.Get(); ok {
		errorLen = "Some(" + strconv.Itoa(int(length)) + ")"
	}
	return "Utf8Error { valid_up_to: " + strconv.Itoa(e.validUpTo) + ", error_len: " + errorLen + " }"
}

// ParseBoolError is str::ParseBoolError: what parsing a string that is
// neither `true` nor `false` as a bool returns.
type ParseBoolError struct{}

// String is the error's Display text.
func (e *ParseBoolError) String() string { return "provided string was not `true` or `false`" }

// Error makes a ParseBoolError a Go error.
func (e *ParseBoolError) Error() string { return e.String() }

// FmtDisplay is `impl Display for ParseBoolError`.
func (e *ParseBoolError) FmtDisplay(f *rs.Formatter) error { return f.Pad(e.String()) }

// FmtDebug is the derived `Debug`: `ParseBoolError`.
func (e *ParseBoolError) FmtDebug(f *rs.Formatter) error { return f.WriteStr(e.debugString()) }

func (e *ParseBoolError) debugString() string { return "ParseBoolError" }
