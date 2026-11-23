// Port of mj-accounts/src/quota_harness.rs.

// Host CLI resolution shared by quota probes and login renewals.

package accounts

import (
	"context"
	"fmt"
	"os"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
)

// quotaProgram is the program a quota check or login renewal runs.
type quotaProgram struct {
	command     string
	environment map[string]string
	// installation keeps the harness and managed Node leases until the child
	// exits; release gives them back. Always nil on Windows, which has no
	// managed installation (Rust: `Infallible`).
	installation *harnessinstall.Located
}

// release gives back the installation's leases (Rust drops `_installation`
// with the last reference). Safe on nil and when called more than once.
func (p *quotaProgram) release() {
	if p == nil || p.installation == nil {
		return
	}
	if p.installation.Managed != nil {
		p.installation.Managed.Release()
	}
	p.installation = nil
}

// commandText: CommandSpec carries UTF-8 text. Reject paths it cannot
// represent instead of changing their bytes or asking the child's PATH to
// choose again.
func commandText(command string) (string, error) {
	if !utf8.ValidString(command) {
		return "", fmt.Errorf("quota harness path is not UTF-8: %q", command)
	}
	return command, nil
}

// quotaEnvironmentValue is name's value in environment, else this process's.
// CommandSpec applies its environment in key order; Windows folds aliases,
// so the last spelling wins there too.
func quotaEnvironmentValue(environment map[string]string, name string) (string, bool) {
	panic("unported: mj-accounts/src/quota_harness.rs environment_value")
}

// hostProgram is the host's own CLI for harness, when it has one: the
// program a quota check runs without any installation. The one lookup both
// locate and PrepareManaged use, so a prewarm never installs what a check
// would not.
func hostProgram(ctx context.Context, harness core.HarnessKind, environment map[string]string) (string, bool, error) {
	panic("unported: mj-accounts/src/quota_harness.rs host_program")
}

// searchEnvironment is the environment the installer searches and caches
// under: this process's, overlaid with the profile's.
func searchEnvironment(environment map[string]string) map[string]string {
	search := map[string]string{}
	for _, entry := range os.Environ() {
		if name, value, ok := strings.Cut(entry, "="); ok {
			search[name] = value
		}
	}
	for name, value := range environment {
		search[name] = value
	}
	return search
}

// managedInstallation is the pinned managed installation a quota check runs
// when the host has no CLI. The first call on a host does the cold install.
func managedInstallation(ctx context.Context, harness core.HarnessKind, environment map[string]string) (harnessinstall.Located, error) {
	name := harness.CLIBinaryName()
	search := searchEnvironment(environment)
	policy := core.ExecutionPolicyConfiguredApprovals
	command, args := core.FallbackLaunch(harness, policy)
	located, err := harnessinstall.Locate(ctx, core.LocalHarnessRuntime(), harness, policy, command, args, search)
	if err != nil {
		return harnessinstall.Located{}, fmt.Errorf("no %s to run for quota: could not prepare the pinned harness: %w", name, err)
	}
	return located, nil
}

// ManagedPreparationFailure is one harness PrepareManaged could not prepare
// (Rust: a `(HarnessKind, anyhow::Error)` pair).
type ManagedPreparationFailure struct {
	Harness core.HarnessKind
	Err     error
}

// PrepareManaged installs, ahead of the first quota check, the pinned
// harness each request would otherwise install inline. The caller passes
// only requests that run a host harness, filtered with
// QuotaRefreshRequest.RunsHostHarness on a blocking goroutine, because it
// may read the profile's login file; they are prepared only on a host that
// does not provide the harness. The installer caches per
// `XDG_CACHE_HOME`/`HOME`, so work is grouped by (harness, cache root):
// groups run concurrently, and within a group the profiles are tried in
// order until one preparation succeeds, so one profile's broken environment
// does not hide the others. The daemon container's image carries its
// harnesses and installs nothing. The leases are released on return: a
// check takes its own, and an installation in progress is shared through
// the installer's own lock. Every failure is returned with its harness;
// nothing is retried here, because the next check that needs the harness
// prepares it again. Cancelling ctx cancels the installers' process groups.
// Unix only in Rust.
func PrepareManaged(ctx context.Context, requests []QuotaRefreshRequest) []ManagedPreparationFailure {
	panic("unported: mj-accounts/src/quota_harness.rs prepare_managed")
}

// quotaHarnessCandidate is one profile's way to the managed harness: through
// the shared quota resolution (host CLI first), or Antigravity's, which has
// no host step.
type quotaHarnessCandidate struct {
	antigravity bool
	environment map[string]string
}

// locate is the program a quota check for harness runs: the host's CLI, or
// the pinned managed installation.
func locate(ctx context.Context, harness core.HarnessKind, environment map[string]string) (*quotaProgram, error) {
	panic("unported: mj-accounts/src/quota_harness.rs locate")
}
