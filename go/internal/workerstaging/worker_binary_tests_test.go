// Port of mj-worker-staging/src/worker_binary/tests.rs, part 1: the portable
// tests up to the worker source pins. The Unix-only tests are in
// worker_binary_tests_unix_test.go.

package workerstaging

import (
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// testsModule is the Rust module path these tests are named under, for
// targets.TestName.
const testsModule = "mj_worker_staging::worker_binary::tests"

func writeFixture(t *testing.T, path string, contents []byte) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, contents, 0o666); err != nil {
		t.Fatal(err)
	}
}

func makeFixtureDir(t *testing.T, path string) {
	t.Helper()
	if err := os.MkdirAll(path, 0o777); err != nil {
		t.Fatal(err)
	}
}

func readFixture(t *testing.T, path string) string {
	t.Helper()
	contents, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return string(contents)
}

func fixtureExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

func requireErrorContains(t *testing.T, err error, expected ...string) {
	t.Helper()
	if err == nil {
		t.Fatalf("expected an error containing %q", expected)
	}
	for _, text := range expected {
		if !strings.Contains(err.Error(), text) {
			t.Fatalf("%q does not contain %q", err.Error(), text)
		}
	}
}

// The worker writes each harness's GitHub CLI approval into a file inside
// the session's staged profile, so the profile staging allowlist must carry
// that file from the profile home. A harness that names a new file in
// HarnessKind.GhApproval without adding it there starts its sessions
// without the approval.
func TestEveryStagedGhApprovalFileIsCopiedByProfileStaging(t *testing.T) {
	for _, harness := range core.HarnessKindAll {
		approval, ok := harness.GhApproval().(core.GhApprovalStagedFile)
		if !ok {
			continue
		}
		path := approval.Value
		entries := ProfileConfigurationPaths(harness)
		if !slices.ContainsFunc(entries, func(entry string) bool {
			rest, found := strings.CutPrefix(path, entry)
			return path == entry || found && strings.HasPrefix(rest, "/")
		}) {
			t.Fatalf("%s gh approval %s is not staged: %q", harness.DisplayName(), path, entries)
		}
	}
}

func TestOracleLaunchRequiresATopLevelSessionAndAnEffectiveChoice(t *testing.T) {
	session := store.CheckpointTestSession("primary")
	choices := core.DefaultSubagentConfig()
	effort := "high"
	choice := core.SubagentChoice{Model: "strong", Effort: &effort}
	session.Oracle = true
	if OracleLaunchEnabled(&session, true, &choices) {
		t.Fatal("no choice enabled the oracle")
	}
	choices.Oracle = []core.SubagentChoice{choice}
	if !OracleLaunchEnabled(&session, true, &choices) {
		t.Fatal("a configured choice did not enable the oracle")
	}
	if OracleLaunchEnabled(&session, false, &choices) {
		t.Fatal("a child got the oracle")
	}
	session.Oracle = false
	if OracleLaunchEnabled(&session, true, &choices) {
		t.Fatal("a session without the oracle got it")
	}
	session.Oracle = true
	session.OracleChoice = &choice
	choices.Oracle = nil
	if !OracleLaunchEnabled(&session, true, &choices) {
		t.Fatal("the session's own choice did not enable the oracle")
	}
}

func TestSkipsStaleAndUnstampedCandidatesAndReportsThemWhenNoneMatch(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("#1300: fails on a Windows controller")
	}
	directory := t.TempDir()
	controller := filepath.Join(directory, "target/debug/mj")
	stale := filepath.Join(filepath.Dir(controller), "mj-worker-x86_64-unknown-linux-musl")
	legacy := filepath.Join(directory, "target/worker/x86_64-unknown-linux-musl/debug/mj-worker")
	matching := filepath.Join(directory, "target/x86_64-unknown-linux-musl/debug/mj-worker")
	for _, path := range []string{controller, stale, legacy, matching} {
		writeFixture(t, path, []byte("unstamped"))
	}
	staleStamp := "\x00MJ-WORKER-BUILD:2.0.0+" + strings.Repeat("a", 40) + "\x00"
	writeFixture(t, stale, []byte(staleStamp))
	writeFixture(t, matching, StampedWorker([]byte("current")))
	resolve := func() (WorkerBinaryAvailability, error) {
		return WorkerBinaryPrerequisiteForCurrent("x86_64", WorkerBinaryRequirementPortableLinux, controller, isFile)
	}
	selected, err := resolve()
	if err != nil {
		t.Fatal(err)
	}
	if local, ok := selected.(WorkerBinaryAvailabilityLocal); !ok || local.Path != matching {
		t.Fatalf("selected %#v", selected)
	}
	if err := os.Remove(matching); err != nil {
		t.Fatal(err)
	}
	_, err = resolve()
	requireErrorContains(t, err,
		stale,
		legacy,
		"2.0.0+",
		core.WorkerBuildID(),
		"missing worker build stamp",
		"cargo build --target x86_64-unknown-linux-musl",
	)
}

// Hard-won: 0cc3263: an explicit stale worker override previously fell through and replaced the named worker
func TestAStaleWorkerBinaryOverrideFailsInsteadOfFallingBack(t *testing.T) {
	const child = "MJ_STALE_WORKER_OVERRIDE_CHILD"
	if _, ok := os.LookupEnv(child); !ok {
		directory := t.TempDir()
		worker := filepath.Join(directory, "override")
		writeFixture(t, worker, []byte("legacy override"))
		workers := filepath.Join(directory, "workers")
		writeFixture(t, filepath.Join(workers, "mj-worker-x86_64-unknown-linux-musl"), StampedWorker([]byte("current")))
		targets.NewIsolatedTest(targets.TestName(testsModule, "TestAStaleWorkerBinaryOverrideFailsInsteadOfFallingBack")).
			IsolatedStore(directory).
			Env("MJ_INSTANCE", "issue-1138-override").
			Env(child, "1").
			Env("MJ_WORKER_BINARY", worker).
			Env("MJ_WORKER_DIR", workers).
			Run()
		return
	}
	// A matching worker in MJ_WORKER_DIR must not replace the named
	// override.
	_, err := WorkerBinaryPrerequisiteForArch("x86_64")
	requireErrorContains(t, err, "MJ_WORKER_BINARY does not match", "override", "missing worker build stamp")
}

func TestStalePinsAreReResolved(t *testing.T) {
	const child = "MJ_STALE_WORKER_PIN_CHILD"
	if _, ok := os.LookupEnv(child); !ok {
		directory := t.TempDir()
		workers := filepath.Join(directory, "workers")
		writeFixture(t, filepath.Join(workers, "mj-worker-x86_64-unknown-linux-musl"), StampedWorker([]byte("current")))
		targets.NewIsolatedTest(targets.TestName(testsModule, "TestStalePinsAreReResolved")).
			IsolatedStore(directory).
			Env("MJ_INSTANCE", "issue-1138-pins").
			Env(child, "1").
			Env("MJ_WORKER_DIR", workers).
			EnvRemove("MJ_WORKER_BINARY").
			Run()
		return
	}
	selected, err := WorkerBinaryPrerequisiteForArch("x86_64")
	if err != nil {
		t.Fatal(err)
	}
	if local, ok := selected.(WorkerBinaryAvailabilityLocal); !ok || local.Source != "MJ_WORKER_DIR" {
		t.Fatalf("selected %#v", selected)
	}
	if err := PinWorkerBinarySources(); err != nil {
		t.Fatal(err)
	}
	pinned := localWorkerPath(t)
	// Model a cache removed between daemon handoffs, then restored with
	// stale bytes.
	writeFixture(t, pinned, []byte("stale restored cache"))
	_, err = WorkerBinaryPrerequisiteForArch("x86_64")
	requireErrorContains(t, err, "missing worker build stamp")
	if err := os.Remove(pinned); err != nil {
		t.Fatal(err)
	}
	path := localWorkerPath(t)
	if err := core.VerifyWorkerBuild(path); err != nil {
		t.Fatal(err)
	}
	if !pathStartsWith(path, filepath.Join(core.DataDir(), "workers/pinned")) {
		t.Fatalf("%s is not pinned", path)
	}
}

// localWorkerPath is the local x86_64 worker the prerequisite selects.
func localWorkerPath(t *testing.T) string {
	t.Helper()
	selected, err := WorkerBinaryPrerequisiteForArch("x86_64")
	if err != nil {
		t.Fatal(err)
	}
	local, ok := selected.(WorkerBinaryAvailabilityLocal)
	if !ok {
		t.Fatalf("local source, not %#v", selected)
	}
	return local.Path
}

var errUnexpected = errors.New("unexpected command")
