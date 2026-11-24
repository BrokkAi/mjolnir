// Port of mj-accounts/src/lib.rs's test setup.

package accounts

import "github.com/BrokkAi/mjolnir/go/internal/core"

// Rust: mj_core::enable_test_fixtures_on_startup!().
func init() { core.EnableTestFixtures() }
