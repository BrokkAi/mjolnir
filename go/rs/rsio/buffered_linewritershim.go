// Port of alloc 1.99.1 src/io/buffered/linewritershim.rs

package rsio

import (
	"bytes"
	"io"
)

// lineWriterShim is LineWriterShim: private implementation of LineWriter.
// This allows a BufWriter to have line buffering logic applied to it, without
// the overhead of a second buffer. It is used by LineWriter and by standard
// output.
type lineWriterShim[W io.Writer] struct {
	buffer *BufWriter[W]
}

func (s lineWriterShim[W]) inner() W { return s.buffer.inner }

func (s lineWriterShim[W]) buffered() []byte { return s.buffer.Buffer() }

// If the last byte in the buffer is \n, flush it in preparation for more
// incoming data. Useful when we know a write is coming that has no newlines.
func (s lineWriterShim[W]) flushIfCompletedLine() error {
	if buffered := s.buffered(); len(buffered) != 0 && buffered[len(buffered)-1] == '\n' {
		return s.buffer.flushBuf()
	}
	return nil
}

// Write some data into this BufWriter with line buffering. This means
// that, if any newlines are present in the data, the data up to the last
// newline is sent directly to the underlying writer, and data after it
// is buffered. Returns the number of bytes written.
//
// This function operates on a "best effort basis"; in keeping with the
// convention of `Write::write`, it makes at most one attempt to write
// new data to the underlying writer. If that write only reports a partial
// success, the remaining data will be buffered.
//
// Because this function attempts to send completed lines to the underlying
// writer, it will also flush the existing buffer if it ends with a
// newline, even if the incoming data does not contain any newlines.
func (s lineWriterShim[W]) write(buf []byte) (int, error) {
	newlineIdx := bytes.LastIndexByte(buf, '\n')
	if newlineIdx < 0 {
		// If there are no new newlines (that is, if this write is less than
		// one line), just do a regular buffered write (which may flush if
		// we exceed the inner buffer's size)
		if err := s.flushIfCompletedLine(); err != nil {
			return 0, err
		}
		return s.buffer.Write(buf)
	}
	// Otherwise, arrange for the lines to be written directly to the
	// inner writer.
	newlineIdx++

	// Flush existing content to prepare for our write. We have to do this
	// before attempting to write `buf` in order to maintain consistency;
	// if we add `buf` to the buffer then try to flush it all at once,
	// we're obligated to return Ok(), which would mean suppressing any
	// errors that occur during flush.
	if err := s.buffer.flushBuf(); err != nil {
		return 0, err
	}

	// This is what we're going to try to write directly to the inner
	// writer. The rest will be buffered, if nothing goes wrong.
	lines := buf[:newlineIdx]

	// Write `lines` directly to the inner writer. In keeping with the
	// `write` convention, make at most one attempt to add new (unbuffered)
	// data. Because this write doesn't touch the BufWriter state directly,
	// and the buffer is known to be empty, we don't need to worry about
	// self.buffer.panicked here.
	flushed, err := callWrite(s.inner(), lines)
	if err != nil {
		return flushed, err
	}

	// If buffer returns Ok(0), propagate that to the caller without
	// doing additional buffering; otherwise we're just guaranteeing
	// an "ErrorKind::WriteZero" later.
	if flushed == 0 {
		return 0, nil
	}

	// Now that the write has succeeded, buffer the rest (or as much of
	// the rest as possible). If there were any unwritten newlines, we
	// only buffer out to the last unwritten newline that fits in the
	// buffer; this helps prevent flushing partial lines on subsequent
	// calls to LineWriterShim::write.

	// Handle the cases in order of most-common to least-common, under
	// the presumption that most writes succeed in totality, and that most
	// writes are smaller than the buffer.
	// - Is this a partial line (ie, no newlines left in the unwritten tail)
	// - If not, does the data out to the last unwritten newline fit in
	//   the buffer?
	// - If not, scan for the last newline that *does* fit in the buffer
	var tail []byte
	if flushed >= newlineIdx {
		tail = buf[flushed:]
		// Avoid unnecessary short writes by not splitting the remaining
		// bytes if they're larger than the buffer.
		// They can be written in full by the next call to write.
		if len(tail) >= s.buffer.Capacity() {
			return flushed, nil
		}
	} else if newlineIdx-flushed <= s.buffer.Capacity() {
		tail = buf[flushed:newlineIdx]
	} else {
		scanArea := buf[flushed:]
		scanArea = scanArea[:s.buffer.Capacity()]
		if i := bytes.LastIndexByte(scanArea, '\n'); i >= 0 {
			tail = scanArea[:i+1]
		} else {
			tail = scanArea
		}
	}

	buffered := s.buffer.writeToBuf(tail)
	return flushed + buffered, nil
}

// Write some data into this BufWriter with line buffering. This means
// that, if any newlines are present in the data, the data up to the last
// newline is sent directly to the underlying writer, and data after it
// is buffered.
//
// Because this function attempts to send completed lines to the underlying
// writer, it will also flush the existing buffer if it contains any
// newlines, even if the incoming data does not contain any newlines.
func (s lineWriterShim[W]) writeAll(buf []byte) error {
	newlineIdx := bytes.LastIndexByte(buf, '\n')
	if newlineIdx < 0 {
		// If there are no new newlines (that is, if this write is less than
		// one line), just do a regular buffered write (which may flush if
		// we exceed the inner buffer's size)
		if err := s.flushIfCompletedLine(); err != nil {
			return err
		}
		return s.buffer.WriteAll(buf)
	}
	lines, tail := buf[:newlineIdx+1], buf[newlineIdx+1:]

	if len(s.buffered()) == 0 {
		if err := WriteAll(s.inner(), lines); err != nil {
			return err
		}
	} else {
		// If there is any buffered data, we add the incoming lines
		// to that buffer before flushing, which saves us at least
		// one write call. We can't really do this with `write`,
		// since we can't do this *and* not suppress errors *and*
		// report a consistent state to the caller in a return
		// value, but here in write_all it's fine.
		if err := s.buffer.WriteAll(lines); err != nil {
			return err
		}
		if err := s.buffer.flushBuf(); err != nil {
			return err
		}
	}

	return s.buffer.WriteAll(tail)
}
