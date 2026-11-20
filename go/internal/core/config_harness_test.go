// Port of the tests in mj-core/src/config/harness.rs.

package core

import (
	"fmt"
	"strings"
	"testing"
)

// The provider keys are OpenCode's own list, so they must come from the
// version Mjolnir runs.
func TestOpencodeProviderEnvironmentWasGeneratedFromThePinnedOpencode(t *testing.T) {
	header, _, _ := strings.Cut(opencodeProviderEnvironment, "\n")
	if !strings.HasPrefix(header, fmt.Sprintf("# OpenCode %s:", OpencodeVersion)) {
		t.Fatalf("regenerate mj-core/assets/opencode-provider-environment.txt with scripts/generate-opencode-provider-environment.py for the pinned OpenCode; it starts %q", header)
	}
}

// Hard-won: 07c1bee37: cached Muse efforts survived the managed harness upgrade.
func TestAManagedHarnessUpgradeInvalidatesDiscoveredCapabilities(t *testing.T) {
	profile := NewHarnessProfile(HarnessKindMuse, "/profiles/muse")
	inputs := profile.DiscoveryInputs()
	if inputs.ManagedInstall != Pin(HarnessKindMuse).InstallID {
		t.Fatalf("managed install %q", inputs.ManagedInstall)
	}
	if profile.CapabilitiesKey("muse") != inputs.CapabilitiesKey("muse", true) {
		t.Fatal("the profile's key is its inputs' key")
	}
	upgraded := inputs
	upgraded.ManagedInstall = "muse-acp-next_muse-next"
	if inputs.CapabilitiesKey("muse", true) == upgraded.CapabilitiesKey("muse", true) {
		t.Fatal("a new managed installation advertises new models and efforts")
	}
}
