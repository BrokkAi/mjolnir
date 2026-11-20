// Port of core 1.99.1 src/io/error.rs, alloc 1.99.1 src/io/error.rs and
// std 1.99.1 src/io/error.rs

package rsio

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// RawOsError is Rust's io::RawOsError: the type of an operating system error
// code (errno on Unix, the GetLastError code on Windows).
type RawOsError = int32

// Error is Rust's io::Error. It is one of four things, as in Rust: an
// operating system error code, a bare ErrorKind, a kind with a static
// message, or a kind with an inner error ("custom").
//
// A value is always held as *Error and implements Go's error. Functions that
// can fail return it in an `error`; KindOf and From recover it.
//
// Error and String give Rust's Display text and DebugString its Debug text.
type Error struct {
	repr errorRepr
	// Exactly one of the following is meaningful, as repr says.
	code    RawOsError
	kind    ErrorKind
	message string
	custom  error
}

type errorRepr uint8

const (
	// reprSimple is ErrorData::Simple. It is the zero value, so that a zero
	// Error is a kind-only error rather than something that cannot print.
	reprSimple errorRepr = iota
	reprOs
	reprSimpleMessage
	reprCustom
)

// stringError is alloc's private StringError: the error a string turns into
// when it is given to Error::new. Display is the string and Debug is the
// string's Debug.
type stringError string

func (s stringError) Error() string { return string(s) }

// String is the Display text.
func (s stringError) String() string { return string(s) }

// DebugString is the Debug text: the string's own.
func (s stringError) DebugString() string { return debugStr(string(s)) }

// FmtDisplay is `impl Display for StringError`.
func (s stringError) FmtDisplay(f *rs.Formatter) error { return rs.DisplayFmt(string(s), f) }

// FmtDebug is `impl Debug for StringError`.
func (s stringError) FmtDebug(f *rs.Formatter) error { return rs.DebugFmt(string(s), f) }

// The errors std builds with const_error! and shares between its modules
// (`Error::INVALID_UTF8` and so on). Each is a kind with a static message.
var (
	errInvalidUtf8         = ErrorFromStaticMessage(ErrorKind_InvalidData, "stream did not contain valid UTF-8")
	errReadExactEof        = ErrorFromStaticMessage(ErrorKind_UnexpectedEof, "failed to fill whole buffer")
	errUnknownThreadCount  = ErrorFromStaticMessage(ErrorKind_NotFound, "the number of hardware threads is not known for the target platform")
	errUnsupportedPlatform = ErrorFromStaticMessage(ErrorKind_Unsupported, "operation not supported on this platform")
	errWriteAllEof         = ErrorFromStaticMessage(ErrorKind_WriteZero, "failed to write whole buffer")
	errZeroTimeout         = ErrorFromStaticMessage(ErrorKind_InvalidInput, "cannot set a 0 duration timeout")
	errNoAddresses         = ErrorFromStaticMessage(ErrorKind_InvalidInput, "could not resolve to any addresses")
)

// ErrorInvalidUtf8 is Error::INVALID_UTF8: what read_to_string and read_line
// report for bytes that are not UTF-8.
func ErrorInvalidUtf8() *Error { return errInvalidUtf8 }

// ErrorReadExactEof is Error::READ_EXACT_EOF: what read_exact reports when
// the reader ends early.
func ErrorReadExactEof() *Error { return errReadExactEof }

// ErrorUnknownThreadCount is Error::UNKNOWN_THREAD_COUNT.
func ErrorUnknownThreadCount() *Error { return errUnknownThreadCount }

// ErrorUnsupportedPlatform is Error::UNSUPPORTED_PLATFORM.
func ErrorUnsupportedPlatform() *Error { return errUnsupportedPlatform }

// ErrorWriteAllEof is Error::WRITE_ALL_EOF: what write_all reports when the
// writer accepts no more bytes.
func ErrorWriteAllEof() *Error { return errWriteAllEof }

// ErrorZeroTimeout is Error::ZERO_TIMEOUT.
func ErrorZeroTimeout() *Error { return errZeroTimeout }

// ErrorNoAddresses is Error::NO_ADDRESSES.
func ErrorNoAddresses() *Error { return errNoAddresses }

// ErrorFromStaticMessage is Error::from_static_message, which is what
// `io::const_error!(kind, "message")` expands to: an error that is a kind and
// a fixed message. Its Debug form is `Error { kind: .., message: ".." }`.
func ErrorFromStaticMessage(kind ErrorKind, message string) *Error {
	return &Error{repr: reprSimpleMessage, kind: kind, message: message}
}

// ErrorFrom is `io::Error::from(kind)` and `kind.into()`: an error that is
// only a kind. It prints as the kind's description.
func ErrorFrom(kind ErrorKind) *Error {
	return &Error{repr: reprSimple, kind: kind}
}

// ErrorFromErrorKind is `impl From<ErrorKind> for Error` under its rule name;
// it is ErrorFrom.
func ErrorFromErrorKind(kind ErrorKind) *Error { return ErrorFrom(kind) }

// ErrorNew is io::Error::new: an error of the given kind that carries an
// inner error. payload is what Rust accepts through
// `Into<Box<dyn Error + Send + Sync>>`: a string (which becomes an error
// whose Display is the string and whose Debug is the string's Debug) or an
// error value. Anything else does not compile in Rust and panics here.
func ErrorNew(kind ErrorKind, payload any) *Error {
	return &Error{repr: reprCustom, kind: kind, custom: customPayload(payload)}
}

// ErrorOther is io::Error::other: ErrorNew with ErrorKind_Other.
func ErrorOther(payload any) *Error {
	return ErrorNew(ErrorKind_Other, payload)
}

func customPayload(payload any) error {
	switch value := payload.(type) {
	case string:
		return stringError(value)
	case error:
		if value == nil {
			break
		}
		return value
	}
	rs.PanicAt(2, "rsio: the payload of an io::Error must be a string or an error, not "+typeName(payload))
	return nil
}

// ErrorFromRawOsError is io::Error::from_raw_os_error: the error for an
// operating system error code. Its kind comes from the code
// (decode_error_kind) and its text is the C library's, or on Windows the
// system's, followed by " (os error N)".
func ErrorFromRawOsError(code RawOsError) *Error {
	return &Error{repr: reprOs, code: code}
}

// ErrorLastOsError stands in for io::Error::last_os_error, which reads errno
// (GetLastError on Windows) after a failed call to the C library. A Go
// program has no errno: the call itself returns the code. So this takes the
// error that call returned, and gives the io::Error Rust would have read.
// It is From, except that a nil error is code 0, as errno would have been.
//
//	if err := unix.Kill(pid, 0); err != nil {
//		return rsio.ErrorLastOsError(err)
//	}
func ErrorLastOsError(err error) *Error {
	if err == nil {
		return ErrorFromRawOsError(0)
	}
	return From(err)
}

// ErrorFromNulError is `impl From<alloc::ffi::NulError> for io::Error`.
func ErrorFromNulError() *Error {
	return ErrorFromStaticMessage(ErrorKind_InvalidInput, "data provided contains a nul byte")
}

// ErrorFromTryReserveError is `impl From<TryReserveError> for io::Error`.
func ErrorFromTryReserveError() *Error {
	return ErrorFrom(ErrorKind_OutOfMemory)
}

// RawOsError is Error::raw_os_error: the operating system code, when the
// error is one.
func (e *Error) RawOsError() rs.Option[RawOsError] {
	if e.repr == reprOs {
		return rs.Some(e.code)
	}
	return rs.None[RawOsError]()
}

// GetRef is Error::get_ref: the inner error of a custom error.
func (e *Error) GetRef() rs.Option[error] {
	if e.repr == reprCustom {
		return rs.Some(e.custom)
	}
	return rs.None[error]()
}

// GetMut is Error::get_mut. Go holds the inner error by reference already,
// so it is GetRef.
func (e *Error) GetMut() rs.Option[error] { return e.GetRef() }

// IntoInner is Error::into_inner: the inner error of a custom error.
func (e *Error) IntoInner() rs.Option[error] { return e.GetRef() }

// Kind is Error::kind.
func (e *Error) Kind() ErrorKind {
	switch e.repr {
	case reprOs:
		return decodeErrorKind(e.code)
	case reprCustom, reprSimple, reprSimpleMessage:
		return e.kind
	default:
		panic(rs.Unreachable(e.repr))
	}
}

// IsInterrupted is Error::is_interrupted, which std uses to decide whether to
// retry a read or a write.
func (e *Error) IsInterrupted() bool {
	switch e.repr {
	case reprOs:
		return isInterrupted(e.code)
	case reprCustom, reprSimple, reprSimpleMessage:
		return e.kind == ErrorKind_Interrupted
	default:
		panic(rs.Unreachable(e.repr))
	}
}

// ErrorDowncast is Error::downcast::<E>: the inner error when the error is
// custom and its inner error has the type E, and otherwise the error itself
// as the second result.
func ErrorDowncast[E error](e *Error) (E, *Error) {
	if e.repr == reprCustom {
		if inner, ok := e.custom.(E); ok {
			return inner, nil
		}
	}
	var zero E
	return zero, e
}

// Error is the Display text, which makes *Error a Go error.
func (e *Error) Error() string { return e.String() }

// String is `impl Display for Error`.
func (e *Error) String() string {
	switch e.repr {
	case reprOs:
		return errorString(e.code) + " (os error " + strconv.FormatInt(int64(e.code), 10) + ")"
	case reprCustom:
		return e.custom.Error()
	case reprSimple:
		return e.kind.asStr()
	case reprSimpleMessage:
		return e.message
	default:
		panic(rs.Unreachable(e.repr))
	}
}

// FmtDisplay is `impl Display for Error`. Only the static message and the
// inner error see the formatter's flags; the other two forms are written
// as they are, as in Rust.
func (e *Error) FmtDisplay(f *rs.Formatter) error {
	switch e.repr {
	case reprOs, reprSimple:
		return f.WriteStr(e.String())
	case reprCustom:
		return rs.DisplayFmt(e.custom, f)
	case reprSimpleMessage:
		return f.Pad(e.message)
	default:
		panic(rs.Unreachable(e.repr))
	}
}

// DebugString is `format!("{:?}", error)`:
//
//	Os { code: 2, kind: NotFound, message: "No such file or directory" }
//	Kind(NotFound)
//	Error { kind: InvalidData, message: "stream did not contain valid UTF-8" }
//	Custom { kind: Other, error: "the inner error's Debug" }
func (e *Error) DebugString() string {
	switch e.repr {
	case reprOs:
		// The message is written between quotes as it is, not escaped.
		return "Os { code: " + strconv.FormatInt(int64(e.code), 10) +
			", kind: " + decodeErrorKind(e.code).DebugString() +
			", message: \"" + errorString(e.code) + "\" }"
	case reprCustom:
		return "Custom { kind: " + e.kind.DebugString() + ", error: " + debugOfError(e.custom) + " }"
	case reprSimple:
		return "Kind(" + e.kind.DebugString() + ")"
	case reprSimpleMessage:
		return "Error { kind: " + e.kind.DebugString() + ", message: " + debugStr(e.message) + " }"
	default:
		panic(rs.Unreachable(e.repr))
	}
}

// osErrorMessage is the "message" field of the Os form: Debug writes the
// text between quotes without escaping it.
type osErrorMessage string

func (m osErrorMessage) FmtDebug(f *rs.Formatter) error {
	return f.WriteStr("\"" + string(m) + "\"")
}

// FmtDebug is `impl Debug for Error` (through its Repr).
func (e *Error) FmtDebug(f *rs.Formatter) error {
	switch e.repr {
	case reprOs:
		return f.DebugStruct("Os").
			Field("code", e.code).
			Field("kind", decodeErrorKind(e.code)).
			Field("message", osErrorMessage(errorString(e.code))).
			Finish()
	case reprCustom:
		return f.DebugStruct("Custom").Field("kind", e.kind).Field("error", e.custom).Finish()
	case reprSimple:
		return f.DebugTuple("Kind").Field(e.kind).Finish()
	case reprSimpleMessage:
		return f.DebugStruct("Error").Field("kind", e.kind).Field("message", e.message).Finish()
	default:
		panic(rs.Unreachable(e.repr))
	}
}

// Source is `Error::source`: for a custom error the source of its inner
// error, and otherwise nothing. The inner error itself is not the source:
// the custom error already prints as it.
func (e *Error) Source() error {
	if e.repr != reprCustom {
		return nil
	}
	return sourceOf(e.custom)
}

// Unwrap gives Go's error chain what Rust's gives: Source.
func (e *Error) Unwrap() error { return e.Source() }

func sourceOf(err error) error {
	switch inner := err.(type) {
	case interface{ Source() error }:
		return inner.Source()
	case interface{ Unwrap() error }:
		return inner.Unwrap()
	}
	return nil
}

// debugOfError is the Debug text of an inner error.
func debugOfError(err error) string {
	if inner, ok := err.(interface{ DebugString() string }); ok {
		return inner.DebugString()
	}
	return rs.DebugString(err)
}

// ErrorKind is Rust's io::ErrorKind. The order is the declaration order,
// which is also how the kinds compare.
type ErrorKind uint8

const (
	ErrorKind_NotFound ErrorKind = iota
	ErrorKind_PermissionDenied
	ErrorKind_ConnectionRefused
	ErrorKind_ConnectionReset
	ErrorKind_HostUnreachable
	ErrorKind_NetworkUnreachable
	ErrorKind_ConnectionAborted
	ErrorKind_NotConnected
	ErrorKind_AddrInUse
	ErrorKind_AddrNotAvailable
	ErrorKind_NetworkDown
	ErrorKind_BrokenPipe
	ErrorKind_AlreadyExists
	ErrorKind_WouldBlock
	ErrorKind_NotADirectory
	ErrorKind_IsADirectory
	ErrorKind_DirectoryNotEmpty
	ErrorKind_ReadOnlyFilesystem
	ErrorKind_FilesystemLoop
	ErrorKind_StaleNetworkFileHandle
	ErrorKind_InvalidInput
	ErrorKind_InvalidData
	ErrorKind_TimedOut
	ErrorKind_WriteZero
	ErrorKind_StorageFull
	ErrorKind_NotSeekable
	ErrorKind_QuotaExceeded
	ErrorKind_FileTooLarge
	ErrorKind_ResourceBusy
	ErrorKind_ExecutableFileBusy
	ErrorKind_Deadlock
	ErrorKind_CrossesDevices
	ErrorKind_TooManyLinks
	ErrorKind_InvalidFilename
	ErrorKind_ArgumentListTooLong
	ErrorKind_Interrupted
	ErrorKind_Unsupported
	ErrorKind_UnexpectedEof
	ErrorKind_OutOfMemory
	ErrorKind_InProgress
	ErrorKind_TooManyOpenFiles
	ErrorKind_InputOutputError
	ErrorKind_Other
	ErrorKind_Uncategorized
)

// errorKindNames holds each variant's Rust name (its derived Debug) and its
// description (ErrorKind::as_str, its Display).
var errorKindNames = [...]struct{ name, description string }{
	ErrorKind_NotFound:               {"NotFound", "entity not found"},
	ErrorKind_PermissionDenied:       {"PermissionDenied", "permission denied"},
	ErrorKind_ConnectionRefused:      {"ConnectionRefused", "connection refused"},
	ErrorKind_ConnectionReset:        {"ConnectionReset", "connection reset"},
	ErrorKind_HostUnreachable:        {"HostUnreachable", "host unreachable"},
	ErrorKind_NetworkUnreachable:     {"NetworkUnreachable", "network unreachable"},
	ErrorKind_ConnectionAborted:      {"ConnectionAborted", "connection aborted"},
	ErrorKind_NotConnected:           {"NotConnected", "not connected"},
	ErrorKind_AddrInUse:              {"AddrInUse", "address in use"},
	ErrorKind_AddrNotAvailable:       {"AddrNotAvailable", "address not available"},
	ErrorKind_NetworkDown:            {"NetworkDown", "network down"},
	ErrorKind_BrokenPipe:             {"BrokenPipe", "broken pipe"},
	ErrorKind_AlreadyExists:          {"AlreadyExists", "entity already exists"},
	ErrorKind_WouldBlock:             {"WouldBlock", "operation would block"},
	ErrorKind_NotADirectory:          {"NotADirectory", "not a directory"},
	ErrorKind_IsADirectory:           {"IsADirectory", "is a directory"},
	ErrorKind_DirectoryNotEmpty:      {"DirectoryNotEmpty", "directory not empty"},
	ErrorKind_ReadOnlyFilesystem:     {"ReadOnlyFilesystem", "read-only filesystem or storage medium"},
	ErrorKind_FilesystemLoop:         {"FilesystemLoop", "filesystem loop or indirection limit (e.g. symlink loop)"},
	ErrorKind_StaleNetworkFileHandle: {"StaleNetworkFileHandle", "stale network file handle"},
	ErrorKind_InvalidInput:           {"InvalidInput", "invalid input parameter"},
	ErrorKind_InvalidData:            {"InvalidData", "invalid data"},
	ErrorKind_TimedOut:               {"TimedOut", "timed out"},
	ErrorKind_WriteZero:              {"WriteZero", "write zero"},
	ErrorKind_StorageFull:            {"StorageFull", "no storage space"},
	ErrorKind_NotSeekable:            {"NotSeekable", "seek on unseekable file"},
	ErrorKind_QuotaExceeded:          {"QuotaExceeded", "quota exceeded"},
	ErrorKind_FileTooLarge:           {"FileTooLarge", "file too large"},
	ErrorKind_ResourceBusy:           {"ResourceBusy", "resource busy"},
	ErrorKind_ExecutableFileBusy:     {"ExecutableFileBusy", "executable file busy"},
	ErrorKind_Deadlock:               {"Deadlock", "deadlock"},
	ErrorKind_CrossesDevices:         {"CrossesDevices", "cross-device link or rename"},
	ErrorKind_TooManyLinks:           {"TooManyLinks", "too many links"},
	ErrorKind_InvalidFilename:        {"InvalidFilename", "invalid filename"},
	ErrorKind_ArgumentListTooLong:    {"ArgumentListTooLong", "argument list too long"},
	ErrorKind_Interrupted:            {"Interrupted", "operation interrupted"},
	ErrorKind_Unsupported:            {"Unsupported", "unsupported"},
	ErrorKind_UnexpectedEof:          {"UnexpectedEof", "unexpected end of file"},
	ErrorKind_OutOfMemory:            {"OutOfMemory", "out of memory"},
	ErrorKind_InProgress:             {"InProgress", "in progress"},
	ErrorKind_TooManyOpenFiles:       {"TooManyOpenFiles", "too many open files"},
	ErrorKind_InputOutputError:       {"InputOutputError", "input/output error"},
	ErrorKind_Other:                  {"Other", "other error"},
	ErrorKind_Uncategorized:          {"Uncategorized", "uncategorized error"},
}

var _ = rs.RegisterEnum[ErrorKind]("ErrorKind", errorKindVariants()...)

func errorKindVariants() []rs.VariantSpec {
	variants := make([]rs.VariantSpec, 0, len(errorKindNames))
	for kind, names := range errorKindNames {
		variants = append(variants, rs.Const(ErrorKind(kind), names.name))
	}
	return variants
}

func (k ErrorKind) asStr() string {
	if int(k) >= len(errorKindNames) {
		panic(rs.Unreachable(uint8(k)))
	}
	return errorKindNames[k].description
}

// String is `impl Display for ErrorKind`: the kind's description, such as
// "entity not found".
func (k ErrorKind) String() string { return k.asStr() }

// FmtDisplay is `impl Display for ErrorKind`. It writes the description
// without padding, as Rust does.
func (k ErrorKind) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(k.asStr()) }

// DebugString is the derived Debug: the variant's name, such as "NotFound".
func (k ErrorKind) DebugString() string {
	if int(k) >= len(errorKindNames) {
		panic(rs.Unreachable(uint8(k)))
	}
	return errorKindNames[k].name
}
