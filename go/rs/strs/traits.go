// Port of core 1.99.1 src/str/traits.rs

package strs

import "github.com/BrokkAi/mjolnir/go/rs"

// Slicing a Rust string checks that each end of the range is on a char
// boundary and panics when it is not; a Go slice expression cuts anywhere and
// can leave half a char. `&s[a..b]` on a string is therefore strs.Slice, and
// `s.get(a..b)` is strs.Get.
//
// An index is a `usize` held in a Go int. A negative int stands for the usize
// that wraps to it, which is always out of range, and is printed that way.

// checkRange is check_range: whether start..end is a range of whole chars of
// s.
func checkRange(s string, start, end int) bool {
	if uint(start) > uint(end) || uint(end) > uint(len(s)) {
		return false
	}

	if start == len(s) {
		// If `start == len(s)`, then `end == len(s)` must also be true.
		return true
	}

	return (start == 0 || isUtf8CharBoundary(s[start])) && (end == len(s) || isUtf8CharBoundary(s[end]))
}

// Slice is `&s[start..end]`: it panics, with Rust's message, when the range
// is out of bounds, backwards, or cuts a char.
func Slice(s string, start, end int) string {
	if !checkRange(s, start, end) {
		rs.PanicAt(1, sliceErrorFail(s, start, end))
	}
	return s[start:end]
}

// SliceTo is `&s[..end]`.
func SliceTo(s string, end int) string {
	if !IsCharBoundary(s, end) {
		rs.PanicAt(1, sliceErrorFail(s, 0, end))
	}
	return s[:end]
}

// SliceFrom is `&s[start..]`.
func SliceFrom(s string, start int) string {
	if !IsCharBoundary(s, start) {
		rs.PanicAt(1, sliceErrorFail(s, start, len(s)))
	}
	return s[start:]
}

// SliceInclusive is `&s[start..=end]`.
func SliceInclusive(s string, start, end int) string {
	if uint(end) < uint(len(s)) {
		end++
		if start <= end && IsCharBoundary(s, start) && IsCharBoundary(s, end) {
			return s[start:end]
		}
	}
	rs.PanicAt(1, sliceErrorFail(s, start, end))
	panic("unreachable")
}

// SliceToInclusive is `&s[..=end]`.
func SliceToInclusive(s string, end int) string {
	if uint(end) < uint(len(s)) {
		end++
		if IsCharBoundary(s, end) {
			return s[:end]
		}
	}
	rs.PanicAt(1, sliceErrorFail(s, 0, end))
	panic("unreachable")
}

// Get is `s.get(start..end)`: the slice, or None where Slice would panic.
func Get(s string, start, end int) rs.Option[string] {
	if checkRange(s, start, end) {
		return rs.Some(s[start:end])
	}
	return rs.None[string]()
}

// GetTo is `s.get(..end)`.
func GetTo(s string, end int) rs.Option[string] {
	if IsCharBoundary(s, end) {
		return rs.Some(s[:end])
	}
	return rs.None[string]()
}

// GetFrom is `s.get(start..)`.
func GetFrom(s string, start int) rs.Option[string] {
	if IsCharBoundary(s, start) {
		return rs.Some(s[start:])
	}
	return rs.None[string]()
}

// GetInclusive is `s.get(start..=end)`.
func GetInclusive(s string, start, end int) rs.Option[string] {
	if uint(end) >= uint(len(s)) {
		return rs.None[string]()
	}
	return Get(s, start, end+1)
}

// GetToInclusive is `s.get(..=end)`.
func GetToInclusive(s string, end int) rs.Option[string] {
	return GetInclusive(s, 0, end)
}

// ParseBool is `s.parse::<bool>()` and bool::from_str: exactly `true` or
// `false`.
func ParseBool(s string) (bool, error) {
	switch s {
	case "true":
		return true, nil
	case "false":
		return false, nil
	default:
		return false, &ParseBoolError{}
	}
}
