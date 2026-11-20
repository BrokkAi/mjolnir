// Port of alloc 1.99.1 src/io/copy.rs, src/io/copy/generic.rs and
// src/io/copy/specialization.rs

package rsio

import (
	"io"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Copy is io::copy: it reads reader to its end, writes everything to writer
// with write_all, and returns the number of bytes copied.
//
// Rust chooses how to move the bytes by the types on the two sides, and so
// does this: a BufReader lends its own buffer, a BufWriter is filled
// directly, a Vec is read into, and everything else goes through a buffer of
// 8 KiB. A reader that can do better still, as a file copying to a file
// inside the kernel, says so through SpecCopy.
func Copy(reader io.Reader, writer io.Writer) (uint64, error) {
	copied := uint64(0)
	if spec, ok := deref(reader).(SpecCopy); ok {
		done, ended, err := spec.SpecCopy(writer)
		if err != nil {
			return 0, asError(err)
		}
		if ended {
			return done, nil
		}
		copied = done
	}
	additional, err := genericCopy(reader, writer)
	if err != nil {
		return 0, err
	}
	return copied + additional, nil
}

// SpecCopy is alloc's SpecCopy trait: a reader that knows a faster way to
// copy itself to some writers than reading and writing, as Rust's File does
// on Linux with copy_file_range, sendfile and splice.
//
// SpecCopy returns the number of bytes it copied and whether the copy is
// complete (CopyState::Ended). When it is not (CopyState::Fallback), Copy
// goes on from there by reading and writing.
type SpecCopy interface {
	SpecCopy(writer io.Writer) (copied uint64, ended bool, err error)
}

// The reader and the writer of a copy can say how well buffered they are,
// and the better buffer is used.
type bufferedReaderSpec interface {
	readBufferPriority() int
	copyTo(to io.Writer) (uint64, error)
}

type bufferedWriterSpec interface {
	writeBufferPriority() int
	copyFrom(reader io.Reader) (uint64, error)
}

const (
	unbuffered = 0
	inMemory   = rs.UsizeMax
)

func genericCopy(reader io.Reader, writer io.Writer) (uint64, error) {
	readPriority := unbuffered
	bufferedReader, isBufferedReader := deref(reader).(bufferedReaderSpec)
	if isBufferedReader {
		readPriority = bufferedReader.readBufferPriority()
	}
	writePriority := unbuffered
	bufferedWriter, isBufferedWriter := deref(writer).(bufferedWriterSpec)
	if isBufferedWriter {
		writePriority = bufferedWriter.writeBufferPriority()
	}

	if readPriority >= DefaultBufSize && readPriority >= writePriority {
		return bufferedReader.copyTo(writer)
	}

	if isBufferedWriter {
		return bufferedWriter.copyFrom(reader)
	}
	// Unlike `BufferedReaderSpec::copy_to`, this _will_ be called as the fallback
	// when both the reader and writer provide no specialization.
	return stackBufferCopy(reader, writer)
}

// BufferedReaderSpec for &[u8].
func (s *SliceReader) readBufferPriority() int { return inMemory }

func (s *SliceReader) copyTo(to io.Writer) (uint64, error) {
	length := len(*s.slice)
	if err := WriteAll(to, *s.slice); err != nil {
		return 0, err
	}
	*s.slice = (*s.slice)[length:]
	return uint64(length), nil
}

// BufferedWriterSpec for Vec<u8>.
func (v *VecWriter) writeBufferPriority() int { return cap(*v.vec) - len(*v.vec) }

func (v *VecWriter) copyFrom(reader io.Reader) (uint64, error) {
	n, err := ReadToEnd(reader, v.vec)
	if err != nil {
		return 0, err
	}
	return uint64(n), nil
}

func stackBufferCopy(reader io.Reader, writer io.Writer) (uint64, error) {
	var storage [DefaultBufSize]byte
	buf := BorrowedBufFromUninit(storage[:])

	var length uint64

	for {
		if err := ReadBuf(reader, buf.Unfilled()); err != nil {
			if buf.Len() == 0 && isInterruptedError(err) {
				continue
			}
			return 0, err
		}

		if len(buf.Filled()) == 0 {
			break
		}

		length += uint64(len(buf.Filled()))
		if err := WriteAll(writer, buf.Filled()); err != nil {
			return 0, err
		}
		buf.Clear()
	}

	return length, nil
}
