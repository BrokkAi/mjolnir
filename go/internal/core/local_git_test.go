// Port of the tests in mj-core/src/local_git.rs.

package core

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"testing"
)

// Hard-won: 71b0e8d1: Git 2.25 echoed unsupported options with success and could persist a false repository path
func TestGitCommonDirectoryResolvesRelativeOutputAndRejectsEchoedOptions(t *testing.T) {
	for _, row := range [][3]string{
		{"/projects/app", ".git\n", "/projects/app/.git"},
		{"/projects/app/src/nested", "../../.git\n", "/projects/app/.git"},
		{"/worktrees/side", "/projects/app/.git\n", "/projects/app/.git"},
	} {
		resolved, err := ResolveGitPath(row[0], row[1])
		if err != nil {
			t.Fatalf("%q: %v", row[1], err)
		}
		if comparePaths(resolved, row[2]) != 0 {
			t.Fatalf("%q resolved to %q, want %q", row[1], resolved, row[2])
		}
	}
	for _, output := range []string{
		"",
		"\n",
		"--path-format=absolute\n.git\n",
		"--path-format=absolute\n",
		"../../../../.git\n",
	} {
		if _, err := ResolveGitPath("/projects/app", output); err == nil {
			t.Fatalf("accepted %q", output)
		}
	}
}

// runTestGit runs git in `path` and fails the test when git does.
func runTestGit(t *testing.T, path string, args ...string) {
	t.Helper()
	command := exec.Command("git", args...)
	command.Dir = path
	output, err := command.CombinedOutput()
	if err != nil {
		t.Fatalf("git %s: %v\n%s", strings.Join(args, " "), err, output)
	}
}

// readTestGit runs git in `path` and returns its standard output.
func readTestGit(t *testing.T, path string, args ...string) string {
	t.Helper()
	command := exec.Command("git", args...)
	command.Dir = path
	output, err := command.Output()
	if err != nil {
		t.Fatalf("git %s: %v", strings.Join(args, " "), err)
	}
	return string(output)
}

func TestLocalStatusDistinguishesCleanAndDirtyRepositories(t *testing.T) {
	directory := t.TempDir()
	runTestGit(t, directory, "init", "-q", "-b", "main")
	runTestGit(t, directory, "config", "user.name", "Hel Test")
	runTestGit(t, directory, "config", "user.email", "hel@example.test")
	if err := os.WriteFile(filepath.Join(directory, "tracked"), []byte("clean"), 0o644); err != nil {
		t.Fatal(err)
	}
	runTestGit(t, directory, "add", ".")
	runTestGit(t, directory, "commit", "-qm", "base")
	status, err := localStatus(directory)
	if err != nil || status != nil {
		t.Fatalf("clean status = %v, %v", status, err)
	}
	if err := os.WriteFile(filepath.Join(directory, "untracked"), []byte("dirty"), 0o644); err != nil {
		t.Fatal(err)
	}
	status, err = localStatus(directory)
	if err != nil || status == nil || !strings.Contains(*status, "untracked") {
		t.Fatalf("dirty status = %v, %v", status, err)
	}
}

// Hard-won: ff0e61c8: sessions in linked worktrees created duplicate project bundles
func TestCanonicalRepositoryMapsALinkedWorktreeToItsMainRepository(t *testing.T) {
	directory := t.TempDir()
	main := filepath.Join(directory, "main")
	if err := os.MkdirAll(main, 0o755); err != nil {
		t.Fatal(err)
	}
	runTestGit(t, main, "init", "-q", "-b", "main")
	runTestGit(t, main, "config", "user.name", "Hel Test")
	runTestGit(t, main, "config", "user.email", "hel@example.test")
	if err := os.WriteFile(filepath.Join(main, "tracked"), []byte("clean"), 0o644); err != nil {
		t.Fatal(err)
	}
	runTestGit(t, main, "add", ".")
	runTestGit(t, main, "commit", "-qm", "base")
	worktree := filepath.Join(directory, "main2")
	runTestGit(t, main, "worktree", "add", "-q", "-b", "side", worktree)

	expected, err := canonicalizePath(main)
	if err != nil {
		t.Fatal(err)
	}
	for _, path := range []string{main, worktree} {
		canonical, err := CanonicalRepository(path)
		if err != nil {
			t.Fatal(err)
		}
		if comparePaths(canonical, expected) != 0 {
			t.Fatalf("%s maps to %s, want %s", path, canonical, expected)
		}
	}
}

func initializedTestRepository(t *testing.T) string {
	t.Helper()
	directory := t.TempDir()
	runTestGit(t, directory, "init", "-q", "-b", "main")
	return directory
}

func TestProjectIdentityUsesDefaultBranchTrackingAndKeepsSelectedWorktree(t *testing.T) {
	path := initializedTestRepository(t)
	runTestGit(t, path, "config", "user.name", "Project Test")
	runTestGit(t, path, "config", "user.email", "project@example.test")
	if err := os.WriteFile(filepath.Join(path, "tracked"), []byte("base"), 0o644); err != nil {
		t.Fatal(err)
	}
	runTestGit(t, path, "add", "tracked")
	runTestGit(t, path, "commit", "-qm", "base")
	for _, remote := range [][2]string{
		{"origin", "https://github.com/acme/app.git"},
		{"upstream", "git@github.com:acme/canonical.git"},
		{"publish", "https://github.com/me/fork.git"},
	} {
		runTestGit(t, path, "remote", "add", remote[0], remote[1])
	}
	runTestGit(t, path, "symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main")
	runTestGit(t, path, "config", "branch.main.remote", "upstream")
	runTestGit(t, path, "config", "branch.main.pushRemote", "publish")
	expected := "git@github.com:acme/canonical.git"
	identity, err := IdentityFetchURL(path, ProcessExecutor{})
	if err != nil || identity == nil || *identity != expected {
		t.Fatalf("identity = %v, %v", identity, err)
	}
	runTestGit(t, path, "checkout", "-qb", "feature")
	runTestGit(t, path, "config", "branch.feature.remote", "publish")
	identity, err = IdentityFetchURL(path, ProcessExecutor{})
	if err != nil || identity == nil || *identity != expected {
		t.Fatalf("feature identity = %v, %v", identity, err)
	}
	selected := filepath.Join(path, "src")
	if err := os.Mkdir(selected, 0o755); err != nil {
		t.Fatal(err)
	}
	local, err := LocalIdentity(selected, ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	if want := (RepositoryIdentityGithub{Owner: "acme", Repository: "canonical"}); local != want {
		t.Fatalf("local identity = %#v", local)
	}
	source, err := ResolveLocalRepository(path, ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	if source.FetchURL != "https://github.com/me/fork.git" || !slices.Equal(source.PushURLs, []string{"https://github.com/me/fork.git"}) {
		t.Fatalf("source = %#v", source)
	}
}

func TestNoRemoteProjectIdentityIsItsCanonicalRepository(t *testing.T) {
	directory := initializedTestRepository(t)
	nested := filepath.Join(directory, "src")
	if err := os.Mkdir(nested, 0o755); err != nil {
		t.Fatal(err)
	}
	identity, err := LocalIdentity(nested, ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	canonical, err := canonicalizePath(directory)
	if err != nil {
		t.Fatal(err)
	}
	local, ok := identity.(RepositoryIdentityLocal)
	if !ok || comparePaths(local.Root, canonical) != 0 {
		t.Fatalf("identity = %#v, want local %s", identity, canonical)
	}
}

func repairTestBundle(path string) ProjectBundle {
	return ProjectBundle{
		PrimaryRepo:  "repo",
		Repositories: []ProjectRepository{{ID: "repo", Local: &path, Destination: "repo"}},
	}
}

// Hard-won: d07fc1f0: a configured missing project looked selectable, then failed without an explanation
func TestAMissingProjectDirectoryIsReportedInPlainLanguage(t *testing.T) {
	missing := filepath.Join(t.TempDir(), "gone")
	bundle := repairTestBundle(missing)
	bundle.Repositories[0].ID = "missingpath"
	_, err := RepositoryRemoteRepairs(&bundle, ProcessExecutor{})
	refusal, ok := RefusalOf(err)
	if !ok {
		t.Fatalf("error is not a refusal: %v", err)
	}
	if want := fmt.Sprintf("Project \"missingpath\": the directory %s does not exist.", missing); refusal.Message() != want {
		t.Fatalf("message = %q, want %q", refusal.Message(), want)
	}
}

func TestStaleTrackingRepairIsReviewableAndPreservesFetchAndPushIntent(t *testing.T) {
	for _, remote := range []string{"origin", "sole"} {
		path := initializedTestRepository(t)
		runTestGit(t, path, "remote", "add", remote, "https://example.com/fetch.git")
		runTestGit(t, path, "config", "branch.main.remote", "missing")
		runTestGit(t, path, "config", "branch.main.merge", "refs/heads/main")
		runTestGit(t, path, "config", fmt.Sprintf("remote.%s.pushurl", remote), "ssh://git@example.com/push.git")
		if remote == "origin" {
			runTestGit(t, path, "remote", "add", "publish", "https://example.com/publish.git")
			runTestGit(t, path, "config", "branch.main.pushRemote", "publish")
		}
		bundle := repairTestBundle(path)
		repairs, err := RepositoryRemoteRepairs(&bundle, ProcessExecutor{})
		if err != nil {
			t.Fatal(err)
		}
		if len(repairs) != 1 || repairs[0].ReplacementRemote != remote || repairs[0].FetchURL != "https://example.com/fetch.git" {
			t.Fatalf("repairs = %#v", repairs)
		}
		pushURL := "ssh://git@example.com/push.git"
		if remote == "origin" {
			pushURL = "https://example.com/publish.git"
		}
		if !slices.Equal(repairs[0].PushURLs, []string{DisplayURL(pushURL)}) {
			t.Fatalf("push URLs = %q", repairs[0].PushURLs)
		}
		configured := func(key string) string {
			value, err := gitConfig(path, key, ProcessExecutor{})
			if err != nil || value == nil {
				t.Fatalf("%s = %v, %v", key, value, err)
			}
			return *value
		}
		if got := configured("branch.main.remote"); got != "missing" {
			t.Fatalf("branch remote = %q before repair", got)
		}
		if err := ApplyRepositoryRemoteRepairs(&bundle, repairs, ProcessExecutor{}); err != nil {
			t.Fatal(err)
		}
		if got := configured("branch.main.remote"); got != remote {
			t.Fatalf("branch remote = %q after repair", got)
		}
		if got := configured("branch.main.merge"); got != "refs/heads/main" {
			t.Fatalf("branch merge = %q after repair", got)
		}
		source, err := ResolveLocalRepository(path, ProcessExecutor{})
		if err != nil {
			t.Fatal(err)
		}
		if source.FetchURL != repairs[0].FetchURL || !slices.Equal(source.PushURLs, []string{pushURL}) {
			t.Fatalf("source = %#v", source)
		}
		again, err := RepositoryRemoteRepairs(&bundle, ProcessExecutor{})
		if err != nil || len(again) != 0 {
			t.Fatalf("repairs after repair = %#v, %v", again, err)
		}
	}
}

func TestTrackingRepairRejectsChangedDestinationsAndUnrelatedRepositories(t *testing.T) {
	path := initializedTestRepository(t)
	runTestGit(t, path, "remote", "add", "origin", "https://example.com/fetch.git")
	runTestGit(t, path, "config", "branch.main.remote", "missing")
	bundle := repairTestBundle(path)
	repairs, err := RepositoryRemoteRepairs(&bundle, ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	other := initializedTestRepository(t)
	otherBundle := repairTestBundle(other)
	if ApplyRepositoryRemoteRepairs(&otherBundle, repairs, ProcessExecutor{}) == nil {
		t.Fatal("applied a repair to an unrelated repository")
	}
	runTestGit(t, path, "remote", "set-url", "origin", "https://example.com/changed.git")
	if ApplyRepositoryRemoteRepairs(&bundle, repairs, ProcessExecutor{}) == nil {
		t.Fatal("applied a repair whose destination changed")
	}
	value, err := gitConfig(path, "branch.main.remote", ProcessExecutor{})
	if err != nil || value == nil || *value != "missing" {
		t.Fatalf("branch remote = %v, %v", value, err)
	}
}

func TestTrackingRepairDoesNotOfferAnUnusableOrAmbiguousDestination(t *testing.T) {
	path := initializedTestRepository(t)
	runTestGit(t, path, "config", "branch.main.remote", "missing")
	runTestGit(t, path, "remote", "add", "origin", "../local")
	bundle := repairTestBundle(path)
	if _, err := RepositoryRemoteRepairs(&bundle, ProcessExecutor{}); err == nil {
		t.Fatal("offered a local-path destination")
	}
	runTestGit(t, path, "remote", "rename", "origin", "first")
	runTestGit(t, path, "remote", "set-url", "first", "https://example.com/first.git")
	runTestGit(t, path, "remote", "add", "second", "https://example.com/second.git")
	if _, err := RepositoryRemoteRepairs(&bundle, ProcessExecutor{}); err == nil {
		t.Fatal("offered an ambiguous destination")
	}
}

func TestResolverUsesANonoriginSoleRemoteForFetchAndPush(t *testing.T) {
	directory := initializedTestRepository(t)
	runTestGit(t, directory, "remote", "add", "upstream", "https://example.com/upstream.git")
	source, err := ResolveLocalRepository(directory, ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	if source.FetchURL != "https://example.com/upstream.git" || !slices.Equal(source.PushURLs, []string{"https://example.com/upstream.git"}) {
		t.Fatalf("source = %#v", source)
	}
}

func TestResolverHonorsBranchPushRemoteAndMultiplePushURLs(t *testing.T) {
	directory := initializedTestRepository(t)
	runTestGit(t, directory, "remote", "add", "fetch", "https://example.com/fetch.git")
	runTestGit(t, directory, "remote", "add", "publish", "https://example.com/publish.git")
	runTestGit(t, directory, "config", "branch.main.remote", "fetch")
	runTestGit(t, directory, "config", "branch.main.pushRemote", "publish")
	runTestGit(t, directory, "config", "--add", "remote.publish.pushurl", "ssh://git@example.com/one.git")
	runTestGit(t, directory, "config", "--add", "remote.publish.pushurl", "ssh://git@example.com/two.git")
	source, err := ResolveLocalRepository(directory, ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	if source.FetchURL != "https://example.com/fetch.git" ||
		!slices.Equal(source.PushURLs, []string{"ssh://git@example.com/one.git", "ssh://git@example.com/two.git"}) {
		t.Fatalf("source = %#v", source)
	}
}

func TestResolverUsesRemotePushDefaultWhenBranchHasNoPushRemote(t *testing.T) {
	directory := initializedTestRepository(t)
	runTestGit(t, directory, "remote", "add", "fetch", "https://example.com/fetch.git")
	runTestGit(t, directory, "remote", "add", "publish", "https://example.com/publish.git")
	runTestGit(t, directory, "config", "branch.main.remote", "fetch")
	runTestGit(t, directory, "config", "remote.pushDefault", "publish")
	source, err := ResolveLocalRepository(directory, ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	if source.FetchURL != "https://example.com/fetch.git" || !slices.Equal(source.PushURLs, []string{"https://example.com/publish.git"}) {
		t.Fatalf("source = %#v", source)
	}
}

func TestResolverAppliesFetchAndPushURLRewrites(t *testing.T) {
	directory := initializedTestRepository(t)
	runTestGit(t, directory, "remote", "add", "fetch", "fetch:org/repo.git")
	runTestGit(t, directory, "remote", "add", "publish", "publish:org/repo.git")
	runTestGit(t, directory, "config", "branch.main.remote", "fetch")
	runTestGit(t, directory, "config", "branch.main.pushRemote", "publish")
	runTestGit(t, directory, "config", "url.https://example.com/.insteadOf", "fetch:")
	runTestGit(t, directory, "config", "url.ssh://git@example.com/.pushInsteadOf", "publish:")
	source, err := ResolveLocalRepository(directory, ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	if source.FetchURL != "https://example.com/org/repo.git" || !slices.Equal(source.PushURLs, []string{"ssh://git@example.com/org/repo.git"}) {
		t.Fatalf("source = %#v", source)
	}
}

func TestResolverRejectsLocalPathsAndIgnoresDirtyOrUnpublishedState(t *testing.T) {
	directory := initializedTestRepository(t)
	if err := os.WriteFile(filepath.Join(directory, "untracked"), []byte("work"), 0o644); err != nil {
		t.Fatal(err)
	}
	runTestGit(t, directory, "remote", "add", "origin", "../another-repository")
	_, err := ResolveLocalRepository(directory, ProcessExecutor{})
	if err == nil || !strings.Contains(err.Error(), "local repository path") {
		t.Fatalf("error = %v", err)
	}
	refusal, ok := RefusalOf(err)
	if !ok {
		t.Fatalf("error is not a reason the caller may see: %v", err)
	}
	if !strings.Contains(refusal.Message(), "not a network URL") || strings.Contains(refusal.Message(), "another-repository") {
		t.Fatalf("refusal = %q", refusal.Message())
	}
}

func TestResolverReportsMissingAndAmbiguousRemoteSelection(t *testing.T) {
	noRemote := initializedTestRepository(t)
	_, err := ResolveLocalRepository(noRemote, ProcessExecutor{})
	refusal, ok := RefusalOf(err)
	if !ok {
		t.Fatalf("a missing remote is a reason the caller may see, not a log reference: %v", err)
	}
	if !strings.Contains(refusal.Message(), "has no remote") {
		t.Fatalf("refusal = %q", refusal.Message())
	}

	ambiguous := initializedTestRepository(t)
	runTestGit(t, ambiguous, "remote", "add", "first", "https://example.com/first.git")
	runTestGit(t, ambiguous, "remote", "add", "second", "https://example.com/second.git")
	_, err = ResolveLocalRepository(ambiguous, ProcessExecutor{})
	if err == nil || !strings.Contains(err.Error(), "multiple Git remotes") {
		t.Fatalf("error = %v", err)
	}
}

// Hard-won: DSH campaign 2026-10-23 (A-6): changed files showed git-quoted octal escapes for non-ASCII names
func TestGitStatusNamesFilesAsTheyAreOnDiskForRealGitOutput(t *testing.T) {
	path := initializedTestRepository(t)
	runTestGit(t, path, "config", "user.name", "Status Test")
	runTestGit(t, path, "config", "user.email", "status@example.test")
	write := func(name, content string) {
		if err := os.WriteFile(filepath.Join(path, name), []byte(content), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	write("before.txt", "one\ntwo\nthree\nfour\n")
	write("tracked 日本.txt", "x\n")
	runTestGit(t, path, "add", ".")
	runTestGit(t, path, "commit", "-q", "-m", "base")
	runTestGit(t, path, "mv", "before.txt", "after 名前.txt")
	write("my notes 日本.txt", "hello\n")
	// Windows forbids `"` in a file name; the other names still exercise
	// spaces, non-ASCII text, and a rename there.
	if runtime.GOOS != "windows" {
		write("quo\"te.txt", "q\n")
	}
	write("tracked 日本.txt", "x\ny\n")
	status := ParseGitStatus(path, "main", nil, readTestGit(t, path, NumstatArgs[:]...), readTestGit(t, path, StatusArgs[:]...))
	var files [][2]string
	for _, file := range status.Changed {
		files = append(files, [2]string{file.Path, file.Kind()})
	}
	expected := [][2]string{
		{"after 名前.txt", "renamed"},
		{"my notes 日本.txt", "new"},
		{"tracked 日本.txt", "modified"},
	}
	if runtime.GOOS != "windows" {
		expected = append(expected, [2]string{"quo\"te.txt", "new"})
	}
	order := func(a, b [2]string) int {
		if c := strings.Compare(a[0], b[0]); c != 0 {
			return c
		}
		return strings.Compare(a[1], b[1])
	}
	slices.SortFunc(files, order)
	slices.SortFunc(expected, order)
	if !slices.Equal(files, expected) {
		t.Fatalf("files = %q, want %q", files, expected)
	}
	for _, file := range status.Changed {
		if file.Path == "tracked 日本.txt" {
			if file.Added == nil || *file.Added != 1 || file.Removed == nil || *file.Removed != 0 {
				t.Fatalf("tracked counts = %v, %v", file.Added, file.Removed)
			}
		}
	}
}

func TestGitStatusParsingJoinsPorcelainNamesWithNumstatCounts(t *testing.T) {
	aheadBehind := "2\t1\n"
	status := ParseGitStatus(
		"/work",
		"feature/x\n",
		&aheadBehind,
		"12\t3\tsrc/main.rs\x00-\t-\tlogo.png\x00\t5\t\x00old.rs\x00new.rs\x00",
		" M src/main.rs\x00?? notes.md\x00R  new.rs\x00old.rs\x00A  logo.png\x00",
	)
	if status.Branch != "feature/x" {
		t.Fatalf("branch = %q", status.Branch)
	}
	if status.Behind == nil || *status.Behind != 2 || status.Ahead == nil || *status.Ahead != 1 {
		t.Fatalf("behind/ahead = %v/%v", status.Behind, status.Ahead)
	}
	if len(status.Changed) != 4 {
		t.Fatalf("changed = %#v", status.Changed)
	}
	main := status.Changed[0]
	if main.Path != "src/main.rs" || main.Kind() != "modified" || *main.Added != 12 || *main.Removed != 3 {
		t.Fatalf("main = %#v", main)
	}
	if status.Changed[1].Kind() != "new" || status.Changed[1].Added != nil {
		t.Fatalf("notes = %#v", status.Changed[1])
	}
	if status.Changed[2].Path != "new.rs" || status.Changed[2].Kind() != "renamed" {
		t.Fatalf("rename = %#v", status.Changed[2])
	}
	if status.Changed[3].Added != nil {
		t.Fatal("binary files count nothing")
	}
	if added, removed := status.Totals(); added != 12 || removed != 8 {
		t.Fatalf("a rename carries its own counts: totals = %d, %d", added, removed)
	}
	if got := status.RowText(); got != "⎇ feature/x ↑1 ↓2 ±4" {
		t.Fatalf("row = %q", got)
	}

	clean := ParseGitStatus("/work", "main", nil, "", "")
	if got := clean.RowText(); got != "⎇ main" {
		t.Fatalf("clean row = %q", got)
	}
	none := ParseGitStatus("/work", "not a git checkout", nil, "", "")
	if got := none.RowText(); got != "" {
		t.Fatalf("no repository row = %q", got)
	}
}

func TestParsesGitVersions(t *testing.T) {
	for _, row := range []struct {
		output string
		want   [3]uint32
	}{
		{"git version 2.53.0\n", [3]uint32{2, 53, 0}},
		{"git version 2.39.5 (Apple Git-154)", [3]uint32{2, 39, 5}},
		{"git version 2.53.0.windows.1", [3]uint32{2, 53, 0}},
		{"git version 2.40", [3]uint32{2, 40, 0}},
	} {
		if got, ok := ParseGitVersion(row.output); !ok || got != row.want {
			t.Fatalf("%q = %v, %v", row.output, got, ok)
		}
	}
	if _, ok := ParseGitVersion("not git"); ok {
		t.Fatal("parsed a version from other output")
	}
	old, _ := ParseGitVersion("git version 2.34.1")
	if CompareGitVersions(old, MinimumGitVersion) >= 0 {
		t.Fatal("2.34.1 is not below the minimum")
	}
	minimum, _ := ParseGitVersion("git version 2.39.5")
	if CompareGitVersions(minimum, MinimumGitVersion) < 0 {
		t.Fatal("2.39.5 is below the minimum")
	}
}
