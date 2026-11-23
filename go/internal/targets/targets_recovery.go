// Port of mj-targets/src/targets/recovery.rs.

package targets

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ProvisionOnLocatorPlan is the clone/bootstrap commands for AWS once the
// exact instance ID and address are known.
func ProvisionOnLocatorPlan(locator *core.TargetsTargetLocator, sessionID string, bundle *core.ProjectBundleSpec, githubToken *string) (core.CommandPlan, error) {
	panic("unported: mj-targets/src/targets/recovery.rs provision_on_locator_plan")
}

// RecoverCheckoutPlan reconstructs repositories without creating or retiring
// their target.
func RecoverCheckoutPlan(
	locator *core.TargetsTargetLocator,
	sessionID string,
	bundle *core.ProjectBundleSpec,
	workspace string,
	githubToken *string,
	existingRepositories map[string]struct{},
) (core.CommandPlan, error) {
	panic("unported: mj-targets/src/targets/recovery.rs recover_checkout_plan")
}

func ReconnectPlan(locator *core.TargetsTargetLocator, sessionID string) (core.CommandPlan, error) {
	return relayProxyPlan(locator, sessionID, core.SshTrafficClassControl)
}

// ProjectMemoryRelayCommand is the relay connection that carries one
// project-memory transaction. A paged snapshot or upload moves frames of up
// to 8 MiB; on the shared Control connection they sit in its send queue
// ahead of every other session's small relay RPC and channel open on that
// host (#1580). This is the only place that picks the lane for
// project-memory traffic.
func ProjectMemoryRelayCommand(locator *core.TargetsTargetLocator, sessionID string) (core.CommandSpec, error) {
	panic("unported: mj-targets/src/targets/recovery.rs project_memory_relay_command")
}

func relayProxyPlan(locator *core.TargetsTargetLocator, sessionID string, class core.SshTrafficClass) (core.CommandPlan, error) {
	panic("unported: mj-targets/src/targets/recovery.rs relay_proxy_plan")
}

// TargetRecoveryPlan describes safe recovery for a container that belongs to
// an active session; nil means no plan. The
// inspect command is deliberately separate from `exec`: a host crash can
// leave the container present but stopped, where `exec` cannot distinguish
// that state from other transport failures.
func TargetRecoveryPlan(locator *core.TargetsTargetLocator, sessionID string) (*core.TargetRecoveryPlan, error) {
	panic("unported: mj-targets/src/targets/recovery.rs target_recovery_plan")
}

// EnsureRecoveryTargetRunning starts a confirmed stopped container target
// and verifies it reached `running`. Missing or foreign containers,
// transport failures, and transitional states fail without running the
// start command.
func EnsureRecoveryTargetRunning(executor core.CommandExecutor, plan *core.TargetRecoveryPlan) (core.TargetRecoveryOutcome, error) {
	panic("unported: mj-targets/src/targets/recovery.rs ensure_recovery_target_running")
}

func inspectRecoveryTarget(executor core.CommandExecutor, plan *core.TargetRecoveryPlan) (string, error) {
	panic("unported: mj-targets/src/targets/recovery.rs inspect_recovery_target")
}
