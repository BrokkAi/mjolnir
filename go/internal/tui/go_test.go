// Port of mj-tui/src/go.rs tests.

package tui

import (
	"reflect"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func goTestMode() GoMode {
	workspace := core.DefaultWorkspaceID
	bundle := "hel"
	createManagedWorktree := false
	return GoMode{
		WorkspaceID:   &workspace,
		Directory:     "/projects/current",
		SaveAsDefault: false,
		Simple:        false,
		Recipe: &core.GoRecipe{
			ProfileID:             "codex-1",
			TargetID:              "podman",
			BundleID:              &bundle,
			CreateManagedWorktree: &createManagedWorktree,
			Subagents: &core.SubagentPolicy{
				Mode:          core.SubagentPolicyMjolnir,
				Orchestrators: false,
			},
			AdditionalMounts: []core.AdditionalMount{},
		},
	}
}

func goTestSimpleMode() GoMode {
	mode := goTestMode()
	mode.Simple = true
	recipe := *mode.Recipe
	recipe.TargetID = simpleTargetID
	recipe.BundleID = nil
	directory := mode.Directory
	recipe.ProjectDirectory = &directory
	createManagedWorktree := true
	recipe.CreateManagedWorktree = &createManagedWorktree
	mode.Recipe = &recipe
	return mode
}

// goTestStartup is the session the daemon would choose in these one-session
// fixtures.
func goTestStartup(dashboard *DashboardState) *core.SessionRecord {
	for session := range dashboard.state.Get().Sessions.IntoValues() {
		return &session
	}
	return nil
}

func goTestContext(dashboard *DashboardState) string {
	return strings.Join(dashboard.goContext(), "\n")
}

func TestSwitchingWorkspacesChangesNewAndContextWithoutRebindingTheLaunchDirectory(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	first := goTestMode()
	firstID := *first.WorkspaceID
	second := goTestMode()
	secondID := "project-b"
	second.WorkspaceID = &secondID
	second.Directory = "/projects/second"
	secondRecipe := *second.Recipe
	secondRecipe.TargetID = "other-runtime"
	second.Recipe = &secondRecipe
	dashboard.RegisterGoWorkspaces([]GoMode{second})
	dashboard.BeginGo(first, false, goTestStartup(dashboard))
	dashboard.SetActiveWorkspace(second.WorkspaceID)
	if !strings.Contains(goTestContext(dashboard), "/projects/second") {
		t.Fatalf("context = %q, want the second directory", goTestContext(dashboard))
	}
	if strings.Contains(goTestContext(dashboard), "/projects/current") {
		t.Fatalf("context = %q, must not name the first directory", goTestContext(dashboard))
	}
	if action := dashboard.DispatchCommand(CommandIdNewSessionWizard); !reflect.DeepEqual(action, DashboardActionGoLaunch{Recipe: *second.Recipe}) {
		t.Fatalf("action = %#v, want the second recipe's launch", action)
	}

	// Preparation from A may finish while B is selected: never overwrite B's
	// recipe.
	prepared := *first.Recipe
	preparedBundle := "prepared-a"
	prepared.BundleID = &preparedBundle
	prepared.Subagents = nil
	action := dashboard.GoLaunchAction(firstID, prepared)
	create, ok := action.(DashboardActionCreateSession)
	if !ok || create.Subagents != nil {
		t.Fatalf("action = %#v, want a launch with no sub-agent policy", action)
	}
	saved := dashboard.RememberGoLaunch(action)
	if saved == nil {
		t.Fatal("the launch should be remembered")
	}
	if saved.Directory != first.Directory {
		t.Fatalf("saved directory = %q, want %q", saved.Directory, first.Directory)
	}
	if saved.Recipe.Subagents != nil {
		t.Fatalf("saved sub-agents = %#v, want none", saved.Recipe.Subagents)
	}
	if !reflect.DeepEqual(dashboard.GoMode().Recipe, second.Recipe) {
		t.Fatalf("go recipe = %#v, want the second recipe", dashboard.GoMode().Recipe)
	}

	unbound := "unbound-workspace"
	dashboard.SetActiveWorkspace(&unbound)
	if dashboard.GoMode() != nil {
		t.Fatal("an unbound workspace has no go mode")
	}
	if len(dashboard.goContext()) != 0 {
		t.Fatalf("context = %q, want none", dashboard.goContext())
	}
	dashboard.DispatchCommand(CommandIdNewSessionWizard)
	if _, ok := dashboard.mode.(ModeNew); !ok {
		t.Fatalf("mode = %T, want the new-session wizard", dashboard.mode)
	}
	dashboard.CancelModal()
	dashboard.SetActiveWorkspace(&firstID)
	if directory := dashboard.GoMode().Directory; directory != first.Directory {
		t.Fatalf("go directory = %q, want %q", directory, first.Directory)
	}
	if action := dashboard.DispatchCommand(CommandIdNewSessionWizard); !reflect.DeepEqual(action, DashboardActionGoLaunch{Recipe: prepared}) {
		t.Fatalf("action = %#v, want the prepared recipe's launch", action)
	}
}

func TestChangeSetupIsExplicitAndCancellingKeepsThePreviousRecipe(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.BeginGo(goTestMode(), false, goTestStartup(dashboard))
	dashboard.DispatchCommand(CommandIdChangeGoSetup)
	if _, ok := dashboard.mode.(ModeNew); !ok {
		t.Fatalf("mode = %T, want the new-session wizard", dashboard.mode)
	}
	dashboard.CancelModal()
	if action := dashboard.DispatchCommand(CommandIdNewSessionWizard); !reflect.DeepEqual(action, DashboardActionGoLaunch{Recipe: *goTestMode().Recipe}) {
		t.Fatalf("action = %#v, want the recipe's launch", action)
	}
}

func TestFailedContextProbeReplacesOldLocationInsteadOfClaimingItIsCurrent(t *testing.T) {
	session := runningSession()
	id := session.ID
	dashboard := dashboardWithSession(session)
	dashboard.BeginGo(goTestMode(), false, goTestStartup(dashboard))
	dashboard.SetGoContext(id, client.DaemonOk(client.DaemonPair[string, string]{First: "/old", Second: "main"}))
	dashboard.SetGoContext(id, client.DaemonErr[client.DaemonPair[string, string]]("target disconnected"))
	text := goTestContext(dashboard)
	if !strings.Contains(text, "target disconnected") {
		t.Fatalf("context = %q, want the failure", text)
	}
	if strings.Contains(text, "/old") {
		t.Fatalf("context = %q, must not claim the old location", text)
	}
}

func TestSimpleModeLaunchesTheFixedLocalRecipeWithReviewOffAndNoWizard(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	simple := goTestSimpleMode()
	simpleDirectory := simple.Directory
	expected := *simple.Recipe
	if action := dashboard.BeginGo(simple, false, nil); !reflect.DeepEqual(action, DashboardActionGoLaunch{Recipe: expected}) {
		t.Fatalf("action = %#v, want the simple recipe's launch", action)
	}
	if !dashboard.SimpleMode() {
		t.Fatal("simple mode should be on")
	}
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode = %T, want the dashboard", dashboard.mode)
	}
	if dashboard.PrefixPending() {
		t.Fatal("the prefix must not be pending")
	}
	if action := dashboard.DispatchCommand(CommandIdNewSessionWizard); !reflect.DeepEqual(action, DashboardActionGoLaunch{Recipe: expected}) {
		t.Fatalf("action = %#v, want the simple recipe's launch", action)
	}

	action := dashboard.GoLaunchAction("workspace", expected)
	create, ok := action.(DashboardActionCreateSession)
	if !ok {
		t.Fatalf("simple mode should create a session directly: %#v", action)
	}
	if create.TargetTemplateID != simpleTargetID {
		t.Fatalf("target = %q, want %q", create.TargetTemplateID, simpleTargetID)
	}
	if create.ProjectDirectory == nil || *create.ProjectDirectory != simpleDirectory {
		t.Fatalf("project directory = %v, want %q", create.ProjectDirectory, simpleDirectory)
	}
	if create.CreateManagedWorktree == nil || !*create.CreateManagedWorktree {
		t.Fatal("simple mode creates a managed worktree")
	}
	if !reflect.DeepEqual(create.Subagents, expected.Subagents) {
		t.Fatalf("sub-agents = %#v, want %#v", create.Subagents, expected.Subagents)
	}
	if create.Review == nil {
		t.Fatal("simple mode turns review off")
	}
	if _, off := (*create.Review).(core.SessionReviewOff); !off {
		t.Fatalf("review = %#v, want off", *create.Review)
	}
	if dashboard.RememberGoLaunch(action) != nil {
		t.Fatal("simple mode must not rewrite the normal go-mode recipe")
	}
}

func TestSimpleModeDoesNotReuseALocalSessionThatMayBeReviewed(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	simple := goTestSimpleMode()
	expected := *simple.Recipe
	reviewed := goTestStartup(dashboard)
	if reviewed == nil {
		t.Fatal("fixture session")
	}
	reviewed.State = core.SessionStateRunning
	reviewed.TargetTemplateID = simpleTargetID
	createManagedWorktree := true
	reviewed.CreateManagedWorktree = &createManagedWorktree
	reviewed.Review = nil

	if action := dashboard.BeginGo(simple, false, reviewed); !reflect.DeepEqual(action, DashboardActionGoLaunch{Recipe: expected}) {
		t.Fatalf("action = %#v, want the simple recipe's launch", action)
	}
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode = %T, want the dashboard", dashboard.mode)
	}
}
