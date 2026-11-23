// Port of mj-tui/src/setup/tests/build_cache.rs.

package tui

import (
	"errors"
	"reflect"
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// setupStringPtr is Some(text.into()) for an Option<String> field.
func setupStringPtr(text string) *string { return &text }

// setupPreviewStateEqual is BuildCachePreviewState's derived PartialEq.
func setupPreviewStateEqual(a, b BuildCachePreviewState) bool {
	return setupValueEqual(a.key, b.key) && reflect.DeepEqual(a.result, b.result) &&
		a.installMbxAvailable == b.installMbxAvailable
}

// setupHasInstallMbx is whether the open page offers the Install mbx action.
func setupHasInstallMbx(dialog *SetupDialog) bool {
	return slices.ContainsFunc(dialog.actions(), func(button setupButtonSpec) bool {
		return button.ID == SetupControlInstallMbx
	})
}

func TestTheBuildCachePageShowsTheValuesItsHostResolvesForBlankFields(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	podman, err := core.UnmarshalTargetTemplateJSON([]byte(`{"kind":"local-podman","image":"example/image:latest"}`))
	if err != nil {
		t.Fatal(err)
	}
	dashboard.config.Mut().Targets["podman-host"] = podman
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "machines")
	preview, ok := setupChoose(t, dashboard, "local").(DashboardActionPreviewBuildCache)
	if !ok {
		t.Fatal("preview build cache")
	}
	generation, previewKey := preview.Generation, preview.Key
	dialog := setupDialog(t, dashboard)
	setupFocusSetting(dialog, "build_cache")
	dialog.prepare()
	// Opening the machine starts the host lookup; opening its cache page does
	// not start a second one.
	if action := dashboard.HandleKey(key(tea.KeyEnter)); !setupActionIsNone(action) {
		t.Fatalf("opening the cache page returned %#v", action)
	}
	if action := dashboard.HandleKey(key(tea.KeyDown)); !setupActionIsNone(action) {
		t.Fatalf("moving on the cache page returned %#v", action)
	}
	resolving := strings.Join(bufferLines(setupRenderBuffer(dashboard, 140, 30)), "\n")
	if !strings.Contains(resolving, "Resolving…") {
		t.Fatal(resolving)
	}

	dashboard.BuildCachePreviewed(generation, previewKey, &core.BuildCachePreview{
		UserManaged:  false,
		Application:  core.BuildCacheApplicationPending{},
		NativeMbx:    setupStringPtr("1.12.0"),
		Directory:    setupStringPtr("/mnt/fast/mbx-cache"),
		MaxTotalSize: core.BuildCacheLimitHostConfiguration{Value: setupStringPtr("500GiB")},
		OffReason:    core.BuildCacheOffUnavailable{Value: "the filesystem under /mnt/fast/mbx-cache does not support reflinks"},
	}, nil, false)
	resolved := strings.Join(bufferLines(setupRenderBuffer(dashboard, 140, 30)), "\n")
	for _, expected := range []string{
		"Enabled",
		"Default (on)",
		"Effective: Off",
		"mbx: the filesystem under /mnt/fast/mbx-cache does not support reflinks",
		"/mnt/fast/mbx-cache",
		"537 GB, host mbx config",
		"mbx build cache is unavailable: the filesystem under",
	} {
		if !strings.Contains(resolved, expected) {
			t.Fatalf("missing %q in\n%s", expected, resolved)
		}
	}

	dashboard.BuildCachePreviewed(generation, previewKey, &core.BuildCachePreview{
		NativeMbx:   setupStringPtr("1.26.0"),
		Application: core.BuildCacheApplicationPending{},
		OffReason: core.BuildCacheOffUnsupportedPlatform{Value: "mbx 1.26.0 runs natively on this Mac at /Users/builder/.local/bin/mbx; bare " +
			"sessions here use it when it is on their login PATH. Container sessions run " +
			"without the shared build cache."},
	}, nil, false)
	unsupported := strings.Join(bufferLines(setupRenderBuffer(dashboard, 140, 30)), "\n")
	if !strings.Contains(unsupported, "mbx: mbx 1.26.0 runs natively on this Mac") {
		t.Fatal(unsupported)
	}
	// Hard-won: #1330: a Mac's working native mbx was reported as missing.
	if strings.Contains(unsupported, "no mbx on the host") {
		t.Fatal(unsupported)
	}
	if strings.Contains(unsupported, "Pending application") || strings.Contains(unsupported, "Mj-managed build cache (mbx)") {
		t.Fatal(unsupported)
	}

	// The status line belongs to this page: leaving it takes the line along.
	shown := setupDialog(t, dashboard).notice
	if shown == nil || !strings.Contains(*shown, "build cache") {
		t.Fatalf("notice = %v", shown)
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	if notice := setupDialog(t, dashboard).notice; notice != nil {
		t.Fatalf("the build cache status outlived its page: %q", *notice)
	}
	dialog = setupDialog(t, dashboard)
	setupFocusSetting(dialog, "build_cache")
	dialog.prepare()
	dashboard.HandleKey(key(tea.KeyEnter))

	// Native tool caches remain selectable without mbx.
	dialog = setupDialog(t, dashboard)
	setupFocusSetting(dialog, "enabled")
	dialog.prepare()
	dashboard.HandleKey(key(tea.KeyEnter))
	setupSelectValue(t, dashboard, false)
	dialog = setupDialog(t, dashboard)
	if enabled := setupValuePath(dialog.draft, "machines", "local", "build_cache", "enabled"); !setupValueEqual(enabled, false) {
		t.Fatalf("the switch controls native tools even when mbx is unavailable: %s", setupValueDisplay(enabled))
	}

	// Changing a setting on the page makes the answer stale and asks again.
	setupValueInsert(&dialog.draft, []string{"machines", "local", "build_cache"}, "max_total_size", "1GiB")
	if _, ok := dashboard.HandleKey(key(tea.KeyDown)).(DashboardActionPreviewBuildCache); !ok {
		t.Fatal("a stale answer is resolved again")
	}
}

// On a host that supports the cache the switch is a checkbox: on, off, and
// back to unset, which means on.
//
// Hard-won: 39cce3de: the build-cache setting lacked a checkbox control.
func TestBuildCacheEnabledRoundTripsDefaultOnAndOffWithHostStats(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "machines")
	preview, ok := setupChoose(t, dashboard, "local").(DashboardActionPreviewBuildCache)
	if !ok {
		t.Fatal("preview build cache")
	}
	dialog := setupDialog(t, dashboard)
	setupFocusSetting(dialog, "build_cache")
	dialog.prepare()
	if action := dashboard.HandleKey(key(tea.KeyEnter)); !setupActionIsNone(action) {
		t.Fatalf("opening the cache page returned %#v", action)
	}
	dashboard.BuildCachePreviewed(preview.Generation, preview.Key, &core.BuildCachePreview{
		UserManaged:  false,
		Application:  core.BuildCacheApplicationPending{},
		NativeMbx:    setupStringPtr("1.12.0"),
		Directory:    setupStringPtr("/home/dev/.cache/mbx"),
		MaxTotalSize: core.BuildCacheLimitSize{Value: "100000000000B"},
		Stats: &core.BuildCacheStats{
			Builds:             155,
			CachedCompilations: 12050,
			AvoidedCompilerNs:  6_004_997_818_721,
			ReflinkedBytes:     47_612_059_386,
		},
	}, nil, false)
	checked := strings.Join(drawn(dashboard, 140, 30), "\n")
	if !strings.Contains(checked, "Default (on)") {
		t.Fatal(checked)
	}
	// The page otherwise only predicts; this line is the one thing on it
	// that says the cache is being used.
	for _, expected := range []string{"155 builds", "12050 compilations from cache", "1h 40m", "44.3"} {
		if !strings.Contains(checked, expected) {
			t.Fatalf("missing %q in\n%s", expected, checked)
		}
	}
	if strings.Contains(checked, "Off:") {
		t.Fatalf("a supported host explains nothing:\n%s", checked)
	}

	for _, value := range []any{true, false, nil} {
		setupChoose(t, dashboard, "enabled")
		setupSelectValue(t, dashboard, value)
		dialog := setupDialog(t, dashboard)
		if enabled := setupValuePath(dialog.draft, "machines", "local", "build_cache", "enabled"); !setupValueEqual(enabled, value) {
			t.Fatalf("enabled = %s, want %s", setupValueDisplay(enabled), setupValueDisplay(value))
		}
		saved, err := setupValueTo(setupSavedConfig(t, dialog))
		if err != nil {
			t.Fatal(err)
		}
		if enabled := setupValuePath(saved, "machines", "local", "build_cache", "enabled"); !setupValueEqual(enabled, value) {
			t.Fatalf("saved enabled = %s, want %s", setupValueDisplay(enabled), setupValueDisplay(value))
		}
	}
}

// Test-and-fix M-3: the machine's list row said "Enabled by default" while
// the build cache page, opened next, said the host cannot share the cache.
//
// Hard-won: b4c3a95d: the machine row contradicted the build-cache page on unsupported hosts.
func TestMachineRowReportsAnUnsupportedCacheHostLikeItsPage(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "machines")
	setupChoose(t, dashboard, "local")
	dialog := setupDialog(t, dashboard)
	// Opening the machine resolves its host, so the row does not have to
	// wait for the build cache page to be opened.
	_, previewKey, ok := dialog.buildCacheMachinePage()
	if !ok {
		t.Fatal("the machine page is open")
	}
	generation := dialog.generation
	if state, ok := dialog.buildCachePreviews["local"]; !ok || !setupValueEqual(state.key, previewKey) {
		t.Fatal("opening the machine resolves its host")
	}
	dashboard.BuildCachePreviewed(generation, previewKey, &core.BuildCachePreview{
		Application: core.BuildCacheApplicationPending{},
		OffReason:   core.BuildCacheOffUnsupportedPlatform{Value: "Shared mbx requires a Linux host"},
	}, nil, false)
	lines := drawn(dashboard, 160, 40)
	if !slices.ContainsFunc(lines, func(line string) bool {
		return strings.Contains(line, "Build caches") && strings.Contains(line, "mbx unavailable") &&
			!strings.Contains(line, "Enabled by default")
	}) {
		t.Fatalf("%q", lines)
	}

	// A Mac reports the mbx its bare sessions use, not a missing one.
	dashboard.BuildCachePreviewed(generation, previewKey, &core.BuildCachePreview{
		NativeMbx:   setupStringPtr("1.26.0"),
		Application: core.BuildCacheApplicationPending{},
		OffReason:   core.BuildCacheOffUnsupportedPlatform{Value: "mbx 1.26.0 runs natively on this Mac"},
	}, nil, false)
	lines = drawn(dashboard, 160, 40)
	if !slices.ContainsFunc(lines, func(line string) bool {
		return strings.Contains(line, "Build caches") && strings.Contains(line, "mbx 1.26.0, bare sessions only")
	}) {
		t.Fatalf("%q", lines)
	}
	if slices.ContainsFunc(lines, func(line string) bool {
		return strings.Contains(line, "mbx unavailable") || strings.Contains(line, "no mbx on the host")
	}) {
		t.Fatalf("%q", lines)
	}
}

func TestMachinePageConfirmsMbxInstallShowsBusyAndRefreshesAfterSuccess(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "machines")
	preview, ok := setupChoose(t, dashboard, "local").(DashboardActionPreviewBuildCache)
	if !ok {
		t.Fatal("preview build cache")
	}
	generation, previewKey := preview.Generation, preview.Key
	dashboard.BuildCachePreviewed(generation, previewKey, &core.BuildCachePreview{
		MbxProfileFile:    setupStringPtr("~/.profile"),
		MbxProfileWarning: setupStringPtr("Your login shell may not read ~/.profile. Add this PATH line to a startup file it reads:"),
		MbxManualPathLine: setupStringPtr("export PATH='/home/test/.local/share/mbx/bin:/home/test/.local/bin'${PATH:+:$PATH}"),
		Application:       core.BuildCacheApplicationPending{},
	}, nil, true)
	if !strings.Contains(strings.Join(drawn(dashboard, 140, 34), "\n"), "Install mbx") {
		t.Fatal("the machine page offers Install mbx")
	}

	setupActivate(t, dashboard, SetupControlInstallMbx)
	confirm, ok := dashboard.mode.(ModeConfirm)
	if !ok {
		t.Fatal("confirmation")
	}
	_, body := confirmationBody(confirm.confirmation, nil, 72)
	rendered := make([]string, len(body))
	for i, line := range body {
		rendered[i] = line.String()
	}
	prompt := strings.Join(rendered, "\n")
	for _, expected := range []string{
		"the local machine",
		"mbx setup --yes",
		"~/.profile",
		"may not read ~/.profile",
		"export PATH='/home/test/.local/share/mbx/bin:/home/test/.local/bin'${PATH:+:$PATH}",
	} {
		if !strings.Contains(prompt, expected) {
			t.Fatalf("missing %q: %s", expected, prompt)
		}
	}

	dashboard.HandleKey(key(tea.KeyRight))
	install, ok := dashboard.HandleKey(key(tea.KeyEnter)).(DashboardActionInstallMbx)
	if !ok {
		t.Fatal("confirmed mbx install")
	}
	if install.Generation != generation || !setupValueEqual(install.Key, previewKey) || install.MachineID != "local" {
		t.Fatalf("install = %#v", install)
	}
	if !dashboard.MbxInstallStarted(generation, previewKey, "local") {
		t.Fatal("the install starts")
	}
	busy := strings.Join(drawn(dashboard, 140, 34), "\n")
	if !strings.Contains(busy, "Installing mbx…") {
		t.Fatal(busy)
	}

	dashboard.MbxInstallFinished(generation, previewKey, "local",
		"Installed mbx at /home/test/.local/bin/mbx (version 1.22.0). Updated ~/.profile. Your login shell may not read ~/.profile. Add this PATH line: export PATH='/home/test/.local/share/mbx/bin:/home/test/.local/bin'${PATH:+:$PATH}",
		nil,
		&core.BuildCachePreview{
			NativeMbx:   setupStringPtr("1.22.0"),
			Directory:   setupStringPtr("/home/test/.cache/mbx"),
			UserManaged: true,
			Application: core.BuildCacheApplicationPending{},
		}, nil, false)
	finished := strings.Join(drawn(dashboard, 140, 34), "\n")
	if !strings.Contains(finished, "Installed mbx at /home/test/.local/bin/mbx") ||
		!strings.Contains(finished, "Updated ~/.profile") {
		t.Fatal(finished)
	}
	if setupHasInstallMbx(setupDialog(t, dashboard)) {
		t.Fatal("an installed mbx is not offered again")
	}
}

func TestMachinePageLabelsOldMbxAsUpgradeAndHidesUnavailableActions(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "machines")
	preview, ok := setupChoose(t, dashboard, "local").(DashboardActionPreviewBuildCache)
	if !ok {
		t.Fatal("preview build cache")
	}
	dashboard.BuildCachePreviewed(preview.Generation, preview.Key, &core.BuildCachePreview{
		NativeMbx:      setupStringPtr("1.21.0"),
		MbxProfileFile: setupStringPtr("~/.bash_profile"),
		UserManaged:    true,
		Application:    core.BuildCacheApplicationPending{},
		OffReason:      core.BuildCacheOffUnavailable{Value: "the host's mbx 1.21.0 is older than the pinned release"},
	}, nil, true)
	dialog := setupDialog(t, dashboard)
	if !slices.ContainsFunc(dialog.actions(), func(button setupButtonSpec) bool {
		return button.ID == SetupControlInstallMbx && button.Label == "Upgrade mbx" && button.Enabled
	}) {
		t.Fatal("an old mbx is offered an upgrade")
	}

	state := dialog.buildCachePreviews["local"]
	state.installMbxAvailable = false
	dialog.buildCachePreviews["local"] = state
	dialog.prepare()
	if setupHasInstallMbx(dialog) {
		t.Fatal("an unavailable install is not offered")
	}
	state = dialog.buildCachePreviews["local"]
	state.result = BuildCachePreviewResultReady{value: &core.BuildCachePreview{
		Application: core.BuildCacheApplicationPending{},
		OffReason:   core.BuildCacheOffUnsupportedPlatform{Value: "Shared mbx requires a Linux host"},
	}}
	dialog.buildCachePreviews["local"] = state
	dialog.prepare()
	if setupHasInstallMbx(dialog) {
		t.Fatal("an unsupported host is not offered an install")
	}
}

func TestMachinePageReportsMbxInstallFailureAndPreservesTheRetryAction(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "machines")
	preview, ok := setupChoose(t, dashboard, "local").(DashboardActionPreviewBuildCache)
	if !ok {
		t.Fatal("preview build cache")
	}
	generation, previewKey := preview.Generation, preview.Key
	absent := func() *core.BuildCachePreview {
		return &core.BuildCachePreview{
			MbxProfileFile: setupStringPtr("~/.profile"),
			Application:    core.BuildCacheApplicationPending{},
		}
	}
	dashboard.BuildCachePreviewed(generation, previewKey, absent(), nil, true)
	if !dashboard.MbxInstallStarted(generation, previewKey, "local") {
		t.Fatal("the install starts")
	}
	dashboard.MbxInstallFinished(generation, previewKey, "local", "",
		errors.New("mbx setup exited with status 1: setup refused"), absent(), nil, true)
	lines := strings.Join(drawn(dashboard, 140, 34), "\n")
	if !strings.Contains(lines, "Could not install mbx: mbx setup exited") || !strings.Contains(lines, "Install mbx") {
		t.Fatal(lines)
	}
}

// Hard-won: Settings review: off-page Save bypassed a late mbx ownership reply.
func TestHostOwnedPendingCacheEditsAreDiscardedBeforeSavingFromAnotherPage(t *testing.T) {
	for _, test := range []struct{ replyOnCachePage, saveStarted bool }{
		{true, false}, {false, false}, {false, true},
	} {
		dashboard := dashboardWithSession(stoppedSession())
		builder, err := core.UnmarshalMachineJSON([]byte(`{"kind":"ssh", "host":"builder"}`))
		if err != nil {
			t.Fatal(err)
		}
		dashboard.config.Mut().Machines["builder"] = builder
		dashboard.BeginSetup()
		setupChoose(t, dashboard, "machines")
		setupChoose(t, dashboard, "local")
		setupChoose(t, dashboard, "build_cache")
		setupChoose(t, dashboard, "max_total_size")
		setupReplaceText(dashboard, "500")
		if test.saveStarted {
			setupChoose(t, dashboard, "directory")
			setupReplaceText(dashboard, "~/cache")
			if _, ok := dashboard.HandleKey(setupCtrl('s')).(DashboardActionResolveSetupPath); !ok {
				t.Fatal("Save expands the target path first")
			}
		}
		dialog := setupDialog(t, dashboard)
		_, cacheKey, _ := dialog.buildCachePage()
		generation := dialog.generation
		path := []string{"machines", "local", "build_cache", "max_total_size"}
		if !dialog.isDirty() {
			t.Fatal("the pending budget is an edit")
		}
		if !test.replyOnCachePage {
			for range 3 {
				dashboard.HandleKey(key(tea.KeyEscape))
			}
			setupChoose(t, dashboard, "machines")
			setupChoose(t, dashboard, "builder")
		}
		dashboard.BuildCachePreviewed(generation, cacheKey, &core.BuildCachePreview{
			NativeMbx:   setupStringPtr("1.21.0"),
			Directory:   setupStringPtr("/native/cache"),
			UserManaged: true,
			Application: core.BuildCacheApplicationApplied{},
		}, nil, false)
		dialog = setupDialog(t, dashboard)
		if _, ok := dialog.fields.get(path); ok {
			t.Fatal("unfocusable buffers must not remain pending")
		}
		if dialog.isDirty() {
			t.Fatal("the discarded pending edit was the only change")
		}
		if notice := dialog.notice; notice == nil || !strings.Contains(*notice, "Discarded pending") ||
			!strings.Contains(*notice, "local") || !strings.Contains(*notice, "mbx installation owns") {
			t.Fatalf("notice = %v", notice)
		}
		for len(setupDialog(t, dashboard).path) > 0 {
			dashboard.HandleKey(key(tea.KeyEscape))
		}
		var action DashboardAction
		if test.saveStarted {
			resumed, ok := dashboard.TakePrerequisiteCheck()
			if !ok {
				t.Fatal("the accepted Save resumes after its owned field is dropped")
			}
			action = resumed
		} else {
			action = dashboard.HandleKey(setupCtrl('s'))
		}
		save, ok := action.(DashboardActionSaveSetup)
		if !ok {
			t.Fatalf("discarded host-owned edits should not block saving: %#v", action)
		}
		saved := setupJSON(t, save.Updated)
		if directory := setupValuePath(saved, "machines", "local", "build_cache", "directory"); directory != nil {
			t.Fatalf("directory = %s", setupValueDisplay(directory))
		}
		if budget := setupValuePath(saved, "machines", "local", "build_cache", "max_total_size"); budget != nil {
			t.Fatalf("the discarded edit must never reach the saved config: %s", save.Updated)
		}
	}
}

// Hard-won: Settings review round 2: off-page installs retained their pre-install preview.
func TestMbxInstallCompletionRefreshesItsMachineAfterNavigation(t *testing.T) {
	preview := func(installed bool) *core.BuildCachePreview {
		value := &core.BuildCachePreview{UserManaged: installed, Application: core.BuildCacheApplicationPending{}}
		if installed {
			value.NativeMbx = setupStringPtr("1.22.0")
			value.Directory = setupStringPtr("/native/cache")
		} else {
			value.MbxProfileFile = setupStringPtr("~/.profile")
		}
		return value
	}
	for _, test := range []struct{ refreshSucceeds, draftChanges bool }{
		{true, false}, {false, false}, {true, true}, {false, true},
	} {
		dashboard := dashboardWithSession(stoppedSession())
		builder, err := core.UnmarshalMachineJSON([]byte(`{"kind":"ssh", "host":"builder"}`))
		if err != nil {
			t.Fatal(err)
		}
		dashboard.config.Mut().Machines["builder"] = builder
		dashboard.BeginSetup()
		setupChoose(t, dashboard, "machines")
		local, ok := setupChoose(t, dashboard, "local").(DashboardActionPreviewBuildCache)
		if !ok {
			t.Fatal("local preview")
		}
		generation, localKey := local.Generation, local.Key
		dashboard.BuildCachePreviewed(generation, localKey, preview(false), nil, true)
		if !dashboard.MbxInstallStarted(generation, localKey, "local") {
			t.Fatal("the install starts")
		}
		if test.draftChanges {
			setupChoose(t, dashboard, "build_cache")
			setupChoose(t, dashboard, "enabled")
			setupSelectValue(t, dashboard, false)
			dashboard.HandleKey(key(tea.KeyEscape))
		}
		dashboard.HandleKey(key(tea.KeyEscape))
		if _, ok := setupChoose(t, dashboard, "builder").(DashboardActionPreviewBuildCache); !ok {
			t.Fatal("opening builder resolves its host")
		}
		dialog := setupDialog(t, dashboard)
		localBefore := dialog.buildCachePreviews["local"]
		builderBefore := dialog.buildCachePreviews["builder"]
		var refresh *core.BuildCachePreview
		var refreshErr error
		if test.refreshSucceeds {
			refresh = preview(true)
		} else {
			refreshErr = errors.New("host preview unavailable")
		}
		dashboard.MbxInstallFinished(generation, localKey, "local", "Installed mbx", nil, refresh, refreshErr, false)
		dialog = setupDialog(t, dashboard)
		if !setupPreviewStateEqual(dialog.buildCachePreviews["builder"], builderBefore) {
			t.Fatal("install completion must only update its own machine")
		}
		if dialog.mbxInstall != nil {
			t.Fatal("the install is still running")
		}
		if test.draftChanges {
			if !setupPreviewStateEqual(dialog.buildCachePreviews["local"], localBefore) {
				t.Fatal("a stale install key must not overwrite or invalidate the newer resolution")
			}
			continue
		}
		if test.refreshSucceeds {
			ready, ok := dialog.buildCachePreviews["local"].result.(BuildCachePreviewResultReady)
			if !ok || ready.value == nil || ready.value.NativeMbx == nil || *ready.value.NativeMbx != "1.22.0" || !ready.value.UserManaged {
				t.Fatalf("local = %#v", dialog.buildCachePreviews["local"].result)
			}
			if dialog.buildCachePreviews["local"].installMbxAvailable {
				t.Fatal("an installed mbx is not offered again")
			}
		} else {
			if _, ok := dialog.buildCachePreviews["local"]; ok {
				t.Fatal("a failed post-install refresh must invalidate the pre-install cache")
			}
			if dialog.notice == nil || !strings.Contains(*dialog.notice, "Could not refresh the build cache preview: host preview unavailable") {
				t.Fatalf("notice = %v", dialog.notice)
			}
		}
		dashboard.HandleKey(key(tea.KeyEscape))
		revisit := setupChoose(t, dashboard, "local")
		if test.refreshSucceeds {
			if !setupActionIsNone(revisit) {
				t.Fatalf("revisit = %#v", revisit)
			}
		} else {
			if _, ok := revisit.(DashboardActionPreviewBuildCache); !ok {
				t.Fatal("revisit must retry after invalidation")
			}
			dashboard.BuildCachePreviewed(generation, localKey, preview(true), nil, false)
		}
		if setupHasInstallMbx(setupDialog(t, dashboard)) {
			t.Fatal("an installed mbx is not offered again")
		}
		setupChoose(t, dashboard, "build_cache")
		dialog = setupDialog(t, dashboard)
		for _, field := range []string{"directory", "max_total_size"} {
			if !slices.ContainsFunc(dialog.descriptors, func(control SettingControl) bool {
				_, readOnly := control.kind.(SettingKindReadOnly)
				return control.path[len(control.path)-1] == field && readOnly
			}) {
				t.Fatalf("fresh ownership must protect %s", field)
			}
		}
	}
}
