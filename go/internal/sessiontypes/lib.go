// Port of mj-session-types/src/lib.rs.

// Package sessiontypes holds types that more than one part of the daemon
// side names, kept apart from all of them: the plan the session runtime
// follows to recover a worker, which worker staging fills in, and the
// failure a harness reports while it prepares.
//
// Staging builds these plans and the session runtime executes them. With the
// types in either one, the other would have to depend on it; here both
// depend only on this package. Put a type here when two packages that should
// not depend on each other both need it, and keep behaviour out: a plan is
// data.
//
// Rust's `enable_test_fixtures_on_startup!` (under `cfg(test)`) is the init
// function in lib_test.go.
package sessiontypes

import (
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

type RelaySessionTarget struct {
	SessionID string
	Spec      core.CommandSpec
	// WorkerRecovery proves the exact worker is absent before restarting it
	// in place. Direct relay clients omit recovery; controller-managed
	// sessions self-heal without turning a shared transport outage into
	// destructive restarts.
	WorkerRecovery *WorkerRecoveryPlan
	ProjectMemory  *ProjectMemorySyncTarget
}

type ProjectMemorySyncTarget struct {
	CanonicalRoot string
}

// WorkerWorkspace is the working directory a bare-target worker must be able
// to enter before it can serve a relay handshake. Container availability is
// checked separately by the target recovery plan; bare targets have no
// runtime object to inspect.
type WorkerWorkspace struct {
	Target    core.ManagedWorktreeTarget
	Directory string
}

type WorkerRecoveryPlan struct {
	// SourceTarget is the durable target identity when this plan was built.
	// A stale actor must never recover a resource after the session moves or
	// starts destruction.
	SourceTarget core.TargetLocator
	Target       *core.TargetRecoveryPlan
	Workspace    *WorkerWorkspace
	// LivenessProbe runs the control worker, so it installs it first.
	LivenessProbe targets.WorkerControlPlan
	// ExitRecord prints the dead worker's own exit record, read before a
	// restart replaces it, so a worker that died of a full disk is not
	// restarted onto the same full disk.
	ExitRecord *core.CommandSpec
	// BinaryRefresh refreshes a stale installed worker before restarting it.
	// The digest is computed inside the recovery task so hashing a large
	// binary never blocks a controller UI loop. Nil is Rust's None.
	BinaryRefresh WorkerBinaryRefresh
	// LaunchRefresh keeps the worker executable and its launch schema
	// paired. Configuration bytes travel through redacted stdin only when
	// their digest is stale.
	LaunchRefresh *WorkerLaunchRefreshPlan
	Restart       targets.WorkerControlPlan
}

// WorkerBinaryRefresh is how recovery refreshes a stale installed worker
// binary before restarting.
//
// A caller with a known source can supply a prepared copy plan. Controller
// target planning always defers resolution: selecting the right
// architecture, downloading and hashing binaries belong in the background
// recovery task.
//
// Variants: WorkerBinaryRefreshPrepared, WorkerBinaryRefreshDeferred.
type WorkerBinaryRefresh interface {
	isWorkerBinaryRefresh()
}

// WorkerBinaryRefreshPrepared is Rust's `WorkerBinaryRefresh::Prepared`.
type WorkerBinaryRefreshPrepared struct {
	Plan WorkerBinaryRefreshPlan
}

// WorkerBinaryRefreshDeferred is Rust's `WorkerBinaryRefresh::Deferred`.
type WorkerBinaryRefreshDeferred struct {
	Refresh DeferredWorkerBinaryRefresh
}

func (WorkerBinaryRefreshPrepared) isWorkerBinaryRefresh() {}
func (WorkerBinaryRefreshDeferred) isWorkerBinaryRefresh() {}

type WorkerBinaryRefreshPlan struct {
	Source          string
	InstalledDigest core.CommandSpec
	Replace         core.CommandPlan
}

// DeferredWorkerBinaryRefresh is a worker refresh resolved at recovery
// time: select the worker binary for the target's own architecture, compare
// it to the installed one, and copy only when they differ.
type DeferredWorkerBinaryRefresh struct {
	Locator         core.TargetsTargetLocator
	SessionID       string
	InstalledDigest core.CommandSpec
}

type WorkerLaunchRefreshPlan struct {
	ExpectedSHA256  string
	InstalledDigest core.CommandSpec
	Replace         core.CommandPlan
}

// HarnessPreparationFailure is a harness that could not be prepared for a
// session: the step that failed and what it reported.
//
// Rust's field `error` is ErrorText: Go forbids a field named like the
// Error method.
type HarnessPreparationFailure struct {
	Step      string
	ErrorText string
}

func (f HarnessPreparationFailure) Error() string {
	return fmt.Sprintf("harness preparation failed at %s: %s", f.Step, f.ErrorText)
}

// LifecycleFailureReason is trusted public guidance attached at the failing
// lifecycle boundary. Never derive it from an error chain, which can contain
// credentials and host paths.
type LifecycleFailureReason int

const (
	LifecycleFailureReasonWorkerContact LifecycleFailureReason = iota
	LifecycleFailureReasonCheckpoint
	LifecycleFailureReasonCleanup
	LifecycleFailureReasonRetainedMoveSource
	LifecycleFailureReasonUnsealedMoveSource
	LifecycleFailureReasonHarnessUnavailable
)

// String is Rust's Display.
func (r LifecycleFailureReason) String() string {
	switch r {
	case LifecycleFailureReasonWorkerContact:
		return "Suspension could not contact the worker. Resume the session to repair its runtime, then retry suspension."
	case LifecycleFailureReasonCheckpoint:
		return "Suspension could not save its recovery checkpoint. Retry suspension; the existing environment was kept."
	case LifecycleFailureReasonCleanup:
		return "Suspension could not finish removing its target. Retry suspension to finish cleanup; retained environments and recovery data were kept."
	case LifecycleFailureReasonRetainedMoveSource:
		return "Suspension interrupted Move before the destination was ready; the source workspace was kept. Resume the session to repair its runtime, then retry suspension."
	case LifecycleFailureReasonUnsealedMoveSource:
		return "Suspension could not establish a sealed Move checkpoint. Resume the session to repair its runtime, then retry suspension; existing environments and recovery data were kept."
	case LifecycleFailureReasonHarnessUnavailable:
		return "Suspension could not save the session without its harness, which failed to open; the environment was kept. Resume the session to retry its harness, or destroy it."
	}
	return fmt.Sprintf("LifecycleFailureReason(%d)", int(r))
}

func (r LifecycleFailureReason) Error() string { return r.String() }

type RegisterSubagentRequest struct {
	NoGithubCredentials bool
	Kind                core.SubagentKind
	Orchestrator        bool
	ReviewerLaunch      *core.ReviewerLaunchConfig
	ParentSessionID     string
	TaskName            string
	ProfileID           string
	Model               *string
	Effort              *string
	FastMode            bool
	// WorkingDirectory: empty means the parent's working directory. An
	// absolute path is used as-is; a relative path is resolved against the
	// parent's working directory. The path is interpreted on the parent's
	// target and must exist there; no other restriction applies.
	WorkingDirectory string
	InitialPrompt    string
	RequestKey       string
	// ReportRoot is the absolute directory on the parent's target that holds
	// its children's report directories, prepared by the controller. Nil
	// registers a child without one.
	ReportRoot *string
	// Category is the canonical task category stored on the accounting row:
	// `primary`, `straightforward`, `subtle`, `oracle`, `reviewer`, or
	// `brief_writer`.
	Category string
}
