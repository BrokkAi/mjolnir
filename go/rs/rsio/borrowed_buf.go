// Port of core 1.99.1 src/io/borrowed_buf.rs

package rsio

import "github.com/BrokkAi/mjolnir/go/rs"

// BorrowedBuf is Rust's io::BorrowedBuf: a buffer being filled, which
// remembers how much of it holds data and whether all of it has been
// initialized. Go memory is always initialized; the flag is kept because
// read_to_end sizes its reads by it, and those sizes are what a reader sees.
//
// It is the argument of the read_buf protocol (ReadBuf), which the standard
// library uses between its own readers. Product code does not use it.
type BorrowedBuf struct {
	buf    []byte
	filled int
	init   bool
}

// BorrowedBufFrom is `BorrowedBuf::from(&mut [u8])`: a buffer over memory
// that is initialized.
func BorrowedBufFrom(buf []byte) *BorrowedBuf {
	return &BorrowedBuf{buf: buf, filled: 0, init: true}
}

// BorrowedBufFromUninit is `BorrowedBuf::from(&mut [MaybeUninit<u8>])`: a
// buffer over memory Rust would not have initialized, such as the spare
// capacity of a Vec.
func BorrowedBufFromUninit(buf []byte) *BorrowedBuf {
	return &BorrowedBuf{buf: buf, filled: 0, init: false}
}

// Capacity is BorrowedBuf::capacity.
func (b *BorrowedBuf) Capacity() int { return len(b.buf) }

// Len is BorrowedBuf::len: the number of bytes filled.
func (b *BorrowedBuf) Len() int { return b.filled }

// IsInit is BorrowedBuf::is_init.
func (b *BorrowedBuf) IsInit() bool { return b.init }

// Filled is BorrowedBuf::filled.
func (b *BorrowedBuf) Filled() []byte { return b.buf[:b.filled] }

// Unfilled is BorrowedBuf::unfilled: a cursor over the part not yet filled.
func (b *BorrowedBuf) Unfilled() *BorrowedCursor { return &BorrowedCursor{buf: b} }

// Clear is BorrowedBuf::clear.
func (b *BorrowedBuf) Clear() *BorrowedBuf {
	b.filled = 0
	return b
}

// SetInit is BorrowedBuf::set_init.
func (b *BorrowedBuf) SetInit() *BorrowedBuf {
	b.init = true
	return b
}

// BorrowedCursor is Rust's io::BorrowedCursor: the writable view of the
// unfilled part of a BorrowedBuf.
type BorrowedCursor struct {
	buf *BorrowedBuf
}

// Reborrow is BorrowedCursor::reborrow. A Go cursor is a reference already.
func (c *BorrowedCursor) Reborrow() *BorrowedCursor { return c }

// Capacity is BorrowedCursor::capacity: the bytes that can still be written.
func (c *BorrowedCursor) Capacity() int { return len(c.buf.buf) - c.buf.filled }

// Written is BorrowedCursor::written: the bytes filled in the underlying
// buffer.
func (c *BorrowedCursor) Written() int { return c.buf.filled }

// IsInit is BorrowedCursor::is_init.
func (c *BorrowedCursor) IsInit() bool { return c.buf.init }

// SetInit is BorrowedCursor::set_init.
func (c *BorrowedCursor) SetInit() { c.buf.init = true }

// AsMut is BorrowedCursor::as_mut: the unfilled part.
func (c *BorrowedCursor) AsMut() []byte { return c.buf.buf[c.buf.filled:] }

// AdvanceChecked is BorrowedCursor::advance_checked.
func (c *BorrowedCursor) AdvanceChecked(n int) *BorrowedCursor {
	initUnfilled := 0
	if c.buf.init {
		initUnfilled = len(c.buf.buf) - c.buf.filled
	}
	if n < 0 || n > initUnfilled {
		rs.PanicAt(1, "assertion failed: n <= init_unfilled")
	}
	c.buf.filled += n
	return c
}

// Advance is BorrowedCursor::advance.
func (c *BorrowedCursor) Advance(n int) *BorrowedCursor {
	c.buf.filled += n
	return c
}

// Append is BorrowedCursor::append.
func (c *BorrowedCursor) Append(buf []byte) {
	if c.Capacity() < len(buf) {
		rs.PanicAt(1, "assertion failed: self.capacity() >= buf.len()")
	}
	copy(c.buf.buf[c.buf.filled:], buf)
	c.buf.filled += len(buf)
}

// EnsureInit is BorrowedCursor::ensure_init: the unfilled part, marked
// initialized.
func (c *BorrowedCursor) EnsureInit() []byte {
	unfilled := c.buf.buf[c.buf.filled:]
	if !c.buf.init {
		clear(unfilled)
		c.buf.init = true
	}
	return unfilled
}
