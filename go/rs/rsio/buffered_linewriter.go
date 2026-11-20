// Port of alloc 1.99.1 src/io/buffered/linewriter.rs

package rsio

import (
	"io"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// LineWriter is Rust's io::LineWriter: a buffered writer that writes its
// buffer out whenever it is given a newline. Its default capacity is 1024.
// Standard output is one.
//
// Like BufWriter it must be dropped, and it owns its writer.
type LineWriter[W io.Writer] struct {
	inner *BufWriter[W]
}

// LineWriterNew is LineWriter::new.
func LineWriterNew[W io.Writer](inner W) *LineWriter[W] {
	// Lines typically aren't that long, don't use a giant buffer
	return LineWriterWithCapacity(1024, inner)
}

// LineWriterWithCapacity is LineWriter::with_capacity.
func LineWriterWithCapacity[W io.Writer](capacity int, inner W) *LineWriter[W] {
	return &LineWriter[W]{inner: BufWriterWithCapacity(capacity, inner)}
}

// GetMut is LineWriter::get_mut.
func (l *LineWriter[W]) GetMut() W { return l.inner.GetMut() }

// IntoInner is LineWriter::into_inner: it writes the buffer out and gives
// the writer back. When that write fails the error is an *IntoInnerError
// holding this LineWriter.
func (l *LineWriter[W]) IntoInner() (W, error) {
	inner, err := l.inner.IntoInner()
	if err != nil {
		cause := err.(*IntoInnerError[*BufWriter[W]])
		return inner, &IntoInnerError[*LineWriter[W]]{writer: l, err: cause.err}
	}
	return inner, nil
}

// GetRef is LineWriter::get_ref.
func (l *LineWriter[W]) GetRef() W { return l.inner.GetRef() }

// Write is Write::write.
func (l *LineWriter[W]) Write(buf []byte) (int, error) {
	return lineWriterShim[W]{buffer: l.inner}.write(buf)
}

// Flush is Write::flush.
func (l *LineWriter[W]) Flush() error { return l.inner.Flush() }

// WriteAll is Write::write_all.
func (l *LineWriter[W]) WriteAll(buf []byte) error {
	return lineWriterShim[W]{buffer: l.inner}.writeAll(buf)
}

// Drop writes the buffer out, ignoring a failure, and drops the writer.
func (l *LineWriter[W]) Drop() { l.inner.Drop() }

// DebugString is `impl Debug for LineWriter`.
func (l *LineWriter[W]) DebugString() string {
	return "LineWriter { writer: " + debugOfValue(l.inner.inner) + ", buffer: " + l.inner.bufferRatio() + ", .. }"
}

// FmtDebug is `impl Debug for LineWriter`.
func (l *LineWriter[W]) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("LineWriter").
		Field("writer", l.inner.inner).
		Field("buffer", rawDebug(l.inner.bufferRatio())).
		FinishNonExhaustive()
}
