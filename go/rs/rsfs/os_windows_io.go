// Port of std 1.99.1 src/os/windows/io/handle.rs and src/os/windows/io/raw.rs
// (the impls for fs::File) and src/sys/io/is_terminal/windows.rs (IsTerminal
// for File)

//go:build windows

package rsfs

import (
	"os"
	"runtime"
	"sync"

	"github.com/BrokkAi/mjolnir/go/rs/rsio"
)

// AsRawHandle is `impl AsRawHandle for File`.
func (f *File) AsRawHandle() rsio.RawHandle {
	defer runtime.KeepAlive(f)
	return f.inner.rawHandle()
}

// AsHandle is `impl AsHandle for File`.
func (f *File) AsHandle() rsio.BorrowedHandle {
	return rsio.BorrowedHandleBorrowRaw(f.AsRawHandle())
}

// released keeps the *os.File of every File given up with IntoRawHandle, so
// that the garbage collector never closes a handle that has a new owner.
var released struct {
	sync.Mutex
	files []*os.File
}

// IntoRawHandle is `impl IntoRawHandle for File`: the handle, which the
// caller owns from then on. The File is empty afterwards.
func (f *File) IntoRawHandle() rsio.RawHandle {
	handle := f.inner.rawHandle()
	released.Lock()
	released.files = append(released.files, f.inner.file)
	released.Unlock()
	f.inner.file = nil
	return handle
}

// FileFromRawHandle is `impl FromRawHandle for File`: a File that owns the
// handle.
func FileFromRawHandle(handle rsio.RawHandle) *File {
	return &File{inner: newSysFile(handle, "")}
}

// FileFromOwnedHandle is `impl From<OwnedHandle> for File`.
func FileFromOwnedHandle(handle *rsio.OwnedHandle) *File {
	return FileFromRawHandle(handle.IntoRawHandle())
}

// IsTerminal is `impl IsTerminal for File`.
func (f *File) IsTerminal() bool {
	return rsio.HandleIsTerminal(f.AsRawHandle())
}

// OsFile is the *os.File that owns the handle, for Go code that needs one
// (os/exec, for one). It stays owned by the File: do not close it, and do
// not use it after the File is dropped.
func (f *File) OsFile() *os.File {
	f.inner.rawHandle()
	return f.inner.file
}
