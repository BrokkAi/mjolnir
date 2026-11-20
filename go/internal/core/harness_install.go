// Port of mj-core/src/harness_install.rs.

// Exact, target-local harness installations, including container fallbacks.

package core

import (
	"archive/tar"
	"bufio"
	"compress/gzip"
	"context"
	"crypto/sha256"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"sort"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"
	"unicode/utf8"

	"github.com/klauspost/compress/zstd"
	"golang.org/x/net/http/httpproxy"
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

// Release is Rust's Drop: it releases the installation's lease and the
// managed Node.js lease it holds.
func (m *ManagedHarness) Release() {
	if m.lease != nil {
		m.lease.Drop()
	}
	if m.managedNode != nil {
		m.managedNode.Release()
	}
}

// ManagedNodeRuntime is a shared managed Node.js installation leased for the
// lifetime of an ACP process. The lease prevents a corrupt-cache repair from
// replacing files beneath a live session.
type ManagedNodeRuntime struct {
	path      string
	leasePath string
	lease     *FileLock
}

// Release is Rust's Drop: it releases the installation's lease.
func (r *ManagedNodeRuntime) Release() {
	if r.lease != nil {
		r.lease.Drop()
	}
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
	done := make(chan struct{})
	go func() {
		defer close(done)
		ticker := time.NewTicker(managedGCInterval)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
			}
			collected, err := SpawnBlocking(func() error {
				return collectObsoleteInstalls(filepath.Join(root, harness.ID()), Pin(harness).InstallID)
			}).Join(ctx)
			if err != nil {
				if ctx.Err() != nil {
					return
				}
				slog.Error("managed harness garbage collection task failed", "error", err, "harness", harness.ID())
				return
			}
			if collected != nil {
				slog.Warn("managed harness garbage collection failed", "error", collected, "harness", harness.ID())
			}
		}
	}()
	return done
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
	bridge, hasBridge := NpmBridgeFor(harness)
	if hasBridge && !bridge.MatchesLauncher(command, args) {
		hasBridge = false
	}
	builtIn := hasBridge || IsFallbackLaunch(harness, executionPolicy, command, args)
	if harness == HarnessKindAntigravity && !builtIn {
		return locatedUnchosen(command, args), nil
	}
	managed := func() (Located, error) {
		resolved, err := resolveWithEnvironment(ctx, harness, executionPolicy, environment)
		if err != nil {
			return Located{}, err
		}
		return locatedManaged(resolved), nil
	}
	switch {
	case policy == HarnessRuntimePolicyImage && builtIn:
		return locateInImage(ctx, harness, executionPolicy, environment)
	case policy == HarnessRuntimePolicyImage:
		return locatedUnchosen(command, args), nil
	// DSH has no managed installation: its launch script resolves a
	// compatible target `dsh-acp` or fetches the pinned package through npx.
	case harness == HarnessKindDsh:
		return locatedUnchosen(command, args), nil
	case policy == HarnessRuntimePolicyManaged:
		return managed()
	case !builtIn:
		return locatedUnchosen(command, args), nil
	case harness == HarnessKindAntigravity:
		program, found, err := FindAntigravityProgram(ctx, environment)
		if err != nil {
			return Located{}, err
		}
		if found {
			matches, err := antigravityMatchesPin(ctx, program, environment)
			if err != nil {
				return Located{}, err
			}
			if matches {
				acpArgs, err := nativeACPArgs(harness, executionPolicy, environment)
				if err != nil {
					return Located{}, err
				}
				entrypoint, cli := program, program
				return Located{
					Command:       program,
					Args:          acpArgs,
					Environment:   map[string]string{"AGY_CLI_DISABLE_AUTO_UPDATE": "true"},
					Entrypoint:    &entrypoint,
					CLIEntrypoint: &cli,
				}, nil
			}
		}
		return managed()
	}
	if hasBridge {
		selected, ok, err := compatibleBridge(ctx, harness, bridge, environment)
		if err != nil {
			return Located{}, err
		}
		if ok {
			return locatedUnchosen(selected, nil), nil
		}
		slog.Info("target bridge is missing or incompatible; preparing the pinned installation", "harness", harness.ID())
	} else {
		search := maps2Clone(environment)
		type availability struct {
			ok  bool
			err error
		}
		result, err := SpawnBlocking(func() availability {
			ok, err := targetNativeHarnessIsAvailable(harness, search)
			return availability{ok, err}
		}).Join(ctx)
		if err != nil {
			return Located{}, fmt.Errorf("find target-native harness task failed: %w", err)
		}
		if result.err != nil {
			return Located{}, result.err
		}
		if result.ok {
			return locatedUnchosen(command, args), nil
		}
		slog.Info("installing the pinned harness in the target instead of relying on missing image-provided tools", "harness", harness.ID())
	}
	return managed()
}

// maps2Clone copies an environment map.
func maps2Clone(environment map[string]string) map[string]string {
	copied := make(map[string]string, len(environment))
	for name, value := range environment {
		copied[name] = value
	}
	return copied
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
	command := exec.Command(program, "--version")
	probeEnvironment := maps2Clone(environment)
	probeEnvironment["AGY_CLI_DISABLE_AUTO_UPDATE"] = "true"
	command.Env = harnessEnvironmentList(probeEnvironment)
	output, err := Capture(ctx, command)
	if err != nil {
		return false, fmt.Errorf("check target agy version: %w", err)
	}
	return output.Status != nil && output.Status.Success() &&
		strings.TrimSpace(string(output.Stdout)) == AntigravityVersion, nil
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
	bridgeName, entryName, cliName := imagePrograms(harness)
	missing := func(name string) error {
		return fmt.Errorf("the daemon's image does not provide %s for %s; the daemon container runs only the image's pinned harnesses, so use an agent-dev image of this release", name, harness.DisplayName())
	}
	search := maps2Clone(environment)
	names := []string{bridgeName, entryName, cliName}
	type lookup struct {
		paths []string
		found []bool
		err   error
	}
	result, err := SpawnBlocking(func() lookup {
		var found lookup
		for _, name := range names {
			path, ok, err := FindCommand(name, search)
			if err != nil {
				return lookup{err: err}
			}
			found.paths = append(found.paths, path)
			found.found = append(found.found, ok)
		}
		return found
	}).Join(ctx)
	if err != nil {
		return Located{}, fmt.Errorf("find the image's harness programs task failed: %w", err)
	}
	if result.err != nil {
		return Located{}, result.err
	}
	for index, name := range names {
		if !result.found[index] {
			return Located{}, missing(name)
		}
	}
	bridge, entrypoint, cli := result.paths[0], result.paths[1], result.paths[2]
	if npm, ok := NpmBridgeFor(harness); ok {
		_, compatible, err := compatibleBridge(ctx, harness, npm, environment)
		if err != nil {
			return Located{}, err
		}
		if !compatible {
			return Located{}, fmt.Errorf("the daemon's image carries an incompatible %s for %s; it needs %s %s",
				npm.Command, harness.DisplayName(), npm.Package, npm.Version)
		}
	}
	var command string
	var args []string
	switch harness {
	case HarnessKindCodex, HarnessKindClaude, HarnessKindMuse:
		command, args = bridge, []string{}
	case HarnessKindOpenCode:
		// OpenCode imports the session's staged logins before its ACP server
		// starts, as the managed launch does.
		command = "sh"
		args = []string{"-c", "opencode=\"$1\"; " + OpencodeACPScript(), "mj-opencode", bridge}
	case HarnessKindDsh:
		probe := exec.Command(bridge, "--version")
		probe.Env = harnessEnvironmentList(environment)
		output, err := RunBounded(ctx, probe, 4096, harnessCommandTimeout)
		if err != nil {
			return Located{}, fmt.Errorf("check the image's dsh-acp version: %w", err)
		}
		reported := strings.ToValidUTF8(string(output.Stdout), "�")
		if !(output.Status != nil && output.Status.Success() && DshACPVersionSupportsNativeInstructions(reported)) {
			return Located{}, fmt.Errorf("the daemon's image carries %s, which cannot load AGENTS.md; it needs dsh-acp %s or newer",
				strings.TrimSpace(reported), DshACPVersion)
		}
		command = "sh"
		args = []string{"-c", DshACPScript(), "mj-dsh", bridge}
	case HarnessKindAntigravity:
		matches, err := antigravityMatchesPin(ctx, bridge, environment)
		if err != nil {
			return Located{}, err
		}
		if !matches {
			return Located{}, fmt.Errorf("the daemon's image must provide agy %s; use an agent-dev image of this release", AntigravityVersion)
		}
		command = bridge
		if args, err = nativeACPArgs(harness, executionPolicy, environment); err != nil {
			return Located{}, err
		}
	case HarnessKindKimi, HarnessKindGrok:
		command = bridge
		if args, err = nativeACPArgs(harness, executionPolicy, environment); err != nil {
			return Located{}, err
		}
	}
	launchEnvironment := map[string]string{}
	if harness == HarnessKindAntigravity {
		launchEnvironment["AGY_CLI_DISABLE_AUTO_UPDATE"] = "true"
	}
	return Located{
		Command:       command,
		Args:          args,
		Environment:   launchEnvironment,
		Entrypoint:    &entrypoint,
		CLIEntrypoint: &cli,
	}, nil
}

// compatibleBridge is the target's bridge on `PATH` when it is usable:
// present, and for Codex and Claude, the pinned version their worker-side
// protocol handling requires.
func compatibleBridge(ctx context.Context, harness HarnessKind, bridge NpmBridge, environment map[string]string) (string, bool, error) {
	search := maps2Clone(environment)
	type lookup struct {
		path string
		ok   bool
		err  error
	}
	found, err := SpawnBlocking(func() lookup {
		path, ok, err := FindCommand(bridge.Command, search)
		return lookup{path, ok, err}
	}).Join(ctx)
	if err != nil {
		return "", false, fmt.Errorf("find target bridge task failed: %w", err)
	}
	if found.err != nil {
		return "", false, found.err
	}
	if !found.ok {
		return "", false, nil
	}
	if harness != HarnessKindCodex && harness != HarnessKindClaude {
		return found.path, true, nil
	}
	probe := exec.Command(found.path, "--version")
	probe.Env = harnessEnvironmentList(environment)
	if home, ok := environment["HOME"]; ok {
		probe.Dir = home
	}
	output, err := RunBounded(ctx, probe, 64*1024, harnessCommandTimeout)
	if err != nil {
		return "", false, fmt.Errorf("check installed %s bridge version: %w", harness.DisplayName(), err)
	}
	expected := bridge.Package + " " + bridge.Version
	if harness == HarnessKindClaude {
		expected = bridge.Version
	}
	compatible := output.Status != nil && output.Status.Success() &&
		strings.TrimRight(strings.ToValidUTF8(string(output.Stdout), "�"), "\n") == expected
	if !compatible {
		return "", false, nil
	}
	return found.path, true, nil
}

// targetNativeHarnessIsAvailable reports whether the target carries the
// native harness its built-in launch script looks for.
func targetNativeHarnessIsAvailable(harness HarnessKind, environment map[string]string) (bool, error) {
	home, hasHome := environment["HOME"]
	var command string
	switch harness {
	case HarnessKindKimi:
		command = "kimi"
	case HarnessKindGrok:
		command = "grok"
	case HarnessKindOpenCode:
		command = "opencode"
	default:
		return false, nil
	}
	if _, ok := environment["PATH"]; !ok {
		return false, nil
	}
	if _, found, err := FindCommand(command, environment); err != nil {
		return false, err
	} else if found {
		return true, nil
	}
	homeSuffix := ""
	switch harness {
	case HarnessKindKimi:
		homeSuffix = ".kimi-code/bin/kimi"
	case HarnessKindOpenCode:
		homeSuffix = ".opencode/bin/opencode"
	}
	if hasHome && homeSuffix != "" && EntrypointIsExecutable(filepath.Join(home, homeSuffix)) {
		return true, nil
	}
	if harness == HarnessKindGrok {
		var homes []string
		if grokHome := environment["GROK_HOME"]; grokHome != "" {
			homes = append(homes, grokHome)
		}
		if hasHome {
			homes = append(homes, filepath.Join(home, ".grok"))
		}
		for _, candidate := range homes {
			if EntrypointIsExecutable(filepath.Join(candidate, "bin/grok")) {
				return true, nil
			}
		}
	}
	return false, nil
}

// FindCommand is the executable command names on the environment's `PATH`,
// canonicalized.
func FindCommand(command string, environment map[string]string) (string, bool, error) {
	selected, ok, err := FindCommandPath(command, environment)
	if err != nil || !ok {
		return "", false, err
	}
	resolved, err := canonicalizePath(selected)
	if err != nil {
		return "", false, fmt.Errorf("resolve selected runtime command: %w", err)
	}
	return resolved, true, nil
}

// FindCommandPath is FindCommand without resolving symbolic links.
func FindCommandPath(command string, environment map[string]string) (string, bool, error) {
	selected := command
	if !filepath.IsAbs(command) {
		if harnessPathComponents(command) != 1 {
			return "", false, errors.New("relative runtime command is ambiguous")
		}
		path, ok := environment["PATH"]
		if !ok {
			return "", false, errors.New("runtime PATH is unavailable")
		}
		selected = ""
		for _, directory := range filepath.SplitList(path) {
			if candidate := filepath.Join(directory, command); EntrypointIsExecutable(candidate) {
				selected = candidate
				break
			}
		}
		if selected == "" {
			return "", false, nil
		}
	}
	if !EntrypointIsExecutable(selected) {
		return "", false, nil
	}
	return selected, true, nil
}

// harnessPathComponents counts a relative path's components as Rust's
// Path::components does: empty and `.` segments after the first vanish.
func harnessPathComponents(path string) int {
	count := 0
	for index, part := range strings.FieldsFunc(path, func(r rune) bool { return os.IsPathSeparator(uint8(r)) }) {
		if part == "." && index > 0 {
			continue
		}
		count++
	}
	return count
}

// resolveAt is Rust's test-only blocking resolve_at.
func resolveAt(root string, harness HarnessKind, executionPolicy ExecutionPolicy, environment map[string]string) (*ManagedHarness, error) {
	return resolveAtAsync(context.Background(), root, harness, executionPolicy, environment)
}

// resolveWithEnvironment installs or reuses the exact managed harness for
// kind from a fully resolved environment. It does not start the account's
// login shell; the caller already merged the login environment into
// environment.
func resolveWithEnvironment(ctx context.Context, harness HarnessKind, executionPolicy ExecutionPolicy, environment map[string]string) (*ManagedHarness, error) {
	if harness == HarnessKindDsh {
		return nil, fmt.Errorf("%s has no managed installation; launch it through its npx script", harness.DisplayName())
	}
	root, err := ManagedHarnessCacheRoot(environment["XDG_CACHE_HOME"], environment["HOME"])
	if err != nil {
		return nil, err
	}
	return resolveAtAsync(ctx, root, harness, executionPolicy, environment)
}

func resolveAtAsync(ctx context.Context, root string, harness HarnessKind, executionPolicy ExecutionPolicy, environment map[string]string) (*ManagedHarness, error) {
	environment = maps2Clone(environment)
	var managedNode *ManagedNodeRuntime
	if HarnessUsesNodeRuntime(harness) {
		var err error
		if managedNode, err = EnsureNodeRuntime(ctx, environment); err != nil {
			return nil, err
		}
	}
	keepNode := false
	defer func() {
		if !keepNode && managedNode != nil {
			managedNode.Release()
		}
	}()
	selected := Pin(harness)
	harnessRoot := filepath.Join(root, harness.ID())
	if err := os.MkdirAll(harnessRoot, 0o777); err != nil {
		return nil, fmt.Errorf("create managed %s cache: %w", harness.DisplayName(), err)
	}
	installLockFileHandle, err := openLock(filepath.Join(harnessRoot, installLockFile))
	if err != nil {
		return nil, err
	}
	installLock, err := lockFile(ctx, installLockFileHandle, false, "managed harness installer")
	if err != nil {
		return nil, err
	}
	defer installLock.Drop()
	if err := removeAbandonedStaging(harnessRoot); err != nil {
		return nil, err
	}

	install := ManagedHarnessInstallDir(root, harness)
	complete, err := completeInstall(install, harness, selected)
	if err != nil {
		return nil, err
	}
	if !complete {
		if _, err := os.Lstat(install); err == nil {
			if err := repairLocked(install, harnessRoot, "managed harness", func() error {
				return fmt.Errorf("managed harness repair deferred: %s is still in use", install)
			}, "lock invalid managed harness for repair"); err != nil {
				return nil, err
			}
		}
		if err := installInto(ctx, harnessRoot, install, harness, selected, environment); err != nil {
			return nil, err
		}
	}
	if err := gcHarnessRoot(harnessRoot, selected.InstallID); err != nil {
		return nil, err
	}

	leasePath := filepath.Join(install, LeaseFile)
	leaseFile, err := openLock(leasePath)
	if err != nil {
		return nil, err
	}
	lease, err := lockFile(ctx, leaseFile, true, "managed harness lease")
	if err != nil {
		return nil, err
	}
	installLock.Drop()

	launchEnvironment := map[string]string{}
	if managedNode != nil {
		path, ok := environment["PATH"]
		if !ok {
			lease.Drop()
			return nil, errors.New("managed Node runtime did not produce PATH")
		}
		launchEnvironment["PATH"] = path
	}
	// The managed Muse install provides only the native server; the adapter
	// itself comes from its pinned npm package.
	if harness == HarnessKindMuse {
		launchEnvironment["MUSE_CLI"] = filepath.Join(install, selected.Entrypoint)
	}
	if harness == HarnessKindCodex {
		launchEnvironment["CODEX_PATH"] = filepath.Join(install, "node_modules/@openai/codex/bin/codex.js")
	}
	// The bridge's SDK bundles an older CLI; select our separately pinned Code.
	if entry, ok := harness.ExtraManagedEntrypoint(); harness == HarnessKindClaude && ok {
		launchEnvironment["CLAUDE_CODE_EXECUTABLE"] = filepath.Join(install, entry)
	}
	// The pinned installation is not the release channel, so its downloader
	// must never fetch a replacement under a running session.
	if harness == HarnessKindOpenCode {
		launchEnvironment["OPENCODE_DISABLE_AUTOUPDATE"] = "1"
	}
	if harness == HarnessKindAntigravity {
		launchEnvironment["AGY_CLI_DISABLE_AUTO_UPDATE"] = "true"
	}
	// Muse launches its adapter the way the other npm bridges do: the target's
	// own `muse-acp` when present, otherwise npx fetches the pinned package.
	var command string
	var args []string
	switch harness {
	case HarnessKindMuse:
		bridge, ok := NpmBridgeFor(harness)
		if !ok {
			panic("Muse installs its adapter from npm")
		}
		command, args = "sh", []string{"-c", bridge.BootstrapScript()}
	case HarnessKindOpenCode:
		// OpenCode imports the session's staged logins before its ACP server
		// starts; the script replaces itself with the pinned executable.
		command = "sh"
		args = []string{"-c", "opencode=\"$1\"; " + OpencodeACPScript(), "mj-opencode", filepath.Join(install, selected.Entrypoint)}
	default:
		command = filepath.Join(install, selected.Entrypoint)
		if args, err = nativeACPArgs(harness, executionPolicy, environment); err != nil {
			lease.Drop()
			return nil, err
		}
	}
	keepNode = true
	return &ManagedHarness{
		Command:       command,
		Args:          args,
		Environment:   launchEnvironment,
		Entrypoint:    filepath.Join(install, selected.Entrypoint),
		CLIEntrypoint: filepath.Join(install, selected.CLIEntrypoint),
		LeasePath:     leasePath,
		CacheRoot:     root,
		lease:         lease,
		managedNode:   managedNode,
	}, nil
}

// repairLocked takes the exclusive lease of the unusable installation at
// install and sets it aside; busy explains a lease another process holds.
func repairLocked(install, parent, what string, busy func() error, lockFailure string) error {
	leaseFile, err := openLock(filepath.Join(install, LeaseFile))
	if err != nil {
		return err
	}
	locked, err := harnessTryLock(leaseFile, false)
	if err != nil {
		leaseFile.Close()
		return fmt.Errorf("%s: %w", lockFailure, err)
	}
	if !locked {
		leaseFile.Close()
		return busy()
	}
	lease := NewFileLock(leaseFile)
	defer lease.Drop()
	return setAsideIncomplete(parent, install, what)
}

func lockFile(ctx context.Context, file *os.File, shared bool, purpose string) (*FileLock, error) {
	started := time.Now()
	cancellation := PreparationCancellation(ctx)
	for {
		if cancellation.Err() != nil {
			file.Close()
			return nil, PreparationCancelled{}
		}
		locked, err := harnessTryLock(file, shared)
		if err != nil {
			file.Close()
			return nil, fmt.Errorf("lock %s: %w", purpose, err)
		}
		if locked {
			return NewFileLock(file), nil
		}
		if time.Since(started) >= installLockTimeout {
			file.Close()
			return nil, fmt.Errorf("timed out after %d seconds waiting to lock %s", int(installLockTimeout.Seconds()), purpose)
		}
		select {
		case <-time.After(lockFilePollInterval):
		case <-cancellation.Done():
			file.Close()
			return nil, PreparationCancelled{}
		}
	}
}

// removeAbandonedStaging deletes the staging and set-aside directories
// earlier installers and repairs left in harnessRoot. The caller holds the
// install lock, so no installer is still writing into them. A directory that
// will not go is reported and retried by the next resolution; it must not
// block a launch, because nothing but this sweep ever looks at it.
func removeAbandonedStaging(harnessRoot string) error {
	entries, err := os.ReadDir(harnessRoot)
	if err != nil {
		return fmt.Errorf("scan managed harness staging in %s: %w", harnessRoot, err)
	}
	for _, entry := range entries {
		name := entry.Name()
		if !utf8.ValidString(name) || !strings.HasPrefix(name, ".install-") || !entry.IsDir() {
			continue
		}
		path := filepath.Join(harnessRoot, name)
		if err := os.RemoveAll(path); err != nil {
			slog.Warn("could not remove abandoned harness staging", "error", err, "path", path)
		}
	}
	return nil
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
	aside, err := os.MkdirTemp(parent, ".install-abandoned-")
	if err != nil {
		return fmt.Errorf("create set-aside directory in %s: %w", parent, err)
	}
	if err := harnessRename(install, aside); err != nil {
		return fmt.Errorf("set aside incomplete %s %s as %s: %w", what, install, aside, err)
	}
	if err := os.RemoveAll(aside); err != nil {
		slog.Warn(fmt.Sprintf("could not remove incomplete %s; the next installation will retry", what), "error", err, "path", aside)
	}
	return nil
}

func installInto(ctx context.Context, harnessRoot, finalPath string, harness HarnessKind, selected HarnessPin, environment map[string]string) error {
	staging, err := os.MkdirTemp(harnessRoot, ".install-")
	if err != nil {
		return fmt.Errorf("create staging directory in %s: %w", harnessRoot, err)
	}
	kept := false
	defer func() {
		if !kept {
			os.RemoveAll(staging)
		}
	}()
	switch harness {
	case HarnessKindCodex:
		err = installNpm(ctx, staging, codexPackageJSON, codexPackageLock, harness, environment)
	case HarnessKindClaude:
		err = installNpm(ctx, staging, claudePackageJSON, claudePackageLock, harness, environment)
	case HarnessKindKimi:
		err = installKimi(ctx, staging, environment)
	case HarnessKindGrok:
		err = installGrok(ctx, staging, environment)
	case HarnessKindMuse:
		err = installMuseNative(ctx, staging, environment)
	case HarnessKindOpenCode:
		err = installOpencode(ctx, staging, environment)
	case HarnessKindAntigravity:
		err = installAntigravity(ctx, staging, environment)
	case HarnessKindDsh:
		panic("DSH has no managed installation; resolve returns before installing")
	}
	if err != nil {
		return err
	}
	if err := relativizeInternalLinks(staging, staging); err != nil {
		return err
	}
	if err := validateEntrypoint(staging, selected, harness); err != nil {
		return err
	}
	leaseFile, err := openLock(filepath.Join(staging, LeaseFile))
	if err != nil {
		return err
	}
	leaseFile.Close()
	manifest := ManagedHarnessManifestForInstall(harness, selected.InstallID)
	body, err := json.MarshalIndent(manifest, "", "  ")
	if err != nil {
		return err
	}
	// The receipt goes in before the rename: a published directory is always
	// complete, so nothing can observe or collect a half-installed tree.
	if err := AtomicWrite(filepath.Join(staging, ManifestFile), body); err != nil {
		return err
	}
	kept = true
	renameErr := harnessRename(staging, finalPath)
	if renameErr == nil {
		if err := validateEntrypoint(finalPath, selected, harness); err != nil {
			return fmt.Errorf("validate relocated managed harness: %w", err)
		}
		return nil
	}
	if _, err := os.Lstat(finalPath); err == nil {
		if err := os.RemoveAll(staging); err != nil {
			return fmt.Errorf("remove losing harness staging %s: %w", staging, err)
		}
		complete, err := completeInstall(finalPath, harness, selected)
		if err != nil {
			return err
		}
		if complete {
			return nil
		}
	}
	return fmt.Errorf("publish managed harness %s: %w", finalPath, renameErr)
}

// relativizeInternalLinks makes installer-generated absolute links inside
// staging survive its rename. Do not follow directory links or rewrite links
// to external tools.
func relativizeInternalLinks(root, directory string) error {
	entries, err := os.ReadDir(directory)
	if err != nil {
		return err
	}
	for _, entry := range entries {
		path := filepath.Join(directory, entry.Name())
		switch {
		case entry.IsDir():
			if err := relativizeInternalLinks(root, path); err != nil {
				return err
			}
		case entry.Type()&fs.ModeSymlink != 0:
			target, err := os.Readlink(path)
			if err != nil {
				return err
			}
			if !filepath.IsAbs(target) {
				continue
			}
			inside, ok := harnessStripPrefix(target, root)
			if !ok {
				continue
			}
			parent, ok := harnessStripPrefix(directory, root)
			if !ok {
				return fmt.Errorf("%s is not inside %s", directory, root)
			}
			var relative []string
			for range harnessComponents(parent) {
				relative = append(relative, "..")
			}
			relative = append(relative, harnessComponents(inside)...)
			if err := os.Remove(path); err != nil {
				return err
			}
			if err := os.Symlink(filepath.Join(relative...), path); err != nil {
				return fmt.Errorf("relocate managed harness link %s: %w", path, err)
			}
		}
	}
	return nil
}

// harnessComponents splits a path into its normal components, as Rust's
// Path::components does for a path without a root.
func harnessComponents(path string) []string {
	var parts []string
	for _, part := range strings.FieldsFunc(path, func(r rune) bool { return os.IsPathSeparator(uint8(r)) }) {
		if part != "." {
			parts = append(parts, part)
		}
	}
	return parts
}

// harnessStripPrefix is Rust's Path::strip_prefix: whole components only.
func harnessStripPrefix(path, prefix string) (string, bool) {
	pathParts := harnessComponents(path)
	prefixParts := harnessComponents(prefix)
	if filepath.IsAbs(path) != filepath.IsAbs(prefix) || len(prefixParts) > len(pathParts) {
		return "", false
	}
	if !slices.Equal(pathParts[:len(prefixParts)], prefixParts) {
		return "", false
	}
	return filepath.Join(pathParts[len(prefixParts):]...), true
}

func installNpm(ctx context.Context, staging string, packageJSON, packageLock []byte, harness HarnessKind, environment map[string]string) error {
	if err := os.WriteFile(filepath.Join(staging, "package.json"), packageJSON, 0o666); err != nil {
		return fmt.Errorf("write managed harness package.json: %w", err)
	}
	if err := os.WriteFile(filepath.Join(staging, "package-lock.json"), packageLock, 0o666); err != nil {
		return fmt.Errorf("write managed harness package-lock.json: %w", err)
	}
	command := harnessCommand("npm", []string{"ci", "--omit=dev", "--no-audit", "--no-fund"}, environment)
	command.Dir = staging
	if err := runBoundedCommandChecked(ctx, command, "install exact managed npm harness", managedInstallTimeout); err != nil {
		return err
	}
	return verifyInstallScriptsRan(ctx, staging, harness, environment)
}

// verifyInstallScriptsRan: npm 12 and later refuse dependency install scripts
// unless the package is listed in the root `allowScripts` field. A refused
// script is only a warning, so the install would otherwise publish a harness
// whose entrypoint is the package's "not installed" stub. Report that here,
// where the message can say which asset to edit, instead of letting the
// bridge fail to start later.
func verifyInstallScriptsRan(ctx context.Context, staging string, harness HarnessKind, environment map[string]string) error {
	command := harnessCommand("npm", []string{"install-scripts", "ls", "--json"}, environment)
	command.Dir = staging
	output, err := RunBounded(ctx, command, 1024*1024, harnessCommandTimeout)
	if err != nil {
		return fmt.Errorf("read managed harness install-script status: %w", err)
	}
	stdout := strings.ToValidUTF8(string(output.Stdout), "�")
	stderr := strings.ToValidUTF8(string(output.Stderr), "�")
	if output.Status == nil || !output.Status.Success() {
		// npm versions that do not gate install scripts have no such command.
		if strings.Contains(stderr, "Unknown command") || strings.Contains(stdout, "Unknown command") {
			return nil
		}
		return fmt.Errorf("read managed harness install-script status failed: %s", outputSummary(&output))
	}
	pending, err := pendingInstallScripts(stdout)
	if err != nil {
		return fmt.Errorf("read managed harness install-script status: %w", err)
	}
	if len(pending) == 0 {
		return nil
	}
	return fmt.Errorf("managed %s harness install skipped install scripts for %s. npm gates dependency install "+
		"scripts, so add each package to allowScripts in "+
		"mj-core/assets/harnesses/%s/package.json and reinstall.",
		harness.DisplayName(), strings.Join(pending, ", "), harness.ID())
}

// pendingInstallScripts collects the `pkg@version` keys npm reports as
// waiting for approval.
func pendingInstallScripts(stdout string) ([]string, error) {
	var status any
	if err := json.Unmarshal([]byte(stdout), &status); err != nil {
		return nil, err
	}
	var groups []any
	switch status := status.(type) {
	case map[string]any:
		groups, _ = status["allowScripts"].([]any)
	case []any:
		groups = status
	default:
		return nil, errors.New("unexpected install-script status")
	}
	pending := []string{}
	for _, group := range groups {
		object, _ := group.(map[string]any)
		changes, ok := object["changes"].([]any)
		if !ok {
			continue
		}
		for _, change := range changes {
			fields, _ := change.(map[string]any)
			if kind, _ := fields["change"].(string); kind != "pending" {
				continue
			}
			if key, ok := fields["key"].(string); ok {
				pending = append(pending, key)
			}
		}
	}
	return pending, nil
}

// installAntigravity: the Linux artifacts are static musl builds, verified to
// have no ELF program interpreter. They run on glibc and musl targets
// regardless of the worker's own compilation target. Metadata acquisition
// also verified Google's SHA-512.
func installAntigravity(ctx context.Context, staging string, environment map[string]string) error {
	var metadata any
	if err := json.Unmarshal(antigravityRuntimeJSON, &metadata); err != nil {
		return err
	}
	return installAntigravityFromMetadata(ctx, staging, environment, metadata)
}

// harnessPlatformKey is Rust's `{OS}-{ARCH}` with std::env::consts names.
func harnessPlatformKey() string {
	return harnessOSName() + "-" + harnessArchName()
}

// harnessArchName is Rust's std::env::consts::ARCH.
func harnessArchName() string {
	switch runtime.GOARCH {
	case "amd64":
		return "x86_64"
	case "arm64":
		return "aarch64"
	case "386":
		return "x86"
	}
	return runtime.GOARCH
}

// harnessMetadataPlatform reads metadata["platforms"][key] for the running
// platform; ok is false when the metadata has none.
func harnessMetadataPlatform(metadata any) (map[string]any, string, bool) {
	key := harnessPlatformKey()
	object, _ := metadata.(map[string]any)
	platforms, _ := object["platforms"].(map[string]any)
	platform, ok := platforms[key]
	if !ok {
		return nil, key, false
	}
	fields, _ := platform.(map[string]any)
	if fields == nil {
		fields = map[string]any{}
	}
	return fields, key, true
}

// harnessMetadataString reports whether metadata[name] is the string want.
func harnessMetadataString(metadata any, name, want string) bool {
	object, _ := metadata.(map[string]any)
	value, ok := object[name].(string)
	return ok && value == want
}

// harnessArtifactField reads one string field of a platform entry.
func harnessArtifactField(platform map[string]any, product, name string) (string, error) {
	value, ok := platform[name].(string)
	if !ok {
		return "", fmt.Errorf("missing %s artifact field %s", product, name)
	}
	return value, nil
}

// installAntigravityFromMetadata takes metadata as decoded JSON (a
// map[string]any tree).
func installAntigravityFromMetadata(ctx context.Context, staging string, environment map[string]string, metadata any) error {
	if !harnessMetadataString(metadata, "version", AntigravityVersion) {
		return errors.New("Antigravity download metadata does not match the managed runtime pin")
	}
	platform, key, ok := harnessMetadataPlatform(metadata)
	if !ok {
		return fmt.Errorf("Antigravity has no managed runtime for %s", key)
	}
	url, err := harnessArtifactField(platform, "Antigravity", "url")
	if err != nil {
		return err
	}
	sha, err := harnessArtifactField(platform, "Antigravity", "sha256")
	if err != nil {
		return err
	}
	archive := filepath.Join(staging, "agy.tar.gz")
	if err := downloadVerifiedAsync(ctx, url, archive, sha, environment); err != nil {
		return err
	}
	bin := filepath.Join(staging, "bin")
	if err := os.MkdirAll(bin, 0o777); err != nil {
		return err
	}
	executable := filepath.Join(bin, "agy")
	cancellation := PreparationCancellation(ctx)
	extracted, err := SpawnBlocking(func() error {
		return extractArchiveFile(cancellation, archive, "antigravity", executable)
	}).Join(ctx)
	if err != nil {
		return fmt.Errorf("Antigravity extraction task failed: %w", err)
	}
	if extracted != nil {
		return extracted
	}
	if err := os.Remove(archive); err != nil {
		return err
	}
	return os.Chmod(executable, 0o755)
}

// installMuseNative installs the native Muse server a managed Muse session
// drives.
//
// The `@brokkai/muse-acp` adapter comes from its pinned npm package, which
// carries the adapter only; the `muse` executable it drives keeps its own
// verified download.
func installMuseNative(ctx context.Context, staging string, environment map[string]string) error {
	var metadata any
	if err := json.Unmarshal(museRuntimeJSON, &metadata); err != nil {
		return err
	}
	if !harnessMetadataString(metadata, "muse_version", MuseVersion) {
		return errors.New("Muse download metadata does not match the managed runtime pin")
	}
	platform, key, ok := harnessMetadataPlatform(metadata)
	if !ok {
		return fmt.Errorf("Muse Code does not have a managed runtime for %s", key)
	}
	bin := filepath.Join(staging, "bin")
	if err := os.MkdirAll(bin, 0o777); err != nil {
		return err
	}
	museTarget, err := harnessArtifactField(platform, "Muse", "muse_target")
	if err != nil {
		return err
	}
	url := fmt.Sprintf("https://lookaside.facebook.com/lookaside/muse/download/?channel=muse&version=%s&file=muse-%s", MuseVersion, museTarget)
	muse := filepath.Join(bin, "muse")
	sha, err := harnessArtifactField(platform, "Muse", "muse_sha256")
	if err != nil {
		return err
	}
	if err := downloadVerifiedAsync(ctx, url, muse, sha, environment); err != nil {
		return err
	}
	return os.Chmod(muse, 0o755)
}

// installOpencode installs the pinned OpenCode release from the per-platform
// npm package its official installer downloads. OpenCode 2 publishes no other
// archive: each `@opencode/cli-<target>` tarball holds the executable at
// `package/bin/opencode`.
func installOpencode(ctx context.Context, staging string, environment map[string]string) error {
	var metadata any
	if err := json.Unmarshal(opencodeRuntimeJSON, &metadata); err != nil {
		return err
	}
	if !harnessMetadataString(metadata, "version", OpencodeVersion) {
		return errors.New("OpenCode download metadata does not match the managed runtime pin")
	}
	platform, key, ok := harnessMetadataPlatform(metadata)
	if !ok {
		return fmt.Errorf("OpenCode does not have a managed runtime for %s", key)
	}
	target, err := harnessArtifactField(platform, "OpenCode", "target")
	if err != nil {
		return err
	}
	file := fmt.Sprintf("cli-%s-%s.tgz", target, OpencodeVersion)
	url := fmt.Sprintf("https://registry.npmjs.org/@opencode/cli-%s/-/%s", target, file)
	archive := filepath.Join(staging, file)
	sha, err := harnessArtifactField(platform, "OpenCode", "sha256")
	if err != nil {
		return err
	}
	if err := downloadVerifiedAsync(ctx, url, archive, sha, environment); err != nil {
		return err
	}
	entrypoint := filepath.Join(staging, "opencode")
	cancellation := PreparationCancellation(ctx)
	extracted, err := SpawnBlocking(func() error {
		return extractArchiveFile(cancellation, archive, "package/bin/opencode", entrypoint)
	}).Join(ctx)
	if err != nil {
		return fmt.Errorf("OpenCode extraction task failed: %w", err)
	}
	if extracted != nil {
		return extracted
	}
	if err := os.Remove(archive); err != nil {
		return err
	}
	return os.Chmod(entrypoint, 0o755)
}

// harnessArchivePathEqual compares archive entry paths as Rust's Path
// equality does: by components, ignoring repeated separators and interior
// `.` segments.
func harnessArchivePathEqual(left, right string) bool {
	normalize := func(path string) []string {
		var parts []string
		for index, part := range strings.Split(path, "/") {
			if part == "" || (part == "." && index > 0) {
				continue
			}
			parts = append(parts, part)
		}
		return parts
	}
	return strings.HasPrefix(left, "/") == strings.HasPrefix(right, "/") &&
		slices.Equal(normalize(left), normalize(right))
}

// harnessTarIsFile is the tar crate's EntryType::is_file.
func harnessTarIsFile(header *tar.Header) bool {
	return header.Typeflag == tar.TypeReg || header.Typeflag == '\x00' || header.Typeflag == tar.TypeCont
}

// extractArchiveFile: extraction runs on a goroutine that cancellation cannot
// interrupt, so the installer awaits it and it checks cancellation between
// entries (and between chunks of one file): staging and the install lock are
// released only after the writer has stopped. One entry or chunk is written
// to completion.
func extractArchiveFile(cancellation context.Context, archive, wanted, destination string) error {
	archiveFile, err := os.Open(archive)
	if err != nil {
		return fmt.Errorf("open archive %s: %w", archive, err)
	}
	defer archiveFile.Close()
	decoder, err := gzip.NewReader(bufio.NewReader(archiveFile))
	if err != nil {
		return fmt.Errorf("read archive %s: %w", archive, err)
	}
	reader := tar.NewReader(decoder)
	for {
		if cancellation.Err() != nil {
			return fmt.Errorf("while extracting: %w", PreparationCancelled{})
		}
		header, err := reader.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			return fmt.Errorf("read archive %s: %w", archive, err)
		}
		if !harnessArchivePathEqual(header.Name, wanted) {
			continue
		}
		if !harnessTarIsFile(header) {
			return fmt.Errorf("archive entry %s is not a regular file", wanted)
		}
		if err := harnessUnpackFile(reader, header, destination); err != nil {
			return fmt.Errorf("extract %s to %s: %w", wanted, destination, err)
		}
		return nil
	}
	return fmt.Errorf("archive %s did not contain %s", archive, wanted)
}

// harnessUnpackFile writes one regular tar entry to destination with the
// entry's permission bits and modification time, as the tar crate's unpack
// does.
func harnessUnpackFile(reader io.Reader, header *tar.Header, destination string) error {
	os.Remove(destination)
	output, err := os.OpenFile(destination, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, fs.FileMode(header.Mode)&0o777)
	if err != nil {
		return err
	}
	if _, err := io.Copy(output, reader); err != nil {
		output.Close()
		return err
	}
	if err := output.Close(); err != nil {
		return err
	}
	if err := os.Chmod(destination, fs.FileMode(header.Mode)&0o777); err != nil {
		return err
	}
	if !header.ModTime.IsZero() {
		return os.Chtimes(destination, header.ModTime, header.ModTime)
	}
	return nil
}

func downloadVerifiedAsync(ctx context.Context, rawURL, destination, expected string, environment map[string]string) error {
	cancellation := PreparationCancellation(ctx)
	if cancellation.Err() != nil {
		return PreparationCancelled{}
	}
	workCtx, cancel := context.WithCancel(ctx)
	defer cancel()
	stop := context.AfterFunc(cancellation, cancel)
	defer stop()
	result := make(chan error, 1)
	go func() {
		result <- func() error {
			if err := downloadVerifiedInner(workCtx, rawURL, destination, environment); err != nil {
				return err
			}
			// Hashing reads the whole artifact; keep it off the goroutines
			// that serve the daemon's clients. It only reads, so a
			// cancellation that abandons it is safe.
			type hashed struct {
				digest string
				err    error
			}
			actual, err := SpawnBlocking(func() hashed {
				digest, err := harnessFileSHA256(destination)
				return hashed{digest, err}
			}).Join(workCtx)
			if err != nil {
				return fmt.Errorf("hash downloaded artifact task failed: %w", err)
			}
			if actual.err != nil {
				return actual.err
			}
			if actual.digest != expected {
				return fmt.Errorf("harness runtime checksum mismatch for %s: expected %s, got %s", destination, expected, actual.digest)
			}
			return nil
		}()
	}()
	var err error
	select {
	case err = <-result:
	case <-cancellation.Done():
		err = PreparationCancelled{}
	}
	if err != nil {
		os.Remove(destination)
	}
	return err
}

// harnessFileSHA256 is Rust's hex::file_sha256: the lowercase SHA-256 of a
// file, read in 1 MiB chunks.
func harnessFileSHA256(path string) (string, error) {
	file, err := os.Open(path)
	if err != nil {
		return "", fmt.Errorf("open file for checksum %s: %w", path, err)
	}
	defer file.Close()
	digest := sha256.New()
	if _, err := io.CopyBuffer(digest, file, make([]byte, 1024*1024)); err != nil {
		return "", fmt.Errorf("hash file %s: %w", path, err)
	}
	return LowerHex(digest.Sum(nil)), nil
}

// harnessWithoutURL drops the URL net/http puts in its errors: after
// redirects it can be a signed URL with a token in its query; the caller's
// URL is the validated one.
func harnessWithoutURL(err error) error {
	var urlError *url.Error
	if errors.As(err, &urlError) {
		return urlError.Err
	}
	return err
}

func downloadVerifiedInner(ctx context.Context, rawURL, destination string, environment map[string]string) error {
	parsed, err := url.Parse(rawURL)
	if err != nil || parsed.Scheme == "" {
		if err == nil {
			err = errors.New("relative URL without a base")
		}
		return fmt.Errorf("parse artifact URL %s: %w", debugString(rawURL), err)
	}
	if !artifactURLIsAllowed(parsed) {
		return fmt.Errorf("artifact downloads require HTTPS: %s", rawURL)
	}
	transport := &http.Transport{
		DialContext:         (&net.Dialer{Timeout: artifactConnectTimeout}).DialContext,
		TLSHandshakeTimeout: artifactConnectTimeout,
		Proxy:               nil,
	}
	for _, name := range []string{"HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"} {
		proxy := environment[name]
		if proxy == "" {
			continue
		}
		if _, err := url.Parse(proxy); err != nil {
			return fmt.Errorf("configure artifact download proxy from %s: %w", name, err)
		}
		noProxy, ok := environment["NO_PROXY"]
		if !ok {
			noProxy = environment["no_proxy"]
		}
		config := httpproxy.Config{HTTPProxy: proxy, HTTPSProxy: proxy, NoProxy: noProxy}
		proxyFor := config.ProxyFunc()
		transport.Proxy = func(request *http.Request) (*url.URL, error) { return proxyFor(request.URL) }
		break
	}
	client := &http.Client{
		Transport:     transport,
		Timeout:       artifactDownloadTimeout,
		CheckRedirect: artifactRedirectPolicy(),
	}
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, rawURL, nil)
	if err != nil {
		return fmt.Errorf("request artifact %s: %w", rawURL, err)
	}
	request.Header.Set("User-Agent", harnessInstallUserAgent)
	response, err := client.Do(request)
	if err != nil {
		return fmt.Errorf("request artifact %s: %w", rawURL, harnessWithoutURL(err))
	}
	defer response.Body.Close()
	if response.StatusCode >= 400 {
		kind := "client"
		if response.StatusCode >= 500 {
			kind = "server"
		}
		return fmt.Errorf("download artifact %s: HTTP status %s error (%s)", rawURL, kind, response.Status)
	}
	if response.ContentLength > maxArtifactBytes {
		return fmt.Errorf("artifact %s is larger than the %d byte download limit", rawURL, maxArtifactBytes)
	}
	file, err := os.Create(destination)
	if err != nil {
		return fmt.Errorf("create artifact file %s: %w", destination, err)
	}
	defer file.Close()
	received := int64(0)
	buffer := make([]byte, 64*1024)
	for {
		count, readErr := response.Body.Read(buffer)
		if count > 0 {
			received += int64(count)
			if received > maxArtifactBytes {
				return fmt.Errorf("artifact %s exceeded the %d byte download limit", rawURL, maxArtifactBytes)
			}
			if _, err := file.Write(buffer[:count]); err != nil {
				return fmt.Errorf("write artifact file %s: %w", destination, err)
			}
		}
		if readErr == io.EOF {
			break
		}
		if readErr != nil {
			return fmt.Errorf("read artifact %s: %w", rawURL, harnessWithoutURL(readErr))
		}
	}
	if err := file.Sync(); err != nil {
		return fmt.Errorf("flush artifact file %s: %w", destination, err)
	}
	if received == 0 {
		return fmt.Errorf("artifact %s was empty", rawURL)
	}
	return nil
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
	if command, ok, err := FindCommandPath("mise", environment); err != nil {
		return "", err
	} else if ok {
		return command, nil
	}
	if runtime.GOOS != "linux" {
		return "", fmt.Errorf("mise has no managed runtime for %s", harnessOSName())
	}
	artifact, err := PinnedMise(harnessArchName())
	if err != nil {
		return "", err
	}
	cacheRoot, err := ManagedHarnessCacheRoot(environment["XDG_CACHE_HOME"], environment["HOME"])
	if err != nil {
		return "", err
	}
	root := filepath.Join(filepath.Dir(cacheRoot), "mise")
	if err := os.MkdirAll(root, 0o777); err != nil {
		return "", err
	}
	lockHandle, err := openLock(filepath.Join(root, installLockFile))
	if err != nil {
		return "", err
	}
	lock, err := lockFile(ctx, lockHandle, false, "managed mise installer")
	if err != nil {
		return "", err
	}
	defer lock.Drop()
	install := filepath.Join(root, MiseVersion)
	executable := filepath.Join(install, "mise")
	if EntrypointIsExecutable(executable) {
		return executable, nil
	}
	staging, err := os.MkdirTemp(root, ".install-")
	if err != nil {
		return "", err
	}
	defer os.RemoveAll(staging)
	downloaded := filepath.Join(staging, "mise")
	if err := downloadVerifiedAsync(ctx, artifact.URL, downloaded, artifact.Sha256, environment); err != nil {
		return "", err
	}
	if err := os.Chmod(downloaded, 0o755); err != nil {
		return "", err
	}
	if err := os.MkdirAll(install, 0o777); err != nil {
		return "", err
	}
	if err := harnessRename(downloaded, executable); err != nil {
		return "", fmt.Errorf("publish managed mise executable: %w", err)
	}
	return executable, nil
}

// harnessOSName is Rust's std::env::consts::OS.
func harnessOSName() string {
	if runtime.GOOS == "darwin" {
		return "macos"
	}
	return runtime.GOOS
}

// EnsureNodeRuntime selects a Node.js runtime for an ACP launch.
//
// A compatible `node`, `npm`, and `npx` already on PATH are retained
// unchanged. Otherwise the exact pinned Node.js distribution is installed in
// Mjolnir's shared cache and prepended to PATH (in environment) for this
// launch. A nil runtime means the system runtime was retained; otherwise it
// names the managed installation.
func EnsureNodeRuntime(ctx context.Context, environment map[string]string) (*ManagedNodeRuntime, error) {
	if systemNodeIsCompatible(ctx, environment) {
		return nil, nil
	}
	root, err := ManagedNodeCacheRoot(environment["XDG_CACHE_HOME"], environment["HOME"])
	if err != nil {
		return nil, err
	}
	var metadata any
	if err := json.Unmarshal(nodeRuntimeJSON, &metadata); err != nil {
		return nil, err
	}
	return installManagedNode(ctx, root, environment, metadata)
}

// ensureNodeRuntimeFromMetadata is Rust's test-only
// ensure_node_runtime_from_metadata.
func ensureNodeRuntimeFromMetadata(ctx context.Context, root string, environment map[string]string, metadata any) (*ManagedNodeRuntime, error) {
	if systemNodeIsCompatible(ctx, environment) {
		return nil, nil
	}
	return installManagedNode(ctx, root, environment, metadata)
}

func installManagedNode(ctx context.Context, root string, environment map[string]string, metadata any) (*ManagedNodeRuntime, error) {
	if !harnessMetadataString(metadata, "version", NodeVersion) {
		return nil, errors.New("Node.js download metadata does not match the managed runtime pin")
	}
	platform, key, ok := harnessMetadataPlatform(metadata)
	if !ok {
		return nil, fmt.Errorf("Node.js does not have a managed runtime for %s", key)
	}
	target, err := harnessArtifactField(platform, "Node.js", "target")
	if err != nil {
		return nil, err
	}
	folder := fmt.Sprintf("node-v%s-%s", NodeVersion, target)
	install := filepath.Join(root, folder)
	if err := ensureNodeArtifactPlatform(platform); err != nil {
		return nil, err
	}

	if err := os.MkdirAll(root, 0o777); err != nil {
		return nil, fmt.Errorf("create managed Node.js cache %s: %w", root, err)
	}
	lockHandle, err := openLock(filepath.Join(root, installLockFile))
	if err != nil {
		return nil, err
	}
	installLock, err := lockFile(ctx, lockHandle, false, "managed Node.js installer")
	if err != nil {
		return nil, err
	}
	defer installLock.Drop()
	if err := removeAbandonedStaging(root); err != nil {
		return nil, err
	}

	cached, err := managedNodeHealth(ctx, install, environment)
	if err != nil {
		return nil, err
	}
	if cached.Kind != ManagedNodeHealthUsable {
		if _, err := os.Lstat(install); err == nil {
			reason, unusable := cached.unusableReason()
			if !unusable {
				reason = "the installation is missing or incomplete"
			}
			if err := repairLocked(install, root, "managed Node.js", func() error {
				return fmt.Errorf("managed Node.js repair deferred: %s is still in use; the cached installation is unusable: %s", install, reason)
			}, "lock invalid managed Node.js for repair"); err != nil {
				return nil, err
			}
		}
		if err := publishManagedNode(ctx, root, install, folder, platform, environment); err != nil {
			return nil, err
		}
	}
	leasePath := filepath.Join(install, LeaseFile)
	leaseFile, err := openLock(leasePath)
	if err != nil {
		return nil, err
	}
	lease, err := lockFile(ctx, leaseFile, true, "managed Node.js lease")
	if err != nil {
		return nil, err
	}
	if err := gcNodeRoot(root, folder); err != nil {
		lease.Drop()
		return nil, err
	}
	installLock.Drop()

	if err := prependPath(environment, filepath.Join(install, "bin")); err != nil {
		lease.Drop()
		return nil, err
	}
	return &ManagedNodeRuntime{path: install, leasePath: leasePath, lease: lease}, nil
}

// publishManagedNode downloads, verifies and extracts the pinned Node.js
// into a staging directory and publishes it at install.
func publishManagedNode(ctx context.Context, root, install, folder string, platform map[string]any, environment map[string]string) error {
	staging, err := os.MkdirTemp(root, ".install-")
	if err != nil {
		return fmt.Errorf("create Node.js staging directory in %s: %w", root, err)
	}
	defer os.RemoveAll(staging)
	file := folder + ".tar.gz"
	archive := filepath.Join(staging, file)
	url, err := nodeDistributionURL(environment, file)
	if err != nil {
		return err
	}
	sha, err := harnessArtifactField(platform, "Node.js", "sha256")
	if err != nil {
		return err
	}
	if err := downloadVerifiedAsync(ctx, url, archive, sha, environment); err != nil {
		return err
	}
	cancellation := PreparationCancellation(ctx)
	extractedErr, err := SpawnBlocking(func() error {
		return extractNodeArchive(cancellation, archive, staging, folder)
	}).Join(ctx)
	if err != nil {
		return fmt.Errorf("Node.js extraction task failed: %w", err)
	}
	if extractedErr != nil {
		return extractedErr
	}
	extracted := filepath.Join(staging, folder)
	health, err := managedNodeHealth(ctx, extracted, environment)
	if err != nil {
		return err
	}
	if reason, unusable := health.unusableReason(); unusable {
		return fmt.Errorf("downloaded Node.js archive is not usable: %s", reason)
	} else if info, err := os.Stat(extracted); err != nil || !info.IsDir() {
		return fmt.Errorf("downloaded Node.js archive did not contain %s", extracted)
	}
	leaseFile, err := openLock(filepath.Join(extracted, LeaseFile))
	if err != nil {
		return err
	}
	leaseFile.Close()
	renameErr := harnessRename(extracted, install)
	if renameErr == nil {
		return nil
	}
	if _, err := os.Lstat(install); err == nil {
		health, err := managedNodeHealth(ctx, install, environment)
		if err != nil {
			return err
		}
		if reason, unusable := health.unusableReason(); unusable {
			return fmt.Errorf("publish managed Node.js %s: %s: %w", install, reason, renameErr)
		}
		return nil
	}
	return fmt.Errorf("publish managed Node.js %s: %w", install, renameErr)
}

func gcNodeRoot(root, keep string) error {
	return gcLeasedDirectories(root, keep, "managed Node.js")
}

// gcLeasedDirectories removes every directory in root but keep (and hidden
// ones) whose lease no process holds. The caller holds root's install lock.
func gcLeasedDirectories(root, keep, what string) error {
	entries, err := os.ReadDir(root)
	if err != nil {
		return fmt.Errorf("scan %s cache %s: %w", what, root, err)
	}
	for _, entry := range entries {
		name := entry.Name()
		if !utf8.ValidString(name) || name == keep || strings.HasPrefix(name, ".") || !entry.IsDir() {
			continue
		}
		path := filepath.Join(root, name)
		leaseFile, err := openLock(filepath.Join(path, LeaseFile))
		if err != nil {
			slog.Warn("could not inspect obsolete "+what, "error", err, "path", path)
			continue
		}
		locked, err := harnessTryLock(leaseFile, false)
		if err != nil {
			leaseFile.Close()
			slog.Warn("could not lock obsolete "+what, "error", err, "path", path)
			continue
		}
		if !locked {
			leaseFile.Close()
			continue
		}
		lease := NewFileLock(leaseFile)
		if err := os.RemoveAll(path); err != nil {
			slog.Warn("could not remove obsolete "+what, "error", err, "path", path)
		}
		lease.Drop()
	}
	return nil
}

func nodeDistributionURL(environment map[string]string, file string) (string, error) {
	// The first setting present wins, even when empty: an explicitly empty
	// NODEJS_ORG_MIRROR selects the official distribution over an inherited
	// NVM_NODEJS_ORG_MIRROR.
	setting, base := "the default mirror", defaultNodeDistURL
	for _, name := range []string{"NODEJS_ORG_MIRROR", "NVM_NODEJS_ORG_MIRROR"} {
		if value, ok := environment[name]; ok {
			if value != "" {
				setting, base = name, value
			}
			break
		}
	}
	// Errors name the setting and the rule, never any part of the URL: a
	// rejected value can hold credentials anywhere (a missing "https://" turns
	// "user:secret@host" into a scheme and path), and these errors reach the
	// daemon log.
	parsed, err := url.Parse(base)
	if err == nil && parsed.Scheme == "" {
		err = errors.New("relative URL without a base")
	}
	if err != nil {
		var urlError *url.Error
		if errors.As(err, &urlError) {
			err = urlError.Err
		}
		return "", fmt.Errorf("%s is not a valid Node.js download mirror URL: %v", setting, err)
	}
	if !artifactURLIsAllowed(parsed) {
		return "", fmt.Errorf("%s must name an HTTPS Node.js download mirror", setting)
	}
	if parsed.User != nil || parsed.RawQuery != "" || parsed.ForceQuery || parsed.Fragment != "" || strings.Contains(base, "#") {
		return "", fmt.Errorf("%s must be a plain HTTPS base URL without credentials, a query, or a fragment", setting)
	}
	return fmt.Sprintf("%s/v%s/%s", strings.TrimRight(base, "/"), NodeVersion, file), nil
}

// ensureNodeArtifactPlatform takes the platform entry of the decoded Node.js
// metadata.
func ensureNodeArtifactPlatform(platform any) error {
	fields, _ := platform.(map[string]any)
	interpreter, ok := fields["interpreter"].(string)
	if !ok {
		return nil
	}
	if info, err := os.Stat(interpreter); err != nil || !info.Mode().IsRegular() {
		return fmt.Errorf("Mjolnir's pinned Node.js for this target is a glibc build, but the required program interpreter %s is missing. Install Node.js 22 or newer with npm and npx on the target PATH", interpreter)
	}
	return nil
}

func systemNodeIsCompatible(ctx context.Context, environment map[string]string) bool {
	path, hasPath := environment["PATH"]
	if _, ok := FindProgramOnPath("node", path, hasPath); !ok {
		return false
	}
	output, err := boundedEnvOutput(ctx, "node", []string{"--version"}, environment)
	if err != nil {
		slog.Warn("system Node.js exists but its version probe failed; using Mjolnir's managed runtime", "error", err)
		return false
	}
	success := output.Status != nil && output.Status.Success()
	if !success || !nodeVersionIsSupported(strings.TrimSpace(strings.ToValidUTF8(string(output.Stdout), "�"))) {
		if !success {
			slog.Warn("system Node.js exists but its version probe exited unsuccessfully; using Mjolnir's managed runtime", "status", harnessExitStatus(output.Status))
		}
		return false
	}
	for _, command := range []string{"npm", "npx"} {
		if _, ok := FindProgramOnPath(command, path, hasPath); !ok {
			return false
		}
		output, err := boundedEnvOutput(ctx, command, []string{"--version"}, environment)
		if err != nil {
			slog.Warn("system Node.js companion command exists but its probe failed; using Mjolnir's managed runtime", "command", command, "error", err)
			return false
		}
		if output.Status == nil || !output.Status.Success() {
			slog.Warn("system Node.js companion command exited unsuccessfully; using Mjolnir's managed runtime", "command", command, "status", harnessExitStatus(output.Status))
			return false
		}
	}
	return true
}

func managedNodeHealth(ctx context.Context, install string, environment map[string]string) (ManagedNodeHealth, error) {
	info, err := os.Stat(install)
	if err != nil {
		return ManagedNodeHealth{Kind: ManagedNodeHealthMissing}, nil
	}
	if !info.IsDir() {
		return ManagedNodeHealth{Kind: ManagedNodeHealthUnusable, Reason: fmt.Sprintf("%s is not a directory", install)}, nil
	}
	bin := filepath.Join(install, "bin")
	for _, name := range []string{"node", "npm", "npx"} {
		if path := filepath.Join(bin, name); !EntrypointIsExecutable(path) {
			return ManagedNodeHealth{Kind: ManagedNodeHealthUnusable, Reason: fmt.Sprintf("%s is missing or not executable", path)}, nil
		}
	}
	probeEnvironment := maps2Clone(environment)
	if err := prependPath(probeEnvironment, bin); err != nil {
		return ManagedNodeHealth{}, err
	}
	output, err := boundedEnvOutput(ctx, filepath.Join(bin, "node"), []string{"--version"}, probeEnvironment)
	if err != nil {
		// A probe cut short by cancellation says nothing about the install,
		// which the caller would otherwise remove.
		if IsPreparationCancelled(err) {
			return ManagedNodeHealth{}, err
		}
		return ManagedNodeHealth{Kind: ManagedNodeHealthUnusable, Reason: fmt.Sprintf("node --version could not run: %v", err)}, nil
	}
	version := strings.TrimSpace(strings.ToValidUTF8(string(output.Stdout), "�"))
	if output.Status == nil || !output.Status.Success() {
		return ManagedNodeHealth{Kind: ManagedNodeHealthUnusable, Reason: fmt.Sprintf("node --version exited with %s", harnessExitStatus(output.Status))}, nil
	}
	if !nodeVersionIsSupported(version) {
		return ManagedNodeHealth{Kind: ManagedNodeHealthUnusable, Reason: fmt.Sprintf("node reports unsupported version %s", debugString(version))}, nil
	}
	for _, name := range []string{"npm", "npx"} {
		output, err := boundedEnvOutput(ctx, filepath.Join(bin, name), []string{"--version"}, probeEnvironment)
		if err != nil {
			if IsPreparationCancelled(err) {
				return ManagedNodeHealth{}, err
			}
			return ManagedNodeHealth{Kind: ManagedNodeHealthUnusable, Reason: fmt.Sprintf("%s --version could not run: %v", name, err)}, nil
		}
		if output.Status == nil || !output.Status.Success() {
			return ManagedNodeHealth{Kind: ManagedNodeHealthUnusable, Reason: fmt.Sprintf("%s --version exited with %s", name, harnessExitStatus(output.Status))}, nil
		}
	}
	return ManagedNodeHealth{Kind: ManagedNodeHealthUsable}, nil
}

// harnessCommand builds a command that runs with exactly environment. A bare
// program name is found on that environment's PATH, as Rust's Command does
// when the child's PATH is set.
func harnessCommand(program string, args []string, environment map[string]string) *exec.Cmd {
	resolved := program
	if !strings.ContainsRune(program, os.PathSeparator) && !strings.ContainsRune(program, '/') {
		if path, ok := environment["PATH"]; ok {
			if found, ok := FindExecutableOnPath(program, path, true); ok {
				resolved = found
			}
		}
	}
	command := exec.Command(resolved, args...)
	if resolved != program {
		command.Args[0] = program
	}
	command.Env = harnessEnvironmentList(environment)
	return command
}

// harnessEnvironmentList is an environment map as exec.Cmd.Env takes it,
// sorted. An empty map is a non-nil empty list, so nothing is inherited.
func harnessEnvironmentList(environment map[string]string) []string {
	list := make([]string, 0, len(environment))
	for name, value := range environment {
		list = append(list, name+"="+value)
	}
	sort.Strings(list)
	return list
}

// harnessExitStatus formats a process exit as Rust's ExitStatus Display
// does.
func harnessExitStatus(state *os.ProcessState) string {
	if state == nil {
		return "unknown status"
	}
	if status, ok := state.Sys().(syscall.WaitStatus); ok && status.Signaled() {
		return fmt.Sprintf("signal: %d (%s)", int(status.Signal()), status.Signal())
	}
	return fmt.Sprintf("exit status: %d", state.ExitCode())
}

// boundedEnvOutput runs command with exactly environment, through the
// shared subprocess helpers.
func boundedEnvOutput(ctx context.Context, program string, args []string, environment map[string]string) (*ProcessOutput, error) {
	command := harnessCommand(program, args, environment)
	output, err := RunBounded(ctx, command, 64*1024, harnessCommandTimeout)
	if err != nil {
		return nil, fmt.Errorf("probe Node.js runtime: %w", err)
	}
	return &output, nil
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
	var existing []string
	if path, ok := environment["PATH"]; ok {
		existing = filepath.SplitList(path)
	}
	if len(existing) > 0 && filepath.Clean(existing[0]) == filepath.Clean(bin) {
		return nil
	}
	for _, entry := range append([]string{bin}, existing...) {
		if strings.ContainsRune(entry, os.PathListSeparator) {
			return fmt.Errorf("construct managed Node.js PATH: path segment contains separator `%c`", os.PathListSeparator)
		}
	}
	path := strings.Join(append([]string{bin}, existing...), string(os.PathListSeparator))
	if !utf8.ValidString(path) {
		return errors.New("managed Node.js PATH is not UTF-8")
	}
	environment["PATH"] = path
	return nil
}

// nodeExtractionHooks is test-only synchronization with the Node.js
// extractor: a test registers a callback for a directory, and the extractor
// calls it before each entry it unpacks under that directory.
var nodeExtractionHooks struct {
	mu    sync.Mutex
	hooks []nodeExtractionHook
}

type nodeExtractionHook struct {
	root string
	hook func(index int)
}

func registerNodeExtractionHook(root string, hook func(index int)) {
	nodeExtractionHooks.mu.Lock()
	defer nodeExtractionHooks.mu.Unlock()
	nodeExtractionHooks.hooks = append(nodeExtractionHooks.hooks, nodeExtractionHook{root, hook})
}

func nodeExtractionEntryReached(destination string, index int) {
	if !testing.Testing() {
		return
	}
	nodeExtractionHooks.mu.Lock()
	var hook func(int)
	for _, registered := range nodeExtractionHooks.hooks {
		if _, ok := harnessStripPrefix(destination, registered.root); ok {
			hook = registered.hook
			break
		}
	}
	nodeExtractionHooks.mu.Unlock()
	if hook != nil {
		hook(index)
	}
}

// harnessTarEntryPath is where the tar crate's unpack_in writes an entry:
// the entry path with any root removed, or ok false for a path that would
// leave destination.
func harnessTarEntryPath(destination, name string) (string, bool) {
	var parts []string
	for _, part := range strings.Split(name, "/") {
		switch part {
		case "", ".":
			continue
		case "..":
			return "", false
		}
		parts = append(parts, part)
	}
	if len(parts) == 0 {
		return "", false
	}
	return filepath.Join(append([]string{destination}, parts...)...), true
}

// harnessUnpackIn writes one tar entry under destination, as the tar
// crate's unpack_in does for the entry types a Node.js archive holds.
func harnessUnpackIn(reader io.Reader, header *tar.Header, destination string) error {
	path, ok := harnessTarEntryPath(destination, header.Name)
	if !ok {
		return nil
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o777); err != nil {
		return err
	}
	mode := fs.FileMode(header.Mode) & 0o777
	switch header.Typeflag {
	case tar.TypeDir:
		if err := os.MkdirAll(path, 0o777); err != nil {
			return err
		}
		return os.Chmod(path, mode)
	case tar.TypeSymlink:
		os.Remove(path)
		return os.Symlink(header.Linkname, path)
	case tar.TypeLink:
		target, ok := harnessTarEntryPath(destination, header.Linkname)
		if !ok {
			return fmt.Errorf("hard link %s escapes the archive", header.Linkname)
		}
		os.Remove(path)
		return os.Link(target, path)
	}
	if !harnessTarIsFile(header) {
		return nil
	}
	return harnessUnpackFile(reader, header, path)
}

// extractNodeArchive unpacks entry by entry (what tar's unpack does) so
// cancellation can stop it between entries; see extractArchiveFile.
func extractNodeArchive(cancellation context.Context, archive, destination, folder string) error {
	archiveFile, err := os.Open(archive)
	if err != nil {
		return fmt.Errorf("open Node.js archive %s: %w", archive, err)
	}
	defer archiveFile.Close()
	if err := os.MkdirAll(destination, 0o777); err != nil {
		return fmt.Errorf("create %s: %w", destination, err)
	}
	decoder, err := gzip.NewReader(bufio.NewReader(archiveFile))
	if err != nil {
		return fmt.Errorf("read Node.js archive: %w", err)
	}
	reader := tar.NewReader(decoder)
	// As the tar crate's unpack does: directory entries (which carry no data)
	// are applied last, deepest first, so a read-only directory's mode is set
	// only after its contents exist. Unpacking creates missing parents itself.
	var directories []*tar.Header
	for index := 0; ; index++ {
		nodeExtractionEntryReached(destination, index)
		if cancellation.Err() != nil {
			return fmt.Errorf("while extracting: %w", PreparationCancelled{})
		}
		header, err := reader.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			return fmt.Errorf("read Node.js archive entry: %w", err)
		}
		if header.Typeflag == tar.TypeDir {
			directories = append(directories, header)
			continue
		}
		if err := harnessUnpackIn(reader, header, destination); err != nil {
			return fmt.Errorf("extract Node.js archive into %s: %w", destination, err)
		}
	}
	sort.Slice(directories, func(i, j int) bool { return directories[i].Name > directories[j].Name })
	for _, directory := range directories {
		if err := harnessUnpackIn(nil, directory, destination); err != nil {
			return fmt.Errorf("extract Node.js archive into %s: %w", destination, err)
		}
	}
	extracted := filepath.Join(destination, folder)
	if info, err := os.Stat(extracted); err != nil || !info.IsDir() {
		return fmt.Errorf("Node.js archive did not extract the expected directory %s", extracted)
	}
	return nil
}

func installKimi(ctx context.Context, staging string, environment map[string]string) error {
	var metadata any
	if err := json.Unmarshal(kimiRuntimeJSON, &metadata); err != nil {
		return err
	}
	return installKimiFromMetadata(ctx, staging, environment, metadata)
}

func installKimiFromMetadata(ctx context.Context, staging string, environment map[string]string, metadata any) error {
	if !harnessMetadataString(metadata, "version", KimiVersion) {
		return errors.New("Kimi Code download metadata does not match the managed runtime pin")
	}
	platform, key, ok := harnessMetadataPlatform(metadata)
	if !ok {
		return fmt.Errorf("Kimi Code does not have a managed runtime for %s", key)
	}
	filename, err := harnessArtifactField(platform, "Kimi Code", "filename")
	if err != nil {
		return err
	}
	destinations := []string{
		fmt.Sprintf("%s/%s/%s", strings.TrimRight(kimiArtifactBase(environment), "/"), KimiVersion, filename),
		fmt.Sprintf("https://code.kimi.ai/kimi-code/binaries/%s/%s", KimiVersion, filename),
	}
	bin := filepath.Join(staging, "bin")
	if err := os.MkdirAll(bin, 0o777); err != nil {
		return err
	}
	entrypoint := filepath.Join(bin, "kimi")
	sha, err := harnessArtifactField(platform, "Kimi Code", "sha256")
	if err != nil {
		return err
	}
	if err := downloadVerifiedFirstAsync(ctx, destinations, entrypoint, sha, environment); err != nil {
		return err
	}
	return os.Chmod(entrypoint, 0o755)
}

func installGrok(ctx context.Context, staging string, environment map[string]string) error {
	var metadata any
	if err := json.Unmarshal(grokRuntimeJSON, &metadata); err != nil {
		return err
	}
	return installGrokFromMetadata(ctx, staging, environment, metadata)
}

func installGrokFromMetadata(ctx context.Context, staging string, environment map[string]string, metadata any) error {
	if !harnessMetadataString(metadata, "version", GrokVersion) {
		return errors.New("Grok download metadata does not match the managed runtime pin")
	}
	platform, key, ok := harnessMetadataPlatform(metadata)
	if !ok {
		return fmt.Errorf("Grok does not have a managed runtime for %s", key)
	}
	target, err := harnessArtifactField(platform, "Grok", "target")
	if err != nil {
		return err
	}
	filename := fmt.Sprintf("grok-%s-%s.zst", GrokVersion, target)
	destinations := []string{
		fmt.Sprintf("%s/%s", strings.TrimRight(grokArtifactBase(environment), "/"), filename),
		"https://storage.googleapis.com/grok-build-public-artifacts/cli/" + filename,
	}
	archive := filepath.Join(staging, filename)
	sha, err := harnessArtifactField(platform, "Grok", "sha256")
	if err != nil {
		return err
	}
	if err := downloadVerifiedFirstAsync(ctx, destinations, archive, sha, environment); err != nil {
		return err
	}
	bin := filepath.Join(staging, "bin")
	if err := os.MkdirAll(bin, 0o777); err != nil {
		return err
	}
	entrypoint := filepath.Join(bin, "grok")
	cancellation := PreparationCancellation(ctx)
	extracted, err := SpawnBlocking(func() error {
		return extractZstdFile(cancellation, archive, entrypoint)
	}).Join(ctx)
	if err != nil {
		return fmt.Errorf("Grok extraction task failed: %w", err)
	}
	if extracted != nil {
		return extracted
	}
	if err := os.Remove(archive); err != nil {
		return err
	}
	if err := os.Chmod(entrypoint, 0o755); err != nil {
		return err
	}
	if err := os.Symlink("grok", filepath.Join(bin, "agent")); err != nil {
		return fmt.Errorf("create managed Grok agent launcher: %w", err)
	}
	return nil
}

// extractZstdFile copies in chunks so cancellation lands within one chunk;
// see extractArchiveFile.
func extractZstdFile(cancellation context.Context, archive, destination string) error {
	input, err := os.Open(archive)
	if err != nil {
		return fmt.Errorf("open Grok archive %s: %w", archive, err)
	}
	defer input.Close()
	decoder, err := zstd.NewReader(bufio.NewReader(input))
	if err != nil {
		return fmt.Errorf("read Grok archive %s: %v", archive, err)
	}
	defer decoder.Close()
	output, err := os.Create(destination)
	if err != nil {
		return fmt.Errorf("create Grok executable %s: %w", destination, err)
	}
	defer output.Close()
	chunk := make([]byte, extractChunkBytes)
	for {
		if cancellation.Err() != nil {
			return fmt.Errorf("while extracting: %w", PreparationCancelled{})
		}
		count, readErr := decoder.Read(chunk)
		if count > 0 {
			if _, err := output.Write(chunk[:count]); err != nil {
				return fmt.Errorf("extract Grok executable to %s: %w", destination, err)
			}
		}
		if readErr == io.EOF {
			return nil
		}
		if readErr != nil {
			return fmt.Errorf("read Grok archive %s: %w", archive, readErr)
		}
	}
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
	command := harnessCommand(program, args, environment)
	command.Dir = dir
	return runBoundedCommandChecked(ctx, command, operation, timeout)
}

// runBoundedCommandChecked runs a prepared command with Rust's
// run_bounded_checked bounds and failure text.
func runBoundedCommandChecked(ctx context.Context, command *exec.Cmd, operation string, timeout time.Duration) error {
	output, err := RunBounded(ctx, command, 4*1024*1024, timeout)
	if err != nil {
		return fmt.Errorf("%s: %w", operation, err)
	}
	if output.Status != nil && output.Status.Success() {
		return nil
	}
	return fmt.Errorf("%s failed: %s", operation, outputSummary(&output))
}

func outputSummary(output *ProcessOutput) string {
	stderr := strings.TrimSpace(strings.ToValidUTF8(string(output.Stderr), "�"))
	stdout := strings.TrimSpace(strings.ToValidUTF8(string(output.Stdout), "�"))
	detail := stderr
	if detail == "" {
		detail = stdout
	}
	runes := []rune(detail)
	if len(runes) > 4_000 {
		runes = runes[len(runes)-4_000:]
	}
	return fmt.Sprintf("%s (%s)", harnessExitStatus(output.Status), string(runes))
}

// downloadVerified is Rust's test-only blocking download_verified.
func downloadVerified(rawURL, destination, expected string, environment map[string]string) error {
	return downloadVerifiedAsync(context.Background(), rawURL, destination, expected, environment)
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
	if info, err := os.Stat(harnessRoot); err != nil || !info.IsDir() {
		return nil
	}
	lockHandle, err := openLock(filepath.Join(harnessRoot, installLockFile))
	if err != nil {
		return err
	}
	locked, err := harnessTryLock(lockHandle, false)
	if err != nil {
		lockHandle.Close()
		return fmt.Errorf("lock managed harness installer for collection: %w", err)
	}
	if !locked {
		lockHandle.Close()
		return nil
	}
	lock := NewFileLock(lockHandle)
	defer lock.Drop()
	return gcHarnessRoot(harnessRoot, keepInstallID)
}

// gcHarnessRoot: the caller holds the install lock for root.
func gcHarnessRoot(root, keepInstallID string) error {
	return gcLeasedDirectories(root, keepInstallID, "managed harness")
}

func AcquireSupervisorLease(path string) (*FileLock, error) {
	lease, err := openLock(path)
	if err != nil {
		return nil, err
	}
	if err := harnessLock(lease, true); err != nil {
		lease.Close()
		return nil, fmt.Errorf("lease managed harness for supervisor %s: %w", path, err)
	}
	return NewFileLock(lease), nil
}
