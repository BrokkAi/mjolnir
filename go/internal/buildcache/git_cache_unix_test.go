//go:build unix

// Port of the Unix-only tests in mj-build-cache/src/git_cache.rs.

package buildcache

import (
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func firstObjectFile(root string) (string, bool) {
	entries, err := os.ReadDir(root)
	if err != nil {
		return "", false
	}
	isHex := func(name string) bool {
		for i := 0; i < len(name); i++ {
			c := name[i]
			if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f' || c >= 'A' && c <= 'F') {
				return false
			}
		}
		return true
	}
	for _, entry := range entries {
		path := filepath.Join(root, entry.Name())
		if !entry.IsDir() || len(entry.Name()) != 2 || !isHex(entry.Name()) {
			continue
		}
		objects, err := os.ReadDir(path)
		if err != nil {
			return "", false
		}
		for _, object := range objects {
			if object.Type().IsRegular() {
				return filepath.Join(path, object.Name()), true
			}
		}
	}
	packs, err := os.ReadDir(filepath.Join(root, "pack"))
	if err != nil {
		return "", false
	}
	for _, pack := range packs {
		if filepath.Ext(pack.Name()) == ".pack" {
			return filepath.Join(root, "pack", pack.Name()), true
		}
	}
	return "", false
}

func mode(t *testing.T, path string) os.FileMode {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	return info.Mode().Perm()
}

// Hard-won: 020257d9f: cached GitHub mirrors reused the original fetch transport.
func TestLocalSnapshotHardlinksObjectsButOwnsItsRefs(t *testing.T) {
	temporary := t.TempDir()
	source := filepath.Join(temporary, "source")
	cacheRoot := filepath.Join(temporary, "cache")
	snapshot := filepath.Join(cacheRoot, "sessions/session-12345678/repository.git")
	for _, step := range []struct {
		args    []string
		purpose string
	}{
		{[]string{"init", "--initial-branch=main", source}, "initialize source repository"},
		{[]string{"-C", source, "config", "user.name", "Hel Test"}, "configure test user"},
		{[]string{"-C", source, "config", "user.email", "hel@example.test"}, "configure test email"},
	} {
		command := core.NewCommandSpec("git", step.args).WithPurpose(step.purpose)
		if _, err := checked(mustRun(t, command), &command); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(source, "README.md"), []byte("cached objects\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	for _, step := range []struct {
		args    []string
		purpose string
	}{
		{[]string{"-C", source, "add", "README.md"}, "stage test content"},
		{[]string{"-C", source, "commit", "-m", "initial"}, "commit test content"},
	} {
		command := core.NewCommandSpec("git", step.args).WithPurpose(step.purpose)
		if _, err := checked(mustRun(t, command), &command); err != nil {
			t.Fatal(err)
		}
	}

	sshURL := "git@github.com:example/cache.git"
	httpsURL := "https://github.com/example/cache.git"
	config := filepath.Join(temporary, "gitconfig")
	rewrite := "url." + source + ".insteadOf"
	gitIn(t, source, "config", "--file", config, rewrite, sshURL)
	executor := gitExecutor(map[string]string{
		"GIT_CONFIG_GLOBAL":   config,
		"GIT_CONFIG_NOSYSTEM": "1",
	})
	key := repositoryCacheKey(sshURL)
	prepare := func(url, snapshot string) error {
		return prepareRepository(CacheHost{}, cacheRoot, key, url, snapshot, nil, nil, nil, executor)
	}
	if err := prepare(sshURL, snapshot); err != nil {
		t.Fatal(err)
	}

	mirror := filepath.Join(cacheRoot, "mirrors", key, "repo.git")
	object, ok := firstObjectFile(filepath.Join(mirror, "objects"))
	if !ok {
		t.Fatal("the mirror has no object")
	}
	relative, ok := pathStripPrefix(object, mirror)
	if !ok {
		t.Fatal(object)
	}
	snapshotObject := filepath.Join(snapshot, relative)
	mirrorDevice, mirrorInode := deviceAndInode(t, object)
	snapshotDevice, snapshotInode := deviceAndInode(t, snapshotObject)
	if mirrorDevice != snapshotDevice || mirrorInode != snapshotInode {
		t.Fatal("the snapshot does not hardlink the mirror's objects")
	}
	if inode(t, filepath.Join(mirror, "HEAD")) == inode(t, filepath.Join(snapshot, "HEAD")) {
		t.Fatal("the snapshot shares the mirror's refs")
	}
	if mode(t, filepath.Dir(snapshot))&0o005 != 0o005 {
		t.Fatalf("session root mode = %o", mode(t, filepath.Dir(snapshot)))
	}
	if mode(t, snapshotObject)&0o004 != 0o004 {
		t.Fatalf("object mode = %o", mode(t, snapshotObject))
	}

	initial := gitIn(t, snapshot, "rev-parse", "--verify", "HEAD")
	// Retire the original transport after populating the shared mirror.
	gitIn(t, source, "config", "--file", config, rewrite, httpsURL)
	missing := filepath.Join(temporary, "missing")
	gitIn(t, source, "config", "--file", config, "url."+missing+".insteadOf", sshURL)
	gitIn(t, source, "switch", "--quiet", "-c", "new-default")
	gitIn(t, source, "commit", "--quiet", "--allow-empty", "-m", "new upstream state")
	if repositoryCacheKey(httpsURL) != key {
		t.Fatal("GitHub transports do not share a mirror")
	}
	refreshed := filepath.Join(filepath.Dir(snapshot), "refreshed.git")
	if err := prepare(httpsURL, refreshed); err != nil {
		t.Fatal(err)
	}
	if gitIn(t, refreshed, "rev-parse", "refs/heads/new-default") != gitIn(t, source, "rev-parse", "HEAD") {
		t.Fatal("the refreshed snapshot missed the new upstream state")
	}
	if gitIn(t, snapshot, "rev-parse", "HEAD") != initial {
		t.Fatal("refreshing the mirror changed an existing snapshot")
	}
}

// Hard-won: 020257d9f: destinations sharing a remote overwrote each other's cutoff snapshots.
func TestACutoffSnapshotIsABundleOfTheCutoffHistoryAlone(t *testing.T) {
	temporary := t.TempDir()
	source := filepath.Join(temporary, "source")
	cacheRoot := filepath.Join(temporary, cacheRelativeRoot)
	mkdirAll(t, source)
	gitIn(t, source, "init", "--quiet", "--initial-branch=main")
	gitIn(t, source, "config", "user.name", "Hel Test")
	gitIn(t, source, "config", "user.email", "hel@example.test")
	commit := func(message string) string {
		if err := os.WriteFile(filepath.Join(source, "README.md"), []byte(message+"\n"), 0o666); err != nil {
			t.Fatal(err)
		}
		gitIn(t, source, "add", "README.md")
		gitIn(t, source, "commit", "--quiet", "-m", message)
		return gitIn(t, source, "rev-parse", "HEAD")
	}
	commit("initial")
	gitIn(t, source, "switch", "--quiet", "-c", "trunk")
	base := commit("base")
	gitIn(t, source, "switch", "--quiet", "main")
	head := commit("head")
	later := commit("later")
	gitIn(t, source, "tag", "-a", "v-later", "-m", "later")
	cutoff := &core.HistoryCutoff{Head: head, Base: &base}

	remote := "git://127.0.0.1:9418/repository"
	config := filepath.Join(temporary, "gitconfig")
	gitIn(t, source, "config", "--file", config, "url."+source+".insteadOf", remote)
	executor := gitExecutor(map[string]string{
		"HOME":                temporary,
		"GIT_CONFIG_GLOBAL":   config,
		"GIT_CONFIG_NOSYSTEM": "1",
	})
	target := &core.TargetsTargetTemplate{
		Kind:      core.TargetsTargetTemplateLocalPodman,
		Container: &core.TargetsContainerTemplate{Image: "ubuntu:24.04", PullPolicy: core.ImagePullPolicyAuto},
	}
	repository := func(destination string, historyCutoff *core.HistoryCutoff) core.RepositorySpec {
		url := remote
		return core.RepositorySpec{URL: &url, Destination: destination, HistoryCutoff: historyCutoff}
	}
	bundle := &core.ProjectBundleSpec{
		Primary: "pinned",
		Repositories: []core.RepositorySpec{
			repository("pinned", cutoff),
			repository("other-pin", &core.HistoryCutoff{Head: base}),
			repository("full", nil),
		},
	}
	var mounts []core.AdditionalMount
	prepared, err := Prepare(target, "session-12345678", bundle, &mounts, nil, nil, executor)
	if err != nil {
		t.Fatal(err)
	}
	if prepared == nil {
		t.Fatal("the clone cache was prepared")
	}
	if len(mounts) != 1 {
		t.Fatalf("mounts = %v", mounts)
	}
	var snapshots []string
	for _, repository := range bundle.Repositories {
		reference := *repository.Reference
		relative, ok := pathStripPrefix(reference, mounts[0].Destination)
		if !ok {
			t.Fatal(reference)
		}
		snapshots = append(snapshots, filepath.Join(mounts[0].Source, relative))
	}
	snapshot, other, full := snapshots[0], snapshots[1], snapshots[2]
	if snapshot == other {
		t.Fatal("two cutoffs of one remote share a snapshot")
	}
	entries, err := os.ReadDir(mounts[0].Source)
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 3 {
		t.Fatalf("entries = %v", entries)
	}
	for _, check := range []struct{ path, refs string }{
		{snapshot, head + " refs/mj/head\n" + base + " refs/mj/base"},
		{other, base + " refs/mj/head"},
	} {
		if got := gitIn(t, temporary, "bundle", "list-heads", check.path); got != check.refs {
			t.Fatalf("got %q, want %q", got, check.refs)
		}
	}
	if gitIn(t, full, "rev-parse", "HEAD") != later {
		t.Fatal("the full snapshot is not at the latest commit")
	}
	key := repositoryCacheKey(remote)
	mirror := filepath.Join(cacheRoot, "mirrors", key, "repo.git")
	gitIn(t, mirror, "cat-file", "-e", later)

	checkout := filepath.Join(temporary, "checkout")
	gitIn(t, temporary, "init", "--quiet", checkout)
	gitIn(t, checkout, "fetch", "--quiet", "--no-tags", "--", snapshot, head)
	present := strings.Split(gitIn(t, checkout, "cat-file", "--batch-all-objects", "--batch-check=%(objectname)"), "\n")
	slices.Sort(present)
	var reachable []string
	for _, line := range strings.Split(gitIn(t, checkout, "rev-list", "--objects", head, base), "\n") {
		reachable = append(reachable, strings.SplitN(line, " ", 2)[0])
	}
	slices.Sort(reachable)
	reachable = slices.Compact(reachable)
	// Fetching the head alone stored the whole bundle, base included.
	if !slices.Equal(present, reachable) {
		t.Fatalf("present %q, reachable %q", present, reachable)
	}
	if slices.Contains(present, later) {
		t.Fatal("later history entered the cutoff snapshot")
	}

	// A warm host-only mirror also serves a detached HEAD outside refs/*.
	gitIn(t, source, "switch", "--quiet", "--detach")
	detached := commit("unreferenced detached HEAD")
	refreshed := filepath.Join(filepath.Dir(full), "detached.git")
	if err := prepareRepository(CacheHost{}, cacheRoot, key, remote, refreshed, nil, nil, nil, executor); err != nil {
		t.Fatal(err)
	}
	if gitIn(t, refreshed, "rev-parse", "HEAD") != detached {
		t.Fatal("the snapshot missed the detached HEAD")
	}

	// A cutoff the host cannot fetch fails before anything is mounted.
	missing := &core.HistoryCutoff{Head: strings.Repeat("1", 40)}
	if err := prepareRepository(CacheHost{}, cacheRoot, key, remote, snapshot, missing, nil, nil, executor); err == nil {
		t.Fatal("an unfetchable cutoff succeeded")
	}
	if exists(snapshot) {
		t.Fatal("a failed cutoff left its snapshot")
	}
}

// The clone cache runs Git on the controller or on a container target's SSH
// host with the SSH command the controller decided, so the operator's
// `GIT_SSH_COMMAND` governs it as it does every other Git command.
// Hard-won: #1466: the clone cache forced accept-new over the operator's GIT_SSH_COMMAND
func TestTheCloneCacheReachesSshRemotesWithTheControllersSshCommand(t *testing.T) {
	temporary := t.TempDir()
	bin := filepath.Join(temporary, "bin")
	mkdirAll(t, bin)
	log := filepath.Join(temporary, "ssh-arguments")
	writeExecutable(t, filepath.Join(bin, "ssh"), "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$SSH_TEST_LOG\"\nexit 255\n")
	for _, check := range []struct{ sshCommand, chosen string }{
		{core.NonInteractiveGitSSHCommand, "StrictHostKeyChecking=accept-new"},
		{"ssh -o StrictHostKeyChecking=yes", "StrictHostKeyChecking=yes"},
	} {
		os.Remove(log)
		command := CacheHost{}.gitShellCommand(prepareRepositoryScript, []string{
			filepath.Join(temporary, "cache"),
			"repository",
			"ssh://git.example.invalid/repository.git",
			filepath.Join(temporary, "cache/sessions/one/repository.bundle"),
			"0",
			check.sshCommand,
			"ssh-fixture-source",
			"0",
			"",
		}, "prepare cached Git objects")
		command.Env = map[string]string{
			"PATH":         bin + ":" + os.Getenv("PATH"),
			"SSH_TEST_LOG": log,
		}
		result := mustRun(t, command)
		if result.Status == 0 {
			t.Fatal("the fake SSH refuses every connection")
		}
		arguments, err := os.ReadFile(log)
		if err != nil {
			t.Fatalf("%s: SSH never ran: %v", check.sshCommand, err)
		}
		options := strings.Split(strings.TrimRight(string(arguments), "\n"), "\n")
		if !slices.Contains(options, check.chosen) {
			t.Fatalf("%s: %s", check.sshCommand, arguments)
		}
		count := 0
		for _, option := range options {
			if strings.HasPrefix(option, "StrictHostKeyChecking=") {
				count++
			}
		}
		if count != 1 {
			t.Fatalf("%s: %s", check.sshCommand, arguments)
		}
	}
}

// Hard-won: #1388: packing one session's history blocked every clone of the same repository.
// Hard-won: 1bd051d40: a mkdir lock was acquired twice under uutils.
func TestConcurrentSnapshotsShareARefreshAndSurviveMirrorEviction(t *testing.T) {
	temporary := t.TempDir()
	root := temporary
	source := filepath.Join(root, "source with spaces")
	cache := filepath.Join(root, "cache")
	wrappers := filepath.Join(root, "bin")
	mkdirAll(t, source, wrappers)
	gitIn(t, source, "init", "--quiet", "--initial-branch=main")
	gitIn(t, source, "config", "user.name", "Cache Test")
	gitIn(t, source, "config", "user.email", "cache@example.test")
	if err := os.WriteFile(filepath.Join(source, "data"), []byte(strings.Repeat("a", 128*1024)), 0o666); err != nil {
		t.Fatal(err)
	}
	gitIn(t, source, "add", "data")
	gitIn(t, source, "commit", "--quiet", "-m", "cutoff")
	head := gitIn(t, source, "rev-parse", "HEAD")
	if err := os.WriteFile(filepath.Join(source, "data"), []byte("later history"), 0o666); err != nil {
		t.Fatal(err)
	}
	gitIn(t, source, "commit", "--quiet", "-am", "later")
	later := gitIn(t, source, "rev-parse", "HEAD")
	cutoff := &core.HistoryCutoff{Head: head}

	executable := func(name string) string {
		command := core.NewCommandSpec("sh", []string{"-c", "command -v " + name})
		result, err := checked(mustRun(t, command), &command)
		if err != nil {
			t.Fatal(err)
		}
		return strings.TrimSpace(string(result.Stdout))
	}
	realGit := executable("git")
	realMkdir := executable("mkdir")
	realSleep := executable("sleep")
	gitWrapper := `#!/bin/sh
set -eu
wait_for() {
    attempts=0
    while [ ! -f "$1" ]; do
        attempts=$((attempts + 1))
        if [ "$attempts" -ge 1200 ]; then
            echo "cache regression gate timed out: $1" >&2
            exit 99
        fi
        sleep 0.05
    done
}
if { [ "$1" = clone ] && [ "$2" = --mirror ] && [ "$3" = -- ]; } ||
   { [ "$1" = -C ] && [ "$3" = fetch ] && [ "$4" = --prune ]; }; then
    echo refresh >> "$CACHE_TEST_ROOT/refreshes"
    : > "$CACHE_TEST_ROOT/refresh-started"
    wait_for "$CACHE_TEST_ROOT/release-refresh"
fi
if [ "$1" = -C ] && [ "$3" = fetch ] && [ "$4" != --prune ]; then
    if [ "${CACHE_TEST_PAUSE_FETCH:-0}" = 1 ]; then
        : > "$CACHE_TEST_ROOT/snapshot-pinned"
        wait_for "$CACHE_TEST_ROOT/release-snapshot"
    fi
    if [ "${CACHE_TEST_FAIL_FETCH:-0}" = 1 ]; then
        echo 'injected cutoff failure' >&2
        exit 42
    fi
fi
exec "$CACHE_TEST_GIT" "$@"
`
	mkdirWrapper := `#!/bin/sh
set -eu
if [ "$#" -eq 1 ] && [ "$1" = "$CACHE_TEST_ROOT/cache/locks/repository" ] &&
   [ "${CACHE_TEST_WAITER:-0}" = 1 ] && [ -d "$1" ]; then
    # Simulate uutils treating a competing mkdir's EEXIST as success.
    : > "$CACHE_TEST_ROOT/waiter-started"
    exit 0
fi
exec "$CACHE_TEST_MKDIR" "$@"
`
	sleepWrapper := `#!/bin/sh
set -eu
if [ "${CACHE_TEST_WAITER:-0}" = 1 ] && [ "$1" = "0.2" ]; then
    : > "$CACHE_TEST_ROOT/waiter-started"
fi
exec "$CACHE_TEST_SLEEP" "$@"
`
	for _, wrapper := range []struct{ name, script string }{
		{"git", gitWrapper},
		{"mkdir", mkdirWrapper},
		{"sleep", sleepWrapper},
	} {
		writeExecutable(t, filepath.Join(wrappers, wrapper.name), wrapper.script)
	}
	environment := map[string]string{
		"PATH":             wrappers + ":" + os.Getenv("PATH"),
		"CACHE_TEST_ROOT":  root,
		"CACHE_TEST_GIT":   realGit,
		"CACHE_TEST_MKDIR": realMkdir,
		"CACHE_TEST_SLEEP": realSleep,
	}
	first := gitExecutor(environmentWith(environment, map[string]string{"CACHE_TEST_PAUSE_FETCH": "1"}))
	second := gitExecutor(environmentWith(environment, map[string]string{"CACHE_TEST_WAITER": "1"}))
	snapshotA := filepath.Join(cache, "sessions/first/repository.bundle")
	snapshotB := filepath.Join(cache, "sessions/second/repository.bundle")
	prepare := func(snapshot string, executor core.CommandExecutor) error {
		return prepareRepository(CacheHost{}, cache, "repository", source, snapshot, cutoff, nil, nil, executor)
	}
	waitFor := func(name string) {
		t.Helper()
		marker := filepath.Join(root, name)
		deadline := time.Now().Add(20 * time.Second)
		for !exists(marker) && time.Now().Before(deadline) {
			time.Sleep(10 * time.Millisecond)
		}
		if !exists(marker) {
			t.Fatalf("missing regression marker: %s", name)
		}
	}

	aDone := make(chan error, 1)
	go func() { aDone <- prepare(snapshotA, first) }()
	waitFor("refresh-started")
	bDone := make(chan error, 1)
	go func() { bDone <- prepare(snapshotB, second) }()
	waitFor("waiter-started")
	// The old implementation abandons this accepted wait after five seconds.
	var completed error
	early := false
	select {
	case completed = <-bDone:
		early = true
	case <-time.After(6 * time.Second):
	}
	if err := os.WriteFile(filepath.Join(root, "release-refresh"), nil, 0o666); err != nil {
		t.Fatal(err)
	}
	waitFor("snapshot-pinned")
	mounted, err := os.ReadDir(filepath.Dir(snapshotA))
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range mounted {
		if !strings.HasPrefix(entry.Name(), "repository.bundle.cutoff-") {
			t.Fatalf("a full-history pin entered the mounted session subtree: %v", mounted)
		}
	}
	if !early {
		select {
		case completed = <-bDone:
		case <-time.After(20 * time.Second):
			t.Fatal("the waiter did not finish")
		}
	}
	// A private snapshot must own its files even if the shared mirror is
	// evicted.
	if err := collectGarbageWithLimit(CacheHost{}, cache, []string{"first", "second"}, 0, core.ProcessExecutor{}); err != nil {
		t.Fatal(err)
	}
	evicted := !exists(filepath.Join(cache, "mirrors/repository"))
	if err := os.WriteFile(filepath.Join(root, "release-snapshot"), nil, 0o666); err != nil {
		t.Fatal(err)
	}
	firstResult := <-aDone
	if completed != nil {
		t.Fatal(completed)
	}
	if firstResult != nil {
		t.Fatal(firstResult)
	}
	if !evicted {
		t.Fatal("the shared mirror was not evicted while the reader was pinned")
	}
	refreshes := strings.Split(strings.TrimRight(readText(t, filepath.Join(root, "refreshes")), "\n"), "\n")
	if len(refreshes) != 1 {
		t.Fatalf("refreshes = %q", refreshes)
	}
	if mirrors, err := os.ReadDir(filepath.Join(cache, "mirrors")); err != nil || len(mirrors) != 0 {
		t.Fatalf("mirrors = %v, %v", mirrors, err)
	}
	for _, snapshot := range []string{snapshotA, snapshotB} {
		if got := gitIn(t, root, "bundle", "list-heads", snapshot); got != head+" refs/mj/head" {
			t.Fatal(got)
		}
		checkout := strings.TrimSuffix(snapshot, filepath.Ext(snapshot)) + ".checkout"
		gitIn(t, root, "init", "--quiet", "--bare", checkout)
		gitIn(t, checkout, "fetch", "--quiet", "--", snapshot, head)
		if result := mustRun(t, core.NewCommandSpec("git", []string{"-C", checkout, "cat-file", "-e", later})); result.Status == 0 {
			t.Fatal("later history entered the snapshot")
		}
		entries, err := os.ReadDir(filepath.Dir(snapshot))
		if err != nil {
			t.Fatal(err)
		}
		for _, entry := range entries {
			name := entry.Name()
			if strings.Contains(name, ".pinned-") || strings.Contains(name, ".cutoff-") || strings.Contains(name, ".partial-") {
				t.Fatalf("leftover %s", name)
			}
		}
	}

	failing := gitExecutor(environmentWith(environment, map[string]string{"CACHE_TEST_FAIL_FETCH": "1"}))
	failed := filepath.Join(cache, "sessions/failed/repository.bundle")
	err = prepare(failed, failing)
	if err == nil || !strings.Contains(err.Error(), "injected cutoff failure") {
		t.Fatalf("error = %v", err)
	}
	if entries, err := os.ReadDir(filepath.Dir(failed)); err != nil || len(entries) != 0 {
		t.Fatalf("entries = %v, %v", entries, err)
	}
	if exists(filepath.Join(cache, "locks/repository")) {
		t.Fatal("the failed preparation left its lock")
	}
	mirrors, err := os.ReadDir(filepath.Join(cache, "mirrors"))
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range mirrors {
		if entry.Name() != "repository" {
			t.Fatalf("mirror entry %s", entry.Name())
		}
	}
}
