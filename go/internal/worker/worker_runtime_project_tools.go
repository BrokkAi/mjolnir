// Port of mj-worker/src/worker_runtime/project_tools.rs.

// Container project recipes run after clone; failures remain repairable
// in-session.

//go:build unix

package worker

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
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

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
)

const projectToolsLogFile = "project-tools.log"

// projectToolsTakeRoots removes the controller's explicit launch directive.
// Only that directive opts a worker in. Bare targets never receive it; the
// harness never receives the internal directive. nil when it is absent.
func projectToolsTakeRoots(environment map[string]string) (*[]string, error) {
	value, ok := environment[core.SessionProjectToolsRoots]
	if !ok {
		return nil, nil
	}
	delete(environment, core.SessionProjectToolsRoots)
	var roots []string
	if err := json.Unmarshal([]byte(value), &roots); err != nil {
		return nil, fmt.Errorf("read project tool checkout roots: %w", err)
	}
	if roots == nil {
		return nil, errors.New("read project tool checkout roots: invalid type: null, expected a sequence")
	}
	return &roots, nil
}

// projectToolsApplyEnvironment is the single owner for both preparation
// commands and the final harness environment, applied only when a checkout
// has a recipe. Image-provided values win so prebuilt installs remain
// reusable.
func projectToolsApplyEnvironment(environment map[string]string, roots []string, mise *string) error {
	data, ok := environment["MISE_DATA_DIR"]
	if !ok {
		data = "/var/lib/hel/mise"
		environment["MISE_DATA_DIR"] = data
	}
	// RUSTUP_HOME and CARGO_HOME are left to the image: unset, rustup uses
	// $HOME/.rustup and $HOME/.cargo, the same HOME for this step and the
	// harness, so an image's existing toolchain stays visible.
	var trusted []string
	for _, path := range toolCacheSplitPaths(environment["MISE_TRUSTED_CONFIG_PATHS"]) {
		if path != "" && !slices.ContainsFunc(roots, func(root string) bool { return workerRuntimePathEqual(root, path) }) {
			trusted = append(trusted, path)
		}
	}
	trusted = append(trusted, roots...)
	trustedPaths, err := toolCacheJoinPaths(trusted)
	if err != nil {
		return err
	}
	environment["MISE_TRUSTED_CONFIG_PATHS"] = trustedPaths
	environment["MISE_IDIOMATIC_VERSION_FILE_ENABLE_TOOLS"] = "rust,node,python,ruby,go"
	shims := core.Join(data, "shims")
	old := toolCacheSplitPaths(environment["PATH"])
	// Cache launchers must wrap tools provided by mise. Cargo's mbx launcher
	// lives alongside the session's GitHub CLI even without a host tool
	// mount.
	var candidates []string
	if bin, ok := environment["MJ_BUILD_TOOL_BIN"]; ok {
		candidates = append(candidates, bin)
	}
	if bin, ok := environment[githubCLIBinEnv]; ok {
		if _, mbx := environment["MBX_SHIMS_DIR"]; mbx {
			candidates = append(candidates, bin)
		}
	}
	var wrappers []string
	for _, bin := range candidates {
		if slices.ContainsFunc(old, func(entry string) bool { return workerRuntimePathEqual(entry, bin) }) {
			wrappers = append(wrappers, bin)
		}
	}
	var miseDirectory *string
	if mise != nil {
		if parent, ok := workerRuntimePathParent(*mise); ok {
			miseDirectory = &parent
		}
	}
	entries := slices.Clone(wrappers)
	entries = append(entries, shims)
	if miseDirectory != nil {
		entries = append(entries, *miseDirectory)
	}
	for _, entry := range old {
		if workerRuntimePathEqual(entry, shims) ||
			(miseDirectory != nil && workerRuntimePathEqual(entry, *miseDirectory)) ||
			slices.ContainsFunc(wrappers, func(wrapper string) bool { return workerRuntimePathEqual(entry, wrapper) }) {
			continue
		}
		entries = append(entries, entry)
	}
	path, err := toolCacheJoinPaths(entries)
	if err != nil {
		return err
	}
	environment["PATH"] = path
	return nil
}

// projectToolsTryExists is Rust's `fs::try_exists`: false for a missing
// path or a dangling link, an error when existence cannot be checked.
func projectToolsTryExists(path string) (bool, error) {
	_, err := os.Stat(path)
	if err == nil {
		return true, nil
	}
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	return false, err
}

func projectToolsRecipeRoots(ctx context.Context, roots []string) ([]string, error) {
	found := []string{}
	for _, root := range roots {
		for _, file := range harnessinstall.RecipeFiles {
			path := filepath.Join(root, file)
			exists, err := projectToolsTryExists(path)
			if err != nil {
				return nil, fmt.Errorf("check project recipe %s: %w", path, err)
			}
			if exists {
				found = append(found, root)
				break
			}
		}
	}
	return found, nil
}

// projectToolsMissing is what one recipe checkout still needs, from mise's
// own status commands.
type projectToolsMissing struct {
	root string
	// files are the recipe files present in the checkout.
	files []string
	// tools are `tool@version` requests that are not installed.
	tools []string
	// packages are `manager:package` entries of `[bootstrap.packages]` that
	// are missing.
	packages []string
	// statusFailure is set when a status command failed; the message then
	// names the recipe files instead of guessing tool names.
	statusFailure *string
	// hook is the recipe file whose `[hooks] postinstall` still has to run.
	hook *string
}

func (m *projectToolsMissing) isEmpty() bool {
	return len(m.tools) == 0 && len(m.packages) == 0 && m.statusFailure == nil && m.hook == nil
}

func (m *projectToolsMissing) describe() string {
	files := strings.Join(m.files, ", ")
	items := slices.Clone(m.tools)
	items = append(items, m.packages...)
	if m.hook != nil {
		items = append(items, fmt.Sprintf("the postinstall hook in %s", *m.hook))
	}
	if m.statusFailure != nil || len(items) == 0 {
		return fmt.Sprintf("everything the recipe in %s declares (%s; mise could not say what is missing)", m.root, files)
	}
	return fmt.Sprintf("%s (from %s in %s)", strings.Join(items, ", "), files, m.root)
}

// projectToolsInstall is the fast part of project tools, finished before the
// harness starts: mise is present, the session environment is applied, and
// mise has said what is missing.
type projectToolsInstall struct {
	mise        string
	environment map[string]string
	missing     []projectToolsMissing
	// launchMs is unique per worker start, so a resumed session's messages
	// are never mistaken for an earlier launch's already-delivered ones.
	launchMs int64
}

func projectToolsStatusOutput(ctx context.Context, mise string, root string, environment map[string]string, args []string) ([]byte, error) {
	command := exec.Command(mise, args...)
	command.Dir = root
	command.Env = workerRuntimeEnvironmentList(environment)
	joined := strings.Join(args, " ")
	output, err := core.Capture(ctx, command)
	if err != nil {
		return nil, fmt.Errorf("mise %s: %w", joined, err)
	}
	if !output.Status.Success() {
		return nil, fmt.Errorf("mise %s failed: %s: %s", joined, workFactsExitStatus(output.Status),
			strings.TrimSpace(fromUTF8Lossy(output.Stderr)))
	}
	return output.Stdout, nil
}

func projectToolsStatus(ctx context.Context, mise string, root string, environment map[string]string, args []string) (any, error) {
	output, err := projectToolsStatusOutput(ctx, mise, root, environment, args)
	if err != nil {
		return nil, err
	}
	value, err := jsonvalue.Parse(output)
	if err != nil {
		return nil, fmt.Errorf("read mise %s output: %w", strings.Join(args, " "), err)
	}
	return value, nil
}

func projectToolsMissingIn(ctx context.Context, mise string, root string, environment map[string]string) (projectToolsMissing, error) {
	var files []string
	for _, file := range harnessinstall.RecipeFiles {
		path := filepath.Join(root, file)
		exists, err := projectToolsTryExists(path)
		if err != nil {
			return projectToolsMissing{}, fmt.Errorf("check project recipe %s: %w", path, err)
		}
		if exists {
			files = append(files, file)
		}
	}
	missing := projectToolsMissing{root: root, files: files}
	// `mise ls --missing --json`: an object keyed by tool name whose records
	// list the requested versions that are not installed.
	// `mise bootstrap packages status --json`: see unsatisfiedPackages.
	err := func() error {
		// `ls --missing` silently omits a request mise cannot resolve (an
		// unknown tool); a dry run fails for it, and the failure then makes
		// the installation run so the broken recipe is reported.
		if _, err := projectToolsStatusOutput(ctx, mise, root, environment, []string{"install", "--dry-run"}); err != nil {
			return err
		}
		tools, err := projectToolsStatus(ctx, mise, root, environment, []string{"ls", "--missing", "--json"})
		if err != nil {
			return err
		}
		if object, ok := tools.(jsonvalue.Object); ok {
			for _, member := range object {
				records, _ := member.Value.([]any)
				for _, record := range records {
					if installed := projectToolsIndex(record, "installed"); installed != false {
						continue
					}
					version := "latest"
					if requested, ok := projectToolsIndex(record, "requested_version").(string); ok {
						version = requested
					} else if fallback, ok := projectToolsIndex(record, "version").(string); ok {
						version = fallback
					}
					missing.tools = append(missing.tools, fmt.Sprintf("%s@%s", member.Key, version))
				}
			}
		}
		packages, err := projectToolsStatus(ctx, mise, root, environment, []string{"bootstrap", "packages", "status", "--json"})
		if err != nil {
			return err
		}
		missing.packages = unsatisfiedPackages(packages)
		return nil
	}()
	if err != nil {
		missing.tools = nil
		missing.packages = nil
		failure := err.Error()
		missing.statusFailure = &failure
	}
	if missing.isEmpty() {
		hook, err := pendingPostinstallHook(ctx, root, harnessinstall.ImageRecipeDir)
		if err != nil {
			return projectToolsMissing{}, err
		}
		missing.hook = hook
	}
	return missing, nil
}

// projectToolsIndex is serde_json's `value[key]`: the member of an object,
// nil (JSON null) for anything else.
func projectToolsIndex(value any, key string) any {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil
	}
	member, _ := object.Get(key)
	return member
}

// pendingPostinstallHook: a global `[hooks] postinstall` runs on every
// `mise install`, and no status command says whether it is satisfied. An
// image built from exactly this recipe ran it at build time; anywhere else
// (a stale image, or a container whose earlier install stopped during the
// hook) it still has to run. Returns the recipe file that declares the hook
// in that case.
func pendingPostinstallHook(ctx context.Context, root string, baked string) (*string, error) {
	read := func(path string) ([]byte, bool, error) {
		contents, err := os.ReadFile(path)
		if errors.Is(err, fs.ErrNotExist) {
			return nil, false, nil
		}
		if err != nil {
			return nil, false, fmt.Errorf("read %s: %w", path, err)
		}
		return contents, true, nil
	}
	var hook *string
	builtFromThisRecipe := true
	for _, file := range harnessinstall.RecipeFiles {
		contents, present, err := read(filepath.Join(root, file))
		if err != nil {
			return nil, err
		}
		image, imagePresent, err := read(filepath.Join(baked, file))
		if err != nil {
			return nil, err
		}
		if present != imagePresent || !bytes.Equal(contents, image) {
			builtFromThisRecipe = false
		}
		isMiseConfig := strings.HasSuffix(file, ".toml") && file != "rust-toolchain.toml"
		if !isMiseConfig || !present {
			continue
		}
		var table map[string]any
		if utf8.Valid(contents) {
			table, err = core.TOMLTableFromStr(string(contents))
		}
		if !utf8.Valid(contents) || err != nil {
			return nil, fmt.Errorf("parse %s", filepath.Join(root, file))
		}
		if hooks, ok := table["hooks"].(map[string]any); ok {
			if _, ok := hooks["postinstall"]; ok && hook == nil {
				declared := file
				hook = &declared
			}
		}
	}
	if builtFromThisRecipe {
		return nil, nil
	}
	return hook, nil
}

// unsatisfiedPackages are the `[bootstrap.packages]` entries that `mise
// bootstrap packages apply` would change, from `mise bootstrap packages
// status --json`: package managers whose packages carry a `state` of
// "installed", "missing" or "version_mismatch". mise 2026.11.9 reports a `*`
// request as a version mismatch whatever version is installed, so that one
// mismatch counts as satisfied.
func unsatisfiedPackages(status any) []string {
	unsatisfied := []string{}
	object, _ := status.(jsonvalue.Object)
	for _, member := range object {
		packages, _ := projectToolsIndex(member.Value, "packages").([]any)
		for _, pkg := range packages {
			requested, ok := projectToolsIndex(pkg, "requested_version").(string)
			if !ok {
				requested = "latest"
			}
			needed := projectToolsIndex(pkg, "desired_state") == "present"
			if needed {
				state, _ := projectToolsIndex(pkg, "state").(string)
				switch state {
				case "installed":
					needed = false
				case "version_mismatch":
					needed = requested != "*"
				}
			}
			if !needed {
				continue
			}
			name, ok := projectToolsIndex(pkg, "package").(string)
			if !ok {
				name = "?"
			}
			switch requested {
			case "latest", "*":
				unsatisfied = append(unsatisfied, fmt.Sprintf("%s:%s", member.Key, name))
			default:
				unsatisfied = append(unsatisfied, fmt.Sprintf("%s:%s=%s", member.Key, name, requested))
			}
		}
	}
	return unsatisfied
}

// projectToolsPlan installs mise if the image has none, applies the session
// environment and asks mise what each recipe checkout is missing. nil when
// nothing is missing.
func projectToolsPlan(ctx context.Context, workerRoot string, roots []string, environment map[string]string) (*projectToolsInstall, error) {
	mise, err := harnessinstall.EnsureMiseRuntime(ctx, environment)
	if err != nil {
		return nil, fmt.Errorf("install mise runtime: %w", err)
	}
	if err := projectToolsApplyEnvironment(environment, roots, &mise); err != nil {
		return nil, err
	}
	var missing []projectToolsMissing
	for _, root := range roots {
		found, err := projectToolsMissingIn(ctx, mise, root, environment)
		if err != nil {
			return nil, err
		}
		if found.statusFailure != nil {
			text := fmt.Sprintf("\nmise could not report what is missing in %s: %s\n", root, *found.statusFailure)
			if err := projectToolsAppendLog(ctx, workerRoot, text); err != nil {
				return nil, err
			}
		}
		if !found.isEmpty() {
			missing = append(missing, found)
		}
	}
	if len(missing) == 0 {
		return nil, nil
	}
	return &projectToolsInstall{
		mise:        mise,
		environment: maps.Clone(environment),
		missing:     missing,
		launchMs:    time.Now().UnixMilli(),
	}, nil
}

func projectToolsAppendLog(ctx context.Context, workerRoot string, text string) error {
	log, err := os.OpenFile(filepath.Join(workerRoot, projectToolsLogFile), os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o666)
	if err != nil {
		return fmt.Errorf("open project tools log: %w", err)
	}
	defer log.Close()
	_, err = log.WriteString(text)
	return err
}

const projectToolsMessageSource = "project-tools"

// tellAgentStarted tells the agent, before it starts, what is being
// installed.
func (i *projectToolsInstall) tellAgentStarted(relay *RelayCell) error {
	descriptions := make([]string, len(i.missing))
	for index := range i.missing {
		descriptions[index] = i.missing[index].describe()
	}
	text := fmt.Sprintf("Mjolnir: this session's project tools are being installed in the background: %s. "+
		"Until Mjolnir sends a second message saying the install finished, commands that need those tools may wait, fail, "+
		"or run a different version the image already has, so hold off on builds and tests that depend on them.",
		strings.Join(descriptions, "; "))
	owner := relay.Lock()
	defer owner.Unlock()
	return owner.DeliverAgentMessage(projectToolsMessageSource, fmt.Sprintf("project-tools-%d-started", i.launchMs), text)
}

// run runs the installation with no deadline, then tells the agent the
// result. Cancelling ctx terminates the running command's process group.
func (i *projectToolsInstall) run(ctx context.Context, workerRoot string, relay *RelayCell) {
	key := fmt.Sprintf("project-tools-%d-finished", i.launchMs)
	logPath := filepath.Join(workerRoot, projectToolsLogFile)
	notices, failures, err := i.install(ctx, workerRoot)
	// Cancellation is Rust's abort of the task: nothing more is said.
	if ctx.Err() != nil {
		return
	}
	if err != nil {
		failure := fmt.Sprintf("project tools installation could not run: %v", err)
		notices = []string{fmt.Sprintf("Project tools: %s. See %s. The session will continue.", failure, logPath)}
		failures = []string{failure}
	}
	text := "Mjolnir: the project tools finished installing. Everything the recipe declares is now available."
	if len(failures) > 0 {
		text = fmt.Sprintf("Mjolnir: the project tools did not finish installing: %s. Output is in %s. "+
			"The session is otherwise unaffected: repair the recipe and rerun `mise install` (and `mise bootstrap packages apply --yes` for package failures) in the checkout.",
			strings.Join(failures, "; "), logPath)
	}
	owner := relay.Lock()
	defer owner.Unlock()
	for _, message := range notices {
		if _, err := owner.RecordObservation(core.RelayObservationNotice{Message: message}); err != nil {
			slog.Warn("record project tools failure notice", "error", err)
		}
	}
	if err := owner.DeliverAgentMessage(projectToolsMessageSource, key, text); err != nil {
		// The agent cannot be told, so the user is.
		slog.Warn("queue project tools result for the agent", "error", err)
		if _, err := owner.RecordObservation(core.RelayObservationNotice{Message: text}); err != nil {
			slog.Warn("record project tools result notice", "error", err)
		}
	}
}

// install returns the notices for the user and the failures for the agent.
func (i *projectToolsInstall) install(ctx context.Context, workerRoot string) ([]string, []string, error) {
	logPath := filepath.Join(workerRoot, projectToolsLogFile)
	log, err := os.OpenFile(logPath, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o666)
	if err != nil {
		return nil, nil, fmt.Errorf("open project tools log: %w", err)
	}
	defer log.Close()
	var notices, failures []string
	for _, missing := range i.missing {
		for _, args := range [][]string{{"bootstrap", "packages", "apply", "--yes"}, {"install"}} {
			label := fmt.Sprintf("mise %s (in %s)", strings.Join(args, " "), missing.root)
			if _, err := log.WriteString(fmt.Sprintf("\n$ %s\n", label)); err != nil {
				return nil, nil, err
			}
			command := exec.Command(i.mise, args...)
			command.Dir = missing.root
			command.Env = workerRuntimeEnvironmentList(i.environment)
			status, err := core.RunToFileAsync(ctx, command, log)
			if ctx.Err() != nil {
				return nil, nil, ctx.Err()
			}
			var failure *string
			switch {
			case err != nil:
				text := fmt.Sprintf("%s failed: %v", label, err)
				failure = &text
			case !status.Success():
				text := fmt.Sprintf("%s failed: %s", label, workFactsExitStatus(status))
				failure = &text
			}
			if failure == nil {
				continue
			}
			if _, err := log.WriteString(*failure + "\n"); err != nil {
				return nil, nil, err
			}
			notices = append(notices, fmt.Sprintf("Project tools: %s. See %s. The session will continue; repair the recipe and rerun the command.", *failure, logPath))
			failures = append(failures, *failure)
			// A broken package recipe must not block independent tool
			// installs.
		}
	}
	return notices, failures, nil
}

// backgroundInstall is the background installation, owned by the running
// harness: stopping or releasing it cancels the task, which terminates the
// running command's process group.
type backgroundInstall struct {
	cancel context.CancelFunc
	task   *core.JoinHandle[struct{}]
}

func backgroundInstallSpawn(install projectToolsInstall, workerRoot string, relay *RelayCell) *backgroundInstall {
	ctx, cancel := context.WithCancel(context.Background())
	task := core.Spawn(func() struct{} {
		install.run(ctx, workerRoot, relay)
		return struct{}{}
	})
	return &backgroundInstall{cancel: cancel, task: task}
}

// stop stops a running installation and waits until its task, and with it
// the command's process group, is gone.
func (b *backgroundInstall) stop(ctx context.Context) {
	b.cancel()
	if _, err := b.task.Join(ctx); err != nil && ctx.Err() == nil {
		slog.Warn("project tools installation panicked", "error", err)
	}
}

// Release is Rust's Drop: it cancels the installation without waiting.
func (b *backgroundInstall) Release() {
	b.cancel()
}
