// Port of mj-store/src/database/session_restart.rs.

// Durable intent for one daemon-owned session restart.

package store

import (
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"reflect"
	"strings"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// sessionRestartNow is Utc::now().to_rfc3339().
func sessionRestartNow() string {
	return core.FormatChronoRFC3339(time.Now())
}

// sessionStateDebug is a session state as Rust's Debug writes it: the
// variant name, "DestroyedWithDataLoss" for "destroyed-with-data-loss".
func sessionStateDebug(state core.SessionState) string {
	parts := strings.Split(state.AsStr(), "-")
	for index, part := range parts {
		if part != "" {
			parts[index] = strings.ToUpper(part[:1]) + part[1:]
		}
	}
	return strings.Join(parts, "")
}

// sessionRestartRowsAffected is rusqlite's execute count.
func sessionRestartRowsAffected(result sql.Result, err error) (int64, error) {
	if err != nil {
		return 0, err
	}
	return result.RowsAffected()
}

type SessionRestartPhase int

const (
	SessionRestartPhaseStopping SessionRestartPhase = iota
	SessionRestartPhaseRestoringInPlace
	SessionRestartPhaseRecreatingContainer
	SessionRestartPhaseCancelRequested
	SessionRestartPhaseFallback
)

// String is the Rust Debug name, which error messages show.
func (p SessionRestartPhase) String() string {
	switch p {
	case SessionRestartPhaseStopping:
		return "Stopping"
	case SessionRestartPhaseRestoringInPlace:
		return "RestoringInPlace"
	case SessionRestartPhaseRecreatingContainer:
		return "RecreatingContainer"
	case SessionRestartPhaseCancelRequested:
		return "CancelRequested"
	case SessionRestartPhaseFallback:
		return "Fallback"
	}
	return fmt.Sprintf("SessionRestartPhase(%d)", int(p))
}

// asStr is the phase's stored text.
func (p SessionRestartPhase) asStr() string {
	switch p {
	case SessionRestartPhaseStopping:
		return "stopping"
	case SessionRestartPhaseRestoringInPlace:
		return "restoring_in_place"
	case SessionRestartPhaseRecreatingContainer:
		return "recreating_container"
	case SessionRestartPhaseCancelRequested:
		return "cancel_requested"
	case SessionRestartPhaseFallback:
		return "fallback"
	}
	panic(fmt.Sprintf("unknown session restart phase %d", int(p)))
}

func sessionRestartPhaseParse(value string) (SessionRestartPhase, error) {
	switch value {
	case "stopping":
		return SessionRestartPhaseStopping, nil
	case "restoring_in_place":
		return SessionRestartPhaseRestoringInPlace, nil
	case "recreating_container":
		return SessionRestartPhaseRecreatingContainer, nil
	case "cancel_requested":
		return SessionRestartPhaseCancelRequested, nil
	case "fallback":
		return SessionRestartPhaseFallback, nil
	}
	return 0, fmt.Errorf("unknown session restart phase %q", value)
}

func (p SessionRestartPhase) canFollow(next SessionRestartPhase) bool {
	if p == next || next == SessionRestartPhaseCancelRequested {
		return true
	}
	switch p {
	case SessionRestartPhaseStopping:
		return next == SessionRestartPhaseRestoringInPlace || next == SessionRestartPhaseRecreatingContainer || next == SessionRestartPhaseFallback
	case SessionRestartPhaseRestoringInPlace:
		return next == SessionRestartPhaseFallback
	case SessionRestartPhaseRecreatingContainer:
		return next == SessionRestartPhaseRestoringInPlace
	}
	return false
}

type SessionRestartIntent struct {
	SessionID                       string
	OperationID                     string
	Phase                           SessionRestartPhase
	CheckpointSHA256                *string
	Handoff                         *core.CheckpointMetadata
	CheckpointStarted               bool
	ContainerRecreationAcknowledged bool
	// Automatic is the historical automatic recreation intent. New daemons
	// never create one; recovery retains this marker to reconcile operations
	// accepted by older builds.
	Automatic        bool
	ContainerUpgrade *targets.RetainedContainer
}

// BeginSessionRestart creates one intent, or returns the unfinished intent
// the session already owns.
func BeginSessionRestart(sessionID, operationID string, cancelled *atomic.Bool, containerRecreationAcknowledged bool) (SessionRestartIntent, error) {
	return beginSessionRestartWith(sessionID, operationID, cancelled, nil, nil)
}

func BeginContainerUpgrade(sessionID, operationID string, cancelled *atomic.Bool, plan targets.RetainedContainer, settings *core.ContainerSettingsChange) (SessionRestartIntent, error) {
	return beginSessionRestartWith(sessionID, operationID, cancelled, &plan, settings)
}

func beginSessionRestartWith(sessionID, operationID string, cancelled *atomic.Bool, containerUpgrade *targets.RetainedContainer, settings *core.ContainerSettingsChange) (SessionRestartIntent, error) {
	return submitDatabaseWrite("begin_session_restart", func(connection *Connection) (SessionRestartIntent, error) {
		if cancelled.Load() {
			return SessionRestartIntent{}, errors.New("Restart was cancelled before its durable intent was recorded")
		}
		tx, err := connection.Begin()
		if err != nil {
			return SessionRestartIntent{}, err
		}
		defer tx.Rollback()
		existing, err := loadSessionRestartWith(tx, sessionID)
		if err != nil {
			return SessionRestartIntent{}, err
		}
		if existing != nil {
			if !reflect.DeepEqual(existing.ContainerUpgrade, containerUpgrade) {
				return SessionRestartIntent{}, errors.New("another Restart or container upgrade already owns this session")
			}
			if err := tx.Commit(); err != nil {
				return SessionRestartIntent{}, err
			}
			return *existing, nil
		}
		record, err := loadSessionWith(tx, sessionID)
		if err != nil {
			return SessionRestartIntent{}, err
		}
		if record == nil {
			return SessionRestartIntent{}, fmt.Errorf("unknown session %s", sessionID)
		}
		// A Restart never acquires replacement authority from an image or
		// settings mismatch, including an acknowledgment sent by an old
		// client.
		if cancelled.Load() {
			return SessionRestartIntent{}, errors.New("Restart was cancelled before its durable intent was recorded")
		}
		if settings != nil {
			if err := writeContainerSettings(tx, sessionID, settings.ResourceAllocation, settings.Mounts, sessionRestartNow()); err != nil {
				return SessionRestartIntent{}, err
			}
		}
		var upgradeJSON *string
		if containerUpgrade != nil {
			encoded, err := databaseMarshalJSON(containerUpgrade)
			if err != nil {
				return SessionRestartIntent{}, err
			}
			text := string(encoded)
			upgradeJSON = &text
		}
		if _, err := tx.Exec(`INSERT INTO session_restart_intents(session_id, operation_id, phase, updated_at, container_upgrade_json)
             VALUES (?1, ?2, 'stopping', ?3, ?4)`, sessionID, operationID, sessionRestartNow(), upgradeJSON); err != nil {
			return SessionRestartIntent{}, err
		}
		if err := tx.Commit(); err != nil {
			return SessionRestartIntent{}, err
		}
		return SessionRestartIntent{
			SessionID:        sessionID,
			OperationID:      operationID,
			Phase:            SessionRestartPhaseStopping,
			ContainerUpgrade: containerUpgrade,
		}, nil
	})
}

// LoadRestartAndSession reads a session's restart intent and its record from
// one snapshot; ok is false when the session has no intent.
func LoadRestartAndSession(sessionID string) (intent SessionRestartIntent, session core.SessionRecord, ok bool, err error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return SessionRestartIntent{}, core.SessionRecord{}, false, err
	}
	defer connection.Close()
	snapshot, err := connection.Begin()
	if err != nil {
		return SessionRestartIntent{}, core.SessionRecord{}, false, err
	}
	defer snapshot.Rollback()
	loaded, err := loadSessionRestartWith(snapshot, sessionID)
	if err != nil || loaded == nil {
		return SessionRestartIntent{}, core.SessionRecord{}, false, err
	}
	record, err := loadSessionWith(snapshot, sessionID)
	if err != nil {
		return SessionRestartIntent{}, core.SessionRecord{}, false, err
	}
	if record == nil {
		return SessionRestartIntent{}, core.SessionRecord{}, false, errors.New("Restart session is missing")
	}
	return *loaded, *record, true, nil
}

// sessionRestartScan reads one session_restart_intents row whose columns
// follow the session ID: operation_id, phase, checkpoint_sha256,
// checkpoint_started, container_recreation_acknowledged, automatic,
// container_upgrade_json, handoff_json.
func sessionRestartScan(scan func(dest ...any) error, sessionID *string) (SessionRestartIntent, error) {
	var intent SessionRestartIntent
	var phase string
	var upgradeJSON, handoffJSON *string
	destinations := []any{&intent.OperationID, &phase, &intent.CheckpointSHA256, &intent.CheckpointStarted,
		&intent.ContainerRecreationAcknowledged, &intent.Automatic, &upgradeJSON, &handoffJSON}
	if sessionID == nil {
		destinations = append([]any{&intent.SessionID}, destinations...)
	} else {
		intent.SessionID = *sessionID
	}
	if err := scan(destinations...); err != nil {
		return SessionRestartIntent{}, err
	}
	var err error
	if intent.Phase, err = sessionRestartPhaseParse(phase); err != nil {
		return SessionRestartIntent{}, err
	}
	if handoffJSON != nil {
		var handoff core.CheckpointMetadata
		if err := json.Unmarshal([]byte(*handoffJSON), &handoff); err != nil {
			return SessionRestartIntent{}, err
		}
		intent.Handoff = &handoff
	}
	if upgradeJSON != nil {
		var upgrade targets.RetainedContainer
		if err := json.Unmarshal([]byte(*upgradeJSON), &upgrade); err != nil {
			return SessionRestartIntent{}, err
		}
		intent.ContainerUpgrade = &upgrade
	}
	return intent, nil
}

func LoadSessionRestarts() ([]SessionRestartIntent, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	type stored struct {
		intent SessionRestartIntent
		err    error
	}
	rows, err := valuesQueryMap(connection, `SELECT session_id, operation_id, phase, checkpoint_sha256, checkpoint_started, container_recreation_acknowledged, automatic, container_upgrade_json, handoff_json
         FROM session_restart_intents ORDER BY session_id`, nil, func(rows *sql.Rows) (stored, error) {
		// A row that does not decode fails the load after the rows are read,
		// as Rust's map over the rows does.
		intent, err := sessionRestartScan(rows.Scan, nil)
		return stored{intent: intent, err: err}, nil
	})
	if err != nil {
		return nil, err
	}
	intents := make([]SessionRestartIntent, 0, len(rows))
	for _, row := range rows {
		if row.err != nil {
			return nil, row.err
		}
		intents = append(intents, row.intent)
	}
	return intents, nil
}

func RecordSessionRestartCheckpoint(sessionID, operationID, checkpointSHA256 string) error {
	_, err := submitDatabaseWrite("record_session_restart_checkpoint", func(connection *Connection) (struct{}, error) {
		changed, err := sessionRestartRowsAffected(connection.Exec(`UPDATE session_restart_intents SET checkpoint_sha256=?3, updated_at=?4
             WHERE session_id=?1 AND operation_id=?2`, sessionID, operationID, checkpointSHA256, sessionRestartNow()))
		if err != nil {
			return struct{}{}, err
		}
		if changed != 1 {
			return struct{}{}, errors.New("session restart ownership changed before checkpoint was recorded")
		}
		return struct{}{}, nil
	})
	return err
}

func MarkSessionRestartCheckpointStarted(sessionID, operationID string) error {
	_, err := submitDatabaseWrite("mark_session_restart_checkpoint_started", func(connection *Connection) (struct{}, error) {
		changed, err := sessionRestartRowsAffected(connection.Exec(`UPDATE session_restart_intents SET checkpoint_started=1, updated_at=?3
             WHERE session_id=?1 AND operation_id=?2`, sessionID, operationID, sessionRestartNow()))
		if err != nil {
			return struct{}{}, err
		}
		if changed != 1 {
			return struct{}{}, errors.New("session restart ownership changed before checkpointing")
		}
		return struct{}{}, nil
	})
	return err
}

func AdvanceSessionRestart(sessionID, operationID string, phase SessionRestartPhase) error {
	_, err := submitDatabaseWrite("advance_session_restart", func(connection *Connection) (struct{}, error) {
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		current, err := loadSessionRestartWith(tx, sessionID)
		if err != nil {
			return struct{}{}, err
		}
		if current == nil {
			return struct{}{}, errors.New("session restart intent disappeared")
		}
		if current.OperationID != operationID {
			return struct{}{}, errors.New("session restart ownership changed")
		}
		if !current.Phase.canFollow(phase) {
			return struct{}{}, fmt.Errorf("invalid session restart phase transition from %s to %s", current.Phase, phase)
		}
		if phase == SessionRestartPhaseRecreatingContainer {
			if current.ContainerUpgrade != nil {
				return struct{}{}, errors.New("explicit replacement requires atomic idle and child admission")
			}
			record, err := loadSessionWith(tx, sessionID)
			if err != nil {
				return struct{}{}, err
			}
			if record == nil {
				return struct{}{}, errors.New("session disappeared before Restart recreation")
			}
			sealed := current.ContainerRecreationAcknowledged && current.CheckpointStarted &&
				current.CheckpointSHA256 != nil && record.Checkpoint != nil &&
				record.Checkpoint.Sha256 == *current.CheckpointSHA256
			if !sealed {
				return struct{}{}, errors.New("Restart recreation requires its own sealed checkpoint")
			}
			switch record.State {
			case core.SessionStateClosing, core.SessionStateDestroying, core.SessionStateError,
				core.SessionStateStopped, core.SessionStateProvisioning:
			default:
				return struct{}{}, errors.New("Restart recreation cannot remove an unsealed live session")
			}
		}
		if _, err := tx.Exec(`UPDATE session_restart_intents SET phase=?3, updated_at=?4
             WHERE session_id=?1 AND operation_id=?2`, sessionID, operationID, phase.asStr(), sessionRestartNow()); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

// RequestSessionRestartCancel retains the settings lock while cancellation
// unwinds, but never recovers accepted cancellation as another Restart after
// a daemon crash.
func RequestSessionRestartCancel(sessionID, operationID string) error {
	_, err := submitDatabaseWrite("request_session_restart_cancel", func(connection *Connection) (struct{}, error) {
		current, err := loadSessionRestartWith(connection, sessionID)
		if err != nil {
			return struct{}{}, err
		}
		if current != nil && current.OperationID == operationID && current.ContainerUpgrade != nil &&
			current.Phase == SessionRestartPhaseRecreatingContainer {
			return struct{}{}, errors.New("container replacement already started and cannot be cancelled")
		}
		_, err = connection.Exec("UPDATE session_restart_intents SET phase='cancel_requested', updated_at=?3 WHERE session_id=?1 AND operation_id=?2",
			sessionID, operationID, sessionRestartNow())
		return struct{}{}, err
	})
	return err
}

// RecoverCancelledSessionRestart keeps a cancelled Restart from becoming an
// ordinary pending suspension on startup: it retains its environment and
// settles the recovery row atomically.
func RecoverCancelledSessionRestart(sessionID, operationID string) error {
	_, err := submitDatabaseWrite("recover_cancelled_session_restart", func(connection *Connection) (struct{}, error) {
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		current, err := loadSessionRestartWith(tx, sessionID)
		if err != nil {
			return struct{}{}, err
		}
		if current != nil && current.OperationID == operationID && current.Phase == SessionRestartPhaseCancelRequested {
			record, err := loadSessionWith(tx, sessionID)
			if err != nil {
				return struct{}{}, err
			}
			if record != nil && record.State == core.SessionStateClosing {
				record.State = core.SessionStateError
				message := "Restart was cancelled; the environment was retained. Restart or suspend to continue."
				record.LastError = &message
				record.UpdatedAt = sessionRestartNow()
				if err := updateLifecycleFields(tx, record); err != nil {
					return struct{}{}, err
				}
			}
			if _, err := tx.Exec("DELETE FROM session_restart_intents WHERE session_id=?1 AND operation_id=?2", sessionID, operationID); err != nil {
				return struct{}{}, err
			}
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

func FinishSessionRestart(sessionID, operationID string) error {
	_, err := submitDatabaseWrite("finish_session_restart", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("DELETE FROM session_restart_intents WHERE session_id=?1 AND operation_id=?2", sessionID, operationID)
		return struct{}{}, err
	})
	return err
}

func CancelSessionRestart(sessionID string) error {
	_, err := submitDatabaseWrite("cancel_session_restart", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("DELETE FROM session_restart_intents WHERE session_id=?1", sessionID)
		return struct{}{}, err
	})
	return err
}

func loadSessionRestartWith(conn Queryer, sessionID string) (*SessionRestartIntent, error) {
	row := conn.QueryRow(`SELECT operation_id, phase, checkpoint_sha256, checkpoint_started, container_recreation_acknowledged, automatic, container_upgrade_json, handoff_json
         FROM session_restart_intents WHERE session_id=?1`, sessionID)
	intent, err := sessionRestartScan(row.Scan, &sessionID)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	return &intent, nil
}

// PrepareContainerUpgrade persists the immutable prepared image before
// stopping the source.
func PrepareContainerUpgrade(sessionID, operationID string, plan targets.RetainedContainer) error {
	_, err := submitDatabaseWrite("prepare_container_upgrade", func(connection *Connection) (struct{}, error) {
		encoded, err := databaseMarshalJSON(plan)
		if err != nil {
			return struct{}{}, err
		}
		changed, err := sessionRestartRowsAffected(connection.Exec("UPDATE session_restart_intents SET container_upgrade_json=?3 WHERE session_id=?1 AND operation_id=?2 AND phase IN ('stopping','recreating_container') AND container_upgrade_json IS NOT NULL",
			sessionID, operationID, string(encoded)))
		if err != nil {
			return struct{}{}, err
		}
		if changed != 1 {
			return struct{}{}, errors.New("container upgrade ownership changed during preparation")
		}
		return struct{}{}, nil
	})
	return err
}

// ensureContainerParentAvailable refuses while member's environment is being
// replaced. Child startup and replacement share the database writer's
// decision. A parked child's stale target cannot be woken after replacement
// commits.
func ensureContainerParentAvailable(conn Queryer, member string) error {
	peers, err := environmentPeers(conn, member)
	if err != nil {
		return err
	}
	for _, peer := range peers {
		if peer.upgradePhase != nil && *peer.upgradePhase == "recreating_container" {
			return errors.New("the parent container is being upgraded; retry after it reconnects")
		}
	}
	return nil
}

// ensureNoPeerContainerUpgrade refuses while another member's replacement
// owns the environment. An environment replacement may boot its own worker
// before releasing its intent. Other members of the environment must wait
// for that release.
func ensureNoPeerContainerUpgrade(conn Queryer, sessionID string) error {
	peers, err := environmentPeers(conn, sessionID)
	if err != nil {
		return err
	}
	for _, peer := range peers {
		if peer.id != sessionID && peer.upgradePhase != nil {
			return errors.New("the container is being upgraded; retry after it reconnects")
		}
	}
	return nil
}

type environmentPeer struct {
	id               string
	state            core.SessionState
	hasTarget        bool
	restartingWorker bool
	kind             *string
	upgradePhase     *string
}

// BoundEnvironmentMemberIDs is the operational sessions currently bound to
// the same environment as the member.
func BoundEnvironmentMemberIDs(member string) ([]string, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	peers, err := environmentPeers(connection, member)
	if err != nil {
		return nil, err
	}
	ids := make([]string, 0, len(peers))
	for _, peer := range peers {
		ids = append(ids, peer.id)
	}
	return ids, nil
}

func environmentPeers(conn Queryer, member string) ([]environmentPeer, error) {
	return valuesQueryMap(conn, `SELECT s.session_id, s.state, t.session_id IS NOT NULL,
                json_extract(c.record_json, '$.kind'), r.phase, w.session_id IS NOT NULL
         FROM environment_bindings b
         JOIN environment_bindings p ON p.environment_id = b.environment_id
         JOIN sessions s ON s.session_id = p.session_id
         LEFT JOIN session_targets t ON t.session_id = p.session_id
         LEFT JOIN subagent_sessions c ON c.child_session_id = p.session_id
         LEFT JOIN session_restart_intents r
             ON r.session_id = p.session_id AND r.container_upgrade_json IS NOT NULL
         LEFT JOIN worker_restart_intents w ON w.session_id = p.session_id
         WHERE b.session_id = ?1
         ORDER BY p.session_id`, []any{member}, func(rows *sql.Rows) (environmentPeer, error) {
		var peer environmentPeer
		var state string
		if err := rows.Scan(&peer.id, &state, &peer.hasTarget, &peer.kind, &peer.upgradePhase, &peer.restartingWorker); err != nil {
			return environmentPeer{}, err
		}
		peer.state = storedSessionState(state)
		return peer, nil
	})
}

type ContainerUpgradeCommit interface {
	isContainerUpgradeCommit()
	IsCommitted() bool
}

type ContainerUpgradeCommitCommitted struct{}

// ContainerUpgradeCommitWaiting is the descriptions captured by the same
// transaction that refused replacement.
type ContainerUpgradeCommitWaiting struct{ Value []string }

func (ContainerUpgradeCommitCommitted) isContainerUpgradeCommit() {}
func (ContainerUpgradeCommitWaiting) isContainerUpgradeCommit()   {}

func (ContainerUpgradeCommitCommitted) IsCommitted() bool { return true }
func (ContainerUpgradeCommitWaiting) IsCommitted() bool   { return false }

func TryCommitContainerUpgrade(sessionID, operationID string) (ContainerUpgradeCommit, error) {
	return submitDatabaseWrite("commit container upgrade", func(connection *Connection) (ContainerUpgradeCommit, error) {
		tx, err := connection.Begin()
		if err != nil {
			return nil, err
		}
		defer tx.Rollback()
		intent, err := loadSessionRestartWith(tx, sessionID)
		if err != nil {
			return nil, err
		}
		if intent == nil {
			return nil, errors.New("container upgrade intent disappeared")
		}
		if intent.OperationID != operationID || intent.ContainerUpgrade == nil ||
			(intent.Phase != SessionRestartPhaseStopping && intent.Phase != SessionRestartPhaseRecreatingContainer) {
			return nil, errors.New("container upgrade ownership changed or cancellation was accepted")
		}
		peers, err := environmentPeers(tx, sessionID)
		if err != nil {
			return nil, err
		}
		blockers := []string{}
		pendingDelegation := false
		for _, peer := range peers {
			if peer.id != sessionID && (peer.restartingWorker ||
				(peer.state != core.SessionStateParked && (peer.state.HasLiveWorker() || peer.hasTarget))) {
				kind := "child"
				if peer.kind != nil {
					kind = *peer.kind
				}
				blockers = append(blockers, fmt.Sprintf("%s session %s (%s)", kind, peer.id, sessionStateDebug(peer.state)))
			}
			pending, err := hasPendingMutatingDelegationsWith(tx, peer.id)
			if err != nil {
				return nil, err
			}
			pendingDelegation = pendingDelegation || pending
		}
		if pendingDelegation {
			blockers = append(blockers, "accepted delegation requests")
		}
		if len(blockers) > 0 {
			return ContainerUpgradeCommitWaiting{Value: blockers}, nil
		}
		if _, err := tx.Exec("UPDATE session_restart_intents SET phase='recreating_container', updated_at=?3 WHERE session_id=?1 AND operation_id=?2",
			sessionID, operationID, sessionRestartNow()); err != nil {
			return nil, err
		}
		if err := tx.Commit(); err != nil {
			return nil, err
		}
		return ContainerUpgradeCommitCommitted{}, nil
	})
}

// ContainerUpgradeOwnsTarget is whether a retained-container intent owns the
// session's environment. The intent is the sole authority over its
// environment, including after an engine failure or daemon replacement.
func ContainerUpgradeOwnsTarget(sessionID string) (bool, error) {
	reader, err := openReader(DatabasePath())
	if err != nil {
		return false, err
	}
	defer reader.Close()
	return containerUpgradeOwnsTargetWith(reader, sessionID)
}

func containerUpgradeOwnsTargetWith(conn Queryer, sessionID string) (bool, error) {
	peers, err := environmentPeers(conn, sessionID)
	if err != nil {
		return false, err
	}
	for _, peer := range peers {
		if peer.upgradePhase != nil {
			return true, nil
		}
	}
	return false, nil
}

// RecordSessionRestartHandoff records the handoff the Restart owns before the
// relay can be sealed. Its digest and archive reference change in one
// database-writer decision.
func RecordSessionRestartHandoff(sessionID, operationID string, handoff *core.CheckpointMetadata) error {
	_, err := submitDatabaseWrite("record_session_restart_handoff", func(connection *Connection) (struct{}, error) {
		encoded, err := databaseMarshalJSON(handoff)
		if err != nil {
			return struct{}{}, err
		}
		changed, err := sessionRestartRowsAffected(connection.Exec("UPDATE session_restart_intents SET handoff_json=?3, checkpoint_sha256=?4, updated_at=?5 WHERE session_id=?1 AND operation_id=?2",
			sessionID, operationID, string(encoded), handoff.Sha256, sessionRestartNow()))
		if err != nil {
			return struct{}{}, err
		}
		if changed != 1 {
			return struct{}{}, errors.New("Restart ownership changed before its handoff was recorded")
		}
		return struct{}{}, nil
	})
	return err
}

func LoadSessionRestart(sessionID string) (*SessionRestartIntent, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return loadSessionRestartWith(connection, sessionID)
}
