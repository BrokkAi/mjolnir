// Port of alloc 1.99.1 src/io/buf_read.rs

package rsio

import (
	"bytes"
	"io"
)

// BufRead is Rust's io::BufRead: a reader with a buffer that can be looked
// at before it is consumed. The trait's other methods are the functions
// below.
type BufRead interface {
	io.Reader
	// FillBuf is fill_buf: the buffered bytes, read from the source first
	// when there are none. An empty result means the end of input.
	FillBuf() ([]byte, error)
	// Consume is consume: amount bytes of the buffer have been used.
	Consume(amount int)
}

// HasDataLeft is `r.has_data_left()`.
func HasDataLeft(r BufRead) (bool, error) {
	buf, err := r.FillBuf()
	if err != nil {
		return false, asError(err)
	}
	return len(buf) != 0, nil
}

// ReadUntil is `r.read_until(byte, buf)`: it appends to *buf up to and
// including the first delim, or to the end of input, and returns the number
// of bytes read.
func ReadUntil(r BufRead, delim byte, buf *[]byte) (int, error) {
	if reader, ok := deref(r).(interface {
		ReadUntil(delim byte, buf *[]byte) (int, error)
	}); ok {
		n, err := reader.ReadUntil(delim, buf)
		return n, asError(err)
	}
	return DefaultReadUntil(r, delim, buf)
}

// SkipUntil is `r.skip_until(byte)`.
func SkipUntil(r BufRead, delim byte) (int, error) {
	if reader, ok := deref(r).(interface {
		SkipUntil(delim byte) (int, error)
	}); ok {
		n, err := reader.SkipUntil(delim)
		return n, asError(err)
	}
	return defaultSkipUntil(r, delim)
}

// ReadLine is `r.read_line(buf)`: it appends one line to *buf, with its
// newline, and returns the number of bytes read, which is 0 at the end of
// input. A line that is not UTF-8 fails with InvalidData, "stream did not
// contain valid UTF-8", leaves *buf as it was, and is consumed.
func ReadLine(r BufRead, buf *string) (int, error) {
	if reader, ok := deref(r).(interface {
		ReadLine(buf *string) (int, error)
	}); ok {
		n, err := reader.ReadLine(buf)
		return n, asError(err)
	}
	return DefaultReadLine(r, buf)
}

// DefaultReadLine is the body of BufRead::read_line.
func DefaultReadLine(r BufRead, buf *string) (int, error) {
	// Note that we are not calling the `.read_until` method here, but
	// rather our hardcoded implementation. For more details as to why, see
	// the comments in `default_read_to_string`.
	return appendToString(buf, func(bytes *[]byte) (int, error) {
		return DefaultReadUntil(r, '\n', bytes)
	})
}

// DefaultReadUntil is the body of BufRead::read_until.
func DefaultReadUntil(r BufRead, delim byte, buf *[]byte) (int, error) {
	read := 0
	for {
		available, err := r.FillBuf()
		if err != nil {
			if isInterruptedError(err) {
				continue
			}
			return 0, asError(err)
		}
		done := false
		if i := bytes.IndexByte(available, delim); i >= 0 {
			done = true
			available = available[:i+1]
		}
		*buf = append(*buf, available...)
		used := len(available)
		r.Consume(used)
		read += used
		if done || used == 0 {
			return read, nil
		}
	}
}

func defaultSkipUntil(r BufRead, delim byte) (int, error) {
	read := 0
	for {
		available, err := r.FillBuf()
		if err != nil {
			if KindOf(err) == ErrorKind_Interrupted {
				continue
			}
			return 0, asError(err)
		}
		done := false
		used := len(available)
		if i := bytes.IndexByte(available, delim); i >= 0 {
			done = true
			used = i + 1
		}
		r.Consume(used)
		read += used
		if done || used == 0 {
			return read, nil
		}
	}
}
