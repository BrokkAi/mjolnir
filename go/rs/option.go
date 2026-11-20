// Port of core 1.99.1 src/option.rs

package rs

// Option is Rust's Option<T>. The zero value is None.
//
// It is used for every Rust Option, including Option<Box<T>> and
// Option<Arc<T>>, so that "absent" has one spelling everywhere and a nil
// pointer is never a legitimate value.
type Option[T any] struct {
	value T
	some  bool
}

// Some returns Some(value).
func Some[T any](value T) Option[T] {
	return Option[T]{value: value, some: true}
}

// None returns None.
func None[T any]() Option[T] {
	return Option[T]{}
}

// IsSome reports whether the option holds a value.
func (o Option[T]) IsSome() bool { return o.some }

// IsNone reports whether the option is None.
func (o Option[T]) IsNone() bool { return !o.some }

// Get returns the value and whether there is one. It is how `if let Some(v) =
// o` and `match` on an option are written:
//
//	if v, ok := o.Get(); ok { ... }
func (o Option[T]) Get() (T, bool) { return o.value, o.some }

// AsRef returns a pointer to the held value, or nil for None. It is
// `as_ref()` and `as_mut()` and the binding of `if let Some(v) = &mut o`:
// writes through the pointer change the option.
func (o *Option[T]) AsRef() *T {
	if !o.some {
		return nil
	}
	return &o.value
}

// IsSomeAnd is Option::is_some_and.
func (o Option[T]) IsSomeAnd(f func(T) bool) bool { return o.some && f(o.value) }

// IsNoneOr is Option::is_none_or.
func (o Option[T]) IsNoneOr(f func(T) bool) bool { return !o.some || f(o.value) }

// Unwrap returns the value, panicking as Rust does on None.
func (o Option[T]) Unwrap() T {
	if !o.some {
		PanicAt(1, "called `Option::unwrap()` on a `None` value")
	}
	return o.value
}

// Expect returns the value, panicking with message on None.
func (o Option[T]) Expect(message string) T {
	if !o.some {
		PanicAt(1, message)
	}
	return o.value
}

// UnwrapOr is Option::unwrap_or.
func (o Option[T]) UnwrapOr(fallback T) T {
	if o.some {
		return o.value
	}
	return fallback
}

// UnwrapOrElse is Option::unwrap_or_else.
func (o Option[T]) UnwrapOrElse(f func() T) T {
	if o.some {
		return o.value
	}
	return f()
}

// UnwrapOrDefault is Option::unwrap_or_default for types whose Default is the
// Go zero value. Use UnwrapOrElse with the type's Default function otherwise.
func (o Option[T]) UnwrapOrDefault() T {
	return o.value
}

// Or is Option::or.
func (o Option[T]) Or(other Option[T]) Option[T] {
	if o.some {
		return o
	}
	return other
}

// OrElse is Option::or_else.
func (o Option[T]) OrElse(f func() Option[T]) Option[T] {
	if o.some {
		return o
	}
	return f()
}

// Xor is Option::xor.
func (o Option[T]) Xor(other Option[T]) Option[T] {
	switch {
	case o.some && !other.some:
		return o
	case !o.some && other.some:
		return other
	default:
		return Option[T]{}
	}
}

// Filter is Option::filter.
func (o Option[T]) Filter(keep func(T) bool) Option[T] {
	if o.some && keep(o.value) {
		return o
	}
	return Option[T]{}
}

// Inspect is Option::inspect.
func (o Option[T]) Inspect(f func(T)) Option[T] {
	if o.some {
		f(o.value)
	}
	return o
}

// Take is Option::take: it returns the option and leaves None.
func (o *Option[T]) Take() Option[T] {
	taken := *o
	*o = Option[T]{}
	return taken
}

// TakeIf is Option::take_if.
func (o *Option[T]) TakeIf(predicate func(*T) bool) Option[T] {
	if o.some && predicate(&o.value) {
		return o.Take()
	}
	return Option[T]{}
}

// Replace is Option::replace: it stores Some(value) and returns the old option.
func (o *Option[T]) Replace(value T) Option[T] {
	old := *o
	*o = Option[T]{value: value, some: true}
	return old
}

// Insert is Option::insert: it stores Some(value) and returns a pointer to it.
func (o *Option[T]) Insert(value T) *T {
	*o = Option[T]{value: value, some: true}
	return &o.value
}

// GetOrInsert is Option::get_or_insert.
func (o *Option[T]) GetOrInsert(value T) *T {
	if !o.some {
		*o = Option[T]{value: value, some: true}
	}
	return &o.value
}

// GetOrInsertWith is Option::get_or_insert_with.
func (o *Option[T]) GetOrInsertWith(f func() T) *T {
	if !o.some {
		*o = Option[T]{value: f(), some: true}
	}
	return &o.value
}

// OkOr is Option::ok_or.
func (o Option[T]) OkOr(err error) (T, error) {
	if o.some {
		return o.value, nil
	}
	var zero T
	return zero, err
}

// OkOrElse is Option::ok_or_else.
func (o Option[T]) OkOrElse(f func() error) (T, error) {
	if o.some {
		return o.value, nil
	}
	var zero T
	return zero, f()
}

// Slice returns the value as a slice of zero or one element, which is how
// `option.iter()` and `option.into_iter()` are ranged over.
func (o Option[T]) Slice() []T {
	if o.some {
		return []T{o.value}
	}
	return nil
}

// OptionMap is Option::map. It is a function because a Go method cannot
// introduce the result's type.
func OptionMap[T, U any](o Option[T], f func(T) U) Option[U] {
	if o.some {
		return Some(f(o.value))
	}
	return Option[U]{}
}

// OptionMapOr is Option::map_or.
func OptionMapOr[T, U any](o Option[T], fallback U, f func(T) U) U {
	if o.some {
		return f(o.value)
	}
	return fallback
}

// OptionMapOrElse is Option::map_or_else.
func OptionMapOrElse[T, U any](o Option[T], fallback func() U, f func(T) U) U {
	if o.some {
		return f(o.value)
	}
	return fallback()
}

// OptionAndThen is Option::and_then.
func OptionAndThen[T, U any](o Option[T], f func(T) Option[U]) Option[U] {
	if o.some {
		return f(o.value)
	}
	return Option[U]{}
}

// OptionAnd is Option::and.
func OptionAnd[T, U any](o Option[T], other Option[U]) Option[U] {
	if o.some {
		return other
	}
	return Option[U]{}
}

// OptionZip is Option::zip.
func OptionZip[T, U any](a Option[T], b Option[U]) Option[Tuple2[T, U]] {
	if a.some && b.some {
		return Some(Tuple2[T, U]{F0: a.value, F1: b.value})
	}
	return Option[Tuple2[T, U]]{}
}

// OptionFlatten is Option::flatten.
func OptionFlatten[T any](o Option[Option[T]]) Option[T] {
	if o.some {
		return o.value
	}
	return Option[T]{}
}

// OptionTranspose is Option<Result<T, E>>::transpose, for a fallible
// computation that produced an option of a result.
func OptionTranspose[T any](o Option[Result[T]]) (Option[T], error) {
	if !o.some {
		return Option[T]{}, nil
	}
	if o.value.Err != nil {
		return Option[T]{}, o.value.Err
	}
	return Some(o.value.Value), nil
}

// OptionFromPtr returns Some(*p), or None when p is nil. It is `.copied()` and
// `.cloned()` on an Option<&T> that Go code holds as a pointer.
func OptionFromPtr[T any](p *T) Option[T] {
	if p == nil {
		return Option[T]{}
	}
	return Some(*p)
}

// OptionOf returns Some(value) when ok and None otherwise. It adapts Go's
// comma-ok results, and is `cond.then_some(value)`.
func OptionOf[T any](value T, ok bool) Option[T] {
	if ok {
		return Some(value)
	}
	return Option[T]{}
}

// OptionThen is bool::then: Some(f()) when cond holds, without calling f
// otherwise.
func OptionThen[T any](cond bool, f func() T) Option[T] {
	if cond {
		return Some(f())
	}
	return Option[T]{}
}
