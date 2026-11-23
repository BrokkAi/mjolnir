// Port of mj-targets/src/targets/tests.rs: stage scopes, plan execution
// order and concurrency, cancellable and bounded executors, remote Docker
// and Apple container targets, borrowed containers and worker liveness.

package targets

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type stageEvent struct {
	stage   core.ProvisionStage
	started bool
}

// stageRecordingExecutor checks that controlled recovery work runs inside
// its stage scope and records each stage report.
type stageRecordingExecutor struct {
	testExecutor
	events []stageEvent
	active []core.ProvisionStage
	fail   bool
}

func newStageRecordingExecutor(t *testing.T, fail bool) *stageRecordingExecutor {
	executor := &stageRecordingExecutor{fail: fail}
	recoveryCopy := core.ProvisionStage{Kind: core.ProvisionStageRecoveryCopy}
	executor.execute = func(*core.CommandSpec) (core.CommandOutput, error) {
		if !slices.Contains(executor.active, recoveryCopy) {
			t.Fatal("controlled recovery work must run inside its stage scope")
		}
		if executor.fail {
			return core.CommandOutput{}, errors.New("controlled recovery work failed")
		}
		return core.CommandOutput{Status: 0, Stdout: []byte{}, Stderr: []byte{}}, nil
	}
	executor.stageStarted = func(stage core.ProvisionStage) {
		if !slices.Contains(executor.active, stage) {
			executor.active = append(executor.active, stage)
		}
		executor.events = append(executor.events, stageEvent{stage, true})
	}
	executor.stageFinished = func(stage core.ProvisionStage) {
		index := slices.Index(executor.active, stage)
		if index < 0 {
			t.Fatal("stage finished without a matching start")
		}
		executor.active = slices.Delete(executor.active, index, index+1)
		executor.events = append(executor.events, stageEvent{stage, false})
	}
	return executor
}

func TestScopedStageReportsCompletionAfterControlledSuccess(t *testing.T) {
	executor := newStageRecordingExecutor(t, false)
	recoveryCopy := core.ProvisionStage{Kind: core.ProvisionStageRecoveryCopy}
	func() {
		stage := core.NewProvisionStageGuard(executor, recoveryCopy)
		defer stage.End()
		command := core.NewCommandSpec("capture", nil)
		if _, err := executor.Execute(&command); err != nil {
			t.Fatal(err)
		}
		if !slices.Contains(executor.active, recoveryCopy) {
			t.Fatal("the stage ended inside its scope")
		}
	}()
	if len(executor.active) != 0 {
		t.Fatalf("%v", executor.active)
	}
	if !reflect.DeepEqual(executor.events, []stageEvent{{recoveryCopy, true}, {recoveryCopy, false}}) {
		t.Fatalf("%v", executor.events)
	}
}

func TestScopedStageReportsCompletionAfterControlledError(t *testing.T) {
	executor := newStageRecordingExecutor(t, true)
	recoveryCopy := core.ProvisionStage{Kind: core.ProvisionStageRecoveryCopy}
	err := func() error {
		stage := core.NewProvisionStageGuard(executor, recoveryCopy)
		defer stage.End()
		command := core.NewCommandSpec("capture", nil)
		_, err := executor.Execute(&command)
		return err
	}()
	if err == nil {
		t.Fatal("the controlled failure was lost")
	}
	if len(executor.active) != 0 {
		t.Fatalf("%v", executor.active)
	}
	if !reflect.DeepEqual(executor.events, []stageEvent{{recoveryCopy, true}, {recoveryCopy, false}}) {
		t.Fatalf("%v", executor.events)
	}
}

func TestExecutorStopsAtFirstFailedCommand(t *testing.T) {
	executor := newFakeExecutor(1)
	plan := core.CommandPlan{
		Description: "test",
		Commands: []core.CommandSpec{
			core.NewCommandSpec("one", nil).WithPurpose("one"),
			core.NewCommandSpec("two", nil).WithPurpose("two"),
			core.NewCommandSpec("three", nil).WithPurpose("three"),
		},
	}
	if _, err := plan.Execute(executor); err == nil {
		t.Fatal("the plan ran past a failed command")
	}
	if len(executor.seen) != 2 {
		t.Fatalf("%d commands", len(executor.seen))
	}
}

// syncFakeExecutor is a goroutine-safe counterpart to fakeExecutor, usable
// with CommandPlan.ExecuteConcurrent.
type syncFakeExecutor struct {
	testExecutor
	mu     sync.Mutex
	seen   []core.CommandSpec
	failAt int
}

func newSyncFakeExecutor(failAt int) *syncFakeExecutor {
	executor := &syncFakeExecutor{failAt: failAt}
	executor.execute = func(command *core.CommandSpec) (core.CommandOutput, error) {
		executor.mu.Lock()
		defer executor.mu.Unlock()
		index := len(executor.seen)
		executor.seen = append(executor.seen, *command)
		status := 0
		if executor.failAt == index {
			status = 1
		}
		return core.CommandOutput{Status: status, Stdout: []byte{}, Stderr: []byte("failure")}, nil
	}
	return executor
}

// barrierExecutor blocks every command until the whole batch has started,
// so a plan only finishes if CommandPlan.ExecuteConcurrent actually starts
// the whole batch before any command in it completes.
type barrierExecutor struct {
	testExecutor
	mu      sync.Mutex
	seen    []core.CommandSpec
	arrived sync.WaitGroup
}

func newBarrierExecutor(parties int) *barrierExecutor {
	executor := &barrierExecutor{}
	executor.arrived.Add(parties)
	executor.execute = func(command *core.CommandSpec) (core.CommandOutput, error) {
		executor.mu.Lock()
		executor.seen = append(executor.seen, *command)
		executor.mu.Unlock()
		executor.arrived.Done()
		executor.arrived.Wait()
		return core.CommandOutput{Status: 0, Stdout: []byte{}, Stderr: []byte{}}, nil
	}
	return executor
}

func TestGroupedCommandsRunConcurrently(t *testing.T) {
	plan := core.CommandPlan{
		Description: "test",
		Commands: []core.CommandSpec{
			core.NewCommandSpec("one", nil).WithPurpose("one").WithParallelGroup(7),
			core.NewCommandSpec("two", nil).WithPurpose("two").WithParallelGroup(7),
			core.NewCommandSpec("three", nil).WithPurpose("three").WithParallelGroup(7),
		},
	}
	executor := newBarrierExecutor(3)

	outputs, err := plan.ExecuteConcurrent(executor)
	if err != nil {
		t.Fatal(err)
	}

	if len(outputs) != 3 {
		t.Fatalf("%d outputs", len(outputs))
	}
	if len(executor.seen) != 3 {
		t.Fatalf("%d commands", len(executor.seen))
	}
}

// orderSensitiveFailureExecutor fails "first" slowly and "third"
// immediately, so a plan-order failure report (rather than a
// completion-order one) can only pick "first".
func orderSensitiveFailureExecutor() (*testExecutor, func() []core.CommandSpec) {
	var mu sync.Mutex
	var seen []core.CommandSpec
	executor := &testExecutor{
		execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
			mu.Lock()
			seen = append(seen, *command)
			mu.Unlock()
			switch command.Purpose {
			case "first":
				time.Sleep(50 * time.Millisecond)
				return core.CommandOutput{Status: 1, Stdout: []byte{}, Stderr: []byte("first failed")}, nil
			case "third":
				return core.CommandOutput{Status: 1, Stdout: []byte{}, Stderr: []byte("third failed")}, nil
			default:
				return core.CommandOutput{Status: 0, Stdout: []byte{}, Stderr: []byte{}}, nil
			}
		},
	}
	return executor, func() []core.CommandSpec {
		mu.Lock()
		defer mu.Unlock()
		return slices.Clone(seen)
	}
}

func TestBatchFailureReportsTheFirstInPlanOrderFailureAndBlocksLaterCommands(t *testing.T) {
	plan := core.CommandPlan{
		Description: "test",
		Commands: []core.CommandSpec{
			core.NewCommandSpec("a", nil).WithPurpose("first").WithParallelGroup(3),
			core.NewCommandSpec("b", nil).WithPurpose("second").WithParallelGroup(3),
			core.NewCommandSpec("c", nil).WithPurpose("third").WithParallelGroup(3),
			core.NewCommandSpec("d", nil).WithPurpose("fourth"),
		},
	}
	executor, seen := orderSensitiveFailureExecutor()

	_, err := plan.ExecuteConcurrent(executor)

	if err == nil || !strings.HasPrefix(err.Error(), "first failed with status 1") {
		t.Fatalf("expected the plan-order failure (\"first\"), got: %v", err)
	}
	commands := seen()
	if len(commands) != 3 {
		t.Fatalf("the whole failing batch starts even though it fails: %d", len(commands))
	}
	if slices.ContainsFunc(commands, func(command core.CommandSpec) bool { return command.Purpose == "fourth" }) {
		t.Fatal("a command after a failed batch must not start")
	}
}

func TestFailureBeforeABatchPreventsTheBatchFromStarting(t *testing.T) {
	plan := core.CommandPlan{
		Description: "test",
		Commands: []core.CommandSpec{
			core.NewCommandSpec("gate", nil).WithPurpose("gate"),
			core.NewCommandSpec("a", nil).WithPurpose("batch-a").WithParallelGroup(9),
			core.NewCommandSpec("b", nil).WithPurpose("batch-b").WithParallelGroup(9),
		},
	}
	executor := newSyncFakeExecutor(0)

	if _, err := plan.ExecuteConcurrent(executor); err == nil {
		t.Fatal("the plan succeeded past a failed gate")
	}
	if len(executor.seen) != 1 {
		t.Fatalf("a batch must not start once an earlier command has already failed: %d", len(executor.seen))
	}
}

func TestCancellableExecutorTerminatesARunningProcess(t *testing.T) {
	skipOnWindows(t, "#1296: process cancellation and relay recovery assume Unix")
	cancelled := new(atomic.Bool)
	executor := core.NewCancellableProcessExecutor(cancelled)
	done := make(chan struct{})
	go func() {
		defer close(done)
		time.Sleep(50 * time.Millisecond)
		cancelled.Store(true)
	}()
	started := time.Now()
	command := core.NewCommandSpec("sh", []string{"-c", "sleep 30"}).WithPurpose("test cancellable process")
	_, err := executor.Execute(&command)
	<-done
	if err == nil || !strings.Contains(err.Error(), "operation cancelled") {
		t.Fatalf("%v", err)
	}
	if time.Since(started) >= 2*time.Second {
		t.Fatalf("took %s", time.Since(started))
	}
}

func TestCancellableExecutorEnforcesItsInlineDeadline(t *testing.T) {
	skipOnWindows(t, "#1296: process cancellation and relay recovery assume Unix")
	executor := core.CancellableProcessExecutorWithTimeout(50 * time.Millisecond)
	started := time.Now()

	command := core.NewCommandSpec("sh", []string{"-c", "sleep 30"}).WithPurpose("test bounded process execution")
	_, err := executor.Execute(&command)

	if err == nil || err.Error() != "timed out after 0.05 s while test bounded process execution" {
		t.Fatalf("%v", err)
	}
	if time.Since(started) >= 2*time.Second {
		t.Fatalf("took %s", time.Since(started))
	}
}

func TestBoundedExecutorTimesOutNamingTheProbeAndStillRunsTheNextOne(t *testing.T) {
	skipOnWindows(t, "#1297: runs sh, du or touch on the controller")
	executor := core.NewBoundedProcessExecutor(time.Second)
	started := time.Now()

	command := core.NewCommandSpec("sh", []string{"-c", "sleep 30"}).WithPurpose("check a wedged prerequisite")
	_, err := executor.Execute(&command)
	if err == nil {
		t.Fatal("a wedged probe answered")
	}

	if time.Since(started) >= 5*time.Second {
		t.Fatalf("took %s", time.Since(started))
	}
	message := err.Error()
	if !strings.Contains(message, "`sh`") {
		t.Fatal(message)
	}
	if !strings.Contains(message, "check a wedged prerequisite") {
		t.Fatalf("the timeout must name the probe that hung: %s", message)
	}
	var timedOut *core.CommandTimedOut
	if !errors.As(err, &timedOut) {
		t.Fatal("a probe timeout is a typed CommandTimedOut so callers can tell it apart")
	}
	if timedOut.Purpose != "check a wedged prerequisite" {
		t.Fatal(timedOut.Purpose)
	}

	// Each command gets its own deadline, so one hung probe does not cancel
	// every probe that follows it.
	next := core.NewCommandSpec("sh", []string{"-c", "printf ready"}).WithPurpose("check the next one")
	output, err := executor.Execute(&next)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 || string(output.Stdout) != "ready" {
		t.Fatalf("%d %q", output.Status, output.Stdout)
	}
}

func TestCancellableExecutorInterruptsABlockedStdinPipe(t *testing.T) {
	skipOnWindows(t, "#1296: process cancellation and relay recovery assume Unix")
	cancelled := new(atomic.Bool)
	executor := core.NewCancellableProcessExecutor(cancelled)
	done := make(chan struct{})
	go func() {
		defer close(done)
		time.Sleep(50 * time.Millisecond)
		cancelled.Store(true)
	}()
	input := strings.NewReader(string(make([]byte, 16*1024*1024)))
	started := time.Now()

	command := core.NewCommandSpec("sh", []string{"-c", "sleep 30"}).WithPurpose("test cancellation while streaming")
	_, err := executor.ExecuteWithStdin(&command, input)

	<-done
	if err == nil || !strings.Contains(err.Error(), "operation cancelled") {
		t.Fatalf("%v", err)
	}
	if time.Since(started) >= 2*time.Second {
		t.Fatalf("took %s", time.Since(started))
	}
}

func TestBootstrapProbesAtRemoteContainerBoundary(t *testing.T) {
	ssh := testSSH()
	plan, err := BootstrapProbePlan(
		ExecutionBoundarySshContainer{Engine: "podman", SSH: &ssh, ContainerID: "abcdef012345"},
		HarnessProbe{Executable: "codex", VersionArgs: []string{"--version"}, BridgeExecutable: stringPointer("codex-acp")},
	)
	if err != nil {
		t.Fatal(err)
	}
	if len(plan.Commands) != 3 {
		t.Fatalf("%d commands", len(plan.Commands))
	}
	if !strings.Contains(lastArg(&plan.Commands[0]), "'podman' 'exec' '-i' 'abcdef012345' 'codex' '--version'") {
		t.Fatal(lastArg(&plan.Commands[0]))
	}
}

func sshDockerTemplate() *core.TargetsTargetTemplate {
	return sshContainerTarget(core.TargetsTargetTemplateSshDocker,
		containerTemplate("ubuntu:24.04", core.ImagePullPolicyAuto, nil, core.TargetsPodmanWorkspaceStorage{}))
}

func TestSSHDockerProvisionsOverlayMountsAndStreamsSecretWithoutLocalDocker(t *testing.T) {
	template := sshDockerTemplate()
	mount := core.AdditionalMount{Source: "/srv/source with 'quotes'", Destination: "/mnt/source", Access: core.MountAccessCow}
	plan := mustProvisionPlan(t, template, []core.AdditionalMount{mount}, nil)
	for _, command := range plan.Commands {
		if command.Program != "ssh" {
			t.Fatalf("%v", command)
		}
	}
	create := plan.Commands[0]
	if !create.CreatesTarget {
		t.Fatal("the first command does not create the target")
	}
	remote := lastArg(&create)
	for _, expected := range []string{"docker volume create", "--pull=missing", core.PosixQuote("/srv/source with 'quotes'")} {
		if !strings.Contains(remote, expected) {
			t.Fatalf("%s lacks %s", remote, expected)
		}
	}
	if strings.Contains(remote, "podman") {
		t.Fatal(remote)
	}
	if err := plan.ProvideTargetEnvironmentSecret(template, "MJ_TEST_SECRET", "secret-value"); err != nil {
		t.Fatal(err)
	}
	create = plan.Commands[0]
	if slices.ContainsFunc(create.Args, func(arg string) bool { return strings.Contains(arg, "secret-value") }) {
		t.Fatalf("%q", create.Args)
	}
	if strings.Contains(fmt.Sprintf("%+v", create), "secret-value") {
		t.Fatal("the rendered command leaks the secret")
	}
	if !strings.Contains(lastArg(&create), "read -r MJ_TEST_SECRET") {
		t.Fatal(lastArg(&create))
	}
}

func sshAppleContainerTemplate() *core.TargetsTargetTemplate {
	return sshContainerTarget(core.TargetsTargetTemplateSshAppleContainer,
		containerTemplate("example.invalid/agent:latest", core.ImagePullPolicyAuto, nil, core.TargetsPodmanWorkspaceStorage{}))
}

func sshAppleContainerLocator(t *testing.T) *core.TargetsTargetLocator {
	ssh := testSSH()
	return &core.TargetsTargetLocator{
		Kind:        core.TargetsTargetLocatorSshAppleContainer,
		SSH:         &ssh,
		ContainerID: mustResourceName(t, testSession),
	}
}

// Apple's runtime keeps its VM disk image on the host that runs it, so a
// remote session is the local one with every command sent over SSH, and
// nothing is run here.
func TestSSHAppleContainerProvisionsWithContainerRunOverSSHOnly(t *testing.T) {
	plan := mustProvisionPlan(t, sshAppleContainerTemplate(), nil, nil)
	if len(plan.Commands) == 0 {
		t.Fatal("no commands")
	}
	for _, command := range plan.Commands {
		if command.Program != "ssh" {
			t.Fatalf("%v", command)
		}
	}
	index := slices.IndexFunc(plan.Commands, func(command core.CommandSpec) bool { return command.CreatesTarget })
	if index < 0 {
		t.Fatal("a command creates the container")
	}
	remote := lastArg(&plan.Commands[index])
	if !strings.Contains(remote, "'container' 'run'") {
		t.Fatal(remote)
	}
	if strings.Contains(remote, "podman") || strings.Contains(remote, "docker") {
		t.Fatal(remote)
	}
	if plan.Commands[0].Program != "ssh" {
		t.Fatal(plan.Commands[0].Program)
	}
}

func TestSSHAppleContainerClosesTheContainerAndSnapshotOverSSH(t *testing.T) {
	plan, err := ClosePlan(sshAppleContainerLocator(t), testSession)
	if err != nil {
		t.Fatal(err)
	}
	command := plan.Scripts()[0]
	if command.Program != "ssh" {
		t.Fatal(command.Program)
	}
	remote := lastArg(&command)
	if !strings.Contains(remote, "container rm --force") {
		t.Fatal(remote)
	}
	// Hard-won: 946dc3fc0: the close script called `container` inside `sh -c` without the Mac's system PATH.
	if !strings.HasPrefix(remote, "'/bin/sh' '-c' ") || !strings.Contains(remote, "path_helper") {
		t.Fatalf("the close script runs under the Mac's system PATH: %s", remote)
	}
	if !strings.Contains(remote, "git/sessions/$2") {
		t.Fatalf("removing the shared cache names the session snapshot: %s", remote)
	}
	if !strings.Contains(remote, fmt.Sprintf(" '%s' ", testSession)) {
		t.Fatalf("the session id the snapshot is named after is passed to the script: %s", remote)
	}
}

func TestSSHAppleContainerAbsenceIsConfirmedOnTheRemoteHost(t *testing.T) {
	locator := sshAppleContainerLocator(t)
	executor := newPodmanPreflightExecutor(podmanOutput(""))
	absent, err := CleanupTargetIsConfirmedAbsent(locator, testSession, executor)
	if err != nil || !absent {
		t.Fatalf("no listed container means the target is absent: %v %v", absent, err)
	}
	if executor.seen[0].Program != "ssh" {
		t.Fatal(executor.seen[0].Program)
	}
	if got := lastArg(&executor.seen[0]); got != core.RemoteCommandLine([]string{"container", "list", "--all", "--quiet"}) {
		t.Fatal(got)
	}

	executor = newPodmanPreflightExecutor(podmanOutput(mustResourceName(t, testSession) + "\n"))
	absent, err = CleanupTargetIsConfirmedAbsent(locator, testSession, executor)
	if err != nil || absent {
		t.Fatalf("a listed container means the target is still present: %v %v", absent, err)
	}
}

func TestSSHAppleContainerSmokeRunsAndRemovesADisposableContainerRemotely(t *testing.T) {
	plan, err := SetupSmokePlan(sshAppleContainerTemplate(), testSession)
	if err != nil {
		t.Fatal(err)
	}
	for _, command := range plan.Commands {
		if command.Program != "ssh" {
			t.Fatalf("%v", command)
		}
	}
	for _, expected := range []string{"'container' 'run'", "'container' 'rm'"} {
		if !slices.ContainsFunc(plan.Commands, func(command core.CommandSpec) bool { return strings.Contains(lastArg(&command), expected) }) {
			t.Fatalf("no command runs %s", expected)
		}
	}
}

func TestSSHDockerSmokeAttemptsCleanupAfterProbeFailureAndPreservesLowerOnCleanupFailure(t *testing.T) {
	fail := func() core.CommandOutput {
		return core.CommandOutput{Status: 1, Stdout: []byte{}, Stderr: []byte("injected failure")}
	}
	executor := newPodmanPreflightExecutor(
		podmanOutput("/tmp/mj-docker-overlay-smoke.test123\n"),
		podmanOutput(""),
		fail(),
		fail(),
	)
	err := RunSetupSmokeTest(sshDockerTemplate(), testSession, executor)
	if err == nil || !strings.Contains(alternateErrorText(err), "cleanup also failed") {
		t.Fatalf("%v", err)
	}
	if len(executor.seen) != 4 {
		t.Fatalf("%d commands", len(executor.seen))
	}
	if !strings.Contains(lastArg(&executor.seen[3]), "docker rm --force") {
		t.Fatal(lastArg(&executor.seen[3]))
	}
	if slices.ContainsFunc(executor.seen, func(command core.CommandSpec) bool { return strings.HasPrefix(lastArg(&command), "'rm'") }) {
		t.Fatal("the lower directory was removed after a failed cleanup")
	}
}

func TestSSHDockerAbsenceDoesNotHideUnavailableDaemon(t *testing.T) {
	ssh := testSSH()
	locator := &core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorSshDocker, SSH: &ssh, ContainerID: mustResourceName(t, testSession)}
	for _, testCase := range []struct {
		status   int
		expected *bool
	}{
		{0, func() *bool { value := true; return &value }()},
		{1, func() *bool { value := false; return &value }()},
		{2, nil},
	} {
		executor := newPodmanPreflightExecutor(core.CommandOutput{Status: testCase.status, Stdout: []byte{}, Stderr: []byte{}})
		absent, err := CleanupTargetIsConfirmedAbsent(locator, testSession, executor)
		if testCase.expected == nil {
			if err == nil {
				t.Fatalf("status %d: an unavailable daemon confirmed absence", testCase.status)
			}
		} else if err != nil || absent != *testCase.expected {
			t.Fatalf("status %d: %v %v", testCase.status, absent, err)
		}
		if executor.seen[0].Program != "ssh" {
			t.Fatal(executor.seen[0].Program)
		}
	}
}

func TestDockerAttachmentInitFailureStopsHelperAndRemovesBackingStorage(t *testing.T) {
	skipOnWindows(t, "Unix only")
	environment := fakeDockerEnvironment(t)
	name := mustResourceName(t, testSession)
	template := containerTemplate("fake:image", core.ImagePullPolicyNever, nil, core.TargetsPodmanWorkspaceStorage{})
	command, err := dockerContainerRun(&template, name, testSession,
		[]core.AdditionalMount{{Source: "/macOS/private temp/source", Destination: "/mnt/cache", Access: core.MountAccessCow}}, "/workspace")
	if err != nil {
		t.Fatal(err)
	}
	output := executeWithFakeDocker(t, environment, &command,
		environmentVariable{"FAKE_DOCKER_FAIL_INIT", "1"},
		environmentVariable{"FAKE_DOCKER_SESSION", testSession},
	)
	if output.Status != 44 {
		t.Fatalf("%d: %s", output.Status, output.Stderr)
	}
	state := filepath.Join(environment, "home/fake-docker")
	if exists(filepath.Join(state, "helper-labels")) {
		t.Fatal("the helper survived")
	}
	if exists(filepath.Join(state, "container-labels")) {
		t.Fatal("the container survived")
	}
	if got := readFile(t, filepath.Join(state, "volumes")); got != "" {
		t.Fatalf("%q", got)
	}
}

func TestDockerVMAttachmentSharesSourceIsolatesWritesAndCleansUp(t *testing.T) {
	skipOnWindows(t, "Unix only")
	skipUnlessIgnoredTestsRequested(t, "requires a running Docker daemon and cached agent-dev image")
	source, err := dockerOverlaySmokeDirectory()
	if err != nil {
		t.Fatal(err)
	}
	defer os.RemoveAll(source)
	session := "vm-test-" + strings.TrimLeft(filepath.Base(source), ".")
	name := mustResourceName(t, session)
	payload := []byte(strings.Repeat("x", 256*1024))
	writeFile(t, filepath.Join(source, "payload"), string(payload))
	// Exercise the image's default user without assuming its UID equals
	// macOS's.
	if err := os.Chmod(source, 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(filepath.Join(source, "payload"), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("payload", filepath.Join(source, "link")); err != nil {
		t.Fatal(err)
	}
	template := agentDevTemplate(core.TargetsPodmanWorkspaceStorage{})
	result := func() error {
		create, err := dockerContainerRun(&template, name, session,
			[]core.AdditionalMount{{Source: source, Destination: "/mnt/source", Access: core.MountAccessCow}}, "/workspace")
		if err != nil {
			return err
		}
		if err := executeChecked(core.ProcessExecutor{}, &create); err != nil {
			return err
		}
		// A file added after launch must remain visible: this is a shared
		// lower, not a copied snapshot.
		if err := os.WriteFile(filepath.Join(source, "after-launch"), []byte("shared"), 0o644); err != nil {
			return err
		}
		exec := core.ContainerExec("docker", name, []string{
			"sh",
			"-c",
			"test \"$(wc -c </mnt/source/payload)\" -eq 262144 && test \"$(cat /mnt/source/after-launch)\" = shared && test -L /mnt/source/link && test \"$(readlink /mnt/source/link)\" = payload && printf changed >/mnt/source/payload && touch /mnt/source/created",
		})
		if err := executeChecked(core.ProcessExecutor{}, &exec); err != nil {
			return err
		}
		original, err := os.ReadFile(filepath.Join(source, "payload"))
		if err != nil {
			return err
		}
		if string(original) != string(payload) {
			return errors.New("original data changed")
		}
		if exists(filepath.Join(source, "created")) {
			return errors.New("container wrote into source")
		}
		return nil
	}()
	close, err := ClosePlan(&core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalDocker, ContainerID: name}, session)
	if err != nil {
		t.Fatal(err)
	}
	for _, command := range close.Scripts() {
		if err := executeChecked(core.ProcessExecutor{}, &command); err != nil {
			t.Fatal(err)
		}
	}
	if result != nil {
		t.Fatal(result)
	}
	for _, kind := range []string{"container", "volume"} {
		command := core.NewCommandSpec("docker", []string{kind, "ls", "--quiet", "--filter", "label=dev.mj.session=" + session})
		output, err := core.ProcessExecutor{}.Execute(&command)
		if err != nil {
			t.Fatal(err)
		}
		if output.Status != 0 {
			t.Fatal(output.Status)
		}
		if len(output.Stdout) != 0 {
			t.Fatalf("leftover %s: %s", kind, output.Stdout)
		}
	}
	if err := RunSetupSmokeTest(containerTarget(core.TargetsTargetTemplateLocalDocker, template), "smoke-"+session, core.ProcessExecutor{}); err != nil {
		t.Fatal(err)
	}
}

func TestDockerCleanupRetainsBackingVolumeWhenOverlayRemovalFails(t *testing.T) {
	skipOnWindows(t, "Unix only")
	environment := fakeDockerEnvironment(t)
	name := mustResourceName(t, testSession)
	seedDockerCleanupState(t, environment, name)
	state := filepath.Join(environment, "home/fake-docker")
	// List the backing first to prove cleanup follows dependencies, not ls
	// order.
	writeFile(t, filepath.Join(state, "volumes"), "mj-volume-backing\nmj-volume-overlay\n")
	close, err := ClosePlan(&core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalDocker, ContainerID: name}, testSession)
	if err != nil {
		t.Fatal(err)
	}
	output := executeWithFakeDocker(t, environment, &close.Scripts()[0], environmentVariable{"FAKE_DOCKER_FAIL_VOLUME_RM", "1"})
	if output.Status != 43 {
		t.Fatalf("%d: %s", output.Status, output.Stderr)
	}
	invocations := readFile(t, filepath.Join(state, "invocations"))
	if !strings.Contains(invocations, "volume rm --force mj-volume-overlay") {
		t.Fatal(invocations)
	}
	if strings.Contains(invocations, "volume rm --force mj-volume-backing") {
		t.Fatal(invocations)
	}
	if got := readFile(t, filepath.Join(state, "volumes")); got != "mj-volume-backing\nmj-volume-overlay\n" {
		t.Fatalf("%q", got)
	}
}

const borrowedChild = "018f9dd2-a3b4-7c8d-9000-ffffffffffff"

func borrowedChildLocator(t *testing.T) *core.TargetsTargetLocator {
	owner := testSession
	return &core.TargetsTargetLocator{
		Kind:         core.TargetsTargetLocatorLocalPodman,
		BorrowedFrom: &owner,
		ContainerID:  mustResourceName(t, testSession),
	}
}

// Hard-won: #1580: project-memory frames on the Control master blocked every other session's relay RPCs
func TestProjectMemoryRelayRidesTheBulkLaneWhileTheLiveRelayStaysOnControl(t *testing.T) {
	// The lane is the only difference: both connect to the same worker proxy.
	for _, locator := range []*core.TargetsTargetLocator{
		sshBareLocator(".local/share/hel/workspaces/" + testSession),
		sshPodmanLocator(mustResourceName(t, testSession), core.TargetsPodmanWorkspaceLocator{}),
	} {
		reconnect, err := ReconnectPlan(locator, testSession)
		if err != nil {
			t.Fatal(err)
		}
		live := reconnect.Commands[0]
		memory, err := ProjectMemoryRelayCommand(locator, testSession)
		if err != nil {
			t.Fatal(err)
		}
		if live.SSHSession == nil || live.SSHSession.Class != core.SshTrafficClassControl {
			t.Fatalf("%v", live.SSHSession)
		}
		if memory.SSHSession == nil || memory.SSHSession.Class != core.SshTrafficClassBulk {
			t.Fatalf("%v", memory.SSHSession)
		}
		memory.SSHSession = live.SSHSession
		if !reflect.DeepEqual(memory, live) {
			t.Fatalf("%+v\n%+v", memory, live)
		}
	}
	// A target without SSH has no lane; the same worker proxy is used.
	local := &core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: "/var/lib/hel/workers/" + testSession}
	memory, err := ProjectMemoryRelayCommand(local, testSession)
	if err != nil {
		t.Fatal(err)
	}
	reconnect, err := ReconnectPlan(local, testSession)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(memory, reconnect.Commands[0]) {
		t.Fatalf("%+v\n%+v", memory, reconnect.Commands[0])
	}
}

func TestWholeTargetPlansRefuseABorrowedContainer(t *testing.T) {
	locator := borrowedChildLocator(t)
	_, closeErr := ClosePlan(locator, borrowedChild)
	_, quiesceErr := QuiescePlan(locator, borrowedChild)
	for _, err := range []error{closeErr, quiesceErr} {
		if err == nil || !strings.Contains(alternateErrorText(err), "borrowed from another session") {
			t.Fatalf("unexpected error: %v", err)
		}
	}
}

func TestTargetRecoveryOfABorrowedContainerBelongsToTheOwningSession(t *testing.T) {
	// A child must not restart the container it borrows, but reporting no
	// plan is not a failure: its own liveness probe and worker restart still
	// have to run.
	plan, err := TargetRecoveryPlan(borrowedChildLocator(t), borrowedChild)
	if err != nil || plan != nil {
		t.Fatalf("%v %v", plan, err)
	}
}

func TestPerWorkerPlansAcceptABorrowedContainerAndLeaveItAlone(t *testing.T) {
	locator := borrowedChildLocator(t)

	if _, err := ReconnectPlan(locator, borrowedChild); err != nil {
		t.Fatalf("a child reconnects through its parent: %v", err)
	}

	cleanup, err := BorrowedWorkerCleanupPlan(locator, borrowedChild)
	if err != nil {
		t.Fatal(err)
	}
	if len(cleanup.Scripts()) != 1 {
		t.Fatalf("%d scripts", len(cleanup.Scripts()))
	}
	arguments := cleanup.Scripts()[0].Args
	index := slices.IndexFunc(arguments, func(argument string) bool { return strings.Contains(argument, "rm -rf") })
	if index < 0 {
		t.Fatal("the cleanup runs a removal script")
	}
	script := arguments[index]
	for _, expected := range []string{
		"/var/lib/hel/workers/" + borrowedChild,
		"/var/lib/hel/profiles/" + borrowedChild,
		".local/share/hel/profiles/" + borrowedChild,
	} {
		if !strings.Contains(script, expected) {
			t.Fatalf("%s lacks %s", script, expected)
		}
	}
	if strings.Contains(script, "/var/lib/hel/workers/"+testSession) {
		t.Fatalf("a child must never remove its parent's worker root: %s", script)
	}
	if strings.Contains(script, "podman rm") || strings.Contains(script, "docker rm") {
		t.Fatalf("a child must never remove the borrowed container: %s", script)
	}
}

func TestMoveDestinationGenerationsHaveIndependentContainerAndVolumeOwnership(t *testing.T) {
	container := containerTemplate("test:latest", core.ImagePullPolicyAuto, nil,
		core.TargetsPodmanWorkspaceStorage{Kind: core.TargetsPodmanWorkspaceStoragePodmanVolume})
	sourceName := mustResourceName(t, testSession)
	destinationName, err := core.MoveResourceName(testSession, "move-one")
	if err != nil {
		t.Fatal(err)
	}
	sourceStorage, err := core.PodmanWorkspaceLocatorFor(&container, testSession)
	if err != nil {
		t.Fatal(err)
	}
	destinationStorage, err := core.PodmanWorkspaceLocatorNamed(&container, destinationName)
	if err != nil {
		t.Fatal(err)
	}
	if reflect.DeepEqual(sourceStorage, destinationStorage) {
		t.Fatalf("%v", sourceStorage)
	}
	bundle := testBundle()
	plan, err := ProvisionPlanNamed(containerTarget(core.TargetsTargetTemplateLocalPodman, container), testSession, &bundle, nil, nil, destinationName, ProvisionOptions{})
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Contains(plan.Commands[0].Args, destinationName) {
		t.Fatalf("%q", plan.Commands[0].Args)
	}
	if slices.Contains(plan.Commands[0].Args, sourceName) {
		t.Fatalf("%q", plan.Commands[0].Args)
	}
	locator := localPodmanLocator(destinationName, destinationStorage)
	if err := core.VerifyLocator(locator, testSession); err != nil {
		t.Fatal(err)
	}
	if err := core.VerifyLocator(locator, "other-session"); err == nil {
		t.Fatal("another session verified the destination")
	}
	cleanup, err := RetireMoveTargetPlan(locator, testSession)
	if err != nil {
		t.Fatal(err)
	}
	for _, command := range cleanup.Scripts() {
		if sameStage(command.Stage, stageOf(core.ProvisionStageCleaningCache)) {
			t.Fatal("retiring a generation cleaned the session cache")
		}
	}
	if !slices.Contains(cleanup.Scripts()[0].Args, destinationName) {
		t.Fatalf("%q", cleanup.Scripts()[0].Args)
	}
	if slices.Contains(cleanup.Scripts()[0].Args, sourceName) {
		t.Fatalf("%q", cleanup.Scripts()[0].Args)
	}
}

// Hard-won: #1508: a failed process probe was reported as worker death, allowing duplicate boots.
func TestWorkerLivenessDoesNotReportDeathWhenTheProbeFails(t *testing.T) {
	skipOnWindows(t, "Unix only")
	workers := t.TempDir()
	root := filepath.Join(workers, "0123456789abcdef")
	if err := os.Mkdir(root, 0o755); err != nil {
		t.Fatal(err)
	}
	// The root has a `hel`, so a worker could have run there.
	writeFile(t, filepath.Join(root, "hel"), "")
	// The staged control worker is what answers: it proves it serves the
	// subcommand (`--help`), then its probe fails.
	control := ControlWorkerPath(root)
	mkdirAll(t, filepath.Dir(control))
	writeExecutable(t, control, "#!/bin/sh\ncase \"$*\" in *--help*) exit 0 ;; esac\nprintf 'cannot open the worker lock\\n' >&2\nexit 71\n")
	if err := os.Chmod(control, 0o700); err != nil {
		t.Fatal(err)
	}
	liveness := func() core.CommandOutput {
		script := WorkerDaemonLivenessScript(root)
		command := core.NewCommandSpec("sh", []string{"-c", script.UnstagedScript()})
		output, err := core.ProcessExecutor{}.Execute(&command)
		if err != nil {
			t.Fatal(err)
		}
		return output
	}
	output := liveness()
	if output.Status != 71 {
		t.Fatalf("%d: %s", output.Status, output.Stderr)
	}
	if len(output.Stdout) != 0 {
		t.Fatalf("%q", output.Stdout)
	}

	// A control worker that was never staged is reported, never read as
	// death.
	if err := os.Remove(control); err != nil {
		t.Fatal(err)
	}
	output = liveness()
	if output.Status != WorkerControlUnstagedStatus {
		t.Fatalf("%d: %s", output.Status, output.Stderr)
	}
	if len(output.Stdout) != 0 {
		t.Fatalf("%q", output.Stdout)
	}
}

func TestPodmanQuiesceAndRemoveSucceedWhenDeadExecConmonCausesStopTimeout(t *testing.T) {
	skipOnWindows(t, "Unix only")
	skipUnlessIgnoredTestsRequested(t, "requires rootless Podman and cached ghcr.io/brokkai/mjolnir/agent-dev:latest image")
	session, err := core.NewSessionID()
	if err != nil {
		t.Fatal(err)
	}
	name := mustResourceName(t, session)
	defer func() {
		command := core.NewCommandSpec("podman", []string{"rm", "--force", "--ignore", name})
		_, _ = core.ProcessExecutor{}.ExecuteCleanup(&command)
	}()
	run := func(args ...string) core.CommandOutput {
		command := core.NewCommandSpec(args[0], args[1:])
		output, err := core.ProcessExecutor{}.Execute(&command)
		if err != nil {
			t.Fatal(err)
		}
		return output
	}

	started := run("podman", "run", "-d", "--name", name,
		"--label", "dev.mj.managed=true",
		"--label", "dev.mj.session="+session,
		"ghcr.io/brokkai/mjolnir/agent-dev:latest", "sleep", "300")
	if started.Status != 0 {
		t.Fatalf("%s", started.Stderr)
	}
	cid := strings.TrimSpace(preflightLossy(started.Stdout))

	exec := run("podman", "exec", "-d", name, "sleep", "300")
	if exec.Status != 0 {
		t.Fatalf("%s", exec.Stderr)
	}
	execID := strings.TrimSpace(preflightLossy(exec.Stdout))

	// Give conmon a moment to register, then kill the exec session's conmon
	// and its exec'd process so Podman hits readExecExitCode()'s 5s timeout.
	time.Sleep(500 * time.Millisecond)
	ps := run("ps", "-eo", "pid,comm,args")
	if ps.Status != 0 {
		t.Fatal(ps.Status)
	}
	execConmon := ""
	for _, line := range preflightLines(preflightLossy(ps.Stdout)) {
		parts := strings.Fields(line)
		if len(parts) >= 2 && parts[1] == "conmon" &&
			strings.Contains(line, cid) && strings.Contains(line, execID) && strings.Contains(line, " -e ") {
			execConmon = parts[0]
			break
		}
	}
	if execConmon == "" {
		t.Fatal("exec conmon pid")
	}
	if killed := run("kill", "-9", execConmon); killed.Status != 0 {
		t.Fatal(killed.Status)
	}
	if killed := run("podman", "exec", name, "sh", "-c", "for p in $(pgrep sleep); do [ \"$p\" != 1 ] && kill -9 \"$p\"; done"); killed.Status != 0 {
		t.Fatal(killed.Status)
	}

	locator := localPodmanLocator(name, core.TargetsPodmanWorkspaceLocator{})

	quiesce, err := QuiescePlan(locator, session)
	if err != nil || quiesce == nil {
		t.Fatalf("%v %v", quiesce, err)
	}
	outputs, err := quiesce.Execute(core.ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	if stderr := preflightLossy(outputs[0].Stderr); !strings.Contains(stderr, "timed out waiting for file") {
		t.Fatalf("expected Podman's exec-exit timeout warning on stderr, got: %s", stderr)
	}
	running := run("podman", "container", "inspect", "--format", "{{.State.Running}}", name)
	if running.Status != 0 || strings.TrimSpace(preflightLossy(running.Stdout)) != "false" {
		t.Fatalf("%d %s", running.Status, running.Stdout)
	}

	cleanup, err := podmanCleanupPlan(locator, session)
	if err != nil {
		t.Fatal(err)
	}
	removed, err := core.ProcessExecutor{}.Execute(&cleanup.Commands[0])
	if err != nil {
		t.Fatal(err)
	}
	removeOutput, err := core.CheckedCommandOutput(&cleanup.Commands[0], removed)
	if err != nil {
		t.Fatal(err)
	}
	if removeOutput.Status != 0 {
		t.Fatal(removeOutput.Status)
	}
	if absent := run("podman", "container", "exists", name); absent.Status != 1 {
		t.Fatal("container should be absent after removal")
	}
}
