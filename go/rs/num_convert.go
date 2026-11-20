// Port of core 1.99.1 src/convert/num.rs and src/num/error.rs

package rs

import "math"

// Conversions between numbers.
//
//   - `T::try_from(v)` between integer types is rs.<T>TryFrom(v):
//     rs.U16TryFrom(width), rs.UsizeTryFrom(count). An int source is read as
//     the signed integer it is, which is right for a usize too, since a usize
//     of this port is never negative.
//   - `x as T` from a float to an integer is rs.CastF64To<T>(x) or
//     rs.CastF32To<T>(x). Rust saturates at the integer's bounds and maps NaN
//     to zero; Go's conversion is implementation-defined outside the range
//     (and differs between amd64 and arm64).
//   - `x as f64` and `x as f32` from an integer, `x as f32` from an f64 and
//     `x as f64` from an f32 are Go's conversions: both round to nearest,
//     ties to even, and an f64 too large for f32 becomes an infinity in both.
//     The oracle tests pin this.
//   - `x as T` between integer types is Go's conversion.

// IntErrorKind is core::num::IntErrorKind: why an integer could not be
// parsed or converted.
type IntErrorKind uint8

const (
	// IntErrorKind_Empty is the value being parsed is empty.
	IntErrorKind_Empty IntErrorKind = iota
	// IntErrorKind_InvalidDigit is a character that is not a digit of the
	// radix, including a sign on its own.
	IntErrorKind_InvalidDigit
	// IntErrorKind_PosOverflow is an integer too large for the target type.
	IntErrorKind_PosOverflow
	// IntErrorKind_NegOverflow is an integer too small for the target type.
	IntErrorKind_NegOverflow
	// IntErrorKind_Zero is zero where the target type cannot be zero.
	IntErrorKind_Zero
	// IntErrorKind_NotAPowerOfTwo is a value that had to be a power of two.
	IntErrorKind_NotAPowerOfTwo
)

var _ = RegisterEnum[IntErrorKind]("IntErrorKind",
	Const(IntErrorKind_Empty, "Empty"),
	Const(IntErrorKind_InvalidDigit, "InvalidDigit"),
	Const(IntErrorKind_PosOverflow, "PosOverflow"),
	Const(IntErrorKind_NegOverflow, "NegOverflow"),
	Const(IntErrorKind_Zero, "Zero"),
	Const(IntErrorKind_NotAPowerOfTwo, "NotAPowerOfTwo"),
)

// String is the variant's name, which is its derived Debug form.
func (k IntErrorKind) String() string {
	switch k {
	case IntErrorKind_Empty:
		return "Empty"
	case IntErrorKind_InvalidDigit:
		return "InvalidDigit"
	case IntErrorKind_PosOverflow:
		return "PosOverflow"
	case IntErrorKind_NegOverflow:
		return "NegOverflow"
	case IntErrorKind_Zero:
		return "Zero"
	case IntErrorKind_NotAPowerOfTwo:
		return "NotAPowerOfTwo"
	default:
		panic(Unreachable(uint8(k)))
	}
}

// FmtDebug is the derived Debug of a data-free enum: the variant's name.
func (k IntErrorKind) FmtDebug(f *Formatter) error {
	return f.WriteStr(k.String())
}

// TryFromIntError is core::num::TryFromIntError: the error of a checked
// conversion between integer types.
type TryFromIntError struct {
	kind IntErrorKind
}

// Kind is TryFromIntError::kind.
func (e TryFromIntError) Kind() IntErrorKind { return e.kind }

// String is the error's Display text.
func (e TryFromIntError) String() string {
	switch e.kind {
	case IntErrorKind_PosOverflow:
		return "number too large to fit in target type"
	case IntErrorKind_NegOverflow:
		return "number too small to fit in target type"
	case IntErrorKind_Zero:
		return "number would be zero for non-zero type"
	case IntErrorKind_NotAPowerOfTwo:
		return "number is not a power of two"
	default:
		panic(Unreachable())
	}
}

// Error makes TryFromIntError a Go error.
func (e TryFromIntError) Error() string { return e.String() }

// FmtDisplay is `impl Display for TryFromIntError`.
func (e TryFromIntError) FmtDisplay(f *Formatter) error { return f.Pad(e.String()) }

// FmtDebug is the derived Debug: `TryFromIntError(PosOverflow)`.
func (e TryFromIntError) FmtDebug(f *Formatter) error {
	return f.DebugTuple("TryFromIntError").Field(e.kind).Finish()
}

// tryFromInt is `T::try_from(v)`. An int is read as a signed integer on both
// sides.
func tryFromInt[T, S anyInt](v S) (T, error) {
	if intIsSigned[S]() && v < 0 {
		if !intIsSigned[T]() || int64(v) < int64(intMin[T]()) {
			return 0, TryFromIntError{kind: IntErrorKind_NegOverflow}
		}
		return T(v), nil
	}
	if uint64(v) > uint64(intMax[T]()) {
		return 0, TryFromIntError{kind: IntErrorKind_PosOverflow}
	}
	return T(v), nil
}

// U8TryFrom is `u8::try_from(v)`.
func U8TryFrom[S anyInt](v S) (uint8, error) { return tryFromInt[uint8](v) }

// U16TryFrom is `u16::try_from(v)`.
func U16TryFrom[S anyInt](v S) (uint16, error) { return tryFromInt[uint16](v) }

// U32TryFrom is `u32::try_from(v)`.
func U32TryFrom[S anyInt](v S) (uint32, error) { return tryFromInt[uint32](v) }

// U64TryFrom is `u64::try_from(v)`.
func U64TryFrom[S anyInt](v S) (uint64, error) { return tryFromInt[uint64](v) }

// I8TryFrom is `i8::try_from(v)`.
func I8TryFrom[S anyInt](v S) (int8, error) { return tryFromInt[int8](v) }

// I16TryFrom is `i16::try_from(v)`.
func I16TryFrom[S anyInt](v S) (int16, error) { return tryFromInt[int16](v) }

// I32TryFrom is `i32::try_from(v)`.
func I32TryFrom[S anyInt](v S) (int32, error) { return tryFromInt[int32](v) }

// I64TryFrom is `i64::try_from(v)`.
func I64TryFrom[S anyInt](v S) (int64, error) { return tryFromInt[int64](v) }

// IsizeTryFrom is `isize::try_from(v)`.
func IsizeTryFrom[S anyInt](v S) (int, error) { return tryFromInt[int](v) }

// UsizeTryFrom is `usize::try_from(v)`: an error for a negative value, and
// for one above UsizeMax (which Rust's 64-bit usize would hold, and which no
// length or index reaches).
func UsizeTryFrom[S anyInt](v S) (int, error) {
	if intIsSigned[S]() && v < 0 {
		return 0, TryFromIntError{kind: IntErrorKind_NegOverflow}
	}
	if uint64(v) > uint64(UsizeMax) {
		return 0, TryFromIntError{kind: IntErrorKind_PosOverflow}
	}
	return int(v), nil
}

// castFloatToInt is `x as T` for a float x: NaN is zero, a value outside T's
// range is the nearer bound, and anything else is truncated toward zero.
func castFloatToInt[T anyInt](x float64) T {
	if x != x {
		return 0
	}
	if intIsSigned[T]() {
		// 2^(BITS-1): one past MAX, and the magnitude of MIN.
		limit := math.Ldexp(1, int(intBits[T]())-1)
		if x >= limit {
			return intMax[T]()
		}
		if x <= -limit {
			return intMin[T]()
		}
		return T(int64(x))
	}
	// 2^BITS: one past MAX.
	limit := math.Ldexp(1, int(intBits[T]()))
	if x >= limit {
		return ^T(0)
	}
	if x <= 0 {
		return 0
	}
	return T(uint64(x))
}

// CastF64ToI8 is `x as i8` for an f64.
func CastF64ToI8(x float64) int8 { return castFloatToInt[int8](x) }

// CastF64ToI16 is `x as i16` for an f64.
func CastF64ToI16(x float64) int16 { return castFloatToInt[int16](x) }

// CastF64ToI32 is `x as i32` for an f64.
func CastF64ToI32(x float64) int32 { return castFloatToInt[int32](x) }

// CastF64ToI64 is `x as i64` for an f64.
func CastF64ToI64(x float64) int64 { return castFloatToInt[int64](x) }

// CastF64ToIsize is `x as isize` for an f64.
func CastF64ToIsize(x float64) int { return castFloatToInt[int](x) }

// CastF64ToU8 is `x as u8` for an f64.
func CastF64ToU8(x float64) uint8 { return castFloatToInt[uint8](x) }

// CastF64ToU16 is `x as u16` for an f64.
func CastF64ToU16(x float64) uint16 { return castFloatToInt[uint16](x) }

// CastF64ToU32 is `x as u32` for an f64.
func CastF64ToU32(x float64) uint32 { return castFloatToInt[uint32](x) }

// CastF64ToU64 is `x as u64` for an f64.
func CastF64ToU64(x float64) uint64 { return castFloatToInt[uint64](x) }

// CastF64ToUsize is `x as usize` for an f64. It saturates at UsizeMax, where
// Rust's 64-bit usize goes on to twice that.
func CastF64ToUsize(x float64) int {
	if x != x || x <= 0 {
		return 0
	}
	if x >= math.Ldexp(1, int(intBits[int]())-1) {
		return UsizeMax
	}
	return int(x)
}

// CastF32ToI8 is `x as i8` for an f32.
func CastF32ToI8(x float32) int8 { return castFloatToInt[int8](float64(x)) }

// CastF32ToI16 is `x as i16` for an f32.
func CastF32ToI16(x float32) int16 { return castFloatToInt[int16](float64(x)) }

// CastF32ToI32 is `x as i32` for an f32.
func CastF32ToI32(x float32) int32 { return castFloatToInt[int32](float64(x)) }

// CastF32ToI64 is `x as i64` for an f32.
func CastF32ToI64(x float32) int64 { return castFloatToInt[int64](float64(x)) }

// CastF32ToIsize is `x as isize` for an f32.
func CastF32ToIsize(x float32) int { return castFloatToInt[int](float64(x)) }

// CastF32ToU8 is `x as u8` for an f32.
func CastF32ToU8(x float32) uint8 { return castFloatToInt[uint8](float64(x)) }

// CastF32ToU16 is `x as u16` for an f32.
func CastF32ToU16(x float32) uint16 { return castFloatToInt[uint16](float64(x)) }

// CastF32ToU32 is `x as u32` for an f32.
func CastF32ToU32(x float32) uint32 { return castFloatToInt[uint32](float64(x)) }

// CastF32ToU64 is `x as u64` for an f32.
func CastF32ToU64(x float32) uint64 { return castFloatToInt[uint64](float64(x)) }

// CastF32ToUsize is `x as usize` for an f32. It saturates at UsizeMax.
func CastF32ToUsize(x float32) int { return CastF64ToUsize(float64(x)) }
