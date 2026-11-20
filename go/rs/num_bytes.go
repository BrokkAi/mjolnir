// Port of core 1.99.1 src/num/uint_macros.rs, src/num/int_macros.rs,
// src/num/f32.rs and src/num/f64.rs (the byte conversions)

package rs

import (
	"encoding/binary"
	"math"
)

// The byte conversions of the number types: `v.to_le_bytes()` is
// rs.<T>ToLeBytes(v) and `T::from_le_bytes(bytes)` is rs.<T>FromLeBytes(bytes),
// with Be for big-endian and Ne for the byte order of the machine. They
// return and take arrays, as Rust's do; slice an array held in a variable
// (`bytes[:]`) to append or write it, and convert a slice of the right
// length with `[4]byte(slice)`.
//
// usize and isize are eight bytes, as on every 64-bit target of the port.
// U128 and I128 have theirs in num_wide.go.

// U8ToLeBytes is `v.to_le_bytes()` on u8.
func U8ToLeBytes(v uint8) [1]byte {
	return [1]byte{byte(v)}
}

// U8ToBeBytes is `v.to_be_bytes()` on u8.
func U8ToBeBytes(v uint8) [1]byte {
	return [1]byte{byte(v)}
}

// U8ToNeBytes is `v.to_ne_bytes()` on u8.
func U8ToNeBytes(v uint8) [1]byte {
	return [1]byte{byte(v)}
}

// U8FromLeBytes is `u8::from_le_bytes(bytes)`.
func U8FromLeBytes(bytes [1]byte) uint8 {
	return uint8(bytes[0])
}

// U8FromBeBytes is `u8::from_be_bytes(bytes)`.
func U8FromBeBytes(bytes [1]byte) uint8 {
	return uint8(bytes[0])
}

// U8FromNeBytes is `u8::from_ne_bytes(bytes)`.
func U8FromNeBytes(bytes [1]byte) uint8 {
	return uint8(bytes[0])
}

// U16ToLeBytes is `v.to_le_bytes()` on u16.
func U16ToLeBytes(v uint16) [2]byte {
	var bytes [2]byte
	binary.LittleEndian.PutUint16(bytes[:], v)
	return bytes
}

// U16ToBeBytes is `v.to_be_bytes()` on u16.
func U16ToBeBytes(v uint16) [2]byte {
	var bytes [2]byte
	binary.BigEndian.PutUint16(bytes[:], v)
	return bytes
}

// U16ToNeBytes is `v.to_ne_bytes()` on u16.
func U16ToNeBytes(v uint16) [2]byte {
	var bytes [2]byte
	binary.NativeEndian.PutUint16(bytes[:], v)
	return bytes
}

// U16FromLeBytes is `u16::from_le_bytes(bytes)`.
func U16FromLeBytes(bytes [2]byte) uint16 {
	return binary.LittleEndian.Uint16(bytes[:])
}

// U16FromBeBytes is `u16::from_be_bytes(bytes)`.
func U16FromBeBytes(bytes [2]byte) uint16 {
	return binary.BigEndian.Uint16(bytes[:])
}

// U16FromNeBytes is `u16::from_ne_bytes(bytes)`.
func U16FromNeBytes(bytes [2]byte) uint16 {
	return binary.NativeEndian.Uint16(bytes[:])
}

// U32ToLeBytes is `v.to_le_bytes()` on u32.
func U32ToLeBytes(v uint32) [4]byte {
	var bytes [4]byte
	binary.LittleEndian.PutUint32(bytes[:], v)
	return bytes
}

// U32ToBeBytes is `v.to_be_bytes()` on u32.
func U32ToBeBytes(v uint32) [4]byte {
	var bytes [4]byte
	binary.BigEndian.PutUint32(bytes[:], v)
	return bytes
}

// U32ToNeBytes is `v.to_ne_bytes()` on u32.
func U32ToNeBytes(v uint32) [4]byte {
	var bytes [4]byte
	binary.NativeEndian.PutUint32(bytes[:], v)
	return bytes
}

// U32FromLeBytes is `u32::from_le_bytes(bytes)`.
func U32FromLeBytes(bytes [4]byte) uint32 {
	return binary.LittleEndian.Uint32(bytes[:])
}

// U32FromBeBytes is `u32::from_be_bytes(bytes)`.
func U32FromBeBytes(bytes [4]byte) uint32 {
	return binary.BigEndian.Uint32(bytes[:])
}

// U32FromNeBytes is `u32::from_ne_bytes(bytes)`.
func U32FromNeBytes(bytes [4]byte) uint32 {
	return binary.NativeEndian.Uint32(bytes[:])
}

// U64ToLeBytes is `v.to_le_bytes()` on u64.
func U64ToLeBytes(v uint64) [8]byte {
	var bytes [8]byte
	binary.LittleEndian.PutUint64(bytes[:], v)
	return bytes
}

// U64ToBeBytes is `v.to_be_bytes()` on u64.
func U64ToBeBytes(v uint64) [8]byte {
	var bytes [8]byte
	binary.BigEndian.PutUint64(bytes[:], v)
	return bytes
}

// U64ToNeBytes is `v.to_ne_bytes()` on u64.
func U64ToNeBytes(v uint64) [8]byte {
	var bytes [8]byte
	binary.NativeEndian.PutUint64(bytes[:], v)
	return bytes
}

// U64FromLeBytes is `u64::from_le_bytes(bytes)`.
func U64FromLeBytes(bytes [8]byte) uint64 {
	return binary.LittleEndian.Uint64(bytes[:])
}

// U64FromBeBytes is `u64::from_be_bytes(bytes)`.
func U64FromBeBytes(bytes [8]byte) uint64 {
	return binary.BigEndian.Uint64(bytes[:])
}

// U64FromNeBytes is `u64::from_ne_bytes(bytes)`.
func U64FromNeBytes(bytes [8]byte) uint64 {
	return binary.NativeEndian.Uint64(bytes[:])
}

// UsizeToLeBytes is `v.to_le_bytes()` on usize.
func UsizeToLeBytes(v int) [8]byte {
	var bytes [8]byte
	binary.LittleEndian.PutUint64(bytes[:], uint64(v))
	return bytes
}

// UsizeToBeBytes is `v.to_be_bytes()` on usize.
func UsizeToBeBytes(v int) [8]byte {
	var bytes [8]byte
	binary.BigEndian.PutUint64(bytes[:], uint64(v))
	return bytes
}

// UsizeToNeBytes is `v.to_ne_bytes()` on usize.
func UsizeToNeBytes(v int) [8]byte {
	var bytes [8]byte
	binary.NativeEndian.PutUint64(bytes[:], uint64(v))
	return bytes
}

// UsizeFromLeBytes is `usize::from_le_bytes(bytes)`.
func UsizeFromLeBytes(bytes [8]byte) int {
	return int(binary.LittleEndian.Uint64(bytes[:]))
}

// UsizeFromBeBytes is `usize::from_be_bytes(bytes)`.
func UsizeFromBeBytes(bytes [8]byte) int {
	return int(binary.BigEndian.Uint64(bytes[:]))
}

// UsizeFromNeBytes is `usize::from_ne_bytes(bytes)`.
func UsizeFromNeBytes(bytes [8]byte) int {
	return int(binary.NativeEndian.Uint64(bytes[:]))
}

// I8ToLeBytes is `v.to_le_bytes()` on i8.
func I8ToLeBytes(v int8) [1]byte {
	return [1]byte{byte(v)}
}

// I8ToBeBytes is `v.to_be_bytes()` on i8.
func I8ToBeBytes(v int8) [1]byte {
	return [1]byte{byte(v)}
}

// I8ToNeBytes is `v.to_ne_bytes()` on i8.
func I8ToNeBytes(v int8) [1]byte {
	return [1]byte{byte(v)}
}

// I8FromLeBytes is `i8::from_le_bytes(bytes)`.
func I8FromLeBytes(bytes [1]byte) int8 {
	return int8(bytes[0])
}

// I8FromBeBytes is `i8::from_be_bytes(bytes)`.
func I8FromBeBytes(bytes [1]byte) int8 {
	return int8(bytes[0])
}

// I8FromNeBytes is `i8::from_ne_bytes(bytes)`.
func I8FromNeBytes(bytes [1]byte) int8 {
	return int8(bytes[0])
}

// I16ToLeBytes is `v.to_le_bytes()` on i16.
func I16ToLeBytes(v int16) [2]byte {
	var bytes [2]byte
	binary.LittleEndian.PutUint16(bytes[:], uint16(v))
	return bytes
}

// I16ToBeBytes is `v.to_be_bytes()` on i16.
func I16ToBeBytes(v int16) [2]byte {
	var bytes [2]byte
	binary.BigEndian.PutUint16(bytes[:], uint16(v))
	return bytes
}

// I16ToNeBytes is `v.to_ne_bytes()` on i16.
func I16ToNeBytes(v int16) [2]byte {
	var bytes [2]byte
	binary.NativeEndian.PutUint16(bytes[:], uint16(v))
	return bytes
}

// I16FromLeBytes is `i16::from_le_bytes(bytes)`.
func I16FromLeBytes(bytes [2]byte) int16 {
	return int16(binary.LittleEndian.Uint16(bytes[:]))
}

// I16FromBeBytes is `i16::from_be_bytes(bytes)`.
func I16FromBeBytes(bytes [2]byte) int16 {
	return int16(binary.BigEndian.Uint16(bytes[:]))
}

// I16FromNeBytes is `i16::from_ne_bytes(bytes)`.
func I16FromNeBytes(bytes [2]byte) int16 {
	return int16(binary.NativeEndian.Uint16(bytes[:]))
}

// I32ToLeBytes is `v.to_le_bytes()` on i32.
func I32ToLeBytes(v int32) [4]byte {
	var bytes [4]byte
	binary.LittleEndian.PutUint32(bytes[:], uint32(v))
	return bytes
}

// I32ToBeBytes is `v.to_be_bytes()` on i32.
func I32ToBeBytes(v int32) [4]byte {
	var bytes [4]byte
	binary.BigEndian.PutUint32(bytes[:], uint32(v))
	return bytes
}

// I32ToNeBytes is `v.to_ne_bytes()` on i32.
func I32ToNeBytes(v int32) [4]byte {
	var bytes [4]byte
	binary.NativeEndian.PutUint32(bytes[:], uint32(v))
	return bytes
}

// I32FromLeBytes is `i32::from_le_bytes(bytes)`.
func I32FromLeBytes(bytes [4]byte) int32 {
	return int32(binary.LittleEndian.Uint32(bytes[:]))
}

// I32FromBeBytes is `i32::from_be_bytes(bytes)`.
func I32FromBeBytes(bytes [4]byte) int32 {
	return int32(binary.BigEndian.Uint32(bytes[:]))
}

// I32FromNeBytes is `i32::from_ne_bytes(bytes)`.
func I32FromNeBytes(bytes [4]byte) int32 {
	return int32(binary.NativeEndian.Uint32(bytes[:]))
}

// I64ToLeBytes is `v.to_le_bytes()` on i64.
func I64ToLeBytes(v int64) [8]byte {
	var bytes [8]byte
	binary.LittleEndian.PutUint64(bytes[:], uint64(v))
	return bytes
}

// I64ToBeBytes is `v.to_be_bytes()` on i64.
func I64ToBeBytes(v int64) [8]byte {
	var bytes [8]byte
	binary.BigEndian.PutUint64(bytes[:], uint64(v))
	return bytes
}

// I64ToNeBytes is `v.to_ne_bytes()` on i64.
func I64ToNeBytes(v int64) [8]byte {
	var bytes [8]byte
	binary.NativeEndian.PutUint64(bytes[:], uint64(v))
	return bytes
}

// I64FromLeBytes is `i64::from_le_bytes(bytes)`.
func I64FromLeBytes(bytes [8]byte) int64 {
	return int64(binary.LittleEndian.Uint64(bytes[:]))
}

// I64FromBeBytes is `i64::from_be_bytes(bytes)`.
func I64FromBeBytes(bytes [8]byte) int64 {
	return int64(binary.BigEndian.Uint64(bytes[:]))
}

// I64FromNeBytes is `i64::from_ne_bytes(bytes)`.
func I64FromNeBytes(bytes [8]byte) int64 {
	return int64(binary.NativeEndian.Uint64(bytes[:]))
}

// IsizeToLeBytes is `v.to_le_bytes()` on isize.
func IsizeToLeBytes(v int) [8]byte {
	var bytes [8]byte
	binary.LittleEndian.PutUint64(bytes[:], uint64(v))
	return bytes
}

// IsizeToBeBytes is `v.to_be_bytes()` on isize.
func IsizeToBeBytes(v int) [8]byte {
	var bytes [8]byte
	binary.BigEndian.PutUint64(bytes[:], uint64(v))
	return bytes
}

// IsizeToNeBytes is `v.to_ne_bytes()` on isize.
func IsizeToNeBytes(v int) [8]byte {
	var bytes [8]byte
	binary.NativeEndian.PutUint64(bytes[:], uint64(v))
	return bytes
}

// IsizeFromLeBytes is `isize::from_le_bytes(bytes)`.
func IsizeFromLeBytes(bytes [8]byte) int {
	return int(binary.LittleEndian.Uint64(bytes[:]))
}

// IsizeFromBeBytes is `isize::from_be_bytes(bytes)`.
func IsizeFromBeBytes(bytes [8]byte) int {
	return int(binary.BigEndian.Uint64(bytes[:]))
}

// IsizeFromNeBytes is `isize::from_ne_bytes(bytes)`.
func IsizeFromNeBytes(bytes [8]byte) int {
	return int(binary.NativeEndian.Uint64(bytes[:]))
}

// F32ToLeBytes is `v.to_le_bytes()` on f32.
func F32ToLeBytes(v float32) [4]byte {
	var bytes [4]byte
	binary.LittleEndian.PutUint32(bytes[:], math.Float32bits(v))
	return bytes
}

// F32ToBeBytes is `v.to_be_bytes()` on f32.
func F32ToBeBytes(v float32) [4]byte {
	var bytes [4]byte
	binary.BigEndian.PutUint32(bytes[:], math.Float32bits(v))
	return bytes
}

// F32ToNeBytes is `v.to_ne_bytes()` on f32.
func F32ToNeBytes(v float32) [4]byte {
	var bytes [4]byte
	binary.NativeEndian.PutUint32(bytes[:], math.Float32bits(v))
	return bytes
}

// F32FromLeBytes is `f32::from_le_bytes(bytes)`.
func F32FromLeBytes(bytes [4]byte) float32 {
	return math.Float32frombits(binary.LittleEndian.Uint32(bytes[:]))
}

// F32FromBeBytes is `f32::from_be_bytes(bytes)`.
func F32FromBeBytes(bytes [4]byte) float32 {
	return math.Float32frombits(binary.BigEndian.Uint32(bytes[:]))
}

// F32FromNeBytes is `f32::from_ne_bytes(bytes)`.
func F32FromNeBytes(bytes [4]byte) float32 {
	return math.Float32frombits(binary.NativeEndian.Uint32(bytes[:]))
}

// F64ToLeBytes is `v.to_le_bytes()` on f64.
func F64ToLeBytes(v float64) [8]byte {
	var bytes [8]byte
	binary.LittleEndian.PutUint64(bytes[:], math.Float64bits(v))
	return bytes
}

// F64ToBeBytes is `v.to_be_bytes()` on f64.
func F64ToBeBytes(v float64) [8]byte {
	var bytes [8]byte
	binary.BigEndian.PutUint64(bytes[:], math.Float64bits(v))
	return bytes
}

// F64ToNeBytes is `v.to_ne_bytes()` on f64.
func F64ToNeBytes(v float64) [8]byte {
	var bytes [8]byte
	binary.NativeEndian.PutUint64(bytes[:], math.Float64bits(v))
	return bytes
}

// F64FromLeBytes is `f64::from_le_bytes(bytes)`.
func F64FromLeBytes(bytes [8]byte) float64 {
	return math.Float64frombits(binary.LittleEndian.Uint64(bytes[:]))
}

// F64FromBeBytes is `f64::from_be_bytes(bytes)`.
func F64FromBeBytes(bytes [8]byte) float64 {
	return math.Float64frombits(binary.BigEndian.Uint64(bytes[:]))
}

// F64FromNeBytes is `f64::from_ne_bytes(bytes)`.
func F64FromNeBytes(bytes [8]byte) float64 {
	return math.Float64frombits(binary.NativeEndian.Uint64(bytes[:]))
}
