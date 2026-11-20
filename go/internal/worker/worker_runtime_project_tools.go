// Port of mj-worker/src/worker_runtime/project_tools.rs.

// Container project recipes run after clone; failures remain repairable
// in-session.

//go:build unix

package worker

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"slices"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/mjolnir/go/internal/core"
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
	panic("unported: mj-worker/src/worker_runtime/project_tools.rs apply_environment")
}

func projectToolsRecipeRoots(ctx context.Context, roots []string) ([]string, error) {
	panic("unported: mj-worker/src/worker_runtime/project_tools.rs recipe_roots")
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
	panic("unported: mj-worker/src/worker_runtime/project_tools.rs status_output")
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
	panic("unported: mj-worker/src/worker_runtime/project_tools.rs missing_in")
}

// pendingPostinstallHook: a global `[hooks] postinstall` runs on every
// `mise install`, and no status command says whether it is satisfied. An
// image built from exactly this recipe ran it at build time; anywhere else
// (a stale image, or a container whose earlier install stopped during the
// hook) it still has to run. Returns the recipe file that declares the hook
// in that case.
func pendingPostinstallHook(ctx context.Context, root string, baked string) (*string, error) {
	panic("unported: mj-worker/src/worker_runtime/project_tools.rs pending_postinstall_hook")
}

// unsatisfiedPackages are the `[bootstrap.packages]` entries that `mise
// bootstrap packages apply` would change, from `mise bootstrap packages
// status --json`: package managers whose packages carry a `state` of
// "installed", "missing" or "version_mismatch". mise 2026.11.9 reports a `*`
// request as a version mismatch whatever version is installed, so that one
// mismatch counts as satisfied.
func unsatisfiedPackages(status any) []string {
	panic("unported: mj-worker/src/worker_runtime/project_tools.rs unsatisfied_packages")
}

// projectToolsPlan installs mise if the image has none, applies the session
// environment and asks mise what each recipe checkout is missing. nil when
// nothing is missing.
func projectToolsPlan(ctx context.Context, workerRoot string, roots []string, environment map[string]string) (*projectToolsInstall, error) {
	panic("unported: mj-worker/src/worker_runtime/project_tools.rs plan")
}

func projectToolsAppendLog(ctx context.Context, workerRoot string, text string) error {
	panic("unported: mj-worker/src/worker_runtime/project_tools.rs append_log")
}

const projectToolsMessageSource = "project-tools"

// tellAgentStarted tells the agent, before it starts, what is being
// installed.
func (i *projectToolsInstall) tellAgentStarted(relay *RelayCell) error {
	panic("unported: mj-worker/src/worker_runtime/project_tools.rs Install::tell_agent_started")
}

// run runs the installation with no deadline, then tells the agent the
// result. Cancelling ctx terminates the running command's process group.
func (i *projectToolsInstall) run(ctx context.Context, workerRoot string, relay *RelayCell) {
	panic("unported: mj-worker/src/worker_runtime/project_tools.rs Install::run")
}

// install returns the notices for the user and the failures for the agent.
func (i *projectToolsInstall) install(ctx context.Context, workerRoot string) ([]string, []string, error) {
	panic("unported: mj-worker/src/worker_runtime/project_tools.rs Install::install")
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
