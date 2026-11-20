// Port of core 1.99.1 src/io/util.rs and alloc 1.99.1 src/io/util.rs

package rsio

import (
	"io"
	"iter"
	"math"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Rust names several functions and the types they return alike but for the
// case: io::empty() returns io::Empty, r.take(n) returns io::Take. Go cannot
// have both under one name. The function keeps the name, because it is what
// product code writes, and the type has a noun added: Empty and EmptyStream,
// Sink and SinkWriter, Repeat and RepeatReader, Take and TakeReader, Chain
// and ChainReader, Bytes and BytesIter, Lines and LinesIter, Split and
// SplitIter, Stdin and StdinHandle.

// dropValue drops a value an adapter owns, when it is a value that must be
// dropped.
func dropValue(v any) {
	if droppable, ok := v.(interface{ Drop() }); ok {
		droppable.Drop()
	}
}

// EmptyStream is io::Empty: a reader that is always at its end and a writer
// that discards.
type EmptyStream struct{}

// Empty is io::empty.
func Empty() EmptyStream { return EmptyStream{} }

var _ = rs.RegisterUnitStruct[EmptyStream]("Empty")

// Read is Read::read: always the end of input.
func (EmptyStream) Read(buf []byte) (int, error) { return 0, nil }

// ReadBuf is Read::read_buf.
func (EmptyStream) ReadBuf(cursor *BorrowedCursor) error { return nil }

// ReadExact is Read::read_exact.
func (EmptyStream) ReadExact(buf []byte) error {
	if len(buf) != 0 {
		return errReadExactEof
	}
	return nil
}

// ReadBufExact is Read::read_buf_exact.
func (EmptyStream) ReadBufExact(cursor *BorrowedCursor) error {
	if cursor.Capacity() != 0 {
		return errReadExactEof
	}
	return nil
}

// ReadToEnd is Read::read_to_end.
func (EmptyStream) ReadToEnd(buf *[]byte) (int, error) { return 0, nil }

// ReadToString is Read::read_to_string.
func (EmptyStream) ReadToString(buf *string) (int, error) { return 0, nil }

// FillBuf is BufRead::fill_buf.
func (EmptyStream) FillBuf() ([]byte, error) { return nil, nil }

// Consume is BufRead::consume.
func (EmptyStream) Consume(amount int) {}

// HasDataLeft is BufRead::has_data_left.
func (EmptyStream) HasDataLeft() (bool, error) { return false, nil }

// ReadUntil is BufRead::read_until.
func (EmptyStream) ReadUntil(delim byte, buf *[]byte) (int, error) { return 0, nil }

// SkipUntil is BufRead::skip_until.
func (EmptyStream) SkipUntil(delim byte) (int, error) { return 0, nil }

// ReadLine is BufRead::read_line.
func (EmptyStream) ReadLine(buf *string) (int, error) { return 0, nil }

// Write is Write::write: everything is accepted and discarded.
func (EmptyStream) Write(buf []byte) (int, error) { return len(buf), nil }

// WriteAll is Write::write_all.
func (EmptyStream) WriteAll(buf []byte) error { return nil }

// Flush is Write::flush.
func (EmptyStream) Flush() error { return nil }

// Seek is Seek::seek through Go's signature: the position is always 0.
func (EmptyStream) Seek(offset int64, whence int) (int64, error) { return 0, nil }

// SeekTo is Seek::seek.
func (EmptyStream) SeekTo(pos SeekFrom) (uint64, error) { return 0, nil }

// StreamLen is Seek::stream_len.
func (EmptyStream) StreamLen() (uint64, error) { return 0, nil }

// StreamPosition is Seek::stream_position.
func (EmptyStream) StreamPosition() (uint64, error) { return 0, nil }

// RepeatReader is io::Repeat: a reader that yields one byte forever.
type RepeatReader struct {
	Byte byte `rs:"byte"`
}

// Repeat is io::repeat.
func Repeat(b byte) *RepeatReader { return &RepeatReader{Byte: b} }

// Read is Read::read.
func (r *RepeatReader) Read(buf []byte) (int, error) {
	fillBytes(buf, r.Byte)
	return len(buf), nil
}

// ReadExact is Read::read_exact.
func (r *RepeatReader) ReadExact(buf []byte) error {
	fillBytes(buf, r.Byte)
	return nil
}

// ReadBuf is Read::read_buf.
func (r *RepeatReader) ReadBuf(cursor *BorrowedCursor) error {
	fillBytes(cursor.AsMut(), r.Byte)
	cursor.Advance(cursor.Capacity())
	return nil
}

// ReadBufExact is Read::read_buf_exact.
func (r *RepeatReader) ReadBufExact(cursor *BorrowedCursor) error { return r.ReadBuf(cursor) }

// ReadToEnd is Read::read_to_end, which cannot succeed.
func (r *RepeatReader) ReadToEnd(buf *[]byte) (int, error) {
	return 0, ErrorFrom(ErrorKind_OutOfMemory)
}

// ReadToString is Read::read_to_string, which cannot succeed.
func (r *RepeatReader) ReadToString(buf *string) (int, error) {
	return 0, ErrorFrom(ErrorKind_OutOfMemory)
}

// DebugString is `impl Debug for Repeat`.
func (r *RepeatReader) DebugString() string { return "Repeat { .. }" }

// FmtDebug is `impl Debug for Repeat`.
func (r *RepeatReader) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Repeat").FinishNonExhaustive()
}

func fillBytes(buf []byte, b byte) {
	for i := range buf {
		buf[i] = b
	}
}

// SinkWriter is io::Sink: a writer that discards.
type SinkWriter struct{}

// Sink is io::sink.
func Sink() SinkWriter { return SinkWriter{} }

var _ = rs.RegisterUnitStruct[SinkWriter]("Sink")

// Write is Write::write.
func (SinkWriter) Write(buf []byte) (int, error) { return len(buf), nil }

// WriteAll is Write::write_all.
func (SinkWriter) WriteAll(buf []byte) error { return nil }

// Flush is Write::flush.
func (SinkWriter) Flush() error { return nil }

// ChainReader is io::Chain: one reader, then another.
//
// It owns both: Drop drops them. Build it on ByRef(r) to keep r.
type ChainReader[T io.Reader, U io.Reader] struct {
	first     T
	second    U
	doneFirst bool
}

// Chain is `first.chain(second)`.
func Chain[T io.Reader, U io.Reader](first T, second U) *ChainReader[T, U] {
	return &ChainReader[T, U]{first: first, second: second, doneFirst: false}
}

// IntoInner is Chain::into_inner.
func (c *ChainReader[T, U]) IntoInner() (T, U) { return c.first, c.second }

// GetRef is Chain::get_ref.
func (c *ChainReader[T, U]) GetRef() (T, U) { return c.first, c.second }

// GetMut is Chain::get_mut.
func (c *ChainReader[T, U]) GetMut() (T, U) { return c.first, c.second }

// Drop drops the two readers.
func (c *ChainReader[T, U]) Drop() {
	dropValue(c.first)
	dropValue(c.second)
}

// Read is Read::read.
func (c *ChainReader[T, U]) Read(buf []byte) (int, error) {
	if !c.doneFirst {
		n, err := callRead(c.first, buf)
		if err != nil {
			return n, err
		}
		if n == 0 && len(buf) != 0 {
			c.doneFirst = true
		} else {
			return n, nil
		}
	}
	return callRead(c.second, buf)
}

// ReadToEnd is Read::read_to_end.
func (c *ChainReader[T, U]) ReadToEnd(buf *[]byte) (int, error) {
	read := 0
	if !c.doneFirst {
		n, err := ReadToEnd(c.first, buf)
		if err != nil {
			return 0, err
		}
		read += n
		c.doneFirst = true
	}
	n, err := ReadToEnd(c.second, buf)
	if err != nil {
		return 0, err
	}
	read += n
	return read, nil
}

// We don't override `read_to_string` here because an UTF-8 sequence could
// be split between the two parts of the chain

// ReadBuf is Read::read_buf.
func (c *ChainReader[T, U]) ReadBuf(buf *BorrowedCursor) error {
	if buf.Capacity() == 0 {
		return nil
	}

	if !c.doneFirst {
		oldLen := buf.Written()
		if err := ReadBuf(c.first, buf.Reborrow()); err != nil {
			return err
		}

		if buf.Written() != oldLen {
			return nil
		}
		c.doneFirst = true
	}
	return ReadBuf(c.second, buf)
}

func asBufRead(v any) BufRead {
	reader, ok := v.(BufRead)
	if !ok {
		rs.PanicAt(2, "rsio: "+typeName(v)+" is not a BufRead")
	}
	return reader
}

// FillBuf is BufRead::fill_buf. Both readers must be BufRead.
func (c *ChainReader[T, U]) FillBuf() ([]byte, error) {
	if !c.doneFirst {
		buf, err := asBufRead(c.first).FillBuf()
		if err != nil {
			return nil, asError(err)
		}
		if len(buf) == 0 {
			c.doneFirst = true
		} else {
			return buf, nil
		}
	}
	buf, err := asBufRead(c.second).FillBuf()
	return buf, asError(err)
}

// Consume is BufRead::consume.
func (c *ChainReader[T, U]) Consume(amount int) {
	if !c.doneFirst {
		asBufRead(c.first).Consume(amount)
	} else {
		asBufRead(c.second).Consume(amount)
	}
}

// ReadUntil is BufRead::read_until.
func (c *ChainReader[T, U]) ReadUntil(delim byte, buf *[]byte) (int, error) {
	read := 0
	if !c.doneFirst {
		n, err := ReadUntil(asBufRead(c.first), delim, buf)
		if err != nil {
			return 0, err
		}
		read += n

		if len(*buf) != 0 && (*buf)[len(*buf)-1] == delim && n != 0 {
			return read, nil
		}
		c.doneFirst = true
	}
	n, err := ReadUntil(asBufRead(c.second), delim, buf)
	if err != nil {
		return 0, err
	}
	read += n
	return read, nil
}

// We don't override `read_line` here because an UTF-8 sequence could be
// split between the two parts of the chain

// TakeReader is io::Take: a reader limited to a number of bytes.
//
// It owns its reader: Drop drops it. Build it on ByRef(r), as Rust code
// writes `r.by_ref().take(n)`, to keep r.
type TakeReader[T io.Reader] struct {
	inner T
	len   uint64
	limit uint64
}

// Take is `r.take(limit)`.
func Take[T io.Reader](inner T, limit uint64) *TakeReader[T] {
	return &TakeReader[T]{inner: inner, limit: limit, len: limit}
}

// Limit is Take::limit: the bytes that can still be read.
func (t *TakeReader[T]) Limit() uint64 { return t.limit }

// Position is Take::position.
func (t *TakeReader[T]) Position() uint64 { return t.len - t.limit }

// SetLimit is Take::set_limit.
func (t *TakeReader[T]) SetLimit(limit uint64) {
	t.len = limit
	t.limit = limit
}

// IntoInner is Take::into_inner.
func (t *TakeReader[T]) IntoInner() T { return t.inner }

// GetRef is Take::get_ref.
func (t *TakeReader[T]) GetRef() T { return t.inner }

// GetMut is Take::get_mut.
func (t *TakeReader[T]) GetMut() T { return t.inner }

// Drop drops the reader.
func (t *TakeReader[T]) Drop() { dropValue(t.inner) }

// Read is Read::read.
func (t *TakeReader[T]) Read(buf []byte) (int, error) {
	// Don't call into inner reader at all at EOF because it may still block
	if t.limit == 0 {
		return 0, nil
	}

	maxLen := int(min(uint64(len(buf)), t.limit))
	n, err := callRead(t.inner, buf[:maxLen])
	if uint64(n) > t.limit {
		rs.PanicAt(1, "number of read bytes exceeds limit")
	}
	t.limit -= uint64(n)
	return n, err
}

// ReadBuf is Read::read_buf.
func (t *TakeReader[T]) ReadBuf(buf *BorrowedCursor) error {
	// Don't call into inner reader at all at EOF because it may still block
	if t.limit == 0 {
		return nil
	}

	if t.limit < uint64(buf.Capacity()) {
		// The condition above guarantees that `self.limit` fits in `usize`.
		limit := int(t.limit)

		isInit := buf.IsInit()

		slicedBuf := BorrowedBufFromUninit(buf.AsMut()[:limit])

		if isInit {
			// `sliced_buf` is a subslice of `buf`, so if `buf` was initialized then
			// `sliced_buf` is.
			slicedBuf.SetInit()
		}

		result := ReadBuf(t.inner, slicedBuf.Unfilled())

		didInitUpToLimit := slicedBuf.IsInit()
		filled := slicedBuf.Len()

		// Avoid accidentally quadratic behaviour by initializing the whole
		// cursor if only part of it was initialized.
		if didInitUpToLimit && !isInit {
			clear(buf.AsMut()[limit:])
			buf.SetInit()
		}

		buf.Advance(filled)

		t.limit -= uint64(filled)

		return result
	}
	written := buf.Written()
	result := ReadBuf(t.inner, buf.Reborrow())
	t.limit -= uint64(buf.Written() - written)
	return result
}

// FillBuf is BufRead::fill_buf. The reader must be a BufRead.
func (t *TakeReader[T]) FillBuf() ([]byte, error) {
	// Don't call into inner reader at all at EOF because it may still block
	if t.limit == 0 {
		return nil, nil
	}

	buf, err := asBufRead(t.inner).FillBuf()
	if err != nil {
		return nil, asError(err)
	}
	available := int(min(uint64(len(buf)), t.limit))
	return buf[:available], nil
}

// Consume is BufRead::consume.
func (t *TakeReader[T]) Consume(amount int) {
	// Don't let callers reset the limit by passing an overlarge value
	amount = int(min(uint64(amount), t.limit))
	t.limit -= uint64(amount)
	asBufRead(t.inner).Consume(amount)
}

func (t *TakeReader[T]) seeker() io.Seeker {
	seeker, ok := any(t.inner).(io.Seeker)
	if !ok {
		rs.PanicAt(2, "rsio: "+typeName(t.inner)+" is not seekable")
	}
	return seeker
}

// SeekTo is Seek::seek. The reader must be seekable.
func (t *TakeReader[T]) SeekTo(pos SeekFrom) (uint64, error) {
	var (
		newPosition uint64
		ok          bool
	)
	switch pos := pos.(type) {
	case *SeekFrom_Start:
		newPosition, ok = pos.F0, true
	case *SeekFrom_Current:
		newPosition, ok = checkedAddSigned(t.Position(), pos.F0)
	case *SeekFrom_End:
		newPosition, ok = checkedAddSigned(t.len, pos.F0)
	default:
		panic(rs.Unreachable(pos))
	}
	if !ok || newPosition > t.len {
		return 0, ErrorFrom(ErrorKind_InvalidInput)
	}
	for newPosition != t.Position() {
		if offset, ok := checkedSignedDiff(newPosition, t.Position()); ok {
			if err := SeekRelative(t.seeker(), offset); err != nil {
				return 0, err
			}
			t.limit -= uint64(offset)
			break
		}
		var offset int64 = math.MinInt64
		if newPosition > t.Position() {
			offset = math.MaxInt64
		}
		if err := SeekRelative(t.seeker(), offset); err != nil {
			return 0, err
		}
		t.limit -= uint64(offset)
	}
	return newPosition, nil
}

// Seek is Seek::seek through Go's signature.
func (t *TakeReader[T]) Seek(offset int64, whence int) (int64, error) {
	pos, err := seekFromGo(offset, whence)
	if err != nil {
		return 0, err
	}
	position, err := t.SeekTo(pos)
	return int64(position), err
}

// StreamLen is Seek::stream_len.
func (t *TakeReader[T]) StreamLen() (uint64, error) { return t.len, nil }

// StreamPosition is Seek::stream_position.
func (t *TakeReader[T]) StreamPosition() (uint64, error) { return t.Position(), nil }

// SeekRelative is Seek::seek_relative.
func (t *TakeReader[T]) SeekRelative(offset int64) error {
	if position, ok := checkedAddSigned(t.Position(), offset); !ok || position > t.len {
		return ErrorFrom(ErrorKind_InvalidInput)
	}
	if err := SeekRelative(t.seeker(), offset); err != nil {
		return err
	}
	t.limit -= uint64(offset)
	return nil
}

// checkedAddSigned is u64::checked_add_signed.
func checkedAddSigned(value uint64, offset int64) (uint64, bool) {
	if offset >= 0 {
		sum := value + uint64(offset)
		return sum, sum >= value
	}
	magnitude := uint64(-(offset + 1)) + 1
	if magnitude > value {
		return 0, false
	}
	return value - magnitude, true
}

// checkedSignedDiff is u64::checked_signed_diff: a - b as an i64.
func checkedSignedDiff(a, b uint64) (int64, bool) {
	if a >= b {
		diff := a - b
		if diff > math.MaxInt64 {
			return 0, false
		}
		return int64(diff), true
	}
	diff := b - a
	if diff > uint64(math.MaxInt64)+1 {
		return 0, false
	}
	return int64(-diff), true
}

// BytesIter is io::Bytes: an iterator over the bytes of a reader.
//
// It owns its reader: Drop drops it.
type BytesIter[R io.Reader] struct {
	inner R
}

// Bytes is `r.bytes()`.
func Bytes[R io.Reader](inner R) *BytesIter[R] { return &BytesIter[R]{inner: inner} }

// Next is Iterator::next: the next byte, an error, or None at the end of
// input.
func (b *BytesIter[R]) Next() rs.Option[rs.Result[byte]] {
	var one [1]byte
	for {
		n, err := callRead(b.inner, one[:])
		if n == 0 && err != nil {
			if isInterruptedError(err) {
				continue
			}
			return rs.Some(rs.Err[byte](err))
		}
		if n == 0 {
			return rs.None[rs.Result[byte]]()
		}
		return rs.Some(rs.Ok(one[0]))
	}
}

// Iter ranges over the remaining bytes.
func (b *BytesIter[R]) Iter() iter.Seq[rs.Result[byte]] {
	return func(yield func(rs.Result[byte]) bool) {
		for {
			item, ok := b.Next().Get()
			if !ok || !yield(item) {
				return
			}
		}
	}
}

// Drop drops the reader.
func (b *BytesIter[R]) Drop() { dropValue(b.inner) }

// SplitIter is io::Split: an iterator over the parts of a reader between
// occurrences of a byte.
//
// It owns its reader: Drop drops it.
type SplitIter[B BufRead] struct {
	buf   B
	delim byte
}

// Split is `r.split(byte)`.
func Split[B BufRead](buf B, delim byte) *SplitIter[B] {
	return &SplitIter[B]{buf: buf, delim: delim}
}

// Next is Iterator::next.
func (s *SplitIter[B]) Next() rs.Option[rs.Result[[]byte]] {
	var buf []byte
	n, err := ReadUntil(s.buf, s.delim, &buf)
	if err != nil {
		return rs.Some(rs.Err[[]byte](err))
	}
	if n == 0 {
		return rs.None[rs.Result[[]byte]]()
	}
	if buf[len(buf)-1] == s.delim {
		buf = buf[:len(buf)-1]
	}
	return rs.Some(rs.Ok(buf))
}

// Iter ranges over the remaining parts.
func (s *SplitIter[B]) Iter() iter.Seq[rs.Result[[]byte]] {
	return func(yield func(rs.Result[[]byte]) bool) {
		for {
			item, ok := s.Next().Get()
			if !ok || !yield(item) {
				return
			}
		}
	}
}

// Drop drops the reader.
func (s *SplitIter[B]) Drop() { dropValue(s.buf) }

// LinesIter is io::Lines: an iterator over the lines of a reader, without
// their "\n" or "\r\n".
//
// It owns its reader: Drop drops it.
type LinesIter[B BufRead] struct {
	buf B
}

// Lines is `r.lines()`.
func Lines[B BufRead](buf B) *LinesIter[B] { return &LinesIter[B]{buf: buf} }

// Next is Iterator::next: the next line, an error (a line that is not UTF-8
// is one, and the iterator goes on after it), or None at the end of input.
func (l *LinesIter[B]) Next() rs.Option[rs.Result[string]] {
	var buf string
	n, err := ReadLine(l.buf, &buf)
	if err != nil {
		return rs.Some(rs.Err[string](err))
	}
	if n == 0 {
		return rs.None[rs.Result[string]]()
	}
	if strings.HasSuffix(buf, "\n") {
		buf = buf[:len(buf)-1]
		if strings.HasSuffix(buf, "\r") {
			buf = buf[:len(buf)-1]
		}
	}
	return rs.Some(rs.Ok(buf))
}

// Iter ranges over the remaining lines.
func (l *LinesIter[B]) Iter() iter.Seq[rs.Result[string]] {
	return func(yield func(rs.Result[string]) bool) {
		for {
			item, ok := l.Next().Get()
			if !ok || !yield(item) {
				return
			}
		}
	}
}

// Drop drops the reader.
func (l *LinesIter[B]) Drop() { dropValue(l.buf) }
