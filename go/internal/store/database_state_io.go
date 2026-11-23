// Port of mj-store/src/database/state_io.rs.

package store

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// sqlConnection is the query surface a rusqlite `&Connection` gives the
// store's functions. *sql.DB, *sql.Conn and *sql.Tx all satisfy it, so a
// function that reads or writes through a connection also runs inside a
// caller's transaction, as Rust's `Transaction` deref to `Connection` allows.
// (Declared by store-1 for its files; the merge may unify it with another
// group's connection type.)
type sqlConnection interface {
	ExecContext(ctx context.Context, query string, args ...any) (sql.Result, error)
	QueryContext(ctx context.Context, query string, args ...any) (*sql.Rows, error)
	QueryRowContext(ctx context.Context, query string, args ...any) *sql.Row
}

// sqlWriteConnection is a connection a function opens its own transaction on
// (rusqlite's `&mut Connection`). *sql.DB and *sql.Conn satisfy it.
type sqlWriteConnection interface {
	sqlConnection
	BeginTx(ctx context.Context, opts *sql.TxOptions) (*sql.Tx, error)
}

// sessionInterns is scoped to a load/publication: no cache retains deleted
// sessions.
type sessionInterns struct {
	projects map[string]*core.ProjectBundleSnapshot
	runtimes map[string]*core.TargetRuntimeSettings
}

// sessionInternsFromState is SessionInterns::from_state.
func sessionInternsFromState(state *core.StateState) (*sessionInterns, error) {
	panic("unported: mj-store/src/database/state_io.rs SessionInterns::from_state")
}

// sessionInternsFromRecord is a pool holding only one record's values. A
// refresh that leaves a session's project and runtime unchanged reuses that
// record's allocations without visiting any other session.
func sessionInternsFromRecord(record *core.SessionRecord) (*sessionInterns, error) {
	pool := &sessionInterns{
		projects: map[string]*core.ProjectBundleSnapshot{},
		runtimes: map[string]*core.TargetRuntimeSettings{},
	}
	if record != nil {
		if record.Project != nil {
			key, err := json.Marshal(record.Project)
			if err != nil {
				return nil, err
			}
			pool.projects[string(key)] = record.Project
		}
		if record.TargetRuntime != nil {
			key, err := json.Marshal(record.TargetRuntime)
			if err != nil {
				return nil, err
			}
			pool.runtimes[string(key)] = record.TargetRuntime
		}
	}
	return pool, nil
}

// share replaces a record's values with equal ones already in this pool.
func (p *sessionInterns) share(record *core.SessionRecord) error {
	panic("unported: mj-store/src/database/state_io.rs SessionInterns::share")
}

// decodeShared decodes the JSON in column of a scanned row, reusing an equal
// value already in pool. row holds the row's column values as scanned
// (rusqlite's Row); a NULL column is nil.
func decodeShared[T any](row []any, column int, pool *map[string]*T) (*T, error) {
	panic("unported: mj-store/src/database/state_io.rs decode_shared")
}

// LoadConfigAndState is the configuration and the committed session
// records, as every reader that needs both loads them.
//
// A configuration the person has to fix, such as an entry whose secret is
// missing, is the reason a request fails, not an internal error, so that
// failure carries a refusal.
func LoadConfigAndState() (core.Config, core.StateState, error) {
	config, err := core.ConfigLoad()
	if err != nil {
		return core.Config{}, core.StateState{}, core.RefusalPrecondition(err.Error()).Wrap(err)
	}
	state, err := LoadState()
	if err != nil {
		return core.Config{}, core.StateState{}, err
	}
	return config, state, nil
}

func LoadState() (core.StateState, error) {
	panic("unported: mj-store/src/database/state_io.rs load_state")
}

func LoadStateFrom(path string) (core.StateState, error) {
	panic("unported: mj-store/src/database/state_io.rs load_state_from")
}

func loadStateWith(connection sqlConnection) (core.StateState, error) {
	panic("unported: mj-store/src/database/state_io.rs load_state_with")
}

// LoadMountHistory is the remembered mount sources and project directories
// by host key, newest first, as LoadState reads them. A dashboard reads this
// again when a wizard opens, since sessions created after it started add to
// the history.
func LoadMountHistory() (map[string][]string, error) {
	panic("unported: mj-store/src/database/state_io.rs load_mount_history")
}

func readMountHistory(connection sqlConnection) (map[string][]string, error) {
	panic("unported: mj-store/src/database/state_io.rs read_mount_history")
}

func SaveState(state *core.StateState) error {
	panic("unported: mj-store/src/database/state_io.rs save_state")
}

func SaveStateTo(path string, state *core.StateState) error {
	panic("unported: mj-store/src/database/state_io.rs save_state_to")
}

// storedSessionContext is one session_contexts row's (bundle_id,
// workspace_id).
type storedSessionContext struct {
	bundleID    string
	workspaceID string
}

func existingContexts(tx *sql.Tx) (map[string]storedSessionContext, error) {
	panic("unported: mj-store/src/database/state_io.rs existing_contexts")
}

func sessionExists(tx *sql.Tx, sessionID string) (bool, error) {
	panic("unported: mj-store/src/database/state_io.rs session_exists")
}

func writeMaterializedSession(tx *sql.Tx, materialized *core.MaterializedSession) error {
	panic("unported: mj-store/src/database/state_io.rs write_materialized_session")
}

// transcriptStorageMetadata is the item kind, settled flag and update
// ordinal a transcript row stores beside its body.
func transcriptStorageMetadata(body core.TranscriptBody, position uint64, latestContentEventOrdinal *uint64) (string, bool, uint64) {
	settled := true
	switch body := body.(type) {
	case core.TranscriptBodyAgent:
		settled = !body.Streaming
	case core.TranscriptBodyThought:
		settled = !body.Streaming
	case core.TranscriptBodyTool:
		status, ok := body.Call.RawStatus()
		if !ok {
			status = "pending"
		}
		settled = status != "pending" && status != "in_progress"
	}
	updateOrdinal := position
	if latestContentEventOrdinal != nil {
		updateOrdinal = *latestContentEventOrdinal
	}
	return core.TranscriptBodyRole(body).StorageKind(), settled, updateOrdinal
}

func upsertTranscriptItem(tx *sql.Tx, sessionID string, item *core.TranscriptItem) error {
	panic("unported: mj-store/src/database/state_io.rs upsert_transcript_item")
}

func replaceMaterializedQueue(tx *sql.Tx, sessionID string, queuedPrompts []core.MaterializedQueuedPrompt) error {
	panic("unported: mj-store/src/database/state_io.rs replace_materialized_queue")
}

// materializedExecutionColumns is the execution_state and
// running_started_at_ms columns of an execution state.
func materializedExecutionColumns(execution core.MaterializedExecutionState) (string, *int64) {
	switch execution := execution.(type) {
	case core.MaterializedExecutionStateRunning:
		startedAtMs := execution.StartedAtMs
		return "running", &startedAtMs
	case core.MaterializedExecutionStateClosing:
		return "closing", nil
	case core.MaterializedExecutionStateClosed:
		return "closed", nil
	}
	return "idle", nil
}

func parseMaterializedExecution(execution string, runningStartedAtMs *int64) (core.MaterializedExecutionState, error) {
	switch {
	case execution == "idle" && runningStartedAtMs == nil:
		return core.MaterializedExecutionStateIdle{}, nil
	case execution == "running" && runningStartedAtMs != nil:
		return core.MaterializedExecutionStateRunning{StartedAtMs: *runningStartedAtMs}, nil
	case execution == "closing" && runningStartedAtMs == nil:
		return core.MaterializedExecutionStateClosing{}, nil
	case execution == "closed" && runningStartedAtMs == nil:
		return core.MaterializedExecutionStateClosed{}, nil
	}
	return nil, fmt.Errorf("invalid materialized execution state %s", core.RustDebugString(execution))
}

// insertSession writes every field of a session, including the ones other
// writers own. Only a flow that authors the whole record — creation, import,
// resume, or orphan adoption — may use this.
func insertSession(tx *sql.Tx, session *core.SessionRecord) error {
	return insertSessionWithParent(tx, session, nil)
}

func insertSessionWithParent(tx *sql.Tx, session *core.SessionRecord, parent *string) error {
	panic("unported: mj-store/src/database/state_io.rs insert_session_with_parent")
}

// updateLifecycleFields updates the columns a lifecycle transition owns, plus
// the target locator that provisioning and teardown maintain with them. The
// row must exist: a transition never resurrects a session another writer
// deleted.
func updateLifecycleFields(tx *sql.Tx, session *core.SessionRecord) error {
	panic("unported: mj-store/src/database/state_io.rs update_lifecycle_fields")
}

func replaceTargets(tx *sql.Tx, session *core.SessionRecord) error {
	return replaceTargetsWithParent(tx, session, nil)
}

func replaceTargetsWithParent(tx *sql.Tx, session *core.SessionRecord, parent *string) error {
	panic("unported: mj-store/src/database/state_io.rs replace_targets_with_parent")
}

func replaceCheckpoint(tx *sql.Tx, session *core.SessionRecord) error {
	panic("unported: mj-store/src/database/state_io.rs replace_checkpoint")
}

// storedTargetColumns is Rust's StoredTargetValues: the session_targets
// columns of one target locator. (Renamed because Go cannot give the type and
// storedTargetValues, the function that builds it, the same name.) A nil
// pointer or slice is a NULL column.
type storedTargetColumns struct {
	Kind             string
	Host             *string
	Resource         *string
	Address          *string
	Workspace        []byte
	WorkerID         *string
	WorkspaceStorage *string
	BorrowedFrom     *string
}

func storedTargetValues(target core.TargetLocator) (storedTargetColumns, error) {
	panic("unported: mj-store/src/database/state_io.rs stored_target_values")
}

func insertTarget(tx *sql.Tx, sessionID string, target core.TargetLocator) error {
	panic("unported: mj-store/src/database/state_io.rs insert_target")
}

func loadTargets(connection sqlConnection, state *core.StateState) error {
	return loadTargetsSelected(connection, state, nil)
}

func loadTargetsSelected(connection sqlConnection, state *core.StateState, id *string) error {
	panic("unported: mj-store/src/database/state_io.rs load_targets_selected")
}

func readTargets(connection sqlConnection, id *string) (map[string]core.TargetLocator, error) {
	panic("unported: mj-store/src/database/state_io.rs read_targets")
}

// decodeTarget decodes the eight target columns (kind, host, resource_id,
// address, workspace, worker_id, workspace_storage, borrowed_from) that start
// at column start of a scanned row. row holds the row's column values as
// scanned (rusqlite's Row); a NULL column is nil.
func decodeTarget(row []any, start int) (core.TargetLocator, error) {
	panic("unported: mj-store/src/database/state_io.rs decode_target")
}

// replaceMounts rewrites a session's attached directories.
//
// session_mounts.read_only keeps the meaning older builds understand, so
// read-write mounts are stored there as not read-only and recorded again in
// session_mount_access. Older builds rewrite session_mounts without touching
// that table, which is what keeps the read-write choice.
func replaceMounts(tx *sql.Tx, sessionID string, mounts []core.AdditionalMount) error {
	panic("unported: mj-store/src/database/state_io.rs replace_mounts")
}

func loadMounts(connection sqlConnection, state *core.StateState) error {
	return loadMountsSelected(connection, state, nil)
}

func loadMountsSelected(connection sqlConnection, state *core.StateState, id *string) error {
	panic("unported: mj-store/src/database/state_io.rs load_mounts_selected")
}

func loadCheckpoints(connection sqlConnection, state *core.StateState) error {
	return loadCheckpointsSelected(connection, state, nil)
}

func loadCheckpointsSelected(connection sqlConnection, state *core.StateState, id *string) error {
	panic("unported: mj-store/src/database/state_io.rs load_checkpoints_selected")
}

// BackfillTargetRuntime fills a missing snapshot without overwriting a
// concurrent writer. A deleted or retargeted session returns nil; its stale
// controller copy can be dropped by reload instead of preventing every
// subsequent reload.
func BackfillTargetRuntime(sessionID string, targetTemplateID string, runtime *core.TargetRuntimeSettings) (*core.TargetRuntimeSettings, error) {
	panic("unported: mj-store/src/database/state_io.rs backfill_target_runtime")
}

const sessionQuery = `SELECT s.session_id, s.title, s.harness_kind, s.last_profile, c.bundle_id,
                s.target_template_id, s.state, s.native_session_id, s.acp_session_title,
                s.session_title_override, c.created_at, s.updated_at,
                s.viewed_through_event_ordinal, s.last_error, s.resource_allocation,
                s.last_checkpoint_error, s.project_directory, s.managed_worktree,
                s.draft_input, s.archived
                , c.workspace_id, s.create_managed_worktree, s.subagents,
                s.container_workspace, s.build_cache_json, s.launch_base, s.target_runtime_json,
                s.launch_branch, s.publication_json, s.checkout_json, s.project_json,
                s.review_json, s.no_project_memory, s.no_mailbox, s.oracle, s.oracle_choice,
                s.no_github_credentials, s.notifications_ignored, s.dismissed_failure_json, s.failure_generation
         FROM sessions s JOIN session_contexts c USING(session_id)`

// decodeSession decodes one sessionQuery row, or nil for a session whose
// harness this build no longer supports. row holds the row's column values
// as scanned (rusqlite's Row); a NULL column is nil.
func decodeSession(row []any, interns *sessionInterns) (*core.SessionRecord, error) {
	panic("unported: mj-store/src/database/state_io.rs decode_session")
}

// LoadSessionRecord reads one complete record with indexed queries from the
// caller's WAL snapshot.
func LoadSessionRecord(id string) (*core.SessionRecord, error) {
	panic("unported: mj-store/src/database/state_io.rs load_session_record")
}

// ReadDurableSessionRecord: recovery checks storage under target ownership
// before touching a process. This bounded read also detects a broken store
// even without a pending write.
func ReadDurableSessionRecord(id string) (*core.SessionRecord, error) {
	panic("unported: mj-store/src/database/state_io.rs read_durable_session_record")
}

func loadSessionWith(connection sqlConnection, id string) (*core.SessionRecord, error) {
	return loadSessionInternedWith(connection, id, &sessionInterns{})
}

func loadSessionInternedWith(connection sqlConnection, id string, interns *sessionInterns) (*core.SessionRecord, error) {
	panic("unported: mj-store/src/database/state_io.rs load_session_interned_with")
}

// LoadOperatorPresence reads the daemon-owned setting without loading all
// session records.
func LoadOperatorPresence() (core.OperatorPresence, error) {
	panic("unported: mj-store/src/database/state_io.rs load_operator_presence")
}

// SetOperatorAway: the shared database writer owns transition and no-op
// decisions.
func SetOperatorAway(away bool, autoAfkWindow time.Duration) (bool, error) {
	panic("unported: mj-store/src/database/state_io.rs set_operator_away")
}

// RecordOperatorAwayWorkerProtocol persists the last negotiated worker
// protocol so AFK-off remains correct before the daemon has reattached every
// worker after a restart.
func RecordOperatorAwayWorkerProtocol(session string, protocol uint32) error {
	panic("unported: mj-store/src/database/state_io.rs record_operator_away_worker_protocol")
}

// updateContainerRequests: only explicit dialog choices change next-creation
// requests. A nil allocation leaves it as it is; a pointer to a nil
// allocation clears it. A nil mounts leaves the mounts as they are.
func updateContainerRequests(tx *sql.Tx, sessionID string, allocation *core.SessionResourceAllocation, mounts *[]core.AdditionalMount) error {
	panic("unported: mj-store/src/database/state_io.rs update_container_requests")
}
