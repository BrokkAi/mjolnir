// Port of core 1.99.1 src/mem/mod.rs (take, replace, swap, forget,
// discriminant)

package rs

import (
	"reflect"
	"unsafe"
)

// Take is `mem::take(dest)`: it returns the value and leaves T's Default in
// its place.
func Take[T any](dest *T) T {
	fresh := defaultValue[T]()
	taken := *dest
	*dest = fresh
	return taken
}

// Replace is `mem::replace(dest, value)`: it stores value and returns what
// was there. Neither is dropped.
func Replace[T any](dest *T, value T) T {
	old := *dest
	*dest = value
	return old
}

// Swap is `mem::swap(a, b)`.
func Swap[T any](a, b *T) {
	*a, *b = *b, *a
}

// Forgetter is implemented by a type that must be dropped, to say what
// `mem::forget` does to it: the value is given up, its Drop never runs, and
// no safety net runs it later either.
type Forgetter interface {
	Forget()
}

// Forget is `mem::forget(value)`. For most values it does nothing, since Go
// only drops what the code drops; a Forgetter (an Arc, a Weak) is told, so
// that it does not drop itself later.
func Forget[T any](value T) {
	if forgetter, ok := any(value).(Forgetter); ok {
		forgetter.Forget()
		return
	}
	if forgetter, ok := any(&value).(Forgetter); ok {
		forgetter.Forget()
	}
}

// MemDiscriminant is `mem::Discriminant<T>`: which variant of the enum T a
// value is, whatever the variant holds. Two of them compare with ==.
type MemDiscriminant[T any] struct {
	// index is the discriminant: the value of a data-free enum, the position
	// of a registered variant, 0 and 1 for None and Some or Ok and Err.
	index int64
	// variant is the Go type of the variant for an enum held as an
	// interface, so that variants are told apart even when unregistered.
	variant reflect.Type
}

// Discriminant is `mem::discriminant(&value)`. Like Rust's, the result means
// nothing for a type that is not an enum.
func Discriminant[T any](value T) MemDiscriminant[T] {
	s := shapeOf(reflect.TypeFor[T]())
	at := unsafe.Pointer(&value)
	switch s.kind {
	case kindInt:
		return MemDiscriminant[T]{index: reflect.NewAt(s.typ, at).Elem().Int()}
	case kindUint:
		return MemDiscriminant[T]{index: int64(reflect.NewAt(s.typ, at).Elem().Uint())}
	case kindOption:
		if *(*bool)(unsafe.Add(at, s.fields[1].offset)) {
			return MemDiscriminant[T]{index: 1}
		}
	case kindResult:
		if *(*error)(unsafe.Add(at, s.fields[1].offset)) != nil {
			return MemDiscriminant[T]{index: 1}
		}
	case kindInterface:
		held := reflect.NewAt(s.typ, at).Elem()
		if held.IsNil() {
			return MemDiscriminant[T]{index: -1}
		}
		variant := held.Elem().Type()
		if info := heldVariantInfo(variant); info != nil {
			return MemDiscriminant[T]{index: int64(info.Index), variant: variant}
		}
		return MemDiscriminant[T]{index: -1, variant: variant}
	}
	return MemDiscriminant[T]{}
}

// Equal is Discriminant's PartialEq.
func (d MemDiscriminant[T]) Equal(other MemDiscriminant[T]) bool {
	return d == other
}

// FmtDebug is Discriminant's Debug: `Discriminant(1)`.
func (d MemDiscriminant[T]) FmtDebug(f *Formatter) error {
	return f.DebugTuple("Discriminant").Field(d.index).Finish()
}
