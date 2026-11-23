// Port of mj-tui/src/setup/tests/navigation.rs.

package tui

import (
	"fmt"
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestOneClickTogglesARealCheckboxAndArrowsOnlyMoveFocus(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.beginSettingsSection("profiles", nil)
	setupChoose(t, dashboard, "codex-1")
	for _, expected := range []bool{false, true} {
		lines := drawn(dashboard, 140, 40)
		setupClickAt(dashboard, point(lines, "Enabled"), 140, 40)
		dialog := setupDialog(t, dashboard)
		if got := setupValuePath(dialog.draft, "profiles", "codex-1", "enabled"); !setupValueEqual(got, expected) {
			t.Fatalf("enabled = %s, want %t", setupValueDisplay(got), expected)
		}
		if dialog.editor != nil {
			t.Fatal("a checkbox click opened an editor")
		}
	}
	dashboard.HandleKey(key(tea.KeyDown))
	dashboard.HandleKey(key(tea.KeyUp))
	dialog := setupDialog(t, dashboard)
	if got := setupValuePath(dialog.draft, "profiles", "codex-1", "enabled"); !setupValueEqual(got, true) {
		t.Fatalf("arrows changed the checkbox: %s", setupValueDisplay(got))
	}
}

// A row whose value is picked from a list opens its dropdown on one click,
// and a page button navigates on that same single-click interaction.
func TestOneClickOpensAPageAndARealDropdown(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupClick(dashboard, "Interface")
	if dialog := setupDialog(t, dashboard); !slices.Equal(dialog.path, []string{"interface"}) {
		t.Fatalf("one click opens a page: %v", dialog.path)
	}

	for _, test := range []struct {
		section      string
		openChildren []string
		label        string
		expectedPath []string
	}{
		{"interface", nil, "Theme", []string{"interface", "theme"}},
		{"subagents", []string{"straightforward"}, "opus", []string{"subagents", "straightforward", "0", "model"}},
	} {
		dashboard := dashboardWithSession(stoppedSession())
		if test.section == "subagents" {
			opus := "opus"
			choice := core.SessionConfigChoice{Value: "opus", Name: "opus"}
			profiles := map[string]core.ProfileCapabilities{}
			for id, profile := range dashboard.config.Get().EnabledProfiles() {
				profiles[profile.CapabilitiesKey(id)] = core.ProfileCapabilities{
					Choices: core.CapabilityState[core.ProfileConfig]{
						Kind: core.CapabilityStateReady,
						Value: core.ProfileConfig{
							Model:      &opus,
							Models:     []core.SessionConfigChoice{choice},
							Efforts:    []core.SessionConfigChoice{},
							ObservedAt: 1,
						},
					},
					Efforts: map[string]core.CapabilityState[[]core.SessionConfigChoice]{},
				}
			}
			dashboard.SetProfileCapabilities(core.ProfileCapabilitiesSnapshot{Profiles: profiles})
			dashboard.config.Mut().Subagents.Straightforward = []core.SubagentChoice{{Model: "opus"}}
		}
		dashboard.beginSettingsSection(test.section, nil)
		for _, child := range test.openChildren {
			setupChoose(t, dashboard, child)
		}
		lines := drawn(dashboard, 140, 40)
		row := point(lines, test.label).Y
		column := cellColumn(lines[row], "▾")
		setupClickAt(dashboard, uv.Pos(column, row), 140, 40)
		dialog := setupDialog(t, dashboard)
		openID, open := dialog.combo.OpenID()
		if !open {
			t.Fatalf("%s: one click opens the dropdown", test.section)
		}
		control, _ := dialog.descriptor(openID)
		if !slices.Equal(control.path, test.expectedPath) {
			t.Fatalf("the dropdown of %v opened, want %v", control.path, test.expectedPath)
		}
	}
}

func TestChoicePopupEscapePreservesTheDraftAndBackgroundClickIsInert(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "interface")
	setupChoose(t, dashboard, "theme")
	dashboard.HandleKey(key(tea.KeyDown))
	buffer := setupRenderBuffer(dashboard, 100, 30)

	// A click away from the popup must not activate the inert mirror of the
	// page's stacked controls or commit the pending choice.
	row, column := -1, -1
	for index, line := range bufferLines(buffer) {
		if at := strings.Index(line, "Save and Close"); at >= 0 {
			row, column = index, at
			break
		}
	}
	if row < 0 {
		t.Fatal("mirrored Save row")
	}
	background := uv.Pos(column+1, row)
	for _, kind := range []tea.MouseMsg{setupMouseDown, setupMouseUp} {
		dashboard.HandleEventResult(mouseAt(kind, background))
	}
	dialog := setupDialog(t, dashboard)
	if _, open := dialog.combo.OpenID(); !open {
		t.Fatal("a background click closed the popup")
	}
	if theme := setupValueGet(dialog.draft, "theme"); !setupValueEqual(theme, "midnight") {
		t.Fatalf("theme = %s", setupValueDisplay(theme))
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	dialog = setupDialog(t, dashboard)
	if _, open := dialog.combo.OpenID(); open {
		t.Fatal("escape closes the popup")
	}
	if theme := setupValueGet(dialog.draft, "theme"); !setupValueEqual(theme, "midnight") {
		t.Fatalf("theme = %s", setupValueDisplay(theme))
	}

	// Reopen it for the pointer-commit part of the behavior.
	setupChoose(t, dashboard, "theme")
	buffer = setupRenderBuffer(dashboard, 100, 30)

	// Find one popup content cell from the rendered form and click it.
	row, column = -1, -1
	for index, line := range bufferLines(buffer) {
		if at := strings.Index(line, "Light"); at >= 0 {
			row, column = index, at
			break
		}
	}
	if row < 0 {
		t.Fatal("Light popup row")
	}
	light := uv.Pos(column, row)
	if !setupDialog(t, dashboard).form.Contains(light.X, light.Y) {
		t.Fatal("the popup row is outside the form")
	}
	dashboard.HandleEventResult(mouseAt(setupMouseDown, light))
	dashboard.HandleEventResult(mouseAt(setupMouseUp, light))
	dialog = setupDialog(t, dashboard)
	if _, open := dialog.combo.OpenID(); open {
		t.Fatal("click commits the popup choice")
	}
	if theme := setupValueGet(dialog.draft, "theme"); !setupValueEqual(theme, "light") {
		t.Fatalf("theme = %s", setupValueDisplay(theme))
	}
}

func TestCancellingOrFailingToSaveAThemeKeepsTheActiveColors(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	original := *dashboard.config.Get()
	setupChooseLightTheme(t, dashboard)
	dashboard.HandleKey(key(tea.KeyEscape))
	if !dashboard.ModalOpen() {
		t.Fatal("Esc on the root of a dirty draft asks first")
	}
	dashboard.HandleKey(key(tea.KeyRight))
	if action := dashboard.HandleKey(key(tea.KeyEnter)); !setupActionIsNone(action) {
		t.Fatalf("discarding returned %#v", action)
	}
	if dashboard.ModalOpen() {
		t.Fatal("discarding closes Settings")
	}
	if !setupConfigEqual(*dashboard.config.Get(), original) {
		t.Fatal("discarding changed the configuration")
	}
	setupAssertRenderedTheme(t, dashboard, core.UiThemeMidnight)

	setupChooseLightTheme(t, dashboard)
	action := dashboard.HandleKey(setupCtrl('s'))
	save, ok := action.(DashboardActionSaveSetup)
	if !ok {
		t.Fatalf("%#v", action)
	}
	dashboard.SetupSaved(save.Generation, core.Config{}, fmt.Errorf("disk full"))
	if !setupConfigEqual(*dashboard.config.Get(), original) {
		t.Fatal("a failed save changed the configuration")
	}
	setupAssertRenderedTheme(t, dashboard, core.UiThemeMidnight)
	dialog := setupDialog(t, dashboard)
	if theme := setupValueGet(dialog.draft, "theme"); !setupValueEqual(theme, "light") {
		t.Fatalf("theme = %s", setupValueDisplay(theme))
	}
	if dialog.notice == nil || !strings.Contains(*dialog.notice, "disk full") {
		t.Fatalf("notice = %v", dialog.notice)
	}
}

func TestBackspaceAtTheRootWithADirtyDraftAsksBeforeDiscarding(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	original := *dashboard.config.Get()
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "phone")
	setupChoose(t, dashboard, "enabled")
	dashboard.HandleKey(key(tea.KeyBackspace))
	dashboard.HandleKey(key(tea.KeyBackspace))
	if _, ok := dashboard.mode.(ModeConfirm); !ok {
		t.Fatalf("dirty settings must confirm before closing: %t", dashboard.ModalOpen())
	}
	// Esc keeps editing with the draft intact.
	dashboard.HandleKey(key(tea.KeyEscape))
	setup, ok := dashboard.mode.(ModeSetup)
	if !ok {
		t.Fatal("settings restored")
	}
	if !setup.isDirty() {
		t.Fatal("the draft lost its edit")
	}
	if !setupConfigEqual(*dashboard.config.Get(), original) {
		t.Fatal("the configuration changed")
	}
}

func TestCancellingSetupPreservesConfigurationAndRenderKeepsControlsVisible(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	original := *dashboard.config.Get()
	for _, size := range [][2]int{{80, 18}, {100, 30}, {140, 42}} {
		dashboard.BeginSetup()
		setupChoose(t, dashboard, "phone")
		setupChoose(t, dashboard, "enabled")
		dashboard.HandleKey(key(tea.KeyBackspace))
		setupChoose(t, dashboard, "targets")
		lines := bufferLines(setupRenderBuffer(dashboard, size[0], size[1]))
		text := strings.Join(lines, "\n")
		if !strings.Contains(text, "Settings › Runtimes") {
			t.Fatal(text)
		}
		// Every page action keeps its own row in one column at the dialog's
		// right edge, on top of each other rather than spread along a row.
		labels := []string{"Add", "Detect runtimes", "Machines"}
		width := 0
		for _, label := range labels {
			width = max(width, len(label))
		}
		type placed struct {
			row, column int
			label       string
		}
		var rows []placed
		for _, label := range labels {
			// The button's padding separates it from prose that happens to
			// use the same word, such as the page's help line.
			padded := "  " + label + "  "
			row := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, padded) })
			if row < 0 {
				t.Fatalf("missing %q in\n%s", label, text)
			}
			line := lines[row]
			// Every button is as wide as the longest of them, so a shorter
			// label is followed by its share of that width, the button's
			// padding, the inner margin, and then the modal border.
			after := line[strings.Index(line, padded)+2+len(label):]
			gap := strings.Repeat(" ", 3+width-len(label)) + "│"
			if !strings.HasPrefix(after, gap) {
				t.Fatalf("%s is not packed against the dialog's right edge: %s", label, line)
			}
			rows = append(rows, placed{row: row, column: cellColumn(line, padded) + 2, label: label})
		}
		for i := 1; i < len(rows); i++ {
			if rows[i-1].row >= rows[i].row {
				t.Fatalf("the buttons are not stacked in order: %v\n%s", rows, text)
			}
			if rows[i].column != rows[0].column {
				t.Fatalf("the stacked buttons do not share a column: %v\n%s", rows, text)
			}
			if rows[i-1].row+1 != rows[i].row {
				t.Fatalf("the stacked buttons leave gaps between them: %v\n%s", rows, text)
			}
		}
		// Back and the commit share the footer row below the column.
		back := point(lines, "  Back  ")
		save := point(lines, "  Save and Close  ")
		if back.Y != save.Y || back.X >= save.X {
			t.Fatal(text)
		}
		for _, placed := range rows {
			if placed.row >= back.Y {
				t.Fatalf("the column overlaps the footer row: %v\n%s", rows, text)
			}
		}
		if strings.Contains(text, "Cancel") {
			t.Fatal(text)
		}
		// Backspace from the root dismisses through the dirty guard.
		dashboard.HandleKey(key(tea.KeyBackspace))
		dashboard.HandleKey(key(tea.KeyBackspace))
		if _, ok := dashboard.mode.(ModeConfirm); !ok {
			t.Fatal(text)
		}
		dashboard.HandleKey(key(tea.KeyRight))
		if action := dashboard.HandleKey(key(tea.KeyEnter)); !setupActionIsNone(action) {
			t.Fatalf("discarding returned %#v", action)
		}
		if dashboard.ModalOpen() {
			t.Fatal("discarding closes Settings")
		}
		if !setupConfigEqual(*dashboard.config.Get(), original) {
			t.Fatal("discarding changed the configuration")
		}
	}
}

// The Runtimes page's Machines action opens the Machines page with the
// unsaved draft intact, and Back returns to the settings root (#1282).
func TestRuntimesPageJumpsToMachinesKeepingTheDraft(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.beginSettingsSection("targets", nil)
	dialog := setupDialog(t, dashboard)
	// An unsaved edit anywhere in the draft must survive the jump.
	dialog.togglePath([]string{"profiles", "codex-1", "enabled"})
	if !dialog.isDirty() {
		t.Fatal("the toggle is an edit")
	}
	dialog.form.Focus(SetupControlOpenMachines)
	dashboard.HandleKey(key(tea.KeyEnter))
	dialog = setupDialog(t, dashboard)
	if !slices.Equal(dialog.path, []string{"machines"}) {
		t.Fatalf("the action opens the Machines page: %v", dialog.path)
	}
	if !dialog.isDirty() {
		t.Fatal("the unsaved draft survives the jump")
	}
	dashboard.HandleKey(key(tea.KeyBackspace))
	dialog = setupDialog(t, dashboard)
	if len(dialog.path) != 0 {
		t.Fatalf("Back from Machines returns to the settings root: %v", dialog.path)
	}
	if !dialog.isDirty() {
		t.Fatal("the draft still holds the edit")
	}
}

// The border promises `Esc back`, and below the first page that is what it
// must do: return to the parent with the draft intact.
//
// Hard-won: 0cc7c1e6: Escape on a subpage discarded the entire settings draft.
func TestEscapeReturnsFromASettingsSubpageAndClosesOnlyFromTheFirstPage(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "machines")
	setupChoose(t, dashboard, "local")
	dashboard.HandleKey(key(tea.KeyEscape))
	setup, ok := dashboard.mode.(ModeSetup)
	if !ok {
		t.Fatal("Esc on a sub-page must keep Settings open")
	}
	if !slices.Equal(setup.path, []string{"machines"}) {
		t.Fatalf("path = %v", setup.path)
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	setup, ok = dashboard.mode.(ModeSetup)
	if !ok {
		t.Fatal("Esc on the Machines page must return to the first page")
	}
	if len(setup.path) != 0 {
		t.Fatalf("path = %v", setup.path)
	}

	// A sub-page edit survives the way back out.
	setupChoose(t, dashboard, "phone")
	setupChoose(t, dashboard, "enabled")
	dashboard.HandleKey(key(tea.KeyEscape))
	setup, ok = dashboard.mode.(ModeSetup)
	if !ok {
		t.Fatal("Esc must not discard the draft from a sub-page")
	}
	if len(setup.path) != 0 {
		t.Fatalf("path = %v", setup.path)
	}
	if enabled := setupValuePath(setup.draft, "phone", "enabled"); !setupValueEqual(enabled, false) {
		t.Fatalf("phone.enabled = %s", setupValueDisplay(enabled))
	}
	if !setup.isDirty() {
		t.Fatal("the draft lost its edit")
	}

	// From the first page Esc closes the dialog, through the discard guard.
	dashboard.HandleKey(key(tea.KeyEscape))
	if _, ok := dashboard.mode.(ModeConfirm); !ok {
		t.Fatal("a dirty draft asks before closing")
	}
}

// A status line belongs to the page or prompt that set it. Finding T-4.
//
// Hard-won: 6094bb1c: a prompt status line remained after its owning settings page closed.
func TestAStatusLineDoesNotOutliveThePromptThatSetIt(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.beginSettingsSection("profiles", nil)
	dashboard.HandleKey(key('a'))
	dashboard.HandleKey(key(tea.KeyEnter))
	shown := setupDialog(t, dashboard).notice
	if shown == nil || *shown != "Choose a name for the new entry." {
		t.Fatalf("notice = %v", shown)
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	setupChoose(t, dashboard, "codex-1")
	setupChoose(t, dashboard, "environment")
	if notice := setupDialog(t, dashboard).notice; notice != nil {
		t.Fatalf("the name prompt's status lingered: %q", *notice)
	}
	page := strings.Join(drawn(dashboard, 140, 40), "\n")
	if strings.Contains(page, "Choose a name") {
		t.Fatal(page)
	}
}

// Rust checks the cursor show and hide sequences the terminal backend
// writes; the Go frame carries the cursor position instead, which is what
// the program shows or hides.
func TestSettingsOwnsTheTerminalCursorAndRestoresTheComposerAfterDismissal(t *testing.T) {
	session := runningSession()
	id := session.ID
	dashboard := dashboardWithSession(session)
	dashboard.FocusPrompt()
	fixture := client.NewReplacementSessionTestFixture(id, 1)
	active := chat.ActiveChatOpen(fixture.Stopped, "hel", nil, fixture.Control, chat.SessionHeaderIdentity{}, "", chat.NewNotices())
	chats := map[string]*chat.ActiveChat{id: active}
	cursorShown := func() bool {
		frame := chat.NewFrame(uv.NewBuffer(140, 40))
		renderCombinedForTest(frame, dashboard, chats, map[PaneId]string{}, false)
		_, shown := frame.CursorPosition()
		return shown
	}
	if !cursorShown() {
		t.Fatal("composer owns the initial cursor")
	}
	dashboard.beginSettingsSection("notify", nil)
	if cursorShown() {
		t.Fatal("settings controls cannot inherit the background cursor")
	}
	setupChoose(t, dashboard, "delay_seconds")
	if !cursorShown() {
		t.Fatal("focused modal text field owns its cursor")
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	if cursorShown() {
		t.Fatal("closing the editor hides its cursor")
	}
	if setup, ok := dashboard.mode.(ModeSetup); !ok || len(setup.path) != 0 {
		t.Fatal("Esc returns to the Settings root")
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	if !cursorShown() {
		t.Fatal("dismissal restores composer cursor")
	}
}

// Hard-won: Settings review: input preparation discarded the rendered viewport.
func TestWheelScrollsSettingsBodyWithoutMovingFocus(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	before := drawn(dashboard, 100, 20)
	profiles := point(before, "Profiles")
	focus, focusOK := setupDialog(t, dashboard).focusedPath()
	for range 5 {
		dashboard.HandleMouse(setupWheel(true, profiles))
		drawn(dashboard, 100, 20)
	}
	after := drawn(dashboard, 100, 20)
	if slices.Equal(after, before) {
		t.Fatal("wheel must move the scrolling body")
	}
	if slices.ContainsFunc(after, func(line string) bool { return strings.Contains(line, "Agent Profiles") }) {
		t.Fatalf("the first page row must scroll out of view: %q", after)
	}
	if path, ok := setupDialog(t, dashboard).focusedPath(); ok != focusOK || !slices.Equal(path, focus) {
		t.Fatalf("focus moved from %v to %v", focus, path)
	}
}
