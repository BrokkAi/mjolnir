// Port of the tests in mj-build-id/src/lib.rs.

package buildid

import (
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Rust enables shared test fixtures at test-binary startup
// (enable_test_fixtures_on_startup!); the Go test binary does it here.
// Runtime test hooks stay off unless a test enables them.
func init() { core.EnableTestFixtures() }

func TestTestStartupEnablesTheFallbackIdentityWithoutRuntimeHooks(t *testing.T) {
	if !core.TestFixturesEnabled() {
		t.Fatal("a test binary must enable the shared test fixtures")
	}
	if identity := core.WorkerBuildID(); !strings.HasSuffix(identity, "+"+strings.Repeat("0", 40)) {
		t.Fatalf("worker build id %q is not the fallback test identity", identity)
	}
	if core.TestHooksEnabled() {
		t.Fatal("runtime test hooks are enabled without an opt-in")
	}
}
