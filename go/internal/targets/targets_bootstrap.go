// Port of mj-targets/src/targets/bootstrap.rs.

package targets

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SSHLoginScript loads the account's login exports, then interprets script
// as POSIX sh. User shells such as zsh have reserved variables and different
// syntax.
func SSHLoginScript(ssh *core.SshTarget, script string) core.CommandSpec {
	loginCommand := "exec /bin/sh -c " + core.PosixQuote(script)
	return core.SSHCommand(core.SshTrafficClassControl, ssh, []string{
		"sh",
		"-c",
		"exec \"$SHELL\" -lc \"$1\"",
		"mj-login",
		loginCommand,
	})
}

// ExecutionBoundary is where a command runs: here, in a container, on an SSH
// host, or in a container on an SSH host.
type ExecutionBoundary interface {
	isExecutionBoundary()
}

type ExecutionBoundaryDirect struct{}

type ExecutionBoundaryContainer struct {
	Engine      string
	ContainerID string
}

type ExecutionBoundarySsh struct {
	SSH *core.SshTarget
}

type ExecutionBoundarySshContainer struct {
	Engine      string
	SSH         *core.SshTarget
	ContainerID string
}

func (ExecutionBoundaryDirect) isExecutionBoundary()       {}
func (ExecutionBoundaryContainer) isExecutionBoundary()    {}
func (ExecutionBoundarySsh) isExecutionBoundary()          {}
func (ExecutionBoundarySshContainer) isExecutionBoundary() {}

type HarnessProbe struct {
	Executable       string
	VersionArgs      []string
	BridgeExecutable *string
}

// BootstrapProbePlan: compatibility is intentionally interpreted by the
// controller. A successful probe permits an image-baked tool to be reused; a
// missing/incompatible tool causes the controller to upload/install its
// release-owned copy.
func BootstrapProbePlan(boundary ExecutionBoundary, harness HarnessProbe) (core.CommandPlan, error) {
	panic("unported: mj-targets/src/targets/bootstrap.rs bootstrap_probe_plan")
}

// InitializeContainerHome completes container creation before clone/profile
// staging can write HOME. Query as the configured container user, then
// create/chown as container root. This also handles images whose non-root
// user's home is missing.
func InitializeContainerHome(locator *core.TargetsTargetLocator, executor core.CommandExecutor) error {
	panic("unported: mj-targets/src/targets/bootstrap.rs initialize_container_home")
}

// InstallGitPlan is the thin Linux Git bootstrap. Managed containers also
// receive GitHub CLI and its HTTPS credential helper so an injected
// `GH_TOKEN` works before clone.
func InstallGitPlan(boundary ExecutionBoundary) core.CommandPlan {
	panic("unported: mj-targets/src/targets/bootstrap.rs install_git_plan")
}

// bundleRepositoriesParallelGroup is the shared CommandSpec.ParallelGroup
// marker for one bundle's per-repository clone/init commands. Every
// CloneCommands call builds its own CommandPlan, so a single fixed marker
// never mixes batches across plans.
const bundleRepositoriesParallelGroup uint32 = 1

func CloneCommands(bundle *core.ProjectBundleSpec, workspace string, githubToken *string, wrap func(args []string) core.CommandSpec) []core.CommandSpec {
	panic("unported: mj-targets/src/targets/bootstrap.rs clone_commands")
}

// cutoffCheckout is one command of cutoffCheckoutCommands: its argv and its
// purpose.
type cutoffCheckout struct {
	args    []string
	purpose string
}

// cutoffCheckoutCommands creates a checkout that holds the cutoff commits and
// their ancestors and nothing else. A clone would copy every branch and tag,
// and a clone-cache reference would lend it the whole mirror through
// `objects/info/alternates`, so the checkout starts empty and fetches only
// these object IDs, from the cache's cutoff bundle when one was prepared and
// from the network otherwise. It has no `origin` remote. The commands run in
// order: each needs the last.
func cutoffCheckoutCommands(destination string, url string, repository *core.RepositorySpec, cutoff *core.HistoryCutoff) []cutoffCheckout {
	panic("unported: mj-targets/src/targets/bootstrap.rs cutoff_checkout_commands")
}

// fetchesRepository reports whether a command from CloneCommands reaches the
// repository's server: a clone, or the fetch of a checkout without later
// history.
func fetchesRepository(args []string) bool {
	if len(args) < 2 {
		return false
	}
	switch args[1] {
	case "clone":
		return true
	case "-C":
		return len(args) > 3 && args[3] == "fetch"
	default:
		return false
	}
}

// cloneWithGithubToken runs one checkout fetch with a GitHub token that only
// that process sees. This also provisions private repositories for workers
// without credentials. The token arrives on stdin, so it stays out of argv
// and Git configuration.
func cloneWithGithubToken(args []string, token string) ([]string, []byte) {
	panic("unported: mj-targets/src/targets/bootstrap.rs clone_with_github_token")
}

// InstallRsyncPlan installs Move transport on newly provisioned Linux
// instances.
func InstallRsyncPlan(boundary ExecutionBoundary) core.CommandPlan {
	panic("unported: mj-targets/src/targets/bootstrap.rs install_rsync_plan")
}
