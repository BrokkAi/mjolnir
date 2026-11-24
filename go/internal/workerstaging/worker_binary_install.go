// Port of mj-worker-staging/src/worker_binary/install.rs.
//
// Rust declares profile_transfer.rs as a submodule of this one; in Go it is a
// file of the same package (worker_binary_profile_transfer.go), so
// ProfileContentManifest needs no re-export here.

package workerstaging

import (
	"archive/tar"
	"compress/gzip"
	"crypto/sha256"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
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
	// Whether the workspace sits on storage of its own that the guidance
	// should name: a Podman volume or helper path, or a Nitro sandbox's
	// volume.
	var workspaceMounted bool
	switch target.Kind {
	case core.TargetsTargetLocatorLocalPodman, core.TargetsTargetLocatorSshPodman:
		workspaceMounted = target.WorkspaceStorage.EffectiveKind() != core.TargetsPodmanWorkspaceLocatorContainerLayer
	case core.TargetsTargetLocatorSshNitro:
		workspaceMounted = true
	case core.TargetsTargetLocatorLocalDocker, core.TargetsTargetLocatorSshDocker,
		core.TargetsTargetLocatorAppleContainer, core.TargetsTargetLocatorSshAppleContainer:
		workspaceMounted = false
	default:
		return nil
	}
	const heading = "## Mjolnir container storage\n\n"
	var guidance strings.Builder
	guidance.WriteString(heading)
	if nativeTemporaryStorage {
		guidance.WriteString("`/tmp` is a private disk-backed volume outside the container's writable overlay. Use it for disposable temporary files. Its contents are excluded from checkpoints and are lost when the session stops or restarts.\n\n")
	}
	if workspaceMounted {
		workspace := core.ContainerWorkspaceRoot(workspace)
		fmt.Fprintf(&guidance, "The workspace storage is mounted at `%s`. ", workspace)
		if workspace != core.ContainerWorkspace {
			guidance.WriteString("The parent `/workspace` directory itself is on the container's writable overlay. ")
		}
		guidance.WriteString("Keep durable results inside the project repository directories; arbitrary files elsewhere on the workspace volume are not checkpointed.\n")
	}
	if guidance.String() != heading {
		if err := AppendStagedInstructions(harness, destination, guidance.String()); err != nil {
			return err
		}
	}
	if len(attachments) == 0 {
		return nil
	}
	var external strings.Builder
	external.WriteString("## Mjolnir external attachments\n\nThese host directories are attached at the container paths listed below. External attachments are not included in project workspace checkpoints.\n\n")
	for _, attachment := range attachments {
		var behavior string
		switch attachment.Access {
		case core.MountAccessRo:
			behavior = "read-only; you can read these files but cannot change them"
		case core.MountAccessCow:
			behavior = "copy-on-write; changes are private to this container, never change the host directory, and are lost when the container is removed or recreated"
		case core.MountAccessRw:
			behavior = "read-write; changes write directly to the host directory and persist after the container is removed"
		}
		fmt.Fprintf(&external, "- Container `%s` from host `%s`: `%s` (%s).\n",
			attachment.Destination, attachment.Source, attachment.Access.Label(), behavior)
	}
	return AppendStagedInstructions(harness, destination, external.String())
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
	path := filepath.Join(destination, harness.AgentInstructionsFile())
	separator := ""
	contents, err := os.ReadFile(path)
	switch {
	case err == nil:
		if !utf8.Valid(contents) {
			// Rust's read_to_string refuses text that is not UTF-8.
			return errors.New("stream did not contain valid UTF-8")
		}
		existing := string(contents)
		switch {
		case strings.Contains(existing, text):
			return nil
		case existing != "" && !strings.HasSuffix(existing, "\n"):
			separator = "\n\n"
		case existing != "":
			separator = "\n"
		}
	case errors.Is(err, fs.ErrNotExist):
	default:
		return rustIOError(err)
	}
	// This is our fresh copy: make it owner-writable without changing the
	// source profile or granting write access to other users. Windows files
	// carry only a read-only attribute, which Go's owner-write bit clears.
	info, err := os.Stat(path)
	switch {
	case err == nil:
		mode := info.Mode() & (fs.ModePerm | fs.ModeSetuid | fs.ModeSetgid | fs.ModeSticky)
		if mode&0o200 == 0 {
			if err := os.Chmod(path, mode|0o200); err != nil {
				return fmt.Errorf("make staged harness instructions %s writable: %w", path, rustIOError(err))
			}
		}
	case errors.Is(err, fs.ErrNotExist):
	default:
		return rustIOError(err)
	}
	file, err := os.OpenFile(path, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o666)
	if err != nil {
		return fmt.Errorf("open staged harness instructions %s: %w", path, rustIOError(err))
	}
	defer file.Close()
	if _, err := file.WriteString(separator); err != nil {
		return rustIOError(err)
	}
	if _, err := file.WriteString(text); err != nil {
		return rustIOError(err)
	}
	return nil
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
	if !(core.HostRunsWorkers &&
		backend.Kind == core.TargetsTargetLocatorLocalBare &&
		profile.Kind.SharesLocalLogin() &&
		profile.AuthScheme().UsesNativeLoginFile()) {
		return nil
	}
	// A relative link would resolve against the worker's home, not the
	// directory the profile home is relative to; without an absolute path
	// the worker keeps a copy.
	file, ok := sharedLoginAbsolutePath(profile.AuthenticationMarker())
	if !ok {
		return nil
	}
	return &SharedLogin{Name: profile.Kind.CredentialFileName(), File: file}
}

// sharedLoginAbsolutePath is Rust's std::path::absolute on Unix: a relative path is
// joined to the current directory without resolving links or `..`, and
// leading `.` components are dropped. False for an empty path or an
// unreadable current directory.
func sharedLoginAbsolutePath(path string) (string, bool) {
	if path == "" {
		return "", false
	}
	if filepath.IsAbs(path) {
		return path, true
	}
	current, err := os.Getwd()
	if err != nil {
		return "", false
	}
	for strings.HasPrefix(path, "./") {
		path = strings.TrimLeft(strings.TrimPrefix(path, "."), "/")
	}
	if path == "." || path == "" {
		return current, true
	}
	return strings.TrimSuffix(current, string(filepath.Separator)) + string(filepath.Separator) + path, true
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
		return rustIOError(err)
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
	link := filepath.Join(home, login.Name)
	if err := installRemoveFile(link); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("remove staged login copy %s: %w", link, rustIOError(err))
	}
	// Only Unix machines run local workers (`HOST_RUNS_WORKERS`).
	var linked error
	if runtime.GOOS == "windows" {
		linked = errors.New("linking a staged login needs a Unix file system")
	} else if err := os.Symlink(login.File, link); err != nil {
		linked = rustIOError(err)
	}
	if linked != nil {
		return fmt.Errorf("link %s to the profile login %s: %w", link, login.File, linked)
	}
	return nil
}

func removeLink(path string) error {
	if info, err := os.Lstat(path); err == nil && info.Mode()&os.ModeSymlink != 0 {
		if err := installRemoveFile(path); err != nil {
			return fmt.Errorf("remove the earlier link %s: %w", path, rustIOError(err))
		}
	}
	return nil
}

// installRemoveFile is Rust's std::fs::remove_file: it unlinks a file or a
// link and, unlike os.Remove, never removes a directory.
func installRemoveFile(path string) error {
	if err := syscall.Unlink(path); err != nil {
		return &fs.PathError{Op: "remove", Path: path, Err: err}
	}
	return nil
}

// installCopyFile is Rust's std::fs::copy: the contents and permissions of
// source, written over destination.
func installCopyFile(source string, destination string) error {
	input, err := os.Open(source)
	if err != nil {
		return err
	}
	defer input.Close()
	info, err := input.Stat()
	if err != nil {
		return err
	}
	if !info.Mode().IsRegular() {
		return errors.New("the source path is neither a regular file nor a symlink to a regular file")
	}
	mode := info.Mode() & (fs.ModePerm | fs.ModeSetuid | fs.ModeSetgid | fs.ModeSticky)
	output, err := os.OpenFile(destination, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, mode.Perm())
	if err != nil {
		return err
	}
	if _, err := io.Copy(output, input); err != nil {
		output.Close()
		return err
	}
	if err := output.Chmod(mode); err != nil {
		output.Close()
		return err
	}
	return output.Close()
}

// profileCopySlots is the number of directory entries copied at once beside
// the caller's own: Rust recurses with rayon, whose global pool has one
// thread per CPU.
var profileCopySlots = make(chan struct{}, runtime.GOMAXPROCS(0))

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
	if _, err := os.Lstat(source); err != nil {
		return fmt.Errorf("read staged profile entry metadata %s: %w", source, rustIOError(err))
	}
	metadata, err := os.Stat(source)
	if errors.Is(err, fs.ErrNotExist) {
		// The entry exists but its link target does not; staging the rest
		// of the profile is more useful than failing on a stale link.
		slog.Warn("skipping staged profile entry whose symlink target is missing", "source", source)
		return nil
	}
	if err != nil {
		return fmt.Errorf("read staged profile entry metadata %s: %w", source, rustIOError(err))
	}
	if metadata.Mode().IsRegular() {
		if parent := filepath.Dir(destination); parent != destination {
			if err := os.MkdirAll(parent, 0o777); err != nil {
				return fmt.Errorf("create staged profile directory %s: %w", parent, rustIOError(err))
			}
		}
		if err := installCopyFile(source, destination); err != nil {
			return fmt.Errorf("copy staged profile file %s to %s: %w", source, destination, rustIOError(err))
		}
		return nil
	}
	if !metadata.IsDir() {
		return nil
	}
	canonical, err := filepath.EvalSymlinks(source)
	if err == nil {
		canonical, err = filepath.Abs(canonical)
	}
	if err != nil {
		return fmt.Errorf("resolve staged profile directory %s: %w", source, rustIOError(err))
	}
	if _, ok := entered[canonical]; ok {
		slog.Warn("skipping staged profile directory that links back into itself", "source", source, "target", canonical)
		return nil
	}
	branch := make(map[string]struct{}, len(entered)+1)
	for path := range entered {
		branch[path] = struct{}{}
	}
	branch[canonical] = struct{}{}
	if err := os.MkdirAll(destination, 0o777); err != nil {
		return fmt.Errorf("create staged profile directory %s: %w", destination, rustIOError(err))
	}
	directory, err := os.Open(source)
	if err != nil {
		return fmt.Errorf("list staged profile directory %s: %w", source, rustIOError(err))
	}
	entries, err := directory.ReadDir(-1)
	directory.Close()
	if err != nil {
		return fmt.Errorf("read staged profile directory entries in %s: %w", source, rustIOError(err))
	}
	// Sibling entries in one directory are independent, so recurse in
	// parallel; this is the level most likely to hold many files (e.g. a
	// skills or plugins tree). An entry runs on its own goroutine when a
	// slot is free and on this one otherwise, so a nested walk never waits
	// for a slot its own ancestors hold.
	var (
		group    sync.WaitGroup
		failure  sync.Mutex
		firstErr error
	)
	record := func(err error) {
		if err == nil {
			return
		}
		failure.Lock()
		if firstErr == nil {
			firstErr = err
		}
		failure.Unlock()
	}
	for _, entry := range entries {
		child := filepath.Join(source, entry.Name())
		if slices.ContainsFunc(excluded, func(path string) bool { return filepath.Clean(path) == child }) {
			continue
		}
		target := filepath.Join(destination, entry.Name())
		select {
		case profileCopySlots <- struct{}{}:
			group.Add(1)
			go func() {
				defer group.Done()
				defer func() { <-profileCopySlots }()
				record(CopyProfileEntryWithin(child, target, branch, excluded))
			}()
		default:
			record(CopyProfileEntryWithin(child, target, branch, excluded))
		}
	}
	group.Wait()
	if firstErr != nil {
		return firstErr
	}
	if err := os.Chmod(destination, metadata.Mode()&(fs.ModePerm|fs.ModeSetuid|fs.ModeSetgid|fs.ModeSticky)); err != nil {
		return fmt.Errorf("set permissions for staged profile directory %s: %w", destination, rustIOError(err))
	}
	return nil
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
	if !core.IsAbsoluteOnHostOrTarget(mbxBinary) {
		return fmt.Errorf("shared cache mbx path is not absolute: %s", mbxBinary)
	}
	bin := workerRoot + "/bin"
	engine, containerID, ssh, err := mbxContainer(locator)
	if err != nil {
		return err
	}
	quotedBinary := core.PosixQuote(mbxBinary)
	// mbx refuses a mise shim as the real Cargo, and a mise project image
	// (mj-dev and every `mj image build` image) has Cargo only behind mise's
	// shims. Under mise, take mise's environment for the build's own
	// directory first: it puts the real toolchain on PATH and pins
	// RUSTUP_TOOLCHAIN the way the shim would, per repository, including
	// tools that finished installing after the session started.
	cargoBody := "MBX_CARGO_SHIM_MODE=1\nMBX_CARGO_SHIM_PATH=$(command -v \"$0\")\nexport MBX_CARGO_SHIM_MODE MBX_CARGO_SHIM_PATH\nif command -v mise >/dev/null 2>&1; then\n    eval \"$(mise env -s bash 2>/dev/null)\" || true\nfi\nexec " + quotedBinary + " \"$@\""
	mbxBody := "exec " + quotedBinary + " \"$@\""
	args := []string{engine, "exec", containerID, "sh", "-c", installMbxShimsScript, "sh", bin, cargoBody, mbxBody}
	steps := []core.CommandStep{core.NewCommandStep("install the build-cache mbx shims", core.WithEngineArgv(args))}
	if configuration != nil {
		steps = append(steps, core.NewCommandStep("link the shared machine mbx configuration",
			mbxConfigurationArgv(engine, containerID, *configuration, configRoots)))
	}
	command := mbxCommand(ssh, core.StepScript(steps)).
		WithPurpose("install the shared-cache mbx shims").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageSyncing})
	_, err = targets.ExecuteChecked(executor, command)
	return err
}

// mbxCommand runs args on this machine, or on ssh's host when the container
// is remote.
func mbxCommand(ssh *core.SshTarget, args []string) core.CommandSpec {
	if ssh == nil {
		return core.NewCommandSpec(args[0], args[1:])
	}
	return core.SSHCommand(core.SshTrafficClassControl, ssh, args)
}

// RemoveGeneratedMbxShims removes the marked mbx shims after cache
// verification failed.
func RemoveGeneratedMbxShims(executor core.CommandExecutor, locator *core.TargetsTargetLocator, workerRoot string) error {
	engine, containerID, ssh, err := mbxContainer(locator)
	if err != nil {
		return err
	}
	args := core.WithEngineArgv([]string{engine, "exec", containerID, "sh", "-c", removeMbxShimsScript, "sh", workerRoot + "/bin"})
	command := mbxCommand(ssh, args).
		WithPurpose("remove marked mbx shims after cache verification failed").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageSyncing})
	_, err = targets.ExecuteChecked(executor, command)
	return err
}

// VerifyMbxBinary checks that the synchronized mbx inside the session
// container reports expectedVersion.
func VerifyMbxBinary(executor core.CommandExecutor, locator *core.TargetsTargetLocator, mbxBinary string, expectedVersion string) error {
	if !core.IsAbsoluteOnHostOrTarget(mbxBinary) {
		return fmt.Errorf("shared cache mbx path is not absolute: %s", mbxBinary)
	}
	engine, containerID, ssh, err := mbxContainer(locator)
	if err != nil {
		return err
	}
	args := core.WithEngineArgv([]string{engine, "exec", containerID, mbxBinary, "--version"})
	command := mbxCommand(ssh, args).
		WithPurpose("verify the synchronized mbx inside the session container").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageSyncing})
	output, err := executor.Execute(&command)
	if err != nil {
		return err
	}
	containerVersion, err := reportedMbxVersion(output, &command)
	if err != nil {
		return err
	}
	if expectedVersion != containerVersion {
		return fmt.Errorf("the container reports mbx %s, but the staged binary is mbx %s", containerVersion, expectedVersion)
	}
	return nil
}

func reportedMbxVersion(output core.CommandOutput, command *core.CommandSpec) (string, error) {
	if output.Status != 0 {
		return "", fmt.Errorf("%s failed with status %d: %s", command.Purpose, output.Status, strings.TrimSpace(lossyText(output.Stderr)))
	}
	fields := strings.Fields(lossyText(output.Stdout))
	if len(fields) == 0 {
		return "", errors.New("mbx --version returned no version")
	}
	return fields[len(fields)-1], nil
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
	var engine string
	var ssh *core.SshTarget
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalPodman:
		engine = "podman"
	case core.TargetsTargetLocatorLocalDocker:
		engine = "docker"
	case core.TargetsTargetLocatorSshPodman:
		engine, ssh = "podman", locator.SSH
	case core.TargetsTargetLocatorSshDocker:
		engine, ssh = "docker", locator.SSH
	default:
		return false, errors.New("the build cache is only installed into Podman and Docker containers")
	}
	args := []string{engine, "exec", locator.ContainerID, "sh", "-c", checkLegacyMbxAndLinkConfigScript, "sh", workerRoot + "/bin", configuration}
	args = append(args, configRoots...)
	command := mbxCommand(ssh, args).
		WithPurpose("inspect legacy mbx and relink its shared configuration").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageSyncing})
	output, err := targets.ExecuteChecked(executor, command)
	if err != nil {
		return false, err
	}
	switch string(output.Stdout) {
	case "legacy":
		return true, nil
	case "new":
		return false, nil
	}
	return false, fmt.Errorf("container mbx scheme inspection returned unexpected output: %s", lossyText(output.Stdout))
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
	if binary, ok := worker.HostBinary(); ok {
		if err := core.VerifyWorkerBuild(binary); err != nil {
			return err
		}
	}
	if err := InstallWorkerProfileFiles(executor, locator, sessionID, workerRoot, profileHome, StagedWorkerProfile{
		Launch:      launchConfig,
		Ownership:   ownership,
		Profile:     profileStage,
		SharedLogin: sharedLogin,
		SharedPaths: sharedPaths,
	}); err != nil {
		return err
	}
	return InstallWorkerBinary(executor, locator, sessionID, worker, "hel")
}

// InstallWorkerProfileFiles installs the staged launch configuration,
// ownership marker and profile on the session's target.
func InstallWorkerProfileFiles(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, workerRoot string, profileHome string, staged StagedWorkerProfile) error {
	launchConfig, ownership, profileStage := staged.Launch, staged.Ownership, staged.Profile
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalBare:
		if info, err := os.Stat(profileStage); err == nil && info.IsDir() {
			// A staged home that is a link to a profile home, left for a
			// session an earlier release started there, is replaced by a
			// directory of its own. Copying through it would write this
			// stage over the person's own configuration.
			if info, err := os.Lstat(profileHome); err == nil && info.Mode()&fs.ModeSymlink != 0 {
				if err := installRemoveFile(profileHome); err != nil {
					return fmt.Errorf("unlink the earlier session's profile home link %s: %w", profileHome, rustIOError(err))
				}
			}
			if err := os.MkdirAll(profileHome, 0o777); err != nil {
				return fmt.Errorf("create isolated local profile: %w", rustIOError(err))
			}
			if err := InstallLocalProfileStage(profileStage, profileHome, staged.SharedLogin); err != nil {
				return err
			}
		}
		for _, command := range []core.CommandSpec{
			core.NewCommandSpec("mkdir", []string{"-p", workerRoot}).
				WithPurpose("create local bare worker directory"),
			core.NewCommandSpec("cp", []string{launchConfig, workerRoot + "/launch.json"}).
				WithPurpose("install local worker launch configuration"),
			core.NewCommandSpec("cp", []string{ownership, workerRoot + "/ownership.json"}).
				WithPurpose("install local worker ownership marker"),
		} {
			if _, err := targets.ExecuteChecked(executor, command); err != nil {
				return err
			}
		}
	case core.TargetsTargetLocatorLocalPodman, core.TargetsTargetLocatorLocalDocker, core.TargetsTargetLocatorAppleContainer:
		var engine string
		switch locator.Kind {
		case core.TargetsTargetLocatorLocalPodman:
			engine = "podman"
		case core.TargetsTargetLocatorLocalDocker:
			engine = "docker"
		default:
			engine = "container"
		}
		containerID := locator.ContainerID
		launchInstalled := workerRoot + "/launch.json"
		ownershipInstalled := workerRoot + "/ownership.json"
		owned := []string{launchInstalled, ownershipInstalled, profileHome}
		commands := []core.CommandSpec{
			core.NewCommandSpec(engine, []string{"exec", containerID, "mkdir", "-p", workerRoot, profileHome}).
				WithPurpose("create target worker directories"),
			core.NewCommandSpec(engine, []string{"cp", launchConfig, containerID + ":" + launchInstalled}).
				WithPurpose("upload worker launch configuration"),
			core.NewCommandSpec(engine, []string{"cp", ownership, containerID + ":" + ownershipInstalled}).
				WithPurpose("upload worker ownership marker"),
			core.NewCommandSpec(engine, []string{"cp", profileStage + "/.", containerID + ":" + profileHome}).
				WithPurpose("upload harness profile allowlist"),
			core.NewCommandSpec(engine, ContainerUploadOwnershipArgs(engine, containerID, workerRoot, owned)).
				WithPurpose("match uploaded files to the worker directory owner"),
			core.NewCommandSpec(engine, []string{"exec", containerID, "chmod", "-R", "go-rwx", profileHome}).
				WithPurpose("restrict harness profile permissions"),
		}
		for _, command := range commands {
			if _, err := targets.ExecuteChecked(executor, command); err != nil {
				return err
			}
		}
	case core.TargetsTargetLocatorAwsEc2, core.TargetsTargetLocatorSshBare:
		if err := InstallWorkerOverSSH(executor, locator.SSH, workerRoot, profileHome, launchConfig, ownership, profileStage, staged.SharedPaths); err != nil {
			return err
		}
	case core.TargetsTargetLocatorSshPodman, core.TargetsTargetLocatorSshDocker,
		core.TargetsTargetLocatorSshAppleContainer, core.TargetsTargetLocatorSshNitro:
		ssh, containerID := locator.SSH, locator.ContainerID
		engine, ok := locator.ContainerEngine()
		if !ok {
			panic("matched a remote container target")
		}
		upload := core.RemoteUploadStaging + "/" + sessionID
		if _, err := targets.ExecuteChecked(executor,
			core.SSHCommand(core.SshTrafficClassControl, ssh, []string{"mkdir", "-p", upload}).
				WithPurpose("create remote upload staging")); err != nil {
			return err
		}
		for _, file := range []struct{ source, name string }{
			{launchConfig, "launch.json"},
			{ownership, "ownership.json"},
		} {
			if err := UploadWorkerFile(executor, ssh, file.source, upload+"/"+file.name, "upload remote container worker file"); err != nil {
				return err
			}
		}
		stagedProfile := upload + "/profile"
		if locator.Kind == core.TargetsTargetLocatorSshNitro {
			// A Nitro guest does not share its profile objects with the
			// hypervisor account or another guest.
			if err := uploadLegacyProfileArchive(executor, ssh, profileStage, stagedProfile, nil); err != nil {
				return err
			}
		} else if err := uploadProfileArchive(executor, ssh, profileStage, stagedProfile, staged.SharedPaths); err != nil {
			return err
		}
		launchInstalled := workerRoot + "/launch.json"
		ownershipInstalled := workerRoot + "/ownership.json"
		owned := []string{launchInstalled, ownershipInstalled, profileHome}
		remote := [][]string{
			{engine, "exec", containerID, "mkdir", "-p", workerRoot, profileHome},
			{engine, "cp", upload + "/launch.json", containerID + ":" + launchInstalled},
			{engine, "cp", upload + "/ownership.json", containerID + ":" + ownershipInstalled},
			// Apple's `container cp` copies a relative `dir/.` as `dir`
			// itself, nesting the profile one level too deep; only an
			// absolute source copies the contents, as Docker and Podman do
			// for either form. The staging path is home-relative.
			{"sh", "-c", fmt.Sprintf(`exec %s cp "$HOME/$1/." "$2"`, core.EngineShellWords(engine)),
				"mj-profile-copy", upload + "/profile", containerID + ":" + profileHome},
			append([]string{engine}, ContainerUploadOwnershipArgs(engine, containerID, workerRoot, owned)...),
			{engine, "exec", containerID, "chmod", "-R", "go-rwx", profileHome},
			{"rm", "-rf", "--", upload},
		}
		// Staging removal stays a final success-only step, as before.
		steps := make([]core.CommandStep, 0, len(remote))
		for _, args := range remote {
			steps = append(steps, core.NewCommandStep("install remote container worker", core.WithEngineArgv(args)))
		}
		if _, err := targets.ExecuteChecked(executor,
			core.SSHCommandOwned(core.SshTrafficClassBulk, ssh, core.StepScript(steps)).
				WithPurpose("install remote container worker")); err != nil {
			return err
		}
	}
	return nil
}

// ClearProfileConfiguration refreshes configuration without removing native
// history or skills the harness maintains itself. Absent destination
// settings must not inherit the source's.
func ClearProfileConfiguration(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string, home string, harness core.HarnessKind) error {
	paths := slices.Clone(ProfileConfigurationPaths(harness))
	paths = append(paths, harness.CredentialFileName())
	paths = append(paths, core.AwsCredentialsFiles[:]...)
	if harness == core.HarnessKindCodex {
		paths = append(paths, StagedCatalogFile)
	}
	slices.Sort(paths)
	paths = slices.Compact(paths)
	var script strings.Builder
	script.WriteString("set -eu\n")
	for _, relative := range paths {
		path := core.Join(home, relative)
		quoted := core.PosixQuote(core.Text(path))
		var protected []string
		for _, owned := range harness.HarnessOwnedSkillPaths() {
			tail, ok := pathStripPrefix(owned, relative)
			if !ok {
				continue
			}
			if components := pathComponents(tail); len(components) > 0 {
				protected = append(protected, core.PosixQuote(components[0]))
			}
		}
		if len(protected) == 0 {
			fmt.Fprintf(&script, "rm -rf -- %s\n", quoted)
		} else {
			fmt.Fprintf(&script, "if [ -L %s ]; then rm -f -- %s; else\nfor entry in %s/* %s/.[!.]* %s/..?*; do\ncase \"${entry##*/}\" in %s) continue ;; esac\nrm -rf -- \"$entry\"\ndone\nfi\n",
				quoted, quoted, quoted, quoted, quoted, strings.Join(protected, "|"))
		}
	}
	command, err := core.CommandOnLocator(core.SshTrafficClassControl, locator, sessionID,
		[]string{"sh", "-c", script.String()}, "replace profile configuration while retaining native state")
	if err != nil {
		return err
	}
	_, err = targets.ExecuteChecked(executor, command)
	return err
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
		return fmt.Errorf("open worker upload %s: %w", source, rustIOError(err))
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
	if _, err := targets.ExecuteChecked(executor,
		core.SSHCommand(core.SshTrafficClassControl, ssh, []string{"mkdir", "-p", workerRoot, profileHome}).
			WithPurpose("create SSH worker directories")); err != nil {
		return err
	}
	for _, file := range []struct{ source, remote string }{
		{launchConfig, workerRoot + "/launch.json"},
		{ownership, workerRoot + "/ownership.json"},
	} {
		if err := UploadWorkerFile(executor, ssh, file.source, file.remote, "upload SSH worker file"); err != nil {
			return err
		}
	}
	incomingProfile := profileHome + ".incoming"
	if err := uploadProfileArchive(executor, ssh, profileStage, incomingProfile, sharedPaths); err != nil {
		return err
	}
	for _, command := range []core.CommandSpec{
		core.SSHCommand(core.SshTrafficClassBulk, ssh, []string{"cp", "-R", incomingProfile + "/.", profileHome}).
			WithPurpose("install SSH harness profile allowlist"),
		core.SSHCommand(core.SshTrafficClassControl, ssh, []string{"rm", "-rf", "--", incomingProfile}).
			WithPurpose("remove SSH profile staging"),
		core.SSHCommand(core.SshTrafficClassControl, ssh, []string{"chmod", "-R", "go-rwx", profileHome}).
			WithPurpose("restrict SSH harness profile permissions"),
	} {
		if _, err := targets.ExecuteChecked(executor, command); err != nil {
			return err
		}
	}
	return nil
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
	var entries []profileArchiveEntry
	var visit func(path string) error
	visit = func(path string) error {
		absolute := filepath.Join(source, path)
		// Like the previous tar builder, materialize staged links.
		metadata, err := os.Stat(absolute)
		if err != nil {
			return rustIOError(err)
		}
		directory := metadata.IsDir()
		var key *string
		if !directory && slices.ContainsFunc(sharedPaths, func(shared string) bool { return pathStartsWith(path, shared) }) {
			if !metadata.Mode().IsRegular() {
				return fmt.Errorf("profile entry is not a regular file: %s", path)
			}
			digest, err := core.FileSHA256(absolute)
			if err != nil {
				return err
			}
			if metadata.Mode().Perm()&0o100 != 0 {
				digest += ".x"
			}
			key = &digest
		}
		entries = append(entries, profileArchiveEntry{path: path, directory: directory, key: key})
		if directory {
			children, err := os.ReadDir(absolute)
			if err != nil {
				return rustIOError(err)
			}
			for _, child := range children {
				if err := visit(filepath.Join(path, child.Name())); err != nil {
					return err
				}
			}
		}
		return nil
	}
	if err := visit(""); err != nil {
		return nil, err
	}
	return entries, nil
}

// profileContentKeys is the content keys the host reported, one per line;
// every one must be among expected.
func profileContentKeys(output []byte, expected map[string]struct{}) (map[string]struct{}, error) {
	text, err := installFromUTF8(output)
	if err != nil {
		return nil, fmt.Errorf("decode missing profile content: %w", err)
	}
	keys := map[string]struct{}{}
	for _, key := range installLines(text) {
		if _, ok := expected[key]; !ok {
			return nil, fmt.Errorf("host reported unexpected profile content key %s", core.RustDebugString(key))
		}
		keys[key] = struct{}{}
	}
	return keys, nil
}

// installFromUTF8 is Rust's `std::str::from_utf8`, failing with the text of
// Rust's Utf8Error.
func installFromUTF8(text []byte) (string, error) {
	index := 0
	for index < len(text) {
		first := text[index]
		if first < 0x80 {
			index++
			continue
		}
		width := 0
		switch {
		case first >= 0xC2 && first <= 0xDF:
			width = 2
		case first >= 0xE0 && first <= 0xEF:
			width = 3
		case first >= 0xF0 && first <= 0xF4:
			width = 4
		}
		if width == 0 {
			return "", fmt.Errorf("invalid utf-8 sequence of 1 bytes from index %d", index)
		}
		low, high := byte(0x80), byte(0xBF)
		switch first {
		case 0xE0:
			low = 0xA0
		case 0xED:
			high = 0x9F
		case 0xF0:
			low = 0x90
		case 0xF4:
			high = 0x8F
		}
		for offset := 1; offset < width; offset++ {
			if index+offset >= len(text) {
				return "", fmt.Errorf("incomplete utf-8 byte sequence from index %d", index)
			}
			next := text[index+offset]
			if offset == 1 && (next < low || next > high) || offset > 1 && (next < 0x80 || next > 0xBF) {
				return "", fmt.Errorf("invalid utf-8 sequence of %d bytes from index %d", offset, index)
			}
		}
		index += width
	}
	return string(text), nil
}

// installLines is Rust's `str::lines`: lines split on `\n`, each without a
// trailing `\r`, and no empty last line after a final newline.
func installLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(text, "\n"), "\n")
	for i, line := range lines {
		lines[i] = strings.TrimSuffix(line, "\r")
	}
	return lines
}

// buildProfileArchive writes the legacy profile archive into a private
// temporary file, rewound to its start: the profile under `profile/`, the
// missing content under `.mj-content/`, and `.mj-keys`.
func buildProfileArchive(source string, entries []profileArchiveEntry, missing map[string]struct{}) (*os.File, error) {
	file, err := profileArchiveSpool()
	if err != nil {
		return nil, fmt.Errorf("create private profile archive: %w", rustIOError(err))
	}
	finished := false
	defer func() {
		if !finished {
			file.Close()
		}
	}()
	// flate2's Compression::fast is level 1.
	gzipWriter, err := gzip.NewWriterLevel(file, gzip.BestSpeed)
	if err != nil {
		return nil, err
	}
	archive := tar.NewWriter(gzipWriter)
	sent := map[string]struct{}{}
	for _, entry := range entries {
		// Profile names live under `profile/`, so no profile file can
		// collide with `.mj-content` or `.mj-keys` at the archive root.
		path := profileArchiveName(entry.path)
		absolute := filepath.Join(source, entry.path)
		switch {
		case entry.directory:
			if err := appendProfileArchivePath(archive, absolute, path); err != nil {
				return nil, err
			}
		case entry.key != nil:
			key := *entry.key
			if _, needed := missing[key]; needed {
				if _, already := sent[key]; !already {
					sent[key] = struct{}{}
					if err := appendProfileArchivePath(archive, absolute, ".mj-content/"+key); err != nil {
						return nil, err
					}
				}
			}
			// Relative to the link's directory: one `..` per path component
			// reaches the work directory that holds `.mj-store`.
			depth := 0
			if entry.path != "" {
				depth = len(strings.Split(filepath.ToSlash(entry.path), "/"))
			}
			if err := archive.WriteHeader(&tar.Header{
				Typeflag: tar.TypeSymlink,
				Name:     path,
				Linkname: strings.Repeat("../", depth) + ".mj-store/" + key,
				Mode:     0o700,
				Format:   tar.FormatGNU,
			}); err != nil {
				return nil, rustIOError(err)
			}
		default:
			if err := appendProfileArchivePath(archive, absolute, path); err != nil {
				return nil, err
			}
		}
	}
	var keys []string
	for _, entry := range entries {
		if entry.key != nil {
			keys = append(keys, *entry.key)
		}
	}
	slices.Sort(keys)
	keys = slices.Compact(keys)
	var list strings.Builder
	for _, key := range keys {
		list.WriteString(key + "\n")
	}
	if err := archive.WriteHeader(&tar.Header{
		Typeflag: tar.TypeReg,
		Name:     ".mj-keys",
		Size:     int64(list.Len()),
		Mode:     0o600,
		Format:   tar.FormatGNU,
	}); err != nil {
		return nil, rustIOError(err)
	}
	if _, err := io.WriteString(archive, list.String()); err != nil {
		return nil, rustIOError(err)
	}
	if err := archive.Close(); err != nil {
		return nil, rustIOError(err)
	}
	if err := gzipWriter.Close(); err != nil {
		return nil, fmt.Errorf("finish profile archive: %w", rustIOError(err))
	}
	if _, err := file.Seek(0, io.SeekStart); err != nil {
		return nil, rustIOError(err)
	}
	finished = true
	return file, nil
}

// profileArchiveName is where a profile entry sits in the archive: under
// `profile/`, written with `/` between components as tar names are, and
// without a trailing separator for the root.
func profileArchiveName(path string) string {
	if path == "" {
		return "profile"
	}
	return "profile/" + filepath.ToSlash(path)
}

// appendProfileArchivePath is the tar crate's `append_path_with_name` on a
// builder that follows symlinks: absolute's metadata and, for a regular
// file, its contents, under name.
func appendProfileArchivePath(archive *tar.Writer, absolute, name string) error {
	info, err := os.Stat(absolute)
	if err != nil {
		return rustIOError(err)
	}
	header, err := tar.FileInfoHeader(info, "")
	if err != nil {
		return rustIOError(err)
	}
	header.Name = name
	header.Format = tar.FormatGNU
	header.ModTime = info.ModTime().Truncate(time.Second)
	header.AccessTime, header.ChangeTime = time.Time{}, time.Time{}
	header.Uname, header.Gname = "", ""
	if !info.Mode().IsRegular() {
		header.Size = 0
		return rustIOError(archive.WriteHeader(header))
	}
	file, err := os.Open(absolute)
	if err != nil {
		return rustIOError(err)
	}
	defer file.Close()
	if err := archive.WriteHeader(header); err != nil {
		return rustIOError(err)
	}
	if _, err := io.CopyN(archive, file, header.Size); err != nil {
		return rustIOError(err)
	}
	return nil
}

// uploadProfileArchive transfers the staged profile through the receiver
// negotiation, or through the legacy archive when the host lacks the
// receiver runtime.
func uploadProfileArchive(executor core.CommandExecutor, ssh *core.SshTarget, source string, destination string, sharedPaths []string) error {
	manifest, err := ProfileContentManifestFromStaged(source, sharedPaths)
	if err != nil {
		return err
	}
	transferred, err := manifest.TryTransfer(executor, ssh, &destination)
	if err != nil {
		return err
	}
	if transferred {
		return nil
	}
	return uploadLegacyProfileArchive(executor, ssh, source, destination, sharedPaths)
}

// uploadLegacyProfileArchive: one transfer per shared content set; unrelated
// profiles transfer together. Only querying/collecting and
// publishing/pinning share the store-wide gate. The destination is staging
// only: callers copy it into a private profile, then remove it.
func uploadLegacyProfileArchive(executor core.CommandExecutor, ssh *core.SshTarget, source string, destination string, sharedPaths []string) error {
	entries, err := profileArchiveEntries(source, sharedPaths)
	if err != nil {
		return err
	}
	keys := map[string]struct{}{}
	for _, entry := range entries {
		if entry.key != nil {
			keys[*entry.key] = struct{}{}
		}
	}
	host := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorSshBare, SSH: ssh, Workspace: ""}
	sortedKeys := make([]string, 0, len(keys))
	for key := range keys {
		sortedKeys = append(sortedKeys, key)
	}
	slices.Sort(sortedKeys)
	contentDigest := sha256.Sum256([]byte(strings.Join(sortedKeys, "\n")))
	gate := StoreGateFor(&host, profileContentStore+"/"+core.LowerHex(contentDigest[:]))
	storeGate := StoreGateFor(&host, profileContentStore)
	registryKey := StoreKey(&host, profileContentStore)
	syncing := core.ProvisionStage{Kind: core.ProvisionStageSyncing}
	queryMissing := func() (map[string]struct{}, error) {
		var input strings.Builder
		for _, key := range sortedKeys {
			input.WriteString(key + "\n")
		}
		store, err := LockGate(executor, storeGate, destination, "query and collect profile content")
		if err != nil {
			return nil, err
		}
		defer store.Release()
		args := []string{"sh", "-c", profileScript(queryProfileContent), "mj-profile-content", profileContentStore}
		args = append(args, activeProfileUploadTokens(registryKey)...)
		query := core.SSHCommand(core.SshTrafficClassControl, ssh, args).
			WithPurpose("query host profile content").
			WithWarnOnStderr().
			WithStage(syncing)
		output, err := executor.ExecuteWithStdin(&query, strings.NewReader(input.String()))
		if err != nil {
			return nil, err
		}
		output, err = core.CheckedCommandOutput(&query, output)
		if err != nil {
			return nil, err
		}
		return profileContentKeys(output.Stdout, keys)
	}
	missing, err := queryMissing()
	if err != nil {
		return err
	}
	// Cached profiles send only private bytes and need no content owner. If
	// content is missing, one uploader decides again after taking its owner:
	// the preceding uploader may already have supplied everything it needs.
	var contentOwner *GateGuard
	defer func() { contentOwner.Release() }()
	if len(missing) > 0 {
		contentOwner, err = LockGate(executor, gate, destination, "send profile content")
		if err != nil {
			return err
		}
	}
	if contentOwner != nil {
		missing, err = queryMissing()
		if err != nil {
			return err
		}
		if len(missing) == 0 {
			contentOwner.Release()
			contentOwner = nil
		}
	}
	// attemptTransfer sends the archive once. Its staging registration and
	// spool last until it returns, as Rust's live through one loop pass.
	attemptTransfer := func() (core.CommandOutput, error) {
		file, err := buildProfileArchive(source, entries, missing)
		if err != nil {
			return core.CommandOutput{}, err
		}
		defer file.Close()
		info, err := file.Stat()
		if err != nil {
			return core.CommandOutput{}, rustIOError(err)
		}
		slog.Info("send profile content", "bytes_sent", info.Size(), "objects_sent", len(missing), "legacy", true, "destination", destination)
		token, err := core.NewCommandID("profile-stage")
		if err != nil {
			return core.CommandOutput{}, err
		}
		work := profileContentStore + "/.stage-" + token
		store, err := LockGate(executor, storeGate, destination, "register private profile staging")
		if err != nil {
			return core.CommandOutput{}, err
		}
		active := registerProfileUpload(registryKey, token)
		store.Release()
		defer active.drop()
		output, transferErr := func() (core.CommandOutput, error) {
			command := core.SSHCommand(core.SshTrafficClassBulk, ssh, []string{
				"sh", "-c", profileScript(stageProfileContent), "mj-profile-stage", work,
			}).WithPurpose("send profile content").WithStage(syncing)
			// The byte stream takes as long as the link needs; an operation
			// deadline would cut a progressing transfer off (#1571). A
			// cancellation still stops it.
			output, err := executor.ExecuteWithStdinWithoutDeadline(&command, file)
			if err != nil {
				return core.CommandOutput{}, fmt.Errorf("stream staged harness profile archive: %w", err)
			}
			if _, err := core.CheckedCommandOutput(&command, output); err != nil {
				return core.CommandOutput{}, err
			}
			// Collection cannot unlink between publication and pinning. The
			// network transfer has already finished before taking this
			// owner.
			store, err := LockGate(executor, storeGate, destination, "publish and pin profile content")
			if err != nil {
				return core.CommandOutput{}, err
			}
			publish := core.SSHCommand(core.SshTrafficClassBulk, ssh, []string{
				"sh", "-c", profileScript(installProfileContent), "mj-profile-archive", destination, profileContentStore, work,
			}).WithPurpose("publish profile content").WithStage(syncing)
			output, err = executor.Execute(&publish)
			store.Release()
			if err != nil {
				return core.CommandOutput{}, err
			}
			if output.Status != 0 {
				return output, nil
			}
			materialize := core.SSHCommand(core.SshTrafficClassBulk, ssh, []string{
				"sh", "-c", profileScript(materializeProfileContent), "mj-profile-materialize", work, destination,
			}).WithPurpose("materialize profile content").WithStage(syncing)
			return executor.Execute(&materialize)
		}()
		if transferErr != nil || output.Status != 0 {
			slog.Warn("profile preparation failed; collector will reclaim staging after verified quiescence", "work", work)
		}
		return output, transferErr
	}
	for attempt := 0; attempt < 2; attempt++ {
		output, err := attemptTransfer()
		if err != nil {
			return err
		}
		if output.Status == 0 {
			return nil
		}
		if attempt == 0 && (output.Status == profileContentMissing || output.Status == profileStageMissing) {
			if output.Status == profileStageMissing {
				// Another daemon may collect the completed prior phase. A
				// fresh query and transfer use the existing retry slot.
				missing, err = queryMissing()
				if err != nil {
					return err
				}
			} else {
				missing, err = profileContentKeys(output.Stdout, keys)
				if err != nil {
					return err
				}
				if len(missing) == 0 {
					return errors.New("host reported missing profile content without any keys")
				}
			}
			continue
		}
		return fmt.Errorf("publish profile content failed with status %d: %s%s", output.Status,
			strings.TrimSpace(lossyText(output.Stderr)), strings.TrimSpace(lossyText(output.Stdout)))
	}
	panic("both upload attempts return or report failure")
}
