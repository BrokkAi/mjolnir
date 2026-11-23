// Port of mj-store/src/lib.rs's test setup.

package store

import "github.com/BrokkAi/mjolnir/go/internal/core"

// Rust enables shared test fixtures at test-binary startup
// (mj_core::enable_test_fixtures_on_startup!); the Go test binary does it
// here.
func init() { core.EnableTestFixtures() }
