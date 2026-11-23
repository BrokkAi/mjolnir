// Port of mj-accounts/src/antigravity_usage.rs.

// Antigravity's own structured `/usage` command, with an isolated login
// copy.

package accounts

import (
	"context"
	"fmt"
	"os"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
)

// antigravityRuntimeEnvironment is the environment the quota CLI is
// resolved and run under: the daemon's, then the profile's, with the
// Antigravity home settings applied. The prewarm resolves the managed
// runtime from this same environment.
func antigravityRuntimeEnvironment(home string, environment map[string]string) map[string]string {
	// Resolve the runtime from the daemon's environment before HOME is
	// replaced for the disposable login. Profile overrides retain their
	// usual precedence.
	inherited := map[string]string{}
	for _, entry := range os.Environ() {
		if name, value, ok := strings.Cut(entry, "="); ok {
			inherited[name] = value
		}
	}
	for name, value := range environment {
		inherited[name] = value
	}
	core.HarnessKindAntigravity.ConfigureHomeEnvironment(home, inherited)
	return inherited
}

// antigravityManagedRuntime is the pinned `agy` a quota check runs. Unlike
// the other quota harnesses it has no host-CLI step: the host is always
// served the managed installation. Unix only in Rust.
func antigravityManagedRuntime(ctx context.Context, environment map[string]string) (harnessinstall.Located, error) {
	policy := core.ExecutionPolicyConfiguredApprovals
	command, args := core.FallbackLaunch(core.HarnessKindAntigravity, policy)
	located, err := harnessinstall.Locate(ctx, core.LocalHarnessRuntime(), core.HarnessKindAntigravity, policy, command, args, environment)
	if err != nil {
		return harnessinstall.Located{}, fmt.Errorf("prepare Antigravity quota CLI: %w", err)
	}
	return located, nil
}

// AntigravityUsageQuery runs `agy -p /usage` against a disposable copy of
// the profile's login and returns its quota windows.
func AntigravityUsageQuery(ctx context.Context, home string, environment map[string]string) ([]client.QuotaWindow, error) {
	panic("unported: mj-accounts/src/antigravity_usage.rs query")
}

func antigravityUsageParse(bytes []byte) ([]client.QuotaWindow, error) {
	panic("unported: mj-accounts/src/antigravity_usage.rs parse")
}
