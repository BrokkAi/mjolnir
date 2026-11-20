// Port of std 1.99.1 src/thread/local.rs

package rsthread

import (
	"reflect"
	"unsafe"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// LocalKey is std::thread::LocalKey<T>: a key to a value that each thread
// has its own copy of, which is what `thread_local!` declares.
//
//	thread_local! {
//	    static VISITS: Cell<usize> = const { Cell::new(0) };
//	}
//
// is a package variable
//
//	var VISITS = rsthread.LocalKeyNew(func() rs.Cell[int] { return rs.CellNew(0) })
//
// A thread's value is made by the initializer the first time the thread uses
// the key. When a thread that Spawn or a scope started ends, its values are
// dropped (their Drop runs, if they have one), the most recently made first,
// as Rust drops thread-locals.
//
// The value is reached through With (or LocalKeyWith for a closure that
// returns something) and, for a Cell or RefCell value, the functions
// LocalKeyCell* and LocalKeyRefCell*.
type LocalKey[T any] struct {
	init func() T
}

// LocalKeyNew declares a thread-local with its initializer.
func LocalKeyNew[T any](init func() T) *LocalKey[T] {
	return &LocalKey[T]{init: init}
}

// localSlot is one thread's value of one key.
type localSlot struct {
	// value is a *T.
	value any
	// drop runs T's destructor on value.
	drop func()
	// destroyed is set once the value was dropped at thread exit.
	destroyed bool
}

// localStorage is one thread's thread-local values.
type localStorage struct {
	slots map[any]*localSlot
	// order lists the slots in the order they were made.
	order []*localSlot
}

const accessErrorPanic = "cannot access a Thread Local Storage value during or after destruction: AccessError"

// slot returns the calling thread's value for key, made with init when the
// thread has none yet and init is not nil.
func (s *localStorage) slot(key any, skip int) *localSlot {
	found, ok := s.slots[key]
	if ok && found.destroyed {
		rs.PanicAt(skip+1, accessErrorPanic)
	}
	if ok {
		return found
	}
	return nil
}

func (s *localStorage) insert(key any, slot *localSlot) {
	if s.slots == nil {
		s.slots = make(map[any]*localSlot)
	}
	s.slots[key] = slot
	s.order = append(s.order, slot)
}

// destroy drops every value, the most recently made first, including values
// made by destructors that run here.
func (s *localStorage) destroy() {
	for len(s.order) > 0 {
		slot := s.order[len(s.order)-1]
		s.order = s.order[:len(s.order)-1]
		slot.destroyed = true
		slot.drop()
	}
}

// get returns a pointer to the calling thread's value, making it if needed.
func (k *LocalKey[T]) get(skip int) *T {
	storage := &currentInner().locals
	if slot := storage.slot(k, skip+1); slot != nil {
		return slot.value.(*T)
	}
	value := k.init()
	// The initializer may have used the key itself. Rust keeps the value
	// made last and drops the other (LazyStorage::initialize).
	if slot := storage.slot(k, skip+1); slot != nil {
		pointer := slot.value.(*T)
		old := *pointer
		*pointer = value
		dropValue(&old)
		return pointer
	}
	return k.insert(storage, value)
}

func (k *LocalKey[T]) insert(storage *localStorage, value T) *T {
	pointer := &value
	storage.insert(k, &localSlot{value: pointer, drop: func() { dropValue(pointer) }})
	return pointer
}

// With is LocalKey::with for a closure with no result: it calls f with the
// calling thread's value.
func (k *LocalKey[T]) With(f func(v *T)) {
	f(k.get(1))
}

// LocalKeyWith is LocalKey::with: f's result for the calling thread's value.
func LocalKeyWith[T, R any](k *LocalKey[T], f func(v *T) R) R {
	return f(k.get(1))
}

// LocalKeyTryWith is LocalKey::try_with: LocalKeyWith, or false instead of
// the panic when the thread's values are being dropped and this one already
// was.
func LocalKeyTryWith[T, R any](k *LocalKey[T], f func(v *T) R) (R, bool) {
	storage := &currentInner().locals
	if found, ok := storage.slots[k]; ok && found.destroyed {
		var zero R
		return zero, false
	}
	return f(k.get(1)), true
}

// set stores value without running the initializer when the thread has no
// value yet: LocalKey::initialize_with.
func set[T any](k *LocalKey[T], value T, assign func(existing *T, value T), skip int) {
	storage := &currentInner().locals
	if slot := storage.slot(k, skip+1); slot != nil {
		assign(slot.value.(*T), value)
		return
	}
	k.insert(storage, value)
}

// LocalKeyCellSet is LocalKey<Cell<T>>::set: it stores value, without running
// the initializer if the thread had no value yet.
func LocalKeyCellSet[T any](k *LocalKey[rs.Cell[T]], value T) {
	set(k, rs.CellNew(value), func(existing *rs.Cell[T], value rs.Cell[T]) {
		existing.Set(value.IntoInner())
	}, 1)
}

// LocalKeyCellGet is LocalKey<Cell<T>>::get.
func LocalKeyCellGet[T any](k *LocalKey[rs.Cell[T]]) T { return k.get(1).Get() }

// LocalKeyCellTake is LocalKey<Cell<T>>::take: the value, leaving T's
// Default.
func LocalKeyCellTake[T any](k *LocalKey[rs.Cell[T]]) T { return k.get(1).Take() }

// LocalKeyCellReplace is LocalKey<Cell<T>>::replace: it stores value and
// returns the previous one.
func LocalKeyCellReplace[T any](k *LocalKey[rs.Cell[T]], value T) T {
	return k.get(1).Replace(value)
}

// LocalKeyCellUpdate is LocalKey<Cell<T>>::update.
func LocalKeyCellUpdate[T any](k *LocalKey[rs.Cell[T]], f func(T) T) { k.get(1).Update(f) }

// LocalKeyRefCellWithBorrow is LocalKey<RefCell<T>>::with_borrow.
func LocalKeyRefCellWithBorrow[T, R any](k *LocalKey[rs.RefCell[T]], f func(v *T) R) R {
	return f(k.get(1).Borrow())
}

// LocalKeyRefCellWithBorrowMut is LocalKey<RefCell<T>>::with_borrow_mut.
func LocalKeyRefCellWithBorrowMut[T, R any](k *LocalKey[rs.RefCell[T]], f func(v *T) R) R {
	return f(k.get(1).BorrowMut())
}

// LocalKeyRefCellSet is LocalKey<RefCell<T>>::set: it stores value, without
// running the initializer if the thread had no value yet.
func LocalKeyRefCellSet[T any](k *LocalKey[rs.RefCell[T]], value T) {
	set(k, rs.RefCellNew(value), func(existing *rs.RefCell[T], value rs.RefCell[T]) {
		*existing.BorrowMut() = value.IntoInner()
	}, 1)
}

// LocalKeyRefCellTake is LocalKey<RefCell<T>>::take.
func LocalKeyRefCellTake[T any](k *LocalKey[rs.RefCell[T]]) T { return k.get(1).Take() }

// LocalKeyRefCellReplace is LocalKey<RefCell<T>>::replace.
func LocalKeyRefCellReplace[T any](k *LocalKey[rs.RefCell[T]], value T) T {
	return k.get(1).Replace(value)
}

// dropValue runs the destructor of a value that is going away, as Rust drops
// it: its own Drop, or that of the value an interface or pointer holds.
func dropValue[T any](value *T) {
	if dropper, ok := any(value).(rs.Dropper); ok {
		dropper.Drop()
		return
	}
	switch reflect.TypeFor[T]().Kind() {
	case reflect.Pointer:
		if *(*unsafe.Pointer)(unsafe.Pointer(value)) == nil {
			return
		}
		fallthrough
	case reflect.Interface:
		if dropper, ok := any(*value).(rs.Dropper); ok {
			dropper.Drop()
		}
	}
}
