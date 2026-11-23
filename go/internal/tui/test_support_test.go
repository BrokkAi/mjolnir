// Port of mj-tui/src/test_support.rs.

// Shared fixtures for the dashboard unit tests.
//
// crossterm's KeyEvent is a tea.KeyPressMsg here and its MouseEvent a
// tea.MouseMsg. A mouse event's kind (crossterm's MouseEventKind) is passed
// as a prototype message of the matching type carrying its button, such as
// tea.MouseClickMsg{Button: tea.MouseLeft} for Down(Left) or
// tea.MouseWheelMsg{Button: tea.MouseWheelUp} for ScrollUp; its position is
// ignored.

package tui

import (
	"fmt"
	"runtime"
	"strings"
	"testing"
	"unicode"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Existing dashboard fixtures describe probe inputs and outcomes. Adapt them
// to the same host rows the daemon publishes to the dashboard.
func (d *DashboardState) setDeploymentCapacityTargets(targets []core.DeploymentCapacityTarget) {
	panic("unported: mj-tui/src/test_support.rs DashboardState::set_deployment_capacity_targets")
}

// applyDeploymentCapacity applies one probe outcome (Rust's
// `Result<Option<DeploymentCapacityUsage>, String>`: failure set for Err,
// usage nil for Ok(None)).
func (d *DashboardState) applyDeploymentCapacity(targetID string, usage *core.DeploymentCapacityUsage, failure *string, sampledAt uint64) {
	panic("unported: mj-tui/src/test_support.rs DashboardState::apply_deployment_capacity")
}

func (d *DashboardState) setTestCapacityStorage(views []core.TargetStorageView) {
	panic("unported: mj-tui/src/test_support.rs DashboardState::set_test_capacity_storage")
}

// key is a press of code with no modifiers held. code is a character or one
// of Bubble Tea's key codes (tea.KeyEnter, tea.KeyUp, ...); characters carry
// their text as a terminal reports them.
func key(code rune) tea.KeyPressMsg {
	press := tea.KeyPressMsg{Code: code}
	if code < tea.KeyExtended && unicode.IsPrint(code) {
		press.Text = string(code)
	}
	return press
}

// bufferLines is the drawn buffer as one string per row.
func bufferLines(buffer *uv.Buffer) []string {
	lines := chat.BufferLines(buffer)
	pinNotificationAges(lines)
	return lines
}

func assertDialogSpacing(t testing.TB, lines []string, title, action string) {
	t.Helper()
	panic("unported: mj-tui/src/test_support.rs assert_dialog_spacing")
}

// drawn is the whole dashboard surface drawn into a terminal of the given
// size, as one string per row.
func drawn(dashboard *DashboardState, width, height int) []string {
	panic("unported: mj-tui/src/test_support.rs drawn")
}

// pinNotificationAges: notification ages use the wall clock in production.
// Goldens retain the header and its spacing while replacing just that
// changing value.
func pinNotificationAges(lines []string) {
	panic("unported: mj-tui/src/test_support.rs pin_notification_ages")
}

// pinnedSpinnerReplacer replaces the compact Scan spinner's frames.
var pinnedSpinnerReplacer = strings.NewReplacer("⠁", "?", "⠂", "?", "⠄", "?", "⡀", "?")

// pinSpinnerGlyphs replaces the compact Scan spinner's frames with `?`.
// Spinners take their frame from a process-wide clock (chat's spinner
// elapsed time), so a golden would otherwise record whichever frame was
// showing when it drew.
func pinSpinnerGlyphs(lines []string) {
	for index, line := range lines {
		lines[index] = pinnedSpinnerReplacer.Replace(line)
	}
}

// point is the cell position of the first drawn row containing label, as
// the column where the label starts and the row it is on.
func point(lines []string, label string) uv.Position {
	for row, line := range lines {
		if strings.Contains(line, label) {
			return uv.Pos(cellColumn(line, label), row)
		}
	}
	panic(fmt.Sprintf("missing %q: %#v", label, lines))
}

// mouseAt is a mouse event of kind at one cell position, with no modifiers
// held.
func mouseAt(kind tea.MouseMsg, at uv.Position) tea.MouseMsg {
	mouse := kind.Mouse()
	mouse.X, mouse.Y, mouse.Mod = at.X, at.Y, 0
	return mouseLike(kind, mouse)
}

// mouseLike is mouse as a message of kind's type.
func mouseLike(kind tea.MouseMsg, mouse tea.Mouse) tea.MouseMsg {
	switch kind.(type) {
	case tea.MouseClickMsg:
		return tea.MouseClickMsg(mouse)
	case tea.MouseReleaseMsg:
		return tea.MouseReleaseMsg(mouse)
	case tea.MouseWheelMsg:
		return tea.MouseWheelMsg(mouse)
	default:
		return tea.MouseMotionMsg(mouse)
	}
}

// cellColumn is the column of needle within a drawn row, counted in cells
// rather than bytes.
func cellColumn(line, needle string) int {
	index := strings.Index(line, needle)
	if index < 0 {
		panic(fmt.Sprintf("missing %s in %q", needle, line))
	}
	return utf8.RuneCountInString(line[:index])
}

// summaryTextCell reports whether a drawn summary cell holds session text
// rather than the rule fill.
func summaryTextCell(cell *uv.Cell) bool {
	return strings.TrimSpace(cell.Content) != "" && cell.Content != "─"
}

func ctrlKey(character rune) tea.KeyPressMsg {
	modifier := tea.ModCtrl
	if runtime.GOOS == "darwin" {
		modifier = tea.ModSuper
	}
	return tea.KeyPressMsg{Code: character, Mod: modifier}
}

// altKey is an Alt chord, for the composer's readline keys. Alt is the same
// modifier everywhere, so this must not go through ctrlKey, which reports
// Super on macOS.
func altKey(character rune) tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: character, Mod: tea.ModAlt}
}

// prefixKey is the default prefix key exactly as a terminal delivers it.
//
// Deliberately literal rather than ctrlKey: that helper reports Super on
// macOS, and the prefix is matched as Control on every platform.
func prefixKey() tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: 'b', Mod: tea.ModCtrl}
}

// route presses a sequence of keys through the prefix router and dispatches
// what it decides exactly as the mj-cli event loop does, so a test drives
// the same path the live terminal does.
func route(dashboard *DashboardState, keys []tea.KeyMsg) DashboardAction {
	panic("unported: mj-tui/src/test_support.rs route")
}

// chordKeys are the key presses that run a command through its first live
// binding.
func chordKeys(dashboard *DashboardState, id CommandId) []tea.KeyMsg {
	panic("unported: mj-tui/src/test_support.rs chord_keys")
}

// chord runs a command the way a person does: its first live binding,
// through the router, dispatched as mj-cli would.
func chord(dashboard *DashboardState, id CommandId) DashboardAction {
	return route(dashboard, chordKeys(dashboard, id))
}

func openNewSessionWizard(dashboard *DashboardState) DashboardAction {
	return chord(dashboard, CommandIdNewSessionWizard)
}

func openPalette(dashboard *DashboardState) DashboardAction {
	return chord(dashboard, CommandIdPalette)
}

func mouseIn(kind tea.MouseMsg, area uv.Rectangle) tea.MouseMsg {
	return mouseAt(kind, uv.Pos(area.Min.X+1, area.Min.Y+1))
}

// mouseAtRow is like mouseIn, but clicks a specific row within area instead
// of always the top-left cell, so tests can hit tail lines further down a
// multi-line hitbox.
func mouseAtRow(kind tea.MouseMsg, area uv.Rectangle, rowOffset int) tea.MouseMsg {
	return mouseAt(kind, uv.Pos(area.Min.X+1, area.Min.Y+rowOffset))
}

// config is the dashboard tests' configuration: three profiles, one
// project, and a local Podman target.
func config() core.Config {
	panic("unported: mj-tui/src/test_support.rs config")
}

func stoppedSession() core.SessionRecord {
	panic("unported: mj-tui/src/test_support.rs stopped_session")
}

// runningSession is the same fixture as stoppedSession, but live. Dashboard
// interactions such as rename and the container editor only apply to a
// session that is actually on the dashboard.
func runningSession() core.SessionRecord {
	session := stoppedSession()
	session.Oracle = true
	session.Project = nil
	session.State = core.SessionStateRunning
	return session
}

func legacyManagedSession(session core.SessionRecord) core.SessionRecord {
	panic("unported: mj-tui/src/test_support.rs legacy_managed_session")
}

// question is a form question the agent is waiting on, as the daemon
// projects it.
func question(sessionID string) core.ElicitationRequest {
	panic("unported: mj-tui/src/test_support.rs question")
}

// resumeCandidatesFor is what the daemon's ResumeCandidates answer holds for
// state: the inactive top-level records, and what every record says about
// native sessions and local checkouts.
func resumeCandidatesFor(config *core.Config, state *core.StateState) client.ResumeCandidates {
	panic("unported: mj-tui/src/test_support.rs resume_candidates_for")
}

// openResumeDialog opens the resume dialog and answers its request for
// stopped sessions from the dashboard's own records, as the daemon would.
func openResumeDialog(dashboard *DashboardState, discoveryID uint64, profiles []ImportProfileOption) {
	panic("unported: mj-tui/src/test_support.rs open_resume_dialog")
}

// focusResumeHelRows moves the open resume dialog to the Mjolnir tab with
// its list focused. The dialog opens on the running sessions with the list
// itself focused, and a stopped record is one tab to the right of them.
func focusResumeHelRows(dashboard *DashboardState) {
	panic("unported: mj-tui/src/test_support.rs focus_resume_hel_rows")
}

// openResumeWizard reaches the resume wizard the way the UI does: open the
// resume dialog, move to the Mjolnir tab, then activate the first row, which
// is the session's own stopped record.
func openResumeWizard(dashboard *DashboardState) DashboardAction {
	openResumeDialog(dashboard, 1, nil)
	focusResumeHelRows(dashboard)
	return dashboard.HandleKey(key(tea.KeyEnter))
}

func dashboardWithSession(session core.SessionRecord) *DashboardState {
	panic("unported: mj-tui/src/test_support.rs dashboard_with_session")
}

func publishFixtureVerdicts(dashboard *DashboardState) {
	panic("unported: mj-tui/src/test_support.rs publish_fixture_verdicts")
}

// setWorking marks a session's turn as running, so it reads as working.
func setWorking(dashboard *DashboardState, sessionID string) {
	panic("unported: mj-tui/src/test_support.rs set_working")
}

// serveAttention supplies the daemon publication independently of the raw
// transcript fixture.
func serveAttention(dashboard *DashboardState, ids []string, attention core.SessionAttention) {
	panic("unported: mj-tui/src/test_support.rs serve_attention")
}

// dashboardWithOneSubagent is a dashboard showing one running parent
// session with one running sub-agent. It returns the parent's id.
func dashboardWithOneSubagent() (*DashboardState, string) {
	panic("unported: mj-tui/src/test_support.rs dashboard_with_one_subagent")
}

// dashboardWithFinishedNativeChild is a dashboard with one running parent
// and one finished harness-native child, as R10 saw one: Codex's "Review
// calc", completed, availability unknown, with its transcript in the store.
// It returns the parent's id and the child's presentation id.
func dashboardWithFinishedNativeChild() (*DashboardState, string, string) {
	panic("unported: mj-tui/src/test_support.rs dashboard_with_finished_native_child")
}

// precisionStorage is precision-3260 as the daemon's storage owner reports
// it: `/` holds the worker roots and profile homes, `~/Projects` is a
// filesystem of its own.
func precisionStorage(rootFree, projectsFree uint64) core.TargetStorageView {
	panic("unported: mj-tui/src/test_support.rs precision_storage")
}

// precisionSession is a running session on precision-3260 whose managed
// clone is under `~/Projects`.
func precisionSession() core.SessionRecord {
	panic("unported: mj-tui/src/test_support.rs precision_session")
}

func testCapacityTarget() core.DeploymentCapacityTarget {
	panic("unported: mj-tui/src/test_support.rs test_capacity_target")
}

func transcriptItem(position uint64, body core.TranscriptBody) *core.TranscriptItem {
	panic("unported: mj-tui/src/test_support.rs transcript_item")
}

func agentMessage(position uint64, text string) *core.TranscriptItem {
	panic("unported: mj-tui/src/test_support.rs agent_message")
}

func thought(position uint64, text string) *core.TranscriptItem {
	panic("unported: mj-tui/src/test_support.rs thought")
}

func workInterruption(position uint64) *core.TranscriptItem {
	panic("unported: mj-tui/src/test_support.rs work_interruption")
}

func materializedSessionFor(sessionID string, transcript []*core.TranscriptItem) core.MaterializedSession {
	panic("unported: mj-tui/src/test_support.rs materialized_session_for")
}

func applyMaterializedTranscript(dashboard *DashboardState, transcript []*core.TranscriptItem) {
	applyMaterializedTranscriptFor(dashboard, "session-1", transcript)
}

func applyMaterializedTranscriptFor(dashboard *DashboardState, sessionID string, transcript []*core.TranscriptItem) {
	materialized := materializedSessionFor(sessionID, transcript)
	dashboard.ApplyMaterializedSession(&materialized)
}

// numberedConversation is a conversation of count numbered exchanges, so
// preview scroll assertions can name the message they expect to see. Agent
// chunks coalesce unless separated, so each pairs with its own prompt.
func numberedConversation(count uint64) []*core.TranscriptItem {
	panic("unported: mj-tui/src/test_support.rs numbered_conversation")
}

func operation(kind SessionOperationKind, stage *core.ProvisionStage) SessionOperationDisplay {
	panic("unported: mj-tui/src/test_support.rs operation")
}

// availabilityAnswer is the launch verdicts this rendering fixture receives
// from the feed.
func availabilityAnswer(dashboard *DashboardState, uninstalled, unreachable []string) client.RuntimeAvailability {
	panic("unported: mj-tui/src/test_support.rs availability_answer")
}

func publishUnreachableMachine(dashboard *DashboardState, answer *client.RuntimeAvailability, machine, detail, remediation string, routine bool) {
	panic("unported: mj-tui/src/test_support.rs publish_unreachable_machine")
}

func answerAvailability(dashboard *DashboardState, answer func(*DashboardState) client.RuntimeAvailability) {
	panic("unported: mj-tui/src/test_support.rs answer_availability")
}

// serveFormCard publishes authoritative form provenance, as the daemon
// runtime feed does.
func serveFormCard(dashboard *DashboardState, owner string, holder *string, request *core.ElicitationRequest) {
	panic("unported: mj-tui/src/test_support.rs serve_form_card")
}
