// Port of mj-store/src/database/committed.rs.

// The writer publishes immutable records before acknowledging an operation.
//
// Connection-local triggers collect affected keys, including cascading
// deletes and writes made through another connection on the writer's
// goroutine. Keys are hints, never commit receipts: after the operation
// returns we read committed rows and publish only actual differences. A
// rolled-back transaction therefore cannot publish a change. Nothing is added
// to the durable schema.

package store

import (
	"database/sql"
	"sync"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// pendingChanges is the change collector of one accepted writer job: the
// (kind, key) pairs its writes touched in the store at path.
type pendingChanges struct {
	path string
	keys map[[2]string]struct{}
}

// pending is Rust's thread-local PENDING. A writer runs one job at a time for
// its store, so the collector of the job in progress is kept per store path.
var pending = struct {
	sync.Mutex
	byPath map[string]*pendingChanges
}{byPath: map[string]*pendingChanges{}}

// observeConnection gives every writable connection the same observer. The
// collector is active only inside an accepted writer job and only for that
// job's database path.
func observeConnection(connection *sql.Conn, path string) error {
	panic("unported: mj-store/src/database/committed.rs observe_connection")
}

func beginOperation(path string) {
	pending.Lock()
	defer pending.Unlock()
	if _, nested := pending.byPath[path]; nested {
		panic("nested database writer operation")
	}
	pending.byPath[path] = &pendingChanges{path: path, keys: map[[2]string]struct{}{}}
}

type CommittedState struct {
	Sequence     uint64
	State        core.StateState
	Moves        core.SnapshotMap[string, core.MoveOperation]
	NativeAgents core.SnapshotMap[string, core.SnapshotMap[string, core.NativeAgentSummary]]
	// StartupGroups is each session's latest startup group, keyed by
	// session. A session with no group has no entry. API status readers ask
	// this record instead of the store, and a change to it publishes a
	// revision.
	StartupGroups core.SnapshotMap[string, StartupGroupStatus]
	// SubagentReports is each sub-agent child's recorded report, keyed by
	// child session. A child with nothing recorded has no entry.
	SubagentReports core.SnapshotMap[string, core.SubagentReport]
	Turns           core.SnapshotMap[string, CommittedTurn]
	// WaitRevisions: only this session's committed wait inputs advance this
	// token.
	WaitRevisions core.SnapshotMap[string, uint64]
}

type CommittedTurn struct {
	State MaterializedTurnState
	// FailedMessage is read once when a failed turn changes, never on each
	// observation.
	FailedMessage    *string
	PendingFormTimes map[string]int64
}

// Equal is the derived PartialEq.
func (t *CommittedTurn) Equal(other *CommittedTurn) bool {
	panic("unported: mj-store/src/database/committed.rs CommittedTurn::eq")
}

// committedTurnRead is CommittedTurn::read: the session's turn record, nil
// when it has no projection row. previous is the published record, whose
// failed message is reused while the failed turn is unchanged.
func committedTurnRead(connection Connection, id string, previous *CommittedTurn) (*CommittedTurn, error) {
	panic("unported: mj-store/src/database/committed.rs CommittedTurn::read")
}

// committedStateBootstrap is CommittedState::bootstrap: every published
// record, read from one snapshot when the writer starts.
func committedStateBootstrap(connection *sql.Conn) (*CommittedState, error) {
	panic("unported: mj-store/src/database/committed.rs CommittedState::bootstrap")
}

// finishOperation reads all changed records from one committed WAL snapshot.
// The caller owns the sole write lane, so no later job can overtake this
// publication. Nil when nothing published changed.
func finishOperation(connection *sql.Conn, previous *CommittedState) (*CommittedState, error) {
	panic("unported: mj-store/src/database/committed.rs finish_operation")
}
