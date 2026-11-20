// Port of qrcode 0.14.1 src/cast.rs

package qrcode

import "github.com/BrokkAi/mjolnir/go/rs"

// debugAssertions is `cfg!(debug_assertions)` for this package. It decides
// what the As conversions, the debug assertions and the u16 arithmetic of
// the crate do. It is a variable only so that the comparison with the Rust
// oracle, which is a release build, can switch it off.
var debugAssertions = rs.DebugAssertions

// tryFromIntPanic is what `T::try_from(self).unwrap()` panics with when the
// value does not fit.
const tryFromIntPanic = "called `Result::unwrap()` on an `Err` value: TryFromIntError(())"

// truncateAsU8 is Truncate::truncate_as_u8 for u16.
func truncateAsU8(v uint16) uint8 {
	return uint8(v & 0xff)
}

// The As trait. With debug assertions each conversion is
// `T::try_from(self).unwrap()`; without, it is `self as T`. A usize or isize
// is a Go int.

// asUsizeI16 is `<i16 as As>::as_usize`. Without debug assertions a negative
// value stays negative: the Go int stands for the usize Rust wraps it to.
func asUsizeI16(v int16) int {
	if debugAssertions && v < 0 {
		rs.PanicAt(1, tryFromIntPanic)
	}
	return int(v)
}

// asUsizeIsize is `<isize as As>::as_usize`.
func asUsizeIsize(v int) int {
	if debugAssertions && v < 0 {
		rs.PanicAt(1, tryFromIntPanic)
	}
	return v
}

// asUsizeU32 is `<u32 as As>::as_usize`.
func asUsizeU32(v uint32) int {
	return int(v)
}

// asU16Usize is `<usize as As>::as_u16`.
func asU16Usize(v int) uint16 {
	if debugAssertions && (v < 0 || v > 0xffff) {
		rs.PanicAt(1, tryFromIntPanic)
	}
	return uint16(v)
}

// asU16U32 is `<u32 as As>::as_u16`.
func asU16U32(v uint32) uint16 {
	if debugAssertions && v > 0xffff {
		rs.PanicAt(1, tryFromIntPanic)
	}
	return uint16(v)
}

// asI16Usize is `<usize as As>::as_i16`.
func asI16Usize(v int) int16 {
	if debugAssertions && (v < 0 || v > 0x7fff) {
		rs.PanicAt(1, tryFromIntPanic)
	}
	return int16(v)
}

// asU32Usize is `<usize as As>::as_u32`.
func asU32Usize(v int) uint32 {
	if debugAssertions && (v < 0 || uint64(v) > 0xffffffff) {
		rs.PanicAt(1, tryFromIntPanic)
	}
	return uint32(v)
}

// asIsizeUsize is `<usize as As>::as_isize`.
func asIsizeUsize(v int) int {
	if debugAssertions && v < 0 {
		rs.PanicAt(1, tryFromIntPanic)
	}
	return v
}

// asIsizeU32 is `<u32 as As>::as_isize`.
func asIsizeU32(v uint32) int {
	return int(v)
}

// The crate's arithmetic on u16 and u8 panics on overflow in a build with
// debug assertions and wraps without them, as Rust's does.

const (
	panicAddOverflow = "attempt to add with overflow"
	panicSubOverflow = "attempt to subtract with overflow"
)

// addU16 is `a + b` on u16.
func addU16(a uint16, b uint16) uint16 {
	sum := a + b
	if debugAssertions && sum < a {
		rs.PanicAt(1, panicAddOverflow)
	}
	return sum
}

// subU16 is `a - b` on u16.
func subU16(a uint16, b uint16) uint16 {
	if debugAssertions && b > a {
		rs.PanicAt(1, panicSubOverflow)
	}
	return a - b
}

// subU8 is `a - b` on u8.
func subU8(a uint8, b uint8) uint8 {
	if debugAssertions && b > a {
		rs.PanicAt(1, panicSubOverflow)
	}
	return a - b
}
