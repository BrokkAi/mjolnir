// Port of core 1.99.1 src/io/cursor.rs and alloc 1.99.1 src/io/cursor.rs

package rsio

import (
	"bytes"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Cursor is Rust's io::Cursor: a position over bytes in memory, readable,
// seekable and, depending on what it was made over, writable.
//
// Rust's Cursor<T> is generic in what holds the bytes, and what a write does
// depends on T. Here the bytes are always a []byte and the constructor says
// which Cursor it is:
//
//   - CursorNew is Cursor<Vec<u8>>, and every Cursor that is only read
//     (Cursor<&[u8]>, Cursor<&Vec<u8>>, Cursor<Bytes>, Cursor<&str>): a write
//     overwrites and extends the bytes, growing them as needed.
//   - CursorNewSlice is Cursor<&mut [u8]>, Cursor<Box<[u8]>> and
//     Cursor<[u8; N]>: a write stops at the end of the bytes.
//   - CursorNewVecRef is Cursor<&mut Vec<u8>>: as CursorNew, on the caller's
//     slice variable.
type Cursor struct {
	inner []byte
	// ref is the caller's Vec for Cursor<&mut Vec<u8>>, and nil otherwise.
	ref   *[]byte
	pos   uint64
	fixed bool
}

// CursorNew is Cursor::new over a Vec<u8> or over bytes that are only read.
func CursorNew(inner []byte) *Cursor {
	return &Cursor{inner: inner, pos: 0}
}

// CursorNewSlice is Cursor::new over a `&mut [u8]`, a `Box<[u8]>` or an
// array: writing does not grow the bytes.
func CursorNewSlice(inner []byte) *Cursor {
	return &Cursor{inner: inner, pos: 0, fixed: true}
}

// CursorNewVecRef is Cursor::new over a `&mut Vec<u8>`: writes go to *inner.
func CursorNewVecRef(inner *[]byte) *Cursor {
	return &Cursor{ref: inner, pos: 0}
}

// CursorDefault is `Cursor::default()`: an empty Cursor<Vec<u8>>.
func CursorDefault() *Cursor { return CursorNew(nil) }

func (c *Cursor) vec() *[]byte {
	if c.ref != nil {
		return c.ref
	}
	return &c.inner
}

// IntoInner is Cursor::into_inner.
func (c *Cursor) IntoInner() []byte { return *c.vec() }

// GetRef is Cursor::get_ref.
func (c *Cursor) GetRef() []byte { return *c.vec() }

// GetMut is Cursor::get_mut: the bytes, to change in place or to replace.
func (c *Cursor) GetMut() *[]byte { return c.vec() }

// Position is Cursor::position.
func (c *Cursor) Position() uint64 { return c.pos }

// SetPosition is Cursor::set_position.
func (c *Cursor) SetPosition(pos uint64) { c.pos = pos }

// remaining is `Cursor::split(self).1`: the bytes from the position on.
func (c *Cursor) remaining() []byte {
	slice := *c.vec()
	pos := min(c.pos, uint64(len(slice)))
	return slice[pos:]
}

// Clone is `impl Clone for Cursor`: a cursor over a copy of the bytes.
func (c *Cursor) Clone() *Cursor {
	return &Cursor{inner: bytes.Clone(*c.vec()), pos: c.pos, fixed: c.fixed}
}

// Equal is the derived PartialEq: the same bytes and the same position.
func (c *Cursor) Equal(other *Cursor) bool {
	return c.pos == other.pos && bytes.Equal(*c.vec(), *other.vec())
}

// FmtDebug is the derived Debug: `Cursor { inner: [..], pos: 0 }`.
func (c *Cursor) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Cursor").Field("inner", *c.vec()).Field("pos", c.pos).Finish()
}

// Read is Read::read.
func (c *Cursor) Read(buf []byte) (int, error) {
	n := copy(buf, c.remaining())
	c.pos += uint64(n)
	return n, nil
}

// ReadBuf is Read::read_buf.
func (c *Cursor) ReadBuf(cursor *BorrowedCursor) error {
	remaining := c.remaining()
	amount := min(cursor.Capacity(), len(remaining))
	cursor.Append(remaining[:amount])
	c.pos += uint64(amount)
	return nil
}

// ReadExact is Read::read_exact.
func (c *Cursor) ReadExact(buf []byte) error {
	remaining := c.remaining()
	if len(buf) > len(remaining) {
		// The only possible error condition is EOF, so place the cursor at "EOF"
		c.pos = uint64(len(*c.vec()))
		return errReadExactEof
	}
	copy(buf, remaining)
	c.pos += uint64(len(buf))
	return nil
}

// ReadToEnd is Read::read_to_end.
func (c *Cursor) ReadToEnd(buf *[]byte) (int, error) {
	content := c.remaining()
	length := len(content)
	*buf = append(*buf, content...)
	c.pos += uint64(length)

	return length, nil
}

// ReadToString is Read::read_to_string. When the remaining bytes are not
// UTF-8 it fails and reads nothing.
func (c *Cursor) ReadToString(buf *string) (int, error) {
	content := c.remaining()
	if !utf8.Valid(content) {
		return 0, errInvalidUtf8
	}
	length := len(content)
	*buf += string(content)

	c.pos += uint64(length)

	return length, nil
}

// FillBuf is BufRead::fill_buf.
func (c *Cursor) FillBuf() ([]byte, error) { return c.remaining(), nil }

// Consume is BufRead::consume.
func (c *Cursor) Consume(amount int) { c.pos += uint64(amount) }

// sliceWrite is io::slice_write.
func sliceWrite(posMut *uint64, slice []byte, buf []byte) (int, error) {
	pos := min(*posMut, uint64(len(slice)))
	amount := copy(slice[pos:], buf)
	*posMut += uint64(amount)
	return amount, nil
}

// sliceWriteAll is io::slice_write_all.
func sliceWriteAll(posMut *uint64, slice []byte, buf []byte) error {
	n, _ := sliceWrite(posMut, slice, buf)
	if n < len(buf) {
		return errWriteAllEof
	}
	return nil
}

func reserveAndPad(posMut *uint64, vec *[]byte, bufLen int) (int, error) {
	if *posMut > uint64(rs.UsizeMax) {
		return 0, ErrorFromStaticMessage(ErrorKind_InvalidInput, "cursor position exceeds maximum possible vector length")
	}
	pos := int(*posMut)

	// For safety reasons, we don't want these numbers to overflow
	// otherwise our allocation won't be enough
	desiredCap, ok := checkedAdd(pos, bufLen)
	if !ok {
		desiredCap = rs.UsizeMax
	}
	if desiredCap > cap(*vec) {
		// We want our vec's total capacity
		// to have room for (pos+buf_len) bytes. Reserve allocates
		// based on additional elements from the length, so we need to
		// reserve the difference
		reserveVec(vec, desiredCap-len(*vec))
	}
	// Pad if pos is above the current len.
	if pos > len(*vec) {
		length := len(*vec)
		*vec = (*vec)[:pos]
		clear((*vec)[length:pos])
	}

	return pos, nil
}

// vecWriteAll is io::cursor::vec_write_all.
func vecWriteAll(posMut *uint64, vec *[]byte, buf []byte) (int, error) {
	bufLen := len(buf)
	pos, err := reserveAndPad(posMut, vec, bufLen)
	if err != nil {
		return 0, err
	}

	// Write the buf then progress the vec forward if necessary
	if end := pos + bufLen; end > len(*vec) {
		*vec = (*vec)[:end]
	}
	copy((*vec)[pos:], buf)

	// Bump us forward
	*posMut += uint64(bufLen)
	return bufLen, nil
}

// Write is Write::write.
func (c *Cursor) Write(buf []byte) (int, error) {
	if c.fixed {
		return sliceWrite(&c.pos, c.inner, buf)
	}
	return vecWriteAll(&c.pos, c.vec(), buf)
}

// WriteAll is Write::write_all. Over a slice it fails with WriteZero,
// "failed to write whole buffer", when the slice ends first.
func (c *Cursor) WriteAll(buf []byte) error {
	if c.fixed {
		return sliceWriteAll(&c.pos, c.inner, buf)
	}
	_, err := vecWriteAll(&c.pos, c.vec(), buf)
	return err
}

// Flush is Write::flush.
func (c *Cursor) Flush() error { return nil }

// SeekTo is Seek::seek.
func (c *Cursor) SeekTo(style SeekFrom) (uint64, error) {
	var (
		basePos uint64
		offset  int64
	)
	switch style := style.(type) {
	case *SeekFrom_Start:
		c.pos = style.F0
		return style.F0, nil
	case *SeekFrom_End:
		basePos, offset = uint64(len(*c.vec())), style.F0
	case *SeekFrom_Current:
		basePos, offset = c.pos, style.F0
	default:
		panic(rs.Unreachable(style))
	}
	position, ok := checkedAddSigned(basePos, offset)
	if !ok {
		return 0, invalidSeek()
	}
	c.pos = position
	return position, nil
}

// Seek is Seek::seek through Go's signature.
func (c *Cursor) Seek(offset int64, whence int) (int64, error) {
	pos, err := seekFromGo(offset, whence)
	if err != nil {
		return 0, err
	}
	position, err := c.SeekTo(pos)
	return int64(position), err
}

// StreamLen is Seek::stream_len.
func (c *Cursor) StreamLen() (uint64, error) { return uint64(len(*c.vec())), nil }

// StreamPosition is Seek::stream_position.
func (c *Cursor) StreamPosition() (uint64, error) { return c.pos, nil }
