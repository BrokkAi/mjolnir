// Port of mj-store/src/database/worker_restart.rs.

// A detached worker boot remains recoverable after the daemon releases
// admission.
//
// The functions that require the session's worker permit take the context
// carrying it (see worker_lifecycle.go).

package store

import (
	"context"
	"fmt"

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
	panic("unported: mj-store/src/database/worker_restart.rs begin_worker_restart_for")
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
	panic("unported: mj-store/src/database/worker_restart.rs worker_restart_phase")
}

func AdvanceWorkerRestart(ctx context.Context, sessionID, operationID string, phase WorkerRestartPhase) error {
	panic("unported: mj-store/src/database/worker_restart.rs advance_worker_restart")
}

func LoadWorkerRestart(sessionID string) (*WorkerRestartIntent, error) {
	panic("unported: mj-store/src/database/worker_restart.rs load_worker_restart")
}

func loadWorkerRestartWith(conn Queryer, sessionID string) (*WorkerRestartIntent, error) {
	panic("unported: mj-store/src/database/worker_restart.rs load_worker_restart_with")
}

func FinishWorkerRestart(ctx context.Context, sessionID, operationID string) error {
	panic("unported: mj-store/src/database/worker_restart.rs finish_worker_restart")
}

func finishWorkerRestartWith(conn Queryer, sessionID, operationID string) error {
	panic("unported: mj-store/src/database/worker_restart.rs finish_worker_restart_with")
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
	panic("unported: mj-store/src/database/worker_restart.rs fail_subagent_unpark")
}
