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
	"database/sql/driver"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"reflect"
	"runtime"
	"slices"
	"strings"
	"sync"
	"weak"

	"modernc.org/sqlite"

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

// changedRecordFunction is the SQL function the observer triggers call.
//
// Rust registers mj_changed_record(kind, key) on each connection as a
// closure over that connection's store path. modernc.org/sqlite registers
// functions for every connection the driver opens, so the Go function takes
// the path as a leading argument: each connection's temporary triggers pass
// the identifier observedStoreID gave its path. The triggers and the
// function exist only on the connection, never in the durable schema.
const changedRecordFunction = "mj_changed_record"

func init() {
	sqlite.MustRegisterScalarFunction(changedRecordFunction, 3, changedRecord)
}

// observedStores numbers the store paths observeConnection has seen, so a
// trigger names its store without quoting a path into SQL.
var observedStores = struct {
	sync.Mutex
	ids   map[string]int64
	paths []string
}{ids: map[string]int64{}}

func observedStoreID(path string) int64 {
	observedStores.Lock()
	defer observedStores.Unlock()
	if id, ok := observedStores.ids[path]; ok {
		return id
	}
	id := int64(len(observedStores.paths))
	observedStores.paths = append(observedStores.paths, path)
	observedStores.ids[path] = id
	return id
}

func observedStorePath(id int64) (string, bool) {
	observedStores.Lock()
	defer observedStores.Unlock()
	if id < 0 || id >= int64(len(observedStores.paths)) {
		return "", false
	}
	return observedStores.paths[id], true
}

// observedConnections is the store path of each observed connection, so
// finishOperation takes the collector of the store its writer runs on (in
// Rust the writer thread's single PENDING). An entry goes away with its
// connection.
var observedConnections = struct {
	sync.Mutex
	paths map[weak.Pointer[Connection]]string
}{paths: map[weak.Pointer[Connection]]string{}}

func observedConnectionPath(connection *Connection) (string, bool) {
	observedConnections.Lock()
	defer observedConnections.Unlock()
	path, ok := observedConnections.paths[weak.Make(connection)]
	return path, ok
}

// changedRecord records one changed (kind, key) for the job in progress on
// the store its trigger observes; outside a job it does nothing.
func changedRecord(_ *sqlite.FunctionContext, args []driver.Value) (driver.Value, error) {
	store, ok := args[0].(int64)
	if !ok {
		return nil, fmt.Errorf("%s: invalid store argument %T", changedRecordFunction, args[0])
	}
	kind, err := changedRecordText(args[1])
	if err != nil {
		return nil, err
	}
	key, err := changedRecordText(args[2])
	if err != nil {
		return nil, err
	}
	path, ok := observedStorePath(store)
	if !ok {
		return int64(0), nil
	}
	pending.Lock()
	defer pending.Unlock()
	if changes := pending.byPath[path]; changes != nil && changes.path == path {
		changes.keys[[2]string{kind, key}] = struct{}{}
	}
	return int64(0), nil
}

// changedRecordText is rusqlite's String conversion of a function argument.
func changedRecordText(value driver.Value) (string, error) {
	switch value := value.(type) {
	case string:
		return value, nil
	case []byte:
		return string(value), nil
	case nil:
		return "", errors.New("Invalid column type Null")
	case int64:
		return "", errors.New("Invalid column type Integer")
	case float64:
		return "", errors.New("Invalid column type Real")
	}
	return "", fmt.Errorf("Invalid column type %T", value)
}

// observedTables are the tables whose changes publish a record: table,
// record kind, and the column holding the record key.
var observedTables = [][3]string{
	{"sessions", "session", "session_id"},
	{"session_contexts", "session", "session_id"},
	{"session_targets", "session", "session_id"},
	{"session_mounts", "session", "session_id"},
	{"session_mount_access", "session", "session_id"},
	{"session_checkpoints", "session", "session_id"},
	{"environment_bindings", "environment_owner", "session_id"},
	{"subagent_sessions", "relation", "child_session_id"},
	{"subagent_preference", "preference", "profile_id"},
	{"oracle_preference", "oracle_preference", "profile_id"},
	{"operator_away", "operator_away", "singleton"},
	{"mount_history", "mount_history", "host"},
	{"project_locations", "mount_history", "host"},
	{"host_container_sizes", "container_size", "host"},
	{"session_moves", "move", "session_id"},
	{"native_agents", "native_agent", "owner"},
	{"startup_steps", "startup", "session_id"},
	{"subagent_handbacks", "report", "child_session_id"},
	{"materialized_sessions", "turn", "session_id"},
	{"api_events", "turn", "session_id"},
}

// observeConnection gives every writable connection the same observer. The
// collector is active only inside an accepted writer job and only for that
// job's database path.
func observeConnection(connection *Connection, path string) error {
	store := observedStoreID(path)
	key := weak.Make(connection)
	observedConnections.Lock()
	observedConnections.paths[key] = path
	observedConnections.Unlock()
	runtime.AddCleanup(connection, func(key weak.Pointer[Connection]) {
		observedConnections.Lock()
		delete(observedConnections.paths, key)
		observedConnections.Unlock()
	}, key)
	for _, observed := range observedTables {
		table, kind, column := observed[0], observed[1], observed[2]
		// Observe tables that exist; opening a connection must not depend
		// on an unrelated optional table. Its own read/write still reports
		// damage.
		var exists bool
		if err := connection.QueryRow(
			"SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE type='table' AND name=?1)",
			table,
		).Scan(&exists); err != nil {
			return err
		}
		if !exists {
			continue
		}
		for _, trigger := range []struct {
			event      string
			references []string
		}{
			{"INSERT", []string{"NEW"}},
			{"DELETE", []string{"OLD"}},
			{"UPDATE", []string{"OLD", "NEW"}},
		} {
			// A projection page can resolve a form and raise an identical
			// form before committing, leaving the pending JSON unchanged.
			if table == "api_events" && trigger.event != "INSERT" {
				continue
			}
			var calls strings.Builder
			for _, reference := range trigger.references {
				var key string
				switch {
				case table == "project_locations":
					key = fmt.Sprintf("'project:' || CAST(%s.host AS TEXT)", reference)
				case kind == "native_agent":
					key = fmt.Sprintf("json_array(%s.owner, %s.child)", reference, reference)
				default:
					key = fmt.Sprintf("CAST(%s.%s AS TEXT)", reference, column)
				}
				fmt.Fprintf(&calls, "SELECT %s(%d, '%s', %s);", changedRecordFunction, store, kind, key)
			}
			condition := ""
			switch {
			case table == "api_events":
				condition = " WHEN json_extract(NEW.body, '$.type') IN ('input_required', 'input_resolved')"
			case kind == "turn" && trigger.event == "UPDATE":
				condition = ` WHEN OLD.session_id IS NOT NEW.session_id
                   OR OLD.execution_state IS NOT NEW.execution_state
                   OR OLD.running_started_at_ms IS NOT NEW.running_started_at_ms
                   OR OLD.active_turn_json IS NOT NEW.active_turn_json
                   OR OLD.last_turn_outcome_json IS NOT NEW.last_turn_outcome_json
                   OR OLD.pending_elicitations_json IS NOT NEW.pending_elicitations_json`
			}
			if _, err := connection.Exec(fmt.Sprintf(
				"CREATE TEMP TRIGGER mj_observe_%s_%s AFTER %s ON main.%s%s\n                 BEGIN %s END;",
				table, trigger.event, trigger.event, table, condition, calls.String(),
			)); err != nil {
				return err
			}
		}
	}
	return nil
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
	if t == nil || other == nil {
		return t == other
	}
	return reflect.DeepEqual(t.State, other.State) &&
		committedOptionalEqual(t.FailedMessage, other.FailedMessage) &&
		maps.Equal(t.PendingFormTimes, other.PendingFormTimes)
}

// committedOptionalEqual is Option<T> equality on pointers.
func committedOptionalEqual[T comparable](a, b *T) bool {
	if a == nil || b == nil {
		return a == b
	}
	return *a == *b
}

// committedRecordEqual is Option<&T> equality on records, by value.
func committedRecordEqual[T any](a, b *T) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return a == b || reflect.DeepEqual(*a, *b)
}

// committedStrings collects one text column, as Rust collects a query_map
// before reading anything else on the connection.
func committedStrings(connection Queryer, query string, args ...any) ([]string, error) {
	rows, err := connection.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var values []string
	for rows.Next() {
		var value string
		if err := rows.Scan(&value); err != nil {
			return nil, err
		}
		values = append(values, value)
	}
	return values, rows.Err()
}

// committedTurnRead is CommittedTurn::read: the session's turn record, nil
// when it has no projection row. previous is the published record, whose
// failed message is reused while the failed turn is unchanged.
func committedTurnRead(connection Queryer, id string, previous *CommittedTurn) (*CommittedTurn, error) {
	state, err := readMaterializedTurnState(connection, id)
	if err != nil || state == nil {
		return nil, err
	}
	var failedMessage *string
	if turn := state.LastTurnOutcome; turn != nil {
		if _, _, failed := core.FailedTurn(turn, nil); failed {
			switch {
			case previous != nil && reflect.DeepEqual(previous.State.LastTurnOutcome, state.LastTurnOutcome):
				failedMessage = previous.FailedMessage
			case turn.TurnStartPosition != nil:
				failedMessage, err = lastMaterializedAgentMessageWithin(connection, id, *turn.TurnStartPosition, turn.CompletedOrdinal)
				if err != nil {
					return nil, err
				}
			}
		}
	}
	requests, err := committedStrings(connection,
		`SELECT json_extract(request.value, '$.id')
             FROM materialized_sessions AS session, json_each(session.pending_elicitations_json) AS request
             WHERE session.session_id = ?1`, id)
	if err != nil {
		return nil, err
	}
	pendingFormTimes := map[string]int64{}
	for _, request := range requests {
		// An ID may be reused after a response. Within the current
		// request, later body updates preserve when it was raised.
		var raised int64
		err := connection.QueryRow(`SELECT recorded_at_ms FROM api_events
                     WHERE session_id = ?1
                       AND json_extract(body, '$.type') = 'input_required'
                       AND json_extract(body, '$.data.request.id') = ?2
                       AND seq > COALESCE((
                           SELECT MAX(seq) FROM api_events
                           WHERE session_id = ?1
                             AND json_extract(body, '$.type') = 'input_resolved'
                             AND json_extract(body, '$.data.elicitation_id') = ?2
                       ), 0)
                     ORDER BY seq LIMIT 1`, id, request).Scan(&raised)
		if err != nil && !errors.Is(err, sql.ErrNoRows) {
			return nil, err
		}
		pendingFormTimes[request] = raised
	}
	return &CommittedTurn{
		State:            *state,
		FailedMessage:    failedMessage,
		PendingFormTimes: pendingFormTimes,
	}, nil
}

// committedStateBootstrap is CommittedState::bootstrap: every published
// record, read from one snapshot when the writer starts.
func committedStateBootstrap(connection *Connection) (*CommittedState, error) {
	transaction, err := connection.BeginWith(TransactionDeferred)
	if err != nil {
		return nil, err
	}
	defer transaction.Rollback()
	state, err := loadStateWith(transaction)
	if err != nil {
		return nil, err
	}
	ids, err := committedStrings(transaction, "SELECT session_id FROM materialized_sessions")
	if err != nil {
		return nil, err
	}
	turns := core.NewSnapshotMap[string, CommittedTurn]()
	for _, id := range ids {
		turn, err := committedTurnRead(transaction, id, nil)
		if err != nil {
			return nil, err
		}
		if turn != nil {
			turns.InsertShared(id, *turn)
		}
	}
	startupGroups := core.NewSnapshotMap[string, StartupGroupStatus]()
	sessionIDs, err := committedStrings(transaction, "SELECT DISTINCT session_id FROM startup_steps WHERE group_id IS NOT NULL")
	if err != nil {
		return nil, err
	}
	for _, sessionID := range sessionIDs {
		group, err := loadLatestStartupStatusWith(transaction, sessionID)
		if err != nil {
			return nil, err
		}
		if group != nil {
			startupGroups.Insert(sessionID, *group)
		}
	}
	subagentReports := core.NewSnapshotMap[string, core.SubagentReport]()
	children, err := committedStrings(transaction, "SELECT child_session_id FROM subagent_handbacks")
	if err != nil {
		return nil, err
	}
	for _, child := range children {
		report, err := loadSubagentReportWith(transaction, child)
		if err != nil {
			return nil, err
		}
		if report != nil {
			subagentReports.Insert(child, *report)
		}
	}
	operations, err := loadMoveOperationsWith(transaction)
	if err != nil {
		return nil, err
	}
	moves := core.NewSnapshotMap[string, core.MoveOperation]()
	for _, operation := range operations {
		moves.Insert(operation.Selection.SessionID, operation)
	}
	nativeAgents, err := committedNativeAgents(transaction)
	if err != nil {
		return nil, err
	}
	return &CommittedState{
		Sequence:        0,
		State:           state,
		Moves:           moves,
		NativeAgents:    nativeAgents,
		StartupGroups:   startupGroups,
		SubagentReports: subagentReports,
		Turns:           turns,
		WaitRevisions:   core.NewSnapshotMap[string, uint64](),
	}, nil
}

// committedNativeAgents is every published native agent summary, by owner
// and child.
func committedNativeAgents(transaction *Transaction) (core.SnapshotMap[string, core.SnapshotMap[string, core.NativeAgentSummary]], error) {
	nativeAgents := core.NewSnapshotMap[string, core.SnapshotMap[string, core.NativeAgentSummary]]()
	rows, err := transaction.Query("SELECT owner, child, body FROM native_agents WHERE staging=0")
	if err != nil {
		return nativeAgents, err
	}
	defer rows.Close()
	for rows.Next() {
		var owner, child, body string
		if err := rows.Scan(&owner, &child, &body); err != nil {
			return nativeAgents, err
		}
		var view core.NativeAgentView
		if err := json.Unmarshal([]byte(body), &view); err != nil {
			return nativeAgents, err
		}
		children, _ := nativeAgents.Get(owner)
		children.Insert(child, core.NativeAgentSummaryOf(&view))
		nativeAgents.Insert(owner, children)
	}
	return nativeAgents, rows.Err()
}

// finishOperation reads all changed records from one committed WAL snapshot.
// The caller owns the sole write lane, so no later job can overtake this
// publication. Nil when nothing published changed.
func finishOperation(connection *Connection, previous *CommittedState) (*CommittedState, error) {
	var changes *pendingChanges
	if path, ok := observedConnectionPath(connection); ok {
		pending.Lock()
		changes = pending.byPath[path]
		delete(pending.byPath, path)
		pending.Unlock()
	}
	if changes == nil {
		return nil, errors.New("database writer operation has no change collector")
	}
	if !connection.isAutocommit() {
		return nil, errors.New("writer job left a transaction open")
	}
	if len(changes.keys) == 0 {
		return nil, nil
	}
	keys := slices.SortedFunc(maps.Keys(changes.keys), func(a, b [2]string) int {
		if order := strings.Compare(a[0], b[0]); order != 0 {
			return order
		}
		return strings.Compare(a[1], b[1])
	})
	transaction, err := connection.BeginWith(TransactionDeferred)
	if err != nil {
		return nil, err
	}
	defer transaction.Rollback()
	var interns *sessionInterns
	state := previous.State
	// The plain maps of the published state are shared; each is copied
	// before its first edit.
	policiesOwned, choicesOwned := false, false
	moves := previous.Moves
	nativeAgents := previous.NativeAgents
	startupGroups := previous.StartupGroups
	subagentReports := previous.SubagentReports
	turns := previous.Turns
	changedHistory := core.NewStateState()
	changed := false
	relations := map[string]struct{}{}
	for _, change := range keys {
		kind, key := change[0], change[1]
		switch kind {
		case "turn":
			current := turns.GetShared(key)
			turn, err := committedTurnRead(transaction, key, current)
			if err != nil {
				return nil, err
			}
			if !current.Equal(turn) {
				if turn != nil {
					turns.InsertShared(key, *turn)
				} else {
					turns.RemoveShared(key)
				}
				changed = true
			}
		case "move":
			operation, err := loadMoveOperationWith(transaction, key)
			if err != nil {
				return nil, err
			}
			if !committedRecordEqual(moves.GetShared(key), operation) {
				if operation != nil {
					moves.Insert(key, *operation)
				} else {
					moves.Remove(key)
				}
				changed = true
			}
		case "native_agent":
			var pair []string
			if err := json.Unmarshal([]byte(key), &pair); err != nil {
				return nil, err
			}
			if len(pair) != 2 {
				return nil, fmt.Errorf("invalid length %d, expected a tuple of size 2", len(pair))
			}
			owner, child := pair[0], pair[1]
			var body *string
			err := transaction.QueryRow(
				"SELECT body FROM native_agents WHERE owner=?1 AND child=?2 AND staging=0",
				owner, child,
			).Scan(&body)
			if err != nil && !errors.Is(err, sql.ErrNoRows) {
				return nil, err
			}
			var summary *core.NativeAgentSummary
			if body != nil {
				var view core.NativeAgentView
				if err := json.Unmarshal([]byte(*body), &view); err != nil {
					return nil, err
				}
				value := core.NativeAgentSummaryOf(&view)
				summary = &value
			}
			var old *core.NativeAgentSummary
			if children := nativeAgents.GetShared(owner); children != nil {
				old = children.GetShared(child)
			}
			if !committedRecordEqual(old, summary) {
				if summary != nil {
					children, _ := nativeAgents.Get(owner)
					children.Insert(child, *summary)
					nativeAgents.Insert(owner, children)
				} else if children := nativeAgents.GetShared(owner); children != nil {
					updated := *children
					updated.Remove(child)
					if updated.IsEmpty() {
						nativeAgents.Remove(owner)
					} else {
						nativeAgents.Insert(owner, updated)
					}
				}
				changed = true
			}
		case "session":
			// Most writes (drafts, titles, lifecycle fields) leave the
			// project and runtime unchanged: reuse this record's values.
			// Only a changed value consults the pool over all sessions.
			previousRecord := state.Sessions.GetShared(key)
			local, err := sessionInternsFromRecord(previousRecord)
			if err != nil {
				return nil, err
			}
			record, err := loadSessionInternedWith(transaction, key, local)
			if err != nil {
				return nil, err
			}
			newSharedValue := false
			if record != nil {
				var oldProject *core.ProjectBundleSnapshot
				var oldRuntime *core.TargetRuntimeSettings
				if previousRecord != nil {
					oldProject, oldRuntime = previousRecord.Project, previousRecord.TargetRuntime
				}
				newSharedValue = (record.Project != nil && (previousRecord == nil || oldProject != record.Project)) ||
					(record.TargetRuntime != nil && (previousRecord == nil || oldRuntime != record.TargetRuntime))
			}
			if newSharedValue {
				if interns == nil {
					if interns, err = sessionInternsFromState(&state); err != nil {
						return nil, err
					}
				}
				if err := interns.share(record); err != nil {
					return nil, err
				}
			}
			membershipChanged := state.Sessions.ContainsKey(key) != (record != nil)
			if !committedRecordEqual(state.Sessions.GetShared(key), record) {
				if record != nil {
					state.Sessions.Insert(key, *record)
				} else {
					state.Sessions.Remove(key)
				}
				changed = true
			}
			// A formerly unsupported parent/child may now be readable.
			// Re-evaluate only its indexed relationships, not all history.
			if membershipChanged {
				children, err := committedStrings(transaction, `SELECT child_session_id FROM subagent_sessions
                         WHERE parent_session_id=?1 OR child_session_id=?1`, key)
				if err != nil {
					return nil, err
				}
				for _, child := range children {
					relations[child] = struct{}{}
				}
			}
		case "environment_owner":
			owner, err := sessionEnvironmentOwnerWith(transaction, key)
			if err != nil {
				return nil, err
			}
			if !committedOptionalEqual(state.EnvironmentOwners.GetShared(key), owner) {
				if owner != nil {
					state.EnvironmentOwners.Insert(key, *owner)
				} else {
					state.EnvironmentOwners.Remove(key)
				}
				changed = true
			}
		case "relation":
			relations[key] = struct{}{}
			children, err := committedStrings(transaction,
				"SELECT child_session_id FROM subagent_sessions WHERE parent_session_id=?1", key)
			if err != nil {
				return nil, err
			}
			for _, child := range children {
				relations[child] = struct{}{}
			}
		case "operator_away":
			var presence core.OperatorPresence
			if err := transaction.QueryRow("SELECT away,revision FROM operator_away WHERE singleton=1").
				Scan(&presence.Away, &presence.Revision); err != nil {
				return nil, err
			}
			if state.OperatorPresence != presence {
				state.OperatorPresence = presence
				changed = true
			}
		case "preference":
			var text *string
			err := transaction.QueryRow("SELECT policy FROM subagent_preference WHERE profile_id=?1", key).Scan(&text)
			if err != nil && !errors.Is(err, sql.ErrNoRows) {
				return nil, err
			}
			var policy *core.SubagentPolicy
			if text != nil {
				parsed, err := core.ParseStoredPolicy(*text)
				if err != nil {
					return nil, err
				}
				policy = &parsed
			}
			var prior *core.SubagentPolicy
			if value, ok := state.LastSubagentPolicies[key]; ok {
				prior = &value
			}
			if !committedRecordEqual(prior, policy) {
				if !policiesOwned {
					state.LastSubagentPolicies = maps.Clone(state.LastSubagentPolicies)
					if state.LastSubagentPolicies == nil {
						state.LastSubagentPolicies = map[string]core.SubagentPolicy{}
					}
					policiesOwned = true
				}
				if policy != nil {
					state.LastSubagentPolicies[key] = *policy
				} else {
					delete(state.LastSubagentPolicies, key)
				}
				changed = true
			}
		case "oracle_preference":
			var enabled *bool
			err := transaction.QueryRow("SELECT enabled FROM oracle_preference WHERE profile_id=?1", key).Scan(&enabled)
			if err != nil && !errors.Is(err, sql.ErrNoRows) {
				return nil, err
			}
			var current *bool
			if value, ok := state.LastOracleChoices[key]; ok {
				current = &value
			}
			if !committedOptionalEqual(current, enabled) {
				if !choicesOwned {
					state.LastOracleChoices = maps.Clone(state.LastOracleChoices)
					if state.LastOracleChoices == nil {
						state.LastOracleChoices = map[string]bool{}
					}
					choicesOwned = true
				}
				if enabled != nil {
					state.LastOracleChoices[key] = *enabled
				} else {
					delete(state.LastOracleChoices, key)
				}
				changed = true
			}
		case "mount_history":
			history, err := readMountHistory(transaction)
			if err != nil {
				return nil, err
			}
			var paths *[]string
			if found := history[key]; len(found) > 0 {
				paths = &found
			}
			current := state.MountHistory.GetShared(key)
			if !committedRecordEqual(current, paths) {
				if paths != nil {
					changedHistory.MountHistory.Insert(key, slices.Clone(*paths))
					state.MountHistory.Insert(key, *paths)
				} else {
					state.MountHistory.Remove(key)
				}
				changed = true
			}
		case "container_size":
			var cpus, memoryBytes int64
			var size *core.HostContainerSize
			err := transaction.QueryRow("SELECT cpus, memory_bytes FROM host_container_sizes WHERE host=?1", key).
				Scan(&cpus, &memoryBytes)
			switch {
			case err == nil:
				size = &core.HostContainerSize{Cpus: uint64(cpus), MemoryBytes: uint64(memoryBytes)}
			case !errors.Is(err, sql.ErrNoRows):
				return nil, err
			}
			if !committedOptionalEqual(state.ContainerSizes.GetShared(key), size) {
				if size != nil {
					changedHistory.ContainerSizes.Insert(key, *size)
					state.ContainerSizes.Insert(key, *size)
				} else {
					state.ContainerSizes.Remove(key)
				}
				changed = true
			}
		case "startup":
			group, err := loadLatestStartupStatusWith(transaction, key)
			if err != nil {
				return nil, err
			}
			if !committedRecordEqual(startupGroups.GetShared(key), group) {
				if group != nil {
					startupGroups.Insert(key, *group)
				} else {
					startupGroups.Remove(key)
				}
				changed = true
			}
		case "report":
			report, err := loadSubagentReportWith(transaction, key)
			if err != nil {
				return nil, err
			}
			if !committedRecordEqual(subagentReports.GetShared(key), report) {
				if report != nil {
					subagentReports.Insert(key, *report)
				} else {
					subagentReports.Remove(key)
				}
				changed = true
			}
		default:
			return nil, fmt.Errorf("unknown committed record kind %s", kind)
		}
	}
	sortedRelations := slices.Sorted(maps.Keys(relations))
	for _, key := range sortedRelations {
		var text *string
		err := transaction.QueryRow("SELECT record_json FROM subagent_sessions WHERE child_session_id=?1", key).Scan(&text)
		if err != nil && !errors.Is(err, sql.ErrNoRows) {
			return nil, err
		}
		var relation *core.SubagentMetadata
		if text != nil {
			var record core.SubagentRecord
			if err := json.Unmarshal([]byte(*text), &record); err != nil {
				return nil, err
			}
			if state.Sessions.ContainsKey(key) && state.Sessions.ContainsKey(record.ParentSessionID) {
				metadata := core.SubagentMetadataFrom(&record)
				relation = &metadata
			}
		}
		if !committedRecordEqual(state.Subagents.GetShared(key), relation) {
			if relation != nil {
				state.Subagents.Insert(key, *relation)
			} else {
				state.Subagents.Remove(key)
			}
			changed = true
		}
	}
	for _, key := range sortedRelations {
		if err := state.ValidateSubagent(key); err != nil {
			return nil, err
		}
	}
	if err := changedHistory.Validate(); err != nil {
		return nil, err
	}
	waitRevisions := previous.WaitRevisions
	next := previous.Sequence + 1
	for id := range previous.State.Sessions.Changes(state.Sessions, committedRecordEqual[core.SessionRecord]) {
		waitRevisions.InsertShared(id, next)
	}
	for id := range previous.State.Subagents.Changes(state.Subagents, committedRecordEqual[core.SubagentMetadata]) {
		waitRevisions.InsertShared(id, next)
	}
	for id := range previous.StartupGroups.Changes(startupGroups, committedRecordEqual[StartupGroupStatus]) {
		waitRevisions.InsertShared(id, next)
	}
	for id := range previous.SubagentReports.Changes(subagentReports, committedRecordEqual[core.SubagentReport]) {
		waitRevisions.InsertShared(id, next)
	}
	for id := range previous.Turns.Changes(turns, (*CommittedTurn).Equal) {
		waitRevisions.InsertShared(id, next)
	}
	if err := transaction.Commit(); err != nil {
		return nil, err
	}
	if !changed {
		return nil, nil
	}
	return &CommittedState{
		Sequence:        next,
		State:           state,
		Moves:           moves,
		NativeAgents:    nativeAgents,
		StartupGroups:   startupGroups,
		SubagentReports: subagentReports,
		Turns:           turns,
		WaitRevisions:   waitRevisions,
	}, nil
}
