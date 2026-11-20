// Port of mj-core/src/test_hooks.rs.

// Crash-boundary hooks and subprocess fixtures for isolated tests.
//
// The default build contains only the no-op hook. Environment lookup, marker
// files, and waiting are compiled in solely with the `testhooks` build tag
// (Rust's `test-hooks` feature); see test_hooks_on.go and test_hooks_off.go.

package core

// ReachTestHook stops at the named crash boundary when an isolated test asks
// for it through MJ_TEST_HOOK; in the default build it does nothing.
func ReachTestHook(name string) error {
	if !testHooksEnabled {
		return nil
	}
	return reachTestHookEnabled(name)
}

// reachTestHookEnabled is the `test-hooks` build of ReachTestHook: it marks
// <MJ_TEST_HOOK_DIR>/<name>.reached and waits up to 120 seconds for
// <name>.continue.
func reachTestHookEnabled(name string) error {
	panic("unported: mj-core/src/test_hooks.rs reach_test_hook")
}

// FakeCommandDispatcher is the checked-in executable used by fake commands
// and worker fixtures (testdata/test_hooks/fake-command.sh). Unix only.
func FakeCommandDispatcher() string {
	panic("unported: mj-core/src/test_hooks.rs fake_command_dispatcher")
}

// FakeWorkerDispatcher is a worker stand-in carrying the build identity a
// test binary reports. Built before tests start, so executing it cannot race
// a test's writer. Unix only.
func FakeWorkerDispatcher() string {
	panic("unported: mj-core/src/test_hooks.rs fake_worker_dispatcher")
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
	panic("unported: mj-core/src/test_hooks.rs install_fake_with_dispatcher")
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
	// of reloading one, as a restored worker does when the harness has no
	// record of the session it was asked to reload.
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
