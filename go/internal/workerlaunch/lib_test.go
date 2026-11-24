// Port of mj-worker-launch/src/lib.rs's test setup.

package workerlaunch

import "github.com/BrokkAi/mjolnir/go/internal/core"

// Rust: `#[cfg(test)] mj_core::enable_test_fixtures_on_startup!();`.
func init() { core.EnableTestFixtures() }
