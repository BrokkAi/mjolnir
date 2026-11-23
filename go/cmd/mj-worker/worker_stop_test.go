// Port of mj-worker-bin/tests/worker_stop.rs: `hel worker
// liveness|stop|holder` against real processes: a real worker, and
// stand-ins that hold the real root lock the way a worker does.

//go:build linux || darwin

package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
	"github.com/BrokkAi/mjolnir/go/internal/worker"
)

const stopTestSession = "018f9dd2-a3b4-7c8d-9000-123456789abc"

// helOutput is what one run of the worker printed and how it ended.
type helOutput struct {
	status int
	stdout []byte
	stderr []byte
}

func (o helOutput) success() bool { return o.status == 0 }

func (o helOutput) String() string {
	return fmt.Sprintf("status %d, stdout %q, stderr %q", o.status, o.stdout, o.stderr)
}

// run runs command to completion, capturing both streams.
func run(t *testing.T, command *exec.Cmd) helOutput {
	t.Helper()
	var stdout, stderr bytes.Buffer
	command.Stdout, command.Stderr = &stdout, &stderr
	err := command.Run()
	var exit *exec.ExitError
	if err != nil && !errors.As(err, &exit) {
		t.Fatalf("run %s: %v", command.Path, err)
	}
	return helOutput{status: command.ProcessState.ExitCode(), stdout: stdout.Bytes(), stderr: stderr.Bytes()}
}

func hel(t *testing.T, args []string, root string, extra ...string) (helOutput, time.Duration) {
	t.Helper()
	started := time.Now()
	arguments := append(append(append([]string{"worker"}, args...), "--root", root), extra...)
	output := run(t, exec.Command(workerExecutable, arguments...))
	return output, time.Since(started)
}

func token(output helOutput) string { return strings.TrimSpace(string(output.stdout)) }

func gone(t *testing.T, pid int) bool {
	t.Helper()
	state := strings.TrimSpace(string(run(t, exec.Command("ps", "-o", "stat=", "-p", strconv.Itoa(pid))).stdout))
	return state == "" || strings.HasPrefix(state, "Z")
}

func waitFor(t *testing.T, path string) {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for {
		if _, err := os.Stat(path); err == nil {
			return
		}
		if !time.Now().Before(deadline) {
			t.Fatalf("%s never appeared", path)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

// cleanup kills what a failed assertion would otherwise leave running.
func cleanup(t *testing.T, groups []int, pidFiles []string) {
	t.Cleanup(func() {
		for _, file := range pidFiles {
			if text, err := os.ReadFile(file); err == nil {
				if pid, err := strconv.Atoi(strings.TrimSpace(string(text))); err == nil {
					_ = syscall.Kill(pid, syscall.SIGKILL)
				}
			}
		}
		for _, group := range groups {
			_ = syscall.Kill(-group, syscall.SIGKILL)
		}
	})
}

// lockHolderWithDetachedDescendant is a process that takes the root's real
// lock as a worker does, keeps it for as long as it lives, and starts a
// descendant that leaves its group and session and ignores TERM, as a
// harness helper can.
func lockHolderWithDetachedDescendant(t *testing.T, root string) *exec.Cmd {
	t.Helper()
	const script = `
import fcntl, os, subprocess, sys, time
root = sys.argv[1]
lock = open(os.path.join(root, "worker.lock"), "w")
fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
subprocess.Popen(
    ["sh", "-c", "trap '' TERM; echo $$ > descendant; while :; do sleep 0.05; done"],
    cwd=root, start_new_session=True)
while not os.path.exists(os.path.join(root, "descendant")):
    time.sleep(0.01)
open(os.path.join(root, "ready"), "w").write("1")
while True:
    time.sleep(1)
`
	command := exec.Command("python3", "-c", script, root)
	command.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := core.SpawnChild(command); err != nil {
		t.Fatalf("start lock holder: %v", err)
	}
	return command
}

func recordWorker(t *testing.T, root string, pid int, birth string) {
	t.Helper()
	data, err := json.Marshal(map[string]any{"pid": pid, "process_birth": birth})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(root, core.WorkerStartupFile), data, 0o666); err != nil {
		t.Fatal(err)
	}
}

func processBirth(t *testing.T, pid int) string {
	t.Helper()
	birth, err := core.ProcessBirthIdentity(uint32(pid))
	if err != nil {
		t.Fatal(err)
	}
	return birth
}

// exited reports whether command's process has exited, reaping it then.
func exited(command *exec.Cmd) bool {
	var status syscall.WaitStatus
	pid, err := syscall.Wait4(command.Process.Pid, &status, syscall.WNOHANG, nil)
	return err == nil && pid == command.Process.Pid
}

func startRealWorker(t *testing.T, root, scratch string) *exec.Cmd {
	t.Helper()
	if _, err := worker.DurableRelayOpen(root, stopTestSession, "test"); err != nil {
		t.Fatal(err)
	}
	config := filepath.Join(scratch, "launch.json")
	writeJSON(t, config, map[string]any{
		"run_mode":           "checkpoint_only",
		"session_id":         stopTestSession,
		"harness":            "codex",
		"bridge_command":     "/missing-harness",
		"bridge_args":        []string{},
		"environment":        map[string]any{"CODEX_HOME": filepath.Join(scratch, "profile")},
		"target_environment": map[string]any{"MJ_INSTANCE": "qa-worker-stop"},
		"cwd":                scratch,
		"execution_policy":   "configured_approvals",
	})
	command := exec.Command(workerExecutable, "worker", "run", "--root", root, "--config", config)
	command.Env = append(os.Environ(), "MJ_INSTANCE=qa-worker-stop")
	if err := core.SpawnChild(command); err != nil {
		t.Fatalf("start worker: %v", err)
	}
	return command
}

func writeJSON(t *testing.T, path string, value any) {
	t.Helper()
	data, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, data, 0o666); err != nil {
		t.Fatal(err)
	}
}

func acquireAndRelease(t *testing.T, root string) {
	t.Helper()
	owner, err := worker.WorkerRootOwnerAcquire(root)
	if err != nil {
		t.Fatalf("the lock is free after the stop: %v", err)
	}
	owner.Release()
}

// Stopping a real worker frees its lock, leaves no process, and a second
// stop of the now-dead root is an immediate no-op.
func TestStoppingALiveWorkerFreesItsLockAndLeavesNoProcess(t *testing.T) {
	scratch := t.TempDir()
	root := filepath.Join(scratch, "worker")
	running := startRealWorker(t, root, scratch)
	cleanup(t, []int{running.Process.Pid}, nil)
	waitFor(t, filepath.Join(root, "control.sock"))
	if output, _ := hel(t, []string{"liveness"}, root); token(output) != "alive" {
		t.Fatalf("liveness %v", output)
	}
	if output, _ := hel(t, []string{"holder"}, root); token(output) != strconv.Itoa(running.Process.Pid) {
		t.Fatalf("holder %v", output)
	}

	stop, took := hel(t, []string{"stop"}, root)
	t.Logf("live worker stop took %v", took)
	if !stop.success() {
		t.Fatal(stop)
	}
	// The worker is a child of this test, so a reaped exit proves it is
	// gone.
	_ = running.Wait()
	acquireAndRelease(t, root)
	if output, _ := hel(t, []string{"liveness"}, root); token(output) != "dead" {
		t.Fatalf("liveness %v", output)
	}

	again, took := hel(t, []string{"stop"}, root)
	t.Logf("dead worker stop took %v", took)
	if !again.success() {
		t.Fatal(again)
	}
	if took >= time.Second {
		t.Fatalf("stopping a dead root took %v", took)
	}
}

// The harness and its helpers lead groups and sessions of their own, so the
// worker's group can empty while they still write into the root. The stop
// must take them down too, with or without the TERM grace, before it
// returns.
// Hard-won: 2f859c8f: a harness outlived its worker group and raced removal of staged state.
func TestStoppingAWorkerKillsADescendantThatLeftItsProcessGroup(t *testing.T) {
	for _, kill := range []bool{false, true} {
		root := t.TempDir()
		holder := lockHolderWithDetachedDescendant(t, root)
		cleanup(t, []int{holder.Process.Pid}, []string{filepath.Join(root, "descendant")})
		waitFor(t, filepath.Join(root, "ready"))
		text, err := os.ReadFile(filepath.Join(root, "descendant"))
		if err != nil {
			t.Fatal(err)
		}
		descendant, err := strconv.Atoi(strings.TrimSpace(string(text)))
		if err != nil {
			t.Fatal(err)
		}
		recordWorker(t, root, holder.Process.Pid, processBirth(t, holder.Process.Pid))
		if output, _ := hel(t, []string{"liveness"}, root); token(output) != "starting" {
			t.Fatalf("liveness %v", output)
		}

		var extra []string
		if kill {
			extra = []string{"--kill"}
		}
		stop, took := hel(t, []string{"stop"}, root, extra...)
		t.Logf("detached-descendant stop (kill=%v) took %v", kill, took)
		if !stop.success() {
			t.Fatal(stop)
		}
		_ = holder.Wait()
		if !gone(t, descendant) {
			t.Fatalf("the descendant survived (kill=%v)", kill)
		}
		acquireAndRelease(t, root)
	}
}

// A pid in a startup record is not an identity. Only the recorded birth
// makes the record the holder's, so an unrelated live process that happens
// to carry the recorded pid is never signalled, and the stop reports the
// failure.
func TestARecycledPidInTheStartupRecordIsNeverSignalled(t *testing.T) {
	root := t.TempDir()
	holder := lockHolderWithDetachedDescendant(t, root)
	unrelated := exec.Command("sleep", "60")
	unrelated.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := core.SpawnChild(unrelated); err != nil {
		t.Fatal(err)
	}
	cleanup(t, []int{holder.Process.Pid, unrelated.Process.Pid}, []string{filepath.Join(root, "descendant")})
	waitFor(t, filepath.Join(root, "ready"))
	recordWorker(t, root, unrelated.Process.Pid, "a-previous-process-birth")

	stop, _ := hel(t, []string{"stop"}, root)

	if stop.success() {
		t.Fatal(stop)
	}
	if !strings.Contains(string(stop.stderr), "no startup record identifies it") {
		t.Fatal(stop)
	}
	if exited(unrelated) {
		t.Fatal("the unrelated process was signalled")
	}
	if exited(holder) {
		t.Fatal("the lock holder was signalled")
	}
	_ = unrelated.Process.Kill()
	_ = unrelated.Wait()
}

// A root with no worker answers `dead` and stops at once, whether the lock
// file never existed or a previous worker left it behind.
func TestARootWithoutAWorkerIsDeadAndStopsAtOnce(t *testing.T) {
	root := t.TempDir()
	for _, lockFileExists := range []bool{false, true} {
		if lockFileExists {
			if err := os.WriteFile(filepath.Join(root, "worker.lock"), nil, 0o666); err != nil {
				t.Fatal(err)
			}
		}
		if liveness, _ := hel(t, []string{"liveness"}, root); token(liveness) != "dead" {
			t.Fatalf("liveness %v", liveness)
		}
		if holder, _ := hel(t, []string{"holder"}, root); token(holder) != "" {
			t.Fatalf("holder %v", holder)
		}
		stop, took := hel(t, []string{"stop"}, root)
		if !stop.success() {
			t.Fatal(stop)
		}
		if took >= time.Second {
			t.Fatalf("took %v", took)
		}
	}
	missing := filepath.Join(root, "never-created")
	if liveness, _ := hel(t, []string{"liveness"}, missing); token(liveness) != "dead" {
		t.Fatalf("liveness %v", liveness)
	}
}

// The controller's control scripts choose a binary in the root by probing
// it, so they are exercised here against the real current binary and, when
// one is supplied, a real released one.

// runScript runs a control script the way the controller does and
// describes the result.
func runScript(t *testing.T, script string) (int, string, string) {
	t.Helper()
	output := run(t, exec.Command("sh", "-c", script))
	if output.status < 0 {
		t.Fatal("the script was signalled")
	}
	return output.status, string(output.stdout), string(output.stderr)
}

// installExecutable puts data at target as an executable by renaming a new
// file over it. Rust copies over `hel` in place; a cgo-linked Go
// executable stays busy (ETXTBSY) for a moment after it exits on Linux, so
// writing into the worker that just ran fails where replacing it does not.
func installExecutable(t *testing.T, target string, data []byte) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(target), 0o777); err != nil {
		t.Fatal(err)
	}
	staged := target + ".installing"
	if err := os.WriteFile(staged, data, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(staged, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(staged, target); err != nil {
		t.Fatal(err)
	}
}

func installHel(t *testing.T, root, source string) {
	t.Helper()
	data, err := os.ReadFile(source)
	if err != nil {
		t.Fatal(err)
	}
	installExecutable(t, filepath.Join(root, "hel"), data)
}

func installOldHel(t *testing.T, root string) {
	t.Helper()
	// clap's own rejection of a subcommand the build predates.
	installExecutable(t, filepath.Join(root, "hel"), []byte("#!/bin/sh\necho \"error: unrecognized subcommand '$2'\" >&2\nexit 2\n"))
}

// Names, arguments, stdout tokens and exit statuses of the control
// commands, and the scripts that choose a binary for them, compared with a
// checked-in file. A change here is a change to the contract in
// worker_stop.go.
func TestControlContractGolden(t *testing.T) {
	scratch := t.TempDir()
	root := filepath.Join(scratch, "worker root")
	var out strings.Builder
	mask := func(text string) string { return strings.ReplaceAll(text, root, "<ROOT>") }
	scripts := []struct {
		name   string
		script targets.ControlScript
	}{
		{"liveness", targets.WorkerDaemonLivenessScript(root)},
		{"holder", targets.WorkerDaemonHolderScript(root)},
		{"stop", targets.StopWorkerDaemonScript(root)},
		{"stop --kill", targets.KillWorkerDaemonScript(root)},
	}
	for index := range scripts {
		fmt.Fprintf(&out, "== script: %s\n%s\n", scripts[index].name, mask(scripts[index].script.UnstagedScript()))
	}
	record := func(title, script string) {
		status, stdout, stderr := runScript(t, script)
		fmt.Fprintf(&out, "== run: %s\nstatus: %d\nstdout: %s\nstderr: %s\n",
			title, status, core.RustDebugString(mask(stdout)), core.RustDebugString(mask(stderr)))
	}

	// `--help` on each contract subcommand, as the probe asks.
	for _, subcommand := range []string{"liveness", "holder", "stop"} {
		status := run(t, exec.Command(workerExecutable, "worker", subcommand, "--help")).status
		record("hel worker "+subcommand+" --help", fmt.Sprintf(
			"%s worker %s --help >/dev/null 2>&1; echo \"exit $?\" >&2; exit %d",
			core.RustDebugString(workerExecutable), subcommand, status))
	}
	// A root that never ran a worker has no `hel` and answers for itself.
	if err := os.MkdirAll(root, 0o777); err != nil {
		t.Fatal(err)
	}
	for index := range scripts {
		record("no hel: "+scripts[index].name, scripts[index].script.UnstagedScript())
	}
	// A root whose `hel` is the current build, no worker running.
	installHel(t, root, workerExecutable)
	for index := range scripts {
		record("current hel, dead: "+scripts[index].name, scripts[index].script.UnstagedScript())
	}
	// A root whose `hel` predates the subcommands and nothing else serves.
	installOldHel(t, root)
	for index := range scripts {
		record("old hel: "+scripts[index].name, scripts[index].script.UnstagedScript())
	}
	// A worker holds the lock; the current build answers for it.
	installHel(t, root, workerExecutable)
	holder := lockHolderWithDetachedDescendant(t, root)
	cleanup(t, []int{holder.Process.Pid}, []string{filepath.Join(root, "descendant")})
	waitFor(t, filepath.Join(root, "ready"))
	recordWorker(t, root, holder.Process.Pid, processBirth(t, holder.Process.Pid))
	holderPid := strconv.Itoa(holder.Process.Pid)
	pidMasked := func(script string) (int, string) {
		status, stdout, _ := runScript(t, script)
		return status, strings.ReplaceAll(strings.TrimSpace(stdout), holderPid, "<PID>")
	}
	for _, step := range []struct {
		title  string
		script int
	}{{"held lock: liveness", 0}, {"held lock: holder", 1}, {"held lock: stop", 2}} {
		status, stdout := pidMasked(scripts[step.script].script.UnstagedScript())
		fmt.Fprintf(&out, "== run: %s\nstatus: %d\nstdout: %s\n", step.title, status, core.RustDebugString(stdout))
	}
	_ = holder.Wait()
	status, stdout := pidMasked(scripts[0].script.UnstagedScript())
	fmt.Fprintf(&out, "== run: after stop: liveness\nstatus: %d\nstdout: %s\n", status, core.RustDebugString(stdout))

	core.AssertGolden(t, "testdata", "worker_control_contract", out.String())
}

// A build whose interface the root already has stops its own worker: no
// control worker is staged beside it and nothing else appears in the root.
func TestACurrentRootStopsThroughItsOwnHelAndStagesNothing(t *testing.T) {
	scratch := t.TempDir()
	root := filepath.Join(scratch, "worker")
	installHel(t, root, workerExecutable)
	holder := lockHolderWithDetachedDescendant(t, root)
	cleanup(t, []int{holder.Process.Pid}, []string{filepath.Join(root, "descendant")})
	waitFor(t, filepath.Join(root, "ready"))
	recordWorker(t, root, holder.Process.Pid, processBirth(t, holder.Process.Pid))
	names := func() []string {
		entries, err := os.ReadDir(root)
		if err != nil {
			t.Fatal(err)
		}
		var names []string
		for _, entry := range entries {
			names = append(names, entry.Name())
		}
		slices.Sort(names)
		return names
	}
	before := names()
	stop := targets.StopWorkerDaemonScript(root)
	status, _, stderr := runScript(t, stop.UnstagedScript())
	if status != 0 {
		t.Fatal(stderr)
	}
	_ = holder.Wait()
	after := names()
	for _, name := range after {
		if strings.HasPrefix(name, "hel-control") {
			t.Fatalf("a control worker was staged: %v", after)
		}
	}
	if !slices.Equal(before, after) {
		t.Fatalf("the stop changed the root: %v then %v", before, after)
	}
}

// A root whose `hel` is a released build without the subcommands (v2.71.0,
// which exits 2 for them) cannot serve them: the script says so with status
// 78 and does nothing. Once the controller has staged a current worker,
// that one serves, and the released `hel` is left untouched.
//
// Needs a real released worker for this host. Download one and run:
//
//	gh release download v2.71.0 -R BrokkAi/mjolnir -p '*x86_64-unknown-linux-gnu.tar.gz'
//	tar xzf brokk-mjolnir-v2.71.0-x86_64-unknown-linux-gnu.tar.gz
//	MJ_RELEASED_WORKER=$PWD/brokk-mjolnir-v2.71.0-x86_64-unknown-linux-gnu/mj-worker-x86_64-unknown-linux-musl \
//	  go test ./cmd/mj-worker -run TestAReleasedRoot
//
// Hard-won: #1573: a daemon that required its own build in every root published it to the target before the first stop.
func TestAReleasedRootWithALiveWorkerIsStoppedByTheStagedControlWorker(t *testing.T) {
	released, ok := os.LookupEnv("MJ_RELEASED_WORKER")
	if !ok {
		t.Skip("needs MJ_RELEASED_WORKER=<path of a released mj-worker without worker liveness|holder|stop>")
	}
	// The premise: the released build rejects the subcommands, and the
	// current build accepts them.
	for _, subcommand := range []string{"liveness", "holder", "stop"} {
		if output := run(t, exec.Command(released, "worker", subcommand, "--help")); output.status != 2 {
			t.Fatalf("%s: %v", subcommand, output)
		}
		if output := run(t, exec.Command(workerExecutable, "worker", subcommand, "--help")); !output.success() {
			t.Fatalf("%s: %v", subcommand, output)
		}
	}
	scratch := t.TempDir()
	root := filepath.Join(scratch, "worker")
	installHel(t, root, released)
	releasedBytes, err := os.ReadFile(filepath.Join(root, "hel"))
	if err != nil {
		t.Fatal(err)
	}
	holder := lockHolderWithDetachedDescendant(t, root)
	cleanup(t, []int{holder.Process.Pid}, []string{filepath.Join(root, "descendant")})
	waitFor(t, filepath.Join(root, "ready"))
	recordWorker(t, root, holder.Process.Pid, processBirth(t, holder.Process.Pid))
	stop := targets.StopWorkerDaemonScript(root)
	// Nothing in the root serves, so nothing ran and the worker is alive.
	status, stdout, _ := runScript(t, stop.UnstagedScript())
	if status != targets.WorkerControlUnstagedStatus || stdout != "" {
		t.Fatalf("status %d stdout %q", status, stdout)
	}
	if exited(holder) {
		t.Fatal("the holder stopped")
	}
	// The controller stages the current worker under the stable name.
	data, err := os.ReadFile(workerExecutable)
	if err != nil {
		t.Fatal(err)
	}
	installExecutable(t, filepath.Join(root, "hel-control"), data)
	liveness := targets.WorkerDaemonLivenessScript(root)
	status, stdout, _ = runScript(t, liveness.UnstagedScript())
	if status != 0 || strings.TrimSpace(stdout) != "starting" {
		t.Fatalf("status %d stdout %q", status, stdout)
	}
	status, stdout, stderr := runScript(t, stop.UnstagedScript())
	if status != 0 || stdout != "" {
		t.Fatalf("status %d stdout %q stderr %s", status, stdout, stderr)
	}
	_ = holder.Wait()
	acquireAndRelease(t, root)
	if current, err := os.ReadFile(filepath.Join(root, "hel")); err != nil || !bytes.Equal(current, releasedBytes) {
		t.Fatal("the released hel changed")
	}
}

// Hard-won: #1603: a worker built with test features carried the test
// identity's build stamp beside its own, and was refused as conflicting.
func TestTheWorkerExecutableCarriesOneBuildStamp(t *testing.T) {
	data, err := os.ReadFile(workerExecutable)
	if err != nil {
		t.Fatal(err)
	}
	build, err := core.WorkerBuildFromBytes(data)
	if err != nil {
		t.Fatal(err)
	}
	if strings.HasSuffix(build, "+0000000000000000000000000000000000000000") {
		t.Fatalf("the worker carries the test identity's stamp, not its own: %s", build)
	}
}
