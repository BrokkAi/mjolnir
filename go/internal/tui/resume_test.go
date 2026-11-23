// Port of mj-tui/src/resume/tests.rs.

package tui

import (
	"errors"
	"fmt"
	"maps"
	"reflect"
	"slices"
	"strconv"
	"strings"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// The mouse gestures the tests press, as test_support's mouseAt takes them
// (crossterm's MouseEventKind).
var (
	resumeLeftDown  tea.MouseMsg = tea.MouseClickMsg{Button: tea.MouseLeft}
	resumeLeftUp    tea.MouseMsg = tea.MouseReleaseMsg{Button: tea.MouseLeft}
	resumeLeftDrag  tea.MouseMsg = tea.MouseMotionMsg{Button: tea.MouseLeft}
	resumeWheelDown tea.MouseMsg = tea.MouseWheelMsg{Button: tea.MouseWheelDown}
)

// resumeMergedRows is the rows for state as the dialog builds them once the
// daemon has answered with the stopped sessions in it.
func resumeMergedRows(config *core.Config, state *core.StateState, profiles []ImportProfileOption, wiki []client.WikiRow) []ResumeRow {
	history := resumeHistoryFrom(resumeCandidatesFor(config, state))
	return mergedResumeRows(config, state, HistoryLoadLoaded{Value: &history}, profiles, wiki)
}

// resumeNewerThanTheCheckpoint is later than stoppedSession's checkpoint, so
// a native row built with it sorts above the Hel record.
const resumeNewerThanTheCheckpoint int64 = 4_000_000_000_000

func resumeNative(id, title string, lastActivityMs int64) ImportSessionOption {
	return ImportSessionOption{
		NativeSessionID:  id,
		Title:            title,
		Cwd:              "/home/dev/Projects/hel",
		ProjectDirectory: "~/Projects/hel",
		Details:          "master · 1.0KB · ~/Projects/hel",
		LastActivityMs:   lastActivityMs,
	}
}

func resumeCodexProfile(sessions []ImportSessionOption) ImportProfileOption {
	return ImportProfileOption{
		ProfileID:    "codex-1",
		HarnessKind:  core.HarnessKindCodex,
		Sessions:     sessions,
		ScanProgress: &[2]int{1, 1},
	}
}

func resumeAppendGoldenState(output *strings.Builder, label string, width, height int, lines []string) {
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, width, height)
	output.WriteString(strings.Join(lines, "\n"))
	output.WriteString("\n")
}

// resumeAppendGoldenValue writes one value the way Rust's `{:?}` writes it.
func resumeAppendGoldenValue(output *strings.Builder, label string, value any) {
	fmt.Fprintf(output, "%s: %s\n", label, resumeDebug(value))
}

// resumeDebug is Rust's Debug formatting of the values the goldens record.
func resumeDebug(value any) string {
	quoted := func(items []string) string {
		parts := make([]string, len(items))
		for index, item := range items {
			parts[index] = strconv.Quote(item)
		}
		return "[" + strings.Join(parts, ", ") + "]"
	}
	switch value := value.(type) {
	case string:
		return strconv.Quote(value)
	case bool:
		return strconv.FormatBool(value)
	case []string:
		return quoted(value)
	case []int:
		parts := make([]string, len(value))
		for index, count := range value {
			parts[index] = strconv.Itoa(count)
		}
		return "[" + strings.Join(parts, ", ") + "]"
	case [2]string:
		return "(" + strconv.Quote(value[0]) + ", " + strconv.Quote(value[1]) + ")"
	case nil, DashboardActionNone:
		return "None"
	case DashboardActionImportSession:
		return fmt.Sprintf(
			"ImportSession { profile_id: %s, native_session_id: %s, display_title: %s }",
			strconv.Quote(value.ProfileID), strconv.Quote(value.NativeSessionID), strconv.Quote(value.DisplayTitle),
		)
	case DashboardActionSearchArchivedSessions:
		return fmt.Sprintf("SearchArchivedSessions { request_id: %d, query: %s }", value.RequestID, strconv.Quote(value.Query))
	case DashboardActionLoadArchivedHits:
		return fmt.Sprintf("LoadArchivedHits { wiki_id: %s, query: %s }", strconv.Quote(value.WikiID), strconv.Quote(value.Query))
	case DashboardActionLoadArchivedBrief:
		return fmt.Sprintf("LoadArchivedBrief { wiki_id: %s }", strconv.Quote(value.WikiID))
	}
	return fmt.Sprintf("%#v", value)
}

// resumeTabDebug is Rust's Debug of a ResumeTab.
func resumeTabDebug(tab ResumeTab) string {
	return [...]string{"Live", "Hel", "Import", "Archive"}[tab.index()]
}

func resumeAppendGolden(output *strings.Builder, label string, width, height int, dashboard *DashboardState) {
	lines := drawn(dashboard, width, height)
	resumeAppendGoldenState(output, label, width, height, lines)
}

// resumeRenderedBuffer is Rust's drawing through `crate::render::render`
// into a TestBackend: the combined surface with no conversation on it, for
// tests that read the drawn cells or the surfaces the frame registered.
func resumeRenderedBuffer(dashboard *DashboardState, width, height int) *uv.Buffer {
	buffer := uv.NewBuffer(width, height)
	frame := chat.NewFrame(buffer)
	withNoColorOverrideForTest(true, func() struct{} {
		renderCombinedForTest(frame, dashboard, map[string]*chat.ActiveChat{}, map[PaneId]string{}, false)
		return struct{}{}
	})
	return buffer
}

func resumeNewDashboard(config core.Config, state core.StateState) *DashboardState {
	return NewDashboardState(config, state, map[string]client.ProfileQuota{})
}

// C-25: a long title gives way to the "[unavailable]" marker, so the marker
// is always shown in full.
// Hard-won: 72f1adfb: Finding C-25 showed a long import title clipped the [unavailable] marker to [unavaila].
func TestALongUnavailableTitleKeepsItsMarker(t *testing.T) {
	for _, width := range []int{120, 80, 60} {
		dashboard := resumeNewDashboard(config(), resumeStateWith(nil))
		session := resumeNative(
			"native-long",
			strings.Repeat("A very long imported conversation title ", 6),
			resumeNewerThanTheCheckpoint,
		)
		reason := "missing Git repo"
		session.UnavailableReason = &reason
		openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile([]ImportSessionOption{session})})
		resumeSwitchToImport(t, dashboard)
		rendered := strings.Join(drawn(dashboard, width, 34), "\n")
		if !strings.Contains(rendered, "[unavailable]") {
			t.Fatalf("width %d:\n%s", width, rendered)
		}
	}
}

func TestUnavailableImportExplainsWhyAndCopiesItsIDWithoutClosing(t *testing.T) {
	for _, width := range []int{120, 60} {
		for _, reason := range []string{
			"missing Git repo",
			"Legacy Codex history cannot be imported",
		} {
			dashboard := resumeNewDashboard(config(), resumeStateWith(nil))
			session := resumeNative("native-unavailable", "Unavailable", resumeNewerThanTheCheckpoint)
			unavailable := reason
			session.UnavailableReason = &unavailable
			openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile([]ImportSessionOption{
				session,
				resumeNative("native-ready", "Ready", 1),
			})})
			resumeSwitchToImport(t, dashboard)
			lines := drawn(dashboard, width, 34)
			rendered := strings.Join(lines, "\n")
			label := "Cannot import"
			if reason == "missing Git repo" {
				label = "Cannot import: missing Git repo"
			}
			if !strings.Contains(rendered, label) {
				t.Fatal(rendered)
			}
			if strings.Contains(rendered, "native-unavailable") {
				t.Fatal(rendered)
			}
			if strings.Contains(rendered, "Enter imports") {
				t.Fatal(rendered)
			}
			resumeAssertAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionNone{})
			for _, kind := range []tea.MouseMsg{resumeLeftDown, resumeLeftUp} {
				resumeAssertAction(t, dashboard.HandleMouse(mouseAt(kind, point(lines, label))), DashboardActionNone{})
			}
			if _, ok := dashboard.mode.(ModeResumeDialog); !ok {
				t.Fatalf("the dialog closed: %#v", dashboard.mode)
			}

			copyID := point(lines, "Copy session ID")
			dashboard.HandleMouse(mouseAt(resumeLeftDown, copyID))
			resumeAssertAction(
				t,
				dashboard.HandleMouse(mouseAt(resumeLeftUp, copyID)),
				DashboardActionCopyNativeSessionId{NativeSessionID: "native-unavailable"},
			)
			if _, ok := dashboard.mode.(ModeResumeDialog); !ok {
				t.Fatalf("the dialog closed: %#v", dashboard.mode)
			}

			// Tab navigation must reach the same copy action.
			if mode, ok := dashboard.mode.(ModeResumeDialog); ok {
				mode.form.Focus(ResumeFocusSessions)
			}
			dashboard.HandleKey(key(tea.KeyTab))
			dashboard.HandleKey(key(tea.KeyTab))
			resumeAssertAction(
				t,
				dashboard.HandleKey(key(tea.KeyEnter)),
				DashboardActionCopyNativeSessionId{NativeSessionID: "native-unavailable"},
			)

			if mode, ok := dashboard.mode.(ModeResumeDialog); ok {
				mode.form.Focus(ResumeFocusSessions)
			}
			dashboard.HandleKey(key(tea.KeyDown))
			dashboard.ClearNotice()
			ready := strings.Join(drawn(dashboard, width, 34), "\n")
			if strings.Contains(ready, "Copy session ID") || strings.Contains(ready, "Cannot import") {
				t.Fatal(ready)
			}
			resumeAssertAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionImportSession{
				ProfileID:       "codex-1",
				NativeSessionID: "native-ready",
				DisplayTitle:    "Ready",
			})
		}
	}
}

func resumeAssertAction(t *testing.T, got, want DashboardAction) {
	t.Helper()
	if wizardActionIsNone(got) && wizardActionIsNone(want) {
		return
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("action = %#v, want %#v", got, want)
	}
}

func resumeStateWith(sessions []core.SessionRecord) core.StateState {
	state := core.NewStateState()
	for _, session := range sessions {
		state.Sessions.Insert(session.ID, session)
	}
	return state
}

func resumeIncompleteMove() core.MoveOperation {
	profile, target := "codex-1", "target-1"
	failure := "queue admission interrupted"
	return core.MoveOperation{
		OperationID: "move-1",
		Selection: core.MoveSelection{
			SessionID:        "session-1",
			ProfileID:        &profile,
			TargetTemplateID: &target,
		},
		SourceProfileID:          "codex-1",
		SourceTargetTemplateID:   "target-1",
		ConfigurationFingerprint: "fingerprint",
		Queue:                    core.ResumeQueueDispositionStart,
		Phase:                    core.MovePhaseCancelled,
		QueueAdmissionStarted:    true,
		QueueAdmissionFinished:   false,
		CancellationRequested:    true,
		CreatedAt:                "2026-01-01T00:00:00Z",
		UpdatedAt:                "2026-01-01T00:00:00Z",
		Error:                    &failure,
	}
}

func resumeTestRows(t *testing.T, dashboard *DashboardState) []ResumeRow {
	t.Helper()
	if _, ok := dashboard.mode.(ModeResumeDialog); !ok {
		t.Fatalf("expected the resume dialog, got %#v", dashboard.mode)
	}
	return slices.Clone(dashboard.resumeRows())
}

func resumeTitles(rows []ResumeRow) []string {
	titles := make([]string, len(rows))
	for index := range rows {
		titles[index] = rows[index].title
	}
	return titles
}

func resumeOpenDialog(t *testing.T, dashboard *DashboardState) *ResumeDialog {
	t.Helper()
	mode, ok := dashboard.mode.(ModeResumeDialog)
	if !ok {
		t.Fatalf("expected the resume dialog, got %#v", dashboard.mode)
	}
	return mode.ResumeDialog
}

func resumeDialogFocus(t *testing.T, dashboard *DashboardState) ResumeFocus {
	t.Helper()
	return resumeOpenDialog(t, dashboard).focused()
}

func TestIncompleteMoveResumeRowOpensSameDestinationRetryControls(t *testing.T) {
	for _, live := range []bool{false, true} {
		dashboard := resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stoppedSession()}))
		dashboard.SetMoveOperations([]core.MoveOperation{resumeIncompleteMove()})
		if live {
			workspace := "searching-workspace"
			dashboard.SetActiveWorkspace(&workspace)
			serveAttention(dashboard, []string{"session-1"}, core.SessionAttentionNeedsInput)
			publishFixtureVerdicts(dashboard)
		}
		openResumeDialog(dashboard, 1, nil)
		if live {
			resumeDialogMut(&dashboard.mode).form.Focus(ResumeFocusSessions)
		} else {
			resumeSwitchToHel(t, dashboard)
		}

		index := slices.IndexFunc(dashboard.resumeRows(), func(row ResumeRow) bool {
			id, ok := row.sessionID()
			return ok && id == "session-1"
		})
		if index < 0 {
			t.Fatal("stopped session remains in resume rows")
		}
		if dashboard.resumeRows()[index].moveRecovery == nil {
			t.Fatal("the row offers its move recovery")
		}
		if got := confirmationButtons(ConfirmationRecoverMove{Operation: resumeIncompleteMove()}); !slices.Equal(got, []string{"Cancel", "Open transcript", "Retry move"}) {
			t.Fatalf("buttons = %q", got)
		}
		resumeAssertAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionNone{})
		mode, ok := dashboard.mode.(ModeConfirm)
		if !ok {
			t.Fatalf("expected a confirmation, got %#v", dashboard.mode)
		}
		if _, ok := mode.confirmation.(ConfirmationRecoverMove); !ok {
			t.Fatalf("expected the move recovery, got %#v", mode.confirmation)
		}
	}
}

func TestMoveRecoveryUsesRecordedPolicyAndRetainedExplicitOverride(t *testing.T) {
	effort := "max"
	policy := core.SubagentPolicy{
		Mode:            core.SubagentPolicyMjolnir,
		Orchestrators:   false,
		Straightforward: &core.SubagentChoice{Model: "gpt-6-luna", Effort: &effort},
		Subtle:          &core.SubagentChoice{Model: "gpt-6-sol"},
	}
	for _, retained := range []*core.SubagentPolicy{nil, {Mode: core.SubagentPolicyNative}} {
		session := runningSession()
		recorded := policy
		session.Subagents = &recorded
		dashboard := resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{session}))
		operation := resumeIncompleteMove()
		operation.Selection.Subagents = retained
		target := "podman"
		operation.Selection.TargetTemplateID = &target
		dashboard.BeginMoveRecovery(operation)
		mode, ok := dashboard.mode.(ModeResume)
		if !ok {
			t.Fatalf("recovery wizard, got %#v", dashboard.mode)
		}
		wizard := mode.ResumeWizard
		want := policy
		if retained != nil {
			want = *retained
		}
		if !wizardSubagentPolicyEqual(wizard.subagents.policy, want) {
			t.Fatalf("policy = %#v, want %#v", wizard.subagents.policy, want)
		}
		if change := wizard.subagentChange(dashboard); !reflect.DeepEqual(change, retained) {
			t.Fatalf("change = %#v, want %#v", change, retained)
		}
		if retained == nil {
			draft := *wizard.subagents
			draft.selectPolicy(0)
			if !draft.selectPolicy(1) {
				t.Fatal("the Mjolnir policy can be chosen again")
			}
			if !wizardSubagentPolicyEqual(draft.policy, policy) {
				t.Fatalf("policy = %#v, want %#v", draft.policy, policy)
			}
		}
		if stored := dashboard.state.Get().Sessions.Index("session-1").Subagents; stored == nil || !wizardSubagentPolicyEqual(*stored, policy) {
			t.Fatalf("the record's policy changed: %#v", stored)
		}
	}
}

func resumeReplaceSearch(t *testing.T, dashboard *DashboardState, search string) {
	t.Helper()
	dialog := resumeOpenDialog(t, dashboard)
	dialog.search = chat.TextInputFromValue(search)
	dashboard.RebuildResumeRows()
}

// resumeFocusControl tabs forward until control has keyboard focus.
func resumeFocusControl(t *testing.T, dashboard *DashboardState, control ResumeFocus) {
	t.Helper()
	for range 8 {
		if resumeOpenDialog(t, dashboard).focused() == control {
			return
		}
		dashboard.HandleKey(key(tea.KeyTab))
	}
	t.Fatalf("%d never received focus", control)
}

func resumeSwitchToImport(t *testing.T, dashboard *DashboardState) {
	t.Helper()
	resumeSwitchToTab(t, dashboard, ResumeTabImport)
}

// resumeSwitchToTab moves the dialog to a tab by name, through the tab
// strip.
func resumeSwitchToTab(t *testing.T, dashboard *DashboardState, tab ResumeTab) {
	t.Helper()
	for range resumeTabCount + 1 {
		dialog := resumeOpenDialog(t, dashboard)
		if dialog.tab == tab {
			if dialog.focused() == ResumeFocusTabs {
				dashboard.HandleKey(key(tea.KeyEnter))
			}
			return
		}
		forward := tab.index() > dialog.tab.index()
		dialog.form.Focus(ResumeFocusTabs)
		if forward {
			dashboard.HandleKey(key(tea.KeyRight))
		} else {
			dashboard.HandleKey(key(tea.KeyLeft))
		}
	}
	t.Fatalf("the dialog never reached %s", resumeTabDebug(tab))
}

func resumeSwitchToHel(t *testing.T, dashboard *DashboardState) {
	t.Helper()
	resumeSwitchToTab(t, dashboard, ResumeTabHel)
}

func resumeSwitchToArchive(t *testing.T, dashboard *DashboardState) {
	t.Helper()
	resumeSwitchToTab(t, dashboard, ResumeTabArchive)
}

func TestLastActiveUsesWordsThroughSevenDaysThenALocalDate(t *testing.T) {
	now, err := time.Parse(time.RFC3339, "2026-08-23T12:00:00-05:00")
	if err != nil {
		t.Fatal(err)
	}
	before := func(milliseconds int64) int64 { return now.UnixMilli() - milliseconds }

	for _, check := range []struct {
		thenMs int64
		want   string
	}{
		{before(30_000), "just now"},
		{before(60_000), "1 minute ago"},
		{before(2 * 60_000), "2 minutes ago"},
		{before(60 * 60_000), "1 hour ago"},
		{before(24 * 60 * 60_000), "1 day ago"},
		{before(sevenDaysMs), "7 days ago"},
		{before(sevenDaysMs + 1), "Aug 16, 2026"},
		{before(9 * 24 * 60 * 60_000), "Aug 14, 2026"},
		{now.UnixMilli() + 1, "just now"},
		{0, "unknown"},
		{int64(^uint64(0) >> 1), "unknown"},
	} {
		if got := formatLastActive(now, check.thenMs); got != check.want {
			t.Errorf("formatLastActive(%d) = %q, want %q", check.thenMs, got, check.want)
		}
	}
}

// A Hel record and the native session it was imported from are one
// conversation, so the dialog shows the Hel record's row and drops the
// native duplicate.
func TestAHelRecordReplacesTheNativeSessionItWasImportedFrom(t *testing.T) {
	// stoppedSession carries native_session_id "native-1".
	dashboard := resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stoppedSession()}))
	merged := resumeMergedRows(
		dashboard.config.Get(),
		dashboard.state.Get(),
		[]ImportProfileOption{resumeCodexProfile([]ImportSessionOption{
			resumeNative("native-1", "Same conversation", 10),
			resumeNative("native-2", "A different conversation", 5),
		})},
		nil,
	)

	if len(merged) != 2 {
		t.Fatalf("%q", resumeTitles(merged))
	}
	adopted := slices.IndexFunc(merged, func(row ResumeRow) bool { return row.key == resumeRowKeyHel("session-1") })
	if adopted < 0 {
		t.Fatal("the hel record keeps its row")
	}
	if merged[adopted].title != "ACP pretty name" || merged[adopted].origin != "podman" {
		t.Fatalf("row = %#v", merged[adopted])
	}
	if slices.ContainsFunc(merged, func(row ResumeRow) bool {
		return row.key == resumeRowKeyNative(core.HarnessKindCodex, "native-1")
	}) {
		t.Fatal("the native duplicate is gone")
	}
	// A native session with no Hel record still shows, marked local.
	nativeOnly := slices.IndexFunc(merged, func(row ResumeRow) bool {
		return row.key == resumeRowKeyNative(core.HarnessKindCodex, "native-2")
	})
	if nativeOnly < 0 {
		t.Fatal("the unadopted native session keeps its row")
	}
	if merged[nativeOnly].origin != "local/hel" {
		t.Fatalf("origin = %q", merged[nativeOnly].origin)
	}
}

// The native session behind a live Hel session must not be offered as an
// import: that would start a second Hel session on the same conversation.
func TestALiveSessionHidesItsNativeCounterpartFromTheDialog(t *testing.T) {
	live := stoppedSession()
	live.State = core.SessionStateRunning
	cfg := config()
	state := resumeStateWith([]core.SessionRecord{live})
	merged := resumeMergedRows(&cfg, &state, []ImportProfileOption{resumeCodexProfile([]ImportSessionOption{
		resumeNative("native-1", "Running under Hel right now", 10),
		resumeNative("native-2", "Idle native session", 5),
	})}, nil)
	if got := resumeTitles(merged); !slices.Equal(got, []string{"Idle native session"}) {
		t.Fatalf("titles = %q", got)
	}
}

// R10-3: a `/clear` leaves the session's earlier native thread in the harness
// home. The store names one thread per session: until the next checkpoint
// the one from before `/clear`, after it the new one. So the Import tab
// offered session C's pre-`/clear` thread, whose history C still holds, and
// importing it would have duplicated that history. A thread that ran in a
// session's own checkout is that session's.
// Hard-won: ebcd71d2: R10-3 showed a session’s pre- or post-/clear Codex thread could be offered as a second import of its own history.
func TestAThreadFromAMjolnirSessionsOwnCheckoutIsNotOfferedForImport(t *testing.T) {
	clone := "/home/dev/reverify-10/project/.mj/clones/0858ecafaa06ecd148aa8bcf70bba899"
	preClear := "01a0d8c9-2bc1-7041-9a64-4b8216558cdc"
	postClear := "01a0d8c9-cc35-7e81-943e-0c480730ac99"
	// Before and after the checkpoint that records the new thread.
	for _, check := range []struct {
		stored string
		state  core.SessionState
	}{
		{preClear, core.SessionStateRunning},
		{postClear, core.SessionStateStopped},
	} {
		session := stoppedSession()
		session.ID = "0858ecafaa06ecd148aa8bcf70bba899"
		session.State = check.state
		stored := check.stored
		session.NativeSessionID = &stored
		directory := clone
		session.ProjectDirectory = &directory
		session.ManagedWorktree = &core.ManagedWorktree{
			Kind:                   core.ManagedCheckoutKindClone,
			SourceProjectDirectory: "/home/dev/reverify-10/project",
			SourceRepository:       "/home/dev/reverify-10/project",
			WorktreeRoot:           clone,
			Branch:                 "main",
			Target:                 core.ManagedWorktreeTargetLocal{},
		}
		before := resumeNative(preClear, "Remember PINEAPPLE and reply OK", 10)
		before.Cwd = clone
		after := resumeNative(postClear, "What word did I ask you to remember?", 20)
		after.Cwd = clone
		own := resumeNative("user-thread", "The user's own thread", 5)
		own.Cwd = "/home/dev/reverify-10/project"
		cfg := config()
		state := resumeStateWith([]core.SessionRecord{session})
		merged := resumeMergedRows(&cfg, &state, []ImportProfileOption{
			resumeCodexProfile([]ImportSessionOption{before, after, own}),
		}, nil)
		var importable []string
		for _, row := range merged {
			if row.key.Kind == ResumeRowKeyKindNative {
				importable = append(importable, row.title)
			}
		}
		if !slices.Equal(importable, []string{"The user's own thread"}) {
			t.Fatalf("with %s stored: %q", check.stored, importable)
		}
	}
}

// A lost or force-destroyed session cannot be resumed; deleting its record
// is the only thing left to do with it.
func TestLostAndDestroyedRowsAreMarkedAndRefuseToResume(t *testing.T) {
	for _, check := range []struct {
		state  core.SessionState
		marker string
		reason string
	}{
		{core.SessionStateLost, "! lost", "lost without a verified checkpoint"},
		{core.SessionStateDestroyedWithDataLoss, "! data lost", "force-destroyed"},
	} {
		session := stoppedSession()
		session.State = check.state
		dashboard := resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{session}))
		openResumeDialog(dashboard, 1, nil)
		resumeSwitchToHel(t, dashboard)

		row := resumeTestRows(t, dashboard)[0]
		if row.status.isRecoverable() {
			t.Fatal("the row is not recoverable")
		}
		if warning, ok := row.status.warning(); !ok || warning != check.marker {
			t.Fatalf("warning = %q", warning)
		}
		if !strings.Contains(row.details, check.reason) {
			t.Fatal(row.details)
		}

		resumeAssertAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionNone{})
		resumeOpenDialog(t, dashboard)
		notice, _ := dashboard.notices.Current()
		if !strings.Contains(notice, check.reason) || !strings.Contains(notice, "Use Destroy") {
			t.Fatal(notice)
		}

		resumeFocusControl(t, dashboard, ResumeFocusDestroy)
		dashboard.HandleKey(key(tea.KeyEnter))
		if _, ok := dashboard.mode.(ModeConfirm); !ok {
			t.Fatalf("expected the destroy confirmation, got %#v", dashboard.mode)
		}

		// Sync completion must refresh the dialog even while its destroy
		// confirmation temporarily covers it.
		completed := resumeSyncProgressStatus()
		completed.Finished = 1
		completed.Successful = 1
		completed.Outcome = client.WikiSyncOutcomeSucceeded{}
		if !dashboard.ApplyWikiSync(true, 1, completed, nil) {
			t.Fatal("the sync completion applies")
		}
		refresh, _, ok := dashboard.RefreshWikiSearch()
		if !ok {
			t.Fatal("refresh under confirmation")
		}
		dashboard.ApplyWikiSearch(refresh, resumeReadyPage([]client.WikiRow{resumeWikiRow("late-result", true)}))
		if _, ok := dashboard.mode.(ModeConfirm); !ok {
			t.Fatalf("expected the destroy confirmation, got %#v", dashboard.mode)
		}
		dashboard.HandleKey(key(tea.KeyEscape))
		mode, ok := dashboard.mode.(ModeResumeDialog)
		if !ok {
			t.Fatalf("dialog reopened, got %#v", dashboard.mode)
		}
		if mode.wiki[0].ID != "late-result" {
			t.Fatalf("wiki = %#v", mode.wiki)
		}
	}
}

// Launch finding R6-4: the Mjolnir tab listed a suspended session its harness
// had not named by its id (tmux/022), while the Destroy dialog and the
// notices called it "project via fake". The Mjolnir and Live tabs now list
// the title every other listing uses.
// Hard-won: dd35efff: Launch finding R6-4 showed unnamed suspended sessions displayed their ID instead of the created title.
func TestASessionItsHarnessHasNotNamedIsListedByItsCreatedTitle(t *testing.T) {
	unnamed := func(session core.SessionRecord, id, title string) core.SessionRecord {
		session.ID = id
		session.Title = title
		session.ACPSessionTitle = nil
		session.SessionTitleOverride = nil
		session.NativeSessionID = nil
		return session
	}
	stopped := unnamed(stoppedSession(), "f20058f83046cc4943a71686253e3661", "project via fake")
	running := unnamed(runningSession(), "0a1b2c3d4e5f60718293a4b5c6d7e8f9", "sibling via fake")
	dashboard := resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stopped, running}))
	publishFixtureVerdicts(dashboard)
	openResumeDialog(dashboard, 1, nil)

	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Equal(got, []string{"sibling via fake"}) {
		t.Fatalf("titles = %q", got)
	}
	live := drawn(dashboard, 140, 40)
	index := slices.IndexFunc(live, func(line string) bool { return strings.Contains(line, "sibling via fake") })
	if index < 0 {
		t.Fatalf("no Live row names the session:\n%s", strings.Join(live, "\n"))
	}
	if strings.Contains(live[index], "0a1b2c3d4e5f") {
		t.Fatal(live[index])
	}

	resumeSwitchToHel(t, dashboard)
	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Equal(got, []string{"project via fake"}) {
		t.Fatalf("titles = %q", got)
	}
	hel := drawn(dashboard, 140, 40)
	index = slices.IndexFunc(hel, func(line string) bool { return strings.Contains(line, "project via fake") })
	if index < 0 {
		t.Fatalf("no Mjolnir row names the session:\n%s", strings.Join(hel, "\n"))
	}
	if strings.Contains(hel[index], "f20058f83046") {
		t.Fatal(hel[index])
	}
}

// The active tab is highlighted whether or not the strip has focus, so the
// current tab is visible at a glance.
func TestTheActiveTabIsHighlightedWithoutFocus(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stoppedSession()}))
	openResumeDialog(dashboard, 1, nil)
	resumeSwitchToHel(t, dashboard)
	if focused := resumeDialogFocus(t, dashboard); focused != ResumeFocusSessions {
		t.Fatalf("focus = %d", focused)
	}

	buffer := resumeRenderedBuffer(dashboard, 120, 34)
	lines := bufferLines(buffer)
	row := slices.IndexFunc(lines, func(line string) bool {
		return strings.Contains(line, " Mjolnir ") && strings.Contains(line, " Import ")
	})
	if row < 0 {
		t.Fatalf("tab strip:\n%s", strings.Join(lines, "\n"))
	}
	active := cellColumn(lines[row], "Mjolnir")
	inactive := cellColumn(lines[row], "Import")
	if got := chat.CellStyle(buffer.CellAt(active, row)).Bg; got != chat.FocusControl().Bg {
		t.Fatalf("active tab background = %#v, want %#v", got, chat.FocusControl().Bg)
	}
	if got := chat.CellStyle(buffer.CellAt(inactive, row)).Bg; got != chat.ThemePalette().Selection {
		t.Fatalf("inactive tab background = %#v, want %#v", got, chat.ThemePalette().Selection)
	}
}

// Hel never modifies a harness home, so a native-only row has no destroy
// action.
func TestANativeOnlyRowCannotBeDestroyedFromHel(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith(nil))
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile([]ImportSessionOption{
		resumeNative("native-2", "Native", resumeNewerThanTheCheckpoint),
	})})
	resumeSwitchToImport(t, dashboard)

	if resumeOpenDialog(t, dashboard).form.IsEnabled(ResumeFocusDestroy) {
		t.Fatal("Destroy is disabled on a native row")
	}
	resumeAssertAction(t, dashboard.HandleKey(key(tea.KeyDelete)), DashboardActionNone{})
	resumeOpenDialog(t, dashboard)
	if notice, _ := dashboard.notices.Current(); !strings.Contains(notice, "never destroys") {
		t.Fatal(notice)
	}
}

func resumeWikiRow(id string, archived bool) client.WikiRow {
	started := "2026-09-01T00:00:00Z"
	preview := "the last thing it said"
	return client.WikiRow{
		ID:       id,
		Tool:     "mjolnir",
		Project:  "/home/dev/project",
		Title:    fmt.Sprintf("archived %s", id),
		Started:  &started,
		Msgs:     7,
		Preview:  &preview,
		Archived: archived,
	}
}

// resumeReadyPage is one answer from an index that has finished building and
// is not syncing.
func resumeReadyPage(rows []client.WikiRow) client.WikiSearchPage {
	return client.WikiSearchPage{
		Rows:   rows,
		Status: client.WikiStatus{State: client.WikiIndexStateReady},
	}
}

// resumeApplyReadyRows puts the open dialog in the state one ready answer
// would leave it in, without going through a request: the tests that only
// care about rows should not have to spell the status out.
func resumeApplyReadyRows(t *testing.T, dashboard *DashboardState, rows []client.WikiRow) {
	t.Helper()
	dashboard.ApplyWikiSearch(resumeOpenDialog(t, dashboard).wikiRequestID, resumeReadyPage(rows))
}

// C-12: when a state filter empties the Live list, the message names the
// filter rather than saying nothing is running.
// Hard-won: db681e6e: Finding C-12 showed a filtered Live list claiming no sessions were running when the filter had no matches.
func TestAnEmptyFilteredLiveListNamesTheFilter(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	openResumeDialog(dashboard, 1, nil)
	dashboard.HandleKey(key('b'))
	if rows := resumeTestRows(t, dashboard); len(rows) != 0 {
		t.Fatalf("no session is blocked: %q", resumeTitles(rows))
	}
	rendered := strings.Join(drawn(dashboard, 120, 40), "\n")
	if !strings.Contains(rendered, "No blocked sessions") || strings.Contains(rendered, "No running sessions") {
		t.Fatal(rendered)
	}
}

// C-13: the archived briefing's UTC date is shown in local time, like every
// other time on screen.
// Hard-won: 8b5f8bb4: Finding C-13 showed archived briefing timestamps in UTC while the rest of the UI used local time.
func TestTheBriefingDateIsShownInLocalTime(t *testing.T) {
	central := time.FixedZone("", -5*3600)
	if got := localizeBriefDate("- Tool: mjolnir | Project: /tmp/project | Date: 2026-09-23 18:46", central); got != "- Tool: mjolnir | Project: /tmp/project | Date: 2026-09-23 13:46" {
		t.Fatal(got)
	}
	withSource := "- Tool: codex | Project: p | Date: 2026-09-24 02:00\n"
	if got := localizeBriefDate(strings.TrimRight(withSource, " \t\r\n"), central); got != "- Tool: codex | Project: p | Date: 2026-09-23 21:00" {
		t.Fatal(got)
	}
	for _, untouched := range []string{
		"- Tool: codex | Project: p | Date: -",
		"Date: 2026-09-23 18:46",
	} {
		if got := localizeBriefDate(untouched, central); got != untouched {
			t.Fatal(got)
		}
	}
}

// C-6: Enter in the search box does what Enter on the list does. On the Live
// tab that jumps to the matched session and closes the dialog.
// Hard-won: 8e13ffb1: Finding C-6 showed Enter in the Live search box only moved focus instead of opening the selected match.
func TestEnterInTheSearchBoxOpensTheLiveMatch(t *testing.T) {
	dashboard := resumeDashboardWithLiveSessionsInTwoWorkspaces()
	openResumeDialog(dashboard, 1, nil)
	drawn(dashboard, 120, 40)

	dashboard.HandleKey(key('/'))
	for _, character := range "mast" {
		dashboard.HandleKey(key(character))
	}
	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Equal(got, []string{"Raise the mast"}) {
		t.Fatalf("titles = %q", got)
	}
	drawn(dashboard, 120, 40)
	dashboard.HandleKey(key(tea.KeyEnter))
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("Enter on the search match left the dialog open: %#v", dashboard.mode)
	}
}

// No production path opens the dialog anywhere but Live (see Milestone 5), so
// `search_focus_pending` served no purpose and was removed. An index answer
// arriving while the dialog is open must still leave the focus wherever the
// person put it.
func TestAnIndexAnswerLeavesTheFocusedControlUnchanged(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	openResumeDialog(dashboard, 1, nil)
	if _, pending := resumeOpenDialog(t, dashboard).wikiSearch.(WikiSearchStatePending); !pending {
		t.Fatalf("search = %#v", resumeOpenDialog(t, dashboard).wikiSearch)
	}
	focusBefore := resumeDialogFocus(t, dashboard)

	resumeApplyReadyRows(t, dashboard, nil)
	if got := resumeDialogFocus(t, dashboard); got != focusBefore {
		t.Fatalf("focus = %d, want %d", got, focusBefore)
	}
}

// resumeConfigWithBareTarget is a configuration with a bare target beside the
// container one, so the two halves of the target-naming rule can both be
// seen.
func resumeConfigWithBareTarget() core.Config {
	cfg := config()
	cfg.Targets = maps.Clone(cfg.Targets)
	cfg.Targets["localhost"] = core.TargetTemplateLocalBare{}
	return cfg
}

// resumeTaggedWikiRow is a Mjolnir session the index carries metadata for:
// the sync stored its target, profile and harness as tags, and the row is
// built from them.
func resumeTaggedWikiRow(id, target, profile string) client.WikiRow {
	row := resumeWikiRow(id, true)
	harness := "codex"
	row.Target = &target
	row.Profile = &profile
	row.Harness = &harness
	return row
}

// resumeSelectTheArchivedRow puts the dialog on the Archived tab with its
// one row selected, which is where the arrow keys leave it. The test that
// follows is about what Enter does, not about reaching the tab.
func resumeSelectTheArchivedRow(t *testing.T, dashboard *DashboardState) {
	t.Helper()
	dashboard.switchResumeTab(ResumeTabArchive)
	resumeOpenDialog(t, dashboard).form.Focus(ResumeFocusSessions)
	dashboard.selectResumeRow(0)
	if selected := resumeOpenDialog(t, dashboard).selected; selected == nil || *selected != resumeRowKeyArchive("gone") {
		t.Fatalf("the archived row is selected: %#v", selected)
	}
}

// An id the configuration no longer has leaves the wizard on its first
// choice; there is nothing better to offer.
func TestAnUnknownIndexedProfileLeavesTheRestoreWizardOnItsFirstChoice(t *testing.T) {
	dashboard := resumeNewDashboard(resumeConfigWithBareTarget(), resumeStateWith(nil))
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile(nil)})
	requestID, _, ok := dashboard.NextWikiSearch()
	if !ok {
		t.Fatal("a search is asked for")
	}
	dashboard.ApplyWikiSearch(requestID, resumeReadyPage([]client.WikiRow{
		resumeTaggedWikiRow("gone", "was-a-target", "was-a-profile"),
	}))
	resumeSelectTheArchivedRow(t, dashboard)
	dashboard.HandleKey(key(tea.KeyEnter))

	mode, ok := dashboard.mode.(ModeResume)
	if !ok {
		t.Fatalf("expected the resume wizard, got %#v", dashboard.mode)
	}
	if mode.profile != 0 || mode.target != 0 {
		t.Fatalf("profile %d, target %d", mode.profile, mode.target)
	}
}

// Typing asks the daemon for a new search, and an answer to an older request
// is ignored because the person has typed since.
func TestTypingAsksForASearchAndStaleAnswersAreDropped(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stoppedSession()}))
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile(nil)})
	resumeApplyReadyRows(t, dashboard, nil)
	// The Live tab matches names itself; the index is asked from the tabs
	// that have nothing else to search with.
	resumeSwitchToHel(t, dashboard)
	resumeFocusControl(t, dashboard, ResumeFocusSearch)

	action := dashboard.HandleKey(key('g'))
	search, ok := action.(DashboardActionSearchArchivedSessions)
	if !ok {
		t.Fatalf("typing must ask for a search, got %#v", action)
	}
	if search.Query != "g" {
		t.Fatalf("query = %q", search.Query)
	}

	dashboard.ApplyWikiSearch(search.RequestID-1, resumeReadyPage([]client.WikiRow{resumeWikiRow("stale", true)}))
	if slices.ContainsFunc(resumeTestRows(t, dashboard), func(row ResumeRow) bool {
		return row.key.Kind == ResumeRowKeyKindArchive
	}) {
		t.Fatal("an answer to an older request is dropped")
	}
	dashboard.ApplyWikiSearch(search.RequestID, resumeReadyPage([]client.WikiRow{resumeWikiRow("fresh", true)}))
	dialog := resumeOpenDialog(t, dashboard)
	if len(dialog.wiki) != 1 || dialog.wiki[0].ID != "fresh" {
		t.Fatalf("wiki = %#v", dialog.wiki)
	}
}

// Enter on the focused Mjolnir row starts its resume, which is what the
// tab's own hint promises and what the Resume button already does.
// Hard-won: c82060f6: Enter on a Mjolnir row did nothing after arrow navigation left focus on the tab strip.
func TestEnterOnAFocusedMjolnirRowBeginsItsResume(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stoppedSession()}))
	publishFixtureVerdicts(dashboard)
	openResumeDialog(dashboard, 1, nil)
	hit := resumeWikiRow("wiki-1", false)
	session := "session-1"
	hit.HelSessionID = &session
	resumeApplyReadyRows(t, dashboard, []client.WikiRow{hit})
	// Drawing registers the dialog's controls, so the keys land where they
	// do in the running dashboard. Live is empty here: the only session is
	// stopped.
	drawn(dashboard, 120, 40)
	if rows := resumeTestRows(t, dashboard); len(rows) != 0 {
		t.Fatalf("no session is running: %q", resumeTitles(rows))
	}

	dashboard.HandleKey(key(tea.KeyRight))
	drawn(dashboard, 120, 40)
	if tab := resumeOpenDialog(t, dashboard).tab; tab != ResumeTabHel {
		t.Fatalf("tab = %s", resumeTabDebug(tab))
	}
	if rows := resumeTestRows(t, dashboard); len(rows) != 1 {
		t.Fatalf("the stopped session is listed: %q", resumeTitles(rows))
	}

	if focused := resumeDialogFocus(t, dashboard); focused != ResumeFocusSessions {
		t.Fatalf("focus = %d", focused)
	}
	resumeAssertAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionNone{})
	if _, ok := dashboard.mode.(ModeResume); !ok {
		t.Fatalf("Enter on the row opens the resume wizard, got %#v", dashboard.mode)
	}
}

// resumeDashboardWithLiveSessionsInTwoWorkspaces is two running sessions in
// the active workspace and one in `other`, each with its own title so a
// name search can pick one out.
func resumeDashboardWithLiveSessionsInTwoWorkspaces() *DashboardState {
	var sessions []core.SessionRecord
	for _, fixture := range []struct{ id, workspace, title string }{
		{"live-alpha", "default", "Sweep the hearth"},
		{"live-beta", "default", "Count the coins"},
		{"live-remote", "other", "Raise the mast"},
	} {
		session := runningSession()
		session.ID = fixture.id
		session.WorkspaceID = fixture.workspace
		title := fixture.title
		session.SessionTitleOverride = &title
		session.NativeSessionID = nil
		sessions = append(sessions, session)
	}
	dashboard := resumeNewDashboard(config(), resumeStateWith(sessions))
	publishFixtureVerdicts(dashboard)
	dashboard.SetWorkspaceNames(map[string]string{
		"default": "Default",
		"other":   "Other",
	})
	return dashboard
}

// resumeWithCheckpointCreated is session with its checkpoint created at.
func resumeWithCheckpointCreated(session core.SessionRecord, at string) core.SessionRecord {
	checkpoint := *session.Checkpoint
	checkpoint.CreatedAt = at
	session.Checkpoint = &checkpoint
	return session
}

// The Live tab lists the running sessions of every workspace, and its search
// box narrows them by name while the index is still building.
func TestGoldenResumeDialogTable(t *testing.T) {
	var output strings.Builder

	local := stoppedSession()
	local.ID = "local-session"
	local.NativeSessionID = nil
	local.TargetTemplateID = "localhost"
	localDirectory := "/mnt/optane/bifrost-fird"
	local.ProjectDirectory = &localDirectory
	remote := stoppedSession()
	remote.ID = "remote-session"
	remote.NativeSessionID = nil
	remote.TargetTemplateID = "precision-3260"
	remoteDirectory := "/home/jonathan/Projects/bifrost"
	remote.ProjectDirectory = &remoteDirectory
	projectConfig := config()
	projectConfig.Targets = maps.Clone(projectConfig.Targets)
	projectConfig.Targets["localhost"] = core.TargetTemplateLocalBare{}
	projectConfig.Targets["precision-3260"] = core.TargetTemplateSshBare{
		SSH:             core.SshConnection{Host: "precision-3260"},
		Permissions:     core.PermissionModeYolo,
		WorkspacePrefix: ".local/share/hel/workspaces",
	}
	dashboard := resumeNewDashboard(projectConfig, resumeStateWith([]core.SessionRecord{local, remote}))
	publishFixtureVerdicts(dashboard)
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile([]ImportSessionOption{
		resumeNative("native-only", "Native project", resumeNewerThanTheCheckpoint),
	})})
	resumeSwitchToHel(t, dashboard)
	resumeAppendGolden(&output, "Hel records with project targets", 140, 34, dashboard)
	resumeSwitchToImport(t, dashboard)
	resumeAppendGolden(&output, "native import target", 140, 34, dashboard)

	session := stoppedSession()
	session.TargetTemplateID = "retired-target"
	retiredConfig := config()
	retiredConfig.Targets = map[string]core.TargetTemplate{}
	dashboard = resumeNewDashboard(retiredConfig, resumeStateWith([]core.SessionRecord{session}))
	publishFixtureVerdicts(dashboard)
	openResumeDialog(dashboard, 1, nil)
	resumeSwitchToHel(t, dashboard)
	resumeAppendGolden(&output, "stored target after config removal", 120, 34, dashboard)

	oldRecord := stoppedSession()
	oldRecord.ID = "old-record"
	oldRecord.NativeSessionID = nil
	oldRecord = resumeWithCheckpointCreated(oldRecord, "2026-01-01T00:00:00Z")
	newRecord := stoppedSession()
	newRecord.ID = "new-record"
	newRecord.NativeSessionID = nil
	newest := "Newest record"
	newRecord.ACPSessionTitle = &newest
	newRecord = resumeWithCheckpointCreated(newRecord, "2026-06-01T00:00:00Z")
	cfg := config()
	ordered := resumeStateWith([]core.SessionRecord{oldRecord, newRecord})
	merged := resumeMergedRows(&cfg, &ordered, []ImportProfileOption{resumeCodexProfile([]ImportSessionOption{
		resumeNative("native-mid", "Native March", 1_772_409_600_000),
		resumeNative("native-new", "Native July", 1_782_950_400_000),
	})}, nil)
	resumeAppendGoldenValue(&output, "ordered resume titles", resumeTitles(merged))
	oldRow := stoppedSession()
	oldRow.ID = "old-record"
	oldRow.NativeSessionID = nil
	oldRow = resumeWithCheckpointCreated(oldRow, "2026-01-01T00:00:00Z")
	newRow := stoppedSession()
	newRow.ID = "new-record"
	newRow.NativeSessionID = nil
	newRow.ACPSessionTitle = &newest
	newRow = resumeWithCheckpointCreated(newRow, "2026-06-01T00:00:00Z")
	dashboard = resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{oldRow, newRow}))
	publishFixtureVerdicts(dashboard)
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile([]ImportSessionOption{
		resumeNative("native-mid", "Native March", 1_772_409_600_000),
		resumeNative("native-new", "Native July", 1_782_950_400_000),
	})})
	resumeSwitchToHel(t, dashboard)
	resumeAppendGolden(&output, "cross-source activity order", 140, 34, dashboard)

	archived := stoppedSession()
	archived.ID = "archived-record"
	archived.NativeSessionID = nil
	archivedTitle := "Archived record"
	archived.ACPSessionTitle = &archivedTitle
	archived.Archived = true
	current := stoppedSession()
	current.ID = "current-record"
	current.NativeSessionID = nil
	currentTitle := "Current record"
	current.ACPSessionTitle = &currentTitle
	dashboard = resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{archived, current}))
	publishFixtureVerdicts(dashboard)
	openResumeDialog(dashboard, 1, nil)
	resumeSwitchToHel(t, dashboard)
	resumeAppendGolden(&output, "history including older archived records", 120, 34, dashboard)

	nativelyArchived := resumeNative("native-codex", "Archived in Codex", 1)
	nativelyArchived.NativelyArchived = true
	dashboard = resumeNewDashboard(config(), resumeStateWith(nil))
	publishFixtureVerdicts(dashboard)
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile([]ImportSessionOption{nativelyArchived})})
	resumeSwitchToImport(t, dashboard)
	resumeAppendGoldenValue(&output, "native archive metadata", resumeTestRows(t, dashboard)[0].nativelyArchived)
	resumeAppendGolden(&output, "native archived conversation", 120, 34, dashboard)
	action := dashboard.HandleKey(key('a'))
	resumeAppendGoldenValue(&output, "archive shortcut action", action)
	resumeAppendGolden(&output, "native archived conversation remains listed", 120, 34, dashboard)

	dashboard = resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stoppedSession()}))
	publishFixtureVerdicts(dashboard)
	openResumeDialog(dashboard, 1, nil)
	resumeSwitchToHel(t, dashboard)
	action = dashboard.HandleKey(key('d'))
	resumeAppendGoldenValue(&output, "d key action", action)
	resumeAppendGolden(&output, "resume dialog before destroy button", 120, 34, dashboard)
	lines := drawn(dashboard, 120, 34)
	destroy := point(lines, "Destroy")
	dashboard.HandleMouse(mouseAt(resumeLeftDown, destroy))
	action = dashboard.HandleMouse(mouseAt(resumeLeftUp, destroy))
	resumeAppendGoldenValue(&output, "Destroy button action", action)
	resumeAppendGolden(&output, "destroy confirmation", 120, 34, dashboard)

	archiveConfig := resumeConfigWithBareTarget()
	state := resumeStateWith(nil)
	worktree := resumeTaggedWikiRow("worktree", "localhost", "codex-2")
	worktree.Project = "/home/dev/project/.mj/worktrees/556ebcbaee181"
	nested := resumeTaggedWikiRow("nested", "localhost", "codex-2")
	nested.Project = "/home/dev/project/.mj/worktrees/556ebcbaee181/sub"
	nested.Target = nil
	wikiRows := []client.WikiRow{
		resumeTaggedWikiRow("bare", "localhost", "codex-2"),
		worktree,
		nested,
		resumeTaggedWikiRow("container", "podman", "codex-2"),
		resumeTaggedWikiRow("retired", "was-a-target", "codex-2"),
		resumeWikiRow("untagged", true),
	}
	archiveRows := resumeMergedRows(&archiveConfig, &state, nil, wikiRows)
	for _, row := range archiveRows {
		resumeAppendGoldenValue(
			&output,
			fmt.Sprintf("archived row %s profile/origin", row.title),
			[2]string{row.profileID, row.origin},
		)
	}
	dashboard = resumeNewDashboard(archiveConfig, state)
	publishFixtureVerdicts(dashboard)
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile(nil)})
	resumeSwitchToArchive(t, dashboard)
	resumeApplyReadyRows(t, dashboard, wikiRows)
	resumeAppendGolden(&output, "archived rows with indexed profile and target", 140, 40, dashboard)

	dashboard = resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stoppedSession()}))
	publishFixtureVerdicts(dashboard)
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile([]ImportSessionOption{
		resumeNative("native-2", "Native", resumeNewerThanTheCheckpoint),
	})})
	resumeSwitchToHel(t, dashboard)
	resumeAppendGolden(&output, "Hel tab", 120, 34, dashboard)
	resumeSwitchToImport(t, dashboard)
	resumeAppendGolden(&output, "Import tab", 120, 34, dashboard)
	dashboard.HandleKey(key(tea.KeyLeft))
	resumeAppendGolden(&output, "Left returns to Hel", 120, 34, dashboard)

	dashboard = resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stoppedSession()}))
	profiles := func() []ImportProfileOption {
		return []ImportProfileOption{resumeCodexProfile([]ImportSessionOption{
			resumeNative("native-2", "Native", resumeNewerThanTheCheckpoint),
		})}
	}
	publishFixtureVerdicts(dashboard)
	openResumeDialog(dashboard, 1, profiles())
	resumeSwitchToHel(t, dashboard)
	resumeAppendGolden(&output, "select resumable Hel row", 120, 34, dashboard)
	action = dashboard.HandleKey(key(tea.KeyEnter))
	resumeAppendGoldenValue(&output, "resume selection action", action)
	resumeAppendGoldenValue(&output, "mode after selecting Hel row", "Resume wizard")
	resumeAppendGolden(&output, "resume wizard", 120, 34, dashboard)

	publishFixtureVerdicts(dashboard)
	openResumeDialog(dashboard, 2, profiles())
	resumeSwitchToImport(t, dashboard)
	resumeAppendGolden(&output, "select importable native row", 120, 34, dashboard)
	action = dashboard.HandleKey(key(tea.KeyEnter))
	resumeAppendGoldenValue(&output, "native import action", action)
	resumeAppendGolden(&output, "dashboard after importing native row", 120, 34, dashboard)

	dashboard = resumeDashboardWithLiveSessionsInTwoWorkspaces()
	publishFixtureVerdicts(dashboard)
	openResumeDialog(dashboard, 1, nil)
	resumeAppendGolden(&output, "Live tab across workspaces", 120, 40, dashboard)
	resumeFocusControl(t, dashboard, ResumeFocusSessions)
	dashboard.HandleKey(key(tea.KeyLeft))
	resumeAppendGolden(&output, "left wraps to Archived tab", 120, 40, dashboard)
	resumeSwitchToTab(t, dashboard, ResumeTabLive)
	drawn(dashboard, 120, 40)
	searchAction := dashboard.HandleKey(key('/'))
	resumeAppendGoldenValue(&output, "focus Live search action", searchAction)
	resumeFocusControl(t, dashboard, ResumeFocusSearch)
	for _, character := range "mast" {
		action := dashboard.HandleKey(key(character))
		resumeAppendGoldenValue(&output, fmt.Sprintf("search key %c action", character), action)
	}
	request, ok := dashboard.ResumeSearchRequestID()
	if !ok {
		t.Fatal("the dialog has a search request")
	}
	dashboard.ApplyResumeTextSearchResult(request, nil, nil)
	resumeAppendGolden(&output, "Live search result", 120, 40, dashboard)

	progress := resumeNewDashboard(config(), resumeStateWith(nil))
	publishFixtureVerdicts(dashboard)
	openResumeDialog(progress, 900, nil)
	progress.ApplyWikiSync(true, 900, resumeSyncProgressStatus(), nil)
	resumeAppendGolden(&output, "session index sync progress", 140, 34, progress)
	// Hard-won: 177f95677: an80-column dialog clipped progress behind its verbose title.
	resumeAppendGolden(&output, "narrow session index sync progress", 80, 34, progress)
	progress.ApplyWikiSync(true, 900, client.WikiSyncStatus{}, errors.New("database is locked"))
	resumeAppendGolden(&output, "session index sync failure", 140, 34, progress)
	resumeAppendGolden(&output, "narrow session index sync failure", 80, 34, progress)

	// Hard-won: 7d06e3b: history loading/error returned before the independent sync status.
	for _, tab := range []ResumeTab{ResumeTabHel, ResumeTabImport} {
		resumeSwitchToTab(t, progress, tab)
		resumeDialogMut(&progress.mode).history = HistoryLoadLoading{}
		progress.ApplyWikiSync(true, 900, resumeSyncProgressStatus(), nil)
		resumeAppendGolden(&output, fmt.Sprintf("%s history loading with sync progress", resumeTabDebug(tab)), 140, 34, progress)
		resumeDialogMut(&progress.mode).history = HistoryLoadFailed{Value: "history lookup failed"}
		progress.ApplyWikiSync(true, 900, client.WikiSyncStatus{}, errors.New("index unavailable"))
		resumeAppendGolden(&output, fmt.Sprintf("%s history failure with sync failure", resumeTabDebug(tab)), 140, 34, progress)
		// Hard-won: 9bbc497: compact sync titles discarded the independent history failure.
		resumeAppendGolden(&output, fmt.Sprintf("%s compact history and sync failures", resumeTabDebug(tab)), 80, 34, progress)
		// Hard-won: aee3f79: final Searching returned before the independent history notice.
		completed := resumeSyncProgressStatus()
		completed.Finished = 1
		completed.Successful = 1
		progress.ApplyWikiSync(true, 900, completed, nil)
		if _, _, ok := progress.RefreshWikiSearch(); !ok {
			t.Fatal("the dialog refreshes its search")
		}
		resumeDialogMut(&progress.mode).history = HistoryLoadLoading{}
		resumeAppendGolden(&output, fmt.Sprintf("%s history loading during final search", resumeTabDebug(tab)), 80, 34, progress)
		resumeDialogMut(&progress.mode).history = HistoryLoadFailed{Value: "history lookup failed"}
		resumeAppendGolden(&output, fmt.Sprintf("%s history failure during final search", resumeTabDebug(tab)), 80, 34, progress)
	}
	core.AssertGolden(t, "testdata", "resume-dialog-table", output.String())
}

func TestGoldenResumeDialogPreview(t *testing.T) {
	var output strings.Builder

	dashboard := resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stoppedSession()}))
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile([]ImportSessionOption{
		resumeNative("native-2", "Native alpha", resumeNewerThanTheCheckpoint),
		resumeNative("native-3", "Native beta", resumeNewerThanTheCheckpoint-1),
	})})
	resumeSwitchToHel(t, dashboard)
	resumeReplaceSearch(t, dashboard, "the phrase")
	codexHit := func(id, native string) client.WikiRow {
		row := resumeWikiRow(id, false)
		row.Tool = "codex"
		row.NativeID = &native
		return row
	}
	recordHit := resumeWikiRow("record-hit", false)
	record := "session-1"
	recordHit.HelSessionID = &record
	resumeApplyReadyRows(t, dashboard, []client.WikiRow{
		codexHit("alpha-hit", "native-2"),
		codexHit("beta-hit", "native-3"),
		recordHit,
		resumeWikiRow("gone", true),
	})
	resumeAppendGolden(&output, "search hits on each tab", 120, 40, dashboard)
	resumeAppendGoldenValue(&output, "tab hit counts", slices.Clone(dashboard.resumeHitCounts))
	resumeApplyReadyRows(t, dashboard, []client.WikiRow{codexHit("alpha-hit", "native-2")})
	resumeAppendGolden(&output, "search result on another tab", 120, 40, dashboard)
	resumeAppendGoldenValue(&output, "tab hit counts after refresh", slices.Clone(dashboard.resumeHitCounts))
	if mode, ok := dashboard.mode.(ModeResumeDialog); ok {
		resumeAppendGoldenValue(&output, "empty tab message", emptySearchMessage(dashboard, mode.ResumeDialog))
	}

	dashboard = resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stoppedSession()}))
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile(nil)})
	resumeApplyReadyRows(t, dashboard, resumeArchivedRows(3))
	resumeSwitchToArchive(t, dashboard)
	resumeReplaceSearch(t, dashboard, "needle")
	resumeApplyReadyRows(t, dashboard, resumeArchivedRows(3))
	selected, ok := resumeOpenDialog(t, dashboard).previewWikiID(dashboard.resumeRows())
	if !ok {
		t.Fatal("the selected archived row has a preview")
	}
	dashboard.ApplyWikiHits(selected, "needle", &client.WikiHitTranscript{
		Blocks:       []client.WikiHitBlock{resumeHitBlock("user", 0), resumeHitBlock("assistant", 60)},
		OmittedAfter: 2,
	})
	resumeAppendGolden(&output, "preview opens at first matching passage", 120, 40, dashboard)
	resumeFocusControl(t, dashboard, ResumeFocusSessions)
	dashboard.HandleKey(key('n'))
	resumeAppendGolden(&output, "n selects and scrolls to second match", 120, 40, dashboard)
	lines := drawn(dashboard, 120, 40)
	previous := point(lines, "[↑]")
	dashboard.HandleMouse(mouseAt(resumeLeftDown, previous))
	dashboard.HandleMouse(mouseAt(resumeLeftUp, previous))
	resumeAppendGolden(&output, "previous-hit arrow", 120, 40, dashboard)
	lines = drawn(dashboard, 120, 40)
	next := point(lines, "[↓]")
	dashboard.HandleMouse(mouseAt(resumeLeftDown, next))
	dashboard.HandleMouse(mouseAt(resumeLeftUp, next))
	resumeAppendGolden(&output, "next-hit arrow", 120, 40, dashboard)

	dashboard = resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stoppedSession()}))
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile(nil)})
	resumeApplyReadyRows(t, dashboard, resumeArchivedRows(1))
	resumeSwitchToArchive(t, dashboard)
	resumeAppendGolden(&output, "archived briefing without a query", 120, 40, dashboard)
	action := dashboard.NextWikiPreview()
	resumeAppendGoldenValue(&output, "briefing request action", action)
	resumeReplaceSearch(t, dashboard, "needle")
	resumeApplyReadyRows(t, dashboard, resumeArchivedRows(1))
	resumeAppendGolden(&output, "archived hit query", 120, 40, dashboard)
	action = dashboard.NextWikiPreview()
	resumeAppendGoldenValue(&output, "hit request action", action)
	duplicate := dashboard.NextWikiPreview()
	resumeAppendGoldenValue(&output, "duplicate hit request", duplicate)
	dashboard.ApplyWikiHits("archive-0", "needle", &client.WikiHitTranscript{})
	cached := dashboard.NextWikiPreview()
	resumeAppendGoldenValue(&output, "cached hit request", cached)
	resumeReplaceSearch(t, dashboard, "other")
	resumeApplyReadyRows(t, dashboard, resumeArchivedRows(1))
	action = dashboard.NextWikiPreview()
	resumeAppendGoldenValue(&output, "new query request action", action)
	resumeAppendGolden(&output, "new archived hit query", 120, 40, dashboard)

	core.AssertGolden(t, "testdata", "resume-dialog-preview", output.String())
}

func resumeTypeQuery(t *testing.T, dashboard *DashboardState, query string) uint64 {
	t.Helper()
	drawn(dashboard, 120, 40)
	resumeFocusControl(t, dashboard, ResumeFocusSearch)
	for _, character := range query {
		dashboard.HandleKey(key(character))
	}
	requestID, ok := dashboard.ResumeSearchRequestID()
	if !ok {
		t.Fatal("the dialog has a search request")
	}
	return requestID
}

func resumeLiveTextHit(sessionID string) client.SessionTextMatch {
	return client.SessionTextMatch{SessionID: sessionID, Kind: client.SessionTextMatchKindUser}
}

func TestLiveConversationMatchesOpenAndPreviewWithoutAHistoryPageHit(t *testing.T) {
	dashboard := resumeDashboardWithLiveSessionsInTwoWorkspaces()
	title := "SSCD duplicate detection with voyage 4 nano"
	dashboard.state.Mut().Sessions.GetMut("live-remote").SessionTitleOverride = &title
	openResumeDialog(dashboard, 1, nil)
	requestID := resumeTypeQuery(t, dashboard, "rvector")
	if rows := resumeTestRows(t, dashboard); len(rows) != 0 {
		t.Fatalf("rows = %q", resumeTitles(rows))
	}

	// The limited history page can be empty or fail independently of Live.
	resumeApplyReadyRows(t, dashboard, nil)
	if !dashboard.ApplyResumeTextSearchResult(requestID, []client.SessionTextMatch{resumeLiveTextHit("live-remote")}, nil) {
		t.Fatal("the live answer applies")
	}
	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Equal(got, []string{title}) {
		t.Fatalf("titles = %q", got)
	}
	resumeAssertAction(t, dashboard.NextWikiPreview(), DashboardActionLoadArchivedHits{
		WikiID: "live-remote",
		Query:  "rvector",
	})
	block := resumePlainBlock("user", "update rvector to use rq8")
	block.Hits = [][2]int{{7, 14}}
	dashboard.ApplyWikiHits("live-remote", "rvector", &client.WikiHitTranscript{
		Blocks:       []client.WikiHitBlock{block},
		OmittedAfter: 0,
	})
	if rendered := strings.Join(drawn(dashboard, 160, 40), "\n"); !strings.Contains(rendered, "update rvector to use rq8") {
		t.Fatal(rendered)
	}
	dashboard.ApplyWikiSearchResult(requestID, client.WikiSearchPage{}, errors.New("history unavailable"))
	if rows := resumeTestRows(t, dashboard); len(rows) != 1 {
		t.Fatalf("rows = %q", resumeTitles(rows))
	}
	if rendered := strings.Join(drawn(dashboard, 160, 40), "\n"); strings.Contains(rendered, "Search failed") {
		t.Fatal(rendered)
	}
	resumeFocusControl(t, dashboard, ResumeFocusSessions)
	resumeAssertAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionSelectWorkspace{WorkspaceID: "other"})
	other := "other"
	dashboard.SetActiveWorkspace(&other)
	if selected, ok := dashboard.SelectedSessionID(); !ok || selected != "live-remote" {
		t.Fatalf("selected = %q", selected)
	}
}

func TestEditingTheQueryRemovesOldLiveMatchesAndRejectsDelayedAnswers(t *testing.T) {
	dashboard := resumeDashboardWithLiveSessionsInTwoWorkspaces()
	openResumeDialog(dashboard, 1, nil)
	old := resumeTypeQuery(t, dashboard, "rvector")
	dashboard.ApplyResumeTextSearchResult(old, []client.SessionTextMatch{resumeLiveTextHit("live-remote")}, nil)
	if rows := resumeTestRows(t, dashboard); len(rows) != 1 {
		t.Fatalf("rows = %q", resumeTitles(rows))
	}

	current := resumeTypeQuery(t, dashboard, "x")
	if rows := resumeTestRows(t, dashboard); len(rows) != 0 {
		t.Fatalf("old matches clear before debounce: %q", resumeTitles(rows))
	}
	if dashboard.ApplyResumeTextSearchResult(old, []client.SessionTextMatch{resumeLiveTextHit("live-remote")}, nil) {
		t.Fatal("an old answer is dropped")
	}
	if dashboard.ApplyResumeTextSearchResult(old, nil, errors.New("old failure")) {
		t.Fatal("an old failure is dropped")
	}
	dashboard.ApplyResumeTextSearchResult(current, nil, nil)
	if rows := resumeTestRows(t, dashboard); len(rows) != 0 {
		t.Fatalf("rows = %q", resumeTitles(rows))
	}

	dashboard.HandleKey(key(tea.KeyEscape))
	if rows := resumeTestRows(t, dashboard); len(rows) != 3 {
		t.Fatalf("clearing the query restores every live row: %q", resumeTitles(rows))
	}
	if dashboard.ApplyResumeTextSearchResult(current, []client.SessionTextMatch{resumeLiveTextHit("live-remote")}, nil) {
		t.Fatal("the cleared query's answer is dropped")
	}
}

func TestFailedLiveSearchKeepsMetadataMatchesAndReportsTheFailure(t *testing.T) {
	dashboard := resumeDashboardWithLiveSessionsInTwoWorkspaces()
	openResumeDialog(dashboard, 1, nil)
	requestID := resumeTypeQuery(t, dashboard, "mast")
	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Equal(got, []string{"Raise the mast"}) {
		t.Fatalf("titles = %q", got)
	}
	dashboard.ApplyResumeTextSearchResult(requestID, nil, errors.New("index unavailable"))
	resumeApplyReadyRows(t, dashboard, []client.WikiRow{resumeWikiRow("old-session", true)})
	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Equal(got, []string{"Raise the mast"}) {
		t.Fatalf("titles = %q", got)
	}
	if rendered := strings.Join(drawn(dashboard, 180, 40), "\n"); !strings.Contains(rendered, "Conversation search failed: index unavailable") {
		t.Fatal(rendered)
	}
	resumeSwitchToArchive(t, dashboard)
	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Equal(got, []string{"archived old-session"}) {
		t.Fatalf("titles = %q", got)
	}
}

// I1-16: LAST ACTIVE for a running session is its last activity, not the
// time its record was last written (often its creation).
// Hard-won: 0841f27b: Finding I1-16 showed the Live tab using record update time instead of the session’s last activity.
func TestTheLiveTabShowsEachSessionsLastActivity(t *testing.T) {
	dashboard := resumeDashboardWithLiveSessionsInTwoWorkspaces()
	var sessionID string
	for id := range dashboard.state.Get().Sessions.Keys() {
		sessionID = id
		break
	}
	if sessionID == "" {
		t.Fatal("a live session")
	}
	recorded, ok := timestampMs(dashboard.state.Get().Sessions.Index(sessionID).UpdatedAt)
	if !ok {
		t.Fatal("the record has a time")
	}
	active := recorded + 20*60*1000
	activeAt := uint64(active)
	details := dashboard.sessionDetails.Mut()
	if *details == nil {
		*details = map[string]*SessionDetail{}
	}
	(*details)[sessionID] = &SessionDetail{lastActivityAtMs: &activeAt}
	openResumeDialog(dashboard, 1, nil)
	rows := resumeTestRows(t, dashboard)
	index := slices.IndexFunc(rows, func(row ResumeRow) bool { return row.key == resumeRowKeyLive(sessionID) })
	if index < 0 {
		t.Fatal("the session is listed")
	}
	if rows[index].lastActivityMs != active {
		t.Fatalf("last activity = %d, want %d", rows[index].lastActivityMs, active)
	}
}

// The arrows go on walking the strip after landing on a tab with nothing in
// it, which is every history tab while all of a dashboard's sessions run.
// Hard-won: c82060f6: Arrow keys stopped switching tabs when the active history list was empty.
func TestTheArrowsWalkTheStripAcrossATabWhoseListIsEmpty(t *testing.T) {
	dashboard := resumeDashboardWithLiveSessionsInTwoWorkspaces()
	openResumeDialog(dashboard, 1, nil)
	drawn(dashboard, 120, 40)
	if focused := resumeDialogFocus(t, dashboard); focused != ResumeFocusSessions {
		t.Fatalf("focus = %d", focused)
	}

	dashboard.HandleKey(key(tea.KeyRight))
	drawn(dashboard, 120, 40)
	if rows := resumeTestRows(t, dashboard); len(rows) != 0 {
		t.Fatalf("nothing is stopped: %q", resumeTitles(rows))
	}
	dialog := resumeOpenDialog(t, dashboard)
	if dialog.tab != ResumeTabHel {
		t.Fatalf("tab = %s", resumeTabDebug(dialog.tab))
	}
	if focused := dialog.focused(); focused != ResumeFocusTabs {
		t.Fatalf("an empty list cannot hold the keyboard, so the strip keeps it: %d", focused)
	}

	dashboard.HandleKey(key(tea.KeyRight))
	if tab := resumeOpenDialog(t, dashboard).tab; tab != ResumeTabImport {
		t.Fatalf("the arrows are still live: %s", resumeTabDebug(tab))
	}

	dashboard.HandleKey(key(tea.KeyLeft))
	dashboard.HandleKey(key(tea.KeyLeft))
	dialog = resumeOpenDialog(t, dashboard)
	if dialog.tab != ResumeTabLive {
		t.Fatalf("and Left comes all the way back: %s", resumeTabDebug(dialog.tab))
	}
	if focused := dialog.focused(); focused != ResumeFocusSessions {
		t.Fatalf("a tab with rows hands the keyboard back to them: %d", focused)
	}
}

// In the search box the arrows belong to the caret, as readline has them,
// and they reach the tab strip only from the end they are pressed against.
// Hard-won: c82060f6: Search-box arrows could not reach the tab strip from the end of a query.
func TestTheSearchArrowsReachTheStripOnlyFromTheEndOfTheQuery(t *testing.T) {
	dashboard := resumeDashboardWithLiveSessionsInTwoWorkspaces()
	openResumeDialog(dashboard, 1, nil)
	resumeApplyReadyRows(t, dashboard, nil)
	drawn(dashboard, 120, 40)
	dashboard.HandleKey(key('/'))
	dashboard.HandlePaste("mast")

	dashboard.HandleKey(key(tea.KeyLeft))
	dialog := resumeOpenDialog(t, dashboard)
	if dialog.search.Cursor() != 3 || dialog.tab != ResumeTabLive {
		t.Fatalf("the caret moved, not the tab: cursor %d, tab %s", dialog.search.Cursor(), resumeTabDebug(dialog.tab))
	}

	dashboard.HandleKey(key(tea.KeyRight))
	dialog = resumeOpenDialog(t, dashboard)
	if dialog.search.Cursor() != 4 || dialog.tab != ResumeTabLive {
		t.Fatalf("the caret reached the end: cursor %d, tab %s", dialog.search.Cursor(), resumeTabDebug(dialog.tab))
	}

	dashboard.HandleKey(key(tea.KeyRight))
	dialog = resumeOpenDialog(t, dashboard)
	if dialog.tab != ResumeTabHel {
		t.Fatalf("pressed against the end it tabs: %s", resumeTabDebug(dialog.tab))
	}
	if dialog.search.Value() != "mast" {
		t.Fatalf("the query comes along: %q", dialog.search.Value())
	}
	if focused := dialog.focused(); focused != ResumeFocusSearch {
		t.Fatalf("a person still typing keeps the box: %d", focused)
	}

	for range 4 {
		dashboard.HandleKey(key(tea.KeyLeft))
	}
	dialog = resumeOpenDialog(t, dashboard)
	if dialog.search.Cursor() != 0 || dialog.tab != ResumeTabHel {
		t.Fatalf("four Lefts only walk the query: cursor %d, tab %s", dialog.search.Cursor(), resumeTabDebug(dialog.tab))
	}

	dashboard.HandleKey(key(tea.KeyLeft))
	if tab := resumeOpenDialog(t, dashboard).tab; tab != ResumeTabLive {
		t.Fatalf("the fifth reaches the strip: %s", resumeTabDebug(tab))
	}
}

// Escape takes the query before it takes the dialog, then hands the keyboard
// back to the list, matching the palette and the Sessions pane's filter.
// Hard-won: c82060f6: Escape closed the dialog instead of clearing the query and leaving search focus in layers.
func TestEscapeClearsTheQueryThenLeavesTheBoxThenClosesTheDialog(t *testing.T) {
	dashboard := resumeDashboardWithLiveSessionsInTwoWorkspaces()
	openResumeDialog(dashboard, 1, nil)
	drawn(dashboard, 120, 40)
	dashboard.HandleKey(key('/'))
	dashboard.HandlePaste("mast")
	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Equal(got, []string{"Raise the mast"}) {
		t.Fatalf("titles = %q", got)
	}

	dashboard.HandleKey(key(tea.KeyEscape))
	if rows := resumeTestRows(t, dashboard); len(rows) != 3 {
		t.Fatalf("the whole list is back: %q", resumeTitles(rows))
	}
	dialog := resumeOpenDialog(t, dashboard)
	if !dialog.search.IsEmpty() {
		t.Fatal("the query went, not the dialog")
	}
	if focused := dialog.focused(); focused != ResumeFocusSearch {
		t.Fatalf("clearing the box does not move the keyboard out of it: %d", focused)
	}

	dashboard.HandleKey(key(tea.KeyEscape))
	if focused := resumeDialogFocus(t, dashboard); focused != ResumeFocusSessions {
		t.Fatalf("an empty box gives the list back: %d", focused)
	}

	dashboard.HandleKey(key(tea.KeyEscape))
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("with nothing left to peel, Escape closes the dialog: %#v", dashboard.mode)
	}
}

// One match is not "1 matches". The heading counts in the reader's grammar.
// Hard-won: c82060f6: The search heading rendered the singular count as “1 matches”.
func TestTheSearchHeadingCountsASingleMatchInTheSingular(t *testing.T) {
	dashboard := resumeDashboardWithLiveSessionsInTwoWorkspaces()
	openResumeDialog(dashboard, 1, nil)
	// A still-building index says so in the heading instead of counting.
	resumeApplyReadyRows(t, dashboard, nil)
	resumeReplaceSearch(t, dashboard, "mast")
	lines := strings.Join(drawn(dashboard, 120, 40), "\n")
	if !strings.Contains(lines, " 1 match ") || strings.Contains(lines, "1 matches") {
		t.Fatal(lines)
	}

	// The plural survives; only the count of one was wrong.
	resumeReplaceSearch(t, dashboard, "Default")
	lines = strings.Join(drawn(dashboard, 120, 40), "\n")
	if !strings.Contains(lines, " 2 matches ") {
		t.Fatal(lines)
	}
}

// Scans arrive one profile at a time; folding one in must not move the
// Import-tab selection off the native row the user was on.
func TestAnIncrementalScanUpdateKeepsTheSelectedRow(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stoppedSession()}))
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile([]ImportSessionOption{
		resumeNative("native-2", "Older", 1),
	})})
	resumeSwitchToImport(t, dashboard)
	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Equal(got, []string{"Older"}) {
		t.Fatalf("titles = %q", got)
	}
	if index := resumeOpenDialog(t, dashboard).rowIndex; index != 0 {
		t.Fatalf("row index = %d", index)
	}

	// A newer native session arrives and sorts above the selected row.
	dashboard.ApplyResumeProfile(1, resumeCodexProfile([]ImportSessionOption{
		resumeNative("native-2", "Older", 1),
		resumeNative("native-3", "Newer", resumeNewerThanTheCheckpoint),
	}))
	dialog := resumeOpenDialog(t, dashboard)
	if dialog.selected == nil || *dialog.selected != resumeRowKeyNative(core.HarnessKindCodex, "native-2") {
		t.Fatalf("selected = %#v", dialog.selected)
	}
	if dialog.rowIndex != 1 {
		t.Fatalf("row index = %d", dialog.rowIndex)
	}
	// A late update for another discovery is ignored.
	dashboard.ApplyResumeProfile(99, resumeCodexProfile(nil))
	if rows := resumeTestRows(t, dashboard); len(rows) != 2 {
		t.Fatalf("rows = %q", resumeTitles(rows))
	}
}

func TestProviderArchiveMetadataDoesNotMoveTheSelectedRow(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith(nil))
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile([]ImportSessionOption{
		resumeNative("native-new", "Newer", resumeNewerThanTheCheckpoint),
		resumeNative("native-old", "Older", 1),
	})})
	resumeSwitchToImport(t, dashboard)
	dashboard.HandleKey(key(tea.KeyDown))
	dialog := resumeOpenDialog(t, dashboard)
	oldKey := resumeRowKeyNative(core.HarnessKindCodex, "native-old")
	if dialog.rowIndex != 1 || dialog.selected == nil || *dialog.selected != oldKey {
		t.Fatalf("row index %d, selected %#v", dialog.rowIndex, dialog.selected)
	}

	old := resumeNative("native-old", "Older", 1)
	old.NativelyArchived = true
	dashboard.ApplyResumeProfile(1, resumeCodexProfile([]ImportSessionOption{
		resumeNative("native-new", "Newer", resumeNewerThanTheCheckpoint),
		old,
	}))

	dialog = resumeOpenDialog(t, dashboard)
	if dialog.rowIndex != 1 || dialog.selected == nil || *dialog.selected != oldKey {
		t.Fatalf("row index %d, selected %#v", dialog.rowIndex, dialog.selected)
	}
	resumeAssertAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionImportSession{
		ProfileID:       "codex-1",
		NativeSessionID: "native-old",
		DisplayTitle:    "Older",
	})
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode = %#v", dashboard.mode)
	}
}

// A scan that failed is reported rather than dropped.
func TestAFailedProfileScanIsReportedInTheDialog(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith(nil))
	failure := "permission denied"
	openResumeDialog(dashboard, 1, []ImportProfileOption{{
		ProfileID:   "codex-1",
		HarnessKind: core.HarnessKindCodex,
		Error:       &failure,
	}})
	resumeSwitchToImport(t, dashboard)
	rendered := strings.Join(bufferLines(resumeRenderedBuffer(dashboard, 100, 30)), "\n")
	if !strings.Contains(rendered, "Scan failed for codex-1") {
		t.Fatal(rendered)
	}
}

// The rows are derived state, rebuilt where their inputs change. A state
// reload and a checkpoint size that arrives from the background reach the
// open dialog straight away.
func TestTheRowListFollowsStateReloadsAndBackgroundUpdatesWhileTheDialogIsOpen(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stoppedSession()}))
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile([]ImportSessionOption{
		resumeNative("native-2", "Native", resumeNewerThanTheCheckpoint),
	})})
	resumeSwitchToHel(t, dashboard)
	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Equal(got, []string{"ACP pretty name"}) {
		t.Fatalf("titles = %q", got)
	}

	reloaded := stoppedSession()
	reloaded.ID = "session-2"
	reloaded.NativeSessionID = nil
	reloadedTitle := "Reloaded record"
	reloaded.ACPSessionTitle = &reloadedTitle
	dashboard.SetState(resumeStateWith([]core.SessionRecord{stoppedSession(), reloaded}))
	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Contains(got, "Reloaded record") {
		t.Fatalf("titles = %q", got)
	}

	size := uint64(2_048)
	dashboard.ApplyCheckpointArchiveSizes(map[string]*uint64{"session-1": &size})
	listed := resumeTestRows(t, dashboard)
	sized := slices.IndexFunc(listed, func(row ResumeRow) bool { return row.key == resumeRowKeyHel("session-1") })
	if sized < 0 {
		t.Fatal("the checkpointed record")
	}
	if !strings.Contains(listed[sized].details, "2.0K") {
		t.Fatal(listed[sized].details)
	}

	resumeSwitchToImport(t, dashboard)
	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Equal(got, []string{"Native"}) {
		t.Fatalf("titles = %q", got)
	}
	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Equal(got, []string{"Native"}) {
		t.Fatalf("titles = %q", got)
	}
}

// A search answer can put a row with a transcript under an unmoved
// selection. The preview pane promises that transcript, so the dialog asks
// for it without waiting for the selection to move.
// Hard-won: b2dbce5a: A live-terminal finding left the preview stuck on “Loading the archived transcript” after a search answer selected a different row.
func TestASearchAnswerAsksForTheNewlySelectedRowsTranscript(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stoppedSession()}))
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile(nil)})
	resumeSwitchToHel(t, dashboard)
	resumeApplyReadyRows(t, dashboard, nil)
	if action := dashboard.NextWikiPreview(); !wizardActionIsNone(action) {
		t.Fatalf("a row with no indexed session has no transcript to show: %#v", action)
	}

	held := resumeWikiRow("held", false)
	session := "session-1"
	snippet := "the phrase that matched"
	held.HelSessionID = &session
	held.Snippet = &snippet
	resumeApplyReadyRows(t, dashboard, []client.WikiRow{held})
	resumeAssertAction(t, dashboard.NextWikiPreview(), DashboardActionLoadArchivedBrief{WikiID: "held"})
	if action := dashboard.NextWikiPreview(); !wizardActionIsNone(action) {
		t.Fatalf("one request per row, not one per answer: %#v", action)
	}
}

// Hard-won: e81c95d9a: a background refresh cleared the selected row before its replacement arrived.
func TestASyncRefreshPreservesResultsAndSelectionUntilBothAnswersArrive(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith(nil))
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile(nil)})
	dashboard.switchResumeTab(ResumeTabArchive)
	resumeApplyReadyRows(t, dashboard, []client.WikiRow{resumeWikiRow("first", true), resumeWikiRow("selected", true)})
	dashboard.selectResumeRow(1)
	before := dashboard.selectedResumeRow().key
	completed := resumeSyncProgressStatus()
	completed.Finished = 1
	completed.Successful = 1
	if !dashboard.ApplyWikiSync(true, 1, completed, nil) {
		t.Fatal("the sync completion applies")
	}
	refresh, _, ok := dashboard.RefreshWikiSearch()
	if !ok {
		t.Fatal("the dialog refreshes its search")
	}
	if got := dashboard.selectedResumeRow().key; got != before {
		t.Fatalf("selected = %#v", got)
	}
	if rows := resumeTestRows(t, dashboard); len(rows) != 2 {
		t.Fatalf("rows = %q", resumeTitles(rows))
	}
	if rendered := strings.Join(drawn(dashboard, 80, 34), "\n"); !strings.Contains(rendered, "Searching") {
		t.Fatal(rendered)
	}
	dashboard.ApplyResumeTextSearchResult(refresh, nil, nil)
	if got := dashboard.selectedResumeRow().key; got != before {
		t.Fatalf("selected = %#v", got)
	}
	if rendered := strings.Join(drawn(dashboard, 80, 34), "\n"); !strings.Contains(rendered, "Searching") {
		t.Fatal(rendered)
	}
	dashboard.ApplyWikiSearch(refresh, resumeReadyPage([]client.WikiRow{
		resumeWikiRow("new", true),
		resumeWikiRow("first", true),
		resumeWikiRow("selected", true),
	}))
	if got := dashboard.selectedResumeRow().key; got != before {
		t.Fatalf("selected = %#v", got)
	}
	if rendered := strings.Join(drawn(dashboard, 80, 34), "\n"); strings.Contains(rendered, "Searching") {
		t.Fatal(rendered)
	}
}

// Hard-won: eb0fbd898: keystroke answers must not clear the ongoing sync status.
func TestAnEmptyTabKeepsItsAnswerWhileTheIndexSyncs(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith(nil))
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile(nil)})
	resumeReplaceSearch(t, dashboard, "quokka")
	resumeSwitchToHel(t, dashboard)
	requestID, _, ok := dashboard.NextWikiSearch()
	if !ok {
		t.Fatal("a search is asked for")
	}
	render := strings.Join(drawn(dashboard, 120, 34), "\n")
	if !strings.Contains(render, "Searching…") || strings.Contains(render, "index building") {
		t.Fatal(render)
	}
	dashboard.ApplyWikiSync(true, 1, resumeSyncProgressStatus(), nil)
	for range 3 {
		dashboard.ApplyWikiSearch(requestID, client.WikiSearchPage{
			Status: client.WikiStatus{State: client.WikiIndexStateReady},
		})
		render := strings.Join(drawn(dashboard, 120, 34), "\n")
		if !strings.Contains(render, "Syncing Claude Code 2/7 ▕███░░░░░░░▏ 1204/3090") {
			t.Fatal(render)
		}
		if strings.Contains(render, "Searching…") {
			t.Fatal(render)
		}
		if dashboard.NeedsFastTick() {
			t.Fatal("nothing animates while the answer stands")
		}
	}
	completed := resumeSyncProgressStatus()
	// The watch skipped the covering success and saw a later failed pass.
	completed.Finished = 2
	completed.Successful = 1
	completed.Outcome = client.WikiSyncOutcomeFailed{Reason: "later pass failed"}
	if !dashboard.ApplyWikiSync(true, 1, completed, nil) {
		t.Fatal("a covering success refreshes despite a later failed pass")
	}
	dashboard.ApplyWikiSearch(requestID, resumeReadyPage(nil))
	render = strings.Join(drawn(dashboard, 120, 34), "\n")
	if strings.Contains(render, "Syncing Claude Code") || strings.Contains(render, "Index sync failed") ||
		strings.Contains(render, "Searching…") {
		t.Fatal(render)
	}
}

func TestSearchErrorsObeyQueryIdentityAndEndThePendingState(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith(nil))
	openResumeDialog(dashboard, 1, nil)
	resumeSwitchToArchive(t, dashboard)
	resumeReplaceSearch(t, dashboard, "quokka")
	old, _, _ := dashboard.NextWikiSearch()
	current, _, _ := dashboard.NextWikiSearch()
	if dashboard.ApplyWikiSearchResult(old, client.WikiSearchPage{}, errors.New("stale failure")) {
		t.Fatal("a stale failure is dropped")
	}
	if _, pending := resumeDialog(dashboard.mode).wikiSearch.(WikiSearchStatePending); !pending {
		t.Fatalf("search = %#v", resumeDialog(dashboard.mode).wikiSearch)
	}
	if !dashboard.ApplyWikiSearchResult(current, client.WikiSearchPage{}, errors.New("database unavailable")) {
		t.Fatal("the current failure applies")
	}
	if got := resumeDialog(dashboard.mode).wikiSearch; got != (WikiSearchStateFailed{Value: "database unavailable"}) {
		t.Fatalf("search = %#v", got)
	}
	render := strings.Join(drawn(dashboard, 120, 34), "\n")
	if !strings.Contains(render, "Search failed: database unavailable") || strings.Contains(render, "Searching…") {
		t.Fatal(render)
	}
	completed := resumeSyncProgressStatus()
	completed.Finished = 1
	completed.Successful = 1
	if !dashboard.ApplyWikiSync(true, 1, completed, nil) {
		t.Fatal("the sync completion applies")
	}
	refresh, _, _ := dashboard.RefreshWikiSearch()
	dashboard.ApplyWikiSearch(refresh, resumeReadyPage(nil))
	dashboard.ApplyResumeTextSearchResult(refresh, nil, nil)
	// Hard-won: 7d06e3b: a later typed query's failure stuck as a sync failure.
	typed, _, _ := dashboard.NextWikiSearch()
	dashboard.ApplyWikiSearchResult(typed, client.WikiSearchPage{}, errors.New("ordinary search failed"))
	dashboard.ApplyResumeTextSearchResult(typed, nil, errors.New("ordinary conversation search failed"))
	if label, ok := resumeDialog(dashboard.mode).sync.Label(); ok {
		t.Fatalf("sync label = %q", label)
	}
	dashboard.ApplyWikiSearch(typed, resumeReadyPage(nil))
	dashboard.ApplyResumeTextSearchResult(typed, nil, nil)
	refresh, _, _ = dashboard.RefreshWikiSearch()
	if !dashboard.ApplyWikiSearchResult(refresh, client.WikiSearchPage{}, errors.New("final read failed")) {
		t.Fatal("the refresh failure applies")
	}
	render = strings.Join(drawn(dashboard, 120, 34), "\n")
	if !strings.Contains(render, "Index sync failed: final read failed") {
		t.Fatal(render)
	}
}

func TestReopeningTheDialogCannotAcceptThePreviousQuerysAnswer(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith(nil))
	openResumeDialog(dashboard, 1, nil)
	old, _, _ := dashboard.NextWikiSearch()
	dashboard.mode = ModeDashboard{}
	if _, ok := dashboard.ResumeSearchRequestID(); ok {
		t.Fatal("no dialog, no request")
	}
	openResumeDialog(dashboard, 2, nil)
	current, _, _ := dashboard.NextWikiSearch()
	if old == current {
		t.Fatalf("request ids repeat: %d", old)
	}
	if dashboard.ApplyWikiSearchResult(old, resumeReadyPage([]client.WikiRow{resumeWikiRow("old", true)}), nil) {
		t.Fatal("the previous dialog's answer is dropped")
	}
	if wiki := resumeDialog(dashboard.mode).wiki; len(wiki) != 0 {
		t.Fatalf("wiki = %#v", wiki)
	}
}

// Cost of the merged row list and of one keypress, on a dialog the size a
// long-lived harness home produces. Run with
// `go test ./internal/tui -run TestResumeRowCostForAFewThousandSessions -v`
// after removing the skip.
func TestResumeRowCostForAFewThousandSessions(t *testing.T) {
	t.Skip("timing measurement, not a behavior assertion")
	const native, records, rounds = 4_000, 400, 200

	var recordList []core.SessionRecord
	for index := range records {
		session := stoppedSession()
		session.ID = fmt.Sprintf("session-%04d", index)
		session.NativeSessionID = nil
		title := fmt.Sprintf("Record %d", index)
		session.ACPSessionTitle = &title
		recordList = append(recordList, session)
	}
	var sessions []ImportSessionOption
	for index := range native {
		sessions = append(sessions, resumeNative(
			fmt.Sprintf("native-%04d", index),
			fmt.Sprintf("Native conversation %d", index),
			int64(index),
		))
	}
	dashboard := resumeNewDashboard(config(), resumeStateWith(recordList))
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile(sessions)})
	resumeSwitchToImport(t, dashboard)

	dialog := resumeOpenDialog(t, dashboard)
	// What one rebuild costs: the merge, the sizes, and search.
	started := time.Now()
	built := 0
	for range rounds {
		rows, _ := buildResumeRows(dashboard.config.Get(), dashboard.state.Get(), dialog, dashboard.checkpointArchiveSizes)
		built += len(rows)
	}
	rebuild := time.Since(started)

	// What the dialog actually pays per key press, which reads the rows
	// rather than rebuilding them.
	started = time.Now()
	for range rounds {
		dashboard.HandleKey(key(tea.KeyDown))
	}
	keypresses := time.Since(started)

	t.Logf(
		"rows=%d rebuild=%v per_rebuild=%v keypresses=%v per_key=%v",
		built/rounds, rebuild, rebuild/rounds, keypresses, keypresses/rounds,
	)
}

// A query's hits are counted on every tab, not only the one on screen, and an
// empty tab says where the other hits are rather than switching by itself.
// Hard-won: 62ba3c1b: Issue #1161 found stopped sub-agent records could fill the resume list with dozens of rows for one session.
func TestSubAgentsAreNeverOfferedForResume(t *testing.T) {
	owner := stoppedSession()
	managedChild := stoppedSession()
	managedChild.Project = nil
	managedChild.ID = "child-1"
	lane := "Managed lane"
	managedChild.ACPSessionTitle = &lane
	nativeChild := "native-child"
	managedChild.NativeSessionID = &nativeChild
	state := resumeStateWith([]core.SessionRecord{owner, managedChild})
	state.Subagents.Insert(managedChild.ID, core.SubagentMetadata{
		Kind:            core.SubagentKindWorker,
		ChildSessionID:  managedChild.ID,
		ParentSessionID: owner.ID,
		TaskName:        "lane",
		ProfileID:       managedChild.LastProfile,
		RequestKey:      "request-1",
		CreatedAt:       managedChild.CreatedAt,
	})
	dashboard := resumeNewDashboard(config(), state)
	// A harness-owned child the owner spawned, shown the way the TUI shows
	// it: as a stopped copy of the owner's record.
	agent := core.NativeAgent{
		OwnerSessionID: owner.ID,
		SessionID:      "a0c7080aee7ead7c5",
		Name:           "Regression: bridge derivation conflict",
		Task:           "Fix the regression",
		State:          core.NativeAgentStateCompleted,
	}
	dashboard.SetNativeAgents([]core.NativeAgentView{{
		GenerationOrdinal: 1,
		Projection:        core.MaterializedSessionEmpty(agent.ViewID()),
		Agent:             agent,
	}})
	openResumeDialog(dashboard, 1, nil)
	resumeSwitchToHel(t, dashboard)

	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Equal(got, []string{"ACP pretty name"}) {
		t.Fatalf("titles = %q", got)
	}

	// A search hit on a child counts on no tab.
	resumeReplaceSearch(t, dashboard, "the phrase")
	childHit := resumeWikiRow("child-hit", false)
	childID := managedChild.ID
	childHit.HelSessionID = &childID
	ownerHit := resumeWikiRow("owner-hit", false)
	ownerID := owner.ID
	ownerHit.HelSessionID = &ownerID
	resumeApplyReadyRows(t, dashboard, []client.WikiRow{childHit, ownerHit})
	if !slices.Equal(dashboard.resumeHitCounts, []int{0, 1, 0, 0}) {
		t.Fatalf("hit counts = %v", dashboard.resumeHitCounts)
	}
	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Equal(got, []string{"ACP pretty name"}) {
		t.Fatalf("titles = %q", got)
	}
}

// resumeArchivedRows is one archived row per index, so the preview pane has a
// row to sit under and the list has enough rows to scroll.
func resumeArchivedRows(count int) []client.WikiRow {
	var rows []client.WikiRow
	for index := range count {
		row := resumeWikiRow(fmt.Sprintf("archive-%d", index), true)
		started := fmt.Sprintf("2026-09-%02dT00:00:00Z", index+1)
		row.Started = &started
		rows = append(rows, row)
	}
	return rows
}

// The wheel moves whichever pane it is over: the preview scrolls under the
// pointer and stops at the end of the text, and the list still moves when
// the pointer is over the list.
func TestTheWheelScrollsThePreviewPaneItIsOver(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stoppedSession()}))
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile(nil)})
	resumeApplyReadyRows(t, dashboard, resumeArchivedRows(12))
	resumeSwitchToArchive(t, dashboard)
	selected, ok := resumeTestRows(t, dashboard)[0].wikiID()
	if !ok {
		t.Fatal("an archived row previews its own transcript")
	}
	transcript := make([]string, 120)
	for index := range transcript {
		transcript[index] = fmt.Sprintf("transcript line %d", index)
	}
	dashboard.ApplyWikiBrief(selected, strings.Join(transcript, "\n"))

	resumeRenderedBuffer(dashboard, 120, 40)
	previewSurface := dashboard.FrameSurfaces().Surface(chat.SurfaceIdResumePreview{})
	if previewSurface == nil {
		t.Fatal("the preview pane registers a scrollable surface")
	}
	preview := *previewSurface
	listSurface := dashboard.FrameSurfaces().Surface(chat.SurfaceIdResumeList{})
	if listSurface == nil {
		t.Fatal("the list registers its surface")
	}
	list := *listSurface
	if preview.TotalRows <= preview.Rect.Dy() {
		t.Fatal("the fixture overflows the pane")
	}

	dashboard.HandleMouse(mouseIn(resumeWheelDown, preview.Rect))
	scroll := func() int { return resumeOpenDialog(t, dashboard).previewScroll }
	if got := scroll(); got != 3 {
		t.Fatalf("one notch moves three rows: %d", got)
	}

	for range 200 {
		dashboard.HandleMouse(mouseIn(resumeWheelDown, preview.Rect))
	}
	end := max(preview.TotalRows-preview.Rect.Dy(), 0)
	if got := scroll(); got != end {
		t.Fatalf("the pane stops at the end: %d, want %d", got, end)
	}

	// The wheel over the list still moves the list, and leaves the pane
	// where the reader left it.
	before := scroll()
	dashboard.HandleMouse(mouseIn(resumeWheelDown, list.Rect))
	dialog := resumeOpenDialog(t, dashboard)
	if dialog.rowIndex != 1 {
		t.Fatalf("the list moved under the pointer: %d", dialog.rowIndex)
	}
	// A different transcript under the selection opens at its top.
	if dialog.previewScroll != 0 || before <= 0 {
		t.Fatalf("preview scroll %d, before %d", dialog.previewScroll, before)
	}
}

func TestThePreviewScrollbarSeeksAndDragsToBothEnds(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith(nil))
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile(nil)})
	resumeApplyReadyRows(t, dashboard, resumeArchivedRows(1))
	resumeSwitchToArchive(t, dashboard)
	transcript := make([]string, 120)
	for index := range transcript {
		transcript[index] = fmt.Sprintf("line %d", index)
	}
	dashboard.ApplyWikiBrief("archive-0", strings.Join(transcript, "\n"))
	drawn(dashboard, 120, 40)
	geometry, ok := resumeOpenDialog(t, dashboard).previewScrollbar.Geometry()
	if !ok {
		t.Fatal("the preview scrollbar has a geometry")
	}
	at := func(kind tea.MouseMsg, row int) tea.MouseMsg {
		return mouseAt(kind, uv.Pos(geometry.Track.Min.X, row))
	}
	if !dashboard.ComponentHandlesMouse(at(resumeLeftDown, geometry.Track.Min.Y)) {
		t.Fatal("the scrollbar takes a press on its track")
	}
	dashboard.HandleMouse(at(resumeLeftDown, geometry.Track.Min.Y))
	dashboard.HandleMouse(at(resumeLeftDrag, 65_535))
	if got := resumeOpenDialog(t, dashboard).previewScroll; got != geometry.MaxScroll {
		t.Fatalf("scroll = %d, want %d", got, geometry.MaxScroll)
	}
	dashboard.HandleMouse(at(resumeLeftDrag, 0))
	if got := resumeOpenDialog(t, dashboard).previewScroll; got != 0 {
		t.Fatalf("scroll = %d", got)
	}
	dashboard.HandleMouse(at(resumeLeftUp, 0))
	if dashboard.ComponentHandlesMouse(at(resumeLeftDrag, 0)) {
		t.Fatal("a released scrollbar no longer takes a drag")
	}
}

// A search that is still running says so, and only the answer to the
// outstanding request ends the wait.
func TestSearchReplyClearsPendingForMatchingRequestOnly(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith([]core.SessionRecord{stoppedSession()}))
	openResumeDialog(dashboard, 1, []ImportProfileOption{resumeCodexProfile(nil)})
	target := uint64(1)
	dashboard.ApplyWikiSync(true, 1, client.WikiSyncStatus{
		Target:     &target,
		Started:    1,
		Requested:  1,
		Finished:   1,
		Successful: 1,
		Outcome:    client.WikiSyncOutcomeSucceeded{},
	}, nil)
	resumeReplaceSearch(t, dashboard, "the phrase")
	requestID, _, ok := dashboard.NextWikiSearch()
	if !ok {
		t.Fatal("a search is asked for")
	}
	pending := func() bool {
		_, pending := resumeOpenDialog(t, dashboard).wikiSearch.(WikiSearchStatePending)
		return pending
	}
	if !pending() {
		t.Fatal("the search is pending")
	}
	if !dashboard.NeedsFastTick() {
		t.Fatal("the searching spinner animates while the answer is outstanding")
	}

	dashboard.ApplyWikiSearch(requestID-1, resumeReadyPage(nil))
	if !pending() {
		t.Fatal("an answer to an older query does not end this wait")
	}

	dashboard.ApplyWikiSearch(requestID, resumeReadyPage(nil))
	if pending() {
		t.Fatal("the answer ends the wait")
	}
	if !dashboard.NeedsFastTick() {
		t.Fatal("Live is still awaiting its independent answer")
	}
	dashboard.ApplyResumeTextSearchResult(requestID, nil, nil)
	if dashboard.NeedsFastTick() {
		t.Fatal("nothing animates once both answers are in")
	}
}

// resumeHitBlock is one matching message: filler lines of context above the
// match, so two blocks put their hits far apart in the pane.
func resumeHitBlock(role string, filler int) client.WikiHitBlock {
	var text strings.Builder
	for index := range filler {
		fmt.Fprintf(&text, "filler line %d\n", index)
	}
	start := text.Len()
	text.WriteString("needle")
	return client.WikiHitBlock{
		Role: role,
		Hits: [][2]int{{start, text.Len()}},
		Text: text.String(),
	}
}

// resumePlainBlock is one message with no match, so a fixture can place plain
// blocks of a given role around the ones that match.
func resumePlainBlock(role, text string) client.WikiHitBlock {
	return client.WikiHitBlock{Role: role, Text: text}
}

// A run of tool messages is one `[tool calls]` line however long the run is,
// and the two conversational roles carry the colours the conversation view
// gives them, so a reader moves between the two surfaces without relearning
// them.
func TestThePreviewCollapsesToolRunsAndColoursTheConversationRoles(t *testing.T) {
	lines, _ := hitTranscriptLines(&client.WikiHitTranscript{
		Blocks: []client.WikiHitBlock{
			resumePlainBlock("user", "make it build"),
			resumePlainBlock("tool", "cargo build"),
			resumePlainBlock("tool", "cargo clippy"),
			resumePlainBlock("tool", "cargo test"),
			resumePlainBlock("assistant", "it builds"),
			resumePlainBlock("tool", "git commit"),
		},
	})
	text := func(line chat.Line) string {
		var content strings.Builder
		for _, span := range line.Spans {
			content.WriteString(span.Content)
		}
		return content.String()
	}
	rendered := make([]string, len(lines))
	for index, line := range lines {
		rendered[index] = text(line)
	}

	if want := []string{
		"User: make it build",
		"",
		"[tool calls]",
		"",
		"Assistant: it builds",
		"",
		"[tool calls]",
	}; !slices.Equal(rendered, want) {
		t.Fatalf("three tool messages in a row are one line, and a later run is its own: %q", rendered)
	}

	lineFor := func(needle string) chat.Line {
		for _, line := range lines {
			if strings.HasPrefix(text(line), needle) {
				return line
			}
		}
		t.Fatalf("a line for the role %q", needle)
		return chat.Line{}
	}
	// The role label is one span in front of the message text, so its colour
	// is the span's; the collapsed run is a whole styled line.
	if got := lineFor("User: ").Spans[0].Style.Fg; got != chat.ThemePalette().Accent {
		t.Fatalf("user label = %#v", got)
	}
	if got := lineFor("Assistant: ").Spans[0].Style.Fg; got != chat.ThemePalette().Secondary {
		t.Fatalf("assistant label = %#v", got)
	}
	if got := lineFor("[tool calls]").Style.Fg; got != chat.ThemePalette().Muted {
		t.Fatalf("tool run = %#v", got)
	}
}

// The runtime feed carries only live sessions, so the Mjolnir tab lists what
// the daemon answers when the dialog opens. Until then neither the stopped
// sessions nor the import list are shown: nothing yet says which native
// sessions Mjolnir already holds.
func TestTheMjolnirTabListsTheDaemonsAnswerAndResumesASessionTheFeedLacks(t *testing.T) {
	dashboard := resumeNewDashboard(config(), resumeStateWith(nil))
	dashboard.ShowResumeDialog(2, []ImportProfileOption{resumeCodexProfile([]ImportSessionOption{
		resumeNative("adopted", "Already held", resumeNewerThanTheCheckpoint),
		resumeNative("free", "Importable", resumeNewerThanTheCheckpoint),
	})})
	resumeSwitchToHel(t, dashboard)
	if rows := resumeTestRows(t, dashboard); len(rows) != 0 {
		t.Fatalf("rows = %q", resumeTitles(rows))
	}
	resumeSwitchToImport(t, dashboard)
	if rows := resumeTestRows(t, dashboard); len(rows) != 0 {
		t.Fatalf("rows = %q", resumeTitles(rows))
	}

	candidates := func() client.ResumeCandidates {
		session := stoppedSession()
		cfg := config()
		return client.ResumeCandidates{
			Candidates: []client.ResumeCandidate{client.ResumeCandidateOf(&session, &cfg)},
			AdoptedNativeSessions: []client.DaemonPair[core.HarnessKind, string]{
				{First: core.HarnessKindCodex, Second: "adopted"},
			},
		}
	}
	if dashboard.ApplyResumeCandidates(1, candidates(), nil) {
		t.Fatal("an answer for an earlier dialog is dropped")
	}
	if rows := resumeTestRows(t, dashboard); len(rows) != 0 {
		t.Fatalf("rows = %q", resumeTitles(rows))
	}
	if !dashboard.ApplyResumeCandidates(2, candidates(), nil) {
		t.Fatal("the answer for this dialog applies")
	}
	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Equal(got, []string{"Importable"}) {
		t.Fatalf("titles = %q", got)
	}
	resumeSwitchToHel(t, dashboard)
	if got := resumeTitles(resumeTestRows(t, dashboard)); !slices.Equal(got, []string{"ACP pretty name"}) {
		t.Fatalf("titles = %q", got)
	}

	// The rows are previews; the wizard opens on the record fetched for the
	// row picked, and an answer for another row is dropped.
	resumeAssertAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionLoadResumeRecord{SessionID: "session-1"})
	other := stoppedSession()
	other.ID = "other"
	resumeFixture := func(record core.SessionRecord) *client.SessionRecordWithVerdict {
		return &client.SessionRecordWithVerdict{
			Record:  record,
			Verdict: client.SessionVerdict{Resume: client.SessionOfferReady},
		}
	}
	dashboard.ApplyResumeRecord("other", resumeFixture(other), nil)
	if _, ok := dashboard.mode.(ModeResumeDialog); !ok {
		t.Fatalf("mode = %#v", dashboard.mode)
	}
	dashboard.ApplyResumeRecord("session-1", resumeFixture(stoppedSession()), nil)
	mode, ok := dashboard.mode.(ModeResume)
	if !ok {
		t.Fatalf("expected the resume wizard, got %#v", dashboard.mode)
	}
	if mode.sessionID != "session-1" {
		t.Fatalf("session = %q", mode.sessionID)
	}
	if !dashboard.state.Get().Sessions.IsEmpty() {
		t.Fatal("the feed's records are unchanged")
	}

	dashboard.CancelModal()
	dashboard.SetSessionVerdicts(core.NewSnapshotMap[string, client.SessionVerdict]())
	dashboard.BeginResumeFor("session-1")
	if _, ok := dashboard.mode.(ModeResume); !ok {
		t.Fatalf("mode = %#v", dashboard.mode)
	}
}

func TestASearchAnswerUpdatesTheOpenDialog(t *testing.T) {
	dashboard := resumeNewDashboard(config(), core.NewStateState())
	openResumeDialog(dashboard, 1, nil)
	resumeSwitchToArchive(t, dashboard)
	request, ok := dashboard.ResumeSearchRequestID()
	if !ok {
		t.Fatal("the dialog has a search request")
	}
	if again, ok := dashboard.ResumeSearchRequestID(); !ok || again != request {
		t.Fatalf("request = %d", again)
	}
	if !dashboard.ApplyWikiSearchResult(request, client.WikiSearchPage{
		Status: client.WikiStatus{State: client.WikiIndexStateReady},
		Rows:   []client.WikiRow{resumeWikiRow("answered-under-help", true)},
	}, nil) {
		t.Fatal("the answer applies")
	}
	resumeAssertAction(t, dashboard.NextWikiPreview(), DashboardActionLoadArchivedBrief{WikiID: "answered-under-help"})
	dashboard.ApplyWikiBrief("answered-under-help", "A retained preview")
	dialog := resumeOpenDialog(t, dashboard)
	if dialog.wiki[0].ID != "answered-under-help" {
		t.Fatalf("wiki = %#v", dialog.wiki)
	}
	if preview, ok := dialog.previews["answered-under-help"]; !ok || preview != "A retained preview" {
		t.Fatalf("preview = %q", preview)
	}
	if dialog.previewPending != nil {
		t.Fatal("the preview is no longer pending")
	}
	if _, answered := dialog.wikiSearch.(WikiSearchStateAnswered); !answered {
		t.Fatalf("search = %#v", dialog.wikiSearch)
	}
}

func resumeSyncProgressStatus() client.WikiSyncStatus {
	target := uint64(1)
	total := 3090
	return client.WikiSyncStatus{
		Epoch:     "fixture",
		Target:    &target,
		Requested: 1,
		Started:   1,
		Running: &client.WikiSyncProgress{
			Phase:    "Native",
			Tool:     "Claude Code",
			Adapter:  2,
			Adapters: 7,
			Done:     1204,
			Total:    &total,
		},
	}
}
