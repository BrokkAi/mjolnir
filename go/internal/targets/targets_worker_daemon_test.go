// Port of the tests in mj-targets/src/targets/worker_daemon.rs.

package targets

import (
	"bytes"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Hard-won: #1550: an SSH root with an apostrophe hid the control path from the text scan, so staging was skipped
func TestSSHControlPlansPreserveQuotedRootStaging(t *testing.T) {
	const session = "0123456789abcdef"
	locator := &core.TargetsTargetLocator{
		Kind:      core.TargetsTargetLocatorSshBare,
		SSH:       &core.SshTarget{Destination: "example.invalid", SSHArgs: []string{}},
		Workspace: ".local/share/hel/workspaces/" + session,
	}
	// SSH locators currently derive a restricted root from the session ID.
	// Exercise arbitrary root text at the script/transport boundary instead.
	root := ".local/share/hel/worker's space/" + session
	closeScript := KillWorkerDaemonScript(root).MapScript(func(kill string) string {
		return kill + "\nrm -rf -- " + core.PosixQuote(root) + "\n"
	})
	for _, script := range []ControlScript{
		StopWorkerDaemonScript(root),
		closeScript,
		WorkerDaemonLivenessScript(root),
	} {
		command := script.IntoCommand(func(script string) core.CommandSpec {
			return core.SSHCommandOwned(core.SshTrafficClassControl, locator.SSH, []string{"sh", "-c", script})
		})
		plan, err := NewWorkerControlPlan("quoted SSH control", []ControlCommand{command}, locator, session)
		if err != nil {
			t.Fatal(err)
		}
		if plan.commands[0].Program != "ssh" {
			t.Fatalf("program = %q", plan.commands[0].Program)
		}
		if !plan.StagesControlWorker() {
			t.Fatal("the quoted root's plan does not stage the control worker")
		}
		if plan.control == nil || !libSamePath(plan.control.root, root) {
			t.Fatalf("control = %+v, want root %q", plan.control, root)
		}
		if _, err := plan.CommandsWithoutStaging(); err == nil {
			t.Fatal("a staging plan handed out its commands")
		}
	}
	// Actual locator plans also retain the builder's requirement.
	expected, err := core.WorkerRoot(locator, session)
	if err != nil {
		t.Fatal(err)
	}
	stop, err := StopWorkerPlan(locator, session, "stop")
	if err != nil {
		t.Fatal(err)
	}
	closing, err := ClosePlan(locator, session)
	if err != nil {
		t.Fatal(err)
	}
	liveness, err := NewWorkerControlPlan("liveness", []ControlCommand{
		WorkerDaemonLivenessScript(expected).IntoCommand(func(script string) core.CommandSpec {
			return core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"sh", "-c", script})
		}),
	}, locator, session)
	if err != nil {
		t.Fatal(err)
	}
	for _, plan := range []WorkerControlPlan{stop, closing, liveness} {
		if plan.control == nil || !libSamePath(plan.control.root, expected) {
			t.Fatalf("control = %+v, want root %q", plan.control, expected)
		}
	}
}

// runWorkerDaemonScript runs a probe script with sh (Unix only, as the one
// Rust case that uses it).
func runWorkerDaemonScript(t *testing.T, script string) (int, string) {
	t.Helper()
	var stdout bytes.Buffer
	command := exec.Command("sh", "-c", script)
	command.Stdout = &stdout
	err := command.Run()
	var exited *exec.ExitError
	if err != nil && !errors.As(err, &exited) {
		t.Fatalf("run the probe script: %v", err)
	}
	return command.ProcessState.ExitCode(), strings.TrimSpace(stdout.String())
}

// Hard-won: c29b1379c: an in-place Move's relay reset made review rescan the checkout and refuse it.
func TestInPlaceResetPreservesTheReviewUntrackedRecord(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Unix only, as in Rust")
	}
	root := filepath.Join(t.TempDir(), "0123456789abcdef")
	if err := os.MkdirAll(filepath.Join(root, "profile"), 0o777); err != nil {
		t.Fatal(err)
	}
	record := filepath.Join(root, core.ReviewUntrackedFile)
	if err := os.WriteFile(record, []byte("original review record"), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(root, core.RelayStateFile), []byte("{}"), 0o666); err != nil {
		t.Fatal(err)
	}
	plan, err := InPlaceWorkerResetPlan(
		&core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: root},
		"0123456789abcdef",
		filepath.Join(root, "profile"),
	)
	if err != nil {
		t.Fatal(err)
	}
	status, stdout := runWorkerDaemonScript(t, plan.commands[0].Args[1])
	if status != 0 {
		t.Fatalf("status %d: %s", status, stdout)
	}
	if contents, err := os.ReadFile(record); err != nil || string(contents) != "original review record" {
		t.Fatalf("review record = %q, %v", contents, err)
	}
	if _, err := os.Stat(filepath.Join(root, core.RelayStateFile)); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("relay state survived: %v", err)
	}
	if _, err := os.Stat(filepath.Join(root, "profile")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("previous profile survived: %v", err)
	}
}
