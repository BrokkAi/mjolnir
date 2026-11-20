package rs

// Unit is Rust's `()` where Go needs a type: the element of a channel that
// carries only a signal, the output of a task that returns nothing.
type Unit struct{}

// Tuple2 is a Rust pair held as a value. A function returning a pair returns
// two results instead.
type Tuple2[A, B any] struct {
	F0 A
	F1 B
}

// Tuple3 is a Rust triple held as a value.
type Tuple3[A, B, C any] struct {
	F0 A
	F1 B
	F2 C
}

// Tuple4 is a Rust 4-tuple held as a value.
type Tuple4[A, B, C, D any] struct {
	F0 A
	F1 B
	F2 C
	F3 D
}

// Tuple5 is a Rust 5-tuple held as a value.
type Tuple5[A, B, C, D, E any] struct {
	F0 A
	F1 B
	F2 C
	F3 D
	F4 E
}

// T2 builds a Tuple2.
func T2[A, B any](a A, b B) Tuple2[A, B] { return Tuple2[A, B]{F0: a, F1: b} }

// T3 builds a Tuple3.
func T3[A, B, C any](a A, b B, c C) Tuple3[A, B, C] { return Tuple3[A, B, C]{F0: a, F1: b, F2: c} }

// T4 builds a Tuple4.
func T4[A, B, C, D any](a A, b B, c C, d D) Tuple4[A, B, C, D] {
	return Tuple4[A, B, C, D]{F0: a, F1: b, F2: c, F3: d}
}

// Is reports whether an enum value is the variant V. It is `matches!(v,
// Enum::Variant { .. })`:
//
//	rs.Is[*RelayEvent_Started](event)
func Is[V any](value any) bool {
	_, ok := value.(V)
	return ok
}
