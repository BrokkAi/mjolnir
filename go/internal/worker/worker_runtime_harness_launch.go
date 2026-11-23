// Port of mj-worker/src/worker_runtime/harness_launch.rs.

// Select the concrete executable before starting a harness.
//
// Rust re-exports mj-harness-install's find_command here as
// `harness_launch::find_command`; Go callers use harnessinstall.FindCommand
// directly.

//go:build unix

package worker

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"maps"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
	"github.com/BrokkAi/mjolnir/go/internal/workerreview"
)

// PreparedHarnessLaunch keeps this preparation alive until the supervisor
// holds its installation lease. Release is Rust's Drop of the leases.
type PreparedHarnessLaunch struct {
	Spec        AcpSupervisorSpec
	environment map[string]string
	managed     *harnessinstall.ManagedHarness
	managedNode *harnessinstall.ManagedNodeRuntime
}

// Release drops the installation leases this preparation holds.
func (l *PreparedHarnessLaunch) Release() {
	if l.managed != nil {
		l.managed.Release()
	}
	if l.managedNode != nil {
		l.managedNode.Release()
	}
}

// PrepareHarnessLaunch resolves the login environment, the managed Node.js
// runtime, the build caches and the harness executable for a bridge launch.
func PrepareHarnessLaunch(ctx context.Context, harness core.HarnessKind, policy core.HarnessRuntimePolicy, executionPolicy core.ExecutionPolicy, spec AcpSupervisorSpec) (*PreparedHarnessLaunch, error) {
	if spec.Environment == nil {
		spec.Environment = map[string]string{}
	}
	delete(spec.Environment, core.FastinoKeyEnvironment)
	environment, err := harnessinstall.WithOverrides(ctx, spec.Environment)
	if err != nil {
		return nil, err
	}
	if err := linkBuildCacheConfiguration(ctx, environment); err != nil {
		return nil, err
	}
	excludeFromHarnessEnvironment(harness, spec.ExcludedEnvironment, environment, spec.Environment)
	for _, name := range spec.ExcludedEnvironment {
		delete(environment, name)
	}
	// Resolve relative PATH entries against the same directory the bridge uses.
	if path, ok := environment["PATH"]; ok {
		path, err := resolveRelativePathEntries(path, spec.Cwd)
		if err != nil {
			return nil, err
		}
		environment["PATH"] = path
		spec.Environment["PATH"] = path
	}
	launch := &PreparedHarnessLaunch{}
	// The image's harnesses bring their own Node.js; only a source that may
	// install prepares Mjolnir's.
	if policy.MayInstall() {
		needed, err := needsNodeRuntime(ctx, harness, environment)
		if err != nil {
			return nil, err
		}
		if needed {
			node, err := harnessinstall.EnsureNodeRuntime(ctx, environment)
			if err != nil {
				return nil, fmt.Errorf("prepare Node.js for %s: %w", harness.DisplayName(), err)
			}
			if node != nil {
				launch.managedNode = node
				lease := node.LeasePath()
				spec.NodeLease = &lease
				path, ok := environment["PATH"]
				if !ok {
					launch.Release()
					return nil, errors.New("managed Node.js runtime did not produce PATH")
				}
				spec.Environment["PATH"] = path
			}
		}
	}
	before := maps.Clone(environment)
	if err := toolCachePrepare(spec.Cwd, environment); err != nil {
		launch.Release()
		return nil, err
	}
	for name, value := range environment {
		if previous, ok := before[name]; !ok || previous != value {
			spec.Environment[name] = value
		}
	}
	located, err := harnessinstall.Locate(ctx, policy, harness, executionPolicy, spec.Command, spec.Args, environment)
	if err != nil {
		launch.Release()
		return nil, fmt.Errorf("prepare %s: %w", harness.DisplayName(), err)
	}
	spec.Command = located.Command
	spec.Args = located.Args
	maps.Copy(spec.Environment, located.Environment)
	maps.Copy(environment, located.Environment)
	launch.managed = located.Managed
	if located.Managed != nil {
		lease := located.Managed.LeasePath
		spec.HarnessLease = &lease
	}
	command := spec.Command
	if !filepath.IsAbs(command) && len(workerRuntimePathComponents(command)) > 1 {
		command = core.Join(spec.Cwd, command)
	}
	resolved, err := resolveCommand(command, environment)
	if err != nil {
		launch.Release()
		return nil, err
	}
	spec.Command = resolved
	// Freeze the explicit provider selection as well as the bridge.
	if provider, ok := environment["CODEX_PATH"]; harness == core.HarnessKindCodex && ok && provider != "" {
		if !filepath.IsAbs(provider) && len(workerRuntimePathComponents(provider)) > 1 {
			provider = core.Join(spec.Cwd, provider)
		}
		found, ok, err := harnessinstall.FindCommand(provider, environment)
		if err != nil {
			launch.Release()
			return nil, err
		}
		if ok {
			spec.Environment["CODEX_PATH"] = found
			environment["CODEX_PATH"] = found
		}
	}
	launch.Spec = spec
	launch.environment = environment
	return launch, nil
}

// resolveRelativePathEntries joins every relative PATH entry onto cwd, as
// Rust's split_paths and join_paths do with Path::join.
func resolveRelativePathEntries(path string, cwd string) (string, error) {
	// Rust's split_paths yields every entry, the empty ones included.
	entries := strings.Split(path, string(os.PathListSeparator))
	for index, entry := range entries {
		if !filepath.IsAbs(entry) {
			entries[index] = core.Join(cwd, entry)
		}
		if strings.ContainsRune(entries[index], os.PathListSeparator) {
			return "", fmt.Errorf("path segment contains separator `%c`", os.PathListSeparator)
		}
	}
	return strings.Join(entries, string(os.PathListSeparator)), nil
}

// freezeNodeEntrypoints keeps Mjolnir's JavaScript entrypoints independent
// of project-selected Node shims. Provider launch APIs accept an executable
// path, so those get a private launcher rather than depending on the
// bridge's PATH again.
func freezeNodeEntrypoints(ctx context.Context, root string, launch *PreparedHarnessLaunch) error {
	bridgeUsesNode, err := usesEnvNode(ctx, launch.Spec.Command)
	if err != nil {
		return err
	}
	type provider struct {
		name       string
		entrypoint string
	}
	var providers []provider
	for _, name := range []string{"CODEX_PATH", "CLAUDE_CODE_EXECUTABLE", "MUSE_CLI"} {
		path, ok := launch.environment[name]
		if !ok {
			continue
		}
		uses, err := usesEnvNode(ctx, path)
		if err != nil {
			return err
		}
		if uses {
			providers = append(providers, provider{name: name, entrypoint: path})
		}
	}
	if !bridgeUsesNode && len(providers) == 0 {
		return nil
	}
	probe := exec.Command("node", "-p", "process.execPath")
	probe.Env = workerRuntimeEnvironmentList(launch.environment)
	probe.Dir = launch.Spec.Cwd
	output, err := core.Capture(ctx, probe)
	if err != nil {
		return fmt.Errorf("resolve bridge Node executable: %w", err)
	}
	if !output.Status.Success() {
		return fmt.Errorf("resolve bridge Node executable: %s", fromUTF8Lossy(output.Stderr))
	}
	if !utf8.Valid(output.Stdout) {
		return errors.New("bridge Node executable path is not UTF-8")
	}
	// process.execPath resolves version-manager shims to the actual interpreter.
	node := strings.TrimSpace(string(output.Stdout))
	if !filepath.IsAbs(node) {
		return errors.New("bridge Node executable is not absolute")
	}
	if bridgeUsesNode {
		launch.Spec.Args = slices.Insert(launch.Spec.Args, 0, launch.Spec.Command)
		launch.Spec.Command = node
	}
	root = filepath.Join(root, "node-entrypoints")
	type launcher struct {
		name string
		path string
	}
	var launchers []launcher
	if len(providers) > 0 {
		if err := os.MkdirAll(root, 0o777); err != nil {
			return fmt.Errorf("prepare Node entrypoint launchers: %w", err)
		}
	}
	for _, provider := range providers {
		path := filepath.Join(root, provider.name)
		script := fmt.Sprintf("#!/bin/sh\nexec %s %s \"$@\"\n", core.PosixQuote(node), core.PosixQuote(provider.entrypoint))
		// The shared writer never holds a writable descriptor in this
		// multithreaded process, so a concurrent fork cannot make the
		// published launcher fail with ETXTBSY.
		if err := writeExecutable(path, script); err != nil {
			return fmt.Errorf("prepare Node entrypoint launchers: %w", err)
		}
		launchers = append(launchers, launcher{name: provider.name, path: path})
	}
	if launch.Spec.Environment == nil {
		launch.Spec.Environment = map[string]string{}
	}
	if launch.environment == nil {
		launch.environment = map[string]string{}
	}
	for _, launcher := range launchers {
		launch.environment[launcher.name] = launcher.path
		launch.Spec.Environment[launcher.name] = launcher.path
	}
	return nil
}

// workerRuntimeEnvironmentList is Rust's `env_clear().envs(map)`: the
// child's whole environment, from environment alone.
func workerRuntimeEnvironmentList(environment map[string]string) []string {
	list := make([]string, 0, len(environment))
	for _, name := range slices.Sorted(maps.Keys(environment)) {
		list = append(list, name+"="+environment[name])
	}
	return list
}

func usesEnvNode(ctx context.Context, path string) (bool, error) {
	file, err := os.Open(path)
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	if err != nil {
		return false, fmt.Errorf("read runtime entrypoint %s: %w", path, err)
	}
	defer file.Close()
	// Read just the interpreter prefix, not the JavaScript bundle or ELF body.
	prefix, err := io.ReadAll(io.LimitReader(file, int64(len("#!/usr/bin/env node\r\n"))))
	if err != nil {
		return false, err
	}
	return bytes.HasPrefix(prefix, []byte("#!/usr/bin/env node\n")) ||
		bytes.HasPrefix(prefix, []byte("#!/usr/bin/env node\r\n")), nil
}

// needsNodeRuntime: DSH can launch its own native binary. Its npm fallback
// is the only branch that needs Node.js. Other Node-using harnesses always
// need the runtime, even when their adapter is already installed.
func needsNodeRuntime(ctx context.Context, harness core.HarnessKind, environment map[string]string) (bool, error) {
	dshIsNative := false
	if harness == core.HarnessKindDsh {
		if _, ok := environment["PATH"]; ok {
			command, found, err := harnessinstall.FindCommandPath("dsh-acp", environment)
			if err != nil {
				return false, err
			}
			if found {
				probe := exec.Command(command, "--version")
				probe.Env = workerRuntimeEnvironmentList(environment)
				output, err := core.RunBounded(ctx, probe, 4096, 5*time.Second)
				switch {
				case err != nil:
					slog.Warn("could not read dsh-acp version; using the pinned Node.js fallback", "error", err)
				case output.Status.Success():
					dshIsNative = core.DshACPVersionSupportsNativeInstructions(fromUTF8Lossy(output.Stdout))
				}
			}
		}
	}
	return core.HarnessUsesNodeRuntime(harness) && !dshIsNative, nil
}

// linkBuildCacheConfiguration: reviewer homes can be created after
// provisioning. Link their mbx lookup to machine policy too, without
// changing XDG_CONFIG_HOME for any other program.
func linkBuildCacheConfiguration(ctx context.Context, environment map[string]string) error {
	sourceDirectory, ok := environment["MJ_MBX_CONFIG_DIR"]
	if !ok {
		return nil
	}
	source := filepath.Join(sourceDirectory, "config.toml")
	root := environment["XDG_CONFIG_HOME"]
	if root == "" {
		home, ok := environment["HOME"]
		if !ok {
			return errors.New("mbx configuration needs HOME")
		}
		root = filepath.Join(home, ".config")
	}
	// The shared cache is optional. The controller reports why it could not
	// prepare the policy and starts the session without the mbx launchers,
	// so a policy that is absent here must not refuse the harness. Report
	// the OS error rather than calling every failure "missing": a host file
	// the container cannot reach is a different problem from one that was
	// never published.
	directory := filepath.Join(root, "mbx")
	destination := filepath.Join(directory, "config.toml")
	metadata, err := os.Stat(source)
	if err != nil {
		removeStaleConfigurationLink(destination, source)
		slog.Warn("shared machine mbx configuration is unavailable; mbx uses its own defaults", "configuration", source, "error", err)
		return nil
	}
	if !metadata.Mode().IsRegular() {
		removeStaleConfigurationLink(destination, source)
		slog.Warn("shared machine mbx configuration is not a file; mbx uses its own defaults", "configuration", source)
		return nil
	}
	if err := os.MkdirAll(directory, 0o777); err != nil {
		return fmt.Errorf("create mbx configuration lookup directory: %w", err)
	}
	if target, err := os.Readlink(destination); err == nil && target == source {
		return nil
	}
	staging, err := os.MkdirTemp(directory, ".tmp")
	if err != nil {
		return fmt.Errorf("stage mbx configuration link: %w", err)
	}
	defer os.RemoveAll(staging)
	link := filepath.Join(staging, "config.toml")
	if err := os.Symlink(source, link); err != nil {
		return fmt.Errorf("link shared mbx configuration: %w", err)
	}
	if err := os.Rename(link, destination); err != nil {
		return fmt.Errorf("publish mbx configuration link: %w", err)
	}
	return nil
}

// removeStaleConfigurationLink: a policy that existed when the home was
// prepared can disappear, leaving a link to nothing. Remove only Mjolnir's
// own link; a real file belongs to the user.
func removeStaleConfigurationLink(destination string, source string) {
	if target, err := os.Readlink(destination); err != nil || target != source {
		return
	}
	if err := os.Remove(destination); err != nil {
		slog.Warn("could not remove a stale shared mbx configuration link", "configuration", destination, "error", err)
	}
}

func resolveCommand(command string, environment map[string]string) (string, error) {
	found, ok, err := harnessinstall.FindCommand(command, environment)
	if err != nil {
		return "", err
	}
	if !ok {
		return "", errors.New("runtime command is not executable on the selected PATH")
	}
	return found, nil
}

// primarySupervisorSpec: only a supported primary may enforce the
// repository's review gate.
func primarySupervisorSpec(config *core.WorkerLaunchConfig) AcpSupervisorSpec {
	spec := AcpSupervisorSpecFrom(config)
	delete(spec.Environment, "MJ_REVIEW_GATE")
	if config.ReviewGate() {
		spec.ExcludedEnvironment = slices.DeleteFunc(spec.ExcludedEnvironment, func(name string) bool { return name == "MJ_REVIEW_GATE" })
		spec.Environment["MJ_REVIEW_GATE"] = "1"
	} else if !slices.Contains(spec.ExcludedEnvironment, "MJ_REVIEW_GATE") {
		spec.ExcludedEnvironment = append(spec.ExcludedEnvironment, "MJ_REVIEW_GATE")
	}
	return spec
}

// prepareReviewGate records each repository's session start and, for a
// gated primary, installs the before-push hook; it returns notices for the
// user.
func prepareReviewGate(launch *core.WorkerLaunchConfig) ([]string, error) {
	if !launch.PreparesReview() {
		return []string{}, nil
	}
	notices := []string{}
	if !launch.ReviewTools() {
		notices = append(notices, fmt.Sprintf("Before-push review is not available for %s; this session runs without review.", launch.Harness.DisplayName()))
		return notices, nil
	}
	runner := workerreview.NewBoundedGit(workerreview.WorkspaceStateTimeout)
	cancelRunner := runner.CancellationGuard()
	defer cancelRunner.Cancel()
	roots := append([]string{launch.Cwd}, reviewerWorkspaceDirectories(launch)...)
	for _, repository := range workerreview.DiscoverRepositories(runner, roots) {
		// Every session that may request review records where its work
		// starts, so its first review covers the commits made after it.
		if _, err := workerreview.EnsureSessionStartHead(runner, repository, launch.SessionID); err != nil {
			return nil, err
		}
		// Only a gated primary also holds its pushes to review.
		if !launch.ReviewGate() {
			continue
		}
		var reason *string
		installed, err := workerreview.InstallPrePushHook(runner, repository)
		if err != nil {
			text := err.Error()
			reason = &text
		} else {
			switch installed := installed.(type) {
			case workerreview.HookInstallRefusedExistingBackup:
				text := fmt.Sprintf("%s already exists", installed.Path)
				reason = &text
			case workerreview.HookInstallRefusedWorkTreeHooksDirectory:
				text := fmt.Sprintf("%s is inside the work tree and is not ignored", installed.Path)
				reason = &text
			}
		}
		if reason != nil {
			notices = append(notices, fmt.Sprintf("Before-push review in %s: %s; the gate is not installed.", repository, *reason))
		}
	}
	return notices, nil
}
