//go:build unix

// Port of mj-worker-staging/src/worker_binary/tests.rs, part 2: the
// `#[cfg(unix)]` tests of upgrade staging, harness preflight and the default
// bridge scripts.

package workerstaging

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestUpgradePreparationLeavesTheInstalledWorkerUnchangedUntilPromotion(t *testing.T) {
	directory := t.TempDir()
	const sessionID = "12121212121212121212121212121212"
	workerRoot := filepath.Join(directory, sessionID)
	makeFixtureDir(t, workerRoot)
	installed := filepath.Join(workerRoot, "hel")
	source := filepath.Join(directory, "new-worker")
	writeFixture(t, installed, []byte("running-worker"))
	writeFixture(t, source, StampedWorker([]byte("replacement-worker")))
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: workerRoot}
	executor := core.ProcessExecutor{}
	if _, err := StageWorkerBinaryForUpgrade(executor, &locator, sessionID, WorkerInstallHost{Path: filepath.Join(directory, "missing")}); err == nil {
		t.Fatal("a missing source was staged")
	}
	if readFixture(t, installed) != "running-worker" {
		t.Fatal("a failed stage changed the installed worker")
	}
	staging, err := StageWorkerBinaryForUpgrade(executor, &locator, sessionID, WorkerInstallHost{Path: source})
	if err != nil {
		t.Fatal(err)
	}
	defer staging.Close()
	if readFixture(t, installed) != "running-worker" {
		t.Fatal("staging changed the installed worker")
	}
	abandoned, err := StageWorkerBinaryForUpgrade(executor, &locator, sessionID, WorkerInstallHost{Path: source})
	if err != nil {
		t.Fatal(err)
	}
	if staging.Name() == abandoned.Name() {
		t.Fatal("two stagings share a name")
	}
	abandonedPath := filepath.Join(workerRoot, abandoned.Name())
	if !fixtureExists(abandonedPath) {
		t.Fatal("the second staging is missing")
	}
	abandoned.Close()
	if fixtureExists(abandonedPath) {
		t.Fatal("a dropped staging survived")
	}
	if !fixtureExists(filepath.Join(workerRoot, staging.Name())) {
		t.Fatal("the held staging was removed")
	}
	if readFixture(t, installed) != "running-worker" {
		t.Fatal("staging changed the installed worker")
	}
	owner := workerPermitFor(t, sessionID, "test upgrade")
	if err := InstallStagedWorkerBinary(owner, staging.Name(), executor, &locator, sessionID); err != nil {
		t.Fatal(err)
	}
	if readFixture(t, installed) != string(StampedWorker([]byte("replacement-worker"))) {
		t.Fatal("promotion did not install the staged worker")
	}
}

// The incident on precision-3260: every failed upload left a truncated
// `hel.prepared-*.next` behind, 39 of them in one worker root. A failed
// upload now removes its own partial file, and the next staging removes any
// left by an earlier daemon, while a staging still in use is kept.
// Hard-won: f54a670: a full disk left partial worker uploads and truncated files
func TestFailedWorkerStagingLeavesNoPartialUploadAndSweepsStaleOnes(t *testing.T) {
	// Runs commands for real, except that the upload writes 32 KiB of its
	// file and then fails the way scp does on a full disk.
	fail := false
	process := core.ProcessExecutor{}
	executor := &workerTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		if fail && command.Purpose == "stage replacement Mjolnir worker" {
			if err := os.WriteFile(command.Args[1], make([]byte, 32*1024), 0o666); err != nil {
				panic(err)
			}
			return commandOutputOf(1, "", "cp: error writing: No space left on device"), nil
		}
		return process.Execute(command)
	}}

	directory := t.TempDir()
	const sessionID = "34343434343434343434343434343434"
	workerRoot := filepath.Join(directory, sessionID)
	makeFixtureDir(t, workerRoot)
	source := filepath.Join(directory, "new-worker")
	writeFixture(t, source, StampedWorker([]byte("replacement-worker")))
	stale := filepath.Join(workerRoot, "hel.prepared-upgrade-stage-0123.next")
	writeFixture(t, stale, make([]byte, 32*1024))
	writeFixture(t, filepath.Join(workerRoot, "hel"), []byte("running-worker"))
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: workerRoot}
	held, err := StageWorkerBinaryForUpgrade(executor, &locator, sessionID, WorkerInstallHost{Path: source})
	if err != nil {
		t.Fatal(err)
	}
	defer held.Close()
	if fixtureExists(stale) {
		t.Fatal("a stale partial upload survived staging")
	}
	fail = true
	writeFixture(t, source, StampedWorker([]byte("different-replacement-worker")))
	_, err = StageWorkerBinaryForUpgrade(executor, &locator, sessionID, WorkerInstallHost{Path: source})
	requireErrorContains(t, err, "No space left on device")
	entries, err := os.ReadDir(workerRoot)
	if err != nil {
		t.Fatal(err)
	}
	var remaining []string
	for _, entry := range entries {
		remaining = append(remaining, entry.Name())
	}
	slices.Sort(remaining)
	if !reflect.DeepEqual(remaining, []string{"hel", held.Name()}) {
		t.Fatalf("%q", remaining)
	}
}

func profileWithPath(kind core.HarnessKind, home, path string) core.HarnessProfile {
	profile := core.NewHarnessProfile(kind, home)
	profile.Environment = core.EnvironmentFromMap(map[string]string{"PATH": path})
	return profile
}

func TestNodePreflightAcceptsAManagedRuntimeWhenNodeIsMissing(t *testing.T) {
	directory := t.TempDir()
	system := "Linux"
	if runtime.GOOS == "darwin" {
		system = "Darwin"
	}
	var machine string
	switch rustArch() {
	case "x86_64":
		machine = "x86_64"
	case "aarch64":
		machine = "aarch64"
		if runtime.GOOS == "darwin" {
			machine = "arm64"
		}
	default:
		t.Fatalf("test has no uname mapping for %s", rustArch())
	}
	core.InstallFakeCommand(directory, "uname",
		fmt.Sprintf("#!/bin/sh\ncase \"$1\" in -s) echo %s ;; -m) echo %s ;; esac\n", system, machine))
	profile := profileWithPath(core.HarnessKindCodex, directory, directory)
	if err := PreflightHarness(core.TargetTemplateLocalBare{}, &profile, core.ProcessExecutor{}); err != nil {
		t.Fatal(err)
	}
}

func TestNodePreflightExplainsWhenTheTargetHasNoManagedRuntime(t *testing.T) {
	directory := t.TempDir()
	profile := profileWithPath(core.HarnessKindCodex, directory, directory)
	err := PreflightHarness(core.TargetTemplateLocalBare{}, &profile, core.ProcessExecutor{})
	if err == nil || !strings.HasPrefix(err.Error(),
		"Codex launch preflight failed on local host; provide Node.js 22+ with npm and npx on the target PATH") {
		t.Fatalf("%v", err)
	}
	requireErrorContains(t, err, "managed Node.js has no artifact")
}

func TestDshPreflightAcceptsOnlyItsNativeBinaryWithoutNodeOrCurl(t *testing.T) {
	directory := t.TempDir()
	current := fmt.Sprintf("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'dsh-acp %s'; fi\n", core.DshACPVersion)
	core.InstallFakeCommand(directory, "dsh-acp", current)
	profile := profileWithPath(core.HarnessKindDsh, directory, directory)

	if err := PreflightHarness(core.TargetTemplateLocalBare{}, &profile, core.ProcessExecutor{}); err != nil {
		t.Fatal(err)
	}

	core.InstallFakeCommand(directory, "dsh-acp", "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'dsh-acp 0.3.0'; fi\n")
	err := PreflightHarness(core.TargetTemplateLocalBare{}, &profile, core.ProcessExecutor{})
	requireErrorContains(t, err, "provide Node.js 22+ with npm and npx", "managed Node.js has no artifact")
}

// runBridge runs a bridge launch with a clean environment of environment.
func runBridge(t *testing.T, program string, arguments []string, environment ...string) core.ProcessOutput {
	t.Helper()
	process := exec.Command(program, arguments...)
	process.Env = environment
	output, err := core.RunWithInput(process, nil)
	if err != nil {
		t.Fatal(err)
	}
	return output
}

// runDefaultBridge runs a default bridge script with launcher in the
// target's home and no harness on PATH. Returns the script's stdout and
// stderr.
func runDefaultBridge(t *testing.T, harness core.HarnessKind, launcher, script string) (string, string) {
	t.Helper()
	home := t.TempDir()
	bin := filepath.Join(home, "fake-bin")
	makeFixtureDir(t, bin)
	executable := filepath.Join(home, launcher)
	writeFixture(t, executable, []byte(script))
	if err := os.Chmod(executable, 0o755); err != nil {
		t.Fatal(err)
	}
	command, arguments := BridgeLaunch(harness, core.ExecutionPolicyConfiguredApprovals)
	output := runBridge(t, command, arguments, "HOME="+home, "PATH="+bin+":/usr/bin:/bin")
	if !output.Status.Success() {
		t.Fatalf("bridge script failed: %s %s", output.Stdout, output.Stderr)
	}
	return string(output.Stdout), string(output.Stderr)
}

// dsh-acp reads the staged AGENTS.md from DSH_ACP_DATA_DIR itself. The
// launch clears DSH_ACP_SYSTEM_PROMPT so a value from the target's login
// shell cannot deliver the same instructions a second time, and a launch
// without a staged home refuses rather than let dsh-acp keep its sessions in
// the repository it runs in.
func TestDshLaunchUsesNativeAgentsMdWithoutDuplicateSystemPrompt(t *testing.T) {
	root := t.TempDir()
	bin := filepath.Join(root, "fake-bin")
	makeFixtureDir(t, bin)
	script := fmt.Sprintf("#!/bin/sh\nif [ \"$1\" = --version ]; then printf 'dsh-acp %s\\n'; exit 0; fi\nprintf '%%s|%%s|%%s' \"$*\" \"$DSH_ACP_SYSTEM_PROMPT\" \"$DSH_ACP_DATA_DIR\"\n", core.DshACPVersion)
	core.InstallFakeCommand(bin, "dsh-acp", script)
	staged := filepath.Join(root, "staged")
	makeFixtureDir(t, staged)
	instructions := "Keep durable results in the repository.\n\nQuote \"this\", `not this`, $HOME and 'that'."
	writeFixture(t, filepath.Join(staged, "AGENTS.md"), []byte(instructions+"\n"))
	launch := func(dataDir *string) core.ProcessOutput {
		command, arguments := BridgeLaunch(core.HarnessKindDsh, core.ExecutionPolicyConfiguredApprovals)
		environment := []string{
			"HOME=" + root,
			"PATH=" + bin + ":/usr/bin:/bin",
			"DSH_ACP_SYSTEM_PROMPT=from the login shell",
		}
		if dataDir != nil {
			environment = append(environment, core.HarnessKindDsh.HomeEnv()+"="+*dataDir)
		}
		return runBridge(t, command, arguments, environment...)
	}

	output := launch(&staged)
	if !output.Status.Success() || string(output.Stdout) != "||"+staged {
		t.Fatalf("%v %q %q", output.Status, output.Stdout, output.Stderr)
	}

	// The native loader accepts instruction files up to 1 MiB. Mjolnir
	// checks the staged file first so an oversized file names the profile's
	// own file before a session launches rather than failing later in
	// dsh-acp.
	home := filepath.Join(root, "profile")
	makeFixtureDir(t, home)
	own := filepath.Join(home, "AGENTS.md")
	profile := core.NewHarnessProfile(core.HarnessKindDsh, home)
	profile.Environment = core.EnvironmentFromMap(map[string]string{"DEEPSEEK_API_KEY": "sk-dummy"})
	refusal := func(err error) string {
		t.Helper()
		refusal, ok := core.RefusalOf(err)
		if !ok {
			t.Fatalf("a refusal for the client, not %v", err)
		}
		if refusal.Kind() != core.RefusalKindPrecondition {
			t.Fatalf("refusal kind %v", refusal.Kind())
		}
		message := refusal.Message()
		if !strings.HasPrefix(message, "profile \"dsh\": ") || !strings.Contains(message, own) {
			t.Fatal(message)
		}
		return message
	}

	// The largest native instruction file starts, and one byte more is
	// refused before the launch.
	limit := core.DshACPInstructionsLimit
	largest := strings.Repeat("é", limit/2)
	if len(largest) != limit {
		t.Fatal("the largest file is not at the limit")
	}
	writeFixture(t, own, []byte(largest))
	if err := profile.EnsureReady("dsh"); err != nil {
		t.Fatal(err)
	}
	if err := StageProfile(&profile, staged); err != nil {
		t.Fatal(err)
	}
	if err := profile.EnsureStagedInstructionsDeliverable("dsh", staged); err != nil {
		t.Fatal(err)
	}
	output = launch(&staged)
	if !output.Status.Success() || string(output.Stdout) != "||"+staged {
		t.Fatalf("%v %q", output.Status, output.Stdout)
	}
	writeFixture(t, own, []byte(largest+"x"))
	// Hard-won: DSH campaign 2026-10-23: one byte too many reached the user only as relay noise naming the staged copy
	message := refusal(profile.EnsureReady("dsh"))
	if !strings.Contains(message, fmt.Sprintf("is %d bytes", limit+1)) ||
		!strings.Contains(message, fmt.Sprintf("at most %d bytes", limit)) ||
		!strings.HasSuffix(message, "Shorten the file by at least 1 byte.") {
		t.Fatal(message)
	}
	if err := StageProfile(&profile, staged); err != nil {
		t.Fatal(err)
	}
	message = refusal(profile.EnsureStagedInstructionsDeliverable("dsh", staged))
	if !strings.Contains(message, fmt.Sprintf("is %d bytes", limit+1)) || !strings.Contains(message, fmt.Sprintf("at most %d bytes", limit)) {
		t.Fatal(message)
	}

	// Mjolnir's note about a container can take a file the profile alone
	// could load over the limit; the staged file is refused before it
	// travels.
	within := limit - 10
	writeFixture(t, own, []byte(strings.Repeat("x", within)))
	if err := profile.EnsureReady("dsh"); err != nil {
		t.Fatal(err)
	}
	if err := StageProfile(&profile, staged); err != nil {
		t.Fatal(err)
	}
	if err := AppendHelTargetEnvironment(core.HarnessKindDsh, staged, &core.TargetsTargetLocator{
		Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: "container",
	}); err != nil {
		t.Fatal(err)
	}
	total := within + len("\n\n") + len(MJContainerEnvironment)
	message = refusal(profile.EnsureStagedInstructionsDeliverable("dsh", staged))
	if !strings.Contains(message, fmt.Sprintf("is %d bytes", within)) ||
		!strings.Contains(message, fmt.Sprintf("with the %d bytes Mjolnir adds to describe", total-within)) ||
		!strings.Contains(message, fmt.Sprintf("would be %d bytes", total)) ||
		!strings.Contains(message, fmt.Sprintf("at least %d bytes", total-limit)) {
		t.Fatal(message)
	}

	// Hard-won: DSH campaign 2026-10-23: invalid instructions reached the user
	// as relay noise instead of a refusal naming the profile's file.
	writeFixture(t, own, []byte("\xff\xfeK\x00e\x00e\x00p\x00\n\x00"))
	message = refusal(profile.EnsureReady("dsh"))
	if !strings.Contains(message, "is not UTF-8 text (line 1 has") {
		t.Fatal(message)
	}

	// Hard-won: c330ea5f2: a directory AGENTS.md passed readiness but failed
	// when dsh-acp opened the session.
	if err := os.Remove(own); err != nil {
		t.Fatal(err)
	}
	makeFixtureDir(t, own)
	message = refusal(profile.EnsureReady("dsh"))
	if !strings.Contains(message, "is not a regular file") || !strings.Contains(message, "Replace it with a regular file.") {
		t.Fatal(message)
	}

	if err := os.Remove(filepath.Join(staged, "AGENTS.md")); err != nil {
		t.Fatal(err)
	}
	output = launch(&staged)
	if !output.Status.Success() || string(output.Stdout) != "||"+staged {
		t.Fatalf("%v %q", output.Status, output.Stdout)
	}

	output = launch(nil)
	if output.Status.Success() || len(output.Stdout) != 0 || !strings.Contains(lossyText(output.Stderr), "DSH_ACP_DATA_DIR") {
		t.Fatalf("%v %q %q", output.Status, output.Stdout, output.Stderr)
	}
}

// Hard-won: c330ea5f2: a pre-0.4 dsh-acp on PATH silently received no staged instructions.
func TestDshLaunchDoesNotSelectAPreNativeLoader(t *testing.T) {
	root := t.TempDir()
	legacyBin := filepath.Join(root, "legacy-bin")
	toolsBin := filepath.Join(root, "tools-bin")
	makeFixtureDir(t, legacyBin)
	makeFixtureDir(t, toolsBin)
	core.InstallFakeCommand(legacyBin, "dsh-acp",
		"#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'dsh-acp 0.3.0'; else echo legacy; fi\n")
	for _, tool := range []string{"node", "npm"} {
		core.InstallFakeCommand(toolsBin, tool, "#!/bin/sh\nexit 0\n")
	}
	core.InstallFakeCommand(toolsBin, "npx", "#!/bin/sh\nprintf 'npx|%s' \"$*\"\n")

	launch := func(path string) core.ProcessOutput {
		staged := filepath.Join(root, "staged")
		makeFixtureDir(t, staged)
		command, arguments := BridgeLaunch(core.HarnessKindDsh, core.ExecutionPolicyConfiguredApprovals)
		if command != "sh" {
			t.Fatal(command)
		}
		return runBridge(t, "/bin/sh", arguments, "HOME="+root, "PATH="+path, core.HarnessKindDsh.HomeEnv()+"="+staged)
	}

	output := launch(legacyBin + string(os.PathListSeparator) + toolsBin)
	if !output.Status.Success() || string(output.Stdout) != fmt.Sprintf("npx|-y %s@%s", core.DshACPPackage, core.DshACPVersion) {
		t.Fatalf("%v %q %q", output.Status, output.Stdout, output.Stderr)
	}

	output = launch(legacyBin)
	stderr := lossyText(output.Stderr)
	if output.Status.Success() ||
		!strings.Contains(stderr, fmt.Sprintf("requires dsh-acp %s or newer", core.DshACPVersion)) ||
		!strings.Contains(stderr, "upgrade it or install Node.js 22+, npm, and npx") {
		t.Fatalf("%v %s", output.Status, stderr)
	}
}

// A Kimi launcher's installer lines on stdout reached the ACP transport and
// broke initialize (#1136). The managed path installs the pinned artifact
// before launch, so the default bridge starts the ACP server directly.
// Hard-won: 2fe3b29: Kimi installer output contaminated ACP stdout
func TestKimiDefaultBridgeStartsInstalledAcpWithCleanStdout(t *testing.T) {
	stdout, stderr := runDefaultBridge(t, core.HarnessKindKimi, ".kimi-code/bin/kimi",
		"#!/bin/sh\nprintf '{\"jsonrpc\":\"2.0\",\"args\":\"%s\"}\\n' \"$*\"\n")
	if stdout != "{\"jsonrpc\":\"2.0\",\"args\":\"acp\"}\n" || stderr != "" {
		t.Fatalf("%q %q", stdout, stderr)
	}
}

func TestGrokDefaultBridgeStartsInstalledAcpWithCleanStdout(t *testing.T) {
	stdout, stderr := runDefaultBridge(t, core.HarnessKindGrok, ".grok/bin/grok",
		"#!/bin/sh\nprintf '{\"jsonrpc\":\"2.0\",\"args\":\"%s\"}\\n' \"$*\"\n")
	if stdout != "{\"jsonrpc\":\"2.0\",\"args\":\"agent stdio\"}\n" || stderr != "" {
		t.Fatalf("%q %q", stdout, stderr)
	}
}
