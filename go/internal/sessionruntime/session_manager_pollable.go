// Port of mj-session-runtime/src/session_manager/pollable.rs (only the
// predicate the dashboard asks so far).

package sessionruntime

import "github.com/BrokkAi/mjolnir/go/internal/core"

// SessionTargetIsPollable reports whether a session has a worker to poll.
//
// A Provisioning session has a target locator as soon as the target itself
// exists, which is *before* its worker binary has been copied into place.
// Polling that window means running `execve` on a file `cp` still holds
// open for writing, which fails with `ETXTBSY` and leaves the session
// recorded as unreachable. Provisioning connects to its own worker when it
// is ready and then marks the session `Running`, which is when there is
// something here to poll.
//
// `Parked` is excluded because a parked sub-agent's worker was stopped on
// purpose: polling it would find a dead worker, and the session manager's
// recovery would start it again. This predicate is what keeps the session
// manager, worker recovery and resource sampling away from parked children.
//
// Rust also counts each call in a test-only visit counter
// (POLLABILITY_VISITS), which only the session manager's own tests read; it
// arrives with them.
func SessionTargetIsPollable(session *core.SessionRecord) bool {
	switch session.State {
	case core.SessionStateError,
		core.SessionStateStartupCleanup,
		core.SessionStateProvisioning,
		core.SessionStateDestroying,
		core.SessionStateParked:
		return false
	}
	return session.State.IsActive() && session.Target != nil
}
