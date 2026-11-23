// Port of mj-github-watch/src/lib.rs's test-only items.

package githubwatch

import "github.com/BrokkAi/mjolnir/go/internal/core"

// Rust's test builds enable mj-core's shared test fixtures at startup
// (enable_test_fixtures_on_startup!); the Go test binary does it here.
func init() { core.EnableTestFixtures() }
