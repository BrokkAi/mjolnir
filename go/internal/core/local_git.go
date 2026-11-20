// Port of mj-core/src/local_git.rs.

// Controller-side support for repositories configured with `local` sources.

package core

import (
	"fmt"
	"os"
	"strings"
	"unicode"
)

// SessionGitStatus is what a session's checkout looks like right now: the
// branch, how far it is from its upstream, and the files that differ from
// HEAD. Produced by the controller from the target's own `git`, consumed by
// the session rows and the changed-files overlay.
type SessionGitStatus struct {
	Checkout string `json:"checkout"`
	// The branch name, `detached HEAD`, or a short reason there is none.
	Branch string `json:"branch"`
	// Commits ahead of and behind the upstream, when there is one.
	Ahead   *uint32       `json:"ahead"`
	Behind  *uint32       `json:"behind"`
	Changed []ChangedFile `json:"changed"`
}

// RowText is the compact form a session row carries beside its target:
// `⎇ main ↑1 ↓2 ±4`. Empty when the checkout is not a repository.
func (s *SessionGitStatus) RowText() string { return s.RowTextWith(false) }

// RowTextWith is RowText with plain ASCII markers when `ascii` is set:
// `br main +1 -2 ~4`.
func (s *SessionGitStatus) RowTextWith(ascii bool) string {
	panic("unported: mj-core/src/local_git.rs SessionGitStatus::row_text_with")
}

// Totals is the lines added and removed across every changed file, from what
// git could count (binary files count nothing).
func (s *SessionGitStatus) Totals() (uint32, uint32) {
	var added, removed uint32
	for _, file := range s.Changed {
		if file.Added != nil {
			added += *file.Added
		}
		if file.Removed != nil {
			removed += *file.Removed
		}
	}
	return added, removed
}

// ChangedFile is one file that differs from HEAD, as `git status --porcelain`
// names it.
type ChangedFile struct {
	Path string `json:"path"`
	// The two-letter porcelain code: `M ` modified in the index, ` M` in the
	// worktree, `A ` added, `D ` deleted, `R ` renamed, `??` untracked.
	Status string `json:"status"`
	// Lines added and removed against HEAD; nil for a binary file or one git
	// did not count (an untracked file).
	Added   *uint32 `json:"added"`
	Removed *uint32 `json:"removed"`
}

// Kind is one word for the change, for a list a person reads.
func (c *ChangedFile) Kind() string {
	code := c.Status
	switch {
	case code == "??":
		return "new"
	case strings.HasPrefix(code, "A"):
		return "added"
	case strings.HasPrefix(code, "D") || strings.HasSuffix(code, "D"):
		return "deleted"
	case strings.HasPrefix(code, "R"):
		return "renamed"
	case strings.HasPrefix(code, "C"):
		return "copied"
	case strings.Contains(code, "U"):
		return "conflict"
	}
	return "modified"
}

// NumstatArgs and StatusArgs are the git arguments whose output
// ParseGitStatus reads. Both are `-z` so git prints names as they are, not
// C-quoted with octal escapes.
var NumstatArgs = [5]string{"--no-optional-locks", "diff", "--numstat", "-z", "HEAD"}
var StatusArgs = [5]string{
	"--no-optional-locks",
	"status",
	"--porcelain",
	"-z",
	"--untracked-files=normal",
}

// ParseGitStatus builds a SessionGitStatus from the raw output of four git
// commands: `rev-parse --abbrev-ref HEAD`, `rev-list --left-right --count
// @{upstream}...HEAD` (or nil when there is no upstream), `diff --numstat -z
// HEAD`, and `status --porcelain -z`. Pure, so the parsing has tests that
// need no repository.
func ParseGitStatus(checkout string, branch string, aheadBehind *string, numstat string, porcelain string) SessionGitStatus {
	panic("unported: mj-core/src/local_git.rs parse_git_status")
}

type DirtyLocalRepository struct {
	ID      string
	Path    string
	Summary string
}

func DirtyLocalRepositories(bundle *ProjectBundle) ([]DirtyLocalRepository, error) {
	panic("unported: mj-core/src/local_git.rs dirty_local_repositories")
}

func CanonicalRepository(path string) (string, error) {
	panic("unported: mj-core/src/local_git.rs canonical_repository")
}

// CanonicalGitOutputPath is a path on this machine that Git printed, in the
// one spelling local repository paths are stored and compared in: what
// canonicalization returns. Git for Windows prints `C:/Users/...`, which
// names the same directory as canonicalize's `\\?\C:\Users\...` but never
// compares equal.
func CanonicalGitOutputPath(path string) (string, error) {
	panic("unported: mj-core/src/local_git.rs canonical_git_output_path")
}

// MainWorktreeRoot maps a repository top level to the top level of its main
// working tree.
//
// A linked worktree created by `git worktree add` reports its own top level,
// but Hel treats it as the same repository as the main working tree.
func MainWorktreeRoot(root string) (string, error) {
	panic("unported: mj-core/src/local_git.rs main_worktree_root")
}

// ResolveGitPath validates and normalizes a Git path or a relative Git
// configuration path against its working directory.
func ResolveGitPath(directory string, output string) (string, error) {
	panic("unported: mj-core/src/local_git.rs resolve_git_path")
}

func localStatus(path string) (*string, error) {
	panic("unported: mj-core/src/local_git.rs local_status")
}

// ResolveLocalRepository resolves the fetch and push endpoints Git would use
// for a host checkout.
//
// Git performs URL rewriting while answering `remote get-url`, so the values
// returned here are the actual network endpoints that should be persisted
// for the target checkout. The local checkout may be dirty or have commits
// that are not published; neither affects endpoint selection.
func ResolveLocalRepository(path string, executor CommandExecutor) (NetworkGitSource, error) {
	return resolveLocalRepositoryWithRemote(path, executor, nil)
}

// IdentityFetchURL: the default branch's fetch remote identifies the
// project. Feature-branch tracking and push destinations describe execution,
// not repository identity.
func IdentityFetchURL(path string, executor CommandExecutor) (*string, error) {
	panic("unported: mj-core/src/local_git.rs identity_fetch_url")
}

// LocalRemoteRepair is a concrete, read-only proposal. Applying it requires a
// separate user action.
type LocalRemoteRepair struct {
	Path              string   `json:"path"`
	Branch            string   `json:"branch"`
	MissingRemote     string   `json:"missing_remote"`
	ReplacementRemote string   `json:"replacement_remote"`
	FetchURL          string   `json:"fetch_url"`
	PushURLs          []string `json:"push_urls"`
}

func (r *LocalRemoteRepair) UnmarshalJSON(data []byte) error {
	type wire LocalRemoteRepair
	return unmarshalDenyUnknown(data, (*wire)(r))
}

func (r LocalRemoteRepair) Error() string {
	return fmt.Sprintf(
		"%s: branch %q tracks missing remote %q. Repair tracking to %q? Fetch: %s; push: %s",
		r.Path, r.Branch, r.MissingRemote, r.ReplacementRemote, r.FetchURL, strings.Join(r.PushURLs, ", "),
	)
}

// Equal is the Rust derived PartialEq.
func (r LocalRemoteRepair) Equal(other LocalRemoteRepair) bool {
	if r.Path != other.Path || r.Branch != other.Branch || r.MissingRemote != other.MissingRemote ||
		r.ReplacementRemote != other.ReplacementRemote || r.FetchURL != other.FetchURL ||
		len(r.PushURLs) != len(other.PushURLs) {
		return false
	}
	for i := range r.PushURLs {
		if r.PushURLs[i] != other.PushURLs[i] {
			return false
		}
	}
	return true
}

// RepositoryRemoteRepairs inspects every local source without changing host
// Git configuration.
func RepositoryRemoteRepairs(bundle *ProjectBundle, executor CommandExecutor) ([]LocalRemoteRepair, error) {
	panic("unported: mj-core/src/local_git.rs repository_remote_repairs")
}

// ApplyRepositoryRemoteRepairs applies only reviewed proposals belonging to
// this bundle, after rechecking their branch, stale setting, and destination
// against the current checkout.
func ApplyRepositoryRemoteRepairs(bundle *ProjectBundle, repairs []LocalRemoteRepair, executor CommandExecutor) error {
	panic("unported: mj-core/src/local_git.rs apply_repository_remote_repairs")
}

// noNetworkRemote is what the caller of a new session or resume is told when
// the project's repository has no remote the target can clone from. It names
// no path or host, so it may travel to any client.
const noNetworkRemote = "the project's Git repository has no remote; SSH and container targets clone the project from a network remote. Add one, for example `git remote add origin https://github.com/owner/repository.git`"

// localRemoteURL is the same, for a remote whose URL is a local path or
// another transport a target cannot reach.
const localRemoteURL = "the project's Git remote is not a network URL (it is a local path or an unsupported transport); SSH and container targets need an https://, ssh://, git://, or user@host:path remote"

func defaultFetchRemote(remotes []string) (string, error) {
	panic("unported: mj-core/src/local_git.rs default_fetch_remote")
}

func resolveLocalRepositoryWithRemote(path string, executor CommandExecutor, replacement *string) (NetworkGitSource, error) {
	panic("unported: mj-core/src/local_git.rs resolve_local_repository_with_remote")
}

func gitCommand(path string, args []string) CommandSpec {
	command := NewCommandSpec("git", append([]string{"-C", path}, args...))
	UnattendedGitEnvironment(&command)
	return command
}

// UnattendedGitEnvironment is the environment for a local Git command that
// may reach a remote with no one present to answer: fail instead of
// prompting for credentials or host keys, and never fetch missing objects
// behind the caller's back.
func UnattendedGitEnvironment(command *CommandSpec) {
	if command.Env == nil {
		command.Env = map[string]string{}
	}
	command.Env["GIT_TERMINAL_PROMPT"] = "0"
	command.Env["GIT_NO_LAZY_FETCH"] = "1"
	if value, ok := LocalGitSSHCommand(); ok {
		command.Env["GIT_SSH_COMMAND"] = value
	}
}

// NonInteractiveGitSSHCommand: SSH asks about an unknown host key itself,
// which `GIT_TERMINAL_PROMPT` does not reach, so Git that no one is watching
// gets a batch-mode command.
const NonInteractiveGitSSHCommand = "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=15"

// LocalGitSSHCommand is how Git that Mjolnir starts on this machine reaches
// an SSH remote: the operator's `GIT_SSH_COMMAND` when it is set, otherwise
// NonInteractiveGitSSHCommand. false means the operator's command applies
// and the child inherits it byte for byte; true returns the default to set.
// RemoteGitSSHCommand is the same decision for Git Mjolnir runs on another
// machine, and these two are the only places it is made (#1466).
func LocalGitSSHCommand() (string, bool) {
	if _, set := os.LookupEnv("GIT_SSH_COMMAND"); set {
		return "", false
	}
	return NonInteractiveGitSSHCommand, true
}

// RemoteGitSSHCommand is LocalGitSSHCommand's decision as text, for Git that
// runs on another machine, such as a clone-cache host or a session's target,
// where the command must travel in an argument. An operator command that is
// not UTF-8 cannot travel as text and is refused rather than altered; Git on
// this computer uses LocalGitSSHCommand and inherits it instead.
func RemoteGitSSHCommand() (string, error) {
	panic("unported: mj-core/src/local_git.rs remote_git_ssh_command")
}

func gitOutput(path string, args []string, executor CommandExecutor, purpose string) (CommandOutput, error) {
	panic("unported: mj-core/src/local_git.rs git_output")
}

func gitText(path string, args []string, executor CommandExecutor, purpose string) (string, error) {
	panic("unported: mj-core/src/local_git.rs git_text")
}

func gitConfig(path string, key string, executor CommandExecutor) (*string, error) {
	panic("unported: mj-core/src/local_git.rs git_config")
}

func parseLines(output []byte, kind string) ([]string, error) {
	panic("unported: mj-core/src/local_git.rs parse_lines")
}

func ensureRemoteName(name string, role string) error {
	if name == "" || name == "." || strings.IndexFunc(name, unicode.IsSpace) >= 0 {
		return fmt.Errorf("%s Git remote selection is empty or refers to the local repository", role)
	}
	return nil
}

func remoteURL(path string, remote string, push bool, executor CommandExecutor) (string, error) {
	urls, err := remoteURLs(path, remote, push, executor)
	if err != nil {
		return "", err
	}
	if len(urls) == 0 {
		return "", fmt.Errorf("Git remote %q has no configured URL", remote)
	}
	return urls[0], nil
}

func remoteURLs(path string, remote string, push bool, executor CommandExecutor) ([]string, error) {
	panic("unported: mj-core/src/local_git.rs remote_urls")
}

// MinimumGitVersion is the oldest Git Mjolnir supports: Debian 12's. It has
// `git merge-tree --write-tree` (Git 2.38), which before-push review needs to
// start a merged branch's review from the merge of its reviewed work.
var MinimumGitVersion = [3]uint32{2, 39, 5}

// ParseGitVersion reads the version in `git version` output, such as
// `git version 2.39.5 (Apple Git-154)` or `git version 2.53.0.windows.1`.
// A missing patch number reads as 0.
func ParseGitVersion(output string) ([3]uint32, bool) {
	panic("unported: mj-core/src/local_git.rs parse_git_version")
}
