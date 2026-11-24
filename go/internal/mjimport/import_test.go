// Port of mj-import/src/import/tests.rs.
//
// Test helpers carry an `importTest` prefix: the package's other test files
// have helpers of their own.
//
// Tests that read a conversation go through brokk-sessionwiki's adapters,
// which the Go build does not have yet (see import_adapter_transcript.go).
// They run up to the first such read and skip there.

package mjimport

import (
	"database/sql"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// importNeedsSessionwiki skips a test at the point it reads a conversation
// through the SessionWiki adapters, while they are not ported.
func importNeedsSessionwiki(t *testing.T) {
	t.Helper()
	adapter, ok := importAdapter(core.HarnessKindClaude)
	if !ok {
		return
	}
	if _, pending := adapter.(sessionwikiPendingAdapter); pending {
		t.Skip("needs brokk-sessionwiki (not yet ported)")
	}
}

func importTestContainerTemplate() core.ContainerTemplate {
	return core.ContainerTemplate{Image: "agent-dev:latest"}
}

func importTestGit(t *testing.T, path string, arguments ...string) {
	t.Helper()
	command := exec.Command("git", arguments...)
	command.Dir = path
	output, err := command.CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v: %s", arguments, err, output)
	}
}

func importTestInitializeRepository(t *testing.T, path string, id string) {
	t.Helper()
	if err := os.MkdirAll(path, 0o777); err != nil {
		t.Fatal(err)
	}
	importTestGit(t, path, "init", "-q", "-b", "main")
	importTestGit(t, path, "config", "user.name", "Hel Test")
	importTestGit(t, path, "config", "user.email", "hel@example.test")
	importTestGit(t, path, "remote", "add", "origin", fmt.Sprintf("https://github.com/example/%s.git", id))
	importTestWrite(t, filepath.Join(path, "README.md"), id)
	importTestGit(t, path, "add", ".")
	importTestGit(t, path, "commit", "-qm", "base")
	// Import deltas against the tracked remote, so a realistic checkout
	// needs a remote-tracking ref.
	importTestGit(t, path, "update-ref", "refs/remotes/origin/main", "HEAD")
	importTestGit(t, path, "branch", "--set-upstream-to", "origin/main", "main")
}

func importTestWrite(t *testing.T, path string, contents string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(contents), 0o666); err != nil {
		t.Fatal(err)
	}
}

func importTestMkdir(t *testing.T, path string) {
	t.Helper()
	if err := os.MkdirAll(path, 0o777); err != nil {
		t.Fatal(err)
	}
}

func importTestCanonical(t *testing.T, path string) string {
	t.Helper()
	canonical, err := importCanonicalize(path)
	if err != nil {
		t.Fatal(err)
	}
	return canonical
}

func importTestSymlink(t *testing.T, target, link string) {
	t.Helper()
	if err := os.Symlink(target, link); err != nil {
		t.Fatal(err)
	}
}

func importTestJSON(t *testing.T, value any) string {
	t.Helper()
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return string(encoded)
}

func importTestEqual(t *testing.T, got, want any) {
	t.Helper()
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %#v, want %#v", got, want)
	}
}

func importTestErrorContains(t *testing.T, err error, contains string, absent string) {
	t.Helper()
	if err == nil {
		t.Fatal("expected an error")
	}
	if !strings.Contains(err.Error(), contains) {
		t.Fatal(err)
	}
	if absent != "" && strings.Contains(err.Error(), absent) {
		t.Fatal(err)
	}
}

func importTestSkipUnlessUnix(t *testing.T) {
	t.Helper()
	if os.PathSeparator != '/' {
		t.Skip("unix only")
	}
}

func TestSessionTargetsIncludeEditedRootsAndKeepCwdPrimary(t *testing.T) {
	directory := importTestDurableFixtureDirectory(t)
	app := filepath.Join(directory, "app")
	sibling := filepath.Join(directory, "sibling")
	importTestInitializeRepository(t, app, "app")
	importTestInitializeRepository(t, sibling, "sibling")
	transcript := ClaudeTranscript{
		Cwd:         app,
		EditedPaths: []string{filepath.Join(sibling, "src/lib.rs")},
	}

	targets, err := SessionEditTargetsFor(&transcript, filepath.Join(directory, "profile"))
	if err != nil {
		t.Fatal(err)
	}
	if len(targets.GitRoots) != 2 {
		t.Fatal(targets.GitRoots)
	}
	for _, root := range []string{importTestCanonical(t, app), importTestCanonical(t, sibling)} {
		if !importTestContains(targets.GitRoots, root) {
			t.Fatal(targets.GitRoots)
		}
	}
	if len(targets.NonGitDirs) != 0 || len(targets.ScratchGitRoots) != 0 {
		t.Fatalf("%#v", targets)
	}
}

func importTestContains(paths []string, path string) bool {
	for _, candidate := range paths {
		if importComparePaths(candidate, path) == 0 {
			return true
		}
	}
	return false
}

func TestSessionTargetsOmitRepositoriesUnderTemporaryDirectories(t *testing.T) {
	scratchHome := t.TempDir()
	directory := t.TempDir()
	app := filepath.Join(directory, "app")
	sibling := filepath.Join(directory, "sibling")
	throwaway := filepath.Join(scratchHome, "throwaway")
	importTestInitializeRepository(t, app, "app")
	importTestInitializeRepository(t, sibling, "sibling")
	importTestInitializeRepository(t, throwaway, "throwaway")
	prefixes := []string{importTestCanonical(t, scratchHome)}
	transcript := ClaudeTranscript{
		Cwd:         app,
		EditedPaths: []string{filepath.Join(sibling, "src/lib.rs"), filepath.Join(throwaway, "notes.md")},
	}

	targets, err := sessionEditTargetsWithScratchPrefixes(&transcript, filepath.Join(directory, "profile"), prefixes)
	if err != nil {
		t.Fatal(err)
	}

	importTestEqual(t, targets.GitRoots, []string{importTestCanonical(t, app), importTestCanonical(t, sibling)})
	importTestEqual(t, targets.ScratchGitRoots, []string{importTestCanonical(t, throwaway)})
}

func TestSessionTargetsKeepACwdRepositoryUnderATemporaryDirectory(t *testing.T) {
	scratchHome := t.TempDir()
	throwaway := filepath.Join(scratchHome, "throwaway")
	importTestInitializeRepository(t, throwaway, "throwaway")
	prefixes := []string{importTestCanonical(t, scratchHome)}
	transcript := ClaudeTranscript{
		Cwd:         throwaway,
		EditedPaths: []string{filepath.Join(throwaway, "notes.md")},
	}

	targets, err := sessionEditTargetsWithScratchPrefixes(&transcript, filepath.Join(scratchHome, "profile"), prefixes)
	if err != nil {
		t.Fatal(err)
	}

	importTestEqual(t, targets.GitRoots, []string{importTestCanonical(t, throwaway)})
	if len(targets.ScratchGitRoots) != 0 {
		t.Fatal(targets.ScratchGitRoots)
	}
}

func TestCodexExtractsOnlyCompletedFileChanges(t *testing.T) {
	directory := t.TempDir()
	app := filepath.Join(directory, "app")
	dependency := filepath.Join(directory, "dependency")
	importTestInitializeRepository(t, app, "app")
	importTestInitializeRepository(t, dependency, "dependency")
	home := filepath.Join(directory, "codex")
	id := "019FEB6C-5FFC-7C12-AD99-BDEAEB6BE79D"
	path := filepath.Join(home, "sessions/2026/08/14", fmt.Sprintf("rollout-%s.jsonl", id))
	importTestMkdir(t, filepath.Dir(path))
	cwd := app + "/./"
	edited := filepath.Join(dependency, "write.txt")
	failed := filepath.Join(directory, "unrelated/failed.txt")
	records := []jsonvalue.Object{
		{{Key: "type", Value: "session_meta"}, {Key: "payload", Value: jsonvalue.Object{
			{Key: "id", Value: id}, {Key: "cwd", Value: cwd}, {Key: "history_mode", Value: "paginated"},
		}}},
		importTestCodexItem(jsonvalue.Object{{Key: "type", Value: "UserMessage"}, {Key: "content", Value: []any{jsonvalue.Object{{Key: "text", Value: "edit"}}}}}),
		importTestCodexItem(jsonvalue.Object{{Key: "type", Value: "FileChange"}, {Key: "status", Value: "completed"}, {Key: "changes", Value: jsonvalue.Object{{Key: edited, Value: jsonvalue.Object{{Key: "type", Value: "add"}}}}}}),
		importTestCodexItem(jsonvalue.Object{{Key: "type", Value: "FileChange"}, {Key: "status", Value: "failed"}, {Key: "changes", Value: jsonvalue.Object{{Key: failed, Value: jsonvalue.Object{{Key: "type", Value: "add"}}}}}}),
		importTestCodexItem(jsonvalue.Object{{Key: "type", Value: "AgentMessage"}, {Key: "content", Value: []any{jsonvalue.Object{{Key: "type", Value: "Text"}, {Key: "text", Value: "edited"}}}}}),
	}
	lines := []string{}
	for _, record := range records {
		lines = append(lines, importTestJSON(t, record))
	}
	importTestWrite(t, path, strings.Join(lines, "\n"))

	located, err := LocateNativeSession(core.HarnessKindCodex, home, ClaudeSessionSelectionNativeSessionID(id))
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, located.NativeSessionID, id)
	nativeID, ok := NativeSessionIDFromPath(core.HarnessKindCodex, located.SourcePath)
	if !ok || nativeID != id {
		t.Fatalf("%q %v", nativeID, ok)
	}
	importNeedsSessionwiki(t)
	transcript, err := ReadNativeTranscript(core.HarnessKindCodex, located.SourcePath)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, transcript.Cwd, cwd)
	importTestEqual(t, transcript.EditedPaths, []string{edited})
	importTestEqual(t, importTestDialogue(t, &transcript), []importTestLine{{"user", "edit"}, {"agent", "edited"}})

	// This fixture lives in temp storage but represents two durable
	// checkouts.
	targets, err := sessionEditTargetsWithScratchPrefixes(&transcript, home, []string{})
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, targets.GitRoots, []string{importTestCanonical(t, app), importTestCanonical(t, dependency)})
	if len(targets.NonGitDirs) != 0 {
		t.Fatal(targets.NonGitDirs)
	}
}

// importTestCodexItem is an `item_completed` event wrapping item.
func importTestCodexItem(item jsonvalue.Object) jsonvalue.Object {
	return jsonvalue.Object{
		{Key: "type", Value: "event_msg"},
		{Key: "payload", Value: jsonvalue.Object{{Key: "type", Value: "item_completed"}, {Key: "item", Value: item}}},
	}
}

func TestClaudePrefersFileHistoryAndAcceptsSuccessfulEditFallback(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "session.jsonl")
	importTestWrite(t, path, NativeFixture(
		`{"type":"user","cwd":"/work/app","message":{"content":"edit"}}`+"\n"+
			`{"type":"file-history-delta","trackingPath":"src/lib.rs","backup":{"realParentDir":"/work/app/src"}}`+"\n"+
			`{"type":"assistant","message":{"content":[{"type":"tool_use","id":"ok","name":"Write","input":{"file_path":"relative.txt"}},{"type":"tool_use","id":"bad","name":"Edit","input":{"file_path":"bad.txt"}}]}}`+"\n"+
			`{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"ok","content":"done"},{"type":"tool_result","tool_use_id":"bad","is_error":true,"content":"failed"}]}}`+"\n",
	))
	importNeedsSessionwiki(t)
	transcript, err := ReadClaudeTranscript(path)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, transcript.EditedPaths, []string{NativeCwd("/work/app/src/lib.rs"), "relative.txt"})
}

func TestKimiExtractsSuccessfulEditsFromAllAgents(t *testing.T) {
	directory := t.TempDir()
	importTestMkdir(t, filepath.Join(directory, "agents/main"))
	importTestWrite(t, filepath.Join(directory, "agents/main/wire.jsonl"),
		`{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"ok","name":"Write","args":{"path":"one.txt"}}}`+"\n"+
			`{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"ok","result":{"output":"done"}}}`+"\n"+
			`{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"bad","name":"Edit","args":{"path":"bad.txt"}}}`+"\n"+
			`{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"bad","result":{"isError":true}}}`+"\n")
	main := filepath.Join(directory, "agents/main/wire.jsonl")
	child := filepath.Join(directory, "agents/nested/child/events.jsonl")
	importTestMkdir(t, filepath.Dir(child))
	if err := os.Rename(main, child); err != nil {
		t.Fatal(err)
	}
	importTestWrite(t, main, `{"type":"turn.prompt","origin":{"kind":"user"},"input":[{"type":"text","text":"edit"}]}`)
	importTestWrite(t, filepath.Join(directory, "state.json"), NativeFixture(`{"workDir":"/work/app"}`))
	importNeedsSessionwiki(t)
	transcript, err := ReadKimiTranscript(directory)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, transcript.EditedPaths, []string{"one.txt"})
}

func TestResolveBundleRequiresAnExactRootSet(t *testing.T) {
	directory := t.TempDir()
	app := filepath.Join(directory, "app")
	sibling := filepath.Join(directory, "sibling")
	importTestInitializeRepository(t, app, "app")
	importTestInitializeRepository(t, sibling, "sibling")
	targets := SessionEditTargets{
		GitRoots:        []string{importTestCanonical(t, app), importTestCanonical(t, sibling)},
		ScratchGitRoots: []string{},
		NonGitDirs:      []string{},
	}
	config := core.DefaultConfig()
	resolution, err := ResolveBundle(&config, app, &targets, nil)
	if err != nil {
		t.Fatal(err)
	}
	synthesized, ok := resolution.(BundleResolutionSynthesized)
	if !ok {
		t.Fatalf("expected synthesized bundle: %#v", resolution)
	}
	bundle := synthesized.Bundle
	if len(bundle.Repositories) != 2 || bundle.PrimaryRepo != "app" {
		t.Fatalf("%#v", bundle)
	}

	config = core.DefaultConfig()
	config.Bundles = map[string]core.ProjectBundle{"multi": bundle}
	resolution, err = ResolveBundle(&config, app, &targets, nil)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, resolution, BundleResolution(BundleResolutionExisting("multi")))
	appOnly := SessionEditTargets{
		GitRoots:        []string{importTestCanonical(t, app)},
		ScratchGitRoots: []string{},
		NonGitDirs:      []string{},
	}
	multi := "multi"
	if _, err := ResolveBundle(&config, app, &appOnly, &multi); err == nil {
		t.Fatal("expected an error")
	}
}

// importTestInitializeLocalRepository makes a repository without a remote,
// which takes the `Local` identity: the case a linked worktree used to split
// into its own project.
func importTestInitializeLocalRepository(t *testing.T, path string, id string) {
	t.Helper()
	importTestInitializeRepository(t, path, id)
	importTestGit(t, path, "remote", "remove", "origin")
}

func importTestAddWorktree(t *testing.T, repository string, worktree string, branch string) string {
	t.Helper()
	importTestGit(t, repository, "worktree", "add", "-q", "-b", branch, worktree)
	return importTestCanonical(t, worktree)
}

func TestResolveBundleMatchesAnExistingBundleFromALinkedWorktree(t *testing.T) {
	directory := t.TempDir()
	app := filepath.Join(directory, "app")
	importTestInitializeLocalRepository(t, app, "app")
	worktree := importTestAddWorktree(t, app, filepath.Join(directory, "app2"), "side")
	app = importTestCanonical(t, app)

	config := core.DefaultConfig()
	config.Bundles = map[string]core.ProjectBundle{
		"app": {
			PrimaryRepo:  "app",
			Repositories: []core.ProjectRepository{{ID: "app", Local: &app, Destination: "app"}},
		},
	}
	targets := SessionEditTargets{GitRoots: []string{worktree}, NonGitDirs: []string{}, ScratchGitRoots: []string{}}

	resolution, err := ResolveBundle(&config, worktree, &targets, nil)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, resolution, BundleResolution(BundleResolutionExisting("app")))
}

func TestResolveBundleSynthesizesAWorktreeSessionAsItsMainRepository(t *testing.T) {
	directory := t.TempDir()
	app := filepath.Join(directory, "app")
	importTestInitializeLocalRepository(t, app, "app")
	worktree := importTestAddWorktree(t, app, filepath.Join(directory, "app2"), "side")
	app = importTestCanonical(t, app)
	targets := SessionEditTargets{GitRoots: []string{worktree, app}, NonGitDirs: []string{}, ScratchGitRoots: []string{}}

	config := core.DefaultConfig()
	resolution, err := ResolveBundle(&config, worktree, &targets, nil)
	if err != nil {
		t.Fatal(err)
	}
	synthesized, ok := resolution.(BundleResolutionSynthesized)
	if !ok {
		t.Fatalf("expected synthesized bundle: %#v", resolution)
	}
	importTestEqual(t, synthesized.ID, "app")
	importTestEqual(t, synthesized.Bundle.PrimaryRepo, "app")
	if len(synthesized.Bundle.Repositories) != 1 {
		t.Fatal(synthesized.Bundle.Repositories)
	}
	importTestEqual(t, synthesized.Bundle.Repositories[0].ID, "app")
	if local := synthesized.Bundle.Repositories[0].Local; local == nil || *local != app {
		t.Fatalf("%v", local)
	}
}

func TestSynthesizedBundlesKeepOneRepositoryPerSharedOrigin(t *testing.T) {
	directory := t.TempDir()
	app := filepath.Join(directory, "app")
	worktree := filepath.Join(directory, "app-review")
	importTestInitializeRepository(t, app, "app")
	importTestInitializeRepository(t, worktree, "app")
	targets := SessionEditTargets{
		GitRoots:        []string{importTestCanonical(t, app), importTestCanonical(t, worktree)},
		ScratchGitRoots: []string{},
		NonGitDirs:      []string{},
	}

	config := core.DefaultConfig()
	resolution, err := ResolveBundle(&config, app, &targets, nil)
	if err != nil {
		t.Fatal(err)
	}
	synthesized, ok := resolution.(BundleResolutionSynthesized)
	if !ok {
		t.Fatalf("expected synthesized bundle: %#v", resolution)
	}
	bundle := synthesized.Bundle

	if len(bundle.Repositories) != 1 || bundle.PrimaryRepo != "app" {
		t.Fatalf("%#v", bundle)
	}
	if bundle.Repositories[0].Github != nil {
		t.Fatal(*bundle.Repositories[0].Github)
	}
	if local := bundle.Repositories[0].Local; local == nil || *local != importTestCanonical(t, app) {
		t.Fatalf("%v", local)
	}
	identity, err := ConfiguredRepositoryIdentity(&bundle.Repositories[0])
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, identity, core.RepositoryIdentity(core.RepositoryIdentityGithub{Owner: "example", Repository: "app"}))
}

func TestSynthesizedBundlesKeepSeparateLocalRepositories(t *testing.T) {
	directory := t.TempDir()
	app := filepath.Join(directory, "app")
	tools := filepath.Join(directory, "tools")
	for _, repository := range [][2]string{{app, "app"}, {tools, "tools"}} {
		importTestInitializeRepository(t, repository[0], repository[1])
		importTestGit(t, repository[0], "remote", "remove", "origin")
	}
	targets := SessionEditTargets{
		GitRoots:        []string{importTestCanonical(t, app), importTestCanonical(t, tools)},
		ScratchGitRoots: []string{},
		NonGitDirs:      []string{},
	}

	config := core.DefaultConfig()
	resolution, err := ResolveBundle(&config, app, &targets, nil)
	if err != nil {
		t.Fatal(err)
	}
	synthesized, ok := resolution.(BundleResolutionSynthesized)
	if !ok {
		t.Fatalf("expected synthesized bundle: %#v", resolution)
	}
	bundle := synthesized.Bundle

	if len(bundle.Repositories) != 2 || bundle.PrimaryRepo != "app" {
		t.Fatalf("%#v", bundle)
	}
	ids := []string{}
	for _, repository := range bundle.Repositories {
		ids = append(ids, repository.ID)
	}
	importTestEqual(t, ids, []string{"app", "tools"})
}

func TestEditTargetsFilterProfileStateAndReportNonGitDirectories(t *testing.T) {
	directory := t.TempDir()
	app := filepath.Join(directory, "app")
	profile := filepath.Join(directory, "profile")
	outside := filepath.Join(directory, "notes")
	importTestInitializeRepository(t, app, "app")
	importTestMkdir(t, profile)
	importTestMkdir(t, outside)
	transcript := ClaudeTranscript{
		Cwd:         app,
		EditedPaths: []string{filepath.Join(profile, "memory.md"), filepath.Join(outside, "draft.md")},
	}

	targets, err := SessionEditTargetsFor(&transcript, profile)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, targets.GitRoots, []string{importTestCanonical(t, app)})
	importTestEqual(t, targets.NonGitDirs, []string{outside})
}

func TestEditTargetsCompareProfilePathsAfterResolvingSymlinks(t *testing.T) {
	importTestSkipUnlessUnix(t)
	directory := t.TempDir()
	actual := filepath.Join(directory, "actual")
	alias := filepath.Join(directory, "alias")
	app := filepath.Join(actual, "app")
	profile := filepath.Join(actual, "profile")
	outside := filepath.Join(actual, "notes")
	importTestInitializeRepository(t, app, "app")
	importTestMkdir(t, profile)
	importTestMkdir(t, outside)
	importTestSymlink(t, actual, alias)
	transcript := ClaudeTranscript{
		Cwd:         filepath.Join(alias, "app"),
		EditedPaths: []string{filepath.Join(alias, "profile/memory.md"), filepath.Join(alias, "notes/draft.md")},
	}

	targets, err := SessionEditTargetsFor(&transcript, filepath.Join(alias, "profile"))
	if err != nil {
		t.Fatal(err)
	}

	importTestEqual(t, targets.GitRoots, []string{importTestCanonical(t, app)})
	importTestEqual(t, targets.NonGitDirs, []string{filepath.Join(alias, "notes")})
}

func TestImportSafetyReportsDirtyRootsAndNonGitOmissions(t *testing.T) {
	directory := t.TempDir()
	app := filepath.Join(directory, "app")
	importTestInitializeRepository(t, app, "app")
	importTestWrite(t, filepath.Join(app, "README.md"), "dirty")
	importTestWrite(t, filepath.Join(app, "untracked.txt"), "new")
	omitted := filepath.Join(directory, "notes")
	scratch := filepath.Join(directory, "scratch")
	issues, err := ImportSafetyIssuesFor(&SessionEditTargets{
		GitRoots:        []string{app},
		ScratchGitRoots: []string{scratch},
		NonGitDirs:      []string{omitted},
	})
	if err != nil {
		t.Fatal(err)
	}

	if len(issues.DirtyGitRoots) != 1 {
		t.Fatal(issues.DirtyGitRoots)
	}
	importTestEqual(t, issues.DirtyGitRoots[0][0], app)
	importTestEqual(t, issues.DirtyGitRoots[0][1], "1 tracked change · 1 untracked path")
	if !issues.HasUntrackedFiles {
		t.Fatal("untracked files not reported")
	}
	importTestEqual(t, issues.OmittedNonGitDirs, []string{omitted})
	importTestEqual(t, issues.ScratchGitRoots, []string{scratch})
}

func TestProjectsJsonlProjectsUserAndAssistantTextInSourceOrder(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "session.jsonl")
	importTestWrite(t, path, NativeFixture(
		`{"type":"user","cwd":"/work/app","message":{"content":"<mj-project-memory>private</mj-project-memory>\nfirst prompt"}}`+"\n"+
			`{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"thinking","thinking":"hidden"},{"type":"text","text":"first reply"}]}}`+"\n"+
			`{"type":"user","message":{"content":[{"type":"tool_result","content":"ignored"}]}}`+"\n"+
			`{"type":"user","isMeta":true,"message":{"content":"ignored meta"}}`+"\n"+
			`{"type":"user","message":{"content":"second prompt"}}`+"\n"+
			`{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"second "},{"type":"text","text":"reply"}]}}`+"\n",
	))

	importNeedsSessionwiki(t)
	transcript, err := ReadClaudeTranscript(path)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, transcript.Cwd, NativeCwd("/work/app"))
	importTestEqual(t, importTestDialogue(t, &transcript), []importTestLine{
		{"user", "first prompt"},
		{"agent", "first reply"},
		{"user", "second prompt"},
		{"agent", "second"},
		{"agent", "reply"},
	})
}

func TestLocalRepositoryCollectionPreservesBundleOrder(t *testing.T) {
	directory := t.TempDir()
	workspace := filepath.Join(directory, "workspace")
	app := filepath.Join(workspace, "app")
	importTestInitializeRepository(t, app, "app")
	importTestInitializeRepository(t, filepath.Join(workspace, "worker"), "worker")
	worker, appSource := "example/worker", "example/app"
	bundle := core.ProjectBundle{
		PrimaryRepo: "app",
		Repositories: []core.ProjectRepository{
			{ID: "worker", Github: &worker, Destination: "worker"},
			{ID: "app", Github: &appSource, Destination: "app"},
		},
	}

	snapshots, err := collectLocalRepositories(&bundle, []string{app, filepath.Join(workspace, "worker")}, true, nil)
	if err != nil {
		t.Fatal(err)
	}
	ids := []string{}
	for _, snapshot := range snapshots {
		ids = append(ids, snapshot.Metadata.ID)
	}
	importTestEqual(t, ids, []string{"worker", "app"})
}

func TestLocalSourceRepositoryImportCarriesNoCommittedBundle(t *testing.T) {
	directory := t.TempDir()
	app := filepath.Join(directory, "app")
	importTestInitializeRepository(t, app, "app")
	importTestGit(t, app, "remote", "remove", "origin")
	importTestWrite(t, filepath.Join(app, "dirty.txt"), "dirty")
	bundle := core.ProjectBundle{
		PrimaryRepo:  "app",
		Repositories: []core.ProjectRepository{{ID: "app", Local: &app, Destination: "app"}},
	}

	snapshots, err := collectLocalRepositories(&bundle, []string{app}, false, nil)
	if err != nil {
		t.Fatal(err)
	}

	if len(snapshots[0].CommittedBundle) != 0 {
		t.Fatal("committed bundle present")
	}
	importTestEqual(t, snapshots[0].Metadata.Origin, app)
	if snapshots[0].Metadata.RemoteWorkspace || len(snapshots[0].Metadata.PushURLs) != 0 {
		t.Fatalf("%#v", snapshots[0].Metadata)
	}
	if len(snapshots[0].UntrackedTar) == 0 {
		t.Fatal("untracked files missing")
	}
}

func TestImportWithoutRemoteTrackingRefsReportsHowToRecover(t *testing.T) {
	directory := t.TempDir()
	app := filepath.Join(directory, "app")
	importTestInitializeRepository(t, app, "app")
	importTestGit(t, app, "update-ref", "-d", "refs/remotes/origin/main")
	_, err := importDeltaBase(app, "https://github.com/example/app.git")

	importTestErrorContains(t, err, "has no remote-tracking refs to import against", "")
}

func TestIsolatedRepositoryImportRecordsNetworkProvenanceAndDelta(t *testing.T) {
	directory := t.TempDir()
	app := filepath.Join(directory, "app")
	importTestInitializeRepository(t, app, "app")
	importTestWrite(t, filepath.Join(app, "session.txt"), "session work")
	importTestGit(t, app, "add", "session.txt")
	importTestGit(t, app, "commit", "-qm", "session work")
	source := "example/app"
	bundle := core.ProjectBundle{
		PrimaryRepo:  "app",
		Repositories: []core.ProjectRepository{{ID: "app", Github: &source, Destination: "app"}},
	}

	snapshots, err := collectLocalRepositories(&bundle, []string{app}, true, nil)
	if err != nil {
		t.Fatal(err)
	}
	snapshot := snapshots[0]

	importTestEqual(t, snapshot.Metadata.Origin, "https://github.com/example/app.git")
	importTestEqual(t, snapshot.Metadata.PushURLs, []string{"https://github.com/example/app.git"})
	if !snapshot.Metadata.RemoteWorkspace || snapshot.Metadata.BaseCommit == "" || len(snapshot.CommittedBundle) == 0 {
		t.Fatalf("%#v", snapshot.Metadata)
	}
}

func TestIsolatedLocalSourceImportResolvesTheCheckoutNetworkRemote(t *testing.T) {
	directory := t.TempDir()
	app := filepath.Join(directory, "app")
	importTestInitializeRepository(t, app, "app")
	bundle := core.ProjectBundle{
		PrimaryRepo:  "app",
		Repositories: []core.ProjectRepository{{ID: "app", Local: &app, Destination: "app"}},
	}

	snapshots, err := collectLocalRepositories(&bundle, []string{app}, true, nil)
	if err != nil {
		t.Fatal(err)
	}
	snapshot := snapshots[0]

	importTestEqual(t, snapshot.Metadata.Origin, "https://github.com/example/app.git")
	if !snapshot.Metadata.RemoteWorkspace {
		t.Fatal("not a remote workspace")
	}
}

// Mjolnir sends Codex its hidden context as an embedded resource, which
// codex-acp turns into a text item wrapped in `<context ref=…>`. That item is
// Mjolnir's, not a prompt the user typed.
// Hard-won: b5ae15db: Codex titled a new session from hidden project memory instead of the user prompt.
func TestCodexJsonlLeavesOutHiddenContextSentAsAResource(t *testing.T) {
	uri := core.HiddenPromptContextURI
	context := fmt.Sprintf("%s\n<context ref=\"%s\">\n<mj-project-memory>\nprivate\n</mj-project-memory>\n</context>", uri, uri)
	record := importTestCodexItem(jsonvalue.Object{
		{Key: "type", Value: "UserMessage"},
		{Key: "id", Value: "user-1"},
		{Key: "content", Value: []any{
			jsonvalue.Object{{Key: "type", Value: "text"}, {Key: "text", Value: context}, {Key: "text_elements", Value: []any{}}},
			jsonvalue.Object{{Key: "type", Value: "text"}, {Key: "text", Value: "first prompt"}, {Key: "text_elements", Value: []any{}}},
		}},
	})
	directory := t.TempDir()
	path := filepath.Join(directory, "rollout.jsonl")
	importTestWrite(t, path, NativeFixture(fmt.Sprintf("%s\n%s\n",
		`{"type":"session_meta","payload":{"session_id":"019feb6c-5ffc-7c12-ad99-bdeaeb6be79d","cwd":"/work/app","history_mode":"paginated"}}`,
		importTestJSON(t, record))))

	importNeedsSessionwiki(t)
	transcript, err := ReadCodexTranscript(path)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, importTestDialogue(t, &transcript), []importTestLine{{"user", "first prompt"}})
}

// Hard-won: #1514: choosing a daemon root changed managed imports into attached checkouts.
// Hard-won: 970ce8a70: live native paths let late Codex output change an import after capture.
func TestNonemptyCodexImportMaterializesAndValidatesCanonicalArchive(t *testing.T) {
	directory := t.TempDir()
	app := filepath.Join(directory, "app")
	importTestInitializeRepository(t, app, "app")

	codexHome := filepath.Join(directory, "codex")
	nativeSessionID := "019feb6c-5ffc-7c12-ad99-bdeaeb6be79d"
	rollout := filepath.Join(codexHome, "sessions/2026/08/14", fmt.Sprintf("rollout-%s.jsonl", nativeSessionID))
	importTestMkdir(t, filepath.Dir(rollout))
	records := []jsonvalue.Object{
		{
			{Key: "timestamp", Value: "2026-08-14T12:00:00.000Z"},
			{Key: "type", Value: "session_meta"},
			{Key: "payload", Value: jsonvalue.Object{
				{Key: "id", Value: nativeSessionID}, {Key: "cwd", Value: app}, {Key: "history_mode", Value: "paginated"},
			}},
		},
		{
			{Key: "timestamp", Value: "2026-08-14T12:00:01.250Z"},
			{Key: "type", Value: "event_msg"},
			{Key: "payload", Value: jsonvalue.Object{
				{Key: "type", Value: "item_completed"},
				{Key: "item", Value: jsonvalue.Object{
					{Key: "type", Value: "UserMessage"},
					{Key: "content", Value: []any{jsonvalue.Object{{Key: "type", Value: "text"}, {Key: "text", Value: "import this"}}}},
				}},
			}},
		},
		{
			{Key: "timestamp", Value: "2026-08-14T12:00:02.500Z"},
			{Key: "type", Value: "event_msg"},
			{Key: "payload", Value: jsonvalue.Object{
				{Key: "type", Value: "item_completed"},
				{Key: "item", Value: jsonvalue.Object{
					{Key: "type", Value: "AgentMessage"},
					{Key: "content", Value: []any{jsonvalue.Object{{Key: "type", Value: "Text"}, {Key: "text", Value: strings.Repeat("imported ", 16*1024)}}}},
				}},
			}},
		},
		{
			{Key: "timestamp", Value: "2026-08-14T12:00:03.750Z"},
			{Key: "type", Value: "event_msg"},
			{Key: "payload", Value: jsonvalue.Object{{Key: "type", Value: "turn_complete"}, {Key: "turn_id", Value: "turn-1"}}},
		},
	}
	lines := []string{}
	for _, record := range records {
		lines = append(lines, importTestJSON(t, record))
	}
	lines = append(lines, `{"type":`)
	importTestWrite(t, rollout, strings.Join(lines, "\n"))

	importNeedsSessionwiki(t)
	transcript, err := ReadCodexTranscript(rollout)
	if err != nil {
		t.Fatal(err)
	}
	expectedUserTime := importTestMillis(t, "2026-08-14T12:00:01.250Z")
	expectedAgentTime := importTestMillis(t, "2026-08-14T12:00:02.500Z")
	expectedActivity := importTestMillis(t, "2026-08-14T12:00:03.750Z")
	source, err := LocateCodexSession(codexHome, ClaudeSessionSelectionNativeSessionID(nativeSessionID))
	if err != nil {
		t.Fatal(err)
	}
	github := "example/app"
	config := core.DefaultConfig()
	config.Bundles = map[string]core.ProjectBundle{"app": {
		PrimaryRepo:  "app",
		Repositories: []core.ProjectRepository{{ID: "app", Github: &github, Destination: "app"}},
	}}
	archiveDirectory := filepath.Join(directory, "archives")
	importTestMkdir(t, archiveDirectory)
	state := core.NewStateState()

	original, err := os.ReadFile(source.JSONLPath)
	if err != nil {
		t.Fatal(err)
	}
	if len(original) <= 64*1024 {
		t.Fatal(len(original))
	}
	var cancelled atomic.Bool
	control := ImportControl{
		Cancelled: &cancelled,
		Progress: func(progress ImportArchiveProgress) {
			if _, writing := progress.(ImportArchiveProgressWritingArchive); !writing {
				return
			}
			file, err := os.OpenFile(source.JSONLPath, os.O_APPEND|os.O_WRONLY, 0)
			if err != nil {
				panic(err)
			}
			defer file.Close()
			later := jsonvalue.Object{
				{Key: "timestamp", Value: "2026-08-14T12:00:04.000Z"}, {Key: "type", Value: "event_msg"},
				{Key: "payload", Value: jsonvalue.Object{{Key: "type", Value: "item_completed"}, {Key: "item", Value: jsonvalue.Object{
					{Key: "type", Value: "AgentMessage"},
					{Key: "content", Value: []any{jsonvalue.Object{{Key: "type", Value: "Text"}, {Key: "text", Value: "later harness output"}}}},
				}}}},
			}
			encoded, err := jsonvalue.Marshal(later)
			if err != nil {
				panic(err)
			}
			if _, err := file.Write(append([]byte("\n"), encoded...)); err != nil {
				panic(err)
			}
		},
		IncludeUntracked: true,
	}
	sourceCwd := source.Cwd
	imported, err := ImportNativeSession(&config, &state, NativeImportRequest{
		DaemonProjectRoot: &sourceCwd,
		Harness:           core.HarnessKindCodex,
		HarnessHome:       codexHome,
		NativeSessionID:   source.NativeSessionID,
		SourcePath:        source.JSONLPath,
		Transcript:        &transcript,
		BundleID:          "app",
		ArchiveDirectory:  archiveDirectory,
	}, &control)
	if err != nil {
		t.Fatal(err)
	}
	verified, err := checkpoint.VerifyArchiveStreaming(imported.ArchivePath)
	if err != nil {
		t.Fatal(err)
	}
	// A daemon-root selection relocates sources without changing workspace
	// ownership.
	record, ok := state.Sessions.Get(imported.SessionID)
	if !ok {
		t.Fatal("imported session missing from state")
	}
	if record.ProjectDirectory != nil {
		t.Fatal(*record.ProjectDirectory)
	}
	if _, managed := record.RecordCheckout().(core.CheckoutManagedWorkspace); !managed {
		t.Fatalf("%#v", record.RecordCheckout())
	}
	importTestEqual(t, verified.CanonicalSession.EventFrontier, uint64(2))
	importTestEqual(t, record.Checkpoint.EventFrontier, verified.CanonicalSession.EventFrontier)
	if activity := verified.CanonicalSession.Session.LastActivityAtMs; activity == nil || *activity != expectedActivity {
		t.Fatalf("%v", activity)
	}
	if len(verified.CanonicalSession.Transcript) != 2 {
		t.Fatal(len(verified.CanonicalSession.Transcript))
	}
	importTestEqual(t, verified.CanonicalSession.Transcript[0].StableID, "user-import-1")
	importTestEqual(t, verified.CanonicalSession.Transcript[0].CreatedAtMs, expectedUserTime)
	importTestEqual(t, verified.CanonicalSession.Transcript[1].CreatedAtMs, expectedAgentTime)
	native := importTestNativePayload(t, &verified.Manifest, func(*core.PayloadDescriptor) bool { return true })
	payload, err := checkpoint.ReadVerifiedCheckpointPayload(imported.ArchivePath, native, native.Size)
	if err != nil {
		t.Fatal(err)
	}
	if string(payload) != string(original) {
		t.Fatal("archived native bytes differ from the captured rollout")
	}
	info, err := os.Stat(source.JSONLPath)
	if err != nil {
		t.Fatal(err)
	}
	if info.Size() <= int64(len(original)) {
		t.Fatal("the late write did not reach the source")
	}
}

// importTestNativePayload is the first native-artifact payload matching.
func importTestNativePayload(t *testing.T, manifest *core.ArchiveManifest, matches func(*core.PayloadDescriptor) bool) *core.PayloadDescriptor {
	t.Helper()
	for index := range manifest.Payloads {
		payload := &manifest.Payloads[index]
		if payload.Role.Kind == core.PayloadRoleNativeArtifact && matches(payload) {
			return payload
		}
	}
	t.Fatal("no native artifact payload")
	return nil
}

func importTestMillis(t *testing.T, text string) int64 {
	t.Helper()
	parsed, err := time.Parse(time.RFC3339Nano, text)
	if err != nil {
		t.Fatal(err)
	}
	return parsed.UnixMilli()
}

const importFixtureSession = "019feb6c-5ffc-7c12-ad99-bdeaeb6be79d"

func importTestGithubBundle(ids ...string) core.ProjectBundle {
	repositories := []core.ProjectRepository{}
	for _, id := range ids {
		github := "example/" + id
		repositories = append(repositories, core.ProjectRepository{ID: id, Github: &github, Destination: id})
	}
	return core.ProjectBundle{PrimaryRepo: ids[0], Repositories: repositories}
}

func importTestTargets(localBare bool) map[string]core.TargetTemplate {
	targets := map[string]core.TargetTemplate{
		"podman": core.TargetTemplateLocalPodman{ContainerTemplate: importTestContainerTemplate()},
	}
	if localBare {
		targets["localhost"] = core.TargetTemplateLocalBare{}
	}
	return targets
}

// importTestDurableFixtureDirectory is a directory outside every temporary
// directory, because import treats repositories under those as scratch. Like
// Rust's `target/import-fixtures`, it sits under the module's own build
// output.
func importTestDurableFixtureDirectory(t *testing.T) string {
	t.Helper()
	base, err := filepath.Abs(filepath.Join("..", "..", "bin", "import-fixtures"))
	if err != nil {
		t.Fatal(err)
	}
	importTestMkdir(t, base)
	directory, err := os.MkdirTemp(base, "durable")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(directory) })
	return directory
}

func TestStopSequenceClaudeImportProducesIdleRawProjectSession(t *testing.T) {
	directory := t.TempDir()
	app := filepath.Join(directory, "app")
	importTestInitializeRepository(t, app, "app")
	claudeHome := filepath.Join(directory, "claude")
	transcriptPath := filepath.Join(claudeHome, "projects/-work-app", importFixtureSession+".jsonl")
	importTestMkdir(t, filepath.Dir(transcriptPath))
	importTestWrite(t, transcriptPath, importTestJSON(t, jsonvalue.Object{
		{Key: "timestamp", Value: "2026-08-14T12:00:00.000Z"},
		{Key: "type", Value: "user"},
		{Key: "cwd", Value: app},
		{Key: "message", Value: jsonvalue.Object{{Key: "content", Value: "import this"}}},
	})+"\n"+importTestJSON(t, jsonvalue.Object{
		{Key: "timestamp", Value: "2026-08-14T12:00:01.000Z"},
		{Key: "type", Value: "assistant"},
		{Key: "message", Value: jsonvalue.Object{
			{Key: "content", Value: []any{jsonvalue.Object{{Key: "type", Value: "text"}, {Key: "text", Value: "imported"}}}},
			{Key: "stop_reason", Value: "stop_sequence"},
		}},
	}))
	importNeedsSessionwiki(t)
	transcript, err := ReadClaudeTranscript(transcriptPath)
	if err != nil {
		t.Fatal(err)
	}
	metadata, err := os.Stat(transcriptPath)
	if err != nil {
		t.Fatal(err)
	}
	source := LocatedClaudeSession{
		NativeSessionID: importFixtureSession,
		JSONLPath:       transcriptPath,
		ModifiedAt:      metadata.ModTime(),
		Title:           "Imported Claude session",
		Cwd:             app,
		GitBranch:       "main",
		SizeBytes:       uint64(metadata.Size()),
	}
	config := core.DefaultConfig()
	config.Bundles = map[string]core.ProjectBundle{"app": importTestGithubBundle("app")}
	config.Targets = importTestTargets(true)
	archiveDirectory := filepath.Join(directory, "archives")
	importTestMkdir(t, archiveDirectory)
	state := core.NewStateState()

	imported, err := ImportNativeSession(&config, &state, NativeImportRequest{
		Harness:          core.HarnessKindClaude,
		HarnessHome:      claudeHome,
		NativeSessionID:  source.NativeSessionID,
		SourcePath:       source.JSONLPath,
		Transcript:       &transcript,
		BundleID:         "app",
		ArchiveDirectory: archiveDirectory,
	}, nil)
	if err != nil {
		t.Fatal(err)
	}

	record, _ := state.Sessions.Get(imported.SessionID)
	verified, err := checkpoint.VerifyArchiveStreaming(imported.ArchivePath)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, verified.CanonicalSession.Session.Execution, core.CanonicalExecutionState{State: core.CanonicalExecutionStateIdle})
	if record.ProjectDirectory == nil || *record.ProjectDirectory != importTestCanonical(t, app) {
		t.Fatalf("%v", record.ProjectDirectory)
	}
	importTestEqual(t, record.TargetTemplateID, "localhost")
}

func TestCodexPaginatedImportIgnoresCompactionArtifacts(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "rollout.jsonl")
	importTestWrite(t, path, NativeFixture(
		`{"type":"session_meta","payload":{"cwd":"/work/app","history_mode":"paginated"}}`+"\n"+
			`{"type":"response_item","payload":{"type":"compaction","encrypted_content":"opaque"}}`+"\n"+
			`{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","id":"user-1","content":[{"type":"text","text":"prompt after compaction"}]}}}`+"\n",
	))
	importNeedsSessionwiki(t)
	transcript, err := ReadCodexTranscript(path)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, importTestDialogue(t, &transcript), []importTestLine{
		{"system", importTestCompactedNotice},
		{"user", "prompt after compaction"},
	})
}

func TestCodexImportRejectsLegacyHistoryWithMigrationGuidance(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "rollout.jsonl")
	importTestWrite(t, path,
		`{"type":"session_meta","payload":{"cwd":"/work/app"}}`+"\n"+
			`{"type":"event_msg","payload":{"type":"user_message","message":"raw prompt"}}`+"\n")
	_, err := ReadCodexTranscript(path)
	if err == nil {
		t.Fatal("expected an error")
	}
	message := err.Error()
	if !strings.Contains(message, "Legacy Codex history cannot be imported") ||
		!strings.Contains(message, "codex migrate-rollouts --apply") {
		t.Fatal(message)
	}
	// R2-9: the CLI has no dialog, so the advice fits both surfaces.
	if !strings.Contains(message, "then try the import again") || strings.Contains(message, "dialog") {
		t.Fatal(message)
	}
}

func TestClaudeImportShowsNoticeForCompactionWithoutRawHistory(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "session.jsonl")
	importTestWrite(t, path, NativeFixture(
		`{"type":"system","subtype":"compact_boundary","cwd":"/work/app"}`+"\n"+
			`{"type":"user","isCompactSummary":true,"message":{"content":"summary"}}`+"\n",
	))
	importNeedsSessionwiki(t)
	transcript, err := ReadClaudeTranscript(path)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, importTestDialogue(t, &transcript), []importTestLine{{"system", importTestCompactedNotice}})
}

func TestCodexLocatorUsesRolloutIDWhenSessionIDNamesAParent(t *testing.T) {
	directory := t.TempDir()
	rollout := filepath.Join(directory, "sessions/2026/08/10/rollout-2026-08-10T00-00-00-019feb6c-6b55-7111-a210-6d85ee0772cd.jsonl")
	importTestMkdir(t, filepath.Dir(rollout))
	importTestWrite(t, rollout,
		`{"type":"session_meta","payload":{"session_id":"019feb6c-5ffc-7c12-ad99-bdeaeb6be79d","id":"019feb6c-6b55-7111-a210-6d85ee0772cd","cwd":"/work/app"}}`)

	located, err := LocateCodexSession(directory, ClaudeSessionSelectionNativeSessionID("019feb6c-6b55-7111-a210-6d85ee0772cd"))
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, located.NativeSessionID, "019feb6c-6b55-7111-a210-6d85ee0772cd")
	importTestEqual(t, located.JSONLPath, rollout)
}

func TestCodexListingUsesNativeTitleAndSessionMetadata(t *testing.T) {
	directory := t.TempDir()
	sessionID := "019feb6c-6b55-7111-a210-6d85ee0772cd"
	rollout := filepath.Join(directory, "sessions/rollout.jsonl")
	importTestMkdir(t, filepath.Dir(rollout))
	importTestWrite(t, rollout, fmt.Sprintf(
		`{"type":"session_meta","payload":{"id":"%s","cwd":"/work/app","git":{"branch":"feature"}}}`, sessionID))
	importTestWrite(t, filepath.Join(directory, "session_index.jsonl"),
		fmt.Sprintf(`{"id":"%s","thread_name":"native title\ncontinued"}`, sessionID))

	sessions, err := ListCodexSessions(directory)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, sessions[0].Title, "native title continued")
	importTestEqual(t, sessions[0].Cwd, "/work/app")
	importTestEqual(t, sessions[0].GitBranch, "feature")
	if sessions[0].SizeBytes == 0 {
		t.Fatal("empty size")
	}
	importTestEqual(t, sessions[0].HistoryMode, CodexHistoryModeLegacy)
}

func TestCodexHistoryTextDoesNotBecomeTheSessionName(t *testing.T) {
	directory := t.TempDir()
	importTestWrite(t, filepath.Join(directory, "history.jsonl"), `{"session_id":"session-1","text":"first user message"}`)

	titles, err := codexNativeTitles(directory)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, titles["session-1"], "session-1")
}

func TestCodexListingMatchesNativeInteractiveVisibilityAndTitlePriority(t *testing.T) {
	directory := t.TempDir()
	sessions := filepath.Join(directory, "sessions")
	archived := filepath.Join(directory, "archived_sessions")
	importTestMkdir(t, sessions)
	importTestMkdir(t, archived)
	interactiveID := "019feb6c-6b55-7111-a210-6d85ee0772cd"
	for _, fixture := range []struct {
		path   string
		id     string
		source any
	}{
		{filepath.Join(sessions, "interactive.jsonl"), interactiveID, "cli"},
		{filepath.Join(sessions, "exec.jsonl"), "019feb6c-6b55-7111-a210-6d85ee0772ce", "exec"},
		{filepath.Join(sessions, "subagent.jsonl"), "019feb6c-6b55-7111-a210-6d85ee0772cf", jsonvalue.Object{
			{Key: "subagent", Value: jsonvalue.Object{{Key: "thread_spawn", Value: jsonvalue.Object{{Key: "parent_thread_id", Value: interactiveID}}}}},
		}},
		{filepath.Join(sessions, "ephemeral.jsonl"), "019feb6c-6b55-7111-a210-6d85ee0772d1", "cli"},
		{filepath.Join(archived, "archived.jsonl"), "019feb6c-6b55-7111-a210-6d85ee0772d0", "cli"},
	} {
		ephemeral := filepath.Base(fixture.path) == "ephemeral.jsonl"
		importTestWrite(t, fixture.path, importTestJSON(t, jsonvalue.Object{
			{Key: "type", Value: "session_meta"},
			{Key: "payload", Value: jsonvalue.Object{
				{Key: "id", Value: fixture.id},
				{Key: "source", Value: fixture.source},
				{Key: "cwd", Value: "/work/app"},
				{Key: "ephemeral", Value: ephemeral},
			}},
		}))
	}
	importTestWrite(t, filepath.Join(directory, "history.jsonl"), importTestJSON(t, jsonvalue.Object{
		{Key: "session_id", Value: interactiveID}, {Key: "text", Value: "Generated history title"},
	}))
	importTestWrite(t, filepath.Join(directory, "session_index.jsonl"), importTestJSON(t, jsonvalue.Object{
		{Key: "id", Value: interactiveID}, {Key: "thread_name", Value: "Explicit native title"},
	}))

	listed, err := ListCodexSessions(directory)
	if err != nil {
		t.Fatal(err)
	}
	if len(listed) != 1 {
		t.Fatalf("%#v", listed)
	}
	importTestEqual(t, listed[0].NativeSessionID, interactiveID)
	importTestEqual(t, listed[0].Title, "Explicit native title")
}

const importTestThreadsSchema = `CREATE TABLE threads (
	id TEXT PRIMARY KEY,
	rollout_path TEXT,
	updated_at INTEGER,
	name TEXT,
	title TEXT,
	cwd TEXT,
	git_branch TEXT,
	archived INTEGER,
	source TEXT,
	preview TEXT,
	history_mode TEXT
);`

func importTestOpenDatabase(t *testing.T, path string) *sql.DB {
	t.Helper()
	connection, err := sql.Open("sqlite", path)
	if err != nil {
		t.Fatal(err)
	}
	connection.SetMaxOpenConns(1)
	if _, err := connection.Exec(importTestThreadsSchema); err != nil {
		t.Fatal(err)
	}
	return connection
}

func TestCodexListingUsesNativeIndexOrderAndIncludesAllPersistedThreads(t *testing.T) {
	directory := t.TempDir()
	sessions := filepath.Join(directory, "sessions")
	importTestMkdir(t, sessions)
	connection := importTestOpenDatabase(t, filepath.Join(directory, "state_5.sqlite"))
	for index := uint64(0); index < 30; index++ {
		id := fmt.Sprintf("019feb6c-6b55-7111-a210-%012x", index)
		rollout := filepath.Join(sessions, fmt.Sprintf("rollout-%s.jsonl", id))
		importTestWrite(t, rollout, "indexed")
		var name any
		if index == 29 {
			name = "Explicit newest title"
		}
		if _, err := connection.Exec(
			"INSERT INTO threads VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 'cli', 'visible', 'paginated')",
			id, rollout, int64(index), name, fmt.Sprintf("Generated %d", index), "/work/app", "feature",
		); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := connection.Exec(
		"INSERT INTO threads VALUES (?1, NULL, 100, 'Ephemeral', 'Ephemeral', '/work/app', 'HEAD', 0, 'cli', 'visible', 'legacy')",
		"019feb6c-6b55-7111-a210-ffffffffffff",
	); err != nil {
		t.Fatal(err)
	}
	if err := connection.Close(); err != nil {
		t.Fatal(err)
	}

	listed, err := ListCodexSessions(directory)
	if err != nil {
		t.Fatal(err)
	}
	if len(listed) != 30 {
		t.Fatal(len(listed))
	}
	importTestEqual(t, listed[0].Title, "Explicit newest title")
	importTestEqual(t, listed[0].HistoryMode, CodexHistoryModePaginated)
	if !listed[0].ModifiedAt.After(listed[1].ModifiedAt) {
		t.Fatal("newest first")
	}
	importTestEqual(t, listed[29].Title, "Generated 0")
	for _, session := range listed {
		if session.Title == "Ephemeral" {
			t.Fatal("ephemeral thread listed")
		}
	}
}

// Hel mirrors Codex's own archive flag one way: the thread is listed and
// flagged so the resume dialog can hide it, and `--latest` still follows
// Codex's default view. Nothing is written back to Codex.
func TestCodexListingFlagsNativelyArchivedThreadsAndLatestSkipsThem(t *testing.T) {
	directory := t.TempDir()
	sessions := filepath.Join(directory, "sessions")
	importTestMkdir(t, sessions)
	database := filepath.Join(directory, "state_5.sqlite")
	connection := importTestOpenDatabase(t, database)
	liveID := "019feb6c-6b55-7111-a210-000000000001"
	archivedID := "019feb6c-6b55-7111-a210-000000000002"
	for _, thread := range []struct {
		id        string
		updatedAt int64
		archived  int64
	}{{liveID, 10, 0}, {archivedID, 20, 1}} {
		rollout := filepath.Join(sessions, fmt.Sprintf("rollout-%s.jsonl", thread.id))
		importTestWrite(t, rollout, "indexed")
		if _, err := connection.Exec(
			"INSERT INTO threads VALUES (?1, ?2, ?3, ?4, NULL, '/work/app', 'main', ?5, 'cli', 'visible', 'paginated')",
			thread.id, rollout, thread.updatedAt, "Thread "+thread.id, thread.archived,
		); err != nil {
			t.Fatal(err)
		}
	}
	if err := connection.Close(); err != nil {
		t.Fatal(err)
	}
	before, err := os.Stat(database)
	if err != nil {
		t.Fatal(err)
	}

	listed, err := ListCodexSessions(directory)
	if err != nil {
		t.Fatal(err)
	}
	if len(listed) != 2 {
		t.Fatal(len(listed))
	}
	importTestEqual(t, listed[0].NativeSessionID, archivedID)
	if !listed[0].NativelyArchived {
		t.Fatal("archived thread not flagged")
	}
	importTestEqual(t, listed[1].NativeSessionID, liveID)
	if listed[1].NativelyArchived {
		t.Fatal("live thread flagged")
	}

	// `--latest` follows Codex's default view and skips what it archived.
	latest, err := LocateCodexSession(directory, ClaudeSessionSelectionLatest{})
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, latest.NativeSessionID, liveID)
	// Naming an archived thread still finds it, so it stays importable.
	byID, err := LocateCodexSession(directory, ClaudeSessionSelectionNativeSessionID(archivedID))
	if err != nil {
		t.Fatal(err)
	}
	if !byID.NativelyArchived {
		t.Fatal("archived thread not flagged")
	}

	after, err := os.Stat(database)
	if err != nil {
		t.Fatal(err)
	}
	if !after.ModTime().Equal(before.ModTime()) {
		t.Fatal("Hel must never write Codex's own database")
	}
}

// Resume scans profile homes, never the sibling worker-private reviewer
// homes.
func TestReviewerNamedProfilesRemainVisibleWithoutImportingSidecarSessions(t *testing.T) {
	directory := t.TempDir()
	profile := filepath.Join(directory, "profiles/reviewer")
	sidecar := filepath.Join(directory, "workers/primary/reviewer/profile")
	ordinaryID := "019feb6c-6b55-7111-a210-6d85ee0772cd"
	reviewerID := "019feb6c-6b55-7111-a210-6d85ee0772ce"
	importTestCodexNamedRollout(t, profile, ordinaryID, "cli")
	importTestCodexNamedRollout(t, sidecar, reviewerID, "cli")
	for _, home := range [][2]string{{profile, ordinaryID}, {sidecar, reviewerID}} {
		importTestWrite(t, filepath.Join(home[0], "session_index.jsonl"), importTestJSON(t, jsonvalue.Object{
			{Key: "id", Value: home[1]}, {Key: "thread_name", Value: "A completed conversation"},
		}))
	}
	listed := []string{}
	if err := ScanNativeSessions(core.HarnessKindCodex, profile, NewNativeScanCache(), func(progress SessionScanProgress[NativeSessionListing]) {
		if progress.Session != nil {
			listed = append(listed, progress.Session.NativeSessionID)
		}
	}); err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, listed, []string{ordinaryID})
	sidecarSessions, err := ListCodexSessions(sidecar)
	if err != nil {
		t.Fatal(err)
	}
	if sidecarSessions[0].NativeSessionID != reviewerID {
		t.Fatal("the reviewer transcript is valid but outside the configured scan root")
	}
}

func TestClaudeListingUsesAITitleAndNativeMetadata(t *testing.T) {
	directory := t.TempDir()
	rollout := filepath.Join(directory, "projects/work/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl")
	importTestMkdir(t, filepath.Dir(rollout))
	importTestWrite(t, rollout,
		`{"type":"user","cwd":"/work/app","gitBranch":"feature","message":{"content":"fallback"}}`+"\n"+
			`{"type":"ai-title","aiTitle":"Native Claude title"}`)

	sessions, err := ListClaudeSessions(directory)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, sessions[0].Title, "Native Claude title")
	importTestEqual(t, sessions[0].Cwd, "/work/app")
	importTestEqual(t, sessions[0].GitBranch, "feature")
	if sessions[0].SizeBytes == 0 {
		t.Fatal("empty size")
	}
}

func TestClaudeListingMatchesNativeTitlePriorityAndVisibility(t *testing.T) {
	directory := t.TempDir()
	project := filepath.Join(directory, "projects/work")
	importTestMkdir(t, project)
	importTestWrite(t, filepath.Join(project, "interactive.jsonl"),
		`{"type":"user","entrypoint":"cli","cwd":"/work/app","message":{"content":"fallback"}}`+"\n"+
			`{"type":"agent-name","agentName":"renamed-agent"}`+"\n"+
			`{"type":"custom-title","customTitle":"Native custom title"}`+"\n"+
			`{"type":"ai-title","aiTitle":"Generated title"}`)
	importTestWrite(t, filepath.Join(project, "print-mode.jsonl"),
		`{"type":"user","entrypoint":"sdk-cli","cwd":"/work/app","message":{"content":"<local-command-caveat>usage poll</local-command-caveat>"}}`)
	importTestWrite(t, filepath.Join(project, "sidechain.jsonl"),
		`{"type":"user","entrypoint":"cli","isSidechain":true,"cwd":"/work/app","message":{"content":"subagent"}}`)

	sessions, err := ListClaudeSessions(directory)
	if err != nil {
		t.Fatal(err)
	}
	if len(sessions) != 1 {
		t.Fatalf("%#v", sessions)
	}
	importTestEqual(t, sessions[0].Title, "Native custom title")
	importTestEqual(t, sessions[0].NativeSessionID, "interactive")
}

func TestClaudeListingPrefersAgentNameToGeneratedTitle(t *testing.T) {
	directory := t.TempDir()
	rollout := filepath.Join(directory, "session.jsonl")
	importTestWrite(t, rollout,
		`{"type":"user","entrypoint":"cli","cwd":"/work/app","message":{"content":"fallback"}}`+"\n"+
			`{"type":"agent-name","agentName":"restic-cleanup"}`+"\n"+
			`{"type":"ai-title","aiTitle":"Clean up fulldata directory organization"}`)

	listing, err := claudeNativeMetadata(rollout)
	if err != nil || listing == nil {
		t.Fatal(listing, err)
	}
	importTestEqual(t, listing.title, "restic-cleanup")
}

func TestClaudeFirstUserMessageIsNotUsedAsASessionNameFallback(t *testing.T) {
	directory := t.TempDir()
	rollout := filepath.Join(directory, "session.jsonl")
	importTestWrite(t, rollout,
		`{"type":"user","entrypoint":"cli","cwd":"/work/app","message":{"content":"first user message"}}`)

	listing, err := claudeNativeMetadata(rollout)
	if err != nil || listing == nil {
		t.Fatal(listing, err)
	}
	importTestEqual(t, listing.title, "Untitled session")
}

// Hard-won: a54b9f41: a named Claude transcript at its exact path was wrongly reported as not found because picker filters were reused.
func TestClaudeLookupByIDFindsASessionThePickerHides(t *testing.T) {
	directory := t.TempDir()
	project := filepath.Join(directory, "projects/work")
	importTestMkdir(t, project)
	rollout := filepath.Join(project, "printed.jsonl")
	importTestWrite(t, rollout,
		`{"type":"user","entrypoint":"sdk","cwd":"/work/app","message":{"content":"printed run"}}`)

	listed, err := ListClaudeSessions(directory)
	if err != nil || len(listed) != 0 {
		t.Fatal(listed, err)
	}
	located, err := LocateClaudeSession(directory, ClaudeSessionSelectionNativeSessionID("printed"))
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, located.NativeSessionID, "printed")
	importTestEqual(t, located.JSONLPath, rollout)
}

func TestClaudeLookupByIDReportsASymlinkedTranscript(t *testing.T) {
	importTestSkipUnlessUnix(t)
	directory := t.TempDir()
	project := filepath.Join(directory, "projects/work")
	importTestMkdir(t, project)
	stored := filepath.Join(directory, "stored.jsonl")
	importTestWrite(t, stored,
		`{"type":"user","entrypoint":"cli","cwd":"/work/app","message":{"content":"stored"}}`)
	importTestSymlink(t, stored, filepath.Join(project, "linked.jsonl"))

	_, err := LocateClaudeSession(directory, ClaudeSessionSelectionNativeSessionID("linked"))
	importTestErrorContains(t, err, "is a symlink", "was not found")
}

// Hard-won: 1495b0bd: a by-id Claude lookup defaulted a missing cwd to an empty path instead of refusing it.
func TestClaudeLookupByIDReportsATranscriptWithoutACwd(t *testing.T) {
	directory := t.TempDir()
	project := filepath.Join(directory, "projects/work")
	importTestMkdir(t, project)
	// Filtered out of the listing, so the lookup takes the by-id path.
	importTestWrite(t, filepath.Join(project, "homeless.jsonl"),
		`{"type":"user","entrypoint":"sdk","message":{"content":"no cwd anywhere"}}`)

	_, err := LocateClaudeSession(directory, ClaudeSessionSelectionNativeSessionID("homeless"))
	importTestErrorContains(t, err, "has no cwd", "was not found")
}

func TestClaudeListingUsesNativeAllProjectsLimit(t *testing.T) {
	directory := t.TempDir()
	project := filepath.Join(directory, "projects/work")
	importTestMkdir(t, project)
	for index := range 51 {
		importTestWrite(t, filepath.Join(project, fmt.Sprintf("session-%02d.jsonl", index)), fmt.Sprintf(
			`{"type":"user","entrypoint":"cli","cwd":"/work/app","message":{"content":"Session %d"}}`, index))
	}

	listed, err := ListClaudeSessions(directory)
	if err != nil {
		t.Fatal(err)
	}
	if len(listed) != 50 {
		t.Fatal(len(listed))
	}
	oldest, err := LocateClaudeSession(directory, ClaudeSessionSelectionNativeSessionID("session-00"))
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, oldest.NativeSessionID, "session-00")
}

func TestKimiWireProjectsUserPromptAndTextWithoutThought(t *testing.T) {
	directory := t.TempDir()
	importTestMkdir(t, filepath.Join(directory, "agents/main"))
	importTestWrite(t, filepath.Join(directory, "state.json"), NativeFixture(`{"workDir":"/work/app"}`))
	wirePath := filepath.Join(directory, "agents/main/wire.jsonl")
	importTestWrite(t, wirePath,
		`{"type":"turn.prompt","origin":{"kind":"user"},"input":[{"type":"text","text":"<mj-project-memory>private</mj-project-memory>"},{"type":"text","text":"first prompt"}]}`+"\n"+
			`{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"think","think":"hidden"}}}`+"\n"+
			`{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"first reply"}}}`+"\n"+
			`{"type":"turn.steer","origin":{"kind":"user"},"input":[{"type":"text","text":"follow up"}]}`+"\n"+
			`{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"second reply"}}}`+"\n")
	info, err := os.Stat(wirePath)
	if err != nil {
		t.Fatal(err)
	}
	expectedFallback := info.ModTime().UnixMilli()

	importNeedsSessionwiki(t)
	transcript, err := ReadKimiTranscript(directory)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, transcript.Cwd, NativeCwd("/work/app"))
	importTestEqual(t, importTestDialogue(t, &transcript), []importTestLine{
		{"user", "first prompt"},
		{"agent", "first reply"},
		{"user", "follow up"},
		{"agent", "second reply"},
	})
	for _, item := range transcript.items {
		if item.CreatedAtMs != expectedFallback {
			t.Fatal(item.CreatedAtMs, expectedFallback)
		}
	}
}

func TestGrokChatHistoryProjectsPromptsThoughtsAndReplies(t *testing.T) {
	directory := t.TempDir()
	history := `{"type":"system","content":"You are Grok."}` + "\n" +
		`{"type":"user","content":[{"type":"text","text":"<system-reminder>ignore me</system-reminder>"}],"synthetic_reason":"system_reminder"}` + "\n" +
		`{"type":"user","content":[{"type":"text","text":"<mj-project-memory>private</mj-project-memory>"},{"type":"text","text":"first prompt"}],"prompt_index":0}` + "\n" +
		`{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"thinking it over"}],"encrypted_content":"opaque"}` + "\n" +
		`{"type":"assistant","content":"first reply","model_id":"grok-4.6"}` + "\n" +
		`{"type":"user","content":[{"type":"text","text":"follow up"}],"prompt_index":1}` + "\n" +
		`{"type":"assistant","content":"second reply","model_id":"grok-4.6"}` + "\n"
	session := GrokSession(directory, NativeCwd("/work/app"), history)
	info, err := os.Stat(filepath.Join(session, "chat_history.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	expectedFallback := info.ModTime().UnixMilli()

	importNeedsSessionwiki(t)
	transcript, err := ReadGrokTranscript(session)
	if err != nil {
		t.Fatal(err)
	}

	importTestEqual(t, transcript.Cwd, NativeCwd("/work/app"))
	importTestEqual(t, importTestDialogue(t, &transcript), []importTestLine{
		{"user", "first prompt"},
		{"thought", "thinking it over"},
		{"agent", "first reply"},
		{"user", "follow up"},
		{"agent", "second reply"},
	})
	for _, item := range transcript.items {
		if item.CreatedAtMs != expectedFallback {
			t.Fatal(item.CreatedAtMs, expectedFallback)
		}
	}
}

func TestGrokReasoningAndMessageChunksKeepTheirOwnUpdateKinds(t *testing.T) {
	directory := t.TempDir()
	history := `{"type":"user","content":[{"type":"text","text":"go"}]}` + "\n" +
		`{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"a"},{"type":"summary_text","text":"b"}]}` + "\n" +
		`{"type":"assistant","content":"done"}` + "\n"
	session := GrokSession(directory, NativeCwd("/work/app"), history)

	importNeedsSessionwiki(t)
	transcript, err := ReadGrokTranscript(session)
	if err != nil {
		t.Fatal(err)
	}

	importTestEqual(t, importTestDialogue(t, &transcript), []importTestLine{{"user", "go"}, {"thought", "a\nb"}, {"agent", "done"}})
}

func TestGrokCompactionBeforeRecoverableHistoryShowsNotice(t *testing.T) {
	directory := t.TempDir()
	history := `{"type":"user","content":[{"type":"text","text":"summary of earlier work"}],"synthetic_reason":"compaction_meta"}` + "\n" +
		`{"type":"user","content":[{"type":"text","text":"carry on"}]}` + "\n"
	session := GrokSession(directory, NativeCwd("/work/app"), history)

	importNeedsSessionwiki(t)
	transcript, err := ReadGrokTranscript(session)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, importTestDialogue(t, &transcript), []importTestLine{{"system", importTestCompactedNotice}, {"user", "carry on"}})
}

func TestGrokEditedPathsNeedACompletedSearchReplaceCall(t *testing.T) {
	directory := t.TempDir()
	history := `{"type":"user","content":[{"type":"text","text":"edit things"}]}` + "\n" +
		`{"type":"assistant","content":"","tool_calls":[{"id":"call-1","name":"search_replace","arguments":"{\"file_path\":\"/work/app/src/lib.rs\",\"old_string\":\"a\",\"new_string\":\"b\"}"},{"id":"call-2","name":"search_replace","arguments":"{\"file_path\":\"/work/app/never-ran.rs\"}"},{"id":"call-3","name":"read_file","arguments":"{\"file_path\":\"/work/app/README.md\"}"}]}` + "\n" +
		`{"type":"tool_result","tool_call_id":"call-1","content":"ok"}` + "\n" +
		`{"type":"tool_result","tool_call_id":"call-3","content":"ok"}` + "\n"
	session := GrokSession(directory, NativeCwd("/work/app"), NativeFixture(history))

	importNeedsSessionwiki(t)
	transcript, err := ReadGrokTranscript(session)
	if err != nil {
		t.Fatal(err)
	}

	// Only the completed write is reported: an unanswered call never ran,
	// and a read is not an edit.
	importTestEqual(t, transcript.EditedPaths, []string{NativeCwd("/work/app/src/lib.rs")})
}

func TestGrokScanListsSessionsAndSkipsTheSharedSearchIndex(t *testing.T) {
	directory := t.TempDir()
	history := `{"type":"user","content":[{"type":"text","text":"the very first thing\nand a second line"}]}` + "\n"
	GrokSession(directory, "/work/app", history)
	sessions := filepath.Join(directory, "sessions")
	importTestWrite(t, filepath.Join(sessions, "session_search.sqlite"), "index")
	importTestWrite(t, filepath.Join(sessions, "%2Fwork%2Fapp", "summary.json.lock"), "")

	listed, err := ListGrokSessions(directory)
	if err != nil {
		t.Fatal(err)
	}

	if len(listed) != 1 {
		t.Fatalf("%#v", listed)
	}
	importTestEqual(t, listed[0].NativeSessionID, "01a00c3a-553f-71e0-95ab-aa04396d3ad7")
	importTestEqual(t, listed[0].Cwd, "/work/app")
	// A user message is conversation content, not a session-name fallback.
	importTestEqual(t, listed[0].Title, listed[0].NativeSessionID)

	located, err := LocateGrokSession(directory, ClaudeSessionSelectionLatest{})
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, located.NativeSessionID, listed[0].NativeSessionID)
}

func TestGrokCwdDirectoryNamesDecodeBackToTheirWorkingDirectory(t *testing.T) {
	directory := t.TempDir()
	encoded := filepath.Join(directory, "%2Fwork%2Fmy%20app")
	importTestMkdir(t, encoded)
	if decoded, ok := grokDecodeCwdDirname(encoded); !ok || decoded != "/work/my app" {
		t.Fatalf("%q %v", decoded, ok)
	}

	// The hash form is not reversible, so the recorded `.cwd` decides.
	hashed := filepath.Join(directory, "app-0123456789abcdef")
	importTestMkdir(t, hashed)
	if decoded, ok := grokDecodeCwdDirname(hashed); ok {
		t.Fatal(decoded)
	}
	importTestWrite(t, filepath.Join(hashed, ".cwd"), "/work/a-very-long-path\n")
	if decoded, ok := grokDecodeCwdDirname(hashed); !ok || decoded != "/work/a-very-long-path" {
		t.Fatalf("%q %v", decoded, ok)
	}
}

func TestKimiLocatorRetainsItsNativeSessionDirectoryPrefix(t *testing.T) {
	directory := t.TempDir()
	id := "90c30a64-54f7-4261-90f1-e75b1c14311c"
	session := filepath.Join(directory, "sessions/project", "session_"+id)
	importTestMkdir(t, session)
	importTestWrite(t, filepath.Join(directory, "session_index.jsonl"), importTestJSON(t, jsonvalue.Object{
		{Key: "sessionId", Value: "session_" + id},
		{Key: "sessionDir", Value: session},
		{Key: "workDir", Value: "/work/app"},
	}))

	located, err := LocateKimiSession(directory, ClaudeSessionSelectionNativeSessionID("session_"+id))
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, located.NativeSessionID, "session_"+id)
	importTestEqual(t, importTestCanonical(t, located.SessionPath), importTestCanonical(t, session))
}

func TestKimiListingMatchesNativeIndexVisibilityAndTitle(t *testing.T) {
	directory := t.TempDir()
	workspace := filepath.Join(directory, "sessions/project")
	importTestMkdir(t, workspace)
	visible := filepath.Join(workspace, "session_visible")
	archived := filepath.Join(workspace, "session_archived")
	deleted := filepath.Join(workspace, "session_deleted")
	unindexed := filepath.Join(workspace, "session_unindexed")
	for _, session := range []string{visible, archived, deleted, unindexed} {
		importTestMkdir(t, session)
	}
	importTestWrite(t, filepath.Join(visible, "state.json"), NativeFixture(
		`{"workDir":"/work/native","title":"Generated","customTitle":"Native custom title"}`))
	importTestWrite(t, filepath.Join(archived, "state.json"), NativeFixture(
		`{"workDir":"/work/app","title":"Archived","archived":true}`))
	index := []string{
		importTestJSON(t, jsonvalue.Object{{Key: "sessionId", Value: "session_visible"}, {Key: "sessionDir", Value: visible}, {Key: "workDir", Value: NativeCwd("/work/index")}}),
		importTestJSON(t, jsonvalue.Object{{Key: "sessionId", Value: "session_archived"}, {Key: "sessionDir", Value: archived}, {Key: "workDir", Value: NativeCwd("/work/app")}}),
		importTestJSON(t, jsonvalue.Object{{Key: "sessionId", Value: "session_deleted"}, {Key: "sessionDir", Value: deleted}, {Key: "workDir", Value: NativeCwd("/work/app")}}),
		importTestJSON(t, jsonvalue.Object{{Key: "sessionId", Value: "session_deleted"}, {Key: "deleted", Value: true}}),
	}
	importTestWrite(t, filepath.Join(directory, "session_index.jsonl"), strings.Join(index, "\n"))

	listed, err := ListKimiSessions(directory)
	if err != nil {
		t.Fatal(err)
	}
	if len(listed) != 1 {
		t.Fatalf("%#v", listed)
	}
	importTestEqual(t, listed[0].NativeSessionID, "session_visible")
	importTestEqual(t, listed[0].Title, "Native custom title")
	importTestEqual(t, listed[0].Cwd, NativeCwd("/work/native"))
}

const importTestCompactedNotice = "Earlier turns were compacted by the native harness."

// importTestLine is one dialogue row: the transcript role and its text.
type importTestLine struct {
	role string
	text string
}

func importTestDialogue(t *testing.T, transcript *ClaudeTranscript) []importTestLine {
	t.Helper()
	text := func(raw json.RawMessage, path ...string) string {
		value, err := jsonvalue.Parse(raw)
		if err != nil {
			t.Fatal(err)
		}
		found, ok := importJSONStr(importJSONPointer(value, path...))
		if !ok {
			t.Fatalf("no text in %s", raw)
		}
		return found
	}
	lines := []importTestLine{}
	for _, item := range transcript.items {
		switch body := item.Body.(type) {
		case core.TranscriptBodyUser:
			lines = append(lines, importTestLine{"user", text(body.Content[0], "text")})
		case core.TranscriptBodyAgent:
			if body.Streaming {
				t.Fatalf("unexpected import item %#v", body)
			}
			lines = append(lines, importTestLine{"agent", text(body.Chunks[0], "content", "text")})
		case core.TranscriptBodyThought:
			if body.Streaming {
				t.Fatalf("unexpected import item %#v", body)
			}
			lines = append(lines, importTestLine{"thought", text(body.Chunks[0], "content", "text")})
		case core.TranscriptBodySystem:
			lines = append(lines, importTestLine{"system", body.Text})
		default:
			t.Fatalf("unexpected import item %#v", body)
		}
	}
	return lines
}

func TestDetachedImportUsesTheSelectedNonoriginRemoteBaseline(t *testing.T) {
	directory := t.TempDir()
	app := filepath.Join(directory, "app")
	importTestInitializeRepository(t, app, "app")
	importTestGit(t, app, "remote", "rename", "origin", "upstream")
	importTestGit(t, app, "symbolic-ref", "refs/remotes/upstream/HEAD", "refs/remotes/upstream/main")
	importTestGit(t, app, "checkout", "--detach")
	baseline, err := importDeltaBase(app, "https://github.com/example/app.git")
	if err != nil {
		t.Fatal(err)
	}
	if baseline == "" {
		t.Fatal("empty baseline")
	}
}

// importTestClaudeScanFixture writes one Claude transcript whose title
// record sits on the last line, after enough chatter to make the line
// prefilter matter.
func importTestClaudeScanFixture(t *testing.T, path string, title string) {
	t.Helper()
	lines := []string{
		`{"type":"user","entrypoint":"cli","cwd":"/work/app","gitBranch":"feature","message":{"content":"start"}}`,
	}
	for index := range 200 {
		lines = append(lines, fmt.Sprintf(`{"type":"assistant","uuid":"line-%d","message":{"content":"chatter %d"}}`, index, index))
	}
	lines = append(lines, fmt.Sprintf(`{"type":"custom-title","customTitle":"%s"}`, title))
	importTestWrite(t, path, strings.Join(lines, "\n"))
}

func importTestScanClaudeTitles(t *testing.T, home string, cache *NativeScanCache) []string {
	t.Helper()
	titles := []string{}
	if err := ScanClaudeSessions(home, cache, func(progress SessionScanProgress[LocatedClaudeSession]) {
		if progress.Session != nil {
			titles = append(titles, progress.Session.Title)
		}
	}); err != nil {
		t.Fatal(err)
	}
	return titles
}

func TestUnchangedClaudeTranscriptsAreNotReopenedOnASecondScan(t *testing.T) {
	directory := t.TempDir()
	rollout := filepath.Join(directory, "projects/work/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl")
	importTestMkdir(t, filepath.Dir(rollout))
	importTestClaudeScanFixture(t, rollout, "First title")
	info, err := os.Stat(rollout)
	if err != nil {
		t.Fatal(err)
	}
	stamp := info.ModTime()

	cache := NewNativeScanCache()
	importTestEqual(t, importTestScanClaudeTitles(t, directory, cache), []string{"First title"})
	importTestEqual(t, cache.ParsedFiles(), uint64(1))

	// Rewrite the file with different content of the same length and
	// restore its modified time. A second scan that opened the file would see
	// the new title; a cached scan cannot.
	importTestClaudeScanFixture(t, rollout, "Later title")
	if err := os.Chtimes(rollout, time.Time{}, stamp); err != nil {
		t.Fatal(err)
	}

	importTestEqual(t, importTestScanClaudeTitles(t, directory, cache), []string{"First title"})
	importTestEqual(t, cache.ParsedFiles(), uint64(1))

	importTestClaudeScanFixture(t, rollout, "Later title")
	if err := os.Chtimes(rollout, time.Time{}, stamp.Add(10*time.Second)); err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, importTestScanClaudeTitles(t, directory, cache), []string{"Later title"})
	importTestEqual(t, cache.ParsedFiles(), uint64(2))
}

const importNamedLookupID = "019feb6c-6b55-7111-a210-000000000001"

// importTestCodexNamedRollout writes a Codex rollout at the standard
// `sessions/<year>/<month>/<day>` depth.
func importTestCodexNamedRollout(t *testing.T, home string, sessionID string, source any) string {
	t.Helper()
	day := filepath.Join(home, "sessions/2026/08/10")
	importTestMkdir(t, day)
	rollout := filepath.Join(day, fmt.Sprintf("rollout-2026-08-10T00-00-00-%s.jsonl", sessionID))
	importTestWrite(t, rollout, importTestJSON(t, jsonvalue.Object{
		{Key: "type", Value: "session_meta"},
		{Key: "payload", Value: jsonvalue.Object{{Key: "id", Value: sessionID}, {Key: "cwd", Value: "/work/app"}, {Key: "source", Value: source}}},
	}))
	return rollout
}

func importTestCodexSessionMeta(t *testing.T) string {
	return importTestJSON(t, jsonvalue.Object{
		{Key: "type", Value: "session_meta"},
		{Key: "payload", Value: jsonvalue.Object{{Key: "id", Value: importNamedLookupID}, {Key: "cwd", Value: "/work/app"}}},
	})
}

// Hard-won: c72b6322: a named Codex rollout hidden by picker filters was wrongly reported as missing.
func TestCodexLookupByIDFindsASessionThePickerHides(t *testing.T) {
	directory := t.TempDir()
	// A structured source identifies a subagent rollout, which the resume
	// picker hides. Naming it by id must still import it.
	rollout := importTestCodexNamedRollout(t, directory, importNamedLookupID, jsonvalue.Object{
		{Key: "subagent", Value: jsonvalue.Object{{Key: "parent_thread_id", Value: "019feb6c-6b55-7111-a210-000000000009"}}},
	})

	listed, err := ListCodexSessions(directory)
	if err != nil || len(listed) != 0 {
		t.Fatal(listed, err)
	}
	located, err := LocateCodexSession(directory, ClaudeSessionSelectionNativeSessionID(importNamedLookupID))
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, located.NativeSessionID, importNamedLookupID)
	importTestEqual(t, located.JSONLPath, rollout)
}

func TestCodexLookupByIDReportsASymlinkedRollout(t *testing.T) {
	importTestSkipUnlessUnix(t)
	directory := t.TempDir()
	stored := filepath.Join(directory, "stored.jsonl")
	importTestWrite(t, stored, importTestCodexSessionMeta(t))
	day := filepath.Join(directory, "sessions/2026/08/10")
	importTestMkdir(t, day)
	importTestSymlink(t, stored, filepath.Join(day, fmt.Sprintf("rollout-2026-08-10T00-00-00-%s.jsonl", importNamedLookupID)))

	_, err := LocateCodexSession(directory, ClaudeSessionSelectionNativeSessionID(importNamedLookupID))
	importTestErrorContains(t, err, "is a symlink", "was not found")
}

// Hard-won: 659575aa: following a Codex session symlink made by-id lookup walk an arbitrarily large unrelated tree.
func TestCodexLookupByIDStopsAtTheRolloutDepthInsideASymlinkedTree(t *testing.T) {
	importTestSkipUnlessUnix(t)
	directory := t.TempDir()
	// A symlink in the sessions tree can point at anything, including a tree
	// far deeper than a rollout can sit in. Walking all of it would make the
	// lookup read an unbounded number of directories.
	linked := filepath.Join(directory, "elsewhere")
	deep := linked
	for level := range 12 {
		deep = filepath.Join(deep, fmt.Sprintf("level-%d", level))
	}
	importTestMkdir(t, deep)
	importTestWrite(t, filepath.Join(deep, fmt.Sprintf("rollout-2026-08-10T00-00-00-%s.jsonl", importNamedLookupID)), importTestCodexSessionMeta(t))
	importTestMkdir(t, filepath.Join(directory, "sessions"))
	importTestSymlink(t, linked, filepath.Join(directory, "sessions/linked"))

	_, err := LocateCodexSession(directory, ClaudeSessionSelectionNativeSessionID(importNamedLookupID))
	importTestErrorContains(t, err, "was not found", "")
}

func TestCodexLookupByIDReportsARolloutBehindASymlinkedDayDirectory(t *testing.T) {
	importTestSkipUnlessUnix(t)
	directory := t.TempDir()
	// A symlink standing in for one level of the rollout shape is still read,
	// so the depth bound does not turn this refusal back into "not found".
	stored := filepath.Join(directory, "stored")
	importTestMkdir(t, stored)
	importTestWrite(t, filepath.Join(stored, fmt.Sprintf("rollout-2026-08-10T00-00-00-%s.jsonl", importNamedLookupID)), importTestCodexSessionMeta(t))
	importTestMkdir(t, filepath.Join(directory, "sessions/2026/08"))
	importTestSymlink(t, stored, filepath.Join(directory, "sessions/2026/08/10"))

	_, err := LocateCodexSession(directory, ClaudeSessionSelectionNativeSessionID(importNamedLookupID))
	importTestErrorContains(t, err, "is a symlinked rollout directory", "was not found")
}

// Hard-won: 659575aa: an unreadable Codex directory aborted lookup instead of returning a bounded, named refusal.
func TestCodexLookupByIDReportsADirectoryItCannotRead(t *testing.T) {
	importTestSkipUnlessUnix(t)
	directory := t.TempDir()
	day := filepath.Join(directory, "sessions/2026/08/10")
	importTestMkdir(t, day)
	if err := os.Chmod(day, 0o000); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.Chmod(day, 0o755) })
	// Root ignores the mode, so there is nothing to observe there.
	if _, err := os.ReadDir(day); err == nil {
		return
	}

	_, err := LocateCodexSession(directory, ClaudeSessionSelectionNativeSessionID(importNamedLookupID))
	if err := os.Chmod(day, 0o755); err != nil {
		t.Fatal(err)
	}

	importTestErrorContains(t, err, "cannot be read", "was not found")
}

// Hard-won: c72b6322: a named Kimi session omitted by its index was wrongly reported as missing.
func TestKimiLookupByIDFindsASessionTheIndexHides(t *testing.T) {
	directory := t.TempDir()
	session := filepath.Join(directory, "sessions/project", "session_"+importNamedLookupID)
	importTestMkdir(t, session)
	importTestWrite(t, filepath.Join(session, "state.json"), NativeFixture(
		`{"workDir":"/work/app","title":"Unindexed","archived":true}`))
	importTestWrite(t, filepath.Join(directory, "session_index.jsonl"), "")

	listed, err := ListKimiSessions(directory)
	if err != nil || len(listed) != 0 {
		t.Fatal(listed, err)
	}
	located, err := LocateKimiSession(directory, ClaudeSessionSelectionNativeSessionID("session_"+importNamedLookupID))
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, located.NativeSessionID, "session_"+importNamedLookupID)
	importTestEqual(t, located.Cwd, NativeCwd("/work/app"))
}

func TestKimiLookupByIDReportsASymlinkedSessionDirectory(t *testing.T) {
	importTestSkipUnlessUnix(t)
	directory := t.TempDir()
	stored := filepath.Join(directory, "stored")
	importTestMkdir(t, stored)
	importTestWrite(t, filepath.Join(stored, "state.json"), `{"workDir":"/work/app"}`)
	workspace := filepath.Join(directory, "sessions/project")
	importTestMkdir(t, workspace)
	importTestSymlink(t, stored, filepath.Join(workspace, "session_"+importNamedLookupID))

	_, err := LocateKimiSession(directory, ClaudeSessionSelectionNativeSessionID("session_"+importNamedLookupID))
	importTestErrorContains(t, err, "is a symlink", "was not found")
}

func TestGrokLookupByIDReportsASymlinkedSessionDirectory(t *testing.T) {
	importTestSkipUnlessUnix(t)
	directory := t.TempDir()
	stored := filepath.Join(directory, "stored")
	importTestMkdir(t, stored)
	importTestWrite(t, filepath.Join(stored, "summary.json"), importTestJSON(t, jsonvalue.Object{
		{Key: "info", Value: jsonvalue.Object{{Key: "cwd", Value: "/work/app"}}},
	}))
	workspace := filepath.Join(directory, "sessions/%2Fwork%2Fapp")
	importTestMkdir(t, workspace)
	importTestSymlink(t, stored, filepath.Join(workspace, importNamedLookupID))

	_, err := LocateGrokSession(directory, ClaudeSessionSelectionNativeSessionID(importNamedLookupID))
	importTestErrorContains(t, err, "is a symlink", "was not found")
}

// Hard-won: 9c566d06: Grok listing offered a session whose symlinked workspace the archive importer could not collect.
func TestGrokListingAndLookupAgreeAboutASymlinkedWorkingDirectory(t *testing.T) {
	importTestSkipUnlessUnix(t)
	directory := t.TempDir()
	// The session directory itself is real; only the working directory above
	// it is a symlink, so the archive step would store nothing for this
	// session.
	stored := filepath.Join(directory, "stored/%2Fwork%2Fapp")
	session := filepath.Join(stored, importNamedLookupID)
	importTestMkdir(t, session)
	importTestWrite(t, filepath.Join(session, "summary.json"), importTestJSON(t, jsonvalue.Object{
		{Key: "info", Value: jsonvalue.Object{{Key: "cwd", Value: "/work/app"}}},
	}))
	importTestMkdir(t, filepath.Join(directory, "sessions"))
	importTestSymlink(t, stored, filepath.Join(directory, "sessions/%2Fwork%2Fapp"))

	listed, err := ListGrokSessions(directory)
	if err != nil || len(listed) != 0 {
		t.Fatal(listed, err)
	}
	_, err = LocateGrokSession(directory, ClaudeSessionSelectionNativeSessionID(importNamedLookupID))
	importTestErrorContains(t, err, "is a symlinked working directory", "was not found")
}

// Hard-won: c72b6322: a named Grok session without cwd was reported as absent instead of explaining why it could not be imported.
func TestGrokLookupByIDReportsASessionWithoutACwd(t *testing.T) {
	directory := t.TempDir()
	// The hashed directory-name form keeps the real cwd in `.cwd`, which is
	// missing here, and the summary records none either.
	session := filepath.Join(directory, "sessions/app-0123456789abcdef", importNamedLookupID)
	importTestMkdir(t, session)
	importTestWrite(t, filepath.Join(session, "summary.json"), importTestJSON(t, jsonvalue.Object{
		{Key: "session_summary", Value: "A session"},
	}))

	_, err := LocateGrokSession(directory, ClaudeSessionSelectionNativeSessionID(importNamedLookupID))
	importTestErrorContains(t, err, "has no cwd", "was not found")
}
