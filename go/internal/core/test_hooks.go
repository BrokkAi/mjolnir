// Port of mj-core/src/test_hooks.rs.

// Crash-boundary hooks and subprocess fixtures for isolated tests.
//
// Runtime hooks and shared test fixtures stay inactive until explicitly
// enabled via EnableTestHooks or EnableTestFixtures. A test package opts in
// to the fixtures by calling EnableTestFixtures from an init function in one
// of its test files (Rust's `enable_test_fixtures_on_startup!`).

package core

import (
	"crypto/sha256"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

var (
	fakeWorkerOnce sync.Once
	fakeWorkerPath string

	testHooksEnabled    atomic.Bool
	testFixturesEnabled atomic.Bool
)

// EnableTestHooks turns on the crash-boundary hooks for this process.
func EnableTestHooks() { testHooksEnabled.Store(true) }

// TestHooksEnabled reports whether EnableTestHooks was called.
func TestHooksEnabled() bool { return testHooksEnabled.Load() }

// EnableTestFixtures opts a test process into shared library fixtures.
// Runtime hooks require their own opt-in.
func EnableTestFixtures() { testFixturesEnabled.Store(true) }

// TestFixturesEnabled reports whether EnableTestFixtures was called.
func TestFixturesEnabled() bool { return testFixturesEnabled.Load() }

// testGlideEndpoint is the GLiDE endpoint a test process routes the default
// classifier source to, set once (Rust TEST_GLIDE_ENDPOINT, a OnceLock).
var testGlideEndpoint struct {
	mu       sync.Mutex
	endpoint *string
}

// SetTestGlideEndpoint routes the default classifier source of this test
// process to endpoint. It panics when the endpoint was already set.
func SetTestGlideEndpoint(endpoint string) {
	testGlideEndpoint.mu.Lock()
	defer testGlideEndpoint.mu.Unlock()
	if testGlideEndpoint.endpoint != nil {
		panic("set test GLiDE endpoint once")
	}
	testGlideEndpoint.endpoint = &endpoint
}

// TestGlideEndpoint is the endpoint SetTestGlideEndpoint set, if any.
func TestGlideEndpoint() (string, bool) {
	testGlideEndpoint.mu.Lock()
	defer testGlideEndpoint.mu.Unlock()
	if testGlideEndpoint.endpoint == nil {
		return "", false
	}
	return *testGlideEndpoint.endpoint, true
}

// ReachTestHook stops at the named crash boundary when an isolated test asks
// for it through MJ_TEST_HOOK: it marks <MJ_TEST_HOOK_DIR>/<name>.reached and
// waits up to 120 seconds for <name>.continue. It does nothing until
// EnableTestHooks is called.
func ReachTestHook(name string) error {
	if !TestHooksEnabled() {
		return nil
	}
	if value, ok := os.LookupEnv("MJ_TEST_HOOK"); !ok || value != name {
		return nil
	}
	if value, ok := os.LookupEnv("MJ_CHAOS_ISOLATED"); !ok || value != "1" {
		return fmt.Errorf("test hook %s requires MJ_CHAOS_ISOLATED=1", name)
	}
	for _, b := range []byte(name) {
		if !(b >= 'a' && b <= 'z' || b >= '0' && b <= '9' || b == '_') {
			return fmt.Errorf("invalid test hook name %q", name)
		}
	}
	directory, ok := os.LookupEnv("MJ_TEST_HOOK_DIR")
	if !ok {
		return errors.New("active test hook requires MJ_TEST_HOOK_DIR")
	}
	if info, err := os.Stat(directory); err != nil || !info.IsDir() {
		return fmt.Errorf("test hook directory %s does not exist", directory)
	}
	reached := filepath.Join(directory, name+".reached")
	marker, err := os.OpenFile(reached, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o666)
	switch {
	case err == nil:
		_, writeErr := fmt.Fprintf(marker, "pid=%d\n", os.Getpid())
		if writeErr != nil {
			_ = marker.Close()
			return fmt.Errorf("write test hook marker %s: %w", reached, writeErr)
		}
		if syncErr := marker.Sync(); syncErr != nil {
			_ = marker.Close()
			return fmt.Errorf("sync test hook marker %s: %w", reached, syncErr)
		}
		_ = marker.Close()
	case errors.Is(err, fs.ErrExist):
	default:
		return fmt.Errorf("create test hook marker %s: %w", reached, err)
	}
	continuation := filepath.Join(directory, name+".continue")
	deadline := time.Now().Add(120 * time.Second)
	for {
		if info, err := os.Stat(continuation); err == nil && info.Mode().IsRegular() {
			return nil
		}
		if !time.Now().Before(deadline) {
			return fmt.Errorf("test hook %s timed out waiting for %s", name, continuation)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

// FakeCommandDispatcher is the checked-in executable used by fake commands
// and worker fixtures (testdata/test_hooks/fake-command.sh). Unix only.
func FakeCommandDispatcher() string {
	_, source, _, ok := runtime.Caller(0)
	if !ok {
		panic("the fake command dispatcher's source directory is unknown")
	}
	return filepath.Join(filepath.Dir(source), "testdata", "test_hooks", "fake-command.sh")
}

// FakeWorkerDispatcher is a worker stand-in carrying the build identity a
// test binary reports. Built before tests start, so executing it cannot race
// a test's writer. Unix only.
func FakeWorkerDispatcher() string {
	fakeWorkerOnce.Do(func() {
		script, err := os.ReadFile(FakeCommandDispatcher())
		if err != nil {
			panic(fmt.Sprintf("fake command fixture: %v", err))
		}
		identity := PackageVersion + "+" + testRevision
		stamped := fmt.Sprintf("%s\n#MJ-BUILD-ID:%s\n#\x00MJ-WORKER-BUILD:%s\x00\n", script, identity, identity)
		sum := sha256.Sum256([]byte(stamped))
		path := filepath.Join(os.TempDir(), "mj-go-fake-worker-"+LowerHex(sum[:8])+".sh")
		if existing, err := os.ReadFile(path); err == nil && string(existing) == stamped {
			fakeWorkerPath = path
			return
		}
		// Rust generates this file at build time, before any test runs. Here
		// it is written with the fork lock held, so no child started
		// meanwhile inherits the write descriptor (ETXTBSY), and renamed into
		// place complete.
		syscall.ForkLock.Lock()
		defer syscall.ForkLock.Unlock()
		temporary := fmt.Sprintf("%s.%d.tmp", path, os.Getpid())
		if err := os.WriteFile(temporary, []byte(stamped), 0o755); err != nil {
			panic(fmt.Sprintf("write stamped fake worker: %v", err))
		}
		if err := os.Chmod(temporary, 0o755); err != nil {
			panic(fmt.Sprintf("make fake worker executable: %v", err))
		}
		if err := os.Rename(temporary, path); err != nil {
			panic(fmt.Sprintf("install stamped fake worker: %v", err))
		}
		fakeWorkerPath = path
	})
	return fakeWorkerPath
}

// InstallFakeCommand installs a shell stand-in, under program, for a command
// the code under test looks up on PATH in directory.
//
// No test may exec a file it has just written. execve answers ETXTBSY ("Text
// file busy") while any process still holds that file open for writing, and
// a test binary is multi-threaded: if another thread forks between the write
// and the exec, its child inherits the still-open write descriptor and keeps
// the file busy past the point where the writer closed it. That is the race
// behind issue #1036. Writing under a temporary name and renaming does not
// fix it, because a rename keeps the same inode.
//
// So the name on PATH is a symlink to a checked-in dispatcher, which this
// process never opens for writing, and the behaviour goes in
// <program>.script, which only /bin/sh ever reads. Nothing execs a written
// file at any point.
func InstallFakeCommand(directory, program, script string) {
	installFakeWithDispatcher(FakeCommandDispatcher(), directory, program, script)
}

// InstallFakeWorker installs a worker stand-in, like InstallFakeCommand, that
// carries a test binary's worker stamp, so controller code running in a test
// accepts it as a matching worker.
func InstallFakeWorker(directory, program, script string) {
	installFakeWithDispatcher(FakeWorkerDispatcher(), directory, program, script)
}

// InstallFakeWorkerFrom installs a worker stand-in from dispatcher. A test in
// which a real program verifies the worker passes the stand-in stamped with
// that program's identity.
func InstallFakeWorkerFrom(dispatcher, directory, program, script string) {
	installFakeWithDispatcher(dispatcher, directory, program, script)
}

func installFakeWithDispatcher(dispatcher, directory, program, script string) {
	if info, err := os.Stat(dispatcher); err != nil || !info.Mode().IsRegular() {
		panic(fmt.Sprintf("fake command dispatcher is missing at %s", dispatcher))
	}
	if err := os.WriteFile(filepath.Join(directory, program+".script"), []byte(script), 0o666); err != nil {
		panic(fmt.Sprintf("write the %s stand-in: %v", program, err))
	}
	installed := filepath.Join(directory, program)
	// A directory may host several fakes, and a test may replace one.
	if err := os.Remove(installed); err != nil && !errors.Is(err, fs.ErrNotExist) {
		panic(fmt.Sprintf("replace the %s stand-in: %v", program, err))
	}
	if err := os.Symlink(dispatcher, installed); err != nil {
		panic(fmt.Sprintf("link the %s stand-in: %v", program, err))
	}
}

// Environment shared by the controller and daemon tests that run a stand-in
// relay (Rust module test_hooks::latch_relay): the parent test sets it on the
// command that runs the relay, and the worker's serve_latch_relay reads it.
// The relay runs as LatchRelayChildTest in a second process of the same test
// binary.
const (
	// LatchRelayChildTest is the exact test that serves the relay; every
	// test binary that starts one defines it.
	LatchRelayChildTest          = "stand_in_relay::latch_relay_child_serves_stdio"
	LatchRelayRoot               = "MJ_TEST_LATCH_RELAY_ROOT"
	LatchRelayMaxProtocol        = "MJ_TEST_LATCH_RELAY_MAX_PROTOCOL"
	LatchRejectProjectMemory     = "MJ_TEST_LATCH_REJECT_PROJECT_MEMORY"
	LatchRecordSubagentAdmission = "MJ_TEST_LATCH_RECORD_SUBAGENT_ADMISSION"
	LatchDropAdmissionCloseReply = "MJ_TEST_LATCH_DROP_ADMISSION_CLOSE_REPLY"
	LatchRelayStarts             = "MJ_TEST_LATCH_RELAY_STARTS"
	LatchRelayRejectRelease      = "MJ_TEST_LATCH_REJECT_RELEASE"
	LatchRelayRunning            = "MJ_TEST_LATCH_RELAY_RUNNING"
	LatchCheckpointOnly          = "MJ_TEST_LATCH_CHECKPOINT_ONLY"
	// LatchRelayFreshNative opens a new native session with this id instead
	// of reloading one, as a restored worker does when its launch names no
	// native session, or when the harness has no record of the session it
	// was asked to reload.
	LatchRelayFreshNative = "MJ_TEST_LATCH_FRESH_NATIVE"
	// LatchRelayReplacedUnused, with LatchRelayFreshNative: the native
	// session the new one replaced because this session never used it.
	// Unset, the relay gives no reason.
	LatchRelayReplacedUnused = "MJ_TEST_LATCH_REPLACED_UNUSED"
	// LatchRelayAnswerPrompts answers every dispatched prompt with one agent
	// message, like a harness.
	LatchRelayAnswerPrompts = "MJ_TEST_LATCH_ANSWER_PROMPTS"
	// LatchRelayAnswer is the text of the answer LatchRelayAnswerPrompts
	// gives.
	LatchRelayAnswer         = "answered by the stand-in harness"
	LatchRelayStartupDelayMs = "MJ_TEST_LATCH_STARTUP_DELAY_MS"
	// LatchRelayJournalsBeforeClose has the worker write something of its
	// own just before it reads a Close, and complete the Close once the
	// barrier it seals has ended, the way a live worker does. The value names
	// the write: LateHarnessNotification or LateHarnessQuestion.
	LatchRelayJournalsBeforeClose = "MJ_TEST_LATCH_JOURNALS_BEFORE_CLOSE"
	// LateHarnessNotification is a harness notification that changes no
	// work, which waits behind the cut.
	LateHarnessNotification = "notification"
	// LatchRelayHoldCommand holds the first request that submits this relay
	// command (`begin_checkpoint` or `close`) until the parent test lets it
	// go: the worker creates <LatchRelayHoldPath>.reached and waits for
	// <LatchRelayHoldPath>.release. A test uses it to act at an exact point
	// of a close.
	LatchRelayHoldCommand = "MJ_TEST_LATCH_HOLD_COMMAND"
	LatchRelayHoldPath    = "MJ_TEST_LATCH_HOLD_PATH"
	// LateHarnessQuestion is a question for a person, which cannot wait and
	// spoils the cut.
	LateHarnessQuestion = "question"
	LatchRelaySession   = "018f9dd2-a3b4-7c8d-9000-0123456789ab"
)

// Environment of the stand-in relay that the leased-submission tests run
// (Rust module test_hooks::leased_relay); the worker's serve_leased_relay
// reads it.
const (
	// LeasedRelayChildTest is the exact test that serves the relay; every
	// test binary that starts one defines it.
	LeasedRelayChildTest = "stand_in_relay::leased_relay_child_serves_stdio"
	LeasedRelayRoot      = "MJ_TEST_LEASED_RELAY_ROOT"
	LeasedRelaySession   = "018f9dd2-a3b4-7c8d-9000-123456789abc"
	// RelayRequestLog is the file in the relay root where the stdio relay
	// child logs each request.
	RelayRequestLog = "requests.log"
	// RelaySessionConfigured makes the stdio relay child report an open
	// harness session.
	RelaySessionConfigured = "MJ_TEST_RELAY_SESSION_CONFIGURED"
	ClearMailboxControl    = "MJ_TEST_CLEAR_MAILBOX_CONTROL"
	DropAdmissionOpenReply = "MJ_TEST_DROP_ADMISSION_OPEN_REPLY"
)
