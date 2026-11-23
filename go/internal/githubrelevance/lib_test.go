// Port of mj-github-relevance/src/lib.rs's test-only items.

package githubrelevance

import (
	"os"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Rust's test builds enable mj-core's shared test fixtures at startup
// (enable_test_fixtures_on_startup!); the Go test binary does it here.
func init() { core.EnableTestFixtures() }

// skipUnlessIgnoredTestsRequested is Rust's #[ignore]: the test runs only
// when MJ_RUN_IGNORED_TESTS=1 asks for it, as `cargo test -- --ignored`
// does.
func skipUnlessIgnoredTestsRequested(t *testing.T, reason string) {
	t.Helper()
	if os.Getenv("MJ_RUN_IGNORED_TESTS") != "1" {
		t.Skip(reason)
	}
}
