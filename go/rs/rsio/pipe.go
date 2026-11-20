// Port of std 1.99.1 src/io/pipe.rs

package rsio

import "github.com/BrokkAi/mjolnir/go/rs"

// PipeReader is io::PipeReader: the read end of an anonymous pipe made by
// Pipe. Drop closes it.
type PipeReader struct {
	inner pipeEnd
}

// PipeWriter is io::PipeWriter: the write end of an anonymous pipe made by
// Pipe. Drop closes it, which is how the reader sees the end of input.
type PipeWriter struct {
	inner pipeEnd
}

// Pipe is io::pipe: an anonymous pipe, closed in child processes unless it
// is passed to one.
func Pipe() (*PipeReader, *PipeWriter, error) {
	reader, writer, err := sysPipe()
	if err != nil {
		return nil, nil, err
	}
	return &PipeReader{inner: reader}, &PipeWriter{inner: writer}, nil
}

// TryClone is PipeReader::try_clone: another handle to the same end.
func (p *PipeReader) TryClone() (*PipeReader, error) {
	inner, err := p.inner.tryClone()
	if err != nil {
		return nil, err
	}
	return &PipeReader{inner: inner}, nil
}

// TryClone is PipeWriter::try_clone: another handle to the same end.
func (p *PipeWriter) TryClone() (*PipeWriter, error) {
	inner, err := p.inner.tryClone()
	if err != nil {
		return nil, err
	}
	return &PipeWriter{inner: inner}, nil
}

// Read is Read::read.
func (p *PipeReader) Read(buf []byte) (int, error) { return p.inner.read(buf) }

// ReadBuf is Read::read_buf.
func (p *PipeReader) ReadBuf(cursor *BorrowedCursor) error {
	n, err := p.inner.read(cursor.AsMut())
	if err != nil {
		return err
	}
	cursor.Advance(n)
	return nil
}

// ReadToEnd is Read::read_to_end.
func (p *PipeReader) ReadToEnd(buf *[]byte) (int, error) {
	return DefaultReadToEnd(p, buf, rs.None[int]())
}

// Drop closes this end. A second call does nothing.
func (p *PipeReader) Drop() { p.inner.drop() }

// DebugString is the derived Debug.
func (p *PipeReader) DebugString() string { return "PipeReader(" + p.inner.debugString() + ")" }

// Write is Write::write.
func (p *PipeWriter) Write(buf []byte) (int, error) { return p.inner.write(buf) }

// Flush is Write::flush.
func (p *PipeWriter) Flush() error { return nil }

// Drop closes this end. A second call does nothing.
func (p *PipeWriter) Drop() { p.inner.drop() }

// DebugString is the derived Debug.
func (p *PipeWriter) DebugString() string { return "PipeWriter(" + p.inner.debugString() + ")" }
