// Port of alloc 1.99.1 src/io/buffered/bufreader.rs and
// src/io/buffered/bufreader/buffer.rs

package rsio

import (
	"io"
	"strconv"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// BufReader is Rust's io::BufReader: a reader with a buffer in front of
// another reader. The default capacity is 8 KiB.
//
// It owns its reader: Drop drops it. Build it on ByRef(r), as Rust code
// writes `BufReader::new(&mut r)`, to keep r.
type BufReader[R io.Reader] struct {
	buf   bufReaderBuffer
	inner R
}

// BufReaderNew is BufReader::new.
func BufReaderNew[R io.Reader](inner R) *BufReader[R] {
	return BufReaderWithCapacity(DefaultBufSize, inner)
}

// BufReaderWithCapacity is BufReader::with_capacity.
func BufReaderWithCapacity[R io.Reader](capacity int, inner R) *BufReader[R] {
	return &BufReader[R]{inner: inner, buf: bufReaderBuffer{buf: make([]byte, capacity)}}
}

// Peek is BufReader::peek: the next n bytes without consuming them, or fewer
// at the end of input. n must not exceed the capacity.
func (b *BufReader[R]) Peek(n int) ([]byte, error) {
	if n > b.Capacity() {
		rs.PanicAt(1, "assertion failed: n <= self.capacity()")
	}
	for n > len(b.buf.buffer()) {
		if b.buf.pos > 0 {
			b.buf.backshift()
		}
		read, err := b.buf.readMore(b.inner)
		if err != nil {
			return nil, err
		}
		if read == 0 {
			// end of file, no more bytes to read
			return b.buf.buffer(), nil
		}
	}
	return b.buf.buffer()[:n], nil
}

// GetRef is BufReader::get_ref.
func (b *BufReader[R]) GetRef() R { return b.inner }

// GetMut is BufReader::get_mut. Reading from the result directly loses the
// bytes that are in the buffer.
func (b *BufReader[R]) GetMut() R { return b.inner }

// Buffer is BufReader::buffer: the bytes read from the reader and not yet
// consumed.
func (b *BufReader[R]) Buffer() []byte { return b.buf.buffer() }

// Capacity is BufReader::capacity.
func (b *BufReader[R]) Capacity() int { return len(b.buf.buf) }

// IntoInner is BufReader::into_inner. The bytes in the buffer are lost.
func (b *BufReader[R]) IntoInner() R { return b.inner }

// DiscardBuffer is BufReader::discard_buffer.
func (b *BufReader[R]) DiscardBuffer() { b.buf.discardBuffer() }

// Drop drops the reader.
func (b *BufReader[R]) Drop() { dropValue(b.inner) }

func (b *BufReader[R]) seeker() io.Seeker {
	seeker, ok := any(b.inner).(io.Seeker)
	if !ok {
		rs.PanicAt(2, "rsio: "+typeName(b.inner)+" is not seekable")
	}
	return seeker
}

// SeekRelative is BufReader::seek_relative: a seek from the position that
// stays in the buffer when it can. The reader must be seekable.
func (b *BufReader[R]) SeekRelative(offset int64) error {
	pos := uint64(b.buf.pos)
	if offset < 0 {
		if pos >= uint64(-offset) {
			b.buf.unconsume(int(-offset))
			return nil
		}
	} else if newPos := pos + uint64(offset); newPos >= pos {
		if newPos <= uint64(b.buf.filled) {
			b.buf.consume(int(offset))
			return nil
		}
	}

	_, err := b.SeekTo(&SeekFrom_Current{F0: offset})
	return err
}

// Read is Read::read.
func (b *BufReader[R]) Read(buf []byte) (int, error) {
	// If we don't have any buffered data and we're doing a massive read
	// (larger than our internal buffer), bypass our internal buffer
	// entirely.
	if b.buf.pos == b.buf.filled && len(buf) >= b.Capacity() {
		b.DiscardBuffer()
		return callRead(b.inner, buf)
	}
	rem, err := b.FillBuf()
	if err != nil {
		return 0, err
	}
	nread := copy(buf, rem)
	b.Consume(nread)
	return nread, nil
}

// ReadBuf is Read::read_buf.
func (b *BufReader[R]) ReadBuf(cursor *BorrowedCursor) error {
	// If we don't have any buffered data and we're doing a massive read
	// (larger than our internal buffer), bypass our internal buffer
	// entirely.
	if b.buf.pos == b.buf.filled && cursor.Capacity() >= b.Capacity() {
		b.DiscardBuffer()
		return ReadBuf(b.inner, cursor)
	}

	rem, err := b.FillBuf()
	if err != nil {
		return err
	}
	amount := min(cursor.Capacity(), len(rem))
	cursor.Append(rem[:amount])

	b.Consume(amount)

	return nil
}

// ReadExact is Read::read_exact.
//
// Small read_exacts from a BufReader are extremely common when used with a deserializer.
// The default implementation calls read in a loop, which results in surprisingly poor code
// generation for the common path where the buffer has enough bytes to fill the passed-in
// buffer.
func (b *BufReader[R]) ReadExact(buf []byte) error {
	if b.buf.consumeWith(len(buf), func(claimed []byte) { copy(buf, claimed) }) {
		return nil
	}

	return DefaultReadExact(b, buf)
}

// ReadBufExact is Read::read_buf_exact.
func (b *BufReader[R]) ReadBufExact(cursor *BorrowedCursor) error {
	if b.buf.consumeWith(cursor.Capacity(), func(claimed []byte) { cursor.Append(claimed) }) {
		return nil
	}

	return defaultReadBufExact(b, cursor)
}

// ReadToEnd is Read::read_to_end.
//
// The inner reader might have an optimized `read_to_end`. Drain our buffer and then
// delegate to the inner implementation.
func (b *BufReader[R]) ReadToEnd(buf *[]byte) (int, error) {
	innerBuf := b.Buffer()
	extendVec(buf, innerBuf)

	nread := len(innerBuf)
	b.DiscardBuffer()
	n, err := ReadToEnd(b.inner, buf)
	if err != nil {
		return 0, err
	}
	return nread + n, nil
}

// ReadToString is Read::read_to_string.
//
// The inner reader might have an optimized `read_to_end`. Drain our buffer and then
// delegate to the inner implementation.
func (b *BufReader[R]) ReadToString(buf *string) (int, error) {
	// In the general `else` case below we must read bytes into a side buffer, check
	// that they are valid UTF-8, and then append them to `buf`. This requires a
	// potentially large memcpy.
	//
	// If `buf` is empty--the most common case--we can leverage `append_to_string`
	// to read directly into `buf`'s internal byte buffer, saving an allocation and
	// a memcpy.
	if len(*buf) == 0 {
		return appendToString(buf, func(bytes *[]byte) (int, error) { return b.ReadToEnd(bytes) })
	}
	// We cannot append our byte buffer directly onto the `buf` String as there could
	// be an incomplete UTF-8 sequence that has only been partially read. We must read
	// everything into a side buffer first and then call `from_utf8` on the complete
	// buffer.
	var bytes []byte
	if _, err := b.ReadToEnd(&bytes); err != nil {
		return 0, err
	}
	if !utf8.Valid(bytes) {
		return 0, errInvalidUtf8
	}
	*buf += string(bytes)

	return len(bytes), nil
}

// FillBuf is BufRead::fill_buf.
func (b *BufReader[R]) FillBuf() ([]byte, error) { return b.buf.fillBuf(b.inner) }

// Consume is BufRead::consume.
func (b *BufReader[R]) Consume(amount int) { b.buf.consume(amount) }

// DebugString is `impl Debug for BufReader`.
func (b *BufReader[R]) DebugString() string {
	return "BufReader { reader: " + debugOfValue(b.inner) + ", buffer: " + b.bufferRatio() + " }"
}

func (b *BufReader[R]) bufferRatio() string {
	return strconv.Itoa(b.buf.filled-b.buf.pos) + "/" + strconv.Itoa(b.Capacity())
}

// FmtDebug is `impl Debug for BufReader`.
func (b *BufReader[R]) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("BufReader").
		Field("reader", b.inner).
		Field("buffer", rawDebug(b.bufferRatio())).
		Finish()
}

// SeekTo is Seek::seek. The reader must be seekable.
func (b *BufReader[R]) SeekTo(pos SeekFrom) (uint64, error) {
	var result uint64
	if current, ok := pos.(*SeekFrom_Current); ok {
		n := current.F0
		remainder := int64(b.buf.filled - b.buf.pos)
		// it should be safe to assume that remainder fits within an i64 as the alternative
		// means we managed to allocate 8 exbibytes and that's absurd.
		// But it's not out of the realm of possibility for some weird underlying reader to
		// support seeking by i64::MIN so we need to handle underflow when subtracting
		// remainder.
		if offset := n - remainder; offset <= n {
			var err error
			if result, err = Seek(b.seeker(), &SeekFrom_Current{F0: offset}); err != nil {
				return 0, err
			}
		} else {
			// seek backwards by our remainder, and then by the offset
			if _, err := Seek(b.seeker(), &SeekFrom_Current{F0: -remainder}); err != nil {
				return 0, err
			}
			b.DiscardBuffer()
			var err error
			if result, err = Seek(b.seeker(), &SeekFrom_Current{F0: n}); err != nil {
				return 0, err
			}
		}
	} else {
		// Seeking with Start/End doesn't care about our buffer length.
		var err error
		if result, err = Seek(b.seeker(), pos); err != nil {
			return 0, err
		}
	}
	b.DiscardBuffer()
	return result, nil
}

// Seek is Seek::seek through Go's signature.
func (b *BufReader[R]) Seek(offset int64, whence int) (int64, error) {
	pos, err := seekFromGo(offset, whence)
	if err != nil {
		return 0, err
	}
	position, err := b.SeekTo(pos)
	return int64(position), err
}

// StreamPosition is Seek::stream_position.
func (b *BufReader[R]) StreamPosition() (uint64, error) {
	remainder := uint64(b.buf.filled - b.buf.pos)
	pos, err := StreamPosition(b.seeker())
	if err != nil {
		return 0, err
	}
	if pos < remainder {
		rs.PanicAt(1, "overflow when subtracting remaining buffer size from inner stream position")
	}
	return pos - remainder, nil
}

// copyTo is BufferedReaderSpec::copy_to for BufReader.
func (b *BufReader[R]) copyTo(to io.Writer) (uint64, error) {
	var length uint64

	for {
		// Hack: this relies on `impl Read for BufReader` always calling fill_buf
		// if the buffer is empty, even for empty slices.
		if _, err := b.Read(nil); err != nil {
			if isInterruptedError(err) {
				continue
			}
			return 0, err
		}
		buf := b.Buffer()
		if len(buf) == 0 {
			return length, nil
		}

		// In case the writer side is a BufWriter then its write_all
		// implements an optimization that passes through large
		// buffers to the underlying writer. That code path is #[cold]
		// but we're still avoiding redundant memcopies when doing
		// a copy between buffered inputs and outputs.
		if err := WriteAll(to, buf); err != nil {
			return 0, err
		}
		length += uint64(len(buf))
		b.DiscardBuffer()
	}
}

func (b *BufReader[R]) readBufferPriority() int { return b.Capacity() }

// bufReaderBuffer is bufreader::Buffer.
type bufReaderBuffer struct {
	// The buffer.
	buf []byte
	// The current seek offset into `buf`, must always be <= `filled`.
	pos int
	// Each call to `fill_buf` sets `filled` to indicate how many bytes at the start of `buf` are
	// initialized with bytes from a read.
	filled int
	// Whether `buf` has been fully initialized. We track this so that we can accurately tell
	// `read_buf` how many bytes of buf are initialized, to bypass as much of its defensive
	// initialization as possible. Calls to `fill_buf` are not required to actually fill the buffer,
	// and omitting this is a huge perf regression for `Read` impls that do not.
	initialized bool
}

func (b *bufReaderBuffer) buffer() []byte { return b.buf[b.pos:b.filled] }

func (b *bufReaderBuffer) discardBuffer() {
	b.pos = 0
	b.filled = 0
}

func (b *bufReaderBuffer) consume(amount int) {
	b.pos = min(b.pos+amount, b.filled)
}

func (b *bufReaderBuffer) consumeWith(amount int, visitor func(claimed []byte)) bool {
	if amount > b.filled-b.pos {
		return false
	}
	visitor(b.buf[b.pos : b.pos+amount])
	// If the indexing into self.buffer() succeeds, amt must be a valid increment.
	b.pos += amount
	return true
}

func (b *bufReaderBuffer) unconsume(amount int) {
	b.pos = max(b.pos-amount, 0)
}

func (b *bufReaderBuffer) readMore(reader io.Reader) (int, error) {
	buf := BorrowedBufFromUninit(b.buf[b.filled:])

	if b.initialized {
		buf.SetInit()
	}

	err := ReadBuf(reader, buf.Unfilled())
	if err != nil && buf.Len() == 0 {
		return 0, err
	}
	b.filled += buf.Len()
	b.initialized = buf.IsInit()
	return buf.Len(), err
}

func (b *bufReaderBuffer) backshift() {
	copy(b.buf, b.buf[b.pos:b.filled])
	b.filled -= b.pos
	b.pos = 0
}

func (b *bufReaderBuffer) fillBuf(reader io.Reader) ([]byte, error) {
	// If we've reached the end of our internal buffer then we need to fetch
	// some more data from the reader.
	// Branch using `>=` instead of the more correct `==`
	// to tell the compiler that the pos..cap slice is always valid.
	if b.pos >= b.filled {
		buf := BorrowedBufFromUninit(b.buf)

		if b.initialized {
			buf.SetInit()
		}

		result := ReadBuf(reader, buf.Unfilled())

		b.pos = 0
		b.filled = buf.Len()
		b.initialized = buf.IsInit()

		if result != nil {
			return nil, result
		}
	}
	return b.buffer(), nil
}

// rawDebug is a Debug value that writes its text as it is, like
// `format_args!` given to a debug builder.
type rawDebug string

func (r rawDebug) FmtDebug(f *rs.Formatter) error { return f.WriteStr(string(r)) }

// debugOfValue is the Debug text of a value another type holds.
func debugOfValue(v any) string {
	if value, ok := v.(interface{ DebugString() string }); ok {
		return value.DebugString()
	}
	return rs.DebugString(v)
}
