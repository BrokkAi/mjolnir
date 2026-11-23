// Port of the test setup in voice-worker/src/main.rs.

package main

import "github.com/BrokkAi/mjolnir/go/internal/core"

// mj_core::enable_test_fixtures_on_startup!() in Rust.
func init() { core.EnableTestFixtures() }
