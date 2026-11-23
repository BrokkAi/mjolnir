// Port of mj-session-types/src/lib.rs (its `cfg(test)` part).

package sessiontypes

import "github.com/BrokkAi/mjolnir/go/internal/core"

// Rust: `#[cfg(test)] mj_core::enable_test_fixtures_on_startup!();`.
func init() { core.EnableTestFixtures() }
