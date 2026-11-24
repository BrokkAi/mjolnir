// Port of mj-session-runtime/src/session_manager/client_backend.rs (only
// what a dashboard calls so far; the backend arrives with the session
// manager port).

package sessionruntime

import "github.com/BrokkAi/mjolnir/go/internal/client"

// Client narrows this controller-owned manager to session lookup for a
// control surface.
func (c SessionManagerControl) Client() client.SessionControl {
	panic("unported: mj-session-runtime/src/session_manager/client_backend.rs SessionManagerControl::client")
}
