// Port of mj-store/src/database/writer.rs.

package store

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// The writer's jobs take the writer's own *Connection (rusqlite's &mut
// Connection, schema.rs's pinned SQLite connection): its per-connection
// pragmas and temporary triggers hold for every job, and a job begins its
// transactions on it. Functions that only run statements take a Queryer;
// those that must run inside their caller's transaction take *Transaction.

const databaseWriteQueueCapacity = 256

// slowWrite is how long a write takes before it is logged with its label.
const slowWrite = 250 * time.Millisecond

// databaseWriteJob is a queued write, handed either the writer's connection
// or the reason it must not be used (err non-nil, conn nil). The job -- not
// the lane -- decides what a refusal means to its caller.
type databaseWriteJob func(conn *Connection, err error)

// databaseWriterMessage is one message on the writer's queue.
type databaseWriterMessage interface {
	isDatabaseWriterMessage()
}

type databaseWriterMessageRun struct {
	label string
	job   databaseWriteJob
}

type databaseWriterMessageShutdown struct{}

func (databaseWriterMessageRun) isDatabaseWriterMessage()      {}
func (databaseWriterMessageShutdown) isDatabaseWriterMessage() {}

// CommittedPublication is what the writer publishes after each write: the
// daemon's current durable records, or why publication stopped (Rust's
// Result<Arc<CommittedState>, Arc<str>>). Err is empty when State is set.
type CommittedPublication struct {
	State *CommittedState
	Err   string
}

// DatabaseWriter is the submission handle for the daemon's ordered SQLite
// write lane. Copies share the lane (Rust's Clone).
//
// DatabaseWriterExecute is synchronous and may apply bounded backpressure, so
// async and UI callers must invoke database mutations from their existing
// supervised blocking tasks.
type DatabaseWriter struct {
	id     uint64
	sender chan databaseWriterMessage
	// stopped is closed once the writer goroutine no longer receives (Rust's
	// disconnected SyncSender). A Go send waits for queue space and for this
	// in one select, which replaces tokio's Notify of freed space.
	stopped   chan struct{}
	path      string
	committed *core.Watch[CommittedPublication]
}

// String is the Debug form: DatabaseWriter { id: .., .. }.
func (w *DatabaseWriter) String() string {
	return fmt.Sprintf("DatabaseWriter { id: %d, .. }", w.id)
}

// CommittedState is the writer's current publication, or the error that
// stopped it.
func (w *DatabaseWriter) CommittedState() (*CommittedState, error) {
	published := w.committed.Borrow()
	if published.Err != "" {
		return nil, errors.New(published.Err)
	}
	return published.State, nil
}

// CommittedChanges is the watch the writer publishes to.
func (w *DatabaseWriter) CommittedChanges() *core.Watch[CommittedPublication] {
	return w.committed
}

// databaseWriterRun runs operation on the writer's connection and publishes
// the records it changed (Rust's DatabaseWriter::run; Go methods cannot be
// generic).
func databaseWriterRun[T any](w *DatabaseWriter, label string, conn *Connection, connErr error, operation func(conn *Connection) (T, error)) (T, error) {
	var zero T
	if connErr != nil {
		return zero, connErr
	}
	previous, err := w.CommittedState()
	if err != nil {
		return zero, err
	}
	beginOperation(w.path)
	value, operationErr := operation(conn)
	next, err := finishOperation(conn, previous)
	if err != nil {
		detail := fmt.Sprintf("database publication failed after %s; a commit may have occurred, do not replay this mutation: %v", label, err)
		w.committed.SendReplace(CommittedPublication{Err: detail})
		return zero, errors.New(detail)
	}
	if next != nil {
		w.committed.SendReplace(CommittedPublication{State: next})
	}
	return value, operationErr
}

// databaseWriteResult is one job's reply.
type databaseWriteResult[T any] struct {
	value T
	err   error
}

// send queues message unless the writer has stopped receiving, which is
// Rust's SyncSender::send failing on a disconnected receiver. A full queue
// waits for space.
func (w *DatabaseWriter) send(message databaseWriterMessage) bool {
	select {
	case <-w.stopped:
		return false
	default:
	}
	select {
	case w.sender <- message:
		return true
	case <-w.stopped:
		return false
	}
}

// DatabaseWriterExecute submits operation and waits for its result (Rust's
// DatabaseWriter::execute; Go methods cannot be generic).
func DatabaseWriterExecute[T any](w *DatabaseWriter, label string, operation func(conn *Connection) (T, error)) (T, error) {
	var zero T
	reply := make(chan databaseWriteResult[T], 1)
	job := func(conn *Connection, err error) {
		value, err := databaseWriterRun(w, label, conn, err, operation)
		reply <- databaseWriteResult[T]{value: value, err: err}
	}
	if !w.send(databaseWriterMessageRun{label: label, job: job}) {
		return zero, fmt.Errorf("submit database writer operation %s: writer stopped", label)
	}
	select {
	case result := <-reply:
		return result.value, result.err
	case <-w.stopped:
		// A job runs before the writer stops receiving, so its reply is
		// already buffered; an empty reply means the job was dropped.
		select {
		case result := <-reply:
			return result.value, result.err
		default:
		}
		return zero, fmt.Errorf("database writer stopped during %s: receiving on a closed channel", label)
	}
}

// DatabaseWriterEnqueue submits operation without waiting for it (Rust's
// DatabaseWriter::enqueue; Go methods cannot be generic). Cancelling ctx
// before the write is queued accepts nothing. After it returns, the caller
// owns a durable obligation to finish publication and its reply.
func DatabaseWriterEnqueue[T, G any](ctx context.Context, w *DatabaseWriter, label string, guard G, operation func(conn *Connection) (T, error)) (*AcceptedDatabaseWrite[T, G], error) {
	reply := make(chan DatabaseWriteReply[T, G], 1)
	message := databaseWriterMessageRun{label: label, job: func(conn *Connection, err error) {
		value, err := databaseWriterRun(w, label, conn, err, operation)
		reply <- DatabaseWriteReply[T, G]{Value: value, Err: err, Guard: guard}
	}}
	stopped := fmt.Errorf("submit database writer operation %s: writer stopped", label)
	select {
	case <-w.stopped:
		return nil, stopped
	default:
	}
	select {
	case w.sender <- message:
		return &AcceptedDatabaseWrite[T, G]{reply: reply, stopped: w.stopped}, nil
	case <-w.stopped:
		return nil, stopped
	case <-ctx.Done():
		return nil, ctx.Err()
	}
}

// DatabaseWriteReply is an accepted write's result and the guard that covered
// it (Rust's (Result<T>, G)).
type DatabaseWriteReply[T, G any] struct {
	Value T
	Err   error
	Guard G
}

// AcceptedDatabaseWrite is a write the lane has queued.
type AcceptedDatabaseWrite[T, G any] struct {
	reply <-chan DatabaseWriteReply[T, G]
	// stopped is the writer's; a job that never runs leaves reply empty.
	stopped <-chan struct{}
}

// Finish waits for the write. Once accepted this wait must not be cancelled:
// the guard covers the committed publication and the originating
// connection's reply attempt.
func (a *AcceptedDatabaseWrite[T, G]) Finish() (DatabaseWriteReply[T, G], error) {
	select {
	case reply := <-a.reply:
		return reply, nil
	case <-a.stopped:
		select {
		case reply := <-a.reply:
			return reply, nil
		default:
		}
		return DatabaseWriteReply[T, G]{}, errors.New("database writer stopped during accepted write: channel closed")
	}
}

func enqueueDatabaseWrite[T, G any](ctx context.Context, label string, guard G, operation func(conn *Connection) (T, error)) (*AcceptedDatabaseWrite[T, G], error) {
	slot := databaseWriterSlot()
	slot.mu.Lock()
	writer := slot.writer
	slot.mu.Unlock()
	if writer == nil {
		return nil, errors.New("database writer is not available")
	}
	return DatabaseWriterEnqueue(ctx, writer, label, guard, operation)
}

// DatabaseWriterOwner owns the daemon's writer goroutine and persistent
// SQLite connection.
//
// The owner is deliberately not copied. Shutdown removes the global
// submission handle, drains accepted work in FIFO order, and waits for the
// goroutine before releasing the connection. Go has no Drop: the owner must
// call Shutdown.
type DatabaseWriterOwner struct {
	writer *DatabaseWriter
	// thread is closed when the writer goroutine has returned; nil once
	// Shutdown has joined it (Rust's Option<JoinHandle>).
	thread <-chan struct{}
	// panicked is the writer goroutine's recovered panic, re-raised on join.
	panicked *any
	stopped  <-chan error
}

// String is the Debug form.
func (o *DatabaseWriterOwner) String() string {
	return fmt.Sprintf("DatabaseWriterOwner { writer: %s, .. }", o.writer)
}

// Shutdown stops the writer after the work it accepted.
func (o *DatabaseWriterOwner) Shutdown() error {
	return o.shutdownInner()
}

func (o *DatabaseWriterOwner) shutdownInner() error {
	if o.thread == nil {
		return nil
	}
	clearDatabaseWriter(o.writer.id)
	sent := o.writer.send(databaseWriterMessageShutdown{})
	workerResult, reported := <-o.stopped
	<-o.thread
	o.thread = nil
	if o.panicked != nil {
		panic(*o.panicked)
	}
	if !reported {
		return errors.New("database writer stopped without reporting its result: receiving on a closed channel")
	}
	if workerResult != nil {
		return workerResult
	}
	if !sent {
		return errors.New("request database writer shutdown: writer stopped")
	}
	return nil
}

// databaseWriterSlotState is the process-wide writer handle.
type databaseWriterSlotState struct {
	mu     sync.Mutex
	writer *DatabaseWriter
}

var databaseWriterSlotValue databaseWriterSlotState

func databaseWriterSlot() *databaseWriterSlotState {
	return &databaseWriterSlotValue
}

// DatabaseWriterInstalled is whether this process holds the database
// writer. Only the daemon does, so work that merely keeps a daemon-owned
// record current, and that other processes also run, asks this instead of
// failing the write there.
func DatabaseWriterInstalled() bool {
	slot := databaseWriterSlot()
	slot.mu.Lock()
	defer slot.mu.Unlock()
	return slot.writer != nil
}

// WriterCommittedState is the daemon's current durable records (Rust's
// database::committed_state, named apart from the CommittedState type).
// Client/offline readers have no writer and get nil; they use explicit store
// reads. A failed daemon projection remains an error.
func WriterCommittedState() (*CommittedState, error) {
	slot := databaseWriterSlot()
	slot.mu.Lock()
	writer := slot.writer
	slot.mu.Unlock()
	if writer == nil {
		return nil, nil
	}
	return writer.CommittedState()
}

// SubscribeCommittedState is the installed writer's publication watch.
func SubscribeCommittedState() (*core.Watch[CommittedPublication], error) {
	slot := databaseWriterSlot()
	slot.mu.Lock()
	writer := slot.writer
	slot.mu.Unlock()
	if writer == nil {
		return nil, errors.New("database writer is not installed")
	}
	return writer.CommittedChanges(), nil
}

func clearDatabaseWriter(id uint64) {
	slot := databaseWriterSlot()
	slot.mu.Lock()
	defer slot.mu.Unlock()
	if slot.writer != nil && slot.writer.id == id {
		slot.writer = nil
	}
}

// InstallIsolatedTestWriter installs the process-wide writer for a test that
// owns its data directory.
//
// Production installs this once, in the daemon, after ControllerStoreGuard
// establishes exclusivity, and the daemon is then the only process that
// writes. A test may do the same only because it re-execs itself with its own
// MJ_DATA_DIR and is therefore alone in its process -- which is exactly why
// the tests that need this are shaped that way.
//
// The returned owner has to be held for the rest of the test: shutting it
// down stops the writer, and the next write fails.
//
// This fixture is compiled unconditionally because the controller package's
// tests need it. It is a thin wrapper over StartDatabaseWriter.
func InstallIsolatedTestWriter() *DatabaseWriterOwner {
	owner, err := StartDatabaseWriter()
	if err != nil {
		panic(fmt.Sprintf("install the writer for an isolated test child: %v", err))
	}
	return owner
}

func StartDatabaseWriter() (*DatabaseWriterOwner, error) {
	return startDatabaseWriterAt(DatabasePath(), true)
}

// nextWriterID numbers writers so a stopped one clears only its own slot;
// nextWriterID.Add(1) hands out 1 first, as Rust's fetch_add from 1 does.
var nextWriterID atomic.Uint64

func startDatabaseWriterAt(path string, installGlobally bool) (*DatabaseWriterOwner, error) {
	connection, err := openWriter(path)
	if err != nil {
		return nil, err
	}
	// 0x10002 is SQLite's recommendation for long-lived connections. Bit 0x10
	// applies a temporary analysis_limit, keeping the first run bounded on a
	// large store that has never been analyzed. Refresh statistics once at
	// every daemon start by design; there is no periodic run.
	optimizeStarted := time.Now()
	if _, err := connection.Exec("PRAGMA optimize=0x10012"); err != nil {
		slog.Warn("failed to optimize SQLite query planner statistics; continuing startup",
			"error", err, "elapsed_ms", time.Since(optimizeStarted).Milliseconds())
	} else {
		slog.Info("optimized SQLite query planner statistics",
			"elapsed_ms", time.Since(optimizeStarted).Milliseconds())
	}
	state, err := readSchemaState(connection)
	if err != nil {
		_ = connection.Close()
		return nil, err
	}
	observedRevision := state.revision
	initial, err := committedStateBootstrap(connection)
	if err != nil {
		_ = connection.Close()
		return nil, err
	}
	receiver := make(chan databaseWriterMessage, databaseWriteQueueCapacity)
	disconnected := make(chan struct{})
	stopped := make(chan error, 1)
	threadDone := make(chan struct{})
	id := nextWriterID.Add(1)
	writer := &DatabaseWriter{
		id:        id,
		sender:    receiver,
		stopped:   disconnected,
		path:      path,
		committed: core.NewWatch(CommittedPublication{State: initial}),
	}
	if installGlobally {
		slot := databaseWriterSlot()
		slot.mu.Lock()
		if slot.writer != nil {
			slot.mu.Unlock()
			_ = connection.Close()
			return nil, errors.New("database writer is already running")
		}
		slot.writer = writer
		slot.mu.Unlock()
	}
	publication := writer
	go func() {
		defer close(threadDone)
		defer func() { _ = connection.Close() }()
		result := func() (result error) {
			defer func() {
				if panicked := recover(); panicked != nil {
					detail := "unknown panic payload"
					switch payload := panicked.(type) {
					case string:
						detail = payload
					case error:
						detail = payload.Error()
					}
					result = fmt.Errorf("database writer thread panicked: %s", detail)
				}
			}()
			for message := range receiver {
				switch message := message.(type) {
				case databaseWriterMessageRun:
					slog.Debug("running database writer operation", "operation", message.label)
					jobStarted := time.Now()
					// Recheck compatibility even after startup, and remember
					// forward progress to detect rollback.
					if err := writerSchemaState(path, connection, message.label, &observedRevision); err != nil {
						message.job(nil, err)
					} else {
						message.job(connection, nil)
					}
					// Every write waits behind the one before it, so a slow
					// one delays all of them, the daemon's exit included.
					if took := time.Since(jobStarted); took >= slowWrite {
						slog.Info("slow database write", "operation", message.label, "duration_ms", took.Milliseconds())
					}
					if _, err := publication.CommittedState(); err != nil {
						return err
					}
				case databaseWriterMessageShutdown:
					return nil
				}
			}
			return errors.New("database writer queue disconnected")
		}()
		// The writer no longer receives: queued jobs are dropped and their
		// callers told the writer stopped.
		close(disconnected)
		if result != nil {
			publication.committed.SendReplace(CommittedPublication{Err: fmt.Sprintf("database writer stopped: %v", result)})
		} else {
			clearDatabaseWriter(id)
		}
		stopped <- result
		close(stopped)
	}()
	return &DatabaseWriterOwner{
		writer:  writer,
		thread:  threadDone,
		stopped: stopped,
	}, nil
}

// writerSchemaState refuses incompatible stores, rollback, and unreadable
// metadata before a job.
func writerSchemaState(path string, conn *Connection, label string, observedRevision *int64) error {
	err := func() error {
		state, err := readSchemaState(conn)
		if err != nil {
			return err
		}
		if state.revision < *observedRevision {
			return core.StoreSchemaMismatch{
				Found:     state.revision,
				Supported: schemaVersion,
				Reason:    core.StoreSchemaMismatchReasonRollback{Previous: *observedRevision},
			}
		}
		*observedRevision = state.revision
		return state.ensureSupported()
	}()
	if err == nil {
		return nil
	}
	slog.Error("could not establish store compatibility; refusing the operation",
		"operation", label, "path", path, "error", err)
	return fmt.Errorf("check database compatibility before %s at %s: %w", label, path, err)
}

func submitDatabaseWrite[T any](label string, operation func(conn *Connection) (T, error)) (T, error) {
	slot := databaseWriterSlot()
	slot.mu.Lock()
	writer := slot.writer
	slot.mu.Unlock()
	if writer != nil {
		return DatabaseWriterExecute(writer, label, operation)
	}
	// There is one way to write, and this is not it. In production the
	// daemon installs the writer after ControllerStoreGuard establishes
	// exclusivity, and it is the only process that writes; a caller reaching
	// here has no exclusivity and would be competing with whatever does.
	var zero T
	return zero, fmt.Errorf("database writer is not available for operation %s", label)
}
