// Port of anyhow 1.0.102 src/wrapper.rs

package anyhow

import "github.com/BrokkAi/mjolnir/go/rs"

// messageError is MessageError<M>: a message that is Display and Debug.
type messageError struct {
	message any
}

func (e *messageError) Error() string { return displayString(e.message) }

func (e *messageError) String() string { return e.Error() }

func (e *messageError) FmtDebug(f *rs.Formatter) error { return rs.DebugFmt(e.message, f) }

func (e *messageError) FmtDisplay(f *rs.Formatter) error { return rs.DisplayFmt(e.message, f) }

// Unwrap is `source()`: a message has none.
func (e *messageError) Unwrap() error { return nil }

// As and Is let the errors package find a message that is itself an error,
// as DowncastRef does.
func (e *messageError) As(target any) bool { return asValue(e.message, target) }

func (e *messageError) Is(target error) bool { return isValue(e.message, target) }

// displayError is DisplayError<M>: a message that is only Display, so its
// Debug form is its Display form.
type displayError struct {
	message any
}

func (e *displayError) Error() string { return displayString(e.message) }

func (e *displayError) String() string { return e.Error() }

func (e *displayError) FmtDebug(f *rs.Formatter) error { return rs.DisplayFmt(e.message, f) }

func (e *displayError) FmtDisplay(f *rs.Formatter) error { return rs.DisplayFmt(e.message, f) }

// Unwrap is `source()`: a message has none.
func (e *displayError) Unwrap() error { return nil }

func (e *displayError) As(target any) bool { return asValue(e.message, target) }

func (e *displayError) Is(target error) bool { return isValue(e.message, target) }

// boxedError is BoxedError: a `Box<dyn StdError + Send + Sync>`.
type boxedError struct {
	inner error
}

func (e *boxedError) Error() string { return displayString(e.inner) }

func (e *boxedError) String() string { return e.Error() }

func (e *boxedError) FmtDebug(f *rs.Formatter) error { return rs.DebugFmt(e.inner, f) }

func (e *boxedError) FmtDisplay(f *rs.Formatter) error { return rs.DisplayFmt(e.inner, f) }

// Unwrap is BoxedError::source: the source of the boxed error.
func (e *boxedError) Unwrap() error { return source(e.inner) }

// As and Is let the errors package see the boxed error itself.
func (e *boxedError) As(target any) bool { return asValue(e.inner, target) }

func (e *boxedError) Is(target error) bool { return isValue(e.inner, target) }
