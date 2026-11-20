package vt100

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The functions in this file stand in for operations of Rust's standard
// library whose failure is part of what vt100 does: the crate indexes and
// unwraps on the assumption that the cursor and the cells are consistent,
// and a byte stream (or a resize) that breaks the assumption makes it panic.
// The port panics at the same point with the same message. Each function
// names the Rust operation it reproduces.
//
// Integer arithmetic needs no helper: vt100 computes positions in u16, and
// Mjolnir is built in the release profile, where u16 arithmetic wraps as
// Go's uint16 does. (In the development profile the same overflow panics,
// which the port does not reproduce.)

// vecIndex is `&v[index]` and `&mut v[index]`. The index is a usize in Rust,
// which is why a subtraction that went below zero arrives here as a very
// large number and is reported as one.
func vecIndex[T any](v []T, index uint) *T {
	if index >= uint(len(v)) {
		rs.PanicAt(1, "index out of bounds: the len is "+strconv.Itoa(len(v))+" but the index is "+strconv.FormatUint(uint64(index), 10))
	}
	return &v[index]
}

// vecGet is `v.get(index)` and `v.get_mut(index)`.
func vecGet[T any](v []T, index uint) rs.Option[*T] {
	if index >= uint(len(v)) {
		return rs.None[*T]()
	}
	return rs.Some(&v[index])
}

// vecInsert is `Vec::insert`.
func vecInsert[T any](v *[]T, index uint, element T) {
	length := uint(len(*v))
	if index > length {
		rs.PanicAt(1, "insertion index (is "+strconv.FormatUint(uint64(index), 10)+") should be <= len (is "+strconv.Itoa(len(*v))+")")
	}
	var zero T
	*v = append(*v, zero)
	copy((*v)[index+1:], (*v)[index:length])
	(*v)[index] = element
}

// vecRemove is `Vec::remove`.
func vecRemove[T any](v *[]T, index uint) T {
	length := uint(len(*v))
	if index >= length {
		rs.PanicAt(1, "removal index (is "+strconv.FormatUint(uint64(index), 10)+") should be < len (is "+strconv.Itoa(len(*v))+")")
	}
	removed := (*v)[index]
	copy((*v)[index:], (*v)[index+1:])
	var zero T
	(*v)[length-1] = zero
	*v = (*v)[:length-1]
	return removed
}

// saturatingAddU16 is `u16::saturating_add`.
func saturatingAddU16(a, b uint16) uint16 {
	sum := a + b
	if sum < a {
		return 0xFFFF
	}
	return sum
}

// saturatingSubU16 is `u16::saturating_sub`.
func saturatingSubU16(a, b uint16) uint16 {
	if b > a {
		return 0
	}
	return a - b
}

// boolU16 is `u16::from(bool)`.
func boolU16(value bool) uint16 {
	if value {
		return 1
	}
	return 0
}
