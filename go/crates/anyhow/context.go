// Port of anyhow 1.0.102 src/context.rs

package anyhow

import "github.com/BrokkAi/mjolnir/go/rs"

// extContext is ext::StdError::ext_context: an anyhow error takes a context
// layer of its own kind, any other error becomes the error under the context.
func extContext(err error, context any) error {
	if impl, ok := err.(*errorImpl); ok {
		return impl.context(context)
	}
	return constructFromContext(context, err)
}

// Context is `result.context(context)` (the Context trait for Result) and
// anyhow::Error::context: it wraps err with context, which becomes the
// outermost message while err becomes its cause.
//
// context is a string or any value that implements Display. A typed context
// can be downcast to: DowncastRef[C] finds it in front of the error it was
// attached to.
//
// Context(nil, ..) is nil, as `Ok(v).context(..)` is `Ok(v)`, so the call can
// follow a fallible one without a branch:
//
//	value, err := load(path)
//	return value, anyhow.Context(err, "load the configuration")
//
// The context argument is evaluated either way, as Rust evaluates the
// argument of `.context(..)`. Use WithContext when building it costs.
func Context(err error, context any) error {
	if err == nil {
		return nil
	}
	return extContext(err, context)
}

// WithContext is `result.with_context(|| ..)`: Context with a context that
// is built only when there is an error. context is not called when err is
// nil.
//
//	value, err := load(path)
//	return value, anyhow.WithContext(err, func() string {
//		return rs.Format("load {}", rspath.Display(path))
//	})
func WithContext[C any](err error, context func() C) error {
	if err == nil {
		return nil
	}
	return extContext(err, context())
}

// OptionContext is `option.context(context)` (the Context trait for Option):
// the value, or for None an error whose message is context.
//
// The error differs from Msg(context) in one way: its {:#?} form is the
// context's Display text rather than its Debug text, so a string prints
// without quotes.
func OptionContext[T any](option rs.Option[T], context any) (T, error) {
	if value, ok := option.Get(); ok {
		return value, nil
	}
	var zero T
	return zero, constructFromDisplay(context)
}

// OptionWithContext is `option.with_context(|| ..)`. context is not called
// for Some.
func OptionWithContext[T, C any](option rs.Option[T], context func() C) (T, error) {
	if value, ok := option.Get(); ok {
		return value, nil
	}
	var zero T
	return zero, constructFromDisplay(context())
}

// FmtDebug is `impl Debug for ContextError<C, E>`. It is what an element of
// Chain prints with {:?}, and what an anyhow error with context prints with
// {:#?}.
func (e *contextError) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Error").
		Field("context", quoted{context: e.context}).
		Field("source", e.error).
		Finish()
}

// FmtDisplay is `impl Display for ContextError<C, E>`: the context's own
// Display, with the formatter's flags.
func (e *contextError) FmtDisplay(f *rs.Formatter) error {
	return rs.DisplayFmt(e.context, f)
}

func (e *contextError) Error() string { return displayString(e.context) }

func (e *contextError) String() string { return e.Error() }

// Unwrap is `impl StdError for ContextError<C, E>`: the source is the error
// the context was attached to.
func (e *contextError) Unwrap() error {
	return errorObject(e.error)
}

// As and Is let the errors package find a context value that is itself an
// error, in front of the cause, as DowncastRef does.
func (e *contextError) As(target any) bool { return asValue(e.context, target) }

func (e *contextError) Is(target error) bool { return isValue(e.context, target) }

// quoted is the crate's Quoted: a context's Display text between double
// quotes, escaped as `str::escape_debug` escapes.
type quoted struct {
	context any
}

func (q quoted) FmtDebug(f *rs.Formatter) error {
	if err := f.WriteChar('"'); err != nil {
		return err
	}
	// The crate escapes each piece the context's Display writes. A Go value
	// gives its text as one piece.
	if err := f.WriteStr(escapeDebug(displayString(q.context))); err != nil {
		return err
	}
	return f.WriteChar('"')
}
