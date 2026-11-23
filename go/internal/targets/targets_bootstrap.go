// Port of mj-targets/src/targets/bootstrap.rs.

package targets

import (
	"fmt"

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
	if err := core.ValidateExecutable(harness.Executable); err != nil {
		return core.CommandPlan{}, err
	}
	probe := append([]string{harness.Executable}, harness.VersionArgs...)
	commands := []core.CommandSpec{
		atBoundary(core.SshTrafficClassControl, boundary, probe).WithPurpose("probe harness version"),
	}
	if harness.BridgeExecutable != nil {
		bridge := *harness.BridgeExecutable
		if err := core.ValidateExecutable(bridge); err != nil {
			return core.CommandPlan{}, err
		}
		commands = append(commands,
			atBoundary(core.SshTrafficClassControl, boundary, []string{bridge, "--version"}).
				WithPurpose("probe ACP bridge version"))
	}
	commands = append(commands,
		atBoundary(core.SshTrafficClassControl, boundary, []string{"git", "--version"}).WithPurpose("probe Git"))
	return core.CommandPlan{Description: "probe reusable target tools", Commands: commands}, nil
}

// InitializeContainerHome completes container creation before clone/profile
// staging can write HOME. Query as the configured container user, then
// create/chown as container root. This also handles images whose non-root
// user's home is missing.
func InitializeContainerHome(locator *core.TargetsTargetLocator, executor core.CommandExecutor) error {
	var ssh *core.SshTarget
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalPodman, core.TargetsTargetLocatorLocalDocker, core.TargetsTargetLocatorAppleContainer:
	case core.TargetsTargetLocatorSshPodman, core.TargetsTargetLocatorSshDocker,
		core.TargetsTargetLocatorSshAppleContainer, core.TargetsTargetLocatorSshNitro:
		ssh = locator.SSH
	default:
		return nil
	}
	engine, ok := locator.ContainerEngine()
	if !ok {
		panic("container target")
	}
	script := fmt.Sprintf(`set -eu
mj_home_engine() { %s "$@"; }
container=$1
owner=$(mj_home_engine exec "$container" sh -c 'printf "%%s:%%s" "$(id -u)" "$(id -g)"')
# The slash sentinel preserves even a HOME ending with newlines through $().
home=$(mj_home_engine exec "$container" sh -c 'printf "%%s/" "${HOME:?container HOME is unset}"')
home=${home%%/}
mj_home_engine exec --user 0 "$container" sh -c 'set -eu; mkdir -p -- "$1"; chown -- "$2" "$1"' mj-home "$home" "$owner"
`, core.EngineShellWords(engine))
	command := core.NewCommandSpec("sh", []string{"-c", script, "mj-container-home", locator.ContainerID})
	if ssh != nil {
		command = commandOverSSH(core.SshTrafficClassControl, command, ssh)
	}
	command = command.WithPurpose("initialize session container home").
		WithStage(core.ProvisionStage{Kind: core.ProvisionStageProvisioning})
	output, err := executor.Execute(&command)
	if err != nil {
		return err
	}
	if output.Status != 0 {
		return fmt.Errorf("initialize session container home failed: %s", lossyText(output.Stderr))
	}
	return nil
}

// InstallGitPlan is the thin Linux Git bootstrap. Managed containers also
// receive GitHub CLI and its HTTPS credential helper so an injected
// `GH_TOKEN` works before clone.
func InstallGitPlan(boundary ExecutionBoundary) core.CommandPlan {
	var script string
	switch boundary.(type) {
	case ExecutionBoundaryContainer, ExecutionBoundarySshContainer:
		script = "set -eu; if ! command -v git >/dev/null 2>&1 || ! command -v gh >/dev/null 2>&1; then SUDO=''; if [ \"$(id -u)\" != 0 ]; then command -v sudo >/dev/null 2>&1 && sudo -n true || { echo 'Git and GitHub CLI installation requires root or passwordless sudo' >&2; exit 1; }; SUDO='sudo -n'; fi; if command -v apt-get >/dev/null 2>&1; then $SUDO apt-get update; $SUDO apt-get install -y git gh ca-certificates curl; elif command -v dnf >/dev/null 2>&1; then $SUDO dnf install -y git gh ca-certificates curl; elif command -v yum >/dev/null 2>&1; then $SUDO yum install -y git gh ca-certificates curl; elif command -v apk >/dev/null 2>&1; then $SUDO apk add --no-cache git github-cli ca-certificates curl; else echo 'Unsupported package manager; install Git and GitHub CLI in the image' >&2; exit 1; fi; fi; git config --global credential.https://github.com.helper '!gh auth git-credential'; git config --global credential.https://gist.github.com.helper '!gh auth git-credential'"
	default:
		script = "set -eu; if command -v git >/dev/null 2>&1; then exit 0; fi; SUDO=''; if [ \"$(id -u)\" != 0 ]; then command -v sudo >/dev/null 2>&1 && sudo -n true || { echo 'Git installation requires root or passwordless sudo' >&2; exit 1; }; SUDO='sudo -n'; fi; if command -v apt-get >/dev/null 2>&1; then $SUDO apt-get update; $SUDO apt-get install -y git ca-certificates curl; elif command -v dnf >/dev/null 2>&1; then $SUDO dnf install -y git ca-certificates curl; elif command -v yum >/dev/null 2>&1; then $SUDO yum install -y git ca-certificates curl; elif command -v apk >/dev/null 2>&1; then $SUDO apk add --no-cache git ca-certificates curl; else echo 'Unsupported package manager; install Git manually' >&2; exit 1; fi"
	}
	return core.CommandPlan{
		Description: "install missing Git",
		Commands: []core.CommandSpec{
			atBoundary(core.SshTrafficClassControl, boundary, []string{"sh", "-c", script}).
				WithPurpose("install Git").
				WithStage(core.ProvisionStage{Kind: core.ProvisionStageCloning}),
		},
	}
}

// bundleRepositoriesParallelGroup is the shared CommandSpec.ParallelGroup
// marker for one bundle's per-repository clone/init commands. Every
// CloneCommands call builds its own CommandPlan, so a single fixed marker
// never mixes batches across plans.
const bundleRepositoriesParallelGroup uint32 = 1

func CloneCommands(bundle *core.ProjectBundleSpec, workspace string, githubToken *string, wrap func(args []string) core.CommandSpec) []core.CommandSpec {
	cloning := core.ProvisionStage{Kind: core.ProvisionStageCloning}
	commands := []core.CommandSpec{
		wrap([]string{"mkdir", "-p", workspace}).WithPurpose("create bundle workspace").WithStage(cloning),
	}
	var restoreOrigins []core.CommandSpec
	for i := range bundle.Repositories {
		repository := &bundle.Repositories[i]
		destination := workspace + "/" + repository.Destination
		if repository.URL == nil {
			panic("validated network repository")
		}
		url := *repository.URL
		var checkoutSnapshot *string
		if repository.Reference != nil && core.IsHostOnlyGitSource(url) {
			checkoutSnapshot = repository.Reference
		}
		wrapFetch := func(args []string) core.CommandSpec {
			if githubToken != nil && core.IsGithubHTTPSURL(url) && fetchesRepository(args) {
				wrapped, input := cloneWithGithubToken(args, *githubToken)
				return wrap(wrapped).WithSensitiveStdin(input)
			}
			return wrap(args)
		}
		if repository.HistoryCutoff != nil {
			for _, step := range cutoffCheckoutCommands(destination, url, repository, repository.HistoryCutoff) {
				commands = append(commands, wrapFetch(step.args).WithPurpose(step.purpose).WithStage(cloning))
			}
			continue
		}
		args := []string{"git"}
		if checkoutSnapshot != nil {
			// A mounted cache can have a different owner from the container
			// user. Git needs to trust this particular read-only repository.
			args = append(args, "-c", "safe.directory="+*checkoutSnapshot)
		}
		args = append(args, "clone")
		if checkoutSnapshot != nil {
			// Clone normally transfers branches and tags. Include the private
			// exact-checkout pin, if the host prepared one, before
			// dissociation copies the borrowed objects into the checkout.
			namespace := core.HostSnapshotExactRefs
			refspec := fmt.Sprintf("+%s/*:%s/*", namespace, namespace)
			args = append(args,
				"--no-local",
				"--dissociate",
				"--reference",
				*checkoutSnapshot,
				"--config",
				"remote.origin.fetch="+refspec,
			)
			restoreOrigins = append(restoreOrigins,
				wrap(core.StepScript([]core.CommandStep{
					core.NewCommandStep("remove the temporary snapshot refspec", []string{
						"git", "-C", destination, "config", "--local", "--fixed-value", "--unset-all",
						"remote.origin.fetch", refspec,
					}),
					core.NewCommandStep("configure the source origin", []string{
						"git", "-C", destination, "remote", "set-url", "origin", url,
					}),
				})).
					WithPurpose("configure origin for "+repository.Destination).
					WithStage(cloning).
					WithParallelGroup(bundleRepositoriesParallelGroup+1))
		}
		for _, pushURL := range repository.PushURLs {
			args = append(args, "--config", "remote.origin.pushurl="+pushURL)
		}
		if checkoutSnapshot == nil && repository.Reference != nil {
			args = append(args, "--reference-if-able", *repository.Reference)
		}
		args = append(args, "--")
		if checkoutSnapshot != nil {
			args = append(args, *checkoutSnapshot)
		} else {
			args = append(args, url)
		}
		args = append(args, destination)
		commands = append(commands,
			wrapFetch(args).
				WithPurpose("clone "+repository.Destination).
				WithStage(cloning).
				WithParallelGroup(bundleRepositoriesParallelGroup))
	}
	return append(commands, restoreOrigins...)
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
	git := func(arguments ...string) []string {
		return append([]string{"git", "-C", destination}, arguments...)
	}
	// A bundle is fetched from by its ref names, which every Git version
	// accepts; fetching an object ID it does not advertise is newer. The
	// network serves the object IDs themselves.
	var source, head, base string
	if repository.Reference != nil {
		source, head, base = *repository.Reference, core.HistoryCutoffBundleHeadRef, core.HistoryCutoffBaseRef
	} else {
		source, head = url, cutoff.Head
		if cutoff.Base != nil {
			base = *cutoff.Base
		}
	}
	fetch := git("fetch", "--quiet", "--no-tags", "--", source, head)
	if cutoff.Base != nil {
		fetch = append(fetch, base+":"+core.HistoryCutoffBaseRef)
	}
	return []cutoffCheckout{
		{
			args:    []string{"git", "init", "--quiet", "--", destination},
			purpose: fmt.Sprintf("initialize %s without later history", repository.Destination),
		},
		{
			args:    fetch,
			purpose: fmt.Sprintf("fetch %s history up to %s", repository.Destination, cutoff.Head),
		},
		{
			args:    git("checkout", "--quiet", "--detach", cutoff.Head),
			purpose: fmt.Sprintf("check out %s at %s", repository.Destination, cutoff.Head),
		},
	}
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
	wrapped := []string{
		"sh",
		"-c",
		"IFS= read -r GH_TOKEN || exit 1; export GH_TOKEN GIT_TERMINAL_PROMPT=0; exec \"$@\"",
		"mj-github-token",
		"git",
	}
	wrapped = append(wrapped, core.GithubTokenCredentialConfig()...)
	if len(args) == 0 || args[0] != "git" {
		panic("a checkout fetch runs git")
	}
	wrapped = append(wrapped, args[1:]...)
	input := append([]byte(token), '\n')
	return wrapped, input
}

// InstallRsyncPlan installs Move transport on newly provisioned Linux
// instances.
func InstallRsyncPlan(boundary ExecutionBoundary) core.CommandPlan {
	script := "set -eu; if rsync --recursive --times --perms --partial --partial-dir=.move-partial --version >/dev/null 2>&1; then exit 0; fi; SUDO=''; if [ \"$(id -u)\" != 0 ]; then command -v sudo >/dev/null 2>&1 && sudo -n true || { echo 'rsync installation requires root or passwordless sudo' >&2; exit 1; }; SUDO='sudo -n'; fi; if command -v apt-get >/dev/null 2>&1; then $SUDO apt-get update; $SUDO apt-get install -y rsync; elif command -v dnf >/dev/null 2>&1; then $SUDO dnf install -y rsync; elif command -v yum >/dev/null 2>&1; then $SUDO yum install -y rsync; elif command -v apk >/dev/null 2>&1; then $SUDO apk add --no-cache rsync; else echo 'Unsupported package manager; install rsync with recursive, permissions, timestamps and partial-dir support in the image' >&2; exit 1; fi; rsync --recursive --times --perms --partial --partial-dir=.move-partial --version >/dev/null"
	return core.CommandPlan{
		Description: "install EC2 Move transport",
		Commands: []core.CommandSpec{
			atBoundary(core.SshTrafficClassControl, boundary, []string{"sh", "-c", script}).
				WithPurpose("install supported rsync").
				WithStage(core.ProvisionStage{Kind: core.ProvisionStageProvisioning}),
		},
	}
}
