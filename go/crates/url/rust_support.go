package url

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// This file holds the few pieces of Rust's core library the port needs that
// go/rs did not have when it was written: string slicing and indexing with
// Rust's panic messages, the String editing methods, and the unit error.

func itoa(i int) string { return strconv.Itoa(i) }

// debugAssertions is `cfg!(debug_assertions)` for this package. It is a
// variable only so that the comparison with the Rust oracle, which is a
// release build, can switch the assertions off.
var debugAssertions = rs.DebugAssertions

// debugAssert is `debug_assert!(expr)`: expr is the Rust source text of the
// condition, which is what the panic message quotes.
func debugAssert(cond bool, expr string) {
	if debugAssertions && !cond {
		rs.PanicAt(1, "assertion failed: "+expr)
	}
}

// debugAssertMessage is `debug_assert!(expr, "message")`.
func debugAssertMessage(cond bool, message string) {
	if debugAssertions && !cond {
		rs.PanicAt(1, message)
	}
}

// rsAssert is `assert!(expr)` with its whole message.
func rsAssert(cond bool, message string) {
	if !cond {
		rs.PanicAt(1, message)
	}
}

// unitError is the `()` in a Rust `Result<T, ()>`: an error that says
// nothing. Its text is what `{:?}` prints for the unit value, which is what
// `unwrap()` on such a result shows.
type unitError struct{}

func (unitError) Error() string { return "()" }

// FmtDebug is `impl Debug for ()`.
func (unitError) FmtDebug(f *rs.Formatter) error { return f.Pad("()") }

// errUnit is the value every `Err(())` of this crate returns.
var errUnit error = unitError{}

// strSlice is `&s[begin..end]` with the panics of core::str's
// slice_error_fail for a range that is out of bounds or backwards.
func strSlice(s string, begin int, end int) string {
	if begin > end || end > len(s) {
		strSliceErrorFail(s, begin, end)
	}
	return s[begin:end]
}

func strSliceErrorFail(s string, begin int, end int) {
	length := len(s)

	// 1. begin is OOB.
	if begin > length {
		rs.PanicAt(2, "start byte index "+itoa(begin)+" is out of bounds for string of length "+itoa(length))
	}

	// 2. end is OOB.
	if end > length {
		rs.PanicAt(2, "end byte index "+itoa(end)+" is out of bounds for string of length "+itoa(length))
	}

	// 3. range is backwards.
	rs.PanicAt(2, "byte range starts at "+itoa(begin)+" but ends at "+itoa(end))
}

// bytesSlice is strSlice for the parser's buffer.
func bytesSlice(b []byte, begin int, end int) []byte {
	if begin > end || end > len(b) {
		strSliceErrorFail(string(b), begin, end)
	}
	return b[begin:end]
}

// drain is `string.drain(start..end)` with the removed text discarded.
func drain(b []byte, start int, end int) []byte {
	return append(b[:start], b[end:]...)
}

// insertStr is String::insert_str.
func insertStr(b []byte, index int, s string) []byte {
	out := make([]byte, 0, len(b)+len(s))
	out = append(out, b[:index]...)
	out = append(out, s...)
	out = append(out, b[index:]...)
	return out
}

// replaceRange is String::replace_range.
func replaceRange(b []byte, start int, end int, with string) []byte {
	out := make([]byte, 0, len(b)-(end-start)+len(with))
	out = append(out, b[:start]...)
	out = append(out, with...)
	out = append(out, b[end:]...)
	return out
}
