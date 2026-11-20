// Port of the tests in mj-core/src/go.rs.

package core

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func goTestRecipe(target string) GoRecipe {
	bundle, directory, worktree := "repo-a", "/repo-a", false
	return GoRecipe{
		ProfileID:             "personal",
		TargetID:              target,
		BundleID:              &bundle,
		ProjectDirectory:      &directory,
		CreateManagedWorktree: &worktree,
		AdditionalMounts:      []AdditionalMount{},
	}
}

// Hard-won: 0877ce6d: mj new failed after a session was created through another surface
func TestTheFirstInteractiveSessionSetsTheDefaultPairAndLaterOnesDoNot(t *testing.T) {
	path := filepath.Join(t.TempDir(), "go.json")
	if err := GoPreferencesRememberFirstPair(path, "fake", "localhost"); err != nil {
		t.Fatal(err)
	}
	if err := GoPreferencesRememberFirstPair(path, "other", "podman"); err != nil {
		t.Fatal(err)
	}
	preferences, err := GoPreferencesLoad(path)
	if err != nil {
		t.Fatal(err)
	}
	if preferences.Default.ProfileID != "fake" || preferences.Default.TargetID != "localhost" {
		t.Fatalf("default = %#v", preferences.Default)
	}

	for _, bundle := range []string{"first", "latest"} {
		if err := GoPreferencesRememberRecent(path, RecentLaunch{ProfileID: "other", TargetID: "podman", BundleID: bundle}); err != nil {
			t.Fatal(err)
		}
	}
	preferences, err = GoPreferencesLoad(path)
	if err != nil {
		t.Fatal(err)
	}
	if preferences.Default.ProfileID != "fake" || preferences.Recent.BundleID != "latest" {
		t.Fatalf("preferences = %#v", preferences)
	}
	body, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var stored map[string]any
	if err := json.Unmarshal(body, &stored); err != nil {
		t.Fatal(err)
	}
	if stored["version"] != float64(2) {
		t.Fatalf("version = %v", stored["version"])
	}

	// A default `mj go` saved is left alone.
	path = filepath.Join(t.TempDir(), "go.json")
	if err := GoPreferencesSaveRecipe(path, "/a", goTestRecipe("docker"), true); err != nil {
		t.Fatal(err)
	}
	if err := GoPreferencesRememberFirstPair(path, "other", "podman"); err != nil {
		t.Fatal(err)
	}
	preferences, err = GoPreferencesLoad(path)
	if err != nil {
		t.Fatal(err)
	}
	if preferences.Default.TargetID != "docker" {
		t.Fatalf("default = %#v", preferences.Default)
	}
}

func TestLongAndSameNamedFoldersHaveDistinctValidWorkspaceNames(t *testing.T) {
	a := GoPreferencesWorkspaceName("/one/project")
	b := GoPreferencesWorkspaceName("/two/project")
	if a == b {
		t.Fatalf("same-named folders share %q", a)
	}
	long := GoPreferencesWorkspaceName("/" + strings.Repeat("folder", 100))
	if _, _, err := NormalizeWorkspaceName(long); err != nil {
		t.Fatalf("%q: %v", long, err)
	}
	if a != GoPreferencesWorkspaceName("/one/project") {
		t.Fatal("workspace name is not stable")
	}
}

func TestWorkspaceBindingsPreserveSelectionAndKeepSameNamedFoldersSeparate(t *testing.T) {
	path := filepath.Join(t.TempDir(), "go.json")
	first := "/one/project"
	must := func(err error) {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
	}
	must(GoPreferencesBindWorkspace(path, first, "workspace-a"))
	must(GoPreferencesRememberSession(path, first, "workspace-a", "conversation-a"))
	must(GoPreferencesSaveRecipe(path, first, goTestRecipe("docker"), false))
	must(GoPreferencesBindWorkspace(path, first, "workspace-a"))
	must(GoPreferencesBindWorkspace(path, "/two/project", "workspace-b"))
	preferences, err := GoPreferencesLoad(path)
	must(err)
	if session := preferences.Workspace(first).LastSessionID; session == nil || *session != "conversation-a" {
		t.Fatalf("last session = %v", session)
	}
	if id := preferences.Workspace("/two/project").WorkspaceID; id != "workspace-b" {
		t.Fatalf("workspace = %q", id)
	}
	must(GoPreferencesBindWorkspace(path, first, "replacement"))
	if GoPreferencesRememberSession(path, first, "workspace-a", "stale") == nil {
		t.Fatal("remembered a session for a replaced binding")
	}
	preferences, err = GoPreferencesLoad(path)
	must(err)
	if session := preferences.Workspace(first).LastSessionID; session != nil {
		t.Fatalf("last session = %q", *session)
	}
}

func TestPreviousPreferencesUpgradeWithoutLosingTheRecipe(t *testing.T) {
	path := filepath.Join(t.TempDir(), "go.json")
	recipe, err := json.Marshal(goTestRecipe("docker"))
	if err != nil {
		t.Fatal(err)
	}
	old := `{"version":1,"default":` + string(recipe) + `,"projects":[]}`
	if err := os.WriteFile(path, []byte(old), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := GoPreferencesBindWorkspace(path, "/project", "workspace"); err != nil {
		t.Fatal(err)
	}
	preferences, err := GoPreferencesLoad(path)
	if err != nil {
		t.Fatal(err)
	}
	if preferences.version != 2 {
		t.Fatalf("version = %d", preferences.version)
	}
	if recipe := preferences.Recipe("/project"); recipe == nil || recipe.TargetID != "docker" {
		t.Fatalf("recipe = %#v", recipe)
	}
}

func TestBrokenPreferencesAreReportedWithoutOverwritingThem(t *testing.T) {
	path := filepath.Join(t.TempDir(), "go.json")
	if err := os.WriteFile(path, []byte("broken"), 0o644); err != nil {
		t.Fatal(err)
	}
	if GoPreferencesSaveRecipe(path, "/a", goTestRecipe("docker"), false) == nil {
		t.Fatal("saved over broken preferences")
	}
	body, err := os.ReadFile(path)
	if err != nil || string(body) != "broken" {
		t.Fatalf("preferences = %q, %v", body, err)
	}
}
