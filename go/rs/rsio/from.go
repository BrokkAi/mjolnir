package rsio

import (
	"errors"
	"io"
	"io/fs"
	"net"
	"os"
	"syscall"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// From gives the io::Error Rust would have produced for a failure that a Go
// package reported in its own way. It is how the packages that stand in for
// Rust's standard library, and product code that calls
// golang.org/x/sys/unix or golang.org/x/sys/windows directly, turn an error
// of os, syscall, net or io into Rust's:
//
//   - an *Error is returned as it is;
//   - the wrappers Go puts around a system error to name the operation and
//     the path (*fs.PathError, *os.LinkError, *os.SyscallError, *net.OpError)
//     are removed, because Rust's error names neither: `open /x: no such file
//     or directory` becomes `No such file or directory (os error 2)`;
//   - a syscall.Errno is the operating system error with that code;
//   - an expired deadline is what Rust gets from a socket with a timeout: the
//     code for "would block" on Unix and WSAETIMEDOUT on Windows;
//   - a use of a closed file is the code the system gives for a descriptor
//     that is not open (EBADF, ERROR_INVALID_HANDLE);
//   - io.EOF and io.ErrUnexpectedEOF are read_exact's error (UnexpectedEof,
//     "failed to fill whole buffer"), the one place where Rust reports the
//     end of input as an error. Reading functions that return 0 at the end
//     of input, as Rust's read does, must not pass io.EOF here;
//   - io.ErrShortWrite is write_all's error (WriteZero, "failed to write
//     whole buffer");
//   - the bare sentinels of io/fs are the matching kind alone;
//   - any other error becomes the inner error of an error of kind Other, as
//     io::Error::other(err) would make it.
//
// From(nil) is nil. Do not store that in an `error`: a nil *Error in an
// error interface is not a nil error.
func From(err error) *Error {
	for err != nil {
		switch value := err.(type) {
		case *Error:
			return value
		case syscall.Errno:
			return fromErrno(value)
		case *fs.PathError:
			err = value.Err
			continue
		case *os.LinkError:
			err = value.Err
			continue
		case *os.SyscallError:
			err = value.Err
			continue
		case *net.OpError:
			err = value.Err
			continue
		}
		break
	}
	if err == nil {
		return nil
	}

	switch {
	case err == io.EOF, err == io.ErrUnexpectedEOF:
		return errReadExactEof
	case err == io.ErrShortWrite:
		return errWriteAllEof
	case err == io.ErrClosedPipe:
		return ErrorNew(ErrorKind_BrokenPipe, err)
	case err == fs.ErrNotExist:
		return ErrorFrom(ErrorKind_NotFound)
	case err == fs.ErrExist:
		return ErrorFrom(ErrorKind_AlreadyExists)
	case err == fs.ErrPermission:
		return ErrorFrom(ErrorKind_PermissionDenied)
	case err == fs.ErrInvalid:
		return ErrorFrom(ErrorKind_InvalidInput)
	case errors.Is(err, os.ErrDeadlineExceeded):
		return ErrorFromRawOsError(deadlineExceededCode)
	case errors.Is(err, fs.ErrClosed), errors.Is(err, net.ErrClosed):
		return ErrorFromRawOsError(closedDescriptorCode)
	}
	return ErrorNew(ErrorKind_Other, err)
}

// KindOf is `error.kind()` for an io::Error held as a Go error. An error
// that is not yet an *Error is converted with From first, so the kind of an
// error straight from os or syscall is the kind Rust would report.
func KindOf(err error) ErrorKind {
	if err == nil {
		rs.PanicAt(1, "rsio.KindOf: the error is nil")
	}
	return From(err).Kind()
}
