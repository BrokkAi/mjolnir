// Port of alloc 1.99.1 src/io/buffered/mod.rs

package rsio

import "github.com/BrokkAi/mjolnir/go/rs"

// IntoInnerError is io::IntoInnerError: the error of BufWriter::into_inner
// and LineWriter::into_inner. It holds the buffered writer, which could not
// write its buffer out, and the error that stopped it.
type IntoInnerError[W any] struct {
	writer W
	err    *Error
}

// IoError is IntoInnerError::error: the error that stopped the write. (The
// name Error is the Go error method.)
func (e *IntoInnerError[W]) IoError() *Error { return e.err }

// IntoInner is IntoInnerError::into_inner: the buffered writer, with its
// buffer intact.
func (e *IntoInnerError[W]) IntoInner() W { return e.writer }

// IntoError is IntoInnerError::into_error.
func (e *IntoInnerError[W]) IntoError() *Error { return e.err }

// IntoParts is IntoInnerError::into_parts.
func (e *IntoInnerError[W]) IntoParts() (*Error, W) { return e.err, e.writer }

// Error is the Display text: the cause's.
func (e *IntoInnerError[W]) Error() string { return e.err.String() }

// String is `impl Display for IntoInnerError`.
func (e *IntoInnerError[W]) String() string { return e.err.String() }

// FmtDisplay is `impl Display for IntoInnerError`.
func (e *IntoInnerError[W]) FmtDisplay(f *rs.Formatter) error { return e.err.FmtDisplay(f) }

// DebugString is the derived Debug: `IntoInnerError(writer, error)`.
func (e *IntoInnerError[W]) DebugString() string {
	return "IntoInnerError(" + debugOfValue(e.writer) + ", " + e.err.DebugString() + ")"
}

// FmtDebug is the derived Debug.
func (e *IntoInnerError[W]) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("IntoInnerError").Field(e.writer).Field(e.err).Finish()
}

// ErrorFromIntoInnerError is `impl From<IntoInnerError<W>> for io::Error`:
// the cause.
func ErrorFromIntoInnerError[W any](e *IntoInnerError[W]) *Error { return e.err }
