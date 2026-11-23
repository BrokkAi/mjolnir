// Port of mj-tui/src/setup/tests/goldens.rs.

package tui

import (
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// setupGoldenDirectory holds tests/golden, copied from mj-tui.
const setupGoldenDirectory = "testdata"

// setupFocused is the open Settings dialog's focused control.
func setupFocused(t testing.TB, dashboard *DashboardState) (SetupControl, bool) {
	t.Helper()
	return setupDialog(t, dashboard).form.Focused()
}

// setupEditorAdding is whether the open dialog's editor names a new entry.
func setupEditorAdding(t testing.TB, dashboard *DashboardState) bool {
	t.Helper()
	editor := setupDialog(t, dashboard).editor
	return editor != nil && editor.adding
}

// setupSave presses Ctrl+S and expects the background save it starts.
func setupSave(t testing.TB, dashboard *DashboardState, what string) (DashboardActionSaveSetup, core.Config) {
	t.Helper()
	action := dashboard.HandleKey(setupCtrl('s'))
	save, ok := action.(DashboardActionSaveSetup)
	if !ok {
		t.Fatalf("expected %s, got %#v", what, action)
	}
	var saved core.Config
	if err := json.Unmarshal([]byte(save.Updated), &saved); err != nil {
		t.Fatalf("saved config: %v", err)
	}
	return save, saved
}

func TestGoldenSettingsRoot(t *testing.T) {
	var output strings.Builder

	root := setupAt(t)
	lines := appendSetupState(&output, "grouped Settings root", root, 140, 42)
	save := point(lines, "  Save and Close  ")
	body := point(lines, "Runtimes")
	fmt.Fprintf(&output, "root footer alignment: save=(%d,%d); body=(%d,%d)\n", save.X, save.Y, body.X, body.Y)

	interface_ := setupAt(t, "interface")
	appendSetupState(&output, "virtual Interface page", interface_, 100, 30)
	setupChooseByKeyboard(t, interface_, "theme")
	appendSetupState(&output, "Interface theme choices", interface_, 100, 30)
	interface_.HandleKey(key(tea.KeyDown))
	interface_.HandleKey(key(tea.KeyTab))
	appendSetupState(&output, "theme selection stays at the root config path", interface_, 100, 30)
	dialog := setupDialog(t, interface_)
	fmt.Fprintf(&output, "draft paths: theme=%s; interface.theme present=%t; keys.prefix=%s\n",
		setupValueDisplay(setupValueGet(dialog.draft, "theme")),
		setupValueHas(setupValueGet(dialog.draft, "interface"), "theme"),
		setupValueDisplay(setupValuePath(dialog.draft, "keys", "prefix")))

	// The same modal body is used by the root, nested pages, popups, editors,
	// and the Code Review child.
	sizeJourney := setupAt(t)
	appendSetupState(&output, "compact root body", sizeJourney, 100, 30)
	appendSetupBodySize(t, &output, sizeJourney)
	setupChooseByKeyboard(t, sizeJourney, "interface")
	appendSetupState(&output, "compact nested page body", sizeJourney, 100, 30)
	appendSetupBodySize(t, &output, sizeJourney)
	setupChooseByKeyboard(t, sizeJourney, "theme")
	appendSetupState(&output, "compact choice popup body", sizeJourney, 100, 30)
	appendSetupBodySize(t, &output, sizeJourney)
	sizeJourney.HandleKey(key(tea.KeyEscape))
	sizeJourney.HandleKey(key(tea.KeyBackspace))
	setupChooseByKeyboard(t, sizeJourney, "advanced")
	appendSetupState(&output, "compact Advanced page body", sizeJourney, 100, 30)
	appendSetupBodySize(t, &output, sizeJourney)
	sizeJourney.HandleKey(key(tea.KeyBackspace))
	setupChooseByKeyboard(t, sizeJourney, "phone")
	setupChooseByKeyboard(t, sizeJourney, "bind")
	appendSetupState(&output, "compact text editor body", sizeJourney, 100, 30)
	appendSetupBodySize(t, &output, sizeJourney)
	for {
		if focused, ok := setupFocused(t, sizeJourney); ok && focused == SetupControlBack {
			break
		}
		sizeJourney.HandleKey(key(tea.KeyTab))
	}
	sizeJourney.HandleKey(key(tea.KeyEnter))
	setupChooseByKeyboard(t, sizeJourney, "review")
	appendSetupState(&output, "compact Code Review body", sizeJourney, 100, 30)
	appendSetupBodySize(t, &output, sizeJourney)

	// Each page renders only the actions available at that location.
	for _, page := range []struct {
		label string
		path  []string
	}{
		{"root footer action", nil},
		{"runtime collection actions", []string{"targets"}},
		{"project collection actions", []string{"bundles"}},
		{"profile collection actions", []string{"profiles"}},
		{"leaf page footer", []string{"phone"}},
		{"text editor actions", []string{"phone", "bind"}},
	} {
		appendSetupState(&output, page.label, setupAt(t, page.path...), 100, 30)
	}

	runtimeActions := setupAt(t, "targets")
	appendSetupState(&output, "runtime collection actions", runtimeActions, 100, 30)
	for range 10 {
		if focused, ok := setupFocused(t, runtimeActions); ok && focused == SetupControlAdd {
			break
		}
		runtimeActions.HandleKey(key(tea.KeyTab))
	}
	focused, focusedOK := setupFocused(t, runtimeActions)
	if !focusedOK || focused != SetupControlAdd {
		t.Fatalf("focused %s", setupControlDebug(focused, focusedOK))
	}
	appendSetupState(&output, "runtime Add reached from the keyboard", runtimeActions, 100, 30)
	fmt.Fprintf(&output, "focused control: %s\n", setupControlDebug(focused, focusedOK))
	action := runtimeActions.HandleKey(key(tea.KeyEnter))
	appendSetupState(&output, "runtime Add opens a new-entry editor", runtimeActions, 100, 30)
	fmt.Fprintf(&output, "action: %s; new entry: %t\n", setupActionDebug(action), setupEditorAdding(t, runtimeActions))

	projects := setupAt(t, "bundles")
	projects.HandleKey(key(tea.KeyEnd))
	projects.HandleKey(key(tea.KeyTab))
	appendSetupState(&output, "project Create reached with Tab", projects, 100, 24)
	fmt.Fprintf(&output, "entry editor adding: %t\n", setupEditorAdding(t, projects))
	projects = setupAt(t, "bundles")
	projects.HandleKey(key(tea.KeyEnd))
	projects.HandleKey(key(tea.KeyDown))
	appendSetupState(&output, "project Create reached with Down", projects, 100, 24)
	fmt.Fprintf(&output, "entry editor adding: %t\n", setupEditorAdding(t, projects))

	// Theme values take effect only after a successful save, and are read
	// back by the next Settings dialog.
	themed := dashboardWithSession(stoppedSession())
	themed.BeginSetup()
	setupChooseByKeyboard(t, themed, "interface")
	setupChooseByKeyboard(t, themed, "theme")
	themed.HandleKey(key(tea.KeyDown))
	themed.HandleKey(key(tea.KeyTab))
	appendSetupState(&output, "Light selected while Midnight remains active", themed, 100, 30)
	fmt.Fprintf(&output, "%s\n", setupRenderedThemeColors(t, themed, core.UiThemeMidnight))
	saved, savedConfig := setupSave(t, themed, "Settings save")
	fmt.Fprintf(&output, "saved theme: %s\n", savedConfig.Theme.Label())
	appendSetupState(&output, "active theme before save acknowledgement", themed, 100, 30)
	fmt.Fprintf(&output, "%s\n", setupRenderedThemeColors(t, themed, core.UiThemeMidnight))
	themed.SetupSaved(saved.Generation, savedConfig, nil)
	appendSetupState(&output, "Light theme after save acknowledgement", themed, 100, 30)
	fmt.Fprintf(&output, "%s\n", setupRenderedThemeColors(t, themed, core.UiThemeLight))
	themed.BeginSetup()
	setupChooseByKeyboard(t, themed, "interface")
	setupChooseByKeyboard(t, themed, "theme")
	appendSetupState(&output, "reopened theme choice", themed, 100, 30)
	choices, selected := setupOpenChoices(t, setupDialog(t, themed))
	fmt.Fprintf(&output, "reopened selected theme: %s\n", setupValueDisplay(choices[selected]))

	// Auto, the accessible and Catppuccin palettes, and the Windows Terminal
	// and terminal-color themes save and apply through the same flow,
	// starting from the reopened choice list. No terminal answers here, so
	// Auto draws Midnight and says why.
	for _, target := range []core.ThemeChoice{
		core.ThemeChoiceAuto{},
		core.ThemeChoiceFixed{Value: core.UiThemeHighContrastLight},
		core.ThemeChoiceFixed{Value: core.UiThemeColorblindDark},
		core.ThemeChoiceFixed{Value: core.UiThemeColorblindLight},
		core.ThemeChoiceFixed{Value: core.UiThemeCatppuccinMocha},
		core.ThemeChoiceFixed{Value: core.UiThemeCatppuccinLatte},
		core.ThemeChoiceFixed{Value: core.UiThemeCampbell},
		core.ThemeChoiceFixed{Value: core.UiThemeOneHalfDark},
		core.ThemeChoiceFixed{Value: core.UiThemeOneHalfLight},
		core.ThemeChoiceFixed{Value: core.UiThemeSolarizedDark},
		core.ThemeChoiceFixed{Value: core.UiThemeSolarizedLight},
		core.ThemeChoiceFixed{Value: core.UiThemeTerminalDark},
		core.ThemeChoiceFixed{Value: core.UiThemeTerminalLight},
	} {
		wanted, err := setupValueTo(target)
		if err != nil {
			t.Fatalf("theme serializes: %v", err)
		}
		choices, current := setupOpenChoices(t, setupDialog(t, themed))
		position := slices.IndexFunc(choices, func(choice any) bool { return setupValueEqual(choice, wanted) })
		if position < 0 {
			t.Fatalf("theme %s offered", target.Label())
		}
		step := rune(tea.KeyDown)
		if position < current {
			step = tea.KeyUp
		}
		for range max(current-position, position-current) {
			themed.HandleKey(key(step))
		}
		themed.HandleKey(key(tea.KeyTab))
		saved, savedConfig := setupSave(t, themed, "Settings save")
		fmt.Fprintf(&output, "saved theme: %s\n", savedConfig.Theme.Label())
		themed.SetupSaved(saved.Generation, savedConfig, nil)
		fmt.Fprintf(&output, "%s after save acknowledgement: %s\n", target.Label(),
			setupRenderedThemeColors(t, themed, chat.Resolve(target, false, chat.TerminalBackground()).Theme))
		themed.BeginSetup()
		setupChooseByKeyboard(t, themed, "interface")
		setupChooseByKeyboard(t, themed, "theme")
	}

	managed := setupAt(t, "profiles")
	setupClick(managed, "Set up managed")
	appendSetupState(&output, "managed agent buttons", managed, 100, 30)
	managed.HandleKey(key(tea.KeyEscape))
	if setupDialog(t, managed).managedSetup != nil {
		t.Fatal("Esc leaves the managed chooser")
	}

	output.WriteString("managed chooser Back returns to Agent Profiles\n")

	core.AssertGolden(t, setupGoldenDirectory, "settings-root", output.String())
}

func TestGoldenBuildCache(t *testing.T) {
	var output strings.Builder
	cachePath := []string{"machines", "local", "build_cache"}

	cache := setupAt(t, cachePath...)
	setupChooseByKeyboard(t, cache, "max_total_size")
	for _, digit := range "25" {
		cache.HandleKey(key(digit))
	}
	cache.HandleKey(key(tea.KeyEnter))
	appendSetupState(&output, "cache budget entered in whole GB", cache, 140, 30)
	dialog := setupDialog(t, cache)
	setupValueInsert(&dialog.draft, cachePath, "max_total_size", "100GiB")
	setupChooseByKeyboard(t, cache, "max_total_size")
	appendSetupState(&output, "existing GiB value opens as whole GB", cache, 140, 30)
	fmt.Fprintf(&output, "editor value: %s\n", setupFocusedField(t, setupDialog(t, cache)).input.String())
	for range 3 {
		cache.HandleKey(key(tea.KeyBackspace))
	}
	for _, character := range "10GiB" {
		cache.HandleKey(key(character))
	}
	cache.HandleKey(key(tea.KeyEnter))
	appendSetupState(&output, "unit suffix refused", cache, 140, 30)
	storedSize := func(dashboard *DashboardState) string {
		return setupValueDisplay(setupValuePath(setupDialog(t, dashboard).draft, "machines", "local", "build_cache", "max_total_size"))
	}
	fmt.Fprintf(&output, "stored size after rejection: %s\n", storedSize(cache))
	*setupFocusedField(t, setupDialog(t, cache)).input.text() = chat.TextInputFromValue("0")
	cache.HandleKey(key(tea.KeyEnter))
	appendSetupState(&output, "zero refused with a default hint", cache, 140, 30)
	fmt.Fprintf(&output, "stored size after rejection: %s\n", storedSize(cache))

	savedSize := setupAt(t, cachePath...)
	setupChooseByKeyboard(t, savedSize, "max_total_size")
	for _, digit := range "12" {
		savedSize.HandleKey(key(digit))
	}
	savedSize.HandleKey(key(tea.KeyEnter))
	save, saved := setupSave(t, savedSize, "cache settings save")
	machine, err := setupValueTo(saved.Machines["local"])
	if err != nil {
		t.Fatalf("machine settings: %v", err)
	}
	fmt.Fprintf(&output, "saved machine.local: %s\n", setupValueDisplay(machine))
	savedSize.SetupSaved(save.Generation, saved, nil)
	savedSize.BeginSetup()
	for _, name := range cachePath {
		setupChooseByKeyboard(t, savedSize, name)
	}
	appendSetupState(&output, "all cache fields remain on the reopened page", savedSize, 140, 30)
	setupChooseByKeyboard(t, savedSize, "max_total_size")
	appendSetupState(&output, "Use default clears only the optional budget", savedSize, 140, 30)
	setupReplaceText(savedSize, "")
	savedSize.HandleKey(key(tea.KeyEnter))
	fmt.Fprintf(&output, "budget after Use default: %s\n", storedSize(savedSize))

	scheduler := setupAt(t, cachePath...)
	setupChooseByKeyboard(t, scheduler, "scheduler")
	appendSetupState(&output, "machine scheduler fields", scheduler, 140, 30)
	setupChooseByKeyboard(t, scheduler, "cpus")
	for _, digit := range "12" {
		scheduler.HandleKey(key(digit))
	}
	scheduler.HandleKey(key(tea.KeyEnter))
	setupChooseByKeyboard(t, scheduler, "memory")
	for _, character := range "6GiB" {
		scheduler.HandleKey(key(character))
	}
	scheduler.HandleKey(key(tea.KeyEnter))
	appendSetupState(&output, "scheduler values entered", scheduler, 140, 30)
	action, ok := scheduler.HandleKey(setupCtrl('s')).(DashboardActionSaveSetup)
	if !ok {
		t.Fatal("expected scheduler save")
	}
	updated := setupJSON(t, action.Updated)
	fmt.Fprintf(&output, "saved scheduler: %s\n",
		setupValueDisplay(setupValuePath(updated, "machines", "local", "build_cache", "scheduler")))

	tabbed := setupAt(t, cachePath...)
	count := len(setupDialog(t, tabbed).descriptors)
	for range count {
		tabbed.HandleKey(key(tea.KeyTab))
	}
	if !setupDialog(t, tabbed).form.IsFocused(SetupControlBack) {
		t.Fatal("Tab reaches Back after the page's controls")
	}
	appendSetupState(&output, "Build cache Tab reaches Back after its controls", tabbed, 140, 30)
	tabbed.HandleKey(key(tea.KeyTab))
	if !setupDialog(t, tabbed).form.IsFocused(SetupControlSave) {
		t.Fatal("Tab reaches Save")
	}
	appendSetupState(&output, "Build cache Tab reaches Save", tabbed, 140, 30)
	_, isSave := tabbed.HandleKey(key(tea.KeyEnter)).(DashboardActionSaveSetup)
	fmt.Fprintf(&output, "Tab-reached Save action: %t\n", isSave)

	arrows := setupAt(t, cachePath...)
	for range count {
		arrows.HandleKey(key(tea.KeyDown))
	}
	if !setupDialog(t, arrows).form.IsFocused(SetupControlBack) {
		t.Fatal("Down reaches Back after the page's controls")
	}
	arrows.HandleKey(key(tea.KeyDown))
	if !setupDialog(t, arrows).form.IsFocused(SetupControlSave) {
		t.Fatal("Down reaches Save")
	}
	appendSetupState(&output, "Build cache arrows reach footer", arrows, 140, 30)
	arrows.HandleKey(key(tea.KeyUp))
	arrows.HandleKey(key(tea.KeyEnter))
	if path := setupDialog(t, arrows).path; !slices.Equal(path, []string{"machines", "local"}) {
		t.Fatalf("path = %v", path)
	}
	appendSetupState(&output, "Up and Enter return from Save to the machine page", arrows, 140, 30)

	shared := setupAt(t, cachePath...)
	setupChooseByKeyboard(t, shared, "max_total_size")
	for _, digit := range "250" {
		shared.HandleKey(key(digit))
	}
	shared.HandleKey(key(tea.KeyEnter))
	appendSetupState(&output, "shared storage budget uses GB", shared, 140, 30)

	// The machine row opens the same page through keyboard activation or a
	// double-click, and its summary reflects the saved machine policy.
	for _, mouse := range []bool{false, true} {
		machine := setupAt(t, "machines", "local")
		label := map[bool][3]string{
			false: {"Build cache summary before keyboard open", "Build cache opened by keyboard", "disabled cache summary after keyboard journey"},
			true:  {"Build cache summary before mouse open", "Build cache opened by mouse", "disabled cache summary after mouse journey"},
		}[mouse]
		lines := appendSetupState(&output, label[0], machine, 160, 40)
		if mouse {
			location := point(lines, "Build caches")
			for range 2 {
				setupClickAt(machine, location, 160, 40)
			}
		} else {
			setupChooseByKeyboard(t, machine, "build_cache")
		}
		appendSetupState(&output, label[1], machine, 160, 40)
		dialog := setupDialog(t, machine)
		setupValueInsert(&dialog.draft, cachePath, "enabled", false)
		setupValueInsert(&dialog.draft, cachePath, "max_total_size", "100GB")
		machine.HandleKey(key(tea.KeyEscape))
		lines = appendSetupState(&output, label[2], machine, 160, 40)
		fmt.Fprintf(&output, "summary contains changed state: %t\n",
			slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, "Disabled · 100 GB budget") }))
	}

	core.AssertGolden(t, setupGoldenDirectory, "build-cache", output.String())
}

// A machine no runtime runs on is no session target, so its Machines row
// says why. Detect runtimes asks every machine in the draft what it runs and
// adds those runtimes under that machine.
func TestGoldenMachinesWithoutRuntimes(t *testing.T) {
	user := "dev"
	machine := func(host string) core.Machine {
		return core.MachineSsh{
			SSH:             core.SshConnection{Host: host, User: &user, ExtraArgs: []string{}},
			WorkspacePrefix: core.DefaultWorkspacePrefix,
		}
	}
	var output strings.Builder
	dashboard := dashboardWithSession(stoppedSession())
	configuration := *dashboard.config.Get()
	configuration.Machines = maps.Clone(configuration.Machines)
	configuration.Machines["nas"] = machine("nas.example.test")
	dashboard.SetConfig(configuration)
	answerAvailability(dashboard, func(dashboard *DashboardState) client.RuntimeAvailability {
		return availabilityAnswer(dashboard, nil, []string{"nas"})
	})
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "machines")
	dashboard.HandleKey(key('a'))
	dashboard.HandlePaste("builder")
	dashboard.HandleKey(key(tea.KeyEnter))
	setupChoose(t, dashboard, "host")
	dashboard.HandlePaste("builder.example.test")
	dashboard.HandleKey(key(tea.KeyEnter))
	setupActivate(t, dashboard, SetupControlBack)
	appendSetupState(&output, "machines without a runtime say why", dashboard, 140, 34)

	setupActivate(t, dashboard, SetupControlBack)
	setupChoose(t, dashboard, "targets")
	dialog := setupDialog(t, dashboard)
	dialog.form.Focus(SetupControlDetectRuntimes)
	generation := dialog.generation
	// Detection asks every machine in the draft, the unsaved one too.
	discover, ok := dashboard.HandleKey(key(tea.KeyEnter)).(DashboardActionDiscoverSetup)
	if !ok {
		t.Fatal("Detect runtimes starts a detection")
	}
	var ids []string
	for _, pair := range discover.Machines {
		ids = append(ids, pair.First)
	}
	if !slices.Equal(ids, []string{"builder", "nas"}) {
		t.Fatalf("machines = %v", ids)
	}
	builder, ok := discover.Machines[0].Second.(core.MachineSsh)
	if !ok {
		t.Fatal("builder is an SSH machine")
	}
	answeredNothing := true
	dashboard.SetupDiscovered(generation, SetupDetection{
		ProfileReadiness: map[string]*string{},
		Scope:            DetectScopeRuntimes,
		Config:           core.DefaultConfig(),
		Machines: []client.MachineRuntimeProbe{
			{
				MachineID: "builder",
				Machine:   discover.Machines[0].Second,
				Finding: client.MachineFindingAnswered{
					Runtimes: []client.DaemonPair[string, core.TargetTemplate]{
						{First: "builder", Second: core.TargetTemplateSshBare{
							SSH:             builder.SSH,
							Permissions:     core.PermissionModeGuardian,
							WorkspacePrefix: core.DefaultWorkspacePrefix,
						}},
						{First: "builder-docker", Second: core.TargetTemplateSshDocker{
							SSH: builder.SSH,
							ContainerTemplate: core.ContainerTemplate{
								Image:            client.DefaultImage,
								PullPolicy:       core.ImagePullPolicyAuto,
								WorkspaceStorage: core.PodmanWorkspaceStoragePodmanVolume{},
							},
						}},
					},
					Rejected: []RejectedRuntime{{
						Kind:   "podman",
						Label:  "Podman",
						Detail: "Podman is not installed.",
					}},
				},
			},
			{
				MachineID: "nas",
				Machine:   discover.Machines[1].Second,
				Finding: client.MachineFindingUnreachable{
					AnsweredNothing: answeredNothing,
					Detail:          "`ssh dev@nas.example.test true` failed: Connection timed out",
					Remediation:     "Check that nas.example.test is up.",
				},
			},
		},
	}, nil)
	appendSetupState(&output, "detection adds each machine's runtimes under it", dashboard, 140, 34)
	dialog = setupDialog(t, dashboard)
	for _, runtime := range [][2]string{{"builder", "bare"}, {"builder-docker", "docker"}} {
		target := setupValuePath(dialog.draft, "targets", runtime[0])
		if !setupValueEqual(setupValueGet(target, "kind"), runtime[1]) || !setupValueEqual(setupValueGet(target, "machine"), "builder") {
			t.Fatalf("%s = %s", runtime[0], setupValueDisplay(target))
		}
	}
	if setupValueHas(setupValueGet(dialog.draft, "targets"), "nas") {
		t.Fatal("an unreachable machine gained a runtime")
	}

	// Starting detection parks its button's activation while the button is
	// disabled. A real key retires that, as it does for a person who moves
	// on to the report button; Enter alone would replay the detect action.
	dashboard.HandleKey(key(tea.KeyTab))
	setupActivate(t, dashboard, SetupControlDetectionResults)
	appendSetupState(&output, "detection results lists each refusal and its fix", dashboard, 140, 34)
	dashboard.HandleKey(key(tea.KeyEnd))
	dashboard.HandleKey(key(tea.KeyEscape))
	appendSetupState(&output, "esc from detection results returns to runtimes", dashboard, 140, 34)

	dashboard.HandleKey(key(tea.KeyEscape))
	setupChoose(t, dashboard, "machines")
	appendSetupState(&output, "only the unreachable machine still lacks a runtime", dashboard, 140, 34)
	core.AssertGolden(t, setupGoldenDirectory, "machines-without-runtimes", output.String())
}

func TestGoldenSettingsSearch(t *testing.T) {
	var output strings.Builder

	memory := setupAt(t)
	setupSearch(memory, "memory")
	appendSetupState(&output, "search memory from Settings root", memory, 140, 30)
	fmt.Fprintf(&output, "matching paths: %s\n", setupDebugPaths(setupResultPaths(t, memory)))
	memory.HandleKey(key(tea.KeyEnter))
	appendSetupState(&output, "memory result focuses its inline field", memory, 140, 30)
	landedPage := setupDialog(t, memory).path
	landedControl := "None"
	if path, ok := setupDialog(t, memory).focusedPath(); ok {
		landedControl = "Some(" + setupDebugStrings(path) + ")"
	}
	fmt.Fprintf(&output, "landed page and focused setting: %s / %s\n", setupDebugStrings(landedPage), landedControl)

	compaction := setupAt(t)
	setupSearch(compaction, "compaction")
	appendSetupState(&output, "help text finds context budget by compaction", compaction, 140, 30)
	fmt.Fprintf(&output, "matching paths: %s\n", setupDebugPaths(setupResultPaths(t, compaction)))

	image := setupAt(t)
	setupSearch(image, "image")
	appendSetupState(&output, "search result includes its section, label, and current value", image, 140, 30)
	fmt.Fprintf(&output, "matching paths: %s\n", setupDebugPaths(setupResultPaths(t, image)))

	core.AssertGolden(t, setupGoldenDirectory, "settings-search", output.String())
}

func TestGoldenGlobalSubagentDiscoveryAndSave(t *testing.T) {
	dashboard := setupAt(t, "subagents")
	choice := func(value string) core.SessionConfigChoice {
		return core.SessionConfigChoice{Value: value, Name: value}
	}
	ready := func(values ...string) core.CapabilityState[[]core.SessionConfigChoice] {
		choices := make([]core.SessionConfigChoice, 0, len(values))
		for _, value := range values {
			choices = append(choices, choice(value))
		}
		return core.CapabilityState[[]core.SessionConfigChoice]{Kind: core.CapabilityStateReady, Value: choices}
	}
	profiles := map[string]core.ProfileCapabilities{}
	index := 0
	for id, profile := range dashboard.config.Get().EnabledProfiles() {
		model := "smarter"
		if index == 0 {
			model = "shared"
		}
		index++
		selected := model
		profiles[profile.CapabilitiesKey(id)] = core.ProfileCapabilities{
			Choices: core.CapabilityState[core.ProfileConfig]{
				Kind: core.CapabilityStateReady,
				Value: core.ProfileConfig{
					Model:      &selected,
					Models:     []core.SessionConfigChoice{choice("shared"), choice(model)},
					Efforts:    []core.SessionConfigChoice{},
					ObservedAt: 1,
				},
			},
			Efforts: map[string]core.CapabilityState[[]core.SessionConfigChoice]{
				"shared": ready("high"),
				model:    ready("high", "max"),
			},
		}
	}
	dashboard.SetProfileCapabilities(core.ProfileCapabilitiesSnapshot{Profiles: profiles})
	if setupDialog(t, dashboard).isDirty() {
		t.Fatal("filling the choices dirtied the draft")
	}
	var output strings.Builder
	appendSetupState(&output, "global task choices", dashboard, 140, 40)
	for _, task := range []string{"primary", "straightforward", "subtle", "oracle"} {
		if entries := setupValuePath(setupDialog(t, dashboard).draft, "subagents", task); !setupValueEqual(entries, []any{}) {
			t.Fatalf("%s = %s", task, setupValueDisplay(entries))
		}
	}
	for _, configured := range [][2]string{{"straightforward", "shared"}, {"subtle", "smarter"}, {"oracle", "smarter"}} {
		task, model := configured[0], configured[1]
		category := []string{"subagents", task}
		setupChoose(t, dashboard, task)
		if path := setupDialog(t, dashboard).path; !slices.Equal(path, category) {
			t.Fatalf("path = %v", path)
		}
		dashboard.HandleKey(key('a'))
		if path := setupDialog(t, dashboard).path; !slices.Equal(path, category) {
			t.Fatalf("Add keeps the user on the single subagent category page: %v", path)
		}
		if task == "straightforward" {
			// Back rejects a partly configured row and keeps focus on its
			// model.
			dashboard.HandleKey(key(tea.KeyEscape))
			if path := setupDialog(t, dashboard).path; !slices.Equal(path, category) {
				t.Fatalf("path = %v", path)
			}
			appendSetupState(&output, "partly configured row rejected", dashboard, 140, 40)
			if action := setupDialog(t, dashboard).save(&core.ProfileCapabilitiesSnapshot{}); !setupActionIsNone(action) {
				t.Fatalf("save returned %#v", action)
			}
		}
		dashboard.HandleKey(key(tea.KeyEnter))
		values, _ := setupOpenChoices(t, setupDialog(t, dashboard))
		shared := 0
		for _, value := range values {
			if setupValueEqual(value, "shared") {
				shared++
			}
		}
		if shared != 1 {
			t.Fatalf("shared is offered %d times", shared)
		}
		setupSelectChoice(t, dashboard, model)
		setupDialog(t, dashboard).focusPath([]string{"subagents", task, "0", "effort"})
		dashboard.HandleKey(key(tea.KeyEnter))
		setupSelectChoice(t, dashboard, "high")
		if task == "straightforward" {
			// Show multiple configured rows on one page, then remove the
			// extra row inline.
			dashboard.HandleKey(key('a'))
			dashboard.HandleKey(key(tea.KeyEnter))
			setupSelectChoice(t, dashboard, "smarter")
			appendSetupState(&output, task, dashboard, 140, 40)
			setupDialog(t, dashboard).focusPath([]string{"subagents", task, "1"})
			dashboard.HandleKey(key(tea.KeyEnter))
			if entries, _ := setupValueArray(setupValuePath(setupDialog(t, dashboard).draft, "subagents", task)); len(entries) != 1 {
				t.Fatalf("%d entries remain", len(entries))
			}
		} else {
			appendSetupState(&output, task, dashboard, 140, 40)
		}
		dashboard.HandleKey(key(tea.KeyEscape))
		if path := setupDialog(t, dashboard).path; !slices.Equal(path, []string{"subagents"}) {
			t.Fatalf("path = %v", path)
		}
	}
	dialog := setupDialog(t, dashboard)
	save, ok := dialog.save(&core.ProfileCapabilitiesSnapshot{}).(DashboardActionSaveSetup)
	if !ok {
		t.Fatal("save")
	}
	var saved core.Config
	if err := json.Unmarshal([]byte(save.Updated), &saved); err != nil {
		t.Fatal(err)
	}
	if !saved.Subagents.IsConfigured() || saved.Subagents.Subtle[0].Model != "smarter" || saved.Subagents.Oracle[0].Model != "smarter" {
		t.Fatalf("subagents = %#v", saved.Subagents)
	}
	dialog.saving = false
	dialog.path = []string{"subagents"}
	setupValueInsert(&dialog.draft, []string{"subagents"}, "straightforward", []any{})
	beforeRejectedSave := setupValueClone(dialog.draft)
	configuration, err := client.ConfigFromDraft(dialog.draft)
	if err != nil {
		t.Fatal(err)
	}
	if len(configuration.Subagents.Straightforward) != 0 || configuration.Subagents.IsConfigured() {
		t.Fatal("the cleared choice is still configured")
	}
	appendSetupState(&output, "cleared choice", dashboard, 140, 40)

	save, ok = setupDialog(t, dashboard).save(&core.ProfileCapabilitiesSnapshot{}).(DashboardActionSaveSetup)
	if !ok {
		t.Fatal("expected background validation save")
	}
	var invalid core.Config
	if err := json.Unmarshal([]byte(save.Updated), &invalid); err != nil {
		t.Fatal(err)
	}
	validation := invalid.Validate()
	if validation == nil {
		t.Fatal("a cleared straightforward choice validates")
	}
	dashboard.SetupSaved(save.Generation, core.Config{}, errors.New(validation.Error()))
	dialog = setupDialog(t, dashboard)
	if !slices.Equal(dialog.path, []string{"subagents"}) {
		t.Fatalf("path = %v", dialog.path)
	}
	if !setupValueEqual(dialog.draft, beforeRejectedSave) {
		t.Fatal("a rejected save changed the draft")
	}
	if dialog.saving {
		t.Fatal("a rejected save leaves Settings saving")
	}
	if dialog.notice == nil || !strings.HasPrefix(*dialog.notice, "Could not save:") {
		t.Fatalf("notice = %v", dialog.notice)
	}
	if len(dashboard.config.Get().Subagents.Straightforward) != 0 {
		t.Fatal("a rejected save reached the configuration")
	}
	appendSetupState(&output, "invalid save keeps draft and reports validation", dashboard, 140, 40)
	core.AssertGolden(t, setupGoldenDirectory, "global-subagents", output.String())
}
