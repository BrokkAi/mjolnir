// Port of std 1.99.1 src/os/fd/owned.rs and src/os/fd/raw.rs (the impls for
// fs::File) and src/sys/io/is_terminal/isatty.rs (IsTerminal for File)

//go:build unix

package rsfs

import (
	"os"
	"runtime"
	"sync"

	"github.com/BrokkAi/mjolnir/go/rs/rsio"
)

// AsRawFd is `impl AsRawFd for File`.
func (f *File) AsRawFd() rsio.RawFd {
	defer runtime.KeepAlive(f)
	return rsio.RawFd(f.inner.rawFd())
}

// AsFd is `impl AsFd for File`.
func (f *File) AsFd() rsio.BorrowedFd {
	return rsio.BorrowedFdBorrowRaw(f.AsRawFd())
}

// released keeps the *os.File of every File given up with IntoRawFd, so that
// the garbage collector never closes a descriptor that has a new owner.
var released struct {
	sync.Mutex
	files []*os.File
}

// IntoRawFd is `impl IntoRawFd for File`: the descriptor, which the caller
// owns from then on. The File is empty afterwards.
func (f *File) IntoRawFd() rsio.RawFd {
	fd := f.inner.rawFd()
	released.Lock()
	released.files = append(released.files, f.inner.file)
	released.Unlock()
	f.inner.file = nil
	f.inner.fd = -1
	return rsio.RawFd(fd)
}

// FileFromRawFd is `impl FromRawFd for File`: a File that owns fd.
func FileFromRawFd(fd rsio.RawFd) *File {
	return &File{inner: newSysFile(int(fd), "")}
}

// FileFromOwnedFd is `impl From<OwnedFd> for File`.
func FileFromOwnedFd(fd *rsio.OwnedFd) *File {
	return FileFromRawFd(fd.IntoRawFd())
}

// IsTerminal is `impl IsTerminal for File`.
func (f *File) IsTerminal() bool {
	return rsio.FdIsTerminal(f.AsRawFd())
}

// OsFile is the *os.File that owns the descriptor, for Go code that needs
// one (os/exec, for one). It stays owned by the File: do not close it, and
// do not use it after the File is dropped.
func (f *File) OsFile() *os.File {
	f.inner.rawFd()
	return f.inner.file
}
