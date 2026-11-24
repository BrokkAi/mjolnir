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

	"github.com/BrokkAi/mjolnir/go/internal/core"

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
	dashboard.SetSessionAttentionFor(sessionID, view.Attention)
	var operational *core.RelayOperationalState
	if view.Snapshot != nil {
		operational = view.Snapshot.Operational.Get()
	}
	var currentStepStartedAtMs *int64
	if operational != nil {
		currentStepStartedAtMs = operational.CurrentStepStartedAtMs
	}
	dashboard.SetCurrentStepStart(sessionID, currentStepStartedAtMs)
	activity := client.SessionActivity{}
	if operational != nil {
		activity = client.SessionActivityOf(operational)
	}
	dashboard.SetSessionActivity(sessionID, activity)
	var question *string
	if operational != nil && view.Attention == core.SessionAttentionNeedsInput {
		if message, ok := operational.QuestionMessage(); ok {
			question = &message
		}
	}
	dashboard.SetQuestionMessage(sessionID, question, operational != nil && operational.SupportsOpenRequest())
	dashboard.SetSessionConnectivity(sessionID, view.Connected)
	// The session menu's top line reads the current model and effort.
	var model, effort *string
	if harness, ok := dashboard.SessionHarnessKind(sessionID); ok && operational != nil {
		facts := core.AcpSessionFactsFromOperational(harness, operational.Config, operational.ConfigOptions, operational.Modes)
		if current, ok := facts.CurrentModel(); ok {
			model = &current
		}
		if current, ok := facts.CurrentEffort(); ok {
			effort = &current
		}
	}
	dashboard.SetSessionCurrentConfig(sessionID, model, effort)
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
	applySessionActivity(dashboard, sessionID, view)
	// The dashboard's refresh_subagent_counts (dashboard/drains.rs), which
	// the Go build does not have yet: every open conversation takes the
	// counts the dashboard holds for its session.
	for _, conversation := range chats {
		session := conversation.SessionID()
		conversation.SetSubagentCount(dashboard.SubagentCountFor(session))
		conversation.SetReviewerCounts(dashboard.ReviewerCountsFor(session))
		conversation.SetSubagentWorkingCount(dashboard.WorkingSubagentCountFor(session))
	}
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
	kind := lifecycleKind(lifecycle.Kind)
	operationID := lifecycle.OperationID
	if current, ok := dashboard.SessionOperationKind(lifecycle.SessionID); !ok || current != kind {
		dashboard.BeginSessionOperationAtWithID(lifecycle.SessionID, kind, nil, lifecycle.StartedAtEpochSeconds, &operationID, lifecycle.Cancellable)
	} else {
		dashboard.SetSessionOperationIdentity(lifecycle.SessionID, &operationID, lifecycle.Cancellable)
	}
	stages := make([]tui.ProvisionStageStart, 0, len(lifecycle.ActiveStages))
	for _, stage := range lifecycle.ActiveStages {
		stages = append(stages, tui.ProvisionStageStart{Stage: stage.First, StartedAt: stage.Second})
	}
	dashboard.ReplaceSessionOperationStages(lifecycle.SessionID, stages)
	if destination := lifecycle.ResumeDestination; destination != nil {
		dashboard.SetResumeDestination(lifecycle.SessionID, destination[0], destination[1])
	}
}
