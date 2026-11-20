// Port of mj-core/src/harness_install.rs.

// Exact, target-local harness installations, including container fallbacks.

package core

import (
	"context"
	_ "embed"
	"errors"
	"fmt"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"
)

const (
	installLockFile    = ".install.lock"
	installLockTimeout = 15 * time.Minute
	// Bounds a harness command that hangs, including version probes; a slow
	// container start must not fail preparation, and the user can cancel it.
	harnessCommandTimeout     = 10 * time.Minute
	managedInstallTimeout     = 15 * time.Minute
	defaultNodeDistURL        = "https://nodejs.org/dist"
	artifactDownloadTimeout   = 10 * time.Minute
	maxArtifactBytes          = 512 * 1024 * 1024
	maxArtifactRedirects      = 10
	harnessInstallUserAgent   = "mj/" + PackageVersion
	artifactConnectTimeout    = 30 * time.Second
	lockFilePollInterval      = 25 * time.Millisecond
	managedGCInterval         = 30 * time.Second
	extractChunkBytes         = 1 << 20
	installedNodeMinimumMajor = 22
)

var (
	//go:embed assets/harnesses/codex/package.json
	codexPackageJSON []byte
	//go:embed assets/harnesses/codex/package-lock.json
	codexPackageLock []byte
	//go:embed assets/harnesses/claude/package.json
	claudePackageJSON []byte
	//go:embed assets/harnesses/claude/package-lock.json
	claudePackageLock []byte
	//go:embed assets/antigravity/runtime.json
	antigravityRuntimeJSON []byte
	//go:embed assets/muse/runtime.json
	museRuntimeJSON []byte
	//go:embed assets/opencode/runtime.json
	opencodeRuntimeJSON []byte
	//go:embed assets/node/runtime.json
	nodeRuntimeJSON []byte
	//go:embed assets/kimi/runtime.json
	kimiRuntimeJSON []byte
	//go:embed assets/grok/runtime.json
	grokRuntimeJSON []byte
)

type ManagedHarness struct {
	Command     string
	Args        []string
	Environment map[string]string
	// Entrypoint is the installed harness executable itself. For an adapter
	// that is launched through a shell bootstrap, this is the native harness
	// or ACP binary the bootstrap would exec; login fallbacks use it directly.
	Entrypoint string
	// CLIEntrypoint is the harness command-line program installed beside an
	// ACP adapter.
	CLIEntrypoint string
	LeasePath     string
	CacheRoot     string
	lease         *FileLock
	managedNode   *ManagedNodeRuntime
}

// ManagedNodeRuntime is a shared managed Node.js installation leased for the
// lifetime of an ACP process. The lease prevents a corrupt-cache repair from
// replacing files beneath a live session.
type ManagedNodeRuntime struct {
	path      string
	leasePath string
	lease     *FileLock
}

type ManagedNodeHealthKind int

const (
	ManagedNodeHealthMissing ManagedNodeHealthKind = iota
	ManagedNodeHealthUsable
	ManagedNodeHealthUnusable
)

// ManagedNodeHealth is Rust's private enum; Reason is set only for Unusable.
type ManagedNodeHealth struct {
	Kind   ManagedNodeHealthKind
	Reason string
}

func (h ManagedNodeHealth) unusableReason() (string, bool) {
	if h.Kind == ManagedNodeHealthUnusable {
		return h.Reason, true
	}
	return "", false
}

func (r *ManagedNodeRuntime) Path() string { return r.path }

func (r *ManagedNodeRuntime) LeasePath() string { return r.leasePath }

// SpawnGCOn starts the periodic collection of obsolete installations of
// harness under root. It stops when ctx is cancelled; the returned channel
// closes when it has stopped.
func SpawnGCOn(ctx context.Context, root string, harness HarnessKind) <-chan struct{} {
	panic("unported: mj-core/src/harness_install.rs spawn_gc_on")
}

// Located is the launch Locate selected for one harness.
type Located struct {
	Command string
	Args    []string
	// Environment holds the variables the launch adds to the harness
	// environment.
	Environment map[string]string
	// Entrypoint is the harness or ACP executable the launch runs, when
	// Mjolnir chose it. Nil for a target's own launch command, whose programs
	// Mjolnir does not name.
	Entrypoint *string
	// CLIEntrypoint is the harness command-line program beside that
	// executable, under the same condition.
	CLIEntrypoint *string
	// Managed is the managed installation the launch runs, which holds its
	// lease. Always nil under HarnessRuntimePolicyImage.
	Managed *ManagedHarness
}

func locatedManaged(managed *ManagedHarness) Located {
	entrypoint, cliEntrypoint := managed.Entrypoint, managed.CLIEntrypoint
	environment := make(map[string]string, len(managed.Environment))
	for name, value := range managed.Environment {
		environment[name] = value
	}
	return Located{
		Command:       managed.Command,
		Args:          append([]string(nil), managed.Args...),
		Environment:   environment,
		Entrypoint:    &entrypoint,
		CLIEntrypoint: &cliEntrypoint,
		Managed:       managed,
	}
}

func locatedUnchosen(command string, args []string) Located {
	return Located{
		Command:     command,
		Args:        append([]string(nil), args...),
		Environment: map[string]string{},
	}
}

// Locate selects the harness launch for policy. This is the one place that
// decides where a harness comes from; sessions, the model-discovery probe,
// `mj login` and quota checks all ask it.
//
// environment is already resolved: worker callers include the account's login
// environment, while interactive login callers pass their terminal
// environment. This function never starts a login shell. command and args are
// the launch the controller described. A command that is not Mjolnir's
// built-in launch (FallbackLaunch or a bare npm bridge name) is the profile's
// own and is kept as given.
//
//   - HarnessRuntimePolicyManaged runs the managed installation.
//   - HarnessRuntimePolicyAmbient runs the target's bridge when it is present
//     and compatible, and otherwise the managed installation.
//   - HarnessRuntimePolicyImage runs the image's pinned programs. A missing or
//     incompatible one is an error; nothing is installed and no managed cache
//     is consulted.
func Locate(ctx context.Context, policy HarnessRuntimePolicy, harness HarnessKind, executionPolicy ExecutionPolicy, command string, args []string, environment map[string]string) (Located, error) {
	panic("unported: mj-core/src/harness_install.rs locate")
}

func nativeACPArgs(harness HarnessKind, policy ExecutionPolicy, environment map[string]string) ([]string, error) {
	args := append([]string(nil), harness.BridgeArgs(policy)...)
	home, err := harness.HomeCLIArgsFromEnvironment(environment)
	if err != nil {
		return nil, err
	}
	return append(args, home...), nil
}

func antigravityMatchesPin(ctx context.Context, program string, environment map[string]string) (bool, error) {
	panic("unported: mj-core/src/harness_install.rs antigravity_matches_pin")
}

// imagePrograms is the image's own programs for harness, by the names the
// agent-dev image installs on `PATH`: the ACP bridge, the native harness the
// launch runs, and the harness's command-line program.
func imagePrograms(harness HarnessKind) (string, string, string) {
	switch harness {
	case HarnessKindCodex:
		return "codex-acp", "codex-acp", "codex"
	case HarnessKindClaude:
		return "claude-agent-acp", "claude-agent-acp", "claude"
	case HarnessKindMuse:
		return "muse-acp", "muse", "muse"
	case HarnessKindKimi:
		return "kimi", "kimi", "kimi"
	case HarnessKindGrok:
		return "grok", "grok", "grok"
	case HarnessKindOpenCode:
		return "opencode", "opencode", "opencode"
	case HarnessKindAntigravity:
		return "agy", "agy", "agy"
	case HarnessKindDsh:
		return "dsh-acp", "dsh-acp", "dsh-acp"
	}
	panic(fmt.Sprintf("unknown harness kind %v", harness))
}

func locateInImage(ctx context.Context, harness HarnessKind, executionPolicy ExecutionPolicy, environment map[string]string) (Located, error) {
	panic("unported: mj-core/src/harness_install.rs locate_in_image")
}

// compatibleBridge is the target's bridge on `PATH` when it is usable:
// present, and for Codex and Claude, the pinned version their worker-side
// protocol handling requires.
func compatibleBridge(ctx context.Context, harness HarnessKind, bridge NpmBridge, environment map[string]string) (string, bool, error) {
	panic("unported: mj-core/src/harness_install.rs compatible_bridge")
}

// targetNativeHarnessIsAvailable reports whether the target carries the
// native harness its built-in launch script looks for.
func targetNativeHarnessIsAvailable(harness HarnessKind, environment map[string]string) (bool, error) {
	panic("unported: mj-core/src/harness_install.rs target_native_harness_is_available")
}

// FindCommand is the executable command names on the environment's `PATH`,
// canonicalized.
func FindCommand(command string, environment map[string]string) (string, bool, error) {
	selected, ok, err := FindCommandPath(command, environment)
	if err != nil || !ok {
		return "", false, err
	}
	resolved, err := filepath.EvalSymlinks(selected)
	if err == nil {
		resolved, err = filepath.Abs(resolved)
	}
	if err != nil {
		return "", false, fmt.Errorf("resolve selected runtime command: %w", err)
	}
	return resolved, true, nil
}

// FindCommandPath is FindCommand without resolving symbolic links.
func FindCommandPath(command string, environment map[string]string) (string, bool, error) {
	panic("unported: mj-core/src/harness_install.rs find_command_path")
}

// resolveWithEnvironment installs or reuses the exact managed harness for
// kind from a fully resolved environment. It does not start the account's
// login shell; the caller already merged the login environment into
// environment.
func resolveWithEnvironment(ctx context.Context, harness HarnessKind, executionPolicy ExecutionPolicy, environment map[string]string) (*ManagedHarness, error) {
	panic("unported: mj-core/src/harness_install.rs resolve_with_environment")
}

func resolveAtAsync(ctx context.Context, root string, harness HarnessKind, executionPolicy ExecutionPolicy, environment map[string]string) (*ManagedHarness, error) {
	panic("unported: mj-core/src/harness_install.rs resolve_at_async")
}

func lockFile(ctx context.Context, file *os.File, shared bool, purpose string) (*FileLock, error) {
	panic("unported: mj-core/src/harness_install.rs lock_file")
}

// removeAbandonedStaging deletes the staging and set-aside directories
// earlier installers and repairs left in harnessRoot. The caller holds the
// install lock, so no installer is still writing into them. A directory that
// will not go is reported and retried by the next resolution; it must not
// block a launch, because nothing but this sweep ever looks at it.
func removeAbandonedStaging(harnessRoot string) error {
	panic("unported: mj-core/src/harness_install.rs remove_abandoned_staging")
}

// setAsideIncomplete takes an unusable installation out of the way so a fresh
// one can be published at install. The caller holds the install lock and the
// installation's exclusive lease, so no session runs from it and no installer
// writes into it. The directory is renamed to a unique sibling that
// removeAbandonedStaging owns, then deleted; rename is atomic, so nothing else
// that still names install (an older release's collector, for one) can reach
// the tree being deleted. A directory that cannot be deleted now is left for
// the next sweep.
func setAsideIncomplete(parent, install, what string) error {
	panic("unported: mj-core/src/harness_install.rs set_aside_incomplete")
}

func installInto(ctx context.Context, harnessRoot, finalPath string, harness HarnessKind, selected HarnessPin, environment map[string]string) error {
	panic("unported: mj-core/src/harness_install.rs install_into")
}

// relativizeInternalLinks makes installer-generated absolute links inside
// staging survive its rename. Do not follow directory links or rewrite links
// to external tools.
func relativizeInternalLinks(root, directory string) error {
	panic("unported: mj-core/src/harness_install.rs relativize_internal_links")
}

func installNpm(ctx context.Context, staging string, packageJSON, packageLock []byte, harness HarnessKind, environment map[string]string) error {
	panic("unported: mj-core/src/harness_install.rs install_npm")
}

// verifyInstallScriptsRan: npm 12 and later refuse dependency install scripts
// unless the package is listed in the root `allowScripts` field. A refused
// script is only a warning, so the install would otherwise publish a harness
// whose entrypoint is the package's "not installed" stub. Report that here,
// where the message can say which asset to edit, instead of letting the
// bridge fail to start later.
func verifyInstallScriptsRan(ctx context.Context, staging string, harness HarnessKind, environment map[string]string) error {
	panic("unported: mj-core/src/harness_install.rs verify_install_scripts_ran")
}

// pendingInstallScripts collects the `pkg@version` keys npm reports as
// waiting for approval.
func pendingInstallScripts(stdout string) ([]string, error) {
	panic("unported: mj-core/src/harness_install.rs pending_install_scripts")
}

// installAntigravity: the Linux artifacts are static musl builds, verified to
// have no ELF program interpreter. They run on glibc and musl targets
// regardless of the worker's own compilation target. Metadata acquisition
// also verified Google's SHA-512.
func installAntigravity(ctx context.Context, staging string, environment map[string]string) error {
	panic("unported: mj-core/src/harness_install.rs install_antigravity")
}

// installAntigravityFromMetadata takes metadata as decoded JSON (a
// map[string]any tree).
func installAntigravityFromMetadata(ctx context.Context, staging string, environment map[string]string, metadata any) error {
	panic("unported: mj-core/src/harness_install.rs install_antigravity_from_metadata")
}

// installMuseNative installs the native Muse server a managed Muse session
// drives.
//
// The `@brokkai/muse-acp` adapter comes from its pinned npm package, which
// carries the adapter only; the `muse` executable it drives keeps its own
// verified download.
func installMuseNative(ctx context.Context, staging string, environment map[string]string) error {
	panic("unported: mj-core/src/harness_install.rs install_muse_native")
}

// installOpencode installs the pinned OpenCode release from the per-platform
// npm package its official installer downloads. OpenCode 2 publishes no other
// archive: each `@opencode/cli-<target>` tarball holds the executable at
// `package/bin/opencode`.
func installOpencode(ctx context.Context, staging string, environment map[string]string) error {
	panic("unported: mj-core/src/harness_install.rs install_opencode")
}

// extractArchiveFile: extraction runs on a goroutine that cancellation cannot
// interrupt, so the installer awaits it and it checks cancellation between
// entries (and between chunks of one file): staging and the install lock are
// released only after the writer has stopped. One entry or chunk is written
// to completion.
func extractArchiveFile(cancellation context.Context, archive, wanted, destination string) error {
	panic("unported: mj-core/src/harness_install.rs extract_archive_file")
}

func downloadVerifiedAsync(ctx context.Context, rawURL, destination, expected string, environment map[string]string) error {
	panic("unported: mj-core/src/harness_install.rs download_verified_async")
}

func downloadVerifiedInner(ctx context.Context, rawURL, destination string, environment map[string]string) error {
	panic("unported: mj-core/src/harness_install.rs download_verified_inner")
}

func artifactURLIsAllowed(u *url.URL) bool {
	if u.Scheme == "https" {
		return true
	}
	if !testing.Testing() || u.Scheme != "http" {
		return false
	}
	host := u.Hostname()
	if ip := net.ParseIP(host); ip != nil {
		return ip.IsLoopback()
	}
	return strings.EqualFold(host, "localhost")
}

func artifactRedirectPolicy() func(*http.Request, []*http.Request) error {
	return func(request *http.Request, via []*http.Request) error {
		if len(via) >= maxArtifactRedirects {
			return errors.New("too many artifact download redirects")
		}
		if !artifactURLIsAllowed(request.URL) {
			return errors.New("artifact download redirected away from HTTPS")
		}
		return nil
	}
}

// EnsureMiseRuntime uses the image's mise or atomically publishes the
// verified static Linux pin. The install lock serializes workers sharing the
// same image cache.
func EnsureMiseRuntime(ctx context.Context, environment map[string]string) (string, error) {
	panic("unported: mj-core/src/harness_install.rs ensure_mise_runtime")
}

// EnsureNodeRuntime selects a Node.js runtime for an ACP launch.
//
// A compatible `node`, `npm`, and `npx` already on PATH are retained
// unchanged. Otherwise the exact pinned Node.js distribution is installed in
// Mjolnir's shared cache and prepended to PATH (in environment) for this
// launch. A nil runtime means the system runtime was retained; otherwise it
// names the managed installation.
func EnsureNodeRuntime(ctx context.Context, environment map[string]string) (*ManagedNodeRuntime, error) {
	panic("unported: mj-core/src/harness_install.rs ensure_node_runtime")
}

func installManagedNode(ctx context.Context, root string, environment map[string]string, metadata any) (*ManagedNodeRuntime, error) {
	panic("unported: mj-core/src/harness_install.rs install_managed_node")
}

func gcNodeRoot(root, keep string) error {
	panic("unported: mj-core/src/harness_install.rs gc_node_root")
}

func nodeDistributionURL(environment map[string]string, file string) (string, error) {
	panic("unported: mj-core/src/harness_install.rs node_distribution_url")
}

// ensureNodeArtifactPlatform takes the platform entry of the decoded Node.js
// metadata.
func ensureNodeArtifactPlatform(platform any) error {
	panic("unported: mj-core/src/harness_install.rs ensure_node_artifact_platform")
}

func systemNodeIsCompatible(ctx context.Context, environment map[string]string) bool {
	panic("unported: mj-core/src/harness_install.rs system_node_is_compatible")
}

func managedNodeHealth(ctx context.Context, install string, environment map[string]string) (ManagedNodeHealth, error) {
	panic("unported: mj-core/src/harness_install.rs managed_node_health")
}

// boundedEnvOutput runs command with exactly environment, through the
// shared subprocess helpers.
func boundedEnvOutput(ctx context.Context, program string, args []string, environment map[string]string) (*ProcessOutput, error) {
	panic("unported: mj-core/src/harness_install.rs bounded_env_output")
}

func nodeVersionIsSupported(version string) bool {
	rest, ok := strings.CutPrefix(version, "v")
	if !ok {
		return false
	}
	major, _, _ := strings.Cut(rest, ".")
	value, err := strconv.ParseUint(major, 10, 64)
	return err == nil && value >= installedNodeMinimumMajor
}

func prependPath(environment map[string]string, bin string) error {
	panic("unported: mj-core/src/harness_install.rs prepend_path")
}

// extractNodeArchive unpacks entry by entry (what tar's unpack does) so
// cancellation can stop it between entries; see extractArchiveFile.
func extractNodeArchive(cancellation context.Context, archive, destination, folder string) error {
	panic("unported: mj-core/src/harness_install.rs extract_node_archive")
}

func installKimi(ctx context.Context, staging string, environment map[string]string) error {
	panic("unported: mj-core/src/harness_install.rs install_kimi")
}

func installKimiFromMetadata(ctx context.Context, staging string, environment map[string]string, metadata any) error {
	panic("unported: mj-core/src/harness_install.rs install_kimi_from_metadata")
}

func installGrok(ctx context.Context, staging string, environment map[string]string) error {
	panic("unported: mj-core/src/harness_install.rs install_grok")
}

func installGrokFromMetadata(ctx context.Context, staging string, environment map[string]string, metadata any) error {
	panic("unported: mj-core/src/harness_install.rs install_grok_from_metadata")
}

// extractZstdFile copies in chunks so cancellation lands within one chunk;
// see extractArchiveFile.
func extractZstdFile(cancellation context.Context, archive, destination string) error {
	panic("unported: mj-core/src/harness_install.rs extract_zstd_file")
}

func kimiArtifactBase(environment map[string]string) string {
	if base, ok := environment["MJ_TEST_KIMI_ARTIFACT_BASE"]; ok && testing.Testing() {
		return base
	}
	return "https://code.kimi.com/kimi-code/binaries"
}

func grokArtifactBase(environment map[string]string) string {
	if base, ok := environment["MJ_TEST_GROK_ARTIFACT_BASE"]; ok && testing.Testing() {
		return base
	}
	return "https://x.ai/cli"
}

func downloadVerifiedFirstAsync(ctx context.Context, urls []string, destination, expected string, environment map[string]string) error {
	var failures []string
	for _, candidate := range urls {
		err := downloadVerifiedAsync(ctx, candidate, destination, expected, environment)
		if err == nil {
			return nil
		}
		failures = append(failures, fmt.Sprintf("%s: %v", candidate, err))
	}
	return fmt.Errorf("could not download a verified managed runtime from any configured source: %s", strings.Join(failures, "; "))
}

func runBoundedChecked(ctx context.Context, program string, args []string, dir string, environment map[string]string, operation string, timeout time.Duration) error {
	panic("unported: mj-core/src/harness_install.rs run_bounded_checked")
}

func outputSummary(output *ProcessOutput) string {
	panic("unported: mj-core/src/harness_install.rs output_summary")
}

func completeInstall(path string, harness HarnessKind, selected HarnessPin) (bool, error) {
	matches, err := ManagedHarnessManifestMatches(path, harness, selected.InstallID)
	if err != nil || !matches {
		return false, err
	}
	return validateEntrypoint(path, selected, harness) == nil, nil
}

func validateEntrypoint(path string, selected HarnessPin, harness HarnessKind) error {
	entries := []string{selected.Entrypoint}
	if extra, ok := harness.ExtraManagedEntrypoint(); ok {
		entries = append(entries, extra)
	}
	for _, entry := range entries {
		entrypoint := filepath.Join(path, entry)
		if !EntrypointIsExecutable(entrypoint) {
			return fmt.Errorf("%s installer did not create executable %s", harness.DisplayName(), entrypoint)
		}
	}
	return nil
}

func EntrypointIsExecutable(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.Mode().IsRegular() && info.Mode().Perm()&0o111 != 0
}

func openLock(path string) (*os.File, error) {
	file, err := os.OpenFile(path, os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return nil, fmt.Errorf("open managed harness lock %s: %w", path, err)
	}
	return file, nil
}

// collectObsoleteInstalls collects the obsolete installations in harnessRoot
// under the install lock, the same lock that publishes and repairs them, so a
// collector never removes an installation another process is publishing. It
// skips the pass when an installer holds the lock; the next tick collects.
func collectObsoleteInstalls(harnessRoot, keepInstallID string) error {
	panic("unported: mj-core/src/harness_install.rs collect_obsolete_installs")
}

// gcHarnessRoot: the caller holds the install lock for root.
func gcHarnessRoot(root, keepInstallID string) error {
	panic("unported: mj-core/src/harness_install.rs gc_harness_root")
}

func AcquireSupervisorLease(path string) (*FileLock, error) {
	panic("unported: mj-core/src/harness_install.rs acquire_supervisor_lease")
}
