// Port of mj-tui/src/setup/tests.rs: the helpers the Settings tests share.
// The tests themselves are the setup_tests_*_test.go files, one per Rust
// module under setup/tests/.
//
// Helper names carry a setup prefix where the Rust name (click, choose,
// search, ...) is too general for the flat package.

package tui

import (
	"encoding/json"
	"fmt"
	"image/color"
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"
	"github.com/charmbracelet/x/ansi"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// setupButtonSpec is one Settings action: Rust's
// `(SetupControl, &'static str, bool)`.
type setupButtonSpec = chat.ButtonSpec[SetupControl]

// setupMouseDown and setupMouseUp are crossterm's Down(Left) and Up(Left).
var (
	setupMouseDown tea.MouseMsg = tea.MouseClickMsg{Button: tea.MouseLeft}
	setupMouseUp   tea.MouseMsg = tea.MouseReleaseMsg{Button: tea.MouseLeft}
)

// setupClickAt presses and releases the left button at one cell, drawing
// after each event as the terminal does.
func setupClickAt(dashboard *DashboardState, at uv.Position, width, height int) {
	for _, kind := range []tea.MouseMsg{setupMouseDown, setupMouseUp} {
		dashboard.HandleMouse(mouseAt(kind, at))
		drawn(dashboard, width, height)
	}
}

// setupWheel is one wheel step at a cell: down when down, up otherwise.
func setupWheel(down bool, at uv.Position) tea.MouseMsg {
	button := tea.MouseWheelUp
	if down {
		button = tea.MouseWheelDown
	}
	return mouseAt(tea.MouseWheelMsg{Button: button}, at)
}

// setupCtrl is a key with Control held, as crossterm's
// `KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)`.
func setupCtrl(character rune) tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: character, Mod: tea.ModCtrl}
}

// setupClick is the module's click: one completed left click on label.
func setupClick(dashboard *DashboardState, label string) {
	lines := drawn(dashboard, 140, 40)
	setupClickAt(dashboard, point(lines, label), 140, 40)
}

// setupDialog is the open Settings dialog, failing the test when another
// mode is open.
func setupDialog(t testing.TB, dashboard *DashboardState) *SetupDialog {
	t.Helper()
	dialog := setupDialogMut(dashboard.mode)
	if dialog == nil {
		t.Fatalf("settings: the dashboard shows %T", dashboard.mode)
	}
	return dialog
}

// setupChoose is the module's choose: Tab to the setting named name on the
// open page and press Enter on it, unless it is a text field.
func setupChoose(t testing.TB, dashboard *DashboardState, name string) DashboardAction {
	t.Helper()
	dialog := setupDialog(t, dashboard)
	dialog.prepare()
	var target SetupControl
	found := false
	for _, control := range dialog.descriptors {
		if len(control.path) > 0 && control.path[len(control.path)-1] == name {
			target, found = control.id, true
			break
		}
	}
	if !found {
		t.Fatalf("missing %q on %s", name, setupDebugStrings(dialog.path))
	}
	for range 100 {
		if focused, ok := setupDialog(t, dashboard).form.Focused(); ok && focused == target {
			break
		}
		dashboard.HandleKey(key(tea.KeyTab))
	}
	dialog = setupDialog(t, dashboard)
	if !dialog.form.IsFocused(target) {
		t.Fatalf("%q is not focused", name)
	}
	control, _ := dialog.descriptor(target)
	if _, field := control.kind.(SettingKindField); field {
		return DashboardActionNone{}
	}
	return dashboard.HandleKey(key(tea.KeyEnter))
}

// setupActivate focuses control and presses Enter on it.
func setupActivate(t testing.TB, dashboard *DashboardState, control SetupControl) {
	t.Helper()
	setupDialog(t, dashboard).form.Focus(control)
	dashboard.HandleKey(key(tea.KeyEnter))
}

func setupChooseLightTheme(t testing.TB, dashboard *DashboardState) {
	t.Helper()
	chord(dashboard, CommandIdOpenConfig)
	setupChoose(t, dashboard, "interface")
	setupChoose(t, dashboard, "theme")
	dashboard.HandleKey(key(tea.KeyDown))
	dashboard.HandleKey(key(tea.KeyEnter))
	dashboard.HandleKey(key(tea.KeyBackspace))
}

// setupRender is Rust's test-only `crate::render::render`: the combined
// surface with no conversation, independent of NO_COLOR.
func setupRender(frame *chat.Frame, dashboard *DashboardState) {
	withNoColorOverrideForTest(true, func() struct{} {
		renderCombinedForTest(frame, dashboard, map[string]*chat.ActiveChat{}, map[PaneId]string{}, false)
		return struct{}{}
	})
}

// setupRenderBuffer draws the dashboard into a fresh terminal of the given
// size (Rust's `Terminal::new(TestBackend::new(..))` and one draw).
func setupRenderBuffer(dashboard *DashboardState, width, height int) *uv.Buffer {
	buffer := uv.NewBuffer(width, height)
	setupRender(chat.NewFrame(buffer), dashboard)
	return buffer
}

// setupThemeCells finds the first cell drawn in the theme's text color on
// its surface, and the first in its accent.
func setupThemeCells(dashboard *DashboardState, buffer *uv.Buffer, selected core.UiTheme) (text, accent *uv.Cell) {
	colors := chat.PaletteFor(selected)
	surface := colors.Surface.TerminalColor()
	if dashboard.ModalOpen() {
		surface = colors.SurfaceRaised.TerminalColor()
	}
	textColor, accentColor := colors.Text.TerminalColor(), colors.Accent.TerminalColor()
	bounds := buffer.Bounds()
	for y := bounds.Min.Y; y < bounds.Max.Y; y++ {
		for x := bounds.Min.X; x < bounds.Max.X; x++ {
			cell := buffer.CellAt(x, y)
			if cell == nil {
				continue
			}
			if text == nil && cell.Style.Bg == surface && cell.Style.Fg == textColor && cell.Content != " " {
				text = cell
			}
			if accent == nil && cell.Style.Fg == accentColor {
				accent = cell
			}
		}
	}
	return text, accent
}

func setupAssertRenderedTheme(t testing.TB, dashboard *DashboardState, selected core.UiTheme) {
	t.Helper()
	text, accent := setupThemeCells(dashboard, setupRenderBuffer(dashboard, 100, 30), selected)
	if text == nil {
		t.Fatalf("no text cell is drawn in %s", selected)
	}
	if accent == nil {
		t.Fatalf("no accent cell is drawn in %s", selected)
	}
}

func setupChooseByKeyboard(t testing.TB, dashboard *DashboardState, name string) DashboardAction {
	t.Helper()
	return setupChoose(t, dashboard, name)
}

// setupAt is a dashboard with Settings open on the page path leads to.
func setupAt(t testing.TB, path ...string) *DashboardState {
	t.Helper()
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	for _, name := range path {
		setupChooseByKeyboard(t, dashboard, name)
	}
	return dashboard
}

func appendSetupState(output *strings.Builder, label string, dashboard *DashboardState, width, height int) []string {
	lines := drawn(dashboard, width, height)
	if output.Len() > 0 {
		output.WriteByte('\n')
	}
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, width, height)
	output.WriteString(strings.Join(lines, "\n"))
	output.WriteByte('\n')
	return lines
}

func appendSetupBodySize(t testing.TB, output *strings.Builder, dashboard *DashboardState) {
	t.Helper()
	surface := dashboard.FrameSurfaces().Surface(chat.SurfaceIdModalBody{})
	if surface == nil {
		t.Fatal("rendered Settings modal body")
	}
	fmt.Fprintf(output, "modal body: %s\n", setupRectDebug(surface.Rect))
}

// setupRectDebug is ratatui's Debug of a Rect.
func setupRectDebug(rect uv.Rectangle) string {
	return fmt.Sprintf("Rect { x: %d, y: %d, width: %d, height: %d }", rect.Min.X, rect.Min.Y, rect.Dx(), rect.Dy())
}

// setupColorDebug is ratatui's Debug of a cell color: Reset for the
// terminal's own, Indexed(n) for the indexed palette, Rgb(r, g, b) for the
// rest.
func setupColorDebug(c color.Color) string {
	switch typed := c.(type) {
	case nil:
		return "Reset"
	case ansi.IndexedColor:
		return fmt.Sprintf("Indexed(%d)", uint8(typed))
	case ansi.BasicColor:
		names := [...]string{"Black", "Red", "Green", "Yellow", "Blue", "Magenta", "Cyan", "Gray",
			"DarkGray", "LightRed", "LightGreen", "LightYellow", "LightBlue", "LightMagenta", "LightCyan", "White"}
		if int(typed) < len(names) {
			return names[typed]
		}
	}
	r, g, b, _ := c.RGBA()
	return fmt.Sprintf("Rgb(%d, %d, %d)", r>>8, g>>8, b>>8)
}

func setupRenderedThemeColors(t testing.TB, dashboard *DashboardState, selected core.UiTheme) string {
	t.Helper()
	text, accent := setupThemeCells(dashboard, setupRenderBuffer(dashboard, 100, 30), selected)
	if text == nil {
		t.Fatal("rendered text uses the selected theme")
	}
	if accent == nil {
		t.Fatal("rendered accent uses the selected theme")
	}
	return fmt.Sprintf("rendered colors: text fg=%s bg=%s; accent fg=%s",
		setupColorDebug(text.Style.Fg), setupColorDebug(text.Style.Bg), setupColorDebug(accent.Style.Fg))
}

func setupDetection(scope DetectScope, config core.Config) SetupDetection {
	return SetupDetection{
		ProfileReadiness: map[string]*string{},
		Scope:            scope,
		Config:           config,
		RejectedRuntimes: nil,
		Machines:         nil,
	}
}

// setupSelectChoice opens the selected field's choice list and commits
// wanted.
func setupSelectChoice(t testing.TB, dashboard *DashboardState, wanted string) {
	t.Helper()
	setupSelectValue(t, dashboard, wanted)
}

func setupSelectValue(t testing.TB, dashboard *DashboardState, wanted any) {
	t.Helper()
	dialog := setupDialog(t, dashboard)
	values, current := setupOpenChoices(t, dialog)
	target := slices.IndexFunc(values, func(value any) bool { return setupValueEqual(value, wanted) })
	if target < 0 {
		t.Fatalf("%s is not offered: %s", setupValueDisplay(wanted), setupValueDisplay(values))
	}
	step := rune(tea.KeyDown)
	if current > target {
		step = tea.KeyUp
	}
	for range max(current-target, target-current) {
		dashboard.HandleKey(key(step))
	}
	dashboard.HandleKey(key(tea.KeyEnter))
}

func setupOpenChoices(t testing.TB, dialog *SetupDialog) ([]any, int) {
	t.Helper()
	id, ok := dialog.combo.OpenID()
	if !ok {
		t.Fatal("dropdown open")
	}
	control, _ := dialog.descriptor(id)
	choices, ok := control.kind.(SettingKindChoices)
	if !ok {
		t.Fatal("dropdown choices")
	}
	return choices.values, dialog.combo.Selection(id, dialog.choicePosition(control.path, choices.values))
}

func setupFocusedField(t testing.TB, dialog *SetupDialog) *PendingField {
	t.Helper()
	path, ok := dialog.focusedFieldPath()
	if !ok {
		t.Fatal("focused field")
	}
	field, ok := dialog.fields.get(path)
	if !ok {
		t.Fatal("field buffer")
	}
	return field
}

func setupFocusSetting(dialog *SetupDialog, name string) {
	dialog.prepare()
	dialog.focusPath(append(slices.Clone(dialog.path), name))
}

func setupReplaceText(dashboard *DashboardState, text string) {
	dashboard.HandleKey(setupCtrl('a'))
	dashboard.HandleKey(setupCtrl('k'))
	dashboard.HandlePaste(text)
	if text == "" {
		dashboard.HandleKey(key(tea.KeyBackspace))
	}
}

// setupSearch opens the settings search and types query into it.
func setupSearch(dashboard *DashboardState, query string) {
	dashboard.HandleKey(key('/'))
	if query != "" {
		dashboard.HandlePaste(query)
	}
}

func setupSearchState(t testing.TB, dashboard *DashboardState) *SearchState {
	t.Helper()
	search := setupDialog(t, dashboard).search
	if search == nil {
		t.Fatal("an open search")
	}
	return search
}

func setupResultPaths(t testing.TB, dashboard *DashboardState) [][]string {
	t.Helper()
	search := setupSearchState(t, dashboard)
	var paths [][]string
	for _, index := range search.matches {
		if index >= 0 && index < len(search.entries) {
			paths = append(paths, slices.Clone(search.entries[index].path))
		}
	}
	return paths
}

// setupDebugPaths is Rust's Debug of a Vec<Vec<String>>.
func setupDebugPaths(paths [][]string) string {
	parts := make([]string, len(paths))
	for i, path := range paths {
		parts[i] = setupDebugStrings(path)
	}
	return "[" + strings.Join(parts, ", ") + "]"
}

func setupCtrlSpace() tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: ' ', Mod: tea.ModCtrl}
}

// setupEditField opens the field on the page section and types text into
// it.
func setupEditField(t testing.TB, dialog *SetupDialog, section, field, text string) {
	t.Helper()
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.mode = ModeSetup{dialog}
	for {
		setup := setupDialog(t, dashboard)
		setup.saving = false
		if len(setup.path) == 0 {
			break
		}
		dashboard.HandleKey(key(tea.KeyEscape))
	}
	setupChoose(t, dashboard, section)
	setupChoose(t, dashboard, field)
	setupReplaceText(dashboard, text)
	updated := setupDialog(t, dashboard)
	if updated != dialog {
		*dialog = *updated
	}
}

func setupApplyFocused(t testing.TB, dialog *SetupDialog, clear bool) error {
	t.Helper()
	path, ok := dialog.focusedPath()
	if !ok {
		t.Fatal("focused field")
	}
	if clear {
		setupFocusedField(t, dialog).input.text().Clear()
	}
	return dialog.applyPending(path)
}

func setupSavedConfig(t testing.TB, dialog *SetupDialog) core.Config {
	t.Helper()
	action := dialog.save(&core.ProfileCapabilitiesSnapshot{})
	dialog.saving = false
	save, ok := action.(DashboardActionSaveSetup)
	if !ok {
		notice := "None"
		if dialog.notice != nil {
			notice = *dialog.notice
		}
		t.Fatalf("save refused: %#v, notice %s", action, notice)
	}
	var config core.Config
	if err := json.Unmarshal([]byte(save.Updated), &config); err != nil {
		t.Fatalf("saved configuration: %v", err)
	}
	return config
}

// setupActionDebug is Rust's Debug of a DashboardAction, for the variants
// the Settings goldens print.
func setupActionDebug(action DashboardAction) string {
	if setupActionIsNone(action) {
		return "None"
	}
	return fmt.Sprintf("%#v", action)
}

// setupControlDebug is Rust's Debug of an Option<SetupControl>.
func setupControlDebug(control SetupControl, ok bool) string {
	if !ok {
		return "None"
	}
	names := map[setupControlKind]string{
		setupControlKindField: "Field", setupControlKindSearch: "Search", setupControlKindResults: "Results",
		setupControlKindOpenSearch: "OpenSearch", setupControlKindBack: "Back", setupControlKindAdd: "Add",
		setupControlKindRemove: "Remove", setupControlKindApply: "Apply", setupControlKindDetectProfiles: "DetectProfiles",
		setupControlKindDetectRuntimes: "DetectRuntimes", setupControlKindDetectionResults: "DetectionResults",
		setupControlKindOpenMachines: "OpenMachines", setupControlKindManagedSetup: "ManagedSetup",
		setupControlKindManagedContinue: "ManagedContinue", setupControlKindSignIn: "SignIn",
		setupControlKindInstallMbx: "InstallMbx", setupControlKindSave: "Save", setupControlKindCancel: "Cancel",
	}
	if row, isRow := control.IsRow(); isRow {
		return fmt.Sprintf("Some(Row(%d))", row)
	}
	return "Some(" + names[control.kind] + ")"
}

// setupJSON is serde_json::json! for a test value.
func setupJSON(t testing.TB, text string) any {
	t.Helper()
	value, err := jsonvalue.Parse([]byte(text))
	if err != nil {
		t.Fatalf("parse %s: %v", text, err)
	}
	return value
}
