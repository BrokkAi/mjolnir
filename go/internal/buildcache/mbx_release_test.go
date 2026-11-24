//go:build unix

// Port of the tests in mj-build-cache/src/mbx/release.rs
// (`#[cfg(all(test, unix))]`), with its test-only BuildStateRelease::for_target.

package buildcache

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// buildStateReleaseForTarget is Rust's test-only
// BuildStateRelease::for_target: the release of a session's own target.
func buildStateReleaseForTarget(session *core.SessionRecord, backend *core.TargetsTargetLocator, config *core.Config) *BuildStateRelease {
	placement := store.EnvironmentPlacementFromSession(session)
	return BuildStateReleaseForEnvironment(session.ID, &placement, backend, config)
}

// releaseSandbox is a directory with a test-only `mbx` that logs each
// `clean` or rejects execution, plus a home with nothing in it.
type releaseSandbox struct {
	t    *testing.T
	root string
}

func newReleaseSandbox(t *testing.T, withMbx bool) *releaseSandbox {
	return newReleaseSandboxWithUnderSupport(t, withMbx, false)
}

func newReleaseSandboxWithUnderSupport(t *testing.T, withMbx, supportsUnder bool) *releaseSandbox {
	t.Helper()
	sandbox := &releaseSandbox{t: t, root: t.TempDir()}
	mkdirAll(t, sandbox.path("home"), sandbox.path("bin"))
	contents := "#!/bin/sh\nexit 1\n"
	if withMbx {
		usage := "echo 'Usage: mbx clean WORKSPACE'"
		if supportsUnder {
			usage = "echo 'Usage: mbx clean [--under ROOT] [WORKSPACE]'"
		}
		contents = fmt.Sprintf("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'mbx %s'; exit 0; fi\n"+
			"if [ \"$1\" = clean ] && [ \"${2:-}\" = --help ]; then\n"+
			"%s\n"+
			"exit 0\n"+
			"fi\n"+
			"printf '%%s|%%s|%%s\\n' \"$*\" \"${MBX_CACHE_DIR:-}\" \"${XDG_CONFIG_HOME:-}\" >> '%s'\n",
			MbxVersion, usage, sandbox.path("log"))
	}
	writeExecutable(t, sandbox.path("bin/mbx"), contents)
	return sandbox
}

func (s *releaseSandbox) path(relative string) string { return filepath.Join(s.root, relative) }

// command prepares a release command with this sandbox's PATH and home.
func (s *releaseSandbox) command(release *BuildStateRelease) core.CommandSpec {
	command := release.command()
	if command.Env == nil {
		command.Env = map[string]string{}
	}
	command.Env["PATH"] = s.path("bin") + ":/usr/bin:/bin"
	command.Env["HOME"] = s.path("home")
	command.Env["MBX_CACHE_DIR"] = ""
	command.Env["XDG_CONFIG_HOME"] = ""
	return command
}

// run runs release with this sandbox's PATH and home.
func (s *releaseSandbox) run(release *BuildStateRelease) core.CommandOutput {
	return mustRun(s.t, s.command(release))
}

func (s *releaseSandbox) log() string {
	data, err := os.ReadFile(s.path("log"))
	if err != nil {
		return ""
	}
	return string(data)
}

func (s *releaseSandbox) installLegacyMbx(path string) {
	mkdirAll(s.t, filepath.Dir(path))
	writeExecutable(s.t, path, fmt.Sprintf("#!/bin/sh\n[ \"$1\" = --version ] && { echo 'mbx 1.19.0'; exit 0; }\n"+
		"printf '%%s|%%s|%%s\\n' \"$*\" \"${MBX_CACHE_DIR:-}\" \"${XDG_CONFIG_HOME:-}\" >> '%s'\n", s.path("log")))
}

func sharedRelease(workspaces ...string) *BuildStateRelease {
	return &BuildStateRelease{
		host:  CacheHost{},
		store: releaseStore{shared: true, directory: "/srv/cache"},
		// Exercise PATH and legacy fallbacks without ever probing a possibly
		// live cache binary on the test host.
		workspaces: workspaces,
	}
}

func requireStatus(t *testing.T, result core.CommandOutput, status int) {
	t.Helper()
	if result.Status != status {
		t.Fatalf("status %d, stdout %q, stderr %q", result.Status, result.Stdout, result.Stderr)
	}
}

func TestReleaseUsesItsOwnCleanupTimeout(t *testing.T) {
	restore := EnableForTest()
	defer restore()
	var recorded *time.Duration
	executor := &funcExecutor{
		execute: func(*core.CommandSpec) (core.CommandOutput, error) { return output(0, "released", ""), nil },
	}
	executor.cleanupWithTimeout = func(command *core.CommandSpec, timeout time.Duration) (core.CommandOutput, error) {
		recorded = &timeout
		return executor.Execute(command)
	}

	release := BuildStateReleaseManagedCheckout(nil, "/gone")
	release.Run(executor)

	if recorded == nil || *recorded != 120*time.Second {
		t.Fatalf("timeout = %v", recorded)
	}
}

func TestOnlyAnInterruptedCommandIsReportedAsNotConfirmed(t *testing.T) {
	if !cleanupWasInterrupted(errors.New("operation cancelled while release")) {
		t.Fatal("a cancelled command is interrupted")
	}
	if cleanupWasInterrupted(errors.New("run sh for release: No such file or directory")) {
		t.Fatal("a spawn failure is not an interruption")
	}
}

func TestARemovedBareCheckoutIsCleanedByItsPhysicalPath(t *testing.T) {
	sandbox := newReleaseSandbox(t, true)
	mkdirAll(t, sandbox.path("real/clones"))
	symlink(t, sandbox.path("real"), sandbox.path("link"))
	release := BuildStateReleaseManagedCheckout(nil, sandbox.path("link/clones/gone"))

	result := sandbox.run(&release)

	requireStatus(t, result, 0)
	physical := canonical(t, sandbox.path("real"))
	if got, want := sandbox.log(), fmt.Sprintf("clean %s/clones/gone||\n", physical); got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestAManagedCheckoutRootIsCleanedByItsPhysicalPath(t *testing.T) {
	sandbox := newReleaseSandboxWithUnderSupport(t, true, true)
	mkdirAll(t, sandbox.path("real/clones"))
	symlink(t, sandbox.path("real"), sandbox.path("link"))
	release := BuildStateReleaseManagedCheckout(nil, sandbox.path("link/clones/gone"))

	result := sandbox.run(&release)

	requireStatus(t, result, 0)
	physical := canonical(t, sandbox.path("real"))
	if got, want := sandbox.log(), fmt.Sprintf("clean --under %s/clones/gone||\n", physical); got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestReleaseScriptRecoversFromADeletedStartingDirectory(t *testing.T) {
	sandbox := newReleaseSandbox(t, true)
	release := BuildStateReleaseManagedCheckout(nil, sandbox.path("gone"))
	deletedCwd := sandbox.path("cwd/vanish")
	mkdirAll(t, deletedCwd)
	cwdLog := sandbox.path("cwd.log")
	writeExecutable(t, sandbox.path("bin/mbx"), fmt.Sprintf(
		"#!/bin/sh\nif [ \"$1\" = --version ]; then pwd >/dev/null 2>&1 || exit 1; echo 'mbx %s'; exit 0; fi\n"+
			"if [ \"$1\" = clean ] && [ \"$2\" = --help ]; then echo 'Usage: mbx clean WORKSPACE'; exit 0; fi\n"+
			"pwd >> '%s'\nprintf '%%s|%%s|%%s\\n' \"$*\" \"${MBX_CACHE_DIR:-}\" \"${XDG_CONFIG_HOME:-}\" >> '%s'\n",
		MbxVersion, cwdLog, sandbox.path("log")))

	command := sandbox.command(&release)
	originalArgs := slices.Clone(command.Args)
	command.Cwd = &deletedCwd
	command.Args = append([]string{
		"-c",
		`directory=$1; script=$2; shift 2; rmdir "$directory" || exit 1; exec sh -c "$script" "$@"`,
		"cwd-remover",
		deletedCwd,
		originalArgs[1],
	}, originalArgs[2:]...)

	result := mustRun(t, command)

	requireStatus(t, result, 0)
	cwd := strings.TrimRight(readText(t, cwdLog), "\n")
	if canonical(t, cwd) != canonical(t, sandbox.path("home")) {
		t.Fatalf("mbx ran in %s", cwd)
	}
}

func TestMbxOutputIsCapturedAfterCleanCompletes(t *testing.T) {
	sandbox := newReleaseSandbox(t, true)
	writeExecutable(t, sandbox.path("bin/mbx"), fmt.Sprintf(
		"#!/bin/sh\n[ \"$1\" = --version ] && { echo 'mbx %s'; exit 0; }\n"+
			"echo 'clean output'\necho 'clean warning' >&2\n", MbxVersion))

	result := sandbox.run(sharedRelease("/workspace/abc/app"))

	requireStatus(t, result, 0)
	if got := lossyTrim(result.Stdout); got != "clean output" {
		t.Fatal(got)
	}
	if got := lossyTrim(result.Stderr); got != "clean warning" {
		t.Fatal(got)
	}
}

// A wall-clock deadline cannot be ordered after `mbx clean` starts: under
// load the script spends longer than a short deadline on its probes, and the
// shell is killed before it launches mbx. Cancellation ends the wait through
// the same check as the deadline, so the release is cancelled only once the
// fake mbx reports that it started, and mbx finishes only after the release
// has stopped waiting for it.
func TestMbxCleanSurvivesItsInterruptedReleaseAndIsNotConfirmed(t *testing.T) {
	sandbox := newReleaseSandbox(t, true)
	mkdirAll(t, sandbox.path("tmp"))
	started := sandbox.path("started")
	proceed := sandbox.path("proceed")
	// The wait for `proceed` is bounded so a failed test leaves no process
	// behind.
	writeExecutable(t, sandbox.path("bin/mbx"), fmt.Sprintf(
		"#!/bin/sh\n[ \"$1\" = --version ] && { echo 'mbx %s'; exit 0; }\n"+
			": > '%s'\nwaited=0\n"+
			"while [ ! -e '%s' ] && [ \"$waited\" -lt 3000 ]; do sleep 0.02; waited=$((waited + 1)); done\n"+
			"printf done > '%s'/completed\nprintf 'mbx output\\n'\nprintf 'mbx warning\\n' >&2\n",
		MbxVersion, started, proceed, sandbox.path("tmp")))
	release := sharedRelease("/workspace/abc/app")
	command := sandbox.command(release)
	command.Env["TMPDIR"] = sandbox.path("tmp")

	var cancelled atomic.Bool
	cancellerDone := make(chan struct{})
	go func() {
		defer close(cancellerDone)
		// Stops early if the release returned without starting mbx.
		for !exists(started) && !cancelled.Load() {
			time.Sleep(10 * time.Millisecond)
		}
		cancelled.Store(true)
	}()
	_, err := core.NewCancellableProcessExecutor(&cancelled).ExecuteCleanupWithTimeout(&command, releaseTimeout)
	cancelled.Store(true)
	<-cancellerDone
	if writeErr := os.WriteFile(proceed, nil, 0o666); writeErr != nil {
		t.Fatal(writeErr)
	}

	if !exists(started) {
		t.Fatal("the release never started mbx clean")
	}
	if err == nil {
		t.Fatal("the interrupted release reported success")
	}
	if !cleanupWasInterrupted(err) {
		t.Fatal(alternate(err))
	}
	countTemporary := func() int {
		entries, err := os.ReadDir(sandbox.path("tmp"))
		if err != nil {
			t.Fatal(err)
		}
		return len(entries)
	}
	deadline := time.Now().Add(60 * time.Second)
	for (!exists(sandbox.path("tmp/completed")) || countTemporary() != 1) && time.Now().Before(deadline) {
		time.Sleep(20 * time.Millisecond)
	}
	if !exists(sandbox.path("tmp/completed")) {
		t.Fatal("mbx clean did not complete")
	}
	if countTemporary() != 1 {
		t.Fatal("the release left its output directory behind")
	}
}

// Hard-won: 741163fe: container workspace cleanup lost the cache mount policy and left stale build state
func TestAContainerWorkspaceIsCleanedAsWrittenWithTheSharedCachePolicy(t *testing.T) {
	sandbox := newReleaseSandbox(t, true)

	result := sandbox.run(sharedRelease("/workspace/abc/app", "/workspace/abc/lib"))

	requireStatus(t, result, 0)
	want := "clean /workspace/abc/app|/srv/cache|/srv/cache/.mjolnir/config\n" +
		"clean /workspace/abc/lib|/srv/cache|/srv/cache/.mjolnir/config\n"
	if got := sandbox.log(); got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestAContainerSessionReleasesEverythingUnderItsWorkspaceRoot(t *testing.T) {
	sandbox := newReleaseSandboxWithUnderSupport(t, true, true)
	session, backend := containerSession(t, "0123456789abcdef0123456789abcdef", nil)
	config := bundleConfig("app", "nested/lib")
	release := buildStateReleaseForTarget(&session, &backend, &config)
	if release == nil {
		t.Fatal("a container session with the cache has a release")
	}

	result := sandbox.run(release)

	requireStatus(t, result, 0)
	if got, want := sandbox.log(), fmt.Sprintf("clean --under %s|/srv/cache|/srv/cache/.mjolnir/config\n", *session.ContainerWorkspace); got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
	remediation := release.remediation()
	for _, expected := range []string{
		"mbx clean --under /workspace/0123456789abcdef0123456789abcdef",
		"older mbx: MBX_CACHE_DIR=",
		"mbx clean /workspace/0123456789abcdef0123456789abcdef/app",
	} {
		if !strings.Contains(remediation, expected) {
			t.Fatalf("%q lacks %q", remediation, expected)
		}
	}
}

func TestOlderMbxFallsBackToEachRepositoryWorkspace(t *testing.T) {
	sandbox := newReleaseSandbox(t, true)
	release := sharedRelease("/workspace/abc/app", "/workspace/abc/nested/lib")
	release.cleanupRoot = stringPointer("/workspace/abc")

	result := sandbox.run(release)

	requireStatus(t, result, 0)
	want := "clean /workspace/abc/app|/srv/cache|/srv/cache/.mjolnir/config\n" +
		"clean /workspace/abc/nested/lib|/srv/cache|/srv/cache/.mjolnir/config\n"
	if got := sandbox.log(); got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestSharedCacheCleanupPrefersTheRecordedHostMbxPath(t *testing.T) {
	sandbox := newReleaseSandbox(t, true)
	preferred := sandbox.path("preferred/mbx")
	mkdirAll(t, filepath.Dir(preferred))
	writeExecutable(t, preferred, fmt.Sprintf(
		"#!/bin/sh\n[ \"$1\" = --version ] && { echo 'mbx %s'; exit 0; }\n"+
			"printf '%%s|%%s|%%s\\n' \"$*\" \"${MBX_CACHE_DIR:-}\" \"${XDG_CONFIG_HOME:-}\" >> '%s'\n",
		MbxVersion, sandbox.path("preferred.log")))
	release := sharedRelease("/workspace/abc/app")
	release.preferredMbx = &preferred

	result := sandbox.run(release)

	requireStatus(t, result, 0)
	if got, want := readText(t, sandbox.path("preferred.log")), "clean /workspace/abc/app|/srv/cache|/srv/cache/.mjolnir/config\n"; got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
	if got := sandbox.log(); got != "" {
		t.Fatal(got)
	}
}

func TestLegacyLocalCleanupFindsAnyVersionedPrivateBinary(t *testing.T) {
	sandbox := newReleaseSandbox(t, false)
	legacyRoot := sandbox.path("data/mbx")
	sandbox.installLegacyMbx(filepath.Join(legacyRoot, "1.19.0/x86_64-unknown-linux-musl/mbx"))
	release := sharedRelease("/workspace/abc/app")
	release.legacyMbxRoot = &legacyRoot

	result := sandbox.run(release)

	requireStatus(t, result, 0)
	if got, want := sandbox.log(), "clean /workspace/abc/app|/srv/cache|/srv/cache/.mjolnir/config\n"; got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestSharedRemoteCleanupFindsLegacyDigestDirectoryCopies(t *testing.T) {
	sandbox := newReleaseSandbox(t, false)
	sandbox.installLegacyMbx(sandbox.path("home/.cache/mjolnir/mbx/sha256-old/mbx"))
	release := sharedRelease("/workspace/abc/app")

	result := sandbox.run(release)

	requireStatus(t, result, 0)
	if got, want := sandbox.log(), "clean /workspace/abc/app|/srv/cache|/srv/cache/.mjolnir/config\n"; got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestABareHostWithoutMbxHasNothingToRelease(t *testing.T) {
	sandbox := newReleaseSandbox(t, false)
	release := BuildStateReleaseManagedCheckout(nil, sandbox.path("gone"))

	result := sandbox.run(&release)

	requireStatus(t, result, 0)
	if got := lossyTrim(result.Stdout); got != absent {
		t.Fatal(got)
	}
}

func TestASharedCacheWithoutMbxIsAFailure(t *testing.T) {
	sandbox := newReleaseSandbox(t, false)

	result := sandbox.run(sharedRelease("/workspace/abc/app"))

	requireStatus(t, result, 3)
}

func bundleConfig(destinations ...string) core.Config {
	config := core.DefaultConfig()
	if config.Bundles == nil {
		config.Bundles = map[string]core.ProjectBundle{}
	}
	repositories := make([]core.ProjectRepository, 0, len(destinations))
	for index, destination := range destinations {
		id := "app"
		if index > 0 {
			id = fmt.Sprintf("repo%d", index)
		}
		repositories = append(repositories, core.ProjectRepository{
			ID:          id,
			Github:      stringPointer(fmt.Sprintf("owner/repo%d", index)),
			Destination: destination,
		})
	}
	config.Bundles["project"] = core.ProjectBundle{PrimaryRepo: "app", Repositories: repositories}
	return config
}

func containerSession(t *testing.T, id string, backendHost *string) (core.SessionRecord, core.TargetsTargetLocator) {
	t.Helper()
	session := store.CheckpointTestSession(id)
	workspace, err := core.NewContainerWorkspace(id)
	if err != nil {
		t.Fatal(err)
	}
	session.ContainerWorkspace = &workspace
	host := "local"
	if backendHost != nil {
		host = "ssh:" + *backendHost
	}
	session.BuildCache = &core.SessionBuildCache{Host: host, Directory: "/srv/cache"}
	containerID, err := core.ResourceName(id)
	if err != nil {
		t.Fatal(err)
	}
	storage := core.TargetsPodmanWorkspaceLocator{Kind: core.TargetsPodmanWorkspaceLocatorContainerLayer}
	backend := core.TargetsTargetLocator{
		Kind:             core.TargetsTargetLocatorLocalPodman,
		ContainerID:      containerID,
		WorkspaceStorage: storage,
	}
	if backendHost != nil {
		backend = core.TargetsTargetLocator{
			Kind:             core.TargetsTargetLocatorSshPodman,
			SSH:              &core.SshTarget{Destination: *backendHost, SSHArgs: []string{}},
			ContainerID:      containerID,
			WorkspaceStorage: storage,
		}
	}
	return session, backend
}

func TestAMovedSessionKeepsTheBuildStateItsLiveTargetShares(t *testing.T) {
	id := "0123456789abcdef0123456789abcdef"
	config := bundleConfig("app")
	retired, retiredBackend := containerSession(t, id, nil)
	release := func() *BuildStateRelease {
		release := buildStateReleaseForTarget(&retired, &retiredBackend, &config)
		if release == nil {
			t.Fatal("the retired target has a release")
		}
		return release
	}

	// A destination container on the same host checks out at the same path.
	live, liveBackend := containerSession(t, id, nil)
	if kept := release().Excluding(&live, &liveBackend); kept != nil {
		t.Fatalf("%#v", kept)
	}

	// One on another host does not share the retired host's cache.
	live, liveBackend = containerSession(t, id, stringPointer("build.test"))
	kept := release().Excluding(&live, &liveBackend)
	if kept == nil {
		t.Fatal("another host's target keeps nothing of this host's state")
	}
	if !slices.Equal(kept.workspaces, []string{"/workspace/" + id + "/app"}) {
		t.Fatalf("%q", kept.workspaces)
	}
}

func TestANestedLiveRootDisablesPrefixCleanupAndFiltersFallbackPaths(t *testing.T) {
	id := "0123456789abcdef0123456789abcdef"
	config := bundleConfig("app", "lib")
	retired, retiredBackend := containerSession(t, id, nil)
	release := buildStateReleaseForTarget(&retired, &retiredBackend, &config)
	if release == nil {
		t.Fatal("the retired target has a release")
	}
	// for_target only names a session's own root; this covers a root that
	// would contain the live one anyway.
	release.cleanupRoot = stringPointer("/workspace")
	release.workspaces = []string{"/workspace/" + id + "/app", "/workspace/other/lib"}
	live, liveBackend := containerSession(t, id, nil)

	kept := release.Excluding(&live, &liveBackend)
	if kept == nil {
		t.Fatal("another workspace remains")
	}

	if kept.cleanupRoot != nil {
		t.Fatal(*kept.cleanupRoot)
	}
	if !slices.Equal(kept.workspaces, []string{"/workspace/other/lib"}) {
		t.Fatalf("%q", kept.workspaces)
	}

	sandbox := newReleaseSandboxWithUnderSupport(t, true, true)
	result := sandbox.run(kept)

	requireStatus(t, result, 0)
	if got, want := sandbox.log(), "clean /workspace/other/lib|/srv/cache|/srv/cache/.mjolnir/config\n"; got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestAReleaseRootNestedUnderTheLiveRootIsDiscarded(t *testing.T) {
	id := "0123456789abcdef0123456789abcdef"
	config := bundleConfig("app")
	retired, retiredBackend := containerSession(t, id, nil)
	release := buildStateReleaseForTarget(&retired, &retiredBackend, &config)
	if release == nil {
		t.Fatal("the retired target has a release")
	}
	release.cleanupRoot = stringPointer("/workspace/" + id + "/retired")
	release.workspaces = []string{"/workspace/" + id + "/retired/app"}
	live, liveBackend := containerSession(t, id, nil)

	if kept := release.Excluding(&live, &liveBackend); kept != nil {
		t.Fatalf("%#v", kept)
	}
}

func TestMoveExclusionComparesPathComponentsNotPrefixText(t *testing.T) {
	id := "0123456789abcdef0123456789abcdef"
	config := bundleConfig("app")
	retired, retiredBackend := containerSession(t, id, nil)
	release := buildStateReleaseForTarget(&retired, &retiredBackend, &config)
	if release == nil {
		t.Fatal("the retired target has a release")
	}
	live, liveBackend := containerSession(t, id, nil)
	live.ContainerWorkspace = stringPointer("/workspace/" + id + "-live")

	kept := release.Excluding(&live, &liveBackend)
	if kept == nil {
		t.Fatal("a sibling workspace keeps the release")
	}

	if optionalText(kept.cleanupRoot) != "/workspace/"+id {
		t.Fatal(optionalText(kept.cleanupRoot))
	}
}

func TestAnSshBareBundleUsesItsWorkspaceAsTheReleaseRoot(t *testing.T) {
	id := "0123456789abcdef0123456789abcdef"
	config := bundleConfig("app", "nested/lib")
	session := store.CheckpointTestSession(id)
	backend := core.TargetsTargetLocator{
		Kind:      core.TargetsTargetLocatorSshBare,
		SSH:       &core.SshTarget{Destination: "builder.test", SSHArgs: []string{}},
		Workspace: ".local/share/hel/workspaces/" + id,
	}

	release := buildStateReleaseForTarget(&session, &backend, &config)
	if release == nil {
		t.Fatal("an SSH bare bundle has a release")
	}

	if optionalText(release.cleanupRoot) != ".local/share/hel/workspaces/"+id {
		t.Fatal(optionalText(release.cleanupRoot))
	}
	want := []string{
		".local/share/hel/workspaces/" + id + "/app",
		".local/share/hel/workspaces/" + id + "/nested/lib",
	}
	if !slices.Equal(release.workspaces, want) {
		t.Fatalf("%q", release.workspaces)
	}
}

func TestAWorkspaceThatIsNotTheSessionsOwnHasNoReleaseRoot(t *testing.T) {
	id := "0123456789abcdef0123456789abcdef"
	config := bundleConfig("app")
	session := store.CheckpointTestSession(id)
	sharedBare := core.TargetsTargetLocator{
		Kind:      core.TargetsTargetLocatorSshBare,
		SSH:       &core.SshTarget{Destination: "builder.test", SSHArgs: []string{}},
		Workspace: "/srv/mj/workspaces",
	}
	if release := buildStateReleaseForTarget(&session, &sharedBare, &config); release != nil {
		t.Fatalf("%#v", release)
	}

	legacy, backend := containerSession(t, id, nil)
	legacy.ContainerWorkspace = stringPointer("/workspace")
	if release := buildStateReleaseForTarget(&legacy, &backend, &config); release != nil {
		t.Fatalf("%#v", release)
	}
}

func TestARelativeBareWorkspaceResolvesAgainstHome(t *testing.T) {
	sandbox := newReleaseSandboxWithUnderSupport(t, true, true)
	release := BuildStateReleaseManagedCheckout(nil, ".local/share/hel/workspaces/gone")

	result := sandbox.run(&release)

	requireStatus(t, result, 0)
	home := canonical(t, sandbox.path("home"))
	if got, want := sandbox.log(), fmt.Sprintf("clean --under %s/.local/share/hel/workspaces/gone||\n", home); got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestBorrowedAndUnmanagedTargetsHaveNoReleaseRoot(t *testing.T) {
	id := "0123456789abcdef0123456789abcdef"
	config := bundleConfig("app")
	session, borrowedBackend := containerSession(t, id, nil)
	borrowedBackend.BorrowedFrom = stringPointer("owner")
	if release := buildStateReleaseForTarget(&session, &borrowedBackend, &config); release != nil {
		t.Fatalf("%#v", release)
	}

	unmanaged := store.CheckpointTestSession(id)
	unmanaged.ProjectDirectory = stringPointer("/home/user/project")
	bareBackend := core.TargetsTargetLocator{
		Kind:      core.TargetsTargetLocatorSshBare,
		SSH:       &core.SshTarget{Destination: "builder.test", SSHArgs: []string{}},
		Workspace: "/home/user/project",
	}
	if release := buildStateReleaseForTarget(&unmanaged, &bareBackend, &config); release != nil {
		t.Fatalf("%#v", release)
	}
}
