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
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// WorkerPermit is a worker mutation capability derived from the session
// owner's admission. Blocking work retains it even when its async waiter
// disappears. Copies of the pointer share it (Rust's Arc clone).
type WorkerPermit struct {
	admission *workerAdmission
}

type workerAdmission struct {
	admission   core.SessionAdmission
	sessionID   string
	operationID string
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
	panic("unported: mj-store/src/worker_lifecycle.rs observe_worker_restart_outcome")
}

func (p *WorkerPermit) VerifyCachedTarget(state *core.StateState) error {
	panic("unported: mj-store/src/worker_lifecycle.rs WorkerPermit::verify_cached_target")
}

func (p *WorkerPermit) VerifyTarget(expected core.TargetLocator) error {
	panic("unported: mj-store/src/worker_lifecycle.rs WorkerPermit::verify_target")
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
	panic("unported: mj-store/src/worker_lifecycle.rs WorkerPermit::settle_dead_restart")
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
// caller's hold.
func WorkerPermitAcquire(ctx context.Context, sessionID string, holder core.HoldsSession) (*WorkerPermit, error) {
	admission := holder.Admission()
	if admission.SessionID() != sessionID {
		return nil, fmt.Errorf("worker admission belongs to another session: %s", sessionID)
	}
	if owner := Current(ctx, sessionID); owner != nil {
		return owner, nil
	}
	operationID, err := client.NewCommandID("worker-owner")
	if err != nil {
		return nil, err
	}
	return &WorkerPermit{admission: &workerAdmission{admission: admission, sessionID: sessionID, operationID: operationID}}, nil
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
// task scope.
func RunWithOwner[T any](ctx context.Context, admission core.HoldsSession, sessionID string, reason string, executor core.CommandExecutor, retained *WorkerPermit, work func(ctx context.Context) (T, error)) (T, error) {
	var zero T
	owner := retained
	if owner != nil {
		if owner.SessionID() != sessionID {
			return zero, fmt.Errorf("retained worker owner mismatch")
		}
	} else {
		acquired, err := WorkerPermitAcquire(ctx, sessionID, admission)
		if err != nil {
			return zero, fmt.Errorf("%s for session %s requires its caller's admission: %w", reason, sessionID, err)
		}
		owner = acquired
	}
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
	return work(owner.Scope(ctx))
}
