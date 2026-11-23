// Port of mj-store/src/worker_lifecycle.rs.

// Worker mutation capabilities derived from RecoveryGate admissions. Nested
// helpers borrow the operation's permit and never decide admission.
//
// Rust keeps the current permit in a tokio task-local (async work) and a
// thread-local (blocking work). Go has neither, so the permit travels in a
// context.Context: Scope and ScopeBlocking return a context that carries it,
// and every function that asks for the current owner takes that context.

package store

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"reflect"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// workerLifecycleSameTarget is TargetLocator's derived equality.
func workerLifecycleSameTarget(left, right core.TargetLocator) bool {
	return reflect.DeepEqual(left, right)
}

// WorkerPermit is a worker mutation capability derived from the session
// owner's admission. Blocking work retains it even when its async waiter
// disappears.
//
// Rust shares one Arc between clones and releases the admission when the
// last clone drops. Go has no Drop, so a permit is a handle: Clone makes
// another, Release gives one up, and the admission ends with the last one.
// Whoever is handed a permit from WorkerPermitAcquire or Clone releases it;
// Current, Require and Capture only borrow the one a scope carries.
type WorkerPermit struct {
	admission *workerAdmission
	released  atomic.Bool
}

type workerAdmission struct {
	admission   core.SessionAdmission
	sessionID   string
	operationID string
	handles     atomic.Int64
}

// Clone is another handle on the same permit.
func (p *WorkerPermit) Clone() *WorkerPermit {
	p.admission.handles.Add(1)
	return &WorkerPermit{admission: p.admission}
}

// Release gives this handle up; the admission ends with the last handle.
// Releasing a handle twice does nothing.
func (p *WorkerPermit) Release() {
	if p == nil || !p.released.CompareAndSwap(false, true) {
		return
	}
	if p.admission.handles.Add(-1) == 0 {
		p.admission.admission.Release()
	}
}

// asyncOwnerKey and blockingOwnerKey are the context keys standing in for
// Rust's ASYNC_OWNER task-local and BLOCKING_OWNER thread-local.
type asyncOwnerKey struct{}

type blockingOwnerKey struct{}

func asyncOwner(ctx context.Context) *WorkerPermit {
	permit, _ := ctx.Value(asyncOwnerKey{}).(*WorkerPermit)
	return permit
}

func blockingOwner(ctx context.Context) *WorkerPermit {
	permit, _ := ctx.Value(blockingOwnerKey{}).(*WorkerPermit)
	return permit
}

// Capture is the permit ctx carries, async scope first; nil when none.
func Capture(ctx context.Context) *WorkerPermit {
	if permit := asyncOwner(ctx); permit != nil {
		return permit
	}
	return blockingOwner(ctx)
}

// Current is the permit ctx carries for sessionID; nil when none.
func Current(ctx context.Context, sessionID string) *WorkerPermit {
	if permit := asyncOwner(ctx); permit != nil && permit.SessionID() == sessionID {
		return permit
	}
	if permit := blockingOwner(ctx); permit != nil && permit.SessionID() == sessionID {
		return permit
	}
	return nil
}

func Require(ctx context.Context, sessionID string) (*WorkerPermit, error) {
	if permit := Current(ctx, sessionID); permit != nil {
		return permit, nil
	}
	return nil, fmt.Errorf("worker mutation for %s has no lifecycle owner", sessionID)
}

// ObserveWorkerRestartOutcome observes a completed boot while holding
// ownership through the snapshot read. A live swap retains its owner; an
// abandoned failed boot can be retried.
func ObserveWorkerRestartOutcome(owner *WorkerPermit, target core.TargetLocator, snapshot *core.ManagedSessionSnapshot) error {
	sessionID := owner.SessionID()
	ctx := owner.ScopeBlocking(context.Background())
	intent, err := LoadWorkerRestart(sessionID)
	if err != nil {
		return err
	}
	if intent == nil || !workerLifecycleSameTarget(intent.Target, target) {
		return nil
	}
	operational := snapshot.Operational.Get()
	if operational == nil {
		operational = &core.RelayOperationalState{}
	}
	failed := operational.HarnessPreparationFailed()
	ready := operational.NativeSessionIsReady() || operational.CheckpointOnly ||
		operational.Execution == core.RelayExecutionStateClosed
	if !ready && !failed {
		return nil
	}
	if failed {
		slog.Warn("replacement worker failed harness preparation; releasing failed intent",
			"session_id", sessionID, "preparation", fmt.Sprintf("%#v", operational.HarnessPreparation))
	}
	phase, err := WorkerRestartPhaseOf(sessionID)
	if err != nil {
		return err
	}
	prepared := phase != nil && *phase == WorkerRestartPhasePrepared
	if !failed && !prepared && intent.DesiredBuild != "" &&
		(snapshot.WorkerBuild == nil || *snapshot.WorkerBuild != intent.DesiredBuild) {
		// Handoff can interrupt a swap before the old process stops. A ready
		// old worker is an aborted swap, not a pending boot to kill.
		actual := "None"
		if snapshot.WorkerBuild != nil {
			actual = "Some(" + core.RustDebugString(*snapshot.WorkerBuild) + ")"
		}
		slog.Warn("ready worker did not reach replacement build; releasing failed intent",
			"session_id", sessionID, "expected", intent.DesiredBuild, "actual", actual)
	}
	return FinishWorkerRestart(ctx, sessionID, intent.OperationID)
}

func (p *WorkerPermit) VerifyCachedTarget(state *core.StateState) error {
	expected, ok := state.Sessions.Get(p.SessionID())
	if !ok {
		return nil
	}
	current, err := ReadDurableSessionRecord(p.SessionID())
	if err != nil {
		return err
	}
	if current != nil && !workerLifecycleSameTarget(current.Target, expected.Target) {
		return errors.New("session target changed while waiting for worker lifecycle ownership")
	}
	return nil
}

func (p *WorkerPermit) VerifyTarget(expected core.TargetLocator) error {
	current, err := ReadDurableSessionRecord(p.SessionID())
	if err != nil {
		return err
	}
	if current == nil || current.Target == nil || !workerLifecycleSameTarget(current.Target, expected) {
		return errors.New("worker target changed before lifecycle ownership")
	}
	return nil
}

// BeginRestart records a worker replacement on target. ctx must carry this
// permit's scope, as Rust's call runs inside it.
func (p *WorkerPermit) BeginRestart(ctx context.Context, target core.TargetLocator, desiredBuild string) error {
	if err := p.VerifyTarget(target); err != nil {
		return err
	}
	return BeginWorkerRestart(ctx, p.SessionID(), &WorkerRestartIntent{
		OperationID:  p.OperationID(),
		Target:       target,
		DesiredBuild: desiredBuild,
	})
}

// SettleDeadRestart releases a pending boot. Only a death proved on the
// selected target releases it.
func (p *WorkerPermit) SettleDeadRestart(ctx context.Context, target core.TargetLocator) error {
	if err := p.VerifyTarget(target); err != nil {
		return err
	}
	intent, err := LoadWorkerRestart(p.SessionID())
	if err != nil {
		return err
	}
	if intent != nil {
		if !workerLifecycleSameTarget(intent.Target, target) {
			return errors.New("pending worker replacement belongs to another target")
		}
		return FinishWorkerRestart(ctx, p.SessionID(), intent.OperationID)
	}
	return nil
}

func (p *WorkerPermit) SessionID() string {
	return p.admission.sessionID
}

func (p *WorkerPermit) OperationID() string {
	return p.admission.operationID
}

// WorkerPermitAcquire derives a permit from admission (Rust's
// WorkerPermit::acquire). Admission is decided only by RecoveryGate. Nested
// mutations retain their operation identity; a fresh capability retains the
// caller's hold. The caller releases the returned handle.
func WorkerPermitAcquire(ctx context.Context, sessionID string, holder core.HoldsSession) (*WorkerPermit, error) {
	admission := holder.Admission()
	if admission.SessionID() != sessionID {
		admission.Release()
		return nil, fmt.Errorf("worker admission belongs to another session: %s", sessionID)
	}
	if owner := Current(ctx, sessionID); owner != nil {
		admission.Release()
		return owner.Clone(), nil
	}
	operationID, err := client.NewCommandID("worker-owner")
	if err != nil {
		admission.Release()
		return nil, err
	}
	permit := &WorkerPermit{admission: &workerAdmission{admission: admission, sessionID: sessionID, operationID: operationID}}
	permit.admission.handles.Store(1)
	return permit, nil
}

func (p *WorkerPermit) Admission() core.SessionAdmission {
	return p.admission.admission
}

// Scope is ctx carrying this permit for async work (Rust's
// ASYNC_OWNER.scope).
func (p *WorkerPermit) Scope(ctx context.Context) context.Context {
	return context.WithValue(ctx, asyncOwnerKey{}, p)
}

// ScopeBlocking is ctx carrying this permit for blocking work (Rust's
// BLOCKING_OWNER, restored when the work returns; a derived context ends
// with the work by construction).
func (p *WorkerPermit) ScopeBlocking(ctx context.Context) context.Context {
	return context.WithValue(ctx, blockingOwnerKey{}, p)
}

// Run runs work under the caller's worker permit for sessionID.
func Run[T any](ctx context.Context, sessionID string, reason string, executor core.CommandExecutor, work func(ctx context.Context) (T, error)) (T, error) {
	owner, err := Require(ctx, sessionID)
	if err != nil {
		var zero T
		return zero, fmt.Errorf("%s for session %s requires its caller's worker permit: %w", reason, sessionID, err)
	}
	return work(owner.Scope(ctx))
}

// RunWithOwner runs work under retained, or a permit acquired from
// admission. A lease returned by an earlier call carries its own admission
// capability. Borrow it before acquiring again, even outside the original
// task scope. RunWithOwner takes over retained, as Rust moves it in, and
// releases it when work returns.
func RunWithOwner[T any](ctx context.Context, admission core.HoldsSession, sessionID string, reason string, executor core.CommandExecutor, retained *WorkerPermit, work func(ctx context.Context) (T, error)) (T, error) {
	var zero T
	owner := retained
	if owner != nil {
		if owner.SessionID() != sessionID {
			owner.Release()
			return zero, fmt.Errorf("retained worker owner mismatch")
		}
	} else {
		acquired, err := WorkerPermitAcquire(ctx, sessionID, admission)
		if err != nil {
			return zero, fmt.Errorf("%s for session %s requires its caller's admission: %w", reason, sessionID, err)
		}
		owner = acquired
	}
	defer owner.Release()
	return work(owner.Scope(ctx))
}

func RunBlocking[T any](ctx context.Context, sessionID string, reason string, executor core.CommandExecutor, work func(ctx context.Context) (T, error)) (T, error) {
	owner, err := Require(ctx, sessionID)
	if err != nil {
		var zero T
		return zero, fmt.Errorf("%s for session %s requires its caller's worker permit: %w", reason, sessionID, err)
	}
	return work(owner.ScopeBlocking(ctx))
}

// RunAdmitted is the entry point for work whose caller already holds the
// session owner.
func RunAdmitted[T any](ctx context.Context, admission core.HoldsSession, sessionID string, work func(ctx context.Context) (T, error)) (T, error) {
	owner, err := WorkerPermitAcquire(ctx, sessionID, admission)
	if err != nil {
		var zero T
		return zero, err
	}
	defer owner.Release()
	return work(owner.Scope(ctx))
}
