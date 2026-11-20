// Port of core 1.99.1 src/default.rs (Default, as #[derive(Default)]
// generates it, done by reflection)

package rs

import (
	"reflect"
	"sync"
	"sync/atomic"
	"unsafe"
)

// How a default value is built.
//
// The Go zero value is already the derived Default of most Rust types: zero,
// false, the empty string, None, an empty Vec or collection, and structs,
// tuples and arrays of those. Default starts from it and changes what
// differs:
//
//   - A type with a registered Default (RegisterDefault) or a
//     ReflectSetDefault method gets that value, wherever it is nested.
//   - In the port of a Rust struct (a registered struct, or a field with an
//     `rs` tag) a pointer is a Box<T> or an Arc<T>, whose Default is a new
//     allocation holding T's Default, and a Go map is an empty map. Neither
//     is ever nil in Rust.
//   - The private state of other types (a collection, a lock, a type of the
//     Go standard library) is left as its zero value says.
//
// An enum that is held as an interface has no usable zero value, so its
// Default must be registered; asking for one that is not is a missing
// registration and panics.

// ReflectDefault is implemented, on the pointer, by a type that builds its
// own Default but cannot register it because it is generic: RegisterDefault
// names one instantiation. Default calls the method on a zero value.
//
//	func (s *SharedSnapshot[T]) ReflectSetDefault() { *s = SharedSnapshotDefault[T]() }
type ReflectDefault interface {
	// ReflectSetDefault stores `Default::default()` in the receiver.
	ReflectSetDefault()
}

type defaultEntry struct {
	// function is the registered func() T.
	function any
	// store writes the registered default to memory of type T.
	store func(dst unsafe.Pointer)
}

var (
	// defaults maps a reflect.Type to its *defaultEntry.
	defaults sync.Map
	// defaultGeneration counts registrations: what was found out about a
	// type before a registration is found out again after it.
	defaultGeneration atomic.Uint64
	// defaultZero maps a reflect.Type to the generation in which its Default
	// was found to be the Go zero value.
	defaultZero sync.Map
	// defaultHooks maps a reflect.Type to whether its pointer implements
	// ReflectDefault.
	defaultHooks       sync.Map
	reflectDefaultType = reflect.TypeFor[ReflectDefault]()
)

// RegisterDefault records that `T::default()` is f(), for a type whose
// Default is not the Go zero value: an enum held as an interface, an enum
// whose `#[default]` variant is not the constant zero, a hand-written `impl
// Default`. Default[T], Take and every derived Default that holds a T then
// use it.
//
//	var _ = rs.RegisterDefault(ReviewTriggerDefault)
func RegisterDefault[T any](f func() T) bool {
	t := reflect.TypeFor[T]()
	entry := &defaultEntry{function: f, store: func(dst unsafe.Pointer) { *(*T)(dst) = f() }}
	if _, registered := defaults.LoadOrStore(t, entry); registered {
		panic("rs: a Default is already registered for " + t.String())
	}
	defaultGeneration.Add(1)
	return true
}

// defaultValue is the body of Default.
func defaultValue[T any]() T {
	t := reflect.TypeFor[T]()
	if entry, ok := defaults.Load(t); ok {
		return entry.(*defaultEntry).function.(func() T)()
	}
	var value T
	generation := defaultGeneration.Load()
	if found, ok := defaultZero.Load(t); ok && found.(uint64) == generation {
		return value
	}
	builder := defaultBuilder{}
	if !builder.apply(shapeOf(t), unsafe.Pointer(&value), true) {
		defaultZero.Store(t, generation)
	}
	return value
}

type defaultBuilder struct {
	// boxes are the pointee types being allocated, outermost first.
	boxes []reflect.Type
}

// apply turns the zero value at dst into the Default of the shape's type and
// reports whether it wrote anything. rust says that dst belongs to the port
// of a Rust type, where a pointer and a map are never nil.
func (b *defaultBuilder) apply(s *shape, dst unsafe.Pointer, rust bool) bool {
	if entry, ok := defaults.Load(s.typ); ok {
		entry.(*defaultEntry).store(dst)
		return true
	}
	if hasDefaultHook(s.typ) {
		reflect.NewAt(s.typ, dst).Interface().(ReflectDefault).ReflectSetDefault()
		return true
	}
	switch s.kind {
	case kindArray:
		changed := false
		for i := range s.typ.Len() {
			if b.apply(s.elem, unsafe.Add(dst, uintptr(i)*s.elem.size), rust) {
				changed = true
			}
		}
		return changed
	case kindPointer:
		if !rust {
			return false
		}
		for _, outer := range b.boxes {
			if outer == s.elem.typ {
				panic("rs.Default: " + s.elem.typ.String() + " holds itself through a pointer, so it has no derived Default")
			}
		}
		b.boxes = append(b.boxes, s.elem.typ)
		box := reflect.New(s.elem.typ).UnsafePointer()
		b.apply(s.elem, box, true)
		b.boxes = b.boxes[:len(b.boxes)-1]
		*(*unsafe.Pointer)(dst) = box
		return true
	case kindGoMap:
		if !rust {
			return false
		}
		reflect.NewAt(s.typ, dst).Elem().Set(reflect.MakeMap(s.typ))
		return true
	case kindInterface:
		if rust && (s.sealed || isRegisteredEnum(s.typ)) {
			panic("rs.Default: the enum " + s.typ.String() + " has no Default registered (rs.RegisterDefault)")
		}
		return false
	case kindStruct:
		if s.stdlib {
			return false
		}
		ported := TypeInfoOf(s.typ) != nil
		changed := false
		for _, field := range s.fields {
			fieldRust := ported || field.tagged || (s.container && rust)
			if b.apply(field.shape, unsafe.Add(dst, field.offset), fieldRust) {
				changed = true
			}
		}
		return changed
	}
	// Numbers, strings, slices, options and collections: the zero value.
	return false
}

func hasDefaultHook(t reflect.Type) bool {
	if found, ok := defaultHooks.Load(t); ok {
		return found.(bool)
	}
	has := t.Kind() != reflect.Interface && reflect.PointerTo(t).Implements(reflectDefaultType)
	defaultHooks.Store(t, has)
	return has
}

func isRegisteredEnum(t reflect.Type) bool {
	info := TypeInfoOf(t)
	return info != nil && info.IsEnum
}
