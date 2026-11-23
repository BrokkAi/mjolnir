// Port of mj-store/src/database/state_io.rs.

package store

import (
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"slices"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// sessionInterns is scoped to a load/publication: no cache retains deleted
// sessions.
type sessionInterns struct {
	projects map[string]*core.ProjectBundleSnapshot
	runtimes map[string]*core.TargetRuntimeSettings
}

// newSessionInterns is SessionInterns::default.
func newSessionInterns() *sessionInterns {
	return &sessionInterns{
		projects: map[string]*core.ProjectBundleSnapshot{},
		runtimes: map[string]*core.TargetRuntimeSettings{},
	}
}

// sessionInternsFromState is SessionInterns::from_state.
func sessionInternsFromState(state *core.StateState) (*sessionInterns, error) {
	pool := newSessionInterns()
	projects := map[*core.ProjectBundleSnapshot]struct{}{}
	runtimes := map[*core.TargetRuntimeSettings]struct{}{}
	for record := range state.Sessions.Values() {
		if value := record.Project; value != nil {
			if _, seen := projects[value]; !seen {
				projects[value] = struct{}{}
				key, err := databaseMarshalJSON(value)
				if err != nil {
					return nil, err
				}
				if _, ok := pool.projects[string(key)]; !ok {
					pool.projects[string(key)] = value
				}
			}
		}
		if value := record.TargetRuntime; value != nil {
			if _, seen := runtimes[value]; !seen {
				runtimes[value] = struct{}{}
				key, err := databaseMarshalJSON(value)
				if err != nil {
					return nil, err
				}
				if _, ok := pool.runtimes[string(key)]; !ok {
					pool.runtimes[string(key)] = value
				}
			}
		}
	}
	return pool, nil
}

// sessionInternsFromRecord is a pool holding only one record's values. A
// refresh that leaves a session's project and runtime unchanged reuses that
// record's allocations without visiting any other session.
func sessionInternsFromRecord(record *core.SessionRecord) (*sessionInterns, error) {
	pool := newSessionInterns()
	if record != nil {
		if record.Project != nil {
			key, err := databaseMarshalJSON(record.Project)
			if err != nil {
				return nil, err
			}
			pool.projects[string(key)] = record.Project
		}
		if record.TargetRuntime != nil {
			key, err := databaseMarshalJSON(record.TargetRuntime)
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
	if p.projects == nil {
		p.projects = map[string]*core.ProjectBundleSnapshot{}
	}
	if p.runtimes == nil {
		p.runtimes = map[string]*core.TargetRuntimeSettings{}
	}
	if value := record.Project; value != nil {
		key, err := databaseMarshalJSON(value)
		if err != nil {
			return err
		}
		shared, ok := p.projects[string(key)]
		if !ok {
			shared = value
			p.projects[string(key)] = value
		}
		record.Project = shared
	}
	if value := record.TargetRuntime; value != nil {
		key, err := databaseMarshalJSON(value)
		if err != nil {
			return err
		}
		shared, ok := p.runtimes[string(key)]
		if !ok {
			shared = value
			p.runtimes[string(key)] = value
		}
		record.TargetRuntime = shared
	}
	return nil
}

// decodeShared decodes the JSON in column of a scanned row, reusing an equal
// value already in pool. row holds the row's column values as scanned
// (rusqlite's Row); a NULL column is nil.
func decodeShared[T any](row []any, column int, pool *map[string]*T) (*T, error) {
	text, err := sqlRowOptText(row, column)
	if err != nil || text == nil {
		return nil, err
	}
	if *pool == nil {
		*pool = map[string]*T{}
	}
	if value, ok := (*pool)[*text]; ok {
		return value, nil
	}
	value := new(T)
	if err := json.Unmarshal([]byte(*text), value); err != nil {
		return nil, sqlConversionFailure(column, err)
	}
	(*pool)[*text] = value
	return value, nil
}

// --- reading scanned row values as rusqlite's Row::get does ---

// sqlConversionFailure is rusqlite's FromSqlConversionFailure for a TEXT column.
func sqlConversionFailure(column int, err error) error {
	return fmt.Errorf("Conversion error from type Text at index: %d, %w", column, err)
}

func sqlInvalidColumnType(column int, value any) error {
	return fmt.Errorf("Invalid column type %s at index: %d", sqlRowStorageClass(value), column)
}

func sqlRowOptText(row []any, column int) (*string, error) {
	switch value := row[column].(type) {
	case nil:
		return nil, nil
	case string:
		return &value, nil
	default:
		return nil, sqlInvalidColumnType(column, value)
	}
}

func sqlRowText(row []any, column int) (string, error) {
	text, err := sqlRowOptText(row, column)
	if err != nil {
		return "", err
	}
	if text == nil {
		return "", sqlInvalidColumnType(column, nil)
	}
	return *text, nil
}

func sqlRowOptInt64(row []any, column int) (*int64, error) {
	switch value := row[column].(type) {
	case nil:
		return nil, nil
	case int64:
		return &value, nil
	default:
		return nil, sqlInvalidColumnType(column, value)
	}
}

func sqlRowInt64(row []any, column int) (int64, error) {
	value, err := sqlRowOptInt64(row, column)
	if err != nil {
		return 0, err
	}
	if value == nil {
		return 0, sqlInvalidColumnType(column, nil)
	}
	return *value, nil
}

func sqlRowUint64(row []any, column int) (uint64, error) {
	value, err := sqlRowInt64(row, column)
	if err != nil {
		return 0, err
	}
	if value < 0 {
		return 0, fmt.Errorf("Integer %d out of range at index %d", value, column)
	}
	return uint64(value), nil
}

func sqlRowOptBool(row []any, column int) (*bool, error) {
	value, err := sqlRowOptInt64(row, column)
	if err != nil || value == nil {
		return nil, err
	}
	flag := *value != 0
	return &flag, nil
}

func sqlRowBool(row []any, column int) (bool, error) {
	value, err := sqlRowInt64(row, column)
	return value != 0, err
}

// sqlRowBlob is rusqlite's ValueRef::as_blob: the bytes of a BLOB column.
func sqlRowBlob(row []any, column int) ([]byte, error) {
	value, ok := row[column].([]byte)
	if !ok {
		return nil, sqlInvalidColumnType(column, row[column])
	}
	return value, nil
}

// scanSQLRow scans the current row into one any per column.
func scanSQLRow(rows *sql.Rows, columns int) ([]any, error) {
	values := make([]any, columns)
	targets := make([]any, columns)
	for i := range values {
		targets[i] = &values[i]
	}
	if err := rows.Scan(targets...); err != nil {
		return nil, err
	}
	return values, nil
}

// optionalJSONColumn is Option::map(serde_json::to_string): NULL for an absent
// value, its JSON otherwise.
func optionalJSONColumn[T any](value *T) (any, error) {
	if value == nil {
		return nil, nil
	}
	encoded, err := databaseMarshalJSON(value)
	if err != nil {
		return nil, err
	}
	return string(encoded), nil
}

// optionalPathBlob is Option::map(path_to_blob): NULL for an absent path.
func optionalPathBlob(path *string) any {
	if path == nil {
		return nil
	}
	return pathToBlob(*path)
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

// cloneState is State::clone: the plain maps are copied so the caller's
// edits stay its own; snapshot maps copy on write.
func cloneState(state *core.StateState) core.StateState {
	clone := *state
	clone.LastSubagentPolicies = maps.Clone(state.LastSubagentPolicies)
	clone.LastOracleChoices = maps.Clone(state.LastOracleChoices)
	return clone
}

func LoadState() (core.StateState, error) {
	committed, err := WriterCommittedState()
	if err != nil {
		return core.StateState{}, err
	}
	if committed != nil {
		return cloneState(&committed.State), nil
	}
	return LoadStateFrom(DatabasePath())
}

func LoadStateFrom(path string) (core.StateState, error) {
	reader, err := openReader(path)
	if err != nil {
		return core.StateState{}, err
	}
	defer reader.Close()
	// Sessions and their relationships must come from the same WAL snapshot.
	// Otherwise a concurrent spawn can look orphaned despite intact foreign
	// keys.
	connection, err := reader.Begin()
	if err != nil {
		return core.StateState{}, err
	}
	defer connection.Rollback()
	return loadStateWith(connection)
}

func loadStateWith(connection Queryer) (core.StateState, error) {
	state := core.NewStateState()
	if err := connection.QueryRow("SELECT away,revision FROM operator_away WHERE singleton=1").
		Scan(&state.OperatorPresence.Away, &state.OperatorPresence.Revision); err != nil {
		return core.StateState{}, err
	}
	err := func() error {
		rows, err := connection.Query(environmentOwnerQuery)
		if err != nil {
			return err
		}
		defer rows.Close()
		owners := map[string]string{}
		for rows.Next() {
			var sessionID, ownerID string
			if err := rows.Scan(&sessionID, &ownerID); err != nil {
				return err
			}
			owners[sessionID] = ownerID
		}
		if err := rows.Err(); err != nil {
			return err
		}
		state.EnvironmentOwners.Extend(maps.All(owners))
		return nil
	}()
	if err != nil {
		return core.StateState{}, err
	}
	err = func() error {
		rows, err := connection.Query("SELECT profile_id, policy FROM subagent_preference ORDER BY profile_id")
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			var profileID, policyJSON string
			if err := rows.Scan(&profileID, &policyJSON); err != nil {
				return err
			}
			policy, err := core.ParseStoredPolicy(policyJSON)
			if err != nil {
				return sqlConversionFailure(1, err)
			}
			state.LastSubagentPolicies[profileID] = policy
		}
		return rows.Err()
	}()
	if err != nil {
		return core.StateState{}, err
	}
	err = func() error {
		rows, err := connection.Query("SELECT profile_id, enabled FROM oracle_preference ORDER BY profile_id")
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			var profileID string
			var enabled bool
			if err := rows.Scan(&profileID, &enabled); err != nil {
				return err
			}
			state.LastOracleChoices[profileID] = enabled
		}
		return rows.Err()
	}()
	if err != nil {
		return core.StateState{}, err
	}
	err = func() error {
		rows, err := connection.Query(sessionQuery + " ORDER BY s.session_id")
		if err != nil {
			return err
		}
		defer rows.Close()
		interns := newSessionInterns()
		sessions := map[string]core.SessionRecord{}
		for rows.Next() {
			row, err := scanSQLRow(rows, sessionQueryColumns)
			if err != nil {
				return err
			}
			session, err := decodeSession(row, interns)
			if err != nil {
				return err
			}
			if session != nil {
				sessions[session.ID] = *session
			}
		}
		if err := rows.Err(); err != nil {
			return err
		}
		state.Sessions.Extend(maps.All(sessions))
		return nil
	}()
	if err != nil {
		return core.StateState{}, err
	}
	// Rust's cfg(test) hook between the two reads.
	afterStateSessionsRead()
	err = func() error {
		rows, err := connection.Query("SELECT child_session_id, record_json FROM subagent_sessions ORDER BY child_session_id")
		if err != nil {
			return err
		}
		defer rows.Close()
		subagents := map[string]core.SubagentMetadata{}
		for rows.Next() {
			var childID, recordJSON string
			if err := rows.Scan(&childID, &recordJSON); err != nil {
				return err
			}
			var record core.SubagentRecord
			if err := json.Unmarshal([]byte(recordJSON), &record); err != nil {
				return sqlConversionFailure(1, err)
			}
			// A relation whose child or parent is not among the sessions this
			// load returned describes nothing. Keeping it would fail the state
			// check below and make every later operation fail with it, which
			// is how a sub-agent spawn came to be refused with "sub-agent ...
			// has no child session" long after the child in question was gone
			// (#1065). The load already skips a session whose harness it
			// cannot parse; a relation that points at such a session is also
			// omitted from memory. Snapshot saves preserve relations whose
			// endpoints were not loaded.
			missing := ""
			if !state.Sessions.ContainsKey(childID) {
				missing = "child"
			} else if !state.Sessions.ContainsKey(record.ParentSessionID) {
				missing = "parent"
			}
			if missing != "" {
				slog.Warn("dropping a sub-agent relation whose session is not in this state",
					"child_session_id", childID, "parent_session_id", record.ParentSessionID, "missing", missing)
				continue
			}
			subagents[childID] = core.SubagentMetadataFrom(&record)
		}
		if err := rows.Err(); err != nil {
			return err
		}
		state.Subagents.Extend(maps.All(subagents))
		return nil
	}()
	if err != nil {
		return core.StateState{}, err
	}
	if err := loadTargets(connection, &state); err != nil {
		return core.StateState{}, err
	}
	if err := loadMounts(connection, &state); err != nil {
		return core.StateState{}, err
	}
	if err := loadCheckpoints(connection, &state); err != nil {
		return core.StateState{}, err
	}
	history, err := readMountHistory(connection)
	if err != nil {
		return core.StateState{}, err
	}
	state.MountHistory.Extend(maps.All(history))
	err = func() error {
		rows, err := connection.Query("SELECT host, cpus, memory_bytes FROM host_container_sizes ORDER BY host")
		if err != nil {
			return err
		}
		defer rows.Close()
		sizes := map[string]core.HostContainerSize{}
		for rows.Next() {
			var host string
			var cpus, memoryBytes int64
			if err := rows.Scan(&host, &cpus, &memoryBytes); err != nil {
				return err
			}
			sizes[host] = core.HostContainerSize{Cpus: uint64(cpus), MemoryBytes: uint64(memoryBytes)}
		}
		if err := rows.Err(); err != nil {
			return err
		}
		state.ContainerSizes.Extend(maps.All(sizes))
		return nil
	}()
	if err != nil {
		return core.StateState{}, err
	}
	if err := state.Validate(); err != nil {
		return core.StateState{}, err
	}
	return state, nil
}

// LoadMountHistory is the remembered mount sources and project directories
// by host key, newest first, as LoadState reads them. A dashboard reads this
// again when a wizard opens, since sessions created after it started add to
// the history.
func LoadMountHistory() (map[string][]string, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return readMountHistory(connection)
}

func readMountHistory(connection Queryer) (map[string][]string, error) {
	history := map[string][]string{}
	err := func() error {
		rows, err := connection.Query("SELECT host, source FROM mount_history ORDER BY host, ordinal")
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			row, err := scanSQLRow(rows, 2)
			if err != nil {
				return err
			}
			host, err := sqlRowText(row, 0)
			if err != nil {
				return err
			}
			source, err := sqlRowBlob(row, 1)
			if err != nil {
				return err
			}
			history[host] = append(history[host], blobToPath(source))
		}
		return rows.Err()
	}()
	if err != nil {
		return nil, err
	}
	catalog, err := readProjectCatalogFrom(connection)
	if err != nil {
		return nil, err
	}
	for _, location := range catalog.Locations {
		key := "project:" + location.Host
		if !slices.Contains(history[key], location.CheckoutRoot) {
			history[key] = append(history[key], location.CheckoutRoot)
		}
	}
	return history, nil
}

func SaveState(state *core.StateState) error {
	snapshot := cloneState(state)
	_, err := submitDatabaseWrite("save_state", func(*Connection) (struct{}, error) {
		return struct{}{}, SaveStateTo(DatabasePath(), &snapshot)
	})
	return err
}

func SaveStateTo(path string, state *core.StateState) error {
	if err := state.Validate(); err != nil {
		return err
	}
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	contexts, err := existingContexts(tx)
	if err != nil {
		return err
	}
	if _, err := tx.Exec(`DELETE FROM subagent_sessions
         WHERE child_session_id NOT IN (SELECT session_id FROM sessions)
            OR parent_session_id NOT IN (SELECT session_id FROM sessions)`); err != nil {
		return err
	}
	type relation struct{ child, parent string }
	var existingSubagents []relation
	err = func() error {
		rows, err := tx.Query("SELECT child_session_id, parent_session_id FROM subagent_sessions")
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			var r relation
			if err := rows.Scan(&r.child, &r.parent); err != nil {
				return err
			}
			existingSubagents = append(existingSubagents, r)
		}
		return rows.Err()
	}()
	if err != nil {
		return err
	}
	for _, r := range existingSubagents {
		if state.Sessions.ContainsKey(r.child) && state.Sessions.ContainsKey(r.parent) && !state.Subagents.ContainsKey(r.child) {
			if _, err := tx.Exec("DELETE FROM subagent_sessions WHERE child_session_id = ?1", r.child); err != nil {
				return err
			}
		}
	}
	// A report outlives nothing: it goes with its child's relation.
	if _, err := tx.Exec(`DELETE FROM subagent_handbacks
         WHERE child_session_id NOT IN (SELECT child_session_id FROM subagent_sessions)`); err != nil {
		return err
	}
	// A snapshot owns only the records it contains. Absence is not a deletion
	// request: unsupported or concurrently inserted sessions were never
	// loaded. Operational deletion goes through deleteSessionFrom instead.
	if _, err := tx.Exec("DELETE FROM mount_history"); err != nil {
		return err
	}
	if _, err := tx.Exec("DELETE FROM host_container_sizes"); err != nil {
		return err
	}
	for session := range state.Sessions.Values() {
		if existing, ok := contexts[session.ID]; ok {
			canonical, err := sessionProjectID(tx, session.ID, session.BundleID)
			if err != nil {
				return err
			}
			if existing.bundleID != canonical {
				return fmt.Errorf("session %s was already associated with bundle %s, not %s", session.ID, existing.bundleID, session.BundleID)
			}
			if existing.workspaceID != session.WorkspaceID {
				return fmt.Errorf("session %s was already associated with workspace %s, not %s", session.ID, existing.workspaceID, session.WorkspaceID)
			}
		}
		if err := insertSession(tx, session); err != nil {
			return err
		}
	}
	for subagent := range state.Subagents.Values() {
		// State contains metadata only. New launches must be registered with
		// their complete intent; metadata saves preserve those stored fields.
		var storedJSON string
		if err := tx.QueryRow("SELECT record_json FROM subagent_sessions WHERE child_session_id=?1", subagent.ChildSessionID).Scan(&storedJSON); err != nil {
			return fmt.Errorf("sub-agent %s must be registered with its launch intent before saving metadata: %w", subagent.ChildSessionID, err)
		}
		var stored core.SubagentRecord
		if err := json.Unmarshal([]byte(storedJSON), &stored); err != nil {
			return err
		}
		subagent.ApplyTo(&stored)
		if err := recordChildAccounting(tx, &stored, nil); err != nil {
			return err
		}
		recordJSON, err := databaseMarshalJSON(&stored)
		if err != nil {
			return err
		}
		if _, err := tx.Exec(`INSERT INTO subagent_sessions(
                 child_session_id, parent_session_id, request_key, record_json
             ) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(child_session_id) DO UPDATE SET
                 parent_session_id = excluded.parent_session_id,
                 request_key = excluded.request_key,
                 record_json = excluded.record_json`,
			subagent.ChildSessionID, subagent.ParentSessionID, subagent.Kind.RequestKey(subagent.RequestKey), string(recordJSON)); err != nil {
			return err
		}
	}
	for host, sources := range state.MountHistory.All() {
		for ordinal, source := range *sources {
			if _, err := tx.Exec("INSERT INTO mount_history(host, source, ordinal) VALUES (?1, ?2, ?3)",
				host, pathToBlob(source), int64(ordinal)); err != nil {
				return err
			}
		}
	}
	for host, size := range state.ContainerSizes.All() {
		if err := writeHostContainerSize(tx, host, *size); err != nil {
			return err
		}
	}
	return tx.Commit()
}

// storedSessionContext is one session_contexts row's (bundle_id,
// workspace_id).
type storedSessionContext struct {
	bundleID    string
	workspaceID string
}

func existingContexts(tx *sql.Tx) (map[string]storedSessionContext, error) {
	rows, err := tx.Query("SELECT session_id, bundle_id, workspace_id FROM session_contexts")
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	contexts := map[string]storedSessionContext{}
	for rows.Next() {
		var sessionID string
		var context storedSessionContext
		if err := rows.Scan(&sessionID, &context.bundleID, &context.workspaceID); err != nil {
			return nil, err
		}
		contexts[sessionID] = context
	}
	return contexts, rows.Err()
}

func sessionExists(tx *sql.Tx, sessionID string) (bool, error) {
	var one int64
	err := tx.QueryRow("SELECT 1 FROM sessions WHERE session_id = ?1", sessionID).Scan(&one)
	if errors.Is(err, sql.ErrNoRows) {
		return false, nil
	}
	return err == nil, err
}

// nonNilSlice is serde's encoding of a present Vec: an empty one is `[]`,
// never `null`.
func nonNilSlice[T any](values []T) []T {
	if values == nil {
		return []T{}
	}
	return values
}

func writeMaterializedSession(tx *sql.Tx, materialized *core.MaterializedSession) error {
	execution, runningStartedAtMs := materializedExecutionColumns(materialized.Execution)
	configuration, err := databaseMarshalJSON(materialized.Configuration)
	if err != nil {
		return err
	}
	pendingElicitations, err := databaseMarshalJSON(nonNilSlice(materialized.PendingElicitations))
	if err != nil {
		return err
	}
	activeTurn, err := optionalJSONColumn(materialized.ActiveTurn)
	if err != nil {
		return err
	}
	var lastTurnOutcome any
	if materialized.LastTurnOutcome != nil {
		serialized, err := serializeTurnOutcome(materialized.LastTurnOutcome)
		if err != nil {
			return err
		}
		lastTurnOutcome = serialized
	}
	if _, err := tx.Exec(`INSERT INTO materialized_sessions(
             session_id, applied_event_ordinal, applied_event_digest, execution_state,
             running_started_at_ms, session_title, configuration_json, last_activity_at_ms,
             pending_elicitations_json, active_turn_json, last_turn_outcome_json,
             input_required_event_frontier
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?2)
         ON CONFLICT(session_id) DO UPDATE SET
             input_required_event_frontier = max(materialized_sessions.input_required_event_frontier,
                 materialized_sessions.applied_event_ordinal, excluded.applied_event_ordinal),
             applied_event_ordinal = excluded.applied_event_ordinal,
             applied_event_digest = excluded.applied_event_digest,
             execution_state = excluded.execution_state,
             running_started_at_ms = excluded.running_started_at_ms,
             session_title = excluded.session_title,
             configuration_json = excluded.configuration_json,
             last_activity_at_ms = excluded.last_activity_at_ms,
             pending_elicitations_json = excluded.pending_elicitations_json,
             active_turn_json = excluded.active_turn_json,
             last_turn_outcome_json = excluded.last_turn_outcome_json`,
		materialized.SessionID,
		materialized.AppliedEventOrdinal,
		materialized.AppliedEventDigest,
		execution,
		runningStartedAtMs,
		materialized.SessionTitle,
		string(configuration),
		materialized.LastActivityAtMs,
		string(pendingElicitations),
		activeTurn,
		lastTurnOutcome,
	); err != nil {
		return err
	}
	// Repair and restore can reinstall the exact compact summary from an
	// archive. Keep that row unchanged so its already-persisted full detail
	// remains reachable.
	matchedSpills := map[string]struct{}{}
	sessionID := materialized.SessionID
	var containsSpills bool
	if err := tx.QueryRow(`SELECT EXISTS(SELECT 1 FROM materialized_transcript_items
         WHERE session_id = ?1 AND detail_offset IS NOT NULL)`, sessionID).Scan(&containsSpills); err != nil {
		return err
	}
	if containsSpills {
		matchesSpill, err := tx.Prepare(`SELECT EXISTS(SELECT 1 FROM materialized_transcript_items
             WHERE session_id = ?1 AND stable_id = ?2 AND detail_offset IS NOT NULL
               AND position = ?3 AND latest_content_event_ordinal IS ?4
               AND created_at_ms = ?5 AND last_changed_at_ms = ?6 AND body_json = ?7)`)
		if err != nil {
			return err
		}
		defer matchesSpill.Close()
		for _, item := range materialized.Transcript {
			if !core.TranscriptBodyDetailTrimmed(item.Body) {
				continue
			}
			body, err := databaseMarshalJSON(item.Body)
			if err != nil {
				return err
			}
			var matches bool
			if err := matchesSpill.QueryRow(sessionID, item.StableID, item.Position, item.LatestContentEventOrdinal,
				item.CreatedAtMs, item.LastChangedAtMs, string(body)).Scan(&matches); err != nil {
				return err
			}
			if matches {
				matchedSpills[item.StableID] = struct{}{}
			}
		}
	}
	matched := slices.Sorted(maps.Keys(matchedSpills))
	matchedJSON, err := databaseMarshalJSON(nonNilSlice(matched))
	if err != nil {
		return err
	}
	if _, err := tx.Exec(`DELETE FROM materialized_transcript_items WHERE session_id = ?1
         AND stable_id NOT IN (SELECT value FROM json_each(?2))`, sessionID, string(matchedJSON)); err != nil {
		return err
	}
	for _, item := range materialized.Transcript {
		if _, ok := matchedSpills[item.StableID]; !ok {
			if err := upsertTranscriptItem(tx, sessionID, item); err != nil {
				return err
			}
		}
	}
	return replaceMaterializedQueue(tx, materialized.SessionID, materialized.QueuedPrompts)
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
	itemKind, settled, updateOrdinal := transcriptStorageMetadata(item.Body, item.Position, item.LatestContentEventOrdinal)
	body, err := databaseMarshalJSON(item.Body)
	if err != nil {
		return err
	}
	var position uint64
	var latestContentEventOrdinal *uint64
	var createdAtMs, lastChangedAtMs int64
	err = tx.QueryRow(`SELECT position, latest_content_event_ordinal, created_at_ms, last_changed_at_ms
             FROM materialized_transcript_items
             WHERE session_id = ?1 AND stable_id = ?2`, sessionID, item.StableID).
		Scan(&position, &latestContentEventOrdinal, &createdAtMs, &lastChangedAtMs)
	switch {
	case err == nil:
		if position != item.Position || createdAtMs != item.CreatedAtMs {
			return core.ProjectionIntegrityError(fmt.Sprintf("transcript item %s changed immutable identity fields", core.RustDebugString(item.StableID)))
		}
		if item.LastChangedAtMs < lastChangedAtMs {
			return core.ProjectionIntegrityError(fmt.Sprintf("transcript item %s moved its changed timestamp backwards", core.RustDebugString(item.StableID)))
		}
		if latestContentEventOrdinal != nil &&
			(item.LatestContentEventOrdinal == nil || *item.LatestContentEventOrdinal < *latestContentEventOrdinal) {
			return core.ProjectionIntegrityError(fmt.Sprintf("transcript item %s moved its latest content ordinal backwards", core.RustDebugString(item.StableID)))
		}
		_, err = tx.Exec(`UPDATE materialized_transcript_items
             SET latest_content_event_ordinal = ?3, last_changed_at_ms = ?4, body_json = ?5,
                 item_kind = ?6, settled = ?7, update_ordinal = ?8,
                 detail_offset = NULL, detail_len = NULL
             WHERE session_id = ?1 AND stable_id = ?2`,
			sessionID, item.StableID, item.LatestContentEventOrdinal, item.LastChangedAtMs, string(body),
			itemKind, settled, updateOrdinal)
		return err
	case errors.Is(err, sql.ErrNoRows):
		_, err = tx.Exec(`INSERT INTO materialized_transcript_items(
                 session_id, stable_id, position, latest_content_event_ordinal,
                 created_at_ms, last_changed_at_ms, body_json, item_kind, settled, update_ordinal
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)`,
			sessionID, item.StableID, item.Position, item.LatestContentEventOrdinal,
			item.CreatedAtMs, item.LastChangedAtMs, string(body), itemKind, settled, updateOrdinal)
		return err
	default:
		return err
	}
}

func replaceMaterializedQueue(tx *sql.Tx, sessionID string, queuedPrompts []core.MaterializedQueuedPrompt) error {
	commandIDs := map[string]struct{}{}
	for _, prompt := range queuedPrompts {
		if isRustBlank(prompt.CommandID) {
			return errors.New("materialized prompt queue has an empty command id")
		}
		if _, duplicate := commandIDs[prompt.CommandID]; duplicate {
			return fmt.Errorf("materialized prompt queue contains duplicate command %s", core.RustDebugString(prompt.CommandID))
		}
		commandIDs[prompt.CommandID] = struct{}{}
	}
	if _, err := tx.Exec("DELETE FROM materialized_queued_prompts WHERE session_id = ?1", sessionID); err != nil {
		return err
	}
	for ordinal, prompt := range queuedPrompts {
		var promptKind core.QueuedCommandKind = core.QueuedCommandKindPrompt{}
		if prompt.Kind != nil {
			promptKind = prompt.Kind
		}
		kind, err := databaseMarshalJSON(promptKind)
		if err != nil {
			return err
		}
		content, err := databaseMarshalJSON(nonNilSlice(prompt.Content))
		if err != nil {
			return err
		}
		if _, err := tx.Exec(`INSERT INTO materialized_queued_prompts(
                 session_id, ordinal, command_id, kind_json, content_json, queued_at_ms,
                 accepted_ordinal
             ) VALUES (?1,?2,?3,?4,?5,?6,?7)`,
			sessionID, int64(ordinal), prompt.CommandID, string(kind), string(content), prompt.QueuedAtMs, prompt.AcceptedOrdinal); err != nil {
			return err
		}
	}
	return nil
}

// isRustBlank is Rust's `text.trim().is_empty()`.
func isRustBlank(text string) bool {
	for _, r := range text {
		if !isRustWhitespaceRune(r) {
			return false
		}
	}
	return true
}

// isRustWhitespaceRune is Rust's char::is_whitespace (Unicode White_Space).
func isRustWhitespaceRune(r rune) bool {
	switch r {
	case '\t', '\n', '\v', '\f', '\r', ' ', 0x85, 0xA0, 0x1680, 0x2028, 0x2029, 0x202F, 0x205F, 0x3000:
		return true
	}
	return 0x2000 <= r && r <= 0x200A
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
	previousError, err := previousSessionError(tx, session.ID)
	if err != nil {
		return err
	}
	canonicalBundle, err := sessionProjectID(tx, session.ID, session.BundleID)
	if err != nil {
		return err
	}
	if _, err := tx.Exec(`INSERT INTO session_contexts(session_id, bundle_id, created_at, workspace_id)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session_id) DO NOTHING`,
		session.ID, canonicalBundle, session.CreatedAt, session.WorkspaceID); err != nil {
		return err
	}
	var storedBundle, storedWorkspace string
	if err := tx.QueryRow("SELECT bundle_id, workspace_id FROM session_contexts WHERE session_id = ?1", session.ID).
		Scan(&storedBundle, &storedWorkspace); err != nil {
		return err
	}
	if storedBundle != canonicalBundle {
		return fmt.Errorf("session %s belongs to bundle %s, not %s", session.ID, storedBundle, session.BundleID)
	}
	if storedWorkspace != session.WorkspaceID {
		return fmt.Errorf("session %s belongs to workspace %s, not %s", session.ID, storedWorkspace, session.WorkspaceID)
	}
	var resourceAllocation any
	if session.ResourceAllocation != nil {
		encoded, err := databaseMarshalJSON(session.ResourceAllocation)
		if err != nil {
			return err
		}
		resourceAllocation = string(encoded)
	}
	managedWorktree, err := optionalJSONColumn(session.ManagedWorktree)
	if err != nil {
		return err
	}
	subagents, err := optionalJSONColumn(session.Subagents)
	if err != nil {
		return err
	}
	buildCache, err := optionalJSONColumn(session.BuildCache)
	if err != nil {
		return err
	}
	targetRuntime, err := optionalJSONColumn(session.TargetRuntime)
	if err != nil {
		return err
	}
	publication, err := optionalJSONColumn(session.Publication)
	if err != nil {
		return err
	}
	checkout, err := optionalJSONColumn(session.Checkout)
	if err != nil {
		return err
	}
	project, err := optionalJSONColumn(session.Project)
	if err != nil {
		return err
	}
	review, err := optionalJSONColumn(session.Review)
	if err != nil {
		return err
	}
	oracleChoice, err := optionalJSONColumn(session.OracleChoice)
	if err != nil {
		return err
	}
	if _, err := tx.Exec(`INSERT INTO sessions(
             session_id, title, harness_kind, last_profile, target_template_id, state,
             native_session_id, acp_session_title, session_title_override, updated_at,
             viewed_through_event_ordinal, last_error, resource_allocation,
             last_checkpoint_error, project_directory, managed_worktree,
             archived, draft_input, create_managed_worktree,
             subagents, container_workspace, build_cache_json, launch_base,
             target_runtime_json, launch_branch, publication_json, checkout_json, project_json,
             review_json, no_project_memory, no_mailbox, oracle, oracle_choice, no_github_credentials,
             notifications_ignored
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31,?32,?33,?34,?35)
         ON CONFLICT(session_id) DO UPDATE SET
             title = excluded.title,
             harness_kind = excluded.harness_kind,
             last_profile = excluded.last_profile,
             target_template_id = excluded.target_template_id,
             state = excluded.state,
             native_session_id = excluded.native_session_id,
             acp_session_title = excluded.acp_session_title,
             session_title_override = excluded.session_title_override,
             updated_at = excluded.updated_at,
             viewed_through_event_ordinal = max(
                 sessions.viewed_through_event_ordinal,
                 excluded.viewed_through_event_ordinal
             ),
             last_error = excluded.last_error,
             resource_allocation = excluded.resource_allocation,
             last_checkpoint_error = excluded.last_checkpoint_error,
             project_directory = excluded.project_directory,
             managed_worktree = excluded.managed_worktree,
             archived = excluded.archived,
             create_managed_worktree = excluded.create_managed_worktree,
             subagents = excluded.subagents,
             container_workspace = excluded.container_workspace,
             build_cache_json = excluded.build_cache_json,
             launch_base = excluded.launch_base,
             target_runtime_json = excluded.target_runtime_json,
             launch_branch = excluded.launch_branch,
             publication_json = excluded.publication_json,
             checkout_json = excluded.checkout_json,
             project_json = coalesce(excluded.project_json, sessions.project_json),
             review_json = excluded.review_json,
             no_project_memory = excluded.no_project_memory,
             no_mailbox = excluded.no_mailbox,
             oracle = excluded.oracle,
             oracle_choice = excluded.oracle_choice,
             no_github_credentials = excluded.no_github_credentials,
             notifications_ignored = excluded.notifications_ignored`,
		session.ID,
		session.Title,
		session.HarnessKind.ID(),
		session.LastProfile,
		session.TargetTemplateID,
		session.State.AsStr(),
		session.NativeSessionID,
		session.ACPSessionTitle,
		session.SessionTitleOverride,
		session.UpdatedAt,
		session.ViewedThroughEventOrdinal,
		session.LastError,
		resourceAllocation,
		session.LastCheckpointError,
		optionalPathBlob(session.ProjectDirectory),
		managedWorktree,
		session.Archived,
		session.DraftInput,
		session.CreateManagedWorktree,
		subagents,
		session.ContainerWorkspace,
		buildCache,
		session.LaunchBase,
		targetRuntime,
		session.LaunchBranch,
		publication,
		checkout,
		project,
		review,
		session.NoProjectMemory,
		session.NoGithubEvents,
		session.Oracle,
		oracleChoice,
		session.NoGithubCredentials,
		session.NotificationsIgnored,
	); err != nil {
		return err
	}
	if _, err := tx.Exec("UPDATE session_contexts SET profile_id = ?2 WHERE session_id = ?1", session.ID, session.LastProfile); err != nil {
		return err
	}
	if _, err := tx.Exec(`INSERT INTO materialized_sessions(session_id) VALUES (?1)
         ON CONFLICT(session_id) DO NOTHING`, session.ID); err != nil {
		return err
	}
	if err := replaceTargetsWithParent(tx, session, parent); err != nil {
		return err
	}
	if err := replaceMounts(tx, session.ID, session.AdditionalMounts); err != nil {
		return err
	}
	if err := replaceCheckpoint(tx, session); err != nil {
		return err
	}
	if err := recordSessionFaultTransition(tx, session, previousError); err != nil {
		return err
	}
	return settleCreateIntent(tx, session)
}

// updateLifecycleFields updates the columns a lifecycle transition owns, plus
// the target locator that provisioning and teardown maintain with them. The
// row must exist: a transition never resurrects a session another writer
// deleted.
//
// Rust destructures SessionRecord exhaustively here so a new field cannot be
// added without deciding whether a lifecycle transition owns it. Go has no
// such check; whoever adds a SessionRecord field decides it here. Fields this
// statement leaves alone have explicit writers: container settings, display
// titles, drafts, archive visibility, the checkout and launch choices, the
// review and GitHub choices (chosen once, at creation), the target
// (replaceTargets below) and the checkpoint (replaceCheckpoint).
func updateLifecycleFields(tx *sql.Tx, session *core.SessionRecord) error {
	previousError, err := previousSessionError(tx, session.ID)
	if err != nil {
		return err
	}
	managedWorktree, err := optionalJSONColumn(session.ManagedWorktree)
	if err != nil {
		return err
	}
	// Resolved while a session is provisioned and assigned to the record
	// right before this write, so the lifecycle path owns it.
	buildCache, err := optionalJSONColumn(session.BuildCache)
	if err != nil {
		return err
	}
	targetRuntime, err := optionalJSONColumn(session.TargetRuntime)
	if err != nil {
		return err
	}
	// A Move can change it along with the profile the session runs on.
	subagents, err := optionalJSONColumn(session.Subagents)
	if err != nil {
		return err
	}
	project, err := optionalJSONColumn(session.Project)
	if err != nil {
		return err
	}
	oracleChoice, err := optionalJSONColumn(session.OracleChoice)
	if err != nil {
		return err
	}
	// The detach ordinal only ever moves forward, so a transition that
	// started before a detach receipt cannot rewind it.
	result, err := tx.Exec(`UPDATE sessions
         SET title = ?2,
             harness_kind = ?3,
             last_profile = ?4,
             target_template_id = ?5,
             state = ?6,
             updated_at = ?7,
             viewed_through_event_ordinal = max(viewed_through_event_ordinal, ?8),
             last_error = ?9,
             last_checkpoint_error = ?10,
             project_directory = ?11,
             managed_worktree = ?12,
             build_cache_json = ?13,
             target_runtime_json = ?14,
             subagents = ?15,
             project_json = coalesce(?16,project_json),
             oracle = ?17,
             oracle_choice = ?18,
             closing_consent = CASE WHEN ?6 = 'closing' THEN closing_consent ELSE NULL END
         WHERE session_id = ?1`,
		session.ID,
		session.Title,
		session.HarnessKind.ID(),
		session.LastProfile,
		session.TargetTemplateID,
		session.State.AsStr(),
		session.UpdatedAt,
		session.ViewedThroughEventOrdinal,
		session.LastError,
		session.LastCheckpointError,
		optionalPathBlob(session.ProjectDirectory),
		managedWorktree,
		buildCache,
		targetRuntime,
		subagents,
		project,
		session.Oracle,
		oracleChoice,
	)
	if err != nil {
		return err
	}
	if changed, err := result.RowsAffected(); err != nil {
		return err
	} else if changed != 1 {
		return fmt.Errorf("unknown session %s", session.ID)
	}
	if _, err := tx.Exec("UPDATE session_contexts SET profile_id = ?2 WHERE session_id = ?1", session.ID, session.LastProfile); err != nil {
		return err
	}
	if err := recordSessionFaultTransition(tx, session, previousError); err != nil {
		return err
	}
	if err := settleCreateIntent(tx, session); err != nil {
		return err
	}
	return replaceTargets(tx, session)
}

func replaceTargets(tx *sql.Tx, session *core.SessionRecord) error {
	return replaceTargetsWithParent(tx, session, nil)
}

func replaceTargetsWithParent(tx *sql.Tx, session *core.SessionRecord, parent *string) error {
	if err := captureSessionEnvironment(tx, session, parent); err != nil {
		return err
	}
	if _, err := tx.Exec("DELETE FROM session_targets WHERE session_id = ?1", session.ID); err != nil {
		return err
	}
	if session.Target != nil {
		return insertTarget(tx, session.ID, session.Target)
	}
	return nil
}

func replaceCheckpoint(tx *sql.Tx, session *core.SessionRecord) error {
	if _, err := tx.Exec("DELETE FROM session_checkpoints WHERE session_id = ?1", session.ID); err != nil {
		return err
	}
	if checkpoint := session.Checkpoint; checkpoint != nil {
		if _, err := tx.Exec(`INSERT INTO session_checkpoints(session_id, archive_path, sha256, created_at, event_frontier)
             VALUES (?1, ?2, ?3, ?4, ?5)`,
			session.ID, pathToBlob(checkpoint.ArchivePath), checkpoint.Sha256, checkpoint.CreatedAt, checkpoint.EventFrontier); err != nil {
			return err
		}
	}
	return nil
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

// podmanStorageJSON serializes a Podman workspace locator; an unset one is
// the Rust default, the container layer.
func podmanStorageJSON(storage core.PodmanWorkspaceLocator) (*string, error) {
	if storage == nil {
		storage = core.PodmanWorkspaceLocatorContainerLayer{}
	}
	encoded, err := databaseMarshalJSON(storage)
	if err != nil {
		return nil, err
	}
	text := string(encoded)
	return &text, nil
}

func storedTargetValues(target core.TargetLocator) (storedTargetColumns, error) {
	switch target := target.(type) {
	case core.TargetLocatorLocalBare:
		return storedTargetColumns{Kind: "local-bare", Workspace: pathToBlob(target.WorkerRoot)}, nil
	case core.TargetLocatorLocalPodman:
		storage, err := podmanStorageJSON(target.WorkspaceStorage)
		if err != nil {
			return storedTargetColumns{}, err
		}
		containerID := target.ContainerID
		return storedTargetColumns{Kind: "local-podman", Resource: &containerID, WorkspaceStorage: storage, BorrowedFrom: target.BorrowedFrom}, nil
	case core.TargetLocatorLocalDocker:
		containerID := target.ContainerID
		return storedTargetColumns{Kind: "local-docker", Resource: &containerID, BorrowedFrom: target.BorrowedFrom}, nil
	case core.TargetLocatorSshAppleContainer:
		host, containerID := target.Host, target.ContainerID
		return storedTargetColumns{Kind: "ssh-apple-container", Host: &host, Resource: &containerID, BorrowedFrom: target.BorrowedFrom}, nil
	case core.TargetLocatorSshNitro:
		host, containerID := target.Host, target.ContainerID
		return storedTargetColumns{Kind: "ssh-nitro", Host: &host, Resource: &containerID, BorrowedFrom: target.BorrowedFrom}, nil
	case core.TargetLocatorSshDocker:
		host, containerID := target.Host, target.ContainerID
		return storedTargetColumns{Kind: "ssh-docker", Host: &host, Resource: &containerID, BorrowedFrom: target.BorrowedFrom}, nil
	case core.TargetLocatorAppleContainer:
		containerID := target.ContainerID
		return storedTargetColumns{Kind: "apple-container", Resource: &containerID, BorrowedFrom: target.BorrowedFrom}, nil
	case core.TargetLocatorAwsEc2:
		instanceID := target.InstanceID
		return storedTargetColumns{Kind: "aws-ec2", Resource: &instanceID, Address: target.Address}, nil
	case core.TargetLocatorSshBare:
		host := target.Host
		return storedTargetColumns{Kind: "ssh-bare", Host: &host, Workspace: pathToBlob(target.Workspace), WorkerID: target.WorkerID}, nil
	case core.TargetLocatorSshPodman:
		storage, err := podmanStorageJSON(target.WorkspaceStorage)
		if err != nil {
			return storedTargetColumns{}, err
		}
		host, containerID := target.Host, target.ContainerID
		return storedTargetColumns{Kind: "ssh-podman", Host: &host, Resource: &containerID, WorkspaceStorage: storage, BorrowedFrom: target.BorrowedFrom}, nil
	}
	return storedTargetColumns{}, fmt.Errorf("unknown target locator %T", target)
}

func insertTarget(tx *sql.Tx, sessionID string, target core.TargetLocator) error {
	values, err := storedTargetValues(target)
	if err != nil {
		return err
	}
	var workspace any
	if values.Workspace != nil {
		workspace = values.Workspace
	}
	_, err = tx.Exec(`INSERT INTO session_targets(session_id, kind, host, resource_id, address, workspace, worker_id, workspace_storage, borrowed_from)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)`,
		sessionID, values.Kind, values.Host, values.Resource, values.Address, workspace, values.WorkerID,
		values.WorkspaceStorage, values.BorrowedFrom)
	return err
}

func loadTargets(connection Queryer, state *core.StateState) error {
	return loadTargetsSelected(connection, state, nil)
}

func loadTargetsSelected(connection Queryer, state *core.StateState, id *string) error {
	targets, err := readTargets(connection, id)
	if err != nil {
		return err
	}
	updated := map[string]core.SessionRecord{}
	for sessionID, target := range targets {
		session, ok := state.Sessions.Get(sessionID)
		if !ok {
			continue
		}
		session.Target = target
		updated[sessionID] = session
	}
	if len(updated) != 0 {
		state.Sessions.Extend(maps.All(updated))
	}
	return nil
}

func readTargets(connection Queryer, id *string) (map[string]core.TargetLocator, error) {
	predicate := ""
	var arguments []any
	if id != nil {
		predicate = " WHERE session_id=?1"
		arguments = append(arguments, *id)
	}
	rows, err := connection.Query(`SELECT session_id, kind, host, resource_id, address, workspace, worker_id, workspace_storage, borrowed_from
         FROM session_targets`+predicate, arguments...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	targets := map[string]core.TargetLocator{}
	for rows.Next() {
		row, err := scanSQLRow(rows, 9)
		if err != nil {
			return nil, err
		}
		sessionID, err := sqlRowText(row, 0)
		if err != nil {
			return nil, err
		}
		target, err := decodeTarget(row, 1)
		if err != nil {
			return nil, err
		}
		targets[sessionID] = target
	}
	return targets, rows.Err()
}

// sortedTargetIDs is readTargets' keys in Rust's BTreeMap order.
func sortedTargetIDs(targets map[string]core.TargetLocator) []string {
	return slices.Sorted(maps.Keys(targets))
}

// decodeTarget decodes the eight target columns (kind, host, resource_id,
// address, workspace, worker_id, workspace_storage, borrowed_from) that start
// at column start of a scanned row. row holds the row's column values as
// scanned (rusqlite's Row); a NULL column is nil.
func decodeTarget(row []any, start int) (core.TargetLocator, error) {
	kind, err := sqlRowText(row, start)
	if err != nil {
		return nil, err
	}
	host, err := sqlRowOptText(row, start+1)
	if err != nil {
		return nil, err
	}
	resource, err := sqlRowOptText(row, start+2)
	if err != nil {
		return nil, err
	}
	address, err := sqlRowOptText(row, start+3)
	if err != nil {
		return nil, err
	}
	workspaceBlob, present, err := blobOrNull(row[start+4])
	if err != nil {
		return nil, err
	}
	var workspace *string
	if present {
		path := blobToPath(workspaceBlob)
		workspace = &path
	}
	workerID, err := sqlRowOptText(row, start+5)
	if err != nil {
		return nil, err
	}
	serializedStorage, err := sqlRowOptText(row, start+6)
	if err != nil {
		return nil, err
	}
	var workspaceStorage core.PodmanWorkspaceLocator = core.PodmanWorkspaceLocatorContainerLayer{}
	if serializedStorage != nil {
		workspaceStorage, err = core.UnmarshalPodmanWorkspaceLocatorJSON([]byte(*serializedStorage))
		if err != nil {
			return nil, sqlConversionFailure(start+6, err)
		}
	}
	borrowedFrom, err := sqlRowOptText(row, start+7)
	if err != nil {
		return nil, err
	}
	// The schema constrains which columns each kind fills; Rust unwraps them.
	required := func(value *string, column string) string {
		if value == nil {
			panic(fmt.Sprintf("session target of kind %s has no %s", kind, column))
		}
		return *value
	}
	switch kind {
	case "local-bare":
		return core.TargetLocatorLocalBare{WorkerRoot: required(workspace, "workspace")}, nil
	case "local-podman":
		return core.TargetLocatorLocalPodman{BorrowedFrom: borrowedFrom, ContainerID: required(resource, "resource_id"), WorkspaceStorage: workspaceStorage}, nil
	case "local-docker":
		return core.TargetLocatorLocalDocker{BorrowedFrom: borrowedFrom, ContainerID: required(resource, "resource_id")}, nil
	case "apple-container":
		return core.TargetLocatorAppleContainer{BorrowedFrom: borrowedFrom, ContainerID: required(resource, "resource_id")}, nil
	case "aws-ec2":
		return core.TargetLocatorAwsEc2{InstanceID: required(resource, "resource_id"), Address: address}, nil
	case "ssh-bare":
		return core.TargetLocatorSshBare{Host: required(host, "host"), Workspace: required(workspace, "workspace"), WorkerID: workerID}, nil
	case "ssh-docker":
		return core.TargetLocatorSshDocker{BorrowedFrom: borrowedFrom, Host: required(host, "host"), ContainerID: required(resource, "resource_id")}, nil
	case "ssh-apple-container":
		return core.TargetLocatorSshAppleContainer{BorrowedFrom: borrowedFrom, Host: required(host, "host"), ContainerID: required(resource, "resource_id")}, nil
	case "ssh-nitro":
		return core.TargetLocatorSshNitro{BorrowedFrom: borrowedFrom, Host: required(host, "host"), ContainerID: required(resource, "resource_id")}, nil
	case "ssh-podman":
		return core.TargetLocatorSshPodman{BorrowedFrom: borrowedFrom, Host: required(host, "host"), ContainerID: required(resource, "resource_id"), WorkspaceStorage: workspaceStorage}, nil
	}
	panic("target kind constrained by schema")
}

// replaceMounts rewrites a session's attached directories.
//
// session_mounts.read_only keeps the meaning older builds understand, so
// read-write mounts are stored there as not read-only and recorded again in
// session_mount_access. Older builds rewrite session_mounts without touching
// that table, which is what keeps the read-write choice.
func replaceMounts(tx *sql.Tx, sessionID string, mounts []core.AdditionalMount) error {
	if _, err := tx.Exec("DELETE FROM session_mounts WHERE session_id = ?1", sessionID); err != nil {
		return err
	}
	if _, err := tx.Exec("DELETE FROM session_mount_access WHERE session_id = ?1", sessionID); err != nil {
		return err
	}
	for ordinal, mount := range mounts {
		if _, err := tx.Exec(`INSERT INTO session_mounts(session_id, ordinal, source, destination, read_only)
             VALUES (?1, ?2, ?3, ?4, ?5)`,
			sessionID, int64(ordinal), pathToBlob(mount.Source), pathToBlob(mount.Destination), mount.Access == core.MountAccessRo); err != nil {
			return err
		}
		if mount.Access == core.MountAccessRw {
			if _, err := tx.Exec(`INSERT INTO session_mount_access(session_id, source, destination, access)
                 VALUES (?1, ?2, ?3, 'rw')`,
				sessionID, pathToBlob(mount.Source), pathToBlob(mount.Destination)); err != nil {
				return err
			}
		}
	}
	return nil
}

func loadMounts(connection Queryer, state *core.StateState) error {
	return loadMountsSelected(connection, state, nil)
}

func loadMountsSelected(connection Queryer, state *core.StateState, id *string) error {
	predicate := ""
	var arguments []any
	if id != nil {
		predicate = " WHERE m.session_id=?1"
		arguments = append(arguments, *id)
	}
	rows, err := connection.Query(`SELECT m.session_id, m.source, m.destination, m.read_only, a.access IS NOT NULL
         FROM session_mounts m
         LEFT JOIN session_mount_access a
             ON a.session_id = m.session_id
             AND a.source = m.source
             AND a.destination = m.destination
         `+predicate+` ORDER BY m.session_id, m.ordinal`, arguments...)
	if err != nil {
		return err
	}
	defer rows.Close()
	updated := map[string]*core.SessionRecord{}
	for rows.Next() {
		row, err := scanSQLRow(rows, 5)
		if err != nil {
			return err
		}
		readOnly, err := sqlRowBool(row, 3)
		if err != nil {
			return err
		}
		readWrite, err := sqlRowBool(row, 4)
		if err != nil {
			return err
		}
		// An older build that made the mount read-only left the access row
		// behind; its later choice wins.
		access := core.MountAccessCow
		switch {
		case readOnly:
			access = core.MountAccessRo
		case readWrite:
			access = core.MountAccessRw
		}
		sessionID, err := sqlRowText(row, 0)
		if err != nil {
			return err
		}
		source, err := sqlRowBlob(row, 1)
		if err != nil {
			return err
		}
		destination, err := sqlRowBlob(row, 2)
		if err != nil {
			return err
		}
		session, ok := updated[sessionID]
		if !ok {
			record, found := state.Sessions.Get(sessionID)
			if !found {
				continue
			}
			session = &record
			updated[sessionID] = session
		}
		session.AdditionalMounts = append(slices.Clip(session.AdditionalMounts), core.AdditionalMount{
			Source:      blobToPath(source),
			Destination: blobToPath(destination),
			Access:      access,
		})
	}
	if err := rows.Err(); err != nil {
		return err
	}
	if len(updated) != 0 {
		state.Sessions.Extend(func(yield func(string, core.SessionRecord) bool) {
			for sessionID, session := range updated {
				if !yield(sessionID, *session) {
					return
				}
			}
		})
	}
	return nil
}

func loadCheckpoints(connection Queryer, state *core.StateState) error {
	return loadCheckpointsSelected(connection, state, nil)
}

func loadCheckpointsSelected(connection Queryer, state *core.StateState, id *string) error {
	predicate := ""
	var arguments []any
	if id != nil {
		predicate = " WHERE session_id=?1"
		arguments = append(arguments, *id)
	}
	rows, err := connection.Query("SELECT session_id, archive_path, sha256, created_at, event_frontier FROM session_checkpoints"+predicate, arguments...)
	if err != nil {
		return err
	}
	defer rows.Close()
	updated := map[string]core.SessionRecord{}
	for rows.Next() {
		row, err := scanSQLRow(rows, 5)
		if err != nil {
			return err
		}
		sessionID, err := sqlRowText(row, 0)
		if err != nil {
			return err
		}
		archivePath, err := sqlRowBlob(row, 1)
		if err != nil {
			return err
		}
		var checkpoint core.CheckpointMetadata
		checkpoint.ArchivePath = blobToPath(archivePath)
		if checkpoint.Sha256, err = sqlRowText(row, 2); err != nil {
			return err
		}
		if checkpoint.CreatedAt, err = sqlRowText(row, 3); err != nil {
			return err
		}
		if checkpoint.EventFrontier, err = sqlRowUint64(row, 4); err != nil {
			return err
		}
		session, ok := updated[sessionID]
		if !ok {
			session, ok = state.Sessions.Get(sessionID)
			if !ok {
				continue
			}
		}
		session.Checkpoint = &checkpoint
		updated[sessionID] = session
	}
	if err := rows.Err(); err != nil {
		return err
	}
	if len(updated) != 0 {
		state.Sessions.Extend(maps.All(updated))
	}
	return nil
}

// BackfillTargetRuntime fills a missing snapshot without overwriting a
// concurrent writer. A deleted or retargeted session returns nil; its stale
// controller copy can be dropped by reload instead of preventing every
// subsequent reload.
func BackfillTargetRuntime(sessionID string, targetTemplateID string, runtime *core.TargetRuntimeSettings) (*core.TargetRuntimeSettings, error) {
	encoded, err := databaseMarshalJSON(runtime)
	if err != nil {
		return nil, err
	}
	return submitDatabaseWrite("backfill_target_runtime", func(*Connection) (*core.TargetRuntimeSettings, error) {
		connection, err := open(DatabasePath())
		if err != nil {
			return nil, err
		}
		defer connection.Close()
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return nil, err
		}
		defer tx.Rollback()
		if _, err := tx.Exec("UPDATE sessions SET target_runtime_json = ?2 WHERE session_id = ?1 AND target_template_id = ?3 AND target_runtime_json IS NULL",
			sessionID, string(encoded), targetTemplateID); err != nil {
			return nil, err
		}
		// Concurrent backfills must all use the winning durable value.
		var stored *string
		err = tx.QueryRow("SELECT target_runtime_json FROM sessions WHERE session_id = ?1 AND target_template_id = ?2",
			sessionID, targetTemplateID).Scan(&stored)
		if err != nil && !errors.Is(err, sql.ErrNoRows) {
			return nil, err
		}
		var winning *core.TargetRuntimeSettings
		if stored != nil {
			winning = new(core.TargetRuntimeSettings)
			if err := json.Unmarshal([]byte(*stored), winning); err != nil {
				return nil, err
			}
		}
		if err := tx.Commit(); err != nil {
			return nil, err
		}
		return winning, nil
	})
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

// sessionQueryColumns is the number of columns sessionQuery selects.
const sessionQueryColumns = 40

// decodeOptionalColumnJSON decodes an optional JSON column into a new T, or
// nil for NULL; a malformed value is rusqlite's conversion failure.
func decodeOptionalColumnJSON[T any](row []any, column int) (*T, error) {
	text, err := sqlRowOptText(row, column)
	if err != nil || text == nil {
		return nil, err
	}
	value := new(T)
	if err := json.Unmarshal([]byte(*text), value); err != nil {
		return nil, sqlConversionFailure(column, err)
	}
	return value, nil
}

// decodeSession decodes one sessionQuery row, or nil for a session whose
// harness this build no longer supports. row holds the row's column values
// as scanned (rusqlite's Row); a NULL column is nil.
func decodeSession(row []any, interns *sessionInterns) (*core.SessionRecord, error) {
	// A harness Mjolnir no longer supports can still own rows an earlier
	// release wrote. Skip such a session with a warning rather than failing
	// the whole listing and hiding every other session with it.
	harnessText, err := sqlRowText(row, 2)
	if err != nil {
		return nil, err
	}
	harnessKind, err := core.ParseHarnessKind(harnessText)
	if err != nil {
		sessionID, err := sqlRowText(row, 0)
		if err != nil {
			return nil, err
		}
		slog.Warn("session harness is no longer supported; the session is not listed",
			"session_id", sessionID, "harness", harnessText)
		return nil, nil
	}
	var record core.SessionRecord
	record.HarnessKind = harnessKind
	if record.OracleChoice, err = decodeOptionalColumnJSON[core.SubagentChoice](row, 35); err != nil {
		return nil, err
	}
	if record.Oracle, err = sqlRowBool(row, 34); err != nil {
		return nil, err
	}
	if record.Project, err = decodeShared(row, 30, &interns.projects); err != nil {
		return nil, err
	}
	if record.TargetRuntime, err = decodeShared(row, 26, &interns.runtimes); err != nil {
		return nil, err
	}
	if record.CreateManagedWorktree, err = sqlRowOptBool(row, 21); err != nil {
		return nil, err
	}
	if record.LaunchBase, err = sqlRowOptText(row, 25); err != nil {
		return nil, err
	}
	if record.LaunchBranch, err = sqlRowOptText(row, 27); err != nil {
		return nil, err
	}
	if record.Checkout, err = decodeOptionalColumnJSON[core.ExactCheckout](row, 29); err != nil {
		return nil, err
	}
	if record.Publication, err = decodeOptionalColumnJSON[core.PublicationAssessment](row, 28); err != nil {
		return nil, err
	}
	subagents, err := sqlRowOptText(row, 22)
	if err != nil {
		return nil, err
	}
	if subagents != nil {
		policy, err := core.ParseStoredPolicy(*subagents)
		if err != nil {
			return nil, sqlConversionFailure(22, err)
		}
		record.Subagents = &policy
	}
	if record.ContainerWorkspace, err = sqlRowOptText(row, 23); err != nil {
		return nil, err
	}
	buildCache, err := sqlRowOptText(row, 24)
	if err != nil {
		return nil, err
	}
	if buildCache != nil {
		decoded := new(core.SessionBuildCache)
		if err := json.Unmarshal([]byte(*buildCache), decoded); err != nil {
			slog.Warn("session build cache record is unreadable", "error", err.Error())
		} else {
			record.BuildCache = decoded
		}
	}
	if record.WorkspaceID, err = sqlRowText(row, 20); err != nil {
		return nil, err
	}
	if record.Archived, err = sqlRowBool(row, 19); err != nil {
		return nil, err
	}
	if record.ID, err = sqlRowText(row, 0); err != nil {
		return nil, err
	}
	if record.Title, err = sqlRowText(row, 1); err != nil {
		return nil, err
	}
	if record.LastProfile, err = sqlRowText(row, 3); err != nil {
		return nil, err
	}
	if record.BundleID, err = sqlRowText(row, 4); err != nil {
		return nil, err
	}
	projectDirectory, present, err := blobOrNull(row[16])
	if err != nil {
		return nil, err
	}
	if present {
		path := blobToPath(projectDirectory)
		record.ProjectDirectory = &path
	}
	if record.ManagedWorktree, err = decodeOptionalColumnJSON[core.ManagedWorktree](row, 17); err != nil {
		return nil, err
	}
	// An unreadable choice, such as a mode a newer release wrote, falls back
	// to `[review]` rather than hiding the session.
	review, err := sqlRowOptText(row, 31)
	if err != nil {
		return nil, err
	}
	if review != nil {
		decoded := new(core.SessionReview)
		if err := json.Unmarshal([]byte(*review), decoded); err != nil {
			slog.Warn("session review choice is unreadable", "error", err.Error())
		} else {
			record.Review = decoded
		}
	}
	if record.NoProjectMemory, err = sqlRowBool(row, 32); err != nil {
		return nil, err
	}
	// Keep the shipped no_mailbox column for the GitHub target opt-out.
	if record.NoGithubEvents, err = sqlRowBool(row, 33); err != nil {
		return nil, err
	}
	if record.NoGithubCredentials, err = sqlRowBool(row, 36); err != nil {
		return nil, err
	}
	if record.NotificationsIgnored, err = sqlRowBool(row, 37); err != nil {
		return nil, err
	}
	if record.TargetTemplateID, err = sqlRowText(row, 5); err != nil {
		return nil, err
	}
	allocation, err := sqlRowOptText(row, 14)
	if err != nil {
		return nil, err
	}
	if allocation != nil {
		if record.ResourceAllocation, err = core.UnmarshalSessionResourceAllocationJSON([]byte(*allocation)); err != nil {
			return nil, sqlConversionFailure(14, err)
		}
	}
	state, err := sqlRowText(row, 6)
	if err != nil {
		return nil, err
	}
	record.State = storedSessionState(state)
	if record.NativeSessionID, err = sqlRowOptText(row, 7); err != nil {
		return nil, err
	}
	acpTitle, err := sqlRowOptText(row, 8)
	if err != nil {
		return nil, err
	}
	if acpTitle != nil {
		if normalized, ok := core.NormalizeSessionTitle(*acpTitle); ok {
			record.ACPSessionTitle = &normalized
		}
	}
	if record.SessionTitleOverride, err = sqlRowOptText(row, 9); err != nil {
		return nil, err
	}
	if record.CreatedAt, err = sqlRowText(row, 10); err != nil {
		return nil, err
	}
	if record.UpdatedAt, err = sqlRowText(row, 11); err != nil {
		return nil, err
	}
	if record.ViewedThroughEventOrdinal, err = sqlRowUint64(row, 12); err != nil {
		return nil, err
	}
	if record.DraftInput, err = sqlRowText(row, 18); err != nil {
		return nil, err
	}
	if record.LastError, err = sqlRowOptText(row, 13); err != nil {
		return nil, err
	}
	if record.FailureGeneration, err = sqlRowUint64(row, 39); err != nil {
		return nil, err
	}
	if record.DismissedFailure, err = decodeOptionalColumnJSON[core.FailureIdentity](row, 38); err != nil {
		return nil, err
	}
	if record.LastCheckpointError, err = sqlRowOptText(row, 15); err != nil {
		return nil, err
	}
	return &record, nil
}

// LoadSessionRecord reads one complete record with indexed queries from the
// caller's WAL snapshot.
func LoadSessionRecord(id string) (*core.SessionRecord, error) {
	committed, err := WriterCommittedState()
	if err != nil {
		return nil, err
	}
	if committed != nil {
		record, ok := committed.State.Sessions.Get(id)
		if !ok {
			return nil, nil
		}
		return &record, nil
	}
	return ReadDurableSessionRecord(id)
}

// ReadDurableSessionRecord: recovery checks storage under target ownership
// before touching a process. This bounded read also detects a broken store
// even without a pending write.
func ReadDurableSessionRecord(id string) (*core.SessionRecord, error) {
	reader, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer reader.Close()
	snapshot, err := reader.Begin()
	if err != nil {
		return nil, err
	}
	defer snapshot.Rollback()
	return loadSessionWith(snapshot, id)
}

func loadSessionWith(connection Queryer, id string) (*core.SessionRecord, error) {
	return loadSessionInternedWith(connection, id, newSessionInterns())
}

func loadSessionInternedWith(connection Queryer, id string, interns *sessionInterns) (*core.SessionRecord, error) {
	record, err := func() (*core.SessionRecord, error) {
		rows, err := connection.Query(sessionQuery+" WHERE s.session_id=?1", id)
		if err != nil {
			return nil, err
		}
		defer rows.Close()
		if !rows.Next() {
			return nil, rows.Err()
		}
		row, err := scanSQLRow(rows, sessionQueryColumns)
		if err != nil {
			return nil, err
		}
		return decodeSession(row, interns)
	}()
	if err != nil || record == nil {
		return nil, err
	}
	state := core.NewStateState()
	state.Sessions.Insert(id, *record)
	if err := loadTargetsSelected(connection, &state, &id); err != nil {
		return nil, err
	}
	if err := loadMountsSelected(connection, &state, &id); err != nil {
		return nil, err
	}
	if err := loadCheckpointsSelected(connection, &state, &id); err != nil {
		return nil, err
	}
	if err := state.Validate(); err != nil {
		return nil, err
	}
	loaded, ok := state.Sessions.Remove(id)
	if !ok {
		return nil, nil
	}
	return &loaded, nil
}

// LoadOperatorPresence reads the daemon-owned setting without loading all
// session records.
func LoadOperatorPresence() (core.OperatorPresence, error) {
	committed, err := WriterCommittedState()
	if err != nil {
		return core.OperatorPresence{}, err
	}
	if committed != nil {
		return committed.State.OperatorPresence, nil
	}
	connection, err := openReader(DatabasePath())
	if err != nil {
		return core.OperatorPresence{}, err
	}
	defer connection.Close()
	var presence core.OperatorPresence
	err = connection.QueryRow("SELECT away,revision FROM operator_away WHERE singleton=1").Scan(&presence.Away, &presence.Revision)
	return presence, err
}

// SetOperatorAway: the shared database writer owns transition and no-op
// decisions.
func SetOperatorAway(away bool, autoAfkWindow time.Duration) (bool, error) {
	changed, err := submitDatabaseWrite("operator away setting", func(connection *Connection) (bool, error) {
		return setWith(connection, away, autoAfkWindow)
	})
	if err != nil {
		return false, err
	}
	if changed {
		NotifyMailboxOutboxChanged()
	}
	return changed, nil
}

// RecordOperatorAwayWorkerProtocol persists the last negotiated worker
// protocol so AFK-off remains correct before the daemon has reattached every
// worker after a restart.
func RecordOperatorAwayWorkerProtocol(session string, protocol uint32) error {
	_, err := submitDatabaseWrite("operator away worker protocol", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("INSERT INTO operator_away_workers(session_id,protocol) VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET protocol=excluded.protocol WHERE protocol != excluded.protocol",
			session, protocol)
		return struct{}{}, err
	})
	return err
}

// updateContainerRequests: only explicit dialog choices change next-creation
// requests. A nil allocation leaves it as it is; a pointer to a nil
// allocation clears it. A nil mounts leaves the mounts as they are.
func updateContainerRequests(tx *sql.Tx, sessionID string, allocation *core.SessionResourceAllocation, mounts *[]core.AdditionalMount) error {
	if allocation != nil {
		var encoded any
		if *allocation != nil {
			value, err := databaseMarshalJSON(*allocation)
			if err != nil {
				return err
			}
			encoded = string(value)
		}
		if _, err := tx.Exec("UPDATE sessions SET resource_allocation=?2 WHERE session_id=?1", sessionID, encoded); err != nil {
			return err
		}
	}
	if mounts != nil {
		return replaceMounts(tx, sessionID, *mounts)
	}
	return nil
}
