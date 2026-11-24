// Port of mj-profile-catalog/src/lib.rs's test setup.

package profilecatalog

import (
	"runtime"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Rust: mj_core::enable_test_fixtures_on_startup!().
func init() { core.EnableTestFixtures() }

// currentThread runs the test on one processor, as Rust's #[tokio::test]
// runs every task on the test's own thread: goroutines the catalog spawns
// run when the test goroutine blocks or yields, not alongside it, so a count
// taken right after a call sees what the call itself started.
func currentThread(t *testing.T) {
	previous := runtime.GOMAXPROCS(1)
	t.Cleanup(func() { runtime.GOMAXPROCS(previous) })
}

func stringPointer(value string) *string { return &value }
