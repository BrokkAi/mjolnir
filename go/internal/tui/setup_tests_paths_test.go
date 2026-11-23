// Port of mj-tui/src/setup/tests/paths.rs.

package tui

import (
	"errors"
	"maps"
	"testing"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// setupAddBuilderMachine puts an SSH machine named builder in the dashboard's
// configuration.
func setupAddBuilderMachine(t testing.TB, dashboard *DashboardState) {
	t.Helper()
	builder, err := core.UnmarshalMachineJSON([]byte(`{"kind":"ssh","host":"builder"}`))
	if err != nil {
		t.Fatal(err)
	}
	dashboard.config.Mut().Machines["builder"] = builder
}

func TestAccountPathApplyExpandsHomeBeforeConfigAndQuotaUse(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "profiles")
	setupChoose(t, dashboard, "codex-1")
	setupChoose(t, dashboard, "home")
	editor := setupFocusedField(t, setupDialog(t, dashboard))
	if editor.input.pathInput() == nil {
		t.Fatal("home is a path field")
	}
	editor.input.text().Clear()
	dashboard.HandlePaste("~/.codex4")
	dashboard.HandleKey(key(tea.KeyEnter))
	dialog := setupDialog(t, dashboard)
	if dialog.editor != nil {
		t.Fatalf("notice %v", dialog.notice)
	}
	configuration, err := client.ConfigFromDraft(dialog.draft)
	if err != nil {
		t.Fatal(err)
	}
	expected, err := core.ExpandLocal("~/.codex4")
	if err != nil {
		t.Fatal(err)
	}
	profile := configuration.Profiles["codex-1"]
	if profile.Home != expected {
		t.Fatalf("home = %q, want %q", profile.Home, expected)
	}
	environment := maps.Clone(profile.Environment.Resolved())
	if environment == nil {
		environment = map[string]string{}
	}
	profile.Kind.ConfigureProfileHomeEnvironment(profile.Home, core.HarnessHostCurrent(), environment)
	if environment["CODEX_HOME"] != expected {
		t.Fatalf("CODEX_HOME = %q, want %q", environment["CODEX_HOME"], expected)
	}
}

func TestRemotePathApplyPreservesFailedAndNewerDrafts(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	setupAddBuilderMachine(t, dashboard)
	remote, err := core.UnmarshalTargetTemplateJSON([]byte(`{"kind":"ssh-bare","host":"builder","permissions":"guardian"}`))
	if err != nil {
		t.Fatal(err)
	}
	dashboard.config.Mut().Targets["remote-path"] = remote
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "machines")
	setupChoose(t, dashboard, "builder")
	setupChoose(t, dashboard, "workspace_prefix")
	setupFocusedField(t, setupDialog(t, dashboard)).input.text().SetValue("~/work")
	resolve, ok := dashboard.HandleKey(key(tea.KeyEnter)).(DashboardActionResolveSetupPath)
	if !ok {
		t.Fatal("resolve path")
	}
	dashboard.SetupPathResolved(resolve.Generation, resolve.Draft, resolve.Path, resolve.Value, "", errors.New("SSH unavailable"))
	dialog := setupDialog(t, dashboard)
	if value := setupFocusedField(t, dialog).input.text().Value(); value != "~/work" {
		t.Fatalf("field = %q", value)
	}
	if dialog.notice == nil || *dialog.notice != "SSH unavailable" {
		t.Fatalf("notice = %v", dialog.notice)
	}
	setupFocusedField(t, dialog).input.text().SetValue("~/newer")
	dashboard.SetupPathResolved(resolve.Generation, resolve.Draft, resolve.Path, resolve.Value, "/remote/work", nil)
	dialog = setupDialog(t, dashboard)
	if value := setupFocusedField(t, dialog).input.text().Value(); value != "~/newer" {
		t.Fatalf("a reply for older text replaced newer text: %q", value)
	}
	setupFocusedField(t, dialog).input.text().SetValue(resolve.Value)
	dashboard.SetupPathResolved(resolve.Generation, resolve.Draft, resolve.Path, resolve.Value, "/remote/work", nil)
	dialog = setupDialog(t, dashboard)
	if prefix := setupValuePath(dialog.draft, "machines", "builder", "workspace_prefix"); !setupValueEqual(prefix, "/remote/work") {
		t.Fatalf("workspace_prefix = %s", setupValueDisplay(prefix))
	}
}

// An identity file is a file, not a directory, and it lives on the machine
// running Mjolnir rather than on the machine it configures.
func TestSetupIdentityFileCompletesFilesLocally(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	setupAddBuilderMachine(t, dashboard)
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "machines")
	setupChoose(t, dashboard, "builder")
	setupChoose(t, dashboard, "identity_file")
	dialog := setupDialog(t, dashboard)
	setupFocusedField(t, dialog).input.text().SetValue("/keys/id")
	dialog.prepare()

	complete, ok := dashboard.HandleKey(setupCtrlSpace()).(DashboardActionCompletePath)
	if !ok {
		t.Fatal("Ctrl-Space asks for completions")
	}
	if _, local := complete.Host.(core.CompletionHostLocal); !local || complete.Kind != core.CompletionKindAny || complete.Prefix != "/keys/id" {
		t.Fatalf("complete = %#v", complete)
	}
	context := dashboard.PathInputContext()
	dashboard.ApplyPathCompletions(context, "/keys/id", core.PathCompletion{
		Candidates: []string{"/keys/id_ed25519", "/keys/id_rsa"},
	})
	input := setupFocusedField(t, setupDialog(t, dashboard)).input.pathInput()
	if input == nil {
		t.Fatal("path editor")
	}
	if !input.IsCompleting() {
		t.Fatal("the completions are showing")
	}

	dashboard.HandleKey(key(tea.KeyDown))
	dashboard.HandleKey(key(tea.KeyEnter))
	if value := setupFocusedField(t, setupDialog(t, dashboard)).input.text().Value(); value != "/keys/id_rsa" {
		t.Fatalf("field = %q", value)
	}
}

// A workspace prefix belongs to the machine it is configured on, so its
// completions come from that machine and list directories only.
func TestSetupWorkspacePrefixCompletesOnItsMachine(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	setupAddBuilderMachine(t, dashboard)
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "machines")
	setupChoose(t, dashboard, "builder")
	setupChoose(t, dashboard, "workspace_prefix")
	dialog := setupDialog(t, dashboard)
	setupFocusedField(t, dialog).input.text().SetValue("/srv/w")
	dialog.prepare()

	action := dashboard.HandleKey(setupCtrlSpace())
	complete, ok := action.(DashboardActionCompletePath)
	if !ok {
		t.Fatalf("expected a completion request, got %#v", action)
	}
	if complete.Prefix != "/srv/w" || complete.Kind != core.CompletionKindDirectories {
		t.Fatalf("complete = %#v", complete)
	}
	machine, ok := complete.Host.(core.CompletionHostMachine)
	if !ok {
		t.Fatal("a target path completes on its own machine")
	}
	if _, ssh := machine.Machine.(core.MachineSsh); !ssh {
		t.Fatalf("machine = %#v", machine.Machine)
	}
}

func TestSaveResolvesPendingTargetPathsAndContinuesOnlyForTheMatchingReply(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	setupAddBuilderMachine(t, dashboard)
	entry := "builder"
	dashboard.beginSettingsSection("machines", &entry)
	setupChoose(t, dashboard, "workspace_prefix")
	setupReplaceText(dashboard, "~/work")
	resolve, ok := dashboard.HandleKey(setupCtrl('s')).(DashboardActionResolveSetupPath)
	if !ok {
		t.Fatal("Save resolves the pending target path")
	}
	// A stale callback cannot finish a Save after another edit.
	setupReplaceText(dashboard, "~/newer")
	dashboard.SetupPathResolved(resolve.Generation, resolve.Draft, resolve.Path, resolve.Value, "/remote/work", nil)
	dialog := setupDialog(t, dashboard)
	if value := setupFocusedField(t, dialog).input.text().Value(); value != "~/newer" {
		t.Fatalf("field = %q", value)
	}
	if dialog.saving {
		t.Fatal("a stale reply finished the Save")
	}
	resolve, ok = dashboard.HandleKey(setupCtrl('s')).(DashboardActionResolveSetupPath)
	if !ok {
		t.Fatal("resolve the new pending path")
	}
	dashboard.SetupPathResolved(resolve.Generation-1, resolve.Draft, resolve.Path, resolve.Value, "/stale/newer", nil)
	if _, resolving := setupDialog(t, dashboard).pendingSave.(PendingSaveResolving); !resolving {
		t.Fatal("a reply for another dialog resumed the Save")
	}
	dashboard.SetupPathResolved(resolve.Generation, resolve.Draft, resolve.Path, resolve.Value, "/remote/newer", nil)
	action, ok := dashboard.TakePrerequisiteCheck()
	if !ok {
		t.Fatal("Save resumes after resolution")
	}
	save, ok := action.(DashboardActionSaveSetup)
	if !ok {
		t.Fatalf("background save: %#v", action)
	}
	saved := setupJSON(t, save.Updated)
	if prefix := setupValuePath(saved, "machines", "builder", "workspace_prefix"); !setupValueEqual(prefix, "/remote/newer") {
		t.Fatalf("workspace_prefix = %s", setupValueDisplay(prefix))
	}
}
