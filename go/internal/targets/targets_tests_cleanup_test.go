// Port of mj-targets/src/targets/tests.rs: worker roots, cleanup and
// retirement plans, and the temporary storage launched with containers,
// exercised against a stand-in engine.

package targets

import (
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// A leaked daemon that survives teardown recreates the root it is asked to
// forget, so the kill has to be part of the same cleanup command.
// Hard-won: 2e7f5ec9: a detached worker recreated the root after close and broke resume.
func TestBareCleanupStopsTheRecordedWorkerBeforeRemovingItsRoot(t *testing.T) {
	workerRoot := "/var/lib/hel/workers/" + testSession
	local, err := ClosePlan(&core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: workerRoot}, testSession)
	if err != nil {
		t.Fatal(err)
	}
	remote, err := ClosePlan(sshBareLocator(".local/share/hel/workspaces/"+testSession), testSession)
	if err != nil {
		t.Fatal(err)
	}

	if got := remote.Scripts()[0].Purpose; got != "stop the remote Mjolnir worker and remove exact SSH session workspace and runtime state" {
		t.Fatal(got)
	}

	for _, script := range []string{local.Scripts()[0].Args[1], lastArg(&remote.Scripts()[0])} {
		stop := strings.Index(script, "worker stop --kill --root")
		if stop < 0 {
			t.Fatalf("cleanup stops the worker through its installed binary: %s", script)
		}
		remove := strings.Index(script, "rm -rf")
		if remove < 0 {
			t.Fatalf("cleanup removes the root: %s", script)
		}
		if stop >= remove {
			t.Fatal("the worker must die before its root does")
		}
		if !strings.Contains(script, "--kill") {
			t.Fatal("retirement kills at once")
		}
		if strings.Contains(script, "pkill") {
			t.Fatal(script)
		}
	}
	if !strings.Contains(lastArg(&remote.Scripts()[0]), ".local/share/hel/workers/"+testSession) {
		t.Fatal(lastArg(&remote.Scripts()[0]))
	}
}

func clearRelayStateScript(t *testing.T, locator *core.TargetsTargetLocator) core.CommandSpec {
	t.Helper()
	plan, err := ClearRelayStatePlan(locator, testSession)
	if err != nil {
		t.Fatal(err)
	}
	if plan == nil {
		t.Fatal("the target reuses its worker root")
	}
	return plan.Scripts()[0]
}

// Resume reuses a bare target's worker root, so anything left writing there
// and any stale relay state has to go before the restore seeds it.
// Hard-won: 2e7f5ec9: closing a bare session left stale relay state that broke the next resume.
func TestResumeCleanupClearsRelayStateOnlyForReusedBareRoots(t *testing.T) {
	local := clearRelayStateScript(t, &core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: "/var/lib/hel/workers/" + testSession})
	if local.Purpose != "stop a leaked local Mjolnir worker and clear its relay state" {
		t.Fatal(local.Purpose)
	}
	script := local.Args[1]
	if !strings.Contains(script, "worker stop --root") {
		t.Fatal(script)
	}
	if !strings.Contains(script, fmt.Sprintf("rm -rf -- '/var/lib/hel/workers/%s/relay-state.json' '/var/lib/hel/workers/%s/relay-journal'", testSession, testSession)) {
		t.Fatal(script)
	}

	remote := clearRelayStateScript(t, sshBareLocator(".local/share/hel/workspaces/"+testSession))
	if remote.Purpose != "stop a leaked remote Mjolnir worker and clear its relay state" {
		t.Fatal(remote.Purpose)
	}
	if !strings.Contains(lastArg(&remote), fmt.Sprintf(".local/share/hel/workers/%s/relay-state.json", testSession)) {
		t.Fatal(lastArg(&remote))
	}

	// Containers and instances are rebuilt from nothing on resume.
	plan, err := ClearRelayStatePlan(localPodmanLocator(mustResourceName(t, testSession), core.TargetsPodmanWorkspaceLocator{}), testSession)
	if err != nil || plan != nil {
		t.Fatalf("%v %v", plan, err)
	}
}

// A fresh restore into a reused local worker root must not write through a
// staged home that is a link to a profile home, so the cleanup before it
// unlinks one. A staged home of the session's own is left for the install
// to overwrite, and the profile home is never touched.
// Hard-won: 1eefcdfc: restore through a staged-home symlink could overwrite the user's profile.
func TestResumeCleanupUnlinksALinkedStagedHomeAndKeepsARealOne(t *testing.T) {
	skipOnWindows(t, "Unix only")
	directory := t.TempDir()
	profileHome := filepath.Join(directory, ".codex")
	mkdirAll(t, filepath.Join(profileHome, "sessions"))
	writeFile(t, filepath.Join(profileHome, "auth.json"), "{}")
	for _, linked := range []bool{true, false} {
		kind := "staged"
		if linked {
			kind = "linked"
		}
		workerRoot := filepath.Join(directory, kind, testSession)
		mkdirAll(t, workerRoot)
		stagedHome := filepath.Join(workerRoot, "profile")
		if linked {
			if err := os.Symlink(profileHome, stagedHome); err != nil {
				t.Fatal(err)
			}
		} else {
			mkdirAll(t, stagedHome)
			writeFile(t, filepath.Join(stagedHome, "auth.json"), "{}")
		}

		cleanup := clearRelayStateScript(t, &core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: workerRoot})
		output, err := core.ProcessExecutor{}.Execute(&cleanup)
		if err != nil {
			t.Fatal(err)
		}

		if output.Status != 0 {
			t.Fatalf("%d: %s", output.Status, output.Stderr)
		}
		if linked {
			if exists(stagedHome) {
				t.Fatal("the linked staged home survived")
			}
		} else if info, err := os.Stat(filepath.Join(stagedHome, "auth.json")); err != nil || !info.Mode().IsRegular() {
			t.Fatal("the session's own staged home was removed")
		}
		if info, err := os.Stat(filepath.Join(profileHome, "auth.json")); err != nil || !info.Mode().IsRegular() {
			t.Fatal("the profile home lost its login")
		}
		if info, err := os.Stat(filepath.Join(profileHome, "sessions")); err != nil || !info.IsDir() {
			t.Fatal("the profile home lost its sessions")
		}
	}
}

func inPlaceResetScript(t *testing.T, locator *core.TargetsTargetLocator, previousProfileRoot string) core.CommandSpec {
	t.Helper()
	plan, err := InPlaceWorkerResetPlan(locator, testSession, previousProfileRoot)
	if err != nil {
		t.Fatal(err)
	}
	return plan.Scripts()[0]
}

// An in-place harness replacement keeps the environment, so the reset has
// to take the old harness out of it: its daemon, its relay state, the
// installed worker files, and its per-session profile home.
func TestInPlaceWorkerResetPlanStopsDaemonClearsRelayStateAndOldProfileHome(t *testing.T) {
	localRoot := "/var/lib/hel/workers/" + testSession
	local := inPlaceResetScript(t, &core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: localRoot}, localRoot+"/profile")
	if local.Purpose != "reset the worker root for an in-place harness replacement" {
		t.Fatal(local.Purpose)
	}
	if !sameStage(local.Stage, stageOf(core.ProvisionStageSyncing)) {
		t.Fatalf("%v", local.Stage)
	}
	if local.Program != "sh" {
		t.Fatal(local.Program)
	}
	script := local.Args[1]
	for _, expected := range []string{
		"worker stop --root",
		fmt.Sprintf("rm -rf -- '%s/relay-state.json' '%s/relay-journal'", localRoot, localRoot),
		fmt.Sprintf("rm -f -- '%s/hel' '%s/launch.json' '%s/ownership.json'", localRoot, localRoot, localRoot),
		fmt.Sprintf("rm -rf -- '%s/profile'", localRoot),
		fmt.Sprintf("mkdir -p -- '%s'", localRoot),
	} {
		if !strings.Contains(script, expected) {
			t.Fatalf("%s lacks %s", script, expected)
		}
	}

	remote := inPlaceResetScript(t, sshBareLocator(".local/share/hel/workspaces/"+testSession), ".local/share/hel/profiles/"+testSession)
	if remote.Program != "ssh" {
		t.Fatal(remote.Program)
	}
	remoteScript := lastArg(&remote)
	if !strings.Contains(remoteScript, fmt.Sprintf(".local/share/hel/workers/%s/ownership.json", testSession)) {
		t.Fatal(remoteScript)
	}
	if !strings.Contains(remoteScript, ".local/share/hel/profiles/"+testSession) {
		t.Fatal(remoteScript)
	}

	// Unlike the bare-target relay cleanup, a container gets the same reset:
	// the container itself survives an in-place swap.
	containerID := mustResourceName(t, testSession)
	container := inPlaceResetScript(t, localPodmanLocator(containerID, core.TargetsPodmanWorkspaceLocator{}), "/var/lib/hel/profiles/"+testSession)
	if container.Program != "podman" {
		t.Fatal(container.Program)
	}
	if !slices.Equal(container.Args[:4], []string{"exec", "-i", containerID, "sh"}) {
		t.Fatalf("%q", container.Args)
	}
	if containerScript := lastArg(&container); !strings.Contains(containerScript, fmt.Sprintf("rm -rf -- '/var/lib/hel/profiles/%s'", testSession)) {
		t.Fatal(containerScript)
	}
}

// A session an earlier release started from a profile home has a link where
// its staged home would be. Replacing its harness in place removes the link
// and never the profile home it points at.
func TestInPlaceResetUnlinksALinkedProfileHomeWithoutFollowingIt(t *testing.T) {
	skipOnWindows(t, "Unix only")
	directory := t.TempDir()
	profileHome := filepath.Join(directory, ".codex")
	mkdirAll(t, filepath.Join(profileHome, "sessions"))
	writeFile(t, filepath.Join(profileHome, "auth.json"), "{}")
	workerRoot := filepath.Join(directory, "workers", testSession)
	mkdirAll(t, workerRoot)
	if err := os.Symlink(profileHome, filepath.Join(workerRoot, "profile")); err != nil {
		t.Fatal(err)
	}

	reset := inPlaceResetScript(t, &core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: workerRoot}, workerRoot+"/profile")
	output, err := core.ProcessExecutor{}.Execute(&reset)
	if err != nil {
		t.Fatal(err)
	}

	if output.Status != 0 {
		t.Fatalf("%d: %s", output.Status, output.Stderr)
	}
	if exists(workerRoot + "/profile") {
		t.Fatal("the linked profile home survived")
	}
	if info, err := os.Stat(filepath.Join(profileHome, "auth.json")); err != nil || !info.Mode().IsRegular() {
		t.Fatal("the profile home lost its login")
	}
	if info, err := os.Stat(filepath.Join(profileHome, "sessions")); err != nil || !info.IsDir() {
		t.Fatal("the profile home lost its sessions")
	}
}

func TestPodmanCleanupIgnoresAnAlreadyAbsentContainer(t *testing.T) {
	name := mustResourceName(t, testSession)
	local, err := ClosePlan(localPodmanLocator(name, core.TargetsPodmanWorkspaceLocator{}), testSession)
	if err != nil {
		t.Fatal(err)
	}
	scripts := local.Scripts()
	if scripts[0].Program != "sh" {
		t.Fatal(scripts[0].Program)
	}
	if !strings.Contains(scripts[0].Args[1], "podman rm --force --ignore") {
		t.Fatal(scripts[0].Args[1])
	}
	if lastArg(&scripts[0]) != testSession {
		t.Fatal(lastArg(&scripts[0]))
	}
	if scripts[2].Purpose != "remove Podman session Git cache snapshot" {
		t.Fatal(scripts[2].Purpose)
	}
	if !sameStage(scripts[2].Stage, stageOf(core.ProvisionStageCleaningCache)) {
		t.Fatalf("%v", scripts[2].Stage)
	}

	remote, err := ClosePlan(sshPodmanLocator(name, core.TargetsPodmanWorkspaceLocator{}), testSession)
	if err != nil {
		t.Fatal(err)
	}
	scripts = remote.Scripts()
	if !strings.Contains(lastArg(&scripts[0]), "podman rm --force --ignore") {
		t.Fatal(lastArg(&scripts[0]))
	}
	if scripts[2].Purpose != "remove Podman session Git cache snapshot" {
		t.Fatal(scripts[2].Purpose)
	}
	if !strings.Contains(lastArg(&scripts[2]), testSession) {
		t.Fatal(lastArg(&scripts[2]))
	}
}

func TestPodmanCleanupRemovesContainerBeforeWorkspaceStorage(t *testing.T) {
	name := mustResourceName(t, testSession)
	volume := name + "-workspace"
	plan, err := ClosePlan(localPodmanLocator(name, core.TargetsPodmanWorkspaceLocator{Kind: core.TargetsPodmanWorkspaceLocatorVolume, Name: volume}), testSession)
	if err != nil {
		t.Fatal(err)
	}

	scripts := plan.Scripts()
	if len(scripts) != 4 {
		t.Fatalf("%d scripts", len(scripts))
	}
	for index, kind := range []core.ProvisionStageKind{
		core.ProvisionStageRemovingContainer,
		core.ProvisionStageRemovingStorage,
		core.ProvisionStageRemovingStorage,
		core.ProvisionStageCleaningCache,
	} {
		if !sameStage(scripts[index].Stage, stageOf(kind)) {
			t.Fatalf("%d: %v", index, scripts[index].Stage)
		}
	}
	if !slices.Contains(scripts[1].Args, volume) {
		t.Fatalf("%q", scripts[1].Args)
	}
	if !strings.Contains(scripts[1].Args[1], "dev.mj.session") {
		t.Fatal(scripts[1].Args[1])
	}
}

func TestPodmanQuiesceStopsAndConfirmsTheOwnedContainerWithoutRemovingIt(t *testing.T) {
	plan, err := QuiescePlan(localPodmanLocator(mustResourceName(t, testSession), core.TargetsPodmanWorkspaceLocator{}), testSession)
	if err != nil || plan == nil {
		t.Fatalf("%v %v", plan, err)
	}
	command := plan.Commands[0]
	if !sameStage(command.Stage, stageOf(core.ProvisionStageStoppingTarget)) {
		t.Fatalf("%v", command.Stage)
	}
	if !strings.Contains(command.Args[1], "podman stop --time 0 --ignore") {
		t.Fatal(command.Args[1])
	}
	if !strings.Contains(command.Args[1], ".State.Running") {
		t.Fatal(command.Args[1])
	}
	if strings.Contains(command.Args[1], "podman rm") {
		t.Fatal(command.Args[1])
	}
}

// Hard-won: 770a4e74c: exec could not stop writers in an existing stopped container.
func TestContainerRetirementUsesTheEngineAndRetainsFilesUntilCleanup(t *testing.T) {
	skipOnWindows(t, "Unix only")
	root := t.TempDir()
	bin := filepath.Join(root, "bin")
	mkdirAll(t, bin)
	script := `#!/bin/sh
set -eu
state=$MJ_ENGINE_TEST_ROOT
[ ! -e "$state/unavailable" ] || exit 17
printf '%s\n' "$*" >>"$state/commands"
case $1 in
    container)
        case $* in
            *State.Running*) cat "$state/running" ;;
            *) printf 'true|%s\n' "$MJ_ENGINE_TEST_SESSION" ;;
        esac ;;
    list) if [ "$(cat "$state/running")" = true ]; then printf '%s\n' "$MJ_ENGINE_TEST_CONTAINER"; fi ;;
    stop) printf 'false\n' >"$state/running" ;;
    *) exit 99 ;;
esac
`
	for _, engine := range []string{"docker", "container"} {
		core.InstallFakeCommand(bin, engine, script)
	}
	containerID := mustResourceName(t, testSession)
	for _, target := range []*core.TargetsTargetLocator{
		{Kind: core.TargetsTargetLocatorLocalDocker, ContainerID: containerID},
		{Kind: core.TargetsTargetLocatorAppleContainer, ContainerID: containerID},
	} {
		for _, running := range []bool{false, true} {
			writeFile(t, filepath.Join(root, "running"), fmt.Sprint(running))
			plan, err := StopContainerPlan(target, testSession)
			if err != nil || plan == nil {
				t.Fatalf("%v %v", plan, err)
			}
			for index := range plan.Commands {
				command := &plan.Commands[index]
				command.Env = maps.Clone(command.Env)
				command.Env["PATH"] = pathWithPrefix(bin)
				command.Env["MJ_ENGINE_TEST_ROOT"] = root
				command.Env["MJ_ENGINE_TEST_SESSION"] = testSession
				command.Env["MJ_ENGINE_TEST_CONTAINER"] = containerID
			}
			writeFile(t, filepath.Join(root, "worker-files"), "retain until every writer stops")
			if _, err := plan.Execute(core.ProcessExecutor{}); err != nil {
				t.Fatal(err)
			}
			if got := strings.TrimSpace(readFile(t, filepath.Join(root, "running"))); got != "false" {
				t.Fatal(got)
			}
			if !exists(filepath.Join(root, "worker-files")) {
				t.Fatal("the worker files were removed before cleanup")
			}
			writeFile(t, filepath.Join(root, "unavailable"), "offline")
			if _, err := plan.Execute(core.ProcessExecutor{}); err == nil {
				t.Fatal("an unavailable engine cannot prove termination")
			}
			if err := os.Remove(filepath.Join(root, "unavailable")); err != nil {
				t.Fatal(err)
			}
		}
	}
	for _, line := range preflightLines(readFile(t, filepath.Join(root, "commands"))) {
		if strings.HasPrefix(line, "exec ") {
			t.Fatal(line)
		}
	}
}

func TestDockerCleanupRemovesContainerThenVolumesThenOverlayBackingFiles(t *testing.T) {
	name := mustResourceName(t, testSession)
	close, err := ClosePlan(&core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalDocker, ContainerID: name}, testSession)
	if err != nil {
		t.Fatal(err)
	}
	command := close.Scripts()[0]
	if command.Program != "sh" {
		t.Fatal(command.Program)
	}
	script := command.Args[1]
	removeContainer := strings.Index(script, "docker rm --force")
	listVolumes := strings.Index(script, "docker volume ls")
	removeVolumes := strings.Index(script, "docker volume rm --force")
	removeOverlay := strings.Index(script, ".cache/mjolnir/docker-overlays")
	if removeContainer < 0 || listVolumes < 0 || removeVolumes < 0 || removeOverlay < 0 {
		t.Fatal(script)
	}
	if !(removeContainer < listVolumes && listVolumes < removeVolumes && removeVolumes < removeOverlay) {
		t.Fatal(script)
	}
	for _, expected := range []string{
		`if [ "$status" -eq 0 ]`,
		"label=dev.mj.managed=true",
		"label=dev.mj.session=$2",
		"true|$2",
		"refusing to remove a Docker container Mjolnir does not own",
	} {
		if !strings.Contains(script, expected) {
			t.Fatalf("script lacks %s", expected)
		}
	}
}

const fakeDockerScript = `#!/bin/sh
set -eu
state="$HOME/fake-docker"
mkdir -p "$state"
printf '%s\n' "$*" >>"$state/invocations"
case "${1-}" in
container)
    case ${2-} in inspect|exists) ;; *) exit 2 ;; esac
    for argument do name=$argument; done
    case $name in *-mount-init) labels=helper-labels ;; *) labels=container-labels ;; esac
    [ -f "$state/$labels" ] || exit 1
    cat "$state/$labels"
    ;;
exec)
    [ "${FAKE_DOCKER_FAIL_EXEC:-0}" = 1 ] && exit 45
    [ "${2-}" = --user ] && { [ "${3-}" = 0 ] || [ "${3-}" = 0:0 ]; }
    touch "$state/tmp-initialized"
    ;;
info)
    exit 0
    ;;
rm)
    [ "${FAKE_DOCKER_FAIL_RM:-0}" = 1 ] && exit 42
    for argument do name=$argument; done
    case $name in *-mount-init) labels=helper-labels ;; *) labels=container-labels ;; esac
    rm -f "$state/$labels"
    ;;
run)
    name=
    while [ "$#" -gt 0 ]; do
        if [ "$1" = --name ]; then
            name=$2
            shift 2
        else
            shift
        fi
    done
    case $name in
    *-mount-init)
        printf 'true|%s\n' "${FAKE_DOCKER_SESSION:-}" >"$state/helper-labels"
        [ "${FAKE_DOCKER_FAIL_INIT:-0}" = 1 ] && exit 44
        ;;

    *)
        printf 'true|%s\n' "${FAKE_DOCKER_SESSION:-}" >"$state/container-labels"
        [ "${FAKE_DOCKER_FAIL_RUN:-0}" = 1 ] && exit 125
        ;;
    esac
    exit 0
    ;;
volume)
    case "${2-}" in
    exists)
        [ -f "$state/volumes" ] || exit 1
        grep -Fx "$3" "$state/volumes" >/dev/null
        ;;
    ls)
        [ -f "$state/volumes" ] && cat "$state/volumes" || true
        ;;
    inspect)
        volume=
        formatted=false
        format=''
        shift 2
        while [ "$#" -gt 0 ]; do
            if [ "$1" = --format ]; then
                formatted=true
                format=$2
                shift 2
            else
                volume=$1
                shift
            fi
        done
        [ -n "$volume" ]
        [ -f "$state/volumes" ]
        grep -Fx "$volume" "$state/volumes" >/dev/null
        if [ "$formatted" = true ]; then
            case $format in
            *Mountpoint*) printf '/daemon/volumes/%s/_data\n' "$volume" ;;
            *attachment-backing*) case $volume in *-backing) echo true ;; *) echo '<no value>' ;; esac ;;
            *)
                if [ "$volume" = "${FAKE_DOCKER_FOREIGN_VOLUME:-}" ]; then
                    echo 'true|someone-else'
                else
                    printf 'true|%s\n' "${FAKE_DOCKER_SESSION:-}"
                fi
                ;;
            esac
        fi
        ;;
    create)
        volume=
        for argument do volume=$argument; done
        [ -n "$volume" ]
        touch "$state/volumes"
        if ! grep -Fx "$volume" "$state/volumes" >/dev/null 2>&1; then
            printf '%s\n' "$volume" >>"$state/volumes"
        fi
        printf '%s\n' "$volume"
        ;;
    rm)
        [ "${FAKE_DOCKER_FAIL_VOLUME_RM:-0}" = 1 ] && exit 43
        for argument do volume=$argument; done
        grep -Fxv "$volume" "$state/volumes" >"$state/remaining" || true
        mv "$state/remaining" "$state/volumes"
        ;;
    *)
        exit 2
        ;;
    esac
    ;;
*)
    exit 2
    ;;
esac
`

// fakeDockerEnvironment is a root holding a stand-in `docker` in bin and its
// state under home/fake-docker.
func fakeDockerEnvironment(t *testing.T) string {
	t.Helper()
	root := t.TempDir()
	bin := filepath.Join(root, "bin")
	home := filepath.Join(root, "home")
	mkdirAll(t, bin)
	mkdirAll(t, filepath.Join(home, "fake-docker"))
	core.InstallFakeCommand(bin, "docker", fakeDockerScript)
	return root
}

type environmentVariable struct{ name, value string }

func executeWithFakeDocker(t *testing.T, environment string, command *core.CommandSpec, extraEnv ...environmentVariable) core.CommandOutput {
	t.Helper()
	cloned := *command
	cloned.Env = maps.Clone(command.Env)
	if cloned.Env == nil {
		cloned.Env = map[string]string{}
	}
	cloned.Env["HOME"] = filepath.Join(environment, "home")
	cloned.Env["PATH"] = pathWithPrefix(filepath.Join(environment, "bin"))
	for _, variable := range extraEnv {
		cloned.Env[variable.name] = variable.value
	}
	output, err := core.ProcessExecutor{}.Execute(&cloned)
	if err != nil {
		t.Fatal(err)
	}
	return output
}

func fakeTemporaryStorageEnvironment(t *testing.T) string {
	t.Helper()
	environment := fakeDockerEnvironment(t)
	core.InstallFakeCommand(filepath.Join(environment, "bin"), "podman", "#!/bin/sh\nexec docker \"$@\"\n")
	return environment
}

func temporaryStorageLaunch(t *testing.T, engine string, name string, mounts []core.AdditionalMount) core.CommandSpec {
	t.Helper()
	template := containerTemplate("fake:image", core.ImagePullPolicyNever, nil,
		core.TargetsPodmanWorkspaceStorage{Kind: core.TargetsPodmanWorkspaceStoragePodmanVolume})
	var command core.CommandSpec
	var err error
	switch engine {
	case "podman":
		command, err = podmanContainerRun(&template, name, testSession, mounts, nil, "/workspace")
	case "docker":
		command, err = dockerContainerRun(&template, name, testSession, mounts, "/workspace")
	default:
		panic("unreachable")
	}
	if err != nil {
		t.Fatal(err)
	}
	return command
}

func temporaryStorageLocator(engine string, name string) *core.TargetsTargetLocator {
	switch engine {
	case "podman":
		return localPodmanLocator(name, core.TargetsPodmanWorkspaceLocator{Kind: core.TargetsPodmanWorkspaceLocatorVolume, Name: name + "-workspace"})
	case "docker":
		return &core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalDocker, ContainerID: name}
	default:
		panic("unreachable")
	}
}

func TestNativeTemporaryStorageIsInitializedAndCleanupIsRepeatable(t *testing.T) {
	skipOnWindows(t, "Unix only")
	for _, engine := range []string{"podman", "docker"} {
		environment := fakeTemporaryStorageEnvironment(t)
		name := mustResourceName(t, testSession)
		command := temporaryStorageLaunch(t, engine, name, nil)
		state := filepath.Join(environment, "home/fake-docker")
		session := environmentVariable{"FAKE_DOCKER_SESSION", testSession}
		output := executeWithFakeDocker(t, environment, &command, session)
		if output.Status != 0 {
			t.Fatalf("%s: %s", engine, output.Stderr)
		}
		if !exists(filepath.Join(state, "tmp-initialized")) {
			t.Fatalf("%s: /tmp was not initialized", engine)
		}
		if !slices.Contains(preflightLines(readFile(t, filepath.Join(state, "volumes"))), name+"-tmp") {
			t.Fatalf("%s: no temporary volume", engine)
		}

		close, err := ClosePlan(temporaryStorageLocator(engine, name), testSession)
		if err != nil {
			t.Fatal(err)
		}
		for range 2 {
			for _, command := range close.Scripts() {
				output := executeWithFakeDocker(t, environment, &command, session)
				if output.Status != 0 {
					t.Fatalf("%s: %s", engine, output.Stderr)
				}
			}
		}
		if exists(filepath.Join(state, "container-labels")) {
			t.Fatalf("%s: the container survived cleanup", engine)
		}
		if got := readFile(t, filepath.Join(state, "volumes")); got != "" {
			t.Fatalf("%s: %q", engine, got)
		}
		invocations := readFile(t, filepath.Join(state, "invocations"))
		if !strings.Contains(invocations, "chown 0:0 /tmp; chmod 1777 /tmp") {
			t.Fatalf("%s: %s", engine, invocations)
		}
		usesRecordedPodmanUser := engine == "podman" && strings.Contains(invocations, fmt.Sprintf("exec %s sh -c", name))
		usesExplicitDockerUser := engine == "docker" && strings.Contains(invocations, "exec --user 0")
		if !usesRecordedPodmanUser && !usesExplicitDockerUser {
			t.Fatalf("%s: %s", engine, invocations)
		}
		if strings.Index(invocations, "rm --force") >= strings.Index(invocations, "volume rm --force") {
			t.Fatalf("%s: %s", engine, invocations)
		}
	}
}

func TestTemporaryStorageGuidanceUsesTheActualContainerMount(t *testing.T) {
	name := mustResourceName(t, testSession)
	locator := localPodmanLocator(name, core.TargetsPodmanWorkspaceLocator{})
	for _, testCase := range []struct {
		mount    any
		expected bool
	}{
		{[]any{map[string]any{"Name": name + "-tmp", "Destination": "/tmp", "RW": true}}, true},
		{[]any{}, false},
		{[]any{map[string]any{"Name": "custom-volume", "Destination": "/tmp", "RW": true}}, false},
	} {
		encoded, err := json.Marshal(testCase.mount)
		if err != nil {
			t.Fatal(err)
		}
		executor := newPodmanPreflightExecutor(podmanOutput(string(encoded)))
		storage, err := InspectContainerStorage(locator, nil, executor)
		if err != nil {
			t.Fatal(err)
		}
		if storage.ManagedTemporaryVolume != testCase.expected {
			t.Fatalf("%s: %v", encoded, storage.ManagedTemporaryVolume)
		}
	}
	executor := newPodmanPreflightExecutor(core.CommandOutput{Status: 1, Stdout: []byte{}, Stderr: []byte("container unavailable")})
	_, err := InspectContainerStorage(locator, nil, executor)
	if err == nil || !strings.Contains(err.Error(), "container unavailable") {
		t.Fatalf("%v", err)
	}
}

func TestFailedLaunchRemovesTemporaryStorageOnlyAfterContainerRemoval(t *testing.T) {
	skipOnWindows(t, "Unix only")
	for _, engine := range []string{"podman", "docker"} {
		for _, failure := range []struct {
			variable string
			status   int
		}{{"FAKE_DOCKER_FAIL_RUN", 125}, {"FAKE_DOCKER_FAIL_EXEC", 45}} {
			for _, retainContainer := range []bool{false, true} {
				environment := fakeTemporaryStorageEnvironment(t)
				name := mustResourceName(t, testSession)
				command := temporaryStorageLaunch(t, engine, name, nil)
				failRemoval := "0"
				if retainContainer {
					failRemoval = "1"
				}
				output := executeWithFakeDocker(t, environment, &command,
					environmentVariable{"FAKE_DOCKER_SESSION", testSession},
					environmentVariable{failure.variable, "1"},
					environmentVariable{"FAKE_DOCKER_FAIL_RM", failRemoval},
				)
				if output.Status != failure.status {
					t.Fatalf("%s: %d: %s", engine, output.Status, output.Stderr)
				}
				state := filepath.Join(environment, "home/fake-docker")
				if exists(filepath.Join(state, "container-labels")) != retainContainer {
					t.Fatalf("%s: container retained %v", engine, !retainContainer)
				}
				volumes := readFile(t, filepath.Join(state, "volumes"))
				if slices.Contains(preflightLines(volumes), name+"-tmp") != retainContainer {
					t.Fatalf("%s: %q", engine, volumes)
				}
				if retainContainer {
					if !strings.Contains(preflightLossy(output.Stderr), "retained") {
						t.Fatalf("%s: %s", engine, output.Stderr)
					}
					if strings.Contains(readFile(t, filepath.Join(state, "invocations")), "volume rm") {
						t.Fatalf("%s: storage removed under a retained container", engine)
					}
				} else if volumes != "" {
					t.Fatalf("%s: %s", engine, volumes)
				}
			}
		}
	}
}

func TestTemporaryStorageRefusesForeignVolumesDuringLaunchAndCleanup(t *testing.T) {
	skipOnWindows(t, "Unix only")
	for _, engine := range []string{"podman", "docker"} {
		environment := fakeTemporaryStorageEnvironment(t)
		name := mustResourceName(t, testSession)
		volume := name + "-tmp"
		state := filepath.Join(environment, "home/fake-docker")
		writeFile(t, filepath.Join(state, "volumes"), volume+"\n")
		env := []environmentVariable{
			{"FAKE_DOCKER_SESSION", testSession},
			{"FAKE_DOCKER_FOREIGN_VOLUME", volume},
		}
		launch := temporaryStorageLaunch(t, engine, name, nil)
		output := executeWithFakeDocker(t, environment, &launch, env...)
		if output.Status == 0 {
			t.Fatalf("%s: launched over a foreign volume", engine)
		}
		if !strings.Contains(preflightLossy(output.Stderr), "foreign") {
			t.Fatalf("%s: %s", engine, output.Stderr)
		}
		if exists(filepath.Join(state, "container-labels")) {
			t.Fatalf("%s: a container started", engine)
		}
		for _, line := range preflightLines(readFile(t, filepath.Join(state, "invocations"))) {
			if strings.HasPrefix(line, "run ") {
				t.Fatalf("%s: %s", engine, line)
			}
		}
		close, err := ClosePlan(temporaryStorageLocator(engine, name), testSession)
		if err != nil {
			t.Fatal(err)
		}
		var failure *core.CommandOutput
		for _, command := range close.Scripts() {
			output := executeWithFakeDocker(t, environment, &command, env...)
			if output.Status != 0 {
				failure = &output
				break
			}
		}
		if failure == nil {
			t.Fatalf("%s: cleanup removed a foreign volume", engine)
		}
		if !strings.Contains(preflightLossy(failure.Stderr), "foreign") {
			t.Fatalf("%s: %s", engine, failure.Stderr)
		}
		if got := readFile(t, filepath.Join(state, "volumes")); got != volume+"\n" {
			t.Fatalf("%s: %q", engine, got)
		}
	}
}

func TestAnExplicitTmpAttachmentIsNotReplacedOrReinitialized(t *testing.T) {
	skipOnWindows(t, "Unix only")
	for _, engine := range []string{"podman", "docker"} {
		environment := fakeTemporaryStorageEnvironment(t)
		name := mustResourceName(t, testSession)
		mount := core.AdditionalMount{Source: "/host/scratch", Destination: "/tmp", Access: core.MountAccessRo}
		launch := temporaryStorageLaunch(t, engine, name, []core.AdditionalMount{mount})
		output := executeWithFakeDocker(t, environment, &launch, environmentVariable{"FAKE_DOCKER_SESSION", testSession})
		if output.Status != 0 {
			t.Fatalf("%s: %s", engine, output.Stderr)
		}
		state := filepath.Join(environment, "home/fake-docker")
		if exists(filepath.Join(state, "tmp-initialized")) {
			t.Fatalf("%s: the attachment was reinitialized", engine)
		}
		volumes, _ := os.ReadFile(filepath.Join(state, "volumes"))
		if strings.Contains(string(volumes), name+"-tmp") {
			t.Fatalf("%s: %s", engine, volumes)
		}
	}
}

func TestRetiringAPodmanGenerationPreservesTheDestinationTemporaryVolume(t *testing.T) {
	skipOnWindows(t, "Unix only")
	environment := fakeTemporaryStorageEnvironment(t)
	name := mustResourceName(t, testSession)
	destination, err := core.MoveResourceName(testSession, "tmp-destination")
	if err != nil {
		t.Fatal(err)
	}
	state := filepath.Join(environment, "home/fake-docker")
	env := environmentVariable{"FAKE_DOCKER_SESSION", testSession}
	launch := temporaryStorageLaunch(t, "podman", name, nil)
	if output := executeWithFakeDocker(t, environment, &launch, env); output.Status != 0 {
		t.Fatalf("%s", output.Stderr)
	}
	destinationVolume := destination + "-tmp"
	volumes := readFile(t, filepath.Join(state, "volumes")) + destinationVolume + "\n"
	writeFile(t, filepath.Join(state, "volumes"), volumes)
	close, err := RetireMoveTargetPlan(temporaryStorageLocator("podman", name), testSession)
	if err != nil {
		t.Fatal(err)
	}
	for _, command := range close.Scripts() {
		if output := executeWithFakeDocker(t, environment, &command, env); output.Status != 0 {
			t.Fatalf("%s", output.Stderr)
		}
	}
	if got := readFile(t, filepath.Join(state, "volumes")); got != destinationVolume+"\n" {
		t.Fatalf("%q", got)
	}
}

func agentDevTemplate(storage core.TargetsPodmanWorkspaceStorage) core.TargetsContainerTemplate {
	return containerTemplate("ghcr.io/brokkai/mjolnir/agent-dev:latest", core.ImagePullPolicyNever, nil, storage)
}

func runAndClose(t *testing.T, locator *core.TargetsTargetLocator, session string, work func() error) {
	t.Helper()
	result := work()
	cleanup, err := ClosePlan(locator, session)
	if err != nil {
		t.Fatal(err)
	}
	for _, command := range cleanup.Scripts() {
		if err := executeChecked(core.ProcessExecutor{}, &command); err != nil {
			t.Fatal(err)
		}
	}
	if result != nil {
		t.Fatal(result)
	}
}

func TestPodmanSessionContainerCanRecordPerfSamples(t *testing.T) {
	skipOnWindows(t, "Unix only")
	skipUnlessIgnoredTestsRequested(t, "requires a cached agent-dev image and a host that permits perf; optionally set MJ_PERF_TEST_HOST for SSH")
	if _, ok := core.InstanceName(); !ok {
		t.Fatal("use MJ_INSTANCE=perfmon-regression")
	}
	var ssh *core.SshTarget
	if destination, ok := os.LookupEnv("MJ_PERF_TEST_HOST"); ok {
		ssh = &core.SshTarget{Destination: destination, SSHArgs: []string{"-o", "BatchMode=yes"}}
	}
	session, err := core.NewSessionID()
	if err != nil {
		t.Fatal(err)
	}
	name := mustResourceName(t, session)
	template := agentDevTemplate(core.TargetsPodmanWorkspaceStorage{Kind: core.TargetsPodmanWorkspaceStorageContainerLayer})
	layer := core.TargetsPodmanWorkspaceLocator{Kind: core.TargetsPodmanWorkspaceLocatorContainerLayer}
	locator := localPodmanLocator(name, layer)
	if ssh != nil {
		locator = &core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorSshPodman, SSH: ssh, ContainerID: name, WorkspaceStorage: layer}
	}
	runAndClose(t, locator, session, func() error {
		run, err := podmanContainerRun(&template, name, session, nil, ssh, "/workspace")
		if err != nil {
			return err
		}
		if err := executeChecked(core.ProcessExecutor{}, &run); err != nil {
			return err
		}
		script := `set -eu
work='i=0; while [ "$i" -lt 500000 ]; do i=$((i+1)); done'
perf stat -- sh -c "$work"
perf record -q -F 99 -g -o /tmp/mj-perf.data -- sh -c "$work"
perf script -i /tmp/mj-perf.data > /tmp/mj-perf.txt
test -s /tmp/mj-perf.txt
`
		command := core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"sh", "-c", script})
		return executeChecked(core.ProcessExecutor{}, &command)
	})
}

func TestPodmanTemporaryVolumeIsNativeAndWritableByRootInDefaultNamespace(t *testing.T) {
	skipOnWindows(t, "Unix only")
	skipUnlessIgnoredTestsRequested(t, "requires rootless Podman with runc installed, a cached agent-dev image, and MJ_INSTANCE=tmp1212")
	if instance, ok := core.InstanceName(); !ok || instance != "tmp1212" {
		t.Fatal(instance)
	}
	runcConf := filepath.Join(t.TempDir(), "runc.conf")
	writeFile(t, runcConf, "[engine]\nruntime = \"runc\"\n")
	runtimes := []string{
		"",
		// Hard-won: #1246: Podman passed `nocopy` to runc, which rejects options on bind mounts.
		runcConf,
	}
	for _, runtimeConf := range runtimes {
		for _, storage := range []core.TargetsPodmanWorkspaceStorage{
			{Kind: core.TargetsPodmanWorkspaceStorageContainerLayer},
			{Kind: core.TargetsPodmanWorkspaceStoragePodmanVolume},
		} {
			withRuntime := func(command core.CommandSpec) core.CommandSpec {
				if runtimeConf != "" {
					command.Env = maps.Clone(command.Env)
					command.Env["CONTAINERS_CONF_OVERRIDE"] = runtimeConf
				}
				return command
			}
			if runtimeConf != "" {
				info := withRuntime(core.NewCommandSpec("podman", []string{"info", "--format", "{{.Host.OCIRuntime.Name}}"}))
				output, err := core.ProcessExecutor{}.Execute(&info)
				if err != nil {
					t.Fatal(err)
				}
				if strings.TrimSpace(preflightLossy(output.Stdout)) != "runc" {
					t.Fatalf("%s", output.Stdout)
				}
			}
			session, err := core.NewSessionID()
			if err != nil {
				t.Fatal(err)
			}
			name := mustResourceName(t, session)
			template := agentDevTemplate(storage)
			workspace, err := core.PodmanWorkspaceLocatorNamed(&template, name)
			if err != nil {
				t.Fatal(err)
			}
			locator := localPodmanLocator(name, workspace)
			result := func() error {
				run, err := podmanContainerRun(&template, name, session, nil, nil, "/workspace")
				if err != nil {
					return err
				}
				run = withRuntime(run)
				if err := executeChecked(core.ProcessExecutor{}, &run); err != nil {
					return err
				}
				script := "set -eu; test \"$(id -u)\" = 0; test \"$(id -g)\" = 0; test \"$(stat -c %a /tmp)\" = 1777; test \"$(stat -c %u /tmp)\" = 0; test \"$(stat -f -c %T /tmp)\" != overlayfs; awk '$5 == \"/tmp\" { print; found=1 } END { exit !found }' /proc/self/mountinfo; dd if=/dev/zero of=/tmp/payload bs=131072 count=2 status=none; test \"$(stat -c %s /tmp/payload)\" = 262144"
				exec := withRuntime(core.ContainerExec("podman", name, []string{"sh", "-c", script}))
				if err := executeChecked(core.ProcessExecutor{}, &exec); err != nil {
					return err
				}
				inspected, err := InspectContainerStorage(locator, nil, core.ProcessExecutor{})
				if err != nil {
					return err
				}
				if !inspected.ManagedTemporaryVolume {
					return errors.New("managed temporary mount is missing")
				}
				return nil
			}()
			cleanup, err := ClosePlan(locator, session)
			if err != nil {
				t.Fatal(err)
			}
			for _, command := range cleanup.Scripts() {
				command := withRuntime(command)
				if err := executeChecked(core.ProcessExecutor{}, &command); err != nil {
					t.Fatal(err)
				}
			}
			if result != nil {
				t.Fatal(result)
			}
			if absent, err := CleanupTargetIsConfirmedAbsent(locator, session, core.ProcessExecutor{}); err != nil || !absent {
				t.Fatalf("%v %v", absent, err)
			}
		}
	}
}

// An agent walks a build's parent chain and kills it, which reaches the
// container's PID 1. The container must survive, and its stop signal must be
// one PID 1 cannot ignore, or every stop and removal waits out the grace
// period. The stop signal is checked rather than timed: a loaded host makes
// removal slow either way.
// Hard-won: #1303: an agent's kill of PID 1 stopped the session container
func TestPodmanSessionContainerSurvivesSignalsSentToPid1(t *testing.T) {
	skipOnWindows(t, "Unix only")
	skipUnlessIgnoredTestsRequested(t, "requires rootless Podman and a cached agent-dev image")
	session, err := core.NewSessionID()
	if err != nil {
		t.Fatal(err)
	}
	name := mustResourceName(t, session)
	template := agentDevTemplate(core.TargetsPodmanWorkspaceStorage{})
	workspace, err := core.PodmanWorkspaceLocatorNamed(&template, name)
	if err != nil {
		t.Fatal(err)
	}
	locator := localPodmanLocator(name, workspace)
	inspectFormat := func(name string, format string) string {
		command := core.NewCommandSpec("podman", []string{"inspect", name, "--format", format})
		output, err := core.ProcessExecutor{}.Execute(&command)
		if err != nil {
			t.Fatal(err)
		}
		return strings.TrimSpace(preflightLossy(output.Stdout))
	}
	runAndClose(t, locator, session, func() error {
		run, err := podmanContainerRun(&template, name, session, nil, nil, "/workspace")
		if err != nil {
			return err
		}
		if err := executeChecked(core.ProcessExecutor{}, &run); err != nil {
			return err
		}
		stopSignal := inspectFormat(name, "{{.Config.StopSignal}}")
		if stopSignal != "SIGKILL" && stopSignal != "KILL" && stopSignal != "9" {
			return fmt.Errorf("stop signal is %s; stop and removal would wait out the grace period", core.RustDebugString(stopSignal))
		}
		for _, signal := range []string{"TERM", "HUP"} {
			kill := core.ContainerExec("podman", name, []string{"sh", "-c", fmt.Sprintf("kill -%s 1", signal)})
			if err := executeChecked(core.ProcessExecutor{}, &kill); err != nil {
				return err
			}
			time.Sleep(2 * time.Second)
			if state := inspectFormat(name, "{{.State.Status}}"); state != "running" {
				return fmt.Errorf("container is %s after kill -%s 1", core.RustDebugString(state), signal)
			}
		}
		return nil
	})
	if absent, err := CleanupTargetIsConfirmedAbsent(locator, session, core.ProcessExecutor{}); err != nil || !absent {
		t.Fatalf("%v %v", absent, err)
	}
}

func seedDockerCleanupState(t *testing.T, environment string, name string) {
	t.Helper()
	home := filepath.Join(environment, "home")
	state := filepath.Join(home, "fake-docker")
	overlay := filepath.Join(home, ".cache/mjolnir/docker-overlays", name)
	cloneCache := filepath.Join(home, ".cache/mjolnir/git/sessions", testSession)
	mkdirAll(t, overlay)
	mkdirAll(t, cloneCache)
	writeFile(t, filepath.Join(overlay, ".hel-session"), testSession+"\n")
	writeFile(t, filepath.Join(state, "container-labels"), fmt.Sprintf("true|%s\n", testSession))
	writeFile(t, filepath.Join(state, "volumes"), "mj-volume-one\nmj-volume-two\n")
	if got := readFile(t, filepath.Join(overlay, ".hel-session")); got != testSession+"\n" {
		t.Fatal(got)
	}
	if got := readFile(t, filepath.Join(state, "container-labels")); got != fmt.Sprintf("true|%s\n", testSession) {
		t.Fatal(got)
	}
}

func TestDockerCleanupPreservesOwnedStateWhenContainerRemovalFails(t *testing.T) {
	skipOnWindows(t, "Unix only")
	environment := fakeDockerEnvironment(t)
	name := mustResourceName(t, testSession)
	seedDockerCleanupState(t, environment, name)
	home := filepath.Join(environment, "home")
	state := filepath.Join(home, "fake-docker")
	overlay := filepath.Join(home, ".cache/mjolnir/docker-overlays", name)
	cloneCache := filepath.Join(home, ".cache/mjolnir/git/sessions", testSession)
	close, err := ClosePlan(&core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalDocker, ContainerID: name}, testSession)
	if err != nil {
		t.Fatal(err)
	}

	output := executeWithFakeDocker(t, environment, &close.Scripts()[0], environmentVariable{"FAKE_DOCKER_FAIL_RM", "1"})
	if output.Status == 0 {
		t.Fatal("cleanup succeeded although container removal failed")
	}
	if !exists(overlay) {
		t.Fatal("overlay was removed after container removal failed")
	}
	if !exists(cloneCache) {
		t.Fatal("clone cache was removed after container removal failed")
	}
	if got := readFile(t, filepath.Join(state, "container-labels")); got != fmt.Sprintf("true|%s\n", testSession) {
		t.Fatal(got)
	}
	if got := readFile(t, filepath.Join(state, "volumes")); got != "mj-volume-one\nmj-volume-two\n" {
		t.Fatal(got)
	}
}

func TestDockerOverlayRunRollbackRemovesOwnedMjDirectoryAfterRunFailure(t *testing.T) {
	skipOnWindows(t, "Unix only")
	environment := fakeDockerEnvironment(t)
	name := mustResourceName(t, testSession)
	home := filepath.Join(environment, "home")
	state := filepath.Join(home, "fake-docker")
	lower := t.TempDir()
	writeFile(t, filepath.Join(lower, "original.txt"), "lower\n")
	template := containerTemplate("fake:image", core.ImagePullPolicyNever, nil, core.TargetsPodmanWorkspaceStorage{})
	command, err := dockerContainerRun(&template, name, testSession,
		[]core.AdditionalMount{{Source: lower, Destination: "/mnt/cache", Access: core.MountAccessCow}}, "/workspace")
	if err != nil {
		t.Fatal(err)
	}

	output := executeWithFakeDocker(t, environment, &command,
		environmentVariable{"FAKE_DOCKER_FAIL_RUN", "1"},
		environmentVariable{"FAKE_DOCKER_SESSION", testSession},
	)
	if output.Status != 125 {
		t.Fatalf("%d: %s", output.Status, output.Stderr)
	}
	if exists(filepath.Join(home, ".cache/mjolnir/docker-overlays", name)) {
		t.Fatal("failed Docker run left its owned overlay backing directory")
	}
	if exists(filepath.Join(state, "container-labels")) {
		t.Fatal("the failed container survived")
	}
	if got := readFile(t, filepath.Join(state, "volumes")); got != "" {
		t.Fatalf("%q", got)
	}
	invocations := readFile(t, filepath.Join(state, "invocations"))
	if !strings.Contains(invocations, "--label dev.mj.session="+testSession) {
		t.Fatal(invocations)
	}
	if !strings.Contains(invocations, "--label dev.mj.managed=true") {
		t.Fatal(invocations)
	}
}

func TestCloseRejectsBroadOrMismatchedTargets(t *testing.T) {
	if _, err := ClosePlan(sshBareLocator(".local/share/hel/workspaces"), testSession); err == nil {
		t.Fatal("a broad workspace was accepted")
	}
	if _, err := ClosePlan(localPodmanLocator("hel-someone-abcdef", core.TargetsPodmanWorkspaceLocator{}), testSession); err == nil {
		t.Fatal("a mismatched container was accepted")
	}
	if _, err := ClosePlan(sshBareLocator("/"), testSession); err == nil {
		t.Fatal("the root was accepted")
	}
}

func TestBundleRejectsTraversalAndDuplicateDestinations(t *testing.T) {
	invalid := testBundle()
	invalid.Repositories[0].Destination = "../escape"
	if err := invalid.Validate(); err == nil {
		t.Fatal("a traversal was accepted")
	}
	duplicate := testBundle()
	duplicate.Repositories[1].Destination = "app"
	if err := duplicate.Validate(); err == nil {
		t.Fatal("a duplicate destination was accepted")
	}
}
