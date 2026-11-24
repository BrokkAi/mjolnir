// Port of mj-worker-staging/src/worker_binary/harness.rs.

package workerstaging

import (
	"bytes"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"sync"

	"github.com/BrokkAi/mjolnir/go/internal/accounts"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// WorkspacePaths is the repository paths a worker opens: the primary
// repository's, then the others'. sessionID and containerWorkspace identify
// the session whose workspace is used, which for a sub-agent child is its
// parent; containerWorkspace is nil when the session recorded none.
func WorkspacePaths(locator *core.TargetsTargetLocator, bundle *core.ProjectBundle, sessionID string, containerWorkspace *string) (string, []string, error) {
	var root string
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalBare:
		return "", nil, errors.New("local bare projects use their selected directory")
	case core.TargetsTargetLocatorAwsEc2, core.TargetsTargetLocatorSshBare:
		root = locator.Workspace
	default:
		root = core.ContainerWorkspaceRoot(containerWorkspace)
	}
	if locator.Kind == core.TargetsTargetLocatorAwsEc2 && root != ".local/share/hel/workspaces/"+sessionID {
		return "", nil, errors.New("AWS workspace does not match session")
	}
	primary := bundle.Primary()
	if primary == nil {
		return "", nil, errors.New("bundle primary is missing")
	}
	primaryPath := root + "/" + primary.Destination
	additional := []string{}
	for _, repository := range bundle.Repositories {
		if repository.ID != bundle.PrimaryRepo {
			additional = append(additional, root+"/"+repository.Destination)
		}
	}
	return primaryPath, additional, nil
}

// Package versions for ACP bridges and their harnesses. Keep these in
// lockstep with the global npm installs in containers/Containerfile.agent-dev;
// bridge_pins_match_containerfile() below fails the build when they drift.
// Codex 0.148 reuses pending MCP startups during runtime reconciliation.
// Older releases could cancel the first project-memory startup while
// immediately replacing it with an equivalent connection, leaving a false
// failed-tool event at the beginning of every session.

// BridgeReadinessStage is the stage shown after the worker is reachable and
// while its ACP bridge becomes ready. Every remaining harness launches
// through a default launcher that can fetch it, so the stage names the
// harness being installed.
func BridgeReadinessStage(profile *core.HarnessProfile) core.ProvisionStage {
	return core.ProvisionStage{Kind: core.ProvisionStageInstalling, Harness: profile.Kind}
}

// BridgeLaunch is the target-provided launcher used before the worker
// decides whether to replace it with an exact managed installation.
func BridgeLaunch(harness core.HarnessKind, policy core.ExecutionPolicy) (string, []string) {
	return core.FallbackLaunch(harness, policy)
}

func PreflightHarness(template core.TargetTemplate, profile *core.HarnessProfile, executor core.CommandExecutor) error {
	if sshBare, ok := template.(core.TargetTemplateSshBare); ok {
		ssh := core.SshTargetFromSshConnection(&sshBare.SSH)
		if _, err := targets.ExecuteChecked(executor, targets.SSHLoginScript(&ssh,
			"git --version >/dev/null || { echo 'Git is missing or unusable; install Git and, on macOS, the Xcode Command Line Tools on the target' >&2; exit 1; }",
		).WithPurpose("preflight remote Git")); err != nil {
			return err
		}
	}
	if !core.HarnessUsesNodeRuntime(profile.Kind) {
		return nil
	}
	// Only a managed installation downloads Node.js. A daemon container's
	// local sessions run the image's harnesses and never reach this check.
	managed := false
	switch template.(type) {
	case core.TargetTemplateLocalBare:
		managed = core.LocalHarnessRuntime() == core.HarnessRuntimePolicyManaged
	case core.TargetTemplateSshBare:
		managed = true
	}
	if !managed {
		return nil
	}
	// Mjolnir installs its own pinned Node.js when the target has no
	// compatible runtime. The worker performs that download, so the
	// controller only needs to establish that one of the two paths exists
	// before provisioning creates target state.
	script := nodeOrDownloaderPreflightScript()
	if profile.Kind == core.HarnessKindDsh {
		script = fmt.Sprintf("if command -v dsh-acp >/dev/null 2>&1 && %s; then exit 0; fi; %s",
			core.DshACPVersionSupportedScript(), nodeOrDownloaderPreflightScript())
	}
	path, hasPath := profile.Environment.Get("PATH")
	args := []string{"-lc", script}
	if hasPath {
		args = []string{"-c", "export PATH=\"$1\"; " + script, "mj-node-preflight", path}
	}
	var command core.CommandSpec
	var destination string
	switch template := template.(type) {
	case core.TargetTemplateLocalBare:
		command, destination = core.NewCommandSpec("sh", args), "local host"
	case core.TargetTemplateSshBare:
		ssh := core.SshTargetFromSshConnection(&template.SSH)
		if hasPath {
			command = core.SSHCommand(core.SshTrafficClassControl, &ssh, append([]string{"sh"}, args...))
		} else {
			command = targets.SSHLoginScript(&ssh, script)
		}
		destination = ssh.Destination
	}
	command = command.WithPurpose("preflight managed harness Node.js")
	output, err := executor.Execute(&command)
	if err != nil {
		return err
	}
	if output.Status == 0 {
		return nil
	}
	failure := targets.CommandErrorDetail(output.Stderr)
	if failure == "" {
		failure = fmt.Sprintf("%s failed with status %d", command.Purpose, output.Status)
	}
	return fmt.Errorf("%s launch preflight failed on %s; provide Node.js 22+ with npm and npx on the target PATH, or use a target where Mjolnir's managed runtime can run: %w",
		profile.Kind.DisplayName(), destination, errors.New(failure))
}

func nodeOrDownloaderPreflightScript() string {
	return "if command -v node >/dev/null 2>&1 && node -e 'process.exit(Number(process.versions.node.split(\".\")[0]) >= 22 ? 0 : 1)' && command -v npm >/dev/null 2>&1 && npm --version >/dev/null 2>&1 && command -v npx >/dev/null 2>&1 && npx --version >/dev/null 2>&1; then exit 0; fi; managed_target=''; loader=''; system=$(uname -s 2>/dev/null) || system=''; machine=$(uname -m 2>/dev/null) || machine=''; case \"$system:$machine\" in Linux:x86_64) managed_target=linux-x64; loader=/lib64/ld-linux-x86-64.so.2 ;; Linux:aarch64|Linux:arm64) managed_target=linux-arm64; loader=/lib/ld-linux-aarch64.so.1 ;; Darwin:x86_64) managed_target=darwin-x64 ;; Darwin:arm64) managed_target=darwin-arm64 ;; *) echo \"Mjolnir's managed Node.js has no artifact for $system $machine; install Node.js 22 or newer with npm and npx on the target PATH\" >&2; exit 127 ;; esac; managed=\"${XDG_CACHE_HOME:-$HOME/.cache}/mjolnir/node/node-v" +
		core.NodeVersion +
		"-$managed_target\"; if [ -x \"$managed/bin/node\" ] && \"$managed/bin/node\" -e 'process.exit(Number(process.versions.node.split(\".\")[0]) >= 22 ? 0 : 1)' && [ -x \"$managed/bin/npm\" ] && \"$managed/bin/npm\" --version >/dev/null 2>&1 && [ -x \"$managed/bin/npx\" ] && \"$managed/bin/npx\" --version >/dev/null 2>&1; then exit 0; fi; if [ -n \"$loader\" ] && [ ! -f \"$loader\" ]; then echo \"Mjolnir's pinned Node.js for this target is a glibc build, but the required program interpreter $loader is missing; install Node.js 22 or newer with npm and npx in the target environment\" >&2; exit 127; fi; exit 0"
}

// MJContainerEnvironment is Rust's MJ_CONTAINER_ENVIRONMENT.
const MJContainerEnvironment = "## Mjolnir disposable environment\n\nThis session runs in a disposable Mjolnir container. When the session closes, Mjolnir checkpoints everything in project workspace directories under `/workspace`, including committed work, staged and unstaged changes, and untracked files. Mjolnir then removes the container.\n\nOutside the project workspace directories and any external attachments described below, files (including installed packages, `$HOME`, and `/tmp`) are ephemeral and will be lost. Keep durable results in a project workspace directory, a read-write external attachment, or push them to a remote. External attachments follow their listed access modes and are not included in project workspace checkpoints.\n\nNew workspaces start on their own session branch from the default network fetch remote’s default branch. Local unpublished commits and uncommitted files are not copied. Use normal git push to publish the current branch to the configured network push destination. Closing saves a checkpoint; it does not publish commits or update the original local checkout. Resumed sessions restore their saved work.\n"

// profileConfigurationEntry is one staged profile entry and whether it is
// shareable content.
type profileConfigurationEntry struct {
	path   string
	shared bool
}

func profileConfigurationEntries(harness core.HarnessKind) []profileConfigurationEntry {
	// Only the files peculiar to a harness are listed per harness. The
	// instruction file and the synced skill directories are the same facts
	// the rest of Hel reads off HarnessKind, so they are appended from there
	// rather than repeated in every arm.
	var entries []profileConfigurationEntry
	switch harness {
	case core.HarnessKindMuse:
		entries = []profileConfigurationEntry{
			{"auth.json", false},
			{"settings.json", false},
			{"trust.json", false},
			{"rules", true},
		}
	case core.HarnessKindCodex:
		entries = []profileConfigurationEntry{
			{"auth.json", false},
			{"config.toml", false},
			{"instructions.md", false},
			{"rules", true},
		}
	case core.HarnessKindClaude:
		entries = []profileConfigurationEntry{
			{".claude.json", false},
			{".credentials.json", false},
			{"settings.json", false},
			{"plugins", true},
		}
	case core.HarnessKindKimi:
		entries = []profileConfigurationEntry{
			{"credentials", false},
			{"config.toml", false},
			{"device_id", false},
			{"SYSTEM.md", false},
			{"mcp.json", false},
			{"agents", true},
			{"plugins", true},
		}
	case core.HarnessKindGrok:
		entries = []profileConfigurationEntry{
			{"auth.json", false},
			{"config.toml", false},
			{"agent_id", false},
			{"plugins", true},
		}
	case core.HarnessKindOpenCode:
		// OpenCode's logins live in its database, not its configuration
		// directory; they are exported below.
		entries = []profileConfigurationEntry{
			{"opencode.json", false},
			{"opencode.jsonc", false},
		}
	case core.HarnessKindAntigravity:
		entries = []profileConfigurationEntry{
			{"antigravity-cli/antigravity-oauth-token", false},
		}
	case core.HarnessKindDsh:
		// dsh-acp keeps no configuration or login of its own; only the
		// instruction file and skills tree below are staged.
	}
	entries = append(entries, profileConfigurationEntry{harness.AgentInstructionsFile(), false})
	for _, directory := range harness.SyncedSkillDirs() {
		entries = append(entries, profileConfigurationEntry{directory, true})
	}
	return entries
}

func ProfileConfigurationPaths(harness core.HarnessKind) []string {
	entries := profileConfigurationEntries(harness)
	paths := make([]string, 0, len(entries))
	for _, entry := range entries {
		paths = append(paths, entry.path)
	}
	return paths
}

const museModelCatalog = ".data/muse/model-catalog"

// SharedProfilePaths is the profile kinds that may outlive a session in the
// receiver's store. Harness configuration, instructions, credentials and
// new kinds are private unless explicitly declared here or as a staged
// project-memory root.
func SharedProfilePaths(harness core.HarnessKind) []string {
	var paths []string
	for _, entry := range profileConfigurationEntries(harness) {
		if entry.shared {
			paths = append(paths, entry.path)
		}
	}
	switch harness {
	case core.HarnessKindCodex:
		paths = append(paths, StagedCatalogFile)
	case core.HarnessKindMuse:
		paths = append(paths, museModelCatalog)
	}
	return paths
}

// StageProfile stages the profile's own files only. Outside this file,
// stage through StageProfileWithCatalog.
func StageProfile(profile *core.HarnessProfile, destination string) error {
	harness := profile.Kind
	source := profile.Home
	if err := os.MkdirAll(destination, 0o777); err != nil {
		return rustIOError(err)
	}
	allowlist := ProfileConfigurationPaths(harness)
	// Allowlist entries (and, within each, a copied directory's children)
	// are independent of one another, so copying them concurrently shortens
	// the stage step for profiles with large skills/plugins trees. What the
	// harness maintains itself inside an allowlisted entry, such as the
	// skills Claude Code syncs from the user's claude.ai account, stays
	// behind: the harness provisions its own copy in the session home.
	var harnessOwned []string
	for _, owned := range harness.HarnessOwnedSkillPaths() {
		harnessOwned = append(harnessOwned, pathJoin(source, owned))
	}
	failures := make([]error, len(allowlist))
	var group sync.WaitGroup
	for index, name := range allowlist {
		group.Go(func() {
			from := pathJoin(source, name)
			if _, err := os.Stat(from); err == nil {
				failures[index] = CopyProfileEntryExcept(from, pathJoin(destination, name), harnessOwned)
			}
		})
	}
	group.Wait()
	for _, err := range failures {
		if err != nil {
			return err
		}
	}
	if err := StripStagedRefreshToken(harness, destination); err != nil {
		return err
	}
	// `codex app-server` refuses `--profile`, so a Codex profile overlay
	// reaches the session already layered into the staged `config.toml`.
	if profile.CodexProfile != nil {
		config, err := profile.CodexConfig()
		if err != nil {
			return err
		}
		if config != nil {
			if err := os.WriteFile(pathJoin(destination, "config.toml"), []byte(*config), 0o666); err != nil {
				return fmt.Errorf("stage Codex configuration %s: %w", profile.DescribeCodexConfig(), rustIOError(err))
			}
		}
	}
	if harness == core.HarnessKindOpenCode {
		logins, err := accounts.ProfileLogins(profile)
		if err != nil {
			return err
		}
		if logins != nil {
			if err := core.AtomicWrite(pathJoin(destination, harness.CredentialFileName()), logins); err != nil {
				return fmt.Errorf("stage OpenCode logins: %w", err)
			}
		}
	}
	// `muse serve` never fetches its model list: it reads the list the
	// person's own `muse` last saved in its data directory, and without one
	// offers only its built-in model. A staged home starts with empty data,
	// so it gets that saved list.
	if harness == core.HarnessKindMuse {
		data, ok := harness.NestedDataDir(source)
		if !ok {
			panic("Muse has an XDG home")
		}
		catalog := pathJoin(data, "model-catalog")
		if _, err := os.Stat(catalog); err == nil {
			if err := CopyProfileEntry(catalog, pathJoin(destination, museModelCatalog)); err != nil {
				return err
			}
		}
	}
	return nil
}

// StripStagedRefreshToken: every staged copy of a login is made here
// (sessions, reviewers, sub-agents and capability probes), so this is where
// a harness copy loses the refresh token that only the daemon may spend
// (#1490).
func StripStagedRefreshToken(harness core.HarnessKind, destination string) error {
	if !harness.CopiesOmitRefreshToken() {
		return nil
	}
	path := pathJoin(destination, harness.CredentialFileName())
	login, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil
	}
	if err != nil {
		return fmt.Errorf("read staged login %s: %w", path, rustIOError(err))
	}
	copied := core.CredentialCopyForHarness(harness, login)
	if !bytes.Equal(copied, login) {
		if err := core.WriteCredentialFile(harness, path, copied); err != nil {
			return fmt.Errorf("remove the refresh token from staged login %s: %w", path, err)
		}
	}
	return nil
}

// StageProfileWithCatalog stages profile into destination for one harness
// run, together with the model catalog of its Codex provider. Every staged
// copy outside this file goes through here, so a session, a reviewer and a
// capability probe all offer the same models: a reviewer staged without the
// catalog offered OpenAI's built-in model names on a DeepSeek profile
// (#1320).
func StageProfileWithCatalog(profileID string, profile *core.HarnessProfile, destination string, fetch CatalogFetch, cache CatalogCache) error {
	if err := StageProfile(profile, destination); err != nil {
		return err
	}
	return StageCodexCatalog(profileID, profile, destination, fetch, cache)
}
