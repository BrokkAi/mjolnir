// Port of mj-core/src/logging.rs.

// Diagnostic output must never turn an unavailable disk into a process
// failure.

package core

import (
	"errors"
	"fmt"
	"io"
	"math"
	"os"
	"runtime"
	"runtime/debug"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
	"unicode"
	"unicode/utf8"
)

const recentLogBytes = 12 * 1024

// DroppedLines is one interval of unavailable diagnostic output, owned by its
// writer.
type DroppedLines struct {
	Count   uint64
	FirstAt string
	LastAt  string
	// Kind is the failure's kind as Rust's io::ErrorKind Debug form names it
	// (`StorageFull`), which the summary line shows.
	Kind  string
	Error string
}

// resilientWriterState is Rust's private logging::State.
type resilientWriterState struct {
	mu          sync.Mutex
	writer      io.Writer
	dropped     *DroppedLines
	recent      []string
	recentBytes int
}

// ResilientWriter serializes lines, failure accounting and a bounded tail at
// one decision point. Write errors consume/drop the line and return success
// to the logger, preventing its own stderr error path. A later line tries
// output again. Copies share one state, as Rust's clones do.
type ResilientWriter struct {
	state *resilientWriterState
}

func NewResilientWriter(writer io.Writer) ResilientWriter {
	return ResilientWriter{state: &resilientWriterState{writer: writer}}
}

func (w ResilientWriter) RecentLog() []string {
	w.state.mu.Lock()
	defer w.state.mu.Unlock()
	return append([]string(nil), w.state.recent...)
}

func (w ResilientWriter) DroppedLines() *DroppedLines {
	w.state.mu.Lock()
	defer w.state.mu.Unlock()
	if w.state.dropped == nil {
		return nil
	}
	dropped := *w.state.dropped
	return &dropped
}

func (s *resilientWriterState) remember(bytes []byte) {
	text := lossyUTF8(bytes)
	for len(text) > 0 {
		end := strings.IndexByte(text, '\n') + 1
		if end == 0 {
			end = len(text)
		}
		line := text[:end]
		text = text[end:]
		start := len(line) - min(len(line), recentLogBytes)
		for start < len(line) && !utf8.RuneStart(line[start]) {
			start++
		}
		line = strings.Clone(line[start:])
		s.recentBytes += len(line)
		s.recent = append(s.recent, line)
		for s.recentBytes > recentLogBytes && len(s.recent) > 0 {
			old := s.recent[0]
			s.recent = s.recent[1:]
			s.recentBytes -= len(old)
		}
	}
}

func (s *resilientWriterState) failed(err error, count uint64) {
	now := FormatChronoRFC3339(time.Now())
	kind, text := ioErrorKindName(err), ioErrorDisplay(err)
	if s.dropped != nil {
		if s.dropped.Count > math.MaxUint64-count {
			s.dropped.Count = math.MaxUint64
		} else {
			s.dropped.Count += count
		}
		s.dropped.LastAt = now
		s.dropped.Kind = kind
		s.dropped.Error = text
		return
	}
	s.dropped = &DroppedLines{Count: count, FirstAt: now, LastAt: now, Kind: kind, Error: text}
}

// Write records bytes, writing them out when the writer accepts them. It
// always reports success.
func (w ResilientWriter) Write(bytes []byte) (int, error) {
	if len(bytes) == 0 {
		return 0, nil
	}
	state := w.state
	state.mu.Lock()
	defer state.mu.Unlock()
	lines := uint64(max(strings.Count(string(bytes), "\n"), 1))
	if dropped := state.dropped; dropped != nil {
		summary := fmt.Sprintf("dropped %d log lines between %s and %s: %s (%s)\n",
			dropped.Count, dropped.FirstAt, dropped.LastAt, dropped.Error, dropped.Kind)
		if err := writeAll(state.writer, []byte(summary)); err != nil {
			state.remember(bytes)
			state.failed(err, lines)
			return len(bytes), nil
		}
		state.remember([]byte(summary))
		state.dropped = nil
	}
	state.remember(bytes)
	if err := writeAll(state.writer, bytes); err != nil {
		state.failed(err, lines)
	}
	return len(bytes), nil
}

// Flush flushes the writer when it can flush, recording a failure instead of
// returning it.
func (w ResilientWriter) Flush() error {
	w.state.mu.Lock()
	defer w.state.mu.Unlock()
	if flusher, ok := w.state.writer.(interface{ Flush() error }); ok {
		if err := flusher.Flush(); err != nil {
			w.state.failed(err, 0)
		}
	}
	return nil
}

// ioErrorKindName names err as Rust's io::ErrorKind Debug form does
// (`StorageFull` for ENOSPC), for the dropped-lines summary.
func ioErrorKindName(err error) string {
	var errno syscall.Errno
	if !errors.As(err, &errno) {
		return "Other"
	}
	if kind, ok := ioErrorKinds[errno]; ok {
		return kind
	}
	return "Uncategorized"
}

// PanicMessage is the message of a recovered panic, with the goroutine's
// stack when backtraces are explicitly enabled, without invoking stderr's
// default handler.
func PanicMessage(recovered any) string {
	var message string
	switch value := recovered.(type) {
	case error:
		message = value.Error()
	case string:
		message = value
	default:
		message = fmt.Sprint(value)
	}
	if file, line, ok := panicLocation(); ok {
		message = fmt.Sprintf("panicked at %s:%d:\n%s", file, line, message)
	} else {
		message = "panicked:\n" + message
	}
	if backtraceEnabled() {
		message += "\n" + string(debug.Stack())
	}
	return message
}

var (
	stderrWriterOnce  sync.Once
	stderrWriterValue ResilientWriter
	stdoutWriterOnce  sync.Once
	stdoutWriterValue ResilientWriter
)

// StderrWriter is the sink worker logging and direct diagnostics share,
// with its recent tail.
func StderrWriter() ResilientWriter {
	stderrWriterOnce.Do(func() { stderrWriterValue = NewResilientWriter(os.Stderr) })
	return stderrWriterValue
}

// StderrLine writes line and a newline to StderrWriter.
func StderrLine(line string) {
	_, _ = StderrWriter().Write([]byte(line + "\n"))
}

// StdoutLine writes line and a newline to the shared resilient stdout.
func StdoutLine(line string) {
	stdoutWriterOnce.Do(func() { stdoutWriterValue = NewResilientWriter(os.Stdout) })
	_, _ = stdoutWriterValue.Write([]byte(line + "\n"))
}

// DiagnosticStderr is Rust's diagnostic_stderr! macro.
func DiagnosticStderr(format string, args ...any) { StderrLine(fmt.Sprintf(format, args...)) }

// DiagnosticStdout is Rust's diagnostic_stdout! macro.
func DiagnosticStdout(format string, args ...any) { StdoutLine(fmt.Sprintf(format, args...)) }

// LogTimestampFormat is the timestamp format in log file names, as a Go time
// layout (Rust's chrono `%Y%m%dT%H%M%S%.3fZ`). It sorts as text in time
// order.
const LogTimestampFormat = "20060102T150405.000Z"

// crashedLogMark marks a daemon's log that was kept because the daemon did
// not shut down cleanly: `mj-daemon-<time>-<pid>.log` becomes
// `mj-daemon-<time>-<pid>.crashed.log`. Retention treats such logs as a kind
// of their own, so ordinary logs cannot push them out.
const crashedLogMark = ".crashed"

// CrashedLogName is the name `mj-daemon-<time>-<pid>.log` is kept under after
// a crash, or false when name is not an unmarked managed log name.
func CrashedLogName(name string) (string, bool) {
	stem, ok := strings.CutSuffix(name, ".log")
	if !ok || !strings.HasPrefix(name, "mj-") || strings.HasSuffix(stem, crashedLogMark) {
		return "", false
	}
	return stem + crashedLogMark + ".log", true
}

// WithoutCrashedMark is name without its crashed mark, and whether it had
// one. Everything that parses managed log names reads them through this, so
// a crashed log parses as the log it was.
func WithoutCrashedMark(name string) (string, bool) {
	if stem, ok := strings.CutSuffix(name, ".log"); ok {
		if stem, ok := strings.CutSuffix(stem, crashedLogMark); ok {
			return stem + ".log", true
		}
	}
	return name, false
}

// LogName is what a managed log file's name says. This is the one reader of
// the `mj-<kind>-<time>-<pid>[.crashed].log` shape; log retention and the
// previous-daemon report both read names through it.
type LogName struct {
	// Kind is the process kind label (`daemon`, `tui`, `cli`), or nil for the
	// older `mj-<time>-<pid>.log` names, which carried no kind.
	Kind    *string
	Started string
	// Pid is nil when the pid field does not parse.
	Pid     *uint32
	Crashed bool
}

// ParseLogName parses a managed log file name in either format, with or
// without the crashed mark, or returns nil when name is not one.
func ParseLogName(name string) *LogName {
	name, crashed := WithoutCrashedMark(name)
	stem, ok := strings.CutPrefix(name, "mj-")
	if !ok {
		return nil
	}
	if stem, ok = strings.CutSuffix(stem, ".log"); !ok {
		return nil
	}
	parts := strings.Split(stem, "-")
	parsed := LogName{Crashed: crashed}
	var pid string
	switch len(parts) {
	case 3:
		kind := parts[0]
		parsed.Kind, parsed.Started, pid = &kind, parts[1], parts[2]
	case 2:
		parsed.Started, pid = parts[0], parts[1]
	default:
		return nil
	}
	// Rust's u32 parse accepts one leading `+`.
	if value, err := strconv.ParseUint(strings.TrimPrefix(pid, "+"), 10, 32); err == nil {
		narrowed := uint32(value)
		parsed.Pid = &narrowed
	}
	return &parsed
}

// DaemonLogName is the start time and process of a daemon-kind log, read
// from its file name.
type DaemonLogName struct {
	Started string
	Pid     uint32
	Crashed bool
}

// ParseDaemonLogName parses `mj-daemon-<time>-<pid>.log`, with or without
// the crashed mark. The `mj node` process that serves a daemon writes the
// same kind of log.
func ParseDaemonLogName(name string) *DaemonLogName {
	parsed := ParseLogName(name)
	if parsed == nil || parsed.Kind == nil || *parsed.Kind != "daemon" || parsed.Pid == nil {
		return nil
	}
	return &DaemonLogName{Started: parsed.Started, Pid: *parsed.Pid, Crashed: parsed.Crashed}
}

// writeAll is Rust's write_all: a short write without an error is one.
func writeAll(writer io.Writer, bytes []byte) error {
	written, err := writer.Write(bytes)
	if err == nil && written < len(bytes) {
		return io.ErrShortWrite
	}
	return err
}

// ioErrorDisplay is the text Rust's io::Error shows: for an OS error the
// system's description and its number (`No space left on device (os error
// 28)`), otherwise the error itself.
func ioErrorDisplay(err error) string {
	var errno syscall.Errno
	if !errors.As(err, &errno) {
		return err.Error()
	}
	description := errno.Error()
	if description != "" {
		first, size := utf8.DecodeRuneInString(description)
		description = string(unicode.ToUpper(first)) + description[size:]
	}
	return fmt.Sprintf("%s (os error %d)", description, int(errno))
}

// ioErrorKinds names OS errors as Rust's io::ErrorKind Debug form does,
// following std's decode_error_kind.
var ioErrorKinds = map[syscall.Errno]string{
	syscall.E2BIG:         "ArgumentListTooLong",
	syscall.EADDRINUSE:    "AddrInUse",
	syscall.EADDRNOTAVAIL: "AddrNotAvailable",
	syscall.EBUSY:         "ResourceBusy",
	syscall.ECONNABORTED:  "ConnectionAborted",
	syscall.ECONNREFUSED:  "ConnectionRefused",
	syscall.ECONNRESET:    "ConnectionReset",
	syscall.EDEADLK:       "Deadlock",
	syscall.EDQUOT:        "QuotaExceeded",
	syscall.EEXIST:        "AlreadyExists",
	syscall.EFBIG:         "FileTooLarge",
	syscall.EHOSTUNREACH:  "HostUnreachable",
	syscall.EINTR:         "Interrupted",
	syscall.EINVAL:        "InvalidInput",
	syscall.EISDIR:        "IsADirectory",
	syscall.ELOOP:         "FilesystemLoop",
	syscall.ENOENT:        "NotFound",
	syscall.ENOMEM:        "OutOfMemory",
	syscall.ENOSPC:        "StorageFull",
	syscall.ENOSYS:        "Unsupported",
	syscall.EMLINK:        "TooManyLinks",
	syscall.ENAMETOOLONG:  "InvalidFilename",
	syscall.ENETDOWN:      "NetworkDown",
	syscall.ENETUNREACH:   "NetworkUnreachable",
	syscall.ENOTCONN:      "NotConnected",
	syscall.ENOTDIR:       "NotADirectory",
	syscall.ENOTEMPTY:     "DirectoryNotEmpty",
	syscall.EPIPE:         "BrokenPipe",
	syscall.EROFS:         "ReadOnlyFilesystem",
	syscall.ESPIPE:        "NotSeekable",
	syscall.ESTALE:        "StaleNetworkFileHandle",
	syscall.ETIMEDOUT:     "TimedOut",
	syscall.ETXTBSY:       "ExecutableFileBusy",
	syscall.EXDEV:         "CrossesDevices",
	syscall.EINPROGRESS:   "InProgress",
	syscall.EACCES:        "PermissionDenied",
	syscall.EPERM:         "PermissionDenied",
	syscall.EAGAIN:        "WouldBlock",
}

// lossyUTF8 is Rust's String::from_utf8_lossy: each maximal invalid
// sequence becomes one U+FFFD.
func lossyUTF8(bytes []byte) string {
	if utf8.Valid(bytes) {
		return string(bytes)
	}
	var text strings.Builder
	for len(bytes) > 0 {
		r, size := utf8.DecodeRune(bytes)
		if r != utf8.RuneError || size > 1 {
			text.Write(bytes[:size])
			bytes = bytes[size:]
			continue
		}
		text.WriteRune(utf8.RuneError)
		bytes = bytes[invalidUTF8PrefixLength(bytes):]
	}
	return text.String()
}

// invalidUTF8PrefixLength is how many bytes of an invalid sequence one
// replacement character stands for: the lead byte and the continuation
// bytes that could still have completed it.
func invalidUTF8PrefixLength(bytes []byte) int {
	lead := bytes[0]
	var need int
	low, high := byte(0x80), byte(0xBF)
	switch {
	case lead >= 0xC2 && lead <= 0xDF:
		need = 1
	case lead == 0xE0:
		need, low = 2, 0xA0
	case lead >= 0xE1 && lead <= 0xEC, lead == 0xEE, lead == 0xEF:
		need = 2
	case lead == 0xED:
		need, high = 2, 0x9F
	case lead == 0xF0:
		need, low = 3, 0x90
	case lead >= 0xF1 && lead <= 0xF3:
		need = 3
	case lead == 0xF4:
		need, high = 3, 0x8F
	default:
		return 1
	}
	length := 1
	for length <= need && length < len(bytes) {
		b := bytes[length]
		if b < low || b > high {
			break
		}
		low, high = 0x80, 0xBF
		length++
	}
	return length
}

// panicLocation is where the panic being recovered was raised: the first
// frame below the runtime's panic machinery.
func panicLocation() (string, int, bool) {
	pcs := make([]uintptr, 64)
	frames := runtime.CallersFrames(pcs[:runtime.Callers(1, pcs)])
	panicking := false
	for {
		frame, more := frames.Next()
		if panicking && !strings.HasPrefix(frame.Function, "runtime.") {
			return frame.File, frame.Line, true
		}
		if frame.Function == "runtime.gopanic" {
			panicking = true
		}
		if !more {
			return "", 0, false
		}
	}
}

// backtraceEnabled is whether backtraces were explicitly asked for, as
// Rust's Backtrace::capture reads RUST_LIB_BACKTRACE, then RUST_BACKTRACE.
func backtraceEnabled() bool {
	value, ok := os.LookupEnv("RUST_LIB_BACKTRACE")
	if !ok {
		value, ok = os.LookupEnv("RUST_BACKTRACE")
	}
	return ok && value != "0"
}
