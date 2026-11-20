// Port of mj-core/src/targets/ssh.rs.
//
// SSH command builders, connection sharing (control masters and the session
// ledger), refusal classification and retry, and target validation. The
// Rust module gates connection sharing on cfg(unix); here those paths check
// runtime.GOOS where it matters.

package core

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"math"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"time"
	"unicode"
	"unicode/utf8"
)

// SshTrafficClass: separate TCP connections keep file transfers from
// queuing ahead of relay and liveness traffic. The command builder owns this
// choice.
type SshTrafficClass string

const (
	SshTrafficClassControl SshTrafficClass = "control"
	SshTrafficClassBulk    SshTrafficClass = "bulk"
)

// SshSessionRequest: a command's target and traffic class travel together
// through wrappers and serialization, so a copied session request cannot
// lose its classification. Target is flattened beside class.
type SshSessionRequest struct {
	Target SshTarget
	Class  SshTrafficClass
}

// MarshalJSON flattens Target beside class, as serde's flatten does.
func (r SshSessionRequest) MarshalJSON() ([]byte, error) {
	args := r.Target.SSHArgs
	if args == nil {
		args = []string{}
	}
	return json.Marshal(struct {
		Destination string          `json:"destination"`
		SSHArgs     []string        `json:"ssh_args"`
		Class       SshTrafficClass `json:"class"`
	}{r.Target.Destination, args, r.Class})
}

// UnmarshalJSON reads the flattened form.
func (r *SshSessionRequest) UnmarshalJSON(data []byte) error {
	var wire struct {
		Destination string          `json:"destination"`
		SSHArgs     []string        `json:"ssh_args"`
		Class       SshTrafficClass `json:"class"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	*r = SshSessionRequest{Target: SshTarget{Destination: wire.Destination, SSHArgs: wire.SSHArgs}, Class: wire.Class}
	return nil
}

// SSHConnectivityProbe is the connectivity probe `mj doctor` runs against an
// SSH target.
//
// It reuses the provisioning argument order so the probe fails exactly where
// a real session would, with two deliberate overrides prepended. OpenSSH
// honours the first occurrence of an option, so these win over the
// provisioning defaults: `BatchMode=yes` never prompts for a password, and
// `StrictHostKeyChecking=yes` never accepts an unknown host key. Doctor
// diagnoses; the user decides whether to trust a key.
//
// Those overrides are also why the probe joins a shared master but never
// opens one (see CommandSpec.SSHProbeSession): as the master it would hold
// them over every later session on that connection, and a plain `mj doctor`
// would leave an `ssh` process behind for the whole `ControlPersist` window
// even though the user asked only for a diagnosis.
func SSHConnectivityProbe(ssh *SshTarget) CommandSpec {
	args := []string{"-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes"}
	args = append(args, ssh.SSHArgs...)
	args = append(args, ssh.Destination, JoinRemoteCommand([]string{"true"}))
	// The socket is named after the target as configured, not after these
	// probe-only overrides, so the probe finds the daemon's master.
	return NewCommandSpec("ssh", args).SSHProbeSession(ssh).WithPurpose("verify SSH connectivity")
}

// SSHCommand builds an `ssh` command running args on the host.
func SSHCommand(class SshTrafficClass, ssh *SshTarget, args []string) CommandSpec {
	return SSHCommandOwned(class, ssh, append([]string{}, args...))
}

// SSHCommandOwned builds an `ssh` command that runs as one session on a
// shared connection. The executor leases the session and adds its options
// just before the command is spawned; see CommandSpec.WithSSHSession.
func SSHCommandOwned(class SshTrafficClass, ssh *SshTarget, remoteArgs []string) CommandSpec {
	args := append([]string{}, ssh.SSHArgs...)
	args = append(args, ssh.Destination, RemoteCommandLine(remoteArgs))
	return NewCommandSpec("ssh", args).WithSSHSession(ssh, class)
}

// RemoteUploadStaging is the home-relative directory on an SSH host where
// files bound for a remote container wait before the engine copies them in.
// Home-relative rather than `~/`, because SSHCommand quotes every argument
// while `scp` expands `~`.
const RemoteUploadStaging = ".cache/mjolnir/uploads"

// SCPUpload uploads a local file or directory to the SSH host.
func SCPUpload(ssh *SshTarget, source string, remote string, recursive bool) CommandSpec {
	args := scpArgs(ssh)
	if recursive {
		args = append(args, "-r")
	}
	args = append(args, scpLocalPath(source), ssh.Destination+":"+remote)
	return scpCommand(ssh, args)
}

// SCPDownload downloads a remote file from the SSH host.
func SCPDownload(ssh *SshTarget, remote string, local string) CommandSpec {
	args := scpArgs(ssh)
	args = append(args, ssh.Destination+":"+remote, scpLocalPath(local))
	return scpCommand(ssh, args)
}

// scpLocalPath is a controller path as `scp` must see it. `scp` reads an
// argument with a colon before any separator as `host:path`, and Windows
// OpenSSH exempts only a plain drive letter, so a verbatim path from
// canonicalization (`\\?\C:\...`) would name the host `\\?\C`. Spell it
// without the verbatim prefix; nothing else about the path changes.
func scpLocalPath(path string) string {
	if runtime.GOOS != "windows" {
		return path
	}
	const verbatim, verbatimUNC = `\\?\`, `\\?\UNC\`
	if strings.HasPrefix(path, verbatimUNC) {
		return `\\` + path[len(verbatimUNC):]
	}
	if strings.HasPrefix(path, verbatim) {
		drive := path[len(verbatim):]
		if len(drive) >= 2 && drive[1] == ':' && isSSHASCIILetter(drive[0]) && (len(drive) == 2 || drive[2] == '\\') {
			return drive
		}
	}
	return path
}

// scpArgs is the connection's `ssh` arguments rewritten for `scp`, which
// spells the port option `-P`; to `scp`, `-p` means "preserve file times".
func scpArgs(ssh *SshTarget) []string {
	args := make([]string, 0, len(ssh.SSHArgs))
	for _, argument := range ssh.SSHArgs {
		if argument == "-p" {
			argument = "-P"
		}
		args = append(args, argument)
	}
	return args
}

func scpCommand(ssh *SshTarget, args []string) CommandSpec {
	// `scp` runs `ssh` underneath, so it takes a session on a shared
	// connection and is retried the same way.
	return NewCommandSpec("scp", args).WithSSHSession(ssh, SshTrafficClassBulk)
}

// controlPersist is how long a shared master connection stays alive after
// its last channel closes. The master is an `ssh` process that outlives the
// daemon by this long, so it is kept short enough to be unsurprising and
// long enough to cover a whole provision.
const controlPersist = "60"

// masterKeepalive is the master's `ServerAliveInterval` seconds and
// `ServerAliveCountMax`: a host silent for a minute is gone. Patient enough
// that a stall of a few seconds does not drop the sessions on the
// connection.
var masterKeepalive = [2]uint32{15, 4}

// ControlMasterEnv is the environment override that turns connection
// sharing off. Any of `0`, `off`, `false`, or `no` disables it.
const ControlMasterEnv = "MJ_SSH_CONTROL_MASTER"

// maxControlPath is the longest `ControlPath` that still fits in a Unix
// socket address. `sun_path` holds 108 bytes on Linux and 104 on macOS,
// minus the terminating NUL.
const maxControlPath = 103

// connectionHashHex is the hex digits of the connection hash in a socket
// name. 64 bits keeps the names short while making a collision between two
// targets implausible.
const connectionHashHex = 16

// controlSocketNameReserve is the bytes reserved after the directory for a
// socket's name: a separator, the connection hash, `-` and a shard index of
// up to four digits, and the `.` plus 16 random characters `ssh` appends to
// the path while it binds a new master.
const controlSocketNameReserve = 1 + connectionHashHex + 1 + 4 + 17

func sharingDisabled(value *string) bool {
	if value == nil {
		return false
	}
	switch strings.ToLower(strings.TrimSpace(*value)) {
	case "0", "off", "false", "no":
		return true
	}
	return false
}

// SshSharingForTestKind names a SshSharingForTest variant.
type SshSharingForTestKind int

const (
	// SshSharingForTestDisabled behaves as though the escape hatch were set.
	SshSharingForTestDisabled SshSharingForTestKind = iota
	// SshSharingForTestDirectory keeps the control sockets in Directory.
	SshSharingForTestDirectory
)

// SshSharingForTest is how a test pins connection sharing instead of letting
// it resolve from the environment.
type SshSharingForTest struct {
	Kind      SshSharingForTestKind
	Directory string
}

var (
	sharingOverrideMu      sync.Mutex
	sharingOverrideSetting *SshSharingForTest
)

// SetSSHConnectionSharingForTest pins connection sharing for a test, or
// restores the real resolution with nil. Tests must not depend on the
// developer's `$XDG_RUNTIME_DIR` or home directory, so every test that
// inspects `ssh` arguments pins it. Not part of the daemon's behaviour.
func SetSSHConnectionSharingForTest(setting *SshSharingForTest) {
	sharingOverrideMu.Lock()
	defer sharingOverrideMu.Unlock()
	sharingOverrideSetting = setting
}

func sharingOverride() *SshSharingForTest {
	sharingOverrideMu.Lock()
	defer sharingOverrideMu.Unlock()
	if sharingOverrideSetting == nil {
		return nil
	}
	copied := *sharingOverrideSetting
	return &copied
}

// The directory resolved from the environment, once per process (Rust: a
// OnceLock inside control_socket_dir).
var (
	controlSocketDirOnce  sync.Once
	controlSocketDirValue string
	controlSocketDirOK    bool
)

// controlSocketDir is the directory holding this instance's control sockets,
// or false when sharing is off.
//
// `$XDG_RUNTIME_DIR/mjolnir/<identity>` is preferred because it is short,
// per-user, and on tmpfs; the instance's data directory is the fallback.
// Each instance gets its own directory because each daemon counts only its
// own sessions: two daemons sharing masters would together exceed the
// server's per-connection session limit. Neither location is world-writable,
// and the directory is created 0700 because `ssh` will not create it itself.
func controlSocketDir() (string, bool) {
	if override := sharingOverride(); override != nil {
		switch override.Kind {
		case SshSharingForTestDisabled:
			return "", false
		case SshSharingForTestDirectory:
			return prepareControlDir(override.Directory)
		}
	}
	controlSocketDirOnce.Do(func() {
		var setting *string
		if value, ok := os.LookupEnv(ControlMasterEnv); ok {
			setting = &value
		}
		if sharingDisabled(setting) {
			return
		}
		var dataDirOverride *string
		if value, ok := EnvOverrideOS("DATA_DIR"); ok {
			dataDirOverride = &value
		}
		var instance *string
		if value, ok := InstanceName(); ok {
			instance = &value
		}
		identity := controlDirIdentity(instance, dataDirOverride)
		var runtimeDir *string
		if value, ok := os.LookupEnv("XDG_RUNTIME_DIR"); ok {
			runtimeDir = &value
		}
		controlSocketDirValue, controlSocketDirOK = prepareControlDir(defaultControlDir(runtimeDir, identity))
	})
	return controlSocketDirValue, controlSocketDirOK
}

// defaultControlDir is where an instance keeps its sockets when no test pins
// the directory.
func defaultControlDir(runtimeDir *string, identity string) string {
	if runtimeDir != nil && *runtimeDir != "" {
		return filepath.Join(*runtimeDir, "mjolnir", identity)
	}
	// The data directory is already specific to the instance.
	return filepath.Join(DataDir(), "ssh")
}

// controlDirIdentity is the name of this instance's directory under
// `$XDG_RUNTIME_DIR/mjolnir`: the identity InstanceIdentity stamps on the
// instance's workers. A named instance uses its name, and a data-directory
// override (`MJ_DATA_DIR`, as every end-to-end lab sets) uses the
// fingerprint of that directory, so neither shares the default instance's
// masters or sweeps its lock files. The default instance keeps `default`,
// where its sockets have always been.
func controlDirIdentity(instance *string, dataDirOverride *string) string {
	if instance == nil && dataDirOverride == nil {
		return "default"
	}
	dataDir := DataDir()
	if dataDirOverride != nil {
		dataDir = *dataDirOverride
	}
	name := ""
	if instance != nil {
		name = *instance
	}
	return InstanceIdentityFor(name, dataDir)
}

// prepareControlDir creates the socket directory 0700 and rejects one whose
// sockets would not fit in a Unix socket address. Failure means no sharing,
// never a failed command.
func prepareControlDir(dir string) (string, bool) {
	if len(dir)+controlSocketNameReserve > maxControlPath {
		slog.Debug("skipping SSH connection sharing: control socket path would be too long", "directory", dir)
		return "", false
	}
	if err := os.MkdirAll(dir, 0o777); err != nil {
		slog.Debug("skipping SSH connection sharing: control directory is unavailable", "directory", dir, "error", err)
		return "", false
	}
	if err := os.Chmod(dir, 0o700); err != nil {
		slog.Debug("skipping SSH connection sharing: cannot restrict control directory", "directory", dir, "error", err)
		return "", false
	}
	return dir, true
}

// connectionKey is the identity of one configured connection: its
// destination and the user's own `ssh` arguments, which can change the port,
// user, or route. Two targets that differ in either get separate masters.
// The class is spelled as Rust's Debug spells it ("Control", "Bulk"), which
// the socket name hashes.
func connectionKey(ssh *SshTarget, class SshTrafficClass) string {
	// Rust's Debug spelling of the class.
	name := "Control"
	if class == SshTrafficClassBulk {
		name = "Bulk"
	}
	var key strings.Builder
	key.WriteString(name)
	key.WriteByte(0)
	key.WriteString(ssh.Destination)
	for _, argument := range ssh.SSHArgs {
		key.WriteByte(0)
		key.WriteString(argument)
	}
	return key.String()
}

// controlSocketName is the socket file name for one shard of a traffic
// class: `<hash>-<shard>`. The hash includes the class, so each class has
// independent masters and ledger entries.
//
// Mjolnir names sockets itself rather than using ssh's `%C` so that the name
// follows exactly the key the daemon counts sessions under, and so the
// daemon knows the concrete path when it must remove a stale socket.
func controlSocketName(ssh *SshTarget, class SshTrafficClass, shard int) string {
	digest := sha256.Sum256([]byte(connectionKey(ssh, class)))
	return fmt.Sprintf("%s-%d", LowerHex(digest[:connectionHashHex/2]), shard)
}

// controlPathOption is the `-o` value that points `ssh` at socket.
//
// An ssh option is one line of config, and ssh splits that line on
// whitespace unless the value stands inside double quotes. A socket path
// with a space in it therefore ends the option early and ssh refuses the
// whole command line. `%` starts a token expansion in `ControlPath`, so it is
// doubled. A path that needs neither is passed exactly as it always was.
func controlPathOption(socket string) string {
	path := strings.ReplaceAll(socket, "%", "%%")
	if strings.ContainsFunc(path, func(character rune) bool {
		return unicode.IsSpace(character) || character == '"' || character == '\\'
	}) {
		// Inside the quotes OpenSSH unescapes only these two.
		path = `"` + strings.ReplaceAll(strings.ReplaceAll(path, `\`, `\\`), `"`, `\"`) + `"`
	}
	return "ControlPath=" + path
}

// userConfiguresSharing reports whether the user's own `ssh` arguments
// already configure connection sharing. OpenSSH keeps the first value it
// sees, so Mjolnir cannot add its own sharing options without either
// overriding the user or being overridden; a user who configures sharing
// owns it, and Mjolnir adds none.
func userConfiguresSharing(sshArgs []string) bool {
	for _, argument := range sshArgs {
		if strings.HasPrefix(argument, "-S") {
			return true
		}
		option := strings.TrimLeftFunc(strings.TrimPrefix(argument, "-o"), unicode.IsSpace)
		option = sshASCIILower(option)
		for _, name := range []string{"controlmaster", "controlpath"} {
			if rest, ok := strings.CutPrefix(option, name); ok && rest != "" && strings.ContainsRune("= \t", rune(rest[0])) {
				return true
			}
		}
	}
	return false
}

// PushConnectionReuseArgs appends the options that let this command *reuse*
// a shared master without ever becoming one.
//
// Use this for commands that carry deliberately impatient options, such as
// the short `ConnectTimeout` and one-miss `ServerAlive` keepalive of a
// validation probe or a Tab completion. With `ControlMaster=no` the command
// joins the connection's first master when one is up and otherwise opens
// its own direct connection, keeping its fail-fast options to itself.
//
// These commands run in processes without the daemon's session ledger
// (`mj doctor`, completion) or only validate a target, so they are not
// counted and are not bound to a master with `ProxyCommand=false`: for a
// diagnosis, a direct connection is the stated behaviour.
func PushConnectionReuseArgs(args *[]string, ssh *SshTarget) {
	if runtime.GOOS == "windows" || userConfiguresSharing(ssh.SSHArgs) {
		return
	}
	dir, ok := controlSocketDir()
	if !ok {
		return
	}
	socket := filepath.Join(dir, controlSocketName(ssh, SshTrafficClassControl, 0))
	*args = append(*args, "-o", "ControlMaster=no", "-o", controlPathOption(socket))
}

// JoinRemoteCommand is args quoted for a POSIX shell, joined into one line.
func JoinRemoteCommand(args []string) string {
	quoted := make([]string, len(args))
	for i, arg := range args {
		quoted[i] = PosixQuote(arg)
	}
	return strings.Join(quoted, " ")
}

// RemoteCommandLine is the command line an `ssh` invocation sends for args:
// each argument quoted, run under WithMacosSystemPath. Every builder of an
// SSH remote command uses this, so a Mac host finds the same programs
// whatever the command is.
func RemoteCommandLine(args []string) string {
	return JoinRemoteCommand(WithMacosSystemPath(append([]string{}, args...)))
}

// DaemonLoginCommand discovers a daemon using the account's login-shell
// PATH. The outer command is portable across POSIX shells and fish; only
// /bin/sh parses parameter expansion. The login shell receives a quoted argv
// command, not shell input supplied by configuration.
func DaemonLoginCommand(args []string) string {
	command := JoinRemoteCommand(args)
	return JoinRemoteCommand([]string{
		"/bin/sh",
		"-c",
		fmt.Sprintf(`exec "${SHELL:-/bin/sh}" -lc %s`, PosixQuote(command)),
	})
}

// WithMacosSystemPath runs args with, on a Mac, the directories macOS lists
// in /etc/paths and /etc/paths.d and Homebrew's Apple-silicon prefix after
// the session's own PATH, so a directory the account already chose still
// comes first. Another host, which has no `path_helper`, runs args
// unchanged.
//
// An OpenSSH command session on macOS gets /usr/bin:/bin:/usr/sbin:/sbin
// only: macOS adds /etc/paths in login shells, and Homebrew's installer adds
// /opt/homebrew/bin through the login shell's profile. Apple's `container`
// (/usr/local/bin) and Homebrew's `docker`, `podman` and `colima` would
// otherwise not be found.
func WithMacosSystemPath(args []string) []string {
	return append([]string{
		"/bin/sh",
		"-c",
		`if [ -x /usr/libexec/path_helper ]; then session=$PATH; eval "$(PATH= /usr/libexec/path_helper -s)"; PATH=${session:+$session:}$PATH:/opt/homebrew/bin:/opt/homebrew/sbin; fi; exec "$@"`,
		"sh",
	}, args...)
}

// SSHDirectoryExists checks whether a directory exists on the configured SSH
// host.
func SSHDirectoryExists(ssh *SshTarget, path string, executor CommandExecutor) (bool, error) {
	command := SSHValidationCommand(ssh, []string{"test", "-d", path}, "validate remote directory")
	output, err := executor.Execute(&command)
	if err != nil {
		return false, err
	}
	switch output.Status {
	case 0:
		return true, nil
	case 1:
		return false, nil
	}
	stderr := sshLossy(output.Stderr)
	failure := fmt.Errorf("remote directory check failed with status %d: %s", output.Status, strings.TrimSpace(stderr))
	if refusal := hostKeyRefusal(stderr, ssh.SSHArgs); refusal != nil {
		return false, refusal.Wrap(failure)
	}
	return false, failure
}

// hostKeyRefusal is what the caller is told when `ssh` refused the host's
// key, so it does not get only a daemon log reference (launch finding R3-6).
//
// OpenSSH's wording is the only signal: "Host key verification failed." ends
// both an unknown key under strict checking and a key that changed. The
// sentence quotes that line and names no host, so it may reach any client;
// the full ssh text stays on the error chain for the daemon log. It names
// the known_hosts file the machine's ssh options name (launch finding R6-5).
func hostKeyRefusal(stderr string, sshArgs []string) *Refusal {
	if !strings.Contains(stderr, "Host key verification failed") {
		return nil
	}
	knownHosts := knownHostsFileFromSSHArgs(sshArgs)
	file := knownHosts.phrase()
	var message string
	if strings.Contains(stderr, "REMOTE HOST IDENTIFICATION HAS CHANGED") {
		keygen := "`ssh-keygen -R`"
		if path, ok := knownHosts.firstNamed(); ok {
			keygen = fmt.Sprintf("`ssh-keygen -f %s -R`", path)
		}
		message = fmt.Sprintf("ssh reported \"Host key verification failed\": the machine's host key is not the one saved in %s. If you expected the change, remove the old entry with %s and the host name, add the new key, and try again.", file, keygen)
	} else {
		message = fmt.Sprintf("ssh reported \"Host key verification failed\": the machine's host key is not in %s, and its ssh options require a known key. Add the host key (for example with `ssh-keyscan`, after checking the fingerprint), or put `-o StrictHostKeyChecking=accept-new` in the machine's extra_args, and try again.", file)
	}
	refusal := RefusalPrecondition(message)
	return &refusal
}

// KnownHostsFileKind names a KnownHostsFile variant.
type KnownHostsFileKind int

const (
	// KnownHostsFileDefault: no option names one, so ssh uses its default.
	KnownHostsFileDefault KnownHostsFileKind = iota
	// KnownHostsFileNamed: `-o UserKnownHostsFile=` names Paths, one file or
	// several separated by spaces.
	KnownHostsFileNamed
	// KnownHostsFileUnknown: a config file given with `-F` may name any
	// file, or the option names no usable file.
	KnownHostsFileUnknown
)

// KnownHostsFile is the known_hosts file ssh checks a host key against, as
// far as the options on its command line tell.
type KnownHostsFile struct {
	Kind  KnownHostsFileKind
	Paths string
}

// knownHostsFileFromSSHArgs reads ssh's arguments the way OpenSSH does: the
// first `UserKnownHostsFile` given with `-o` wins, the keyword is matched
// without regard to case, and its value follows `=` or a space.
func knownHostsFileFromSSHArgs(args []string) KnownHostsFile {
	configFile := false
	for index := 0; index < len(args); index++ {
		arg := args[index]
		var option string
		hasOption := false
		if arg == "-o" {
			if index+1 < len(args) {
				index++
				option, hasOption = args[index], true
			}
		} else if rest, ok := strings.CutPrefix(arg, "-o"); ok {
			option, hasOption = rest, true
		}
		if strings.HasPrefix(arg, "-F") {
			configFile = true
		}
		if !hasOption {
			continue
		}
		value, ok := optionValue(option, "UserKnownHostsFile")
		if !ok {
			continue
		}
		value = strings.TrimSpace(strings.Trim(value, `"`))
		if value == "" || sshEqualFoldASCII(value, "none") {
			return KnownHostsFile{Kind: KnownHostsFileUnknown}
		}
		return KnownHostsFile{Kind: KnownHostsFileNamed, Paths: value}
	}
	if configFile {
		return KnownHostsFile{Kind: KnownHostsFileUnknown}
	}
	return KnownHostsFile{Kind: KnownHostsFileDefault}
}

// phrase is the file as a refusal names it.
func (f *KnownHostsFile) phrase() string {
	switch f.Kind {
	case KnownHostsFileDefault:
		return "~/.ssh/known_hosts"
	case KnownHostsFileNamed:
		return strings.Join(strings.Fields(f.Paths), " or ")
	default:
		return "the known_hosts file ssh uses"
	}
}

// firstNamed is the first file an option names, which is the one ssh adds
// keys to.
func (f *KnownHostsFile) firstNamed() (string, bool) {
	if f.Kind != KnownHostsFileNamed {
		return "", false
	}
	fields := strings.Fields(f.Paths)
	if len(fields) == 0 {
		return "", false
	}
	return fields[0], true
}

// optionValue is the value of keyword in one ssh option (`Keyword=value`,
// `Keyword value`, or `Keyword = value`), or false for another keyword.
func optionValue(option, keyword string) (string, bool) {
	option = strings.TrimLeftFunc(option, unicode.IsSpace)
	end := strings.IndexFunc(option, func(character rune) bool {
		return character == '=' || unicode.IsSpace(character)
	})
	if end < 0 {
		end = len(option)
	}
	name, rest := option[:end], option[end:]
	if !sshEqualFoldASCII(name, keyword) {
		return "", false
	}
	rest = strings.TrimLeftFunc(rest, unicode.IsSpace)
	return strings.TrimSpace(strings.TrimPrefix(rest, "=")), true
}

// ValidateBareProjectDirectory verifies that a bare-SSH project path exists
// and has a committed Git HEAD.
func ValidateBareProjectDirectory(ssh *SshTarget, path string, executor CommandExecutor) error {
	if err := ValidateBareProjectPath(path); err != nil {
		return err
	}
	exists, err := SSHDirectoryExists(ssh, path, executor)
	if err != nil {
		return err
	}
	if !exists {
		return RefusalUnusable(fmt.Sprintf("remote project directory %s does not exist or is not a directory on %s", path, ssh.Destination))
	}
	command := SSHValidationCommand(ssh, []string{"git", "-C", path, "rev-parse", "--verify", "HEAD"}, "validate bare SSH Git project")
	output, err := executor.Execute(&command)
	if err != nil {
		return err
	}
	if output.Status != 0 {
		detail := strings.TrimSpace(sshLossy(output.Stderr))
		if detail == "" {
			return fmt.Errorf("remote project directory %s has no valid Git HEAD", path)
		}
		return fmt.Errorf("remote project directory %s has no valid Git HEAD: %s", path, detail)
	}
	return nil
}

// ValidateBareProjectPath requires an absolute path without `..`.
func ValidateBareProjectPath(path string) error {
	if !IsAbsolute(path) || slices.Contains(sshPathComponents(path), "..") {
		return errors.New("bare project directory must be an absolute safe path")
	}
	return nil
}

// SSHValidationCommand is a fail-fast `ssh` probe running remoteArgs.
func SSHValidationCommand(ssh *SshTarget, remoteArgs []string, purpose string) CommandSpec {
	args := append([]string{}, ssh.SSHArgs...)
	args = append(args,
		"-o", "BatchMode=yes",
		"-o", fmt.Sprintf("ConnectTimeout=%d", uint64(sshProbeConnectTimeout/time.Second)),
		"-o", "ServerAliveInterval=2",
		"-o", "ServerAliveCountMax=1",
		ssh.Destination, RemoteCommandLine(remoteArgs),
	)
	return NewCommandSpec("ssh", args).SSHProbeSession(ssh).WithPurpose(purpose)
}

// PosixQuote wraps a value so a POSIX shell reads it as one literal
// argument. Used at the SSH boundary here and when Hel rebuilds an agent's
// terminal command line (`terminal::shell_line`).
func PosixQuote(value string) string {
	return "'" + strings.ReplaceAll(value, "'", `'\''`) + "'"
}

// VerifyLocator refuses a locator that does not belong to sessionID.
func VerifyLocator(locator *TargetsTargetLocator, sessionID string) error {
	if err := ValidateSessionID(sessionID); err != nil {
		return err
	}
	switch locator.Kind {
	case TargetsTargetLocatorLocalBare:
		components := sshPathComponents(locator.WorkerRoot)
		if !IsAbsolute(locator.WorkerRoot) || slices.Contains(components, "..") ||
			len(components) == 0 || components[len(components)-1] != sessionID {
			return errors.New("refusing cleanup: invalid local bare worker root")
		}
	case TargetsTargetLocatorLocalPodman, TargetsTargetLocatorLocalDocker, TargetsTargetLocatorAppleContainer,
		TargetsTargetLocatorSshPodman, TargetsTargetLocatorSshDocker, TargetsTargetLocatorSshAppleContainer,
		TargetsTargetLocatorSshNitro:
		if owner := locator.BorrowedFrom; owner != nil {
			if err := ValidateSessionID(*owner); err != nil {
				return err
			}
			if *owner == sessionID {
				return errors.New("refusing cleanup: a borrowed container cannot be owned by the borrowing session")
			}
			belongs, err := ResourceNameBelongsTo(locator.ContainerID, *owner)
			if err != nil {
				return err
			}
			if !belongs && !IsRuntimeContainerID(locator.ContainerID) {
				return errors.New("refusing cleanup: borrowed container locator is neither the owning session's generated name nor an immutable runtime ID")
			}
		} else {
			belongs, err := ResourceNameBelongsTo(locator.ContainerID, sessionID)
			if err != nil {
				return err
			}
			if !belongs && !IsRuntimeContainerID(locator.ContainerID) {
				return errors.New("refusing cleanup: container locator is neither the generated name nor an immutable runtime ID")
			}
		}
	case TargetsTargetLocatorAwsEc2:
		if !ValidEc2InstanceID(locator.InstanceID) {
			return errors.New("refusing cleanup: invalid EC2 instance ID")
		}
		return VerifySessionWorkspace(locator.Workspace, sessionID)
	case TargetsTargetLocatorSshBare:
		if workerID := locator.WorkerID; workerID != nil {
			if err := ValidateSessionID(*workerID); err != nil {
				return err
			}
			if *workerID != sessionID {
				return errors.New("refusing cleanup: SSH worker identity does not match session ID")
			}
			return ValidateWorkspacePrefix(locator.Workspace)
		}
		return VerifySessionWorkspace(locator.Workspace, sessionID)
	default:
		return fmt.Errorf("unknown target locator kind %q", locator.Kind)
	}
	return nil
}

// IsBorrowed reports whether this locator names a target another session
// owns: a sub-agent child either borrowing its parent's container or running
// as its own worker inside the parent's SSH workspace.
func IsBorrowed(locator *TargetsTargetLocator) bool {
	switch locator.Kind {
	case TargetsTargetLocatorSshBare:
		return locator.WorkerID != nil
	case TargetsTargetLocatorLocalBare, TargetsTargetLocatorAwsEc2:
		return false
	default:
		return locator.BorrowedFrom != nil
	}
}

// VerifySessionWorkspace requires a safe workspace ending in the exact
// session ID.
func VerifySessionWorkspace(workspace, sessionID string) error {
	if err := ValidateWorkspacePrefix(workspace); err != nil {
		return err
	}
	trimmed := strings.TrimRight(workspace, "/")
	if trimmed[strings.LastIndexByte(trimmed, '/')+1:] != sessionID {
		return errors.New("refusing cleanup: workspace does not end in the exact session ID")
	}
	return nil
}

// ValidateSessionID requires 8-128 ASCII letters, digits, '-' or '_'.
func ValidateSessionID(value string) error {
	valid := len(value) >= 8 && len(value) <= 128
	for i := 0; valid && i < len(value); i++ {
		c := value[i]
		valid = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '-' || c == '_'
	}
	if !valid {
		return fmt.Errorf("session ID must be 8-128 ASCII letters, digits, '-' or '_'")
	}
	return nil
}

// ValidateRelativePath refuses an empty, absolute or non-normal bundle path.
func ValidateRelativePath(value string) error {
	unsafe := value == "" || filepath.IsAbs(value) || strings.HasPrefix(value, "/")
	if runtime.GOOS == "windows" && !unsafe {
		// A drive or UNC prefix, or a root without one, is not a Normal
		// component.
		unsafe = filepath.VolumeName(value) != "" || strings.HasPrefix(value, `\`)
	}
	if !unsafe {
		for index, part := range sshPathSplit(value) {
			// Rust's components skip an interior "." but keep a leading one.
			if part == ".." || (part == "." && index == 0) {
				unsafe = true
				break
			}
		}
	}
	if unsafe {
		return fmt.Errorf("unsafe relative bundle path %s", debugString(value))
	}
	return nil
}

// ValidateWorkspacePrefix refuses an unsafe workspace path.
func ValidateWorkspacePrefix(value string) error {
	if value == "" || value == "/" || value == "~" || value == "~/" ||
		strings.ContainsRune(value, 0) || slices.Contains(strings.Split(value, "/"), "..") {
		return errors.New("unsafe workspace path")
	}
	return nil
}

// ValidateContainerTemplate refuses an invalid image and run arguments that
// override the generated name or Mjolnir's ownership labels.
func ValidateContainerTemplate(template *TargetsContainerTemplate) error {
	if strings.TrimSpace(template.Image) == "" || strings.HasPrefix(template.Image, "-") {
		return errors.New("invalid container image")
	}
	for _, arg := range template.ExtraRunArgs {
		if arg == "--name" || strings.HasPrefix(arg, "--name=") {
			return errors.New("container template may not override the generated name")
		}
	}
	for _, arg := range template.ExtraRunArgs {
		if arg == "--label" {
			return errors.New("container template may not override Mjolnir ownership labels")
		}
		for _, label := range []string{SessionLabel, ManagedLabel, InstanceLabel} {
			if strings.HasPrefix(arg, "--label="+label+"=") {
				return errors.New("container template may not override Mjolnir ownership labels")
			}
		}
	}
	return nil
}

// ValidateSSH refuses an invalid SSH destination.
func ValidateSSH(ssh *SshTarget) error {
	if strings.TrimSpace(ssh.Destination) == "" || strings.HasPrefix(ssh.Destination, "-") ||
		strings.ContainsFunc(ssh.Destination, unicode.IsSpace) {
		return errors.New("invalid SSH destination")
	}
	return nil
}

// ValidateExecutable refuses an invalid executable name.
func ValidateExecutable(value string) error {
	if value == "" || strings.HasPrefix(value, "-") || strings.ContainsFunc(value, unicode.IsSpace) {
		return errors.New("invalid executable name")
	}
	return nil
}

// ValidEc2InstanceID reports whether value is `i-` and at least eight hex
// digits.
func ValidEc2InstanceID(value string) bool {
	rest, ok := strings.CutPrefix(value, "i-")
	return ok && len(rest) >= 8 && isSSHASCIIHex(rest)
}

// IsRuntimeContainerID reports whether value is 12-128 hex digits.
func IsRuntimeContainerID(value string) bool {
	return len(value) >= 12 && len(value) <= 128 && isSSHASCIIHex(value)
}

// SSHTransportExitStatus: `ssh` reserves exit status 255 for its own
// transport failures; a remote command never produces it, so the remote
// side provably never ran.
const SSHTransportExitStatus = 255

// isPreAuthenticationHangup matches stderr lines OpenSSH prints only when
// the server hangs up before authentication. `sshd`'s `MaxStartups` produces
// these when it drops an unauthenticated connection, and so does a server
// that is still starting.
//
// The packet layer reports a close or reset during key exchange or
// authentication as "Connection closed by ADDR port N" or "Connection reset
// by ADDR port N". Once the session runs, the client reports the same events
// differently, and those must not match: the remote command may already be
// running, even if its start marker never arrived.
func isPreAuthenticationHangup(line string) bool {
	// `kex_exchange_identification` since OpenSSH 8.4,
	// `ssh_exchange_identification` before it.
	if strings.Contains(line, "exchange_identification:") ||
		strings.Contains(line, "Connection timed out during banner exchange") {
		return true
	}
	for _, prefix := range []string{"Connection closed by ", "Connection reset by "} {
		_, peer, ok := strings.Cut(line, prefix)
		if !ok {
			continue
		}
		peer = strings.TrimRightFunc(peer, unicode.IsSpace)
		separator := strings.LastIndex(peer, " port ")
		if separator < 0 {
			continue
		}
		port := peer[separator+len(" port "):]
		if port != "" && strings.Trim(port, "0123456789") == "" {
			return true
		}
	}
	return false
}

// IsTransportRejection reports whether a finished `ssh` process was turned
// away by the transport rather than by the remote command.
//
// The remote command never started in this case, so the caller may retry
// the whole invocation without worrying about repeating a side effect.
func IsTransportRejection(status int, stderr string) bool {
	_, ok := SSHRefusal(status, stderr)
	return ok
}

// sessionRefusalMarker is what `ssh` prints when the server refuses a new
// session on an existing, authenticated shared connection. `sshd` does this
// once the connection carries `MaxSessions` sessions.
const sessionRefusalMarker = "Session open refused by peer"

// SshRefusal says why the SSH server turned an `ssh` invocation away before
// its remote command started. Either way the command never ran, so it may
// be retried.
type SshRefusal int

const (
	// SshRefusalBeforeAuthentication: the server dropped a new connection
	// before authentication, which is what `MaxStartups` does, and a dead
	// shared master looks the same to a session bound to it.
	SshRefusalBeforeAuthentication SshRefusal = iota
	// SshRefusalSessionLimit: the shared connection is up and authenticated,
	// but the server refused one more session on it (`MaxSessions`).
	SshRefusalSessionLimit
)

// retryMessage is a log message that names this refusal.
func (r SshRefusal) retryMessage() string {
	if r == SshRefusalBeforeAuthentication {
		return "the SSH server closed the connection before authentication; retrying"
	}
	return "the SSH server refused another session on a shared connection (MaxSessions); retrying"
}

// logRetry logs one retry of a command this refusal turned away.
//
// A refused session on a live shared connection is routine while many
// sessions start at once, and the retry nearly always gets in (launch
// finding R3-5 counted 98 in one dashboard log, all answered), so it is
// logged at debug level. A connection closed before authentication can mean
// a master died, so it stays a warning.
func (r SshRefusal) logRetry(destination, purpose string, attempt int, delay time.Duration, stderr string) {
	attrs := []any{"destination", destination, "purpose", purpose, "attempt", attempt,
		"delay_ms", uint64(delay.Milliseconds()), "stderr", stderr}
	if r == SshRefusalSessionLimit {
		slog.Debug(r.retryMessage(), attrs...)
	} else {
		slog.Warn(r.retryMessage(), attrs...)
	}
}

// sshPersistentRefusal is how long a refusal that the command's walk cannot
// route around is retried in place before the command fails: a connection
// closed before authentication, a session refused on every shard the command
// can reach (it opened a master itself and no other shard is left to try),
// or a refusal for a lease with no shared master.
//
// `sshd`'s `MaxStartups` (start:rate:full) drops connections at random and
// only while unauthenticated connections pile up, and a slot frees when one
// of them finishes. A full minute of consecutive refusals therefore points
// to a permanent cause: a firewall or gateway that always closes the
// connection, `MaxSessions 0`, or sessions that never end. Cancellation and
// deadlines of the caller end the retries sooner.
const sshPersistentRefusal = 60 * time.Second

// sshProbeConnectTimeout is the `ConnectTimeout` of a fail-fast probe
// (target validation), which is also the longest its refusals are retried:
// a probe that waited longer for the server to let it in than it waits to
// connect would not be fail-fast.
const sshProbeConnectTimeout = 3 * time.Second

// sshPersistentRefusalOverrideMs is the test override of
// sshPersistentRefusal in milliseconds. math.MaxUint64 means "no override".
var sshPersistentRefusalOverrideMs atomic.Uint64

// sshRetryBackoffOverrideMs is the test override of the retry backoff in
// milliseconds. math.MaxUint64 means "no override".
var sshRetryBackoffOverrideMs atomic.Uint64

func init() {
	sshPersistentRefusalOverrideMs.Store(math.MaxUint64)
	sshRetryBackoffOverrideMs.Store(math.MaxUint64)
}

// SetSSHPersistentRefusalForTest shortens the persistent-refusal bound so
// tests can reach it without waiting a minute; nil restores it. Not part of
// the daemon's behaviour.
func SetSSHPersistentRefusalForTest(bound *time.Duration) {
	if bound == nil {
		sshPersistentRefusalOverrideMs.Store(math.MaxUint64)
		return
	}
	sshPersistentRefusalOverrideMs.Store(uint64(bound.Milliseconds()))
}

func persistentRefusal() time.Duration {
	ms := sshPersistentRefusalOverrideMs.Load()
	if ms == math.MaxUint64 {
		return sshPersistentRefusal
	}
	return time.Duration(ms) * time.Millisecond
}

// SshRefusalPersisted is a refusal the server kept up for the whole
// sshPersistentRefusal window. Its text names the host, the possible causes
// and the fixes.
type SshRefusalPersisted struct {
	refusal     SshRefusal
	destination string
	elapsed     time.Duration
	stderr      string
}

func (e *SshRefusalPersisted) Error() string {
	seconds := uint64(e.elapsed / time.Second)
	if e.refusal == SshRefusalBeforeAuthentication {
		return fmt.Sprintf("the SSH server at %s closed every connection before authentication "+
			"for %d s (%s). Either sshd's MaxStartups limit is still full, or a "+
			"firewall or SSH gateway closes the connections. Raise MaxStartups in sshd_config "+
			"on that host, or allow the connection through the firewall or gateway",
			e.destination, seconds, e.stderr)
	}
	return fmt.Sprintf("the SSH server at %s refused every session on a shared connection "+
		"for %d s (%s). Either sshd_config sets MaxSessions 0, which allows "+
		"no shared sessions, or other sessions hold every slot. Set MaxSessions to 1 or "+
		"more (10 is the default) in sshd_config on that host, or set "+
		"%s=off to give every command its own connection",
		e.destination, seconds, e.stderr, ControlMasterEnv)
}

// SshRefusalRetry is what follows each refusal of one `ssh` command: the
// single owner of the retry decision, shared by the executors, the relay
// connect, Move and the resource poller.
//
// The command never ran, so it is safe to retry. A refused session on a
// shared master that another shard may still have room on retries at once:
// the command's SshWalk, which this value owns, sends the next lease to
// another master, if the walk has one left. Every other refusal waits with a
// jittered backoff and retries, and fails once the first such refusal is a
// full bound old: sshPersistentRefusal, or for a fail-fast probe its own
// connect timeout. The same bound applies whether or not the caller can be
// cancelled.
type SshRefusalRetry struct {
	walk SshWalk
	// stuckSince is when the first refusal that did not walk on happened.
	stuckSince *time.Time
	// bound is how long such refusals are retried.
	bound time.Duration
	// waits counts refusals that waited; it indexes the backoff.
	waits int
}

// NewSshRefusalRetry starts a retry with the persistent-refusal bound.
func NewSshRefusalRetry() *SshRefusalRetry {
	return &SshRefusalRetry{bound: persistentRefusal()}
}

// SshRefusalRetryForCommand is the retry for command: a fail-fast probe
// gives a refusal no more time than its own connect timeout.
func SshRefusalRetryForCommand(command *CommandSpec) *SshRefusalRetry {
	retry := NewSshRefusalRetry()
	if command.SSHSessionProbe {
		retry.bound = min(retry.bound, sshProbeConnectTimeout)
	}
	return retry
}

// Walk is the command's walk across the shards of its connection. Pass it
// to every lease the command takes.
func (r *SshRefusalRetry) Walk() *SshWalk { return &r.walk }

// Next is the wait before the next attempt, or the *SshRefusalPersisted to
// report when the refusal has lasted the bound. walksOn is what
// SshSessionLease.NoteRefusal returned: whether the refusal is a step of the
// walk to another master. This logs the retry or the exhaustion.
func (r *SshRefusalRetry) Next(refusal SshRefusal, walksOn bool, destination, purpose, stderr string) (time.Duration, error) {
	stderr = strings.TrimSpace(stderr)
	// Ask the walk now, so shards added since the lease count.
	if walksOn && r.walk.canWalkOn() {
		// The walk sends the next lease to a shard this command has not
		// tried, or opens a new one, so this refusal cannot repeat: no wait,
		// no time spent. The command is no longer stuck, so a later refusal
		// it cannot route around gets the whole bound again; the walk still
		// ends, because each walk opens at most one master.
		r.stuckSince = nil
		refusal.logRetry(destination, purpose, r.waits, 0, stderr)
		return 0, nil
	}
	// A connection dropped before authentication usually means the master
	// died: try the same shard again. A session refused on every shard the
	// command has tried goes back to the one tried least recently.
	if refusal == SshRefusalBeforeAuthentication {
		r.walk.RetryLastShard()
	}
	if r.stuckSince == nil {
		now := time.Now()
		r.stuckSince = &now
	}
	elapsed := time.Since(*r.stuckSince)
	remaining := r.bound - elapsed
	if remaining <= 0 {
		slog.Warn("the SSH server kept refusing the command; giving up",
			"destination", destination, "purpose", purpose, "attempts", r.waits,
			"elapsed_s", uint64(elapsed/time.Second), "stderr", stderr)
		return 0, &SshRefusalPersisted{refusal: refusal, destination: destination, elapsed: elapsed, stderr: stderr}
	}
	r.waits++
	// The last wait ends at the bound, so the command is never held longer.
	delay := min(SSHRetryDelay(r.waits), remaining)
	refusal.logRetry(destination, purpose, r.waits, delay, stderr)
	return delay, nil
}

// SSHRefusal classifies a finished `ssh` process that the server turned
// away. A refused session is checked first: a session bound to its master
// with `ProxyCommand=false` also reports a closed connection after the
// refusal.
func SSHRefusal(status int, stderr string) (SshRefusal, bool) {
	if status != SSHTransportExitStatus {
		return 0, false
	}
	return SSHRefusalInStderr(stderr)
}

// SSHRefusalInStderr is SSHRefusal for a command that runs `ssh` as its
// transport and exits with a status of its own (`rsync` exits 12 when its
// transport closes), so only the ssh messages it relays on stderr can tell a
// refusal.
func SSHRefusalInStderr(stderr string) (SshRefusal, bool) {
	if strings.Contains(stderr, sessionRefusalMarker) {
		return SshRefusalSessionLimit, true
	}
	for _, line := range strings.Split(stderr, "\n") {
		if isPreAuthenticationHangup(strings.TrimSuffix(line, "\r")) {
			return SshRefusalBeforeAuthentication, true
		}
	}
	return 0, false
}

// SSHStartMarker is the first line every remote command mj sends over `ssh`
// prints on stdout, before the command itself runs (see withStartMarker).
//
// Stdout is empty exactly when the session never got as far as running the
// command's shell: a server that refuses a session or drops a connection
// before authentication prints nothing there. So a session that ended with
// no stdout byte at all provably never started and is always safe to retry,
// and one that printed even a byte is a real result, whatever its exit
// status. The ledger also counts a session as open only from this byte.
const SSHStartMarker = "MJOLNIR-SSH-SESSION-STARTED\n"

// withStartMarker is the remote command line with the start marker printed
// first. `;` joins them because callers prepend compound statements to the
// line (`read`, `export`, `exec`), and it is valid in sh, bash, zsh, fish and
// csh alike.
func withStartMarker(remoteCommand string) string {
	return fmt.Sprintf(`printf '%%s\n' '%s'; %s`, strings.TrimRight(SSHStartMarker, "\n"), remoteCommand)
}

// StartScan removes SSHStartMarker from the front of a session's stdout and
// tells when the session has started.
//
// The first stdout byte of any kind proves the session started. A stream
// that begins with the marker has it removed. A stream that does not (a
// login shell's rc file printing first, or a forced command) passes through
// unchanged and still counts as started. A marker split across reads is
// held until the next chunk decides which of the two it is.
type StartScan struct {
	// state is the stream so far: still a prefix of the marker, or decided.
	state ScanState
	// held is the bytes held while they could still be the start of the
	// marker.
	held    []byte
	started bool
}

// ScanState is whether a StartScan is still matching the marker.
type ScanState int

const (
	ScanStateMatching ScanState = iota
	ScanStatePassing
)

// NewStartScan starts matching the marker.
func NewStartScan() *StartScan {
	return &StartScan{state: ScanStateMatching}
}

// Push feeds one chunk. The bytes to pass on are appended to out. Returns
// true the first time a byte arrives, which is the session's start.
func (s *StartScan) Push(chunk []byte, out *[]byte) bool {
	if len(chunk) == 0 {
		return false
	}
	first := !s.started
	s.started = true
	switch s.state {
	case ScanStatePassing:
		*out = append(*out, chunk...)
	case ScanStateMatching:
		s.held = append(s.held, chunk...)
		marker := []byte(SSHStartMarker)
		compared := min(len(s.held), len(marker))
		if !bytes.Equal(s.held[:compared], marker[:compared]) {
			s.state = ScanStatePassing
			*out = append(*out, s.held...)
			s.held = nil
		} else if len(s.held) >= len(marker) {
			s.state = ScanStatePassing
			*out = append(*out, s.held[len(marker):]...)
			s.held = nil
		}
	}
	return first
}

// Finish: the stream ended. Bytes held as a possible marker were ordinary
// output.
func (s *StartScan) Finish(out *[]byte) {
	*out = append(*out, s.held...)
	s.held = nil
	s.state = ScanStatePassing
}

// StartPhase is where one SSH session stands: Waiting until its stdout shows
// a byte, Started after that, Ended when stdout closed without any.
type StartPhase int

const (
	StartPhaseWaiting StartPhase = iota
	StartPhaseStarted
	StartPhaseEnded
)

// StartWait is how a blocking wait on a StartGate ended.
type StartWait int

const (
	StartWaitStarted StartWait = iota
	StartWaitEnded
	StartWaitCancelled
)

// StartGate is the owner of the fact "this SSH session started its command",
// one per lease. The reader of the session's stdout reports into it; the
// writer of its stdin, the retry decision and the ledger ask it.
type StartGate struct {
	mu    sync.Mutex
	phase StartPhase
	// settled is closed when the phase leaves Waiting.
	settled chan struct{}
	confirm *SessionConfirm
}

// StartGateStandalone is a gate no lease owns: the start is known, but no
// ledger counts it.
func StartGateStandalone() *StartGate {
	return newStartGate(nil)
}

func newStartGate(confirm *SessionConfirm) *StartGate {
	return &StartGate{phase: StartPhaseWaiting, settled: make(chan struct{}), confirm: confirm}
}

// Phase is where the session stands.
func (g *StartGate) Phase() StartPhase {
	g.mu.Lock()
	defer g.mu.Unlock()
	return g.phase
}

func (g *StartGate) settle(next StartPhase) bool {
	g.mu.Lock()
	defer g.mu.Unlock()
	if g.phase != StartPhaseWaiting {
		return false
	}
	g.phase = next
	close(g.settled)
	return true
}

// MarkStarted: the session's stdout produced its first byte: the command's
// shell ran. The lease becomes an open session in the ledger.
func (g *StartGate) MarkStarted() {
	if g.settle(StartPhaseStarted) && g.confirm != nil {
		g.confirm.confirm()
	}
}

// MarkEnded: the session's stdout closed. Without an earlier byte, the
// session never started.
func (g *StartGate) MarkEnded() {
	g.settle(StartPhaseEnded)
}

// WaitBlocking waits until the session has started or ended, polling
// cancelled.
func (g *StartGate) WaitBlocking(cancelled func() bool) StartWait {
	for {
		switch g.Phase() {
		case StartPhaseStarted:
			return StartWaitStarted
		case StartPhaseEnded:
			return StartWaitEnded
		}
		if cancelled() {
			return StartWaitCancelled
		}
		timer := time.NewTimer(25 * time.Millisecond)
		select {
		case <-g.settled:
		case <-timer.C:
		}
		timer.Stop()
	}
}

// Wait waits until the session has started (true) or ended without starting
// or ctx ended.
func (g *StartGate) Wait(ctx context.Context) bool {
	select {
	case <-g.settled:
		return g.Phase() == StartPhaseStarted
	case <-ctx.Done():
		return false
	}
}

// Refusal reports whether the server turned the session away. A session
// that printed a byte started, so its exit status is its own whatever stderr
// says. One that printed nothing, including one that never carried the
// marker (`scp`), is judged by the exit status and stderr alone.
func (g *StartGate) Refusal(status int, stderr string) (SshRefusal, bool) {
	if g.Phase() == StartPhaseStarted {
		return 0, false
	}
	return SSHRefusal(status, stderr)
}

// masterCheckInterval is how long a successful `ssh -O check` of a master is
// trusted before the next lease on that shard checks again.
const masterCheckInterval = 5 * time.Second

// SSHMasterOpenTimeout is the per-command deadline for checking or opening a
// master from code that has no executor of its own, such as the relay and
// the resource pollers.
const SSHMasterOpenTimeout = 60 * time.Second

// SshWalk is one command's walk across the shards of a connection.
//
// The server decides how many sessions a master accepts, and other
// processes share the same control sockets, so no count kept here can say
// whether a shard has room. A command instead tries shards in order: when
// the server refuses its session on one (`MaxSessions`), the command's next
// lease takes the next shard it has not tried, whoever opened that shard.
// When it has tried every shard that exists, it opens a new one, unless it
// already opened a master during this walk: then the server is refusing
// masters one after another, and the command retries the shard it tried
// least recently, after the wait SshRefusalRetry decides, within the
// persistent-refusal bound. Each retry first looks for shards added
// meanwhile. The walk is the state change that ends the retry loop, so a
// refusal that has a shard to go to needs no wait and spends no time.
// Placement is the one rule, shared by the lease and by the question
// whether a refusal can walk on.
//
// One command owns one walk (see SshRefusalRetry.Walk) and passes it to
// each lease it takes.
type SshWalk struct {
	// start is where the walk started: the shard that most recently started
	// a session for the connection. Only a starting point; a stale one costs
	// extra refusals and never a failure.
	start *int
	// visited is the shards this command has leased, least recently tried
	// first, whether or not they accepted it.
	visited []int
	// opened is the shards whose master this command opened.
	opened []int
	// connection is the connection the walk is on, once it has leased a
	// shard.
	connection *WalkedConnection
}

// WalkedConnection is the ledger and connection key of the shards a walk
// goes over.
type WalkedConnection struct {
	ledger *SessionLedger
	key    string
}

// PlacementKind names a Placement variant.
type PlacementKind int

const (
	// PlacementUntried: a shard the command has not tried.
	PlacementUntried PlacementKind = iota
	// PlacementOpenNew: every shard has been tried and the command opened no
	// master yet: add a shard and open its master.
	PlacementOpenNew
	// PlacementRetryOldest: every shard has been tried and the command
	// already opened a master: the shard it tried least recently.
	PlacementRetryOldest
)

// Placement is where a walk's next lease goes. Shard is set for Untried and
// RetryOldest.
type Placement struct {
	Kind  PlacementKind
	Shard int
}

func placementOf(connection *Connection, walk *SshWalk) Placement {
	count := len(connection.shards)
	start := connection.hint
	if walk.start != nil {
		start = *walk.start
	}
	for offset := range count {
		shard := (start + offset) % count
		if !slices.Contains(walk.visited, shard) {
			return Placement{Kind: PlacementUntried, Shard: shard}
		}
	}
	if len(walk.visited) > 0 && len(walk.opened) > 0 {
		return Placement{Kind: PlacementRetryOldest, Shard: walk.visited[0]}
	}
	return Placement{Kind: PlacementOpenNew}
}

// canWalkOn reports whether the next lease of this walk goes to a shard that
// can answer a session the server refused for `MaxSessions` at once: one the
// command has not tried, or a new one. Looked up when asked, so shards added
// since the last lease count.
func (w *SshWalk) canWalkOn() bool {
	if w.connection == nil {
		return false
	}
	connections, unlock := w.connection.ledger.lockConnections()
	defer unlock()
	connection, ok := connections[w.connection.key]
	return ok && placementOf(connection, w).Kind != PlacementRetryOldest
}

// RetryLastShard: the command's last lease cannot be answered by another
// shard: a connection dropped before authentication usually means the
// master died. The retry takes that shard again; its lease checks the master
// and reopens it if needed. A retry after a failure that was no refusal (the
// session ran, so the shard accepted it) takes it again too, instead of
// advancing the walk.
func (w *SshWalk) RetryLastShard() {
	if len(w.visited) > 0 {
		w.visited = w.visited[:len(w.visited)-1]
	}
}

// Shard is one master connection this process knows about: when its master
// was last found alive. How many sessions it carries is the server's to say.
type Shard struct {
	// verifiedAt is when `ssh -O check` last found this shard's master
	// running.
	verifiedAt *time.Time
	// opening serializes checking and opening this shard's master, so
	// concurrent leases never start two openers for one socket.
	opening *OpeningLock
}

func newShard() *Shard {
	return &Shard{opening: &OpeningLock{}}
}

// OpeningLock is ownership of one shard's check-and-open step within this
// process. The record of who holds it is the lock itself: taking it sets the
// holder and releasing the guard on any path clears it.
type OpeningLock struct {
	mu     sync.Mutex
	holder *OpeningHolder
}

// OpeningHolder is who holds an OpeningLock, and since when.
type OpeningHolder struct {
	thread string
	since  time.Time
}

// OpeningGuard holds an OpeningLock until release (Rust: Drop).
type OpeningGuard struct {
	lock *OpeningLock
}

func (l *OpeningLock) tryLock() *OpeningGuard {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.holder != nil {
		return nil
	}
	l.holder = &OpeningHolder{thread: currentThreadName(), since: time.Now()}
	return &OpeningGuard{lock: l}
}

func (l *OpeningLock) describeHolder() string {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.holder == nil {
		return "released just now"
	}
	return fmt.Sprintf("thread %s held for %ds", l.holder.thread, uint64(time.Since(l.holder.since)/time.Second))
}

func (g *OpeningGuard) release() {
	g.lock.mu.Lock()
	g.lock.holder = nil
	g.lock.mu.Unlock()
}

// currentThreadName names the caller for lock-wait reports. Go goroutines
// have no names.
func currentThreadName() string {
	return "goroutine"
}

// Connection is the shards of one connection and where walks start.
type Connection struct {
	shards []*Shard
	// hint is the shard that most recently started a session.
	hint int
}

// Reservation is a shard a lease was placed on.
type Reservation struct {
	shard   int
	opening *OpeningLock
}

// SessionLedger is this process's view of the shards of each connection,
// keyed by connection. It records which shards exist and whether their
// masters are known to be alive, nothing about how full they are.
type SessionLedger struct {
	mu          sync.Mutex
	connections map[string]*Connection
}

func newSessionLedger() *SessionLedger {
	return &SessionLedger{connections: map[string]*Connection{}}
}

var (
	sessionLedgerOnce     sync.Once
	sessionLedgerInstance *SessionLedger
)

func sessionLedgerGlobal() *SessionLedger {
	sessionLedgerOnce.Do(func() { sessionLedgerInstance = newSessionLedger() })
	return sessionLedgerInstance
}

// lockConnections locks the ledger and returns its connections with the
// unlock function (Rust: SessionLedger::connections's guard).
func (l *SessionLedger) lockConnections() (map[string]*Connection, func()) {
	l.mu.Lock()
	return l.connections, l.mu.Unlock
}

// reserve chooses the shard for walk's next lease by Placement. This is the
// only placement rule, and the ledger's lock makes it one decision.
func (l *SessionLedger) reserve(key string, walk *SshWalk) Reservation {
	if walk.connection == nil {
		walk.connection = &WalkedConnection{ledger: l, key: key}
	}
	connections, unlock := l.lockConnections()
	defer unlock()
	connection, ok := connections[key]
	if !ok {
		connection = &Connection{}
		connections[key] = connection
	}
	if walk.start == nil {
		hint := connection.hint
		walk.start = &hint
	}
	var index int
	switch placement := placementOf(connection, walk); placement.Kind {
	case PlacementUntried:
		index = placement.Shard
	case PlacementOpenNew:
		connection.shards = append(connection.shards, newShard())
		index = len(connection.shards) - 1
	case PlacementRetryOldest:
		walk.visited = slices.DeleteFunc(walk.visited, func(visited int) bool { return visited == placement.Shard })
		index = placement.Shard
	}
	walk.visited = append(walk.visited, index)
	return Reservation{shard: index, opening: connection.shards[index].opening}
}

func (l *SessionLedger) slot(key string, reservation *Reservation, socket string) *LeasedSlot {
	return &LeasedSlot{ledger: l, key: key, shard: reservation.shard, socket: socket}
}

// lease leases a session on walk's next shard, opening that shard's master
// first when it is not known to be running.
func (l *SessionLedger) lease(ssh *SshTarget, class SshTrafficClass, dir string, executor CommandExecutor, walk *SshWalk) (*SshSessionLease, error) {
	key := connectionKey(ssh, class)
	reservation := l.reserve(key, walk)
	slot := l.slot(key, &reservation, filepath.Join(dir, controlSocketName(ssh, class, reservation.shard)))
	if slot.needsCheck() {
		opening, err := takeOpening(ssh, &reservation, executor)
		if err != nil {
			return nil, err
		}
		defer opening.release()
		// Another lease may have checked or opened the master while this one
		// waited for the lock.
		if slot.needsCheck() {
			opened, err := ensureMaster(ssh, slot.socket, executor)
			if err != nil {
				return nil, err
			}
			if opened {
				walk.opened = append(walk.opened, reservation.shard)
			}
			now := time.Now()
			slot.setVerified(&now)
		}
	}
	return newSshSessionLease(slot, false), nil
}

// leaseProbe places a fail-fast probe on walk's next shard, without
// checking or opening that shard's master.
//
// The probe joins the master when it is up and otherwise connects on its
// own, so its short timeouts never become a master's settings. It uses one
// of the master's sessions while it runs, so the server may refuse it like
// any other session; the walk then moves it on. Past the last known shard it
// takes the next index, which joins that shard's master if some process
// opened it and otherwise connects directly.
func (l *SessionLedger) leaseProbe(ssh *SshTarget, dir string, walk *SshWalk) *SshSessionLease {
	key := connectionKey(ssh, SshTrafficClassControl)
	reservation := l.reserve(key, walk)
	socket := filepath.Join(dir, controlSocketName(ssh, SshTrafficClassControl, reservation.shard))
	return newSshSessionLease(l.slot(key, &reservation, socket), true)
}

// ensureMaster makes sure a master is listening on socket, opening one if
// needed. Returns whether this call opened it.
//
// The master is opened explicitly, with `ControlMaster=yes`, and then
// checked again. Nothing else is attempted when that fails: the caller gets
// an error naming the destination instead of a direct connection. The master
// is not tried with a session here: other processes join a published
// control socket without these locks, so a refusal of such a session could
// come from their sessions instead of from the server's `MaxSessions`.
func ensureMaster(ssh *SshTarget, socket string, executor CommandExecutor) (bool, error) {
	// Another process of this instance (the old daemon during a restart) may
	// be checking and opening the same socket. Without this lock both find no
	// master and both open one; the second finds the socket bound, prints
	// "already exists, disabling multiplexing", and keeps a plain background
	// connection that no ControlPersist ever closes (J-18). The lock also
	// keeps one process from removing, as stale, a socket the other has just
	// bound.
	lock, err := lockMasterOpeningUnless(socket, executor)
	if err != nil {
		return false, err
	}
	defer releaseMasterLock(lock)
	running, err := masterRunning(ssh, socket, executor)
	if err != nil {
		return false, err
	}
	if running {
		return false, nil
	}
	// A master that died without cleaning up leaves its socket behind, and
	// `ssh` will not bind over it: the opener would print "already exists,
	// disabling multiplexing" and hold a plain connection instead.
	if err := os.Remove(socket); err == nil {
		slog.Debug("removed a stale SSH control socket", "socket", socket)
	} else if !errors.Is(err, fs.ErrNotExist) {
		return false, fmt.Errorf("remove stale SSH control socket %s: %w", socket, err)
	}
	command := masterOpenCommand(ssh, socket)
	opened, err := executor.Execute(&command)
	if err != nil {
		return false, err
	}
	running, err = masterRunning(ssh, socket, executor)
	if err != nil {
		return false, err
	}
	if running {
		slog.Info("opened a shared SSH connection", "destination", ssh.Destination, "socket", socket)
		return true, nil
	}
	detail := strings.TrimSpace(sshLossy(opened.Stderr))
	if detail == "" {
		detail = fmt.Sprintf("ssh exited with status %d", opened.Status)
	}
	return false, fmt.Errorf("could not open a shared SSH connection to %s: %s", ssh.Destination, detail)
}

// lockMasterOpeningUnless takes the cross-process lock file beside socket,
// waiting (and reporting the wait as a stage to executor, when given) while
// another process holds it, unless executor is cancelled.
func lockMasterOpeningUnless(socket string, executor CommandExecutor) (*FileLock, error) {
	path := socket + ".lock"
	var wait *LockWait
	var queued *ProvisionStageGuard
	defer func() {
		if wait != nil {
			// Does nothing once the wait was acquired.
			wait.Drop()
		}
		if queued != nil {
			queued.End()
		}
	}()
	for {
		if executor != nil && executor.CancellationRequested() {
			return nil, errors.New("operation cancelled while waiting for SSH master lock")
		}
		file, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY, 0o666)
		if err != nil {
			return nil, fmt.Errorf("open SSH master lock %s: %w", path, err)
		}
		locked, err := sshTryLockFile(file)
		if err != nil {
			file.Close()
			return nil, fmt.Errorf("lock SSH master lock %s: %w", path, err)
		}
		if !locked {
			file.Close()
			// The lock file is empty, so it cannot name its holder: that is
			// another process, or another open of the file here.
			if wait == nil {
				wait = NewLockWait("an SSH master file lock")
				if executor != nil {
					queued = NewProvisionStageGuard(executor, ProvisionStage{Kind: ProvisionStageWaitingForSshConnection})
				}
			}
			wait.Blocked(path, "thread "+currentThreadName(), func() string {
				return "held by another process (the lock file records no pid)"
			})
			time.Sleep(25 * time.Millisecond)
			continue
		}
		lock := NewFileLock(file)
		// The daemon removes stale locks when it starts. A lock taken on a
		// file removed meanwhile serializes nothing, so take the one at the
		// path now.
		if isFileAt(file, path) {
			if wait != nil {
				wait.Acquired()
			}
			return lock, nil
		}
		releaseMasterLock(lock)
	}
}

// removeStaleMasterLocksIn removes lock files, and the sockets of masters
// that are no longer running, that no process holds in dir.
func removeStaleMasterLocksIn(dir string) {
	entries, err := os.ReadDir(dir)
	if err != nil {
		slog.Debug("cannot list SSH master locks", "directory", dir, "error", err)
		return
	}
	// Both generations use <16 hex digits>-<shard>. The class is in the
	// hash, so discovery does not need a target configuration or a class.
	found := map[string]struct{}{}
	for _, entry := range entries {
		name := strings.TrimSuffix(entry.Name(), ".lock")
		hash, shard, ok := strings.Cut(name, "-")
		if ok && utf8.ValidString(name) && len(hash) == connectionHashHex && isSSHASCIIHex(hash) &&
			shard != "" && strings.Trim(shard, "0123456789") == "" {
			found[filepath.Join(dir, name)] = struct{}{}
		}
	}
	sockets := make([]string, 0, len(found))
	for socket := range found {
		sockets = append(sockets, socket)
	}
	slices.Sort(sockets)
	for _, socket := range sockets {
		removeStaleMasterLock(socket)
	}
}

// isFileAt reports whether the open file is still the file at path.
func isFileAt(file *os.File, path string) bool {
	open, err := file.Stat()
	if err != nil {
		return false
	}
	named, err := os.Stat(path)
	if err != nil {
		return false
	}
	return os.SameFile(open, named)
}

func masterRunning(ssh *SshTarget, socket string, executor CommandExecutor) (bool, error) {
	command := masterCheckCommand(ssh, socket)
	output, err := executor.Execute(&command)
	if err != nil {
		return false, err
	}
	return output.Status == 0, nil
}

func masterCheckCommand(ssh *SshTarget, socket string) CommandSpec {
	args := append([]string{}, ssh.SSHArgs...)
	args = append(args, "-o", controlPathOption(socket), "-O", "check", ssh.Destination)
	return NewCommandSpec("ssh", args).WithPurpose("check a shared SSH connection")
}

func masterOpenCommand(ssh *SshTarget, socket string) CommandSpec {
	args := append([]string{}, ssh.SSHArgs...)
	args = append(args,
		"-o", "BatchMode=yes",
		"-o", "ConnectTimeout=10",
		"-o", fmt.Sprintf("ServerAliveInterval=%d", masterKeepalive[0]),
		"-o", fmt.Sprintf("ServerAliveCountMax=%d", masterKeepalive[1]),
		"-f", "-N",
		"-o", "ControlMaster=yes",
		"-o", controlPathOption(socket),
		"-o", "ControlPersist="+controlPersist,
		ssh.Destination,
	)
	return NewCommandSpec("ssh", args).WithSSHDestination(ssh.Destination).WithPurpose("open a shared SSH connection")
}

// LeasedSlot is the shard a lease holds.
type LeasedSlot struct {
	ledger *SessionLedger
	key    string
	shard  int
	socket string
}

// SessionConfirm moves the connection's walk start to a shard whose session
// started. It does nothing without connection sharing.
type SessionConfirm struct {
	ledger *SessionLedger
	key    string
	shard  int
}

func (c *SessionConfirm) confirm() {
	if c.ledger == nil {
		return
	}
	connections, unlock := c.ledger.lockConnections()
	defer unlock()
	if connection, ok := connections[c.key]; ok {
		connection.hint = c.shard
	}
}

func (s *LeasedSlot) needsCheck() bool {
	connections, unlock := s.ledger.lockConnections()
	defer unlock()
	connection, ok := connections[s.key]
	if !ok || s.shard >= len(connection.shards) {
		return true
	}
	verified := connection.shards[s.shard].verifiedAt
	return verified == nil || time.Since(*verified) >= masterCheckInterval
}

func (s *LeasedSlot) setVerified(verifiedAt *time.Time) {
	connections, unlock := s.ledger.lockConnections()
	defer unlock()
	if connection, ok := connections[s.key]; ok && s.shard < len(connection.shards) {
		connection.shards[s.shard].verifiedAt = verifiedAt
	}
}

func (s *LeasedSlot) confirmHandle() *SessionConfirm {
	return &SessionConfirm{ledger: s.ledger, key: s.key, shard: s.shard}
}

// SshSessionLease is one session on a shared SSH connection, or an unshared
// lease when sharing is off.
type SshSessionLease struct {
	slot  *LeasedSlot
	probe bool
	gate  *StartGate
}

func newSshSessionLease(slot *LeasedSlot, probe bool) *SshSessionLease {
	var confirm *SessionConfirm
	if slot != nil {
		confirm = slot.confirmHandle()
	}
	return &SshSessionLease{slot: slot, probe: probe, gate: newStartGate(confirm)}
}

func sshSessionLeaseUnshared() *SshSessionLease {
	return &SshSessionLease{gate: newStartGate(nil)}
}

// Gate is the lease's start gate.
func (l *SshSessionLease) Gate() *StartGate { return l.gate }

// ControlPath is the control socket the session binds to, when it shares a
// connection.
func (l *SshSessionLease) ControlPath() (string, bool) {
	if l.slot == nil {
		return "", false
	}
	return l.slot.socket, true
}

// Invalidate forgets that the lease's master was verified, so the next lease
// checks it.
func (l *SshSessionLease) Invalidate() {
	if l.slot != nil {
		l.slot.setVerified(nil)
	}
}

// NoteRefusal tells the ledger what refusal means and returns whether
// retrying at once can land on another shard.
func (l *SshSessionLease) NoteRefusal(refusal SshRefusal) bool {
	if refusal == SshRefusalBeforeAuthentication {
		l.Invalidate()
		return false
	}
	_, ok := l.ControlPath()
	return ok
}

// SshSessions is the process's shared SSH connections.
type SshSessions struct{}

// SshSessionsLease leases a session on ssh's shared connection (Rust:
// SshSessions::lease).
func SshSessionsLease(ssh *SshTarget, class SshTrafficClass, executor CommandExecutor, walk *SshWalk) (*SshSessionLease, error) {
	if runtime.GOOS == "windows" || userConfiguresSharing(ssh.SSHArgs) {
		return sshSessionLeaseUnshared(), nil
	}
	dir, ok := controlSocketDir()
	if !ok {
		return sshSessionLeaseUnshared(), nil
	}
	return sessionLedgerGlobal().lease(ssh, class, dir, executor, walk)
}

// SshSessionsRemoveStaleMasterLocks sweeps this instance's control
// directory (Rust: SshSessions::remove_stale_master_locks).
func SshSessionsRemoveStaleMasterLocks() {
	if runtime.GOOS == "windows" {
		return
	}
	if dir, ok := controlSocketDir(); ok {
		removeStaleMasterLocksIn(dir)
	}
}

// SshSessionsLeaseProbe places a fail-fast probe on ssh's shared connection
// (Rust: SshSessions::lease_probe).
func SshSessionsLeaseProbe(ssh *SshTarget, walk *SshWalk) *SshSessionLease {
	if runtime.GOOS == "windows" || userConfiguresSharing(ssh.SSHArgs) {
		return sshSessionLeaseUnshared()
	}
	dir, ok := controlSocketDir()
	if !ok {
		return sshSessionLeaseUnshared()
	}
	return sessionLedgerGlobal().leaseProbe(ssh, dir, walk)
}

// PushSessionArgs appends the options that bind a command to the lease's
// master, when it has one.
func PushSessionArgs(args *[]string, lease *SshSessionLease) {
	if socket, ok := lease.ControlPath(); ok {
		pushSessionOptions(args, socket, lease.probe)
	}
}

func pushSessionOptions(args *[]string, socket string, probe bool) {
	*args = append(*args, "-o", "ControlMaster=no", "-o", controlPathOption(socket))
	if !probe {
		*args = append(*args, "-o", "ProxyCommand=false")
	}
}

// SessionCommandArgs is the command's arguments with the lease's session
// options in front.
func SessionCommandArgs(program string, args []string, ssh *SshTarget, lease *SshSessionLease) []string {
	session := make([]string, 0, len(args)+6)
	PushSessionArgs(&session, lease)
	if len(session) == 0 {
		return append([]string{}, args...)
	}
	appendSessionCommand(&session, program, args, ssh)
	return session
}

// appendSessionCommand appends the command's arguments after the session
// options, spelling a user's `-J` jump host as `-o ProxyJump=` so it is not
// mistaken for a session option.
func appendSessionCommand(session *[]string, program string, args []string, ssh *SshTarget) {
	if program == "ssh" && len(args) >= len(ssh.SSHArgs) && slices.Equal(args[:len(ssh.SSHArgs)], ssh.SSHArgs) {
		user, rest := args[:len(ssh.SSHArgs)], args[len(ssh.SSHArgs):]
		for index := 0; index < len(user); index++ {
			argument := user[index]
			jump, ok := strings.CutPrefix(argument, "-J")
			switch {
			case !ok:
				*session = append(*session, argument)
			case jump == "":
				if index+1 < len(user) {
					index++
					*session = append(*session, "-o", "ProxyJump="+user[index])
				} else {
					*session = append(*session, argument)
				}
			default:
				*session = append(*session, "-o", "ProxyJump="+jump)
			}
		}
		*session = append(*session, rest...)
	} else {
		*session = append(*session, args...)
	}
}

// sshRetryBackoffMs is the jittered backoff (low, high milliseconds) of the
// first and every later wait.
var sshRetryBackoffMs = [2][2]uint64{{500, 2_000}, {2_000, 4_000}}

// SetSSHRetryBackoffForTest fixes the retry backoff; nil restores it. Not
// part of the daemon's behaviour.
func SetSSHRetryBackoffForTest(delay *time.Duration) {
	if delay == nil {
		sshRetryBackoffOverrideMs.Store(math.MaxUint64)
		return
	}
	sshRetryBackoffOverrideMs.Store(uint64(delay.Milliseconds()))
}

// SSHRetryDelay is the jittered wait before retry attemptsMade.
func SSHRetryDelay(attemptsMade int) time.Duration {
	if override := sshRetryBackoffOverrideMs.Load(); override != math.MaxUint64 {
		return time.Duration(override) * time.Millisecond
	}
	index := min(max(attemptsMade-1, 0), len(sshRetryBackoffMs)-1)
	low, high := sshRetryBackoffMs[index][0], sshRetryBackoffMs[index][1]
	// A failed draw only costs jitter, so fall back to the lower bound.
	var spread uint64
	var random [8]byte
	if _, err := rand.Read(random[:]); err == nil {
		spread = binary.LittleEndian.Uint64(random[:]) % (high - low + 1)
	}
	return time.Duration(low+spread) * time.Millisecond
}

// takeOpening takes reservation's in-process opening lock, polling while
// another lease holds it. A lease that has to queue reports it as a stage,
// so a lifecycle watching for stalls does not count the queue.
func takeOpening(ssh *SshTarget, reservation *Reservation, executor CommandExecutor) (*OpeningGuard, error) {
	var wait *LockWait
	var queued *ProvisionStageGuard
	defer func() {
		if wait != nil {
			// Does nothing once the wait was acquired.
			wait.Drop()
		}
		if queued != nil {
			queued.End()
		}
	}()
	for {
		if executor.CancellationRequested() {
			return nil, errors.New("operation cancelled while waiting for SSH master")
		}
		if guard := reservation.opening.tryLock(); guard != nil {
			if wait != nil {
				wait.Acquired()
			}
			return guard, nil
		}
		if wait == nil {
			wait = NewLockWait("an SSH master")
			queued = NewProvisionStageGuard(executor, ProvisionStage{Kind: ProvisionStageWaitingForSshConnection})
		}
		wait.Blocked(fmt.Sprintf("%s shard %d", ssh.Destination, reservation.shard),
			"thread "+currentThreadName(), reservation.opening.describeHolder)
		time.Sleep(25 * time.Millisecond)
	}
}

// releaseMasterLock unlocks a master lock file and closes it, as dropping
// Rust's FileLock does.
func releaseMasterLock(lock *FileLock) {
	file := lock.File()
	lock.Drop()
	file.Close()
}

// removeStaleMasterLock removes socket, when no master answers on it, and
// then its lock file, unless another process holds the lock or the lock file
// was replaced. See removeStaleMasterLocksIn.
func removeStaleMasterLock(socket string) {
	lockPath := socket + ".lock"
	file, err := os.OpenFile(lockPath, os.O_CREATE|os.O_WRONLY, 0o666)
	if err != nil {
		return
	}
	// Held: another process is checking or opening this master now.
	if locked, err := sshTryLockFile(file); err != nil || !locked {
		file.Close()
		return
	}
	lock := NewFileLock(file)
	defer releaseMasterLock(lock)
	if !isFileAt(file, lockPath) {
		return
	}
	if _, err := os.Lstat(socket); err == nil {
		// -O check only contacts the local socket; it never connects to this
		// placeholder destination. This also finds old-hash masters still
		// serving workers during daemon handoff.
		ssh := SshTarget{Destination: "mj-stale-master-check", SSHArgs: []string{"-F", "/dev/null"}}
		running, err := masterRunning(&ssh, socket, ProcessExecutor{})
		if err != nil {
			slog.Warn("cannot check stale SSH master", "socket", socket, "error", err)
			return
		}
		if running {
			return
		}
		if err := os.Remove(socket); err != nil {
			slog.Debug("cannot remove stale SSH master socket", "socket", socket, "error", err)
			return
		}
	}
	if err := os.Remove(lockPath); err != nil {
		slog.Debug("cannot remove a stale SSH master lock", "lock", lockPath, "error", err)
	} else {
		slog.Debug("removed a stale SSH master lock", "lock", lockPath)
	}
}

// sshLossy is Rust's String::from_utf8_lossy.
func sshLossy(output []byte) string {
	return strings.ToValidUTF8(string(output), "�")
}

// sshASCIILower is Rust's str::to_ascii_lowercase.
func sshASCIILower(value string) string {
	return strings.Map(func(character rune) rune {
		if character >= 'A' && character <= 'Z' {
			return character + ('a' - 'A')
		}
		return character
	}, value)
}

// sshEqualFoldASCII is Rust's str::eq_ignore_ascii_case.
func sshEqualFoldASCII(left, right string) bool {
	return len(left) == len(right) && sshASCIILower(left) == sshASCIILower(right)
}

func isSSHASCIILetter(character byte) bool {
	return (character >= 'a' && character <= 'z') || (character >= 'A' && character <= 'Z')
}

// isSSHASCIIHex reports whether every byte of value is an ASCII hex digit.
func isSSHASCIIHex(value string) bool {
	for index := 0; index < len(value); index++ {
		character := value[index]
		if !((character >= '0' && character <= '9') || (character >= 'a' && character <= 'f') || (character >= 'A' && character <= 'F')) {
			return false
		}
	}
	return true
}

// sshPathSplit splits a path at its separators, dropping empty parts: `/`
// everywhere, and `\` too on Windows, as Rust's Path does.
func sshPathSplit(path string) []string {
	return strings.FieldsFunc(path, func(character rune) bool {
		return character == '/' || (runtime.GOOS == "windows" && character == '\\')
	})
}

// sshPathComponents is the parts of path as Rust's Path::components names
// them for the checks here: a `.` is kept only as the first part of a
// relative path.
func sshPathComponents(path string) []string {
	relative := !filepath.IsAbs(path) && !strings.HasPrefix(path, "/")
	var components []string
	for index, part := range sshPathSplit(path) {
		if part == "." && !(index == 0 && relative) {
			continue
		}
		components = append(components, part)
	}
	return components
}
