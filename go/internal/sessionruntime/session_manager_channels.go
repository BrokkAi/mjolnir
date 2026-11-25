// Port of mj-session-runtime/src/session_manager/channels.rs (only the
// channel ends a dashboard holds so far; the manager behind them arrives
// with the session manager port).

package sessionruntime

import (
	"context"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SessionManagerUpdate is one session's latest view.
type SessionManagerUpdate struct {
	// RawObservation is the daemon-only cache input captured before
	// model-policy projection. Never sent on the wire.
	RawObservation *core.SharedSnapshot[core.RelayOperationalState]
	SessionID      string
	View           client.ManagedSessionView
}

// SessionManagerShutdown is the exclusive owner of the manager task and
// every relay actor below it.
//
// Long-running control surfaces explicitly call Shutdown before their
// runtime goes away. Close is Rust's Drop: an aborting fallback for paths
// that cannot wait.
type SessionManagerShutdown struct {
	// signal is Rust's Option<oneshot::Sender<()>>: closed once to ask the
	// manager to stop; nil once used.
	signal chan<- struct{}
	// task is the manager task; nil once awaited or aborted.
	task *core.JoinHandle[struct{}]
}

// Shutdown signals the manager and waits for its task.
func (s *SessionManagerShutdown) Shutdown(ctx context.Context) error {
	panic("unported: mj-session-runtime/src/session_manager/channels.rs SessionManagerShutdown::shutdown")
}

// Close signals the manager and abandons its task (Rust's Drop).
func (s *SessionManagerShutdown) Close() {
	panic("unported: mj-session-runtime/src/session_manager/channels.rs SessionManagerShutdown::drop")
}

// SessionManagerUpdates is the bounded latest-state feed for the dashboard.
// At most one snapshot per session is retained, and sessions are served in
// first-pending order.
type SessionManagerUpdates struct{}

// Recv waits for the next session's update; false when the manager has
// stopped or ctx ended. Cancel-safe.
func (u *SessionManagerUpdates) Recv(ctx context.Context) (SessionManagerUpdate, bool) {
	panic("unported: mj-session-runtime/src/session_manager/channels.rs SessionManagerUpdates::recv")
}

// TryRecv takes an update that is already pending, without waiting.
func (u *SessionManagerUpdates) TryRecv() (SessionManagerUpdate, bool) {
	panic("unported: mj-session-runtime/src/session_manager/channels.rs SessionManagerUpdates::try_recv")
}
