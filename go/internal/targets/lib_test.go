// Port of mj-targets/src/lib.rs's test setup.

package targets

import "github.com/BrokkAi/mjolnir/go/internal/core"

// Rust: mj_core::enable_test_fixtures_on_startup!(), and cfg!(test) for
// this package's own tests.
func init() {
	core.EnableTestFixtures()
	cfgTest = true
}
