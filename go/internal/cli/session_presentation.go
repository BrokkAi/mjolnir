// Port of mj-cli/src/session_presentation.rs.
//
// Shared, display-only projections for live session and lifecycle views.
//
// The dashboard and workspace views consume the same bounded runtime
// snapshots. Keeping these setters here prevents either surface from
// turning a display update into persistence or chat-side behavior.

package cli

import (
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// applySessionActivity applies the bounded activity and connectivity facts
// from one managed view.
//
// The relay snapshot contains all of the live facts needed by both surfaces:
// the current step clock, operational activity, and whether the relay can be
// reached. A missing snapshot deliberately clears activity to its default.
func applySessionActivity(dashboard *tui.DashboardState, sessionID string, view *client.ManagedSessionView) {
	panic("unported: mj-cli/src/session_presentation.rs apply_session_activity")
}

// applyWorkerActivity applies one worker report and hands the open
// conversations the sub-agent counts it can change.
//
// A child's worker reporting a turn changes no record, so the parent's
// composer footer (`Subagents · N`) would otherwise keep the count from the
// last record change and read `0/M` for the child's whole turn (I2-7).
func applyWorkerActivity(
	dashboard *tui.DashboardState,
	chats map[string]*chat.ActiveChat,
	sessionID string,
	view *client.ManagedSessionView,
) {
	panic("unported: mj-cli/src/session_presentation.rs apply_worker_activity")
}

// lifecycleKind maps a daemon lifecycle to the operation kind rendered by a
// surface.
func lifecycleKind(kind client.RuntimeLifecycleKind) tui.SessionOperationKind {
	switch kind {
	case client.RuntimeLifecycleKindCreate:
		return tui.SessionOperationKindLaunching
	case client.RuntimeLifecycleKindSuspend:
		return tui.SessionOperationKindSuspending
	case client.RuntimeLifecycleKindResume:
		return tui.SessionOperationKindResuming
	case client.RuntimeLifecycleKindMove:
		return tui.SessionOperationKindMoving
	case client.RuntimeLifecycleKindDestroyStopped,
		client.RuntimeLifecycleKindForceDestroy,
		client.RuntimeLifecycleKindCleanup:
		return tui.SessionOperationKindDestroying
	case client.RuntimeLifecycleKindStopSubagent:
		return tui.SessionOperationKindStopping
	}
	// RuntimeLifecycleKind's decoder admits only the kinds above.
	panic(fmt.Sprintf("unknown runtime lifecycle kind %q", string(kind)))
}

// applyLifecycleDisplay projects one daemon lifecycle into the display
// operation overlay.
//
// This helper intentionally has no notice, persistence, or chat behavior.
// The caller owns those surface-specific concerns and may skip this helper
// when a local lifecycle operation takes precedence over the daemon view.
func applyLifecycleDisplay(dashboard *tui.DashboardState, lifecycle *client.RuntimeLifecycleView) {
	panic("unported: mj-cli/src/session_presentation.rs apply_lifecycle_display")
}
