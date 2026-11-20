package rs

// Clone is `value.clone()` for a type whose Clone is derived, or written by
// hand as a Clone method. It copies as Rust does: strings, numbers and
// data-free enums as they are; slices, collections, options, boxed values
// (pointers) and enum variants deeply; a pointer or slice in a struct field
// tagged `rs:"name,arc"` is shared, as cloning an Arc shares.
//
// A value of a type with a method `Clone() T` (or, on the pointer, `Clone()
// T`) is copied by that method.
func Clone[T any](value T) T {
	return cloneValue(value)
}

// ClonePtr returns a pointer to a deep copy of what p points to. It is
// `Arc::new((*p).clone())`, the copy `Arc::make_mut` makes of a shared value
// before it is changed.
func ClonePtr[T any](p *T) *T {
	copied := cloneValue(*p)
	return &copied
}

// Eq is `a == b` for values Go's own == does not compare the way
// derive(PartialEq) does: structs with slices or collections, enum variants
// (which Go would compare by pointer), options and tuples of those. A type
// with a method `Equal(other T) bool` (or `Equal(other *T) bool`) is compared
// by it. A nil slice equals an empty one, as two empty Vecs are equal.
func Eq[T any](a, b T) bool {
	return equalValues(a, b)
}

// Cmp is `a.cmp(&b)`: negative, zero or positive. Numbers and strings compare
// as in Rust (strings by bytes); structs and tuples compare field by field in
// declaration order; enums by variant position, then by fields; options with
// None first; slices lexicographically. A type with a method `Compare(other
// T) int` (or `Compare(other *T) int`) is compared by it.
func Cmp[T any](a, b T) int {
	return compareValues(a, b)
}

// Min is `std::cmp::min` (the first argument when they are equal).
func Min[T any](a, b T) T {
	if compareValues(b, a) < 0 {
		return b
	}
	return a
}

// Max is `std::cmp::max` (the second argument when they are equal).
func Max[T any](a, b T) T {
	if compareValues(b, a) < 0 {
		return a
	}
	return b
}

// Default is `T::default()` for a type whose Default is derived: every field
// is its own default. Options are None, numbers zero, strings and
// collections empty, and a data-free enum is its `#[default]` variant, which
// the port keeps as the constant with value zero unless the type registers
// another. A type with a hand-written Default has a `<Type>Default()`
// function; call that instead, and register it with RegisterDefault so that
// a derived Default holding the type finds it (default.go has the rules).
func Default[T any]() T {
	return defaultValue[T]()
}
