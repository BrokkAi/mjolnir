// Shared setup of the ports of mj-worker-bin/tests: Cargo builds the
// `mj-worker` executable for integration tests and names it in
// CARGO_BIN_EXE_mj-worker; here TestMain builds it once, stamped as
// go/scripts/build.sh stamps it, beside a development-build marker so it
// refuses the default instance as every test build does.

package main

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// workerExecutable is the built worker (Rust's
// env!("CARGO_BIN_EXE_mj-worker")).
var workerExecutable string

// ioTimeout is mj-worker's test_support IO_TIMEOUT: interpreter startup and
// temporary-file I/O can exceed five seconds when worker suites run
// concurrently. This is a hang guard, not an application SLA.
const ioTimeout = 30 * time.Second

func TestMain(m *testing.M) {
	core.EnableTestFixtures()
	directory, err := buildWorkerExecutable()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	code := m.Run()
	_ = os.RemoveAll(directory)
	os.Exit(code)
}

// buildWorkerExecutable builds ./cmd/mj-worker into a fresh directory under
// go/bin, with the worker stamp go/scripts/build.sh gives it, and returns
// the directory.
func buildWorkerExecutable() (string, error) {
	module, err := filepath.Abs(filepath.Join("..", ".."))
	if err != nil {
		return "", err
	}
	git := func(args ...string) (string, error) {
		command := exec.Command("git", args...)
		command.Dir = module
		output, err := command.Output()
		if err != nil {
			return "", fmt.Errorf("git %s: %w", strings.Join(args, " "), err)
		}
		return strings.TrimSpace(string(output)), nil
	}
	revision, err := git("rev-parse", "HEAD")
	if err != nil {
		return "", err
	}
	// The worker input digest is the Git tree of this Go module, as in
	// go/scripts/build.sh.
	inputs, err := git("rev-parse", "HEAD:./")
	if err != nil {
		return "", err
	}
	bin := filepath.Join(module, "bin")
	if err := os.MkdirAll(bin, 0o777); err != nil {
		return "", err
	}
	directory, err := os.MkdirTemp(bin, "mj-worker-test-")
	if err != nil {
		return "", err
	}
	if err := os.Mkdir(filepath.Join(directory, ".mj-development-build"), 0o777); err != nil {
		return "", err
	}
	stamp := filepath.Join(directory, "stamp.go")
	if err := os.WriteFile(stamp, []byte(fmt.Sprintf(
		"package buildid\n\nconst workerBuildStampLiteral = \"\\x00MJ-WORKER-BUILD:%s+%s\\x00\"\n",
		core.PackageVersion, inputs)), 0o666); err != nil {
		return "", err
	}
	overlay := filepath.Join(directory, "overlay.json")
	if err := os.WriteFile(overlay, []byte(fmt.Sprintf(`{"Replace":{%q:%q}}`,
		filepath.Join(module, "internal", "buildid", "stamp.go"), stamp)), 0o666); err != nil {
		return "", err
	}
	const buildid = "github.com/BrokkAi/mjolnir/go/internal/buildid"
	executable := filepath.Join(directory, "mj-worker")
	command := exec.Command("go", "build", "-buildvcs=false", "-overlay", overlay,
		"-ldflags", fmt.Sprintf("-X '%s.buildRevision=%s' -X '%s.workerInputsID=%s'", buildid, revision, buildid, inputs),
		"-o", executable, "./cmd/mj-worker")
	command.Dir = module
	if output, err := command.CombinedOutput(); err != nil {
		return "", fmt.Errorf("build the worker: %w\n%s", err, output)
	}
	workerExecutable = executable
	return directory, nil
}
