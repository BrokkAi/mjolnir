// Port of mj-tui/src/ingest/tests.rs.

package tui

import (
	"encoding/json"
	"reflect"
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ingestDetail is `dashboard.session_details[id]`: it fails the test when
// the session has no detail.
func ingestDetail(t *testing.T, dashboard *DashboardState, sessionID string) *SessionDetail {
	t.Helper()
	detail, ok := (*dashboard.sessionDetails.Get())[sessionID]
	if !ok {
		t.Fatalf("no session detail for %s", sessionID)
	}
	return detail
}

// ingestSetViewedThrough sets a held record's read cursor, as the Rust tests
// do through `state.sessions.get_mut(..)`.
func ingestSetViewedThrough(t *testing.T, state *core.StateState, sessionID string, ordinal uint64) {
	t.Helper()
	record := state.Sessions.GetMut(sessionID)
	if record == nil {
		t.Fatalf("no record for %s", sessionID)
	}
	record.ViewedThroughEventOrdinal = ordinal
}

func ingestText(value *string) string {
	if value == nil {
		return "<none>"
	}
	return *value
}

func ingestString(value string) *string {
	return &value
}

func TestDelayedProjectionAndSummaryCannotReviveAcknowledgedContent(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	materialized := materializedSessionFor("session-1", []*core.TranscriptItem{agentMessage(2, "done"), workInterruption(3)})
	prepared := PreparedMaterializedSessionDetailFromMaterialized(materialized, 0, MaterializedProjectionCache{})
	summary := PreparedMaterializedSessionSummaryFromMaterialized(core.MaterializedSessionSummary{
		SessionID:                         "session-1",
		AppliedEventOrdinal:               3,
		Execution:                         core.MaterializedExecutionStateIdle{},
		LastAgentMessage:                  ingestString("done"),
		LastAgentMessageFollowsLastUser:   true,
		AgentMessageLatestContentOrdinals: []uint64{2},
		InterruptionEventOrdinals:         []uint64{3},
	}, 0)
	ingestSetViewedThrough(t, dashboard.state.Mut(), "session-1", 3)
	if !dashboard.ApplyPreparedMaterializedSessionSummary(summary) {
		t.Fatal("the summary should apply")
	}
	detail := ingestDetail(t, dashboard, "session-1")
	if detail.unreadAgentMessages != 0 || detail.unreadInterruptions != 0 {
		t.Fatalf("unread = (%d, %d), want (0, 0)", detail.unreadAgentMessages, detail.unreadInterruptions)
	}
	if !dashboard.ApplyPreparedMaterializedSession(prepared) {
		t.Fatal("the projection should apply")
	}
	detail = ingestDetail(t, dashboard, "session-1")
	if detail.unreadAgentMessages != 0 || detail.unreadInterruptions != 0 {
		t.Fatalf("unread = (%d, %d), want (0, 0)", detail.unreadAgentMessages, detail.unreadInterruptions)
	}
	newer := materializedSessionFor("session-1", []*core.TranscriptItem{
		agentMessage(2, "done"),
		workInterruption(3),
		agentMessage(4, "new reply"),
	})
	dashboard.ApplyMaterializedSession(&newer)
	if got := ingestDetail(t, dashboard, "session-1").unreadAgentMessages; got != 1 {
		t.Fatalf("unread agent messages = %d, want 1", got)
	}
}

// Hard-won: e063b7f41d77: a background config refresh dismissed newer palette interactions.
func TestChangedConfigPreservesANewPaletteAndItsQuery(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	savedConfig := *dashboard.config.Get()
	savedConfig.Advanced.DetailedActivityClocks = !savedConfig.Advanced.DetailedActivityClocks
	openPalette(dashboard)
	dashboard.HandlePaste("rename")
	dashboard.SetConfig(savedConfig)
	if !reflect.DeepEqual(*dashboard.config.Get(), savedConfig) {
		t.Fatal("the saved configuration should be in force")
	}
	if _, ok := dashboard.mode.(ModePalette); !ok {
		t.Fatalf("mode = %T, want the palette", dashboard.mode)
	}
	dashboard.HandleKey(key(tea.KeyEnter))
	if _, ok := dashboard.mode.(ModeRename); !ok {
		t.Fatalf("mode = %T, want the rename editor", dashboard.mode)
	}

	session := stoppedSession()
	session.State = core.SessionStateRunning
	unchanged := dashboardWithSession(session)
	saved := *unchanged.config.Get()
	openPalette(unchanged)
	unchanged.HandlePaste("container")
	unchanged.SetConfig(saved)
	if _, ok := unchanged.mode.(ModePalette); !ok {
		t.Fatalf("mode = %T, want the palette", unchanged.mode)
	}
	unchanged.HandleKey(key(tea.KeyEnter))
	if _, ok := unchanged.mode.(ModeEditContainer); !ok {
		t.Fatalf("mode = %T, want the container editor", unchanged.mode)
	}
}

func TestResumeIsProjectedIntoActiveWhileBackgroundWorkRuns(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSessionOperation("session-1", SessionOperationKindResuming, nil)

	if state := dashboard.state.Get().Sessions.Index("session-1").State; state != core.SessionStateStopped {
		t.Fatalf("state = %v, want stopped", state)
	}
	if got := len(dashboard.orderedSessions()); got != 1 {
		t.Fatalf("ordered sessions = %d, want 1", got)
	}
	if kind := (*dashboard.sessionOperations.Get())["session-1"].kind; kind != SessionOperationKindResuming {
		t.Fatalf("operation = %v, want resuming", kind)
	}
}

func TestACompletedRemoteLaunchRestoresTheReadyRowWithoutAnotherRecordChange(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSessionOperation("session-1", SessionOperationKindLaunching, nil)
	ready := stoppedSession()
	ready.State = core.SessionStateRunning
	state := *dashboard.state.Get()
	state.Sessions.Insert(ready.ID, ready)
	dashboard.SetState(state)
	if dashboard.TransitionKind("session-1") == nil {
		t.Fatal("the launch should still show its transition")
	}
	dashboard.FinishSessionOperation("session-1")
	if kind := dashboard.TransitionKind("session-1"); kind != nil {
		t.Fatalf("transition = %v, want none", *kind)
	}
	if state := dashboard.orderedSessions()[0].State; state != core.SessionStateRunning {
		t.Fatalf("state = %v, want running", state)
	}
	dashboard.SelectActiveSession("session-1")
	if action, ok := dashboard.openSelectedSession().(DashboardActionOpen); !ok {
		t.Fatalf("action = %#v, want Open", action)
	}
}

func TestNoticeReplacementDoesNotOverwriteANewerNotice(t *testing.T) {
	dashboard := NewDashboardState(config(), core.StateState{}, map[string]client.ProfileQuota{})
	dashboard.SetNotice("Refreshing profile quotas…")
	if !dashboard.ReplaceNoticeIf("Refreshing profile quotas…", "Profile quotas refreshed.") {
		t.Fatal("the matching notice should be replaced")
	}
	if notice, _ := dashboard.Notice(); notice != "Profile quotas refreshed." {
		t.Fatalf("notice = %q", notice)
	}

	dashboard.SetNotice("A later operation failed")
	if dashboard.ReplaceNoticeIf("Refreshing profile quotas…", "Profile quotas refreshed.") {
		t.Fatal("a newer notice must not be replaced")
	}
	if notice, _ := dashboard.Notice(); notice != "A later operation failed" {
		t.Fatalf("notice = %q", notice)
	}
}

func TestRuntimeReviewProjectionRestoresAndRemovesSessionActivity(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	review := client.RuntimeReviewView{
		ReviewID:  "",
		Roots:     []string{},
		SessionID: "session-1",
		Questions: nil,
		Phase:     core.TurnReviewPhaseLaunchingReviewer{},
		Roles:     []core.RoleStatus{},
		Status:    "starting the reviewer…",
	}

	dashboard.SetSessionReviews([]client.RuntimeReviewView{review})
	if len(dashboard.sessionReviews("session-1")) == 0 {
		t.Fatal("the review should be listed")
	}

	// A session keeps every review it has open: a settled one does not hide
	// the one still working, and the row shows the working one.
	settled := review
	settled.ReviewID = "review-a"
	settled.Roots = []string{"/workspace/a"}
	settled.Phase = core.TurnReviewPhaseVerdict{Verdict: core.ReviewVerdictFindings{Synthesis: "[P2] weak test"}}
	dashboard.SetSessionReviews([]client.RuntimeReviewView{settled, review})
	if got := len(dashboard.sessionReviews("session-1")); got != 2 {
		t.Fatalf("reviews = %d, want 2", got)
	}
	if label, ok := reviewStatusLabel(dashboard.sessionReviews("session-1")); !ok || label != "Reviewing" {
		t.Fatalf("status label = %q, %v", label, ok)
	}

	// Runtime snapshots are complete projections: an empty replacement
	// closes the badge rather than leaving the previous review stuck.
	dashboard.SetSessionReviews(nil)
	if len(dashboard.sessionReviews("session-1")) != 0 {
		t.Fatal("an empty projection should close every review")
	}
}

// The dashboard and every other view (chat, background workers) share one
// notifications bar: a clone installed with ShareNotices sees what the
// dashboard sets, and the dashboard sees what the clone sets.
//
// Hard-won: f3705cca23cc: background notices were invisible in chat because each view had its own slot.
func TestASharedNoticeIsVisibleThroughEveryCloneOfTheHandle(t *testing.T) {
	dashboard := NewDashboardState(config(), core.StateState{}, map[string]client.ProfileQuota{})
	shared := chat.NewNotices()
	dashboard.ShareNotices(shared)

	dashboard.SetNotice("Background import finished")
	if notice, ok := shared.Current(); !ok || notice != "Background import finished" {
		t.Fatalf("shared notice = %q, %v", notice, ok)
	}

	shared.Clear()
	if notice, ok := dashboard.Notice(); ok {
		t.Fatalf("notice = %q, want none", notice)
	}

	shared.Set("Quota refresh finished")
	if notice, ok := dashboard.Notice(); !ok || notice != "Quota refresh finished" {
		t.Fatalf("notice = %q, %v", notice, ok)
	}
}

func TestUnreadCountUsesLogicalAgentPositionsAfterTheDetachCursor(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)
	applyMaterializedTranscript(dashboard, []*core.TranscriptItem{
		agentMessage(1, "first message"),
		thought(3, "thinking"),
		agentMessage(4, "second message"),
	})

	if got := ingestDetail(t, dashboard, "session-1").unreadAgentMessages; got != 2 {
		t.Fatalf("unread agent messages = %d, want 2", got)
	}

	state := *dashboard.state.Get()
	ingestSetViewedThrough(t, &state, "session-1", 1)
	dashboard.SetState(state)
	if got := ingestDetail(t, dashboard, "session-1").unreadAgentMessages; got != 1 {
		t.Fatalf("unread agent messages = %d, want 1", got)
	}

	state = *dashboard.state.Get()
	ingestSetViewedThrough(t, &state, "session-1", 4)
	dashboard.SetState(state)
	if got := ingestDetail(t, dashboard, "session-1").unreadAgentMessages; got != 0 {
		t.Fatalf("unread agent messages = %d, want 0", got)
	}
}

func TestPendingQuestionOrdinalStaysPairedWhenANewerSummaryArrives(t *testing.T) {
	session := materializedSessionFor("session-1", nil)
	session.AppliedEventOrdinal = 5
	request, err := core.ElicitationRequestFromACPParams("request-1", json.RawMessage(`{
		"mode": "form",
		"sessionId": "session-1",
		"message": "Choose a path",
		"requestedSchema": {
			"type": "object",
			"properties": {"path": {"type": "string"}}
		}
	}`))
	if err != nil {
		t.Fatalf("valid test question: %v", err)
	}
	session.PendingElicitations = []core.ElicitationRequest{request}
	dashboard := dashboardWithSession(runningSession())
	dashboard.ApplyMaterializedSession(&session)
	if ordinal, _, ok := dashboard.PendingElicitations("session-1"); !ok || ordinal != 5 {
		t.Fatalf("pending ordinal = %d, %v, want 5", ordinal, ok)
	}

	summary := core.MaterializedSessionSummary{
		SessionID:                         "session-1",
		AppliedEventOrdinal:               6,
		Execution:                         core.MaterializedExecutionStateIdle{},
		AgentMessageLatestContentOrdinals: []uint64{},
		InterruptionEventOrdinals:         []uint64{},
	}
	dashboard.ApplyPreparedMaterializedSessionSummary(PreparedMaterializedSessionSummaryFromMaterialized(summary, 0))
	if ordinal, _, ok := dashboard.PendingElicitations("session-1"); !ok || ordinal != 5 {
		t.Fatalf("pending ordinal = %d, %v, want 5", ordinal, ok)
	}
}

func TestAdjacentInterruptionMarkersKeepEachOrdinalForUnreadTracking(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)
	materialized := materializedSessionFor("session-1", []*core.TranscriptItem{workInterruption(3), workInterruption(4)})
	materialized.AppliedEventOrdinal = 4
	dashboard.ApplyMaterializedSession(&materialized)

	detail := ingestDetail(t, dashboard, "session-1")
	if !slices.Equal(detail.interruptionEventOrdinals, []uint64{3, 4}) {
		t.Fatalf("interruption ordinals = %v, want [3 4]", detail.interruptionEventOrdinals)
	}
	if detail.unreadInterruptions != 2 {
		t.Fatalf("unread interruptions = %d, want 2", detail.unreadInterruptions)
	}

	state := *dashboard.state.Get()
	ingestSetViewedThrough(t, &state, "session-1", 3)
	dashboard.SetState(state)
	if got := ingestDetail(t, dashboard, "session-1").unreadInterruptions; got != 1 {
		t.Fatalf("reading through the older marker leaves the newer ordinal unread: unread = %d, want 1", got)
	}
}

func TestMaterializedMessageUpdateDoesNotDuplicateUnreadCount(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)
	initial := materializedSessionFor("session-1", []*core.TranscriptItem{agentMessage(1, "first ")})
	initial.QueuedPrompts = append(initial.QueuedPrompts, core.MaterializedQueuedPrompt{
		CommandID:  "queued-1",
		Kind:       core.QueuedCommandKindPrompt{},
		Content:    []any{map[string]any{"type": "text", "text": "next task"}},
		QueuedAtMs: 0,
	})
	dashboard.ApplyMaterializedSession(&initial)

	state := *dashboard.state.Get()
	ingestSetViewedThrough(t, &state, "session-1", 1)
	dashboard.SetState(state)
	if got := ingestDetail(t, dashboard, "session-1").unreadAgentMessages; got != 0 {
		t.Fatalf("unread agent messages = %d, want 0", got)
	}

	updated := agentMessage(1, "first continuation")
	ordinal := uint64(2)
	updated.LatestContentEventOrdinal = &ordinal
	updated.LastChangedAtMs = 2_000
	projection := materializedSessionFor("session-1", []*core.TranscriptItem{updated})
	projection.AppliedEventOrdinal = 2
	dashboard.ApplyMaterializedSession(&projection)

	detail := ingestDetail(t, dashboard, "session-1")
	if detail.unreadAgentMessages != 1 {
		t.Fatalf("unread agent messages = %d, want 1", detail.unreadAgentMessages)
	}
	if got := ingestText(detail.lastAgentMessage); got != "first continuation" {
		t.Fatalf("last agent message = %q", got)
	}
	if len(detail.queuedPrompts) != 0 {
		t.Fatalf("queued prompts = %v, want none", detail.queuedPrompts)
	}

	state = *dashboard.state.Get()
	ingestSetViewedThrough(t, &state, "session-1", 2)
	dashboard.SetState(state)
	if got := ingestDetail(t, dashboard, "session-1").unreadAgentMessages; got != 0 {
		t.Fatalf("unread agent messages = %d, want 0", got)
	}
}

func TestPreparedMaterializedSessionDropsStaleOrdinals(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)
	latest := materializedSessionFor("session-1", []*core.TranscriptItem{agentMessage(2, "latest")})
	latest.AppliedEventOrdinal = 2
	stale := materializedSessionFor("session-1", []*core.TranscriptItem{agentMessage(1, "stale")})
	stale.AppliedEventOrdinal = 1

	if !dashboard.ApplyPreparedMaterializedSession(PreparedMaterializedSessionDetailFromMaterialized(latest, 0, MaterializedProjectionCache{})) {
		t.Fatal("the latest projection should apply")
	}
	if dashboard.ApplyPreparedMaterializedSession(PreparedMaterializedSessionDetailFromMaterialized(stale, 0, MaterializedProjectionCache{})) {
		t.Fatal("a stale projection must not apply")
	}

	if got := ingestText(ingestDetail(t, dashboard, "session-1").lastAgentMessage); got != "latest" {
		t.Fatalf("last agent message = %q, want latest", got)
	}
}

func TestStoredSummaryRestoresDashboardMessagesWithoutMarkingTranscriptReady(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	session.ViewedThroughEventOrdinal = 3
	dashboard := dashboardWithSession(session)
	activity := int64(8_000)
	summary := core.MaterializedSessionSummary{
		SessionID:                         "session-1",
		AppliedEventOrdinal:               7,
		LastActivityAtMs:                  &activity,
		Execution:                         core.MaterializedExecutionStateRunning{StartedAtMs: 4_000},
		SessionTitle:                      ingestString("Persisted title"),
		LastAgentMessage:                  ingestString("Persisted answer"),
		LastUserMessage:                   ingestString("Persisted question"),
		LastAgentMessageFollowsLastUser:   true,
		AgentMessageLatestContentOrdinals: []uint64{2, 5, 7},
		InterruptionEventOrdinals:         []uint64{6},
	}

	if !dashboard.ApplyPreparedMaterializedSessionSummary(PreparedMaterializedSessionSummaryFromMaterialized(summary, 3)) {
		t.Fatal("the summary should apply")
	}

	detail := ingestDetail(t, dashboard, "session-1")
	if got := ingestText(detail.lastUserMessage); got != "Persisted question" {
		t.Fatalf("last user message = %q", got)
	}
	if got := ingestText(detail.lastAgentMessage); got != "Persisted answer" {
		t.Fatalf("last agent message = %q", got)
	}
	if detail.unreadAgentMessages != 2 {
		t.Fatalf("unread agent messages = %d, want 2", detail.unreadAgentMessages)
	}
	if detail.unreadInterruptions != 1 {
		t.Fatalf("unread interruptions = %d, want 1", detail.unreadInterruptions)
	}
	if !detail.lastAgentMessageFollowsLastUser {
		t.Fatal("the answer should follow the question")
	}
	if detail.currentTurnStartedAt == nil || *detail.currentTurnStartedAt != 4 {
		t.Fatalf("turn start = %v, want 4", detail.currentTurnStartedAt)
	}
	if detail.transcriptHydration != TranscriptHydrationSummary {
		t.Fatalf("hydration = %v, want summary", detail.transcriptHydration)
	}
	if detail.transcript != nil {
		t.Fatal("a summary must not carry a transcript")
	}
}

// Hard-won: e3e9d9fc21: the TUI named sessions from its own transcript copy, so its names disagreed with the web viewer's.
func TestDashboardNamesASessionOnlyFromTheDaemonRecord(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	summary := core.MaterializedSessionSummary{
		SessionID:                         "session-1",
		AppliedEventOrdinal:               1,
		Execution:                         core.MaterializedExecutionStateIdle{},
		SessionTitle:                      ingestString("Transcript title"),
		AgentMessageLatestContentOrdinals: []uint64{},
		InterruptionEventOrdinals:         []uint64{},
	}
	if !dashboard.ApplyPreparedMaterializedSessionSummary(PreparedMaterializedSessionSummaryFromMaterialized(summary, 0)) {
		t.Fatal("the summary should apply")
	}
	materialized := materializedSessionFor("session-1", []*core.TranscriptItem{agentMessage(2, "done")})
	materialized.SessionTitle = ingestString("Transcript title")
	dashboard.ApplyMaterializedSession(&materialized)

	lines := drawn(dashboard, 120, 40)
	if !slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, "ACP pretty name") }) {
		t.Fatalf("the record's name is missing:\n%s", strings.Join(lines, "\n"))
	}
	if slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, "Transcript title") }) {
		t.Fatalf("the transcript's title must not name the session:\n%s", strings.Join(lines, "\n"))
	}
}

func TestLateStartupSummaryDoesNotEraseLiveToolPreviewAtTheSameFrontier(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	snapshot := materializedSessionFor("session-1", []*core.TranscriptItem{thought(2, "Checking files")})
	dashboard.ApplyMaterializedSession(&snapshot)
	summary := core.MaterializedSessionSummary{
		SessionID:                         "session-1",
		AppliedEventOrdinal:               snapshot.AppliedEventOrdinal,
		LastActivityAtMs:                  snapshot.LastActivityAtMs,
		Execution:                         snapshot.Execution,
		AgentMessageLatestContentOrdinals: []uint64{},
		InterruptionEventOrdinals:         []uint64{},
	}
	if dashboard.ApplyPreparedMaterializedSessionSummary(PreparedMaterializedSessionSummaryFromMaterialized(summary, 0)) {
		t.Fatal("a summary at the same frontier must not apply")
	}
	if got := ingestText(ingestDetail(t, dashboard, "session-1").latestAgentActivityAfterLastUser); got != "Checking files" {
		t.Fatalf("latest activity = %q", got)
	}
}

func TestStoredSummaryElidesHiddenContextFromTheUserPreview(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	activity := int64(2_000)
	summary := core.MaterializedSessionSummary{
		SessionID:           "session-1",
		AppliedEventOrdinal: 2,
		LastActivityAtMs:    &activity,
		Execution:           core.MaterializedExecutionStateIdle{},
		LastUserMessage: ingestString("<hel-project-memory>private</hel-project-memory>\n" +
			"Visible question"),
		AgentMessageLatestContentOrdinals: []uint64{},
		InterruptionEventOrdinals:         []uint64{},
	}

	if !dashboard.ApplyPreparedMaterializedSessionSummary(PreparedMaterializedSessionSummaryFromMaterialized(summary, 0)) {
		t.Fatal("the summary should apply")
	}

	if got := ingestText(ingestDetail(t, dashboard, "session-1").lastUserMessage); got != "Visible question" {
		t.Fatalf("last user message = %q", got)
	}
}

// setAgentText rewrites one agent message the way the projection does: the
// item is copied, so every other handle in the transcript survives.
func setAgentText(item **core.TranscriptItem, text string, contentOrdinal uint64) {
	copied := **item
	chunk, err := json.Marshal(map[string]any{"content": map[string]any{"type": "text", "text": text}})
	if err != nil {
		panic(err)
	}
	copied.Body = core.TranscriptBodyAgent{Chunks: []json.RawMessage{chunk}, Streaming: false}
	copied.LatestContentEventOrdinal = &contentOrdinal
	copied.LastChangedAtMs = int64(contentOrdinal) * 1_000
	*item = &copied
}

// The projection reuses per-item results across updates, so every shape of
// transcript change must land where a full rescan would.
func TestIncrementalProjectionMatchesAFullRescanThroughTranscriptChanges(t *testing.T) {
	const viewedThroughEventOrdinal = 1
	// One transcript, changed the way the projection changes it: items are
	// appended, and an item that changes is replaced by a copy while the
	// rest keep their handles.
	transcript := []*core.TranscriptItem{}
	updates := [][]*core.TranscriptItem{slices.Clone(transcript)}
	transcript = append(transcript, agentMessage(1, "first"), thought(2, "thinking"))
	updates = append(updates, slices.Clone(transcript))
	transcript = append(transcript, agentMessage(3, "answer"))
	updates = append(updates, slices.Clone(transcript))
	// More content streams into the tail message.
	setAgentText(&transcript[2], "answer, at length", 4)
	updates = append(updates, slices.Clone(transcript))
	// The tail message loses its text, so the previous answer no longer holds
	// and the earlier items have to decide it.
	setAgentText(&transcript[2], "   ", 5)
	updates = append(updates, slices.Clone(transcript))
	// An item inside the unchanged prefix changes.
	setAgentText(&transcript[0], "first, corrected", 6)
	updates = append(updates, slices.Clone(transcript))
	transcript = append(transcript, workInterruption(7))
	updates = append(updates, slices.Clone(transcript))
	// `/clear` starts a new conversation; the earlier messages stay in the
	// transcript but leave the dashboard preview.
	boundary := transcriptItem(8, core.TranscriptBodySystem{Text: "Context cleared"})
	boundary.StableID = core.ContextBoundaryPrefix + "clear-1"
	transcript = append(transcript, boundary)
	clearedUpdate := len(updates)
	updates = append(updates, slices.Clone(transcript))
	transcript = append(transcript, agentMessage(9, "fresh start"))
	updates = append(updates, slices.Clone(transcript))
	// A restore rebuilds every item, sharing no handles.
	transcript = []*core.TranscriptItem{agentMessage(1, "restored"), agentMessage(2, "and again")}
	updates = append(updates, slices.Clone(transcript))
	// A checkpoint restore leaves a shorter transcript.
	transcript = transcript[:1]
	updates = append(updates, transcript)

	cache := MaterializedProjectionCache{}
	for index, transcript := range updates {
		session := materializedSessionFor("session-1", transcript)
		incremental := PreparedMaterializedSessionDetailFromMaterialized(session, viewedThroughEventOrdinal, cache)
		rescanned := PreparedMaterializedSessionDetailFromMaterialized(session, viewedThroughEventOrdinal, MaterializedProjectionCache{})
		if ingestText(incremental.lastAgentMessage) != ingestText(rescanned.lastAgentMessage) {
			t.Fatalf("last agent message after update %d: %q, want %q", index,
				ingestText(incremental.lastAgentMessage), ingestText(rescanned.lastAgentMessage))
		}
		if !slices.Equal(incremental.agentMessageLatestContentOrdinals, rescanned.agentMessageLatestContentOrdinals) {
			t.Fatalf("agent ordinals after update %d: %v, want %v", index,
				incremental.agentMessageLatestContentOrdinals, rescanned.agentMessageLatestContentOrdinals)
		}
		if incremental.unreadAgentMessages != rescanned.unreadAgentMessages {
			t.Fatalf("unread count after update %d: %d, want %d", index,
				incremental.unreadAgentMessages, rescanned.unreadAgentMessages)
		}
		if !slices.Equal(incremental.interruptionEventOrdinals, rescanned.interruptionEventOrdinals) {
			t.Fatalf("interruption ordinals after update %d: %v, want %v", index,
				incremental.interruptionEventOrdinals, rescanned.interruptionEventOrdinals)
		}
		if incremental.unreadInterruptions != rescanned.unreadInterruptions {
			t.Fatalf("unread interruption count after update %d: %d, want %d", index,
				incremental.unreadInterruptions, rescanned.unreadInterruptions)
		}
		if index == clearedUpdate {
			if incremental.lastAgentMessage != nil || incremental.lastUserMessage != nil ||
				incremental.latestAgentActivityAfterLastUser != nil {
				t.Fatalf("the cleared conversation still previews: %q, %q, %q",
					ingestText(incremental.lastAgentMessage), ingestText(incremental.lastUserMessage),
					ingestText(incremental.latestAgentActivityAfterLastUser))
			}
		}
		if index == clearedUpdate+1 {
			if got := ingestText(incremental.lastAgentMessage); got != "fresh start" {
				t.Fatalf("last agent message after the clear = %q, want fresh start", got)
			}
		}
		cache = incremental.projection
	}
}

func TestProjectionCacheKeepsTerminalDiffstatsAcrossUnrelatedUpdates(t *testing.T) {
	call := core.OpaqueToolCallFromValue(json.RawMessage(`{
		"toolCallId": "edit",
		"title": "Edit src/lib.rs",
		"status": "completed",
		"content": [{
			"type": "diff",
			"path": "/workspace/src/lib.rs",
			"oldText": "alpha\n",
			"newText": "alpha\nbeta\n"
		}]
	}`))
	tool := &core.TranscriptItem{
		StableID:        "tool:edit",
		Position:        1,
		CreatedAtMs:     1,
		LastChangedAtMs: 2,
		Body:            core.TranscriptBodyTool{Call: call},
	}
	first := PreparedMaterializedSessionDetailFromMaterialized(
		materializedSessionFor("session-1", []*core.TranscriptItem{tool}),
		0,
		MaterializedProjectionCache{},
	)
	if got := len(first.projection.toolDiffstats); got != 1 {
		t.Fatalf("tool diffstats = %d, want 1", got)
	}

	second := PreparedMaterializedSessionDetailFromMaterialized(
		materializedSessionFor("session-1", []*core.TranscriptItem{tool, agentMessage(2, "unrelated update")}),
		0,
		first.projection,
	)
	if got := len(second.projection.toolDiffstats); got != 1 {
		t.Fatalf("tool diffstats = %d, want 1", got)
	}
	lines := second.transcript.BrowserTranscript(nil).Entries[0].Lines
	if want := []string{"Edit", "/workspace/src/lib.rs  +1 −0"}; !slices.Equal(lines, want) {
		t.Fatalf("lines = %q, want %q", lines, want)
	}
}

// Hard-won: e063b7f41d77: sessions without a user row showed an empty sidebar preview despite live activity.
func TestTranscriptWithoutUserEntriesKeepsAgentActivityForTheSidebar(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	applyMaterializedTranscript(dashboard, []*core.TranscriptItem{thought(2, "Checking the workspace")})
	detail := ingestDetail(t, dashboard, "session-1")
	if detail.lastUserMessage != nil {
		t.Fatalf("last user message = %q, want none", *detail.lastUserMessage)
	}
	if got := ingestText(detail.latestAgentActivityAfterLastUser); got != "Checking the workspace" {
		t.Fatalf("latest activity = %q", got)
	}
	applyMaterializedTranscript(dashboard, []*core.TranscriptItem{agentMessage(3, "The checks passed")})
	detail = ingestDetail(t, dashboard, "session-1")
	if detail.lastUserMessage != nil {
		t.Fatalf("last user message = %q, want none", *detail.lastUserMessage)
	}
	if !detail.lastAgentMessageFollowsLastUser {
		t.Fatal("an answer with no question follows the last user message")
	}
	if got := ingestText(detail.lastAgentMessage); got != "The checks passed" {
		t.Fatalf("last agent message = %q", got)
	}
}

func TestMaterializedIdleStateClearsAStaleTurnClock(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	running := core.MaterializedSessionEmpty("session-1")
	running.Execution = core.MaterializedExecutionStateRunning{StartedAtMs: 1_000_000}
	dashboard.ApplyMaterializedSession(&running)
	idle := core.MaterializedSessionEmpty("session-1")
	dashboard.ApplyMaterializedSession(&idle)

	if started := ingestDetail(t, dashboard, "session-1").currentTurnStartedAt; started != nil {
		t.Fatalf("turn start = %d, want none", *started)
	}
}

func TestFailedClosingRecordIsRecoveryStatusNotAnActiveTransition(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateClosing
	session.LastError = ingestString("checkpoint copy failed")
	dashboard := dashboardWithSession(session)

	if kind := dashboard.TransitionKind("session-1"); kind != nil {
		t.Fatalf("transition = %v, want none", *kind)
	}
	kind := dashboard.TransitionFailureKind("session-1")
	if kind == nil {
		t.Fatal("failure kind = none, want suspending")
	}
	if *kind != core.SessionTransitionKindSuspending {
		t.Fatalf("failure kind = %v, want suspending", *kind)
	}
}
