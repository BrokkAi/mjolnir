// Port of anyhow 1.0.102 src/error.rs

package anyhow

import (
	"reflect"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// New is anyhow::Error::new and `From<E> for anyhow::Error`: it makes an
// anyhow error from any error value. The result prints, chains and downcasts
// as err does; what changes is that it is now known to be an anyhow::Error,
// so {:#} prints its sources and {:?} its message and "Caused by:" list.
//
// An error that is already an anyhow error is returned as it is, as the
// identity conversion `?` applies. New(nil) is nil.
func New(err error) error {
	if err == nil {
		return nil
	}
	if _, ok := err.(*errorImpl); ok {
		return err
	}
	return constructFromStd(err)
}

// Msg is anyhow::Error::msg, and `anyhow!(message)` for a message that is not
// an error: an error whose text is the message's Display output and which has
// no source. message is a string or any value that implements Display; the
// error downcasts to its type.
//
// A message that is itself an error keeps only its text, as in the crate. Use
// New to keep its sources.
func Msg(message any) error {
	return constructFromAdhoc(message)
}

// FromBoxed is anyhow::Error::from_boxed, and `anyhow!(boxed)` for a
// `Box<dyn Error + Send + Sync>`. It differs from New in one way: the result
// downcasts to the box, DowncastRef[error], and not to the type inside it.
func FromBoxed(boxedError error) error {
	return constructFromBoxed(boxedError)
}

func constructFromStd(err error) error {
	return &errorImpl{vtable: vtableStd, object: err}
}

func constructFromAdhoc(message any) error {
	return &errorImpl{vtable: vtableAdhoc, object: &messageError{message: message}}
}

func constructFromDisplay(message any) error {
	return &errorImpl{vtable: vtableDisplay, object: &displayError{message: message}}
}

func constructFromContext(context any, err error) error {
	return &errorImpl{vtable: vtableContext, object: &contextError{context: context, error: err}}
}

func constructFromBoxed(err error) error {
	return &errorImpl{vtable: vtableBoxed, object: &boxedError{inner: err}}
}

// context is anyhow::Error::context: the cause is an anyhow error.
func (e *errorImpl) context(context any) error {
	return &errorImpl{vtable: vtableContextChain, object: &contextError{context: context, error: e}}
}

// Chain is anyhow::Error::chain: the error and its sources, outermost first.
// The elements are what the crate's iterator yields, `&dyn Error` values: a
// context layer is its own element, printing as the context and downcasting
// to nothing, and the innermost error object follows with its own sources.
//
// Chain(nil) is empty.
func Chain(err error) []error {
	var chain []error
	for cause := errorObject(err); cause != nil; cause = source(cause) {
		chain = append(chain, cause)
	}
	return chain
}

// RootCause is anyhow::Error::root_cause: the last element of Chain.
func RootCause(err error) error {
	chain := Chain(err)
	if len(chain) == 0 {
		rs.PanicAt(1, "called `Option::unwrap()` on a `None` value")
	}
	return chain[len(chain)-1]
}

// Source is `error.source()` on an anyhow error, which Rust reaches through
// Deref: the source of the error object, the second element of Chain.
func Source(err error) rs.Option[error] {
	object := errorObject(err)
	if object == nil {
		return rs.None[error]()
	}
	cause := source(object)
	return rs.OptionOf(cause, cause != nil)
}

// AsRef is `error.as_ref()` and `&*error`: the error object inside an anyhow
// error, as the `dyn std::error::Error` a `source()` method returns. For an
// error that is not a handle made by this package it is the error itself.
func AsRef(err error) error {
	return errorObject(err)
}

// Is is anyhow::Error::is: whether DowncastRef[T] finds a value.
func Is[T any](err error) bool {
	_, ok := DowncastRef[T](err)
	return ok
}

// Downcast is anyhow::Error::downcast: the value DowncastRef[T] finds, or the
// error itself when there is none (the crate's `Err(self)`).
func Downcast[T any](err error) (T, error) {
	value, ok := DowncastRef[T](err)
	if !ok {
		var zero T
		return zero, err
	}
	return value, nil
}

// DowncastRef is anyhow::Error::downcast_ref. On an anyhow error it finds a
// value of type T in the outermost error object or in any context layer: a
// context value itself (`context(C)` with a typed C), or the error the
// innermost context was attached to. It does not look at that error's
// sources; those are found by ranging over Chain.
//
// On any other error, including an element of Chain, it is the `dyn Error`
// downcast: it reports the error itself when that is a T.
//
// T may be a struct or its pointer whichever of the two the error holds, an
// enum's interface, or `error` for the box that FromBoxed wrapped.
func DowncastRef[T any](err error) (T, bool) {
	for {
		impl, ok := err.(*errorImpl)
		if !ok {
			return matchValue[T](err)
		}
		switch impl.vtable {
		case vtableStd:
			// object_downcast::<E>
			return matchValue[T](impl.object)
		case vtableAdhoc:
			// object_downcast::<M>
			return matchValue[T](impl.object.(*messageError).message)
		case vtableDisplay:
			// object_downcast::<M>
			return matchValue[T](impl.object.(*displayError).message)
		case vtableBoxed:
			// object_downcast::<Box<dyn StdError + Send + Sync>>
			return matchBoxed[T](impl.object.(*boxedError).inner)
		case vtableContext:
			// context_downcast::<C, E>
			object := impl.object.(*contextError)
			if value, ok := matchValue[T](object.context); ok {
				return value, true
			}
			return matchValue[T](object.error)
		case vtableContextChain:
			// context_chain_downcast::<C>
			object := impl.object.(*contextError)
			if value, ok := matchValue[T](object.context); ok {
				return value, true
			}
			// Recurse down the context chain per the inner error's vtable.
			err = object.error
		default:
			panic(rs.Unreachable())
		}
	}
}

var boxedDynError = reflect.TypeFor[error]()

// matchValue reports value as a T when the Rust type T stands for is the
// type of value: `TypeId::of::<E>() == target`.
//
// One Rust type is a Go struct or a pointer to it depending on how it was
// ported (a Copy struct is held by value, any other by pointer), and
// downcast_ref hands out a reference either way, so both spellings of T match
// both ways of holding the value. T as an interface matches a value that
// implements it, which is how an enum's variants are found.
func matchValue[T any](value any) (T, bool) {
	var zero T
	target := reflect.TypeFor[T]()
	if target == boxedDynError {
		// Box<dyn Error + Send + Sync> is the type of no error but the box
		// FromBoxed holds; matchBoxed answers for that one.
		return zero, false
	}
	if typed, ok := value.(T); ok {
		return typed, true
	}
	stored := reflect.ValueOf(value)
	if !stored.IsValid() {
		return zero, false
	}
	if target.Kind() == reflect.Pointer && stored.Type() == target.Elem() {
		pointer := reflect.New(target.Elem())
		pointer.Elem().Set(stored)
		return pointer.Interface().(T), true
	}
	if stored.Kind() == reflect.Pointer && !stored.IsNil() && stored.Type().Elem() == target {
		return stored.Elem().Interface().(T), true
	}
	return zero, false
}

// matchBoxed is object_downcast::<Box<dyn StdError + Send + Sync>>: only the
// box itself is found, never the type inside it.
func matchBoxed[T any](inner error) (T, bool) {
	var zero T
	if reflect.TypeFor[T]() != boxedDynError {
		return zero, false
	}
	typed, ok := any(inner).(T)
	return typed, ok
}

// Error is `error.to_string()`: the {} form, the outermost message alone.
func (e *errorImpl) Error() string {
	return displayString(e.object)
}

// String is the {} form, as Error.
func (e *errorImpl) String() string {
	return e.Error()
}

// FmtDisplay is `impl Display for anyhow::Error`.
func (e *errorImpl) FmtDisplay(f *rs.Formatter) error {
	return e.display(f)
}

// FmtDebug is `impl Debug for anyhow::Error`.
func (e *errorImpl) FmtDebug(f *rs.Formatter) error {
	return e.debug(f)
}

// Unwrap is `source()`, as it is for every error of the port: the cause under
// the outermost message, the second element of Chain.
func (e *errorImpl) Unwrap() error {
	return source(e.object)
}

// As lets errors.As find what DowncastRef finds: the error object, or a typed
// context in front of it. The errors package then goes on to Unwrap.
func (e *errorImpl) As(target any) bool {
	return asValue(e.object, target)
}

// Is lets errors.Is match the error object.
func (e *errorImpl) Is(target error) bool {
	return isValue(e.object, target)
}

// errorVTable says how an error was constructed, which decides what it
// downcasts to. It stands for the crate's ErrorVTable; the other entries of
// that table (drop, boxing, backtrace) have nothing to do in Go.
type errorVTable uint8

const (
	// vtableStd: construct_from_std, an error value E.
	vtableStd errorVTable = iota
	// vtableAdhoc: construct_from_adhoc, MessageError<M>.
	vtableAdhoc
	// vtableDisplay: construct_from_display, DisplayError<M>.
	vtableDisplay
	// vtableContext: construct_from_context, ContextError<C, E> over an
	// error value E.
	vtableContext
	// vtableContextChain: Error::context, ContextError<C, Error> over an
	// anyhow error.
	vtableContextChain
	// vtableBoxed: construct_from_boxed, BoxedError.
	vtableBoxed
)

// errorImpl is what an anyhow::Error points to (ErrorImpl<E>): the handle
// this package returns as an error.
type errorImpl struct {
	vtable errorVTable
	// object is ErrorImpl::_object as the `dyn StdError` the vtable's
	// object_ref gives: the error value itself, or one of messageError,
	// displayError, contextError, boxedError.
	object error
}

// contextError is ContextError<C, E>.
type contextError struct {
	context any
	// error is the cause: an error value E, or the *errorImpl of an anyhow
	// error.
	error error
}

// errorObject is ErrorImpl::error: the `dyn StdError` inside an anyhow
// error. An error that is not a handle is its own object.
func errorObject(err error) error {
	if impl, ok := err.(*errorImpl); ok {
		return impl.object
	}
	return err
}
