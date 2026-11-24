// Port of the tests in mj-import/src/project_catalog.rs.

package mjimport

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"maps"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strings"
	"testing"
	"time"

	_ "modernc.org/sqlite"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

func catalogGit(t *testing.T, directory string, purpose string, args ...string) {
	t.Helper()
	command := core.NewCommandSpec("git", append([]string{"-C", directory}, args...)).WithPurpose(purpose)
	output, err := core.ProcessExecutor{}.Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 {
		t.Fatalf("git %q: %s", args, output.Stderr)
	}
}

func memoryRoot(key string) string {
	return filepath.Join(core.DataDir(), "projects", key, "memory")
}

func mustMemoryKey(t *testing.T, identity core.ProjectMemoryIdentity, err error) string {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
	key, err := core.ProjectMemoryIdentityKey(identity)
	if err != nil {
		t.Fatal(err)
	}
	return key
}

func mustSnapshot(t *testing.T, root string) core.ProjectMemorySnapshot {
	t.Helper()
	store := core.NewProjectMemoryStore(root)
	snapshot, err := store.Snapshot()
	if err != nil {
		t.Fatal(err)
	}
	return snapshot
}

func TestUnavailableHistoryIsRetiredAndRenamedRepositoriesKeepMemory(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("#1291: local bare fixture; a Windows controller runs no local bare sessions")
	}
	const child = "MJ_DISCOVERY_RECOVERY_TEST_CHILD"
	if os.Getenv(child) == "" {
		targets.NewIsolatedTest(targets.TestName("project_catalog::tests", "TestUnavailableHistoryIsRetiredAndRenamedRepositoriesKeepMemory")).
			Env(child, "1").
			IsolatedStore(t.TempDir()).
			Run()
		return
	}
	root := t.TempDir()
	writer := store.InstallIsolatedTestWriter()
	defer writer.Shutdown()
	checkout := filepath.Join(root, "healthy-checkout")
	if err := os.Mkdir(checkout, 0o755); err != nil {
		t.Fatal(err)
	}
	for _, args := range [][]string{
		{"init", "-q"},
		{"remote", "add", "origin", "https://github.com/Example/new-name.git"},
		{"config", "user.name", "Discovery Test"},
		{"config", "user.email", "discovery@example.invalid"},
		{"commit", "--allow-empty", "-qm", "initial"},
	} {
		catalogGit(t, checkout, "initialize discovery recovery checkout", args...)
	}
	github := "Example/legacy-repository"
	legacyBundle := core.ProjectBundle{
		PrimaryRepo: "old-id",
		Repositories: []core.ProjectRepository{{
			ID:          "old-id",
			Github:      &github,
			Destination: "old-id",
		}},
	}
	if _, _, err := core.ConfigUpdate(func(config *core.Config) (struct{}, error) {
		clear(config.Profiles)
		config.Bundles = map[string]core.ProjectBundle{"old-project": legacyBundle}
		config.Targets = map[string]core.TargetTemplate{"localhost": core.TargetTemplateLocalBare{}}
		return struct{}{}, nil
	}); err != nil {
		t.Fatal(err)
	}
	legacyMember, err := core.RepositoryMemoryIdentityFromConfigured(&legacyBundle.Repositories[0])
	if err != nil {
		t.Fatal(err)
	}
	legacyKey := mustMemoryKey(t, core.NewProjectMemoryIdentityBundle(legacyMember, []core.RepositoryMemoryIdentity{legacyMember}), nil)
	legacyRoot := memoryRoot(legacyKey)
	bundleMemory := core.ProjectMemorySnapshot{Files: map[string]string{"/bundle.md": "Unrelated configured project memory"}}
	legacyStore := core.NewProjectMemoryStore(legacyRoot)
	if _, err := legacyStore.InstallSnapshot(&bundleMemory); err != nil {
		t.Fatal(err)
	}
	record := store.TestSession("healthy", "old-project")
	record.ProjectDirectory = &checkout
	record.TargetTemplateID = "localhost"
	record.Target = core.TargetLocatorLocalBare{WorkerRoot: filepath.Join(root, "workers", record.ID)}
	rawIdentity, err := core.ProjectMemoryIdentityForLegacySession(&record, &legacyBundle, nil)
	rawRoot := memoryRoot(mustMemoryKey(t, rawIdentity, err))
	memory := core.ProjectMemorySnapshot{Files: map[string]string{"/notes.md": "Keep this raw-session memory"}}
	rawStore := core.NewProjectMemoryStore(rawRoot)
	if _, err := rawStore.InstallSnapshot(&memory); err != nil {
		t.Fatal(err)
	}
	if err := store.SaveSession(&record); err != nil {
		t.Fatal(err)
	}

	missing := filepath.Join(root, "deleted-checkout")
	nongit := filepath.Join(root, "not-a-repository")
	if err := os.Mkdir(nongit, 0o755); err != nil {
		t.Fatal(err)
	}
	for index, path := range []string{missing, nongit} {
		message := "old unavailable error"
		if err := store.SeedFailure(`"codex"`, root, path, false, &message); err != nil {
			t.Fatal(err)
		}
		record := store.TestSession(fmt.Sprintf("stale-%d", index), "old-project")
		record.ProjectDirectory = &path
		record.TargetTemplateID = "localhost"
		if err := store.SaveSession(&record); err != nil {
			t.Fatal(err)
		}
	}
	executor := core.CancellableProcessExecutorWithTimeout(30 * time.Second)
	changes, err := store.ProjectDiscoveryChanges(false)
	if err != nil {
		t.Fatal(err)
	}
	for _, change := range changes {
		message := "previous discovery failure"
		if err := store.FinishProjectDiscovery(change.Sequence, &message); err != nil {
			t.Fatal(err)
		}
	}
	// All candidates are behind the durable progress frontier. Starting a
	// new daemon's catalog must retry them without a user pressing Retry.
	catalog := &Catalog{}
	ctx, shutdown := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		defer close(done)
		catalog.Run(ctx)
	}()
	deadline := time.Now().Add(10 * time.Second)
	for {
		time.Sleep(25 * time.Millisecond)
		view, err := catalog.View()
		if err != nil {
			t.Fatal(err)
		}
		if view.Status.State != core.ProjectCatalogStateRefreshing {
			if view.Status.State != core.ProjectCatalogStateReady {
				t.Fatalf("status = %+v", view.Status)
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("the catalog did not finish refreshing within ten seconds")
		}
	}
	shutdown()
	<-done
	if errs, err := store.DiscoveryErrors(); err != nil || len(errs) != 0 {
		t.Fatalf("discovery errors = %q, %v", errs, err)
	}
	if failures, err := store.SeedFailures(); err != nil || len(failures) != 0 {
		t.Fatalf("seed failures = %+v, %v", failures, err)
	}
	if changes, err := store.ProjectDiscoveryChanges(false); err != nil || len(changes) != 0 {
		t.Fatalf("changes = %+v, %v", changes, err)
	}
	state, err := store.LoadState()
	if err != nil {
		t.Fatal(err)
	}
	if state.Sessions.Len() != 3 {
		t.Fatalf("retiring candidates preserves session history: %d sessions", state.Sessions.Len())
	}
	healthy, _ := state.Sessions.Get("healthy")
	accepted := healthy.Project
	if accepted == nil || accepted.Bundle.PrimaryRepo != "new-name" {
		t.Fatalf("accepted = %+v", accepted)
	}
	acceptedIdentity, err := accepted.MemoryIdentity()
	canonicalRoot := memoryRoot(mustMemoryKey(t, acceptedIdentity, err))
	if got := mustSnapshot(t, canonicalRoot); !reflect.DeepEqual(got, memory) {
		t.Fatalf("canonical memory = %+v", got)
	}
	if resolved, err := core.ResolveCanonicalRoot(rawRoot); err != nil || resolved != canonicalRoot {
		t.Fatalf("raw root resolves to %q, %v", resolved, err)
	}
	if got := mustSnapshot(t, legacyRoot); !reflect.DeepEqual(got, bundleMemory) {
		t.Fatalf("legacy memory = %+v", got)
	}
	// An explicit configured-project migration uses its old source, even
	// when the new snapshot has a different repository ID.
	if err := migrateMemory(accepted, &legacyBundle, nil); err != nil {
		t.Fatal(err)
	}
	merged := core.ProjectMemorySnapshot{Files: maps.Clone(memory.Files)}
	maps.Copy(merged.Files, bundleMemory.Files)
	if got := mustSnapshot(t, canonicalRoot); !reflect.DeepEqual(got, merged) {
		t.Fatalf("merged memory = %+v", got)
	}
	if resolved, err := core.ResolveCanonicalRoot(legacyRoot); err != nil || resolved != canonicalRoot {
		t.Fatalf("legacy root resolves to %q, %v", resolved, err)
	}

	// A location already accepted into the catalog must be checked again:
	// deleting its Git metadata cannot leave it falsely discoverable.
	if err := os.RemoveAll(filepath.Join(checkout, ".git")); err != nil {
		t.Fatal(err)
	}
	var unavailable core.RepositoryUnavailable
	if err := discoverLocal(checkout, executor); !errors.As(err, &unavailable) {
		t.Fatalf("error = %v", err)
	}
	if historicalDiscoveryError(errors.New("permission denied"), checkout) == nil {
		t.Fatal("a real failure was treated as unavailable")
	}
	malformed := filepath.Join(root, "malformed-native-session.jsonl")
	if err := os.WriteFile(malformed, []byte("{\"cwd\": broken JSON\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	message := "old parse failure"
	if err := store.SeedFailure(`"codex"`, root, malformed, true, &message); err != nil {
		t.Fatal(err)
	}
	errs, err := refresh(executor, true)
	if err != nil {
		t.Fatal(err)
	}
	if len(errs) != 1 {
		t.Fatalf("real discovery failures must remain visible: %q", errs)
	}
	if !strings.Contains(errs[0], "malformed-native-session.jsonl") {
		t.Fatalf("error = %q", errs[0])
	}
}

func TestDiscoverySeedsTenSessionsOnceAndRefreshesOnlyMjolnirChanges(t *testing.T) {
	const child = "MJ_PROJECT_CATALOG_TEST_CHILD"
	if os.Getenv(child) == "" {
		targets.NewIsolatedTest(targets.TestName("project_catalog::tests", "TestDiscoverySeedsTenSessionsOnceAndRefreshesOnlyMjolnirChanges")).
			Env(child, "1").
			IsolatedStore(t.TempDir()).
			Run()
		return
	}
	root := t.TempDir()
	writer := store.InstallIsolatedTestWriter()
	defer writer.Shutdown()
	home := filepath.Join(root, "codex")
	if err := os.MkdirAll(home, 0o755); err != nil {
		t.Fatal(err)
	}
	index, err := sql.Open("sqlite", filepath.Join(home, "state_5.sqlite"))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := index.Exec("CREATE TABLE threads(id TEXT,rollout_path TEXT,updated_at INTEGER,name TEXT,title TEXT,cwd TEXT,git_branch TEXT,history_mode TEXT,archived INTEGER,source TEXT,preview TEXT)"); err != nil {
		t.Fatal(err)
	}
	initialize := func(path string) {
		if err := os.MkdirAll(path, 0o755); err != nil {
			t.Fatal(err)
		}
		for _, args := range [][]string{
			{"init", "-q"},
			{"config", "user.name", "Discovery Test"},
			{"config", "user.email", "discovery@example.invalid"},
			{"commit", "--allow-empty", "-qm", "initial"},
		} {
			catalogGit(t, path, "initialize discovery fixture", args...)
		}
	}
	for i := range 12 {
		project := filepath.Join(root, fmt.Sprintf("project-%02d", i))
		initialize(project)
		log := filepath.Join(home, fmt.Sprintf("session-%d.jsonl", i))
		if err := os.WriteFile(log, []byte("conversation deliberately unreadable"), 0o644); err != nil {
			t.Fatal(err)
		}
		if _, err := index.Exec("INSERT INTO threads VALUES(?1,?2,?3,'title','title',?4,'main','paginated',0,'cli','preview')",
			fmt.Sprintf("session-%d", i), log, i, project); err != nil {
			t.Fatal(err)
		}
	}
	profile := func(kind core.HarnessKind, home string) core.HarnessProfile {
		profile := core.NewHarnessProfile(kind, home)
		profile.Enabled = true
		profile.MjolnirGuardian = true
		return profile
	}
	if _, _, err := core.ConfigUpdate(func(config *core.Config) (struct{}, error) {
		config.Profiles = map[string]core.HarnessProfile{
			"codex":          profile(core.HarnessKindCodex, home),
			"missing-claude": profile(core.HarnessKindClaude, filepath.Join(root, "missing")),
		}
		config.Targets = map[string]core.TargetTemplate{"localhost": core.TargetTemplateLocalBare{}}
		return struct{}{}, nil
	}); err != nil {
		t.Fatal(err)
	}
	errs, err := refresh(core.CancellableProcessExecutorWithTimeout(30*time.Second), false)
	if err != nil {
		t.Fatal(err)
	}
	mentionsMissing := false
	for _, message := range errs {
		mentionsMissing = mentionsMissing || strings.Contains(message, "missing")
	}
	if !mentionsMissing {
		t.Fatalf("errors = %q", errs)
	}
	view, err := store.ReadProjectCatalog()
	if err != nil {
		t.Fatal(err)
	}
	if len(view.Projects) != 10 || len(view.Locations) != 10 {
		t.Fatalf("projects = %d locations = %d", len(view.Projects), len(view.Locations))
	}
	// Discovered projects belong to the catalog. config.toml holds only the
	// bundles the user saved.
	if config, err := core.ConfigLoad(); err != nil || len(config.Bundles) != 0 {
		t.Fatalf("bundles = %+v, %v", config.Bundles, err)
	}
	for _, location := range view.Locations {
		if base := filepath.Base(location.Directory); base == "project-00" || base == "project-01" {
			t.Fatalf("the two oldest sessions were seeded: %+v", location)
		}
	}
	if err := index.Close(); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, "state_5.sqlite"), []byte("broken index that must never be scanned again"), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := refresh(core.CancellableProcessExecutorWithTimeout(30*time.Second), false); err != nil {
		t.Fatal(err)
	}
	if view, err := store.ReadProjectCatalog(); err != nil || len(view.Locations) != 10 {
		t.Fatalf("locations = %d, %v", len(view.Locations), err)
	}
	path := filepath.Join(root, "new-mj-project")
	initialize(path)
	selected := filepath.Join(path, "src")
	if err := os.Mkdir(selected, 0o755); err != nil {
		t.Fatal(err)
	}
	record := store.TestSession("new-mj-session", "legacy-raw")
	record.ProjectDirectory = &selected
	record.TargetTemplateID = "localhost"
	record.LastProfile = "codex"
	if err := store.SaveSession(&record); err != nil {
		t.Fatal(err)
	}
	if _, err := refresh(core.CancellableProcessExecutorWithTimeout(30*time.Second), false); err != nil {
		t.Fatal(err)
	}
	state, err := store.LoadState()
	if err != nil {
		t.Fatal(err)
	}
	loaded, _ := state.Sessions.Get(record.ID)
	if loaded.ProjectDirectory == nil || *loaded.ProjectDirectory != selected {
		t.Fatalf("project directory = %v", loaded.ProjectDirectory)
	}
	canonical, err := catalogCanonicalize(path)
	if err != nil {
		t.Fatal(err)
	}
	var identity core.RepositoryIdentity
	for _, id := range sortedIdentityKeys(loaded.Project) {
		identity = loaded.Project.Identities[id]
		break
	}
	if identity != (core.RepositoryIdentityLocal{Root: canonical}) {
		t.Fatalf("identity = %#v", identity)
	}
	if view, err := store.ReadProjectCatalog(); err != nil || len(view.Locations) != 11 {
		t.Fatalf("locations = %d, %v", len(view.Locations), err)
	}
	if config, err := core.ConfigLoad(); err != nil || len(config.Bundles) != 0 {
		t.Fatalf("bundles = %+v, %v", config.Bundles, err)
	}
	if changes, err := store.ProjectDiscoveryChanges(false); err != nil || len(changes) != 0 {
		t.Fatalf("changes = %+v, %v", changes, err)
	}
}

// sortedIdentityKeys is a snapshot's repository ids in order (Rust's
// BTreeMap iteration); none for a missing project.
func sortedIdentityKeys(project *core.ProjectBundleSnapshot) []string {
	if project == nil {
		return nil
	}
	return slices.Sorted(maps.Keys(project.Identities))
}
