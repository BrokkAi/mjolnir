// Port of mj-tui/src/dialogs/tests.rs.

package tui

import (
	"errors"
	"fmt"
	"net/netip"
	"reflect"
	"slices"
	"strings"
	"testing"
	"time"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// dialogsRender is Rust's test-only `crate::render::render`: the combined
// surface with no conversation on it.
func dialogsRender(frame *chat.Frame, dashboard *DashboardState) {
	withNoColorOverrideForTest(true, func() struct{} {
		renderCombinedForTest(frame, dashboard, map[string]*chat.ActiveChat{}, map[PaneId]string{}, false)
		return struct{}{}
	})
}

// dialogsTerminal is a TestBackend terminal: one buffer the dashboard is
// drawn into, and the cursor the last draw left.
type dialogsTerminal struct {
	buffer *uv.Buffer
	cursor *uv.Position
}

func newDialogsTerminal(width, height int) *dialogsTerminal {
	return &dialogsTerminal{buffer: uv.NewBuffer(width, height)}
}

// draw renders the dashboard into a cleared buffer, as each ratatui draw
// does.
func (t *dialogsTerminal) draw(dashboard *DashboardState) {
	area := t.buffer.Bounds()
	t.buffer = uv.NewBuffer(area.Dx(), area.Dy())
	frame := chat.NewFrame(t.buffer)
	dialogsRender(frame, dashboard)
	t.cursor = nil
	if position, ok := frame.CursorPosition(); ok {
		t.cursor = &position
	}
}

// text is every cell's symbol in order, as the Rust tests join them.
func (t *dialogsTerminal) text() string {
	var text strings.Builder
	area := t.buffer.Bounds()
	for y := area.Min.Y; y < area.Max.Y; y++ {
		for x := area.Min.X; x < area.Max.X; x++ {
			cell := t.buffer.CellAt(x, y)
			if cell == nil || cell.Content == "" {
				text.WriteByte(' ')
				continue
			}
			text.WriteString(cell.Content)
		}
	}
	return text.String()
}

// dialogsSnapshotMode is Rust's Mode::clone for the modes these tests compare
// against later.
func dialogsSnapshotMode(t *testing.T, mode Mode) Mode {
	t.Helper()
	switch mode := mode.(type) {
	case ModeDashboard:
		return mode
	case ModeConfirm:
		clone := mode.Clone()
		return ModeConfirm{&clone}
	case ModeWeb:
		clone := mode.Clone()
		return ModeWeb{&clone}
	}
	t.Fatalf("no snapshot for mode %T", mode)
	return nil
}

// dialogsModesEqual is Rust's derived Mode PartialEq for the modes these
// tests compare: a dialog's form compares by chat.Dialog.Equal, as Rust's
// Form has its own PartialEq, not field by field.
func dialogsModesEqual(a, b Mode) bool {
	switch a := a.(type) {
	case ModeDashboard:
		_, ok := b.(ModeDashboard)
		return ok
	case ModeConfirm:
		other, ok := b.(ModeConfirm)
		if !ok {
			return false
		}
		left, right := a.ConfirmDialog, other.ConfirmDialog
		namesEqual := (left.sessionName == nil) == (right.sessionName == nil) &&
			(left.sessionName == nil || *left.sessionName == *right.sessionName)
		return left.scroll == right.scroll &&
			left.maxScroll == right.maxScroll &&
			reflect.DeepEqual(left.confirmation, right.confirmation) &&
			namesEqual &&
			left.form.Equal(&right.form)
	}
	return reflect.DeepEqual(a, b)
}

// dialogsDebugAction is Rust's `{:?}` of the actions the dialog goldens
// record.
func dialogsDebugAction(action DashboardAction) string {
	switch action := action.(type) {
	case DashboardActionNone:
		return "None"
	case DashboardActionCheckGitIdentity:
		return "CheckGitIdentity"
	case DashboardActionTestTarget:
		return fmt.Sprintf("TestTarget { target_id: %s }", core.RustDebugString(action.TargetID))
	case DashboardActionRenameTarget:
		return fmt.Sprintf("RenameTarget { old_id: %s, new_id: %s }",
			core.RustDebugString(action.OldID), core.RustDebugString(action.NewID))
	case DashboardActionProbeGitStatus:
		return fmt.Sprintf("ProbeGitStatus { session_id: %s }", core.RustDebugString(action.SessionID))
	case DashboardActionSaveGitIdentity:
		return fmt.Sprintf("SaveGitIdentity { name: %s, email: %s }",
			core.RustDebugString(action.Name), core.RustDebugString(action.Email))
	case DashboardActionSuspend:
		return fmt.Sprintf("Suspend { session_id: %s, consent: %s }",
			core.RustDebugString(action.SessionID), dialogsDebugConsent(action.Consent))
	}
	return fmt.Sprintf("%#v", action)
}

func dialogsDebugConsent(consent core.SuspendConsent) string {
	kind := func(kind core.ConsentKind, shown any) string {
		switch kind {
		case core.ConsentNone:
			return "None"
		case core.ConsentAny:
			return "Some(Any)"
		default:
			return fmt.Sprintf("Some(Shown(%#v))", shown)
		}
	}
	return fmt.Sprintf(
		"SuspendConsent { discard_untracked: %s, abort_operations: %s, without_checkpoint: %t, confirm_no_checkpoint: %t }",
		kind(consent.DiscardUntracked.Kind, consent.DiscardUntracked.Shown),
		kind(consent.AbortOperations.Kind, consent.AbortOperations.Shown),
		consent.WithoutCheckpoint,
		consent.ConfirmNoCheckpoint,
	)
}

// dialogsDebugOptionalAction is Rust's `{:?}` of an Option<DashboardAction>.
func dialogsDebugOptionalAction(action DashboardAction) string {
	if action == nil {
		return "None"
	}
	return "Some(" + dialogsDebugAction(action) + ")"
}

// dialogsDebugNotice is Rust's `{:?}` of the current notice.
func dialogsDebugNotice(dashboard *DashboardState) string {
	notice, ok := dashboard.notices.Current()
	if !ok {
		return "None"
	}
	return "Some(" + core.RustDebugString(notice) + ")"
}

var (
	dialogsLeftDown = tea.MouseClickMsg{Button: tea.MouseLeft}
	dialogsLeftUp   = tea.MouseReleaseMsg{Button: tea.MouseLeft}
)

func dialogsControlKey(character rune) tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: character, Mod: tea.ModCtrl}
}

func dialogsTestRepair() core.LocalRemoteRepair {
	return core.LocalRemoteRepair{
		Path:              "/project",
		Branch:            "main",
		MissingRemote:     "upstream",
		ReplacementRemote: "origin",
		FetchURL:          "https://example.com/repo.git",
		PushURLs:          []string{"ssh://git@example.com/repo.git"},
	}
}

func TestRemoteRepairRequiresConfirmationAndRestoresThePreviousScreen(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.ShowCloseFailure("session-1", "prior dialog")
	previous := dialogsSnapshotMode(t, dashboard.mode)
	repair := dialogsTestRepair()
	worktree := false
	retry := DashboardActionCreateSession{
		WorkspaceID:           core.DefaultWorkspaceID,
		ProfileID:             "codex",
		TargetTemplateID:      "docker",
		BundleID:              "project",
		CreateManagedWorktree: &worktree,
	}
	dashboard.ShowRemoteRepairConfirmation("repo", []core.LocalRemoteRepair{repair}, retry)
	terminal := newDialogsTerminal(100, 25)
	terminal.draw(dashboard)
	text := terminal.text()
	if !strings.Contains(text, "Repair Git tracking?") {
		t.Fatal(text)
	}
	if !strings.Contains(text, "ssh://git@example.com/repo.git") {
		t.Fatal(text)
	}
	if action := dashboard.HandleKey(key(tea.KeyEscape)); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("Esc: %#v", action)
	}
	if !dialogsModesEqual(dashboard.mode, previous) {
		t.Fatalf("mode %#v, want %#v", dashboard.mode, previous)
	}
	dashboard.ShowRemoteRepairConfirmation("repo", []core.LocalRemoteRepair{repair}, retry)
	dashboard.HandleKey(key(tea.KeyRight))
	want := DashboardActionRepairRepositoryRemotes{
		BundleID: "repo",
		Repairs:  []core.LocalRemoteRepair{repair},
		Retry:    retry,
	}
	if action := dashboard.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, want) {
		t.Fatalf("Enter: %#v", action)
	}
	if !dialogsModesEqual(dashboard.mode, previous) {
		t.Fatalf("mode %#v, want %#v", dashboard.mode, previous)
	}
}

// After a failed launch, the footer still said "Launching demo via codex…"
// 40 seconds later, under the Launch failed dialog (launch finding R13-8).
// A success replaces that notice with "Session … is ready"; a failure now
// replaces it too.
// Hard-won: 968e8ad2: After failure, the footer no longer says Launching and instead shows the terminal failure notice.
func TestLaunchFailureReplacesTheLaunchingNotice(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.SetNotice("Launching demo via codex…")

	dashboard.ShowLaunchFailure("Codex is not installed on local host", nil)

	if notice, ok := dashboard.Notice(); !ok || notice != "The session could not start." {
		t.Fatalf("notice %q, %t", notice, ok)
	}
	if _, ok := dashboard.mode.(ModeConfirm); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
}

func TestLaunchFailureScrollsLongDetailsAndRestoresInterruptedDialog(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.ShowCloseFailure("session-1", "prior error")
	previous := dialogsSnapshotMode(t, dashboard.mode)
	var details strings.Builder
	for index := range 60 {
		fmt.Fprintf(&details, "diagnostic line %d\n", index)
	}
	dashboard.ShowLaunchFailure(details.String(), nil)
	terminal := newDialogsTerminal(90, 20)
	terminal.draw(dashboard)
	for range 30 {
		dashboard.HandleKey(key(tea.KeyPgDown))
	}
	terminal.draw(dashboard)
	text := terminal.text()
	if !strings.Contains(text, "diagnostic line 59") {
		t.Fatal(text)
	}
	if strings.Contains(text, "Retry launch") {
		t.Fatal(text)
	}
	if action := dashboard.HandleKey(key(tea.KeyEscape)); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("Esc: %#v", action)
	}
	if !dialogsModesEqual(dashboard.mode, previous) {
		t.Fatalf("mode %#v, want %#v", dashboard.mode, previous)
	}
}

// A failed import used to report only on the footer, which cut the reason
// off and cleared on the next key press, so pressing Okay seemed to do
// nothing. The reason now stays in a dialog until dismissed.
func TestImportFailureShowsTheReasonUntilDismissed(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	previous := dialogsSnapshotMode(t, dashboard.mode)
	dashboard.ShowImportFailure("the project's Git repository has no remote")
	if notice, ok := dashboard.Notice(); !ok || notice != "The session import failed." {
		t.Fatalf("notice %q, %t", notice, ok)
	}
	terminal := newDialogsTerminal(90, 20)
	terminal.draw(dashboard)
	text := terminal.text()
	if !strings.Contains(text, "Import failed") {
		t.Fatal(text)
	}
	if !strings.Contains(text, "has no remote") {
		t.Fatal(text)
	}
	if action := dashboard.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("Enter: %#v", action)
	}
	if !dialogsModesEqual(dashboard.mode, previous) {
		t.Fatalf("mode %#v, want %#v", dashboard.mode, previous)
	}
}

func drawWebDialog(dialog *WebDialog, width, height int) []string {
	buffer := uv.NewBuffer(width, height)
	frame := chat.NewFrame(buffer)
	surfaces := chat.NewFrameSurfaces()
	renderWebDialog(frame, frame.Area(), dialog, &surfaces)
	return bufferLines(buffer)
}

func failedWebDashboard() *DashboardState {
	dashboard := dashboardWithSession(runningSession())
	dashboard.OpenWebDialog()
	dashboard.ApplyWebAccess(client.WebViewerAccessFailed{
		Address:      netip.MustParseAddrPort("127.0.0.1:37650"),
		Message:      "Port 37650 is already in use.",
		PortConflict: true,
	})
	return dashboard
}

func dialogsWebDialog(t *testing.T, dashboard *DashboardState) *WebDialog {
	t.Helper()
	mode, ok := dashboard.mode.(ModeWeb)
	if !ok {
		t.Fatalf("web dialog expected, got %T", dashboard.mode)
	}
	return mode.WebDialog
}

func activateWeb(t *testing.T, dashboard *DashboardState, control DialogControl) DashboardAction {
	t.Helper()
	dialog := dialogsWebDialog(t, dashboard).Clone()
	drawWebDialog(&dialog, 80, 24)
	dialog.form.Focus(control)
	return dashboard.handleWebEvent(key(tea.KeyEnter), dialog)
}

func TestWebRecoveryEntersLoadingImmediatelyAndDismissCancelsStatusPolling(t *testing.T) {
	dashboard := failedWebDashboard()
	if action := activateWeb(t, dashboard, DialogControlWebAnotherPort); !reflect.DeepEqual(action,
		DashboardActionRecoverWebViewer{Recovery: client.WebViewerRecoveryAnotherPort{}}) {
		t.Fatalf("Use another port: %#v", action)
	}
	dialog := dialogsWebDialog(t, dashboard)
	if !dialog.loading {
		t.Fatal("recovery must enter loading")
	}
	rendered := strings.Join(drawWebDialog(dialog, 80, 24), "\n")
	if !strings.Contains(rendered, "Starting web viewer") {
		t.Fatal(rendered)
	}
	if !strings.Contains(rendered, "× Web viewer") {
		t.Fatal(rendered)
	}
	if strings.Contains(rendered, "  Use another port  ") {
		t.Fatal(rendered)
	}
	if action := dashboard.HandleKey(key(tea.KeyEscape)); !reflect.DeepEqual(action, DashboardActionCancelWebAccess{}) {
		t.Fatalf("Esc: %#v", action)
	}
	dashboard.ApplyWebAccess(client.WebViewerAccessStarting{})
	if _, ok := dashboard.mode.(ModeWeb); ok {
		t.Fatal("late results must not reopen the dialog")
	}
}

func TestWebRetryAndInspectionDispatchRealActionsAndInspectionCanFailInPlace(t *testing.T) {
	dashboard := failedWebDashboard()
	if action := activateWeb(t, dashboard, DialogControlWebRetry); !reflect.DeepEqual(action,
		DashboardActionRecoverWebViewer{Recovery: client.WebViewerRecoveryRetry{}}) {
		t.Fatalf("Retry: %#v", action)
	}
	dashboard = failedWebDashboard()
	if action := activateWeb(t, dashboard, DialogControlWebInspect); !reflect.DeepEqual(action, DashboardActionInspectWebListener{}) {
		t.Fatalf("Inspect: %#v", action)
	}
	if !dialogsWebDialog(t, dashboard).inspecting {
		t.Fatal("inspection must start")
	}
	dashboard.ApplyWebListeners(nil, errors.New("Permission denied while inspecting listeners."))
	dialog := dialogsWebDialog(t, dashboard)
	if dialog.inspecting {
		t.Fatal("a failed inspection must end")
	}
	rendered := strings.Join(drawWebDialog(dialog, 80, 24), "\n")
	if !strings.Contains(rendered, "Permission denied") {
		t.Fatal(rendered)
	}
	if !strings.Contains(rendered, "  Use another port  ") {
		t.Fatal(rendered)
	}
}

func TestStoppingAWebListenerRequiresASeparateConfirmationWithCancelSelected(t *testing.T) {
	dashboard := failedWebDashboard()
	process := client.WebListenerProcess{
		Pid:        4242,
		Name:       "mj",
		Executable: "/opt/mj/bin/mj",
		StartedAt:  123,
	}
	dashboard.ApplyWebListeners([]client.WebListenerProcess{process}, nil)
	if action := activateWeb(t, dashboard, DialogControlWebStop); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("Stop: %#v", action)
	}
	dialog := dialogsWebDialog(t, dashboard)
	if focused, ok := dialog.form.Focused(); !ok || focused != DialogControlWebCancelStop {
		t.Fatalf("focused %v, %t", focused, ok)
	}
	rendered := strings.Join(drawWebDialog(dialog, 80, 24), "\n")
	for _, wanted := range []string{"PID 4242", "/opt/mj/bin/mj", "  Stop and retry  "} {
		if !strings.Contains(rendered, wanted) {
			t.Fatalf("missing %q in\n%s", wanted, rendered)
		}
	}
	if action := activateWeb(t, dashboard, DialogControlWebCancelStop); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("Cancel stop: %#v", action)
	}
	if dialogsWebDialog(t, dashboard).confirmStop != nil {
		t.Fatal("cancelling must drop the stop confirmation")
	}
	activateWeb(t, dashboard, DialogControlWebStop)
	if action := activateWeb(t, dashboard, DialogControlWebConfirmStop); !reflect.DeepEqual(action,
		DashboardActionRecoverWebViewer{Recovery: client.WebViewerRecoveryStopAndRetry{Process: process}}) {
		t.Fatalf("Confirm stop: %#v", action)
	}
}

func TestAnUnrelatedListenerHasNoEnabledStopAction(t *testing.T) {
	dashboard := failedWebDashboard()
	reason := "This is not an identified Mjolnir server."
	dashboard.ApplyWebListeners([]client.WebListenerProcess{{
		Pid:                4242,
		Name:               "other-server",
		Executable:         "/opt/other-server",
		StartedAt:          123,
		StopDisabledReason: &reason,
	}}, nil)
	rendered := drawWebDialog(dialogsWebDialog(t, dashboard), 80, 24)
	row := slices.IndexFunc(rendered, func(line string) bool { return strings.Contains(line, "  Stop server…  ") })
	if row < 0 {
		t.Fatalf("no stop button:\n%s", strings.Join(rendered, "\n"))
	}
	// Rust's str::find is a byte offset.
	column := strings.Index(rendered[row], "  Stop server…  ")
	for _, kind := range []tea.MouseMsg{dialogsLeftDown, dialogsLeftUp} {
		dialog := dialogsWebDialog(t, dashboard).Clone()
		if action := dashboard.handleWebEvent(mouseAt(kind, uv.Pos(column+2, row)), dialog); !reflect.DeepEqual(action, DashboardActionNone{}) {
			t.Fatalf("click: %#v", action)
		}
	}
	if dialogsWebDialog(t, dashboard).confirmStop != nil {
		t.Fatal("a disabled stop must not ask for confirmation")
	}
}

// Launch finding R3-11 (with J-24): the container editor named the session
// by its 32-hex id where the row and every other dialog use its title, and
// drew its access choice with two dropdown glyphs ("ro · read-only ▾ ▾").
// Hard-won: b313aacd: The header uses the session title instead of its id and the mount access row contains one dropdown glyph.
func TestTheContainerEditorNamesTheSessionAndDrawsOneDropdownGlyph(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	openContainerEditor(t, dashboard)
	shown := strings.Join(drawn(dashboard, 120, 40), "\n")
	if !strings.Contains(shown, "Session: ACP pretty name") {
		t.Fatal(shown)
	}
	if strings.Contains(shown, "Session: session-1") {
		t.Fatal(shown)
	}
	var access string
	for _, line := range strings.Split(shown, "\n") {
		if strings.Contains(line, "Access:") {
			access = line
			break
		}
	}
	if access == "" {
		t.Fatalf("no access row:\n%s", shown)
	}
	if count := strings.Count(access, chat.ComboBoxGlyph); count != 1 {
		t.Fatalf("%d dropdown glyphs: %s", count, access)
	}
}

func dashboardWithContainerSession() *DashboardState {
	session := runningSession()
	session.AdditionalMounts = []core.AdditionalMount{{
		Source:      "/srv/data",
		Destination: "/mnt/data",
		Access:      core.MountAccessCow,
	}}
	dashboard := dashboardWithSession(session)
	dashboard.state.Mut().MountHistory.Insert("local", []string{"/srv/models"})
	return dashboard
}

func containerEditor(t *testing.T, dashboard *DashboardState) *ContainerEditor {
	t.Helper()
	mode, ok := dashboard.mode.(ModeEditContainer)
	if !ok {
		t.Fatalf("expected the container editor, got %T", dashboard.mode)
	}
	return mode.ContainerEditor
}

// throughThePalette reaches a session command the way the user does now: the
// palette chord, type enough of the name to pick it out, Enter. The session
// edit dialog these fixtures used to press `e` for no longer exists.
func throughThePalette(t *testing.T, dashboard *DashboardState, query string) {
	t.Helper()
	openPalette(dashboard)
	if _, ok := dashboard.mode.(ModePalette); !ok {
		t.Fatalf("the palette chord opens the palette, got %T", dashboard.mode)
	}
	for _, character := range query {
		dashboard.HandleKey(key(character))
	}
	dashboard.HandleKey(key(tea.KeyEnter))
}

func openContainerEditor(t *testing.T, dashboard *DashboardState) {
	t.Helper()
	throughThePalette(t, dashboard, "container")
	if _, ok := dashboard.mode.(ModeEditContainer); !ok {
		t.Fatalf("expected the container editor, got %T", dashboard.mode)
	}
}

func TestContainerEditorEditsAListedMountInPlace(t *testing.T) {
	dashboard := dashboardWithContainerSession()
	openContainerEditor(t, dashboard)

	// A new directory starts read-only; the combobox moves it to
	// copy-on-write.
	for containerEditor(t, dashboard).focused() != ContainerEditFocusSource {
		dashboard.HandleKey(key(tea.KeyTab))
	}
	for _, character := range "/nfs/share" {
		dashboard.HandleKey(key(character))
	}
	dashboard.HandleKey(key(tea.KeyTab))
	dashboard.HandleKey(key(tea.KeyTab))
	if focused := containerEditor(t, dashboard).focused(); focused != ContainerEditFocusAccess {
		t.Fatalf("focused %v", focused)
	}
	if access := containerEditor(t, dashboard).access; access != core.MountAccessRo {
		t.Fatalf("access %v", access)
	}
	dashboard.HandleKey(key(tea.KeyEnter))
	if !containerEditor(t, dashboard).accessCombo.IsOpen(ContainerEditFocusAccess) {
		t.Fatal("Enter opens the access combobox")
	}
	dashboard.HandleKey(key(tea.KeyDown))
	dashboard.HandleKey(key(tea.KeyEnter))
	if access := containerEditor(t, dashboard).access; access != core.MountAccessCow {
		t.Fatalf("access %v", access)
	}
	for containerEditor(t, dashboard).focused() != ContainerEditFocusSource {
		dashboard.HandleKey(key(tea.KeyTab))
	}
	dashboard.HandleKey(key(tea.KeyEnter))
	if mounts := len(containerEditor(t, dashboard).mounts); mounts != 2 {
		t.Fatalf("%d mounts", mounts)
	}

	// Space on a listed row loads that attachment into the editor fields,
	// where the combobox is the only way to change its access mode.
	for containerEditor(t, dashboard).focused() != ContainerEditFocusMounts {
		dashboard.HandleKey(key(tea.KeyTab))
	}
	dashboard.HandleKey(key(tea.KeyUp))
	if index := containerEditor(t, dashboard).mountIndex; index != 0 {
		t.Fatalf("mount index %d", index)
	}
	dashboard.HandleKey(key(' '))
	editor := containerEditor(t, dashboard)
	if editor.focused() != ContainerEditFocusSource {
		t.Fatalf("focused %v", editor.focused())
	}
	if editor.source.Value() != "/srv/data" || editor.destination.Value() != "/mnt/data" {
		t.Fatalf("source %q, destination %q", editor.source.Value(), editor.destination.Value())
	}
	if editor.access != core.MountAccessCow {
		t.Fatalf("access %v", editor.access)
	}
	if editor.editingMount == nil || *editor.editingMount != 0 {
		t.Fatalf("editing mount %v", editor.editingMount)
	}

	for containerEditor(t, dashboard).focused() != ContainerEditFocusAccess {
		dashboard.HandleKey(key(tea.KeyTab))
	}
	dashboard.HandleKey(key(tea.KeyEnter))
	dashboard.HandleKey(key(tea.KeyDown))
	dashboard.HandleKey(key(tea.KeyEnter))
	if access := containerEditor(t, dashboard).access; access != core.MountAccessRw {
		t.Fatalf("access %v", access)
	}

	// Accepting the edited entry replaces the row it came from instead of
	// attaching the same directory twice.
	for containerEditor(t, dashboard).focused() != ContainerEditFocusSource {
		dashboard.HandleKey(key(tea.KeyTab))
	}
	dashboard.HandleKey(key(tea.KeyEnter))
	if editing := containerEditor(t, dashboard).editingMount; editing != nil {
		t.Fatalf("editing mount %d", *editing)
	}
	if source := containerEditor(t, dashboard).source.Value(); strings.TrimSpace(source) != "" {
		t.Fatalf("source %q", source)
	}

	for containerEditor(t, dashboard).focused() != ContainerEditFocusSave {
		dashboard.HandleKey(key(tea.KeyTab))
	}
	want := DashboardActionSaveContainerSettings{
		SessionID: "session-1",
		AdditionalMounts: []core.AdditionalMount{
			{Source: "/srv/data", Destination: "/mnt/data", Access: core.MountAccessRw},
			{Source: "/nfs/share", Destination: "/mnt/share", Access: core.MountAccessCow},
		},
		MountHistory: []string{"/srv/models"},
	}
	if action := dashboard.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, want) {
		t.Fatalf("Save: %#v", action)
	}
}

func TestFocusedTextFieldsOwnReadlineKeysAndControlCCancels(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.beginRename()
	for _, character := range " alpha beta" {
		dashboard.HandleKey(key(character))
	}

	if action := dashboard.HandleKey(dialogsControlKey('w')); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("Ctrl-W: %#v", action)
	}
	mode, ok := dashboard.mode.(ModeRename)
	if !ok {
		t.Fatalf("expected rename editor, got %T", dashboard.mode)
	}
	if title := mode.title.Value(); !strings.HasSuffix(title, "alpha ") {
		t.Fatalf("title %q", title)
	}

	if action := dashboard.HandleKey(dialogsControlKey('c')); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("Ctrl-C: %#v", action)
	}
	if !dashboard.dialogConfirmationOpen() {
		t.Fatal("Ctrl-C asks before discarding the draft")
	}
	dashboard.HandleKey(key(tea.KeyEnter))
	if dashboard.dialogConfirmationOpen() {
		t.Fatal("Enter keeps editing")
	}
	if focus := renameFocus(t, dashboard); focus != DialogControlField {
		t.Fatalf("focus %v", focus)
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	dashboard.HandleKey(key(tea.KeyRight))
	dashboard.HandleKey(key(tea.KeyEnter))
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
}

func renameFocus(t *testing.T, dashboard *DashboardState) DialogControl {
	t.Helper()
	mode, ok := dashboard.mode.(ModeRename)
	if !ok {
		t.Fatalf("expected rename editor, got %T", dashboard.mode)
	}
	focused, ok := mode.form.Focused()
	if !ok {
		t.Fatal("unexpected rename focus: None")
	}
	switch focused {
	case DialogControlField, DialogControlCancel, DialogControlSave:
		return focused
	}
	t.Fatalf("unexpected rename focus: %v", focused)
	return 0
}

func dialogsImportBundleFocused(dashboard *DashboardState, control DialogControl) bool {
	mode, ok := dashboard.mode.(ModeConfirmImportBundle)
	if !ok {
		return false
	}
	focused, ok := mode.form.Focused()
	return ok && focused == control
}

func TestImportSafetyKeepsManagedCheckoutChoiceWithoutFileInclusionToggle(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.ShowImportBundleConfirmation(
		[]string{"/work/repo — 1 tracked change · 222561 untracked paths"},
		nil,
		nil,
		core.ManagedWorktreeOptions{Available: true, DefaultCreate: false},
		"/work/repo",
	)
	terminal := newDialogsTerminal(120, 30)
	terminal.draw(dashboard)
	rendered := terminal.text()
	for _, wanted := range []string{
		"☐ Create isolated checkout",
		"Checked creates a clean, separate checkout.",
		"These checkouts have uncommitted changes:",
		" Cancel ",
		" Continue ",
	} {
		if !strings.Contains(rendered, wanted) {
			t.Fatalf("missing %q", wanted)
		}
	}
	create := false
	if action := dashboard.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, DashboardActionConfirmImportBundle{
		ProjectRoot:           "/work/repo",
		CreateManagedWorktree: &create,
		Accepted:              true,
	}) {
		t.Fatalf("Enter: %#v", action)
	}

	dashboard.ShowImportBundleConfirmation(
		nil,
		nil,
		nil,
		core.ManagedWorktreeOptions{Available: true, DefaultCreate: false},
		"/work/repo",
	)
	terminal.draw(dashboard)
	for range 3 {
		if dialogsImportBundleFocused(dashboard, DialogControlImportManagedWorktree) {
			break
		}
		dashboard.HandleKey(key(tea.KeyTab))
	}
	if !dialogsImportBundleFocused(dashboard, DialogControlImportManagedWorktree) {
		t.Fatal("Tab reaches the managed checkout choice")
	}
	dashboard.HandleKey(key(' '))
	for range 3 {
		if dialogsImportBundleFocused(dashboard, DialogControlImportContinue) {
			break
		}
		dashboard.HandleKey(key(tea.KeyTab))
	}
	if !dialogsImportBundleFocused(dashboard, DialogControlImportContinue) {
		t.Fatal("Tab reaches Continue")
	}
	create = true
	if action := dashboard.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, DashboardActionConfirmImportBundle{
		ProjectRoot:           "/work/repo",
		CreateManagedWorktree: &create,
		Accepted:              true,
	}) {
		t.Fatalf("Enter: %#v", action)
	}
}

func TestImportingSessionRendersUnknownThenKnownProgressAndIgnoresNavigation(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.ShowImportProgress("Chosen session")
	if action := dashboard.HandleKey(key(tea.KeyDown)); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("Down: %#v", action)
	}
	terminal := newDialogsTerminal(100, 24)
	terminal.draw(dashboard)
	if rendered := terminal.text(); !strings.Contains(rendered, "Importing session · progress 1/?") {
		t.Fatal(rendered)
	}

	total := 4
	dashboard.UpdateImportProgress(2, &total, "Native session parsed.")
	terminal.draw(dashboard)
	rendered := terminal.text()
	if !strings.Contains(rendered, "Importing session · progress 2/4") {
		t.Fatal(rendered)
	}
	if !strings.Contains(rendered, "Native session parsed.") {
		t.Fatal(rendered)
	}

	mode, ok := dashboard.mode.(ModeImporting)
	if !ok {
		t.Fatalf("expected import progress, got %T", dashboard.mode)
	}
	mode.lastUpdated = time.Now().Add(-importStallWarningAfter)
	terminal.draw(dashboard)
	if rendered := terminal.text(); !strings.Contains(rendered, "filesystem may be stalled") {
		t.Fatal(rendered)
	}
	if action := dashboard.HandleKey(key(tea.KeyEscape)); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("Esc: %#v", action)
	}
	if _, ok := dashboard.mode.(ModeConfirm); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
	if action := dashboard.HandleKey(key(tea.KeyRight)); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("Right: %#v", action)
	}
	if action := dashboard.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, DashboardActionCancelImport{}) {
		t.Fatalf("Enter: %#v", action)
	}
}

func TestDeletingASessionKeepsItsBranchUnlessAsked(t *testing.T) {
	dashboard := dashboardWithSession(legacyManagedSession(runningSession()))
	dashboard.FocusSessions()
	dashboard.DispatchCommand(CommandIdDestroySession)
	mode, ok := dashboard.mode.(ModeConfirm)
	if !ok {
		t.Fatalf("delete confirmation, got %T", dashboard.mode)
	}
	if buttons := confirmationButtons(mode.confirmation); !slices.Equal(buttons, []string{"Cancel", "Destroy session"}) {
		t.Fatalf("buttons %q", buttons)
	}
	if action := dashboard.HandleKey(key('c')); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("c: %#v", action)
	}
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
	dashboard.DispatchCommand(CommandIdDestroySession)
	if action := dashboard.HandleKey(key('d')); !reflect.DeepEqual(action, DashboardActionForceDestroy{SessionID: "session-1"}) {
		t.Fatalf("d: %#v", action)
	}
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
}

func TestFailedSuspensionOffersRetryAndSuspendWithoutCheckpoint(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)
	dashboard.ShowCloseFailure("session-1", "archive unavailable")
	mode, ok := dashboard.mode.(ModeConfirm)
	if !ok {
		t.Fatalf("confirmation, got %T", dashboard.mode)
	}
	if buttons := confirmationButtons(mode.confirmation); !slices.Equal(buttons,
		[]string{"Cancel", "Retry suspension", "Suspend without checkpoint…"}) {
		t.Fatalf("buttons %q", buttons)
	}
	dashboard.HandleKey(key(tea.KeyRight))
	if action := dashboard.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, DashboardActionSuspend{
		SessionID: "session-1",
		Consent:   core.SuspendConsent{},
	}) {
		t.Fatalf("Retry: %#v", action)
	}
	dashboard.ShowCloseFailure("session-1", "archive unavailable")
	dashboard.HandleKey(key(tea.KeyRight))
	dashboard.HandleKey(key(tea.KeyRight))
	if action := dashboard.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, DashboardActionSuspend{
		SessionID: "session-1",
		Consent: core.SuspendConsent{
			WithoutCheckpoint:   true,
			ConfirmNoCheckpoint: true,
		},
	}) {
		t.Fatalf("Suspend without checkpoint: %#v", action)
	}
}

func TestButtonConfirmationsKeepTheirButtonRowVisible(t *testing.T) {
	confirmations := []Confirmation{
		ConfirmationDestroyStopped{SessionID: "session-1"},
		ConfirmationCloseFailed{SessionID: "session-1", Error: "archive unavailable"},
		ConfirmationForceDestroy{SessionID: "session-1"},
	}
	for _, confirmation := range confirmations {
		for _, size := range [][2]int{{120, 30}, {100, 24}, {80, 22}} {
			dashboard := dashboardWithSession(stoppedSession())
			dialog := newConfirmDialog(confirmation)
			dashboard.mode = ModeConfirm{&dialog}
			terminal := newDialogsTerminal(size[0], size[1])
			terminal.draw(dashboard)
			rendered := strings.Join(bufferLines(terminal.buffer), "\n")
			for _, label := range confirmationButtons(confirmation) {
				if !strings.Contains(rendered, " "+label+" ") {
					t.Fatalf("%#v at %dx%d hides %s", confirmation, size[0], size[1], label)
				}
			}
		}
	}
}

func TestDestroyStoppedConfirmationDestroysFromItsPrimaryButton(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	openResumeDialog(dashboard, 1, nil)
	focusResumeHelRows(dashboard)
	dashboard.HandleKey(key(tea.KeyDelete))
	mode, ok := dashboard.mode.(ModeConfirm)
	if !ok {
		t.Fatalf("expected destroy confirmation, got %T", dashboard.mode)
	}
	if buttons := confirmationButtons(mode.confirmation); !slices.Equal(buttons, []string{"Cancel", "Destroy session"}) {
		t.Fatalf("buttons %q", buttons)
	}
	if focused, ok := mode.form.Focused(); !ok || focused != DialogControlConfirmButton(0) {
		t.Fatalf("focused %v, %t", focused, ok)
	}
	dashboard.HandleKey(key(tea.KeyRight))
	if action := dashboard.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, DashboardActionDestroyStopped{SessionID: "session-1"}) {
		t.Fatalf("Destroy: %#v", action)
	}
	// Destroying from the dialog leaves the user in the dialog.
	if _, ok := dashboard.mode.(ModeResumeDialog); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
}

func appendDialogRender(output *strings.Builder, label string, width, height int, lines []string) {
	if output.Len() > 0 {
		output.WriteByte('\n')
	}
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, width, height)
	output.WriteString(strings.Join(lines, "\n"))
	output.WriteByte('\n')
}

func clickDialogLabel(dashboard *DashboardState, lines []string, label string) DashboardAction {
	position := point(lines, label)
	var action DashboardAction = DashboardActionNone{}
	for _, kind := range []tea.MouseMsg{dialogsLeftDown, dialogsLeftUp} {
		action = dashboard.HandleMouse(mouseAt(kind, position))
	}
	return action
}

// normalizeNoticeLogAges replaces the renderer's relative age with a
// same-width marker so the golden pins the complete modal without pinning
// wall time.
func normalizeNoticeLogAges(lines []string) {
	for index, line := range lines {
		ago := strings.Index(line, " ago ")
		if ago < 0 {
			continue
		}
		tokenStart := strings.LastIndex(line[:ago], " ") + 1
		lines[index] = line[:tokenStart] + strings.Repeat("?", ago-tokenStart) + line[ago:]
	}
}

func TestGoldenDashboardDialogs(t *testing.T) {
	var output strings.Builder

	{
		dashboard := dashboardWithSession(runningSession())
		dashboard.ShowCloseFailure("session-1", "archive unavailable")
		lines := drawn(dashboard, 100, 28)
		appendDialogRender(&output, "close failure", 100, 28, lines)
		action := dashboard.HandleKey(key('r'))
		if !reflect.DeepEqual(action, DashboardActionSuspend{SessionID: "session-1", Consent: core.SuspendConsent{}}) {
			t.Fatalf("r: %#v", action)
		}
		fmt.Fprintf(&output, "action: %s\n", dialogsDebugAction(action))
	}

	// Suspend dialog with untracked files, in-progress operation, and raw
	// checkout.
	untracked := make([]core.UntrackedPath, 0, 7)
	for index := 1; index <= 7; index++ {
		untracked = append(untracked, core.UntrackedPath{
			Checkout: core.DefaultCheckoutKey(),
			Path:     fmt.Sprintf("scratch-%d.txt", index),
			Bytes:    128,
			Identity: core.NewFileIdentity(fmt.Sprintf("id-%d", index)),
		})
	}
	for _, preflight := range []struct {
		label     string
		preflight core.SuspendPreflight
	}{
		{"suspend dialog with untracked files", core.SuspendPreflight{SessionID: "session-1", Untracked: untracked}},
		{"suspend dialog with in-progress operation", core.SuspendPreflight{
			SessionID: "session-1",
			Operations: []core.ShownOperation{{
				Checkout: core.DefaultCheckoutKey(),
				Kind:     core.GitOperationKindRebase,
			}},
		}},
		{"suspend dialog with raw checkout", core.SuspendPreflight{SessionID: "session-1", Raw: true}},
	} {
		dashboard := dashboardWithSession(runningSession())
		check := preflight.preflight
		dialog := newConfirmDialog(ConfirmationSuspendSession{
			SessionID: "session-1",
			Preflight: &check,
		})
		dashboard.mode = ModeConfirm{&dialog}
		lines := drawn(dashboard, 100, 28)
		appendDialogRender(&output, preflight.label, 100, 28, lines)
	}

	targets := dashboardWithSession(runningSession())
	targets.CycleFocus(false)
	targets.CycleFocus(false)
	action := targets.HandleKey(key('e'))
	fmt.Fprintf(&output, "action: %s\n", dialogsDebugAction(action))
	lines := drawn(targets, 120, 34)
	appendDialogRender(&output, "target actions", 120, 34, lines)
	action = clickDialogLabel(targets, lines, "Test")
	fmt.Fprintf(&output, "action: %s\n", dialogsDebugAction(action))
	targets.ApplyTargetTest("podman", nil)
	lines = drawn(targets, 120, 34)
	appendDialogRender(&output, "target test result", 120, 34, lines)

	clickDialogLabel(targets, lines, "Rename")
	lines = drawn(targets, 120, 34)
	appendDialogRender(&output, "target ID editor", 120, 34, lines)
	targets.HandleKey(key(tea.KeyEnd))
	for _, character := range []rune{'-', 'v', '2'} {
		targets.HandleKey(key(character))
	}
	lines = drawn(targets, 120, 34)
	appendDialogRender(&output, "edited target ID", 120, 34, lines)
	action = targets.HandleKey(key(tea.KeyEnter))
	fmt.Fprintf(&output, "action: %s\n", dialogsDebugAction(action))

	action = targets.HandleKey(key('e'))
	fmt.Fprintf(&output, "action: %s\n", dialogsDebugAction(action))
	lines = drawn(targets, 120, 34)
	clickDialogLabel(targets, lines, "Rename")
	action = targets.HandleKey(key(tea.KeyEscape))
	fmt.Fprintf(&output, "action: %s\n", dialogsDebugAction(action))
	appendDialogRender(
		&output,
		"cancelled target ID editor returns to actions",
		120,
		34,
		drawn(targets, 120, 34),
	)

	changed := dashboardWithSession(runningSession())
	changed.FocusSessions()
	action = chord(changed, CommandIdChangedFiles)
	fmt.Fprintf(&output, "action: %s\n", dialogsDebugAction(action))
	appendDialogRender(
		&output,
		"changed files while the checkout probe is pending",
		100,
		24,
		drawn(changed, 100, 24),
	)
	var numstat, porcelain strings.Builder
	for index := range 36 {
		fmt.Fprintf(&numstat, "%d\t%d\tsrc/module-%02d.rs\x00", index+1, index, index)
		fmt.Fprintf(&porcelain, " M src/module-%02d.rs\x00", index)
	}
	aheadBehind := "1\t3"
	changed.SetGitStatus("session-1", client.DaemonOk(core.ParseGitStatus(
		"/workspace/project",
		"feature/dialogs",
		&aheadBehind,
		numstat.String(),
		porcelain.String(),
	)))
	lines = drawn(changed, 100, 24)
	appendDialogRender(&output, "changed files first page", 100, 24, lines)
	changed.HandleKey(key(tea.KeyPgDown))
	appendDialogRender(
		&output,
		"changed files after paging down",
		100,
		24,
		drawn(changed, 100, 24),
	)
	action = changed.HandleKey(key('r'))
	fmt.Fprintf(&output, "action: %s\n", dialogsDebugAction(action))
	action = changed.HandleKey(key(tea.KeyEscape))
	fmt.Fprintf(&output, "action: %s\n", dialogsDebugAction(action))
	appendDialogRender(
		&output,
		"dashboard after closing changed files",
		100,
		24,
		drawn(changed, 100, 24),
	)

	notices := dashboardWithSession(runningSession())
	for index := range 12 {
		notices.SetFailureNotice(fmt.Sprintf(
			"Build step %02d failed: the remote worker could not start its checkout or load the requested profile",
			index,
		))
	}
	openPalette(notices)
	for _, character := range "Recent messages" {
		notices.HandleKey(key(character))
	}
	appendDialogRender(
		&output,
		"command palette search for recent messages",
		100,
		28,
		drawn(notices, 100, 28),
	)
	action = notices.HandleKey(key(tea.KeyEnter))
	fmt.Fprintf(&output, "action: %s\n", dialogsDebugAction(action))
	lines = drawn(notices, 100, 28)
	normalizeNoticeLogAges(lines)
	appendDialogRender(&output, "recent messages", 100, 28, lines)
	notices.HandleKey(key(tea.KeyPgDown))
	lines = drawn(notices, 100, 28)
	normalizeNoticeLogAges(lines)
	appendDialogRender(&output, "recent messages after paging down", 100, 28, lines)
	notices.HandleKey(key(tea.KeyHome))
	action = notices.HandleKey(key(tea.KeyEscape))
	fmt.Fprintf(&output, "action: %s\n", dialogsDebugAction(action))

	cpu := dashboardWithSession(runningSession())
	sessionCPU := core.NewSnapshotMap[string, client.SessionCpuView]()
	sessionCPU.Insert("session-1", client.SessionCpuView{
		State: client.SessionCpuViewMeasured,
		Usage: core.SessionCpuUsage{
			RecentPermille:    230,
			HourlyPermille:    180,
			HourlyCoveredSecs: 3600,
			OnlineCpus:        8,
			Scope:             core.CpuSampleScopeProcessTree,
		},
	})
	for index := 2; index <= 18; index++ {
		id := fmt.Sprintf("session-%02d", index)
		session := runningSession()
		session.ID = id
		session.Title = fmt.Sprintf("CPU session %02d", index)
		session.ACPSessionTitle = nil
		cpu.state.Mut().Sessions.Insert(id, session)
		sessionCPU.Insert(id, client.SessionCpuView{
			State: client.SessionCpuViewMeasured,
			Usage: core.SessionCpuUsage{
				RecentPermille:    uint16(100 + index),
				HourlyPermille:    uint16(80 + index),
				HourlyCoveredSecs: 3600,
				OnlineCpus:        8,
				Scope:             core.CpuSampleScopeProcessTree,
			},
		})
	}
	publishFixtureVerdicts(cpu)
	cpu.SetSessionCPU(sessionCPU)
	openPalette(cpu)
	for _, character := range "CPU by session" {
		cpu.HandleKey(key(character))
	}
	appendDialogRender(
		&output,
		"command palette search for CPU by session",
		120,
		36,
		drawn(cpu, 120, 36),
	)
	action = cpu.HandleKey(key(tea.KeyEnter))
	fmt.Fprintf(&output, "action: %s\n", dialogsDebugAction(action))
	appendDialogRender(&output, "CPU by session", 120, 36, drawn(cpu, 120, 36))
	cpu.HandleKey(key(tea.KeyPgDown))
	appendDialogRender(&output, "CPU by session after paging down", 120, 36, drawn(cpu, 120, 36))
	action = cpu.HandleKey(key(tea.KeyEscape))
	fmt.Fprintf(&output, "action: %s\n", dialogsDebugAction(action))

	// Extend the full dialog golden through the Profiles menu and real
	// controls.
	usage := dashboardWithSession(runningSession())
	usage.beginSupportPaneMenu(SupportPaneQuota)
	appendDialogRender(&output, "Profiles menu with usage report", 120, 36, drawn(usage, 120, 36))
	usage.HandleKey(key('3'))
	appendDialogRender(&output, "Usage by session loading", 120, 36, drawn(usage, 120, 36))
	first, query, ok := usage.SessionUsageRequest()
	if !ok {
		t.Fatal("the usage report asks for its rows")
	}
	if !reflect.DeepEqual(query, core.SessionUsageQuery{}) {
		t.Fatalf("query %#v", query)
	}
	model := func(name string) *string { return &name }
	report := core.SessionUsageReport{
		Profiles:     []string{"claude", "codex"},
		CostUsdTicks: core.CostUsdTicks{Unknown: true},
		Sessions: []core.SessionUsageEstimate{
			{
				SessionID:    "session-2",
				Title:        "Write release notes",
				Profile:      "claude",
				CostUsdTicks: core.KnownCost(82_000_000_000),
				Models: []core.ModelUsageEstimate{
					{Model: model("claude-opus-5-5"), CostUsdTicks: core.KnownCost(62_000_000_000)},
					{Model: model("gpt-6.1-sol"), CostUsdTicks: core.KnownCost(20_000_000_000)},
				},
				Coverage: core.UsageCoverage{RecordedTurns: 5, FullTurnReports: 3, MissingReports: 2},
			},
			{
				SessionID:     "session-1",
				Title:         "Investigate recovery",
				Profile:       "codex",
				SubagentCount: 2,
				CostUsdTicks:  core.CostUsdTicks{Unknown: true},
				Models: []core.ModelUsageEstimate{
					{Model: model("gpt-6.1-sol"), CostUsdTicks: core.KnownCost(30_000_000_000)},
					{Model: model("gpt-6.1-terra"), CostUsdTicks: core.KnownCost(10_000_000_000)},
					{Model: model("claude-opus-5-5"), CostUsdTicks: core.KnownCost(71_000_000_000)},
					{Model: model("gpt-6-luna"), CostUsdTicks: core.KnownCost(1_050_000_000)},
					{Model: model("custom-model"), CostUsdTicks: core.CostUsdTicks{Unknown: true}},
				},
				Coverage: core.UsageCoverage{RecordedTurns: 4, FullTurnReports: 4},
			},
		},
	}
	roleModel := func(role core.UsageAgentRole, model core.ModelUsageEstimate) core.AgentUsageEstimate {
		return core.AgentUsageEstimate{Role: role, CostUsdTicks: model.CostUsdTicks, Models: []core.ModelUsageEstimate{model}}
	}
	codexModels := report.Sessions[1].Models
	splitSol := func(cost uint64) core.ModelUsageEstimate {
		split := codexModels[0]
		split.CostUsdTicks = core.KnownCost(cost)
		return split
	}
	report.AgentUsage = []core.AgentUsageEstimate{
		{
			Role:         core.UsageAgentRolePrimary,
			Models:       []core.ModelUsageEstimate{report.Sessions[0].Models[0], codexModels[1]},
			CostUsdTicks: core.KnownCost(72_000_000_000),
		},
		roleModel(core.UsageAgentRoleOrchestrator, splitSol(20_000_000_000)),
		roleModel(core.UsageAgentRoleSubtle, splitSol(15_000_000_000)),
		roleModel(core.UsageAgentRoleStraightforward, splitSol(10_000_000_000)),
		roleModel(core.UsageAgentRoleOracle, splitSol(5_000_000_000)),
		roleModel(core.UsageAgentRoleReviewer, codexModels[2]),
		roleModel(core.UsageAgentRoleSubagent, codexModels[3]),
		roleModel(core.UsageAgentRoleUnclassifiedSubagent, codexModels[4]),
	}
	// Dollars per hour; the 20th bar also ran a model with no known rate,
	// and the 14th ran only such models.
	hourDollars := []uint64{0, 0, 3, 8, 5, 0, 0, 12, 30, 22, 18, 9, 4, 0, 0, 0, 6, 14, 26, 41, 35, 20, 11, 7}
	for n, dollars := range hourDollars {
		report.Hours = append(report.Hours, core.HourlyUsage{
			StartMs:           (490_000 + int64(n)) * core.HourMs,
			KnownCostUsdTicks: dollars * 10_000_000_000,
			HasUnknownCost:    n == 19 || n == 13,
		})
	}
	usage.ApplySessionUsageResult(first, report, nil)
	appendDialogRender(&output, "Usage by session All with model costs", 120, 36, drawn(usage, 120, 36))
	// Hard-won: #1501: a wrapped fixed header clipped models and crowded out sessions.
	for range 3 {
		usage.HandleKey(key(tea.KeyTab))
	}
	usage.HandleKey(key(tea.KeyHome))
	for range 15 {
		usage.HandleKey(key(tea.KeyDown))
	}
	appendDialogRender(&output, "Usage by session All with remaining agent roles", 120, 36, drawn(usage, 120, 36))
	appendDialogRender(&output, "Usage narrow last model total", 70, 22, drawn(usage, 70, 22))
	usage.HandleKey(key(tea.KeyEnd))
	appendDialogRender(&output, "Usage narrow last session", 70, 22, drawn(usage, 70, 22))
	usage.HandleKey(key(tea.KeyHome))
	backTab := tea.KeyPressMsg{Code: tea.KeyTab, Mod: tea.ModShift}
	for range 3 {
		usage.HandleKey(backTab)
	}
	drawn(usage, 120, 36)
	usage.HandleKey(key(tea.KeyEnter))
	appendDialogRender(&output, "Usage profile dropdown", 120, 36, drawn(usage, 120, 36))
	usageMode, ok := usage.mode.(ModeSessionUsageReport)
	if !ok {
		t.Fatalf("usage dialog closed, got %T", usage.mode)
	}
	codexIndex := slices.Index(usageMode.profiles, "codex") + 1
	for range codexIndex {
		usage.HandleKey(key(tea.KeyDown))
	}
	usage.HandleKey(key(tea.KeyEnter))
	profileRequest, query, _ := usage.SessionUsageRequest()
	if query.Profile == nil || *query.Profile != "codex" {
		t.Fatalf("profile %v", query.Profile)
	}
	// The old All result cannot replace the filtered loading state.
	usage.ApplySessionUsageResult(first, report, nil)
	if mode, ok := usage.mode.(ModeSessionUsageReport); !ok || mode.report != nil {
		t.Fatal("the old All result replaced the filtered loading state")
	}
	codexReport := report
	codexSession := report.Sessions[1]
	codexSession.Models = slices.DeleteFunc(slices.Clone(codexSession.Models), func(model core.ModelUsageEstimate) bool {
		return model.Model != nil && *model.Model == "claude-opus-5-5"
	})
	codexReport.Sessions = []core.SessionUsageEstimate{codexSession}
	codexReport.AgentUsage = []core.AgentUsageEstimate{
		roleModel(core.UsageAgentRolePrimary, codexModels[1]),
		roleModel(core.UsageAgentRoleOrchestrator, splitSol(10_000_000_000)),
		roleModel(core.UsageAgentRoleSubtle, splitSol(10_000_000_000)),
		roleModel(core.UsageAgentRoleStraightforward, splitSol(5_000_000_000)),
		roleModel(core.UsageAgentRoleOracle, splitSol(5_000_000_000)),
		{Role: core.UsageAgentRoleReviewer, Models: []core.ModelUsageEstimate{}, CostUsdTicks: core.KnownCost(0)},
		roleModel(core.UsageAgentRoleSubagent, codexModels[3]),
		roleModel(core.UsageAgentRoleUnclassifiedSubagent, codexModels[4]),
	}
	usage.ApplySessionUsageResult(profileRequest, codexReport, nil)
	// Hard-won: #1501: unknown totals hid the known per-model sums in both views.
	appendDialogRender(&output, "Usage Codex with model totals", 120, 36, drawn(usage, 120, 36))
	// Clicking the first bar with cost selects its hour, and the rows reload.
	lines = drawn(usage, 120, 36)
	baseRow := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "$0 ") })
	if baseRow < 0 {
		t.Fatal("no graph base row")
	}
	column := slices.Index([]rune(lines[baseRow]), '▂')
	click := func(usage *DashboardState) {
		for _, kind := range []tea.MouseMsg{dialogsLeftDown, dialogsLeftUp} {
			usage.HandleMouse(mouseAt(kind, uv.Pos(column, baseRow)))
		}
	}
	click(usage)
	hourRequest, query, _ := usage.SessionUsageRequest()
	if query.HourStartMs == nil || *query.HourStartMs != (490_000+2)*core.HourMs {
		t.Fatalf("hour %v", query.HourStartMs)
	}
	usage.ApplySessionUsageResult(hourRequest, codexReport, nil)
	appendDialogRender(&output, "Usage one hour selected", 120, 36, drawn(usage, 120, 36))
	// Clicking the selected hour again goes back to all hours.
	click(usage)
	if _, query, _ := usage.SessionUsageRequest(); query.HourStartMs != nil {
		t.Fatalf("hour %d", *query.HourStartMs)
	}
	allRequest, _, _ := usage.SessionUsageRequest()
	usage.ApplySessionUsageResult(allRequest, codexReport, nil)
	// The arrow keys move from the clicked bar to its neighbour; Enter goes
	// back to all hours.
	usage.HandleKey(key(tea.KeyLeft))
	if _, query, _ := usage.SessionUsageRequest(); query.HourStartMs == nil || *query.HourStartMs != (490_000+1)*core.HourMs {
		t.Fatalf("hour %v", query.HourStartMs)
	}
	// Holding an arrow past the oldest hour stays on it instead of clearing
	// the choice.
	for range 30 {
		usage.HandleKey(key(tea.KeyLeft))
	}
	if _, query, _ := usage.SessionUsageRequest(); query.HourStartMs == nil || *query.HourStartMs != 490_000*core.HourMs {
		t.Fatalf("hour %v", query.HourStartMs)
	}
	usage.HandleKey(key(tea.KeyEnter))
	if _, query, _ := usage.SessionUsageRequest(); query.HourStartMs != nil {
		t.Fatalf("hour %d", *query.HourStartMs)
	}
	drawn(usage, 120, 36)
	usage.HandleKey(backTab)
	usage.HandleKey(key(' '))
	weeklyRequest, query, _ := usage.SessionUsageRequest()
	if !query.IncludeSuspended {
		t.Fatal("Space includes suspended sessions")
	}
	suspended := codexReport
	suspendedSession := codexReport.Sessions[0]
	suspendedSession.Suspended = true
	suspended.Sessions = []core.SessionUsageEstimate{suspendedSession}
	usage.ApplySessionUsageResult(weeklyRequest, suspended, nil)
	appendDialogRender(&output, "Usage weekly including suspended", 120, 36, drawn(usage, 120, 36))
	// A short terminal drops the graph and keeps the rows.
	appendDialogRender(&output, "Usage short terminal", 70, 17, drawn(usage, 70, 17))
	usage.ApplySessionUsageResult(weeklyRequest, core.SessionUsageReport{}, errors.New("test store unavailable"))
	appendDialogRender(&output, "Usage report failure", 120, 36, drawn(usage, 120, 36))
	usage.HandleKey(key(tea.KeyEscape))
	if _, _, ok := usage.SessionUsageRequest(); ok {
		t.Fatal("closing the report ends its request")
	}
	usage.DispatchCommand(CommandIdSessionUsageReport)
	reopened, _, _ := usage.SessionUsageRequest()
	if weeklyRequest == reopened {
		t.Fatal("a reopened report asks under a new request")
	}
	usage.ApplySessionUsageResult(weeklyRequest, report, nil)
	if mode, ok := usage.mode.(ModeSessionUsageReport); !ok || mode.report != nil {
		t.Fatal("a stale result replaced the reopened report")
	}
	usage.HandleKey(key(tea.KeyEscape))

	// The first new session asks the binary to read the host's Git identity;
	// asking again while that read runs starts nothing more.
	identity := dashboardWithSession(runningSession())
	identity.RequireGitIdentityCheck()
	action = identity.DispatchCommand(CommandIdNewSessionWizard)
	fmt.Fprintf(&output, "new session before the Git identity check: %s\n", dialogsDebugAction(action))
	action = identity.DispatchCommand(CommandIdNewSessionWizard)
	fmt.Fprintf(&output, "new session while the check runs: %s\n", dialogsDebugAction(action))
	fmt.Fprintf(&output, "notice while the check runs: %s\n", dialogsDebugNotice(identity))
	adaName := "Ada Lovelace"
	identity.ApplyGitIdentityCheck(client.GitIdentity{Name: &adaName}, nil)
	for _, character := range "ada.example.com" {
		identity.HandleKey(key(character))
	}
	appendDialogRender(&output, "Git identity with the email typed", 100, 24, drawn(identity, 100, 24))
	action = identity.HandleKey(key(tea.KeyEnter))
	fmt.Fprintf(&output, "action: %s\n", dialogsDebugAction(action))
	action = identity.HandleKey(key(tea.KeyEscape))
	fmt.Fprintf(&output, "escape while saving: %s\n", dialogsDebugAction(action))
	appendDialogRender(&output, "Git identity saving", 100, 24, drawn(identity, 100, 24))
	identity.ApplyGitIdentitySave(client.GitIdentity{}, errors.New("Git email \"ada.example.com\" needs an @"))
	appendDialogRender(&output, "Git identity save failure keeps the fields", 100, 24, drawn(identity, 100, 24))
	action = identity.HandleKey(key(tea.KeyEnter))
	fmt.Fprintf(&output, "action: %s\n", dialogsDebugAction(action))
	adaEmail := "ada@example.com"
	identity.ApplyGitIdentitySave(client.GitIdentity{Name: &adaName, Email: &adaEmail}, nil)
	followup := identity.TakeGitIdentityFollowup()
	fmt.Fprintf(&output, "follow-up after saving: %s\n", dialogsDebugOptionalAction(followup))
	fmt.Fprintf(&output, "notice after saving: %s\n", dialogsDebugNotice(identity))
	if _, ok := identity.mode.(ModeNew); !ok {
		t.Fatalf("mode %T", identity.mode)
	}
	if action := identity.DispatchCommand(CommandIdNewSessionWizard); reflect.DeepEqual(action, DashboardActionCheckGitIdentity{}) {
		t.Fatal("a settled identity is not checked again")
	}

	skipped := dashboardWithSession(runningSession())
	skipped.RequireGitIdentityCheck()
	skipped.DispatchCommand(CommandIdNewSessionWizard)
	skipped.ApplyGitIdentityCheck(client.GitIdentity{}, nil)
	action = skipped.HandleKey(key(tea.KeyEscape))
	fmt.Fprintf(&output, "skip: %s\n", dialogsDebugAction(action))
	followup = skipped.TakeGitIdentityFollowup()
	fmt.Fprintf(&output, "follow-up after skipping: %s\n", dialogsDebugOptionalAction(followup))
	fmt.Fprintf(&output, "notice after skipping: %s\n", dialogsDebugNotice(skipped))
	if _, ok := skipped.mode.(ModeNew); !ok {
		t.Fatalf("mode %T", skipped.mode)
	}

	// A launch that bypasses the new-session key waits for the same check,
	// and runs even when the user moved on before the answer came back.
	launch := dashboardWithSession(runningSession())
	launch.RequireGitIdentityCheck()
	recipe := DashboardActionGoLaunch{Recipe: core.GoRecipe{
		ProfileID:        "codex",
		TargetID:         "localhost",
		AdditionalMounts: []core.AdditionalMount{},
	}}
	action = launch.GateLaunchOnGitIdentity(recipe)
	fmt.Fprintf(&output, "fast-start launch before the check: %s\n", dialogsDebugAction(action))
	launch.DispatchCommand(CommandIdSessionUsageReport)
	launch.ApplyGitIdentityCheck(client.GitIdentity{}, nil)
	if followup := launch.TakeGitIdentityFollowup(); !reflect.DeepEqual(followup, recipe) {
		t.Fatalf("follow-up %#v", followup)
	}
	fmt.Fprintf(&output, "notice when the check finds no identity behind another dialog: %s\n", dialogsDebugNotice(launch))

	core.AssertGolden(t, "testdata", "dashboard-dialogs", strings.TrimRight(output.String(), "\n"))
}

func TestGoldenTuiWebViewerDialog(t *testing.T) {
	var output strings.Builder
	url := "https://example.test/auth/login?token=secret"
	qr, err := renderQR(url)
	if err != nil {
		t.Fatalf("QR code: %v", err)
	}
	var qrRows []string
	for _, line := range dialogsLines(qr) {
		qrRows = append(qrRows, strings.ReplaceAll(line, " ", "·"))
	}
	width := 0
	if len(qrRows) > 0 {
		width = utf8.RuneCountInString(qrRows[0])
	}
	appendDialogRender(&output, "QR module map; · marks a light module", width, len(qrRows), qrRows)

	noQR := webDialogLoading()
	noQR.loading = false
	viewerURL, viewerCode, fallback := "http://127.0.0.1:37650", "022160", "automatic Tailscale detection is disabled"
	noQR.viewerURL, noQR.viewerCode, noQR.fallbackReason = &viewerURL, &viewerCode, &fallback
	appendDialogRender(&output, "access details without QR", 140, 40, drawWebDialog(&noQR, 140, 40))

	longURL := "https://a-very-long-machine-name.some-tailnet.ts.net:37650/viewer"
	longQR, err := renderQR(longURL)
	if err != nil {
		t.Fatalf("QR code: %v", err)
	}
	wrapped := webDialogLoading()
	wrapped.loading = false
	wrapped.viewerURL, wrapped.viewerCode, wrapped.qr = &longURL, &viewerCode, &longQR
	appendDialogRender(&output, "long URL wrapped with QR", 60, 40, drawWebDialog(&wrapped, 60, 40))

	conflict := failedWebDashboard()
	dialog := dialogsWebDialog(t, conflict)
	for _, size := range [][2]int{{140, 40}, {80, 24}, {60, 20}} {
		appendDialogRender(
			&output,
			"port conflict and recovery actions",
			size[0],
			size[1],
			drawWebDialog(dialog, size[0], size[1]),
		)
	}

	core.AssertGolden(t, "testdata", "tui-web-viewer-dialog", strings.TrimRight(output.String(), "\n"))
}

func TestGoldenTuiContainerEditorDetails(t *testing.T) {
	var output strings.Builder

	cachedSession := runningSession()
	cachedSession.ResourceAllocation = core.SessionResourceAllocationContainer{Cpus: 4, MemoryBytes: 8 << 30}
	maxSize := "1000GB"
	cachedSession.BuildCache = &core.SessionBuildCache{
		Host:      "ssh:morannon",
		Directory: "/mnt/nvme/mbx",
		MaxSize:   &maxSize,
	}
	cached := dashboardWithSession(cachedSession)
	openContainerEditor(t, cached)
	appendDialogRender(&output, "session build cache", 120, 40, drawn(cached, 120, 40))

	containerEditor(t, cached).cpus = chat.TextInputFromValue("1.5")
	for containerEditor(t, cached).focused() != ContainerEditFocusSave {
		cached.HandleKey(key(tea.KeyTab))
	}
	cached.HandleKey(key(tea.KeyEnter))
	appendDialogRender(&output, "invalid fractional CPUs", 120, 40, drawn(cached, 120, 40))

	uncached := dashboardWithSession(runningSession())
	openContainerEditor(t, uncached)
	appendDialogRender(&output, "session without build cache", 120, 40, drawn(uncached, 120, 40))

	mounted := dashboardWithContainerSession()
	openContainerEditor(t, mounted)
	appendDialogRender(&output, "container settings can replace the container", 100, 40, drawn(mounted, 100, 40))

	mounted.CancelModal()
	mounted.FocusSessions()
	mounted.SetContainerUpgradeSessions(map[string]struct{}{"session-1": {}})
	selectedImage := "sha256:selected"
	preview := core.ContainerUpgradePreview{
		SessionID:         "session-1",
		SourceContainerID: "existing-container",
		Warning:           core.ContainerUpgradeWarning,
		CurrentImage:      "ghcr.io/brokkai/mjolnir/agent-dev:2.51.0",
		CurrentImageID:    "sha256:previous",
		TargetImageID:     &selectedImage,
		TargetImage:       "ghcr.io/brokkai/mjolnir/agent-dev:2.52.0",
		Available:         true,
		RetainedMounts:    []string{"/workspace", "/cache", "/tmp"},
	}
	if action := mounted.DispatchCommand(CommandIdRestartSession); !reflect.DeepEqual(action, DashboardActionPreviewContainerUpgrade{
		SessionID:   "session-1",
		UpgradeOnly: false,
	}) {
		t.Fatalf("restart: %#v", action)
	}
	mounted.ShowContainerUpgradePreview(preview, false)
	appendDialogRender(
		&output,
		"Restart keeps its environment; container upgrade is a separate choice",
		120,
		32,
		drawn(mounted, 120, 32),
	)
	if action := mounted.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("Enter: %#v", action)
	}
	if _, ok := mounted.mode.(ModeDashboard); !ok {
		t.Fatalf("mode %T", mounted.mode)
	}
	mounted.ShowContainerUpgradePreview(preview, false)
	drawn(mounted, 120, 32)
	mounted.HandleKey(key(tea.KeyRight))
	if action := mounted.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, DashboardActionRestartSession{
		SessionID:                      "session-1",
		AcknowledgeContainerRecreation: false,
	}) {
		t.Fatalf("Restart agent: %#v", action)
	}
	mounted.ShowContainerUpgradePreview(preview, true)
	appendDialogRender(
		&output,
		"Explicit upgrade discloses image versions, retained storage, and writable-layer loss",
		120,
		32,
		drawn(mounted, 120, 32),
	)
	mounted.HandleKey(key(tea.KeyRight))
	if action := mounted.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, DashboardActionUpgradeContainer{
		Request: core.ContainerUpgradeRequest{Selection: preview, AcknowledgeWritableLayerLoss: true},
	}) {
		t.Fatalf("Upgrade: %#v", action)
	}
	mounted.SetContainerUpgradeSessions(map[string]struct{}{})
	if action := mounted.DispatchCommand(CommandIdRestartSession); !reflect.DeepEqual(action, DashboardActionRestartSession{
		SessionID:                      "session-1",
		AcknowledgeContainerRecreation: false,
	}) {
		t.Fatalf("restart: %#v", action)
	}

	core.AssertGolden(t, "testdata", "tui-container-editor-details", output.String())
}

func TestGoldenTuiImportConfirmation(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.ShowImportBundleConfirmation(
		[]string{"/work/repo — 1 tracked change · 2 untracked paths"},
		nil,
		[]string{"/tmp/mj-golden-scratch"},
		core.ManagedWorktreeOptions{Available: true, DefaultCreate: true},
		"/work/repo",
	)
	lines := drawn(dashboard, 120, 30)
	var output strings.Builder
	appendDialogRender(&output, "scratch repository excluded from workspace", 120, 30, lines)
	dashboard.ShowImportProgress("Imported session")
	dashboard.HandleKey(key(tea.KeyEscape))
	if _, ok := dashboard.mode.(ModeConfirm); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
	dashboard.ShowImportRegistration()
	if action := dashboard.HandleKey(key(tea.KeyEscape)); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("Esc: %#v", action)
	}
	if _, ok := dashboard.mode.(ModeImporting); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
	appendDialogRender(&output, "daemon registration cannot be cancelled", 120, 30, drawn(dashboard, 120, 30))
	dashboard.ShowImportUpload()
	if mode, ok := dashboard.mode.(ModeImporting); !ok || !mode.cancellable {
		t.Fatal("the upload can be cancelled")
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	if _, ok := dashboard.mode.(ModeConfirm); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
	dashboard.ShowImportRegistration()
	if action := dashboard.HandleKey(key(tea.KeyEscape)); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("Esc: %#v", action)
	}
	if mode, ok := dashboard.mode.(ModeImporting); !ok || mode.cancellable {
		t.Fatal("registration cannot be cancelled")
	}
	core.AssertGolden(t, "testdata", "tui-import-confirmation", output.String())
}

func TestGoldenTuiCheckpointOriginDialog(t *testing.T) {
	appendState := func(output *strings.Builder, label string, terminal *dialogsTerminal, width, height int) {
		if terminal.cursor == nil {
			t.Fatal("source cursor")
		}
		cursor := *terminal.cursor
		buffer := terminal.buffer
		lines := bufferLines(buffer)
		appendDialogRender(output, label, width, height, lines)

		area := buffer.Bounds()
		sourceRow := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "Source:") })
		if sourceRow < 0 {
			t.Fatal("source field row")
		}
		background := func(x, y int) chat.Color {
			return chat.CellStyle(buffer.CellAt(x, y)).Bg
		}
		fieldX := area.Min.X + cellColumn(lines[sourceRow], "Source:") + 8
		sourceY := area.Min.Y + sourceRow
		sourceFocused := background(fieldX, sourceY) == chat.Field(true).Bg
		buttonRow := slices.IndexFunc(lines, func(line string) bool {
			return strings.Contains(line, " Cancel ") && strings.Contains(line, " Check origin ")
		})
		if buttonRow < 0 {
			t.Fatal("origin action row")
		}
		buttonY := area.Min.Y + buttonRow
		cancelX := area.Min.X + cellColumn(lines[buttonRow], "Cancel")
		checkX := area.Min.X + cellColumn(lines[buttonRow], "Check origin")
		selection := chat.ThemePalette().Selection
		buttonsSelected := background(cancelX, buttonY) == selection && background(checkX, buttonY) == selection
		if sourceFocused {
			fmt.Fprintf(output, "cursor: (%d, %d)\n", cursor.X, cursor.Y)
		} else {
			fmt.Fprintln(output, "cursor: hidden")
		}
		fmt.Fprintf(output, "source field focused: %t\n", sourceFocused)
		fmt.Fprintf(output, "both actions selected: %t\n", buttonsSelected)
		fmt.Fprintf(output, "Cancel action focused: %t\n", background(cancelX, buttonY) == chat.ThemePalette().Accent)
	}

	dashboard := dashboardWithSession(stoppedSession())
	dashboard.ShowRepositoryOriginDialog(
		"session-1",
		"bifrost",
		"b41dc78",
		"https://github.com/BrokkAi/bifrost.git",
		"BrokkAi/bifrost-dev",
		DashboardActionNone{},
	)
	terminal := newDialogsTerminal(100, 24)
	terminal.draw(dashboard)
	var output strings.Builder
	appendState(&output, "focused replacement source", terminal, 100, 24)

	dashboard.HandleKey(key(tea.KeyTab))
	terminal.draw(dashboard)
	appendState(&output, "focus moves to origin actions", terminal, 100, 24)

	core.AssertGolden(t, "testdata", "tui-checkpoint-origin-dialog", output.String())
}

func dialogsResumeLaunch() DashboardActionResumeSession {
	return DashboardActionResumeSession{
		WorkspaceID:      core.DefaultWorkspaceID,
		SessionID:        "session-1",
		ProfileID:        "codex-1",
		TargetTemplateID: "podman",
	}
}

func dialogsRepositoryOrigin(t *testing.T, dashboard *DashboardState) *RepositoryOriginDialog {
	t.Helper()
	mode, ok := dashboard.mode.(ModeRepositoryOrigin)
	if !ok {
		t.Fatalf("expected repository origin dialog, got %T", dashboard.mode)
	}
	return mode.RepositoryOriginDialog
}

func TestMissingCheckpointHistoryDialogAcceptsAReplacementOrigin(t *testing.T) {
	launch := dialogsResumeLaunch()
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.ShowRepositoryOriginDialog(
		"session-1",
		"bifrost",
		"b41dc78",
		"https://github.com/BrokkAi/bifrost.git",
		"BrokkAi/bifrost",
		launch,
	)
	dashboard.HandlePaste("BrokkAi/bifrost-dev\n")

	if action := dashboard.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, DashboardActionReplaceResumeRepositoryOrigin{
		SessionID:    "session-1",
		RepositoryID: "bifrost",
		Replacement:  "BrokkAi/bifrost-dev",
		Launch:       launch,
	}) {
		t.Fatalf("Enter: %#v", action)
	}
	if commit := dialogsRepositoryOrigin(t, dashboard).missingCommit; commit != "b41dc78" {
		t.Fatalf("missing commit %q", commit)
	}

	dashboard.ApplyRepositoryOriginFailure("bifrost", "That origin does not contain checkpoint base b41dc78.")
	dialog := dialogsRepositoryOrigin(t, dashboard)
	if focused, ok := dialog.form.Focused(); !ok || focused != DialogControlField {
		t.Fatalf("focused %v, %t", focused, ok)
	}
	if dialog.error == nil || *dialog.error != "That origin does not contain checkpoint base b41dc78." {
		t.Fatalf("error %v", dialog.error)
	}
}

// The replacement origin may be a URL or `owner/repo`, so completion is
// offered only while the text reads as a path on this machine.
func TestRepositoryOriginCompletesLocalPaths(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.ShowRepositoryOriginDialog(
		"session-1",
		"bifrost",
		"b41dc78",
		"https://github.com/BrokkAi/bifrost.git",
		"BrokkAi/bifrost",
		dialogsResumeLaunch(),
	)
	complete := dialogsControlKey(' ')

	dashboard.HandlePaste("BrokkAi/bifrost-dev")
	if action := dashboard.HandleKey(complete); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("complete: %#v", action)
	}

	dialogsRepositoryOrigin(t, dashboard).replacement.SetValue("/srv/b")
	if action := dashboard.HandleKey(complete); !reflect.DeepEqual(action, DashboardActionCompletePath{
		Host:   core.CompletionHostLocal{},
		Kind:   core.CompletionKindDirectories,
		Prefix: "/srv/b",
	}) {
		t.Fatalf("complete: %#v", action)
	}
	context := dashboard.PathInputContext()
	dashboard.ApplyPathCompletions(context, "/srv/b", core.PathCompletion{
		Candidates: []string{"/srv/bifrost/", "/srv/bridge/"},
	})
	if !dialogsRepositoryOrigin(t, dashboard).replacement.IsCompleting() {
		t.Fatal("the completions open")
	}

	dashboard.HandleKey(key(tea.KeyEnter))
	if replacement := dialogsRepositoryOrigin(t, dashboard).replacement.Value(); replacement != "/srv/bifrost/" {
		t.Fatalf("replacement %q", replacement)
	}
}

// Hard-won: bda82985: Notice rows shifted when an age exceeded the fixed eight-cell column.
func TestTheNoticeLogAgeColumnKeepsMessagesAlignedPastOneMinute(t *testing.T) {
	ages := noticeLogAges([]uint64{44, 77, 3_700})
	for _, age := range ages {
		if utf8.RuneCountInString(age) != utf8.RuneCountInString(ages[0]) {
			t.Fatalf("%q", ages)
		}
		if !strings.HasSuffix(age, "ago ") {
			t.Fatalf("%q", ages)
		}
	}
	if !strings.HasPrefix(strings.TrimLeft(ages[0], " "), "44s") {
		t.Fatalf("%q", ages)
	}
}

func TestLaunchFailureSurvivesNoticesAndRetriesOriginalSettingsOnce(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	retry := DashboardActionCreateSession{
		WorkspaceID:      "original-workspace",
		ProfileID:        "codex",
		BundleID:         "project",
		TargetTemplateID: "docker",
	}
	dashboard.ShowLaunchFailure("upload failed", retry)
	dashboard.SetNotice("Quota refreshed")
	terminal := newDialogsTerminal(90, 25)
	terminal.draw(dashboard)
	text := terminal.text()
	for _, wanted := range []string{"Launch failed", "upload failed", "Retry launch"} {
		if !strings.Contains(text, wanted) {
			t.Fatalf("missing %q", wanted)
		}
	}
	dashboard.HandleKey(key(tea.KeyRight))
	if action := dashboard.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, retry) {
		t.Fatalf("Retry launch: %#v", action)
	}
	if _, ok := dashboard.mode.(ModeConfirm); ok {
		t.Fatal("the retry closes the dialog")
	}
}
