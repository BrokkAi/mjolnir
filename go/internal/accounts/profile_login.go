// Port of mj-accounts/src/profile_login.rs.

// Read-only sign-in status for configured profiles.
//
// This is deliberately separate from capability probing. A model probe
// answers what the harness offers after it starts; this answers whether a
// credential is present before anyone starts it. Settings shows the answer
// beside each profile so a signed-out profile is visible before a session
// fails.

package accounts

import (
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ProfileLoginStatuses is the status of every profile in config, keyed by
// profile id. Each profile is probed concurrently under its own 10 s
// deadline (Rust: `profile_login::statuses`).
func ProfileLoginStatuses(config *core.Config, cancelled *atomic.Bool) map[string]core.LoginStatus {
	panic("unported: mj-accounts/src/profile_login.rs statuses")
}

// ProfileLoginStatus is whether profile has a usable harness login or needs
// no interactive login (Rust: `profile_login::status`).
func ProfileLoginStatus(profileID string, profile *core.HarnessProfile, executor core.CommandExecutor) core.LoginStatus {
	panic("unported: mj-accounts/src/profile_login.rs status")
}
