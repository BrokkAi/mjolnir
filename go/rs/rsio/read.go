// Port of alloc 1.99.1 src/io/read.rs

package rsio

import (
	"io"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Rust's Read trait has no Go type of its own: a reader is an io.Reader, and
// the trait's methods are the functions in this file, which take one.
//
// The contract of Read differs between the two languages, and the functions
// here accept both, so that they work on the types of this package, on
// readers written in product code, and on Go's own readers alike:
//
//   - Rust's read returns Ok(0) at the end of input. The readers of the port
//     (this package, rsfs.File, and every `impl Read` translated from Rust)
//     do the same: Read returns (0, nil). A Go reader returns io.EOF there.
//     Both mean the end of input here.
//   - Rust's read fails with an io::Error. Here every error comes back as an
//     *Error; an error of another type is converted with From.
//
// A reader of the port must not be handed to a Go library that expects Go's
// contract (io.Copy, bufio, a decoder): at the end of input it would be
// asked again forever. Wrap it with GoReader for that.

// DefaultBufSize is io::DEFAULT_BUF_SIZE.
const DefaultBufSize = 8 * 1024

// callRead makes one Read call and gives its result the way Rust's read
// reports it: io.EOF is not an error. A Go reader may return bytes together
// with an error; then both are returned, and the caller must use the bytes
// before it fails.
func callRead(r io.Reader, buf []byte) (int, error) {
	n, err := r.Read(buf)
	if err == nil || err == io.EOF {
		return n, nil
	}
	return n, From(err)
}

// asError gives an error returned by a method of another type as an *Error,
// and keeps nil a nil interface.
func asError(err error) error {
	if err == nil {
		return nil
	}
	return From(err)
}

// isInterruptedError is `e.is_interrupted()` on an error held as a Go error.
func isInterruptedError(err error) bool {
	return From(err).IsInterrupted()
}

// Read is `r.read(buf)` with Rust's contract: it returns the number of bytes
// read, which is 0 at the end of input (and for an empty buf), or an *Error.
func Read(r io.Reader, buf []byte) (int, error) {
	return callRead(r, buf)
}

// ReadBuf is `r.read_buf(cursor)`: the read_buf protocol of the standard
// library, by which a reader fills a buffer it was lent and says whether it
// initialized the rest. A reader whose Rust type overrides read_buf has a
// method `ReadBuf(*BorrowedCursor) error`; every other reader gets the
// default, which is one call of Read.
func ReadBuf(r io.Reader, cursor *BorrowedCursor) error {
	if reader, ok := deref(r).(interface {
		ReadBuf(cursor *BorrowedCursor) error
	}); ok {
		return asError(reader.ReadBuf(cursor))
	}
	return DefaultReadBuf(func(buf []byte) (int, error) { return callRead(r, buf) }, cursor)
}

// DefaultReadBuf is io::default_read_buf.
func DefaultReadBuf(read func(buf []byte) (int, error), cursor *BorrowedCursor) error {
	n, err := read(cursor.EnsureInit())
	// A Go reader can return bytes with its error. They are kept, as Rust
	// keeps what a read_buf wrote before it failed.
	cursor.AdvanceChecked(n)
	return err
}

// ReadToEnd is `r.read_to_end(buf)`: it reads until the end of input,
// appending to *buf, and returns the number of bytes read. Bytes read before
// an error stay in *buf.
func ReadToEnd(r io.Reader, buf *[]byte) (int, error) {
	if reader, ok := deref(r).(interface {
		ReadToEnd(buf *[]byte) (int, error)
	}); ok {
		n, err := reader.ReadToEnd(buf)
		return n, asError(err)
	}
	return DefaultReadToEnd(r, buf, rs.None[int]())
}

// ReadToString is `r.read_to_string(buf)`: it reads until the end of input
// and appends to *buf. When the bytes read are not UTF-8 it fails with
// InvalidData, "stream did not contain valid UTF-8", and leaves *buf as it
// was.
func ReadToString(r io.Reader, buf *string) (int, error) {
	if reader, ok := deref(r).(interface {
		ReadToString(buf *string) (int, error)
	}); ok {
		n, err := reader.ReadToString(buf)
		return n, asError(err)
	}
	return DefaultReadToString(r, buf, rs.None[int]())
}

// ReadExact is `r.read_exact(buf)`: it fills buf, or fails with
// UnexpectedEof, "failed to fill whole buffer".
func ReadExact(r io.Reader, buf []byte) error {
	if reader, ok := deref(r).(interface {
		ReadExact(buf []byte) error
	}); ok {
		return asError(reader.ReadExact(buf))
	}
	return DefaultReadExact(r, buf)
}

// ReadBufExact is `r.read_buf_exact(cursor)`.
func ReadBufExact(r io.Reader, cursor *BorrowedCursor) error {
	if reader, ok := deref(r).(interface {
		ReadBufExact(cursor *BorrowedCursor) error
	}); ok {
		return asError(reader.ReadBufExact(cursor))
	}
	return defaultReadBufExact(r, cursor)
}

// ReadToStringOwned is the function `std::io::read_to_string(reader)`: the
// whole input as a string. (The trait method is ReadToString.)
func ReadToStringOwned(reader io.Reader) (string, error) {
	var buf string
	if _, err := ReadToString(reader, &buf); err != nil {
		return "", err
	}
	return buf, nil
}

// appendToString is io::append_to_string: f appends bytes to a buffer, and
// they are added to *buf only if they are UTF-8.
func appendToString(buf *string, f func(bytes *[]byte) (int, error)) (int, error) {
	var appended []byte
	n, err := f(&appended)
	if !utf8.Valid(appended) {
		if err != nil {
			return 0, err
		}
		return 0, errInvalidUtf8
	}
	*buf += string(appended)
	return n, err
}

// reserveVec is Vec::reserve for a byte slice: when the spare capacity is
// less than additional, the slice moves to one whose capacity grows as a
// Vec's does (doubling, at least 8). read_to_end sizes its reads by the
// capacity, so it must grow the same way.
func reserveVec(buf *[]byte, additional int) {
	if cap(*buf)-len(*buf) >= additional {
		return
	}
	required := len(*buf) + additional
	newCap := max(cap(*buf)*2, required, 8)
	grown := make([]byte, len(*buf), newCap)
	copy(grown, *buf)
	*buf = grown
}

// DefaultReadToEnd is io::default_read_to_end, the body of read_to_end.
// sizeHint is the number of bytes the caller expects, when it knows.
func DefaultReadToEnd(r io.Reader, buf *[]byte, sizeHint rs.Option[int]) (int, error) {
	startLen := len(*buf)
	startCap := cap(*buf)
	// Optionally limit the maximum bytes read on each iteration.
	// This adds an arbitrary fiddle factor to allow for more data than we expect.
	maxReadSize := DefaultBufSize
	if hint, ok := sizeHint.Get(); ok {
		if sum, ok := checkedAdd(hint, 1024); ok {
			if rounded, ok := checkedNextMultipleOf(sum, DefaultBufSize); ok {
				maxReadSize = rounded
			}
		}
	}

	// Tracks how many bytes are initialized in the buffer
	initUntil := len(*buf)

	const probeSize = 32

	smallProbeRead := func() (int, error) {
		var probe [probeSize]byte

		for {
			n, err := callRead(r, probe[:])
			if err != nil && n == 0 {
				if isInterruptedError(err) {
					continue
				}
				return 0, err
			}
			// there is no way to recover from allocation failure here
			// because the data has already been read.
			*buf = append(*buf, probe[:n]...)
			return n, err
		}
	}

	// avoid inflating empty/small vecs before we have determined that there's anything to read
	if hint, ok := sizeHint.Get(); (!ok || hint == 0) && cap(*buf)-len(*buf) < probeSize {
		read, err := smallProbeRead()
		if err != nil {
			return 0, err
		}

		if read == 0 {
			return 0, nil
		}
	}

	for {
		if cap(*buf)-len(*buf) < probeSize && cap(*buf) == startCap {
			// The buffer might be an exact fit. Let's read into a probe buffer
			// and see if it returns `Ok(0)`. If so, we've avoided an
			// unnecessary doubling of the capacity. But if not, append the
			// probe buffer to the primary buffer and let its capacity grow.
			read, err := smallProbeRead()
			if err != nil {
				return 0, err
			}

			if read == 0 {
				return len(*buf) - startLen, nil
			}

			initUntil = len(*buf)
			// In the case of very short reads, continue to use the stack buffer
			// until either we reach the end or we need to reallocate.
			continue
		}

		// Avoid unnecessarily short reads by ensuring there's at least PROBE_SIZE space available.
		if cap(*buf)-len(*buf) < probeSize {
			reserveVec(buf, probeSize)
			// When reallocation occurs, we have to update init_until accordingly
			// to re-calibrate how many bytes are actually initialized in the buffer
			initUntil = len(*buf)
		}

		// We set a threshold of >PROBE_SIZE initialized yet unfilled bytes left in the
		// spare buffer before determining that we need to initialize more bytes into
		// the spare buffer
		var bufLen int
		if initUntil > len(*buf)+probeSize {
			bufLen = initUntil - len(*buf)
		} else {
			bufLen = min(maxReadSize, cap(*buf)-len(*buf))
		}
		wasInit := initUntil >= len(*buf)+bufLen

		spare := (*buf)[len(*buf) : len(*buf)+bufLen]
		readBuf := BorrowedBufFromUninit(spare)

		if wasInit {
			// These bytes were initialized but not filled in the previous loop
			readBuf.SetInit()
		}

		cursor := readBuf.Unfilled()
		var result error
		for {
			result = ReadBuf(r, cursor.Reborrow())
			if result != nil && isInterruptedError(result) {
				continue
			}
			// Do not stop now in case of error: we might have received both data
			// and an error
			break
		}

		bytesRead := cursor.Written()
		isInit := readBuf.IsInit()

		if isInit {
			initUntil = len(*buf) + bufLen
		}

		*buf = (*buf)[:len(*buf)+bytesRead]

		// Now that all data is pushed to the vector, we can fail without data loss
		if result != nil {
			return 0, result
		}

		if bytesRead == 0 {
			return len(*buf) - startLen, nil
		}

		// Use heuristics to determine the max read size if no initial size hint was provided
		if sizeHint.IsNone() {
			// The reader is returning short reads but it doesn't call ensure_init().
			// In that case we no longer need to restrict read sizes to avoid
			// initialization costs.
			// When reading from disk we usually don't get any short reads except at EOF.
			// So we wait for at least 2 short reads before uncapping the read buffer;
			// this helps with the Windows issue.
			if !isInit {
				maxReadSize = rs.UsizeMax
			} else if bytesRead == maxReadSize {
				// the spare buffer has initialized and read in `max_read_size` bytes.
				// it's possible that we have more than `max_read_size` bytes to read
				// left, so a larger buffer may be necessary to minimize the number of
				// iterations of reading in bytes to the buffer
				maxReadSize = saturatingMul(maxReadSize, 2)
			}
		}
	}
}

// checkedNextMultipleOf is usize::checked_next_multiple_of.
func checkedNextMultipleOf(value, multiple int) (int, bool) {
	remainder := value % multiple
	if remainder == 0 {
		return value, true
	}
	return checkedAdd(value, multiple-remainder)
}

// checkedAdd is usize::checked_add for the non-negative ints that stand for
// usize.
func checkedAdd(a, b int) (int, bool) {
	sum := a + b
	if sum < a {
		return 0, false
	}
	return sum, true
}

// saturatingMul is usize::saturating_mul.
func saturatingMul(a, b int) int {
	if a != 0 && b > rs.UsizeMax/a {
		return rs.UsizeMax
	}
	return a * b
}

// DefaultReadToString is io::default_read_to_string, the body of
// read_to_string.
func DefaultReadToString(r io.Reader, buf *string, sizeHint rs.Option[int]) (int, error) {
	// Note that we do *not* call `r.read_to_end()` here. We are passing
	// `&mut Vec<u8>` (the raw contents of `buf`) into the `read_to_end`
	// method to fill it up. An arbitrary implementation could overwrite the
	// entire contents of the vector, not just append to it (which is what
	// we are expecting).
	//
	// To prevent extraneously checking the UTF-8-ness of the entire buffer
	// we pass it to our hardcoded `default_read_to_end` implementation which
	// we know is guaranteed to only read data into the end of the buffer.
	return appendToString(buf, func(bytes *[]byte) (int, error) {
		return DefaultReadToEnd(r, bytes, sizeHint)
	})
}

// DefaultReadExact is io::default_read_exact, the body of read_exact.
func DefaultReadExact(r io.Reader, buf []byte) error {
	for len(buf) != 0 {
		n, err := callRead(r, buf)
		if err != nil {
			if n == 0 && isInterruptedError(err) {
				continue
			}
			return err
		}
		if n == 0 {
			break
		}
		buf = buf[n:]
	}
	if len(buf) != 0 {
		return errReadExactEof
	}
	return nil
}

func defaultReadBufExact(r io.Reader, cursor *BorrowedCursor) error {
	for cursor.Capacity() > 0 {
		prevWritten := cursor.Written()
		if err := ReadBuf(r, cursor.Reborrow()); err != nil {
			if isInterruptedError(err) {
				continue
			}
			return err
		}

		if cursor.Written() == prevWritten {
			return errReadExactEof
		}
	}

	return nil
}

// Ref is `&mut r` used as a reader or a writer, and the result of
// `r.by_ref()`: it reads, writes and seeks through V, and it has no Drop, so
// that an adapter built on it (a BufReader, a Take, a BufWriter) does not
// drop V when it is dropped. An adapter built on the value itself owns it,
// as in Rust.
type Ref[T any] struct {
	V T
}

// ByRef is `r.by_ref()` and `&mut r`: see Ref.
func ByRef[T any](v T) *Ref[T] { return &Ref[T]{V: v} }

// deref gives the value a Ref refers to, so that the functions of this
// package find the methods a Rust type overrides behind a `&mut`.
func deref(v any) any {
	for {
		ref, ok := v.(interface{ rsioDeref() any })
		if !ok {
			return v
		}
		v = ref.rsioDeref()
	}
}

func (r *Ref[T]) rsioDeref() any { return r.V }

// Read reads from V. V must be a reader.
func (r *Ref[T]) Read(buf []byte) (int, error) {
	reader, ok := any(r.V).(io.Reader)
	if !ok {
		rs.PanicAt(1, "rsio.Ref: "+typeName(r.V)+" is not a reader")
	}
	return callRead(reader, buf)
}

// Write writes to V. V must be a writer.
func (r *Ref[T]) Write(buf []byte) (int, error) {
	writer, ok := any(r.V).(io.Writer)
	if !ok {
		rs.PanicAt(1, "rsio.Ref: "+typeName(r.V)+" is not a writer")
	}
	return callWrite(writer, buf)
}

// Flush flushes V.
func (r *Ref[T]) Flush() error {
	writer, ok := any(r.V).(io.Writer)
	if !ok {
		rs.PanicAt(1, "rsio.Ref: "+typeName(r.V)+" is not a writer")
	}
	return Flush(writer)
}

// Seek seeks V. V must be seekable.
func (r *Ref[T]) Seek(offset int64, whence int) (int64, error) {
	seeker, ok := any(r.V).(io.Seeker)
	if !ok {
		rs.PanicAt(1, "rsio.Ref: "+typeName(r.V)+" is not seekable")
	}
	position, err := seeker.Seek(offset, whence)
	return position, asError(err)
}

// FillBuf is BufRead::fill_buf on V. V must be a BufRead.
func (r *Ref[T]) FillBuf() ([]byte, error) {
	reader, ok := any(r.V).(BufRead)
	if !ok {
		rs.PanicAt(1, "rsio.Ref: "+typeName(r.V)+" is not a BufRead")
	}
	buf, err := reader.FillBuf()
	return buf, asError(err)
}

// Consume is BufRead::consume on V. V must be a BufRead.
func (r *Ref[T]) Consume(amount int) {
	reader, ok := any(r.V).(BufRead)
	if !ok {
		rs.PanicAt(1, "rsio.Ref: "+typeName(r.V)+" is not a BufRead")
	}
	reader.Consume(amount)
}

// GoReader gives a reader of the port to a Go library: the result follows
// Go's contract, returning io.EOF at the end of input. A reader that already
// follows Go's contract passes through unchanged in behaviour.
func GoReader(r io.Reader) io.Reader { return &goReader{r: r} }

type goReader struct {
	r   io.Reader
	eof bool
}

func (g *goReader) Read(buf []byte) (int, error) {
	if g.eof {
		return 0, io.EOF
	}
	if len(buf) == 0 {
		return 0, nil
	}
	for {
		n, err := g.r.Read(buf)
		switch {
		case err == io.EOF:
			g.eof = true
			return n, io.EOF
		case err != nil:
			if n == 0 && isInterruptedError(err) {
				continue
			}
			return n, err
		case n == 0:
			g.eof = true
			return 0, io.EOF
		default:
			return n, nil
		}
	}
}
