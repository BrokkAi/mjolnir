// Port of mj-build-cache/src/mbx/install.rs.
//
// Install the pinned native mbx binary on a local or SSH container host.
//
// Rust's test-only install_mbx_with lives in this file's tests.

package buildcache

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"

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
	panic("unported: mj-build-cache/src/mbx/install.rs install_candidate_kind")
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
	panic("unported: mj-build-cache/src/mbx/install.rs PreparedMbx::drop")
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
	panic("unported: mj-build-cache/src/mbx/install.rs prepare_mbx_with")
}

// publish publishes the staged binary. Every Mj-owned mutation publishes a
// complete file; interruption is repaired by rerunning.
func (p *preparedMbx) publish(executor core.CommandExecutor) (MbxInstallResult, error) {
	panic("unported: mj-build-cache/src/mbx/install.rs PreparedMbx::publish")
}

func canonicalHome(host CacheHost, executor core.CommandExecutor) (string, error) {
	panic("unported: mj-build-cache/src/mbx/install.rs canonical_home")
}

func hostArchitecture(host CacheHost, executor core.CommandExecutor) (string, error) {
	panic("unported: mj-build-cache/src/mbx/install.rs host_architecture")
}

func destination(home string, existing *NativeMbx) (installDestination, error) {
	panic("unported: mj-build-cache/src/mbx/install.rs destination")
}

func stageBinary(host CacheHost, home, binary, program string, executor core.CommandExecutor) (string, error) {
	panic("unported: mj-build-cache/src/mbx/install.rs stage_binary")
}

func cleanupRemoteStaging(host CacheHost, path string) {
	// Use the executor's existing 15-second cleanup bound, independent of the
	// cancelled request.
	cleanup := core.NewCancellableProcessExecutor(&atomic.Bool{})
	removeRemoteTemporary(host, path, cleanup)
}

func removeRemoteTemporary(host CacheHost, path string, executor core.CommandExecutor) {
	panic("unported: mj-build-cache/src/mbx/install.rs remove_remote_temporary")
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
	panic("unported: mj-build-cache/src/mbx/install.rs login_profile_details")
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

func lockLocalMbx(home string, executor core.CommandExecutor) (*core.FileLock, error) {
	panic("unported: mj-build-cache/src/mbx/install.rs lock_local_mbx")
}

// updateProfile adds the mbx PATH block to the host's login profile, and
// reports whether the profile changed.
func updateProfile(host CacheHost, home, program string, executor core.CommandExecutor) (bool, MbxProfileDetails, error) {
	panic("unported: mj-build-cache/src/mbx/install.rs update_profile")
}

func parseProfileSelection(output []byte) (profileSelection, error) {
	panic("unported: mj-build-cache/src/mbx/install.rs parse_profile_selection")
}

// selectionFromLines reads a profile selection from the remaining result
// lines, which it must consume exactly (Rust takes the line iterator).
func selectionFromLines(lines []string) (profileSelection, error) {
	panic("unported: mj-build-cache/src/mbx/install.rs selection_from_lines")
}

func validateProfileFile(file string) error {
	if strings.HasPrefix(file, "~/") || filepath.IsAbs(file) {
		return nil
	}
	return fmt.Errorf("host selected a non-absolute login profile path: %s", core.RustDebugString(file))
}

func profileDetails(selection *profileSelection, binaryDir string) MbxProfileDetails {
	panic("unported: mj-build-cache/src/mbx/install.rs profile_details")
}

func posixProfileBlock(binaryDir, shimDir string) string {
	panic("unported: mj-build-cache/src/mbx/install.rs posix_profile_block")
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
	panic("unported: mj-build-cache/src/mbx/install.rs pinned_linux_binary")
}

func retainVerifiedDownload(destination string, binary []byte) (string, error) {
	panic("unported: mj-build-cache/src/mbx/install.rs retain_verified_download")
}

// fetchArchive downloads url; cancelling ctx abandons the download.
func fetchArchive(ctx context.Context, url string) ([]byte, error) {
	panic("unported: mj-build-cache/src/mbx/install.rs fetch_archive")
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
	panic("unported: mj-build-cache/src/mbx/install.rs extract_binary")
}
