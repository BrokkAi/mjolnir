// Port of mj-cli/src/dashboard/actions.rs.
//
// What the dashboard does with the work its key handling asks for.
//
// Every arm here either updates UI state directly or starts a background
// job, including SetupLogin, whose terminal connection runs in a background
// task.

package cli

import (
	"context"
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// mbxInstallCompletion is a finished mbx install with the build cache
// preview read after it. Each error is the failure text of its half (Rust's
// Result<_, String>).
type mbxInstallCompletion struct {
	install             string
	installError        *string
	preview             *core.BuildCachePreview
	previewError        *string
	installMbxAvailable bool
}

// chatToggle is a chat-scoped toggle the host's keys can run.
type chatToggle uint8

const (
	chatToggleTranscriptRendering chatToggle = iota
	chatToggleDictation
)

// applyChatToggle applies a toggle to the conversation on screen, or says
// there is none.
//
// The keys that run these live in the dashboard's command registry, so they
// answer from every surface, including a pane that has never opened a chat.
func applyChatToggle(dashboard *tui.DashboardState, activeChat *chat.ActiveChat, toggle chatToggle) {
	if activeChat == nil {
		dashboard.SetNotice("No conversation is open.")
		return
	}
	switch toggle {
	case chatToggleTranscriptRendering:
		activeChat.ToggleTranscriptRendering()
	case chatToggleDictation:
		if reason, refused := activeChat.ToggleDictation(); refused {
			dashboard.SetNotice(reason)
		}
	}
}

// copiedSessionIDNotice is what the notice bar says after the session
// menu's Copy session ID. The clipboard gets the full ID; the notice names
// the short form the rest of the dashboard uses.
func copiedSessionIDNotice(sessionID string) string {
	return fmt.Sprintf("Copied session ID %s", core.ShortID(sessionID))
}

func startSuspension(context *dashboardContext, sessionID string, consent core.SuspendConsent) {
	panic("unported: mj-cli/src/dashboard/actions.rs start_suspension")
}

// applyDashboardAction carries out one dashboard action.
func applyDashboardAction(ctx context.Context, context *dashboardContext, action tui.DashboardAction) error {
	panic("unported: mj-cli/src/dashboard/actions.rs apply_dashboard_action")
}

// startMovePreparation begins read-only preparation from either wizard
// input or an attachment reply.
func startMovePreparation(context *dashboardContext, action tui.DashboardAction) {
	panic("unported: mj-cli/src/dashboard/actions.rs start_move_preparation")
}

// resumeLaunchDestination is the session and target of a resume launch.
func resumeLaunchDestination(action tui.DashboardAction) (sessionID string, targetID string, err error) {
	panic("unported: mj-cli/src/dashboard/actions.rs resume_launch_destination")
}

func startResumeRepositoryPreflight(context *dashboardContext, launch tui.DashboardAction) error {
	panic("unported: mj-cli/src/dashboard/actions.rs start_resume_repository_preflight")
}

// startCreateSessionPreflight resolves the network plan for an isolated
// creation while the wizard remains visible. Every Git and network
// operation is supervised by the cancellable blocking worker; the terminal
// event loop only receives the finished plan.
func startCreateSessionPreflight(context *dashboardContext, launch tui.DashboardAction) {
	panic("unported: mj-cli/src/dashboard/actions.rs start_create_session_preflight")
}

// failCreateSessionPreflight ends a creation check that could not start, so
// the review offers Retry instead of waiting on a check that is not
// running.
func failCreateSessionPreflight(context *dashboardContext, failure string) {
	panic("unported: mj-cli/src/dashboard/actions.rs fail_create_session_preflight")
}

func startSessionLaunch(context *dashboardContext, action tui.DashboardAction) {
	startSessionLaunchWithRepositoryPreflight(context, action, nil)
}

func startPreflightedSessionLaunch(context *dashboardContext, action tui.DashboardAction, repositoryPreflight core.ResumeRepositorySourceReceipt) {
	startSessionLaunchWithRepositoryPreflight(context, action, &repositoryPreflight)
}

func startSessionLaunchWithRepositoryPreflight(context *dashboardContext, action tui.DashboardAction, repositoryPreflight *core.ResumeRepositorySourceReceipt) {
	panic("unported: mj-cli/src/dashboard/actions.rs start_session_launch_with_repository_preflight")
}

// markActiveChatRetiring tells the open chat, if it is this session's, that
// the session is being retired on purpose. The stop closes the relay feed,
// and the chat must read that as the expected end rather than a lost actor
// to chase.
func markActiveChatRetiring(activeChat *chat.ActiveChat, sessionID string) {
	if activeChat != nil && activeChat.SessionID() == sessionID {
		activeChat.SetSessionRetiring(true)
	}
}

// beginLifecycleOperation marks a session busy in the UI and hands back
// what runs its operation off the loop. The daemon owns lifecycle/recovery
// serialization.
func (c *dashboardContext) beginLifecycleOperation(sessionID string, kind tui.SessionOperationKind) lifecycleOperationRequest {
	panic("unported: mj-cli/src/dashboard/actions.rs DashboardContext::begin_lifecycle_operation")
}

// spawnDaemonRestart restarts the daemon from this surface, off the event
// loop.
//
// Stopping and starting a daemon takes seconds and holds a file lock, so it
// can never run on the render loop. The outcome is a sentence for the
// notice bar rather than a silent success, for the same reason `mj daemon
// restart` now names the build that came up: a restart that lands on
// someone else's build must not look like the one that was asked for.
func spawnDaemonRestart(context *dashboardContext) {
	panic("unported: mj-cli/src/dashboard/actions.rs spawn_daemon_restart")
}

// spawnDaemonPlacement moves the daemon where a saved `[daemon]` setting
// says, off the event loop.
//
// Startup owns that decision: it compares where the daemon runs with the
// configuration and hands off to a daemon launched in the right place, so
// workers keep running. This only asks it now rather than at the next
// start.
func spawnDaemonPlacement(context *dashboardContext) {
	panic("unported: mj-cli/src/dashboard/actions.rs spawn_daemon_placement")
}

// spawnWebRequest owns each web viewer dialog request, cancels stale
// polling, and observes background task failures.
func spawnWebRequest(context *dashboardContext, action tui.DashboardAction) {
	panic("unported: mj-cli/src/dashboard/actions.rs spawn_web_request")
}

func lifecycleMoveOutcome(reply client.DaemonReply) (core.MoveOutcome, error) {
	panic("unported: mj-cli/src/dashboard/actions.rs lifecycle_move_outcome")
}

func spawnNotificationDismissal(context *dashboardContext, action tui.DashboardAction) {
	panic("unported: mj-cli/src/dashboard/actions.rs spawn_notification_dismissal")
}

// The test-only dispatch_restart is declared with the tests.

// requestNotificationDismissal asks the daemon to dismiss one session's
// notification; the single and the workspace-wide dismissal both send it.
func requestNotificationDismissal(ctx context.Context, sessionID string, target core.NotificationDismissal) (uint64, error) {
	panic("unported: mj-cli/src/dashboard/actions.rs request_notification_dismissal")
}
