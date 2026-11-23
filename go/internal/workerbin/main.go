// Port of mj-worker-bin/src/main.rs.

// Package workerbin is the worker executable's program: its command line
// (cli.go), its stderr logging (logging.go) and the dispatch of each
// command to the worker library. go/cmd/mj-worker installs the build
// identity and calls Main.
//
// Rust's crate is a binary, so all of its items are private; they are
// unexported here, except Main. Rust's `test-hooks` Cargo feature is the
// `testhooks` build tag (feature_testhooks.go). Rust also installs rustls's
// ring provider as the process default; Go's crypto/tls needs no provider.
package workerbin

import (
	"context"
	"errors"
	"fmt"
	"iter"
	"os"
	"runtime"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/worker"
)

// cli is the worker's command line (Rust's clap-derived `Cli`; cli.go
// parses it).
type cli struct {
	// loginEnvironmentReady is the internal handoff: the parent explicitly
	// supplied the clean login snapshot. Hidden and global.
	loginEnvironmentReady bool
	// workerRootLockFD is the descriptor transferred by login-environment
	// re-exec. Hidden and global.
	workerRootLockFD *int32
	command          command
}

// command is the top-level subcommand. Rust's enum has the one variant
// commandWorker.
type command interface{ isCommand() }

// commandWorker is `mj-worker worker ...`: internal target-side worker
// commands (hidden).
type commandWorker struct{ workerArgs }

func (commandWorker) isCommand() {}

// workerArgs carries the worker subcommand.
type workerArgs struct {
	command workerCommand
}

// authorizedKeysArgs carries the authorized-keys subcommand.
type authorizedKeysArgs struct {
	command authorizedKeysCommand
}

// authorizedKeysCommand is a `worker authorized-keys` subcommand.
type authorizedKeysCommand interface{ isAuthorizedKeysCommand() }

// authorizedKeysCommandInstall installs a restricted temporary Move key
// under an exclusive flock.
type authorizedKeysCommandInstall struct {
	path        string
	operationID string
	from        string
	command     string
	publicKey   string
}

// authorizedKeysCommandRemove removes a temporary Move key by its operation
// ID.
type authorizedKeysCommandRemove struct {
	path        string
	operationID string
}

// authorizedKeysCommandReconcile removes stale `mj-move-*` keys not listed
// in `--keep-operation`.
type authorizedKeysCommandReconcile struct {
	path string
	// keepOperations is the repeatable `--keep-operation`.
	keepOperations []string
}

func (authorizedKeysCommandInstall) isAuthorizedKeysCommand()   {}
func (authorizedKeysCommandRemove) isAuthorizedKeysCommand()    {}
func (authorizedKeysCommandReconcile) isAuthorizedKeysCommand() {}

// workerCommand is a `worker` subcommand. Each variant's fields are its
// `--kebab-case` options, in declaration order.
type workerCommand interface{ isWorkerCommand() }

// workerCommandAuthorizedKeys is the locked `~/.ssh/authorized_keys` editor
// for direct Move transfers (hidden).
type workerCommandAuthorizedKeys struct{ authorizedKeysArgs }

// workerCommandExportMoveStream writes a framed Move stream (archive plus an
// optional `.mj/keep/` tar) to stdout (hidden).
type workerCommandExportMoveStream struct {
	archive string
	keepDir *string
	// skip is the repeatable `--skip`.
	skip []string
}

// workerCommandReceiveMoveStream receives and verifies a framed Move stream
// from stdin (hidden).
type workerCommandReceiveMoveStream struct {
	archive        string
	expectedSHA256 string
	keepDir        *string
}

// workerCommandLiveness prints `alive`, `starting` or `dead` for the worker
// that owns a root, decided by the lock on its `worker.lock` (hidden).
type workerCommandLiveness struct {
	root string
}

// workerCommandHolder prints the pid of the worker holding a root's lock, or
// nothing (hidden).
type workerCommandHolder struct {
	root string
}

// workerCommandStop stops the worker that owns a root and every process it
// started, and returns only when its lock is free and none of them is left
// (hidden).
type workerCommandStop struct {
	root string
	// kill kills at once instead of allowing the TERM grace.
	kill bool
}

// workerCommandQuiesce asks a live worker to stop every process for a
// managed suspend (hidden).
type workerCommandQuiesce struct {
	root      string
	operation string
	checkout  string
	// childSessions is the repeatable `--child-session`.
	childSessions []string
}

// workerCommandIdentityAlive prints `alive` or `dead` for a recorded worker
// process identity in this target's PID namespace (hidden).
type workerCommandIdentityAlive struct {
	pid   uint32
	start uint64
}

// workerCommandBuildID prints this worker's compatibility identity. The
// controller asks an image's worker this before linking a session to it
// (hidden).
type workerCommandBuildID struct{}

// workerCommandDiscoverConfig discovers profile model choices without
// submitting a prompt.
type workerCommandDiscoverConfig struct {
	spec string
}

// workerCommandRun owns an ACP bridge and durable session event log.
type workerCommandRun struct {
	root   string
	config string
}

// workerCommandPrepareHarness prepares an exact managed harness without
// starting a session worker.
type workerCommandPrepareHarness struct {
	config string
}

// workerCommandProxy proxies JSON-lines between stdio and a detached worker.
type workerCommandProxy struct {
	root string
}

// workerCommandAcpSupervisor supervises the ACP bridge process tree for a
// worker daemon.
type workerCommandAcpSupervisor struct {
	spec string
}

// workerCommandExportCheckpoint builds a target-side archive for verified
// controller transfer.
type workerCommandExportCheckpoint struct {
	// spec is the export specification path, or `-` to read it from
	// standard input.
	spec string
}

// workerCommandSuspendPreflight runs read-only workspace inventory and an
// optional dry-run size estimate on the target.
type workerCommandSuspendPreflight struct {
	// spec is the preflight specification path, or `-` to read it from
	// standard input.
	spec string
}

// workerCommandCaptureCheckpoint seals target-owned checkpoint inputs while
// ACP dispatch is frozen (hidden).
type workerCommandCaptureCheckpoint struct{}

// workerCommandPackCheckpoint packages a sealed checkpoint after ACP
// dispatch has resumed (hidden).
type workerCommandPackCheckpoint struct{}

// workerCommandRestoreCheckpoint restores a verified archive into a freshly
// cloned target.
type workerCommandRestoreCheckpoint struct {
	spec string
	// missingParent restores only missing parent state; it never modifies
	// the checkout or child reports. It conflicts with missingCheckout.
	missingParent   bool
	missingCheckout bool
	// relaySeed is the current controller-owned queue/frontier, for partial
	// parent recovery. It requires missingParent.
	relaySeed *string
}

// workerCommandDiscardQueuedPrompts journals a queue discard while the
// worker is stopped, before any dispatch (hidden).
type workerCommandDiscardQueuedPrompts struct {
	root      string
	sessionID string
	commandID string
}

// workerCommandInstallResource installs one streamed resource directory on a
// remote target.
type workerCommandInstallResource struct {
	destination string
}

// workerCommandMemoryMcp serves project history tools over MCP stdio.
type workerCommandMemoryMcp struct {
	// root is the hidden `--root` (Rust's `_root`), accepted for staged Kimi
	// profiles created by older controllers and ignored.
	root          *string
	historySocket *string
}

// workerCommandMailboxHook drains mailbox events for one harness tool hook.
type workerCommandMailboxHook struct {
	socket string
	// event is one of mailboxHookEvents.
	event string
	// format is one of mailboxHookFormats; `standard` by default.
	format string
}

// workerCommandMailboxRequest exchanges one mailbox lease request for a
// durable harness inbox.
type workerCommandMailboxRequest struct {
	socket string
}

// workerCommandSubagentMcp serves Mjolnir-owned delegation tools over MCP
// stdio.
type workerCommandSubagentMcp struct {
	socket string
	// harness is the harness used to choose the MCP server instructions.
	harness *core.HarnessKind
	// role is `parent` (the default), which serves the delegation tools, or
	// `child`, which serves only `handback`, a child's report to the session
	// that started it.
	role core.SubagentMcpRole
	// agentMailboxesEnabled is the hidden `--agent-mailboxes-enabled
	// <true|false>`, accepted for older launch arguments; true by default.
	agentMailboxesEnabled bool
	// subtleSubagents is `--subtle-subagents <true|false>`, true by default;
	// false when only the straightforward task type is configured: the
	// spawn_worker tool then offers no task type.
	subtleSubagents bool
	// reviewGate is hidden, accepted for configurations written by older
	// workers; the review tools now belong to the `--review-server` server.
	reviewGate bool
	// reviewServer serves the before-push review tools as `mj-review`
	// instead of the delegation tools.
	reviewServer bool
}

// workerCommandDiff prints a unified diff of the session's work in one
// repository.
type workerCommandDiff struct {
	repository string
	// base is the commit the session started from, when the controller
	// recorded one.
	base *string
	// branch is the session branch, whose reflog names the commit it was
	// created at.
	branch *string
	// json includes the resolved base and HEAD commit in JSON output.
	json bool
}

// workerCommandReadFile writes one file from the session workspace to
// standard output.
type workerCommandReadFile struct {
	root string
	// path is relative to the workspace root.
	path string
}

// workerCommandFetch writes a file, a zip archive of a directory, or a JSON
// listing of either to standard output, after one JSON line naming which it
// is and where it resolved.
type workerCommandFetch struct {
	// path is the file or directory to fetch.
	path string
	// roots is the repeatable `--root`: directories the path may resolve
	// inside.
	roots []string
	// temp also allows this machine's temporary directory.
	temp bool
	// list lists the path instead of reading it.
	list bool
}

// workerCommandStoreFetch saves fetched bytes in this target's temporary
// directory.
type workerCommandStoreFetch struct {
	session   string
	source    string
	directory bool
	// length is Rust's usize; it is used as a u64.
	length uint64
}

// workerCommandWriteFile atomically publishes stdin as a file in the
// session workspace.
type workerCommandWriteFile struct {
	root      string
	path      string
	overwrite bool
	// length is the expected input length; when present, premature EOF or
	// trailing bytes must not publish a partial file.
	length *uint64
}

// workerCommandPushBranch pushes the repository's current HEAD to a branch
// on its push remote.
type workerCommandPushBranch struct {
	// root is the worker root containing the session's existing Git
	// authentication.
	root       string
	repository string
	branch     string
}

func (workerCommandAuthorizedKeys) isWorkerCommand()       {}
func (workerCommandExportMoveStream) isWorkerCommand()     {}
func (workerCommandReceiveMoveStream) isWorkerCommand()    {}
func (workerCommandLiveness) isWorkerCommand()             {}
func (workerCommandHolder) isWorkerCommand()               {}
func (workerCommandStop) isWorkerCommand()                 {}
func (workerCommandQuiesce) isWorkerCommand()              {}
func (workerCommandIdentityAlive) isWorkerCommand()        {}
func (workerCommandBuildID) isWorkerCommand()              {}
func (workerCommandDiscoverConfig) isWorkerCommand()       {}
func (workerCommandRun) isWorkerCommand()                  {}
func (workerCommandPrepareHarness) isWorkerCommand()       {}
func (workerCommandProxy) isWorkerCommand()                {}
func (workerCommandAcpSupervisor) isWorkerCommand()        {}
func (workerCommandExportCheckpoint) isWorkerCommand()     {}
func (workerCommandSuspendPreflight) isWorkerCommand()     {}
func (workerCommandCaptureCheckpoint) isWorkerCommand()    {}
func (workerCommandPackCheckpoint) isWorkerCommand()       {}
func (workerCommandRestoreCheckpoint) isWorkerCommand()    {}
func (workerCommandDiscardQueuedPrompts) isWorkerCommand() {}
func (workerCommandInstallResource) isWorkerCommand()      {}
func (workerCommandMemoryMcp) isWorkerCommand()            {}
func (workerCommandMailboxHook) isWorkerCommand()          {}
func (workerCommandMailboxRequest) isWorkerCommand()       {}
func (workerCommandSubagentMcp) isWorkerCommand()          {}
func (workerCommandDiff) isWorkerCommand()                 {}
func (workerCommandReadFile) isWorkerCommand()             {}
func (workerCommandFetch) isWorkerCommand()                {}
func (workerCommandStoreFetch) isWorkerCommand()           {}
func (workerCommandWriteFile) isWorkerCommand()            {}
func (workerCommandPushBranch) isWorkerCommand()           {}

// mailboxHookEvents are the values `mailbox-hook --event` accepts (clap's
// value_parser list).
var mailboxHookEvents = []string{"PostToolUse", "PostToolBatch"}

// mailboxHookFormats are the values `mailbox-hook --format` accepts; the
// first is the default.
var mailboxHookFormats = []string{"standard", "antigravity"}

// testHookEnvironment are the feature-gated test controls the login re-exec
// carries across: the reliability lab's crash hooks run inside the worker
// after that environment-isolating re-exec. Ordinary launcher variables must
// stay excluded. Only a `testhooks` build carries them.
var testHookEnvironment = []string{
	"MJ_CHAOS_ISOLATED",
	"MJ_TEST_HOOK",
	"MJ_TEST_HOOK_DIR",
	"MJ_TEST_GLIDE_ENDPOINT",
}

func writeWorkerExitRecord(root string, reason string) {
	writeWorkerExitRecordDetails(root, reason, nil, nil)
}

// writeWorkerExitRecordDetails writes the exit record, plus the sentence a
// refusal wants the person who asked to read.
//
// A worker refuses before it has a control socket, so this file is its only
// way to say anything. The controller lifts `refusal` back onto the error
// chain, which turns the failure into a 409 carrying this text instead of a
// generic 500.
func writeWorkerExitRecordDetails(
	root string,
	reason string,
	refusal *string,
	startupFailure *core.WorkerHarnessStartupFailure,
) {
	if info, err := os.Stat(root); err != nil || !info.IsDir() {
		return
	}
	if err := worker.WriteWithRecentLog(root, reason, refusal, startupFailure, core.StderrWriter().RecentLog()); err != nil {
		core.DiagnosticStderr("Mjolnir: could not write worker exit record: %v", err)
	}
}

// lastWordsRoot is the root installWorkerLastWords recorded; empty before.
var lastWordsRoot string

// installWorkerLastWords makes a panic leave its message in root's exit
// record (Rust's panic hook). Go has no process-wide panic hook: the
// goroutine that may panic defers recordWorkerLastWords, which records the
// panic and raises it again. The hook never chains to a default handler
// that writes elsewhere: stderr is a disk-backed worker log.
func installWorkerLastWords(root string) {
	lastWordsRoot = root
}

// recordWorkerLastWords is the deferred half of installWorkerLastWords: it
// writes `panic: <message>` to stderr and to the exit record, then panics
// again with the same value.
func recordWorkerLastWords() {
	recovered := recover()
	if recovered == nil {
		return
	}
	reason := "panic: " + core.PanicMessage(recovered)
	core.DiagnosticStderr("%s", reason)
	if lastWordsRoot != "" {
		writeWorkerExitRecord(lastWordsRoot, reason)
	}
	panic(recovered)
}

// bootstrapLoginEnvironment cleans the worker itself as well as its
// harnesses: Git and other target-side helpers must not inherit controller
// or build-tool variables either. A command that needs the target's login
// environment re-executes the worker with it (bootstrapLoginEnvironmentReexec)
// and does not return on success; the re-executed process, marked with
// `--login-environment-ready`, adopts its parent's snapshot.
func bootstrapLoginEnvironment(parsed *cli, owner *worker.WorkerRootOwner) error {
	panic("unported: mj-worker-bin/src/main.rs bootstrap_login_environment")
}

// envVars is std::env::vars(): the process environment as name and value.
func envVars() iter.Seq2[string, string] {
	return func(yield func(string, string) bool) {
		for _, entry := range os.Environ() {
			name, value, _ := strings.Cut(entry, "=")
			if !yield(name, value) {
				return
			}
		}
	}
}

// envMap is the process environment as a map, for helpers that take
// std::env::vars() as one.
func envMap() map[string]string {
	environment := make(map[string]string)
	for name, value := range envVars() {
		environment[name] = value
	}
	return environment
}

// Main is the worker program (Rust's main): it parses the command line,
// takes the root's ownership for `worker run`, runs the command and returns
// the process exit status. A refusal (exportRefused) leaves with
// checkpoint.ExportRefusedExitCode and its reason on standard output and
// standard error; any other error is logged, recorded in a worker run's exit
// record, printed as Rust's main prints an error, and exits 1.
func Main() int {
	panic("unported: mj-worker-bin/src/main.rs main")
}

// reportMainError prints err as Rust prints the error a `main` returns
// (`Error: ` and anyhow's Debug form, its causes under "Caused by:") and
// returns the exit status 1.
func reportMainError(err error) int {
	panic("unported: mj-worker-bin/src/main.rs main (error report)")
}

// workerRuntimeThreads is the number of threads that run Go code at once in
// every `hel worker` process (Rust's Tokio worker threads). The default is
// one per visible CPU, so on a 96-CPU host with no `--cpus` limit each of a
// session's five worker processes held about 96 threads. With sub-agents
// that filled the container's pids limit and killed the parent's harness.
// These processes mostly relay messages and wait on I/O, so a few threads
// are enough whatever the host size. Go applies it as GOMAXPROCS.
const workerRuntimeThreads = 4

// runWorker bootstraps the login environment and runs the command.
// exitRoot is the worker root whose exit record covers startup, or empty
// when the command is not `worker run`.
func runWorker(parsed *cli, exitRoot string, owner *worker.WorkerRootOwner) error {
	if exitRoot != "" {
		worker.RecordStartupStep(exitRoot, core.WorkerStartupStepLoginEnvironment)
	}
	if err := bootstrapLoginEnvironment(parsed, owner); err != nil {
		return fmt.Errorf("initialize worker environment: %w", err)
	}
	if exitRoot != "" {
		worker.RecordStartupStep(exitRoot, "runtime")
	}
	runtime.GOMAXPROCS(workerRuntimeThreads)
	return runCommand(context.Background(), parsed.command, owner)
}

// runCommand dispatches one parsed command to the worker library.
func runCommand(ctx context.Context, parsed command, owner *worker.WorkerRootOwner) error {
	panic("unported: mj-worker-bin/src/main.rs run_command")
}

// exportRefused is a precondition an export could not meet, carried out of
// runCommand so Main can answer with checkpoint.ExportRefusedExitCode.
type exportRefused string

func (e exportRefused) Error() string { return string(e) }

// exportError turns a checkpoint.SessionExportError into the worker's
// error: a refusal becomes exportRefused, a failure its cause.
func exportError(err error) error {
	var refused checkpoint.SessionExportErrorRefused
	if errors.As(err, &refused) {
		return exportRefused(string(refused))
	}
	var failed checkpoint.SessionExportErrorFailed
	if errors.As(err, &failed) {
		return failed.Err
	}
	return err
}

// pushError turns a checkpoint.PushBranchError into the worker's error: a
// missing remote and an invalid branch are refusals; a push that ran and
// failed stays an ordinary error.
func pushError(err checkpoint.PushBranchError) error {
	switch err := err.(type) {
	case nil:
		return nil
	case checkpoint.PushBranchErrorNoRemote, checkpoint.PushBranchErrorInvalidBranch:
		return exportRefused(err.Error())
	case checkpoint.PushBranchErrorFailed:
		return err.Err
	default:
		return err
	}
}

// writeStdout writes an export payload to standard output unchanged. A diff
// and a file are bytes the caller reassembles, so nothing may add or trim a
// newline. Go's standard output is unbuffered, so there is nothing to flush.
func writeStdout(data []byte) error {
	if _, err := os.Stdout.Write(data); err != nil {
		return fmt.Errorf("write to standard output: %w", err)
	}
	return nil
}

// printJSONLine is `println!("{}", serde_json::to_string(&value)?)`.
func printJSONLine(value any) error {
	data, err := serdeJSON(value)
	if err != nil {
		return err
	}
	return writeStdout(append(data, '\n'))
}

// serdeJSON is serde_json::to_vec: encoding/json's output without its HTML
// escapes (`<`, `>`, `&`, U+2028 and U+2029 stay as written).
func serdeJSON(value any) ([]byte, error) {
	panic("unported: mj-worker-bin/src/main.rs serde_json::to_string")
}
