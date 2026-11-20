// Port of the tests in mj-core/src/targets/ssh.rs.

package core

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// sharingTestLock: the connection-sharing override is process-wide, so the
// tests that set it take turns (Rust: SHARING_TEST_LOCK).
var sharingTestLock sync.Mutex

// lockMasterOpening takes the file lock that serializes checking and opening
// the master on socket across processes, without an executor.
func lockMasterOpening(socket string) (*FileLock, error) {
	return lockMasterOpeningUnless(socket, nil)
}

// sshExecuteOnly is a test executor whose every run goes to execute.
type sshExecuteOnly struct {
	CommandExecutorDefaults
	execute func(command *CommandSpec) (CommandOutput, error)
}

func (e *sshExecuteOnly) Execute(command *CommandSpec) (CommandOutput, error) {
	return e.execute(command)
}

func (e *sshExecuteOnly) ExecuteCleanup(command *CommandSpec) (CommandOutput, error) {
	return e.execute(command)
}

func (e *sshExecuteOnly) ExecuteCleanupWithTimeout(command *CommandSpec, _ time.Duration) (CommandOutput, error) {
	return e.execute(command)
}

func (e *sshExecuteOnly) ExecuteWithStdin(command *CommandSpec, _ io.Reader) (CommandOutput, error) {
	return e.execute(command)
}

func (e *sshExecuteOnly) ExecuteWithoutDeadline(command *CommandSpec) (CommandOutput, error) {
	return e.execute(command)
}

func (e *sshExecuteOnly) ExecuteWithStdinWithoutDeadline(command *CommandSpec, input io.Reader) (CommandOutput, error) {
	return e.ExecuteWithStdin(command, input)
}

func (e *sshExecuteOnly) CancellationRequestedWithoutDeadline() bool {
	return e.CancellationRequested()
}

func (e *sshExecuteOnly) PauseDeadline() *DeadlinePause { return DeadlinePauseNone() }

func sshPlainTarget(destination string) SshTarget {
	return SshTarget{Destination: destination}
}

func TestRemotePodmanExecKeepsTheContainerConfiguredUser(t *testing.T) {
	ssh := sshPlainTarget("podman-root-test")
	command := SSHCommandOwned(SshTrafficClassControl, &ssh, []string{"podman", "exec", "--detach", "session", "true"})
	if !strings.Contains(command.Args[len(command.Args)-1], "'podman' 'exec' '--detach' 'session' 'true'") {
		t.Fatalf("%q", command.Args)
	}
}

// An OpenSSH command session on macOS starts with only the base system
// directories on PATH, so every remote line must bring in /usr/local/bin
// (Apple's `container`, Docker Desktop) and /opt/homebrew/bin (Homebrew's
// `docker` and `podman`) by itself, after the directories the session
// already had. Another host runs the command with its PATH unchanged.
//
// Hard-won: f6b68bcc9: SSH Apple container probes ran `container` without /usr/local/bin and exited 127.
func TestRemoteLinesRunUnderTheMacosSystemPath(t *testing.T) {
	skipUnlessUnix(t)
	session := "/usr/bin:/bin:/usr/sbin:/sbin"
	probe := RemoteCommandLine([]string{"sh", "-c", `printf '%s|%s' "$*" "$PATH"`, "probe", "two words"})
	command := NewCommandSpec("/bin/sh", []string{"-c", probe})
	command.ClearEnv = true
	command.Env["PATH"] = session
	output, err := ProcessExecutor{}.Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 {
		t.Fatalf("%+v", output)
	}
	args, path, _ := strings.Cut(string(output.Stdout), "|")
	if args != "two words" {
		t.Fatalf("args %q", args)
	}
	if runtime.GOOS == "darwin" {
		if !strings.HasPrefix(path, session+":") {
			t.Fatal(path)
		}
		for _, directory := range []string{"/usr/local/bin", "/opt/homebrew/bin"} {
			if !slices.Contains(strings.Split(path, ":"), directory) {
				t.Fatal(path)
			}
		}
	} else if path != session {
		t.Fatalf("%q", path)
	}
}

// Launch finding R3-11: `*.lock` files stayed in the instance's socket
// directory after their masters exited. The daemon clears, when it starts,
// each lock whose master's socket is gone, and leaves a lock that guards a
// live socket or that another process holds.
//
// Hard-won: 761279ef: an exited master left lock files that blocked later opens.
func TestStaleMasterLocksAreRemovedButHeldOnesStay(t *testing.T) {
	skipUnlessUnix(t)
	dir := t.TempDir()
	stale := filepath.Join(dir, "aaaaaaaaaaaaaaaa-0.lock")
	if err := os.WriteFile(stale, nil, 0o666); err != nil {
		t.Fatal(err)
	}
	held := filepath.Join(dir, "cccccccccccccccc-0.lock")
	holder, err := lockMasterOpening(filepath.Join(dir, "cccccccccccccccc-0"))
	if err != nil {
		t.Fatal(err)
	}

	removeStaleMasterLocksIn(dir)
	if fileExists(stale) {
		t.Fatal("a lock whose master is gone is removed")
	}
	if !fileExists(held) {
		t.Fatal("a lock another opener holds stays")
	}

	releaseMasterLock(holder)
	// A process another test forks while the holder is open shares its lock
	// until that child execs, so the sweep may find the lock still held for
	// a few milliseconds; the daemon would try again at its next start.
	for range 200 {
		removeStaleMasterLocksIn(dir)
		if !fileExists(held) {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	if fileExists(held) {
		t.Fatal("a lock nobody holds any more is removed")
	}
}

func fileExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

const (
	borrowParent = "0123456789abcdef0123456789abcdef"
	borrowChild  = "fedcba9876543210fedcba9876543210"
)

func borrowedPodman(t *testing.T, owner string) TargetsTargetLocator {
	t.Helper()
	name, err := ResourceName(owner)
	if err != nil {
		t.Fatal(err)
	}
	return TargetsTargetLocator{Kind: TargetsTargetLocatorLocalPodman, ContainerID: name, BorrowedFrom: &owner}
}

// Hard-won: 5cea6d11: a child inherited its parent locator and was refused before starting.
func TestVerifyLocatorAcceptsAContainerBorrowedFromItsOwner(t *testing.T) {
	locator := borrowedPodman(t, borrowParent)
	if err := VerifyLocator(&locator, borrowChild); err != nil {
		t.Fatalf("a child may borrow its parent's container: %v", err)
	}
}

func TestVerifyLocatorRejectsAContainerBorrowedFromTheCheckingSession(t *testing.T) {
	locator := borrowedPodman(t, borrowParent)
	err := VerifyLocator(&locator, borrowParent)
	if err == nil {
		t.Fatal("a session cannot borrow from itself")
	}
	if !strings.Contains(err.Error(), "cannot be owned by the borrowing session") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestVerifyLocatorRejectsABorrowedContainerNamingAnotherSession(t *testing.T) {
	name, err := ResourceName(borrowChild)
	if err != nil {
		t.Fatal(err)
	}
	owner := borrowParent
	locator := TargetsTargetLocator{Kind: TargetsTargetLocatorLocalPodman, ContainerID: name, BorrowedFrom: &owner}
	err = VerifyLocator(&locator, borrowChild)
	if err == nil {
		t.Fatal("the container must belong to the recorded owner")
	}
	if !strings.Contains(err.Error(), "borrowed container locator") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestWorkerRootOfABorrowedContainerIsTheChildsOwnDirectory(t *testing.T) {
	locator := borrowedPodman(t, borrowParent)
	root, err := WorkerRoot(&locator, borrowChild)
	if err != nil {
		t.Fatal(err)
	}
	if want := "/var/lib/hel/workers/" + borrowChild; root != want {
		t.Fatalf("%q != %q", root, want)
	}
}

// sshRecordingExecutor records the commands it is handed and reports an
// empty success.
type sshRecordingExecutor struct {
	sshExecuteOnly
	mu   sync.Mutex
	seen []CommandSpec
}

func newSSHRecordingExecutor() *sshRecordingExecutor {
	executor := &sshRecordingExecutor{}
	executor.execute = func(command *CommandSpec) (CommandOutput, error) {
		executor.mu.Lock()
		defer executor.mu.Unlock()
		executor.seen = append(executor.seen, *command)
		return CommandOutput{}, nil
	}
	return executor
}

// sshSharingSocketDir is a short directory for control sockets: macOS's
// default temporary path leaves too little room for SSH's hash.
func sshSharingSocketDir(t *testing.T) string {
	t.Helper()
	dir, err := os.MkdirTemp("/tmp", "mj-ssh-")
	if err != nil {
		t.Fatalf("short control socket directory: %v", err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	return dir
}

// sshSpawnedArgs leases a session for command through the process-wide
// ledger with the sockets pinned to dir (nil: sharing disabled), and
// returns the arguments it would be spawned with.
func sshSpawnedArgs(t *testing.T, command *CommandSpec, dir *string, masters CommandExecutor) []string {
	t.Helper()
	setting := SshSharingForTest{Kind: SshSharingForTestDisabled}
	if dir != nil {
		setting = SshSharingForTest{Kind: SshSharingForTestDirectory, Directory: *dir}
	}
	SetSSHConnectionSharingForTest(&setting)
	session, err := command.OpenSSHSession(masters, &SshWalk{})
	SetSSHConnectionSharingForTest(nil)
	if err != nil {
		t.Fatalf("session: %v", err)
	}
	return append([]string{}, session.Command().Args...)
}

func pinSharingDirectory(dir string) {
	SetSSHConnectionSharingForTest(&SshSharingForTest{Kind: SshSharingForTestDirectory, Directory: dir})
}

// A built command carries a session request instead of sharing options; at
// spawn time the session options go in front of everything the user
// configured, so OpenSSH honours them over any proxy setting.
func TestSessionOptionsLeadTheSpawnedCommand(t *testing.T) {
	skipUnlessUnix(t)
	sharingTestLock.Lock()
	defer sharingTestLock.Unlock()
	socketDir := sshSharingSocketDir(t)
	ssh := SshTarget{
		Destination: "session-options-host",
		SSHArgs:     []string{"-p", "2222", "-o", "ProxyCommand=nc %h %p"},
	}
	command := SSHCommand(SshTrafficClassControl, &ssh, []string{"true"})
	remote := RemoteCommandLine([]string{"true"})
	if want := []string{"-p", "2222", "-o", "ProxyCommand=nc %h %p", "session-options-host", remote}; !slices.Equal(command.Args, want) {
		t.Fatalf("stored arguments never contain sharing options: %q", command.Args)
	}
	if command.SSHSession == nil || command.SSHSession.Target.Destination != ssh.Destination ||
		!slices.Equal(command.SSHSession.Target.SSHArgs, ssh.SSHArgs) {
		t.Fatalf("%+v", command.SSHSession)
	}
	if command.SSHDestination == nil || *command.SSHDestination != "session-options-host" {
		t.Fatalf("%v", command.SSHDestination)
	}

	masters := newSSHFakeMasters()
	args := sshSpawnedArgs(t, &command, &socketDir, masters)
	socket := filepath.Join(socketDir, controlSocketName(&ssh, SshTrafficClassControl, 0))
	want := []string{
		"-o", "ControlMaster=no",
		"-o", "ControlPath=" + socket,
		"-o", "ProxyCommand=false",
		"-p", "2222",
		"-o", "ProxyCommand=nc %h %p",
		"session-options-host",
		withStartMarker(remote),
	}
	if !slices.Equal(args, want) {
		t.Fatalf("%q\n!= %q", args, want)
	}
	if masters.openers() != 1 {
		t.Fatalf("openers %d", masters.openers())
	}
	info, err := os.Stat(socketDir)
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode().Perm() != 0o700 {
		t.Fatalf("mode %o", info.Mode().Perm())
	}
}

// A control socket is the value of an `ssh` option, and `ssh` splits an
// unquoted value on whitespace. On macOS the sockets sit below
// `~/Library/Application Support/mjolnir`, so every option line Mjolnir
// wrote was refused with "keyword controlpath extra arguments at end of
// line" and no Mac target answered at all. `ssh -G` resolves options
// without connecting, so this holds the value we emit against ssh's own
// parser. `%` is expanded even inside the quotes, so it is doubled.
func TestAControlPathWithASpaceReachesSshIntact(t *testing.T) {
	skipUnlessUnix(t)
	sharingTestLock.Lock()
	defer sharingTestLock.Unlock()
	socketDir := sshSharingSocketDir(t)
	dir := filepath.Join(socketDir, "Application Support/%h")
	ssh := SshTarget{Destination: "socket-spaces.invalid"}
	pinSharingDirectory(dir)
	var args []string
	PushConnectionReuseArgs(&args, &ssh)
	SetSSHConnectionSharingForTest(nil)

	if _, err := exec.LookPath("ssh"); err != nil {
		return // no ssh client on this machine
	}
	resolved := exec.Command("ssh", append(append([]string{"-F", "/dev/null", "-G"}, args...), ssh.Destination)...)
	var stdout, stderr bytes.Buffer
	resolved.Stdout, resolved.Stderr = &stdout, &stderr
	if err := resolved.Run(); err != nil {
		t.Fatalf("ssh refused the options we built: %s\n%q", stderr.String(), args)
	}
	expected := "controlpath " + filepath.Join(dir, controlSocketName(&ssh, SshTrafficClassControl, 0))
	if !slices.Contains(strings.Split(stdout.String(), "\n"), expected) {
		t.Fatalf("expected %q:\n%s", expected, stdout.String())
	}
}

func stringPointer(value string) *string { return &value }

// Each instance keeps its own sockets, because each daemon counts only its
// own sessions against a master.
func TestControlSocketsLiveInADirectoryPerInstance(t *testing.T) {
	skipUnlessUnix(t)
	runtimeDir := stringPointer("/run/user/1000")
	if got := defaultControlDir(runtimeDir, controlDirIdentity(stringPointer("hel2"), nil)); got != "/run/user/1000/mjolnir/hel2" {
		t.Fatal(got)
	}
	if got := defaultControlDir(runtimeDir, controlDirIdentity(nil, nil)); got != "/run/user/1000/mjolnir/default" {
		t.Fatal(got)
	}
}

// A daemon isolated only by `MJ_DATA_DIR` (every luna lab) is another
// instance, so it must not share the default instance's masters or sweep its
// lock files (launch finding R5-1). Its directory is named by the same
// fingerprint `instance_identity` stamps on its workers.
//
// Hard-won: 5cd9c555: MJ_DATA_DIR-only daemons reused default-instance sockets and swept each other’s locks.
func TestADataDirectoryOverrideGetsItsOwnSocketDirectory(t *testing.T) {
	skipUnlessUnix(t)
	runtimeDir := stringPointer("/run/user/1000")
	lab := "/tmp/lab-a/data"
	otherLab := "/tmp/lab-b/data"
	labDir := defaultControlDir(runtimeDir, controlDirIdentity(nil, &lab))
	if labDir == "/run/user/1000/mjolnir/default" {
		t.Fatal(labDir)
	}
	if want := filepath.Join("/run/user/1000/mjolnir", InstanceIdentityFor("", lab)); labDir != want {
		t.Fatalf("%q != %q", labDir, want)
	}
	if labDir == defaultControlDir(runtimeDir, controlDirIdentity(nil, &otherLab)) {
		t.Fatal("two labs share a directory")
	}
	// A named instance keeps its name whatever its data directory, as
	// `instance_identity` does.
	if got := defaultControlDir(runtimeDir, controlDirIdentity(stringPointer("hel2"), &lab)); got != "/run/user/1000/mjolnir/hel2" {
		t.Fatal(got)
	}
}

// Sockets are named `<hash>-<shard>`, and the hash follows the whole
// configured connection, not only the destination.
func TestSocketNamesIdentifyTheConnectionAndTheShard(t *testing.T) {
	skipUnlessUnix(t)
	plain := SshTarget{Destination: "host"}
	otherPort := SshTarget{Destination: "host", SSHArgs: []string{"-p", "2222"}}
	first := controlSocketName(&plain, SshTrafficClassControl, 0)
	second := controlSocketName(&plain, SshTrafficClassControl, 1)
	if len(first) != connectionHashHex+2 {
		t.Fatal(first)
	}
	if !strings.HasSuffix(first, "-0") || !strings.HasSuffix(second, "-1") {
		t.Fatal(first, second)
	}
	if first[:connectionHashHex] != second[:connectionHashHex] {
		t.Fatal(first, second)
	}
	if first[:connectionHashHex] == controlSocketName(&otherPort, SshTrafficClassControl, 0)[:connectionHashHex] {
		t.Fatal("another port shares the hash")
	}
}

// A command with a two-second keepalive must join a master, never open one:
// as the master it would impose that keepalive on every later session
// sharing the connection.
func TestFailFastCommandsReuseAMasterWithoutBecomingOne(t *testing.T) {
	skipUnlessUnix(t)
	sharingTestLock.Lock()
	defer sharingTestLock.Unlock()
	socketDir := sshSharingSocketDir(t)
	pinSharingDirectory(socketDir)
	ssh := SshTarget{Destination: "host"}
	masters := newSSHFakeMasters()
	validationCommand := SSHValidationCommand(&ssh, []string{"true"}, "test")
	validation := sshSpawnedArgs(t, &validationCommand, &socketDir, masters)
	if masters.commands() != 0 {
		t.Fatal("a probe never checks or opens a master")
	}
	if slices.Contains(validation, "ProxyCommand=false") {
		t.Fatalf("%q", validation)
	}
	pinSharingDirectory(socketDir)
	executor := newSSHRecordingExecutor()
	_, err := SSHCompletions(&ssh, "/srv/pr", CompletionKindDirectories, executor)
	if err != nil {
		t.Fatalf("completion runs: %v", err)
	}
	completion := executor.seen[0].Args
	SetSSHConnectionSharingForTest(nil)

	controlPath := fmt.Sprintf("ControlPath=%s/%s", socketDir, controlSocketName(&ssh, SshTrafficClassControl, 0))
	for _, args := range [][]string{validation, completion} {
		if !slices.Contains(args, "ControlMaster=no") || !slices.Contains(args, controlPath) {
			t.Fatalf("%q", args)
		}
		if slices.ContainsFunc(args, func(arg string) bool { return strings.HasPrefix(arg, "ControlPersist") }) {
			t.Fatalf("a fail-fast command must not set how long a master lingers: %q", args)
		}
		master := slices.Index(args, "ControlMaster=no")
		if !slices.Contains(args, "ServerAliveCountMax=1") {
			t.Fatalf("its own keepalive: %q", args)
		}
		destination := slices.Index(args, "host")
		if destination < 0 || master >= destination {
			t.Fatalf("%q", args)
		}
	}
}

// `mj doctor` diagnoses and exits. Its connectivity probe must join a master
// when one is up and otherwise open a plain connection, so a doctor run never
// leaves a `ControlPersist` master behind, and the probe's own strict
// overrides never bind a shared connection.
func TestConnectivityProbeJoinsAMasterWithoutBecomingOne(t *testing.T) {
	skipUnlessUnix(t)
	sharingTestLock.Lock()
	defer sharingTestLock.Unlock()
	socketDir := sshSharingSocketDir(t)
	pinSharingDirectory(socketDir)
	ssh := SshTarget{Destination: "host"}
	masters := newSSHFakeMasters()
	probe := SSHConnectivityProbe(&ssh)
	args := sshSpawnedArgs(t, &probe, &socketDir, masters)

	if masters.commands() != 0 {
		t.Fatal("a probe never opens a master")
	}
	if !slices.Contains(args, "ControlMaster=no") {
		t.Fatalf("%q", args)
	}
	if !slices.Contains(args, fmt.Sprintf("ControlPath=%s/%s", socketDir, controlSocketName(&ssh, SshTrafficClassControl, 0))) {
		t.Fatalf("the probe must still join an existing master: %q", args)
	}
	if slices.ContainsFunc(args, func(arg string) bool { return strings.HasPrefix(arg, "ControlPersist") }) {
		t.Fatalf("a doctor probe must not set how long a master lingers: %q", args)
	}
	master := slices.Index(args, "ControlMaster=no")
	strict := slices.Index(args, "StrictHostKeyChecking=yes")
	if strict < 0 || master >= strict {
		t.Fatalf("%q", args)
	}
}

func TestAControlPathThatCannotFitASocketAddressIsSkipped(t *testing.T) {
	skipUnlessUnix(t)
	sharingTestLock.Lock()
	defer sharingTestLock.Unlock()
	root := t.TempDir()
	long := filepath.Join(root, strings.Repeat("a", maxControlPath))
	ssh := SshTarget{Destination: "long-path-host"}
	masters := newSSHFakeMasters()
	command := SSHCommand(SshTrafficClassControl, &ssh, []string{"true"})
	args := sshSpawnedArgs(t, &command, &long, masters)
	if want := []string{"long-path-host", withStartMarker(RemoteCommandLine([]string{"true"}))}; !slices.Equal(args, want) {
		t.Fatalf("%q", args)
	}
	if masters.commands() != 0 {
		t.Fatal(masters.commands())
	}
	if fileExists(long) {
		t.Fatal("an unusable directory must not be created")
	}
}

// Against a real host: leasing a session opens a master that `ssh -O check`
// finds, and a command runs through it. Set `MJ_E2E_SSH_HOST` to a reachable
// destination to run it.
func TestALeasedSessionRunsThroughAnOpenedMasterOnARealHost(t *testing.T) {
	skipUnlessUnix(t)
	sharingTestLock.Lock()
	defer sharingTestLock.Unlock()
	host, ok := os.LookupEnv("MJ_E2E_SSH_HOST")
	if !ok {
		return
	}
	socketDir := sshSharingSocketDir(t)
	ssh := SshTarget{Destination: host, SSHArgs: []string{"-o", "BatchMode=yes"}}
	pinSharingDirectory(socketDir)
	command := SSHCommand(SshTrafficClassControl, &ssh, []string{"true"})
	output, outputErr := ProcessExecutor{}.Execute(&command)
	SetSSHConnectionSharingForTest(nil)
	socket := filepath.Join(socketDir, controlSocketName(&ssh, SshTrafficClassControl, 0))
	checkCommand := masterCheckCommand(&ssh, socket)
	check, err := ProcessExecutor{}.Execute(&checkCommand)
	if err != nil {
		t.Fatalf("ssh -O check must run: %v", err)
	}
	_ = exec.Command("ssh", "-O", "exit", "-o", controlPathOption(socket), host).Run()
	if outputErr != nil {
		t.Fatalf("ssh must run: %v", outputErr)
	}
	if output.Status != 0 {
		t.Fatalf("ssh %s true failed: %s", host, output.Stderr)
	}
	if check.Status != 0 {
		t.Fatalf("no master is running: %s", check.Stderr)
	}
}

// sshLocalSshd is a throwaway `sshd` on 127.0.0.1 run by the current user,
// with `MaxSessions` set, so a test meets the refusal a real server gives.
// stop ends the server and exits the masters it carried.
type sshLocalSshd struct {
	child     *exec.Cmd
	ssh       SshTarget
	directory string
	sockets   string
}

// startLocalSshd is nil when this machine has no `sshd` or `ssh-keygen` to
// run.
func startLocalSshd(t *testing.T, maxSessions int, sockets string) *sshLocalSshd {
	t.Helper()
	var sshd string
	for _, path := range []string{"/usr/sbin/sshd", "/usr/local/sbin/sshd"} {
		if fileExists(path) {
			sshd = path
			break
		}
	}
	if sshd == "" {
		return nil
	}
	directory, err := os.MkdirTemp("/tmp", "mj-sshd-")
	if err != nil {
		t.Fatalf("sshd directory: %v", err)
	}
	t.Cleanup(func() { os.RemoveAll(directory) })
	for _, name := range []string{"hostkey", "client"} {
		keygen := exec.Command("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", filepath.Join(directory, name))
		if err := keygen.Run(); err != nil {
			var exit *exec.ExitError
			if errors.As(err, &exit) {
				t.Fatalf("ssh-keygen %s: %v", name, err)
			}
			return nil
		}
	}
	key, err := os.ReadFile(filepath.Join(directory, "client.pub"))
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(directory, "authorized_keys"), key, 0o600); err != nil {
		t.Fatal(err)
	}
	// The kernel hands out a free port; the window before sshd binds it is
	// the usual one for test servers.
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	port := listener.Addr().(*net.TCPAddr).Port
	listener.Close()
	config := filepath.Join(directory, "sshd_config")
	if err := os.WriteFile(config, fmt.Appendf(nil,
		"Port %d\nListenAddress 127.0.0.1\nHostKey %[2]s/hostkey\n"+
			"PidFile %[2]s/sshd.pid\nAuthorizedKeysFile %[2]s/authorized_keys\n"+
			"StrictModes no\nUsePAM no\nPasswordAuthentication no\n"+
			"MaxSessions %[3]d\n", port, directory, maxSessions), 0o600); err != nil {
		t.Fatal(err)
	}
	log, err := os.Create(filepath.Join(directory, "sshd.log"))
	if err != nil {
		t.Fatal(err)
	}
	defer log.Close()
	child := exec.Command(sshd, "-D", "-e", "-f", config)
	child.Stderr = log
	if err := child.Start(); err != nil {
		t.Fatalf("start sshd: %v", err)
	}
	user, err := exec.Command("id", "-un").Output()
	if err != nil {
		t.Fatalf("id -un: %v", err)
	}
	server := &sshLocalSshd{
		child: child,
		ssh: SshTarget{
			Destination: strings.TrimSpace(string(user)) + "@127.0.0.1",
			SSHArgs: []string{
				"-F", "/dev/null",
				"-p", strconv.Itoa(port),
				"-i", filepath.Join(directory, "client"),
				"-o", "IdentitiesOnly=yes",
				"-o", "StrictHostKeyChecking=no",
				"-o", "UserKnownHostsFile=/dev/null",
				"-o", "BatchMode=yes",
			},
		},
		directory: directory,
		sockets:   sockets,
	}
	deadline := time.Now().Add(10 * time.Second)
	for {
		connection, err := net.Dial("tcp", net.JoinHostPort("127.0.0.1", strconv.Itoa(port)))
		if err == nil {
			connection.Close()
			break
		}
		if time.Now().After(deadline) {
			logged, _ := os.ReadFile(filepath.Join(directory, "sshd.log"))
			server.stop()
			t.Fatalf("sshd did not listen: %s", logged)
		}
		time.Sleep(50 * time.Millisecond)
	}
	return server
}

// stop exits the masters, which persist for a minute after their last
// session unless told to exit, and stops the server (Rust: Drop).
func (s *sshLocalSshd) stop() {
	entries, _ := os.ReadDir(s.sockets)
	for _, entry := range entries {
		if entry.Type()&os.ModeSocket != 0 {
			args := append([]string{"-O", "exit", "-o", controlPathOption(filepath.Join(s.sockets, entry.Name()))}, s.ssh.SSHArgs...)
			_ = exec.Command("ssh", append(args, s.ssh.Destination)...).Run()
		}
	}
	_ = s.child.Process.Kill()
	_ = s.child.Wait()
}

// Real ssh and the outer and login shells must all read the discovery line
// as the argv it was built from.
func TestDaemonDiscoveryLoginCommandSurvivesRealSshShellParsing(t *testing.T) {
	skipUnlessUnix(t)
	socketDir := sshSharingSocketDir(t)
	server := startLocalSshd(t, 2, socketDir)
	if server == nil {
		return
	}
	defer server.stop()
	expected := "space ' quote \" double $dollar `backtick` ; separator\nnewline"
	line := DaemonLoginCommand([]string{"printf", "%s", expected})
	command := exec.Command("ssh", append(append([]string{}, server.ssh.SSHArgs...), server.ssh.Destination, line)...)
	output, err := RunWithInput(command, nil)
	if err != nil {
		t.Fatal(err)
	}
	if !output.Status.Success() {
		t.Fatalf("%s", output.Stderr)
	}
	if string(output.Stdout) != expected {
		t.Fatalf("%q", output.Stdout)
	}
	// Exercise each installed outer shell and login shell separately. In
	// particular fish must never see the POSIX parameter expansion.
	for _, shell := range []string{"/bin/bash", "/bin/zsh", "/usr/bin/fish"} {
		if !fileExists(shell) {
			continue
		}
		command := exec.Command(shell, "-c", line)
		command.Env = append(os.Environ(), "SHELL="+shell)
		output, err := RunWithInput(command, nil)
		if err != nil {
			t.Fatal(err)
		}
		if !output.Status.Success() {
			t.Fatalf("%s: %s", shell, output.Stderr)
		}
		if string(output.Stdout) != expected {
			t.Fatalf("%s: %q", shell, output.Stdout)
		}
	}
}

// Hard-won: #1558: bulk bytes queued relay exchanges behind the same TCP send queue.
func TestBulkAndControlUseIndependentRealMastersAndPreserveLegacyCleanup(t *testing.T) {
	skipUnlessUnix(t)
	sharingTestLock.Lock()
	defer sharingTestLock.Unlock()
	sockets := sshSharingSocketDir(t)
	server := startLocalSshd(t, 1, sockets)
	if server == nil {
		t.Log("skipped: this machine has no sshd")
		return
	}
	defer server.stop()
	ledger := newSessionLedger()
	lease := func(class SshTrafficClass) *SshSessionLease {
		lease, err := ledger.lease(&server.ssh, class, sockets, ProcessExecutor{}, &SshWalk{})
		if err != nil {
			t.Fatal(err)
		}
		return lease
	}
	bulk := lease(SshTrafficClassBulk)
	control := lease(SshTrafficClassControl)
	bulkSocket, _ := bulk.ControlPath()
	controlSocket, _ := control.ControlPath()
	if bulkSocket == controlSocket {
		t.Fatal(bulkSocket)
	}
	check := func(socket string) CommandOutput {
		command := masterCheckCommand(&server.ssh, socket)
		output, err := ProcessExecutor{}.Execute(&command)
		if err != nil {
			t.Fatal(err)
		}
		return output
	}
	bulkMaster := check(bulkSocket)
	controlMaster := check(controlSocket)
	if bulkMaster.Status != 0 || controlMaster.Status != 0 {
		t.Fatalf("%+v %+v", bulkMaster, controlMaster)
	}
	if bytes.Equal(bulkMaster.Stderr, controlMaster.Stderr) {
		t.Fatal("different master processes")
	}
	command := SSHCommand(SshTrafficClassBulk, &server.ssh, []string{"cat"})
	raw := NewCommandSpec("ssh", SessionCommandArgs("ssh", command.Args, &server.ssh, bulk))
	payload := bytes.Repeat([]byte("x"), 256*1024+17)
	output, err := ProcessExecutor{}.ExecuteWithStdin(&raw, bytes.NewReader(payload))
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 || !bytes.Equal(output.Stdout, payload) {
		t.Fatalf("status %d, %d bytes", output.Status, len(output.Stdout))
	}

	// A pre-class daemon leaves this same socket grammar, with a hash
	// computed without the class. Live old masters must survive the sweep.
	legacy := filepath.Join(sockets, "0123456789abcdef-7")
	if _, err := ensureMaster(&server.ssh, legacy, ProcessExecutor{}); err != nil {
		t.Fatal(err)
	}
	removeStaleMasterLocksIn(sockets)
	for _, socket := range []string{bulkSocket, controlSocket, legacy} {
		if !fileExists(socket) {
			t.Fatalf("live master %s was swept", socket)
		}
	}
	stop := func(socket string) {
		args := append([]string{"-O", "exit", "-o", controlPathOption(socket)}, server.ssh.SSHArgs...)
		command := NewCommandSpec("ssh", append(args, server.ssh.Destination))
		output, err := ProcessExecutor{}.Execute(&command)
		if err != nil {
			t.Fatal(err)
		}
		if output.Status != 0 {
			t.Fatalf("ssh -O exit: %s", output.Stderr)
		}
	}
	stop(legacy)
	// A crashed old master can leave a socket inode with no listener.
	listener, err := net.ListenUnix("unix", &net.UnixAddr{Name: legacy, Net: "unix"})
	if err != nil {
		t.Fatal(err)
	}
	listener.SetUnlinkOnClose(false)
	listener.Close()
	removeStaleMasterLocksIn(sockets)
	if fileExists(legacy) || fileExists(legacy+".lock") {
		t.Fatal("a dead legacy master is swept")
	}

	// A lost bulk master must fail; neither the control master nor a direct
	// connection may be substituted for its prepared command.
	stop(bulkSocket)
	output, err = ProcessExecutor{}.ExecuteWithStdin(&raw, bytes.NewReader(payload))
	if err != nil {
		t.Fatal(err)
	}
	if output.Status == 0 {
		t.Fatal("a lost bulk master must fail")
	}
	command = SSHCommand(SshTrafficClassControl, &server.ssh, []string{"printf", "control alive"})
	raw = NewCommandSpec("ssh", SessionCommandArgs("ssh", command.Args, &server.ssh, control))
	output, err = ProcessExecutor{}.Execute(&raw)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 || string(output.Stdout) != "control alive" {
		t.Fatalf("%d %q", output.Status, output.Stdout)
	}
}

func globalShardCount(ssh *SshTarget) int {
	connections, unlock := sessionLedgerGlobal().lockConnections()
	defer unlock()
	if connection, ok := connections[connectionKey(ssh, SshTrafficClassControl)]; ok {
		return len(connection.shards)
	}
	return 0
}

// Against a real `sshd` that allows two sessions per connection, with another
// process (a second ledger on the same control sockets: an overlapping
// daemon, `mj doctor`, Tab completion) holding the first master full: more
// concurrent commands than one connection can carry all succeed, because a
// command the server refuses never started and walks to the next master.
// That includes commands that stream more than a pipe's worth of stdin:
// nothing of their input is read until the session has started, so a
// refusal never loses any of it. A guarded session with no master still
// fails instead of connecting on its own.
//
// Hard-won: 75da0c2b0: another process filled a master the ledger counted as free, and every refusal at its "known capacity" spent an attempt until the command failed.
func TestCommandsWalkPastMastersThatAnotherProcessFilledOnARealSshd(t *testing.T) {
	skipUnlessUnix(t)
	sharingTestLock.Lock()
	defer sharingTestLock.Unlock()
	socketDir := sshSharingSocketDir(t)
	server := startLocalSshd(t, 2, socketDir)
	if server == nil {
		t.Log("skipped: this machine has no sshd")
		return
	}
	defer server.stop()
	ssh := server.ssh.Clone()
	pinSharingDirectory(socketDir)
	defer SetSSHConnectionSharingForTest(nil)
	// The other process opens the first master and fills both of its
	// sessions with sleepers that outlast the whole test.
	started := t.TempDir()
	other := newSessionLedger()
	var held []*exec.Cmd
	for index := range 2 {
		lease, err := other.lease(&ssh, SshTrafficClassControl, socketDir, ProcessExecutor{}, &SshWalk{})
		if err != nil {
			t.Fatalf("the other process leases a session: %v", err)
		}
		marker := filepath.Join(started, fmt.Sprintf("held-%d", index))
		command := SSHCommand(SshTrafficClassControl, &ssh, []string{"sh", "-c", `touch "$0"; sleep 20`, marker})
		child := exec.Command("ssh", SessionCommandArgs("ssh", command.Args, &ssh, lease)...)
		if err := child.Start(); err != nil {
			t.Fatalf("spawn a held session: %v", err)
		}
		held = append(held, child)
	}
	defer func() {
		for _, child := range held {
			_ = child.Process.Kill()
			_ = child.Wait()
		}
	}()
	deadline := time.Now().Add(20 * time.Second)
	for !fileExists(filepath.Join(started, "held-0")) || !fileExists(filepath.Join(started, "held-1")) {
		if time.Now().After(deadline) {
			t.Fatal("the held sessions never started")
		}
		time.Sleep(20 * time.Millisecond)
	}

	sleeper := SSHCommand(SshTrafficClassControl, &ssh, []string{"sleep", "3"})
	counter := SSHCommand(SshTrafficClassControl, &ssh, []string{"wc", "-c"})
	payload := bytes.Repeat([]byte("x"), 192*1024)
	type result struct {
		output CommandOutput
		err    error
	}
	results := make([]result, 5)
	var group sync.WaitGroup
	for index := range 5 {
		group.Go(func() {
			if index < 3 {
				results[index].output, results[index].err = ProcessExecutor{}.Execute(&sleeper)
			} else {
				results[index].output, results[index].err = ProcessExecutor{}.ExecuteWithStdin(&counter, bytes.NewReader(payload))
			}
		})
	}
	group.Wait()
	shards := globalShardCount(&ssh)
	ledger := newSessionLedger()
	orphanReservation := Reservation{shard: 99, opening: &OpeningLock{}}
	orphanLease := newSshSessionLease(ledger.slot(connectionKey(&ssh, SshTrafficClassControl), &orphanReservation,
		filepath.Join(socketDir, controlSocketName(&ssh, SshTrafficClassControl, 99))), false)
	orphanCommand := NewCommandSpec("ssh", SessionCommandArgs("ssh", sleeper.Args, &ssh, orphanLease)).
		WithPurpose("run a guarded session with no master")
	orphan, orphanErr := ProcessExecutor{}.Execute(&orphanCommand)

	for index, result := range results {
		if result.err != nil {
			t.Fatalf("ssh must run: %v", result.err)
		}
		if result.output.Status != 0 {
			t.Fatalf("session %d failed: %s", index, result.output.Stderr)
		}
		if index >= 3 {
			if got := strings.TrimSpace(string(result.output.Stdout)); got != strconv.Itoa(len(payload)) {
				t.Fatalf("streamed session %d received its whole input exactly once: %q", index, got)
			}
		} else if len(result.output.Stdout) != 0 {
			t.Fatalf("the start marker is stripped: %q", result.output.Stdout)
		}
	}
	if shards < 2 {
		t.Fatalf("the first master was full, so the commands walked on: %d shards", shards)
	}
	if orphanErr != nil {
		t.Fatalf("ssh must run: %v", orphanErr)
	}
	if orphan.Status != 255 {
		t.Fatalf("a guarded session with no master must not connect: %s", orphan.Stderr)
	}
}

// fastSSHRetries collapses the retry backoff for one test and restores both
// test overrides when it ends (Rust: targets::fake_ssh::FastRetries).
func fastSSHRetries(t *testing.T) {
	t.Helper()
	delay := 5 * time.Millisecond
	SetSSHRetryBackoffForTest(&delay)
	t.Cleanup(func() {
		SetSSHRetryBackoffForTest(nil)
		SetSSHPersistentRefusalForTest(nil)
	})
}

// A server that allows no sessions on a connection (`MaxSessions 0`) refuses
// the session on every master, and the master this command opened has no
// shard after it to walk to. The command retries in place for the
// persistent-refusal bound and then fails, naming the host, the setting and
// the fixes, without having opened a second master.
func TestAServerThatAllowsNoSessionsFailsTheCommandAfterTheBound(t *testing.T) {
	skipUnlessUnix(t)
	sharingTestLock.Lock()
	defer sharingTestLock.Unlock()
	fastSSHRetries(t)
	socketDir := sshSharingSocketDir(t)
	server := startLocalSshd(t, 0, socketDir)
	if server == nil {
		t.Log("skipped: this machine has no sshd")
		return
	}
	defer server.stop()
	ssh := server.ssh.Clone()
	pinSharingDirectory(socketDir)
	bound := 1500 * time.Millisecond
	SetSSHPersistentRefusalForTest(&bound)
	started := time.Now()
	command := SSHCommand(SshTrafficClassControl, &ssh, []string{"true"})
	_, err := ProcessExecutor{}.Execute(&command)
	elapsed := time.Since(started)
	SetSSHConnectionSharingForTest(nil)

	if err == nil {
		t.Fatal("a server without sessions fails")
	}
	message := err.Error()
	for _, part := range []string{ssh.Destination, "MaxSessions 0", ControlMasterEnv} {
		if !strings.Contains(message, part) {
			t.Fatalf("%q lacks %q", message, part)
		}
	}
	if elapsed < bound || elapsed >= 20*time.Second {
		t.Fatal(elapsed)
	}
	if shards := globalShardCount(&ssh); shards != 1 {
		t.Fatalf("the command did not walk to another master: %d shards", shards)
	}
}

// sshJoinsNewMaster runs a command's executor calls through ProcessExecutor,
// and right after the master a command opened is published, joins that
// master with a session of another process (lease_probe, completions and
// PushConnectionReuseArgs do this without the opening locks) and waits until
// it has started.
type sshJoinsNewMaster struct {
	sshExecuteOnly
	ssh     SshTarget
	sockets string
	marker  string
	// holdSeconds is how long the other process's session lasts.
	holdSeconds int
	// addsShard: another command adds a shard to the ledger while the other
	// process's session runs.
	addsShard bool
	t         *testing.T
	holder    *exec.Cmd
	holderEnd chan error
}

func newSSHJoinsNewMaster(t *testing.T, ssh SshTarget, sockets, marker string, holdSeconds int, addsShard bool) *sshJoinsNewMaster {
	executor := &sshJoinsNewMaster{ssh: ssh, sockets: sockets, marker: marker, holdSeconds: holdSeconds, addsShard: addsShard, t: t}
	executor.execute = executor.run
	return executor
}

func (e *sshJoinsNewMaster) run(command *CommandSpec) (CommandOutput, error) {
	output, err := ProcessExecutor{}.Execute(command)
	if err != nil {
		return output, err
	}
	if slices.Contains(command.Args, "ControlMaster=yes") && e.holder == nil {
		ledger := newSessionLedger()
		reservation := Reservation{shard: 0, opening: &OpeningLock{}}
		lease := newSshSessionLease(ledger.slot(connectionKey(&e.ssh, SshTrafficClassControl), &reservation,
			filepath.Join(e.sockets, controlSocketName(&e.ssh, SshTrafficClassControl, 0))), false)
		held := SSHCommand(SshTrafficClassControl, &e.ssh, []string{"sh", "-c", fmt.Sprintf(`touch "$0"; sleep %d`, e.holdSeconds), e.marker})
		e.holder = exec.Command("ssh", SessionCommandArgs("ssh", held.Args, &e.ssh, lease)...)
		if err := e.holder.Start(); err != nil {
			e.t.Fatalf("spawn the other process's session: %v", err)
		}
		e.holderEnd = make(chan error, 1)
		go func(holder *exec.Cmd, end chan<- error) { end <- holder.Wait() }(e.holder, e.holderEnd)
		deadline := time.Now().Add(20 * time.Second)
		for !fileExists(e.marker) {
			if time.Now().After(deadline) {
				e.t.Fatal("the other session never started")
			}
			time.Sleep(20 * time.Millisecond)
		}
		if e.addsShard {
			connections, unlock := sessionLedgerGlobal().lockConnections()
			key := connectionKey(&e.ssh, SshTrafficClassControl)
			connection, ok := connections[key]
			if !ok {
				connection = &Connection{}
				connections[key] = connection
			}
			connection.shards = append(connection.shards, newShard())
			unlock()
		}
	}
	return output, nil
}

// With `MaxSessions 1`, another process can join a master the moment it is
// published and take its only session. The command that opened the master
// is then refused for a reason that says nothing about the server's limit:
// it waits in place and succeeds once the other session ends, and the master
// is not closed under the other session.
//
// Hard-won: dcc760d7c: the first-session check on a new master misread another process's session as MaxSessions 0, failed the lease and closed the master under that session.
func TestAnotherProcessTakingTheOnlySessionOfANewMasterIsNotMaxsessionsZero(t *testing.T) {
	skipUnlessUnix(t)
	sharingTestLock.Lock()
	defer sharingTestLock.Unlock()
	fastSSHRetries(t)
	socketDir := sshSharingSocketDir(t)
	server := startLocalSshd(t, 1, socketDir)
	if server == nil {
		t.Log("skipped: this machine has no sshd")
		return
	}
	defer server.stop()
	ssh := server.ssh.Clone()
	pinSharingDirectory(socketDir)
	executor := newSSHJoinsNewMaster(t, ssh.Clone(), socketDir, filepath.Join(t.TempDir(), "held"), 2, false)
	command := SSHCommand(SshTrafficClassControl, &ssh, []string{"true"})
	started := time.Now()
	output, err := withSSHRefusalRetry(&command, executor, nil, func(command *CommandSpec, gate *StartGate) (CommandOutput, error) {
		return ProcessExecutor{}.runOnce(command, gate)
	})
	SetSSHConnectionSharingForTest(nil)
	if executor.holder == nil {
		t.Fatal("the other session ran")
	}
	holderErr := <-executor.holderEnd

	waited := time.Since(started)
	if err != nil {
		t.Fatalf("the command is not misreported as MaxSessions 0: %v", err)
	}
	if output.Status != 0 {
		t.Fatalf("%s", output.Stderr)
	}
	if waited < 500*time.Millisecond {
		t.Fatalf("the command was refused and waited for the other session: %v", waited)
	}
	if holderErr != nil {
		t.Fatalf("the master was closed under the other process's session: %v", holderErr)
	}
}

// With `MaxSessions 1`, a long-lived session of another process (a relay)
// takes the only session of the master a command just opened. Another shard
// exists by then, opened by some other command. The refused command walks to
// it at once and runs, instead of waiting in place for the persistent-refusal
// bound on the master it opened.
//
// Hard-won: ca6580092: the command that opened a master was pinned to it, so a long-lived session on that master made it fail after 60 s while other shards had room.
func TestACommandThatOpenedAFullMasterWalksToAShardAnotherCommandAdded(t *testing.T) {
	skipUnlessUnix(t)
	sharingTestLock.Lock()
	defer sharingTestLock.Unlock()
	socketDir := sshSharingSocketDir(t)
	server := startLocalSshd(t, 1, socketDir)
	if server == nil {
		t.Log("skipped: this machine has no sshd")
		return
	}
	defer server.stop()
	ssh := server.ssh.Clone()
	pinSharingDirectory(socketDir)
	executor := newSSHJoinsNewMaster(t, ssh.Clone(), socketDir, filepath.Join(t.TempDir(), "held"), 30, true)
	command := SSHCommand(SshTrafficClassControl, &ssh, []string{"true"})
	started := time.Now()
	output, err := withSSHRefusalRetry(&command, executor, nil, func(command *CommandSpec, gate *StartGate) (CommandOutput, error) {
		return ProcessExecutor{}.runOnce(command, gate)
	})
	waited := time.Since(started)
	SetSSHConnectionSharingForTest(nil)
	if executor.holder == nil {
		t.Fatal("the other session ran")
	}
	stillHeld := true
	select {
	case <-executor.holderEnd:
		stillHeld = false
	default:
	}
	_ = executor.holder.Process.Kill()
	if stillHeld {
		<-executor.holderEnd
	}
	shards := globalShardCount(&ssh)

	if err != nil {
		t.Fatalf("the command ran on the other shard: %v", err)
	}
	if output.Status != 0 {
		t.Fatalf("%s", output.Stderr)
	}
	if !stillHeld {
		t.Fatal("the other session outlasted the command")
	}
	if waited >= 15*time.Second {
		t.Fatalf("the command waited instead of walking on: %v", waited)
	}
	if shards != 2 {
		t.Fatalf("the command used the shard that was added: %d shards", shards)
	}
}

// sshFakeMasters is a hand-written stand-in for `ssh` that models masters:
// `-O check` succeeds only for a socket whose master it opened, and an opener
// starts one unless told to refuse. It records every command.
type sshFakeMasters struct {
	sshExecuteOnly
	mu                  sync.Mutex
	running             map[string]bool
	refuseOpen          *string
	socketExistedAtOpen []bool
	seen                []CommandSpec
}

func newSSHFakeMasters() *sshFakeMasters {
	masters := &sshFakeMasters{running: map[string]bool{}}
	masters.execute = masters.run
	return masters
}

func sshFakeSocket(command *CommandSpec) string {
	for _, arg := range command.Args {
		if socket, ok := strings.CutPrefix(arg, "ControlPath="); ok {
			return socket
		}
	}
	panic("every master command names its socket")
}

func sshHasPair(args []string, first, second string) bool {
	for index := 0; index+1 < len(args); index++ {
		if args[index] == first && args[index+1] == second {
			return true
		}
	}
	return false
}

func (m *sshFakeMasters) kill(socket string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	delete(m.running, socket)
}

func (m *sshFakeMasters) commands() int {
	m.mu.Lock()
	defer m.mu.Unlock()
	return len(m.seen)
}

func (m *sshFakeMasters) openers() int {
	m.mu.Lock()
	defer m.mu.Unlock()
	count := 0
	for _, command := range m.seen {
		if slices.Contains(command.Args, "ControlMaster=yes") {
			count++
		}
	}
	return count
}

func (m *sshFakeMasters) run(command *CommandSpec) (CommandOutput, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.seen = append(m.seen, *command)
	if command.Program != "ssh" {
		panic("the ledger ran " + command.Program)
	}
	socket := sshFakeSocket(command)
	var status int
	var stderr string
	switch {
	case sshHasPair(command.Args, "-O", "check"):
		if !m.running[socket] {
			status, stderr = 255, "Control socket connect: No such file or directory"
		}
	case sshHasPair(command.Args, "-O", "exit"):
		delete(m.running, socket)
	case slices.Contains(command.Args, "ControlMaster=yes"):
		m.socketExistedAtOpen = append(m.socketExistedAtOpen, fileExists(socket))
		if m.refuseOpen != nil {
			status, stderr = 255, *m.refuseOpen
		} else {
			m.running[socket] = true
		}
	default:
		panic(fmt.Sprintf("the ledger ran an unexpected command: %+v", command))
	}
	return CommandOutput{Status: status, Stderr: []byte(stderr)}, nil
}

func sshShardOf(t *testing.T, lease *SshSessionLease) string {
	t.Helper()
	path, ok := lease.ControlPath()
	if !ok {
		t.Fatal("a shared lease has a socket")
	}
	name := filepath.Base(path)
	return name[strings.LastIndexByte(name, '-')+1:]
}

// The marker can arrive in any split of reads, and a stream that does not
// begin with it passes through whole: the first byte of anything proves the
// session started.
func TestTheStartMarkerIsStrippedHoweverItIsSplitAcrossReads(t *testing.T) {
	stream := []byte(SSHStartMarker + "payload\nmore")
	for split := 1; split < len(stream); split++ {
		scan := NewStartScan()
		var out []byte
		head, tail := stream[:split], stream[split:]
		if !scan.Push(head, &out) {
			t.Fatal("the first byte starts the session")
		}
		if scan.Push(tail, &out) {
			t.Fatal("only the first byte starts the session")
		}
		scan.Finish(&out)
		if string(out) != "payload\nmore" {
			t.Fatalf("split at %d: %q", split, out)
		}
	}
	// One byte at a time, as a slow pipe delivers it.
	scan := NewStartScan()
	var out []byte
	for _, character := range stream {
		scan.Push([]byte{character}, &out)
	}
	if string(out) != "payload\nmore" {
		t.Fatalf("%q", out)
	}

	// A stream that diverges from the marker, and one that ends inside it.
	scan = NewStartScan()
	out = nil
	if !scan.Push([]byte("MJOLNIR-x"), &out) || string(out) != "MJOLNIR-x" {
		t.Fatalf("%q", out)
	}
	scan = NewStartScan()
	out = nil
	if !scan.Push([]byte("MJOLN"), &out) {
		t.Fatal("the first byte starts the session")
	}
	if len(out) != 0 {
		t.Fatal("held until the next chunk decides")
	}
	scan.Finish(&out)
	if string(out) != "MJOLN" {
		t.Fatalf("%q", out)
	}
	scan = NewStartScan()
	var empty []byte
	if scan.Push(nil, &empty) {
		t.Fatal("no byte, no start")
	}
}

// `scp` spells the port `-P`; passing an `ssh` `-p` through would ask it to
// preserve file times and read the port as a file name. Every `scp` also
// opens a connection, so it is retried like `ssh`.
//
// Hard-won: a1b93a2e: controller uploads used SSH -p where scp requires -P.
func TestScpTranslatesTheSshPortOptionAndIsTaggedWithItsDestination(t *testing.T) {
	skipUnlessUnix(t)
	sharingTestLock.Lock()
	defer sharingTestLock.Unlock()
	SetSSHConnectionSharingForTest(&SshSharingForTest{Kind: SshSharingForTestDisabled})
	ssh := SshTarget{Destination: "build@10.0.0.1", SSHArgs: []string{"-p", "2222"}}

	upload := SCPUpload(&ssh, "/tmp/local", "remote/path", true)
	download := SCPDownload(&ssh, "remote/archive.zip", "/tmp/local.zip")
	SetSSHConnectionSharingForTest(nil)

	if want := []string{"-P", "2222", "-r", "/tmp/local", "build@10.0.0.1:remote/path"}; !slices.Equal(upload.Args, want) {
		t.Fatalf("%q", upload.Args)
	}
	if want := []string{"-P", "2222", "build@10.0.0.1:remote/archive.zip", "/tmp/local.zip"}; !slices.Equal(download.Args, want) {
		t.Fatalf("%q", download.Args)
	}
	for _, command := range []CommandSpec{upload, download} {
		if command.Program != "scp" {
			t.Fatal(command.Program)
		}
		if command.SSHDestination == nil || *command.SSHDestination != "build@10.0.0.1" {
			t.Fatalf("%v", command.SSHDestination)
		}
	}
}

// sshNextLease takes the next lease of walk on ledger.
func sshNextLease(t *testing.T, ledger *SessionLedger, ssh *SshTarget, dir string, masters CommandExecutor, walk *SshWalk) *SshSessionLease {
	t.Helper()
	lease, err := ledger.lease(ssh, SshTrafficClassControl, dir, masters, walk)
	if err != nil {
		t.Fatalf("lease: %v", err)
	}
	return lease
}

// A command's walk tries each shard once, in order from where the last
// started session was, wrapping around, and opens a new shard only after it
// has tried every one. A command that has opened a master does not open
// another: with every shard tried it goes back to the one it tried least
// recently. A stale hint (here, a shard that no longer exists in the order
// the walk expects) changes the order and never fails a lease. A probe walks
// the same way and opens no master.
func TestAWalkTriesEveryShardOnceBeforeItOpensANewOne(t *testing.T) {
	skipUnlessUnix(t)
	dir := sshSharingSocketDir(t)
	ledger := newSessionLedger()
	ssh := sshPlainTarget("walk-host")
	masters := newSSHFakeMasters()

	// Three commands, each refused on every shard that exists, each opening
	// the next shard.
	var leases []*SshSessionLease
	for shards := 1; shards <= 3; shards++ {
		walk := &SshWalk{}
		var last *SshSessionLease
		for range shards {
			last = sshNextLease(t, ledger, &ssh, dir, masters, walk)
		}
		leases = append(leases, last)
	}
	var names []string
	for _, lease := range leases {
		names = append(names, sshShardOf(t, lease))
	}
	if !slices.Equal(names, []string{"0", "1", "2"}) {
		t.Fatalf("each refusal opens the next shard: %q", names)
	}
	if masters.openers() != 3 {
		t.Fatal(masters.openers())
	}

	// A session started on shard 2: the next walk starts there, wraps through
	// 0 and 1, and only then opens shard 3. Having opened it, it goes back to
	// shard 2, tried least recently, and then to 0.
	leases[2].Gate().MarkStarted()
	walkShards := func(walk *SshWalk, count int) []string {
		var walked []string
		for range count {
			walked = append(walked, sshShardOf(t, sshNextLease(t, ledger, &ssh, dir, masters, walk)))
		}
		return walked
	}
	if walked := walkShards(&SshWalk{}, 6); !slices.Equal(walked, []string{"2", "0", "1", "3", "2", "0"}) {
		t.Fatalf("%q", walked)
	}

	// A command that opened no master tries all four shards and opens a
	// fifth.
	if walked := walkShards(&SshWalk{}, 5); !slices.Equal(walked, []string{"2", "3", "0", "1", "4"}) {
		t.Fatalf("%q", walked)
	}

	// A probe never opens a master, even past the last shard.
	openers := masters.openers()
	probing := &SshWalk{}
	var probes []string
	for range 7 {
		probes = append(probes, sshShardOf(t, ledger.leaseProbe(&ssh, dir, probing)))
	}
	if !slices.Equal(probes, []string{"2", "3", "4", "0", "1", "5", "6"}) {
		t.Fatalf("%q", probes)
	}
	if masters.openers() != openers {
		t.Fatal(masters.openers())
	}
}

// Commands that are refused at the same moment share the shard one of them
// opened instead of each opening its own.
func TestWalksPastTheLastShardShareTheShardThatWasJustAdded(t *testing.T) {
	skipUnlessUnix(t)
	dir := sshSharingSocketDir(t)
	ledger := newSessionLedger()
	ssh := sshPlainTarget("shared-walk-host")
	masters := newSSHFakeMasters()
	// Another command opened shard 0; these three joined it.
	sshNextLease(t, ledger, &ssh, dir, masters, &SshWalk{})
	walks := []*SshWalk{{}, {}, {}}
	for _, walk := range walks {
		sshNextLease(t, ledger, &ssh, dir, masters, walk)
	}
	// All three were refused on shard 0 and now ask for the next one.
	var next []string
	for _, walk := range walks {
		next = append(next, sshShardOf(t, sshNextLease(t, ledger, &ssh, dir, masters, walk)))
	}
	if !slices.Equal(next, []string{"1", "1", "1"}) {
		t.Fatalf("%q", next)
	}
	if masters.openers() != 2 {
		t.Fatalf("one master for the shard they share: %d", masters.openers())
	}
}

// A refused session on a shared master walks on at once when a shard is left
// for the command, whoever opened it: one it has not tried (looked up when
// the refusal happens, so a shard added meanwhile counts), or a new one when
// it has opened no master itself. A command that opened a master and has
// tried every shard has nowhere to go: the retry waits. So does a refusal
// with no shared master (user-configured sharing), or the zero-delay retry
// would never end, and a connection dropped before authentication.
//
// Hard-won: ca6580092: a command was pinned to the master it opened, so a full master made it retry in place while other shards had room.
func TestARefusedSessionWalksOnOnlyWhileAShardIsLeftForTheCommand(t *testing.T) {
	skipUnlessUnix(t)
	dir := sshSharingSocketDir(t)
	ledger := newSessionLedger()
	ssh := sshPlainTarget("refused-host")
	masters := newSSHFakeMasters()
	opening := &SshWalk{}
	opener := sshNextLease(t, ledger, &ssh, dir, masters, opening)
	if !opener.NoteRefusal(SshRefusalSessionLimit) {
		t.Fatal("a refused session on a shared master may walk on")
	}
	if opening.canWalkOn() {
		t.Fatal("it opened the only shard and has tried it")
	}

	// Another command joins shard 0 and is refused there too: it opens shard
	// 1, which is now left for the opener.
	joining := &SshWalk{}
	shared := sshNextLease(t, ledger, &ssh, dir, masters, joining)
	if !joining.canWalkOn() {
		t.Fatal("a command that opened nothing opens one")
	}
	second := sshNextLease(t, ledger, &ssh, dir, masters, joining)
	if shard := sshShardOf(t, second); shard != "1" {
		t.Fatal(shard)
	}
	if !opening.canWalkOn() {
		t.Fatal("a shard the opener has not tried exists")
	}
	next := sshNextLease(t, ledger, &ssh, dir, masters, opening)
	if shard := sshShardOf(t, next); shard != "1" {
		t.Fatalf("whoever opened it: %s", shard)
	}
	if opening.canWalkOn() {
		t.Fatal("every shard tried")
	}

	if shared.NoteRefusal(SshRefusalBeforeAuthentication) {
		t.Fatal("a pre-authentication hangup waits")
	}
	if sshSessionLeaseUnshared().NoteRefusal(SshRefusalSessionLimit) {
		t.Fatal("a refusal with no shared master waits")
	}
}

// Target validation and the connectivity probe walk the shards like any
// command, but open no master, and carry no `ProxyCommand=false`: where no
// master exists they connect directly, as probes always did.
func TestProbesJoinAShardWithoutOpeningIt(t *testing.T) {
	skipUnlessUnix(t)
	dir := sshSharingSocketDir(t)
	ledger := newSessionLedger()
	ssh := sshPlainTarget("probe-host")
	masters := newSSHFakeMasters()

	session := sshNextLease(t, ledger, &ssh, dir, masters, &SshWalk{})
	probe := ledger.leaseProbe(&ssh, dir, &SshWalk{})
	if sshShardOf(t, session) != "0" || sshShardOf(t, probe) != "0" {
		t.Fatal(sshShardOf(t, session), sshShardOf(t, probe))
	}
	if masters.openers() != 1 {
		t.Fatal("a probe never opens a master")
	}

	var args []string
	PushSessionArgs(&args, probe)
	if slices.Contains(args, "ProxyCommand=false") {
		t.Fatalf("a probe may connect directly when its master is down: %q", args)
	}
}

func TestSeparateConnectionsAreCountedSeparately(t *testing.T) {
	skipUnlessUnix(t)
	dir := sshSharingSocketDir(t)
	ledger := newSessionLedger()
	masters := newSSHFakeMasters()
	one, two := sshPlainTarget("one"), sshPlainTarget("two")
	first := sshNextLease(t, ledger, &one, dir, masters, &SshWalk{})
	second := sshNextLease(t, ledger, &two, dir, masters, &SshWalk{})
	if sshShardOf(t, first) != "0" || sshShardOf(t, second) != "0" {
		t.Fatal(sshShardOf(t, first), sshShardOf(t, second))
	}
	firstPath, _ := first.ControlPath()
	secondPath, _ := second.ControlPath()
	if firstPath == secondPath {
		t.Fatal(firstPath)
	}
}

func TestAnInvalidatedLeaseMakesTheNextLeaseReopenADeadMaster(t *testing.T) {
	skipUnlessUnix(t)
	dir := sshSharingSocketDir(t)
	ledger := newSessionLedger()
	ssh := sshPlainTarget("host")
	masters := newSSHFakeMasters()
	first := sshNextLease(t, ledger, &ssh, dir, masters, &SshWalk{})
	firstPath, _ := first.ControlPath()
	masters.kill(firstPath)

	// Without a failure report the recent check is still trusted.
	sshNextLease(t, ledger, &ssh, dir, masters, &SshWalk{})
	if masters.openers() != 1 {
		t.Fatal(masters.openers())
	}

	first.Invalidate()
	second := sshNextLease(t, ledger, &ssh, dir, masters, &SshWalk{})
	if masters.openers() != 2 {
		t.Fatal(masters.openers())
	}
	if secondPath, _ := second.ControlPath(); secondPath != firstPath {
		t.Fatal(firstPath, secondPath)
	}
}

func TestAMasterThatCannotBeOpenedIsAnErrorNamingTheDestination(t *testing.T) {
	skipUnlessUnix(t)
	dir := sshSharingSocketDir(t)
	ledger := newSessionLedger()
	ssh := sshPlainTarget("build@10.0.0.1")
	masters := newSSHFakeMasters()
	masters.refuseOpen = stringPointer("Permission denied (publickey).")

	_, err := ledger.lease(&ssh, SshTrafficClassControl, dir, masters, &SshWalk{})
	if err == nil {
		t.Fatal("no master means no session")
	}
	message := err.Error()
	if !strings.Contains(message, "build@10.0.0.1") || !strings.Contains(message, "Permission denied") {
		t.Fatal(message)
	}
	if masters.openers() != 1 {
		t.Fatal("the opener is not retried by the ledger")
	}

	masters.refuseOpen = nil
	lease := sshNextLease(t, ledger, &ssh, dir, masters, &SshWalk{})
	if shard := sshShardOf(t, lease); shard != "0" {
		t.Fatal(shard)
	}
}

// sshRacingMasters is a stand-in for `ssh` shared by two goroutines that play
// two daemon processes (the old and new daemon during a restart). An opener
// takes a while to authenticate; if the socket is bound when it finishes,
// real `ssh` prints "already exists, disabling multiplexing" and keeps a
// plain background connection that nothing will ever close.
type sshRacingMasters struct {
	sshExecuteOnly
	mu      sync.Mutex
	bound   map[string]bool
	masters atomic.Int64
	orphans atomic.Int64
}

func newSSHRacingMasters() *sshRacingMasters {
	racing := &sshRacingMasters{bound: map[string]bool{}}
	racing.execute = racing.run
	return racing
}

func (m *sshRacingMasters) run(command *CommandSpec) (CommandOutput, error) {
	socket := sshFakeSocket(command)
	status := 0
	switch {
	case sshHasPair(command.Args, "-O", "check"):
		m.mu.Lock()
		if !m.bound[socket] {
			status = 255
		}
		m.mu.Unlock()
	case !slices.Contains(command.Args, "ControlMaster=yes"):
		// The first session on the master.
	default:
		time.Sleep(100 * time.Millisecond)
		m.mu.Lock()
		if m.bound[socket] {
			m.orphans.Add(1)
		} else {
			m.bound[socket] = true
			m.masters.Add(1)
		}
		m.mu.Unlock()
	}
	return CommandOutput{Status: status}, nil
}

// Two daemon processes that lease on the same instance's sockets at once
// (J-18) must open one master between them, not one master and one orphaned
// plain connection.
//
// Hard-won: 07a04c5d: two daemons could leave a second orphan SSH connection for one socket.
func TestTwoProcessesOpeningOneSocketOpenOneMaster(t *testing.T) {
	skipUnlessUnix(t)
	dir := sshSharingSocketDir(t)
	ssh := sshPlainTarget("racing-host")
	fake := newSSHRacingMasters()
	var group sync.WaitGroup
	errs := make([]error, 2)
	for index := range 2 {
		group.Go(func() {
			// Each process has its own ledger.
			_, errs[index] = newSessionLedger().lease(&ssh, SshTrafficClassControl, dir, fake, &SshWalk{})
		})
	}
	group.Wait()
	for _, err := range errs {
		if err != nil {
			t.Fatalf("lease: %v", err)
		}
	}
	if fake.masters.Load() != 1 || fake.orphans.Load() != 0 {
		t.Fatalf("masters %d, orphans %d", fake.masters.Load(), fake.orphans.Load())
	}
}

func TestAStaleSocketIsRemovedBeforeTheMasterIsOpened(t *testing.T) {
	skipUnlessUnix(t)
	dir := sshSharingSocketDir(t)
	ledger := newSessionLedger()
	ssh := sshPlainTarget("host")
	socket := filepath.Join(dir, controlSocketName(&ssh, SshTrafficClassControl, 0))
	if err := os.WriteFile(socket, nil, 0o666); err != nil {
		t.Fatalf("stale socket stand-in: %v", err)
	}
	masters := newSSHFakeMasters()

	if _, err := ledger.lease(&ssh, SshTrafficClassControl, dir, masters, &SshWalk{}); err != nil {
		t.Fatalf("opens: %v", err)
	}

	if !slices.Equal(masters.socketExistedAtOpen, []bool{false}) {
		t.Fatal(masters.socketExistedAtOpen)
	}
}

func TestAMasterOpenTimesOutDuringHandshakeAndHonorsTheUsersShorterBudget(t *testing.T) {
	skipUnlessUnix(t)
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	port := listener.Addr().(*net.TCPAddr).Port
	finish := make(chan struct{})
	serverDone := make(chan struct{})
	go func() {
		defer close(serverDone)
		accepted := make(chan net.Conn, 1)
		go func() {
			connection, err := listener.Accept()
			if err == nil {
				accepted <- connection
			}
		}()
		select {
		case connection := <-accepted:
			// Accept TCP but never send an SSH banner. ConnectTimeout must
			// cover the handshake, not just the TCP connect.
			select {
			case <-finish:
			case <-time.After(5 * time.Second):
			}
			connection.Close()
		case <-finish:
		case <-time.After(5 * time.Second):
		}
		listener.Close()
	}()
	directory := t.TempDir()
	ssh := SshTarget{
		Destination: "127.0.0.1",
		SSHArgs:     []string{"-F", "/dev/null", "-p", strconv.Itoa(port), "-o", "ConnectTimeout=1"},
	}
	started := time.Now()
	// Exercise one open: the retry loop around it separately retries
	// pre-authentication timeouts with backoff.
	command := masterOpenCommand(&ssh, filepath.Join(directory, "master"))
	output, err := CancellableProcessExecutorWithTimeout(3*time.Second).runOnce(&command, nil)
	close(finish)
	<-serverDone
	if err != nil {
		t.Fatalf("SSH's handshake timeout must beat the executor deadline: %v", err)
	}
	if output.Status != 255 {
		t.Fatal(output.Status)
	}
	if !strings.Contains(string(output.Stderr), "timed out") {
		t.Fatalf("%s", output.Stderr)
	}
	if time.Since(started) >= 3*time.Second {
		t.Fatal(time.Since(started))
	}
}

func TestSessionArgsForbidADirectConnection(t *testing.T) {
	skipUnlessUnix(t)
	dir := sshSharingSocketDir(t)
	ledger := newSessionLedger()
	masters := newSSHFakeMasters()
	ssh := sshPlainTarget("host")
	lease := sshNextLease(t, ledger, &ssh, dir, masters, &SshWalk{})
	var args []string
	PushSessionArgs(&args, lease)
	socket, _ := lease.ControlPath()
	if want := []string{"-o", "ControlMaster=no", "-o", "ControlPath=" + socket, "-o", "ProxyCommand=false"}; !slices.Equal(args, want) {
		t.Fatalf("%q", args)
	}
}

// A session refused on a live shared connection is told apart from a
// connection dropped before authentication, even though a session bound with
// `ProxyCommand=false` prints a closed connection after the refusal.
//
// Hard-won: 79fcdde4: an authenticated session refusal was mislabeled as a pre-authentication hangup.
func TestARefusedSessionIsNamedApartFromAPreAuthenticationHangup(t *testing.T) {
	refusal, ok := SSHRefusal(255, "mux_client_request_session: session request failed: Session open refused by peer\n"+
		"kex_exchange_identification: Connection closed by remote host\n"+
		"Connection closed by UNKNOWN port 65535")
	if !ok || refusal != SshRefusalSessionLimit {
		t.Fatal(refusal, ok)
	}
	refusal, ok = SSHRefusal(255, "Connection closed by 192.168.1.77 port 22")
	if !ok || refusal != SshRefusalBeforeAuthentication {
		t.Fatal(refusal, ok)
	}
	if _, ok := SSHRefusal(1, "Session open refused by peer"); ok {
		t.Fatal("not a transport status")
	}
	if strings.Contains(SshRefusalSessionLimit.retryMessage(), "before authentication") {
		t.Fatal(SshRefusalSessionLimit.retryMessage())
	}
}

// Hard-won: 75da0c2b0: a reset after the remote command started was read as a pre-authentication refusal and the command ran again
func TestTransportRejectionMatchesOnlySshdHangups(t *testing.T) {
	cases := []struct {
		status   int
		stderr   string
		expected bool
	}{
		{255, "Connection closed by 192.168.1.77 port 22", true},
		{255, "kex_exchange_identification: read: Connection reset by peer", true},
		{255, "ssh: Connection reset by 10.0.0.1 port 22", true},
		{255, "Connection timed out during banner exchange", true},
		{255, "Permission denied (publickey).", false},
		{255, "ssh: connect to host h port 22: Connection refused", false},
		{1, "Connection closed by 192.168.1.77 port 22", false},
		// OpenSSH before 8.4 spells the banner exchange differently.
		{255, "ssh_exchange_identification: Connection closed by remote host", true},
		{255, "ssh_exchange_identification: read: Connection reset by peer", true},
		// The same events once the session runs: the command may have started.
		{255, "Connection to h closed by remote host.", false},
		{255, "Read from remote host h: Connection reset by peer", false},
		{255, "client_loop: send disconnect: Connection reset by peer", false},
	}
	for _, c := range cases {
		if got := IsTransportRejection(c.status, c.stderr); got != c.expected {
			t.Fatalf("status %d stderr %q: %v", c.status, c.stderr, got)
		}
	}
}

func TestMasterFileAndThreadAdmissionHonorTheExecutorDeadline(t *testing.T) {
	skipUnlessUnix(t)
	dir := sshSharingSocketDir(t)
	socket := filepath.Join(dir, "held-master")
	held, err := lockMasterOpening(socket)
	if err != nil {
		t.Fatal(err)
	}
	defer releaseMasterLock(held)
	executor := CancellableProcessExecutorWithTimeout(50 * time.Millisecond)
	if _, err := lockMasterOpeningUnless(socket, executor); err == nil {
		t.Fatal("a cancelled wait for the file lock fails")
	}

	ledger := newSessionLedger()
	ssh := sshPlainTarget("cancelled-master-host")
	reservation := ledger.reserve(connectionKey(&ssh, SshTrafficClassControl), &SshWalk{})
	opening := reservation.opening.tryLock()
	if opening == nil {
		t.Fatal("the opening lock is free")
	}
	defer opening.release()
	executor = CancellableProcessExecutorWithTimeout(50 * time.Millisecond)
	if _, err := ledger.lease(&ssh, SshTrafficClassControl, dir, executor, &SshWalk{}); err == nil {
		t.Fatal("a cancelled wait for the opening lock fails")
	}
}

// sshNoMaster fails the test if a lease runs any command, such as opening a
// master.
func newSSHNoMaster() *sshExecuteOnly {
	return &sshExecuteOnly{execute: func(command *CommandSpec) (CommandOutput, error) {
		panic(fmt.Sprintf("an unshared lease ran %s %q", command.Program, command.Args))
	}}
}

// Every `ssh` and `scp` on a native Windows controller, and on Unix with
// `MJ_SSH_CONTROL_MASTER=off`, takes an unshared lease: it opens no master
// and the command spawns exactly as built, on its own connection.
func TestUnsharedLeasesOpenNoMasterAndSpawnTheBuiltCommand(t *testing.T) {
	sharingTestLock.Lock()
	defer sharingTestLock.Unlock()
	SetSSHConnectionSharingForTest(&SshSharingForTest{Kind: SshSharingForTestDisabled})
	defer SetSSHConnectionSharingForTest(nil)
	ssh := SshTarget{Destination: "dev@203.0.113.7", SSHArgs: []string{"-p", "2238"}}
	commands := []CommandSpec{
		SSHCommand(SshTrafficClassControl, &ssh, []string{"git", "status"}),
		SSHValidationCommand(&ssh, []string{"true"}, "validate"),
		SSHConnectivityProbe(&ssh),
		SCPUpload(&ssh, "archive.zip", "remote/archive.zip", false),
		SCPDownload(&ssh, "remote/archive.zip", "archive.zip"),
	}
	for index := range commands {
		command := &commands[index]
		session, err := command.OpenSSHSession(newSSHNoMaster(), &SshWalk{})
		if err != nil {
			t.Fatal(err)
		}
		lease := session.Lease()
		if lease == nil {
			t.Fatal("an SSH command takes a lease")
		}
		if _, ok := lease.ControlPath(); ok {
			t.Fatal("an unshared lease has no socket")
		}
		lease.Invalidate()
		// Only the remote line of an `ssh` command changes: it starts by
		// printing the start marker. `scp` has no remote line.
		expected := append([]string{}, command.Args...)
		if command.Program == "ssh" {
			expected[len(expected)-1] = withStartMarker(expected[len(expected)-1])
		}
		if !slices.Equal(session.Command().Args, expected) {
			t.Fatalf("%s: %q", command.Purpose, session.Command().Args)
		}
	}
}

// Hard-won: #1238: a canonicalized `\\?\C:` checkpoint path reached Windows scp as a host name.
func TestScpNamesVerbatimControllerPathsWithoutTheirPrefix(t *testing.T) {
	if runtime.GOOS != "windows" {
		t.Skip("Windows only")
	}
	ssh := sshPlainTarget("dev@203.0.113.7")
	for _, c := range []struct{ local, expected string }{
		{`\\?\C:\mj\a.hel.zip`, `C:\mj\a.hel.zip`},
		{`\\?\UNC\server\share\a.hel.zip`, `\\server\share\a.hel.zip`},
		{`C:\mj\a.hel.zip`, `C:\mj\a.hel.zip`},
	} {
		upload := SCPUpload(&ssh, c.local, "a.zip", false)
		if want := []string{c.expected, "dev@203.0.113.7:a.zip"}; !slices.Equal(upload.Args, want) {
			t.Fatalf("%q", upload.Args)
		}
		download := SCPDownload(&ssh, "a.zip", c.local)
		if want := []string{"dev@203.0.113.7:a.zip", c.expected}; !slices.Equal(download.Args, want) {
			t.Fatalf("%q", download.Args)
		}
	}
}
