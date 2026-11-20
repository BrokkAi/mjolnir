// Port of mj-core/src/logging.rs.

// Diagnostic output must never turn an unavailable disk into a process
// failure.

package core

import (
	"fmt"
	"io"
	"os"
	"strconv"
	"strings"
	"sync"
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
	panic("unported: mj-core/src/logging.rs State::remember")
}

func (s *resilientWriterState) failed(err error, count uint64) {
	panic("unported: mj-core/src/logging.rs State::failed")
}

// Write records bytes, writing them out when the writer accepts them. It
// always reports success.
func (w ResilientWriter) Write(bytes []byte) (int, error) {
	panic("unported: mj-core/src/logging.rs ResilientWriter::write")
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
	panic("unported: mj-core/src/logging.rs io::ErrorKind Debug")
}

// PanicMessage is the message of a recovered panic, with the goroutine's
// stack when backtraces are explicitly enabled, without invoking stderr's
// default handler.
func PanicMessage(recovered any) string {
	panic("unported: mj-core/src/logging.rs panic_message")
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
