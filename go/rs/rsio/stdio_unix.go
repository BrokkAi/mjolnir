// Port of std 1.99.1 src/sys/stdio/unix.rs and src/sys/io/is_terminal/isatty.rs

//go:build unix

package rsio

import (
	"syscall"

	"golang.org/x/sys/unix"
)

// stdinBufSize is sys::stdio::STDIN_BUF_SIZE.
const stdinBufSize = DefaultBufSize

// The standard streams are the descriptors 0, 1 and 2, whatever os.Stdin and
// its siblings have been set to, as in Rust.
const (
	stdinFileno  = 0
	stdoutFileno = 1
	stderrFileno = 2
)

func stdinRead(buf []byte) (int, error) { return fdRead(stdinFileno, buf) }

func stdoutWrite(buf []byte) (int, error) { return fdWrite(stdoutFileno, buf) }

func stderrWrite(buf []byte) (int, error) { return fdWrite(stderrFileno, buf) }

// isEbadf is sys::stdio::is_ebadf.
func isEbadf(err error) bool {
	code, ok := From(err).RawOsError().Get()
	return ok && code == RawOsError(syscall.EBADF)
}

// stdioIsTerminal is sys::io::is_terminal for a standard stream.
func stdioIsTerminal(fd int) bool { return FdIsTerminal(RawFd(fd)) }

// FdIsTerminal is sys::io::is_terminal: libc's isatty, which asks the
// descriptor for its terminal attributes.
func FdIsTerminal(fd RawFd) bool {
	_, err := unix.IoctlGetTermios(int(fd), ioctlReadTermios)
	return err == nil
}

// fdRead is FileDesc::read: one read(2) on a descriptor.
//
// A read that a signal interrupted is made again here. Rust returns the
// interruption (ErrorKind::Interrupted) and its callers retry; but a Go
// program receives signals a Rust program does not (the runtime preempts
// goroutines with them), so the retry is done where the Go runtime itself
// does it.
func fdRead(fd int, buf []byte) (int, error) {
	if len(buf) > readLimit {
		buf = buf[:readLimit]
	}
	for {
		n, err := unix.Read(fd, buf)
		if err == unix.EINTR {
			continue
		}
		if err != nil {
			return 0, fromErrno(err.(syscall.Errno))
		}
		return n, nil
	}
}

// fdWrite is FileDesc::write: one write(2) on a descriptor, which may write
// less than buf. See fdRead for interruptions.
func fdWrite(fd int, buf []byte) (int, error) {
	if len(buf) > readLimit {
		buf = buf[:readLimit]
	}
	for {
		n, err := unix.Write(fd, buf)
		if err == unix.EINTR {
			continue
		}
		if err != nil {
			return 0, fromErrno(err.(syscall.Errno))
		}
		return n, nil
	}
}
