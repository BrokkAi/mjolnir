// Port of mj-tui/src/setup/tests/settings.rs.

package tui

import (
	"encoding/json"
	"fmt"
	"maps"
	"os"
	"os/exec"
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Workers read the Jev switch when they start, so a running session keeps
// classifying until it is resumed or restarted. The page and the save notice
// say so instead of "Off: nothing is sent". Launch re-verification finding
// R3-8.
//
// Hard-won: 8dccf5db: the save notice omitted when running sessions would follow the jev change.
func TestTurningJevOffSaysRunningSessionsFollowAfterAResumeOrRestart(t *testing.T) {
	jev := client.Help([]string{"jev"})
	if strings.Contains(jev, "nothing is sent") || !strings.Contains(jev, "resume or restart") {
		t.Fatal(jev)
	}

	dashboard := dashboardWithSession(stoppedSession())
	dashboard.beginSettingsSection("jev", nil)
	dialog := setupDialog(t, dashboard)
	setupValueInsert(&dialog.draft, []string{"jev"}, "enabled", false)
	generation := dialog.generation
	saved := setupSavedConfig(t, dialog)
	if saved.Jev.Enabled {
		t.Fatal("Jev is still on")
	}
	dashboard.SetupSaved(generation, saved, nil)
	notice, _ := dashboard.Notice()
	if !strings.Contains(notice, "running sessions follow it after their next resume or restart") {
		t.Fatal(notice)
	}

	// A save that leaves Jev alone does not mention it.
	dashboard = dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	dialog = setupDialog(t, dashboard)
	generation = dialog.generation
	saved = setupSavedConfig(t, dialog)
	dashboard.SetupSaved(generation, saved, nil)
	if notice, _ := dashboard.Notice(); strings.Contains(notice, "Jev") {
		t.Fatal(notice)
	}
}

// Launch campaign finding A-3: once `ascii` was chosen, the popup offered no
// way back to "Follows the terminal", and saving that choice must remove the
// key rather than write a third value.
//
// Hard-won: 688cfd2a: the ASCII choice could not be cleared back to terminal-following behavior.
func TestSymbolsCanReturnToTheUnsetStateAndSavingRemovesTheKey(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	configuration := *dashboard.config.Get()
	symbols := core.SymbolSetAscii
	configuration.Advanced.Symbols = &symbols
	dashboard.SetConfig(configuration)
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "advanced")
	setupChoose(t, dashboard, "symbols")
	popup := strings.Join(bufferLines(setupRenderBuffer(dashboard, 100, 30)), "\n")
	if !strings.Contains(popup, "Follows the terminal") || !strings.Contains(popup, "unicode") || !strings.Contains(popup, "ascii") {
		t.Fatal(popup)
	}

	dashboard.HandleKey(key(tea.KeyUp))
	dashboard.HandleKey(key(tea.KeyUp))
	dashboard.HandleKey(key(tea.KeyEnter))
	if value := setupValuePath(setupDialog(t, dashboard).draft, "advanced", "symbols"); value != nil {
		t.Fatalf("symbols = %s", setupValueDisplay(value))
	}
	action := dashboard.HandleKey(setupCtrl('s'))
	save, ok := action.(DashboardActionSaveSetup)
	if !ok {
		t.Fatalf("expected Settings save, got %#v", action)
	}
	var saved core.Config
	if err := json.Unmarshal([]byte(save.Updated), &saved); err != nil {
		t.Fatal(err)
	}
	if saved.Advanced.Symbols != nil {
		t.Fatalf("symbols = %s", *saved.Advanced.Symbols)
	}
	if strings.Contains(save.Updated, "symbols") {
		t.Fatal(save.Updated)
	}
}

// The first page describes the draft, not the saved file, and a limit put
// back to its default still shows the limit. Launch campaign finding C-22.
//
// Hard-won: e5bdf191: the first-page summary read saved values instead of the active draft.
func TestTheFirstPageSummarizesDraftedValuesBeforeTheyAreSaved(t *testing.T) {
	configuration := config()
	dialog := newSetupDialog(&configuration)
	setupEditField(t, dialog, "sessionwiki", "archive_after_days", "30")
	if err := setupApplyFocused(t, dialog, false); err != nil {
		t.Fatal(err)
	}
	setupEditField(t, dialog, "subagents", "max_concurrent", "4")
	if err := setupApplyFocused(t, dialog, false); err != nil {
		t.Fatal(err)
	}
	summary := func(key string) string {
		return rowSummary(nil, key, setupValueGet(dialog.draft, key), dialog.draft, nil)
	}
	if got := summary("sessionwiki"); got != "Archives after 30 days" {
		t.Fatal(got)
	}
	if got := summary("subagents"); got != "Up to 4 at once" {
		t.Fatal(got)
	}

	setupEditField(t, dialog, "subagents", "max_concurrent", "")
	if err := setupApplyFocused(t, dialog, true); err != nil {
		t.Fatal(err)
	}
	if got := summary("subagents"); got != "Up to 8 at once" {
		t.Fatal(got)
	}
}

// A long save error is shown whole: the notice grows to fit it instead of
// stopping after three lines. Launch campaign finding C-20.
//
// Hard-won: 006bb14c: save notices were truncated after three rows.
func TestALongSaveErrorIsShownWhole(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	notice := fmt.Sprintf("Could not save: %s closing words",
		strings.Repeat("Setup would change the project that a running session uses. ", 5))
	setupDialog(t, dashboard).notice = &notice
	lines := drawn(dashboard, 120, 40)
	if !slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, "closing words") }) {
		t.Fatal(strings.Join(lines, "\n"))
	}
	// The footer's controls are still drawn below it.
	if !slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, "Save") }) {
		t.Fatal(strings.Join(lines, "\n"))
	}
}

// Hard-won: #1153: saving unrelated Settings fields overwrote named-instance web listener ports.
func TestNamedInstanceSettingsPreserveTheDefaultAndExplicitWebListener(t *testing.T) {
	const child = "MJ_TEST_SETTINGS_INSTANCE_CHILD"
	const instance = "settings-1153"
	if _, ok := os.LookupEnv(child); !ok {
		// Instance selection is process-wide: exercise a named instance in
		// its own process rather than changing the other tests' environment.
		command := exec.Command(os.Args[0], "-test.run", "^TestNamedInstanceSettingsPreserveTheDefaultAndExplicitWebListener$", "-test.v")
		command.Env = append(os.Environ(), child+"=1", "MJ_INSTANCE="+instance)
		output, err := core.RunWithInput(command, nil)
		if err != nil {
			t.Fatal(err)
		}
		if !output.Status.Success() {
			t.Fatalf("%s\n%s", output.Stdout, output.Stderr)
		}
		return
	}
	name := instance
	defaulted := core.DefaultPhoneBindFor(&name)
	for _, bind := range []string{defaulted, "127.0.0.1:39777"} {
		dashboard := dashboardWithSession(stoppedSession())
		phone := core.DefaultPhoneConfig()
		phone.Bind = bind
		dashboard.config.Mut().Phone = phone
		if bind == defaulted {
			stored, err := setupValueTo(*dashboard.config.Get())
			if err != nil {
				t.Fatal(err)
			}
			if setupValueHas(stored, "phone") {
				t.Fatal("the default listener is stored")
			}
		}
		dashboard.BeginSetup()
		rendered := strings.Join(drawn(dashboard, 140, 48), "\n")
		if !strings.Contains(rendered, "On · "+bind) {
			t.Fatal(rendered)
		}
		dialog := setupDialog(t, dashboard)
		if dialog.isDirty() {
			t.Fatal("opening Settings must preserve defaults")
		}
		setupValueInsert(&dialog.draft, []string{"keys"}, "prefix", "ctrl+]")
		saved := setupSavedConfig(t, dialog)
		if saved.Phone.Bind != bind || saved.Keys.Prefix != "ctrl+]" {
			t.Fatalf("bind %q, prefix %q", saved.Phone.Bind, saved.Keys.Prefix)
		}
	}
}

// setupCapabilities is every enabled profile of dashboard offering the
// models fast and smart, with efforts per model.
func setupCapabilities(dashboard *DashboardState, efforts map[string][]string) core.ProfileCapabilitiesSnapshot {
	choice := func(value string) core.SessionConfigChoice {
		return core.SessionConfigChoice{Value: value, Name: value}
	}
	fast := "fast"
	profiles := map[string]core.ProfileCapabilities{}
	for id, profile := range dashboard.config.Get().EnabledProfiles() {
		byModel := map[string]core.CapabilityState[[]core.SessionConfigChoice]{}
		for model, values := range efforts {
			choices := make([]core.SessionConfigChoice, 0, len(values))
			for _, value := range values {
				choices = append(choices, choice(value))
			}
			byModel[model] = core.CapabilityState[[]core.SessionConfigChoice]{Kind: core.CapabilityStateReady, Value: choices}
		}
		profiles[profile.CapabilitiesKey(id)] = core.ProfileCapabilities{
			Choices: core.CapabilityState[core.ProfileConfig]{
				Kind: core.CapabilityStateReady,
				Value: core.ProfileConfig{
					Model:      &fast,
					Models:     []core.SessionConfigChoice{choice("fast"), choice("smart")},
					Efforts:    []core.SessionConfigChoice{},
					ObservedAt: 1,
				},
			},
			Efforts: byModel,
		}
	}
	return core.ProfileCapabilitiesSnapshot{Profiles: profiles}
}

func TestSubagentsSettingsReusesCachedEffortsAcrossReopensAndModelSwitches(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.SetProfileCapabilities(setupCapabilities(dashboard, map[string][]string{
		"fast":  {"low"},
		"smart": {"high", "max"},
	}))

	// Opening the subagents dialog populates options from the process cache
	// immediately.
	dashboard.config.Mut().Subagents.Straightforward = []core.SubagentChoice{{Model: "fast"}}
	dashboard.beginSettingsSection("subagents", nil)
	if action := dashboard.takeProfileHydration(); action != nil {
		t.Fatalf("hydration = %#v", action)
	}
	setupChoose(t, dashboard, "straightforward")
	modelPath := []string{"subagents", "straightforward", "0", "model"}
	setupDialog(t, dashboard).focusPath(modelPath)
	dashboard.HandleKey(key(tea.KeyEnter))
	setupSelectChoice(t, dashboard, "smart")
	if action := dashboard.takeProfileHydration(); action != nil {
		t.Fatalf("hydration = %#v", action)
	}

	effortPath := []string{"subagents", "straightforward", "0", "effort"}
	setupDialog(t, dashboard).focusPath(effortPath)
	dashboard.HandleKey(key(tea.KeyEnter))
	efforts, _ := setupOpenChoices(t, setupDialog(t, dashboard))
	if !setupValueEqual(efforts, []any{nil, "high", "max"}) {
		t.Fatalf("efforts = %s", setupValueDisplay(efforts))
	}

	// Closing and reopening preserves the cached model and effort options.
	for range 3 {
		dashboard.HandleKey(key(tea.KeyEscape))
	}
	dashboard.beginSettingsSection("subagents", nil)
	if action := dashboard.takeProfileHydration(); action != nil {
		t.Fatalf("hydration = %#v", action)
	}
	setupChoose(t, dashboard, "straightforward")
	setupDialog(t, dashboard).focusPath(modelPath)
	dashboard.HandleKey(key(tea.KeyEnter))
	models, _ := setupOpenChoices(t, setupDialog(t, dashboard))
	if !setupValueEqual(models, []any{"fast", "smart"}) {
		t.Fatalf("models = %s", setupValueDisplay(models))
	}
}

func TestSubagentSinglePageRejectsPartlyConfiguredRowsAndHandlesRemoveAndNarrowLayout(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.SetProfileCapabilities(setupCapabilities(dashboard, map[string][]string{
		"fast":  {"low"},
		"smart": {"high"},
	}))
	dashboard.beginSettingsSection("subagents", nil)
	setupChoose(t, dashboard, "straightforward")
	category := []string{"subagents", "straightforward"}
	if path := setupDialog(t, dashboard).path; !slices.Equal(path, category) {
		t.Fatalf("path = %v", path)
	}
	model0 := []string{"subagents", "straightforward", "0", "model"}

	// Add stays on the category page, marks the draft dirty, and focuses row
	// 0 model.
	dashboard.HandleKey(key('a'))
	dialog := setupDialog(t, dashboard)
	if !slices.Equal(dialog.path, category) {
		t.Fatalf("path = %v", dialog.path)
	}
	if !dialog.isDirty() {
		t.Fatal("incomplete row must count as dirty")
	}
	if path, _ := dialog.focusedPath(); !slices.Equal(path, model0) {
		t.Fatalf("focused %v", path)
	}

	// Esc, Backspace, Back button, and Ctrl+S all reject the partly
	// configured row.
	for _, trigger := range []tea.KeyPressMsg{key(tea.KeyEscape), key(tea.KeyBackspace), setupCtrl('s')} {
		if action := dashboard.HandleKey(trigger); !setupActionIsNone(action) {
			t.Fatalf("%v returned %#v", trigger, action)
		}
		dialog := setupDialog(t, dashboard)
		if !slices.Equal(dialog.path, category) {
			t.Fatalf("path = %v", dialog.path)
		}
		if dialog.notice == nil || !strings.Contains(*dialog.notice, "Straightforward entry 1") {
			t.Fatalf("expected incomplete-row notice, got %v", dialog.notice)
		}
	}

	// A background capability refresh with a discovery error must not
	// clobber the active notice.
	unreachable := maps.Clone(dashboard.profileCapabilities.Profiles)
	for identity, capabilities := range unreachable {
		capabilities.Choices = core.CapabilityState[core.ProfileConfig]{Kind: core.CapabilityStateFailed, Error: "worker unavailable"}
		unreachable[identity] = capabilities
	}
	saved := dashboard.profileCapabilities
	dashboard.SetProfileCapabilities(core.ProfileCapabilitiesSnapshot{Profiles: maps.Clone(unreachable)})
	dialog = setupDialog(t, dashboard)
	if dialog.notice == nil || !strings.Contains(*dialog.notice, "Straightforward entry 1") {
		t.Fatalf("background discovery error overwrote active validation notice: %v", dialog.notice)
	}
	// Existing discovery notices still refresh to new failure details and
	// clear on recovery.
	pending := "Discovery: pending"
	dialog.notice = &pending
	dashboard.SetProfileCapabilities(core.ProfileCapabilitiesSnapshot{Profiles: unreachable})
	if notice := setupDialog(t, dashboard).notice; notice == nil || !strings.Contains(*notice, "worker unavailable") {
		t.Fatalf("notice = %v", notice)
	}
	dashboard.SetProfileCapabilities(saved)
	if notice := setupDialog(t, dashboard).notice; notice != nil {
		t.Fatalf("notice = %q", *notice)
	}

	// Jumping away via search and trying to save still rejects and jumps
	// back to the incomplete row.
	dashboard.HandleKey(key('/'))
	for _, character := range "theme" {
		dashboard.HandleKey(key(character))
	}
	dashboard.HandleKey(key(tea.KeyEnter))
	if path := setupDialog(t, dashboard).path; !slices.Equal(path, []string{"interface"}) {
		t.Fatalf("path = %v", path)
	}
	if action := dashboard.HandleKey(setupCtrl('s')); !setupActionIsNone(action) {
		t.Fatalf("save returned %#v", action)
	}
	dialog = setupDialog(t, dashboard)
	if !slices.Equal(dialog.path, category) {
		t.Fatalf("path = %v", dialog.path)
	}
	if path, _ := dialog.focusedPath(); !slices.Equal(path, model0) {
		t.Fatalf("focused %v", path)
	}

	// Removing the only incomplete row via Delete unblocks Back and clears
	// dirty state.
	dashboard.HandleKey(key(tea.KeyDelete))
	dialog = setupDialog(t, dashboard)
	if !slices.Equal(dialog.path, category) {
		t.Fatalf("path = %v", dialog.path)
	}
	if entries := setupValuePath(dialog.draft, "subagents", "straightforward"); !setupValueEqual(entries, []any{}) {
		t.Fatalf("entries = %s", setupValueDisplay(entries))
	}
	if dialog.isDirty() {
		t.Fatal("removing the incomplete row leaves no change")
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	if path := setupDialog(t, dashboard).path; !slices.Equal(path, []string{"subagents"}) {
		t.Fatalf("path = %v", path)
	}

	// Re-enter, configure two rows, verify narrow rendering, and remove row
	// 0 to check index shifting.
	setupChoose(t, dashboard, "straightforward")
	dashboard.HandleKey(key('a'))
	dashboard.HandleKey(key(tea.KeyEnter))
	setupSelectChoice(t, dashboard, "fast")
	dashboard.HandleKey(key('a'))
	dashboard.HandleKey(key(tea.KeyEnter))
	setupSelectChoice(t, dashboard, "smart")
	setupDialog(t, dashboard).focusPath([]string{"subagents", "straightforward", "1", "effort"})
	dashboard.HandleKey(key(tea.KeyEnter))
	setupSelectChoice(t, dashboard, "high")

	// Narrow layout renders stacked model/effort/remove controls without
	// overlapping or panicking.
	narrow := strings.Join(drawn(dashboard, 60, 30), "\n")
	for _, expected := range []string{"1 model", "1 effort", "Remove 1", "2 model"} {
		if !strings.Contains(narrow, expected) {
			t.Fatal(narrow)
		}
	}

	// Removing row 0 shifts row 1 ("smart" / "high") into index 0 and keeps
	// the page open.
	setupDialog(t, dashboard).focusPath(model0)
	dashboard.HandleKey(key(tea.KeyDelete))
	dialog = setupDialog(t, dashboard)
	if !slices.Equal(dialog.path, category) {
		t.Fatalf("path = %v", dialog.path)
	}
	if entries := setupValuePath(dialog.draft, "subagents", "straightforward"); !setupValueEqual(entries, setupJSON(t, `[{"model": "smart", "effort": "high"}]`)) {
		t.Fatalf("entries = %s", setupValueDisplay(entries))
	}
}
