// Port of alloc 1.99.1 src/io/impls.rs and core 1.99.1 src/io/impls.rs (the
// in-memory implementations)

package rsio

import "unicode/utf8"

// Rust implements Read for `&[u8]` and Write for `Vec<u8>` and `&mut [u8]`.
// A Go slice has no methods, so each has a small type here that stands on a
// slice variable.

// SliceReader is `&[u8]` as a reader: reading takes bytes off the front of
// the slice it stands on.
type SliceReader struct {
	slice *[]byte
}

// Slice is `&mut slice` used as a reader, where slice is a `&[u8]`: reads
// advance *slice, as they advance the Rust variable.
func Slice(slice *[]byte) *SliceReader { return &SliceReader{slice: slice} }

// SliceFrom is a `&[u8]` passed by value as a reader.
func SliceFrom(slice []byte) *SliceReader { return &SliceReader{slice: &slice} }

// Remaining is the slice itself: the bytes not read yet.
func (s *SliceReader) Remaining() []byte { return *s.slice }

// Read is Read::read.
func (s *SliceReader) Read(buf []byte) (int, error) {
	amount := copy(buf, *s.slice)
	*s.slice = (*s.slice)[amount:]
	return amount, nil
}

// ReadBuf is Read::read_buf.
func (s *SliceReader) ReadBuf(cursor *BorrowedCursor) error {
	amount := min(cursor.Capacity(), len(*s.slice))
	cursor.Append((*s.slice)[:amount])
	*s.slice = (*s.slice)[amount:]
	return nil
}

// ReadExact is Read::read_exact.
func (s *SliceReader) ReadExact(buf []byte) error {
	if len(buf) > len(*s.slice) {
		// `read_exact` makes no promise about the content of `buf` if it
		// fails so don't bother about that.
		*s.slice = (*s.slice)[len(*s.slice):]
		return errReadExactEof
	}
	copy(buf, *s.slice)
	*s.slice = (*s.slice)[len(buf):]
	return nil
}

// ReadBufExact is Read::read_buf_exact.
func (s *SliceReader) ReadBufExact(cursor *BorrowedCursor) error {
	if cursor.Capacity() > len(*s.slice) {
		// Append everything we can to the cursor.
		cursor.Append(*s.slice)
		*s.slice = (*s.slice)[len(*s.slice):]
		return errReadExactEof
	}
	amount := cursor.Capacity()
	cursor.Append((*s.slice)[:amount])
	*s.slice = (*s.slice)[amount:]
	return nil
}

// ReadToEnd is Read::read_to_end.
func (s *SliceReader) ReadToEnd(buf *[]byte) (int, error) {
	length := len(*s.slice)
	*buf = append(*buf, *s.slice...)
	*s.slice = (*s.slice)[length:]
	return length, nil
}

// ReadToString is Read::read_to_string. When the bytes are not UTF-8 it
// fails and reads nothing.
func (s *SliceReader) ReadToString(buf *string) (int, error) {
	if !utf8.Valid(*s.slice) {
		return 0, errInvalidUtf8
	}
	length := len(*s.slice)
	*buf += string(*s.slice)
	*s.slice = (*s.slice)[length:]
	return length, nil
}

// FillBuf is BufRead::fill_buf.
func (s *SliceReader) FillBuf() ([]byte, error) { return *s.slice, nil }

// Consume is BufRead::consume.
func (s *SliceReader) Consume(amount int) { *s.slice = (*s.slice)[amount:] }

// VecWriter is `Vec<u8>` as a writer: writing appends to the slice it
// stands on.
type VecWriter struct {
	vec *[]byte
}

// Vec is `&mut vec` used as a writer, where vec is a `Vec<u8>`.
func Vec(vec *[]byte) *VecWriter { return &VecWriter{vec: vec} }

// Write is Write::write.
func (v *VecWriter) Write(buf []byte) (int, error) {
	*v.vec = append(*v.vec, buf...)
	return len(buf), nil
}

// WriteAll is Write::write_all.
func (v *VecWriter) WriteAll(buf []byte) error {
	*v.vec = append(*v.vec, buf...)
	return nil
}

// Flush is Write::flush.
func (v *VecWriter) Flush() error { return nil }

// SliceWriter is `&mut [u8]` as a writer: writing overwrites the front of
// the slice it stands on and moves the slice past what was written, so that
// it ends up empty when it is full.
type SliceWriter struct {
	slice *[]byte
}

// SliceMut is `&mut slice` used as a writer, where slice is a `&mut [u8]`.
func SliceMut(slice *[]byte) *SliceWriter { return &SliceWriter{slice: slice} }

// Write is Write::write.
func (s *SliceWriter) Write(data []byte) (int, error) {
	amount := copy(*s.slice, data)
	*s.slice = (*s.slice)[amount:]
	return amount, nil
}

// WriteAll is Write::write_all.
func (s *SliceWriter) WriteAll(data []byte) error {
	if n, _ := s.Write(data); n < len(data) {
		return errWriteAllEof
	}
	return nil
}

// Flush is Write::flush.
func (s *SliceWriter) Flush() error { return nil }
