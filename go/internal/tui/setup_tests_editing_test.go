// Port of mj-tui/src/setup/tests/editing.rs.

package tui

import (
	"encoding/json"
	"errors"
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestSetupAddsARemoteRuntimeAndReportsInvalidFieldsWithoutLosingTheDraft(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "machines")
	dashboard.HandleKey(key('a'))
	dashboard.HandlePaste("builder")
	dashboard.HandleKey(key(tea.KeyEnter))
	action := dashboard.HandleKey(setupCtrl('s'))
	save, ok := action.(DashboardActionSaveSetup)
	if !ok {
		t.Fatalf("expected background validation, got %#v", action)
	}
	var invalid core.Config
	if err := json.Unmarshal([]byte(save.Updated), &invalid); err != nil {
		t.Fatal(err)
	}
	validation := invalid.Validate()
	if validation == nil {
		t.Fatal("a machine without a host validates")
	}
	dashboard.SetupSaved(save.Generation, core.Config{}, errors.New(validation.Error()))
	dialog := setupDialog(t, dashboard)
	if dialog.notice == nil || !strings.Contains(*dialog.notice, "SSH host") {
		t.Fatalf("notice = %v", dialog.notice)
	}
	setupChoose(t, dashboard, "host")
	dashboard.HandlePaste("builder.example.test")
	dashboard.HandleKey(key(tea.KeyEnter))
	setupActivate(t, dashboard, SetupControlBack)
	setupActivate(t, dashboard, SetupControlBack)

	// A runtime on the new machine: Docker, chosen from the runtime kinds,
	// and the machine, chosen from the machines the draft now has.
	setupChoose(t, dashboard, "targets")
	dashboard.HandleKey(key('a'))
	dashboard.HandlePaste("builder-docker")
	dashboard.HandleKey(key(tea.KeyEnter))
	setupChoose(t, dashboard, "kind")
	setupSelectChoice(t, dashboard, "docker")
	setupChoose(t, dashboard, "machine")
	setupSelectChoice(t, dashboard, "builder")

	action = dashboard.HandleKey(setupCtrl('s'))
	save, ok = action.(DashboardActionSaveSetup)
	if !ok {
		t.Fatalf("%#v", action)
	}
	var saved core.Config
	if err := json.Unmarshal([]byte(save.Updated), &saved); err != nil {
		t.Fatal(err)
	}
	docker, ok := saved.Targets["builder-docker"].(core.TargetTemplateSshDocker)
	if !ok || docker.SSH.Host != "builder.example.test" {
		t.Fatalf("builder-docker = %#v", saved.Targets["builder-docker"])
	}
	generation := setupDialog(t, dashboard).generation
	dashboard.SetupSaved(generation, core.Config{}, errors.New("disk full"))
	dialog = setupDialog(t, dashboard)
	if dialog.saving {
		t.Fatal("a failed save leaves Settings saving")
	}
	if host := setupValuePath(dialog.draft, "machines", "builder", "host"); !setupValueEqual(host, "builder.example.test") {
		t.Fatalf("host = %s", setupValueDisplay(host))
	}
	if dialog.notice == nil || !strings.Contains(*dialog.notice, "disk full") {
		t.Fatalf("notice = %v", dialog.notice)
	}
}

// Campaign finding W-1: the second Esc out of a dirty Settings draft opens
// the "Discard Settings changes?" prompt, which looks like the dashboard
// behind it. Esc on that prompt means "Keep editing", so a third Esc returns
// to Settings. Once the draft is discarded, Esc on the dashboard is inert.
func TestEscapeOnTheDiscardPromptKeepsEditingAndDashboardEscapeStaysInert(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.config.Mut().Review.Reviewers = []core.SubagentChoice{{Model: "review-model"}}
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "review")
	// Model, effort, then the reviewer's Remove button.
	dashboard.HandleKey(key(tea.KeyTab))
	dashboard.HandleKey(key(tea.KeyTab))
	dashboard.HandleKey(key(tea.KeyEnter))
	dashboard.HandleKey(key(tea.KeyEscape))
	dashboard.HandleKey(key(tea.KeyEscape))
	if _, ok := dashboard.mode.(ModeConfirm); !ok {
		t.Fatal("the second Esc asks before discarding")
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	setup, ok := dashboard.mode.(ModeSetup)
	if !ok {
		t.Fatal("Esc on the discard prompt keeps editing")
	}
	if reviewers := setupValuePath(setup.draft, "review", "reviewers"); !setupValueEqual(reviewers, []any{}) {
		t.Fatalf("reviewers = %s", setupValueDisplay(reviewers))
	}
	// Discard, then Esc on the plain dashboard does nothing.
	dashboard.HandleKey(key(tea.KeyEscape))
	if _, ok := dashboard.mode.(ModeConfirm); !ok {
		t.Fatal("Esc asks before discarding again")
	}
	dashboard.HandleKey(key(tea.KeyRight))
	dashboard.HandleKey(key(tea.KeyEnter))
	if dashboard.ModalOpen() {
		t.Fatal("discarding closes Settings")
	}
	if action := dashboard.HandleKey(key(tea.KeyEscape)); !setupActionIsNone(action) {
		t.Fatalf("Esc on the dashboard returned %#v", action)
	}
	if dashboard.ModalOpen() {
		t.Fatal("Esc on the dashboard opened a modal")
	}
}

// A refused value is reported with the field it was typed into, not on the
// dialog's bottom rows a page below it.
//
// Hard-won: 9e96756a: field validation errors appeared far below the edited field.
func TestARejectedFieldValueIsReportedUnderTheField(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "machines")
	setupChoose(t, dashboard, "local")
	setupChoose(t, dashboard, "build_cache")
	setupChoose(t, dashboard, "max_total_size")
	backgroundNotice := setupCopyString(setupDialog(t, dashboard).notice)
	for _, typed := range []rune{'1', '.', '5'} {
		dashboard.HandleKey(key(typed))
	}
	dashboard.HandleKey(key(tea.KeyEnter))
	dialog := setupDialog(t, dashboard)
	if err := setupFocusedField(t, dialog).err; err == nil || *err != "Enter a whole number of gigabytes." {
		t.Fatalf("field error = %v", err)
	}
	if !setupStringsEqual(dialog.notice, backgroundNotice) {
		t.Fatal("field validation must not replace a background notice")
	}
	lines := drawn(dashboard, 140, 30)
	field := point(lines, "1.5").Y
	message := point(lines, "Enter a whole number of gigabytes.").Y
	if message != field+1 {
		t.Fatalf("the message is %d rows from the field:\n%s", message-field, strings.Join(lines, "\n"))
	}
}

// setupStringsEqual is Option<String> equality.
func setupStringsEqual(a, b *string) bool {
	return (a == nil) == (b == nil) && (a == nil || *a == *b)
}

// The numeric settings are edited as text but saved as numbers, "Use
// default" puts the default back, and text that is not a number is refused
// under the field without losing the draft. Launch campaign finding C-23.
//
// Hard-won: ef88371a: numeric field text failed to convert during save.
func TestNumericSettingsRoundTripUseTheirDefaultAndRefuseText(t *testing.T) {
	fields := []struct {
		section, key string
		read         func(*core.Config) uint64
		defaulted    uint64
	}{
		{"notify", "delay_seconds", func(c *core.Config) uint64 { return c.Notify.DelaySeconds }, 2},
		{"sessionwiki", "archive_after_days", func(c *core.Config) uint64 {
			if c.Sessionwiki.ArchiveAfterDays == nil {
				return 0
			}
			return uint64(*c.Sessionwiki.ArchiveAfterDays)
		}, 0},
		{"subagents", "max_concurrent", func(c *core.Config) uint64 { return uint64(c.Subagents.MaxConcurrent) }, 8},
	}
	for _, field := range fields {
		configuration := config()
		dialog := newSetupDialog(&configuration)
		setupEditField(t, dialog, field.section, field.key, "5")
		if err := setupApplyFocused(t, dialog, false); err != nil {
			t.Fatalf("%s.%s: %v", field.section, field.key, err)
		}
		if value := setupValuePath(dialog.draft, field.section, field.key); !setupValueEqual(value, int64(5)) {
			t.Fatalf("%s.%s = %s", field.section, field.key, setupValueDisplay(value))
		}
		saved := setupSavedConfig(t, dialog)
		if got := field.read(&saved); got != 5 {
			t.Fatalf("%s.%s saved %d", field.section, field.key, got)
		}

		setupEditField(t, dialog, field.section, field.key, "5")
		if err := setupApplyFocused(t, dialog, true); err != nil {
			t.Fatalf("%s.%s: %v", field.section, field.key, err)
		}
		saved = setupSavedConfig(t, dialog)
		if got := field.read(&saved); got != field.defaulted {
			t.Fatalf("%s.%s saved %d after clearing, want %d", field.section, field.key, got, field.defaulted)
		}

		setupEditField(t, dialog, field.section, field.key, "five")
		err := setupApplyFocused(t, dialog, false)
		if err == nil || !strings.HasPrefix(err.Error(), "Enter a whole number") {
			t.Fatalf("%s.%s: %v", field.section, field.key, err)
		}
		if setupFocusedField(t, dialog).err == nil {
			t.Fatalf("%s.%s: the invalid field retains its error", field.section, field.key)
		}
	}
}

func TestUserManagedBudgetsShowHostValuesAndCannotBeEdited(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "machines")
	setupChoose(t, dashboard, "local")
	setupChoose(t, dashboard, "build_cache")
	dialog := setupDialog(t, dashboard)
	_, previewKey, ok := dialog.buildCachePage()
	if !ok {
		t.Fatal("the build cache page is open")
	}
	version, directory, budget := "1.21.0", "/native/cache", "500GB"
	dialog.buildCachePreviews["local"] = BuildCachePreviewState{
		key:                 previewKey,
		installMbxAvailable: false,
		result: BuildCachePreviewResultReady{value: &core.BuildCachePreview{
			NativeMbx:    &version,
			Directory:    &directory,
			MaxTotalSize: core.BuildCacheLimitHostConfiguration{Value: &budget},
			UserManaged:  true,
			Application:  core.BuildCacheApplicationApplied{},
		}},
	}
	dialog.prepare()
	text := strings.Join(drawn(dashboard, 140, 30), "\n")
	if !strings.Contains(text, "User-managed mbx") || !strings.Contains(text, "500 GB, host mbx config") {
		t.Fatal(text)
	}
	dialog = setupDialog(t, dashboard)
	if !slices.ContainsFunc(dialog.descriptors, func(control SettingControl) bool {
		_, readOnly := control.kind.(SettingKindReadOnly)
		return control.path[len(control.path)-1] == "max_total_size" && readOnly
	}) {
		t.Fatal("the budget is read-only")
	}
	for range 8 {
		dashboard.HandleKey(key(tea.KeyTab))
	}
	dialog = setupDialog(t, dashboard)
	if path, ok := dialog.focusedPath(); ok && len(path) > 0 && path[len(path)-1] == "max_total_size" {
		t.Fatal("focus landed on a read-only budget")
	}
	if budget := setupValuePath(dialog.draft, "machines", "local", "build_cache", "max_total_size"); budget != nil {
		t.Fatalf("budget = %s", setupValueDisplay(budget))
	}
}

func TestPendingTextSurvivesBackCountsAsDirtyAndDoesNotTriggerShortcuts(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.beginSettingsSection("phone", nil)
	setupChoose(t, dashboard, "bind")
	setupReplaceText(dashboard, "a/path")
	dialog := setupDialog(t, dashboard)
	if !dialog.isDirty() || dialog.search != nil || dialog.editor != nil {
		t.Fatalf("dirty %t, search %t, editor %t", dialog.isDirty(), dialog.search != nil, dialog.editor != nil)
	}
	if bind := setupValuePath(dialog.draft, "phone", "bind"); setupValueEqual(bind, "a/path") {
		t.Fatal("typing applied the value")
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	setupChoose(t, dashboard, "phone")
	setupChoose(t, dashboard, "bind")
	if value := setupFocusedField(t, setupDialog(t, dashboard)).input.text().Value(); value != "a/path" {
		t.Fatalf("field = %q", value)
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	dashboard.HandleKey(key(tea.KeyEscape))
	if _, ok := dashboard.mode.(ModeConfirm); !ok {
		t.Fatal("pending text needs the Settings discard guard")
	}
}

func TestSaveRefusesInvalidPendingTextAndRevealsTheFieldOnItsPage(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.beginSettingsSection("subagents", nil)
	setupChoose(t, dashboard, "max_concurrent")
	setupReplaceText(dashboard, "many")
	dashboard.HandleKey(key(tea.KeyEscape))
	if action := dashboard.HandleKey(setupCtrl('s')); !setupActionIsNone(action) {
		t.Fatalf("save returned %#v", action)
	}
	dialog := setupDialog(t, dashboard)
	if dialog.saving {
		t.Fatal("an invalid draft saved")
	}
	if !slices.Equal(dialog.path, []string{"subagents"}) {
		t.Fatalf("path = %v", dialog.path)
	}
	if path, _ := dialog.focusedPath(); !slices.Equal(path, []string{"subagents", "max_concurrent"}) {
		t.Fatalf("focused %v", path)
	}
	field := setupFocusedField(t, dialog)
	if field.input.text().Value() != "many" || field.err == nil || !strings.Contains(*field.err, "whole number") {
		t.Fatalf("field %q, error %v", field.input.text().Value(), field.err)
	}
	rendered := strings.Join(drawn(dashboard, 100, 20), "\n")
	if !strings.Contains(rendered, "many") || !strings.Contains(rendered, "Enter a whole number") {
		t.Fatal(rendered)
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	setupChoose(t, dashboard, "review")
	if setupDialog(t, dashboard).reviewEditor == nil {
		t.Fatal("Code Review opens")
	}
	if action := dashboard.HandleKey(setupCtrl('s')); !setupActionIsNone(action) {
		t.Fatalf("save returned %#v", action)
	}
	dialog = setupDialog(t, dashboard)
	if dialog.reviewEditor != nil {
		t.Fatal("Save must reveal the invalid Settings field even from Code Review")
	}
	if path, _ := dialog.focusedPath(); !slices.Equal(path, []string{"subagents", "max_concurrent"}) {
		t.Fatalf("focused %v", path)
	}
}

func TestEnterRevealsValidationWhenTheFocusedFieldIsScrolledAway(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	entry := "codex-1"
	dashboard.beginSettingsSection("profiles", &entry)
	setupChoose(t, dashboard, "context_window_bytes")
	setupReplaceText(dashboard, "many")
	lines := drawn(dashboard, 100, 14)
	at := point(lines, "many")
	for range 8 {
		dashboard.HandleMouse(setupWheel(false, at))
		drawn(dashboard, 100, 14)
	}
	scrolled := drawn(dashboard, 100, 14)
	if slices.Equal(scrolled, lines) {
		t.Fatal("scrolling must actually move the content")
	}
	if slices.ContainsFunc(scrolled, func(line string) bool { return strings.Contains(line, "many") }) {
		t.Fatalf("the pending field must scroll out of view: %q", scrolled)
	}
	dashboard.HandleKey(key(tea.KeyEnter))
	rendered := strings.Join(drawn(dashboard, 100, 14), "\n")
	if !strings.Contains(rendered, "many") || !strings.Contains(rendered, "Enter a whole number") {
		t.Fatal(rendered)
	}
}

// Hard-won: Settings review: pre-event preparation erased the field cursor map.
func TestClickingInsideAnInlineFieldPlacesTheCursorAtThatCharacter(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.beginSettingsSection("phone", nil)
	setupChoose(t, dashboard, "bind")
	setupReplaceText(dashboard, "abcdef")
	lines := drawn(dashboard, 140, 40)
	at := point(lines, "abcdef")
	setupClickAt(dashboard, uv.Pos(at.X+3, at.Y), 140, 40)
	dashboard.HandleKey(key('X'))
	if value := setupFocusedField(t, setupDialog(t, dashboard)).input.text().Value(); value != "abcXdef" {
		t.Fatalf("field = %q", value)
	}
}

// Hard-won: Settings review: removing Clear made nullable Permissions impossible to reset.
func TestNullablePermissionsDropdownRestoresDefaultAfterAConcreteChoice(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	bare, err := core.UnmarshalTargetTemplateJSON([]byte(`{"kind":"ssh-bare","host":"builder","permissions":"guardian"}`))
	if err != nil {
		t.Fatal(err)
	}
	dashboard.config.Mut().Targets["bare"] = bare
	entry := "bare"
	dashboard.beginSettingsSection("targets", &entry)
	setupChoose(t, dashboard, "permissions")
	dialog := setupDialog(t, dashboard)
	values, _ := setupOpenChoices(t, dialog)
	if !slices.ContainsFunc(values, func(value any) bool { return value == nil }) {
		t.Fatalf("no Default among %s", setupValueDisplay(values))
	}
	label := dialog.choiceLabel([]string{"targets", "bare", "permissions"}, nil)
	if label != "Default (Ask for approvals)" {
		t.Fatalf("label = %q", label)
	}
	if !strings.Contains(strings.Join(drawn(dashboard, 140, 40), "\n"), label) {
		t.Fatal("the Default label is not drawn")
	}
	setupSelectValue(t, dashboard, nil)
	saved := setupSavedConfig(t, setupDialog(t, dashboard))
	if target, ok := saved.Targets["bare"].(core.TargetTemplateSshBare); !ok || target.Permissions != core.PermissionModeGuardian {
		t.Fatalf("the default must resolve through the normal configuration reader: %#v", saved.Targets["bare"])
	}
	setupChoose(t, dashboard, "permissions")
	values, selected := setupOpenChoices(t, setupDialog(t, dashboard))
	if values[selected] != nil {
		t.Fatalf("an unset value must display its Default choice: %s", setupValueDisplay(values[selected]))
	}
	setupSelectChoice(t, dashboard, "yolo")
	setupChoose(t, dashboard, "permissions")
	setupSelectValue(t, dashboard, nil)
	if permissions := setupValuePath(setupDialog(t, dashboard).draft, "targets", "bare", "permissions"); permissions != nil {
		t.Fatalf("permissions = %s", setupValueDisplay(permissions))
	}
}

// Hard-won: Settings live test: corrected archive text left a duplicate validation notice.
func TestInlineValidationClearsWithItsFieldWithoutReplacingBackgroundNotices(t *testing.T) {
	for _, test := range []struct{ save, backgroundFailure bool }{
		{false, false}, {true, false}, {false, true}, {true, true},
	} {
		dashboard := dashboardWithSession(stoppedSession())
		dashboard.beginSettingsSection("sessionwiki", nil)
		setupChoose(t, dashboard, "archive_after_days")
		setupReplaceText(dashboard, "invalid-days")
		generation := setupDialog(t, dashboard).generation
		if test.backgroundFailure {
			dashboard.ArchiveSpacePreviewed(generation, nil, core.ArchiveSpacePreview{}, errors.New("archive read failed"))
		}
		notice := setupCopyString(setupDialog(t, dashboard).notice)
		if test.backgroundFailure {
			if notice == nil || !strings.Contains(*notice, "archive read failed") {
				t.Fatalf("notice = %v", notice)
			}
		} else if notice != nil {
			t.Fatalf("notice = %q", *notice)
		}
		if test.save {
			dashboard.HandleKey(setupCtrl('s'))
		} else {
			dashboard.HandleKey(key(tea.KeyEnter))
		}
		dialog := setupDialog(t, dashboard)
		if err := setupFocusedField(t, dialog).err; err == nil || !strings.Contains(*err, "Enter a whole number of days") {
			t.Fatalf("field error = %v", err)
		}
		if !setupStringsEqual(dialog.notice, notice) {
			t.Fatal("inline validation must not create or replace a page notice")
		}
		rendered := strings.Join(drawn(dashboard, 140, 42), "\n")
		if count := strings.Count(rendered, "Enter a whole number of days"); count != 1 {
			t.Fatalf("the validation message should appear only beneath the field: %s", rendered)
		}
		setupReplaceText(dashboard, "8")
		dialog = setupDialog(t, dashboard)
		if setupFocusedField(t, dialog).err != nil {
			t.Fatal("corrected text keeps its error")
		}
		if !setupStringsEqual(dialog.notice, notice) {
			t.Fatal("typing replaced the page notice")
		}
		days := uint32(8)
		dashboard.ArchiveSpacePreviewed(generation, &days, core.ArchiveSpacePreview{}, nil)
		rendered = strings.Join(drawn(dashboard, 140, 42), "\n")
		if strings.Contains(rendered, "Enter a whole number of days") {
			t.Fatalf("corrected pending text must remove its validation error: %s", rendered)
		}
		if !strings.Contains(rendered, "would reclaim") {
			t.Fatalf("the estimate must follow the pending 8: %s", rendered)
		}
		dashboard.HandleKey(key(tea.KeyEnter))
		dialog = setupDialog(t, dashboard)
		if setupFocusedField(t, dialog).err != nil {
			t.Fatal("an accepted value keeps its error")
		}
		if days := setupValuePath(dialog.draft, "sessionwiki", "archive_after_days"); !setupValueEqual(days, int64(8)) {
			t.Fatalf("archive_after_days = %s", setupValueDisplay(days))
		}
		if !setupStringsEqual(dialog.notice, notice) {
			t.Fatal("successful field validation must preserve unrelated background notices too")
		}
	}
}
