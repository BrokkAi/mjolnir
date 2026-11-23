// Port of mj-store/src/database/writer.rs.

package store

import (
	"context"
	"errors"
	"fmt"
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
	panic("unported: mj-store/src/database/writer.rs DatabaseWriter::run")
}

// DatabaseWriterExecute submits operation and waits for its result (Rust's
// DatabaseWriter::execute; Go methods cannot be generic).
func DatabaseWriterExecute[T any](w *DatabaseWriter, label string, operation func(conn *Connection) (T, error)) (T, error) {
	panic("unported: mj-store/src/database/writer.rs DatabaseWriter::execute")
}

// DatabaseWriterEnqueue submits operation without waiting for it (Rust's
// DatabaseWriter::enqueue; Go methods cannot be generic). Cancelling ctx
// before the write is queued accepts nothing. After it returns, the caller
// owns a durable obligation to finish publication and its reply.
func DatabaseWriterEnqueue[T, G any](ctx context.Context, w *DatabaseWriter, label string, guard G, operation func(conn *Connection) (T, error)) (*AcceptedDatabaseWrite[T, G], error) {
	panic("unported: mj-store/src/database/writer.rs DatabaseWriter::enqueue")
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
	panic("unported: mj-store/src/database/writer.rs AcceptedDatabaseWrite::finish")
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
	panic("unported: mj-store/src/database/writer.rs DatabaseWriterOwner::shutdown_inner")
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
	panic("unported: mj-store/src/database/writer.rs start_database_writer_at")
}

// writerSchemaState refuses incompatible stores, rollback, and unreadable
// metadata before a job.
func writerSchemaState(path string, conn *Connection, label string, observedRevision *int64) error {
	panic("unported: mj-store/src/database/writer.rs writer_schema_state")
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
