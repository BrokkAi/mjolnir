// Port of std 1.99.1 src/sys/pipe/windows.rs

//go:build windows

package rsio

import (
	"syscall"

	"golang.org/x/sys/windows"
)

// pipeEnd is sys::pipe::Pipe: a Handle.
type pipeEnd struct {
	handle *OwnedHandle
}

func sysPipe() (pipeEnd, pipeEnd, error) {
	var readPipe, writePipe windows.Handle
	if err := windows.CreatePipe(&readPipe, &writePipe, nil, 0); err != nil {
		return pipeEnd{}, pipeEnd{}, From(err)
	}
	return pipeEnd{handle: OwnedHandleFromRawHandle(readPipe)}, pipeEnd{handle: OwnedHandleFromRawHandle(writePipe)}, nil
}

func (p pipeEnd) read(buf []byte) (int, error) {
	var done uint32
	err := windows.ReadFile(p.handle.AsRawHandle(), buf, &done, nil)
	if err != nil {
		// The special treatment of BrokenPipe is to deal with Windows
		// pipe semantics. Windows pipes return ERROR_BROKEN_PIPE when
		// the write end of the pipe is closed, instead of returning EOF.
		// So the standard library treats that as EOF.
		if errno, ok := err.(syscall.Errno); ok && (errno == windows.ERROR_BROKEN_PIPE || errno == windows.ERROR_HANDLE_EOF) {
			return 0, nil
		}
		return 0, From(err)
	}
	return int(done), nil
}

func (p pipeEnd) write(buf []byte) (int, error) {
	var done uint32
	if err := windows.WriteFile(p.handle.AsRawHandle(), buf, &done, nil); err != nil {
		return 0, From(err)
	}
	return int(done), nil
}

func (p pipeEnd) tryClone() (pipeEnd, error) {
	handle, err := p.handle.TryClone()
	if err != nil {
		return pipeEnd{}, err
	}
	return pipeEnd{handle: handle}, nil
}

func (p pipeEnd) drop() { p.handle.Drop() }

func (p pipeEnd) debugString() string { return "Handle(" + p.handle.DebugString() + ")" }

// AsRawHandle is `impl AsRawHandle for PipeReader`.
func (p *PipeReader) AsRawHandle() RawHandle { return p.inner.handle.AsRawHandle() }

// AsHandle is `impl AsHandle for PipeReader`.
func (p *PipeReader) AsHandle() BorrowedHandle { return p.inner.handle.AsHandle() }

// IntoRawHandle is `impl IntoRawHandle for PipeReader`.
func (p *PipeReader) IntoRawHandle() RawHandle { return p.inner.handle.IntoRawHandle() }

// PipeReaderFromOwnedHandle is `impl From<OwnedHandle> for PipeReader`.
func PipeReaderFromOwnedHandle(handle *OwnedHandle) *PipeReader {
	return &PipeReader{inner: pipeEnd{handle: handle}}
}

// AsRawHandle is `impl AsRawHandle for PipeWriter`.
func (p *PipeWriter) AsRawHandle() RawHandle { return p.inner.handle.AsRawHandle() }

// AsHandle is `impl AsHandle for PipeWriter`.
func (p *PipeWriter) AsHandle() BorrowedHandle { return p.inner.handle.AsHandle() }

// IntoRawHandle is `impl IntoRawHandle for PipeWriter`.
func (p *PipeWriter) IntoRawHandle() RawHandle { return p.inner.handle.IntoRawHandle() }

// PipeWriterFromOwnedHandle is `impl From<OwnedHandle> for PipeWriter`.
func PipeWriterFromOwnedHandle(handle *OwnedHandle) *PipeWriter {
	return &PipeWriter{inner: pipeEnd{handle: handle}}
}
