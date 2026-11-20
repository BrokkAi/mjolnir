// Port of alloc 1.99.1 src/io/buffered/bufwriter.rs

package rsio

import (
	"io"
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// BufWriter is Rust's io::BufWriter: a writer with a buffer in front of
// another writer. The default capacity is 8 KiB.
//
// It must be dropped: Drop writes the buffer out, ignoring an error as
// Rust's destructor does, and then drops the writer it owns. Build it on
// ByRef(w), as Rust code writes `BufWriter::new(&mut w)`, to keep w. Code
// that must see a failed write calls Flush first, as in Rust.
type BufWriter[W io.Writer] struct {
	// The buffer. Its length is what is buffered and its capacity is the
	// writer's capacity, so nothing here may append to it.
	buf []byte
	// #30888: If the inner writer panics in a call to write, we don't want to
	// write the buffered data a second time in BufWriter's destructor. This
	// flag tells the Drop impl if it should skip the flush.
	panicked bool
	inner    W
	// gone is set when the writer has been taken apart or dropped.
	gone bool
}

// BufWriterNew is BufWriter::new.
func BufWriterNew[W io.Writer](inner W) *BufWriter[W] {
	return BufWriterWithCapacity(DefaultBufSize, inner)
}

// BufWriterWithCapacity is BufWriter::with_capacity.
func BufWriterWithCapacity[W io.Writer](capacity int, inner W) *BufWriter[W] {
	return &BufWriter[W]{inner: inner, buf: make([]byte, 0, capacity), panicked: false}
}

// IntoInner is BufWriter::into_inner: it writes the buffer out and gives the
// writer back. When that write fails the error is an *IntoInnerError, which
// holds this BufWriter and the cause.
func (w *BufWriter[W]) IntoInner() (W, error) {
	if err := w.flushBuf(); err != nil {
		var zero W
		return zero, &IntoInnerError[*BufWriter[W]]{writer: w, err: From(err)}
	}
	inner, _ := w.IntoParts()
	return inner, nil
}

// IntoParts is BufWriter::into_parts: the writer and the bytes not yet
// written, without writing them. The bytes are an error, *WriterPanicked,
// when a write to the writer panicked.
func (w *BufWriter[W]) IntoParts() (W, rs.Result[[]byte]) {
	buf := w.buf
	w.buf = nil
	w.gone = true
	if w.panicked {
		return w.inner, rs.Err[[]byte](&WriterPanicked{buf: buf})
	}
	return w.inner, rs.Ok(buf)
}

// flushBuf is BufWriter::flush_buf: it writes the buffer to the writer,
// without flushing the writer.
func (w *BufWriter[W]) flushBuf() error {
	written := 0
	// BufGuard: whatever happens, the bytes that were written leave the
	// buffer.
	defer func() {
		if written > 0 {
			remaining := copy(w.buf, w.buf[written:])
			w.buf = w.buf[:remaining]
		}
	}()

	for written < len(w.buf) {
		w.panicked = true
		n, err := callWrite(w.inner, w.buf[written:])
		w.panicked = false

		written += n
		if err != nil {
			if n == 0 && isInterruptedError(err) {
				continue
			}
			return err
		}
		if n == 0 {
			return ErrorFromStaticMessage(ErrorKind_WriteZero, "failed to write the buffered data")
		}
	}
	return nil
}

// writeToBuf buffers some data without flushing it, regardless of the size
// of the data. Writes as much as possible without exceeding capacity.
// Returns the number of bytes written.
func (w *BufWriter[W]) writeToBuf(buf []byte) int {
	available := w.spareCapacity()
	amountToBuffer := min(available, len(buf))

	w.writeToBufferUnchecked(buf[:amountToBuffer])

	return amountToBuffer
}

// GetRef is BufWriter::get_ref.
func (w *BufWriter[W]) GetRef() W { return w.inner }

// GetMut is BufWriter::get_mut. Writing to the result directly puts those
// bytes ahead of the ones in the buffer.
func (w *BufWriter[W]) GetMut() W { return w.inner }

// Buffer is BufWriter::buffer: the bytes not yet written.
func (w *BufWriter[W]) Buffer() []byte { return w.buf }

// Capacity is BufWriter::capacity.
func (w *BufWriter[W]) Capacity() int { return cap(w.buf) }

// Ensure this function does not get inlined into `write`, so that it
// remains inlineable and its common path remains as short as possible.
// If this function ends up being called frequently relative to `write`,
// it's likely a sign that the client is using an improperly sized buffer
// or their write patterns are somewhat pathological.
func (w *BufWriter[W]) writeCold(buf []byte) (int, error) {
	if len(buf) > w.spareCapacity() {
		if err := w.flushBuf(); err != nil {
			return 0, err
		}
	}

	// Why not len > capacity? To avoid a needless trip through the buffer when the input
	// exactly fills it. We'd just need to flush it to the underlying writer anyway.
	if len(buf) >= cap(w.buf) {
		w.panicked = true
		n, err := callWrite(w.inner, buf)
		w.panicked = false
		return n, err
	}
	// Write to the buffer. In this case, we write to the buffer even if it fills it
	// exactly. Doing otherwise would mean flushing the buffer, then writing this
	// input to the inner writer, which in many cases would be a worse strategy.
	w.writeToBufferUnchecked(buf)

	return len(buf), nil
}

// Ensure this function does not get inlined into `write_all`, so that it
// remains inlineable and its common path remains as short as possible.
// If this function ends up being called frequently relative to `write_all`,
// it's likely a sign that the client is using an improperly sized buffer
// or their write patterns are somewhat pathological.
func (w *BufWriter[W]) writeAllCold(buf []byte) error {
	// Normally, `write_all` just calls `write` in a loop. We can do better
	// by calling `self.get_mut().write_all()` directly, which avoids
	// round trips through the buffer in the event of a series of partial
	// writes in some circumstances.

	if len(buf) > w.spareCapacity() {
		if err := w.flushBuf(); err != nil {
			return err
		}
	}

	// Why not len > capacity? To avoid a needless trip through the buffer when the input
	// exactly fills it. We'd just need to flush it to the underlying writer anyway.
	if len(buf) >= cap(w.buf) {
		w.panicked = true
		err := WriteAll(w.inner, buf)
		w.panicked = false
		return err
	}
	// Write to the buffer. In this case, we write to the buffer even if it fills it
	// exactly. Doing otherwise would mean flushing the buffer, then writing this
	// input to the inner writer, which in many cases would be a worse strategy.
	w.writeToBufferUnchecked(buf)

	return nil
}

// Requires `buf.len() <= self.buf.capacity() - self.buf.len()`,
// i.e., that input buffer length is less than or equal to spare capacity.
func (w *BufWriter[W]) writeToBufferUnchecked(buf []byte) {
	oldLen := len(w.buf)
	w.buf = w.buf[:oldLen+len(buf)]
	copy(w.buf[oldLen:], buf)
}

func (w *BufWriter[W]) spareCapacity() int { return cap(w.buf) - len(w.buf) }

// WriterPanicked is io::WriterPanicked: what BufWriter::into_parts gives in
// place of the buffer when the writer panicked during a write.
type WriterPanicked struct {
	buf []byte
}

// IntoInner is WriterPanicked::into_inner: the buffered bytes, some of which
// may have been written.
func (p *WriterPanicked) IntoInner() []byte { return p.buf }

// Error is the Display text.
func (p *WriterPanicked) Error() string { return p.String() }

// String is `impl Display for WriterPanicked`.
func (p *WriterPanicked) String() string {
	return "BufWriter inner writer panicked, what data remains unwritten is not known"
}

// FmtDisplay is `impl Display for WriterPanicked`.
func (p *WriterPanicked) FmtDisplay(f *rs.Formatter) error { return f.Pad(p.String()) }

func (p *WriterPanicked) bufferRatio() string {
	return strconv.Itoa(len(p.buf)) + "/" + strconv.Itoa(cap(p.buf))
}

// DebugString is `impl Debug for WriterPanicked`.
func (p *WriterPanicked) DebugString() string {
	return "WriterPanicked { buffer: " + p.bufferRatio() + " }"
}

// FmtDebug is `impl Debug for WriterPanicked`.
func (p *WriterPanicked) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("WriterPanicked").Field("buffer", rawDebug(p.bufferRatio())).Finish()
}

// Write is Write::write.
func (w *BufWriter[W]) Write(buf []byte) (int, error) {
	// Use < instead of <= to avoid a needless trip through the buffer in some cases.
	// See `write_cold` for details.
	if len(buf) < w.spareCapacity() {
		w.writeToBufferUnchecked(buf)

		return len(buf), nil
	}
	return w.writeCold(buf)
}

// WriteAll is Write::write_all.
func (w *BufWriter[W]) WriteAll(buf []byte) error {
	// Use < instead of <= to avoid a needless trip through the buffer in some cases.
	// See `write_all_cold` for details.
	if len(buf) < w.spareCapacity() {
		w.writeToBufferUnchecked(buf)

		return nil
	}
	return w.writeAllCold(buf)
}

// Flush is Write::flush: it writes the buffer out and flushes the writer.
func (w *BufWriter[W]) Flush() error {
	if err := w.flushBuf(); err != nil {
		return err
	}
	return Flush(w.inner)
}

func (w *BufWriter[W]) bufferRatio() string {
	return strconv.Itoa(len(w.buf)) + "/" + strconv.Itoa(cap(w.buf))
}

// DebugString is `impl Debug for BufWriter`.
func (w *BufWriter[W]) DebugString() string {
	return "BufWriter { writer: " + debugOfValue(w.inner) + ", buffer: " + w.bufferRatio() + " }"
}

// FmtDebug is `impl Debug for BufWriter`.
func (w *BufWriter[W]) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("BufWriter").
		Field("writer", w.inner).
		Field("buffer", rawDebug(w.bufferRatio())).
		Finish()
}

func (w *BufWriter[W]) seeker() io.Seeker {
	seeker, ok := any(w.inner).(io.Seeker)
	if !ok {
		rs.PanicAt(2, "rsio: "+typeName(w.inner)+" is not seekable")
	}
	return seeker
}

// SeekTo is Seek::seek: it writes the buffer out, then seeks the writer,
// which must be seekable.
func (w *BufWriter[W]) SeekTo(pos SeekFrom) (uint64, error) {
	if err := w.flushBuf(); err != nil {
		return 0, err
	}
	return Seek(w.seeker(), pos)
}

// Seek is Seek::seek through Go's signature.
func (w *BufWriter[W]) Seek(offset int64, whence int) (int64, error) {
	if err := w.flushBuf(); err != nil {
		return 0, err
	}
	position, err := w.seeker().Seek(offset, whence)
	return position, asError(err)
}

// Drop is `impl Drop for BufWriter`: it writes the buffer out unless a write
// to the writer panicked, ignoring a failure, and then drops the writer. A
// BufWriter that was taken apart (IntoInner, IntoParts) owns nothing, and a
// second Drop does nothing.
func (w *BufWriter[W]) Drop() {
	if w.gone {
		return
	}
	w.gone = true
	if !w.panicked {
		// dtors should not panic, so we ignore a failed flush
		_ = w.flushBuf()
	}
	dropValue(w.inner)
}

// copyFrom is BufferedWriterSpec::copy_from for BufWriter.
func (w *BufWriter[W]) copyFrom(reader io.Reader) (uint64, error) {
	if w.Capacity() < DefaultBufSize {
		// Since neither this buffer nor the reader's buffer are large enough,
		// fall back to the unspecialized implementation.
		return stackBufferCopy(reader, w)
	}

	var length uint64
	init := false

	for {
		readBuf := BorrowedBufFromUninit(w.buf[len(w.buf):cap(w.buf)])

		if init {
			// `init` is only true after `reader` initializes
			// `read_buf`. See the comment about `flush_buf` below.
			readBuf.SetInit()
		}

		if readBuf.Capacity() >= DefaultBufSize {
			cursor := readBuf.Unfilled()
			err := ReadBuf(reader, cursor.Reborrow())
			bytesRead := cursor.Written()
			if err != nil {
				if bytesRead == 0 && isInterruptedError(err) {
					continue
				}
				length += uint64(bytesRead)
				w.buf = w.buf[:len(w.buf)+bytesRead]
				return 0, err
			}

			if bytesRead == 0 {
				return length, nil
			}

			init = readBuf.IsInit()
			length += uint64(bytesRead)

			w.buf = w.buf[:len(w.buf)+bytesRead]

			// Read again if the buffer still has enough capacity, as BufWriter itself would do
			// This will occur if the reader returns short reads
		} else {
			// `flush_buf` will not de-initialize any elements of
			// the spare capacity so we can remember `init` across this.
			if err := w.flushBuf(); err != nil {
				return 0, err
			}
		}
	}
}

func (w *BufWriter[W]) writeBufferPriority() int { return w.Capacity() }
