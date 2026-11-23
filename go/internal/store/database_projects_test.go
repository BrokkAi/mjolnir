// Port of the tests in mj-store/src/database/projects.rs.

package store

import (
	"encoding/json"
	"path/filepath"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// projectsTestSnapshot is a one-repository project whose repository has id.
func projectsTestSnapshot(id string) core.ProjectBundleSnapshot {
	github := "acme/app"
	return core.ProjectBundleSnapshot{
		Bundle: core.ProjectBundle{
			PrimaryRepo: id,
			Repositories: []core.ProjectRepository{{
				ID:          id,
				Github:      &github,
				Destination: "app",
			}},
		},
		Identities: map[string]core.RepositoryIdentity{
			id: core.RepositoryIdentityGithub{Owner: "acme", Repository: "app"},
		},
		NetworkSources: map[string]core.NetworkGitSource{},
	}
}

// projectsTestClone is a deep copy, as Rust's clone of a snapshot.
func projectsTestClone(t *testing.T, project core.ProjectBundleSnapshot) core.ProjectBundleSnapshot {
	t.Helper()
	encoded, err := json.Marshal(project)
	if err != nil {
		t.Fatal(err)
	}
	var clone core.ProjectBundleSnapshot
	if err := json.Unmarshal(encoded, &clone); err != nil {
		t.Fatal(err)
	}
	return clone
}

// projectsTestSame compares two optional snapshots as Rust's PartialEq does,
// through their encoding.
func projectsTestSame(t *testing.T, got *core.ProjectBundleSnapshot, want *core.ProjectBundleSnapshot) bool {
	t.Helper()
	left, err := json.Marshal(got)
	if err != nil {
		t.Fatal(err)
	}
	right, err := json.Marshal(want)
	if err != nil {
		t.Fatal(err)
	}
	return string(left) == string(right)
}

func projectsTestStore(t *testing.T, path, bundleID string, project core.ProjectBundleSnapshot) string {
	t.Helper()
	canonical, err := storeCatalogProjectAt(path, bundleID, &project, true)
	if err != nil {
		t.Fatal(err)
	}
	return canonical
}

func projectsTestSave(t *testing.T, path string, record core.SessionRecord) {
	t.Helper()
	if err := SaveSessionTo(path, &record); err != nil {
		t.Fatal(err)
	}
}

func projectsTestPrompt(t *testing.T, path, sessionID, bundleID string, ordinal uint64, text string) {
	t.Helper()
	if err := recordPromptTo(path, sessionID, bundleID, ordinal, nil, text); err != nil {
		t.Fatal(err)
	}
}

func projectsTestCatalogSize(t *testing.T, path string) int {
	t.Helper()
	connection, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	view, err := readProjectCatalogFrom(connection)
	if err != nil {
		t.Fatal(err)
	}
	return len(view.Projects)
}

func projectsTestLoad(t *testing.T, path, sessionID string) core.SessionRecord {
	t.Helper()
	state, err := LoadStateFrom(path)
	if err != nil {
		t.Fatal(err)
	}
	record, ok := state.Sessions.Get(sessionID)
	if !ok {
		t.Fatalf("session %s did not load", sessionID)
	}
	return record
}

func TestEditingAProjectLayoutPreservesOldWorkerHistoryAndCheckpointIDs(t *testing.T) {
	path := filepath.Join(t.TempDir(), "projects.sqlite")
	old := projectsTestSnapshot("app")
	oldSession := session("old-session", "project")
	projectsTestSave(t, path, oldSession)
	projectsTestStore(t, path, "project", projectsTestClone(t, old))
	edited := projectsTestClone(t, old)
	edited.Bundle.Repositories[0].Destination = "custom-layout"
	projectsTestStore(t, path, "project", edited)
	projectsTestSave(t, path, session("new-session", "project"))
	projectsTestPrompt(t, path, "old-session", "project", 1, "old worker prompt")
	projectsTestPrompt(t, path, "new-session", "project", 1, "new worker prompt")
	projectsTestSave(t, path, oldSession)
	loadedOld := projectsTestLoad(t, path, "old-session")
	if !projectsTestSame(t, loadedOld.Project, &old) {
		t.Fatalf("old session's project changed: %+v", loadedOld.Project)
	}
	if loadedOld.BundleID == "project" {
		t.Fatal("the old session must move to the retired project")
	}
	if loadedNew := projectsTestLoad(t, path, "new-session"); loadedNew.BundleID != "project" {
		t.Fatalf("new session bundle = %s", loadedNew.BundleID)
	}
	oldHistory, err := searchPromptsFrom(path, "old-session", "project", core.HistoryScopeProject, "worker")
	if err != nil {
		t.Fatal(err)
	}
	newHistory, err := searchPromptsFrom(path, "new-session", "project", core.HistoryScopeProject, "worker")
	if err != nil {
		t.Fatal(err)
	}
	if len(oldHistory) != 1 || len(newHistory) != 1 {
		t.Fatalf("old history %v, new history %v", oldHistory, newHistory)
	}
	if oldHistory[0].Text == newHistory[0].Text {
		t.Fatalf("the two projects share history: %s", oldHistory[0].Text)
	}
	if size := projectsTestCatalogSize(t, path); size != 1 {
		t.Fatalf("%d catalog projects", size)
	}
}

func TestEquivalentProjectsMergeDurablyAndPreserveAcceptedRepositoryIDs(t *testing.T) {
	path := filepath.Join(t.TempDir(), "projects.sqlite")
	primary := projectsTestSnapshot("app")
	alias := projectsTestSnapshot("legacy-app")
	if canonical := projectsTestStore(t, path, "canonical", primary); canonical != "canonical" {
		t.Fatalf("canonical = %s", canonical)
	}
	if canonical := projectsTestStore(t, path, "duplicate", projectsTestClone(t, alias)); canonical != "canonical" {
		t.Fatalf("duplicate stored as %s", canonical)
	}
	connection, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	if canonical, err := canonicalProjectID(connection, "duplicate"); err != nil || canonical != "canonical" {
		t.Fatalf("canonical of duplicate = %s, %v", canonical, err)
	}
	var stored string
	if err := connection.QueryRow("SELECT snapshot_json FROM project_aliases WHERE bundle_id='duplicate'").Scan(&stored); err != nil {
		t.Fatal(err)
	}
	var storedSnapshot core.ProjectBundleSnapshot
	if err := json.Unmarshal([]byte(stored), &storedSnapshot); err != nil {
		t.Fatal(err)
	}
	if storedSnapshot.Bundle.PrimaryRepo != "legacy-app" {
		t.Fatalf("alias primary repo = %s", storedSnapshot.Bundle.PrimaryRepo)
	}
	view, err := readProjectCatalogFrom(connection)
	if err != nil {
		t.Fatal(err)
	}
	if len(view.Projects) != 1 {
		t.Fatalf("%d catalog projects", len(view.Projects))
	}
	_ = connection.Close()
	if canonical := projectsTestStore(t, path, "duplicate", projectsTestClone(t, alias)); canonical != "canonical" {
		t.Fatalf("restored duplicate stored as %s", canonical)
	}
	if size := projectsTestCatalogSize(t, path); size != 1 {
		t.Fatalf("%d catalog projects", size)
	}
	edited := projectsTestClone(t, alias)
	edited.Bundle.Repositories[0].Destination = "different-layout"
	if canonical := projectsTestStore(t, path, "duplicate", edited); canonical != "duplicate" {
		t.Fatalf("edited duplicate stored as %s", canonical)
	}
	connection, err = open(path)
	if err != nil {
		t.Fatal(err)
	}
	canonical, err := canonicalProjectID(connection, "duplicate")
	_ = connection.Close()
	if err != nil || canonical != "duplicate" {
		t.Fatalf("canonical of edited duplicate = %s, %v", canonical, err)
	}
	if size := projectsTestCatalogSize(t, path); size != 2 {
		t.Fatalf("%d catalog projects", size)
	}
}

func TestConsolidationRebindsHistoryAndFreezesLegacySessionDefinitionAcrossRestart(t *testing.T) {
	path := filepath.Join(t.TempDir(), "projects.sqlite")
	canonical := projectsTestSnapshot("app")
	legacy := projectsTestSnapshot("legacy-app")
	legacySession := session("legacy-session", "duplicate")
	projectsTestSave(t, path, legacySession)
	projectsTestPrompt(t, path, "legacy-session", "duplicate", 1, "accepted prompt")
	projectsTestStore(t, path, "canonical", canonical)
	projectsTestStore(t, path, "duplicate", projectsTestClone(t, legacy))
	loaded := projectsTestLoad(t, path, "legacy-session")
	if loaded.BundleID != "canonical" {
		t.Fatalf("bundle = %s", loaded.BundleID)
	}
	if loaded.Project == nil || loaded.Project.Bundle.PrimaryRepo != "legacy-app" {
		t.Fatalf("project = %+v", loaded.Project)
	}
	// A worker still presents the old ID after a merge.
	projectsTestPrompt(t, path, "legacy-session", "duplicate", 2, "late prompt")
	projectsTestSave(t, path, legacySession)
	history, err := searchPromptsFrom(path, "other-session", "duplicate", core.HistoryScopeProject, "prompt")
	if err != nil {
		t.Fatal(err)
	}
	if len(history) != 2 {
		t.Fatalf("history = %v", history)
	}
	if reloaded := projectsTestLoad(t, path, "legacy-session"); !projectsTestSame(t, reloaded.Project, &legacy) {
		t.Fatalf("legacy project changed: %+v", reloaded.Project)
	}
}
