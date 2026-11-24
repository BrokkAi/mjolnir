// Port of mj-cli/src/logging.rs.
//
// Durable, non-blocking diagnostics for controller-facing Mjolnir processes.
//
// tracing-subscriber becomes log/slog: startControllerLog installs the
// process-wide slog default writing tracing's fmt lines (no ANSI) behind
// RUST_LOG's EnvFilter. go/internal/workerbin/logging.go has that handler
// and filter for the worker; the CLI's body is to share it rather than
// write a second one.

package cli

import (
	"os"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const retainedLogs = 10

// recentLogWindow: logs started within this window are kept beyond
// retainedLogs, up to recentLogLimit of a kind. Scripts and agents start a
// CLI command every second or so; a newest-ten window then lost the log of
// a command that ran a minute ago, such as the one whose slow daemon upgrade
// needed explaining.
const recentLogWindow = time.Hour

const recentLogLimit = 1000

// logFlushTimeout gives a queued burst time to drain under contention
// without allowing a stuck writer to hold process shutdown indefinitely.
const logFlushTimeout = 10 * time.Second

// processKind is the kind of process writing a Mjolnir log, recorded in the
// log filename so retention can keep the newest logs of each kind
// separately. A short-lived `mj sessions` or `mj wait` invocation must never
// crowd the long-running daemon's own log out of a single shared newest-N
// window.
type processKind int

const (
	// processKindDaemon is the persistent per-user daemon (`mj daemon-run`).
	processKindDaemon processKind = iota
	// processKindTui is an interactive dashboard or other terminal-owning
	// surface (no subcommand, `go`, `workspaces`, `app`).
	processKindTui
	// processKindCli is a one-shot CLI invocation (everything else).
	processKindCli
	// processKindCrashedDaemon is a daemon log kept because that daemon
	// crashed (see core.CrashedLogName). Never the kind of a new log.
	processKindCrashedDaemon
)

func (k processKind) label() string {
	switch k {
	case processKindDaemon, processKindCrashedDaemon:
		return "daemon"
	case processKindTui:
		return "tui"
	}
	return "cli"
}

// controllerLog keeps the log writer draining until the process ends;
// Close is Rust's Drop of its guard.
type controllerLog struct {
	writerGuard *reliableWorkerGuard
}

// reliableWriter is a non-blocking logger with reliable delivery to its
// writer goroutine. A bounded, lossy queue can discard a fatal error when a
// controller emits a burst of diagnostics. An unbounded queue keeps the UI
// call site free of filesystem I/O while retaining every record until the
// worker writes it or the process exits.
type reliableWriter struct {
	sender *core.Unbounded[logMessage]
}

// logMessage is a line to write, or a flush (flushed set) whose result the
// worker reports.
type logMessage struct {
	line    []byte
	flushed chan error
}

// reliableWorkerGuard drains the writer when it is closed.
type reliableWorkerGuard struct {
	sender *core.Unbounded[logMessage]
}

// Write queues bytes. A stopped worker is reported on stderr; the write
// still succeeds, as the Rust writer's does.
func (w reliableWriter) Write(bytes []byte) (int, error) {
	panic("unported: mj-cli/src/logging.rs ReliableWriter::write")
}

// Flush waits for the worker to flush, reporting a failure on stderr.
func (w reliableWriter) Flush() error {
	panic("unported: mj-cli/src/logging.rs ReliableWriter::flush")
}

func (w reliableWriter) flushChecked() error {
	panic("unported: mj-cli/src/logging.rs ReliableWriter::flush_checked")
}

// Close is Rust's Drop: flush once before exit. The process-wide logger
// owns another sender for the rest of the process, so the worker stays
// valid after this flush and a late diagnostic cannot turn into terminal
// output. The detached writer stops at process exit.
func (g *reliableWorkerGuard) Close() {
	panic("unported: mj-cli/src/logging.rs ReliableWorkerGuard::drop")
}

func reliableNonBlocking(file core.ResilientWriter) (reliableWriter, *reliableWorkerGuard, error) {
	panic("unported: mj-cli/src/logging.rs reliable_non_blocking")
}

func writeLogMessages(file core.ResilientWriter, receiver *core.Unbounded[logMessage]) {
	panic("unported: mj-cli/src/logging.rs write_log_messages")
}

// panicLogWriter (Rust `PANIC_LOG`) lets panic messages bypass the queue:
// process exit cannot discard them while the asynchronous writer has not
// yet run. This is diagnostic I/O, not UI work. Set once.
var panicLogWriter struct {
	once   sync.Once
	writer *core.ResilientWriter
}

// panicLog reports reason on stderr and appends it to this process's log.
func panicLog(reason string) {
	panic("unported: mj-cli/src/logging.rs panic_log")
}

// deferredLogFile creates its file on the first write. Startup can also run
// out of space; it keeps trying to create the log on later lines instead of
// making a diagnostic file a prerequisite for daemon startup.
type deferredLogFile struct {
	path string
	file *os.File
}

// Write creates the file owner-only on the first write. The descriptor
// remains valid after ENOSPC and is not replaced.
func (f *deferredLogFile) Write(bytes []byte) (int, error) {
	panic("unported: mj-cli/src/logging.rs DeferredLogFile::write")
}

// Flush is std::fs::File's flush, which has nothing buffered to write: it
// does not sync.
func (f *deferredLogFile) Flush() error {
	return nil
}

// startControllerLog starts this process's diagnostic log in
// `<data>/logs`, pruning old logs first, and installs it as the slog
// default (Rust `ControllerLog::start`).
func startControllerLog(command string, kind processKind) (*controllerLog, error) {
	panic("unported: mj-cli/src/logging.rs ControllerLog::start")
}

// envFilter is the RUST_LOG filter, or defaultFilter with the reason
// RUST_LOG was ignored (nil when it was not).
func envFilter(defaultFilter string) (filter string, filterError *string) {
	panic("unported: mj-cli/src/logging.rs env_filter")
}

func logFilename(kind processKind) string {
	panic("unported: mj-cli/src/logging.rs log_filename")
}

// currentDaemonPID reads the PID of the daemon currently recorded in
// `<dataDir>/daemon.json`, if any. A missing or unparseable file means "no
// protected PID" rather than an error: absence just means no daemon has
// started under this data directory yet, or its metadata predates this
// process.
func currentDaemonPID(dataDir string) (uint32, bool) {
	panic("unported: mj-cli/src/logging.rs current_daemon_pid")
}

// parseLogFilename parses a managed Mjolnir log filename (already known to
// start with `mj-` and end with `.log`) into its process kind and PID. The
// current format is `mj-<kind>-<timestamp>-<pid>.log`; the pre-existing
// format `mj-<timestamp>-<pid>.log` (no kind, three `-`-separated parts) is
// treated as kind `cli` so its retention matches today's short CLI
// invocations. The PID is nil when it cannot be parsed, which keeps the file
// eligible for pruning as before this change.
func parseLogFilename(name string) (kind processKind, pid *uint32, ok bool) {
	panic("unported: mj-cli/src/logging.rs parse_log_filename")
}

// logTimestamp is the start timestamp in a managed log filename, in either
// format.
func logTimestamp(name string) (string, bool) {
	parsed := core.ParseLogName(name)
	if parsed == nil {
		return "", false
	}
	return parsed.Started, true
}

// daemonLogPath is the newest diagnostic log of the daemon with pid under
// dataDir, or false when there is none.
func daemonLogPath(dataDir string, pid uint32) (string, bool, error) {
	panic("unported: mj-cli/src/logging.rs daemon_log_path")
}

func pruneLogs(directory string, retain int, protectedPID *uint32) error {
	panic("unported: mj-cli/src/logging.rs prune_logs")
}

// pruneLogsSince prunes each kind to its newest retain logs, keeping as well
// those whose filename timestamp is at or after recent (up to
// recentLogLimit) and those of processes still running, which may still be
// writing.
func pruneLogsSince(directory string, retain int, protectedPID *uint32, recent string) error {
	panic("unported: mj-cli/src/logging.rs prune_logs_since")
}

func removeExpiredLog(path string) error {
	panic("unported: mj-cli/src/logging.rs remove_expired_log")
}
