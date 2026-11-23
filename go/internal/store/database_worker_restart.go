// Port of mj-store/src/database/worker_restart.rs.

// A detached worker boot remains recoverable after the daemon releases
// admission.
//
// The functions that require the session's worker permit take the context
// carrying it (see worker_lifecycle.go).

package store

import (
	"context"
	"errors"
	"fmt"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type WorkerRestartIntent struct {
	OperationID  string
	Target       core.TargetLocator
	DesiredBuild string
}

func BeginWorkerRestart(ctx context.Context, sessionID string, intent *WorkerRestartIntent) error {
	return beginWorkerRestartFor(ctx, sessionID, intent, false, nil)
}

// BeginSubagentUnpark makes the wake and its boot owner visible together.
// Recovery must never see a still-parked child whose worker has already
// accepted launch. profileChange is the (previous, next) profile, when the
// wake changes it.
func BeginSubagentUnpark(ctx context.Context, sessionID string, intent *WorkerRestartIntent, profileChange *[2]string) error {
	return beginWorkerRestartFor(ctx, sessionID, intent, true, profileChange)
}

func beginWorkerRestartFor(ctx context.Context, sessionID string, intent *WorkerRestartIntent, unpark bool, profileChange *[2]string) error {
	if _, err := Require(ctx, sessionID); err != nil {
		return err
	}
	_, err := submitDatabaseWrite("begin_worker_restart", func(connection *Connection) (struct{}, error) {
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		record, err := loadSessionWith(tx, sessionID)
		if err != nil {
			return struct{}{}, err
		}
		if record == nil || record.Target == nil || !workerLifecycleSameTarget(record.Target, intent.Target) {
			return struct{}{}, errors.New("worker restart target changed before admission")
		}
		if unpark {
			if err := ensureContainerParentAvailable(tx, sessionID); err != nil {
				return struct{}{}, err
			}
		} else if err := ensureNoPeerContainerUpgrade(tx, sessionID); err != nil {
			return struct{}{}, err
		}
		target, err := databaseMarshalJSON(intent.Target)
		if err != nil {
			return struct{}{}, err
		}
		result, err := tx.Exec(`INSERT INTO worker_restart_intents(session_id,operation_id,target_json,desired_build,phase)
             VALUES (?1,?2,?3,?4,'prepared') ON CONFLICT(session_id) DO NOTHING`,
			sessionID, intent.OperationID, string(target), intent.DesiredBuild)
		if err != nil {
			return struct{}{}, err
		}
		if claimed, err := result.RowsAffected(); err != nil {
			return struct{}{}, err
		} else if claimed != 1 {
			return struct{}{}, fmt.Errorf("another worker replacement already owns session %s", sessionID)
		}
		if unpark {
			var previous, next *string
			if profileChange != nil {
				previous, next = &profileChange[0], &profileChange[1]
			}
			result, err := tx.Exec(`UPDATE sessions SET state='running', last_error=NULL, updated_at=?2,
                         last_profile=COALESCE(?3, last_profile)
                     WHERE session_id=?1 AND state='parked'
                       AND (?4 IS NULL OR last_profile=?4)
                       AND EXISTS (SELECT 1 FROM subagent_sessions WHERE child_session_id=?1)`,
				sessionID, core.FormatChronoRFC3339(time.Now()), next, previous)
			if err != nil {
				return struct{}{}, err
			}
			if woken, err := result.RowsAffected(); err != nil {
				return struct{}{}, err
			} else if woken != 1 {
				return struct{}{}, errors.New("only a parked sub-agent can accept a wake")
			}
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

type WorkerRestartPhase int

const (
	WorkerRestartPhasePrepared WorkerRestartPhase = iota
	WorkerRestartPhaseSwapping
	WorkerRestartPhaseAwaitingReadiness
)

// String is the Rust Debug name.
func (p WorkerRestartPhase) String() string {
	switch p {
	case WorkerRestartPhasePrepared:
		return "Prepared"
	case WorkerRestartPhaseSwapping:
		return "Swapping"
	case WorkerRestartPhaseAwaitingReadiness:
		return "AwaitingReadiness"
	}
	return fmt.Sprintf("WorkerRestartPhase(%d)", int(p))
}

// transition is the stored phase this phase follows and its own stored
// phase.
func (p WorkerRestartPhase) transition() (previous, next string) {
	switch p {
	case WorkerRestartPhasePrepared:
		return "prepared", "prepared"
	case WorkerRestartPhaseSwapping:
		return "prepared", "swapping"
	case WorkerRestartPhaseAwaitingReadiness:
		return "swapping", "awaiting_readiness"
	}
	panic(fmt.Sprintf("unknown worker replacement phase %d", int(p)))
}

// WorkerRestartPhaseOf is the session's worker replacement phase; nil when
// it has none (Rust's worker_restart_phase, named apart from the type).
func WorkerRestartPhaseOf(sessionID string) (*WorkerRestartPhase, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	var stored string
	found, err := valuesOptional(connection.QueryRow("SELECT phase FROM worker_restart_intents WHERE session_id=?1", sessionID).Scan(&stored))
	if err != nil || !found {
		return nil, err
	}
	var phase WorkerRestartPhase
	switch stored {
	case "prepared":
		phase = WorkerRestartPhasePrepared
	case "swapping":
		phase = WorkerRestartPhaseSwapping
	case "awaiting_readiness":
		phase = WorkerRestartPhaseAwaitingReadiness
	default:
		return nil, fmt.Errorf("unknown worker replacement phase %s", stored)
	}
	return &phase, nil
}

func AdvanceWorkerRestart(ctx context.Context, sessionID, operationID string, phase WorkerRestartPhase) error {
	if _, err := Require(ctx, sessionID); err != nil {
		return err
	}
	_, err := submitDatabaseWrite("advance_worker_restart", func(connection *Connection) (struct{}, error) {
		previous, next := phase.transition()
		result, err := connection.Exec("UPDATE worker_restart_intents SET phase=?3 WHERE session_id=?1 AND operation_id=?2 AND phase=?4",
			sessionID, operationID, next, previous)
		if err != nil {
			return struct{}{}, err
		}
		changed, err := result.RowsAffected()
		if err != nil {
			return struct{}{}, err
		}
		if changed != 1 {
			return struct{}{}, errors.New("worker restart ownership changed")
		}
		return struct{}{}, nil
	})
	return err
}

func LoadWorkerRestart(sessionID string) (*WorkerRestartIntent, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return loadWorkerRestartWith(connection, sessionID)
}

func loadWorkerRestartWith(conn Queryer, sessionID string) (*WorkerRestartIntent, error) {
	var intent WorkerRestartIntent
	var target string
	found, err := valuesOptional(conn.QueryRow("SELECT operation_id,target_json,desired_build FROM worker_restart_intents WHERE session_id=?1", sessionID).
		Scan(&intent.OperationID, &target, &intent.DesiredBuild))
	if err != nil || !found {
		return nil, err
	}
	intent.Target, err = core.UnmarshalTargetLocator([]byte(target))
	if err != nil {
		return nil, err
	}
	return &intent, nil
}

func FinishWorkerRestart(ctx context.Context, sessionID, operationID string) error {
	if _, err := Require(ctx, sessionID); err != nil {
		return err
	}
	_, err := submitDatabaseWrite("finish_worker_restart", func(connection *Connection) (struct{}, error) {
		return struct{}{}, finishWorkerRestartWith(connection, sessionID, operationID)
	})
	return err
}

func finishWorkerRestartWith(conn Queryer, sessionID, operationID string) error {
	_, err := conn.Exec("DELETE FROM worker_restart_intents WHERE session_id=?1 AND operation_id=?2", sessionID, operationID)
	return err
}

// SubagentUnparkFailure is why a sub-agent wake failed.
type SubagentUnparkFailure interface {
	isSubagentUnparkFailure()
}

type SubagentUnparkFailureRetryable struct {
	Detail   string
	Evidence *core.WakeFailure
}

type SubagentUnparkFailureWorkingDirectoryUnavailable struct{ Value string }

func (SubagentUnparkFailureRetryable) isSubagentUnparkFailure()                   {}
func (SubagentUnparkFailureWorkingDirectoryUnavailable) isSubagentUnparkFailure() {}

// FailSubagentUnpark settles a failed wake and its boot intent in one
// transaction.
func FailSubagentUnpark(ctx context.Context, sessionID, operationID string, failure SubagentUnparkFailure) error {
	owner, err := Require(ctx, sessionID)
	if err != nil {
		return err
	}
	if owner.OperationID() != operationID {
		return errors.New("unpark ownership changed")
	}
	var state core.SessionState
	var detail string
	switch failure := failure.(type) {
	case SubagentUnparkFailureRetryable:
		state = core.SessionStateParked
		if failure.Evidence != nil {
			detail, err = failure.Evidence.Encode(failure.Detail)
			if err != nil {
				return err
			}
		} else {
			detail = failure.Detail
		}
	case SubagentUnparkFailureWorkingDirectoryUnavailable:
		state, detail = core.SessionStateError, failure.Value
	}
	_, err = submitDatabaseWrite("fail_subagent_unpark", func(connection *Connection) (struct{}, error) {
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		result, err := tx.Exec(`UPDATE sessions SET state=?5, last_error=?3, updated_at=?4
             WHERE session_id=?1 AND state='running'
             AND EXISTS (SELECT 1 FROM worker_restart_intents WHERE session_id=?1 AND operation_id=?2)
             AND EXISTS (SELECT 1 FROM subagent_sessions WHERE child_session_id=?1)`,
			sessionID, operationID, detail, core.FormatChronoRFC3339(time.Now()), state.AsStr())
		if err != nil {
			return struct{}{}, err
		}
		if settled, err := result.RowsAffected(); err != nil {
			return struct{}{}, err
		} else if settled != 1 {
			return struct{}{}, errors.New("failed wake no longer owns the running child")
		}
		if err := finishWorkerRestartWith(tx, sessionID, operationID); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return err
}
