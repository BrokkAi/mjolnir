// Port of mj-targets/src/targets/tests.rs: the shared fixtures, the executor
// stream tests, and the Podman, Docker and Apple container preflight tests.

package targets

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Rust's `mj_core::enable_test_fixtures_on_startup!()` in lib.rs.
func init() { core.EnableTestFixtures() }

const testSession = "018f9dd2-a3b4-7c8d-9000-123456789abc"

// testExecutor stands in for the Rust tests' CommandExecutor impls. Each
// function field overrides the trait method it names; the others keep the
// trait's defaults, which the Rust impls inherit.
type testExecutor struct {
	core.CommandExecutorDefaults
	execute          func(*core.CommandSpec) (core.CommandOutput, error)
	executeWithStdin func(*core.CommandSpec, io.Reader) (core.CommandOutput, error)
	cancelled        func() bool
	stageStarted     func(core.ProvisionStage)
	stageFinished    func(core.ProvisionStage)
	notice           func(string)
}

func (e *testExecutor) Execute(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.execute(command)
}

func (e *testExecutor) ExecuteCleanup(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e *testExecutor) ExecuteCleanupWithTimeout(command *core.CommandSpec, _ time.Duration) (core.CommandOutput, error) {
	return e.ExecuteCleanup(command)
}

func (e *testExecutor) ExecuteWithStdin(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	if e.executeWithStdin == nil {
		return e.CommandExecutorDefaults.ExecuteWithStdin(command, input)
	}
	return e.executeWithStdin(command, input)
}

func (e *testExecutor) ExecuteWithoutDeadline(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e *testExecutor) ExecuteWithStdinWithoutDeadline(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	return e.ExecuteWithStdin(command, input)
}

func (e *testExecutor) CancellationRequested() bool {
	if e.cancelled == nil {
		return false
	}
	return e.cancelled()
}

func (e *testExecutor) CancellationRequestedWithoutDeadline() bool {
	return e.CancellationRequested()
}

func (e *testExecutor) PauseDeadline() *core.DeadlinePause { return core.DeadlinePauseNone() }

func (e *testExecutor) StageStarted(stage core.ProvisionStage) {
	if e.stageStarted != nil {
		e.stageStarted(stage)
	}
}

func (e *testExecutor) StageFinished(stage core.ProvisionStage) {
	if e.stageFinished != nil {
		e.stageFinished(stage)
	}
}

func (e *testExecutor) NotifyNotice(notice string) {
	if e.notice != nil {
		e.notice(notice)
	}
}

// skipOnWindows is Rust's `#[cfg_attr(windows, ignore = ...)]` and
// `#[cfg(unix)]` on a test.
func skipOnWindows(t *testing.T, reason string) {
	t.Helper()
	if runtime.GOOS == "windows" {
		t.Skip(reason)
	}
}

// skipUnlessLinux is Rust's `#[cfg(target_os = "linux")]` on a test.
func skipUnlessLinux(t *testing.T) {
	t.Helper()
	if runtime.GOOS != "linux" {
		t.Skip("Linux only")
	}
}

// skipUnlessIgnoredTestsRequested is Rust's #[ignore]: the test runs only
// when MJ_RUN_IGNORED_TESTS=1 asks for it, as `cargo test -- --ignored` does.
func skipUnlessIgnoredTestsRequested(t *testing.T, reason string) {
	t.Helper()
	if os.Getenv("MJ_RUN_IGNORED_TESTS") != "1" {
		t.Skip(reason)
	}
}

// writeExecutable writes a script a test then runs, holding the fork lock
// while the file is open for writing, so no child started meanwhile inherits
// the write descriptor and makes the exec fail with ETXTBSY (#1036). Rust
// writes these files directly; a Go test binary forks from other goroutines.
func writeExecutable(t *testing.T, path string, content string) {
	t.Helper()
	syscall.ForkLock.Lock()
	defer syscall.ForkLock.Unlock()
	if err := os.WriteFile(path, []byte(content), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, 0o755); err != nil {
		t.Fatal(err)
	}
}

// assertStreamsToEOF: a child that reads its stdin to end of file, as the
// checkpoint export worker does, only exits once the write end of the pipe
// is closed. Every executor that streams stdin has to close it after the
// last chunk.
func assertStreamsToEOF(t *testing.T, executor core.CommandExecutor) {
	t.Helper()
	payload := bytes.Repeat([]byte{'x'}, 256*1024)
	input := bytes.NewReader(payload)
	started := time.Now()

	command := core.NewCommandSpec("sh", []string{"-c", "cat"}).WithPurpose("echo a stream read to eof")
	output, err := executor.ExecuteWithStdin(&command, input)
	if err != nil {
		t.Fatal(err)
	}

	if output.Status != 0 {
		t.Fatalf("status %d", output.Status)
	}
	if !bytes.Equal(output.Stdout, payload) {
		t.Fatalf("stdout has %d bytes", len(output.Stdout))
	}
	if len(output.Stderr) != 0 {
		t.Fatalf("stderr %q", output.Stderr)
	}
	if time.Since(started) >= 2*time.Second {
		t.Fatalf("took %s", time.Since(started))
	}
}

func TestProcessExecutorCompletesAStreamAgainstAChildThatReadsToEOF(t *testing.T) {
	skipOnWindows(t, "#1297: runs sh, du or touch on the controller")
	assertStreamsToEOF(t, core.ProcessExecutor{})
}

func TestCancellableExecutorCompletesAStreamAgainstAChildThatReadsToEOF(t *testing.T) {
	skipOnWindows(t, "#1297: runs sh, du or touch on the controller")
	assertStreamsToEOF(t, core.NewCancellableProcessExecutor(new(atomic.Bool)))
}

func TestCancellableExecutorDrainsLargeStdoutAndStderrConcurrently(t *testing.T) {
	skipOnWindows(t, "Unix only")
	command := core.NewCommandSpec("sh", []string{
		"-c",
		"head -c 131072 /dev/zero | tr '\\000' x; head -c 131072 /dev/zero | tr '\\000' y >&2",
	}).WithPurpose("emit large command output")
	output, err := core.CancellableProcessExecutorWithTimeout(5 * time.Second).Execute(&command)
	if err != nil {
		t.Fatal(err)
	}

	if output.Status != 0 {
		t.Fatalf("status %d", output.Status)
	}
	if !bytes.Equal(output.Stdout, bytes.Repeat([]byte{'x'}, 128*1024)) {
		t.Fatalf("stdout has %d bytes", len(output.Stdout))
	}
	if !bytes.Equal(output.Stderr, bytes.Repeat([]byte{'y'}, 128*1024)) {
		t.Fatalf("stderr has %d bytes", len(output.Stderr))
	}
}

func stringPointer(value string) *string { return &value }

func testBundle() core.ProjectBundleSpec {
	return core.ProjectBundleSpec{
		Primary: "app",
		Repositories: []core.RepositorySpec{
			{
				URL:         stringPointer("git@github.com:example/app.git"),
				PushURLs:    []string{},
				Destination: "app",
			},
			{
				URL:         stringPointer("https://github.com/example/lib.git"),
				PushURLs:    []string{},
				Destination: "libs/lib",
			},
		},
	}
}

func testSSH() core.SshTarget {
	return core.SshTarget{
		Destination: "dev@example.test",
		SSHArgs:     []string{"-o", "BatchMode=yes"},
	}
}

// podmanPreflightExecutor answers each command with the next queued output
// and records what it was asked to run.
type podmanPreflightExecutor struct {
	testExecutor
	seen    []core.CommandSpec
	outputs []core.CommandOutput
}

func newPodmanPreflightExecutor(outputs ...core.CommandOutput) *podmanPreflightExecutor {
	executor := &podmanPreflightExecutor{outputs: outputs}
	executor.execute = func(command *core.CommandSpec) (core.CommandOutput, error) {
		executor.seen = append(executor.seen, *command)
		if len(executor.outputs) == 0 {
			panic("removal index (is 0) should be < len (is 0)")
		}
		output := executor.outputs[0]
		executor.outputs = executor.outputs[1:]
		return output, nil
	}
	return executor
}

func podmanOutput(stdout string) core.CommandOutput {
	return core.CommandOutput{Status: 0, Stdout: []byte(stdout), Stderr: []byte{}}
}

func TestAWaiterForTheAppleContainerGateStopsWhenCancelled(t *testing.T) {
	var cancelled atomic.Bool
	checked := make(chan struct{}, 1)
	var firstCheck sync.Once
	executor := &testExecutor{
		execute: func(*core.CommandSpec) (core.CommandOutput, error) {
			panic("a cancelled waiter must not run the Apple container CLI")
		},
		cancelled: func() bool {
			firstCheck.Do(func() { checked <- struct{}{} })
			return cancelled.Load()
		},
	}

	gate := appleContainerRuntimeGate()
	gate.Lock()
	result := make(chan error, 1)
	waiter := make(chan struct{})
	go func() {
		defer close(waiter)
		_, err := EnsureAppleContainerRuntime(executor)
		result <- err
	}()

	select {
	case <-checked:
	case <-time.After(time.Second):
		gate.Unlock()
		t.Fatal("the waiter did not begin checking cancellation")
	}
	time.Sleep(25 * time.Millisecond)
	cancelled.Store(true)
	var err error
	select {
	case err = <-result:
	case <-time.After(500 * time.Millisecond):
		gate.Unlock()
		t.Fatal("cancellation did not release the gate waiter")
	}
	gate.Unlock()
	<-waiter

	if err == nil || !strings.Contains(err.Error(), "cancelled while waiting") {
		t.Fatalf("%v", err)
	}
}

type uidMapping struct{ containerID, hostID, size uint64 }

// podmanInfoJSON is a `podman info --format json` result with the fields the
// preflight reads.
func podmanInfoJSON(rootless bool, serviceIsRemote bool, uidMap []uidMapping) string {
	mapping := make([]map[string]uint64, 0, len(uidMap))
	for _, entry := range uidMap {
		mapping = append(mapping, map[string]uint64{
			"container_id": entry.containerID,
			"host_id":      entry.hostID,
			"size":         entry.size,
		})
	}
	encoded, err := json.Marshal(map[string]any{
		"host": map[string]any{
			"security":        map[string]any{"rootless": rootless},
			"serviceIsRemote": serviceIsRemote,
			"idMappings": map[string]any{
				"uidmap": mapping,
				"gidmap": mapping,
			},
		},
	})
	if err != nil {
		panic(err)
	}
	return string(encoded)
}

func rootlessPodmanInfo() string {
	return podmanInfoJSON(true, false, []uidMapping{{0, 1000, 1}, {1, 100000, 65536}})
}

func rootlessMachineInfo() string {
	return podmanInfoJSON(true, true, []uidMapping{{0, 1000, 1}, {1, 100000, 65536}})
}

func rootfulPodmanInfo() string {
	return podmanInfoJSON(false, false, []uidMapping{{0, 0, 1}, {1, 100000, 65536}})
}

func podmanInspection(status string, sessionID string, managed string) core.CommandOutput {
	encoded, err := json.Marshal([]any{map[string]any{
		"Id": "0123456789abcdef",
		"Config": map[string]any{
			"Labels": map[string]string{
				core.ManagedLabel: managed,
				core.SessionLabel: sessionID,
			},
		},
		"State": map[string]any{"Status": status},
	}})
	if err != nil {
		panic(err)
	}
	return podmanOutput(string(encoded))
}

// missingProgramExecutor fails every command the way a spawn of a program
// that is not on PATH fails.
func missingProgramExecutor() *testExecutor {
	return &testExecutor{
		execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
			return core.CommandOutput{}, fmt.Errorf("run %s for %s: %w", command.Program, command.Purpose, fs.ErrNotExist)
		},
	}
}

// Launch finding R3-11: with no `podman` on PATH, Setup said "Postcondition
// `podman --version` succeeds with Podman 4.0.0 or newer could not be
// checked: run podman for check Podman version". It now says, in plain
// words, which command could not run, what it would have checked, and why.
// Hard-won: eea74e3f: Setup and doctor hid why a missing Podman check could not run.
func TestAMissingPodmanIsReportedAsACommandThatCouldNotRun(t *testing.T) {
	_, err := VerifyLocalPodman(missingProgramExecutor())
	if err == nil {
		t.Fatal("verification succeeded without podman")
	}
	if postcondition, ok := FailedPodmanPostcondition(err); !ok || postcondition != PodmanPostconditionVersion {
		t.Fatalf("postcondition %v %v", postcondition, ok)
	}
	message := err.Error()
	if strings.Contains(message, "Postcondition") {
		t.Fatal(message)
	}
	if strings.Contains(message, "run podman for") {
		t.Fatal(message)
	}
	if !strings.Contains(message, "could not run `podman --version` to check that Podman 4.0.0 or newer is installed") {
		t.Fatal(message)
	}
	if !strings.Contains(message, "`podman` is not installed or not on PATH") {
		t.Fatal(message)
	}
}

func TestPodmanPreflightAcceptsARemoteRootlessPodmanMachine(t *testing.T) {
	if runtime.GOOS != "darwin" {
		t.Skip("macOS only")
	}
	executor := newPodmanPreflightExecutor(
		podmanOutput("podman version 5.4.2\n"),
		podmanOutput(rootlessMachineInfo()),
	)

	preflight, err := VerifyLocalPodman(executor)
	if err != nil {
		t.Fatalf("macOS Podman machine is supported: %v", err)
	}

	if preflight.Version != "5.4.2" {
		t.Fatal(preflight.Version)
	}
}

func TestPodmanPreflightReportsARootfulPodmanAsNotRootless(t *testing.T) {
	executor := newPodmanPreflightExecutor(
		podmanOutput("podman version 5.4.2\n"),
		podmanOutput(rootfulPodmanInfo()),
	)

	_, err := VerifyLocalPodman(executor)

	if postcondition, ok := FailedPodmanPostcondition(err); !ok || postcondition != PodmanPostconditionRootless {
		t.Fatalf("%v", err)
	}
	message := err.Error()
	if !strings.Contains(message, "Postcondition Podman is rootless") {
		t.Fatal(message)
	}
	if !strings.Contains(message, "reports rootful mode") {
		t.Fatal(message)
	}
	if !strings.Contains(message, "podman machine set --rootful=false") {
		t.Fatal(message)
	}
	if strings.Contains(message, "Podman on macOS always uses a Linux VM") {
		t.Fatal(message)
	}
}

func TestPodmanPreflightReportsAnIncompleteUIDMap(t *testing.T) {
	executor := newPodmanPreflightExecutor(
		podmanOutput("podman version 5.4.2\n"),
		podmanOutput(podmanInfoJSON(true, false, []uidMapping{{0, 1000, 1}})),
	)

	_, err := VerifyLocalPodman(executor)

	if postcondition, ok := FailedPodmanPostcondition(err); !ok || postcondition != PodmanPostconditionUIDMap {
		t.Fatalf("%v", err)
	}
	message := err.Error()
	if !strings.Contains(message, "maps container UIDs 0 and 1") {
		t.Fatal(message)
	}
	if !strings.Contains(message, "UID map 0:1000:1") {
		t.Fatal(message)
	}
}

func TestPodmanPreflightRejectsARemoteClientOnALinuxHost(t *testing.T) {
	if runtime.GOOS == "darwin" {
		t.Skip("not on macOS")
	}
	executor := newPodmanPreflightExecutor(
		podmanOutput("podman version 5.4.2\n"),
		podmanOutput(rootlessMachineInfo()),
	)

	_, err := VerifyLocalPodman(executor)

	if postcondition, ok := FailedPodmanPostcondition(err); !ok || postcondition != PodmanPostconditionRemote {
		t.Fatalf("%v", err)
	}
	if !strings.Contains(err.Error(), "uses a local engine") {
		t.Fatal(err)
	}
}

func TestPodmanPreflightReportsUnparsableInfo(t *testing.T) {
	executor := newPodmanPreflightExecutor(
		podmanOutput("podman version 5.4.2\n"),
		podmanOutput("not json\n"),
	)

	_, err := VerifyLocalPodman(executor)

	if postcondition, ok := FailedPodmanPostcondition(err); !ok || postcondition != PodmanPostconditionInfo {
		t.Fatalf("%v", err)
	}
	if !strings.Contains(err.Error(), "unparsable output") {
		t.Fatal(err)
	}
}

// Launch finding R5-3: on a host without Docker, doctor and Setup said
// "Docker preflight failed: run `docker info` as the user running Mjolnir:
// run docker for check Docker daemon: No such file or directory (os error
// 2)". Each case now has one plain sentence: not installed (in the words the
// launch options use), not running, or not answering.
// Hard-won: 028b8327: Setup, doctor, and the wizard gave conflicting local Docker errors.
func TestDockerPreflightSaysInOneSentenceWhyDockerCannotRunSessions(t *testing.T) {
	_, err := VerifyLocalDocker(missingProgramExecutor())
	if err == nil || err.Error() != "Docker is not installed on this host." {
		t.Fatalf("%v", err)
	}
	var unavailable DockerUnavailable
	if !errors.As(err, &unavailable) || unavailable.Kind != DockerUnavailableNotInstalled {
		t.Fatalf("%#v", err)
	}

	stopped := newPodmanPreflightExecutor(core.CommandOutput{
		Status: 1,
		Stderr: []byte("Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?\n"),
	})
	_, err = VerifyLocalDocker(stopped)
	message := err.Error()
	if !strings.HasPrefix(message, "Docker is installed, but its daemon is not running.") {
		t.Fatal(message)
	}
	if !strings.Contains(message, "unix:///var/run/docker.sock") {
		t.Fatal(message)
	}

	refused := newPodmanPreflightExecutor(core.CommandOutput{
		Status: 1,
		Stderr: []byte("permission denied while trying to connect to the Docker daemon socket\n"),
	})
	_, err = VerifyLocalDocker(refused)
	message = err.Error()
	if !strings.HasPrefix(message, "Docker did not answer its check on this host: `docker version` exited with status 1: permission denied") {
		t.Fatal(message)
	}
}

func TestPodmanPreflightRejectsUnsupportedVersionWithUpgradeRemediation(t *testing.T) {
	executor := newPodmanPreflightExecutor(podmanOutput("podman version 3.4.7\n"))

	_, err := VerifyLocalPodman(executor)
	message := err.Error()
	if !strings.Contains(message, "Podman 4.0.0 or newer") {
		t.Fatal(message)
	}
	if !strings.Contains(message, "apt install -y podman uidmap") {
		t.Fatal(message)
	}
	if !strings.Contains(message, PodmanDocumentationURL) {
		t.Fatal(message)
	}
}

// The uid mapping used by session containers is Podman's rootless default,
// so every supported 4.x release passes the version preflight.
func TestPodmanPreflightAcceptsSupportedFourSeriesReleases(t *testing.T) {
	for _, version := range []string{"4.0.0", "4.0.3", "4.2.0", "4.3.0"} {
		executor := newPodmanPreflightExecutor(
			podmanOutput(fmt.Sprintf("podman version %s\n", version)),
			podmanOutput(rootlessPodmanInfo()),
		)
		preflight, err := VerifyLocalPodman(executor)
		if err != nil {
			t.Fatalf("Podman %s: %v", version, err)
		}
		if preflight.Version != version {
			t.Fatalf("Podman %s: %s", version, preflight.Version)
		}
	}
}

// batchedSSHProbes stands in for the remote host: one batched command,
// framed exactly as the remote script prints it.
func batchedSSHProbes(probes ...ProbeFixture) core.CommandOutput {
	return core.CommandOutput{
		Status: 0,
		Stdout: SSHPodmanProbeFixture(probes),
		Stderr: []byte{},
	}
}

func passingSSHProbes(lingerStatus int, lingerStdout string, lingerStderr string) core.CommandOutput {
	return batchedSSHProbes(
		ProbeFixture{"version", 0, "podman version 5.4.2\n", ""},
		ProbeFixture{"info", 0, rootlessPodmanInfo(), ""},
		ProbeFixture{"linger", lingerStatus, lingerStdout, lingerStderr},
	)
}

// Hard-won: 024dc021: four SSH round trips added 2.27 seconds to each remote session.
func TestSSHPodmanPreflightRunsEveryProbeInOneNoninteractiveSSHCommand(t *testing.T) {
	executor := newPodmanPreflightExecutor(passingSSHProbes(0, "yes\n", ""))

	ssh := testSSH()
	preflight, err := VerifySSHPodman(&ssh, executor)
	if err != nil {
		t.Fatal(err)
	}

	if preflight.Version != "5.4.2" {
		t.Fatal(preflight.Version)
	}
	if len(executor.seen) != 1 {
		t.Fatalf("%d commands", len(executor.seen))
	}
	command := executor.seen[0]
	if command.Program != "ssh" {
		t.Fatal(command.Program)
	}
	for _, argument := range []string{"BatchMode=yes", "ConnectTimeout=3", "dev@example.test"} {
		if !slices.Contains(command.Args, argument) {
			t.Fatalf("%q lacks %s", command.Args, argument)
		}
	}
	remote := command.Args[len(command.Args)-1]
	if !strings.Contains(remote, "'sh' '-c'") {
		t.Fatal(remote)
	}
	if strings.Contains(remote, "'-lc'") {
		t.Fatal(remote)
	}
	for _, probe := range []string{"podman --version", "podman info --format json", "loginctl show-user"} {
		if !strings.Contains(remote, probe) {
			t.Fatalf("%s lacks %s", remote, probe)
		}
	}
	if len(preflight.Warnings) != 0 {
		t.Fatalf("%v", preflight.Warnings)
	}
}

func TestSSHPodmanPreflightWarnsWhenRemoteUserLingeringIsDisabled(t *testing.T) {
	executor := newPodmanPreflightExecutor(passingSSHProbes(0, "no\n", ""))

	ssh := testSSH()
	preflight, err := VerifySSHPodman(&ssh, executor)
	if err != nil {
		t.Fatal(err)
	}

	if len(preflight.Warnings) != 1 {
		t.Fatalf("%v", preflight.Warnings)
	}
	warning := preflight.Warnings[0]
	if !strings.Contains(warning.Detail, "lingering is disabled on dev@example.test") {
		t.Fatal(warning.Detail)
	}
	if !strings.Contains(warning.Detail, "last SSH connection closes") {
		t.Fatal(warning.Detail)
	}
	if !strings.Contains(warning.Remediation, "sudo loginctl enable-linger") {
		t.Fatal(warning.Remediation)
	}
}

func TestSSHPodmanPreflightWarnsWhenLingerCheckIsUnavailable(t *testing.T) {
	executor := newPodmanPreflightExecutor(passingSSHProbes(127, "", "sh: loginctl: not found\n"))

	ssh := testSSH()
	preflight, err := VerifySSHPodman(&ssh, executor)
	if err != nil {
		t.Fatal(err)
	}

	if len(preflight.Warnings) != 1 {
		t.Fatalf("%v", preflight.Warnings)
	}
	warning := preflight.Warnings[0]
	for _, expected := range []string{"durability check is unavailable", "`loginctl` was not found", "may not use systemd", "Mjolnir cannot verify"} {
		if !strings.Contains(warning.Detail, expected) {
			t.Fatalf("%s lacks %s", warning.Detail, expected)
		}
	}
	if strings.Contains(warning.Detail, "Hel") {
		t.Fatal(warning.Detail)
	}
	if !strings.Contains(warning.Remediation, "service manager") {
		t.Fatal(warning.Remediation)
	}
}

// A Mac reached over SSH runs Podman through its Podman machine, which
// `podman info` reports as a remote service, and has no `loginctl`. A Linux
// host that reports a remote service still points at another host's engine.
// Hard-won: 2096c275d: an SSH Mac's Podman machine was refused as a remote engine.
func TestSSHPodmanPreflightAcceptsThePodmanMachineOfAMacHostOnly(t *testing.T) {
	machineOn := func(platform string) *podmanPreflightExecutor {
		return newPodmanPreflightExecutor(batchedSSHProbes(
			ProbeFixture{"version", 0, "podman version 6.1.4\n", ""},
			ProbeFixture{"platform", 0, platform, ""},
			ProbeFixture{"info", 0, rootlessMachineInfo(), ""},
			ProbeFixture{"linger", 127, "", "sh: loginctl: command not found\n"},
		))
	}

	ssh := testSSH()
	preflight, err := VerifySSHPodman(&ssh, machineOn("Darwin\n"))
	if err != nil {
		t.Fatal(err)
	}
	if preflight.Version != "6.1.4" {
		t.Fatal(preflight.Version)
	}
	if len(preflight.Warnings) != 0 {
		t.Fatalf("%v", preflight.Warnings)
	}

	_, err = VerifySSHPodman(&ssh, machineOn("Linux\n"))
	if err == nil || !strings.Contains(err.Error(), "serviceIsRemote=true") {
		t.Fatalf("%v", err)
	}
}

func TestSSHPodmanPreflightReportsAnUnreachableHostSeparatelyFromPodman(t *testing.T) {
	executor := newPodmanPreflightExecutor(core.CommandOutput{
		Status: 255,
		Stderr: []byte("ssh: connect to host example.test port 22: Connection timed out"),
	})

	ssh := testSSH()
	_, err := VerifySSHPodman(&ssh, executor)
	message := err.Error()

	if !strings.Contains(message, "SSH could not run the probes on dev@example.test") {
		t.Fatal(message)
	}
	if !strings.Contains(message, "Connection timed out") {
		t.Fatal(message)
	}
	if strings.Contains(message, "Podman 4.0.0") {
		t.Fatal(message)
	}
}

func TestSSHPodmanPreflightRejectsAnUnusableDestinationWithoutRunningSSH(t *testing.T) {
	executor := newPodmanPreflightExecutor()
	target := core.SshTarget{Destination: "--oProxyCommand=touch /tmp/pwn", SSHArgs: []string{}}

	_, err := VerifySSHPodman(&target, executor)

	if err == nil || !strings.Contains(err.Error(), "SSH destination is unusable") {
		t.Fatalf("%v", err)
	}
	if len(executor.seen) != 0 {
		t.Fatalf("%v", executor.seen)
	}
}

func TestSSHPodmanPreflightReportsEachFailingBatchedProbe(t *testing.T) {
	ssh := testSSH()
	missingPodman := newPodmanPreflightExecutor(batchedSSHProbes(
		ProbeFixture{"version", 127, "", "sh: podman: not found"},
	))
	_, err := VerifySSHPodman(&ssh, missingPodman)
	message := err.Error()
	if !strings.Contains(message, "Remote Podman preflight failed on dev@example.test") {
		t.Fatal(message)
	}
	if !strings.Contains(message, "Podman 4.0.0 or newer") {
		t.Fatal(message)
	}
	if !strings.Contains(message, "Podman reported: sh: podman: not found") {
		t.Fatal(message)
	}

	rootful := newPodmanPreflightExecutor(batchedSSHProbes(
		ProbeFixture{"version", 0, "podman version 5.4.2\n", ""},
		ProbeFixture{"info", 0, rootfulPodmanInfo(), ""},
	))
	_, err = VerifySSHPodman(&ssh, rootful)
	message = err.Error()
	if !strings.Contains(message, "Postcondition Podman is rootless was not met") {
		t.Fatal(message)
	}
	if !strings.Contains(message, "On dev@example.test: Run Mjolnir as the ordinary user") {
		t.Fatal(message)
	}

	uidMap := newPodmanPreflightExecutor(batchedSSHProbes(
		ProbeFixture{"version", 0, "podman version 5.4.2\n", ""},
		ProbeFixture{"info", 0, podmanInfoJSON(true, false, []uidMapping{{0, 1000, 1}}), ""},
	))
	_, err = VerifySSHPodman(&ssh, uidMap)
	message = err.Error()
	if !strings.Contains(message, "maps container UIDs 0 and 1 was not met") {
		t.Fatal(message)
	}
	if !strings.Contains(message, "On dev@example.test: Install UID-map helpers") {
		t.Fatal(message)
	}

	infoFailed := newPodmanPreflightExecutor(batchedSSHProbes(
		ProbeFixture{"version", 0, "podman version 5.4.2\n", ""},
		ProbeFixture{"info", 1, "", "cannot find newuidmap executable"},
	))
	_, err = VerifySSHPodman(&ssh, infoFailed)
	message = err.Error()
	if !strings.Contains(message, "Podman reported: cannot find newuidmap executable") {
		t.Fatal(message)
	}
	if !strings.Contains(message, "apt install -y uidmap") {
		t.Fatal(message)
	}
}

func TestSSHPodmanPreflightReportsUnparsableBatchedOutputAsATransportFailure(t *testing.T) {
	truncated := SSHPodmanProbeFixture([]ProbeFixture{{"version", 0, "podman version 5.4.2\n", ""}})
	// Truncated mid-probe: the version block never closes.
	truncated = truncated[:40]
	for _, stdout := range [][]byte{[]byte(""), []byte("nothing framed here\n"), truncated} {
		executor := newPodmanPreflightExecutor(core.CommandOutput{
			Status: 0,
			Stdout: stdout,
			Stderr: []byte("broken pipe"),
		})

		ssh := testSSH()
		_, err := VerifySSHPodman(&ssh, executor)
		message := err.Error()

		if !strings.Contains(message, "SSH could not run the probes on dev@example.test") {
			t.Fatal(message)
		}
		if !strings.Contains(message, "unparsable output") {
			t.Fatal(message)
		}
	}
}

func TestSSHPodmanPreflightWarnsWhenTheLingerValueIsUnrecognized(t *testing.T) {
	ssh := testSSH()
	unrecognized := newPodmanPreflightExecutor(passingSSHProbes(0, "maybe\n", ""))
	preflight, err := VerifySSHPodman(&ssh, unrecognized)
	if err != nil {
		t.Fatal(err)
	}
	if len(preflight.Warnings) != 1 {
		t.Fatalf("%v", preflight.Warnings)
	}
	if !strings.Contains(preflight.Warnings[0].Detail, `unrecognized Linger value "maybe"`) {
		t.Fatal(preflight.Warnings[0].Detail)
	}

	failed := newPodmanPreflightExecutor(passingSSHProbes(1, "", "Failed to connect to bus\n"))
	preflight, err = VerifySSHPodman(&ssh, failed)
	if err != nil {
		t.Fatal(err)
	}
	if len(preflight.Warnings) != 1 {
		t.Fatalf("%v", preflight.Warnings)
	}
	if !strings.Contains(preflight.Warnings[0].Detail, "`loginctl` exited with status 1: Failed to connect to bus") {
		t.Fatal(preflight.Warnings[0].Detail)
	}

	// Output that ends after the Podman probes still yields a usable target.
	missing := newPodmanPreflightExecutor(batchedSSHProbes(
		ProbeFixture{"version", 0, "podman version 5.4.2\n", ""},
		ProbeFixture{"info", 0, rootlessPodmanInfo(), ""},
	))
	preflight, err = VerifySSHPodman(&ssh, missing)
	if err != nil {
		t.Fatal(err)
	}
	if len(preflight.Warnings) != 1 {
		t.Fatalf("%v", preflight.Warnings)
	}
	if !strings.Contains(preflight.Warnings[0].Detail, "durability check is unavailable") {
		t.Fatal(preflight.Warnings[0].Detail)
	}
}

func TestBatchedPreflightParserKeepsMultilineAndNoisyProbeOutput(t *testing.T) {
	stdout := SSHPodmanProbeFixture([]ProbeFixture{
		{"version", 0, "podman version 5.4.2\n", "warning: one\nwarning: two\n"},
		{"info", 3, "{\n  \"host\": {}\n}\n", "no trailing newline"},
	})

	probes := parsePodmanProbeOutput(stdout)

	if len(probes) != 2 {
		t.Fatalf("%v", probes)
	}
	version := probes["version"]
	if version.Status != 0 {
		t.Fatal(version.Status)
	}
	if got := strings.TrimSpace(preflightLossy(version.Stdout)); got != "podman version 5.4.2" {
		t.Fatal(got)
	}
	if got := strings.TrimSpace(preflightLossy(version.Stderr)); got != "warning: one\nwarning: two" {
		t.Fatal(got)
	}
	info := probes["info"]
	if info.Status != 3 {
		t.Fatal(info.Status)
	}
	if got := len(preflightLines(preflightLossy(info.Stdout))); got != 3 {
		t.Fatal(got)
	}
	if got := strings.TrimSpace(preflightLossy(info.Stderr)); got != "no trailing newline" {
		t.Fatal(got)
	}
}

func TestManagedResourceIdentityArgsBuildContainerLabels(t *testing.T) {
	instance := core.InstanceIdentity()
	got := managedResourceIdentityArgs(managedResourceKindContainer, testSession)
	want := []string{
		"--label",
		"dev.mj.session=018f9dd2-a3b4-7c8d-9000-123456789abc",
		"--label",
		"dev.mj.managed=true",
		"--label",
		"dev.mj.instance=" + instance,
	}
	if !slices.Equal(got, want) {
		t.Fatalf("%q", got)
	}
}

func TestContainerTemplateOwnershipErrorsNameMjolnir(t *testing.T) {
	template := core.TargetsContainerTemplate{
		Image:        "ubuntu:24.04",
		PullPolicy:   core.ImagePullPolicyAuto,
		ExtraRunArgs: []string{"--label=dev.mj.managed=false"},
	}

	err := core.ValidateContainerTemplate(&template)
	if err == nil || err.Error() != "container template may not override Mjolnir ownership labels" {
		t.Fatalf("%v", err)
	}
}

// containerTemplate is a ContainerTemplate with no build cache.
func containerTemplate(image string, policy core.ImagePullPolicy, extraRunArgs []string, storage core.TargetsPodmanWorkspaceStorage) core.TargetsContainerTemplate {
	if extraRunArgs == nil {
		extraRunArgs = []string{}
	}
	return core.TargetsContainerTemplate{
		Image:            image,
		PullPolicy:       policy,
		ExtraRunArgs:     extraRunArgs,
		WorkspaceStorage: storage,
	}
}

func containerTarget(kind core.TargetsTargetTemplateKind, container core.TargetsContainerTemplate) *core.TargetsTargetTemplate {
	return &core.TargetsTargetTemplate{Kind: kind, Container: &container}
}

func sshContainerTarget(kind core.TargetsTargetTemplateKind, container core.TargetsContainerTemplate) *core.TargetsTargetTemplate {
	ssh := testSSH()
	return &core.TargetsTargetTemplate{Kind: kind, SSH: &ssh, Container: &container}
}

func localPodmanLocator(containerID string, storage core.TargetsPodmanWorkspaceLocator) *core.TargetsTargetLocator {
	return &core.TargetsTargetLocator{
		Kind:             core.TargetsTargetLocatorLocalPodman,
		ContainerID:      containerID,
		WorkspaceStorage: storage,
	}
}

func sshPodmanLocator(containerID string, storage core.TargetsPodmanWorkspaceLocator) *core.TargetsTargetLocator {
	ssh := testSSH()
	return &core.TargetsTargetLocator{
		Kind:             core.TargetsTargetLocatorSshPodman,
		SSH:              &ssh,
		ContainerID:      containerID,
		WorkspaceStorage: storage,
	}
}

func sshBareLocator(workspace string) *core.TargetsTargetLocator {
	ssh := testSSH()
	return &core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorSshBare, SSH: &ssh, Workspace: workspace}
}

func mustResourceName(t *testing.T, sessionID string) string {
	t.Helper()
	name, err := core.ResourceName(sessionID)
	if err != nil {
		t.Fatal(err)
	}
	return name
}

func lastArg(command *core.CommandSpec) string {
	return command.Args[len(command.Args)-1]
}

// containsWindow is Rust's `slice.windows(n).any(|w| w == window)`.
func containsWindow(arguments []string, window ...string) bool {
	for start := 0; start+len(window) <= len(arguments); start++ {
		if slices.Equal(arguments[start:start+len(window)], window) {
			return true
		}
	}
	return false
}

func stageOf(kind core.ProvisionStageKind) *core.ProvisionStage {
	return &core.ProvisionStage{Kind: kind}
}

func sameStage(got *core.ProvisionStage, want *core.ProvisionStage) bool {
	return reflect.DeepEqual(got, want)
}

func readFile(t *testing.T, path string) string {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return string(data)
}

func writeFile(t *testing.T, path string, content string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}

func mkdirAll(t *testing.T, path string) {
	t.Helper()
	if err := os.MkdirAll(path, 0o755); err != nil {
		t.Fatal(err)
	}
}

func exists(path string) bool {
	_, err := os.Lstat(path)
	return err == nil
}

// alternateErrorText renders err as anyhow's `{:#}`: each cause whose text a
// layer does not already include, such as the source a Refusal was attached
// to, follows it after ": ".
func alternateErrorText(err error) string {
	text := err.Error()
	for cause := errors.Unwrap(err); cause != nil; cause = errors.Unwrap(cause) {
		if !strings.Contains(text, cause.Error()) {
			text += ": " + cause.Error()
		}
	}
	return text
}

// pathWithPrefix is the test process's PATH with directory in front.
func pathWithPrefix(directory string) string {
	return directory + string(filepath.ListSeparator) + os.Getenv("PATH")
}
