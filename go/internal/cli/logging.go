// Port of mj-cli/src/logging.rs.
//
// Durable, non-blocking diagnostics for controller-facing Mjolnir processes.
//
// tracing-subscriber becomes log/slog: startControllerLog installs the
// process-wide slog default writing tracing's fmt lines (no ANSI) behind
// RUST_LOG's EnvFilter. The handler and filter are the worker's
// (workerbin.NewTracingHandler), so both kinds of process write the same
// lines.

package cli

import (
	"context"
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync/atomic"
	"syscall"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/workerbin"
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

// Close flushes the log before exit.
func (l *controllerLog) Close() {
	l.writerGuard.Close()
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
	length := len(bytes)
	if err := w.sender.Send(logMessage{line: append([]byte(nil), bytes...)}); err != nil {
		core.DiagnosticStderr("Mjolnir log worker stopped; dropped diagnostic: %s", core.LossyUTF8(bytes))
	}
	return length, nil
}

// Flush waits for the worker to flush, reporting a failure on stderr.
func (w reliableWriter) Flush() error {
	if err := w.flushChecked(); err != nil {
		core.DiagnosticStderr("Mjolnir log writer could not flush: %s", err)
	}
	return nil
}

func (w reliableWriter) flushChecked() error {
	flushed := make(chan error, 1)
	if err := w.sender.Send(logMessage{flushed: flushed}); err != nil {
		return errors.New("log worker stopped")
	}
	timer := time.NewTimer(logFlushTimeout)
	defer timer.Stop()
	select {
	case result := <-flushed:
		return result
	case <-timer.C:
		return errors.New("log worker did not flush: timed out waiting on channel")
	}
}

// Close is Rust's Drop: flush once before exit. The process-wide logger
// owns another sender for the rest of the process, so the worker stays
// valid after this flush and a late diagnostic cannot turn into terminal
// output. The detached writer stops at process exit.
func (g *reliableWorkerGuard) Close() {
	sender := g.sender
	if sender == nil {
		return
	}
	g.sender = nil
	writer := reliableWriter{sender: sender}
	if err := writer.flushChecked(); err != nil {
		core.DiagnosticStderr("Mjolnir log writer failed to drain before exit: %s", err)
	}
}

func reliableNonBlocking(file core.ResilientWriter) (reliableWriter, *reliableWorkerGuard, error) {
	sender := core.NewUnbounded[logMessage]()
	go func() {
		defer func() {
			if recovered := recover(); recovered != nil {
				core.DiagnosticStderr("Mjolnir log writer panicked: %v", recovered)
			}
		}()
		writeLogMessages(file, sender)
	}()
	return reliableWriter{sender: sender}, &reliableWorkerGuard{sender: sender}, nil
}

func writeLogMessages(file core.ResilientWriter, receiver *core.Unbounded[logMessage]) {
	for {
		message, ok := receiver.Recv(context.Background())
		if !ok {
			break
		}
		if message.flushed == nil {
			_, _ = file.Write(message.line)
			continue
		}
		result := file.Flush()
		select {
		case message.flushed <- result:
		default:
			core.DiagnosticStderr("Mjolnir log writer flush completion could not be reported")
		}
	}
	_ = file.Flush()
}

// panicLogWriter (Rust `PANIC_LOG`) lets panic messages bypass the queue:
// process exit cannot discard them while the asynchronous writer has not
// yet run. This is diagnostic I/O, not UI work. Set once, as OnceLock::set.
var panicLogWriter atomic.Pointer[core.ResilientWriter]

// panicLog reports reason on stderr and appends it to this process's log.
func panicLog(reason string) {
	core.DiagnosticStderr("%s", reason)
	if writer := panicLogWriter.Load(); writer != nil {
		_, _ = writer.Write([]byte(core.FormatChronoRFC3339(time.Now()) + " " + reason + "\n"))
	}
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
	if f.file == nil {
		if parent := filepath.Dir(f.path); parent != f.path {
			if err := os.MkdirAll(parent, 0o777); err != nil {
				return 0, err
			}
		}
		file, err := os.OpenFile(f.path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
		if err != nil {
			return 0, err
		}
		f.file = file
	}
	return f.file.Write(bytes)
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
	dataDir := core.DataDir()
	directory := filepath.Join(dataDir, "logs")
	var prepared error
	if err := core.CreateOwnerOnlyDirectory(directory); err != nil {
		prepared = err
	} else {
		var protected *uint32
		if pid, ok := currentDaemonPID(dataDir); ok {
			protected = &pid
		}
		if err := pruneLogs(directory, retainedLogs-1, protected); err != nil {
			// anyhow's `{}` shows the context alone.
			prepared = fmt.Errorf("prune diagnostic logs in %s", directory)
		}
	}
	if prepared != nil {
		core.DiagnosticStderr("Mjolnir could not prepare diagnostic logs: %s", prepared)
	}
	path := filepath.Join(directory, logFilename(kind))
	file := core.NewResilientWriter(&deferredLogFile{path: path})
	panicLogWriter.CompareAndSwap(nil, &file)
	writer, writerGuard, err := reliableNonBlocking(file)
	if err != nil {
		return nil, err
	}
	filter, filterError := envFilter("info")
	handler, err := workerbin.NewTracingHandler(filter, writer)
	if err != nil {
		return nil, fmt.Errorf("install Mjolnir log subscriber: %w", err)
	}
	if !controllerLogInstalled.CompareAndSwap(false, true) {
		return nil, errors.New("install Mjolnir log subscriber: a global default trace dispatcher has already been set")
	}
	slog.SetDefault(slog.New(handler))
	slog.Info("Mjolnir started",
		"version", core.PackageVersion,
		"process_id", os.Getpid(),
		"command", command,
		"log", path)
	if filterError != nil {
		slog.Warn("ignored invalid RUST_LOG filter", "error", *filterError)
	}
	return &controllerLog{writerGuard: writerGuard}, nil
}

// controllerLogInstalled is whether startControllerLog has set the
// process-wide logger, which tracing allows once.
var controllerLogInstalled atomic.Bool

// envFilter is the RUST_LOG filter, or defaultFilter with the reason
// RUST_LOG was ignored (nil when it was not).
func envFilter(defaultFilter string) (filter string, filterError *string) {
	value, present := os.LookupEnv("RUST_LOG")
	if !present {
		return defaultFilter, nil
	}
	if !utf8.ValidString(value) {
		reason := "environment variable was not valid unicode: " + workerbin.RustOsStrDebug(value)
		return defaultFilter, &reason
	}
	if _, err := workerbin.NewTracingHandler(value, nil); err != nil {
		reason := err.Error()
		return defaultFilter, &reason
	}
	return value, nil
}

func logFilename(kind processKind) string {
	return fmt.Sprintf("mj-%s-%s-%d.log", kind.label(), time.Now().UTC().Format(core.LogTimestampFormat), os.Getpid())
}

// currentDaemonPID reads the PID of the daemon currently recorded in
// `<dataDir>/daemon.json`, if any. A missing or unparseable file means "no
// protected PID" rather than an error: absence just means no daemon has
// started under this data directory yet, or its metadata predates this
// process.
func currentDaemonPID(dataDir string) (uint32, bool) {
	body, err := os.ReadFile(filepath.Join(dataDir, "daemon.json"))
	if err != nil {
		return 0, false
	}
	var metadata client.DaemonMetadata
	if err := metadata.UnmarshalJSON(body); err != nil {
		return 0, false
	}
	return metadata.Pid, true
}

// parseLogFilename parses a managed Mjolnir log filename (already known to
// start with `mj-` and end with `.log`) into its process kind and PID. The
// current format is `mj-<kind>-<timestamp>-<pid>.log`; the pre-existing
// format `mj-<timestamp>-<pid>.log` (no kind, three `-`-separated parts) is
// treated as kind `cli` so its retention matches today's short CLI
// invocations. The PID is nil when it cannot be parsed, which keeps the file
// eligible for pruning as before this change.
func parseLogFilename(name string) (kind processKind, pid *uint32, ok bool) {
	parsed := core.ParseLogName(name)
	if parsed == nil {
		return 0, nil, false
	}
	switch {
	case parsed.Kind != nil && *parsed.Kind == "daemon" && parsed.Crashed:
		kind = processKindCrashedDaemon
	case parsed.Kind != nil && *parsed.Kind == "daemon":
		kind = processKindDaemon
	case parsed.Crashed:
		return 0, nil, false
	case parsed.Kind != nil && *parsed.Kind == "tui":
		kind = processKindTui
	case parsed.Kind == nil || *parsed.Kind == "cli":
		kind = processKindCli
	default:
		return 0, nil, false
	}
	return kind, parsed.Pid, true
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
	directory := filepath.Join(dataDir, "logs")
	entries, err := os.ReadDir(directory)
	if errors.Is(err, fs.ErrNotExist) {
		return "", false, nil
	}
	if err != nil {
		return "", false, fmt.Errorf("find the launched daemon's diagnostic log: %w", err)
	}
	latest := ""
	found := false
	for _, entry := range entries {
		name := entry.Name()
		if !utf8.ValidString(name) {
			continue
		}
		kind, entryPID, ok := parseLogFilename(name)
		if !ok || kind != processKindDaemon || entryPID == nil || *entryPID != pid {
			continue
		}
		path := filepath.Join(directory, name)
		if !found || path > latest {
			latest, found = path, true
		}
	}
	return latest, found, nil
}

func pruneLogs(directory string, retain int, protectedPID *uint32) error {
	recent := time.Now().Add(-recentLogWindow).UTC().Format(core.LogTimestampFormat)
	return pruneLogsSince(directory, retain, protectedPID, recent)
}

// pruneLogsSince prunes each kind to its newest retain logs, keeping as well
// those whose filename timestamp is at or after recent (up to
// recentLogLimit) and those of processes still running, which may still be
// writing.
func pruneLogsSince(directory string, retain int, protectedPID *uint32, recent string) error {
	entries, err := os.ReadDir(directory)
	if err != nil {
		return fmt.Errorf("read Mjolnir log directory %s: %w", directory, err)
	}
	logsByKind := map[processKind][]string{}
	for _, entry := range entries {
		name := entry.Name()
		if !utf8.ValidString(name) {
			continue
		}
		if !strings.HasPrefix(name, "mj-") || !strings.HasSuffix(name, ".log") {
			continue
		}
		// A file whose kind cannot be parsed falls back to `cli`, the same
		// group a malformed-but-managed name would have landed in before
		// filenames carried a kind.
		kind, pid, ok := parseLogFilename(name)
		if !ok {
			kind, pid = processKindCli, nil
		}
		if pid != nil && protectedPID != nil && *pid == *protectedPID {
			// Never prune the current daemon's own log, regardless of kind
			// or age; it may still be writing to it.
			continue
		}
		if pid != nil && client.ProcessIsAlive(*pid) {
			// A process that is still running may still be writing its log.
			continue
		}
		logsByKind[kind] = append(logsByKind[kind], filepath.Join(directory, name))
	}
	for _, logs := range logsByKind {
		sort.Strings(logs)
		startedRecently := 0
		for _, path := range logs {
			if started, ok := logTimestamp(filepath.Base(path)); ok && started >= recent {
				startedRecently++
			}
		}
		keep := max(retain, min(startedRecently, recentLogLimit))
		remove := max(len(logs)-keep, 0)
		for _, path := range logs[:remove] {
			if err := removeExpiredLog(path); err != nil {
				return err
			}
		}
	}
	return nil
}

func removeExpiredLog(path string) error {
	err := removeFile(path)
	switch {
	case err == nil:
		return nil
	// Another Mjolnir process may have pruned this file after the scan.
	case errors.Is(err, fs.ErrNotExist):
		return nil
	}
	return fmt.Errorf("remove expired Mjolnir log %s: %w", path, err)
}

// removeFile is Rust's std::fs::remove_file: it unlinks a file or a link
// and, unlike os.Remove, never removes a directory.
func removeFile(path string) error {
	if err := syscall.Unlink(path); err != nil {
		return &fs.PathError{Op: "remove", Path: path, Err: err}
	}
	return nil
}
