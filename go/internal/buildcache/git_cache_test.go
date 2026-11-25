// Port of the tests in mj-build-cache/src/git_cache.rs. The Unix-only ones
// are in git_cache_unix_test.go.

package buildcache

import (
	"encoding/json"
	"fmt"
	"maps"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// recordingExecutor answers every command alike and records the commands
// and notices.
type recordingExecutor struct {
	mu       sync.Mutex
	commands []core.CommandSpec
	notices  []string
	status   int
	stdout   []byte
}

func (r *recordingExecutor) executor() *funcExecutor {
	return &funcExecutor{
		execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
			r.mu.Lock()
			defer r.mu.Unlock()
			r.commands = append(r.commands, *command)
			return core.CommandOutput{Status: r.status, Stdout: slices.Clone(r.stdout), Stderr: []byte("unavailable")}, nil
		},
		notice: func(notice string) {
			r.mu.Lock()
			defer r.mu.Unlock()
			r.notices = append(r.notices, notice)
		},
	}
}

// gitExecutor runs commands as local processes with extra environment, and
// reports no Podman containers.
func gitExecutor(environment map[string]string) *funcExecutor {
	process := processWithEnv(environment)
	return &funcExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		if command.Program == "podman" {
			return output(0, "[]", ""), nil
		}
		return process.Execute(command)
	}}
}

func podmanFixtureBundle(source string) (*core.TargetsTargetTemplate, *core.ProjectBundleSpec) {
	target := &core.TargetsTargetTemplate{
		Kind:      core.TargetsTargetTemplateLocalPodman,
		Container: &core.TargetsContainerTemplate{Image: "ubuntu:24.04", PullPolicy: core.ImagePullPolicyAuto},
	}
	bundle := &core.ProjectBundleSpec{
		Primary: "app",
		Repositories: []core.RepositorySpec{{
			URL:         &source,
			Destination: "app",
		}},
	}
	return target, bundle
}

func TestUnavailableHostGitReportsTheSourceWithoutMutatingTheBundle(t *testing.T) {
	recording := &recordingExecutor{status: 127}
	source := "git://127.0.0.2/app.git"
	target, bundle := podmanFixtureBundle(source)
	var mounts []core.AdditionalMount

	_, err := Prepare(target, "session-12345678", bundle, &mounts, nil, nil, recording.executor())
	if err == nil {
		t.Fatal("a host-only source without host Git is refused")
	}
	refusal, ok := core.RefusalOf(err)
	if !ok || !strings.Contains(refusal.Message(), source) {
		t.Fatalf("error = %v", err)
	}
	if bundle.Repositories[0].Reference != nil {
		t.Fatal(*bundle.Repositories[0].Reference)
	}
	if len(mounts) != 0 {
		t.Fatalf("mounts = %v", mounts)
	}
}

func TestUnavailableHostGitLeavesAGithubBundleToCloneDirectly(t *testing.T) {
	recording := &recordingExecutor{status: 127}
	source := "https://github.com/example/app.git"
	target, bundle := podmanFixtureBundle(source)
	sessionID := "session-12345678"
	var mounts []core.AdditionalMount
	prepared, err := Prepare(target, sessionID, bundle, &mounts, nil, nil, recording.executor())
	if err != nil || prepared != nil {
		t.Fatalf("prepared = %v, %v", prepared, err)
	}
	if len(mounts) != 0 {
		t.Fatalf("mounts = %v", mounts)
	}
	if bundle.Repositories[0].Reference != nil {
		t.Fatal(*bundle.Repositories[0].Reference)
	}
	if !strings.Contains(recording.notices[0], "using direct clones") {
		t.Fatal(recording.notices[0])
	}
	plan, err := targets.ProvisionPlan(target, sessionID, bundle, mounts, nil)
	if err != nil {
		t.Fatal(err)
	}
	index := slices.IndexFunc(plan.Commands, func(command core.CommandSpec) bool { return command.Purpose == "clone app" })
	if index < 0 {
		t.Fatal("no clone command")
	}
	clone := plan.Commands[index]
	if !slices.Contains(clone.Args, source) {
		t.Fatalf("%q", clone.Args)
	}
	if slices.ContainsFunc(clone.Args, func(argument string) bool { return strings.HasPrefix(argument, "--reference") }) {
		t.Fatalf("%q", clone.Args)
	}
}

// Rust also checks that the Debug form says `<redacted>`; Go's fmt renders
// the unexported input as an address, so the check here is that the token
// travels as sensitive stdin instead.
func TestCacheTokenNeverEntersArgumentsOrDebugOutput(t *testing.T) {
	recording := &recordingExecutor{status: 1}
	token := "github-token-that-must-stay-private"
	_ = prepareRepository(
		CacheHost{},
		"/home/test/.cache/mjolnir/git",
		"abc123",
		"https://github.com/example/app.git",
		"/home/test/.cache/mjolnir/git/sessions/session-12345678/abc123.git",
		nil,
		nil,
		&token,
		recording.executor(),
	)

	command := recording.commands[len(recording.commands)-1]
	if slices.ContainsFunc(command.Args, func(argument string) bool { return strings.Contains(argument, token) }) {
		t.Fatalf("%q", command.Args)
	}
	for _, format := range []string{"%v", "%+v", "%#v"} {
		if strings.Contains(fmt.Sprintf(format, command), token) {
			t.Fatalf("%s rendering leaks the token", format)
		}
	}
	if input, ok := command.SensitiveStdin(); !ok || !strings.Contains(string(input), token) {
		t.Fatal("the token is not carried as sensitive stdin")
	}
	encoded, err := json.Marshal(command)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(encoded), token) {
		t.Fatal(string(encoded))
	}
}

func TestDockerCacheDiscoversManagedSessionsThroughDockerJsonLines(t *testing.T) {
	sessionID := "0123456789abcdef0123456789abcdef"
	recording := &recordingExecutor{stdout: []byte(fmt.Sprintf(
		"{\"Labels\":\"%s=true,%s=%s\"}\n", core.ManagedLabel, core.SessionLabel, sessionID))}
	target := &core.TargetsTargetTemplate{
		Kind:      core.TargetsTargetTemplateLocalDocker,
		Container: &core.TargetsContainerTemplate{Image: "ubuntu:24.04", PullPolicy: core.ImagePullPolicyAuto},
	}

	host, ok := CacheHostForTarget(target)
	if !ok {
		t.Fatal("Docker has a clone-cache host")
	}
	sessions, err := host.managedSessions("docker", recording.executor())
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(sessions, []string{sessionID}) {
		t.Fatalf("%q", sessions)
	}

	if len(recording.commands) != 1 {
		t.Fatalf("commands = %d", len(recording.commands))
	}
	if recording.commands[0].Program != "docker" {
		t.Fatal(recording.commands[0].Program)
	}
	want := []string{"ps", "--all", "--filter", "label=dev.mj.managed=true", "--format", "json"}
	if !slices.Equal(recording.commands[0].Args, want) {
		t.Fatalf("%q", recording.commands[0].Args)
	}
}

// gitIn runs git in directory, failing the test on a non-zero exit, and is
// its trimmed stdout.
func gitIn(t *testing.T, directory string, args ...string) string {
	t.Helper()
	command := core.NewCommandSpec("git", append([]string{"-C", directory}, args...)).WithPurpose("fixture Git")
	result, err := checked(mustRun(t, command), &command)
	if err != nil {
		t.Fatal(err)
	}
	return strings.TrimSpace(string(result.Stdout))
}

func directoryKib(t *testing.T, path string) uint64 {
	t.Helper()
	command := core.NewCommandSpec("du", []string{"-sk", path}).WithPurpose("measure test clone-cache entry")
	result, err := checked(mustRun(t, command), &command)
	if err != nil {
		t.Fatal(err)
	}
	size, err := strconv.ParseUint(strings.Fields(string(result.Stdout))[0], 10, 64)
	if err != nil {
		t.Fatal(err)
	}
	return size
}

// Hard-won: 932a8969f: interrupted GC left its eviction lock.
// Hard-won: 13c4fc559: interrupted eviction left a partial mirror.
func TestCacheGcEvictsTheOldestMirrorToMeetItsSoftCap(t *testing.T) {
	skipOnWindows(t, "#1297: runs sh, du or touch on the controller")
	temporary := t.TempDir()
	cacheRoot := filepath.Join(temporary, "cache")
	mirrors := filepath.Join(cacheRoot, "mirrors")
	old := filepath.Join(mirrors, "old")
	recent := filepath.Join(mirrors, "recent")
	mkdirAll(t, old)
	if err := os.WriteFile(filepath.Join(old, "objects"), []byte(strings.Repeat("o", 16*1024)), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(old, "last-used"), nil, 0o666); err != nil {
		t.Fatal(err)
	}
	time.Sleep(time.Second)
	mkdirAll(t, recent)
	if err := os.WriteFile(filepath.Join(recent, "objects"), []byte(strings.Repeat("n", 16*1024)), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(recent, "last-used"), nil, 0o666); err != nil {
		t.Fatal(err)
	}
	// `du` counts allocated blocks, which FUSE or compressed development
	// mounts can report far below the written bytes, so a fixed offset from
	// one entry may never exceed the soft cap. Place the cap between "both
	// mirrors" and "only the newest mirror", measured with the same du the
	// garbage collector uses.
	total := directoryKib(t, mirrors)
	oldest := directoryKib(t, old)
	if oldest == 0 {
		t.Fatal("du must see the older mirror's blocks")
	}
	limit := total - (oldest+1)/2

	if err := collectGarbageWithLimit(CacheHost{}, cacheRoot, nil, limit, core.ProcessExecutor{}); err != nil {
		t.Fatal(err)
	}

	if exists(old) {
		t.Fatal("the oldest mirror was not evicted")
	}
	if !exists(recent) {
		t.Fatal("the newest mirror was evicted")
	}

	if runtime.GOOS == "windows" {
		return
	}
	// Interrupt GC after deletion removes Git metadata from a mirror.
	if err := os.Remove(filepath.Join(cacheRoot, "last-gc")); err != nil {
		t.Fatal(err)
	}
	source := filepath.Join(temporary, "source")
	for _, repository := range []string{source, filepath.Join(recent, "repo.git")} {
		git := core.NewCommandSpec("git", []string{"init", "--bare", repository})
		if _, err := checked(mustRun(t, git), &git); err != nil {
			t.Fatal(err)
		}
	}
	wrappers := filepath.Join(temporary, "bin")
	mkdirAll(t, wrappers)
	evictionLock := filepath.Join(cacheRoot, "locks/recent")
	core.InstallFakeCommand(wrappers, "rm", `#!/bin/sh
set -eu
if [ "$1" = -rf ] && [ -d "${3:-}" ]; then
    case "$3" in
        "$CACHE_TEST_EVICTION") entry=$3 ;;
        "$CACHE_TEST_MIRRORS"/.trash-*)
            test ! -e "$CACHE_TEST_EVICTION_LOCK"
            entry="$3/mirror"
            ;;
        *) exec /bin/rm "$@" ;;
    esac
    /bin/rm -- "$entry/repo.git/HEAD"
    kill -TERM "$PPID"
    exit 0
fi
exec /bin/rm "$@"
`)
	executor := gitExecutor(map[string]string{
		"PATH":                     wrappers + ":" + os.Getenv("PATH"),
		"CACHE_TEST_EVICTION":      recent,
		"CACHE_TEST_MIRRORS":       mirrors,
		"CACHE_TEST_EVICTION_LOCK": evictionLock,
	})
	err := collectGarbageWithLimit(CacheHost{}, cacheRoot, nil, 0, executor)
	if err == nil || !strings.Contains(err.Error(), "status 143") {
		t.Fatalf("error = %v", err)
	}
	if exists(recent) {
		t.Fatal("an interrupted delete left a published mirror")
	}
	entries, err := os.ReadDir(mirrors)
	if err != nil {
		t.Fatal(err)
	}
	index := slices.IndexFunc(entries, func(entry os.DirEntry) bool { return strings.HasPrefix(entry.Name(), ".trash-") })
	if index < 0 {
		t.Fatal("no trash directory")
	}
	trash := filepath.Join(mirrors, entries[index].Name())
	if info, err := os.Stat(filepath.Join(trash, "mirror/repo.git")); err != nil || !info.IsDir() {
		t.Fatal("the trash lost the mirror")
	}
	if exists(filepath.Join(trash, "mirror/repo.git/HEAD")) {
		t.Fatal("the interrupted delete did not start")
	}
	if exists(evictionLock) {
		t.Fatal("the eviction lock remains")
	}
	if exists(filepath.Join(cacheRoot, "locks/gc")) {
		t.Fatal("the GC lock remains")
	}
	rootEntries, err := os.ReadDir(cacheRoot)
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range rootEntries {
		if strings.HasPrefix(entry.Name(), "gc-candidates-") {
			t.Fatalf("%s remains", entry.Name())
		}
	}

	if err := os.Remove(filepath.Join(wrappers, "rm")); err != nil {
		t.Fatal(err)
	}
	core.InstallFakeCommand(wrappers, "sleep",
		"#!/bin/sh\necho 'preparation unexpectedly waited for the eviction lock' >&2\nexit 99\n")
	snapshot := filepath.Join(cacheRoot, "sessions/new/recent.git")
	if err := prepareRepository(CacheHost{}, cacheRoot, "recent", source, snapshot, nil, nil, nil, executor); err != nil {
		t.Fatal(err)
	}
	if info, err := os.Stat(filepath.Join(snapshot, "HEAD")); err != nil || !info.Mode().IsRegular() {
		t.Fatal("the snapshot has no HEAD")
	}
	if info, err := os.Stat(filepath.Join(recent, "repo.git/HEAD")); err != nil || !info.Mode().IsRegular() {
		t.Fatal("the mirror was not republished")
	}
	if exists(evictionLock) {
		t.Fatal("the eviction lock remains")
	}

	if err := collectGarbage(CacheHost{}, cacheRoot, nil, core.ProcessExecutor{}); err != nil {
		t.Fatal(err)
	}
	if exists(trash) {
		t.Fatal("the next GC did not reap the interrupted eviction")
	}
	if !exists(recent) {
		t.Fatal("the republished mirror was evicted")
	}
}

func TestCacheGcRemovesOnlyOldOrphanSessionSnapshots(t *testing.T) {
	skipOnWindows(t, "#1297: runs sh, du or touch on the controller")
	temporary := t.TempDir()
	cacheRoot := filepath.Join(temporary, "cache")
	orphan := filepath.Join(cacheRoot, "sessions/orphan-session")
	live := filepath.Join(cacheRoot, "sessions/live-session")
	starting := filepath.Join(cacheRoot, "sessions/starting-session")
	mkdirAll(t, orphan, live)
	// A just-prepared snapshot whose container does not exist yet.
	inherited := filepath.Join(starting, "repository.git/objects/pack/old.pack")
	mkdirAll(t, filepath.Dir(inherited))
	if err := os.WriteFile(inherited, nil, 0o666); err != nil {
		t.Fatal(err)
	}
	for _, path := range []string{orphan, live, inherited} {
		command := core.NewCommandSpec("touch", []string{"-t", "200001010000", path}).WithPurpose("age test clone-cache snapshot")
		if _, err := checked(mustRun(t, command), &command); err != nil {
			t.Fatal(err)
		}
	}

	if err := collectGarbageWithLimit(CacheHost{}, cacheRoot, []string{"live-session"}, cacheMaxKib, core.ProcessExecutor{}); err != nil {
		t.Fatal(err)
	}

	if exists(orphan) {
		t.Fatal("an old orphan snapshot remains")
	}
	if !exists(live) {
		t.Fatal("a live session's snapshot was removed")
	}
	// Hard-won: #1567: a just-prepared snapshot with no container yet was
	// collected because its hardlinked mirror objects carry old times.
	if !exists(inherited) {
		t.Fatal("a just-prepared snapshot was collected")
	}
}

// environmentWith is a copy of environment with more variables.
func environmentWith(environment map[string]string, extra map[string]string) map[string]string {
	result := maps.Clone(environment)
	maps.Copy(result, extra)
	return result
}
