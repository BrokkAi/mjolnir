// Port of std 1.99.1 src/sys/pipe/unix.rs

//go:build unix

package rsio

// pipeEnd is sys::pipe::Pipe: a FileDesc.
type pipeEnd struct {
	fd *OwnedFd
}

func (p pipeEnd) read(buf []byte) (int, error) { return fdRead(int(p.fd.AsRawFd()), buf) }

func (p pipeEnd) write(buf []byte) (int, error) { return fdWrite(int(p.fd.AsRawFd()), buf) }

func (p pipeEnd) tryClone() (pipeEnd, error) {
	fd, err := p.fd.TryClone()
	if err != nil {
		return pipeEnd{}, err
	}
	return pipeEnd{fd: fd}, nil
}

func (p pipeEnd) drop() { p.fd.Drop() }

func (p pipeEnd) debugString() string { return "FileDesc(" + p.fd.DebugString() + ")" }

// AsRawFd is `impl AsRawFd for PipeReader`.
func (p *PipeReader) AsRawFd() RawFd { return p.inner.fd.AsRawFd() }

// AsFd is `impl AsFd for PipeReader`.
func (p *PipeReader) AsFd() BorrowedFd { return p.inner.fd.AsFd() }

// IntoRawFd is `impl IntoRawFd for PipeReader`.
func (p *PipeReader) IntoRawFd() RawFd { return p.inner.fd.IntoRawFd() }

// PipeReaderFromOwnedFd is `impl From<OwnedFd> for PipeReader`.
func PipeReaderFromOwnedFd(fd *OwnedFd) *PipeReader { return &PipeReader{inner: pipeEnd{fd: fd}} }

// AsRawFd is `impl AsRawFd for PipeWriter`.
func (p *PipeWriter) AsRawFd() RawFd { return p.inner.fd.AsRawFd() }

// AsFd is `impl AsFd for PipeWriter`.
func (p *PipeWriter) AsFd() BorrowedFd { return p.inner.fd.AsFd() }

// IntoRawFd is `impl IntoRawFd for PipeWriter`.
func (p *PipeWriter) IntoRawFd() RawFd { return p.inner.fd.IntoRawFd() }

// PipeWriterFromOwnedFd is `impl From<OwnedFd> for PipeWriter`.
func PipeWriterFromOwnedFd(fd *OwnedFd) *PipeWriter { return &PipeWriter{inner: pipeEnd{fd: fd}} }
