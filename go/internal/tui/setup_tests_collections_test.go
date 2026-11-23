// Port of mj-tui/src/setup/tests/collections.rs.

package tui

import (
	"maps"
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestThisMachineIsAlwaysListedAndCannotBeRemoved(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "machines")
	if keys := setupDialog(t, dashboard).keys(); !slices.Equal(keys, []string{"local"}) {
		t.Fatalf("keys = %v", keys)
	}
	setupChoose(t, dashboard, "local")
	dialog := setupDialog(t, dashboard)
	if !slices.ContainsFunc(dialog.actions(), func(button setupButtonSpec) bool {
		return button.ID == SetupControlRemove && !button.Enabled
	}) {
		t.Fatal("Remove is offered disabled on this machine")
	}
	dashboard.HandleKey(key(tea.KeyDelete))
	dialog = setupDialog(t, dashboard)
	if !setupValueHas(setupValueGet(dialog.draft, "machines"), "local") {
		t.Fatal("this machine was removed")
	}
	if keys := dialog.keys(); !slices.Equal(keys, []string{"build_cache"}) {
		t.Fatalf("keys = %v", keys)
	}
}

func TestTheSessionwikiPageEstimatesWhatAnArchiveWindowWouldReclaim(t *testing.T) {
	used := core.ArchiveSpacePreview{
		Sessions:            40,
		Bytes:               5_153_960_755,
		ReclaimableSessions: 0,
		ReclaimableBytes:    0,
	}
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	dialog := setupDialog(t, dashboard)
	setupFocusSetting(dialog, "sessionwiki")
	dialog.prepare()

	// Entering the page measures what sessions use now, exactly once.
	preview, ok := dashboard.HandleKey(key(tea.KeyEnter)).(DashboardActionPreviewArchiveSpace)
	if !ok || preview.OlderThanDays != nil {
		t.Fatal("entering the page must ask for the space sessions use")
	}
	generation := preview.Generation
	if action := dashboard.HandleKey(key(tea.KeyDown)); !setupActionIsNone(action) {
		t.Fatalf("moving asked again: %#v", action)
	}
	rendered := func() string {
		return strings.Join(bufferLines(setupRenderBuffer(dashboard, 140, 30)), "\n")
	}
	if !strings.Contains(rendered(), "Resolving…") {
		t.Fatal("the estimate is resolving")
	}

	dashboard.ArchiveSpacePreviewed(generation, nil, used, nil)
	if never := rendered(); !strings.Contains(never, "Never · sessions use 4.8G") {
		t.Fatalf("the row must report the space sessions use:\n%s", never)
	}

	// Typing a number asks again for that number, keystroke by keystroke.
	dialog = setupDialog(t, dashboard)
	setupFocusSetting(dialog, "archive_after_days")
	dialog.prepare()
	dashboard.HandleKey(key(tea.KeyEnter))
	for _, typed := range []struct {
		character rune
		days      uint32
	}{{'3', 3}, {'0', 30}} {
		action, ok := dashboard.HandleKey(key(typed.character)).(DashboardActionPreviewArchiveSpace)
		if !ok || action.Generation != generation || action.OlderThanDays == nil || *action.OlderThanDays != typed.days {
			t.Fatalf("typing %c asked %#v", typed.character, action)
		}
	}

	// The answer for the value already typed past is dropped.
	three := uint32(3)
	stalePreview := used
	stalePreview.ReclaimableSessions, stalePreview.ReclaimableBytes = 39, 5_000_000_000
	dashboard.ArchiveSpacePreviewed(generation, &three, stalePreview, nil)
	if stale := rendered(); strings.Contains(stale, "39 sessions") {
		t.Fatalf("an answer for a value typed past must not be shown:\n%s", stale)
	}

	thirty := uint32(30)
	current := used
	current.ReclaimableSessions, current.ReclaimableBytes = 12, 1_288_490_188
	dashboard.ArchiveSpacePreviewed(generation, &thirty, current, nil)
	// The estimate follows pending text and stays under the same field after
	// apply.
	if editing := rendered(); !strings.Contains(editing, "would reclaim 1.2G of 4.8G (12 of 40 sessions)") ||
		strings.Contains(editing, "30 · would reclaim") {
		t.Fatalf("the editor must show what the typed value would reclaim:\n%s", editing)
	}
	dashboard.HandleKey(key(tea.KeyEnter))
	if reclaim := rendered(); !strings.Contains(reclaim, "would reclaim 1.2G of 4.8G (12 of 40 sessions)") {
		t.Fatalf("the row must report what the saved value would reclaim:\n%s", reclaim)
	}
}

// A new SSH machine keeps its workspaces under Mjolnir's own directory, not
// the product's former name. Launch campaign finding C-17.
//
// Hard-won: d0c3d418: new SSH machines defaulted workspaces under the former product-name directory.
func TestANewSshMachineKeepsWorkspacesUnderTheMjolnirDirectory(t *testing.T) {
	machine := client.Defaults([]string{"machines", "box"}, jsonvalue.Object{{Key: "kind", Value: "ssh"}})
	if prefix := setupValueGet(machine, "workspace_prefix"); !setupValueEqual(prefix, ".local/share/mjolnir/workspaces") {
		t.Fatalf("workspace_prefix = %s", setupValueDisplay(prefix))
	}
}

func TestRemovingAMachineRemovesTheRuntimesThatNamedIt(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	configuration := *dashboard.config.Get()
	configuration.Machines = maps.Clone(configuration.Machines)
	configuration.Targets = maps.Clone(configuration.Targets)
	user := "dev"
	ssh := core.SshConnection{Host: "builder.example.test", User: &user, ExtraArgs: []string{}}
	configuration.Machines["builder"] = core.MachineSsh{SSH: ssh, WorkspacePrefix: core.DefaultWorkspacePrefix}
	configuration.Targets["builder-bare"] = core.TargetTemplateSshBare{
		SSH:             ssh,
		Permissions:     core.PermissionModeGuardian,
		WorkspacePrefix: core.DefaultWorkspacePrefix,
	}
	dashboard.SetConfig(configuration)
	entry := "builder"
	dashboard.beginSettingsSection("machines", &entry)
	setupClick(dashboard, "Remove")
	dialog := setupDialog(t, dashboard)
	if setupValueHas(setupValueGet(dialog.draft, "machines"), "builder") {
		t.Fatal("the machine was not removed")
	}
	if targets := setupValueGet(dialog.draft, "targets"); setupValueHas(targets, "builder-bare") {
		t.Fatalf("a runtime goes with the machine it names: %s", setupValueDisplay(targets))
	}
	if dialog.notice == nil || !strings.Contains(*dialog.notice, "builder-bare") {
		t.Fatalf("the removal must say what went with the machine: %v", dialog.notice)
	}
	// The save the removal feeds must accept the draft, rather than failing
	// on a runtime that names a machine it no longer defines.
	saved := setupSavedConfig(t, dialog)
	if _, ok := saved.Machines["builder"]; ok {
		t.Fatal("the saved configuration keeps the machine")
	}
	if _, ok := saved.Targets["builder-bare"]; ok {
		t.Fatal("the saved configuration keeps the runtime")
	}
}

func TestRemoveTargetsTheCollectionEntryDetailPage(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	entry := "codex-1"
	dashboard.beginSettingsSection("profiles", &entry)
	setupClick(dashboard, "Remove")
	dialog := setupDialog(t, dashboard)
	if !slices.Equal(dialog.path, []string{"profiles"}) {
		t.Fatalf("path = %v", dialog.path)
	}
	profiles := setupValueGet(dialog.draft, "profiles")
	if setupValueHas(profiles, "codex-1") || !setupValueHas(profiles, "claude-1") {
		t.Fatalf("profiles = %s", setupValueDisplay(profiles))
	}
	if slices.ContainsFunc(dialog.actions(), func(button setupButtonSpec) bool {
		return button.ID == SetupControlSignIn || button.ID == SetupControlRemove
	}) {
		t.Fatal("the collection page offers an entry's actions")
	}
}

func TestEmptyOptionalFieldUsesDefaultButEnvironmentEmptyStringIsPreserved(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	entry := "codex-1"
	dashboard.beginSettingsSection("profiles", &entry)
	setupChoose(t, dashboard, "guardian_review_model")
	setupReplaceText(dashboard, "custom")
	dashboard.HandleKey(key(tea.KeyEnter))
	setupReplaceText(dashboard, "")
	dashboard.HandleKey(key(tea.KeyEnter))
	if model := setupValuePath(setupDialog(t, dashboard).draft, "profiles", "codex-1", "guardian_review_model"); model != nil {
		t.Fatalf("guardian_review_model = %s", setupValueDisplay(model))
	}
	setupChoose(t, dashboard, "environment")
	setupActivate(t, dashboard, SetupControlAdd)
	dashboard.HandlePaste("EMPTY")
	dashboard.HandleKey(key(tea.KeyEnter))
	dashboard.HandleKey(key(tea.KeyEnter))
	if value := setupValuePath(setupDialog(t, dashboard).draft, "profiles", "codex-1", "environment", "EMPTY"); !setupValueEqual(value, "") {
		t.Fatalf("EMPTY = %s", setupValueDisplay(value))
	}
}

func TestDeletingAnArrayEntryRebindsPendingTextToTheSurvivingEntry(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	dialog := setupDialog(t, dashboard)
	setupValueInsert(&dialog.draft, []string{"bundles", "hel"}, "repositories", setupJSON(t, `[
		{"id":"first", "github":null, "local":null, "destination":"first"},
		{"id":"second", "github":null, "local":null, "destination":"second"}
	]`))
	setupChoose(t, dashboard, "bundles")
	setupChoose(t, dashboard, "hel")
	setupChoose(t, dashboard, "repositories")
	setupChoose(t, dashboard, "1")
	setupChoose(t, dashboard, "destination")
	setupReplaceText(dashboard, "second-pending")
	dashboard.HandleKey(key(tea.KeyEscape))
	setupChoose(t, dashboard, "0")
	setupClick(dashboard, "Remove")
	setupChoose(t, dashboard, "0")
	setupChoose(t, dashboard, "destination")
	dialog = setupDialog(t, dashboard)
	repositories, _ := setupValueArray(setupValuePath(dialog.draft, "bundles", "hel", "repositories"))
	if len(repositories) == 0 || !setupValueEqual(setupValueGet(repositories[0], "id"), "second") {
		t.Fatalf("repositories = %s", setupValueDisplay(repositories))
	}
	if value := setupFocusedField(t, dialog).input.text().Value(); value != "second-pending" {
		t.Fatalf("field = %q", value)
	}
}

// Hard-won: Settings live test: a scalar entry's pending buffer disabled its parent page shortcuts.
func TestEnvironmentCollectionShortcutsRemainActiveAfterInlineEdits(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupSearch(dashboard, "Environment variables")
	dashboard.HandleKey(key(tea.KeyEnter))
	dialog := setupDialog(t, dashboard)
	collection := slices.Clone(dialog.path)
	if !slices.Equal(collection, []string{"profiles", "claude-1", "environment"}) {
		t.Fatalf("path = %v", collection)
	}
	if keys := dialog.keys(); len(keys) != 0 {
		t.Fatalf("keys = %v", keys)
	}
	for _, entry := range []struct{ name, pending string }{
		{"LIVE_A", "pending-alpha"}, {"LIVE_B", "pending-beta"},
	} {
		dashboard.HandleKey(key('a'))
		if editor := setupDialog(t, dashboard).editor; editor == nil || !editor.adding {
			t.Fatal("a must open Add even when the focused page button has a retained buffer")
		}
		dashboard.HandlePaste(entry.name)
		dashboard.HandleKey(key(tea.KeyEnter))
		dashboard.HandlePaste(entry.pending)
		if value := setupFocusedField(t, setupDialog(t, dashboard)).input.text().Value(); value != entry.pending {
			t.Fatalf("field = %q", value)
		}
		dashboard.HandleKey(key(tea.KeyEscape))
		dialog = setupDialog(t, dashboard)
		if !slices.Equal(dialog.path, collection) {
			t.Fatalf("path = %v", dialog.path)
		}
		if dialog.textInputFocused() {
			t.Fatal("the entry is now a Page button")
		}
		if !dialog.isDirty() {
			t.Fatal("the pending text is an edit")
		}
	}
	dashboard.HandleKey(key('/'))
	if setupDialog(t, dashboard).search == nil {
		t.Fatal("the collection search shortcut must also remain available")
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	setupChoose(t, dashboard, "LIVE_A")
	if value := setupFocusedField(t, setupDialog(t, dashboard)).input.text().Value(); value != "pending-alpha" {
		t.Fatalf("navigation must retain the original edit: %q", value)
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	dashboard.HandleKey(key(tea.KeyBackspace))
	if path := setupDialog(t, dashboard).path; !slices.Equal(path, []string{"profiles", "claude-1"}) {
		t.Fatalf("Backspace must navigate from the collection's Page button: %v", path)
	}
}
