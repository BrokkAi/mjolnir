// Port of mj-tui/src/setup/tests/profiles.rs.

package tui

import (
	"bytes"
	"encoding/json"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// setupSerdeEqual is a derived PartialEq for a configuration value,
// compared through the form it is stored in.
func setupSerdeEqual(a, b any) bool {
	left, errLeft := json.Marshal(a)
	right, errRight := json.Marshal(b)
	return errLeft == nil && errRight == nil && bytes.Equal(left, right)
}

// Managed onboarding must not overwrite existing detected profiles or require
// the user to choose a home path.
func TestManagedSetupCreatesACollisionFreeProfileAndRequestsLogin(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	profiles := dashboard.config.Get().Profiles
	existing := profiles[slices.Sorted(maps.Keys(profiles))[0]]
	existing.Kind = core.HarnessKindClaude
	dashboard.config.Mut().Profiles["claude"] = existing
	dashboard.beginSettingsSection("profiles", nil)
	setupClick(dashboard, "Set up managed")
	dashboard.HandleKey(key(tea.KeyDown))
	action := dashboard.HandleKey(key(tea.KeyEnter))

	login, ok := action.(DashboardActionSaveSetupAndLogin)
	if !ok {
		t.Fatalf("managed setup did not request save and login: %#v", action)
	}
	if login.ProfileID != "claude-2" {
		t.Fatalf("profile = %q", login.ProfileID)
	}
	saved := setupJSON(t, login.Updated)
	profile := setupValuePath(saved, "profiles", login.ProfileID)
	if kind := setupValueGet(profile, "kind"); !setupValueEqual(kind, "claude") {
		t.Fatalf("kind = %s", setupValueDisplay(kind))
	}
	home, _ := setupValueString(setupValueGet(profile, "home"))
	if !filepath.IsAbs(home) {
		t.Fatal(home)
	}
	if !strings.HasSuffix(filepath.ToSlash(home), "/profiles/claude-2/claude") {
		t.Fatal(home)
	}
}

// Managed onboarding must keep its login handoff while an existing inline
// draft blocks Save; correcting that field must save both edits before
// requesting login.
func TestManagedSetupKeepsLoginIntentThroughPendingFieldValidation(t *testing.T) {
	dashboard := setupAt(t, "subagents")
	setupChoose(t, dashboard, "max_concurrent")
	setupReplaceText(dashboard, "many")
	dashboard.HandleKey(key(tea.KeyEscape))
	setupChoose(t, dashboard, "profiles")
	setupClick(dashboard, "Set up managed")
	dashboard.HandleKey(key(tea.KeyDown))
	if action := dashboard.HandleKey(key(tea.KeyEnter)); !setupActionIsNone(action) {
		t.Fatalf("an invalid draft saved: %#v", action)
	}
	dialog := setupDialog(t, dashboard)
	if dialog.saving {
		t.Fatal("an invalid draft is saving")
	}
	if path, _ := dialog.focusedPath(); !slices.Equal(path, []string{"subagents", "max_concurrent"}) {
		t.Fatalf("focused %v", path)
	}
	field := setupFocusedField(t, dialog)
	if field.input.text().Value() != "many" || field.err == nil {
		t.Fatalf("field %q, error %v", field.input.text().Value(), field.err)
	}
	// Hard-won: 2dfa44874: managed setup's own profile template gave a held-open Claude profile the Codex fields.
	profile := []string{"profiles", "claude"}
	value, _ := setupLookup(dialog.draft, profile)
	shown := client.VisibleKeys(profile, value)
	if !slices.Contains(shown, "claude_permission_mode") {
		t.Fatalf("%v", shown)
	}
	if slices.ContainsFunc(shown, func(key string) bool { return key == "guardian_review_model" || key == "codex_profile" }) {
		t.Fatalf("%v", shown)
	}
	setupReplaceText(dashboard, "5")
	action := dashboard.HandleKey(setupCtrl('s'))
	login, ok := action.(DashboardActionSaveSetupAndLogin)
	if !ok {
		t.Fatalf("corrected inline field lost onboarding login: %#v", action)
	}
	var saved core.Config
	if err := json.Unmarshal([]byte(login.Updated), &saved); err != nil {
		t.Fatal(err)
	}
	if login.ProfileID != "claude" || saved.Subagents.MaxConcurrent != 5 || saved.Profiles[login.ProfileID].Kind != core.HarnessKindClaude {
		t.Fatalf("profile %q, max_concurrent %d", login.ProfileID, saved.Subagents.MaxConcurrent)
	}
}

// Hard-won: 8d16bf96: unrelated settings saves failed when a configured API-key file disappeared.
func TestSettingsSaveApiKeyProfilesWithoutResolvingCredentialsOnTheUiThread(t *testing.T) {
	directory := t.TempDir()
	home := filepath.Join(directory, "codex")
	if err := os.Mkdir(home, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(
		"model_provider = 'deepseek'\n[model_providers.deepseek]\nname = 'DeepSeek'\nbase_url = 'https://api.deepseek.com/v1'\nenv_key = 'DEEPSEEK_API_KEY'\nwire_api = 'responses'\n",
	), 0o644); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(directory, "config.toml")
	secrets := filepath.Join(directory, "secrets.toml")
	if err := os.WriteFile(secrets, []byte("DEEPSEEK_API_KEY = 'test-secret'\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	stored, err := setupValueTo(core.DefaultConfig())
	if err != nil {
		t.Fatal(err)
	}
	setupValueInsert(&stored, nil, "profiles", jsonvalue.Object{{Key: "deepseek", Value: jsonvalue.Object{
		{Key: "kind", Value: "codex"},
		{Key: "home", Value: home},
		{Key: "environment", Value: jsonvalue.Object{
			{Key: "DEEPSEEK_API_KEY", Value: jsonvalue.Object{{Key: "from_secret", Value: "DEEPSEEK_API_KEY"}}},
		}},
	}}})
	read := core.WithSecretResolver(core.SecretResolverBeside(path), func() setupConfigRead {
		var configuration core.Config
		err := setupValueFrom(stored, &configuration)
		return setupConfigRead{config: configuration, err: err}
	})
	configuration, err := read.unpack()
	if err != nil {
		t.Fatal(err)
	}
	if err := configuration.Validate(); err != nil {
		t.Fatal(err)
	}
	// Opening and saving must work even when credentials are unavailable to
	// the UI. The background saver resolves and validates before writing.
	if err := os.Remove(secrets); err != nil {
		t.Fatal(err)
	}
	dialog := newSetupDialog(&configuration)
	if dialog.isDirty() {
		t.Fatal("a fresh dialog is dirty")
	}
	setupValueInsert(&dialog.draft, []string{"notify"}, "bell", !configuration.Notify.Bell)
	if !dialog.isDirty() {
		t.Fatal("the bell edit is a change")
	}
	save, ok := dialog.save(&core.ProfileCapabilitiesSnapshot{}).(DashboardActionSaveSetup)
	if !ok {
		t.Fatalf("expected background save: %v", dialog.notice)
	}
	if !dialog.saving {
		t.Fatal("the dialog is saving")
	}
	if strings.Contains(save.Updated, "test-secret") {
		t.Fatal("the save carries a resolved secret")
	}
	updated := setupJSON(t, save.Updated)
	if bell := setupValuePath(updated, "notify", "bell"); !setupValueEqual(bell, !configuration.Notify.Bell) {
		t.Fatalf("bell = %s", setupValueDisplay(bell))
	}
	reference := setupValuePath(updated, "profiles", "deepseek", "environment", "DEEPSEEK_API_KEY")
	if !setupValueEqual(reference, setupJSON(t, `{"from_secret": "DEEPSEEK_API_KEY"}`)) {
		t.Fatalf("reference = %s", setupValueDisplay(reference))
	}
}

func TestSigningInOnTheProfileDetailPageIsASettingsAction(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	entry := "codex-1"
	dashboard.beginSettingsSection("profiles", &entry)
	dialog := setupDialog(t, dashboard)
	dialog.prepare()
	dialog.form.Focus(SetupControlSignIn)
	generation := dialog.generation
	login, ok := dashboard.HandleKey(key(tea.KeyEnter)).(DashboardActionSetupLogin)
	if !ok {
		t.Fatal("Sign in starts a login for the selected profile")
	}
	if login.Generation != generation || login.ProfileID != "codex-1" {
		t.Fatalf("login = %#v", login)
	}
}

func TestTheProfilesListShowsEachProfilesSignInStatus(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.SetProfileLoginStatuses(0, map[string]core.LoginStatus{
		"codex-1":  core.LoginStatusSignedOut{},
		"claude-1": core.LoginStatusSignedIn{},
	})
	dashboard.beginSettingsSection("profiles", nil)
	lines := drawn(dashboard, 140, 40)
	codex := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "codex-1") })
	if codex < 0 {
		t.Fatalf("missing codex row: %q", lines)
	}
	if !strings.Contains(lines[codex], "signed out") {
		t.Fatalf("%q", lines[codex])
	}
	claude := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "claude-1") })
	if claude < 0 {
		t.Fatalf("missing claude row: %q", lines)
	}
	if !strings.Contains(lines[claude], "signed in") {
		t.Fatalf("%q", lines[claude])
	}
}

func TestAnOlderSignInStatusReadDoesNotReplaceANewerOne(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.SetProfileLoginStatuses(2, map[string]core.LoginStatus{"codex-1": core.LoginStatusSignedOut{}})
	dashboard.SetProfileLoginStatuses(1, map[string]core.LoginStatus{"codex-1": core.LoginStatusSignedIn{}})
	status, ok := dashboard.profileLoginStatus("codex-1")
	if _, signedOut := status.(core.LoginStatusSignedOut); !ok || !signedOut {
		t.Fatalf("status = %#v", status)
	}
}

func TestEditingAProfileDisablesSignInUntilTheChangeIsSaved(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	entry := "codex-1"
	dashboard.beginSettingsSection("profiles", &entry)
	dialog := setupDialog(t, dashboard)
	setupValueInsert(&dialog.draft, []string{"profiles", "codex-1"}, "home", "/tmp/another-home")
	dialog.prepare()
	index := slices.IndexFunc(dialog.pageActions(), func(button setupButtonSpec) bool { return button.ID == SetupControlSignIn })
	if index < 0 {
		t.Fatal("the profiles page offers sign-in")
	}
	if dialog.pageActions()[index].Enabled {
		t.Fatal("Sign in must wait for the edit to be saved")
	}
}

func TestDisablingAProfileKeepsModelBasedReviewers(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	high := "high"
	dashboard.config.Mut().Review.Reviewers = []core.SubagentChoice{{Model: "review-model", Effort: &high}}
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "profiles")
	setupChoose(t, dashboard, "codex-1")

	enabled := strings.Join(bufferLines(setupRenderBuffer(dashboard, 100, 30)), "\n")
	if !strings.Contains(enabled, "☒") {
		t.Fatal(enabled)
	}

	setupChoose(t, dashboard, "enabled")
	dialog := setupDialog(t, dashboard)
	if value := setupValuePath(dialog.draft, "profiles", "codex-1", "enabled"); !setupValueEqual(value, false) {
		t.Fatalf("enabled = %s", setupValueDisplay(value))
	}
	if setupValueHas(setupValueGet(dialog.draft, "review"), "trigger") {
		t.Fatal("disabling a profile wrote a review trigger")
	}
	reviewers, _ := setupValueArray(setupValuePath(dialog.draft, "review", "reviewers"))
	if len(reviewers) == 0 || !setupValueEqual(setupValueGet(reviewers[0], "model"), "review-model") ||
		!setupValueEqual(setupValueGet(reviewers[0], "effort"), "high") {
		t.Fatalf("reviewers = %s", setupValueDisplay(reviewers))
	}

	disabled := strings.Join(bufferLines(setupRenderBuffer(dashboard, 100, 30)), "\n")
	if !strings.Contains(disabled, "☐") {
		t.Fatal(disabled)
	}
}

func TestDetectionAddsConflictingInstallationsToTheDraftWithoutLosingSettings(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	original := *dashboard.config.Get()
	dashboard.BeginSetup()
	generation := setupDialog(t, dashboard).generation
	discovered := original
	discovered.Profiles = maps.Clone(original.Profiles)
	discovered.Targets = maps.Clone(original.Targets)
	discovered.Bundles = maps.Clone(original.Bundles)
	codex := discovered.Profiles["codex-1"]
	codex.Home = "/profiles/new-codex"
	discovered.Profiles["codex-1"] = codex
	discovered.Targets["podman"] = core.TargetTemplateLocalBare{}
	hel := discovered.Bundles["hel"]
	hel.Repositories = slices.Clone(hel.Repositories)
	repository := "owner/new-repository"
	hel.Repositories[0].Github = &repository
	discovered.Bundles["hel"] = hel
	for range 2 {
		for _, scope := range []DetectScope{DetectScopeProfiles, DetectScopeRuntimes} {
			dashboard.SetupDiscovered(generation, setupDetection(scope, discovered), nil)
		}
		draft, err := client.ConfigFromDraft(setupDialog(t, dashboard).draft)
		if err != nil {
			t.Fatal(err)
		}
		if !setupSerdeEqual(draft.Profiles["codex-1"], original.Profiles["codex-1"]) {
			t.Fatal("detection replaced codex-1")
		}
		if draft.Profiles["codex-1-2"].Home != "/profiles/new-codex" {
			t.Fatalf("codex-1-2 home = %q", draft.Profiles["codex-1-2"].Home)
		}
		if !setupSerdeEqual(draft.Targets["podman"], original.Targets["podman"]) {
			t.Fatal("detection replaced podman")
		}
		if _, bare := draft.Targets["podman-2"].(core.TargetTemplateLocalBare); !bare {
			t.Fatalf("podman-2 = %#v", draft.Targets["podman-2"])
		}
		// Neither detection touches projects, so the draft keeps only the
		// bundles the user already had.
		if !setupSerdeEqual(draft.Bundles, original.Bundles) {
			t.Fatal("detection changed the projects")
		}
		if len(draft.Profiles) != len(original.Profiles)+1 || len(draft.Targets) != len(original.Targets)+1 {
			t.Fatalf("%d profiles, %d targets", len(draft.Profiles), len(draft.Targets))
		}
	}
	if !setupConfigEqual(*dashboard.config.Get(), original) {
		t.Fatal("discovery only edits the draft")
	}
}

// On a machine with no coding agent, Detect profiles said "No agent
// installation was found that this draft does not already have." "Draft" is
// an internal word, and the sentence gave no next step (launch finding
// R13-4).
//
// Hard-won: 60750d28: empty profile discovery did not explain that the machine had no agent.
func TestDetectingProfilesOnAMachineWithoutAnAgentSaysSoPlainly(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	generation := setupDialog(t, dashboard).generation

	dashboard.SetupDiscovered(generation, setupDetection(DetectScopeProfiles, core.DefaultConfig()), nil)

	notice := setupDialog(t, dashboard).notice
	if notice == nil {
		t.Fatal("detection reports what it did")
	}
	want := "No coding agent installation was found on this machine. Add a profile, choose Sign in, " +
		"or install Codex, Claude Code, Kimi Code, Grok Build, Muse Code, OpenCode, DSH, or Antigravity, " +
		"sign in to it once, and choose Detect profiles again."
	if *notice != want {
		t.Fatalf("notice = %q", *notice)
	}
}

// An agent that is installed but already has a profile is not "not found".
//
// Hard-won: 60750d28: discovery gave no distinct outcome when every agent profile already existed.
func TestDetectingProfilesThatAllExistSaysEachAgentAlreadyHasOne(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	original := *dashboard.config.Get()
	dashboard.BeginSetup()
	generation := setupDialog(t, dashboard).generation

	dashboard.SetupDiscovered(generation, setupDetection(DetectScopeProfiles, original), nil)

	notice := setupDialog(t, dashboard).notice
	if notice == nil {
		t.Fatal("detection reports what it did")
	}
	if *notice != "Every coding agent found on this machine already has a profile." {
		t.Fatalf("notice = %q", *notice)
	}
}

// A profile page lists only the settings its kind reads: the guardian review
// model and Codex profile belong to Codex alone, the Claude permission mode
// to Claude alone, and the Mjolnir guardian toggle belongs to every profile.
// Changing a profile's kind drops the old kind's fields.
//
// Hard-won: DSH campaign 2026-10-23: Settings showed guardian and Codex fields for DSH
func TestAProfilePageHidesCodexOnlySettingsForOtherKinds(t *testing.T) {
	path := []string{"profiles", "p"}
	profile := func(kind string) any {
		var draft any = jsonvalue.Object{{Key: "profiles", Value: jsonvalue.Object{{Key: "p", Value: jsonvalue.Object{
			{Key: "kind", Value: kind},
			{Key: "home", Value: "/h"},
		}}}}}
		root := []string{}
		client.Expand(&draft, &root)
		return draft
	}
	keys := func(draft any) []string {
		value, ok := setupLookup(draft, path)
		if !ok {
			t.Fatal("the profile page exists")
		}
		return client.VisibleKeys(path, value)
	}
	only := []struct {
		owners []string
		key    string
	}{
		{[]string{"codex"}, "guardian_review_model"},
		{[]string{"codex"}, "codex_profile"},
		{[]string{"claude"}, "claude_permission_mode"},
		{[]string{"antigravity"}, "third_party_models"},
	}
	for _, kind := range []string{"codex", "claude", "kimi", "grok", "muse", "open-code", "dsh", "antigravity"} {
		draft := profile(kind)
		shown := keys(draft)
		if !slices.Contains(shown, "mjolnir_guardian") {
			t.Fatal(kind)
		}
		toggle := append(slices.Clone(path), "mjolnir_guardian")
		if err := client.Stage(&draft, toggle, false); err != nil {
			t.Fatal(err)
		}
		if guardian := setupValuePath(draft, "profiles", "p", "mjolnir_guardian"); !setupValueEqual(guardian, false) {
			t.Fatalf("%s: mjolnir_guardian = %s", kind, setupValueDisplay(guardian))
		}
		for _, field := range only {
			if slices.Contains(shown, field.key) != slices.Contains(field.owners, kind) {
				t.Fatalf("%s %s", kind, field.key)
			}
		}
	}

	for _, change := range [][2]string{{"codex", "claude"}, {"kimi", "claude"}} {
		draft := profile(change[0])
		kind := append(slices.Clone(path), "kind")
		if err := client.Stage(&draft, kind, change[1]); err != nil {
			t.Fatal(err)
		}
		shown := keys(draft)
		for _, field := range only {
			if slices.Contains(shown, field.key) != slices.Contains(field.owners, change[1]) {
				t.Fatalf("%s to %s: %s", change[0], change[1], field.key)
			}
		}
	}
}
