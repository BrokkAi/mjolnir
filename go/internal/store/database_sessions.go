// Port of mj-store/src/database/sessions.rs.

package store

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"math"
	"reflect"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// unitResult is the result of a write that returns nothing (Rust's `()`).
type unitResult = struct{}

// targetLocatorsEqual is Option<Box<TargetLocator>> equality.
func targetLocatorsEqual(a, b core.TargetLocator) bool {
	return reflect.DeepEqual(a, b)
}

// changedRows is rusqlite's execute result: the number of changed rows.
func changedRows(result sql.Result, err error) (int64, error) {
	if err != nil {
		return 0, err
	}
	return result.RowsAffected()
}

// SaveImportedSession persists one operational session without rewriting
// unrelated controller state. Dashboard lifecycle jobs use this path so
// independent jobs can commit concurrently without restoring stale copies of
// other sessions.
func SaveImportedSession(session *core.SessionRecord, materialized *core.MaterializedSession) error {
	sessionCopy := *session
	materializedCopy := *materialized
	_, err := submitDatabaseWrite("save_imported_session", func(connection *Connection) (unitResult, error) {
		if err := validateSessionRecord(&sessionCopy); err != nil {
			return unitResult{}, err
		}
		if err := materializedCopy.Validate(); err != nil {
			return unitResult{}, err
		}
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return unitResult{}, err
		}
		defer tx.Rollback()
		exists, err := sessionExists(tx, sessionCopy.ID)
		if err != nil {
			return unitResult{}, err
		}
		if exists {
			return unitResult{}, errors.New("imported session already exists")
		}
		if err := insertSession(tx, &sessionCopy); err != nil {
			return unitResult{}, err
		}
		if err := writeMaterializedSession(tx, &materializedCopy); err != nil {
			return unitResult{}, err
		}
		return unitResult{}, tx.Commit()
	})
	return err
}

func SaveSession(session *core.SessionRecord) error {
	sessionCopy := *session
	_, err := submitDatabaseWrite("save_session", func(*Connection) (unitResult, error) {
		return unitResult{}, SaveSessionTo(DatabasePath(), &sessionCopy)
	})
	return err
}

// HostContainerSizeChoice is a host and the container size most recently
// launched on it: Rust's `(String, HostContainerSize)` in the optional
// container size a session save remembers.
type HostContainerSizeChoice struct {
	Host string
	Size core.HostContainerSize
}

func SaveNewSession(session *core.SessionRecord, containerSize *HostContainerSizeChoice, rememberLaunchChoices bool, rememberContainerSize bool) error {
	return SaveNewSessionWithPreferences(session, containerSize, NewSessionPreferences{
		RememberLaunchChoices:  rememberLaunchChoices,
		RememberContainerSize:  rememberContainerSize,
		RememberSubagentPolicy: rememberLaunchChoices,
	})
}

type NewSessionPreferences struct {
	RememberLaunchChoices  bool
	RememberContainerSize  bool
	RememberSubagentPolicy bool
}

func SaveNewSessionWithPreferences(session *core.SessionRecord, containerSize *HostContainerSizeChoice, preferences NewSessionPreferences) error {
	sessionCopy := *session
	_, err := submitDatabaseWrite("save_new_session", func(*Connection) (unitResult, error) {
		return unitResult{}, saveNewSessionWithPreferencesTo(DatabasePath(), &sessionCopy, containerSize, preferences)
	})
	return err
}

// saveNewSessionTo is test-only in Rust.
func saveNewSessionTo(path string, session *core.SessionRecord, containerSize *HostContainerSizeChoice, rememberLaunchChoices bool, rememberContainerSize bool) error {
	return saveNewSessionWithPreferencesTo(path, session, containerSize, NewSessionPreferences{
		RememberLaunchChoices:  rememberLaunchChoices,
		RememberContainerSize:  rememberContainerSize,
		RememberSubagentPolicy: rememberLaunchChoices,
	})
}

func saveNewSessionWithPreferencesTo(path string, session *core.SessionRecord, containerSize *HostContainerSizeChoice, preferences NewSessionPreferences) error {
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
	if err := validateSessionRecord(session); err != nil {
		return err
	}
	if session.LastProfile == "" {
		return errors.New("a session profile id cannot use the reserved legacy preference key")
	}
	if err := insertSession(tx, session); err != nil {
		return err
	}
	if _, err := insertCreateIntent(tx, session, preferences.RememberLaunchChoices); err != nil {
		return err
	}
	if containerSize != nil && preferences.RememberContainerSize {
		if err := writeHostContainerSize(tx, containerSize.Host, containerSize.Size); err != nil {
			return err
		}
	}
	var policy core.SubagentPolicy
	if session.Subagents != nil {
		policy = *session.Subagents
	}
	if preferences.RememberSubagentPolicy && policy.UpdatesRememberedChoice(session.HarnessKind) {
		if err := saveSubagentPreference(tx, session.LastProfile, policy); err != nil {
			return err
		}
	}
	if preferences.RememberLaunchChoices {
		if _, err := tx.Exec(`INSERT INTO oracle_preference(profile_id, enabled) VALUES(?1, ?2)
             ON CONFLICT(profile_id) DO UPDATE SET enabled = excluded.enabled`,
			session.LastProfile, session.Oracle); err != nil {
			return err
		}
	}
	return tx.Commit()
}

func saveSubagentPreference(tx *sql.Tx, profile string, policy core.SubagentPolicy) error {
	encoded, err := databaseMarshalJSON(policy)
	if err != nil {
		return err
	}
	_, err = tx.Exec(`INSERT INTO subagent_preference(profile_id, policy) VALUES(?1, ?2)
         ON CONFLICT(profile_id) DO UPDATE SET policy = excluded.policy`,
		profile, string(encoded))
	return err
}

// SetPublicationAssessmentIfCurrent updates publication evidence only while
// the exact stopped checkpoint is still current. An age scan may race a
// user's Resume or a new checkpoint.
func SetPublicationAssessmentIfCurrent(sessionID string, assessment *core.PublicationAssessment) (bool, error) {
	encoded, err := databaseMarshalJSON(assessment)
	if err != nil {
		return false, err
	}
	checkpointSha256 := assessment.CheckpointSha256
	return submitDatabaseWrite("set_publication_assessment_if_current", func(*Connection) (bool, error) {
		connection, err := open(DatabasePath())
		if err != nil {
			return false, err
		}
		defer connection.Close()
		updated, err := changedRows(connection.Exec(`UPDATE sessions SET publication_json = ?2
             WHERE session_id = ?1 AND state = 'stopped'
               AND EXISTS (SELECT 1 FROM session_checkpoints c
                           WHERE c.session_id = ?1 AND c.sha256 = ?3)`,
			sessionID, string(encoded), checkpointSha256))
		return updated == 1, err
	})
}

// OracleAlreadyLive: a primary may have one live oracle; registration
// refused another.
type OracleAlreadyLive struct{}

func (OracleAlreadyLive) Error() string { return "this session already has a live oracle" }

// SaveSubagentSession persists a borrowed-target child and its parent
// relationship atomically.
func SaveSubagentSession(session *core.SessionRecord, subagent *core.SubagentRecord) error {
	return RegisterSubagentWithStartup(session, subagent, nil, nil, nil, false)
}

// RegisterSubagentWithStartup publishes the child and its complete initial
// input in the same transaction. Recovery must never find a newly registered
// worker without its first brief.
func RegisterSubagentWithStartup(session *core.SessionRecord, subagent *core.SubagentRecord, startup []StartupDelivery, reportDir *string, category *string, ownEnvironment bool) error {
	return RegisterSubagentWithStartupAdmitted(session, subagent, startup, reportDir, category, ownEnvironment,
		func(*core.StateState, *core.SubagentRecord) error { return nil })
}

// RegisterSubagentWithStartupAdmitted runs the controller's admission policy
// against the latest committed state in the same serialized write that
// registers the child.
func RegisterSubagentWithStartupAdmitted(session *core.SessionRecord, subagent *core.SubagentRecord, startup []StartupDelivery, reportDir *string, category *string, ownEnvironment bool, admit func(*core.StateState, *core.SubagentRecord) error) error {
	sessionCopy := *session
	subagentCopy := *subagent
	_, err := submitDatabaseWrite("save_subagent_session", func(*Connection) (unitResult, error) {
		connection, err := open(DatabasePath())
		if err != nil {
			return unitResult{}, err
		}
		defer connection.Close()
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return unitResult{}, err
		}
		defer tx.Rollback()
		committed, err := WriterCommittedState()
		if err != nil {
			return unitResult{}, err
		}
		if committed == nil {
			return unitResult{}, errors.New("sub-agent admission requires the database writer")
		}
		if err := admit(&committed.State, &subagentCopy); err != nil {
			return unitResult{}, err
		}
		var parentState string
		if err := tx.QueryRow("SELECT state FROM sessions WHERE session_id=?1", subagentCopy.ParentSessionID).Scan(&parentState); err != nil {
			return unitResult{}, err
		}
		if !storedSessionState(parentState).CanReceiveChildWork() {
			return unitResult{}, errors.New("parent is no longer accepting child work")
		}
		if !ownEnvironment {
			if err := ensureContainerParentAvailable(tx, subagentCopy.ParentSessionID); err != nil {
				return unitResult{}, err
			}
		}
		if subagentCopy.Kind == core.SubagentKindOracle {
			states, err := queryTextColumn(tx, `SELECT s.state FROM subagent_sessions c
                 JOIN sessions s ON s.session_id = c.child_session_id
                 WHERE c.parent_session_id = ?1 AND c.request_key LIKE 'oracle:%'`, subagentCopy.ParentSessionID)
			if err != nil {
				return unitResult{}, err
			}
			for _, stored := range states {
				if state, ok := core.SessionStateFromStored(stored); ok && state.CanReceiveChildWork() {
					return unitResult{}, OracleAlreadyLive{}
				}
			}
		}
		var parent *string
		if !ownEnvironment {
			parentID := subagentCopy.ParentSessionID
			parent = &parentID
		}
		if err := insertSessionWithParent(tx, &sessionCopy, parent); err != nil {
			return unitResult{}, err
		}
		if _, err := insertCreateIntent(tx, &sessionCopy, false); err != nil {
			return unitResult{}, err
		}
		if err := recordChildAccounting(tx, &subagentCopy, category); err != nil {
			return unitResult{}, err
		}
		record, err := databaseMarshalJSON(&subagentCopy)
		if err != nil {
			return unitResult{}, err
		}
		if _, err := tx.Exec(`INSERT INTO subagent_sessions(
                 child_session_id, parent_session_id, request_key, record_json
             ) VALUES (?1, ?2, ?3, ?4)`,
			subagentCopy.ChildSessionID, subagentCopy.ParentSessionID,
			subagentCopy.Kind.RequestKey(subagentCopy.RequestKey), string(record)); err != nil {
			return unitResult{}, err
		}
		if _, err := enqueueStartupDeliveriesWith(tx, startup); err != nil {
			return unitResult{}, err
		}
		// The report directory commits with the registration, so no later
		// step can leave a registered child without it.
		if reportDir != nil {
			if err := recordSubagentReportDirWith(tx, subagentCopy.ChildSessionID, *reportDir); err != nil {
				return unitResult{}, err
			}
		}
		return unitResult{}, tx.Commit()
	})
	return err
}

// MarkSubagentTurnNoticed records the child turn whose completion notice the
// parent already has.
func MarkSubagentTurnNoticed(childSessionID string, turn uint64) error {
	_, err := submitDatabaseWrite("mark_subagent_turn_noticed", func(*Connection) (unitResult, error) {
		relation, err := LoadSubagent(childSessionID)
		if err != nil {
			return unitResult{}, err
		}
		if relation == nil {
			return unitResult{}, fmt.Errorf("unknown sub-agent session %s", childSessionID)
		}
		relation.NoticedTurn = &turn
		encoded, err := databaseMarshalJSON(relation)
		if err != nil {
			return unitResult{}, err
		}
		connection, err := open(DatabasePath())
		if err != nil {
			return unitResult{}, err
		}
		defer connection.Close()
		_, err = connection.Exec("UPDATE subagent_sessions SET record_json = ?2 WHERE child_session_id = ?1",
			childSessionID, string(encoded))
		return unitResult{}, err
	})
	return err
}

// decodeSubagentRecord decodes a stored sub-agent record.
func decodeSubagentRecord(text string) (core.SubagentRecord, error) {
	var record core.SubagentRecord
	if err := json.Unmarshal([]byte(text), &record); err != nil {
		return core.SubagentRecord{}, fmt.Errorf("decode sub-agent record: %w", err)
	}
	return record, nil
}

func LoadSubagent(childSessionID string) (*core.SubagentRecord, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	var stored string
	err = connection.QueryRow("SELECT record_json FROM subagent_sessions WHERE child_session_id = ?1", childSessionID).Scan(&stored)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	record, err := decodeSubagentRecord(stored)
	if err != nil {
		return nil, err
	}
	return &record, nil
}

func SetSubagentResumeWithParent(childSessionID string, resumeWithParent bool) error {
	_, err := submitDatabaseWrite("set_subagent_resume_with_parent", func(connection *Connection) (unitResult, error) {
		changed, err := changedRows(connection.Exec(`UPDATE subagent_sessions
             SET record_json = CASE WHEN ?2
                 THEN json_set(record_json, '$.resume_with_parent', json('true'))
                 ELSE json_remove(record_json, '$.resume_with_parent')
             END WHERE child_session_id = ?1`,
			childSessionID, resumeWithParent))
		if err != nil {
			return unitResult{}, err
		}
		if changed != 1 {
			return unitResult{}, fmt.Errorf("unknown sub-agent session %s", childSessionID)
		}
		return unitResult{}, nil
	})
	return err
}

// SubagentHasHandedBack is whether a sub-agent child has handed back its
// report for its parent's newest task, from what the store holds; see
// core.HasHandedBack. A session that is not a child, or that has never
// finished a turn, has not.
func SubagentHasHandedBack(childSessionID string, liveness *core.Liveness) (bool, error) {
	relation, err := LoadSubagent(childSessionID)
	if err != nil || relation == nil {
		return false, err
	}
	// A child is parked only once its turn ended and its parent was told, so
	// whatever the parent was going to get from it, it has.
	state, ok, err := LoadSessionState(childSessionID)
	if err != nil {
		return false, err
	}
	if ok && state == core.SessionStateParked {
		return true, nil
	}
	turn, err := LoadMaterializedTurnOutcome(childSessionID)
	if err != nil || turn == nil {
		return false, err
	}
	report, err := LoadSubagentReport(childSessionID)
	if err != nil {
		return false, err
	}
	_, idle := turn.Execution.(core.MaterializedExecutionStateIdle)
	working := turn.ActiveTurn != nil || !idle
	return core.HasHandedBack(relation.HandbackTool, &report, working, turn.LastTurnOutcome, liveness, core.EpochMillis()), nil
}

func ListSubagents(parentSessionID string) ([]core.SubagentRecord, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	stored, err := queryTextColumn(connection, `SELECT record_json FROM subagent_sessions
         WHERE parent_session_id = ?1 ORDER BY rowid`, parentSessionID)
	if err != nil {
		return nil, err
	}
	return decodeSubagentRecords(stored)
}

func decodeSubagentRecords(stored []string) ([]core.SubagentRecord, error) {
	records := make([]core.SubagentRecord, 0, len(stored))
	for _, text := range stored {
		record, err := decodeSubagentRecord(text)
		if err != nil {
			return nil, err
		}
		records = append(records, record)
	}
	return records, nil
}

// SubagentListingDetails is a child's durable role and latest turn selection
// for the parent's listing.
type SubagentListingDetails struct {
	Relation  core.SubagentRecord
	Category  *string
	Selection core.UsageSelection
}

func ListSubagentDetails(parent string) ([]SubagentListingDetails, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	rows, err := connection.Query(`SELECT c.record_json, a.category, t.model, t.effort
         FROM subagent_sessions c
         LEFT JOIN subagent_accounting a USING (child_session_id)
         LEFT JOIN sessions s ON s.session_id = c.child_session_id
         LEFT JOIN session_turn_selections t ON t.rowid = (
             SELECT rowid FROM session_turn_selections
             WHERE session_id = c.child_session_id ORDER BY rowid DESC LIMIT 1
         ) AND (t.profile IS NULL OR t.profile = s.last_profile)
         WHERE c.parent_session_id = ?1 ORDER BY c.rowid`, parent)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	details := []SubagentListingDetails{}
	for rows.Next() {
		var record string
		var detail SubagentListingDetails
		if err := rows.Scan(&record, &detail.Category, &detail.Selection.Model, &detail.Selection.Effort); err != nil {
			return nil, err
		}
		if detail.Relation, err = decodeSubagentRecord(record); err != nil {
			return nil, err
		}
		details = append(details, detail)
	}
	return details, rows.Err()
}

// ListSubagentIDs is a parent's child session ids in registration order,
// read from the index without decoding any record.
func ListSubagentIDs(parentSessionID string) ([]string, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	ids, err := queryTextColumn(connection, `SELECT child_session_id FROM subagent_sessions
         WHERE parent_session_id = ?1 ORDER BY rowid`, parentSessionID)
	return nonNilSlice(ids), err
}

// ListSubagentsAmong is the parent's records for the given children, in
// registration order. A parent can hold thousands of children; a caller that
// needs a few decodes only those.
func ListSubagentsAmong(parentSessionID string, childSessionIDs []string) ([]core.SubagentRecord, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	ids, err := databaseMarshalJSON(nonNilSlice(childSessionIDs))
	if err != nil {
		return nil, err
	}
	stored, err := queryTextColumn(connection, `SELECT record_json FROM subagent_sessions
         WHERE parent_session_id = ?1
           AND child_session_id IN (SELECT value FROM json_each(?2))
         ORDER BY rowid`, parentSessionID, string(ids))
	if err != nil {
		return nil, err
	}
	return decodeSubagentRecords(stored)
}

// RecordStoppedSubagents records, on the parent's session, the sub-agents its
// suspend is about to stop. A child already listed keeps its place and takes
// the newer details, so a suspend that runs again after a restart lists each
// child once.
func RecordStoppedSubagents(parentSessionID string, stopped []core.StoppedSubagent) error {
	stoppedCopy := append([]core.StoppedSubagent(nil), stopped...)
	_, err := submitDatabaseWrite("record_stopped_subagents", func(*Connection) (unitResult, error) {
		return unitResult{}, recordStoppedSubagentsTo(DatabasePath(), parentSessionID, stoppedCopy)
	})
	return err
}

func recordStoppedSubagentsTo(path string, parentSessionID string, stopped []core.StoppedSubagent) error {
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
	for i := range stopped {
		child := &stopped[i]
		encoded, err := databaseMarshalJSON(child)
		if err != nil {
			return err
		}
		if _, err := tx.Exec(`INSERT INTO stopped_subagents(parent_session_id, child_session_id, record_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(parent_session_id, child_session_id) DO UPDATE SET
                 record_json = excluded.record_json`,
			parentSessionID, child.ChildSessionID, string(encoded)); err != nil {
			return err
		}
	}
	return tx.Commit()
}

// LoadStoppedSubagents is the sub-agents a suspend of this session stopped
// and its model has not been told about yet, in the order they were
// recorded.
func LoadStoppedSubagents(parentSessionID string) ([]core.StoppedSubagent, error) {
	return loadStoppedSubagentsFrom(DatabasePath(), parentSessionID)
}

func loadStoppedSubagentsFrom(path string, parentSessionID string) ([]core.StoppedSubagent, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	stored, err := queryTextColumn(connection, `SELECT record_json FROM stopped_subagents
         WHERE parent_session_id = ?1 ORDER BY rowid`, parentSessionID)
	if err != nil {
		return nil, err
	}
	stopped := make([]core.StoppedSubagent, 0, len(stored))
	for _, text := range stored {
		var child core.StoppedSubagent
		if err := json.Unmarshal([]byte(text), &child); err != nil {
			return nil, fmt.Errorf("decode stopped sub-agent record: %w", err)
		}
		stopped = append(stopped, child)
	}
	return stopped, nil
}

// ClearStoppedSubagents forgets the stopped sub-agents whose note the
// parent's relay has taken. Only the named children go, so a child listed
// after the note was built waits for the next one.
func ClearStoppedSubagents(parentSessionID string, childSessionIDs []string) error {
	ids := append([]string(nil), childSessionIDs...)
	_, err := submitDatabaseWrite("clear_stopped_subagents", func(*Connection) (unitResult, error) {
		return unitResult{}, clearStoppedSubagentsFrom(DatabasePath(), parentSessionID, ids)
	})
	return err
}

func clearStoppedSubagentsFrom(path string, parentSessionID string, childSessionIDs []string) error {
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
	for _, childSessionID := range childSessionIDs {
		if _, err := tx.Exec(`DELETE FROM stopped_subagents
             WHERE parent_session_id = ?1 AND child_session_id = ?2`,
			parentSessionID, childSessionID); err != nil {
			return err
		}
	}
	return tx.Commit()
}

// LoadSessionState is one session's stored lifecycle state; false when the
// store holds no such session.
func LoadSessionState(sessionID string) (core.SessionState, bool, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return "", false, err
	}
	defer connection.Close()
	var stored string
	err = connection.QueryRow("SELECT state FROM sessions WHERE session_id = ?1", sessionID).Scan(&stored)
	if errors.Is(err, sql.ErrNoRows) {
		return "", false, nil
	}
	if err != nil {
		return "", false, err
	}
	return storedSessionState(stored), true, nil
}

// LoadSubagentReport is everything recorded about one child's report. A
// child with nothing recorded yet reads as the empty report.
func LoadSubagentReport(childSessionID string) (core.SubagentReport, error) {
	return loadSubagentReportFrom(DatabasePath(), childSessionID)
}

func loadSubagentReportFrom(path string, childSessionID string) (core.SubagentReport, error) {
	connection, err := openReader(path)
	if err != nil {
		return core.SubagentReport{}, err
	}
	defer connection.Close()
	report, err := loadSubagentReportWith(connection, childSessionID)
	if err != nil || report == nil {
		return core.SubagentReport{}, err
	}
	return *report, nil
}

// loadSubagentReportWith is a child's recorded report, or nil when nothing
// is recorded for it.
func loadSubagentReportWith(connection Queryer, childSessionID string) (*core.SubagentReport, error) {
	var handbackCommand, handbackMessage, reminderCommand, reminderFor, reminderFailedFor, reportDir *string
	var handbackAt, reminderAt, awaitedOrdinal *int64
	err := connection.QueryRow(`SELECT handback_command_id, handback_message, handback_recorded_at_ms,
                    reminder_command_id, reminder_for_command_id, reminder_sent_at_ms,
                    reminder_failed_for_command_id, awaited_ordinal, report_dir
             FROM subagent_handbacks WHERE child_session_id = ?1`, childSessionID).
		Scan(&handbackCommand, &handbackMessage, &handbackAt, &reminderCommand, &reminderFor, &reminderAt,
			&reminderFailedFor, &awaitedOrdinal, &reportDir)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	report := &core.SubagentReport{ReminderFailedFor: reminderFailedFor, ReportDir: reportDir}
	if handbackCommand != nil && handbackMessage != nil && handbackAt != nil {
		report.Handback = &core.SubagentHandback{CommandID: *handbackCommand, Message: *handbackMessage, RecordedAtMs: *handbackAt}
	}
	if reminderCommand != nil && reminderFor != nil && reminderAt != nil {
		report.Reminder = &core.HandbackReminder{CommandID: *reminderCommand, ForCommandID: *reminderFor, SentAtMs: *reminderAt}
	}
	if awaitedOrdinal != nil && *awaitedOrdinal >= 0 {
		ordinal := uint64(*awaitedOrdinal)
		report.AwaitedOrdinal = &ordinal
	}
	return report, nil
}

// RecordSubagentReportDir records the directory Mjolnir created for a
// child's report files. It is recorded only for a sub-agent child.
func RecordSubagentReportDir(childSessionID string, reportDir string) error {
	_, err := submitDatabaseWrite("record_subagent_report_dir", func(*Connection) (unitResult, error) {
		return unitResult{}, recordSubagentReportDirTo(DatabasePath(), childSessionID, reportDir)
	})
	return err
}

func recordSubagentReportDirTo(path string, childSessionID string, reportDir string) error {
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	return recordSubagentReportDirWith(connection, childSessionID, reportDir)
}

func recordSubagentReportDirWith(connection Queryer, childSessionID string, reportDir string) error {
	_, err := connection.Exec(`INSERT INTO subagent_handbacks(child_session_id, report_dir)
         SELECT ?1, ?2 WHERE EXISTS (
             SELECT 1 FROM subagent_sessions WHERE child_session_id = ?1
         )
         ON CONFLICT(child_session_id) DO UPDATE SET report_dir = excluded.report_dir`,
		childSessionID, reportDir)
	return err
}

// RecordSubagentPrompt records that the parent gave a child a prompt,
// accepted at ordinal. It is recorded only for a sub-agent child, and never
// moves backwards.
func RecordSubagentPrompt(childSessionID string, ordinal uint64) error {
	_, err := submitDatabaseWrite("record_subagent_prompt", func(*Connection) (unitResult, error) {
		return unitResult{}, recordSubagentPromptTo(DatabasePath(), childSessionID, ordinal)
	})
	return err
}

func recordSubagentPromptTo(path string, childSessionID string, ordinal uint64) error {
	if ordinal > math.MaxInt64 {
		return errors.New("prompt ordinal exceeds the store's range")
	}
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	_, err = connection.Exec(`INSERT INTO subagent_handbacks(child_session_id, awaited_ordinal)
         SELECT ?1, ?2 WHERE EXISTS (
             SELECT 1 FROM subagent_sessions WHERE child_session_id = ?1
         )
         ON CONFLICT(child_session_id) DO UPDATE SET
             awaited_ordinal = max(coalesce(awaited_ordinal, 0), excluded.awaited_ordinal)`,
		childSessionID, int64(ordinal))
	return err
}

// RecordSubagentHandback records a child's report for the turn it names,
// unless a report for that turn is already recorded. Returns whether this
// one was recorded: a child delivers one report per turn, and the check and
// the write are one transaction so two racing calls cannot both be accepted.
func RecordSubagentHandback(childSessionID string, handback *core.SubagentHandback) (bool, error) {
	handbackCopy := *handback
	return submitDatabaseWrite("record_subagent_handback", func(*Connection) (bool, error) {
		return recordSubagentHandbackTo(DatabasePath(), childSessionID, &handbackCopy)
	})
}

func recordSubagentHandbackTo(path string, childSessionID string, handback *core.SubagentHandback) (bool, error) {
	connection, err := open(path)
	if err != nil {
		return false, err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return false, err
	}
	defer tx.Rollback()
	var recordedFor *string
	err = tx.QueryRow("SELECT handback_command_id FROM subagent_handbacks WHERE child_session_id = ?1", childSessionID).Scan(&recordedFor)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return false, err
	}
	// The first report for a turn wins, including when a restart
	// continuation of that turn reports again under its own command ID.
	if recordedFor != nil && core.ReportAnswers(*recordedFor, handback.CommandID) {
		return false, nil
	}
	if _, err := tx.Exec(`INSERT INTO subagent_handbacks(
             child_session_id, handback_command_id, handback_message, handback_recorded_at_ms
         ) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(child_session_id) DO UPDATE SET
             handback_command_id = excluded.handback_command_id,
             handback_message = excluded.handback_message,
             handback_recorded_at_ms = excluded.handback_recorded_at_ms`,
		childSessionID, handback.CommandID, handback.Message, handback.RecordedAtMs); err != nil {
		return false, err
	}
	if err := tx.Commit(); err != nil {
		return false, err
	}
	return true, nil
}

// RecordHandbackReminder records the reminder sent after a child's turn
// ended without a report.
func RecordHandbackReminder(childSessionID string, reminder *core.HandbackReminder) error {
	reminderCopy := *reminder
	_, err := submitDatabaseWrite("record_handback_reminder", func(*Connection) (unitResult, error) {
		connection, err := open(DatabasePath())
		if err != nil {
			return unitResult{}, err
		}
		defer connection.Close()
		_, err = connection.Exec(`INSERT INTO subagent_handbacks(
                 child_session_id, reminder_command_id, reminder_for_command_id, reminder_sent_at_ms
             ) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(child_session_id) DO UPDATE SET
                 reminder_command_id = excluded.reminder_command_id,
                 reminder_for_command_id = excluded.reminder_for_command_id,
                 reminder_sent_at_ms = excluded.reminder_sent_at_ms`,
			childSessionID, reminderCopy.CommandID, reminderCopy.ForCommandID, reminderCopy.SentAtMs)
		return unitResult{}, err
	})
	return err
}

// RecordHandbackReminderFailed records that the reminder for a turn could
// not be sent, so the child's last message stands as its report.
func RecordHandbackReminderFailed(childSessionID string, forCommandID string) error {
	_, err := submitDatabaseWrite("record_handback_reminder_failed", func(*Connection) (unitResult, error) {
		connection, err := open(DatabasePath())
		if err != nil {
			return unitResult{}, err
		}
		defer connection.Close()
		_, err = connection.Exec(`INSERT INTO subagent_handbacks(child_session_id, reminder_failed_for_command_id)
             VALUES (?1, ?2)
             ON CONFLICT(child_session_id) DO UPDATE SET
                 reminder_failed_for_command_id = excluded.reminder_failed_for_command_id`,
			childSessionID, forCommandID)
		return unitResult{}, err
	})
	return err
}

func LookupSubagentRequest(parentSessionID string, requestKey string) (*core.SubagentRecord, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	var stored string
	err = connection.QueryRow(`SELECT record_json FROM subagent_sessions
             WHERE parent_session_id = ?1 AND request_key = ?2`, parentSessionID, requestKey).Scan(&stored)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	record, err := decodeSubagentRecord(stored)
	if err != nil {
		return nil, err
	}
	return &record, nil
}

// SaveSessionWithContainerSize persists a session and the container size it
// most recently launched on its host in one transaction.
func SaveSessionWithContainerSize(session *core.SessionRecord, host string, size core.HostContainerSize) error {
	sessionCopy := *session
	_, err := submitDatabaseWrite("save_session_with_container_size", func(*Connection) (unitResult, error) {
		return unitResult{}, saveSessionWithContainerSizeTo(DatabasePath(), &sessionCopy, &HostContainerSizeChoice{Host: host, Size: size})
	})
	return err
}

// SaveResumedSession: resume publication owns the target and checkout.
// Requested size and mounts, client titles, drafts, archive visibility and
// worker titles keep their owners.
func SaveResumedSession(session *core.SessionRecord, containerSize *HostContainerSizeChoice) error {
	sessionCopy := *session
	_, err := submitDatabaseWrite("save_resumed_session", func(*Connection) (unitResult, error) {
		return unitResult{}, saveResumedSessionTo(DatabasePath(), &sessionCopy, containerSize)
	})
	return err
}

// AcceptResumedSession commits explicit dialog choices together with the
// first provisioning publication.
func AcceptResumedSession(session *core.SessionRecord, containerSize *HostContainerSizeChoice, requests *SessionResumeOptions) error {
	sessionCopy := *session
	requestsCopy := *requests
	_, err := submitDatabaseWrite("accept resumed session", func(*Connection) (unitResult, error) {
		return unitResult{}, saveResumedSessionInEnvironmentTo(DatabasePath(), &sessionCopy, containerSize, nil, &requestsCopy, nil)
	})
	return err
}

func saveResumedSessionTo(path string, session *core.SessionRecord, containerSize *HostContainerSizeChoice) error {
	return saveResumedSessionInEnvironmentTo(path, session, containerSize, nil, nil, nil)
}

func RestoreResumedSession(session *core.SessionRecord, environmentID string) error {
	sessionCopy := *session
	_, err := submitDatabaseWrite("restore resumed environment", func(*Connection) (unitResult, error) {
		return unitResult{}, saveResumedSessionInEnvironmentTo(DatabasePath(), &sessionCopy, nil, &environmentID, nil, nil)
	})
	return err
}

// SettleSupersededMoveSource restores the source and releases Move's restore
// authority in one commit. Read the latest intent here: destination
// retirement may have updated it.
func SettleSupersededMoveSource(session *core.SessionRecord, environmentID *string, operationID string) error {
	sessionCopy := *session
	_, err := submitDatabaseWrite("settle superseded Move source", func(*Connection) (unitResult, error) {
		return unitResult{}, saveResumedSessionInEnvironmentTo(DatabasePath(), &sessionCopy, nil, environmentID, nil, &operationID)
	})
	return err
}

func saveResumedSessionInEnvironmentTo(path string, session *core.SessionRecord, containerSize *HostContainerSizeChoice, environmentID *string, requests *SessionResumeOptions, closeMove *string) error {
	if err := validateSessionRecord(session); err != nil {
		return err
	}
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	tx, err := connection.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	var bundle, workspace string
	if err := tx.QueryRow("SELECT bundle_id, workspace_id FROM session_contexts WHERE session_id = ?1", session.ID).
		Scan(&bundle, &workspace); err != nil {
		return err
	}
	if bundle != session.BundleID || workspace != session.WorkspaceID {
		return fmt.Errorf("session %s context changed before resume publication", session.ID)
	}
	if environmentID != nil {
		var owner string
		if err := tx.QueryRow("SELECT owner_session_id FROM environments WHERE environment_id=?1", *environmentID).Scan(&owner); err != nil {
			return err
		}
		if owner != session.ID {
			return errors.New("resume rollback does not own the selected environment")
		}
		if _, err := tx.Exec("UPDATE environment_bindings SET environment_id=?2 WHERE session_id=?1", session.ID, *environmentID); err != nil {
			return err
		}
	}
	if err := updateLifecycleFields(tx, session); err != nil {
		return err
	}
	if requests != nil {
		var allocation *core.SessionResourceAllocation
		if requests.ResourceAllocation != nil {
			allocation = &requests.ResourceAllocation
		}
		if err := updateContainerRequests(tx, session.ID, allocation, requests.AdditionalMounts); err != nil {
			return err
		}
	}
	checkout, err := optionalJSONColumn(session.Checkout)
	if err != nil {
		return err
	}
	if _, err := tx.Exec(`UPDATE sessions SET native_session_id = ?2, container_workspace = ?3,
             create_managed_worktree = ?4, launch_base = ?5, launch_branch = ?6,
             checkout_json = ?7
         WHERE session_id = ?1`,
		session.ID, session.NativeSessionID, session.ContainerWorkspace, session.CreateManagedWorktree,
		session.LaunchBase, session.LaunchBranch, checkout); err != nil {
		return err
	}
	if err := replaceCheckpoint(tx, session); err != nil {
		return err
	}
	if containerSize != nil {
		if err := writeHostContainerSize(tx, containerSize.Host, containerSize.Size); err != nil {
			return err
		}
	}
	if closeMove != nil {
		if err := resolveCancelledMoveForCloseWith(tx, session.ID, *closeMove, session.UpdatedAt); err != nil {
			return err
		}
	}
	return tx.Commit()
}

// SaveLifecycleSession updates only the fields a lifecycle transition owns on
// a session that already exists. Everything else — display titles,
// checkpoints, container settings, and attached directories — stays with its
// own writer.
func SaveLifecycleSession(session *core.SessionRecord) error {
	sessionCopy := *session
	_, err := submitDatabaseWrite("save_lifecycle_session", func(*Connection) (unitResult, error) {
		return unitResult{}, saveLifecycleSessionTo(DatabasePath(), &sessionCopy)
	})
	return err
}

// AdoptionCut is the optional current-state cut an adoption installs: Rust's
// `(MaterializedSession, LiveSessionBaseline)`.
type AdoptionCut struct {
	Projection core.MaterializedSession
	Baseline   core.LiveSessionBaseline
}

// EnqueueAdoptedSession claims the worker and installs its optional cut as
// Disconnected in one writer transaction. Ordinary attachment later owns all
// post-cut replay.
func EnqueueAdoptedSession[G any](ctx context.Context, expected core.SessionRecord, cut *AdoptionCut, updatedAt string, guard G) (*AcceptedDatabaseWrite[core.SessionRecord, G], error) {
	return enqueueDatabaseWrite(ctx, "install adopted session", guard, func(connection *Connection) (core.SessionRecord, error) {
		return installAdoptedSessionWith(connection, expected, cut, updatedAt)
	})
}

func installAdoptedSessionWith(connection *Connection, expected core.SessionRecord, cut *AdoptionCut, updatedAt string) (core.SessionRecord, error) {
	if err := core.ReachTestHook("adoption_install_write_accepted"); err != nil {
		return core.SessionRecord{}, err
	}
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return core.SessionRecord{}, err
	}
	defer tx.Rollback()
	var workspaceExists bool
	if err := tx.QueryRow("SELECT EXISTS(SELECT 1 FROM workspaces WHERE workspace_id = ?1)", expected.WorkspaceID).Scan(&workspaceExists); err != nil {
		return core.SessionRecord{}, err
	}
	if !workspaceExists {
		workspace, err := createOrGetWorkspaceWith(tx, "Recovered")
		if err != nil {
			return core.SessionRecord{}, err
		}
		expected.WorkspaceID = workspace.ID
	}
	locator := expected.Target
	if locator == nil {
		return core.SessionRecord{}, errors.New("adoption has no locator")
	}
	targets, err := readTargets(tx, nil)
	if err != nil {
		return core.SessionRecord{}, err
	}
	for _, id := range sortedTargetIDs(targets) {
		if id != expected.ID && targets[id].SameResource(locator) {
			return core.SessionRecord{}, fmt.Errorf("worker is already named by session %s", id)
		}
	}
	current, err := loadSessionWith(tx, expected.ID)
	if err != nil {
		return core.SessionRecord{}, err
	}
	if current != nil {
		if !(current.State == core.SessionStateDisconnected && current.NativeSessionID == nil &&
			targetLocatorsEqual(current.Target, expected.Target)) {
			return core.SessionRecord{}, fmt.Errorf("session %s changed before adoption install", expected.ID)
		}
	} else {
		if err := validateSessionRecord(&expected); err != nil {
			return core.SessionRecord{}, err
		}
		if err := insertSession(tx, &expected); err != nil {
			return core.SessionRecord{}, err
		}
	}
	if cut != nil {
		if cut.Projection.SessionID != expected.ID {
			return core.SessionRecord{}, errors.New("adoption cut belongs to another session")
		}
		if _, err := installLiveSessionBaselineWith(tx, &cut.Projection, &cut.Baseline); err != nil {
			return core.SessionRecord{}, err
		}
	}
	if _, err := tx.Exec("UPDATE sessions SET state='disconnected', native_session_id=NULL, updated_at=?2, last_error=NULL WHERE session_id=?1",
		expected.ID, updatedAt); err != nil {
		return core.SessionRecord{}, err
	}
	current, err = loadSessionWith(tx, expected.ID)
	if err != nil {
		return core.SessionRecord{}, err
	}
	if current == nil {
		return core.SessionRecord{}, errors.New("adopted session disappeared")
	}
	if err := tx.Commit(); err != nil {
		return core.SessionRecord{}, err
	}
	if cut != nil && len(cut.Projection.PendingElicitations) != 0 {
		NotifyMailboxOutboxChanged()
	}
	return *current, nil
}

// EnqueueAdoptionConnected publishes only the worker that the atomic adoption
// install claimed. Ordinary catch-up runs without an install holder and may
// race a destroy or another attach.
func EnqueueAdoptionConnected[G any](ctx context.Context, expected core.SessionRecord, nativeSessionID *string, updatedAt string, guard G) (*AcceptedDatabaseWrite[core.SessionRecord, G], error) {
	return enqueueDatabaseWrite(ctx, "publish adopted connection", guard, func(connection *Connection) (core.SessionRecord, error) {
		return publishAdoptionConnectedWith(connection, &expected, nativeSessionID, updatedAt)
	})
}

func publishAdoptionConnectedWith(connection *Connection, expected *core.SessionRecord, nativeSessionID *string, updatedAt string) (core.SessionRecord, error) {
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return core.SessionRecord{}, err
	}
	defer tx.Rollback()
	current, err := loadSessionWith(tx, expected.ID)
	if err != nil {
		return core.SessionRecord{}, err
	}
	if current == nil {
		return core.SessionRecord{}, errors.New("adopted session disappeared before attachment publication")
	}
	if !targetLocatorsEqual(current.Target, expected.Target) ||
		(current.State != core.SessionStateDisconnected && current.State != core.SessionStateRunning) {
		return core.SessionRecord{}, errors.New("adopted worker changed during attachment")
	}
	if current.State == core.SessionStateDisconnected {
		if current.NativeSessionID != nil {
			return core.SessionRecord{}, errors.New("adoption attachment ownership changed")
		}
		if err := markSessionWorkerConnectedWith(tx, expected.ID, nativeSessionID, updatedAt); err != nil {
			return core.SessionRecord{}, err
		}
	}
	current, err = loadSessionWith(tx, expected.ID)
	if err != nil {
		return core.SessionRecord{}, err
	}
	if current == nil {
		return core.SessionRecord{}, errors.New("adopted session disappeared")
	}
	if err := tx.Commit(); err != nil {
		return core.SessionRecord{}, err
	}
	return *current, nil
}

// RefuseAdoptionIfCurrent refuses a legacy provisional adoption without
// restoring stale session edits.
func RefuseAdoptionIfCurrent(expected *core.SessionRecord, detail string, updatedAt string) (core.SessionRecord, error) {
	expectedCopy := *expected
	return submitDatabaseWrite("refuse legacy adoption", func(connection *Connection) (core.SessionRecord, error) {
		return refuseAdoptionWith(connection, &expectedCopy, detail, updatedAt)
	})
}

func refuseAdoptionWith(connection *Connection, expected *core.SessionRecord, detail string, updatedAt string) (core.SessionRecord, error) {
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return core.SessionRecord{}, err
	}
	defer tx.Rollback()
	current, err := loadSessionWith(tx, expected.ID)
	if err != nil {
		return core.SessionRecord{}, err
	}
	if current == nil {
		return core.SessionRecord{}, errors.New("provisional adoption record disappeared")
	}
	if !(current.State == core.SessionStateDisconnected && current.NativeSessionID == nil &&
		targetLocatorsEqual(current.Target, expected.Target) &&
		current.TargetTemplateID == expected.TargetTemplateID &&
		current.LastProfile == expected.LastProfile) {
		return core.SessionRecord{}, errors.New("adoption refusal ownership changed")
	}
	previousError, err := previousSessionError(tx, current.ID)
	if err != nil {
		return core.SessionRecord{}, err
	}
	current.Target = nil
	current.State = core.SessionStateLost
	lastError := detail
	if current.LastError != nil {
		lastError = *current.LastError + "\n" + detail
	}
	current.LastError = &lastError
	current.UpdatedAt = updatedAt
	if _, err := tx.Exec("UPDATE sessions SET state=?2, last_error=?3, updated_at=?4 WHERE session_id=?1",
		current.ID, current.State.AsStr(), current.LastError, current.UpdatedAt); err != nil {
		return core.SessionRecord{}, err
	}
	if err := recordSessionFaultTransition(tx, current, previousError); err != nil {
		return core.SessionRecord{}, err
	}
	if err := settleCreateIntent(tx, current); err != nil {
		return core.SessionRecord{}, err
	}
	if err := replaceTargets(tx, current); err != nil {
		return core.SessionRecord{}, err
	}
	refused, err := loadSessionWith(tx, expected.ID)
	if err != nil {
		return core.SessionRecord{}, err
	}
	if refused == nil {
		return core.SessionRecord{}, errors.New("refused adoption record disappeared")
	}
	if err := tx.Commit(); err != nil {
		return core.SessionRecord{}, err
	}
	return *refused, nil
}

// RecordAdoptionFailureIfCurrent records a failed adoption only while its
// disconnected resource owner is still current. A successful concurrent
// adoption must never be demoted by a loser.
func RecordAdoptionFailureIfCurrent(expected *core.SessionRecord, detail string, updatedAt string) (*core.SessionRecord, error) {
	expectedCopy := *expected
	return submitDatabaseWrite("record adoption failure", func(*Connection) (*core.SessionRecord, error) {
		return recordAdoptionFailureIfCurrentTo(DatabasePath(), &expectedCopy, detail, updatedAt)
	})
}

func recordAdoptionFailureIfCurrentTo(path string, expected *core.SessionRecord, detail string, updatedAt string) (*core.SessionRecord, error) {
	connection, err := open(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return nil, err
	}
	defer tx.Rollback()
	current, err := loadSessionWith(tx, expected.ID)
	if err != nil || current == nil {
		return nil, err
	}
	if current.State == core.SessionStateDisconnected && current.NativeSessionID == nil &&
		targetLocatorsEqual(current.Target, expected.Target) &&
		current.TargetTemplateID == expected.TargetTemplateID &&
		current.LastProfile == expected.LastProfile &&
		current.BundleID == expected.BundleID {
		lastError := detail
		current.LastError = &lastError
		current.UpdatedAt = updatedAt
		if err := updateLifecycleFields(tx, current); err != nil {
			return nil, err
		}
	}
	if err := tx.Commit(); err != nil {
		return nil, err
	}
	return current, nil
}

// FailUnreadySession applies either a live preparation failure or a dead
// worker's startup failure only to the lifecycle state and revision that the
// caller observed.
func FailUnreadySession(sessionID string, cause string, observedState core.SessionState, observedUpdatedAt string) (*core.SessionRecord, error) {
	return submitDatabaseWrite("fail_unready_session", func(connection *Connection) (*core.SessionRecord, error) {
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return nil, err
		}
		defer tx.Rollback()
		record, err := loadSessionWith(tx, sessionID)
		if err != nil || record == nil {
			return nil, err
		}
		if (observedState != core.SessionStateRunning && observedState != core.SessionStateDisconnected) ||
			record.State != observedState || record.UpdatedAt != observedUpdatedAt {
			return nil, nil
		}
		record.State = core.SessionStateError
		record.UpdatedAt = time.Now().UTC().Format("2006-01-02T15:04:05Z")
		record.LastError = &cause
		if err := updateLifecycleFields(tx, record); err != nil {
			return nil, err
		}
		if err := tx.Commit(); err != nil {
			return nil, err
		}
		return record, nil
	})
}

// SaveSuspensionIntent: acceptance of Suspend owns its intent, readiness and
// Closing publication.
func SaveSuspensionIntent(session *core.SessionRecord, readyMove *core.MoveOperation, consent *core.SuspendConsent) error {
	sessionCopy := *session
	consentJSON, err := optionalJSONColumn(consent)
	if err != nil {
		return err
	}
	_, err = submitDatabaseWrite("save suspension intent", func(connection *Connection) (unitResult, error) {
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return unitResult{}, err
		}
		defer tx.Rollback()
		operation, err := loadMoveOperationWith(tx, sessionCopy.ID)
		if err != nil {
			return unitResult{}, err
		}
		if operation != nil {
			if operation.IsResolved() {
				if _, err := tx.Exec("UPDATE session_moves SET close_requested_operation_id=NULL WHERE session_id=?1", sessionCopy.ID); err != nil {
					return unitResult{}, err
				}
			} else {
				if readyMove != nil && readyMove.OperationID == operation.OperationID {
					operation.DestinationTarget = readyMove.DestinationTarget
					operation.DestinationNativeSessionID = readyMove.DestinationNativeSessionID
					operation.UpdatedAt = sessionCopy.UpdatedAt
					if err := saveMoveOperationWith(tx, operation); err != nil {
						return unitResult{}, err
					}
				}
				// Only this accepted request grants close authority. A Move's
				// cancellation/phase alone never does, even for historical
				// rows.
				if sessionCopy.State != core.SessionStateDestroying {
					sessionCopy.State = core.SessionStateClosing
				}
				if _, err := tx.Exec("UPDATE session_moves SET close_requested_operation_id=?2 WHERE session_id=?1 AND operation_id=?2",
					sessionCopy.ID, operation.OperationID); err != nil {
					return unitResult{}, err
				}
			}
		}
		if err := validateSessionRecord(&sessionCopy); err != nil {
			return unitResult{}, err
		}
		if err := updateLifecycleFields(tx, &sessionCopy); err != nil {
			return unitResult{}, err
		}
		if _, err := tx.Exec("UPDATE sessions SET closing_consent = ?2 WHERE session_id = ?1", sessionCopy.ID, consentJSON); err != nil {
			return unitResult{}, err
		}
		return unitResult{}, tx.Commit()
	})
	return err
}

// LoadClosingConsent reads the persisted suspend consent for a session
// currently in Closing.
func LoadClosingConsent(sessionID string) (*core.SuspendConsent, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	var raw *string
	err = connection.QueryRow("SELECT closing_consent FROM sessions WHERE session_id = ?1", sessionID).Scan(&raw)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil || raw == nil {
		return nil, err
	}
	consent := new(core.SuspendConsent)
	if err := json.Unmarshal([]byte(*raw), consent); err != nil {
		return nil, fmt.Errorf("decode sessions.closing_consent: %w", err)
	}
	return consent, nil
}

func SaveFailedClose(session *core.SessionRecord) (core.SessionRecord, error) {
	sessionCopy := *session
	return submitDatabaseWrite("save failed close", func(connection *Connection) (core.SessionRecord, error) {
		if err := validateSessionRecord(&sessionCopy); err != nil {
			return core.SessionRecord{}, err
		}
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return core.SessionRecord{}, err
		}
		defer tx.Rollback()
		var current string
		if err := tx.QueryRow("SELECT state FROM sessions WHERE session_id=?1", sessionCopy.ID).Scan(&current); err != nil {
			return core.SessionRecord{}, err
		}
		sessionCopy.State = storedSessionState(current)
		operation, err := loadMoveOperationWith(tx, sessionCopy.ID)
		if err != nil {
			return core.SessionRecord{}, err
		}
		var rowID string
		var closeID *string
		err = tx.QueryRow("SELECT operation_id, close_requested_operation_id FROM session_moves WHERE session_id=?1", sessionCopy.ID).Scan(&rowID, &closeID)
		closeIntent := true
		if errors.Is(err, sql.ErrNoRows) {
			closeIntent = false
		} else if err != nil {
			return core.SessionRecord{}, err
		}
		acceptedMoveClose := operation != nil && !operation.IsResolved() && closeIntent &&
			exactMoveCloseIntent(operation.OperationID, rowID, closeID)
		unresolvedMove := operation != nil && !operation.IsResolved()
		if (sessionCopy.State == core.SessionStateClosing && (!unresolvedMove || acceptedMoveClose)) ||
			(sessionCopy.State == core.SessionStateProvisioning && acceptedMoveClose) {
			sessionCopy.State = core.SessionStateError
		}
		if err := updateLifecycleFields(tx, &sessionCopy); err != nil {
			return core.SessionRecord{}, err
		}
		// Destroying is still an owned, verified teardown, not a terminal
		// failure. Its request must survive until startup finishes cleanup.
		if sessionCopy.State != core.SessionStateDestroying {
			if _, err := tx.Exec("UPDATE session_moves SET close_requested_operation_id=NULL WHERE session_id=?1", sessionCopy.ID); err != nil {
				return core.SessionRecord{}, err
			}
		}
		if err := tx.Commit(); err != nil {
			return core.SessionRecord{}, err
		}
		return sessionCopy, nil
	})
}

// SaveClosedSession: a verified close of a restored destination also ends
// its cancelled Move. Check the old placement before publishing the terminal
// lifecycle state.
func SaveClosedSession(session *core.SessionRecord) error {
	sessionCopy := *session
	_, err := submitDatabaseWrite("save closed session", func(connection *Connection) (unitResult, error) {
		if err := validateSessionRecord(&sessionCopy); err != nil {
			return unitResult{}, err
		}
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return unitResult{}, err
		}
		defer tx.Rollback()
		var readyMove *core.MoveOperation
		if sessionCopy.State == core.SessionStateStopped {
			operation, err := loadMoveOperationWith(tx, sessionCopy.ID)
			if err != nil {
				return unitResult{}, err
			}
			if operation != nil && operation.Phase == core.MovePhaseCancelled && !operation.QueueAdmissionStarted &&
				operation.DestinationTarget != nil {
				readyMove = operation
			}
		}
		if readyMove != nil {
			id := sessionCopy.ID
			targets, err := readTargets(tx, &id)
			if err != nil {
				return unitResult{}, err
			}
			matched := false
			for _, target := range targets {
				if targetLocatorsEqual(target, readyMove.DestinationTarget) {
					matched = true
					break
				}
			}
			if !matched {
				readyMove = nil
			}
		}
		if err := updateLifecycleFields(tx, &sessionCopy); err != nil {
			return unitResult{}, err
		}
		if readyMove != nil {
			if err := resolveCancelledMoveForCloseWith(tx, sessionCopy.ID, readyMove.OperationID, sessionCopy.UpdatedAt); err != nil {
				return unitResult{}, err
			}
		}
		if sessionCopy.State == core.SessionStateStopped {
			if _, err := tx.Exec("UPDATE session_moves SET close_requested_operation_id=NULL WHERE session_id=?1", sessionCopy.ID); err != nil {
				return unitResult{}, err
			}
		}
		return unitResult{}, tx.Commit()
	})
	return err
}

// ClearRecordedLifecycleFailure clears exactly the failure observed at turn
// start. The writer owns the comparison and update together so a concurrent
// lifecycle result survives.
func ClearRecordedLifecycleFailure(previous *core.SessionRecord, updatedAt string) (bool, error) {
	if _, ok := previous.PublicError(); !ok {
		return false, nil
	}
	sessionID, lastError := previous.ID, previous.LastError
	return submitDatabaseWrite("clear_recorded_lifecycle_failure", func(connection *Connection) (bool, error) {
		changed, err := changedRows(connection.Exec(`UPDATE sessions SET last_error = NULL, updated_at = ?3
             WHERE session_id = ?1 AND last_error = ?2`, sessionID, lastError, updatedAt))
		return changed == 1, err
	})
}

// ReplaceCheckpointArchiveIfCurrent points one checkpoint row at a
// replacement archive when it still names expectedArchivePath and
// expectedSha256.
//
// The update changes only archive_path and sha256, in one immediate
// transaction. created_at and event_frontier stay as they are. A missing row
// or a row that no longer matches returns false so a concurrent checkpoint
// write is left alone. An illegal digest is an error from the table's check
// constraint.
func ReplaceCheckpointArchiveIfCurrent(sessionID string, expectedArchivePath string, expectedSha256 string, replacement *core.CheckpointMetadata) (bool, error) {
	expectedArchive := pathToBlob(expectedArchivePath)
	archivePath := pathToBlob(replacement.ArchivePath)
	sha256 := replacement.Sha256
	return submitDatabaseWrite("replace_checkpoint_archive_if_current", func(connection *Connection) (bool, error) {
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return false, err
		}
		defer tx.Rollback()
		changed, err := changedRows(tx.Exec(`UPDATE session_checkpoints
                 SET archive_path = ?3, sha256 = ?4
                 WHERE session_id = ?1 AND archive_path = ?2 AND sha256 = ?5`,
			sessionID, expectedArchive, archivePath, sha256, expectedSha256))
		if err != nil {
			return false, err
		}
		if err := tx.Commit(); err != nil {
			return false, err
		}
		return changed == 1, nil
	})
}

func SaveCheckpointedSession(session *core.SessionRecord) error {
	sessionCopy := *session
	_, err := submitDatabaseWrite("save_checkpointed_session", func(*Connection) (unitResult, error) {
		return unitResult{}, saveCheckpointedSessionTo(DatabasePath(), &sessionCopy)
	})
	return err
}

// RecoverInterruptedCheckpointingSessions recovers lifecycle rows stranded
// by a process exit during checkpoint creation. This must be called once by
// the top-level controller process while it owns the controller-store guard,
// not by per-operation reloads.
func RecoverInterruptedCheckpointingSessions(updatedAt string) (int, error) {
	return submitDatabaseWrite("recover_interrupted_checkpointing_sessions", func(*Connection) (int, error) {
		return recoverInterruptedCheckpointingSessionsTo(DatabasePath(), updatedAt)
	})
}

// SetSessionTitleOverride changes only the user-owned display name. This
// avoids writing a stale SessionRecord over independently committed
// checkpoint or relay metadata.
func SetSessionTitleOverride(sessionID string, title string, updatedAt string) error {
	_, err := submitDatabaseWrite("set_session_title_override", func(*Connection) (unitResult, error) {
		return unitResult{}, setSessionTitleOverrideTo(DatabasePath(), sessionID, title, updatedAt)
	})
	return err
}

// RenameProfileReferences rewrites a configured profile id in every
// persisted session and recorded turn in one SQLite transaction.
// Configuration is stored separately, so the controller owns coordinating
// this update with the matching config-map rename.
func RenameProfileReferences(oldID string, newID string) (int, error) {
	return submitDatabaseWrite("rename_profile_references", func(*Connection) (int, error) {
		return renameProfileReferencesAt(DatabasePath(), oldID, newID)
	})
}

func renameProfileReferencesAt(path string, oldID string, newID string) (int, error) {
	if oldID == "" || newID == "" {
		return 0, errors.New("profile ids cannot use the reserved legacy preference key")
	}
	if oldID == newID {
		return 0, nil
	}
	connection, err := open(path)
	if err != nil {
		return 0, err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return 0, err
	}
	defer tx.Rollback()
	changed, err := changedRows(tx.Exec("UPDATE sessions SET last_profile = ?2 WHERE last_profile = ?1", oldID, newID))
	if err != nil {
		return 0, err
	}
	for _, statement := range []string{
		"UPDATE session_contexts SET profile_id = ?2 WHERE profile_id = ?1",
		"UPDATE session_turn_selections SET profile = ?2 WHERE profile = ?1",
		// The source wins on a rename. A retry after commit finds no source
		// row.
		`INSERT INTO subagent_preference(profile_id, policy)
         SELECT ?2, policy FROM subagent_preference WHERE profile_id = ?1
         ON CONFLICT(profile_id) DO UPDATE SET policy = excluded.policy`,
	} {
		if _, err := tx.Exec(statement, oldID, newID); err != nil {
			return 0, err
		}
	}
	if _, err := tx.Exec("DELETE FROM subagent_preference WHERE profile_id = ?1", oldID); err != nil {
		return 0, err
	}
	if _, err := tx.Exec(`INSERT INTO oracle_preference(profile_id, enabled)
         SELECT ?2, enabled FROM oracle_preference WHERE profile_id = ?1
         ON CONFLICT(profile_id) DO UPDATE SET enabled = excluded.enabled`, oldID, newID); err != nil {
		return 0, err
	}
	if _, err := tx.Exec("DELETE FROM oracle_preference WHERE profile_id = ?1", oldID); err != nil {
		return 0, err
	}
	if err := tx.Commit(); err != nil {
		return 0, err
	}
	return int(changed), nil
}

// RenameTargetReferences rewrites a configured target id in every persisted
// session in one SQLite transaction.
func RenameTargetReferences(oldID string, newID string) (int, error) {
	return renameSessionReference("target_template_id", oldID, newID)
}

func renameSessionReference(column string, oldID string, newID string) (int, error) {
	if column != "target_template_id" {
		return 0, errors.New("unsupported session reference column")
	}
	return submitDatabaseWrite("rename_session_reference", func(*Connection) (int, error) {
		return renameSessionReferenceAt(DatabasePath(), column, oldID, newID)
	})
}

func renameSessionReferenceAt(path string, column string, oldID string, newID string) (int, error) {
	connection, err := open(path)
	if err != nil {
		return 0, err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return 0, err
	}
	defer tx.Rollback()
	changed, err := changedRows(tx.Exec(fmt.Sprintf("UPDATE sessions SET %s = ?2 WHERE %s = ?1", column, column), oldID, newID))
	if err != nil {
		return 0, err
	}
	if err := tx.Commit(); err != nil {
		return 0, err
	}
	return int(changed), nil
}

// SetSessionArchived changes only whether the resume dialog hides this
// session. Archiving is a display choice, so it has its own writer and never
// rewrites lifecycle, checkpoint, or title columns another task owns.
func SetSessionArchived(sessionID string, archived bool) error {
	_, err := submitDatabaseWrite("set_session_archived", func(*Connection) (unitResult, error) {
		return unitResult{}, setSessionArchivedTo(DatabasePath(), sessionID, archived)
	})
	return err
}

// MarkSessionTargetMissing records that the managed target of an otherwise
// live session is definitively gone. A verified checkpoint keeps the session
// recoverable as an error on the dashboard; without one, the session is
// lost. The state predicate keeps a late poll result from overwriting a
// concurrent lifecycle transition. The bool is false when nothing changed.
func MarkSessionTargetMissing(sessionID string, detail string, updatedAt string) (core.SessionState, bool, error) {
	type marked struct {
		state core.SessionState
		ok    bool
	}
	result, err := submitDatabaseWrite("mark_session_target_missing", func(*Connection) (marked, error) {
		state, ok, err := markSessionTargetMissingTo(DatabasePath(), sessionID, detail, updatedAt)
		return marked{state, ok}, err
	})
	return result.state, result.ok, err
}

func markSessionTargetMissingTo(path string, sessionID string, detail string, updatedAt string) (core.SessionState, bool, error) {
	return markSessionTargetMissingIfCurrentTo(path, sessionID, detail, updatedAt, nil)
}

// MarkSessionTargetMissingIfCurrent records a definitive worker failure only
// while the observed session record is still current. A delayed background
// write must not invalidate a resume.
func MarkSessionTargetMissingIfCurrent(sessionID string, detail string, updatedAt string, observedUpdatedAt string) (core.SessionState, bool, error) {
	type marked struct {
		state core.SessionState
		ok    bool
	}
	result, err := submitDatabaseWrite("mark_session_target_missing_if_current", func(*Connection) (marked, error) {
		state, ok, err := markSessionTargetMissingIfCurrentTo(DatabasePath(), sessionID, detail, updatedAt, &observedUpdatedAt)
		return marked{state, ok}, err
	})
	return result.state, result.ok, err
}

func markSessionTargetMissingIfCurrentTo(path string, sessionID string, detail string, updatedAt string, observedUpdatedAt *string) (core.SessionState, bool, error) {
	connection, err := open(path)
	if err != nil {
		return "", false, err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return "", false, err
	}
	defer tx.Rollback()
	owned, err := containerUpgradeOwnsTargetWith(tx, sessionID)
	if err != nil {
		return "", false, err
	}
	if owned {
		return "", false, nil
	}
	previousError, err := previousSessionError(tx, sessionID)
	if err != nil {
		return "", false, err
	}
	changed, err := changedRows(tx.Exec(`UPDATE sessions
         SET state = CASE
                 WHEN EXISTS(
                     SELECT 1 FROM session_checkpoints
                     WHERE session_checkpoints.session_id = sessions.session_id
                 ) THEN 'error'
                 ELSE 'lost'
             END,
             last_error = ?2,
             updated_at = ?3
         WHERE session_id = ?1
           AND (?4 IS NULL OR updated_at = ?4)
           AND state IN ('running', 'disconnected')`,
		sessionID, detail, updatedAt, observedUpdatedAt))
	if err != nil {
		return "", false, err
	}
	if changed > 1 {
		return "", false, fmt.Errorf("updated %d sessions for %s", changed, sessionID)
	}
	var state core.SessionState
	if changed == 1 {
		var stored string
		if err := tx.QueryRow("SELECT state FROM sessions WHERE session_id = ?1", sessionID).Scan(&stored); err != nil {
			return "", false, err
		}
		state = storedSessionState(stored)
	}
	if changed == 1 && (previousError == nil || *previousError != detail) {
		if err := insertAPIEvent(tx, sessionID, time.Now().UnixMilli(), core.ApiEventDataSessionFault{
			Reason:  core.OutcomeReasonRuntimeUnavailable,
			Message: detail,
		}); err != nil {
			return "", false, err
		}
	}
	if err := tx.Commit(); err != nil {
		return "", false, err
	}
	return state, changed == 1, nil
}

// updateOneSession runs an UPDATE that must change exactly one session.
func updateOneSession(connection Queryer, sessionID string, query string, args ...any) error {
	changed, err := changedRows(connection.Exec(query, args...))
	if err != nil {
		return err
	}
	if changed != 1 {
		return fmt.Errorf("unknown session %s", sessionID)
	}
	return nil
}

func setSessionArchivedTo(path string, sessionID string, archived bool) error {
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	return updateOneSession(connection, sessionID, "UPDATE sessions SET archived = ?2 WHERE session_id = ?1", sessionID, archived)
}

// SetSessionNotificationsIgnored changes only whether this session raises
// notifications. Ignoring is a display choice, so it has its own writer and
// never rewrites lifecycle, checkpoint, or title columns another task owns.
func SetSessionNotificationsIgnored(sessionID string, ignored bool) error {
	_, err := submitDatabaseWrite("set_session_notifications_ignored", func(*Connection) (unitResult, error) {
		return unitResult{}, setSessionNotificationsIgnoredTo(DatabasePath(), sessionID, ignored)
	})
	return err
}

func setSessionNotificationsIgnoredTo(path string, sessionID string, ignored bool) error {
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	return updateOneSession(connection, sessionID, "UPDATE sessions SET notifications_ignored = ?2 WHERE session_id = ?1", sessionID, ignored)
}

// HiddenNativeSession is one entry of the hidden native session set: Rust's
// `(HarnessKind, String)`.
type HiddenNativeSession struct {
	Harness         core.HarnessKind
	NativeSessionID string
}

// HiddenNativeSessions is the native sessions the resume dialog hides. Hel
// never writes into a harness home, so the hidden set lives here instead of
// in the harness's own store.
func HiddenNativeSessions() (map[HiddenNativeSession]struct{}, error) {
	return hiddenNativeSessionsFrom(DatabasePath())
}

func hiddenNativeSessionsFrom(path string) (map[HiddenNativeSession]struct{}, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	rows, err := connection.Query("SELECT harness_kind, native_session_id FROM hidden_native_sessions")
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	hidden := map[HiddenNativeSession]struct{}{}
	for rows.Next() {
		var harnessText, nativeSessionID string
		if err := rows.Scan(&harnessText, &nativeSessionID); err != nil {
			return nil, err
		}
		// Rows for a harness this release no longer supports are ignored, not
		// fatal; they simply hide nothing.
		harness, err := core.ParseHarnessKind(harnessText)
		if err != nil {
			slog.Warn("ignoring a hidden native session for a harness that is no longer supported", "harness", harnessText)
			continue
		}
		hidden[HiddenNativeSession{Harness: harness, NativeSessionID: nativeSessionID}] = struct{}{}
	}
	return hidden, rows.Err()
}

// SetNativeSessionHidden hides or reveals one native session in the resume
// dialog.
func SetNativeSessionHidden(harness core.HarnessKind, nativeSessionID string, hidden bool) error {
	_, err := submitDatabaseWrite("set_native_session_hidden", func(*Connection) (unitResult, error) {
		return unitResult{}, setNativeSessionHiddenTo(DatabasePath(), harness, nativeSessionID, hidden)
	})
	return err
}

func setNativeSessionHiddenTo(path string, harness core.HarnessKind, nativeSessionID string, hidden bool) error {
	if isRustBlank(nativeSessionID) {
		return errors.New("native session id is empty")
	}
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	if hidden {
		_, err = connection.Exec(`INSERT INTO hidden_native_sessions(harness_kind, native_session_id, hidden_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(harness_kind, native_session_id) DO NOTHING`,
			harness.ID(), nativeSessionID, core.FormatChronoRFC3339(time.Now()))
	} else {
		_, err = connection.Exec(`DELETE FROM hidden_native_sessions
             WHERE harness_kind = ?1 AND native_session_id = ?2`,
			harness.ID(), nativeSessionID)
	}
	return err
}

// SetSessionContainerSettings changes only the per-session container
// provisioning inputs: the size overrides and the attached directories.
// Everything else the session row owns is left to its own writer. A nil
// allocation clears the size override.
func SetSessionContainerSettings(sessionID string, allocation core.SessionResourceAllocation, mounts []core.AdditionalMount, updatedAt string) error {
	mountsCopy := append([]core.AdditionalMount(nil), mounts...)
	_, err := submitDatabaseWrite("set_session_container_settings", func(*Connection) (unitResult, error) {
		return unitResult{}, setSessionContainerSettingsTo(DatabasePath(), sessionID, allocation, mountsCopy, updatedAt)
	})
	return err
}

func setSessionContainerSettingsTo(path string, sessionID string, allocation core.SessionResourceAllocation, mounts []core.AdditionalMount, updatedAt string) error {
	if isRustBlank(updatedAt) {
		return errors.New("session update timestamp is empty")
	}
	if err := core.ValidateAdditionalMounts(mounts); err != nil {
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
	var restarting bool
	if err := tx.QueryRow("SELECT EXISTS(SELECT 1 FROM session_restart_intents WHERE session_id = ?1)", sessionID).Scan(&restarting); err != nil {
		return err
	}
	if restarting {
		return core.RefusalPrecondition("finish Restart or suspend the session before changing container settings")
	}
	if err := writeContainerSettings(tx, sessionID, allocation, mounts, updatedAt); err != nil {
		return err
	}
	return tx.Commit()
}

func writeContainerSettings(tx *sql.Tx, sessionID string, allocation core.SessionResourceAllocation, mounts []core.AdditionalMount, updatedAt string) error {
	var encoded any
	if allocation != nil {
		value, err := databaseMarshalJSON(allocation)
		if err != nil {
			return err
		}
		encoded = string(value)
	}
	if err := updateOneSession(tx, sessionID, `UPDATE sessions
         SET resource_allocation = ?2, updated_at = ?3
         WHERE session_id = ?1`, sessionID, encoded, updatedAt); err != nil {
		return err
	}
	return replaceMounts(tx, sessionID, mounts)
}

func setSessionTitleOverrideTo(path string, sessionID string, title string, updatedAt string) error {
	if isRustBlank(title) {
		return errors.New("session title is empty")
	}
	if isRustBlank(updatedAt) {
		return errors.New("session update timestamp is empty")
	}
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	return updateOneSession(connection, sessionID, `UPDATE sessions
         SET session_title_override = ?2, updated_at = ?3
         WHERE session_id = ?1`, sessionID, title, updatedAt)
}

// SetSessionACPTitleAtProjection persists a title resolved from a
// materialized relay view. The writer checks the projection frontier in the
// same statement as the update, so a delayed observation cannot overwrite a
// title from a newer view.
func SetSessionACPTitleAtProjection(sessionID string, appliedOrdinal uint64, appliedDigest string, title *string) error {
	_, err := submitDatabaseWrite("set_session_acp_title_at_projection", func(connection *Connection) (unitResult, error) {
		return unitResult{}, setSessionACPTitleAtProjectionWith(connection, sessionID, appliedOrdinal, appliedDigest, title)
	})
	return err
}

// normalizedSessionTitle is Option::and_then(normalize_session_title).
func normalizedSessionTitle(title *string) *string {
	if title == nil {
		return nil
	}
	normalized, ok := core.NormalizeSessionTitle(*title)
	if !ok {
		return nil
	}
	return &normalized
}

func setSessionACPTitleAtProjectionWith(connection Queryer, sessionID string, appliedOrdinal uint64, appliedDigest string, title *string) error {
	_, err := connection.Exec(`UPDATE sessions SET acp_session_title = ?2
         WHERE session_id = ?1 AND acp_session_title IS NOT ?2 AND EXISTS (
             SELECT 1 FROM materialized_sessions
             WHERE session_id = ?1 AND applied_event_ordinal = ?3 AND applied_event_digest = ?4
         )`, sessionID, normalizedSessionTitle(title), appliedOrdinal, appliedDigest)
	return err
}

// setSessionACPTitleTo is test-only in Rust.
func setSessionACPTitleTo(path string, sessionID string, title *string) error {
	if title != nil && isRustBlank(*title) {
		return errors.New("ACP session title is empty")
	}
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	return updateOneSession(connection, sessionID, "UPDATE sessions SET acp_session_title = ?2 WHERE session_id = ?1",
		sessionID, normalizedSessionTitle(title))
}

// MarkSessionWorkerConnected commits the successful handshake for a newly
// provisioned worker without replacing checkpoint or display metadata owned
// by other controller tasks.
func MarkSessionWorkerConnected(sessionID string, nativeSessionID *string, updatedAt string) error {
	_, err := submitDatabaseWrite("mark_session_worker_connected", func(*Connection) (unitResult, error) {
		return unitResult{}, markSessionWorkerConnectedTo(DatabasePath(), sessionID, nativeSessionID, updatedAt)
	})
	return err
}

// AdoptNativeSessionID points a session at a native session its worker
// opened on its own. Only that column moves: the session's lifecycle state
// belongs to whatever operation is running.
func AdoptNativeSessionID(sessionID string, nativeSessionID string) error {
	_, err := submitDatabaseWrite("adopt_native_session_id", func(*Connection) (unitResult, error) {
		return unitResult{}, adoptNativeSessionIDTo(DatabasePath(), sessionID, nativeSessionID)
	})
	return err
}

func adoptNativeSessionIDTo(path string, sessionID string, nativeSessionID string) error {
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	return updateOneSession(connection, sessionID, "UPDATE sessions SET native_session_id = ?2 WHERE session_id = ?1", sessionID, nativeSessionID)
}

func markSessionWorkerConnectedTo(path string, sessionID string, nativeSessionID *string, updatedAt string) error {
	if isRustBlank(updatedAt) {
		return errors.New("worker connection timestamp is empty")
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
	if err := markSessionWorkerConnectedWith(tx, sessionID, nativeSessionID, updatedAt); err != nil {
		return err
	}
	return tx.Commit()
}

func markSessionWorkerConnectedWith(tx *sql.Tx, sessionID string, nativeSessionID *string, updatedAt string) error {
	if err := updateOneSession(tx, sessionID, `UPDATE sessions
         SET state = 'running',
             native_session_id = coalesce(?2, native_session_id),
             updated_at = ?3,
             last_error = NULL
         WHERE session_id = ?1`, sessionID, nativeSessionID, updatedAt); err != nil {
		return err
	}
	// The create commits in this write: nothing is left for a later daemon.
	return settleCreateIntentForState(tx, sessionID, core.SessionStateRunning)
}

func recoverInterruptedCheckpointingSessionsTo(path string, updatedAt string) (int, error) {
	if isRustBlank(updatedAt) {
		return 0, errors.New("checkpoint recovery timestamp is empty")
	}
	connection, err := open(path)
	if err != nil {
		return 0, err
	}
	defer connection.Close()
	tx, err := connection.Begin()
	if err != nil {
		return 0, err
	}
	defer tx.Rollback()
	ids, err := queryTextColumn(tx, "SELECT session_id FROM checkpoint_operations")
	if err != nil {
		return 0, err
	}
	for _, id := range ids {
		reason := core.OutcomeReasonControllerRestarted
		message := "Checkpoint operation interrupted by controller restart"
		if err := finishCheckpointOperation(tx, id, core.CommandResultKindFailed, &reason, &message); err != nil {
			return 0, err
		}
	}
	changed, err := changedRows(tx.Exec("UPDATE sessions SET state = 'running', updated_at = ?1, last_checkpoint_error = ?2 WHERE state = 'checkpointing'",
		updatedAt, "checkpointing was interrupted by a controller restart; the target was left running"))
	if err != nil {
		return 0, err
	}
	if err := tx.Commit(); err != nil {
		return 0, err
	}
	return int(changed), nil
}

func SaveSessionTo(path string, session *core.SessionRecord) error {
	return saveSessionWithContainerSizeTo(path, session, nil)
}

func saveSessionWithContainerSizeTo(path string, session *core.SessionRecord, containerSize *HostContainerSizeChoice) error {
	if err := validateSessionRecord(session); err != nil {
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
	var existingBundle string
	err = tx.QueryRow("SELECT bundle_id FROM session_contexts WHERE session_id = ?1", session.ID).Scan(&existingBundle)
	switch {
	case err == nil:
		canonical, err := sessionProjectID(tx, session.ID, session.BundleID)
		if err != nil {
			return err
		}
		if existingBundle != canonical {
			return fmt.Errorf("session %s was already associated with bundle %s, not %s", session.ID, existingBundle, session.BundleID)
		}
	case errors.Is(err, sql.ErrNoRows):
	default:
		return err
	}
	merged := *session
	var moving bool
	if err := tx.QueryRow(`SELECT EXISTS(SELECT 1 FROM session_moves WHERE session_id=?1
         AND json_extract(operation_json, '$.phase') IN (
             'capturing','transferring','restoring','destination_starting',
             'destination_owns','source_retired','queue_admitted','dispatch_released',
             'preparing','closing_source','resuming_destination','starting_queue'
         ))`, merged.ID).Scan(&moving); err != nil {
		return err
	}
	if moving {
		// A Move may provision for minutes while clients keep editing drafts
		// and titles. Merge these independently owned fields in this same
		// transaction rather than restoring the lifecycle's earlier copy.
		if err := tx.QueryRow(`SELECT draft_input, session_title_override, acp_session_title, viewed_through_event_ordinal, archived
             FROM sessions WHERE session_id=?1`, merged.ID).
			Scan(&merged.DraftInput, &merged.SessionTitleOverride, &merged.ACPSessionTitle,
				&merged.ViewedThroughEventOrdinal, &merged.Archived); err != nil {
			return err
		}
	}
	if err := insertSession(tx, &merged); err != nil {
		return err
	}
	if containerSize != nil {
		if err := writeHostContainerSize(tx, containerSize.Host, containerSize.Size); err != nil {
			return err
		}
	}
	return tx.Commit()
}

func saveLifecycleSessionTo(path string, session *core.SessionRecord) error {
	if err := validateSessionRecord(session); err != nil {
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
	if err := updateLifecycleFields(tx, session); err != nil {
		return err
	}
	return tx.Commit()
}

// SaveStartupCleanupOutcome settles teardown and archives failed children in
// the same owner transaction.
func SaveStartupCleanupOutcome(session *core.SessionRecord, archive bool) error {
	sessionCopy := *session
	_, err := submitDatabaseWrite("save_startup_cleanup_outcome", func(*Connection) (unitResult, error) {
		if err := validateSessionRecord(&sessionCopy); err != nil {
			return unitResult{}, err
		}
		connection, err := open(DatabasePath())
		if err != nil {
			return unitResult{}, err
		}
		defer connection.Close()
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return unitResult{}, err
		}
		defer tx.Rollback()
		if err := updateLifecycleFields(tx, &sessionCopy); err != nil {
			return unitResult{}, err
		}
		if sessionCopy.State == core.SessionStateProvisioning {
			// A retried child start continues its create.
			if err := restoreCreateIntentForRetry(tx, &sessionCopy); err != nil {
				return unitResult{}, err
			}
		}
		if archive {
			if _, err := tx.Exec("UPDATE sessions SET archived=1 WHERE session_id=?1", sessionCopy.ID); err != nil {
				return unitResult{}, err
			}
			// Only the non-retry cleanup outcome owns final startup failure.
			if err := settleFailedParentMessagesWith(tx, &sessionCopy); err != nil {
				return unitResult{}, err
			}
		}
		return unitResult{}, tx.Commit()
	})
	return err
}

func saveCheckpointedSessionTo(path string, session *core.SessionRecord) error {
	if err := validateSessionRecord(session); err != nil {
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
	if err := updateLifecycleFields(tx, session); err != nil {
		return err
	}
	if _, err := tx.Exec("UPDATE sessions SET native_session_id = ?2 WHERE session_id = ?1", session.ID, session.NativeSessionID); err != nil {
		return err
	}
	if err := replaceCheckpoint(tx, session); err != nil {
		return err
	}
	return tx.Commit()
}

func validateSessionRecord(session *core.SessionRecord) error {
	validation := core.NewStateState()
	validation.Sessions.Insert(session.ID, *session)
	return validation.Validate()
}

// DeleteSession removes one operational session while retaining its
// relational history context. A database trigger removes its prompt history
// with it.
func DeleteSession(sessionID string) error {
	_, err := submitDatabaseWrite("delete_session", func(*Connection) (unitResult, error) {
		return unitResult{}, deleteSessionFrom(DatabasePath(), sessionID)
	})
	return err
}

func deleteSessionFrom(path string, sessionID string) error {
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	if err := core.ValidateID("session", sessionID); err != nil {
		return err
	}
	if _, err := connection.Exec("DELETE FROM sessions WHERE session_id = ?1", sessionID); err != nil {
		return err
	}
	if err := RemoveSessionToolDetails(dataDirFrom(path), sessionID); err != nil {
		slog.Warn("session deleted but its tool detail file could not be removed", "session_id", sessionID, "error", err.Error())
	}
	return nil
}

// SetSessionDraftInput overwrites the unsent chat input carried across a
// detach. Unlike the read receipt this is not monotonic: a draft can shrink,
// and an empty string clears it.
func SetSessionDraftInput(sessionID string, draft string) error {
	_, err := submitDatabaseWrite("set_session_draft_input", func(*Connection) (unitResult, error) {
		return unitResult{}, setSessionDraftInputAt(DatabasePath(), sessionID, draft)
	})
	return err
}

func setSessionDraftInputAt(path string, sessionID string, draft string) error {
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
	if err := updateOneSession(tx, sessionID, "UPDATE sessions SET draft_input = ?2 WHERE session_id = ?1", sessionID, draft); err != nil {
		return err
	}
	return tx.Commit()
}

// AppendSessionDraftInput appends recovered input on the writer lane,
// preserving every preceding durable edit instead of combining with a stale
// daemon snapshot.
func AppendSessionDraftInput(sessionID string, text string) error {
	_, err := submitDatabaseWrite("append_session_draft_input", func(connection *Connection) (unitResult, error) {
		tx, err := connection.Begin()
		if err != nil {
			return unitResult{}, err
		}
		defer tx.Rollback()
		var existing string
		if err := tx.QueryRow("SELECT draft_input FROM sessions WHERE session_id=?1", sessionID).Scan(&existing); err != nil {
			return unitResult{}, err
		}
		merged, err := core.MergeDrafts(existing, text)
		if err != nil {
			return unitResult{}, err
		}
		if err := updateOneSession(tx, sessionID, "UPDATE sessions SET draft_input=?2 WHERE session_id=?1", sessionID, merged); err != nil {
			return unitResult{}, err
		}
		return unitResult{}, tx.Commit()
	})
	return err
}

// ClearSessionDraftInputIfMatches retires a submitted shared draft without
// erasing a newer client's edit.
func ClearSessionDraftInputIfMatches(sessionID string, expected string) error {
	_, err := submitDatabaseWrite("clear_session_draft_input_if_matches", func(connection *Connection) (unitResult, error) {
		_, err := connection.Exec("UPDATE sessions SET draft_input = '' WHERE session_id = ?1 AND draft_input = ?2", sessionID, expected)
		return unitResult{}, err
	})
	return err
}

// RebindSessionBundle re-associates a session with another project bundle.
//
// A session's bundle is otherwise fixed, because prompt history is grouped
// by it. Resume calls this when it converts a session between its raw and
// bundle representations: the project is the same, so its history follows
// it, and only the name Hel files it under changes. Files the session under
// bundleID's project and returns the bundle id the session context now
// holds: the canonical project when the catalog knows bundleID as an alias of
// another bundle. A caller keeping the session record must adopt the
// returned id, because resume publication checks the record's bundle against
// the context.
func RebindSessionBundle(sessionID string, bundleID string) (string, error) {
	return submitDatabaseWrite("rebind_session_bundle", func(*Connection) (string, error) {
		return rebindSessionBundleTo(DatabasePath(), sessionID, bundleID)
	})
}

func rebindSessionBundleTo(path string, sessionID string, bundleID string) (string, error) {
	connection, err := open(path)
	if err != nil {
		return "", err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return "", err
	}
	defer tx.Rollback()
	canonicalBundle, err := canonicalProjectID(tx, bundleID)
	if err != nil {
		return "", err
	}
	if _, err := tx.Exec("INSERT OR IGNORE INTO project_session_aliases(session_id,bundle_id) SELECT session_id,bundle_id FROM session_contexts WHERE session_id=?1", sessionID); err != nil {
		return "", err
	}
	changed, err := changedRows(tx.Exec("UPDATE session_contexts SET bundle_id = ?2 WHERE session_id = ?1", sessionID, canonicalBundle))
	if err != nil {
		return "", err
	}
	if changed == 0 {
		if _, err := tx.Exec(`INSERT INTO session_contexts(session_id, bundle_id, created_at, profile_id)
             VALUES (?1, ?2, ?3, (SELECT last_profile FROM sessions WHERE session_id = ?1))`,
			sessionID, canonicalBundle, core.FormatChronoRFC3339(time.Now())); err != nil {
			return "", err
		}
	}
	if err := tx.Commit(); err != nil {
		return "", err
	}
	return canonicalBundle, nil
}

// DismissRecordedFailure compares and dismisses under the database writer,
// so a concurrent new failure cannot inherit the receipt for the failure the
// person saw.
func DismissRecordedFailure(sessionID string, failure core.FailureIdentity) error {
	encoded, err := databaseMarshalJSON(failure)
	if err != nil {
		return err
	}
	_, err = submitDatabaseWrite("dismiss recorded failure", func(connection *Connection) (unitResult, error) {
		changed, err := changedRows(connection.Exec("UPDATE sessions SET dismissed_failure_json=?2 WHERE session_id=?1 AND state=?3 AND last_error IS ?4 AND failure_generation=?5",
			sessionID, string(encoded), failure.State.AsStr(), failure.LastError, failure.Generation))
		if err != nil {
			return unitResult{}, err
		}
		if changed != 1 {
			return unitResult{}, errors.New("notification is no longer current")
		}
		return unitResult{}, nil
	})
	return err
}

// SessionResumeOptions is what a resume may change about the session it
// restarts. A nil AdditionalMounts or ResourceAllocation is no request.
type SessionResumeOptions struct {
	AdditionalMounts   *[]core.AdditionalMount
	ResourceAllocation core.SessionResourceAllocation
	DiscardQueue       bool
}
