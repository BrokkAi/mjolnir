// Port of core 1.99.1 src/io/write.rs

package rsio

import "io"

// Rust's Write trait has no Go type of its own: a writer is an io.Writer,
// and the trait's methods are the functions in this file, which take one.
// `write!(w, ..)` and `writeln!` are rs.Write and rs.Writeln.
//
// As for readers, the two languages differ, and the functions here accept
// both kinds of writer:
//
//   - Rust's write may accept only part of the buffer and say so with a
//     count smaller than the buffer, without an error. The writers of the
//     port do that. A Go writer must return an error with a short count;
//     io.ErrShortWrite from one is read as the short count it stands for.
//   - Rust's flush is a method of the trait. Here it is a method
//     `Flush() error`; a writer without one has nothing to flush.
//
// A writer of the port must not be handed to a Go library that expects a
// whole write or an error (fmt.Fprintf, io.WriteString): a short count would
// go unnoticed. Wrap it with GoWriter for that.

// callWrite makes one Write call and gives its result as Rust's write
// reports it.
func callWrite(w io.Writer, buf []byte) (int, error) {
	n, err := w.Write(buf)
	if err == nil || err == io.ErrShortWrite {
		return n, nil
	}
	return n, From(err)
}

// Write is `w.write(buf)` with Rust's contract: it returns how many bytes of
// buf the writer accepted, which can be fewer than len(buf), or an *Error.
func Write(w io.Writer, buf []byte) (int, error) {
	return callWrite(w, buf)
}

// Flush is `w.flush()`.
func Flush(w io.Writer) error {
	if flusher, ok := w.(interface{ Flush() error }); ok {
		return asError(flusher.Flush())
	}
	return nil
}

// WriteAll is `w.write_all(buf)`: it writes all of buf, or fails. A writer
// that accepts nothing more makes it fail with WriteZero, "failed to write
// whole buffer".
func WriteAll(w io.Writer, buf []byte) error {
	if writer, ok := deref(w).(interface {
		WriteAll(buf []byte) error
	}); ok {
		return asError(writer.WriteAll(buf))
	}
	return DefaultWriteAll(w, buf)
}

// DefaultWriteAll is the body of Write::write_all.
func DefaultWriteAll(w io.Writer, buf []byte) error {
	for len(buf) != 0 {
		n, err := callWrite(w, buf)
		if err != nil {
			if n == 0 && isInterruptedError(err) {
				continue
			}
			return err
		}
		if n == 0 {
			return errWriteAllEof
		}
		buf = buf[n:]
	}
	return nil
}

// GoWriter gives a writer of the port to a Go library: the result follows
// Go's contract, writing the whole buffer or returning an error. It is
// write_all: a writer that accepts nothing more gives io.ErrShortWrite.
func GoWriter(w io.Writer) io.Writer { return &goWriter{w: w} }

type goWriter struct {
	w io.Writer
}

func (g *goWriter) Write(buf []byte) (int, error) {
	written := 0
	for written < len(buf) {
		n, err := callWrite(g.w, buf[written:])
		written += n
		if err != nil {
			if n == 0 && isInterruptedError(err) {
				continue
			}
			return written, err
		}
		if n == 0 {
			return written, io.ErrShortWrite
		}
	}
	return written, nil
}

// Flush flushes the writer underneath.
func (g *goWriter) Flush() error { return Flush(g.w) }
