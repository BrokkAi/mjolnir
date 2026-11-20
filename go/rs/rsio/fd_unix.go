// Port of std 1.99.1 src/os/fd/owned.rs and src/os/fd/raw.rs

//go:build unix

package rsio

import (
	"strconv"
	"syscall"

	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// std::os::fd lives here, in the package every other one that holds a file
// descriptor already imports: rsfs.File, the standard streams, sockets and
// child process pipes all give out the types below.

// RawFd is std::os::fd::RawFd: a file descriptor as a number.
type RawFd = int32

// AsRawFd is the trait std::os::fd::AsRawFd.
type AsRawFd interface {
	AsRawFd() RawFd
}

// IntoRawFd is the trait std::os::fd::IntoRawFd: the value gives up its
// descriptor, and no longer closes it.
type IntoRawFd interface {
	IntoRawFd() RawFd
}

// AsFd is the trait std::os::fd::AsFd.
type AsFd interface {
	AsFd() BorrowedFd
}

// BorrowedFd is std::os::fd::BorrowedFd: a file descriptor that someone else
// owns and closes.
type BorrowedFd struct {
	fd RawFd
}

// BorrowedFdBorrowRaw is BorrowedFd::borrow_raw.
func BorrowedFdBorrowRaw(fd RawFd) BorrowedFd {
	if fd == -1 {
		rs.PanicAt(1, "fd != -1")
	}
	return BorrowedFd{fd: fd}
}

// TryCloneToOwned is BorrowedFd::try_clone_to_owned: a new descriptor for
// the same open file, with close-on-exec set, numbered 3 or above.
func (b BorrowedFd) TryCloneToOwned() (*OwnedFd, error) {
	// We want to atomically duplicate this file descriptor and set the
	// CLOEXEC flag, and currently that's done via F_DUPFD_CLOEXEC. This
	// is a POSIX flag that was added to Linux in 2.6.24.
	//
	// Avoid using file descriptors below 3 as they are used for stdio
	fd, err := unix.FcntlInt(uintptr(b.fd), unix.F_DUPFD_CLOEXEC, 3)
	if err != nil {
		return nil, From(err)
	}
	return OwnedFdFromRawFd(RawFd(fd)), nil
}

// AsRawFd is AsRawFd::as_raw_fd.
func (b BorrowedFd) AsRawFd() RawFd { return b.fd }

// AsFd is AsFd::as_fd.
func (b BorrowedFd) AsFd() BorrowedFd { return b }

// IsTerminal is IsTerminal::is_terminal.
func (b BorrowedFd) IsTerminal() bool { return FdIsTerminal(b.fd) }

// DebugString is `impl Debug for BorrowedFd`.
func (b BorrowedFd) DebugString() string {
	return "BorrowedFd { fd: " + strconv.Itoa(int(b.fd)) + " }"
}

// FmtDebug is `impl Debug for BorrowedFd`.
func (b BorrowedFd) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("BorrowedFd").Field("fd", b.fd).Finish()
}

// OwnedFd is std::os::fd::OwnedFd: a file descriptor that this value owns.
// Drop closes it.
type OwnedFd struct {
	fd RawFd
	// released is set once the descriptor was closed or given away.
	released bool
}

// OwnedFdFromRawFd is OwnedFd::from_raw_fd: the value takes ownership of an
// open descriptor.
func OwnedFdFromRawFd(fd RawFd) *OwnedFd {
	if fd == -1 {
		rs.PanicAt(1, "fd != -1")
	}
	return &OwnedFd{fd: fd}
}

// OwnedFdFrom is `OwnedFd::from(value)` for anything that can give up its
// descriptor: a File, a socket, a pipe end.
func OwnedFdFrom(value IntoRawFd) *OwnedFd {
	return OwnedFdFromRawFd(value.IntoRawFd())
}

// TryClone is OwnedFd::try_clone.
func (o *OwnedFd) TryClone() (*OwnedFd, error) { return o.AsFd().TryCloneToOwned() }

// AsRawFd is AsRawFd::as_raw_fd.
func (o *OwnedFd) AsRawFd() RawFd {
	o.check()
	return o.fd
}

// AsFd is AsFd::as_fd.
func (o *OwnedFd) AsFd() BorrowedFd {
	o.check()
	return BorrowedFd{fd: o.fd}
}

// IntoRawFd is IntoRawFd::into_raw_fd: the caller now owns the descriptor.
func (o *OwnedFd) IntoRawFd() RawFd {
	o.check()
	o.released = true
	return o.fd
}

func (o *OwnedFd) check() {
	if o.released {
		rs.PanicAt(2, "rsio: use of an OwnedFd after it was dropped or given away")
	}
}

// IsTerminal is IsTerminal::is_terminal.
func (o *OwnedFd) IsTerminal() bool { return FdIsTerminal(o.AsRawFd()) }

// Drop is `impl Drop for OwnedFd`: it closes the descriptor. A second call,
// and a call after IntoRawFd, does nothing.
func (o *OwnedFd) Drop() {
	if o.released {
		return
	}
	o.released = true
	// Note that errors are ignored when closing a file descriptor. According to POSIX 2024,
	// we can and indeed should retry `close` on `EINTR`
	// (https://pubs.opengroup.org/onlinepubs/9799919799/functions/close.html),
	// but it is not clear yet how well widely-used implementations are conforming with this
	// mandate since older versions of POSIX left the state of the FD after an `EINTR`
	// unspecified. Ignoring errors is "fine" because some of the major Unices (in
	// particular, Linux) do make sure to always close the FD, even when `close()` is
	// interrupted, and the scenario is rare to begin with. If we retried on a
	// not-POSIX-compliant implementation, the consequences could be really bad since we may
	// close the wrong FD. Helpful link to an epic discussion by POSIX workgroup that led to
	// the latest POSIX wording: http://austingroupbugs.net/view.php?id=529
	_ = syscall.Close(int(o.fd))
}

// DebugString is `impl Debug for OwnedFd`.
func (o *OwnedFd) DebugString() string {
	return "OwnedFd { fd: " + strconv.Itoa(int(o.fd)) + " }"
}

// FmtDebug is `impl Debug for OwnedFd`.
func (o *OwnedFd) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("OwnedFd").Field("fd", o.fd).Finish()
}

// AsRawFd is `impl AsRawFd for Stdin`.
func (s *StdinHandle) AsRawFd() RawFd { return stdinFileno }

// AsFd is `impl AsFd for Stdin`.
func (s *StdinHandle) AsFd() BorrowedFd { return BorrowedFd{fd: stdinFileno} }

// AsRawFd is `impl AsRawFd for StdinLock`.
func (l *StdinLock) AsRawFd() RawFd { return stdinFileno }

// AsFd is `impl AsFd for StdinLock`.
func (l *StdinLock) AsFd() BorrowedFd { return BorrowedFd{fd: stdinFileno} }

// AsRawFd is `impl AsRawFd for Stdout`.
func (s *StdoutHandle) AsRawFd() RawFd { return stdoutFileno }

// AsFd is `impl AsFd for Stdout`.
func (s *StdoutHandle) AsFd() BorrowedFd { return BorrowedFd{fd: stdoutFileno} }

// AsRawFd is `impl AsRawFd for StdoutLock`.
func (l *StdoutLock) AsRawFd() RawFd { return stdoutFileno }

// AsFd is `impl AsFd for StdoutLock`.
func (l *StdoutLock) AsFd() BorrowedFd { return BorrowedFd{fd: stdoutFileno} }

// AsRawFd is `impl AsRawFd for Stderr`.
func (s *StderrHandle) AsRawFd() RawFd { return stderrFileno }

// AsFd is `impl AsFd for Stderr`.
func (s *StderrHandle) AsFd() BorrowedFd { return BorrowedFd{fd: stderrFileno} }

// AsRawFd is `impl AsRawFd for StderrLock`.
func (l *StderrLock) AsRawFd() RawFd { return stderrFileno }

// AsFd is `impl AsFd for StderrLock`.
func (l *StderrLock) AsFd() BorrowedFd { return BorrowedFd{fd: stderrFileno} }
