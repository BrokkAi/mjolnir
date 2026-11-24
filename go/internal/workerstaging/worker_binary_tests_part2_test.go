// Port of mj-worker-staging/src/worker_binary/tests.rs, part 2: worker
// source pins, downloads, probes, starts and stops, and SSH installs.

package workerstaging

import (
	"archive/tar"
	"bufio"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strings"
	"sync"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/accounts"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// drainArchive reads a gzip tar to its end, as a receiver would.
func drainArchive(input io.Reader) error {
	decompressed, err := gzip.NewReader(input)
	if err != nil {
		return err
	}
	reader := tar.NewReader(decompressed)
	for {
		if _, err := reader.Next(); err == io.EOF {
			return nil
		} else if err != nil {
			return err
		}
		if _, err := io.Copy(io.Discard, reader); err != nil {
			return err
		}
	}
}

func TestMacsClaudeKeychainLoginIsTheOnlyStagedLoginAndHasNoRefreshToken(t *testing.T) {
	keychainLogin := []byte(`{"claudeAiOauth":{"accessToken":"keychain","refreshToken":"keychain-refresh","expiresAt":100}}`)
	stagedLoginBytes := `{"claudeAiOauth":{"accessToken":"keychain","expiresAt":100}}`
	// What StageProfile copies from a profile home that an earlier release
	// filled from a session.
	leftoverLogin := []byte(`{"claudeAiOauth":{"accessToken":"previous","refreshToken":"previous","expiresAt":200}}`)

	directory := t.TempDir()
	staged := filepath.Join(directory, "profile")
	makeFixtureDir(t, staged)
	stagedLogin := filepath.Join(staged, ".credentials.json")
	profile := core.NewHarnessProfile(core.HarnessKindClaude, filepath.Join(directory, "claude"))
	keychain := accounts.FakeKeychainHolding(keychainLogin)

	// The Keychain holds the login on a Mac only: nothing else asks it.
	if err := StageClaudeKeychainLogin(&profile, core.HarnessHostOther, staged, map[string]string{}, keychain); err != nil {
		t.Fatal(err)
	}
	if len(keychain.Calls()) != 0 {
		t.Fatal("the Keychain was asked off a Mac")
	}

	token := map[string]string{"CLAUDE_CODE_OAUTH_TOKEN": "token"}
	if err := StageClaudeKeychainLogin(&profile, core.HarnessHostMacOs, staged, token, keychain); err != nil {
		t.Fatal(err)
	}
	if len(keychain.Calls()) != 0 || fixtureExists(stagedLogin) {
		t.Fatal("a token login asked the Keychain")
	}

	if err := StageClaudeKeychainLogin(&profile, core.HarnessHostMacOs, staged, map[string]string{}, keychain); err != nil {
		t.Fatal(err)
	}
	if readFixture(t, stagedLogin) != stagedLoginBytes {
		t.Fatal(readFixture(t, stagedLogin))
	}

	// Freshness cannot distinguish a refreshed grant from another account.
	// The current Keychain login stays authoritative even when an earlier
	// staged copy has a later expiry.
	writeFixture(t, stagedLogin, leftoverLogin)
	if err := StageClaudeKeychainLogin(&profile, core.HarnessHostMacOs, staged, map[string]string{}, keychain); err != nil {
		t.Fatal(err)
	}
	if readFixture(t, stagedLogin) != stagedLoginBytes {
		t.Fatal(readFixture(t, stagedLogin))
	}

	// With no Keychain item, or a locked one, a leftover profile-home file
	// is not a login and is never staged as one.
	for _, keychain := range []*accounts.FakeKeychain{
		accounts.NewFakeKeychain(),
		accounts.FakeKeychainHolding(keychainLogin).Locked(),
	} {
		writeFixture(t, stagedLogin, leftoverLogin)
		if err := StageClaudeKeychainLogin(&profile, core.HarnessHostMacOs, staged, map[string]string{}, keychain); err != nil {
			t.Fatal(err)
		}
		if fixtureExists(stagedLogin) {
			t.Fatal("a leftover login was staged")
		}
	}
}

// commandRecorder records the commands an executor runs.
type commandRecorder struct {
	mu       sync.Mutex
	commands []core.CommandSpec
}

func (r *commandRecorder) record(command *core.CommandSpec) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.commands = append(r.commands, *command)
}

func (r *commandRecorder) all() []core.CommandSpec {
	r.mu.Lock()
	defer r.mu.Unlock()
	return slices.Clone(r.commands)
}

// take is the recorded commands, clearing the record (Rust: RefCell::take).
func (r *commandRecorder) take() []core.CommandSpec {
	r.mu.Lock()
	defer r.mu.Unlock()
	commands := r.commands
	r.commands = nil
	return commands
}

// recordingExecutor records every command and answers status 0 with no
// output.
func recordingExecutor(recorder *commandRecorder) *workerTestExecutor {
	return &workerTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		recorder.record(command)
		return core.CommandOutput{}, nil
	}}
}

func renderedCommands(commands []core.CommandSpec) []string {
	lines := make([]string, len(commands))
	for i, command := range commands {
		lines[i] = command.Program + " " + strings.Join(command.Args, " ")
	}
	return lines
}

// A client replacing the daemon pins the new build's workers during the
// handoff. The daemon it then starts must find them already indexed, and
// must still take the pinned snapshot itself.
// Hard-won: daa70d7: worker source warming delayed daemon reconnect under large-session load
func TestWarmingPinsTheSourcesForTheNextDaemonWithoutTakingItsSnapshot(t *testing.T) {
	const child = "MJ_WARM_WORKER_PIN_CHILD"
	if _, ok := os.LookupEnv(child); !ok {
		directory := t.TempDir()
		workers := filepath.Join(directory, "workers")
		writeFixture(t, filepath.Join(workers, "mj-worker-x86_64-unknown-linux-musl"), StampedWorker([]byte("current")))
		targets.NewIsolatedTest(targets.TestName(testsModule, "TestWarmingPinsTheSourcesForTheNextDaemonWithoutTakingItsSnapshot")).
			IsolatedStore(directory).
			Env("MJ_INSTANCE", "handoff-warm-pins").
			Env(child, "1").
			Env("MJ_WORKER_DIR", workers).
			EnvRemove("MJ_WORKER_BINARY").
			Run()
		return
	}
	source := filepath.Join(os.Getenv("MJ_WORKER_DIR"), "mj-worker-x86_64-unknown-linux-musl")
	cacheRoot := filepath.Join(core.DataDir(), "workers", "pinned")
	if _, ok := IndexedPinnedWorker(cacheRoot, source); ok {
		t.Fatal("the source was indexed before warming")
	}
	if err := WarmWorkerBinarySources(); err != nil {
		t.Fatal(err)
	}
	warmed, ok := IndexedPinnedWorker(cacheRoot, source)
	if !ok {
		t.Fatal("warming indexed the source for the next daemon")
	}
	if PinnedWorkerBinarySources.Get() != nil {
		t.Fatal("warming must leave the pinned snapshot to the daemon")
	}
	if err := PinWorkerBinarySources(); err != nil {
		t.Fatal(err)
	}
	if path := localWorkerPath(t); path != warmed {
		t.Fatalf("pinned %s, warmed %s", path, warmed)
	}
}

// Test-and-fix M-4: the daemon log named neither the worker it chose for a
// target nor where it came from.
// Hard-won: deb1c66: shipped startup logs omitted the selected worker source and build diagnostics
func TestPinningLogsEachSelectedWorkerWithItsSourceAndBuild(t *testing.T) {
	// Other tests install handlers of their own, so the event is checked
	// alone, in a child with a global one.
	const child = "MJ_PINNING_LOG_TEST_CHILD"
	if _, ok := os.LookupEnv(child); !ok {
		root := t.TempDir()
		targets.NewIsolatedTest(targets.TestName(testsModule, "TestPinningLogsEachSelectedWorkerWithItsSourceAndBuild")).
			Env(child, "1").
			IsolatedStore(root).
			Run()
		return
	}
	directory := t.TempDir()
	worker := filepath.Join(directory, "worker")
	writeFixture(t, worker, StampedWorker([]byte("logged bytes")))
	cache := filepath.Join(directory, "cache")
	log := core.NewCapturedLog()
	slog.SetDefault(slog.New(log))
	WorkerBinarySourceSnapshotCapture(cache, func(arch string, requirement WorkerBinaryRequirement) (WorkerBinaryAvailability, error) {
		if arch == "x86_64" && requirement == WorkerBinaryRequirementPortableLinux {
			return WorkerBinaryAvailabilityLocal{Path: worker, Source: "beside the mj binary"}, nil
		}
		return nil, errors.New("no worker")
	})
	var selected []string
	for _, event := range log.AtOrAbove(slog.LevelInfo) {
		if strings.Contains(event, "worker source selected") {
			selected = append(selected, event)
		}
	}
	if len(selected) != 1 {
		t.Fatalf("%q", selected)
	}
	for _, expected := range []string{"beside the mj binary", "x86_64-unknown-linux-musl", worker, core.WorkerBuildID()} {
		if !strings.Contains(selected[0], expected) {
			t.Fatalf("missing %q in %s", expected, selected[0])
		}
	}
}

func TestPinningRejectsStaleSourcesBeforePublication(t *testing.T) {
	directory := t.TempDir()
	source := filepath.Join(directory, "worker")
	writeFixture(t, source, []byte("legacy worker"))
	cache := filepath.Join(directory, "cache")
	snapshot := WorkerBinarySourceSnapshotCapture(cache, func(string, WorkerBinaryRequirement) (WorkerBinaryAvailability, error) {
		return WorkerBinaryAvailabilityLocal{Path: source, Source: "stale fixture"}, nil
	})
	if _, err := snapshot.Resolve("x86_64", WorkerBinaryRequirementPortableLinux); err == nil {
		t.Fatal("a stale source was pinned")
	}
	if dirEntryCountOrZero(t, cache) != 0 {
		t.Fatal("a stale source was published")
	}
}

func dirEntryCountOrZero(t *testing.T, path string) int {
	t.Helper()
	entries, err := os.ReadDir(path)
	if err != nil {
		t.Fatal(err)
	}
	return len(entries)
}

func TestDownloadsRejectStaleBuildsEvenWithTheExpectedChecksum(t *testing.T) {
	const child = "MJ_STALE_DOWNLOAD_CHILD"
	if _, ok := os.LookupEnv(child); !ok {
		directory := t.TempDir()
		targets.NewIsolatedTest(targets.TestName(testsModule, "TestDownloadsRejectStaleBuildsEvenWithTheExpectedChecksum")).
			IsolatedStore(directory).
			Env("MJ_INSTANCE", "issue-1138-download").
			Env(child, "1").
			Run()
		return
	}
	body := []byte(strings.Repeat("x", 150_000))
	sum := sha256.Sum256(body)
	digest := core.LowerHex(sum[:])
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	url := fmt.Sprintf("http://%s/worker", listener.Addr())
	served := make(chan error, 1)
	go func() {
		stream, err := listener.Accept()
		if err != nil {
			served <- err
			return
		}
		defer stream.Close()
		request := bufio.NewReader(stream)
		for {
			line, err := request.ReadString('\n')
			if err != nil {
				served <- err
				return
			}
			if line == "\r\n" {
				break
			}
		}
		fmt.Fprintf(stream, "HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n", len(body))
		_, err = stream.Write(body)
		served <- err
	}()
	_, err = DownloadWorker(url, digest, "x86_64-unknown-linux-musl")
	if serveErr := <-served; serveErr != nil {
		t.Fatal(serveErr)
	}
	requireErrorContains(t, err, "missing worker build stamp")
	cached := filepath.Join(core.DataDir(), "workers/pinned", digest, "hel")
	if fixtureExists(cached) {
		t.Fatal("a stale download was cached")
	}
	writeFixture(t, cached, body)
	_, err = DownloadWorker(url, digest, "x86_64-unknown-linux-musl")
	requireErrorContains(t, err, "missing worker build stamp")
}

// digestExecutor answers every command with installedLine, as a target's
// digest probe of its installed worker.
func digestExecutor(installedLine string, recorder *commandRecorder) *workerTestExecutor {
	return &workerTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		recorder.record(command)
		return commandOutputOf(0, installedLine, ""), nil
	}}
}

// sshBareLocator: the SshBare worker_root guard requires the workspace to
// end in the exact session ID, so build the locator around the session under
// test.
func sshBareLocator(sessionID string) core.TargetsTargetLocator {
	return core.TargetsTargetLocator{
		Kind:      core.TargetsTargetLocatorSshBare,
		SSH:       &core.SshTarget{Destination: "user@host.test", SSHArgs: []string{}},
		Workspace: "/srv/mj/" + sessionID,
	}
}

func TestAMatchingDigestNeverAuthorizesAStaleWorker(t *testing.T) {
	directory := t.TempDir()
	source := filepath.Join(directory, "worker")
	writeFixture(t, source, []byte("same stale worker on host and target"))
	digest, err := core.WorkerExecutableDigest(source)
	if err != nil {
		t.Fatal(err)
	}
	var recorder commandRecorder
	executor := digestExecutor(digest+"  /root/hel\n", &recorder)
	locator := sshBareLocator("session-remote")
	probe := core.NewCommandSpec("true", nil)
	_, err = ReplaceTargetWorkerBinaryIfStale(executor, &locator, "session-remote", &probe, source)
	requireErrorContains(t, err, "missing worker build stamp")
	if len(recorder.all()) != 0 {
		t.Fatal("a stale worker reached the target")
	}
}

// A reqwest::blocking::Client owns a private Tokio runtime that it drops
// with the client, which panics inside a Tokio context. Go has no such
// runtime; the helper must still return an ordinary error for a refused
// connection.
func TestFetchCatalogOverHTTPSDoesNotPanicInsideARuntimeContext(t *testing.T) {
	// Port 1 refuses the connection at once, so no network is needed.
	result, err := FetchCatalogOverHTTPS("http://127.0.0.1:1/models", "unused-key")
	if err == nil {
		t.Fatalf("expected a connection error, got %q", result)
	}
}

// The stored choice selects delegation for supported parent harnesses, but
// a child never receives its parent's Mjolnir delegation tools.
func TestTheSessionChoiceDecidesWhetherMjolnirReplacesNativeDelegation(t *testing.T) {
	claude := func(choice *bool) core.SessionRecord {
		session := store.CheckpointTestSession("s-1")
		session.HarnessKind = core.HarnessKindClaude
		if choice != nil {
			if *choice {
				session.Subagents = &core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir}
			} else {
				session.Subagents = &core.SubagentPolicy{Mode: core.SubagentPolicyNative}
			}
		}
		return session
	}
	yes, no := true, false
	check := func(session core.SessionRecord, isChild, want bool) {
		t.Helper()
		if got := SubagentToolsEnabled(&session, isChild); got != want {
			t.Fatalf("SubagentToolsEnabled = %v, want %v", got, want)
		}
	}
	check(claude(&no), false, false)
	check(claude(&yes), false, true)
	check(claude(nil), false, false)
	check(claude(&yes), true, false)

	grok := claude(&yes)
	grok.HarnessKind = core.HarnessKindGrok
	check(grok, false, false)

	codex := claude(nil)
	codex.HarnessKind = core.HarnessKindCodex
	check(codex, false, false)
	codex.Subagents = &core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir}
	check(codex, false, true)
}

func TestAStoredSetupTokenReachesOnlyClaudeWorkersThatDoNotSetTheirOwn(t *testing.T) {
	directory := t.TempDir()
	tokenPath := filepath.Join(directory, "profiles/claude/claude-oauth-token")
	missing := filepath.Join(directory, "profiles/absent/claude-oauth-token")
	if err := core.WriteClaudeOauthToken(tokenPath, []byte("sk-ant-oat01-stored")); err != nil {
		t.Fatal(err)
	}

	claude := map[string]string{}
	if !ApplyClaudeSetupToken(claude, core.HarnessKindClaude, tokenPath) || claude[core.ClaudeOauthTokenEnv] != "sk-ant-oat01-stored" {
		t.Fatalf("%v", claude)
	}

	// Every other harness ignores the variable, so it must not appear.
	for _, kind := range core.HarnessKindAll {
		if kind == core.HarnessKindClaude {
			continue
		}
		environment := map[string]string{}
		if ApplyClaudeSetupToken(environment, kind, tokenPath) || len(environment) != 0 {
			t.Fatalf("%v must not read the token", kind)
		}
	}

	// A profile that sets the variable itself stays authoritative.
	overridden := map[string]string{core.ClaudeOauthTokenEnv: "profile-token"}
	if !ApplyClaudeSetupToken(overridden, core.HarnessKindClaude, tokenPath) || overridden[core.ClaudeOauthTokenEnv] != "profile-token" {
		t.Fatalf("%v", overridden)
	}

	// A profile with no stored token launches exactly as before.
	without := map[string]string{}
	if ApplyClaudeSetupToken(without, core.HarnessKindClaude, missing) || len(without) != 0 {
		t.Fatalf("%v", without)
	}

	// A blank profile value is not a login and must not hide the stored
	// token.
	blank := map[string]string{core.ClaudeOauthTokenEnv: "  "}
	if !ApplyClaudeSetupToken(blank, core.HarnessKindClaude, tokenPath) || blank[core.ClaudeOauthTokenEnv] != "sk-ant-oat01-stored" {
		t.Fatalf("%v", blank)
	}
}

func TestPinnedSnapshotSurvivesSourceReplacementAndMissingCandidateInstall(t *testing.T) {
	directory := t.TempDir()
	source := filepath.Join(directory, "worker")
	writeFixture(t, source, StampedWorker([]byte("before")))
	cache := filepath.Join(directory, "cache")
	resolveSource := func(string, WorkerBinaryRequirement) (WorkerBinaryAvailability, error) {
		return WorkerBinaryAvailabilityLocal{Path: source, Source: "test source"}, nil
	}
	resolvedBytes := func(snapshot *WorkerBinarySourceSnapshot) string {
		t.Helper()
		selected, err := snapshot.Resolve("x86_64", WorkerBinaryRequirementPortableLinux)
		if err != nil {
			t.Fatal(err)
		}
		local, ok := selected.(WorkerBinaryAvailabilityLocal)
		if !ok {
			t.Fatal("source should be local")
		}
		return readFixture(t, local.Path)
	}
	pinned := WorkerBinarySourceSnapshotCapture(cache, resolveSource)

	writeFixture(t, source, []byte("in-place mutation"))
	if resolvedBytes(pinned) != string(StampedWorker([]byte("before"))) {
		t.Fatal("an in-place mutation reached the pin")
	}

	replacement := filepath.Join(directory, "replacement")
	writeFixture(t, replacement, StampedWorker([]byte("after")))
	if err := os.Rename(replacement, source); err != nil {
		t.Fatal(err)
	}
	if resolvedBytes(pinned) != string(StampedWorker([]byte("before"))) {
		t.Fatal("a replacement reached the pin")
	}
	freshReplaced := WorkerBinarySourceSnapshotCapture(cache, resolveSource)
	if resolvedBytes(freshReplaced) != string(StampedWorker([]byte("after"))) {
		t.Fatal("a fresh pin missed the replacement")
	}

	missing := filepath.Join(directory, "missing-worker")
	missingSnapshot := WorkerBinarySourceSnapshotCapture(cache, func(string, WorkerBinaryRequirement) (WorkerBinaryAvailability, error) {
		if isFile(missing) {
			return WorkerBinaryAvailabilityLocal{Path: missing, Source: "new source"}, nil
		}
		return nil, errors.New("candidate is unavailable")
	})
	writeFixture(t, missing, StampedWorker([]byte("now installed")))
	if _, err := missingSnapshot.Resolve("x86_64", WorkerBinaryRequirementPortableLinux); err == nil {
		t.Fatal("a snapshot taken before the install found the candidate")
	}
	freshSnapshot := WorkerBinarySourceSnapshotCapture(cache, func(string, WorkerBinaryRequirement) (WorkerBinaryAvailability, error) {
		return WorkerBinaryAvailabilityLocal{Path: missing, Source: "new source"}, nil
	})
	if _, err := freshSnapshot.Resolve("x86_64", WorkerBinaryRequirementPortableLinux); err != nil {
		t.Fatal(err)
	}

	var remoteURL struct {
		sync.Mutex
		value string
	}
	remoteURL.value = "https://old.example/{target}"
	remoteSnapshot := WorkerBinarySourceSnapshotCapture(filepath.Join(directory, "remote-cache"), func(arch string, _ WorkerBinaryRequirement) (WorkerBinaryAvailability, error) {
		remoteURL.Lock()
		defer remoteURL.Unlock()
		return WorkerBinaryAvailabilityRemote{
			URL:    strings.ReplaceAll(remoteURL.value, "{target}", arch),
			SHA256: strings.Repeat("a", 64),
			Triple: arch + "-unknown-linux-musl",
		}, nil
	})
	remoteURL.Lock()
	remoteURL.value = "https://new.example/{target}"
	remoteURL.Unlock()
	selected, err := remoteSnapshot.Resolve("x86_64", WorkerBinaryRequirementPortableLinux)
	if err != nil {
		t.Fatal(err)
	}
	remote, ok := selected.(WorkerBinaryAvailabilityRemote)
	if !ok || remote.URL != "https://old.example/x86_64" {
		t.Fatalf("%#v", selected)
	}

	blockedCache := filepath.Join(directory, "blocked-cache")
	writeFixture(t, blockedCache, []byte("not a directory"))
	failedSnapshot := WorkerBinarySourceSnapshotCapture(blockedCache, resolveSource)
	if _, err := failedSnapshot.Resolve("x86_64", WorkerBinaryRequirementPortableLinux); err == nil {
		t.Fatal("a pin into a file succeeded")
	}
}

func TestReplacedDevControllerStillFindsItsMuslSibling(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("Linux only, as in Rust")
	}
	controller := "target/debug/mj (deleted)"
	musl := "target/x86_64-unknown-linux-musl/debug/mj"
	path, source, ok := SelectSiblingWorker(controller, "x86_64-unknown-linux-musl", func(path string) bool { return path == musl })
	if !ok || path != musl || source != "development musl sibling" {
		t.Fatalf("%q %q %v", path, source, ok)
	}
}

func TestReplacedDevControllerNeverSelectsTheNewGlibcControllerAsItsWorker(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("Linux only, as in Rust")
	}
	controller := "target/debug/mj (deleted)"
	replacement := "target/debug/mj"
	if path, _, ok := SelectSiblingWorker(controller, "x86_64-unknown-linux-musl", func(path string) bool { return path == replacement }); ok {
		t.Fatalf("selected %s", path)
	}
}

// foreignArch is an architecture no host builds for, so the lookup cannot
// take one of the "native mj binary" shortcuts and reaches the end on any
// machine.
const foreignArch = "riscv64"

// A rebuilt or renamed checkout leaves a running daemon pointing at a path
// that holds nothing. Searching beside that path finds nothing and blames
// the user for a worker that may well be installed correctly.
func TestAReplacedControllerIsReportedInsteadOfAMissingWorker(t *testing.T) {
	stale := "/src/.backup-vHXvCs/target/debug/mj (deleted)"
	var probed []string
	_, err := WorkerBinaryPrerequisiteForCurrent(foreignArch, WorkerBinaryRequirementPortableLinux, stale, func(path string) bool {
		probed = append(probed, path)
		return false
	})
	requireErrorContains(t, err, "was replaced or removed on disk", "restart the Mjolnir daemon",
		// The path is named without the kernel's deletion marker.
		"/src/.backup-vHXvCs/target/debug/mj)")
	if strings.Contains(err.Error(), "(deleted)") {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(probed, []string{stale}) {
		t.Fatalf("nothing beside a path that no longer exists is worth probing: %q", probed)
	}
}

// The guard is about a controller path that no longer exists and nothing
// else: a controller still on disk keeps its whole sibling lookup, and keeps
// the plain "no Linux worker" answer when that lookup comes up empty. A
// present controller is never its own portable worker, so with nothing
// installed beside it the lookup ends in that plain answer.
func TestAPresentControllerStillLooksBesideItself(t *testing.T) {
	controller := "/opt/brokk/mj"
	var probed []string
	_, err := WorkerBinaryPrerequisiteForCurrent(foreignArch, WorkerBinaryRequirementPortableLinux, controller, func(path string) bool {
		probed = append(probed, path)
		return path == controller
	})
	if !slices.ContainsFunc(probed, func(path string) bool { return filepath.Base(path) == "mj-worker-riscv64-unknown-linux-musl" }) {
		t.Fatalf("the packaged worker name must still be probed: %q", probed)
	}
	requireErrorContains(t, err, "no worker for riscv64-unknown-linux-musl")
	if strings.Contains(err.Error(), "restart the Mjolnir daemon") {
		t.Fatal(err)
	}

	// With nothing beside it either, a present controller still gets the
	// generic message; only a replaced one is told to restart.
	root := "/"
	_, err = WorkerBinaryPrerequisiteForCurrent(foreignArch, WorkerBinaryRequirementPortableLinux, root, func(path string) bool { return path == root })
	requireErrorContains(t, err, "no worker for riscv64-unknown-linux-musl")
	if strings.Contains(err.Error(), "restart the Mjolnir daemon") {
		t.Fatal(err)
	}
}

// The worker writes its records without a trailing newline. The probe read
// them out of a text dump by searching for the next section marker, which
// that missing newline hid, so every startup step read as none and the
// readiness wait never extended for a worker that was making progress. The
// probe now runs the real script and reads one JSON document.
//
// The root contains spaces because macOS puts worker roots under
// `~/Library/Application Support/...`; an unquoted root split the script
// into separate words.
// Hard-won: 2fd45e3: worker records without a trailing newline were mistaken for empty output
func TestProbingADeadWorkerReadsItsRecordsAsTheWorkerWroteThem(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("#1297: runs sh, du or touch on the controller")
	}
	const session = "0123456789abcdef0123456789abcdef"
	root := filepath.Join(t.TempDir(), "Application Support", "0123456789abcdef0123456789abcdef")
	writeFixture(t, filepath.Join(root, core.WorkerStartupFile), []byte(`{
  "step": "harness-resolve",
  "pid": 999999999,
  "steps": [
    { "step": "start", "at": "2026-09-30T03:20:42.270Z" },
    { "step": "harness-resolve", "at": "2026-09-30T03:20:44.173Z" }
  ]
}`))
	writeFixture(t, filepath.Join(root, core.WorkerExitFile), []byte("{\n  \"reason\": \"panic\",\n  \"refusal\": null\n}"))
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: root}

	probe, err := ProbeWorker(core.ProcessExecutor{}, &locator, session, root)
	if err != nil {
		t.Fatal(err)
	}

	if step, ok := probe.Step(); !ok || step != "harness-resolve" {
		t.Fatalf("step %q %v", step, ok)
	}
	if probe.Exit == nil || probe.Exit.Reason != "panic" {
		t.Fatalf("%v", probe.Exit)
	}
	// No worker runs for this temporary root.
	if probe.Alive() {
		t.Fatalf("%v", probe)
	}
	if probe.String() != "the worker exited: panic" {
		t.Fatal(probe.String())
	}
}

// Output that is not the probe's document is an error, not a worker with
// nothing to report.
func TestAProbeThatPrintsSomethingElseIsAnError(t *testing.T) {
	const session = "0123456789abcdef0123456789abcdef"
	garbage := &workerTestExecutor{execute: func(*core.CommandSpec) (core.CommandOutput, error) {
		return commandOutputOf(0, "{\"startup\":{\"step\":\"start\"}--- worker.log", ""), nil
	}}
	root := "/nonexistent/" + session
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: root}

	_, err := ProbeWorker(garbage, &locator, session, root)
	requireErrorContains(t, err, "read the worker probe")
}

func workerPermitFor(t *testing.T, session, reason string) *store.WorkerPermit {
	t.Helper()
	owner, err := store.WorkerTestPermit(context.Background(), session, reason)
	if err != nil {
		t.Fatal(err)
	}
	if owner == nil {
		t.Fatal("the test gate refused a worker permit")
	}
	return owner
}

// A worker that died leaves an exit record behind. Starting a new worker
// must clear it first, or the startup connect loop reads the previous death
// as this worker's and gives up on a healthy daemon.
// Hard-won: a6ad3cc: concurrent checkpoint, upgrade, and recovery paths shared worker launch logs
func TestStartingAWorkerUsesPrivateLogsWithoutTouchingIncumbentFiles(t *testing.T) {
	const child = "MJ_TEST_PRIVATE_WORKER_LAUNCH_LOGS"
	if _, ok := os.LookupEnv(child); !ok {
		directory := t.TempDir()
		targets.NewIsolatedTest(targets.TestName(testsModule, "TestStartingAWorkerUsesPrivateLogsWithoutTouchingIncumbentFiles")).
			IsolatedStore(directory).
			Env(child, "1").
			Run()
		return
	}
	writer := store.InstallIsolatedTestWriter()
	defer writer.Shutdown()
	tokenPath := filepath.Join(t.TempDir(), "agent-pat")
	writeFixture(t, tokenPath, []byte("github_pat_at_start"))
	github := core.GithubConfig{SessionTokenPath: &tokenPath}

	const id = "13131313131313131313131313131313"
	podman, err := core.ResourceName(id)
	if err != nil {
		t.Fatal(err)
	}
	for _, locator := range []core.TargetsTargetLocator{
		{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: "/worker/root/" + id},
		{Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: podman},
	} {
		session := store.CheckpointTestSession(id)
		if locator.Kind == core.TargetsTargetLocatorLocalBare {
			session.Target = core.TargetLocatorLocalBare{WorkerRoot: locator.WorkerRoot}
		} else {
			session.Target = core.TargetLocatorLocalPodman{ContainerID: locator.ContainerID}
		}
		if err := store.SaveSession(&session); err != nil {
			t.Fatal(err)
		}
		var recorder commandRecorder
		executor := recordingExecutor(&recorder)
		root, err := core.WorkerRoot(&locator, id)
		if err != nil {
			t.Fatal(err)
		}
		if err := StartWorker(workerPermitFor(t, id, "test start"), executor, &locator, root, &github); err != nil {
			t.Fatal(err)
		}

		commands := recorder.all()
		if commands[0].Purpose != "install configured session GitHub token" {
			t.Fatal(commands[0].Purpose)
		}
		if !slices.ContainsFunc(commands[1].Args, func(arg string) bool { return strings.Contains(arg, "worker-launch.XXXXXXXX") }) {
			t.Fatalf("%q", commands[1].Args)
		}
		if strings.Contains(fmt.Sprintf("%#v", commands), "github_pat_at_start") {
			t.Fatal("the token reached a command")
		}
		var script string
		for _, command := range commands {
			for _, argument := range command.Args {
				if script == "" && strings.Contains(argument, "worker-launch.XXXXXXXX") {
					script = argument
				}
			}
		}
		if script == "" {
			t.Fatal("a launch has a private diagnostic log")
		}
		if strings.Contains(script, "rm -f") {
			t.Fatalf("launchers cannot clear incumbent files: %s", script)
		}
		for _, incumbent := range []string{"worker-exit.json", "control.sock", "/worker.log"} {
			if strings.Contains(script, incumbent) {
				t.Fatalf("the launcher touches %s: %s", incumbent, script)
			}
		}
		session.NoGithubCredentials = true
		if err := store.SaveSession(&session); err != nil {
			t.Fatal(err)
		}
		recorder.take()
		root, err = core.WorkerRoot(&locator, session.ID)
		if err != nil {
			t.Fatal(err)
		}
		if err := StartWorker(workerPermitFor(t, session.ID, "credential-free start"), executor, &locator, root, &github); err != nil {
			t.Fatal(err)
		}
		for _, command := range recorder.all() {
			if command.Purpose == "install configured session GitHub token" {
				t.Fatal("a credential-free session got the GitHub token")
			}
		}
	}
}

func TestStoppingAWorkerRunsTheDaemonStopScript(t *testing.T) {
	const id = "14141414141414141414141414141414"
	locator := core.TargetsTargetLocator{
		Kind:      core.TargetsTargetLocatorSshBare,
		SSH:       &core.SshTarget{Destination: "user@example.test", SSHArgs: []string{}},
		Workspace: "/workspace/" + id,
	}
	var recorder commandRecorder
	executor := recordingExecutor(&recorder)
	root, err := core.WorkerRoot(&locator, id)
	if err != nil {
		t.Fatal(err)
	}
	if err := StopWorker(workerPermitFor(t, id, "test stop"), executor, &locator, root); err != nil {
		t.Fatal(err)
	}

	commands := recorder.all()
	// One command: the script itself picks the binary on the target.
	if len(commands) != 1 || commands[0].Purpose != "stop Mjolnir worker daemon" {
		t.Fatalf("%#v", commands)
	}
	last := commands[0].Args[len(commands[0].Args)-1]
	if !strings.HasPrefix(targets.UnwrappedRemoteLine(last), "'sh' '-c' ") {
		t.Fatalf("raw SSH worker management must not source login profiles: %#v", commands)
	}
	if !slices.ContainsFunc(commands[0].Args, func(argument string) bool { return strings.Contains(argument, "worker stop --root") }) {
		t.Fatalf("stop must go through the installed worker binary: %#v", commands)
	}
}

func TestCheckpointWorkerStopRestoresAStoppedPodmanTargetFirst(t *testing.T) {
	const session = "17171717171717171717171717171717"
	containerID, err := core.ResourceName(session)
	if err != nil {
		t.Fatal(err)
	}
	inspection := func(status string) core.CommandOutput {
		document, err := json.Marshal([]any{map[string]any{
			"Config": map[string]any{"Labels": map[string]any{
				core.ManagedLabel: "true",
				core.SessionLabel: session,
			}},
			"State": map[string]any{"Status": status},
		}})
		if err != nil {
			t.Fatal(err)
		}
		return core.CommandOutput{Stdout: document}
	}
	outputs := []core.CommandOutput{{}, inspection("exited"), {}, inspection("running"), {}, {}}
	var recorder commandRecorder
	executor := &workerTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		recorder.record(command)
		output := outputs[0]
		outputs = outputs[1:]
		return output, nil
	}}
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: containerID}
	root, err := core.WorkerRoot(&locator, session)
	if err != nil {
		t.Fatal(err)
	}
	owner := workerPermitFor(t, session, "test stop")
	if err := StopWorkerAfterTargetRecovery(owner.ScopeBlocking(context.Background()), executor, &locator, session, root); err != nil {
		t.Fatal(err)
	}

	var purposes []string
	for _, command := range recorder.all() {
		purposes = append(purposes, command.Purpose)
	}
	want := []string{
		"check for Mjolnir session container",
		"inspect Mjolnir session container",
		"start stopped Mjolnir session container",
		"inspect Mjolnir session container",
		"stop Mjolnir worker daemon",
	}
	if !reflect.DeepEqual(purposes, want) {
		t.Fatalf("%q", purposes)
	}
}

// sshHostExecutor is an SSH host that remembers published store paths and
// returns their digest on later probes, including a container's
// transport-cache lookup.
type sshHostExecutor struct {
	workerTestExecutor
	recorder      commandRecorder
	hostFiles     map[string]struct{}
	workerUploads [][]byte
}

func newSSHHostExecutor() *sshHostExecutor {
	host := &sshHostExecutor{hostFiles: map[string]struct{}{}}
	host.workerTestExecutor.execute = host.run
	host.workerTestExecutor.executeWithStdin = host.runWithStdin
	return host
}

func isHexDigest(text string) bool {
	if len(text) != 64 {
		return false
	}
	for _, character := range []byte(text) {
		if !(character >= '0' && character <= '9' || character >= 'a' && character <= 'f' || character >= 'A' && character <= 'F') {
			return false
		}
	}
	return true
}

func (h *sshHostExecutor) run(command *core.CommandSpec) (core.CommandOutput, error) {
	h.recorder.record(command)
	status := 0
	var stdout []byte
	remote := ""
	if len(command.Args) > 0 {
		remote = command.Args[len(command.Args)-1]
	}
	switch command.Purpose {
	case "check negotiated profile receiver support":
		// This models the legacy remote tar boundary; real negotiation is
		// exercised against the bundled receiver in profile_transfer.
		status = 127
	case "publish shared Mjolnir worker":
		// Publication now shares one shell batch with verification and
		// discard. Its directory is an argument inside the quoted script.
		unquoted := strings.NewReplacer("'", "", "\\", "", "\"", "").Replace(remote)
		directory := ""
		for _, word := range strings.Fields(unquoted) {
			if index := strings.LastIndex(word, "worker-binaries/"); index >= 0 && isHexDigest(word[index+len("worker-binaries/"):]) {
				directory = word
				break
			}
		}
		if directory == "" {
			panic("published worker store directory")
		}
		h.hostFiles[directory+"/hel"] = struct{}{}
	case "link shared Mjolnir worker into session":
		// Arguments end with: source temporary destination digest mode.
		words := strings.Fields(remote)
		source := strings.Trim(words[len(words)-5], "'")
		if _, ok := h.hostFiles[source]; !ok {
			status = 75
		}
	case "verify shared Mjolnir worker":
		words := strings.Fields(remote)
		path := strings.Trim(words[len(words)-1], "'")
		if _, ok := h.hostFiles[path]; ok {
			parts := strings.Split(path, "/")
			digest := parts[len(parts)-2]
			stdout = []byte(digest + "  " + path + "\n")
		} else {
			status = 1
		}
	}
	return core.CommandOutput{Status: status, Stdout: stdout}, nil
}

func (h *sshHostExecutor) runWithStdin(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	switch command.Purpose {
	case "stage replacement Mjolnir worker":
		sent, err := io.ReadAll(input)
		if err != nil {
			return core.CommandOutput{}, err
		}
		h.workerUploads = append(h.workerUploads, sent)
	case "upload SSH worker file", "upload remote container worker file":
		sent, err := io.ReadAll(input)
		if err != nil {
			return core.CommandOutput{}, err
		}
		if string(sent) != "{}" {
			panic(fmt.Sprintf("uploaded %q", sent))
		}
	case "query host profile content":
		if _, err := io.Copy(io.Discard, input); err != nil {
			return core.CommandOutput{}, err
		}
	case "send profile content":
		if err := drainArchive(input); err != nil {
			return core.CommandOutput{}, err
		}
	default:
		panic("unexpected SSH input for " + command.Purpose)
	}
	return h.run(command)
}

// takeUploads is the worker bytes uploaded so far, clearing the record.
func (h *sshHostExecutor) takeUploads() [][]byte {
	uploads := h.workerUploads
	h.workerUploads = nil
	return uploads
}

func installOnSSHHost(t *testing.T, executor *sshHostExecutor, fixture *PodmanInstallFixture, locator *core.TargetsTargetLocator, session string) []string {
	t.Helper()
	root, err := core.WorkerRoot(locator, session)
	if err != nil {
		t.Fatal(err)
	}
	if err := InstallWorkerFiles(executor, locator, session, root, root+"/profile", WorkerInstallHost{Path: fixture.WorkerBinary},
		fixture.LaunchConfig, fixture.Ownership, fixture.ProfileStage, nil, nil); err != nil {
		t.Fatal(err)
	}
	return renderedCommands(executor.recorder.take())
}

// sessionWorkerRoot is the home-relative worker root SSH-bare and EC2
// sessions use (see core.WorkerRoot), used for every install here so the
// roots are easy to name in assertions.
func sessionWorkerRoot(session string) string {
	return ".local/share/hel/workers/" + session
}

// bareLocatorOnTheContainerHost is an SSH-bare session on the same host as
// NewPodmanInstallFixture's container.
func bareLocatorOnTheContainerHost(session string) core.TargetsTargetLocator {
	return core.TargetsTargetLocator{
		Kind:      core.TargetsTargetLocatorSshBare,
		SSH:       &core.SshTarget{Destination: "user@example.test", SSHArgs: []string{}},
		Workspace: ".local/share/hel/workspaces/" + session,
	}
}

// R3-4 (J-16): every SSH-bare session uploaded its own 139 MB worker, so ten
// parallel creates on one host each held a daemon action slot for minutes.
// The binary now crosses the network once per build and host; each session
// gets a link in its own worker root.
// Hard-won: 62a232f: every SSH-bare session uploaded its full 139 MB worker and stalled creates
func TestSSHBareInstallsUploadTheWorkerOncePerHostAndLinkItPerSession(t *testing.T) {
	fixture := NewPodmanInstallFixture()
	defer fixture.Close()
	executor := newSSHHostExecutor()
	const first = "0123456789abcdef0123456789abcdef"
	const second = "fedcba9876543210fedcba9876543210"
	firstLocator := bareLocatorOnTheContainerHost(first)
	lines := installOnSSHHost(t, executor, fixture, &firstLocator, first)
	if uploads := executor.takeUploads(); len(uploads) != 1 || string(uploads[0]) != readFixture(t, fixture.WorkerBinary) {
		t.Fatalf("%d uploads", len(uploads))
	}
	if !slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, "worker-binaries/") }) {
		t.Fatalf("%q", lines)
	}
	secondLocator := bareLocatorOnTheContainerHost(second)
	lines = installOnSSHHost(t, executor, fixture, &secondLocator, second)
	if uploads := executor.takeUploads(); len(uploads) != 0 {
		t.Fatalf("%q", lines)
	}
	if !slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, "ln --") && strings.Contains(line, "mv -f --") }) {
		t.Fatalf("%q", lines)
	}
	for _, name := range []string{"launch.json", "ownership.json"} {
		destination := sessionWorkerRoot(second) + "/" + name
		if !slices.ContainsFunc(lines, func(line string) bool {
			return strings.HasPrefix(line, "ssh ") && strings.Contains(line, "mj-upload") && strings.Contains(line, destination)
		}) {
			t.Fatalf("%s was not uploaded: %q", name, lines)
		}
	}
}

// Hard-won: 62a232f: SSH-bare and SSH-container installs duplicated the host worker cache
func TestSSHBareAndSSHContainerInstallsShareOneWorkerCache(t *testing.T) {
	fixture := NewPodmanInstallFixture()
	defer fixture.Close()
	executor := newSSHHostExecutor()
	const bare = "0123456789abcdef0123456789abcdef"
	bareLocator := bareLocatorOnTheContainerHost(bare)
	installOnSSHHost(t, executor, fixture, &bareLocator, bare)

	if uploads := executor.takeUploads(); len(uploads) != 1 || string(uploads[0]) != readFixture(t, fixture.WorkerBinary) {
		t.Fatalf("%d uploads", len(uploads))
	}
	const container = "fedcba9876543210fedcba9876543210"
	lines := installOnSSHHost(t, executor, fixture, &fixture.Locator, container)
	if uploads := executor.takeUploads(); len(uploads) != 0 {
		t.Fatalf("a container session on the same host reuses the cached worker, got %q", lines)
	}

	nitroID, err := core.ResourceName(container)
	if err != nil {
		t.Fatal(err)
	}
	nitro := core.TargetsTargetLocator{
		Kind:        core.TargetsTargetLocatorSshNitro,
		SSH:         &core.SshTarget{Destination: "user@example.test", SSHArgs: []string{}},
		ContainerID: nitroID,
	}
	mbx := filepath.Join(filepath.Dir(fixture.WorkerBinary), "mbx")
	body := append([]byte("#!/bin/sh\n"), []byte(strings.Repeat("#", 128*1024))...)
	writeFixture(t, mbx, body)
	installed, err := InstallSessionExecutable(executor, &nitro, container, mbx, "mbx")
	if err != nil {
		t.Fatal(err)
	}
	if uploads := executor.takeUploads(); len(uploads) != 1 || string(uploads[0]) != string(body) {
		t.Fatalf("%d uploads", len(uploads))
	}

	guestRoot, err := core.WorkerRoot(&nitro, container)
	if err != nil {
		t.Fatal(err)
	}
	if installed != filepath.Join(guestRoot, "mbx") {
		t.Fatalf("installed %s", installed)
	}
	commands := renderedCommands(executor.recorder.take())
	if !slices.ContainsFunc(commands, func(line string) bool {
		return strings.Contains(line, "microvm") && strings.Contains(line, "'cp'") && strings.Contains(line, ".transport-")
	}) {
		t.Fatalf("%q", commands)
	}
	if !slices.ContainsFunc(commands, func(line string) bool {
		return strings.Contains(line, guestRoot+"/mbx") && strings.Contains(line, "ln --")
	}) {
		t.Fatalf("%q", commands)
	}
}

func TestDockerUploadsAndReplacementsAreUsableByTheNonRootWorker(t *testing.T) {
	if os.Getenv(targets.IncludeIgnoredEnv) != "1" {
		t.Skip("requires Docker and the locally installed agent-dev image")
	}
	fixture := NewPodmanInstallFixture()
	defer fixture.Close()
	session, err := core.NewSessionID()
	if err != nil {
		t.Fatal(err)
	}
	containerID, err := core.ResourceName(session)
	if err != nil {
		t.Fatal(err)
	}
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalDocker, ContainerID: containerID}
	process := core.ProcessExecutor{}
	if _, err := targets.ExecuteChecked(process, core.NewCommandSpec("docker", []string{
		"run", "--pull=never", "-d", "--name", containerID, "ghcr.io/brokkai/mjolnir/agent-dev:latest", "sleep", "infinity",
	})); err != nil {
		t.Fatal(err)
	}
	result := func() error {
		root, err := core.WorkerRoot(&locator, session)
		if err != nil {
			return err
		}
		profile := root + "/profile"
		if err := os.WriteFile(filepath.Join(fixture.ProfileStage, "credential"), []byte("private"), 0o666); err != nil {
			return err
		}
		if err := InstallWorkerFiles(process, &locator, session, root, profile, WorkerInstallHost{Path: fixture.WorkerBinary},
			fixture.LaunchConfig, fixture.Ownership, fixture.ProfileStage, nil, nil); err != nil {
			return err
		}
		owner, err := store.WorkerTestPermit(context.Background(), session, "test binary replacement")
		if err != nil {
			return err
		}
		if owner == nil {
			return errors.New("test worker is owned")
		}
		if err := ReplaceInstalledWorkerBinary(owner.ScopeBlocking(context.Background()), process, &locator, session, WorkerInstallHost{Path: fixture.WorkerBinary}); err != nil {
			return err
		}
		_, err = targets.ExecuteChecked(process, core.NewCommandSpec("docker", []string{
			"exec", containerID, "sh", "-c",
			"test \"$(id -u)\" != 0 && test -x \"$1/hel\" && test -r \"$1/launch.json\" && test -r \"$1/ownership.json\" && test -r \"$1/profile/credential\" && test -w \"$1/profile/credential\"",
			"sh", root,
		}))
		return err
	}()
	_, cleanup := targets.ExecuteChecked(process, core.NewCommandSpec("docker", []string{"rm", "-f", containerID}))
	if result != nil {
		t.Fatal(result)
	}
	if cleanup != nil {
		t.Fatal(cleanup)
	}
}

func TestReplacingAnInstalledPodmanWorkerWritesThroughANextPath(t *testing.T) {
	const session = "16161616161616161616161616161616"
	containerID, err := core.ResourceName(session)
	if err != nil {
		t.Fatal(err)
	}
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: containerID}
	var recorder commandRecorder
	executor := recordingExecutor(&recorder)
	fixture := NewPodmanInstallFixture()
	defer fixture.Close()
	plan, err := WorkerBinaryUploadPlan(&locator, session, WorkerInstallHost{Path: fixture.WorkerBinary},
		"/var/lib/hel/workers/"+session+"/hel.private", nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := plan.Execute(executor); err != nil {
		t.Fatal(err)
	}

	lines := renderedCommands(recorder.all())
	ownership := lines[1]
	lines = append(lines[:1:1], lines[2:]...)
	if !strings.HasPrefix(ownership, "podman exec --user 0:0 "+containerID+" sh -c") ||
		!strings.Contains(ownership, "chown -R") ||
		!strings.HasSuffix(ownership, "/var/lib/hel/workers/"+session+"/hel.private.next") {
		t.Fatal(ownership)
	}
	want := []string{
		fmt.Sprintf("podman cp %s %s:/var/lib/hel/workers/%s/hel.private.next", fixture.WorkerBinary, containerID, session),
		fmt.Sprintf("podman exec %s mv -f /var/lib/hel/workers/%s/hel.private.next /var/lib/hel/workers/%s/hel.private", containerID, session, session),
		fmt.Sprintf("podman exec %s chmod 700 /var/lib/hel/workers/%s/hel.private", containerID, session),
	}
	if !reflect.DeepEqual(lines, want) {
		t.Fatalf("%q", lines)
	}
}

func TestPodmanWorkerStartAndUpgradeUseEachContainersRecordedIdentity(t *testing.T) {
	const session = "26262626262626262626262626262626"
	source := filepath.Join(t.TempDir(), "replacement")
	writeFixture(t, source, StampedWorker([]byte("replacement")))
	localName, err := core.ResourceName(session)
	if err != nil {
		t.Fatal(err)
	}
	ssh := &core.SshTarget{Destination: "user@example.test", SSHArgs: []string{}}
	for _, entry := range []struct {
		kind    string
		locator core.TargetsTargetLocator
	}{
		{"local", core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: localName}},
		{"ssh", core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorSshPodman, SSH: ssh, ContainerID: localName}},
	} {
		start := StartWorkerCommand(&entry.locator, "/var/lib/hel/workers/session")
		startText := start.Program + " " + strings.Join(start.Args, " ")
		if strings.Contains(startText, "--user") {
			t.Fatalf("%s worker start must inherit the configured container user: %s", entry.kind, startText)
		}

		var remoteSource *string
		if entry.kind == "ssh" {
			private := ".private-host-worker/hel"
			remoteSource = &private
		}
		upgrade, err := WorkerBinaryUploadPlan(&entry.locator, session, WorkerInstallHost{Path: source},
			"/var/lib/hel/workers/"+session+"/hel.private", remoteSource)
		if err != nil {
			t.Fatal(err)
		}
		commands := renderedCommands(upgrade.Commands)
		index := slices.IndexFunc(commands, func(command string) bool { return strings.Contains(command, "chown -R") })
		if index < 0 {
			t.Fatal("the upload owner is normalized")
		}
		if ownership := commands[index]; !strings.Contains(ownership, "--user") || !strings.Contains(ownership, "0:0") {
			t.Fatalf("%s ownership normalization still needs container root: %s", entry.kind, ownership)
		}
		for _, command := range commands {
			if strings.Contains(command, "exec ") && !strings.Contains(command, "chown -R") && strings.Contains(command, "--user") {
				t.Fatalf("%s worker promotion must inherit the configured user: %s", entry.kind, command)
			}
		}
	}
}

// The Containerfile is a copy of containers/Containerfile.agent-dev as Rust
// include_str!s it. The copy carries e14e4a6fe's codex-acp 1.15.1 pin, the
// commit that moved the Go core pins and the image together after the
// pinned Rust snapshot; the image file was retired in 0fc96c573.
func TestBridgeFallbackPinsMatchTheAgentDevContainerfile(t *testing.T) {
	containerfileBytes, err := os.ReadFile("testdata/containers/Containerfile.agent-dev")
	if err != nil {
		t.Fatal(err)
	}
	containerfile := string(containerfileBytes)
	lockstep := "The image and the bridge_launch() npx fallbacks have to stay in lockstep, otherwise a container session and an npx session run different adapter versions."

	if codex := "codex-acp@" + core.CodexACPVersion; !strings.Contains(containerfile, codex) {
		t.Fatalf("containers/Containerfile.agent-dev must install %s. %s", codex, lockstep)
	}
	if claude := "claude-agent-acp@" + core.ClaudeACPVersion; !strings.Contains(containerfile, claude) {
		t.Fatalf("containers/Containerfile.agent-dev must install %s. %s", claude, lockstep)
	}
	if claudeCode := "@anthropic-ai/claude-code@" + core.ClaudeCLIVersion; !strings.Contains(containerfile, claudeCode) {
		t.Fatalf("containers/Containerfile.agent-dev must install %s", claudeCode)
	}
	if !strings.Contains(containerfile, "ENV CLAUDE_CODE_EXECUTABLE=/usr/local/bin/claude") {
		t.Fatal("the image does not name the Claude executable")
	}
	if muse := core.MuseACPPackage + "@" + core.MuseACPVersion; !strings.Contains(containerfile, muse) {
		t.Fatalf("containers/Containerfile.agent-dev must install %s. %s", muse, lockstep)
	}
	museCheck := "require('/usr/local/lib/node_modules/" + core.MuseACPPackage + "/package.json').version\")\" = \"" + core.MuseACPVersion + "\" ]"
	if !strings.Contains(containerfile, museCheck) {
		t.Fatalf("containers/Containerfile.agent-dev must check the installed adapter is %s; a stale check fails only when the image is published", core.MuseACPVersion)
	}
	if !strings.Contains(containerfile, "OPENCODE_DISABLE_AUTOUPDATE=1") {
		t.Fatal("the image leaves OpenCode updates on")
	}
	var antigravity map[string]any
	if err := json.Unmarshal([]byte(readFixture(t, "../harnessinstall/assets/antigravity/runtime.json")), &antigravity); err != nil {
		t.Fatal(err)
	}
	if antigravity["version"] != core.AntigravityVersion {
		t.Fatalf("antigravity runtime %v, pin %s", antigravity["version"], core.AntigravityVersion)
	}
	for _, line := range []string{
		"COPY mj-harness-install/assets/antigravity/runtime.json /opt/mjolnir/antigravity-runtime.json",
		"RUN python3 /opt/mjolnir/install-antigravity.py /opt/mjolnir/antigravity-runtime.json /usr/local/bin",
		"ENV AGY_CLI_DISABLE_AUTO_UPDATE=true",
	} {
		if !strings.Contains(containerfile, line) {
			t.Fatalf("the image lacks %q", line)
		}
	}

	for _, pkg := range []string{
		"@moonshot-ai/kimi-code@" + core.KimiVersion,
		"@xai-official/grok@" + core.GrokVersion,
		"@opencode/cli@" + core.OpencodeVersion,
	} {
		if !strings.Contains(containerfile, pkg) {
			t.Fatalf("containers/Containerfile.agent-dev must install %s", pkg)
		}
	}

	// DSH has no managed installation. The image installs the pinned npm
	// package, and the launch otherwise fetches that same version through
	// npx.
	dsh := core.DshACPPackage + "@" + core.DshACPVersion
	if !strings.Contains(containerfile, dsh) {
		t.Fatalf("containers/Containerfile.agent-dev must install %s", dsh)
	}
	command, arguments := BridgeLaunch(core.HarnessKindDsh, core.ExecutionPolicyConfiguredApprovals)
	if command != "sh" {
		t.Fatal(command)
	}
	if !slices.ContainsFunc(arguments, func(argument string) bool { return strings.Contains(argument, "npx -y "+dsh) }) {
		t.Fatalf("the DSH launch must fetch the pinned %s", dsh)
	}

	var pkg, lock map[string]any
	if err := json.Unmarshal([]byte(readFixture(t, "../harnessinstall/assets/harnesses/claude/package.json")), &pkg); err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal([]byte(readFixture(t, "../harnessinstall/assets/harnesses/claude/package-lock.json")), &lock); err != nil {
		t.Fatal(err)
	}
	lookup := func(value any, keys ...string) any {
		for _, key := range keys {
			object, ok := value.(map[string]any)
			if !ok {
				return nil
			}
			value = object[key]
		}
		return value
	}
	for _, dependency := range []struct{ name, version string }{
		{"@agentclientprotocol/claude-agent-acp", core.ClaudeACPVersion},
		{"@anthropic-ai/claude-code", core.ClaudeCLIVersion},
	} {
		if lookup(pkg, "dependencies", dependency.name) != dependency.version ||
			lookup(lock, "packages", "", "dependencies", dependency.name) != dependency.version ||
			lookup(lock, "packages", "node_modules/"+dependency.name, "version") != dependency.version {
			t.Fatalf("%s is not pinned at %s", dependency.name, dependency.version)
		}
	}
}
