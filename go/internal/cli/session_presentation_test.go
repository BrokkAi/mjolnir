// Tests of session_presentation.go, ported from the tests module of
// mj-cli/src/session_presentation.rs.

package cli

import (
	"slices"
	"strings"
	"testing"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

func presentationDashboard() *tui.DashboardState {
	config, state := presentationDashboardParts()
	return tui.NewDashboardState(config, state, map[string]client.ProfileQuota{})
}

func presentationDashboardParts() (core.Config, core.StateState) {
	config := core.DefaultConfig()
	if config.Profiles == nil {
		config.Profiles = map[string]core.HarnessProfile{}
	}
	if config.Targets == nil {
		config.Targets = map[string]core.TargetTemplate{}
	}
	if config.Bundles == nil {
		config.Bundles = map[string]core.ProjectBundle{}
	}
	config.Profiles["profile-1"] = core.NewHarnessProfile(core.HarnessKindCodex, "/profiles/profile-1")
	config.Targets["target-1"] = core.TargetTemplateLocalBare{}
	github := "owner/project"
	config.Bundles["bundle-1"] = core.ProjectBundle{
		PrimaryRepo: "project",
		Repositories: []core.ProjectRepository{{
			ID:          "project",
			Github:      &github,
			Destination: "project",
		}},
	}
	state := core.NewStateState()
	state.Sessions.Insert("session-1", core.SessionRecord{
		Oracle:           true,
		ID:               "session-1",
		WorkspaceID:      core.DefaultWorkspaceID,
		Title:            "Presentation test",
		HarnessKind:      core.HarnessKindCodex,
		LastProfile:      "profile-1",
		BundleID:         "bundle-1",
		TargetTemplateID: "target-1",
		AdditionalMounts: []core.AdditionalMount{},
		State:            core.SessionStateRunning,
		CreatedAt:        "2026-01-01T00:00:00Z",
		UpdatedAt:        "2026-01-01T00:00:00Z",
	})
	return config, state
}

func presentationOperational(execution core.RelayExecutionState, currentStepStartedAtMs *int64) core.RelayOperationalState {
	protocol := uint32(core.RelayProtocolVersion)
	return core.RelayOperationalState{
		RelayProtocolVersion:   &protocol,
		SessionID:              "session-1",
		Execution:              execution,
		LatestDigest:           core.RelayEventGenesisDigest,
		AcknowledgedDigest:     core.RelayEventGenesisDigest,
		RecoveryFloorDigest:    core.RelayEventGenesisDigest,
		Config:                 map[string]string{},
		CurrentStepStartedAtMs: currentStepStartedAtMs,
	}
}

func presentationManagedView(operational core.RelayOperationalState, connected bool) client.ManagedSessionView {
	materialized := core.MaterializedSessionEmpty("session-1")
	return client.ManagedSessionView{
		Attention: core.SessionAttentionUnreachable,
		Snapshot: &core.ManagedSessionSnapshot{
			Window:       core.ProjectionWindowOf(&materialized),
			Materialized: materialized,
			Operational:  core.NewSharedSnapshot(operational),
		},
		Connected: connected,
	}
}

// presentationDashboardText draws the dashboard with the Sessions pane
// maximized on a 240x30 terminal: its text, row by row, and every cell's
// foreground colour.
func presentationDashboardText(dashboard *tui.DashboardState) (string, []chat.Color) {
	buffer := uv.NewBuffer(240, 30)
	dashboard.SetPaneSize(tui.SupportPaneSessions, core.PaneSizeMaximized)
	tui.RenderCombinedWithTheme(chat.NewFrame(buffer), dashboard, map[string]*chat.ActiveChat{}, map[tui.PaneId]string{}, false, core.UiThemeMidnight)
	text := strings.Join(chat.BufferLines(buffer), "\n")
	var colors []chat.Color
	for y := range buffer.Height() {
		for x := range buffer.Width() {
			colors = append(colors, chat.CellStyle(buffer.CellAt(x, y)).Fg)
		}
	}
	return text, colors
}

func questionMessage(dashboard *tui.DashboardState, sessionID string) *string {
	if message, ok := dashboard.QuestionMessage(sessionID); ok {
		return &message
	}
	return nil
}

func TestActivityProjectionRetainsOpenRequestTargetAcrossLaterAssessments(t *testing.T) {
	dashboard := presentationDashboard()
	operational := presentationOperational(core.RelayExecutionStateIdle, nil)
	assessment := core.TurnAssessmentPending("turn-1", 1, 1, core.CompletionEvidence{StopReason: "EndTurn"})
	assessment.Status = core.StatusAssessed
	awaitInput := core.AssessmentActionAwaitInput
	assessment.Action = &awaitInput
	located := "agent:msg_1"
	assessment.QuestionMessage = &located
	question := assessment.InferredQuestion()
	if question == nil {
		t.Fatal("an assessed await-input turn infers a question")
	}
	operational.OpenRequest = &core.OpenRequest{Question: *question, QuestionMessage: assessment.QuestionMessage}
	first := assessment
	operational.Assessment = &first
	view := presentationManagedView(operational, true)
	view.Attention = core.SessionAttentionNeedsInput
	applySessionActivity(dashboard, "session-1", &view)
	if got := questionMessage(dashboard, "session-1"); got == nil || *got != "agent:msg_1" {
		t.Fatalf("question message = %v, want agent:msg_1", got)
	}

	assessment.TurnID = "later-turn"
	assessment.CompletedOrdinal = 2
	finished := core.AssessmentActionFinished
	assessment.Action = &finished
	assessment.QuestionMessage = nil
	later := assessment
	view.Snapshot.Operational.Mut().Assessment = &later
	applySessionActivity(dashboard, "session-1", &view)
	if got := questionMessage(dashboard, "session-1"); got == nil || *got != "agent:msg_1" {
		t.Fatalf("question message after a later assessment = %v, want agent:msg_1", got)
	}

	view.Attention = core.SessionAttentionWorking
	applySessionActivity(dashboard, "session-1", &view)
	if got := questionMessage(dashboard, "session-1"); got != nil {
		t.Fatalf("question message while working = %q, want none", *got)
	}

	view.Attention = core.SessionAttentionIdle
	view.Snapshot.Operational.Mut().OpenRequest = nil
	applySessionActivity(dashboard, "session-1", &view)
	if got := questionMessage(dashboard, "session-1"); got != nil {
		t.Fatalf("question message once answered = %q, want none", *got)
	}
}

func TestActivityProjectionCapturesCurrentStepAndConnectivity(t *testing.T) {
	dashboard := presentationDashboard()
	verdicts := core.NewSnapshotMap[string, client.SessionVerdict]()
	verdicts.Insert("session-1", client.SessionVerdict{LiveDashboard: true, NeedsPerson: true})
	dashboard.SetSessionVerdicts(verdicts)
	started := int64(1_700_000_000_000)
	view := presentationManagedView(presentationOperational(core.RelayExecutionStateRunning, &started), false)

	applySessionActivity(dashboard, "session-1", &view)

	text, colors := presentationDashboardText(dashboard)
	if !strings.Contains(text, "Unreachable") {
		t.Errorf("connection status missing from %q", text)
	}
	if strings.Contains(text, "Working ") {
		t.Error("stale activity must not hide the connection failure")
	}
	if !slices.Contains(colors, chat.ThemePalette().Error) {
		t.Error("disconnected row was not marked red")
	}
	view.Connected = true
	view.Attention = core.SessionAttentionWorking
	applySessionActivity(dashboard, "session-1", &view)
	text, _ = presentationDashboardText(dashboard)
	if !strings.Contains(text, "Working ") {
		t.Errorf("live SDK step clock missing from %q", text)
	}
	if strings.Contains(text, "Unreachable") {
		t.Error("a connected session still reads Unreachable")
	}
}

// The runtime feed can update the count without publishing changed records.
// Hard-won: 6b8110e29c: the parent footer stayed at 0/M through a child turn.
func TestARuntimeVerdictUpdatesTheParentFooterWorkingCount(t *testing.T) {
	t.Skip("needs mj-cli's dashboard/drains.rs (not yet ported): refresh_subagent_counts")
}
