// Port of mj-targets/src/targets/cleanup.rs.

package targets

import (
	"errors"
	"path/filepath"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func ClosePlan(locator *core.TargetsTargetLocator, sessionID string) (WorkerControlPlan, error) {
	return closePlanScoped(locator, sessionID, true, nil)
}

// RetireMoveTargetPlan: a Move can have two generations on one host. Shared
// session caches remain owned by the destination.
func RetireMoveTargetPlan(locator *core.TargetsTargetLocator, sessionID string) (WorkerControlPlan, error) {
	return closePlanScoped(locator, sessionID, false, nil)
}

// TerminateEc2InstanceCommand: termination needs the durable AWS identity
// even before SSH has an address.
func TerminateEc2InstanceCommand(profile string, region string, instanceID string) (core.CommandSpec, error) {
	if !core.ValidEc2InstanceID(instanceID) {
		return core.CommandSpec{}, errors.New("refusing cleanup: invalid EC2 instance ID")
	}
	// EC2 TerminateInstances is idempotent for an already-terminated
	// instance.
	return core.NewCommandSpec("aws", []string{
		"--profile", profile,
		"--region", region,
		"ec2", "terminate-instances",
		"--instance-ids", instanceID,
	}).WithPurpose("terminate exact EC2 session instance"), nil
}

// closePlanScoped builds a session's close plan; stopped is the proof that
// the worker's managed Stop completed, or nil when the plan stops it.
func closePlanScoped(locator *core.TargetsTargetLocator, sessionID string, removeSharedCache bool, stopped StoppedWorkerProof) (WorkerControlPlan, error) {
	panic("unported: mj-targets/src/targets/cleanup.rs close_plan_scoped")
}

// LocalNestedProfileRoot is where a session of a harness with an XDG-nested
// home (Muse, OpenCode) on this machine keeps its per-session profile root,
// which lies under the data directory rather than the worker root, so
// closing the session names it separately.
func LocalNestedProfileRoot(sessionID string) string {
	return filepath.Join(core.DataDir(), "profiles", sessionID)
}

// BorrowedWorkerCleanupPlan stops and removes only a child session's private
// worker state from a target owned by its parent. This never removes the
// target or project workspace.
func BorrowedWorkerCleanupPlan(locator *core.TargetsTargetLocator, childSessionID string) (WorkerControlPlan, error) {
	return borrowedWorkerCleanupPlanScoped(locator, childSessionID, nil)
}

func borrowedWorkerCleanupPlanScoped(locator *core.TargetsTargetLocator, childSessionID string, stopped StoppedWorkerProof) (WorkerControlPlan, error) {
	panic("unported: mj-targets/src/targets/cleanup.rs borrowed_worker_cleanup_plan_scoped")
}

const podmanContainerIdentityScript = `set -eu
container=$1
session=$2
if identity=$(podman container inspect --format '{{index .Config.Labels "dev.mj.managed"}}|{{index .Config.Labels "dev.mj.session"}}' "$container" 2>/dev/null); then
    [ "$identity" = "true|$session" ] || {
        echo 'refusing to operate on a Podman container Mjolnir does not own for this session' >&2
        exit 2
    }
elif ! podman info >/dev/null 2>&1; then
    echo 'could not determine whether the Podman session container exists' >&2
    exit 1
else
    exit 0
fi
`

// QuiescePlan stops a Podman target without deleting its potentially large
// writable layer; nil for a target that has none. A successful return means
// the exact owned container is absent or not running.
func QuiescePlan(locator *core.TargetsTargetLocator, sessionID string) (*core.CommandPlan, error) {
	panic("unported: mj-targets/src/targets/cleanup.rs quiesce_plan")
}

// StopContainerPlan stops every writer in an owned container through its
// engine; nil for a target that is not a container. Unlike `exec`, this also
// proves termination when a container exists but is already stopped. Podman
// retains its existing storage-preserving quiescence implementation.
func StopContainerPlan(locator *core.TargetsTargetLocator, sessionID string) (*core.CommandPlan, error) {
	panic("unported: mj-targets/src/targets/cleanup.rs stop_container_plan")
}

func podmanCleanupPlan(locator *core.TargetsTargetLocator, sessionID string) (core.CommandPlan, error) {
	panic("unported: mj-targets/src/targets/cleanup.rs podman_cleanup_plan")
}

// CleanupTargetIsConfirmedAbsent confirms that a container is absent after
// its exact delete command failed. Other target deletion commands are
// already idempotent: filesystem removal uses `rm -rf`, Podman uses
// `--ignore`, and EC2 termination is an idempotent API operation. Apple
// lists exact container IDs; Docker checks both the exact container name and
// exact session-labeled volumes while distinguishing an unavailable daemon
// from absence.
func CleanupTargetIsConfirmedAbsent(locator *core.TargetsTargetLocator, sessionID string, executor core.CommandExecutor) (bool, error) {
	panic("unported: mj-targets/src/targets/cleanup.rs cleanup_target_is_confirmed_absent")
}

func podmanAbsenceCommand(ssh *core.SshTarget, containerID string, workspaceStorage *core.TargetsPodmanWorkspaceLocator, sessionID string) core.CommandSpec {
	panic("unported: mj-targets/src/targets/cleanup.rs podman_absence_command")
}

// StoppedCleanupPlan is a removal plan that follows completed managed Stop
// and retains the launch permits. This authority does not assert
// containment of arbitrary detached writers.
type StoppedCleanupPlan[P StoppedWorkerProof] struct {
	proof P
	plan  WorkerControlPlan
}

// StoppedWorkerProof is proof that the managed Stop of one exact worker
// completed and that its launch permits are still held. The lifecycle code
// that performs the Stop issues it; nothing in this package can construct
// one, so a removal plan that skips the worker's own stop cannot be built
// from a guess.
type StoppedWorkerProof interface {
	SessionID() string
	Target() *core.TargetsTargetLocator
	Root() string
}

func (p *StoppedCleanupPlan[P]) Execute(executor core.CommandExecutor, stager ControlWorkerStager) ([]core.CommandOutput, error) {
	return p.plan.Execute(executor, stager)
}

func CloseStoppedTargetPlan[P StoppedWorkerProof](proof P) (StoppedCleanupPlan[P], error) {
	panic("unported: mj-targets/src/targets/cleanup.rs close_stopped_target_plan")
}

func RetireStoppedMoveTargetPlan[P StoppedWorkerProof](proof P) (StoppedCleanupPlan[P], error) {
	panic("unported: mj-targets/src/targets/cleanup.rs retire_stopped_move_target_plan")
}

func StoppedBorrowedWorkerCleanupPlan[P StoppedWorkerProof](proof P) (StoppedCleanupPlan[P], error) {
	panic("unported: mj-targets/src/targets/cleanup.rs stopped_borrowed_worker_cleanup_plan")
}
