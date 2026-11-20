// Port of core 1.99.1 src/io/seek.rs

package rsio

import (
	"io"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// SeekFrom is Rust's io::SeekFrom: where a seek goes. It is one of
// *SeekFrom_Start, *SeekFrom_End and *SeekFrom_Current.
type SeekFrom interface {
	isSeekFrom()
}

// SeekFrom_Start is SeekFrom::Start: an offset from the start.
type SeekFrom_Start struct{ F0 uint64 }

// SeekFrom_End is SeekFrom::End: an offset from the end.
type SeekFrom_End struct{ F0 int64 }

// SeekFrom_Current is SeekFrom::Current: an offset from the position.
type SeekFrom_Current struct{ F0 int64 }

func (*SeekFrom_Start) isSeekFrom()   {}
func (*SeekFrom_End) isSeekFrom()     {}
func (*SeekFrom_Current) isSeekFrom() {}

var _ = rs.RegisterEnum[SeekFrom]("SeekFrom",
	rs.Variant[SeekFrom_Start]("Start", rs.ShapeTuple),
	rs.Variant[SeekFrom_End]("End", rs.ShapeTuple),
	rs.Variant[SeekFrom_Current]("Current", rs.ShapeTuple),
)

// Rust's Seek trait has no Go type of its own: a seekable value is an
// io.Seeker, whose Seek takes an offset and one of io.SeekStart,
// io.SeekCurrent and io.SeekEnd, and the trait's methods are the functions
// in this file.
//
// A type whose Rust seek accepts positions an int64 cannot hold (a Cursor)
// also has `SeekTo(SeekFrom) (uint64, error)`, and Seek below prefers it.

// seekTo is the method a type has when its Rust seek is not expressible
// through io.Seeker alone.
type seekTo interface {
	SeekTo(pos SeekFrom) (uint64, error)
}

// Seek is `s.seek(pos)`: it returns the new position from the start.
func Seek(s io.Seeker, pos SeekFrom) (uint64, error) {
	if seeker, ok := deref(s).(seekTo); ok {
		position, err := seeker.SeekTo(pos)
		return position, asError(err)
	}
	var (
		offset int64
		whence int
	)
	switch pos := pos.(type) {
	case *SeekFrom_Start:
		// Casting to `i64` is fine, too large values will end up as
		// negative which will cause an error in `lseek64`.
		offset, whence = int64(pos.F0), io.SeekStart
	case *SeekFrom_End:
		offset, whence = pos.F0, io.SeekEnd
	case *SeekFrom_Current:
		offset, whence = pos.F0, io.SeekCurrent
	default:
		panic(rs.Unreachable(pos))
	}
	position, err := s.Seek(offset, whence)
	if err != nil {
		return 0, From(err)
	}
	return uint64(position), nil
}

// seekFromGo turns the arguments of Go's Seek into a SeekFrom. An unknown
// whence is the error the system gives for one.
func seekFromGo(offset int64, whence int) (SeekFrom, error) {
	switch whence {
	case io.SeekStart:
		if offset < 0 {
			return nil, invalidSeek()
		}
		return &SeekFrom_Start{F0: uint64(offset)}, nil
	case io.SeekEnd:
		return &SeekFrom_End{F0: offset}, nil
	case io.SeekCurrent:
		return &SeekFrom_Current{F0: offset}, nil
	}
	return nil, invalidSeek()
}

func invalidSeek() *Error {
	return ErrorFromStaticMessage(ErrorKind_InvalidInput, "invalid seek to a negative or overflowing position")
}

// Rewind is `s.rewind()`.
func Rewind(s io.Seeker) error {
	_, err := Seek(s, &SeekFrom_Start{F0: 0})
	return err
}

// StreamLen is `s.stream_len()`.
func StreamLen(s io.Seeker) (uint64, error) {
	if seeker, ok := deref(s).(interface{ StreamLen() (uint64, error) }); ok {
		length, err := seeker.StreamLen()
		return length, asError(err)
	}
	return StreamLenDefault(s)
}

// StreamPosition is `s.stream_position()`.
func StreamPosition(s io.Seeker) (uint64, error) {
	if seeker, ok := deref(s).(interface{ StreamPosition() (uint64, error) }); ok {
		position, err := seeker.StreamPosition()
		return position, asError(err)
	}
	return Seek(s, &SeekFrom_Current{F0: 0})
}

// SeekRelative is `s.seek_relative(offset)`.
func SeekRelative(s io.Seeker, offset int64) error {
	if seeker, ok := deref(s).(interface{ SeekRelative(offset int64) error }); ok {
		return asError(seeker.SeekRelative(offset))
	}
	_, err := Seek(s, &SeekFrom_Current{F0: offset})
	return err
}

// StreamLenDefault is io::stream_len_default.
func StreamLenDefault(s io.Seeker) (uint64, error) {
	oldPos, err := StreamPosition(s)
	if err != nil {
		return 0, err
	}
	length, err := Seek(s, &SeekFrom_End{F0: 0})
	if err != nil {
		return 0, err
	}

	// Avoid seeking a third time when we were already at the end of the
	// stream. The branch is usually way cheaper than a seek operation.
	if oldPos != length {
		if _, err := Seek(s, &SeekFrom_Start{F0: oldPos}); err != nil {
			return 0, err
		}
	}

	return length, nil
}
