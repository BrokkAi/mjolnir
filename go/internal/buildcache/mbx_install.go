// Port of mj-build-cache/src/mbx/install.rs.
//
// Install the pinned native mbx binary on a local or SSH container host.
//
// Rust's test-only install_mbx_with lives in this file's tests.

package buildcache

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const mbxX8664SHA256 = "b523a9324d50fd434e355fc7a5af5f7155d62854dd6a5a74173d52655f168d45"
const mbxAarch64SHA256 = "900844b980f9e3daf778d38a19046281cc35e07108a9c821befef6130846df1f"

const profileScript = `set -eu
login_shell=
if command -v getent >/dev/null 2>&1 && command -v id >/dev/null 2>&1; then
    login_name=$(id -un 2>/dev/null || true)
    if [ -n "$login_name" ]; then
        login_shell=$(getent passwd "$login_name" 2>/dev/null | awk -F: 'NR == 1 { print $7; exit }' || true)
    fi
fi
[ -n "$login_shell" ] || login_shell=${SHELL:-sh}
shell_name=${login_shell##*/}
[ -n "$shell_name" ] || shell_name=sh
case "$shell_name" in
    zsh) profile=$HOME/.zprofile; shown='~/.zprofile'; shell_kind=posix ;;
    bash)
        if [ -e "$HOME/.bash_profile" ]; then
            profile=$HOME/.bash_profile; shown='~/.bash_profile'
        elif [ -e "$HOME/.bash_login" ]; then
            profile=$HOME/.bash_login; shown='~/.bash_login'
        else
            profile=$HOME/.profile; shown='~/.profile'
        fi
        shell_kind=posix
        ;;
    fish)
        config_home=${XDG_CONFIG_HOME:-$HOME/.config}
        case "$config_home" in /*) ;; *) config_home=$HOME/.config ;; esac
        config_home=${config_home%/}
        [ -n "$config_home" ] || config_home=/
        profile=$config_home/fish/conf.d/mbx.fish
        case "$profile" in
            "$HOME"/*) shown="~/${profile#"$HOME"/}" ;;
            *) shown=$profile ;;
        esac
        shell_kind=fish
        ;;
    *) profile=$HOME/.profile; shown='~/.profile'; shell_kind=unknown ;;
esac
binary_dir=$2
data_home=${XDG_DATA_HOME:-$HOME/.local/share}
case "$data_home" in /*) ;; *) data_home=$HOME/.local/share ;; esac
data_home=${data_home%/}
[ -n "$data_home" ] || data_home=/
shim_dir=$data_home/mbx/bin
action=$1
if [ "$action" = preview ]; then
    printf 'preview\n%s\n%s\n%s' "$shown" "$shell_kind" "$shim_dir"
    exit 0
fi
[ "$action" = update ] || { echo 'unknown profile action' >&2; exit 2; }
posix_block=$3
fish_block=$4
# Local callers hold this same host lock through Rust; SSH callers acquire it here.
lock=${5-"$HOME/.mbx.lock"}
if [ -n "$lock" ]; then
    exec 9>"$lock"
    flock -x 9
fi
if [ "$shell_kind" = fish ]; then block=$fish_block; else block=$posix_block; fi
existing=
if [ -e "$profile" ]; then
    # A sentinel preserves trailing newlines for exact owned-prefix comparisons.
    existing=$(cat -- "$profile" && printf '.')
    existing=${existing%.}
fi
start='# >>> mbx (added by Mjolnir) >>>'
end='# <<< mbx <<<'
# Inspect only the suffix after the last completed block.
trailing=${existing##*"$end"}
case "$trailing" in
    *"$start"*)
        fragment="$start${trailing#*"$start"}"
        prefix=${existing%"$fragment"}
        offset=$(printf '%s' "$prefix" | wc -c)
        line=$(printf '%s' "$prefix" | wc -l)
        line=$((line + 1))
        # Byte comparisons also handle a write interrupted halfway through a line.
        length=$(printf '%s' "$fragment" | wc -c)
        matched=$(printf '%s' "$fragment" | MBX_PROFILE_BLOCK="$block" MBX_PROFILE_LENGTH="$length" LC_ALL=C awk '
            { actual=actual $0 "\n" }
            END {
                actual=substr(actual, 1, ENVIRON["MBX_PROFILE_LENGTH"]+0)
                expected=ENVIRON["MBX_PROFILE_BLOCK"]
                n=length(actual); if (length(expected)<n) n=length(expected)
                for (i=1; i<=n && substr(actual,i,1)==substr(expected,i,1); i++) {}
                print i-1
            }')
        if [ "$matched" -ne "$length" ]; then
            first=$((offset + 1))
            last=$((offset + matched))
            printf '%s\n' "$profile: incomplete Mj block at line $line is followed by other content. Delete only bytes $first through $last (the incomplete Mj block prefix beginning with '$start'); preserve subsequent content, then run Install again." >&2
            exit 1
        fi
        # dd preserves the prefix and truncates in place at the seek offset,
        # following symlinks. No copy or rename can replace unrelated contents.
        if ! error=$(dd if=/dev/null of="$profile" bs=1 seek="$offset" 2>&1); then
            printf '%s\n' "repair $profile at line $line failed: $error" >&2
            exit 1
        fi
        existing=$prefix
        ;;
esac
case "$existing" in
    *"$block"*) status=unchanged ;;
    *)
        profile_dir=${profile%/*}
        mkdir -p -- "$profile_dir"
        # One append preserves user bytes and follows profile symlinks.
        block="
$block
"
        if ! printf '%s' "$block" >> "$profile"; then
            printf '%s\n' "append $profile failed with an I/O error; run Install again to repair the incomplete Mj block" >&2
            exit 1
        fi
        status=changed
        ;;
esac
printf '%s\n%s\n%s\n%s' "$status" "$shown" "$shell_kind" "$shim_dir"
`

// nextTemporary numbers the staged uploads this process makes.
var nextTemporary atomic.Uint64

// MbxInstallKind is whether machine settings should offer an install or
// upgrade.
type MbxInstallKind int

const (
	MbxInstallKindInstall MbxInstallKind = iota
	MbxInstallKindUpgrade
)

// MbxInstallResult is the verified result of an mbx installation.
type MbxInstallResult struct {
	Program        string
	Version        string
	ProfileChanged bool
	ProfileFile    string
	ProfileWarning *string
	ManualPathLine *string
	Kind           MbxInstallKind
}

// installDestination is where an install would put mbx (Rust enum
// InstallDestination): Supported, with the install kind, when supported is
// true; Unsupported (a binary managed outside Mjolnir's install locations)
// otherwise.
type installDestination struct {
	supported bool
	program   string
	// kind is meaningful only when supported.
	kind MbxInstallKind
}

// MbxProfileDetails is the login-profile change shown before and after
// installing mbx.
type MbxProfileDetails struct {
	File           string
	Warning        *string
	ManualPathLine *string
}

type profileSelection struct {
	file      string
	shellKind string
	shimDir   string
}

// InstallKind is the install action to show for the current read-only cache
// preview, or false for none.
func InstallKind(preview *core.BuildCachePreview) (MbxInstallKind, bool) {
	// A profile path is populated only when destination selection supports
	// installing or upgrading this host's mbx.
	if preview.MbxProfileFile == nil {
		return 0, false
	}
	return InstallCandidateKind(preview)
}

// InstallCandidateKind is the install action the host's mbx calls for, or
// false for none, before the login profile is known.
func InstallCandidateKind(preview *core.BuildCachePreview) (MbxInstallKind, bool) {
	// The pinned release is a static Linux executable.
	if _, unsupported := preview.OffReason.(core.BuildCacheOffUnsupportedPlatform); unsupported {
		return 0, false
	}
	if preview.NativeMbx == nil {
		return MbxInstallKindInstall, true
	}
	found, foundErr := parseSemver(*preview.NativeMbx)
	pinned, pinnedErr := parseSemver(MbxVersion)
	if foundErr == nil && pinnedErr == nil && compareSemver(found, pinned) > 0 {
		return 0, false
	}
	// A matching binary can still have interrupted setup/profile updates.
	// Keep the existing action available so rerunning always repairs them.
	return MbxInstallKindUpgrade, true
}

// preparedMbx is a fully staged install. Its drop removes unpublished files
// after executor teardown (Rust: Drop); every owner calls drop once.
type preparedMbx struct {
	host    CacheHost
	home    string
	program string
	kind    MbxInstallKind
	staged  *string
}

// drop removes the staged binary when it was never published (Rust: Drop).
func (p *preparedMbx) drop() {
	if p.staged == nil {
		return
	}
	path := *p.staged
	p.staged = nil
	if p.host.IsLocal() {
		if err := os.Remove(path); err != nil {
			slog.Warn("remove staged mbx binary", "error", err, "path", path)
		}
		return
	}
	cleanupRemoteStaging(p.host, path)
}

// prepareMbx probes, fetches, verifies and uploads without publishing or
// holding handoff admission.
func prepareMbx(machine core.Machine, executor core.CommandExecutor) (*preparedMbx, error) {
	return prepareMbxWith(machine, executor, nil, func(triple string) (string, error) {
		return PinnedLinuxBinary(triple, executor)
	})
}

// InstallMbx installs from a standalone CLI operation without a daemon
// handoff owner.
func InstallMbx(machine core.Machine, executor core.CommandExecutor) (MbxInstallResult, error) {
	prepared, err := prepareMbx(machine, executor)
	if err != nil {
		return MbxInstallResult{}, err
	}
	defer prepared.drop()
	return prepared.publish(executor)
}

func prepareMbxWith(machine core.Machine, executor core.CommandExecutor, homeOverride *string, getBinary func(triple string) (string, error)) (*preparedMbx, error) {
	host, ok := CacheHostForMachine(machine)
	if !ok {
		return nil, errors.New("this machine has no persistent container host where mbx can be installed")
	}
	platform, err := host.Platform(executor)
	if err != nil {
		return nil, err
	}
	if platform != HostPlatformLinux {
		return nil, errors.New("mbx can only be installed on a Linux container host")
	}
	triple, err := hostArchitecture(host, executor)
	if err != nil {
		return nil, err
	}
	var home string
	if homeOverride != nil {
		home = *homeOverride
	} else if home, err = canonicalHome(host, executor); err != nil {
		return nil, err
	}

	if err := checkCancelled(executor); err != nil {
		return nil, err
	}
	existing, err := ProbeNativeVersion(host, executor)
	if err != nil {
		return nil, err
	}
	selected, err := destination(home, existing)
	if err != nil {
		return nil, err
	}
	if !selected.supported {
		return nil, fmt.Errorf("mbx at %s is managed outside Mjolnir's install locations. Upgrade it with its package manager (for mise, run `mise upgrade`).", selected.program)
	}
	program, kind := selected.program, selected.kind
	if !filepath.IsAbs(program) {
		return nil, errors.New("the selected mbx destination is not absolute")
	}

	if err := checkCancelled(executor); err != nil {
		return nil, err
	}
	binary, err := getBinary(triple)
	if err != nil {
		return nil, err
	}
	if info, err := os.Stat(binary); err != nil || !info.Mode().IsRegular() {
		return nil, errors.New("the verified mbx download is not a regular file")
	}
	if err := checkCancelled(executor); err != nil {
		return nil, err
	}
	staged, err := stageBinary(host, home, binary, program, executor)
	if err != nil {
		return nil, err
	}
	return &preparedMbx{host: host, home: home, program: program, kind: kind, staged: &staged}, nil
}

// publish publishes the staged binary. Every Mj-owned mutation publishes a
// complete file; interruption is repaired by rerunning.
func (p *preparedMbx) publish(executor core.CommandExecutor) (MbxInstallResult, error) {
	host, program, kind := p.host, p.program, p.kind
	if p.staged == nil {
		return MbxInstallResult{}, errors.New("mbx install has no staged binary")
	}
	staged := *p.staged
	command := host.ShellCommand(`chmod 755 -- "$1" && mv -f -- "$1" "$2"`, "hel-mbx-install-publish",
		[]string{staged, program}, "publish the mbx binary atomically on the SSH host")
	if host.IsLocal() {
		if err := setExecutable(staged); err != nil {
			return MbxInstallResult{}, err
		}
		if err := os.Rename(staged, program); err != nil {
			return MbxInstallResult{}, fmt.Errorf("publish staged mbx binary: %w", err)
		}
	} else {
		output, err := executor.Execute(&command)
		if err != nil {
			return MbxInstallResult{}, err
		}
		if _, err := installChecked(output, &command); err != nil {
			return MbxInstallResult{}, err
		}
	}
	p.staged = nil
	if err := checkCancelled(executor); err != nil {
		return MbxInstallResult{}, err
	}
	if err := runSetup(host, program, executor); err != nil {
		return MbxInstallResult{}, err
	}
	profileChanged, profile, err := updateProfile(host, p.home, program, executor)
	if err != nil {
		return MbxInstallResult{}, err
	}

	if err := checkCancelled(executor); err != nil {
		return MbxInstallResult{}, err
	}
	verified, err := ProbeNativeVersion(host, executor)
	if err != nil {
		return MbxInstallResult{}, err
	}
	if verified == nil {
		return MbxInstallResult{}, errors.New("mbx was not found after installation")
	}
	if !pathEqual(verified.Program, program) {
		return MbxInstallResult{}, fmt.Errorf("mbx verification found %s instead of the installed path %s", verified.Program, program)
	}
	if verified.Version != MbxVersion {
		return MbxInstallResult{}, fmt.Errorf("installed mbx reported version %s, expected %s", verified.Version, MbxVersion)
	}
	return MbxInstallResult{
		Program:        program,
		Version:        verified.Version,
		ProfileChanged: profileChanged,
		ProfileFile:    profile.File,
		ProfileWarning: profile.Warning,
		ManualPathLine: profile.ManualPathLine,
		Kind:           kind,
	}, nil
}

func canonicalHome(host CacheHost, executor core.CommandExecutor) (string, error) {
	home, err := host.Home(executor)
	if err != nil {
		return "", err
	}
	command := host.ShellCommand(`cd -- "$1" && pwd -P`, "hel-mbx-home", []string{home},
		"resolve the container host's physical home directory")
	output, err := executor.Execute(&command)
	if err != nil {
		return "", err
	}
	output, err = installChecked(output, &command)
	if err != nil {
		return "", err
	}
	text, err := fromUTF8(output.Stdout)
	if err != nil {
		return "", fmt.Errorf("host home directory is not UTF-8: %w", err)
	}
	home = strings.TrimSpace(text)
	if !filepath.IsAbs(home) {
		return "", errors.New("container host home is not an absolute path")
	}
	return home, nil
}

func hostArchitecture(host CacheHost, executor core.CommandExecutor) (string, error) {
	command := host.Command([]string{"uname", "-m"}, "detect mbx host architecture")
	output, err := executor.Execute(&command)
	if err != nil {
		return "", err
	}
	output, err = installChecked(output, &command)
	if err != nil {
		return "", err
	}
	text, err := fromUTF8(output.Stdout)
	if err != nil {
		return "", fmt.Errorf("decode mbx host architecture: %w", err)
	}
	switch architecture := strings.TrimSpace(text); architecture {
	case "x86_64", "aarch64":
		return architecture, nil
	default:
		return "", fmt.Errorf("no pinned static mbx release for Linux architecture %s", core.RustDebugString(architecture))
	}
}

func destination(home string, existing *NativeMbx) (installDestination, error) {
	if existing == nil {
		return installDestination{supported: true, program: pathJoin(home, ".local/bin/mbx"), kind: MbxInstallKindInstall}, nil
	}
	if !filepath.IsAbs(existing.Program) {
		return installDestination{}, errors.New("native mbx probe returned a non-absolute path")
	}
	for _, directory := range []string{pathJoin(home, ".local/bin"), pathJoin(home, ".cargo/bin")} {
		if _, ok := pathStripPrefix(existing.Program, directory); ok {
			return installDestination{supported: true, program: existing.Program, kind: MbxInstallKindUpgrade}, nil
		}
	}
	return installDestination{program: existing.Program}, nil
}

func stageBinary(host CacheHost, home, binary, program string, executor core.CommandExecutor) (string, error) {
	parent, ok := pathParent(program)
	if !ok {
		return "", errors.New("the selected mbx path has no parent directory")
	}
	if host.IsLocal() {
		return stageLocalBinary(parent, binary, executor)
	}

	command := host.Command([]string{"mkdir", "-p", "--", parent}, "create the mbx install directory")
	output, err := executor.Execute(&command)
	if err != nil {
		return "", err
	}
	if _, err := installChecked(output, &command); err != nil {
		return "", err
	}

	sequence := nextTemporary.Add(1) - 1
	temporary := pathJoin(parent, fmt.Sprintf(".mbx.mjolnir-%d-%d.tmp", os.Getpid(), sequence))
	remoteRelative, ok := pathStripPrefix(temporary, home)
	if !ok {
		return "", errors.New("mbx install directory is outside the SSH host home")
	}
	upload := core.SCPUpload(host.SSH, binary, remoteRelative, false).
		WithPurpose("upload the pinned mbx binary to the SSH host")
	output, err = executor.Execute(&upload)
	if err == nil {
		_, err = installChecked(output, &upload)
	}
	if err != nil {
		cleanupRemoteStaging(host, temporary)
		return "", err
	}
	if err := checkCancelled(executor); err != nil {
		cleanupRemoteStaging(host, temporary)
		return "", err
	}
	return temporary, nil
}

// stageLocalBinary copies the verified download beside its destination on
// this machine, removing the copy on every failure (Rust: the NamedTempFile
// dropped before keep).
func stageLocalBinary(parent, binary string, executor core.CommandExecutor) (staged string, err error) {
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return "", fmt.Errorf("create mbx install directory %s: %w", parent, err)
	}
	temporary, err := os.CreateTemp(parent, ".tmp")
	if err != nil {
		return "", fmt.Errorf("create temporary mbx binary in %s: %w", parent, err)
	}
	path := temporary.Name()
	defer func() {
		if closeErr := temporary.Close(); err == nil && closeErr != nil {
			err = fmt.Errorf("retain staged mbx binary: %w", closeErr)
		}
		if err != nil {
			os.Remove(path)
			staged = ""
		}
	}()
	source, err := os.Open(binary)
	if err != nil {
		return "", fmt.Errorf("open downloaded mbx binary %s: %w", binary, err)
	}
	defer source.Close()
	if _, err := io.Copy(temporary, source); err != nil {
		return "", fmt.Errorf("copy downloaded mbx binary into the install directory: %w", err)
	}
	if err := checkCancelled(executor); err != nil {
		return "", err
	}
	if err := temporary.Sync(); err != nil {
		return "", err
	}
	if err := setExecutable(path); err != nil {
		return "", err
	}
	return path, nil
}

func cleanupRemoteStaging(host CacheHost, path string) {
	// Use the executor's existing 15-second cleanup bound, independent of the
	// cancelled request.
	cleanup := core.NewCancellableProcessExecutor(&atomic.Bool{})
	removeRemoteTemporary(host, path, cleanup)
}

func removeRemoteTemporary(host CacheHost, path string, executor core.CommandExecutor) {
	command := host.Command([]string{"rm", "-f", "--", path}, "remove partial mbx install upload")
	output, err := executor.ExecuteCleanup(&command)
	switch {
	case err != nil:
		slog.Warn(fmt.Sprintf("could not remove a partial mbx installation upload: %s", alternate(err)))
	case output.Status != 0:
		slog.Warn(fmt.Sprintf("could not remove a partial mbx installation upload: %s", trimmedLossy(output.Stderr)),
			"status", output.Status)
	}
}

// setExecutable makes a downloaded mbx executable. Outside Unix there is no
// mode to set, as in Rust's `cfg(not(unix))` version.
func setExecutable(path string) error {
	if runtime.GOOS == "windows" {
		return nil
	}
	if err := os.Chmod(path, 0o755); err != nil {
		return fmt.Errorf("make downloaded mbx executable: %w", err)
	}
	return nil
}

func runSetup(host CacheHost, program string, executor core.CommandExecutor) error {
	// Pre-existing mbx 1.30.0 defect: setup.rs:275–278 calls util.rs:125–141
	// to rename the cargo launcher before setting its executable bit. Daemon
	// handoff and disconnect add ways to interrupt this existing window.
	// Interruption can leave it non-executable until Install runs again. The
	// structural fix belongs in mbx: set the temporary file's mode before
	// persist, then bump our pin.
	command := host.Command([]string{program, "setup", "--yes"}, "run mbx setup on the container host")
	output, err := executor.Execute(&command)
	if err != nil {
		return err
	}
	_, err = installChecked(output, &command)
	return err
}

// LoginProfileDetails is the login profile an install would update on host,
// or nil when the host's mbx is managed outside Mjolnir's install locations.
func LoginProfileDetails(host CacheHost, executor core.CommandExecutor) (*MbxProfileDetails, error) {
	home, err := canonicalHome(host, executor)
	if err != nil {
		return nil, err
	}
	existing, err := ProbeNativeVersion(host, executor)
	if err != nil {
		return nil, err
	}
	selected, err := destination(home, existing)
	if err != nil {
		return nil, err
	}
	if !selected.supported {
		return nil, nil
	}
	binaryDir, ok := pathParent(selected.program)
	if !ok {
		return nil, errors.New("the selected mbx path has no parent directory")
	}
	selection, err := inspectProfileSelection(host, binaryDir, executor)
	if err != nil {
		return nil, err
	}
	details := profileDetails(&selection, binaryDir)
	return &details, nil
}

func inspectProfileSelection(host CacheHost, binaryDir string, executor core.CommandExecutor) (profileSelection, error) {
	command := host.ShellCommand(profileScript, "hel-mbx-profile", []string{"preview", binaryDir},
		"resolve the host login profile for the mbx PATH block")
	output, err := executor.Execute(&command)
	if err != nil {
		return profileSelection{}, err
	}
	output, err = installChecked(output, &command)
	if err != nil {
		return profileSelection{}, err
	}
	return parseProfileSelection(output.Stdout)
}

// localMbxLock is the host-wide mbx install lock on this machine and the
// file that holds it. release unlocks and closes it (Rust: Drop).
type localMbxLock struct {
	lock *core.FileLock
	file *os.File
}

func (l *localMbxLock) release() {
	l.lock.Drop()
	l.file.Close()
}

func lockLocalMbx(home string, executor core.CommandExecutor) (*localMbxLock, error) {
	file, err := os.OpenFile(pathJoin(home, ".mbx.lock"), os.O_CREATE|os.O_RDWR, 0o666)
	if err != nil {
		return nil, fmt.Errorf("open mbx install lock: %w", err)
	}
	lock, err := core.AcquireFileLockCancellable(file, executor.CancellationRequested)
	if err != nil {
		file.Close()
		if cancelled := checkCancelled(executor); cancelled != nil {
			return nil, cancelled
		}
		return nil, fmt.Errorf("lock mbx profile installation: %w", err)
	}
	return &localMbxLock{lock: lock, file: file}, nil
}

// updateProfile adds the mbx PATH block to the host's login profile, and
// reports whether the profile changed.
func updateProfile(host CacheHost, home, program string, executor core.CommandExecutor) (bool, MbxProfileDetails, error) {
	binaryDir, ok := pathParent(program)
	if !ok {
		return false, MbxProfileDetails{}, errors.New("the selected mbx path has no parent directory")
	}
	var lock *localMbxLock
	if host.IsLocal() {
		var err error
		if lock, err = lockLocalMbx(home, executor); err != nil {
			return false, MbxProfileDetails{}, err
		}
		defer lock.release()
	}
	selection, err := inspectProfileSelection(host, binaryDir, executor)
	if err != nil {
		return false, MbxProfileDetails{}, err
	}
	posixBlock := posixProfileBlock(binaryDir, selection.shimDir)
	fishBlock := fishProfileBlock(binaryDir, selection.shimDir)
	lockArgument := ""
	if lock == nil {
		lockArgument = pathJoin(home, ".mbx.lock")
	}
	command := host.ShellCommand(profileScript, "hel-mbx-profile",
		[]string{"update", binaryDir, posixBlock, fishBlock, lockArgument},
		"add the mbx PATH block to the container host login profile")
	output, err := executor.Execute(&command)
	if err != nil {
		return false, MbxProfileDetails{}, err
	}
	output, err = installChecked(output, &command)
	if err != nil {
		return false, MbxProfileDetails{}, fmt.Errorf("update shell profile %s: %w", selection.file, err)
	}
	text, err := fromUTF8(output.Stdout)
	if err != nil {
		return false, MbxProfileDetails{}, fmt.Errorf("profile update result is not UTF-8: %w", err)
	}
	lines := rustLines(text)
	if len(lines) == 0 {
		return false, MbxProfileDetails{}, errors.New("profile update returned no result")
	}
	status := lines[0]
	updated, err := selectionFromLines(lines[1:])
	if err != nil {
		return false, MbxProfileDetails{}, err
	}
	var changed bool
	switch status {
	case "changed":
		changed = true
	case "unchanged":
	default:
		return false, MbxProfileDetails{}, fmt.Errorf("profile update returned an unexpected result: %s", core.RustDebugString(status))
	}
	return changed, profileDetails(&updated, binaryDir), nil
}

func parseProfileSelection(output []byte) (profileSelection, error) {
	text, err := fromUTF8(output)
	if err != nil {
		return profileSelection{}, fmt.Errorf("host profile selection is not UTF-8: %w", err)
	}
	lines := rustLines(text)
	if len(lines) == 0 || lines[0] != "preview" {
		return profileSelection{}, errors.New("host profile selection returned an unexpected result")
	}
	return selectionFromLines(lines[1:])
}

// selectionFromLines reads a profile selection from the remaining result
// lines, which it must consume exactly (Rust takes the line iterator).
func selectionFromLines(lines []string) (profileSelection, error) {
	if len(lines) < 1 {
		return profileSelection{}, errors.New("profile selection returned no path")
	}
	file := lines[0]
	if err := validateProfileFile(file); err != nil {
		return profileSelection{}, err
	}
	if len(lines) < 2 {
		return profileSelection{}, errors.New("profile selection returned no shell kind")
	}
	shellKind := lines[1]
	switch shellKind {
	case "posix", "fish", "unknown":
	default:
		return profileSelection{}, fmt.Errorf("host selected an unexpected login shell kind: %s", core.RustDebugString(shellKind))
	}
	if len(lines) < 3 {
		return profileSelection{}, errors.New("profile selection returned no mbx shim directory")
	}
	shimDir := lines[2]
	if !filepath.IsAbs(shimDir) {
		return profileSelection{}, fmt.Errorf("host selected a non-absolute mbx shim directory: %s", core.RustDebugString(shimDir))
	}
	if len(lines) > 3 {
		return profileSelection{}, errors.New("profile selection returned unexpected extra fields")
	}
	return profileSelection{file: file, shellKind: shellKind, shimDir: shimDir}, nil
}

// rustLines is Rust's `str::lines`: lines split on `\n`, each without a
// trailing `\r`, and no empty last line after a final newline.
func rustLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(text, "\n"), "\n")
	for i, line := range lines {
		lines[i] = strings.TrimSuffix(line, "\r")
	}
	return lines
}

func validateProfileFile(file string) error {
	if strings.HasPrefix(file, "~/") || filepath.IsAbs(file) {
		return nil
	}
	return fmt.Errorf("host selected a non-absolute login profile path: %s", core.RustDebugString(file))
}

func profileDetails(selection *profileSelection, binaryDir string) MbxProfileDetails {
	var manualPathLine, warning *string
	if selection.shellKind == "unknown" {
		directories := selection.shimDir + ":" + binaryDir
		line := "export PATH=" + shellQuote(directories) + "${PATH:+:$PATH}"
		manualPathLine = &line
		text := "Your login shell may not read ~/.profile. Add this PATH line to a startup file it reads:"
		warning = &text
	}
	return MbxProfileDetails{File: selection.file, Warning: warning, ManualPathLine: manualPathLine}
}

// posixProfileBlockTemplate is the POSIX PATH block, with BINARY_DIR and
// SHIM_DIR standing for the quoted directories.
const posixProfileBlockTemplate = `# >>> mbx (added by Mjolnir) >>>
_mjolnir_mbx_prepend_path() {
    _mjolnir_mbx_dir=$1
    _mjolnir_mbx_rest=${PATH-}
    _mjolnir_mbx_new=
    _mjolnir_mbx_first=yes
    if [ -n "$_mjolnir_mbx_rest" ]; then
        while :; do
            case $_mjolnir_mbx_rest in
                *:*)
                    _mjolnir_mbx_entry=${_mjolnir_mbx_rest%%:*}
                    _mjolnir_mbx_rest=${_mjolnir_mbx_rest#*:}
                    _mjolnir_mbx_more=yes
                    ;;
                *)
                    _mjolnir_mbx_entry=$_mjolnir_mbx_rest
                    _mjolnir_mbx_more=no
                    ;;
            esac
            if [ "$_mjolnir_mbx_entry" != "$_mjolnir_mbx_dir" ]; then
                if [ "$_mjolnir_mbx_first" = yes ]; then
                    _mjolnir_mbx_new=$_mjolnir_mbx_entry
                    _mjolnir_mbx_first=no
                else
                    _mjolnir_mbx_new=$_mjolnir_mbx_new:$_mjolnir_mbx_entry
                fi
            fi
            [ "$_mjolnir_mbx_more" = yes ] || break
        done
    fi
    if [ "$_mjolnir_mbx_first" = yes ]; then
        PATH=$_mjolnir_mbx_dir
    else
        PATH=$_mjolnir_mbx_dir:$_mjolnir_mbx_new
    fi
}
_mjolnir_mbx_prepend_path BINARY_DIR
_mjolnir_mbx_prepend_path SHIM_DIR
export PATH
# <<< mbx <<<`

func posixProfileBlock(binaryDir, shimDir string) string {
	block := strings.Replace(posixProfileBlockTemplate, "_mjolnir_mbx_prepend_path BINARY_DIR",
		"_mjolnir_mbx_prepend_path "+shellQuote(binaryDir), 1)
	return strings.Replace(block, "_mjolnir_mbx_prepend_path SHIM_DIR",
		"_mjolnir_mbx_prepend_path "+shellQuote(shimDir), 1)
}

func fishProfileBlock(binaryDir, shimDir string) string {
	return "# >>> mbx (added by Mjolnir) >>>\nfish_add_path --path --prepend --move -- " + fishQuote(binaryDir) +
		"\nfish_add_path --path --prepend --move -- " + fishQuote(shimDir) + "\n# <<< mbx <<<"
}

func shellQuote(value string) string {
	return "'" + strings.ReplaceAll(value, "'", `'\''`) + "'"
}

func fishQuote(value string) string {
	return "'" + strings.ReplaceAll(strings.ReplaceAll(value, `\`, `\\`), "'", `\'`) + "'"
}

func checkCancelled(executor core.CommandExecutor) error {
	if executor.CancellationRequested() {
		return errors.New("mbx installation cancelled")
	}
	return nil
}

// installChecked is install.rs's `checked`: output when the command
// succeeded, its failure otherwise.
func installChecked(output core.CommandOutput, command *core.CommandSpec) (core.CommandOutput, error) {
	if output.Status != 0 {
		return core.CommandOutput{}, fmt.Errorf("%s failed with status %d: %s",
			command.Purpose, output.Status, trimmedLossy(output.Stderr))
	}
	return output, nil
}

func releaseURL(triple string) string {
	if url, ok := mbxTestReleaseURLValue(); ok {
		return url
	}
	return "https://github.com/jdx/mr-boxington/releases/download/v" + MbxVersion + "/mbx-" + triple + "-unknown-linux-musl.tar.gz"
}

func expectedDigest(triple string) (string, error) {
	switch triple {
	case "x86_64":
		return mbxX8664SHA256, nil
	case "aarch64":
		return mbxAarch64SHA256, nil
	}
	return "", fmt.Errorf("no pinned mbx release for %s", triple)
}

// PinnedLinuxBinary materializes the verified Linux release for the target's
// architecture on the controller, regardless of the controller's own OS and
// architecture. Cancellation of the download follows the executor's.
func PinnedLinuxBinary(triple string, executor core.CommandExecutor) (string, error) {
	expected, err := expectedDigest(triple)
	if err != nil {
		return "", err
	}
	// Keep the original verified cache layout for offline retries and other
	// hosts.
	destination := pathJoin(pathJoin(pathJoin(pathJoin(core.DataDir(), "mbx"), MbxVersion), triple), "mbx")
	if info, err := os.Stat(destination); err == nil && info.Mode().IsRegular() {
		return destination, nil
	}
	root := pathJoin(core.DataDir(), "mbx-download")
	if err := os.MkdirAll(root, 0o777); err != nil {
		return "", fmt.Errorf("create mbx download staging root: %w", err)
	}
	directory, err := os.MkdirTemp(root, ".tmp")
	if err != nil {
		return "", fmt.Errorf("create mbx download staging directory: %w", err)
	}
	defer os.RemoveAll(directory)
	url := releaseURL(triple)
	// The download runs in its own goroutine; this one watches the
	// executor's cancellation and abandons it, as Rust drops the future.
	archive, err := func() ([]byte, error) {
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		type fetched struct {
			archive []byte
			err     error
		}
		done := make(chan fetched, 1)
		go func() {
			archive, err := fetchArchive(ctx, url)
			done <- fetched{archive: archive, err: err}
		}()
		// Match the cancellable subprocess executor's flag polling interval.
		ticker := time.NewTicker(25 * time.Millisecond)
		defer ticker.Stop()
		for {
			if err := checkCancelled(executor); err != nil {
				return nil, err
			}
			select {
			case result := <-done:
				return result.archive, result.err
			case <-ticker.C:
			}
		}
	}()
	if err != nil {
		return "", err
	}
	if err := checkCancelled(executor); err != nil {
		return "", err
	}
	if err := os.WriteFile(pathJoin(directory, "release.tar.gz"), archive, 0o666); err != nil {
		return "", fmt.Errorf("write staged mbx archive: %w", err)
	}
	digest := sha256.Sum256(archive)
	actual := core.LowerHex(digest[:])
	if !strings.EqualFold(actual, expected) {
		return "", fmt.Errorf("downloaded mbx checksum mismatch: expected %s, got %s", expected, actual)
	}
	binary, err := extractBinary(archive)
	if err != nil {
		return "", err
	}
	if err := checkCancelled(executor); err != nil {
		return "", err
	}
	return retainVerifiedDownload(destination, binary)
}

func retainVerifiedDownload(destination string, binary []byte) (published string, err error) {
	parent, ok := pathParent(destination)
	if !ok {
		return "", errors.New("verified mbx cache path has no parent")
	}
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return "", fmt.Errorf("create verified mbx cache directory: %w", err)
	}
	temporary, err := os.CreateTemp(parent, ".tmp")
	if err != nil {
		return "", err
	}
	path := temporary.Name()
	// The temporary name never outlives this call: it is removed once linked
	// to the destination, or on failure (Rust: the NamedTempFile dropped).
	defer os.Remove(path)
	if _, err := temporary.Write(binary); err != nil {
		temporary.Close()
		return "", err
	}
	if err := temporary.Sync(); err != nil {
		temporary.Close()
		return "", err
	}
	if err := temporary.Close(); err != nil {
		return "", err
	}
	if err := setExecutable(path); err != nil {
		return "", err
	}
	// Publish without replacing an existing file (Rust: persist_noclobber).
	if err := os.Link(path, destination); err != nil {
		if info, statErr := os.Stat(destination); statErr == nil && info.Mode().IsRegular() {
			// A concurrent verified download published the same pinned
			// release first.
			return destination, nil
		}
		return "", fmt.Errorf("publish verified mbx download cache: %w", err)
	}
	return destination, nil
}

// fetchArchive downloads url; cancelling ctx abandons the download.
func fetchArchive(ctx context.Context, url string) ([]byte, error) {
	// Preserve the original per-download deadline, including standalone EC2
	// installs.
	client := &http.Client{Timeout: 120 * time.Second}
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, fmt.Errorf("download %s: %w", url, err)
	}
	response, err := client.Do(request)
	if err != nil {
		return nil, fmt.Errorf("download %s: %w", url, err)
	}
	defer response.Body.Close()
	if response.StatusCode >= 400 {
		class := "client error"
		if response.StatusCode >= 500 {
			class = "server error"
		}
		return nil, fmt.Errorf("download %s: HTTP status %s (%s) for url (%s)", url, class, response.Status, url)
	}
	body, err := io.ReadAll(response.Body)
	if err != nil {
		return nil, fmt.Errorf("read mbx release response: %w", err)
	}
	return body, nil
}

// mbxTestReleaseURL replaces the pinned release URL (Rust's
// `#[doc(hidden)] pub static MBX_TEST_RELEASE_URL: OnceLock<String>`).
var mbxTestReleaseURL struct {
	sync.Mutex
	url *string
}

// SetMbxTestReleaseURL sets the release URL tests download mbx from. As with
// Rust's OnceLock::set, only the first call takes effect; it reports whether
// this one did.
func SetMbxTestReleaseURL(url string) bool {
	mbxTestReleaseURL.Lock()
	defer mbxTestReleaseURL.Unlock()
	if mbxTestReleaseURL.url != nil {
		return false
	}
	mbxTestReleaseURL.url = &url
	return true
}

func mbxTestReleaseURLValue() (string, bool) {
	mbxTestReleaseURL.Lock()
	defer mbxTestReleaseURL.Unlock()
	if mbxTestReleaseURL.url == nil {
		return "", false
	}
	return *mbxTestReleaseURL.url, true
}

// extractBinary extracts the archive's single binary; its licence texts are
// not installed.
func extractBinary(archive []byte) ([]byte, error) {
	decompressed, err := gzip.NewReader(bytes.NewReader(archive))
	if err != nil {
		return nil, fmt.Errorf("read the mbx release archive: %w", err)
	}
	reader := tar.NewReader(decompressed)
	for {
		header, err := reader.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return nil, fmt.Errorf("read the mbx release archive: %w", err)
		}
		if !pathEqual(header.Name, "mbx") {
			continue
		}
		contents, err := io.ReadAll(reader)
		if err != nil {
			return nil, fmt.Errorf("read the mbx binary from its release archive: %w", err)
		}
		return contents, nil
	}
	return nil, errors.New("the mbx release archive contains no mbx binary")
}
