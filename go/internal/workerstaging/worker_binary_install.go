// Port of mj-worker-staging/src/worker_binary/install.rs.
//
// Rust declares profile_transfer.rs as a submodule of this one; in Go it is a
// file of the same package (worker_binary_profile_transfer.go), so
// ProfileContentManifest needs no re-export here.

package workerstaging

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"sync"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// AppendHelTargetEnvironment adds lifecycle guidance only for targets that
// Hel destroys as a whole.
func AppendHelTargetEnvironment(harness core.HarnessKind, destination string, target *core.TargetsTargetLocator) error {
	var environment string
	switch target.Kind {
	case core.TargetsTargetLocatorLocalPodman, core.TargetsTargetLocatorLocalDocker,
		core.TargetsTargetLocatorAppleContainer, core.TargetsTargetLocatorSshPodman,
		core.TargetsTargetLocatorSshDocker, core.TargetsTargetLocatorSshAppleContainer,
		core.TargetsTargetLocatorSshNitro:
		environment = MJContainerEnvironment
	case core.TargetsTargetLocatorAwsEc2:
		workspace := target.Workspace
		environment = "## Mjolnir disposable environment\n\nThis session runs on a disposable Mjolnir EC2 instance. When the session closes, Mjolnir checkpoints everything in project workspace directories under `$HOME/" + workspace + "`, including committed work, staged and unstaged changes, and untracked files. Mjolnir then terminates the instance.\n\nEverything outside `$HOME/" + workspace + "`, including installed packages, the rest of `$HOME`, and `/tmp`, is ephemeral and will be lost. Keep durable results in the workspace or push them to a remote.\n\nNew workspaces start on their own session branch from the default network fetch remote’s default branch. Local unpublished commits and uncommitted files are not copied. Use normal git push to publish the current branch to the configured network push destination. Closing saves a checkpoint; it does not publish commits or update the original local checkout. Resumed sessions restore their saved work.\n"
	default:
		// LocalBare and SshBare.
		return nil
	}
	return AppendStagedInstructions(harness, destination, environment)
}

// AppendContainerStorageGuidance describes the container's temporary and
// workspace storage and its external attachments in the staged harness
// instructions. workspace is the session's recorded container workspace, nil
// for a session that predates per-session workspaces.
func AppendContainerStorageGuidance(harness core.HarnessKind, destination string, target *core.TargetsTargetLocator, workspace *string, nativeTemporaryStorage bool, attachments []core.AdditionalMount) error {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs append_container_storage_guidance")
}

// AppendSessionStateInstructions adds the Suspend and Move rules for every
// harness, including bare sessions.
func AppendSessionStateInstructions(harness core.HarnessKind, destination string) error {
	return AppendStagedInstructions(harness, destination, core.SessionStateInstructions)
}

// AppendSubagentPolicy adds the Mjolnir delegation policy when the session
// delegates through Mjolnir.
func AppendSubagentPolicy(harness core.HarnessKind, destination string, policy *core.SubagentPolicy, limit int, offersSubtle bool) error {
	if policy.Mode == core.SubagentPolicyMjolnir {
		return AppendStagedInstructions(harness, destination, core.DelegationPolicy(limit, offersSubtle))
	}
	return nil
}

// AppendOrchestratorPolicy adds the orchestrator delegation policy.
func AppendOrchestratorPolicy(harness core.HarnessKind, destination string) error {
	return AppendStagedInstructions(harness, destination, core.OrchestratorDelegationPolicy())
}

// AppendComposedSessionInstructions is called only on a fresh profile stage,
// never on a retained session profile.
//
// Rust's `index: Result<&str, &str>` is index (the project memory index
// text) when indexFailed is false, or the warning to show in its place when
// it is true, as core.Compose takes it.
func AppendComposedSessionInstructions(launch *core.WorkerLaunchConfig, destination string, index string, indexFailed bool) error {
	if !core.DeliveredByStaging(launch.Harness) {
		return nil
	}
	facts := core.InstructionContextFromLaunch(launch)
	if text := core.Compose(&facts, index, indexFailed); text != "" {
		if err := AppendStagedInstructions(launch.Harness, destination, text); err != nil {
			return err
		}
	}
	if launch.Orchestrator {
		return AppendOrchestratorPolicy(launch.Harness, destination)
	}
	return nil
}

// AppendStagedInstructions appends text to the harness's instruction file in
// the staged home destination, unless the file already contains it.
func AppendStagedInstructions(harness core.HarnessKind, destination string, text string) error {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs append_staged_instructions")
}

// SharedLogin is a profile's own login file that a worker on this machine
// links to instead of copying (see core.HarnessKind.SharesLocalLogin).
type SharedLogin struct {
	// Name is where the login file sits in a staged home.
	Name string
	// File is the profile's login file the staged home links to.
	File string
}

// SharedProfileLogin is the login file a worker for profile on backend
// shares with the profile, or nil when the worker keeps its own copy: on
// every other target, for a harness that cannot share one, and for a
// profile that signs in some other way than the harness's login file.
func SharedProfileLogin(profile *core.HarnessProfile, backend *core.TargetsTargetLocator) *SharedLogin {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs shared_profile_login")
}

// InstallLocalProfileStage copies a staged home into a home on this machine,
// then links the login file when the worker shares the profile's own.
//
// A link an earlier install left is removed before copying, whether or not
// this install shares the login: copying a file through it would write the
// stage over the profile's login.
func InstallLocalProfileStage(stage string, home string, sharedLogin *SharedLogin) error {
	entries, err := os.ReadDir(stage)
	if err != nil {
		return err
	}
	for _, entry := range entries {
		destination := filepath.Join(home, entry.Name())
		if err := removeLink(destination); err != nil {
			return err
		}
		if err := CopyProfileEntry(filepath.Join(stage, entry.Name()), destination); err != nil {
			return err
		}
	}
	if sharedLogin != nil {
		return LinkSharedLogin(home, sharedLogin)
	}
	return nil
}

// LinkSharedLogin replaces the login file in home with a link to the
// profile's own.
func LinkSharedLogin(home string, login *SharedLogin) error {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs link_shared_login")
}

func removeLink(path string) error {
	if info, err := os.Lstat(path); err == nil && info.Mode()&os.ModeSymlink != 0 {
		if err := os.Remove(path); err != nil {
			return fmt.Errorf("remove the earlier link %s: %w", path, err)
		}
	}
	return nil
}

// CopyProfileEntry copies one profile entry; see CopyProfileEntryWithin.
func CopyProfileEntry(source string, destination string) error {
	return CopyProfileEntryWithin(source, destination, map[string]struct{}{}, nil)
}

// CopyProfileEntryExcept is CopyProfileEntry, leaving out the paths in
// excluded. Each is compared with the path the walk reaches it by, below
// source as given, not with where a link resolves.
func CopyProfileEntryExcept(source string, destination string, excluded []string) error {
	return CopyProfileEntryWithin(source, destination, map[string]struct{}{}, excluded)
}

// CopyProfileEntryWithin copies one profile entry, following symlinks so a
// profile home that links its settings or instructions elsewhere still
// stages their contents. entered holds the canonical paths of the
// directories already entered on this branch of the recursion, which stops a
// symlinked directory cycle.
func CopyProfileEntryWithin(source string, destination string, entered map[string]struct{}, excluded []string) error {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs copy_profile_entry_within")
}

// ContainerUploadOwnershipArgs: container copies may be root-owned while the
// worker directory belongs to a different user. Run this ownership-only
// operation as container root; normal Podman execs inherit the user recorded
// when each container was created.
func ContainerUploadOwnershipArgs(engine string, containerID string, workerRoot string, paths []string) []string {
	// The microVM engine runs every exec as the session's configured user,
	// which is root (see the run arguments), so it needs no user flag.
	var userFlag []string
	switch engine {
	case "podman":
		userFlag = []string{"--user", "0:0"}
	case core.MicrovmEngine:
	default:
		userFlag = []string{"--user", "0"}
	}
	args := append([]string{"exec"}, userFlag...)
	args = append(args, containerID, "sh", "-c",
		// GNU and BusyBox stat both support this numeric ownership format.
		`set -eu; owner=$(stat -c '%u:%g' -- "$1"); shift; chown -R "$owner" -- "$@"`,
		"sh", workerRoot)
	return append(args, paths...)
}

// mbxConfigScript links every mbx lookup to the one policy already reachable
// through the shared cache mount. File symlinks follow atomic replacements at
// the source.
const mbxConfigScript = `set -eu
source=$1
shift
[ -r "$source/config.toml" ] || { echo 'shared machine mbx configuration is missing' >&2; exit 1; }
for root in "$HOME/.config" "$@"; do
    mkdir -p -- "$root/mbx"
    link="$root/mbx/.config.mj-$$"
    ln -s -- "$source/config.toml" "$link"
    mv -Tf -- "$link" "$root/mbx/config.toml"
done
`

const checkLegacyMbxAndLinkConfigScript = `set -eu
bin=$1 source=$2
shift 2
scheme=new
if [ -f "$bin/mbx" ]; then
    if grep -Fqx '# mjolnir-mbx-shim' "$bin/mbx"; then
        scheme=new
    else
        grep_status=$?
        case "$grep_status" in
            1) scheme=legacy ;;
            *)
                echo "could not inspect the Mjolnir mbx shim marker in $bin/mbx (grep exited $grep_status)" >&2
                exit 1
                ;;
        esac
    fi
fi
if [ "$scheme" = legacy ]; then
    [ -r "$source/config.toml" ] || { echo 'shared machine mbx configuration is missing' >&2; exit 1; }
    for root in "$HOME/.config" "$@"; do
        mkdir -p -- "$root/mbx"
        link="$root/mbx/.config.mj-$$"
        ln -s -- "$source/config.toml" "$link"
        mv -Tf -- "$link" "$root/mbx/config.toml"
    done
    printf 'legacy'
else
    printf 'new'
fi
`

const installMbxShimsScript = `set -eu
bin=$1 cargo_body=$2 mbx_body=$3
mkdir -p -- "$bin"
write_shim() {
    target=$1 body=$2
    temporary=$(mktemp "${target}.mj-XXXXXX")
    trap 'rm -f -- "$temporary"' EXIT HUP INT TERM
    {
        printf '%s\n' '#!/bin/sh' '# mjolnir-mbx-shim'
        printf '%s\n' "$body"
    } >"$temporary"
    chmod 755 -- "$temporary"
    mv -f -- "$temporary" "$target"
    trap - EXIT HUP INT TERM
}
write_shim "$bin/cargo" "$cargo_body"
write_shim "$bin/mbx" "$mbx_body"
`

const removeMbxShimsScript = `set -eu
bin=$1
for name in cargo mbx; do
    path="$bin/$name"
    if [ -f "$path" ] && [ ! -L "$path" ] && grep -Fqx '# mjolnir-mbx-shim' "$path"; then
        rm -f -- "$path"
    fi
done
`

// mbxContainer is the engine, container and SSH host (nil when local) of a
// session that can hold the build-cache mbx.
func mbxContainer(locator *core.TargetsTargetLocator) (engine string, containerID string, ssh *core.SshTarget, err error) {
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalPodman:
		return "podman", locator.ContainerID, nil, nil
	case core.TargetsTargetLocatorLocalDocker:
		return "docker", locator.ContainerID, nil, nil
	case core.TargetsTargetLocatorSshPodman:
		return "podman", locator.ContainerID, locator.SSH, nil
	case core.TargetsTargetLocatorSshDocker:
		return "docker", locator.ContainerID, locator.SSH, nil
	case core.TargetsTargetLocatorSshNitro:
		return core.MicrovmEngine, locator.ContainerID, locator.SSH, nil
	}
	return "", "", nil, errors.New("the build cache is only installed into Podman, Docker and Nitro sessions")
}

// InstallMbxShims installs marked Cargo and mbx launchers pointing at the
// staged binary. configuration is nil when no shared machine configuration
// is linked.
func InstallMbxShims(executor core.CommandExecutor, locator *core.TargetsTargetLocator, workerRoot string, mbxBinary string, configuration *string, configRoots []string) error {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs install_mbx_shims")
}

// RemoveGeneratedMbxShims removes the marked mbx shims after cache
// verification failed.
func RemoveGeneratedMbxShims(executor core.CommandExecutor, locator *core.TargetsTargetLocator, workerRoot string) error {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs remove_generated_mbx_shims")
}

// VerifyMbxBinary checks that the synchronized mbx inside the session
// container reports expectedVersion.
func VerifyMbxBinary(executor core.CommandExecutor, locator *core.TargetsTargetLocator, mbxBinary string, expectedVersion string) error {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs verify_mbx_binary")
}

func reportedMbxVersion(output core.CommandOutput, command *core.CommandSpec) (string, error) {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs reported_mbx_version")
}

// mbxConfigurationArgv upgrades configuration without touching an executable
// a live build may use.
func mbxConfigurationArgv(engine string, containerID string, configuration string, configRoots []string) []string {
	args := []string{engine, "exec", "-i", containerID, "sh", "-c", mbxConfigScript, "mj-mbx-config", configuration}
	return append(args, configRoots...)
}

// LinkLegacyMbxConfiguration preserves legacy private mbx installations and
// links their shared config in the same container command that distinguishes
// them from generated shims. It reports whether the container has a legacy
// installation.
func LinkLegacyMbxConfiguration(executor core.CommandExecutor, locator *core.TargetsTargetLocator, workerRoot string, configuration string, configRoots []string) (bool, error) {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs link_legacy_mbx_configuration")
}

// StagedWorkerProfile is the staged files a worker's profile install
// uploads.
type StagedWorkerProfile struct {
	Launch      string
	Ownership   string
	Profile     string
	SharedLogin *SharedLogin
	SharedPaths []string
}

// InstallWorkerFiles verifies the worker build, installs the staged launch
// configuration, ownership marker and profile, then the worker binary as
// `hel`.
func InstallWorkerFiles(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, workerRoot string, profileHome string, worker WorkerInstall, launchConfig string, ownership string, profileStage string, sharedLogin *SharedLogin, sharedPaths []string) error {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs install_worker_files")
}

// InstallWorkerProfileFiles installs the staged launch configuration,
// ownership marker and profile on the session's target.
func InstallWorkerProfileFiles(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, workerRoot string, profileHome string, staged StagedWorkerProfile) error {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs install_worker_profile_files")
}

// ClearProfileConfiguration refreshes configuration without removing native
// history or skills the harness maintains itself. Absent destination
// settings must not inherit the source's.
func ClearProfileConfiguration(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, home string, harness core.HarnessKind) error {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs clear_profile_configuration")
}

// UploadWorkerFile streams a file into private staging, where the caller
// owns partial-file cleanup. OpenSSH's scp reads its input itself, outside
// the lifecycle's byte accounting. Carry the bytes on the same supervised
// stdin path as profiles.
func UploadWorkerFile(executor core.CommandExecutor, ssh *core.SshTarget, source string, destination string, purpose string) error {
	command := core.SSHCommand(core.SshTrafficClassBulk, ssh, []string{
		"sh", "-c", "umask 077; exec cat > \"$1\"", "mj-upload", destination,
	}).WithPurpose(purpose)
	input, err := os.Open(source)
	if err != nil {
		return fmt.Errorf("open worker upload %s: %w", source, err)
	}
	defer input.Close()
	output, err := executor.ExecuteWithStdin(&command, input)
	if err != nil {
		return err
	}
	_, err = core.CheckedCommandOutput(&command, output)
	return err
}

// InstallWorkerOverSSH installs the session-private SSH metadata; binary
// installation is shared with creation, upgrades and recovery in
// InstallWorkerBinary.
func InstallWorkerOverSSH(executor core.CommandExecutor, ssh *core.SshTarget, workerRoot string, profileHome string, launchConfig string, ownership string, profileStage string, sharedPaths []string) error {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs install_worker_over_ssh")
}

const profileContentStore = ".cache/mjolnir/profile-content"

// profileContentMissing is the exit status of installProfileContent when the
// host lacks content the archive only linked to.
const profileContentMissing = 75

// profileStageMissing: a retired/incomplete stage consumes the same existing
// retry slot as a missing content pin, but needs a fresh host query rather
// than missing keys.
const profileStageMissing = 76

// profileUpload is one active private profile staging directory.
// Registration and collection take the same store gate. A collector's
// snapshot cannot predate a directory that an active attempt is about to
// use.
type profileUpload struct {
	store string
	token string
}

// profileUploads is the active staging tokens by store registry key.
type profileUploads struct {
	sync.Mutex
	tokens map[string]map[string]struct{}
}

// activeProfileUploads is Rust's lazily created `profile_uploads()`.
var activeProfileUploads = &profileUploads{tokens: map[string]map[string]struct{}{}}

func registerProfileUpload(store string, token string) *profileUpload {
	activeProfileUploads.Lock()
	defer activeProfileUploads.Unlock()
	if activeProfileUploads.tokens[store] == nil {
		activeProfileUploads.tokens[store] = map[string]struct{}{}
	}
	activeProfileUploads.tokens[store][token] = struct{}{}
	return &profileUpload{store: store, token: token}
}

// activeProfileUploadTokens is the tokens registered for store, in no
// particular order.
func activeProfileUploadTokens(store string) []string {
	activeProfileUploads.Lock()
	defer activeProfileUploads.Unlock()
	tokens := make([]string, 0, len(activeProfileUploads.tokens[store]))
	for token := range activeProfileUploads.tokens[store] {
		tokens = append(tokens, token)
	}
	return tokens
}

// drop is Rust's Drop for ProfileUpload: it unregisters the token.
func (u *profileUpload) drop() {
	activeProfileUploads.Lock()
	defer activeProfileUploads.Unlock()
	if tokens, ok := activeProfileUploads.tokens[u.store]; ok {
		delete(tokens, u.token)
		if len(tokens) == 0 {
			delete(activeProfileUploads.tokens, u.store)
		}
	}
}

// profileScript reuses teardown's process identity, including boot/start
// identity on Linux. A failed identity query never authorizes deletion.
func profileScript(body string) string {
	return core.ProcessBirthScript + "\nmj_profile_stage_missing_status=" + strconv.Itoa(profileStageMissing) +
		"\n" + profileOwnerScript + "\n" + body
}

const profileOwnerScript = `mj_profile_stage_missing() {
    echo "stage removed by a collector or incomplete: $work" >&2
    return "$mj_profile_stage_missing_status"
}
mj_profile_check_marker() {
    marker=$(cat -- "$1" 2>/dev/null) || { mj_profile_stage_missing; return "$mj_profile_stage_missing_status"; }
    [ "$marker" = "${work##*/}" ] || mj_profile_stage_missing
}
mj_profile_claim() {
    birth=$(mj_process_birth "$$") || birth=
    if [ -z "$birth" ]; then
        echo "cannot identify profile stage process $$ in $work" >&2
        return 1
    fi
    # The first write of a later phase replaces its owner atomically. Never
    # recreate a collected directory, or treat a partial receive as complete.
    if ! { printf '%s\n%s\n' "$$" "$birth" > "$work/.owner.next" &&
           mv -- "$work/.owner.next" "$work/.owner"; }; then
        mj_profile_stage_missing; return "$mj_profile_stage_missing_status"
    fi
    mj_profile_check_marker "$work/.token"
}
mj_profile_quiescent() {
    # Foreground payload work has finished. Match the exact owning identity,
    # so a killed later phase cannot inherit its predecessor's completion.
    if ! { cat -- "$work/.owner" > "$work/.quiescent.next" &&
           mv -- "$work/.quiescent.next" "$work/.quiescent"; }; then
        mj_profile_stage_missing; return "$mj_profile_stage_missing_status"
    fi
}
mj_profile_warn_unknown() {
    recorded=$(tr '\n' ':' < "$1/.owner" 2>/dev/null) || recorded=unavailable
    echo "profile staging ownership unknown or work not quiescent; retaining $1 (owner $recorded)" >&2
}
mj_profile_owner_state() {
    pid=$(sed -n '1p' "$1/.owner" 2>/dev/null) || pid=
    birth=$(sed -n '2p' "$1/.owner" 2>/dev/null) || birth=
    case $pid in *[!0123456789]*|'') printf unknown; return ;; esac
    [ -n "$birth" ] || { printf unknown; return; }
    current=$(mj_process_birth "$pid" 2>/dev/null) || current=
    if [ -n "$current" ]; then
        if [ "$current" = "$birth" ]; then printf alive; return; fi
    else
        # ps's documented empty selection is status 1 without output/errors.
        # An unreadable birth alone does not prove absence.
        status=0
        selected=$(LC_ALL=C ps -o pid= -p "$pid" 2> "$2") || status=$?
        if [ "$status" != 1 ] || [ -n "$selected" ] || [ -s "$2" ]; then
            printf unknown; return
        fi
    fi
    # A dead shell alone does not prove its tar/cp children stopped. Only
    # that exact owner can attest that all foreground work finished.
    if cmp -s "$1/.owner" "$1/.quiescent"; then printf dead; else printf unknown; fi
}`

// queryProfileContent: match store.rs's COLLECT and COLLECT_INTERVAL: retain
// content for a day and collect at most hourly. Touch requested content
// before collecting it.
const queryProfileContent = `set -eu
umask 077
store="$HOME/$1"; shift
mkdir -p -- "$store"
chmod -- 700 "$store"
present=$(mktemp "$store/.present-XXXXXXXXXX")
trap 'rm -f -- "$present"' EXIT
while IFS= read -r key; do
    if [ -f "$store/$key" ]; then
        printf '%s\n' "$key" >> "$present"
    else
        printf '%s\n' "$key"
    fi
done
(cd "$store" && xargs sh -c '[ "$#" -eq 0 ] || touch -- "$@"' sh) < "$present"
if [ ! -f "$store/.collected" ] || [ -n "$(find "$store/.collected" -mmin +60 -print)" ]; then
    find "$store" -mindepth 1 -maxdepth 1 -type f -mmin +1440 -exec rm -f -- {} +
    # Upload and pin directories that an interrupted run left behind.
    find "$store" -mindepth 1 -maxdepth 1 -type d ! -name '.stage-*' ! -name '.retired-*' -mmin +1440 -exec rm -rf -- {} +
    touch -- "$store/.collected"
fi
# Private staging is reclaimed by ownership, never by age. The caller holds
# the store gate and passes its live registry from that same locked view.
mj_profile_restore() {
    if [ -e "$2" ] || ! mv -- "$1" "$2"; then
        echo "could not restore profile staging; retaining $1" >&2
    fi
}
for work in "$store"/.stage-* "$store"/.retired-*; do
    [ -d "$work" ] || continue
    [ "${work##*/}" != .stage-negotiated ] || continue
    name=$(cat -- "$work/.token" 2>/dev/null) || name=
    case $name in .stage-*) ;; *)
        mj_profile_warn_unknown "$work"; continue ;;
    esac
    case $name in *[!abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-]*)
        mj_profile_warn_unknown "$work"; continue ;;
    esac
    token=${name#.stage-}
    active=0
    for live in "$@"; do [ "$live" != "$token" ] || active=1; done
    [ "$active" = 0 ] || continue
    snapshot=$(cat -- "$work/.owner" 2>/dev/null) || snapshot=
    state=$(mj_profile_owner_state "$work" "$present")
    case ${work##*/} in .retired-*)
        case $state in
            dead) rm -rf -- "$work" ;;
            alive) mj_profile_restore "$work" "$store/$name" ;;
            *) mj_profile_warn_unknown "$work" ;;
        esac
        continue ;;
    esac
    if [ "$state" = alive ]; then continue; fi
    if [ "$state" != dead ] || [ "${work##*/}" != "$name" ]; then
        mj_profile_warn_unknown "$work"; continue
    fi
    # Claim and retirement contend on the original directory path. Once it is
    # retired no later phase can install an owner marker or begin using it.
    retired=$(mktemp -d "$store/.retired-XXXXXXXXXX")
    rmdir -- "$retired"
    mv -- "$work" "$retired" 2>/dev/null || continue
    after=$(cat -- "$retired/.owner" 2>/dev/null) || after=
    state=$(mj_profile_owner_state "$retired" "$present")
    if [ "$after" != "$snapshot" ] || [ "$state" = alive ]; then
        mj_profile_restore "$retired" "$work"
    elif [ "$state" = dead ]; then
        rm -rf -- "$retired"
    else
        mj_profile_warn_unknown "$retired"
    fi
done`

// stageProfileContent: the archive holds the profile under `profile/`, with
// private files and a relative symlink per shared path into `.mj-store`;
// beside it, outside the profile's names, `.mj-content/<key>` for content the
// host lacked and `.mj-keys` (every shared key, one per line). This script
// extracts it into a private work directory and points `.mj-store` there at
// the store. One dereferencing `cp -RL` then materializes the whole profile,
// so the cost does not grow with one process per file.
const stageProfileContent = `set -eu
umask 077
work="$HOME/$1"
mkdir -p -- "$work"
trap 'rm -rf -- "$work"' EXIT
printf '%s\n' "${work##*/}" > "$work/.token"
mj_profile_claim
tar -xzf - --no-same-owner --no-same-permissions -C "$work"
printf '%s\n' "${work##*/}" > "$work/.received"
mj_profile_quiescent
trap - EXIT`

const installProfileContent = `set -eu
umask 077
destination=$1
store="$HOME/$2"
work="$HOME/$3"
mj_profile_claim
mj_profile_check_marker "$work/.received"
[ -d "$work/profile" ] && [ -f "$work/.mj-keys" ] || { mj_profile_stage_missing; exit "$mj_profile_stage_missing_status"; }
rm -rf -- "$destination"
mkdir -p -- "$(dirname -- "$destination")"
chmod -- 700 "$store"
upload=
pins=
trap 'rm -rf -- "$work" ${upload:+"$upload"} ${pins:+"$pins"}' EXIT
tree="$work/profile"
content="$work/.mj-content"
if [ -d "$content" ]; then
    case $(uname -s) in Darwin) sum='shasum -a 256';; *) sum=sha256sum;; esac
    expected=$(cd "$content" && find . -type f | wc -l)
    upload="$work/.upload"
    mkdir -- "$upload"
    find "$content" -type f -exec sh -c 'cp -- "$@" "$0"' "$upload" {} +
    # Verify the copies that will be published, in one digest pass: a
    # truncated copy fails its digest and a missing one fails the count.
    # Relative names (./<hex key>) keep the store path out of the output,
    # where sha256sum would escape a backslash or newline in it.
    (cd "$upload" && find . -type f -exec $sum -- {} +) > "$work/sums"
    verified=$(awk '{ key = $0; sub(/.*\//, "", key); digest = key; sub(/\.x$/, "", digest);
        if ($1 != digest) { print "profile content digest mismatch: " key > "/dev/stderr"; bad = 1 } n++ }
        END { if (bad) exit 1; print n + 0 }' "$work/sums")
    [ "$verified" -eq "$expected" ] || { echo "verified $verified of $expected profile content files" >&2; exit 1; }
    find "$upload" -type f ! -name '*.x' -exec chmod -- 600 {} +
    find "$upload" -type f -name '*.x' -exec chmod -- 700 {} +
    # Each rename publishes one verified file atomically; rmdir proves all moved.
    find "$upload" -type f -exec sh -c 'mv -f -- "$@" "$0"' "$store" {} +
    rmdir -- "$upload"
fi
# Pin every needed file with a hard link in a private directory of the store
# (always the same filesystem), then check and copy from the pins: content
# removed from the store afterwards cannot change what this run decided.
pins="$work/.pins"
mkdir -- "$pins"
ln -s -- "$pins" "$work/.mj-store"
missing=
if [ -f "$work/.mj-keys" ]; then
    (cd "$store" && xargs sh -c '[ "$#" -eq 0 ] || ln -- "$@" "$0"' "$pins") < "$work/.mj-keys" || :
    while IFS= read -r key; do
        [ -f "$pins/$key" ] || missing="$missing$key
"
    done < "$work/.mj-keys"
fi
if [ -n "$missing" ]; then
    printf '%s' "$missing"
    exit 75
fi
# Materialization uses these private pins after releasing the store owner.
printf '%s\n' "${work##*/}" > "$work/.published"
mj_profile_quiescent
trap - EXIT`

const materializeProfileContent = `set -eu
umask 077
work="$HOME/$1"; destination=$2; pins="$work/.pins"
mj_profile_claim
mj_profile_check_marker "$work/.published"
[ -d "$work/profile" ] && [ -d "$pins" ] && [ -f "$work/.mj-keys" ] || { mj_profile_stage_missing; exit "$mj_profile_stage_missing_status"; }
while IFS= read -r key; do
    [ -f "$pins/$key" ] || { mj_profile_stage_missing; exit "$mj_profile_stage_missing_status"; }
done < "$work/.mj-keys"
trap 'rm -rf -- "$work" "$pins"' EXIT
cp -RL -- "$work/profile" "$destination" || { echo "could not copy stored profile content into $destination" >&2; exit 1; }
mj_profile_quiescent`

// profileArchiveEntry is one entry of a staged profile, relative to its
// root. key is the content key of a shared regular file (its SHA-256, with
// `.x` appended when it is executable), nil for a private file or a
// directory.
type profileArchiveEntry struct {
	path      string
	directory bool
	key       *string
}

// profileArchiveEntries walks source, materializing staged links, and keys
// the regular files below sharedPaths. The root itself is the first entry,
// with an empty path.
func profileArchiveEntries(source string, sharedPaths []string) ([]profileArchiveEntry, error) {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs profile_archive_entries")
}

// profileContentKeys is the content keys the host reported, one per line;
// every one must be among expected.
func profileContentKeys(output []byte, expected map[string]struct{}) (map[string]struct{}, error) {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs profile_content_keys")
}

// buildProfileArchive writes the legacy profile archive into a private
// temporary file, rewound to its start: the profile under `profile/`, the
// missing content under `.mj-content/`, and `.mj-keys`.
func buildProfileArchive(source string, entries []profileArchiveEntry, missing map[string]struct{}) (*os.File, error) {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs build_profile_archive")
}

// uploadProfileArchive transfers the staged profile through the receiver
// negotiation, or through the legacy archive when the host lacks the
// receiver runtime.
func uploadProfileArchive(executor core.CommandExecutor, ssh *core.SshTarget, source string, destination string, sharedPaths []string) error {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs upload_profile_archive")
}

// uploadLegacyProfileArchive: one transfer per shared content set; unrelated
// profiles transfer together. Only querying/collecting and
// publishing/pinning share the store-wide gate. The destination is staging
// only: callers copy it into a private profile, then remove it.
func uploadLegacyProfileArchive(executor core.CommandExecutor, ssh *core.SshTarget, source string, destination string, sharedPaths []string) error {
	panic("unported: mj-worker-staging/src/worker_binary/install.rs upload_legacy_profile_archive")
}
