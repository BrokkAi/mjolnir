// Port of mj-checkpoint/src/archive/git.rs.

package checkpoint

import (
	"archive/tar"
	"bytes"
	"cmp"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"maps"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// GitCommandRunner is a command boundary that lets Git collection and
// restore be tested without invoking the host's Git executable.
//
// Implementations must be safe for concurrent use (Rust: Sync): collection
// runs independent Git commands from several goroutines at once.
type GitCommandRunner interface {
	Run(repository string, command *GitCommand) (GitOutput, error)
}

// gitFileRunner is a GitCommandRunner that writes a command's standard
// output directly to a file rather than buffering it in memory (an override
// of Rust's GitCommandRunner::run_to_file).
type gitFileRunner interface {
	RunToFile(repository string, command *GitCommand, output *os.File) (GitOutput, error)
}

// runToFile runs command with its stdout written to output. A runner without
// its own RunToFile runs the command and writes the captured output, as the
// Rust trait's default method does. The returned Stdout is empty. The caller
// keeps owning output.
func runToFile(runner GitCommandRunner, repository string, command *GitCommand, output *os.File) (GitOutput, error) {
	if fileRunner, ok := runner.(gitFileRunner); ok {
		return fileRunner.RunToFile(repository, command, output)
	}
	result, err := runner.Run(repository, command)
	if err != nil {
		return GitOutput{}, err
	}
	if _, err := output.Write(result.Stdout); err != nil {
		return GitOutput{}, err
	}
	return GitOutput{Status: result.Status, Stdout: []byte{}, Stderr: result.Stderr}, nil
}

// GitCommand is one Git invocation. Empty Stdin and Env are nil.
type GitCommand struct {
	Arguments []string
	Stdin     []byte
	// Env is extra environment for this one command, as name/value pairs.
	// The review capture points Git at a scratch index through
	// `GIT_INDEX_FILE`, which is the only way to stage a tree without
	// touching the repository's real index.
	Env [][2]string
}

// GitOutput is a finished Git command: its exit status (-1 when it has none,
// such as after a signal) and its captured output.
type GitOutput struct {
	Status int
	Stdout []byte
	Stderr []byte
}

// NonInteractiveGitEnv is environment that keeps a Git child from waiting on
// a person. No command run through SystemGit has an operator watching its
// terminal - a checkpoint export shares the controller's - so a credential
// prompt or a promisor lazy fetch would hold the command open until its
// caller's deadline. Both have to fail fast instead.
var NonInteractiveGitEnv = [2][2]string{
	{"GIT_TERMINAL_PROMPT", "0"},
	{"GIT_NO_LAZY_FETCH", "1"},
}

// SystemGit runs the host's `git` executable.
type SystemGit struct{}

func (SystemGit) Run(repository string, command *GitCommand) (GitOutput, error) {
	process := prepareGitCommand(repository, command)
	output, err := core.RunWithInput(process, command.Stdin)
	if err != nil {
		return GitOutput{}, fmt.Errorf("run git in %s: %w", repository, err)
	}
	return GitOutput{Status: output.Status.ExitCode(), Stdout: output.Stdout, Stderr: output.Stderr}, nil
}

// RunToFile runs Git with its stdout written directly to output. The caller
// keeps owning output.
func (SystemGit) RunToFile(repository string, command *GitCommand, output *os.File) (GitOutput, error) {
	process := prepareGitCommand(repository, command)
	result, err := core.RunWithInputToFile(process, command.Stdin, output)
	if err != nil {
		return GitOutput{}, fmt.Errorf("run git in %s: %w", repository, err)
	}
	return GitOutput{Status: result.Status.ExitCode(), Stdout: result.Stdout, Stderr: result.Stderr}, nil
}

func prepareGitCommand(repository string, command *GitCommand) *exec.Cmd {
	process := exec.Command("git", command.Arguments...)
	process.Dir = repository
	// Later entries win in os/exec, as later `Command::env` calls do in Rust.
	process.Env = os.Environ()
	for _, pair := range NonInteractiveGitEnv {
		process.Env = append(process.Env, pair[0]+"="+pair[1])
	}
	for _, pair := range command.Env {
		process.Env = append(process.Env, pair[0]+"="+pair[1])
	}
	ownSSHCommand := slices.ContainsFunc(command.Env, func(pair [2]string) bool {
		return pair[0] == "GIT_SSH_COMMAND"
	})
	if ssh, ok := core.LocalGitSSHCommand(); ok && !ownSSHCommand {
		process.Env = append(process.Env, "GIT_SSH_COMMAND="+ssh)
	}
	return process
}

// syntheticGitIdentity is a stable identity for internal commits and
// reflogs; it is never written to user config.
func syntheticGitIdentity() [][2]string {
	return [][2]string{
		{"GIT_AUTHOR_NAME", "Mjolnir"},
		{"GIT_AUTHOR_EMAIL", "mjolnir@localhost"},
		{"GIT_COMMITTER_NAME", "Mjolnir"},
		{"GIT_COMMITTER_EMAIL", "mjolnir@localhost"},
	}
}

// syntheticGitCommitIdentity: commit objects use a fixed date for stable
// IDs; reflogs must use current time because Git does not list epoch-zero
// stash reflog entries.
func syntheticGitCommitIdentity() [][2]string {
	return append(syntheticGitIdentity(),
		[2]string{"GIT_AUTHOR_DATE", "@0 +0000"},
		[2]string{"GIT_COMMITTER_DATE", "@0 +0000"})
}

// GitHistoryMode is how much committed history a snapshot has to carry.
// Committed work that is already reachable from an origin ref is durable at
// its source, so it is never bundled. One of GitHistoryModeSessionDelta,
// GitHistoryModeCloneFrom, GitHistoryModeDeltaFrom, GitHistoryModeNoBundle.
type GitHistoryMode interface {
	isGitHistoryMode()
}

// GitHistoryModeSessionDelta bundles commits reachable from HEAD but from no
// `refs/remotes/origin/*` ref. Errors when the repository has no origin refs
// at all.
type GitHistoryModeSessionDelta struct{}

// GitHistoryModeCloneFrom preserves every local ref and stash entry beyond a
// recorded source commit.
type GitHistoryModeCloneFrom string

// GitHistoryModeDeltaFrom bundles commits since `merge-base(HEAD, rev)`.
// Errors when the revision or the merge base cannot be resolved.
type GitHistoryModeDeltaFrom string

// GitHistoryModeNoBundle has no committed bundle: origin serves all
// committed history. Dirty state and identity are still collected.
type GitHistoryModeNoBundle struct{}

func (GitHistoryModeSessionDelta) isGitHistoryMode() {}
func (GitHistoryModeCloneFrom) isGitHistoryMode()    {}
func (GitHistoryModeDeltaFrom) isGitHistoryMode()    {}
func (GitHistoryModeNoBundle) isGitHistoryMode()     {}

type GitCollectionSpec struct {
	ID                  string
	RelativeDestination string
	History             GitHistoryMode
	OriginOverride      *string
}

// GitSnapshotProgress reports collection progress. Its one variant is
// GitSnapshotProgressUntrackedFile.
type GitSnapshotProgress interface {
	isGitSnapshotProgress()
}

type GitSnapshotProgressUntrackedFile struct {
	Current int
	Total   int
	Path    string
}

func (GitSnapshotProgressUntrackedFile) isGitSnapshotProgress() {}

type UntrackedFiles int

const (
	UntrackedFilesInclude UntrackedFiles = iota
	UntrackedFilesExclude
)

func CollectGitSnapshot(runner GitCommandRunner, repository string, spec *GitCollectionSpec) (RepositorySnapshot, error) {
	return CollectGitSnapshotWithProgress(runner, repository, spec, UntrackedFilesInclude,
		func(GitSnapshotProgress) error { return nil })
}

// CollectGitSnapshotWithProgress captures the checkout around repository.
// progress must be safe for concurrent use (Rust: Sync): untracked files of
// the checkout and of its linked worktrees are walked concurrently.
func CollectGitSnapshotWithProgress(
	runner GitCommandRunner,
	repository string,
	spec *GitCollectionSpec,
	untrackedFiles UntrackedFiles,
	progress func(GitSnapshotProgress) error,
) (RepositorySnapshot, error) {
	if err := ValidateComponent(spec.ID, "repository id"); err != nil {
		return RepositorySnapshot{}, err
	}
	if err := validateArchiveRelativePath(spec.RelativeDestination); err != nil {
		return RepositorySnapshot{}, err
	}

	// A session can work in a subdirectory of its checkout, but its work is
	// the whole checkout's, and a managed checkout is deleted on Stop. Every
	// command runs from the top level, so work outside the subdirectory is
	// kept and every path is named from where a restore starts.
	checkout, err := locateCheckout(runner, repository)
	if err != nil {
		return RepositorySnapshot{}, err
	}
	repository = checkout.topLevel
	identity, err := collectGitIdentity(runner, repository, spec.OriginOverride)
	if err != nil {
		return RepositorySnapshot{}, err
	}
	var cloneState *cloneRefState
	if base, ok := spec.History.(GitHistoryModeCloneFrom); ok {
		state, err := collectCloneRefs(runner, repository, string(base))
		if err != nil {
			return RepositorySnapshot{}, err
		}
		cloneState = &state
	}
	// Temporary refs made for linked worktrees; removed with the clone's on
	// every exit path below.
	var worktreeRefs []string
	snapshot, captured := func() (RepositorySnapshot, error) {
		var history gitHistorySelection
		if cloneState != nil {
			history = cloneState.history.clone()
		} else {
			selected, err := selectGitHistory(runner, repository, spec.History, identity.headCommit)
			if err != nil {
				return RepositorySnapshot{}, err
			}
			history = selected
		}
		var reviewRefs map[string]string
		if cloneState == nil {
			refs, err := collectReviewRefs(runner, repository)
			if err != nil {
				return RepositorySnapshot{}, err
			}
			reviewRefs = refs
		}
		includeReviewHistory(&history, reviewRefs)
		linked, err := discoverLinkedWorktrees(runner, repository)
		if err != nil {
			return RepositorySnapshot{}, err
		}
		linkedDirectories := make(map[string]struct{}, len(linked))
		for _, worktree := range linked {
			linkedDirectories[worktree.directory] = struct{}{}
		}
		prepared, err := prepareLinkedWorktrees(runner, linked, cloneState != nil, &worktreeRefs)
		if err != nil {
			return RepositorySnapshot{}, err
		}
		if err := includeWorktreeHistory(runner, repository, spec.History, &history, prepared, worktreeRefs); err != nil {
			return RepositorySnapshot{}, err
		}
		var unmergedPaths []string
		if cloneState != nil && cloneState.operation != nil {
			unmergedPaths = cloneState.operation.unmergedPaths()
		}
		snapshot, err := collectGitContents(runner, &checkout, spec, identity, history, unmergedPaths,
			untrackedFiles, linkedDirectories, progress)
		if err != nil {
			return RepositorySnapshot{}, err
		}
		snapshot.LinkedWorktrees, err = collectLinkedWorktreeContents(runner, prepared, untrackedFiles,
			linkedDirectories, progress)
		if err != nil {
			return RepositorySnapshot{}, err
		}
		if cloneState == nil && len(reviewRefs) > 0 {
			scope, err := SessionStartScope(runner, repository)
			if err != nil {
				return RepositorySnapshot{}, err
			}
			snapshot.Metadata.ReviewBaselineRef = &scope
		}
		snapshot.Metadata.SavedRefs = reviewRefs
		return snapshot, nil
	}()
	temporaryRefs := worktreeRefs
	if cloneState != nil {
		temporaryRefs = append(temporaryRefs, cloneState.temporaryRefs...)
	}
	cleanup := removeTemporaryRefs(runner, repository, temporaryRefs)
	switch {
	case captured == nil && cleanup != nil:
		return RepositorySnapshot{}, cleanup
	case captured != nil && cleanup == nil:
		return RepositorySnapshot{}, captured
	case captured != nil && cleanup != nil:
		return RepositorySnapshot{}, fmt.Errorf("also failed to remove temporary refs: %v: %w", cleanup, captured)
	}
	if cloneState != nil {
		for name := range cloneState.savedRefs {
			if strings.HasPrefix(name, core.MjolnirRefNamespace) {
				scope, err := SessionStartScope(runner, repository)
				if err != nil {
					return RepositorySnapshot{}, err
				}
				snapshot.Metadata.ReviewBaselineRef = &scope
				break
			}
		}
		snapshot.Metadata.SavedRefs = cloneState.savedRefs
		snapshot.Metadata.StashStack = cloneState.stashStack
		if operation := cloneState.operation; operation != nil {
			metadata, payloads := operation.metadata, operation.payloads
			snapshot.Metadata.GitOperation = &metadata
			snapshot.OperationPayloads = &payloads
		}
	}
	return snapshot, nil
}

// CollectGitMetadataSnapshot collects only enough Git identity to associate
// native harness state with an existing project, together with its durable
// review ref names and object IDs. The project remains the recovery source
// for objects and working files.
func CollectGitMetadataSnapshot(runner GitCommandRunner, repository string, spec *GitCollectionSpec) (RepositorySnapshot, error) {
	if err := ValidateComponent(spec.ID, "repository id"); err != nil {
		return RepositorySnapshot{}, err
	}
	if err := validateArchiveRelativePath(spec.RelativeDestination); err != nil {
		return RepositorySnapshot{}, err
	}
	identity, err := collectGitIdentity(runner, repository, spec.OriginOverride)
	if err != nil {
		return RepositorySnapshot{}, err
	}
	savedRefs, err := collectReviewRefs(runner, repository)
	if err != nil {
		return RepositorySnapshot{}, err
	}
	// Move handoffs omit workspace payloads but still own the immutable base
	// needed to finalize the independently transferred destination checkout.
	managedBase, err := RemoteWorkspaceBase(runner, repository)
	if err != nil {
		return RepositorySnapshot{}, err
	}
	var reviewBaselineRef *string
	if len(savedRefs) > 0 {
		scope, err := SessionStartScope(runner, repository)
		if err != nil {
			return RepositorySnapshot{}, err
		}
		reviewBaselineRef = &scope
	}
	baseCommit := identity.headCommit
	if managedBase != nil {
		baseCommit = *managedBase
	}
	return RepositorySnapshot{
		Metadata: core.RepositoryMetadata{
			// Every capture here is metadata only, whether or not it carries
			// refs: restore must never check out its commit into a checkout
			// other sessions or the user may be working in.
			ReviewRefsMetadataOnly: true,
			ReviewBaselineRef:      reviewBaselineRef,
			SavedRefs:              savedRefs,
			ID:                     spec.ID,
			RelativeDestination:    spec.RelativeDestination,
			Origin:                 identity.origin,
			PushURLs:               identity.pushURLs,
			RemoteWorkspace:        managedBase != nil,
			BaseCommit:             baseCommit,
			HeadCommit:             identity.headCommit,
			Branch:                 identity.branch,
		},
	}, nil
}

// CollectPlainDirectorySnapshot is a snapshot of a bare local session's
// directory when it is not a Git checkout.
//
// Nothing about the folder is captured: it is the user's own directory, not
// a managed checkout, so a restore never replaces its files. The metadata
// still names the directory so an archive describes the shape of the session
// it belongs to, and it carries no commit, so nothing claims Git provenance.
func CollectPlainDirectorySnapshot(spec *GitCollectionSpec) (RepositorySnapshot, error) {
	if err := ValidateComponent(spec.ID, "repository id"); err != nil {
		return RepositorySnapshot{}, err
	}
	if err := validateArchiveRelativePath(spec.RelativeDestination); err != nil {
		return RepositorySnapshot{}, err
	}
	if spec.OriginOverride != nil {
		return RepositorySnapshot{}, errors.New("a plain project directory has no origin to override")
	}
	return RepositorySnapshot{
		Metadata: core.RepositoryMetadata{
			ID:                  spec.ID,
			RelativeDestination: spec.RelativeDestination,
			PlainDirectory:      true,
		},
	}, nil
}

// checkoutLocation is a Git checkout, found from a directory somewhere
// inside it.
type checkoutLocation struct {
	topLevel string
	// subdirectory is the directory's path below topLevel; nil at the top
	// level.
	subdirectory *string
}

func locateCheckout(runner GitCommandRunner, directory string) (checkoutLocation, error) {
	output, err := gitBytes(runner, directory, []string{"rev-parse", "--show-toplevel", "--show-prefix"}, nil,
		"locate the Git checkout's top level")
	if err != nil {
		return checkoutLocation{}, err
	}
	// One line each; the prefix line is empty at the top level.
	output = bytes.TrimSuffix(output, []byte("\n"))
	split := bytes.IndexByte(output, '\n')
	if split < 0 {
		return checkoutLocation{}, errors.New("Git did not report the checkout's top level and prefix")
	}
	topLevel, err := core.FromGitBytes(output[:split])
	if err != nil {
		return checkoutLocation{}, err
	}
	if !filepath.IsAbs(topLevel) {
		return checkoutLocation{}, fmt.Errorf("Git reported a relative checkout top level %s", topLevel)
	}
	prefix, err := core.FromGitBytes(output[split+1:])
	if err != nil {
		return checkoutLocation{}, err
	}
	location := checkoutLocation{topLevel: topLevel}
	if subdirectory := gitCollectComponents(prefix); subdirectory != "" {
		if err := validateArchiveRelativePath(subdirectory); err != nil {
			return checkoutLocation{}, err
		}
		location.subdirectory = &subdirectory
	}
	return location, nil
}

type collectedGitIdentity struct {
	origin     string
	pushURLs   []string
	headCommit string
	branch     *string
}

func collectGitIdentity(runner GitCommandRunner, repository string, originOverride *string) (collectedGitIdentity, error) {
	headCommit, err := gitText(runner, repository, []string{"rev-parse", "--verify", "HEAD"})
	if err != nil {
		return collectedGitIdentity{}, fmt.Errorf("repository has no valid Git HEAD: %w", err)
	}
	var origin string
	if originOverride != nil {
		origin, err = RedactOriginCredentials(*originOverride)
		if err != nil {
			return collectedGitIdentity{}, err
		}
	} else {
		output, err := runGit(runner, repository, []string{"remote", "get-url", "origin"}, nil)
		if err != nil {
			return collectedGitIdentity{}, err
		}
		if output.Status == 0 {
			text, err := trimOutput(output.Stdout, "read Git origin")
			if err != nil {
				return collectedGitIdentity{}, err
			}
			if origin, err = RedactOriginCredentials(text); err != nil {
				return collectedGitIdentity{}, err
			}
		}
	}
	pushURLs, err := collectGitPushURLs(runner, repository)
	if err != nil {
		return collectedGitIdentity{}, err
	}
	branchOutput, err := runGit(runner, repository, []string{"symbolic-ref", "--quiet", "--short", "HEAD"}, nil)
	if err != nil {
		return collectedGitIdentity{}, err
	}
	var branch *string
	switch branchOutput.Status {
	case 0:
		text, err := trimOutput(branchOutput.Stdout, "read Git branch")
		if err != nil {
			return collectedGitIdentity{}, err
		}
		branch = &text
	case 1:
	default:
		return collectedGitIdentity{}, gitFailure("read Git branch", &branchOutput)
	}
	return collectedGitIdentity{origin: origin, pushURLs: pushURLs, headCommit: headCommit, branch: branch}, nil
}

// collectGitPushURLs reads only explicitly configured push destinations.
// `git remote get-url --push` falls back to the fetch URL when no push URL
// is configured, which would lose the distinction that lets a managed
// workspace keep its host's default push destination after restore.
func collectGitPushURLs(runner GitCommandRunner, repository string) ([]string, error) {
	output, err := runGit(runner, repository, []string{"config", "--local", "--get-all", "remote.origin.pushurl"}, nil)
	if err != nil {
		return nil, err
	}
	switch output.Status {
	case 0:
		text, err := gitDecodeUTF8(output.Stdout)
		if err != nil {
			return nil, fmt.Errorf("decode Git push URLs: %w", err)
		}
		var urls []string
		for _, url := range gitLines(text) {
			if strings.TrimSpace(url) == "" {
				continue
			}
			redacted, err := RedactOriginCredentials(strings.TrimSpace(url))
			if err != nil {
				return nil, err
			}
			urls = append(urls, redacted)
		}
		return urls, nil
	case 1:
		return nil, nil
	default:
		return nil, gitFailure("read Git push URLs", &output)
	}
}

// RemoteWorkspaceBase reads the immutable launch base recorded on a managed
// network workspace. Returning nil for an absent or false marker keeps raw
// local DeltaFrom captures separate from the managed restore path.
func RemoteWorkspaceBase(runner GitCommandRunner, repository string) (*string, error) {
	output, err := runGit(runner, repository, []string{"config", "--local", "--bool", "--get", "mj.remoteWorkspace"}, nil)
	if err != nil {
		return nil, err
	}
	var marker string
	switch output.Status {
	case 0:
		if marker, err = trimOutput(output.Stdout, "read managed workspace marker"); err != nil {
			return nil, err
		}
	case 1:
		return nil, nil
	default:
		return nil, gitFailure("read managed workspace marker", &output)
	}
	if marker != "true" {
		return nil, nil
	}

	base, err := runGit(runner, repository, []string{"config", "--local", "--get", "mj.baseCommit"}, nil)
	if err != nil {
		return nil, err
	}
	if base.Status != 0 {
		return nil, fmt.Errorf("managed workspace is marked true but mj.baseCommit is missing: %v",
			gitFailure("read managed workspace base commit", &base))
	}
	commit, err := trimOutput(base.Stdout, "read managed workspace base commit")
	if err != nil {
		return nil, err
	}
	if commit == "" {
		return nil, errors.New("managed workspace is marked true but mj.baseCommit is empty")
	}
	return &commit, nil
}

// ManagedBaseIsAvailable reports whether the recorded managed base is an
// available commit in this checkout. A missing legacy object is distinct
// from a Git execution/storage failure.
func ManagedBaseIsAvailable(runner GitCommandRunner, repository string, base string) (bool, error) {
	output, err := runGit(runner, repository, []string{"rev-parse", "--verify", "--quiet", base + "^{commit}"}, nil)
	if err != nil {
		return false, err
	}
	switch output.Status {
	case 0:
		return true, nil
	case 1:
		return false, nil
	default:
		return false, gitFailure("inspect managed base object", &output)
	}
}

func mergeBase(runner GitCommandRunner, repository string, baseRevision string) (*string, error) {
	base, err := runGit(runner, repository, []string{"rev-parse", "--verify", baseRevision}, nil)
	if err != nil {
		return nil, err
	}
	if base.Status != 0 {
		return nil, nil
	}
	commit, err := trimOutput(base.Stdout, "decode Git base revision")
	if err != nil {
		return nil, err
	}
	if commit == "" {
		return nil, nil
	}
	merged, err := runGit(runner, repository, []string{"merge-base", "HEAD", commit}, nil)
	if err != nil {
		return nil, err
	}
	switch merged.Status {
	case 0:
		text, err := trimOutput(merged.Stdout, "decode Git merge base")
		if err != nil {
			return nil, err
		}
		if text == "" {
			return nil, nil
		}
		return &text, nil
	case 1:
		return nil, nil
	default:
		return nil, gitFailure("find Git merge base", &merged)
	}
}

// HasOriginRefs is true when the repository has at least one
// `refs/remotes/origin/*` ref, the exclusion set every session delta is
// measured against.
func HasOriginRefs(runner GitCommandRunner, repository string) (bool, error) {
	refs, err := gitText(runner, repository, []string{"for-each-ref", "--format=%(objectname)", "refs/remotes/origin"})
	if err != nil {
		return false, fmt.Errorf("list origin refs: %w", err)
	}
	return refs != "", nil
}

// worktreeReviewScopes is where linked worktrees scoped their review state,
// as `refs/mj/worktrees/<name>/review-baseline`.
const worktreeReviewScopes = "refs/mj/worktrees/"

// SessionStartScope is the name that scopes the session-start refs of the
// worktree `repository` is. It is a name only: the ref it spells is no
// longer written, and this reads and writes nothing in the repository.
//
// Every linked worktree shares its repository's refs, so a session's start
// ref is keyed by a hash of this name to keep one worktree's start from
// being taken as another's. The main worktree uses `refs/mj/review-baseline`;
// a linked worktree uses its own name under `refs/mj/worktrees/`.
func SessionStartScope(runner GitCommandRunner, repository string) (string, error) {
	gitDir, commonDir, err := gitDirectories(runner, repository)
	if err != nil {
		return "", err
	}
	same, err := sameDirectory(gitDir, commonDir)
	if err != nil {
		return "", err
	}
	if same {
		return "refs/mj/review-baseline", nil
	}
	// Path::file_name: the last component when it is a normal one.
	components := archivePathComponents(gitDir)
	if len(components) == 0 || components[len(components)-1].kind != archiveComponentNormal ||
		!utf8.ValidString(components[len(components)-1].text) {
		return "", fmt.Errorf("name the worktree of %s", gitDir)
	}
	return worktreeReviewScopes + components[len(components)-1].text + "/review-baseline", nil
}

// PushReviewBaseRef is the selected push review base for a worktree,
// separate from obsolete per-turn tree baselines and from repository-wide
// reviewed-tree receipts.
func PushReviewBaseRef(scope string) string {
	return "refs/mj/push-base/" + digestBytes([]byte(scope))
}

// MigrateReviewRef moves a legacy session-start ref without overwriting a
// concurrent initializer or deleting a legacy value that changed after it
// was read.
func MigrateReviewRef(runner GitCommandRunner, repository string, legacy string, reference string) error {
	current, err := runGit(runner, repository, []string{"show-ref", "--verify", "--quiet", reference}, nil)
	if err != nil {
		return err
	}
	if current.Status == 0 {
		return nil
	}
	old, err := runGit(runner, repository, []string{"rev-parse", "--verify", "--quiet", legacy}, nil)
	if err != nil {
		return err
	}
	if old.Status != 0 {
		return nil
	}
	object, err := trimOutput(old.Stdout, "read the legacy session-start ref")
	if err != nil {
		return err
	}
	// One Git transaction publishes the new ref and removes the old one only
	// if neither value changed. Failure is visible; no baseline is discarded.
	transaction := fmt.Sprintf("create %s %s\ndelete %s %s\n", reference, object, legacy, object)
	_, err = gitBytes(runner, repository, []string{"update-ref", "--stdin"}, []byte(transaction),
		"migrate the legacy session-start ref")
	return err
}

// absoluteGitPath resolves Git's path output against repository without
// requiring --path-format (Git 2.31).
func absoluteGitPath(repository string, output []byte) (string, error) {
	path, err := core.FromGitBytes(bytes.TrimSuffix(output, []byte("\n")))
	if err != nil {
		return "", err
	}
	// std::path::absolute, which snapshot.rs uses as well.
	return snapshotAbsolute(gitJoin(repository, path))
}

// gitDirectories returns the worktree's own Git directory and the
// repository's common one, which are the same directory in the main
// worktree.
func gitDirectories(runner GitCommandRunner, repository string) (gitDir string, commonDir string, err error) {
	gitDirOutput, err := gitBytes(runner, repository, []string{"rev-parse", "--absolute-git-dir"}, nil,
		"locate Git directory")
	if err != nil {
		return "", "", err
	}
	commonOutput, err := gitBytes(runner, repository, []string{"rev-parse", "--git-common-dir"}, nil,
		"locate common Git directory")
	if err != nil {
		return "", "", err
	}
	if gitDir, err = absoluteGitPath(repository, gitDirOutput); err != nil {
		return "", "", err
	}
	if commonDir, err = absoluteGitPath(repository, commonOutput); err != nil {
		return "", "", err
	}
	return gitDir, commonDir, nil
}

func sameDirectory(left string, right string) (bool, error) {
	canonicalLeft, err := gitCanonicalize(left)
	if err != nil {
		return false, fmt.Errorf("resolve %s: %w", left, err)
	}
	canonicalRight, err := gitCanonicalize(right)
	if err != nil {
		return false, fmt.Errorf("resolve %s: %w", right, err)
	}
	return canonicalLeft == canonicalRight, nil
}

// gitCanonicalize is Rust's `fs::canonicalize`: an absolute path with every
// link resolved, which must exist.
func gitCanonicalize(path string) (string, error) {
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	return filepath.EvalSymlinks(absolute)
}

// scratchIndex is a scratch index of its own for one capture or merge,
// named to Git through `GIT_INDEX_FILE`, so the repository's real index,
// working tree and HEAD are never touched.
//
// The file lives in the Git directory and is removed by close, which every
// owner defers so it runs on every exit path (Rust: Drop); leaving it behind
// would grow the Git directory by one file per review.
type scratchIndex struct {
	path string
}

func scratchIndexCreate(runner GitCommandRunner, repository string) (*scratchIndex, error) {
	gitDir, err := gitText(runner, repository, []string{"rev-parse", "--absolute-git-dir"})
	if err != nil {
		return nil, fmt.Errorf("locate the Git directory for a review capture: %w", err)
	}
	index, err := os.CreateTemp(gitDir, "hel-review-index-*")
	if err != nil {
		return nil, fmt.Errorf("create a scratch Git index in %s: %w", gitDir, err)
	}
	// Git wants to create the index itself; an existing empty file is read
	// as a corrupt index.
	path := index.Name()
	_ = index.Close()
	_ = os.Remove(path)
	return &scratchIndex{path: path}, nil
}

// close removes the scratch index file, if Git wrote one. A failure is
// ignored, as Rust's TempPath drop ignores it.
func (s *scratchIndex) close() {
	_ = os.Remove(s.path)
}

func (s *scratchIndex) env() [][2]string {
	return [][2]string{{"GIT_INDEX_FILE", s.path}}
}

func (s *scratchIndex) run(runner GitCommandRunner, repository string, arguments []string, stdin []byte, action string) ([]byte, error) {
	return gitSuccess(runner, repository, &GitCommand{Arguments: arguments, Stdin: stdin, Env: s.env()}, action)
}

func (s *scratchIndex) writeTree(runner GitCommandRunner, repository string) (string, error) {
	output, err := s.run(runner, repository, []string{"write-tree"}, nil, "write the review capture tree")
	if err != nil {
		return "", err
	}
	tree, err := trimOutput(output, "decode the review capture tree id")
	if err != nil {
		return "", err
	}
	if tree == "" {
		return "", errors.New("git write-tree produced no tree id")
	}
	return tree, nil
}

// CapturePaths records `start` plus the current content of `paths` as a Git
// tree object and returns its id. `start` is the commit or tree to begin
// from, or nil for an unborn branch, which has no tree to start from.
//
// This is deliberately proportional to `paths` and not to the working tree.
// The scratch index is built with `read-tree`, so it has no stat cache and
// `git add` reads exactly the paths it is given: every other tracked file is
// carried over from `start` without being opened, and an untracked file that
// is not named is simply not in the tree. A capture of the whole working
// tree would instead hash every untracked file in it, which in a large
// workspace is hundreds of thousands of files and object writes.
//
// After this call `git status` reports exactly what it reported before.
// `git add -A` with a pathspec records a deletion as a deletion, and honours
// the ignore rules, which is what makes two captures comparable as "what the
// agent changed".
func CapturePaths(runner GitCommandRunner, repository string, start *string, paths []string) (string, error) {
	scratch, err := scratchIndexCreate(runner, repository)
	if err != nil {
		return "", err
	}
	defer scratch.close()
	from := "--empty"
	if start != nil {
		from = *start
	}
	if _, err := scratch.run(runner, repository, []string{"read-tree", from}, nil, "initialize the scratch index"); err != nil {
		return "", err
	}
	if len(paths) > 0 {
		arguments := append([]string{"add", "-A", "--"}, paths...)
		if _, err := scratch.run(runner, repository, arguments, nil, "stage the changed paths into a scratch index"); err != nil {
			return "", err
		}
	}
	return scratch.writeTree(runner, repository)
}

// ReviewPoint is the tree a review point names, the commit HEAD was at when
// it was taken, where that sits in HEAD's reflog, the turn's commits no
// review has seen yet, and the session that took it.
type ReviewPoint struct {
	Tree string
	// Head is nil for an unborn branch, and for a bare tree id pinned by a
	// release that did not record HEAD.
	Head *string
	// Reflog is nil for a point recorded by a release that did not record
	// it, and for one taken while HEAD's reflog did not end at Head.
	Reflog *ReflogAnchor
	// Pending are commits the turn made that were not in the workspace when
	// this point was captured, such as work committed on a branch it then
	// left. A later review still counts them as the turn's.
	Pending []string
	// Session is nil for a point recorded by a release that did not record
	// it.
	Session *string
}

// ReflogAnchor is where a review point sits in its worktree's HEAD reflog:
// how many entries the reflog held when the point was taken, and the time of
// the newest one.
//
// Entries expire only from the oldest end, so the count finds the entry even
// when one second holds several entries for one commit; the time finds it
// again after expiry has shifted the count.
type ReflogAnchor struct {
	Entries int
	// Timestamp is nil when the reflog was empty.
	Timestamp *int64
}

// RecordReviewPoint records `point` as one Git object and returns its id:
// the id a review hands on as its capture and later pins as a baseline.
//
// The object is an annotated tag pointing at the tree, with one line per
// field as its message: `head <commit>`, `reflog <entries> [<timestamp>]`,
// one `pending <commit>` per pending commit, and `session <id>`. The tree and
// its commit are one fact, so they travel as one id, and the session lets a
// pin say whose it is. A tag is still a tree-ish, so `git diff`,
// `git ls-tree` and `^{tree}` read it exactly as they read the bare tree ids
// earlier releases stored, with no change to the controller's records or the
// relay protocol. Unlike a commit, a tag pointing at a tree does not appear
// in `git log --all`. The fixed tagger and date make equal captures equal
// ids.
func RecordReviewPoint(runner GitCommandRunner, repository string, point *ReviewPoint) (string, error) {
	var tag strings.Builder
	fmt.Fprintf(&tag, "object %s\ntype tree\ntag mjolnir-review-point\n"+
		"tagger Mjolnir <mjolnir@localhost> 0 +0000\n\n", point.Tree)
	if point.Head != nil {
		fmt.Fprintf(&tag, "head %s\n", *point.Head)
	}
	if anchor := point.Reflog; anchor != nil {
		fmt.Fprintf(&tag, "reflog %d", anchor.Entries)
		if anchor.Timestamp != nil {
			fmt.Fprintf(&tag, " %d", *anchor.Timestamp)
		}
		tag.WriteByte('\n')
	}
	for _, commit := range point.Pending {
		fmt.Fprintf(&tag, "pending %s\n", commit)
	}
	if point.Session != nil {
		fmt.Fprintf(&tag, "session %s\n", *point.Session)
	}
	output, err := gitBytes(runner, repository, []string{"mktag"}, []byte(tag.String()), "record the review point")
	if err != nil {
		return "", err
	}
	return trimOutput(output, "decode the review point id")
}

// ReadReviewPoint reads a review point, or a bare tree id from an earlier
// release as a point with no recorded HEAD.
func ReadReviewPoint(runner GitCommandRunner, repository string, id string) (ReviewPoint, error) {
	kindOutput, err := gitBytes(runner, repository, []string{"cat-file", "-t", id}, nil, "read the review point type")
	if err != nil {
		return ReviewPoint{}, err
	}
	kind, err := trimOutput(kindOutput, "decode the review point type")
	if err != nil {
		return ReviewPoint{}, err
	}
	switch kind {
	case "tree":
		return ReviewPoint{Tree: id}, nil
	case "tag":
	default:
		return ReviewPoint{}, fmt.Errorf("review point %s is a %s, not a tag or tree", id, kind)
	}
	tagOutput, err := gitBytes(runner, repository, []string{"cat-file", "tag", id}, nil, "read the review point")
	if err != nil {
		return ReviewPoint{}, err
	}
	tag := gitFromUTF8Lossy(tagOutput)
	header, message, found := strings.Cut(tag, "\n\n")
	if !found {
		header, message = tag, ""
	}
	field := func(text string, name string) *string {
		for _, line := range gitLines(text) {
			if value, ok := strings.CutPrefix(line, name); ok {
				return &value
			}
		}
		return nil
	}
	if kind := field(header, "type "); kind == nil || *kind != "tree" {
		return ReviewPoint{}, fmt.Errorf("review point %s does not name a tree", id)
	}
	tree := field(header, "object ")
	if tree == nil {
		return ReviewPoint{}, fmt.Errorf("review point %s names no object", id)
	}
	// An anchor that does not parse is treated as absent, which makes the
	// next review diff the whole tree rather than misattribute.
	var reflog *ReflogAnchor
	if anchor := field(message, "reflog "); anchor != nil {
		reflog = parseReflogAnchor(*anchor)
	}
	var pending []string
	for _, line := range gitLines(message) {
		if commit, ok := strings.CutPrefix(line, "pending "); ok {
			pending = append(pending, commit)
		}
	}
	return ReviewPoint{
		Tree:    *tree,
		Head:    field(message, "head "),
		Reflog:  reflog,
		Pending: pending,
		Session: field(message, "session "),
	}, nil
}

// parseReflogAnchor reads `<entries> [<timestamp>]`, or nil when it does not
// parse.
func parseReflogAnchor(anchor string) *ReflogAnchor {
	parts := strings.Fields(anchor)
	if len(parts) == 0 {
		return nil
	}
	entries, err := gitParseUint(parts[0], 63)
	if err != nil {
		return nil
	}
	result := &ReflogAnchor{Entries: int(entries)}
	if len(parts) > 1 {
		timestamp, err := strconv.ParseInt(parts[1], 10, 64)
		if err != nil {
			return nil
		}
		result.Timestamp = &timestamp
	}
	return result
}

// ReflogEntry is one entry of a worktree's HEAD reflog: the commit HEAD was
// set to, that commit's tree and parents, when, and the action Git recorded.
type ReflogEntry struct {
	Commit    string
	Tree      string
	Parents   []string
	Timestamp int64
	Message   string
}

// MovesOnly reports whether this entry only moved HEAD to a commit that
// already existed, as a checkout, reset, fast-forward, clone, or the start,
// end and `reset` steps of a rebase do, rather than recording a commit made
// in this worktree. `git rebase --rebase-merges` resets to its `onto` label
// between the branches it replays.
//
// Git names the action at the start of every message (`commit:`,
// `checkout: moving from …`, `merge topic: Fast-forward`, `rebase (pick):
// …`). A commit's own subject can only follow a `commit`, `cherry-pick`,
// `revert`, `am` or rebase action, so the moves are told apart by their
// action alone. A message this does not recognize, such as one written
// under a custom `GIT_REFLOG_ACTION`, counts as a commit made here: a review
// that covers too much is the safe way to be wrong.
func (e ReflogEntry) MovesOnly() bool {
	message := e.Message
	if strings.HasPrefix(message, "commit") {
		return false
	}
	action, rest, found := strings.Cut(message, ": ")
	if !found {
		action, rest = message, ""
	}
	if strings.HasPrefix(message, "checkout: moving from ") ||
		strings.HasPrefix(message, "reset: moving to ") ||
		strings.HasPrefix(message, "clone: from ") ||
		strings.HasPrefix(message, "branch: ") {
		return true
	}
	// `merge topic: Fast-forward` and `pull …: Fast-forward`. After
	// `cherry-pick` the same words could be a commit's subject, so
	// `cherry-pick --ff` counts as a commit.
	if (strings.HasPrefix(action, "merge ") || action == "pull" || strings.HasPrefix(action, "pull ")) &&
		archiveEqualFoldASCII(rest, "fast-forward") {
		return true
	}
	for _, step := range []string{"(start)", "(finish)", "(abort)", "(reset)"} {
		if strings.HasSuffix(action, step) {
			return true
		}
	}
	return false
}

// Amends reports whether this entry records an amend, which replaces the
// commit HEAD held before it.
func (e ReflogEntry) Amends() bool {
	return strings.HasSuffix(e.action(), "(amend)")
}

// Replays reports whether this entry records a commit replaying another
// one: a rebase step or a cherry-pick. Such a commit keeps the author, the
// authoring time and, unless it was reworded, the subject of the one it
// replays.
func (e ReflogEntry) Replays() bool {
	action := e.action()
	return strings.HasPrefix(action, "rebase") ||
		strings.Contains(action, "--rebase") ||
		strings.HasPrefix(action, "cherry-pick")
}

func (e ReflogEntry) action() string {
	action, _, _ := strings.Cut(e.Message, ": ")
	return action
}

// ReadHeadReflog is the worktree's HEAD reflog, oldest entry first, or no
// entries when HEAD has no reflog.
//
// A linked worktree has a HEAD reflog of its own, so this is the history of
// one checkout. `git log -g` shows the current commit as an entry even when
// the reflog is gone, so its existence is checked first.
func ReadHeadReflog(runner GitCommandRunner, repository string) ([]ReflogEntry, error) {
	exists, err := runGit(runner, repository, []string{"reflog", "exists", "HEAD"}, nil)
	if err != nil {
		return nil, err
	}
	switch exists.Status {
	case 0:
	case 1:
		return nil, nil
	default:
		return nil, gitFailure("check for a HEAD reflog", &exists)
	}
	output, err := gitBytes(runner, repository, []string{
		"log",
		"--walk-reflogs",
		"--no-abbrev",
		"--no-show-signature",
		"--date=raw",
		"-z",
		"--format=%H%x1f%T%x1f%P%x1f%gd%x1f%gs",
		"HEAD",
	}, nil, "read the HEAD reflog")
	if err != nil {
		return nil, err
	}
	var entries []ReflogEntry
	for _, raw := range bytes.Split(output, []byte{0}) {
		if len(raw) == 0 {
			continue
		}
		record := gitFromUTF8Lossy(raw)
		fields := strings.SplitN(record, "\x1f", 5)
		if len(fields) != 5 {
			return nil, fmt.Errorf("malformed HEAD reflog entry %s", gitRustDebugString(record))
		}
		commit, tree, parents, selector, message := fields[0], fields[1], fields[2], fields[3], fields[4]
		// `--date=raw` makes the selector `HEAD@{<seconds> <zone>}`.
		timestamp, ok := int64(0), false
		if _, rest, found := strings.Cut(selector, "{"); found {
			if words := strings.Fields(rest); len(words) > 0 {
				if seconds, err := strconv.ParseInt(words[0], 10, 64); err == nil {
					timestamp, ok = seconds, true
				}
			}
		}
		if !ok {
			return nil, fmt.Errorf("HEAD reflog selector %s has no time", gitRustDebugString(selector))
		}
		entries = append(entries, ReflogEntry{
			Commit:    strings.TrimSpace(commit),
			Tree:      tree,
			Parents:   gitFields(parents),
			Timestamp: timestamp,
			Message:   message,
		})
	}
	slices.Reverse(entries)
	return entries, nil
}

// TreeWithoutPaths is `tree` with `paths` removed, written as a new tree
// object through a scratch index. Paths `tree` does not hold are ignored.
func TreeWithoutPaths(runner GitCommandRunner, repository string, tree string, paths []string) (string, error) {
	scratch, err := scratchIndexCreate(runner, repository)
	if err != nil {
		return "", err
	}
	defer scratch.close()
	if _, err := scratch.run(runner, repository, []string{"read-tree", tree}, nil, "read the tree into a scratch index"); err != nil {
		return "", err
	}
	var stdin []byte
	for _, path := range paths {
		stdin = append(stdin, path...)
		stdin = append(stdin, 0)
	}
	if _, err := scratch.run(runner, repository, []string{"update-index", "--force-remove", "-z", "--stdin"}, stdin,
		"remove paths from the scratch index"); err != nil {
		return "", err
	}
	return scratch.writeTree(runner, repository)
}

// MergeConflicts is what MergeTrees records where both sides changed the
// same lines.
type MergeConflicts int

const (
	// MergeConflictsMark writes conflict markers holding both sides.
	MergeConflictsMark MergeConflicts = iota
	// MergeConflictsKeepOurs keeps the `ours` side's lines.
	MergeConflictsKeepOurs
)

// MergeTrees three-way merges `theirs` into `ours` from their common
// `ancestor` (nil for histories with no common ancestor) and returns the
// resulting tree.
//
// This is Git's resolve strategy run in a scratch index, and it works on any
// Git a worker may have; `git merge-tree --write-tree` needs Git 2.38.
// `read-tree -m` settles every path only one side changed. A path both sides
// changed as regular files is merged with `git merge-file`, which settles
// lines where the changes overlap as `conflicts` says. Any other path both
// sides changed, and a file `merge-file` cannot merge such as a binary one,
// keeps `ours`. There is no rename detection: a file `theirs` renamed while
// `ours` changed it ends up under both names.
func MergeTrees(
	runner GitCommandRunner,
	repository string,
	ancestor *string,
	ours string,
	theirs string,
	conflicts MergeConflicts,
) (string, error) {
	var base string
	if ancestor != nil {
		base = *ancestor
	} else {
		empty, err := EmptyTreeID(runner, repository)
		if err != nil {
			return "", err
		}
		base = empty
	}
	scratch, err := scratchIndexCreate(runner, repository)
	if err != nil {
		return "", err
	}
	defer scratch.close()
	if _, err := scratch.run(runner, repository, []string{"read-tree", "-m", "-i", "--aggressive", base, ours, theirs}, nil,
		"merge the trees into a scratch index"); err != nil {
		return "", err
	}
	listed, err := scratch.run(runner, repository, []string{"ls-files", "--unmerged", "-z"}, nil,
		"list the paths both sides changed")
	if err != nil {
		return "", err
	}
	unmerged, err := unmergedPaths(listed)
	if err != nil {
		return "", err
	}
	if len(unmerged) > 0 {
		resolutions, err := resolveConflicts(runner, repository, unmerged, conflicts)
		if err != nil {
			return "", err
		}
		if _, err := scratch.run(runner, repository, []string{"update-index", "-z", "--index-info"}, resolutions,
			"record the resolved paths"); err != nil {
			return "", err
		}
	}
	return scratch.writeTree(runner, repository)
}

// indexEntry is one index entry: its mode and object id.
type indexEntry struct {
	mode   string
	object string
}

func (e *indexEntry) isRegularFile() bool {
	return e.mode == "100644" || e.mode == "100755"
}

// unmergedPath is a path `read-tree -m` left unmerged, with its ancestor,
// ours and theirs entries (index stages 1, 2 and 3); a side without the path
// has a nil entry.
type unmergedPath struct {
	// path holds the raw bytes Git printed.
	path   string
	stages [3]*indexEntry
}

func (p *unmergedPath) ours() *indexEntry {
	return p.stages[1]
}

// contentMerge returns the ancestor, ours and theirs entries when both sides
// changed the path as regular files, which is the one kind of conflict
// merged by content. The ancestor is nil unless it is a regular file too.
func (p *unmergedPath) contentMerge() (ancestor *indexEntry, ours *indexEntry, theirs *indexEntry, ok bool) {
	ancestor, ours, theirs = p.stages[0], p.stages[1], p.stages[2]
	if ours == nil || theirs == nil || !ours.isRegularFile() || !theirs.isRegularFile() {
		return nil, nil, nil, false
	}
	if ancestor != nil && !ancestor.isRegularFile() {
		ancestor = nil
	}
	return ancestor, ours, theirs, true
}

// indexInfo is the `update-index -z --index-info` record that settles this
// path at `entry`, or removes it when entry is nil.
func (p *unmergedPath) indexInfo(entry *indexEntry) []byte {
	var record []byte
	if entry != nil {
		record = fmt.Appendf(nil, "%s %s\t", entry.mode, entry.object)
	} else {
		// Mode 0 removes every stage of the path. Git checks the object id
		// only for the repository's id length, which any stage shows.
		length := 40
		if index := slices.IndexFunc(p.stages[:], func(stage *indexEntry) bool { return stage != nil }); index >= 0 {
			length = len(p.stages[index].object)
		}
		record = fmt.Appendf(nil, "0 %s\t", strings.Repeat("0", length))
	}
	record = append(record, p.path...)
	return append(record, 0)
}

// unmergedPaths reads `git ls-files --unmerged -z`:
// `<mode> <object> <stage>\t<path>\0`, one record per stage, grouped by path.
func unmergedPaths(output []byte) ([]unmergedPath, error) {
	var paths []unmergedPath
	for _, record := range bytes.Split(output, []byte{0}) {
		if len(record) == 0 {
			continue
		}
		tab := bytes.IndexByte(record, '\t')
		if tab < 0 {
			return nil, errors.New("an unmerged index entry has no path")
		}
		fields, err := gitDecodeUTF8(record[:tab])
		if err != nil {
			return nil, fmt.Errorf("decode an unmerged entry: %w", err)
		}
		parts := strings.Split(fields, " ")
		if len(parts) != 3 {
			return nil, fmt.Errorf("malformed unmerged index entry %s", gitRustDebugString(fields))
		}
		stage, err := gitParseUint(parts[2], 64)
		if err != nil {
			return nil, fmt.Errorf("parse an unmerged entry's stage: %w", err)
		}
		if stage < 1 || stage > 3 {
			return nil, fmt.Errorf("unexpected index stage %d", stage)
		}
		path := string(record[tab+1:])
		if len(paths) == 0 || paths[len(paths)-1].path != path {
			paths = append(paths, unmergedPath{path: path})
		}
		paths[len(paths)-1].stages[stage-1] = &indexEntry{mode: parts[0], object: parts[1]}
	}
	return paths, nil
}

// resolveConflicts settles every unmerged path and returns the
// `update-index -z --index-info` input that records the results.
func resolveConflicts(runner GitCommandRunner, repository string, conflicts []unmergedPath, lines MergeConflicts) ([]byte, error) {
	var objects []string
	for index := range conflicts {
		ancestor, ours, theirs, ok := conflicts[index].contentMerge()
		if !ok {
			continue
		}
		if ancestor != nil {
			objects = append(objects, ancestor.object)
		}
		objects = append(objects, ours.object, theirs.object)
	}
	blobs, err := readBlobs(runner, repository, objects)
	if err != nil {
		return nil, err
	}
	files, err := os.MkdirTemp("", ".tmp")
	if err != nil {
		return nil, fmt.Errorf("create a directory for merging conflicted files: %w", err)
	}
	defer os.RemoveAll(files)
	type mergedFile struct {
		conflict *unmergedPath
		mode     string
		file     string
	}
	var indexInfo []byte
	var merged []mergedFile
	for number := range conflicts {
		conflict := &conflicts[number]
		mode, file, ok := "", "", false
		if ancestor, ours, theirs, content := conflict.contentMerge(); content {
			mode, file, ok, err = mergeFile(runner, repository, filepath.Join(files, strconv.Itoa(number)),
				ancestor, ours, theirs, blobs, lines)
			if err != nil {
				return nil, err
			}
		}
		if ok {
			merged = append(merged, mergedFile{conflict: conflict, mode: mode, file: file})
		} else {
			indexInfo = append(indexInfo, conflict.indexInfo(conflict.ours())...)
		}
	}
	if len(merged) == 0 {
		return indexInfo, nil
	}
	var list []byte
	for _, file := range merged {
		list = append(list, file.file...)
		list = append(list, '\n')
	}
	ids, err := gitBytes(runner, repository, []string{"hash-object", "-w", "--no-filters", "--stdin-paths"}, list,
		"store the merged files")
	if err != nil {
		return nil, err
	}
	text, err := gitDecodeUTF8(ids)
	if err != nil {
		return nil, fmt.Errorf("decode the merged file ids: %w", err)
	}
	objectIDs := gitLines(text)
	if len(objectIDs) != len(merged) {
		return nil, fmt.Errorf("git hash-object stored %d of %d merged files", len(objectIDs), len(merged))
	}
	for index, file := range merged {
		entry := indexEntry{mode: file.mode, object: objectIDs[index]}
		indexInfo = append(indexInfo, file.conflict.indexInfo(&entry)...)
	}
	return indexInfo, nil
}

// mergeFile merges one file's three versions with `git merge-file` and
// returns the merged file and its mode, or merged false when Git cannot
// merge it, such as a binary file. The versions are written next to `stem`.
func mergeFile(
	runner GitCommandRunner,
	repository string,
	stem string,
	ancestor *indexEntry,
	ours *indexEntry,
	theirs *indexEntry,
	blobs map[string][]byte,
	conflicts MergeConflicts,
) (mode string, file string, merged bool, err error) {
	write := func(side string, entry *indexEntry) (string, error) {
		path := stem + "." + side
		var content []byte
		if entry != nil {
			blob, ok := blobs[entry.object]
			if !ok {
				return "", fmt.Errorf("blob %s was not read", entry.object)
			}
			content = blob
		}
		if err := os.WriteFile(path, content, 0o666); err != nil {
			return "", fmt.Errorf("write %s: %w", path, err)
		}
		return path, nil
	}
	current, err := write("ours", ours)
	if err != nil {
		return "", "", false, err
	}
	arguments := []string{"merge-file", "-q"}
	if conflicts == MergeConflictsKeepOurs {
		arguments = append(arguments, "--ours")
	}
	arguments = append(arguments, "-L", "reviewed", "-L", "ancestor", "-L", "merged in")
	ancestorPath, err := write("ancestor", ancestor)
	if err != nil {
		return "", "", false, err
	}
	theirsPath, err := write("theirs", theirs)
	if err != nil {
		return "", "", false, err
	}
	arguments = append(arguments, current, ancestorPath, theirsPath)
	output, err := runner.Run(repository, &GitCommand{Arguments: arguments})
	if err != nil {
		return "", "", false, err
	}
	// The status is the number of conflicts, capped at 127; anything else is
	// an error and leaves the file unmerged.
	if output.Status < 0 || output.Status > 127 {
		return "", "", false, nil
	}
	// A mode only one side changed takes that side's mode.
	mode = ours.mode
	if ancestor != nil && ancestor.mode == ours.mode {
		mode = theirs.mode
	}
	return mode, current, true, nil
}

// readBlobs reads blobs in one `git cat-file --batch`, keyed by object id.
func readBlobs(runner GitCommandRunner, repository string, objects []string) (map[string][]byte, error) {
	objects = slices.Clone(objects)
	slices.Sort(objects)
	objects = slices.Compact(objects)
	blobs := map[string][]byte{}
	if len(objects) == 0 {
		return blobs, nil
	}
	var request strings.Builder
	for _, object := range objects {
		request.WriteString(object)
		request.WriteByte('\n')
	}
	output, err := gitBytes(runner, repository, []string{"cat-file", "--batch"}, []byte(request.String()),
		"read the conflicted files")
	if err != nil {
		return nil, err
	}
	// Each answer is `<id> <type> <size>\n<content>\n`, in request order.
	rest := output
	for _, object := range objects {
		end := bytes.IndexByte(rest, '\n')
		if end < 0 {
			return nil, errors.New("git cat-file --batch ended early")
		}
		header, err := gitDecodeUTF8(rest[:end])
		if err != nil {
			return nil, fmt.Errorf("decode a cat-file header: %w", err)
		}
		size, err := gitParseUint(header[strings.LastIndexByte(header, ' ')+1:], 64)
		if err != nil {
			return nil, fmt.Errorf("git cat-file could not read %s: %s", object, header)
		}
		if size > uint64(len(rest)-end-1) {
			return nil, errors.New("git cat-file --batch output is truncated")
		}
		blobs[object] = bytes.Clone(rest[end+1 : end+1+int(size)])
		if next := end + 2 + int(size); next <= len(rest) {
			rest = rest[next:]
		} else {
			rest = nil
		}
	}
	return blobs, nil
}

// CaptureWorktreeTree records the working tree, tracked and untracked alike,
// as a Git tree object and returns its id.
//
// The staging runs against a scratch index file named by `GIT_INDEX_FILE`,
// so the repository's real index, working tree and HEAD are untouched: after
// this call `git status` reports exactly what it reported before. Untracked
// ignored files stay out, because `git add -A` honours the ignore rules,
// which is what makes two captures comparable as "what the agent changed".
func CaptureWorktreeTree(runner GitCommandRunner, repository string) (string, error) {
	scratch, err := scratchIndexCreate(runner, repository)
	if err != nil {
		return "", err
	}
	defer scratch.close()
	locations, err := gitPaths(runner, repository, []string{"index"})
	if err != nil {
		return "", fmt.Errorf("locate the real Git index: %w", err)
	}
	if len(locations) == 0 {
		return "", errors.New("Git omitted the real index location")
	}
	realIndex := locations[len(locations)-1][1]
	missing, err := gitCopyFile(realIndex, scratch.path)
	switch {
	case missing:
		head, err := runGit(runner, repository, []string{"rev-parse", "--verify", "--quiet", "HEAD"}, nil)
		if err != nil {
			return "", err
		}
		if head.Status != 0 && head.Status != 1 {
			return "", gitFailure("resolve capture HEAD", &head)
		}
		base := "--empty"
		if head.Status == 0 {
			base = "HEAD"
		}
		if _, err := scratch.run(runner, repository, []string{"read-tree", base}, nil, "initialize the scratch index"); err != nil {
			return "", err
		}
	case err != nil:
		return "", fmt.Errorf("copy the real Git index for capture: %w", err)
	}
	// `:/` names the whole working tree even when `repository` is a
	// subdirectory of its checkout; `.` would stage only that subdirectory.
	if _, err := scratch.run(runner, repository, []string{"add", "-A", "--", ":/"}, nil,
		"stage the working tree into a scratch index"); err != nil {
		return "", err
	}
	return scratch.writeTree(runner, repository)
}

// gitCopyFile is Rust's `fs::copy`: the contents and permissions of source
// written to destination. missing reports that source does not exist.
func gitCopyFile(source string, destination string) (missing bool, err error) {
	input, err := os.Open(source)
	if errors.Is(err, fs.ErrNotExist) {
		return true, err
	}
	if err != nil {
		return false, err
	}
	defer input.Close()
	info, err := input.Stat()
	if err != nil {
		return false, err
	}
	output, err := os.OpenFile(destination, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, info.Mode().Perm())
	if err != nil {
		return errors.Is(err, fs.ErrNotExist), err
	}
	if _, err := io.Copy(output, input); err != nil {
		_ = output.Close()
		return false, err
	}
	return false, output.Close()
}

// PinReviewTree points `reference` at `tree` so the capture survives garbage
// collection.
//
// A failure here is not fatal to a review -- the objects are still reachable
// until the next gc -- so the caller logs it rather than losing the capture.
func PinReviewTree(runner GitCommandRunner, repository string, reference string, tree string) error {
	_, err := gitBytes(runner, repository, []string{"update-ref", reference, tree}, nil, "pin the review capture tree")
	return err
}

// RecordReviewedTree records a clean review of `tree` at `commit`.
func RecordReviewedTree(runner GitCommandRunner, repository string, tree string, commit string) error {
	return PinReviewTree(runner, repository, "refs/mj/reviewed/"+tree, commit)
}

// RecordOverriddenTree records an overridden review of `tree` at `commit`. A
// push of the tree is allowed, but capture never takes the commit as the
// base of the next review.
func RecordOverriddenTree(runner GitCommandRunner, repository string, tree string, commit string) error {
	return PinReviewTree(runner, repository, "refs/mj/overridden/"+tree, commit)
}

// EmptyTreeID is the empty tree of this repository's object format, read
// from Git rather than hardcoded so a SHA-256 repository works as well as a
// SHA-1 one.
func EmptyTreeID(runner GitCommandRunner, repository string) (string, error) {
	output, err := runner.Run(repository, &GitCommand{Arguments: []string{"mktree"}})
	if err != nil {
		return "", err
	}
	if output.Status != 0 {
		return "", gitFailure("compute the empty tree id", &output)
	}
	return trimOutput(output.Stdout, "decode the empty tree id")
}

// DiffBetweenTrees is the unified diff between two captured trees. A nil
// `base` diffs against the empty tree, which renders the whole capture as
// additions -- what a review wants the first time it runs in a repository.
// Submodule changes are shown whatever `diff.ignoreSubmodules` says, so a
// review cannot miss one.
func DiffBetweenTrees(runner GitCommandRunner, repository string, base *string, current string) (string, error) {
	var from string
	if base != nil {
		from = *base
	} else {
		empty, err := EmptyTreeID(runner, repository)
		if err != nil {
			return "", err
		}
		from = empty
	}
	patch, err := gitBytes(runner, repository,
		[]string{"diff", "--binary", "--no-ext-diff", "--ignore-submodules=none", from, current}, nil,
		"diff the captured review trees")
	if err != nil {
		return "", err
	}
	return gitFromUTF8Lossy(patch), nil
}

// NumstatBetweenTrees is `git diff --numstat -z` between two trees, with
// rename detection: one record per changed file, as Git emits it. A rename's
// record carries an empty path followed by the old and new paths; a binary
// file's counts are `-`. DiffBetweenTrees supplies the same comparison as a
// patch.
func NumstatBetweenTrees(runner GitCommandRunner, repository string, base string, current string) ([]byte, error) {
	return gitBytes(runner, repository,
		[]string{"diff", "--numstat", "-z", "-M", "--no-ext-diff", "--ignore-submodules=none", base, current},
		nil, "count the captured review trees' changed lines per file")
}

type gitHistorySelection struct {
	// baseCommit is informational only; an empty string for session deltas.
	baseCommit string
	// bundleArguments are the arguments for the bundle command, or nil when
	// nothing has to be sent.
	bundleArguments []string
}

// clone copies the selection so the copy's arguments can be changed alone
// (Rust: #[derive(Clone)]).
func (s gitHistorySelection) clone() gitHistorySelection {
	return gitHistorySelection{baseCommit: s.baseCommit, bundleArguments: slices.Clone(s.bundleArguments)}
}

func collectReviewRefs(runner GitCommandRunner, repository string) (map[string]string, error) {
	listed, err := gitText(runner, repository, []string{"for-each-ref", "--format=%(refname) %(objectname)", "refs/mj/"})
	if err != nil {
		return nil, err
	}
	var refs map[string]string
	for _, line := range gitLines(listed) {
		name, oid, found := strings.Cut(line, " ")
		if !found {
			return nil, errors.New("malformed review ref")
		}
		// Old baseline pins are not carried: nothing reads them.
		if core.IsReviewBaselinePin(name) {
			continue
		}
		if refs == nil {
			refs = map[string]string{}
		}
		refs[name] = oid
	}
	return refs, nil
}

func includeReviewHistory(history *gitHistorySelection, refs map[string]string) {
	if len(refs) == 0 {
		return
	}
	if history.bundleArguments == nil {
		history.bundleArguments = []string{"bundle", "create", "-"}
	}
	// Before --not: these are positive tips even for SessionDelta bundles.
	names := make([]string, 0, len(refs))
	for name := range refs {
		names = append(names, name)
	}
	slices.Sort(names)
	history.bundleArguments = slices.Insert(slices.Clip(history.bundleArguments), 3, names...)
}

type cloneRefState struct {
	history       gitHistorySelection
	savedRefs     map[string]string
	stashStack    []core.SavedStashEntry
	operation     *collectedOperation
	temporaryRefs []string
}

func collectCloneRefs(runner GitCommandRunner, repository string, base string) (cloneRefState, error) {
	// An unfinished operation is part of the clone's work: it travels in the
	// snapshot, and its objects in the bundle, like a stash entry's.
	operation, err := operationCollect(runner, repository)
	if err != nil {
		return cloneRefState{}, err
	}
	if _, err := gitText(runner, repository, []string{"rev-parse", "--verify", base + "^{commit}"}); err != nil {
		return cloneRefState{}, fmt.Errorf("managed clone source commit is unavailable: %w", err)
	}
	listRefs := []string{
		"for-each-ref",
		"--format=%(refname) %(objectname)",
		"refs/heads",
		"refs/tags",
		"refs/notes",
		"refs/mj/",
	}
	if operation != nil {
		listRefs = append(listRefs, operationRefNamespaces[:]...)
	}
	listed, err := gitBytesOwned(runner, repository, listRefs, "list the clone's refs")
	if err != nil {
		return cloneRefState{}, err
	}
	lines, err := trimOutput(listed, "decode the clone's refs")
	if err != nil {
		return cloneRefState{}, err
	}
	var savedRefs map[string]string
	for _, line := range gitLines(lines) {
		name, oid, found := strings.Cut(line, " ")
		if !found {
			return cloneRefState{}, errors.New("malformed saved Git ref")
		}
		if oid == "" {
			return cloneRefState{}, fmt.Errorf("saved Git ref %s has no object", name)
		}
		if core.IsReviewBaselinePin(name) {
			continue
		}
		if savedRefs == nil {
			savedRefs = map[string]string{}
		}
		savedRefs[name] = oid
	}
	stashPresent, err := runGit(runner, repository, []string{"show-ref", "--verify", "--quiet", "refs/stash"}, nil)
	if err != nil {
		return cloneRefState{}, err
	}
	var stashStack []core.SavedStashEntry
	switch stashPresent.Status {
	case 0:
		reflog, err := gitText(runner, repository, []string{"reflog", "show", "--format=%H%x09%gs", "refs/stash"})
		if err != nil {
			return cloneRefState{}, err
		}
		for _, line := range gitLines(reflog) {
			commit, message, found := strings.Cut(line, "\t")
			if !found {
				return cloneRefState{}, errors.New("malformed stash reflog")
			}
			stashStack = append(stashStack, core.SavedStashEntry{Commit: commit, Message: message})
		}
	case 1:
	default:
		return cloneRefState{}, gitFailure("inspect clone stash", &stashPresent)
	}
	var temporaryRefs []string
	captureID, err := core.NewSessionID()
	if err != nil {
		return cloneRefState{}, err
	}
	refNames := gitSortedKeys(savedRefs)
	history, staged := func() (gitHistorySelection, error) {
		for index, entry := range stashStack {
			name := fmt.Sprintf("refs/mj-checkpoint/%s/stash/%08d", captureID, index)
			if _, err := gitBytes(runner, repository, []string{"update-ref", name, entry.Commit}, nil,
				"preserve stash entry for checkpoint bundle"); err != nil {
				return gitHistorySelection{}, err
			}
			temporaryRefs = append(temporaryRefs, name)
		}
		if operation != nil {
			if err := operation.carryObjects(runner, repository,
				fmt.Sprintf("refs/mj-checkpoint/%s/operation", captureID), &temporaryRefs); err != nil {
				return gitHistorySelection{}, err
			}
		}
		tips := []string{"HEAD"}
		tips = append(tips, refNames...)
		tips = append(tips, temporaryRefs...)
		countArguments := append([]string{"rev-list", "--count"}, tips...)
		countArguments = append(countArguments, "^"+base)
		counted, err := gitBytesOwned(runner, repository, countArguments, "count clone-owned commits")
		if err != nil {
			return gitHistorySelection{}, err
		}
		countText, err := gitDecodeUTF8(counted)
		if err != nil {
			return gitHistorySelection{}, err
		}
		count, err := gitParseUint(strings.TrimSpace(countText), 64)
		if err != nil {
			return gitHistorySelection{}, fmt.Errorf("count clone-owned commits: %w", err)
		}
		kinds := make([]string, 0, len(refNames))
		for _, name := range refNames {
			kind, err := gitText(runner, repository, []string{"cat-file", "-t", savedRefs[name]})
			if err != nil {
				return gitHistorySelection{}, err
			}
			kinds = append(kinds, kind)
		}
		nonCommitTip := slices.ContainsFunc(kinds, func(kind string) bool { return kind != "commit" }) ||
			(operation != nil && operation.carriesTags())
		selection := gitHistorySelection{baseCommit: base}
		if count > 0 || nonCommitTip {
			selection.bundleArguments = append([]string{"bundle", "create", "-"}, tips...)
			selection.bundleArguments = append(selection.bundleArguments, "^"+base)
		}
		return selection, nil
	}()
	if staged != nil {
		if err := removeTemporaryRefs(runner, repository, temporaryRefs); err != nil {
			return cloneRefState{}, err
		}
		return cloneRefState{}, staged
	}
	return cloneRefState{
		history:       history,
		savedRefs:     savedRefs,
		stashStack:    stashStack,
		operation:     operation,
		temporaryRefs: temporaryRefs,
	}, nil
}

// removeTemporaryRefs removes the refs a capture made so the committed
// bundle carries stash entries and operation objects.
func removeTemporaryRefs(runner GitCommandRunner, repository string, names []string) error {
	// Every ref is attempted, so one failure leaves no others behind.
	var firstFailure error
	for _, name := range names {
		_, err := gitBytes(runner, repository, []string{"update-ref", "-d", name}, nil, "remove temporary checkpoint ref")
		if err != nil && firstFailure == nil {
			firstFailure = err
		}
	}
	return firstFailure
}

func selectGitHistory(runner GitCommandRunner, repository string, mode GitHistoryMode, headCommit string) (gitHistorySelection, error) {
	switch mode := mode.(type) {
	case GitHistoryModeCloneFrom:
		panic("unreachable: clone history has a separate ref collector")
	case GitHistoryModeSessionDelta:
		// Collection stays side-effect free; callers repair missing origin
		// refs before asking for a session delta.
		hasOrigin, err := HasOriginRefs(runner, repository)
		if err != nil {
			return gitHistorySelection{}, err
		}
		if !hasOrigin {
			return gitHistorySelection{}, errors.New("repository has no origin refs to delta against")
		}
		counted, err := gitText(runner, repository, []string{"rev-list", "--count", "HEAD", "--not", "--remotes=origin"})
		if err != nil {
			return gitHistorySelection{}, err
		}
		count, err := gitParseUint(counted, 64)
		if err != nil {
			return gitHistorySelection{}, fmt.Errorf("parse committed delta count: %w", err)
		}
		var arguments []string
		if count > 0 {
			arguments = []string{"bundle", "create", "-", "HEAD", "--not", "--remotes=origin"}
		}
		return gitHistorySelection{bundleArguments: arguments}, nil
	case GitHistoryModeDeltaFrom:
		baseCommit, err := mergeBase(runner, repository, string(mode))
		if err != nil {
			return gitHistorySelection{}, err
		}
		if baseCommit == nil {
			return gitHistorySelection{}, fmt.Errorf("delta base %s is unresolvable", string(mode))
		}
		counted, err := gitText(runner, repository, []string{"rev-list", "--count", *baseCommit + "..HEAD"})
		if err != nil {
			return gitHistorySelection{}, err
		}
		count, err := gitParseUint(counted, 64)
		if err != nil {
			return gitHistorySelection{}, fmt.Errorf("parse committed delta count: %w", err)
		}
		var arguments []string
		if count > 0 {
			arguments = []string{"bundle", "create", "-", "HEAD", "^" + *baseCommit}
		}
		return gitHistorySelection{baseCommit: *baseCommit, bundleArguments: arguments}, nil
	case GitHistoryModeNoBundle:
		return gitHistorySelection{baseCommit: headCommit}, nil
	default:
		return gitHistorySelection{}, fmt.Errorf("unknown Git history mode %T", mode)
	}
}

// patchCommand leaves the unmerged paths out of a patch. Git describes an
// unmerged path as "Unmerged path" or as a combined diff, neither of which
// `git apply` accepts. The operation capture carries those paths, so both
// patches leave them out and keep everything else, such as the files a merge
// staged cleanly.
func patchCommand(arguments []string, unmergedPaths []string) GitCommand {
	command := GitCommand{Arguments: slices.Clone(arguments)}
	if len(unmergedPaths) > 0 {
		command.Arguments = append(command.Arguments, "--")
	}
	for _, path := range unmergedPaths {
		command.Arguments = append(command.Arguments, ":(exclude,literal)"+path)
	}
	return command
}

func collectStagedPatch(runner GitCommandRunner, repository string, unmergedPaths []string) ([]byte, error) {
	command := patchCommand([]string{"diff", "--binary", "--cached", "--no-ext-diff"}, unmergedPaths)
	return gitSuccess(runner, repository, &command, "collect staged Git patch")
}

func collectUnstagedPatch(runner GitCommandRunner, repository string, unmergedPaths []string) ([]byte, error) {
	command := patchCommand([]string{"diff", "--binary", "--no-ext-diff"}, unmergedPaths)
	return gitSuccess(runner, repository, &command, "collect unstaged Git patch")
}

func collectUntrackedTar(
	runner GitCommandRunner,
	repository string,
	untrackedFiles UntrackedFiles,
	linkedWorktrees map[string]struct{},
	progress func(GitSnapshotProgress) error,
) ([]byte, error) {
	if untrackedFiles == UntrackedFilesExclude {
		return nil, nil
	}
	untracked, err := gitBytes(runner, repository,
		[]string{"ls-files", "--others", "--exclude-standard", "-z"}, nil, "list nonignored untracked files")
	if err != nil {
		return nil, err
	}
	return buildUntrackedTar(repository, untracked, untrackedFiles, linkedWorktrees, progress)
}

func collectGitContents(
	runner GitCommandRunner,
	checkout *checkoutLocation,
	spec *GitCollectionSpec,
	identity collectedGitIdentity,
	history gitHistorySelection,
	unmergedPaths []string,
	untrackedFiles UntrackedFiles,
	linkedWorktrees map[string]struct{},
	progress func(GitSnapshotProgress) error,
) (RepositorySnapshot, error) {
	repository := checkout.topLevel
	// These commands only inspect repository state and produce independent
	// payloads, so they run at once, including when several repositories are
	// being collected at once.
	var committedBundle, stagedPatch, unstagedPatch, untrackedTar []byte
	var bundleErr, stagedErr, unstagedErr, untrackedErr error
	var group sync.WaitGroup
	group.Go(func() {
		if history.bundleArguments != nil {
			committedBundle, bundleErr = gitBytesOwned(runner, repository, history.bundleArguments,
				"create committed delta bundle")
		}
	})
	group.Go(func() { stagedPatch, stagedErr = collectStagedPatch(runner, repository, unmergedPaths) })
	group.Go(func() { unstagedPatch, unstagedErr = collectUnstagedPatch(runner, repository, unmergedPaths) })
	group.Go(func() {
		untrackedTar, untrackedErr = collectUntrackedTar(runner, repository, untrackedFiles, linkedWorktrees, progress)
	})
	group.Wait()
	for _, err := range []error{bundleErr, stagedErr, unstagedErr, untrackedErr} {
		if err != nil {
			return RepositorySnapshot{}, err
		}
	}

	return RepositorySnapshot{
		Metadata: core.RepositoryMetadata{
			ID:                   spec.ID,
			RelativeDestination:  spec.RelativeDestination,
			CheckoutSubdirectory: checkout.subdirectory,
			Origin:               identity.origin,
			PushURLs:             identity.pushURLs,
			BaseCommit:           history.baseCommit,
			HeadCommit:           identity.headCommit,
			Branch:               identity.branch,
		},
		CommittedBundle: committedBundle,
		StagedPatch:     stagedPatch,
		UnstagedPatch:   unstagedPatch,
		UntrackedTar:    untrackedTar,
	}, nil
}

// linkedWorktreeDirectory is a linked worktree of the captured repository
// that lives inside the captured checkout.
type linkedWorktreeDirectory struct {
	// relative is the directory below the checkout's top level.
	relative string
	// directory is the directory, with symbolic links resolved.
	directory string
}

// preparedWorktree is a linked worktree whose state has been read and whose
// commits are held by temporary refs.
type preparedWorktree struct {
	location   linkedWorktreeDirectory
	headCommit string
	branch     *string
	lockReason *string
	operation  *collectedOperation
}

// unquoteGitPath decodes a Git C-quoted path, including octal-escaped native
// bytes.
func unquoteGitPath(path []byte) (string, error) {
	quoted, ok := bytes.CutPrefix(path, []byte(`"`))
	if !ok {
		return core.FromGitBytes(path)
	}
	quoted, ok = bytes.CutSuffix(quoted, []byte(`"`))
	if !ok {
		return "", errors.New("unterminated quoted worktree path")
	}
	var decoded []byte
	for len(quoted) > 0 {
		b := quoted[0]
		quoted = quoted[1:]
		if b != '\\' {
			decoded = append(decoded, b)
			continue
		}
		if len(quoted) == 0 {
			return "", errors.New("incomplete worktree path escape")
		}
		escape := quoted[0]
		quoted = quoted[1:]
		switch {
		case escape == 'a':
			decoded = append(decoded, 7)
		case escape == 'b':
			decoded = append(decoded, 8)
		case escape == 't':
			decoded = append(decoded, '\t')
		case escape == 'n':
			decoded = append(decoded, '\n')
		case escape == 'v':
			decoded = append(decoded, 11)
		case escape == 'f':
			decoded = append(decoded, 12)
		case escape == 'r':
			decoded = append(decoded, '\r')
		case escape == '\\' || escape == '"':
			decoded = append(decoded, escape)
		case escape >= '0' && escape <= '7':
			if len(quoted) < 2 || quoted[0] < '0' || quoted[0] > '7' || quoted[1] < '0' || quoted[1] > '7' {
				return "", errors.New("invalid octal worktree path escape")
			}
			value := uint16(escape-'0')*64 + uint16(quoted[0]-'0')*8 + uint16(quoted[1]-'0')
			quoted = quoted[2:]
			if value > 0xff {
				return "", errors.New("worktree path octal escape exceeds one byte")
			}
			decoded = append(decoded, byte(value))
		default:
			return "", errors.New("invalid worktree path escape")
		}
	}
	return core.FromGitBytes(decoded)
}

// registeredWorktreePaths reads registered checkout paths without pruning
// them. Old-Git path checks apply only inside the requested checkout.
func registeredWorktreePaths(runner GitCommandRunner, repository string) ([]string, error) {
	topLevel, err := gitCanonicalize(repository)
	if err != nil {
		return nil, fmt.Errorf("resolve %s: %w", repository, err)
	}
	listed, err := runGit(runner, repository, []string{"worktree", "list", "--porcelain", "-z"}, nil)
	if err != nil {
		return nil, err
	}
	const newlineError = "Git cannot list newline worktree paths; upgrade Git to 2.36 or newer"
	delimiter := byte(0)
	if listed.Status == 129 {
		// Older target Git rejects -z. Its line format cannot represent a
		// newline path, so inspect registration paths before interpreting
		// it.
		if strings.Contains(topLevel, "\n") {
			return nil, errors.New(newlineError)
		}
		_, commonDirectory, err := gitDirectories(runner, repository)
		if err != nil {
			return nil, err
		}
		entries, err := os.ReadDir(filepath.Join(commonDirectory, "worktrees"))
		switch {
		case err == nil:
			for _, entry := range entries {
				if !entry.IsDir() {
					continue
				}
				raw, err := os.ReadFile(filepath.Join(commonDirectory, "worktrees", entry.Name(), "gitdir"))
				if errors.Is(err, fs.ErrNotExist) {
					continue
				}
				if err != nil {
					return nil, fmt.Errorf("read legacy worktree registration: %w", err)
				}
				raw = bytes.TrimSuffix(raw, []byte("\n"))
				registered, err := core.FromGitBytes(raw)
				if err != nil {
					return nil, err
				}
				checkout, ok := checkpointParent(registered)
				if !ok {
					return nil, errors.New("worktree registration has no checkout")
				}
				if !archivePathStartsWith(checkout, repository) && !archivePathStartsWith(checkout, topLevel) {
					continue
				}
				if bytes.Contains(raw, []byte("\n")) {
					return nil, errors.New(newlineError)
				}
			}
		case errors.Is(err, fs.ErrNotExist):
		default:
			return nil, fmt.Errorf("inspect legacy worktree registrations: %w", err)
		}
		if listed, err = runGit(runner, repository, []string{"worktree", "list", "--porcelain"}, nil); err != nil {
			return nil, err
		}
		delimiter = '\n'
	}
	if listed.Status != 0 {
		return nil, gitFailure("list linked worktrees", &listed)
	}
	var paths []string
	for _, line := range bytes.Split(listed.Stdout, []byte{delimiter}) {
		raw, ok := bytes.CutPrefix(line, []byte("worktree "))
		if !ok {
			continue
		}
		var path string
		if delimiter == '\n' {
			path, err = unquoteGitPath(raw)
		} else {
			path, err = core.FromGitBytes(raw)
		}
		if err != nil {
			return nil, err
		}
		paths = append(paths, path)
	}
	return paths, nil
}

// discoverLinkedWorktrees returns the linked worktrees of `repository`'s
// checkout that live inside it, at any depth, parents before children.
//
// A worktree is "ours" by Git's own definition of the same repository: the
// common Git directory that `git rev-parse` names inside it is the
// checkout's. A registered worktree outside the checkout, or whose directory
// is gone, holds nothing this checkout's session owns and is left out; this
// never prunes a registration.
func discoverLinkedWorktrees(runner GitCommandRunner, repository string) ([]linkedWorktreeDirectory, error) {
	topLevel, err := gitCanonicalize(repository)
	if err != nil {
		return nil, fmt.Errorf("resolve %s: %w", repository, err)
	}
	_, commonDirectory, err := gitDirectories(runner, repository)
	if err != nil {
		return nil, err
	}
	registered, err := registeredWorktreePaths(runner, repository)
	if err != nil {
		return nil, err
	}
	var found []linkedWorktreeDirectory
	for _, path := range registered {
		// `worktree list` names the user's worktrees anywhere on disk. Only a
		// path that can lie inside the checkout may fail the capture, so
		// decide that lexically, against the top level as given and resolved.
		if !archivePathStartsWith(path, repository) && !archivePathStartsWith(path, topLevel) {
			continue
		}
		directory, err := gitCanonicalize(path)
		if errors.Is(err, fs.ErrNotExist) {
			// Registered, but the directory is gone: nothing to capture.
			continue
		}
		if err != nil {
			return nil, fmt.Errorf("resolve %s: %w", path, err)
		}
		relative, ok := gitStripPrefix(directory, topLevel)
		if !ok {
			continue
		}
		if relative == "" {
			continue
		}
		if info, err := os.Stat(filepath.Join(directory, ".git")); err != nil || !info.Mode().IsRegular() {
			continue
		}
		if err := validateWorktreeRelativePath(relative); err != nil {
			return nil, err
		}
		// A directory Git no longer reads as a repository is not one of ours.
		_, common, err := gitDirectories(runner, directory)
		if err != nil {
			continue
		}
		same, err := sameDirectory(common, commonDirectory)
		if err != nil {
			return nil, err
		}
		if !same {
			continue
		}
		found = append(found, linkedWorktreeDirectory{relative: relative, directory: directory})
	}
	slices.SortStableFunc(found, func(left, right linkedWorktreeDirectory) int {
		return gitComparePaths(left.relative, right.relative)
	})
	return found, nil
}

// linkedWorktreeDirectories returns the absolute directories of the
// checkout's linked worktrees, for checks that must run in each of them.
func linkedWorktreeDirectories(runner GitCommandRunner, repository string) ([]string, error) {
	checkout, err := locateCheckout(runner, repository)
	if err != nil {
		return nil, err
	}
	worktrees, err := discoverLinkedWorktrees(runner, checkout.topLevel)
	if err != nil {
		return nil, err
	}
	directories := make([]string, 0, len(worktrees))
	for _, worktree := range worktrees {
		directories = append(directories, worktree.directory)
	}
	return directories, nil
}

// prepareLinkedWorktrees reads each worktree's HEAD, lock and (for a clone)
// operation, and makes temporary refs so the committed bundle carries every
// worktree's commits, including a detached HEAD that no branch holds. Each
// ref is added to `temporaryRefs` as soon as it exists, so the caller
// removes it on every path.
func prepareLinkedWorktrees(
	runner GitCommandRunner,
	worktrees []linkedWorktreeDirectory,
	captureOperations bool,
	temporaryRefs *[]string,
) ([]preparedWorktree, error) {
	if len(worktrees) == 0 {
		return nil, nil
	}
	captureID, err := core.NewSessionID()
	if err != nil {
		return nil, err
	}
	prepared := make([]preparedWorktree, 0, len(worktrees))
	for index, location := range worktrees {
		directory := location.directory
		name := location.relative
		describe := func(action string) string { return fmt.Sprintf("%s of linked worktree %s", action, name) }
		headCommit, err := gitText(runner, directory, []string{"rev-parse", "--verify", "HEAD^{commit}"})
		if err != nil {
			return nil, fmt.Errorf("%s: %w", describe("read the HEAD"), err)
		}
		symbolic, err := runGit(runner, directory, []string{"symbolic-ref", "--quiet", "HEAD"}, nil)
		if err != nil {
			return nil, err
		}
		var branch *string
		switch symbolic.Status {
		case 0:
			reference, err := trimOutput(symbolic.Stdout, "decode worktree branch")
			if err != nil {
				return nil, err
			}
			short, ok := strings.CutPrefix(reference, "refs/heads/")
			if !ok {
				return nil, fmt.Errorf("linked worktree %s has HEAD at %s, not a branch", name, reference)
			}
			branch = &short
		case 1:
		default:
			return nil, gitFailure("read the branch of a linked worktree", &symbolic)
		}
		locked, err := gitPaths(runner, directory, []string{"locked"})
		if err != nil {
			return nil, fmt.Errorf("%s: %w", describe("find the lock"), err)
		}
		var lockReason *string
		reason, err := os.ReadFile(locked[0][1])
		switch {
		case err == nil:
			text, err := gitDecodeUTF8(reason)
			if err != nil {
				return nil, fmt.Errorf("%s: %w", describe("decode the lock reason"), err)
			}
			text = strings.TrimRight(text, "\n")
			lockReason = &text
		case errors.Is(err, fs.ErrNotExist):
		default:
			return nil, fmt.Errorf("%s: %w", describe("read the lock"), err)
		}
		pin := fmt.Sprintf("refs/mj-checkpoint/%s/worktree/%08d", captureID, index)
		if _, err := gitBytes(runner, directory, []string{"update-ref", pin, headCommit}, nil,
			"preserve a linked worktree HEAD for the checkpoint bundle"); err != nil {
			return nil, err
		}
		*temporaryRefs = append(*temporaryRefs, pin)
		var operation *collectedOperation
		if captureOperations {
			operation, err = operationCollect(runner, directory)
			if err != nil {
				return nil, fmt.Errorf("%s: %w", describe("capture the Git operation"), err)
			}
			if operation != nil {
				if err := operation.carryObjects(runner, directory,
					fmt.Sprintf("refs/mj-checkpoint/%s/worktree-operation/%08d", captureID, index),
					temporaryRefs); err != nil {
					return nil, err
				}
			}
		}
		prepared = append(prepared, preparedWorktree{
			location:   location,
			headCommit: headCommit,
			branch:     branch,
			lockReason: lockReason,
			operation:  operation,
		})
	}
	return prepared, nil
}

// includeWorktreeHistory adds every worktree's temporary refs to the
// committed bundle as tips, with the same exclusions the checkout's own
// history uses.
func includeWorktreeHistory(
	runner GitCommandRunner,
	repository string,
	mode GitHistoryMode,
	history *gitHistorySelection,
	worktrees []preparedWorktree,
	tips []string,
) error {
	if len(worktrees) == 0 {
		return nil
	}
	var exclusions []string
	switch mode.(type) {
	case GitHistoryModeSessionDelta:
		exclusions = []string{"--not", "--remotes=origin"}
	case GitHistoryModeCloneFrom, GitHistoryModeDeltaFrom:
		exclusions = []string{"^" + history.baseCommit}
	case GitHistoryModeNoBundle:
		// Origin serves all committed history.
		return nil
	default:
		return fmt.Errorf("unknown Git history mode %T", mode)
	}
	if history.bundleArguments != nil {
		// Before any exclusion: these are positive tips.
		history.bundleArguments = slices.Insert(slices.Clip(history.bundleArguments), 3, tips...)
		return nil
	}
	countArguments := append([]string{"rev-list", "--count"}, tips...)
	countArguments = append(countArguments, exclusions...)
	counted, err := gitBytesOwned(runner, repository, countArguments, "count linked worktree commits")
	if err != nil {
		return err
	}
	countText, err := gitDecodeUTF8(counted)
	if err != nil {
		return err
	}
	count, err := gitParseUint(strings.TrimSpace(countText), 64)
	if err != nil {
		return fmt.Errorf("count linked worktree commits: %w", err)
	}
	carriesTags := slices.ContainsFunc(worktrees, func(worktree preparedWorktree) bool {
		return worktree.operation != nil && worktree.operation.carriesTags()
	})
	if count > 0 || carriesTags {
		arguments := append([]string{"bundle", "create", "-"}, tips...)
		history.bundleArguments = append(arguments, exclusions...)
	}
	return nil
}

func collectLinkedWorktreeContents(
	runner GitCommandRunner,
	worktrees []preparedWorktree,
	untrackedFiles UntrackedFiles,
	linkedWorktrees map[string]struct{},
	progress func(GitSnapshotProgress) error,
) ([]LinkedWorktreeSnapshot, error) {
	if len(worktrees) == 0 {
		return nil, nil
	}
	snapshots := make([]LinkedWorktreeSnapshot, len(worktrees))
	failures := make([]error, len(worktrees))
	var group sync.WaitGroup
	for index := range worktrees {
		group.Go(func() {
			snapshots[index], failures[index] = collectLinkedWorktreeContent(runner, &worktrees[index],
				untrackedFiles, linkedWorktrees, progress)
		})
	}
	group.Wait()
	for _, err := range failures {
		if err != nil {
			return nil, err
		}
	}
	return snapshots, nil
}

func collectLinkedWorktreeContent(
	runner GitCommandRunner,
	worktree *preparedWorktree,
	untrackedFiles UntrackedFiles,
	linkedWorktrees map[string]struct{},
	progress func(GitSnapshotProgress) error,
) (LinkedWorktreeSnapshot, error) {
	directory := worktree.location.directory
	var unmergedPaths []string
	if worktree.operation != nil {
		unmergedPaths = worktree.operation.unmergedPaths()
	}
	relative := worktree.location.relative
	worktreeProgress := func(event GitSnapshotProgress) error {
		if file, ok := event.(GitSnapshotProgressUntrackedFile); ok {
			file.Path = gitJoin(relative, file.Path)
			return progress(file)
		}
		return progress(event)
	}
	var stagedPatch, unstagedPatch, untrackedTar []byte
	var stagedErr, unstagedErr, untrackedErr error
	var group sync.WaitGroup
	group.Go(func() { stagedPatch, stagedErr = collectStagedPatch(runner, directory, unmergedPaths) })
	group.Go(func() { unstagedPatch, unstagedErr = collectUnstagedPatch(runner, directory, unmergedPaths) })
	group.Go(func() {
		untrackedTar, untrackedErr = collectUntrackedTar(runner, directory, untrackedFiles, linkedWorktrees,
			worktreeProgress)
	})
	group.Wait()
	var gitOperation *core.SavedGitOperation
	var operationPayloads *GitOperationPayloads
	if operation := worktree.operation; operation != nil {
		metadata, payloads := operation.metadata, operation.payloads
		gitOperation, operationPayloads = &metadata, &payloads
	}
	for _, err := range []error{stagedErr, unstagedErr, untrackedErr} {
		if err != nil {
			return LinkedWorktreeSnapshot{}, fmt.Errorf("linked worktree %s: %w", relative, err)
		}
	}
	return LinkedWorktreeSnapshot{
		Metadata: core.LinkedWorktreeMetadata{
			Path:         relative,
			HeadCommit:   worktree.headCommit,
			Branch:       worktree.branch,
			LockReason:   worktree.lockReason,
			GitOperation: gitOperation,
		},
		StagedPatch:       stagedPatch,
		UnstagedPatch:     unstagedPatch,
		UntrackedTar:      untrackedTar,
		OperationPayloads: operationPayloads,
	}, nil
}

// relocatedReviewRefs keeps every archived ref except old baseline pins,
// which are not restored, and publishes that session's start under the
// destination checkout's name as well. A worktree name changes when a
// session moves into another checkout.
func relocatedReviewRefs(
	runner GitCommandRunner,
	repository string,
	metadata *core.RepositoryMetadata,
	session *string,
) (map[string]string, error) {
	refs := map[string]string{}
	for _, name := range gitSortedKeys(metadata.SavedRefs) {
		oid := metadata.SavedRefs[name]
		if core.IsReviewBaselinePin(name) {
			continue
		}
		if metadata.ReviewRefsMetadataOnly {
			object, err := runGit(runner, repository, []string{"cat-file", "-e", oid}, nil)
			if err != nil {
				return nil, err
			}
			if object.Status != 0 {
				slog.Debug("skip raw review ref whose object is unavailable",
					"reference", name, "object", oid, "repository", repository)
				continue
			}
		}
		refs[name] = oid
	}
	restored := maps.Clone(refs)
	if metadata.ReviewBaselineRef == nil {
		return restored, nil
	}
	source := *metadata.ReviewBaselineRef
	destination, err := SessionStartScope(runner, repository)
	if err != nil {
		return nil, err
	}
	if base, ok := refs[PushReviewBaseRef(source)]; ok {
		restored[PushReviewBaseRef(destination)] = base
	}
	if session == nil {
		return restored, nil
	}
	prefix := fmt.Sprintf("refs/mj/session-start/%s/", digestBytes([]byte(*session)))
	for _, candidate := range []string{source, strings.Replace(source, "refs/mj/", "refs/hel/", 1)} {
		if start, ok := refs[prefix+digestBytes([]byte(candidate))]; ok {
			restored[prefix+digestBytes([]byte(destination))] = start
			break
		}
	}
	return restored, nil
}

// RestoreGitSnapshot restores a snapshot into `repository`, the directory
// its paths are named from. For a snapshot that records a
// `checkout_subdirectory` that is the destination checkout's top level: its
// untracked files are named from there, and `git apply` run from a
// subdirectory skips every path outside it.
func RestoreGitSnapshot(runner GitCommandRunner, repository string, snapshot *RepositorySnapshot) error {
	_, err := restoreGitSnapshotForSession(runner, repository, snapshot, nil, true)
	return err
}

// LegacyRestoreOptions adjust how a legacy (non-origin-delta) snapshot is
// restored.
type LegacyRestoreOptions struct {
	// DetachOnBranchCollision checks out a detached HEAD, with a notice,
	// when the saved branch already exists at a different commit, instead
	// of moving that branch.
	DetachOnBranchCollision bool
}

// restoreGitSnapshotForSession restores a snapshot and returns the resume
// notices describing anything it could not give back as captured.
func restoreGitSnapshotForSession(
	runner GitCommandRunner,
	repository string,
	snapshot *RepositorySnapshot,
	session *string,
	restoreContents bool,
) ([]string, error) {
	return restoreGitSnapshotForSessionWithOptions(runner, repository, snapshot, session, restoreContents,
		LegacyRestoreOptions{})
}

func restoreGitSnapshotForSessionWithOptions(
	runner GitCommandRunner,
	repository string,
	snapshot *RepositorySnapshot,
	session *string,
	restoreContents bool,
	options LegacyRestoreOptions,
) ([]string, error) {
	notices := []string{}
	metadata := &snapshot.Metadata
	// Raw snapshots restore ref metadata only, even when the caller asks to
	// restore repositories. Other sessions may be using this checkout.
	restoreContents = restoreContents && !metadata.ReviewRefsMetadataOnly
	carriesMjolnirRefs := false
	for name := range metadata.SavedRefs {
		if strings.HasPrefix(name, core.MjolnirRefNamespace) {
			carriesMjolnirRefs = true
			break
		}
	}
	if !restoreContents && !carriesMjolnirRefs {
		return notices, nil
	}
	if metadata.RemoteWorkspace {
		if err := core.ValidateNetworkURL(metadata.Origin); err != nil {
			return nil, err
		}
		for _, url := range metadata.PushURLs {
			if err := core.ValidateNetworkURL(url); err != nil {
				return nil, err
			}
		}
	}
	if restoreContents && metadata.Branch != nil {
		if err := ensureBranchAvailableForCheckout(runner, repository, *metadata.Branch); err != nil {
			return nil, err
		}
	}
	operation, payloads := metadata.GitOperation, snapshot.OperationPayloads
	if (operation == nil) != (payloads == nil) {
		return nil, errors.New("the snapshot's Git operation metadata and payloads disagree")
	}
	// A bundle fetched through HEAD cannot carry a linked worktree's commits,
	// and it may have no HEAD of its own, so such a bundle is only unpacked
	// and the checkout names its commit.
	importsWithoutHead := len(metadata.SavedRefs) > 0 || len(metadata.StashStack) > 0 ||
		len(snapshot.LinkedWorktrees) > 0
	if len(snapshot.CommittedBundle) > 0 {
		if err := importCommittedBundle(runner, repository, snapshot.CommittedBundle, importsWithoutHead); err != nil {
			return nil, err
		}
	}
	savedRefs, err := relocatedReviewRefs(runner, repository, metadata, session)
	if err != nil {
		return nil, err
	}
	cloneRefs := len(metadata.SavedRefs) > 0 || len(metadata.StashStack) > 0
	var preservedCollidingBranch *string
	if restoreContents {
		checkoutTarget := "FETCH_HEAD"
		if len(snapshot.CommittedBundle) == 0 || importsWithoutHead {
			checkoutTarget = metadata.HeadCommit
		}
		if metadata.Branch != nil {
			branch := *metadata.Branch
			if _, err := gitBytes(runner, repository, []string{"check-ref-format", "--branch", branch}, nil,
				"validate restored branch"); err != nil {
				return nil, err
			}
			detachForCollision := false
			if options.DetachOnBranchCollision {
				existing, err := runGit(runner, repository,
					[]string{"rev-parse", "--verify", "--quiet", "refs/heads/" + branch + "^{commit}"}, nil)
				if err != nil {
					return nil, err
				}
				switch existing.Status {
				case 0:
					existingCommit, err := trimOutput(existing.Stdout, "read existing branch")
					if err != nil {
						return nil, err
					}
					resolved, err := gitBytes(runner, repository,
						[]string{"rev-parse", "--verify", checkoutTarget + "^{commit}"}, nil, "resolve restored commit")
					if err != nil {
						return nil, fmt.Errorf("%s: %w", checkoutAdvice(snapshot), err)
					}
					targetCommit, err := trimOutput(resolved, "resolve restored commit")
					if err != nil {
						return nil, err
					}
					if existingCommit != targetCommit {
						detachForCollision = true
						preserved := "refs/heads/" + branch
						preservedCollidingBranch = &preserved
						notices = append(notices, fmt.Sprintf(
							"Checked out detached HEAD at %s because branch '%s' already exists at %s in the fresh clone.",
							targetCommit, branch, existingCommit))
					}
				case 1:
				default:
					return nil, gitFailure("read existing branch", &existing)
				}
			}
			if detachForCollision {
				if _, err := gitBytes(runner, repository, []string{"checkout", "--detach", checkoutTarget}, nil,
					"restore detached commit on branch collision"); err != nil {
					return nil, fmt.Errorf("%s: %w", checkoutAdvice(snapshot), err)
				}
			} else if _, err := gitBytes(runner, repository, []string{"checkout", "-B", branch, checkoutTarget}, nil,
				"restore committed branch"); err != nil {
				return nil, fmt.Errorf("%s: %w", checkoutAdvice(snapshot), err)
			}
		} else {
			if _, err := gitBytes(runner, repository, []string{"checkout", "--detach", checkoutTarget}, nil,
				"restore detached commit"); err != nil {
				return nil, fmt.Errorf("%s: %w", checkoutAdvice(snapshot), err)
			}
		}
	}
	// Check out before moving saved refs: changing the currently checked-out
	// branch first leaves the old index and worktree staged against its new
	// tip.
	if cloneRefs {
		for _, name := range gitSortedKeys(savedRefs) {
			oid := savedRefs[name]
			mjolnirRef := strings.HasPrefix(name, core.MjolnirRefNamespace)
			if !restoreContents && !mjolnirRef {
				continue
			}
			if preservedCollidingBranch != nil && *preservedCollidingBranch == name {
				continue
			}
			operationRef := operation != nil && slices.ContainsFunc(operationRefNamespaces[:], func(namespace string) bool {
				return strings.HasPrefix(name, namespace)
			})
			if operationRef {
				continue
			}
			if !strings.HasPrefix(name, "refs/heads/") && !strings.HasPrefix(name, "refs/tags/") &&
				!strings.HasPrefix(name, "refs/notes/") && !mjolnirRef {
				return nil, fmt.Errorf("saved Git ref has an unsupported namespace: %s", name)
			}
			if _, err := gitBytes(runner, repository, []string{"check-ref-format", name}, nil,
				"validate saved Git ref"); err != nil {
				return nil, err
			}
			if _, err := gitBytes(runner, repository, []string{"cat-file", "-e", oid}, nil,
				"verify saved Git ref object"); err != nil {
				return nil, err
			}
			if !mjolnirRef {
				if _, err := gitBytes(runner, repository, []string{"update-ref", name, oid}, nil,
					"restore saved Git ref"); err != nil {
					return nil, err
				}
				continue
			}
			existing, err := runGit(runner, repository, []string{"rev-parse", "--verify", "--quiet", name}, nil)
			if err != nil {
				return nil, err
			}
			old := ""
			if existing.Status == 0 {
				if old, err = trimOutput(existing.Stdout, "read destination review ref"); err != nil {
					return nil, err
				}
			}
			if old != "" {
				if strings.HasPrefix(name, "refs/mj/reviewed/") || strings.HasPrefix(name, "refs/mj/overridden/") {
					continue
				}
				ownedStart := session != nil && strings.HasPrefix(name,
					fmt.Sprintf("refs/mj/session-start/%s/", digestBytes([]byte(*session))))
				if !ownedStart {
					continue
				}
			}
			if _, err := gitBytes(runner, repository, []string{"update-ref", name, oid, old}, nil,
				"restore durable review ref"); err != nil {
				return nil, err
			}
		}
		if restoreContents {
			for index := len(metadata.StashStack) - 1; index >= 0; index-- {
				entry := metadata.StashStack[index]
				if _, err := gitBytes(runner, repository, []string{"cat-file", "-e", entry.Commit}, nil,
					"verify saved stash commit"); err != nil {
					return nil, err
				}
				if _, err := gitBytes(runner, repository,
					[]string{"update-ref", "--create-reflog", "-m", entry.Message, "refs/stash", entry.Commit}, nil,
					"restore saved stash entry"); err != nil {
					return nil, err
				}
			}
		}
	}
	if !restoreContents {
		return notices, nil
	}
	if err := restoreRemoteWorkspaceConfiguration(runner, repository, metadata); err != nil {
		return nil, err
	}
	if len(snapshot.StagedPatch) > 0 {
		if _, err := gitBytes(runner, repository, []string{"apply", "--binary", "--index", "-"}, snapshot.StagedPatch,
			"restore staged Git patch"); err != nil {
			return nil, err
		}
	}
	if len(snapshot.UnstagedPatch) > 0 {
		if _, err := gitBytes(runner, repository, []string{"apply", "--binary", "-"}, snapshot.UnstagedPatch,
			"restore unstaged Git patch"); err != nil {
			return nil, err
		}
	}
	if err := restoreUntrackedTar(repository, snapshot.UntrackedTar); err != nil {
		return nil, err
	}
	if operation != nil {
		notice, err := operationRestore(repository, operation, payloads)
		if err != nil {
			return nil, err
		}
		notices = append(notices, notice)
	}
	// After the checkout is whole: a worktree cannot take a branch the
	// checkout holds, and its directory must not exist when it is added.
	worktreeNotices, err := restoreLinkedWorktrees(runner, repository, snapshot.LinkedWorktrees)
	if err != nil {
		return nil, err
	}
	return append(notices, worktreeNotices...), nil
}

// importCommittedBundle writes the bundle to a temporary file in the
// repository and imports it: unpacked when the checkout names its commit
// itself, fetched through HEAD otherwise. The file is removed on every path.
func importCommittedBundle(runner GitCommandRunner, repository string, data []byte, importsWithoutHead bool) error {
	bundle, err := os.CreateTemp(repository, ".tmp*")
	if err != nil {
		return fmt.Errorf("create temporary Git bundle in %s: %w", repository, err)
	}
	path := bundle.Name()
	defer os.Remove(path)
	if _, err := bundle.Write(data); err != nil {
		_ = bundle.Close()
		return fmt.Errorf("write temporary Git bundle: %w", err)
	}
	if err := bundle.Close(); err != nil {
		return fmt.Errorf("flush temporary Git bundle: %w", err)
	}
	arguments := []string{"fetch", path, "HEAD"}
	if importsWithoutHead {
		arguments = []string{"bundle", "unbundle", path}
	}
	_, err = gitSuccess(runner, repository, &GitCommand{Arguments: arguments}, "import committed delta bundle")
	return err
}

// restoreLinkedWorktrees recreates the checkout's linked worktrees, parents
// before children, each with the HEAD, index, files, operation and lock it
// was captured with.
func restoreLinkedWorktrees(runner GitCommandRunner, repository string, worktrees []LinkedWorktreeSnapshot) ([]string, error) {
	notices := []string{}
	ordered := make([]*LinkedWorktreeSnapshot, len(worktrees))
	for index := range worktrees {
		ordered[index] = &worktrees[index]
	}
	depth := func(worktree *LinkedWorktreeSnapshot) int {
		return len(archivePathComponents(worktree.Metadata.Path))
	}
	slices.SortStableFunc(ordered, func(left, right *LinkedWorktreeSnapshot) int {
		if order := cmp.Compare(depth(left), depth(right)); order != 0 {
			return order
		}
		return gitComparePaths(left.Metadata.Path, right.Metadata.Path)
	})
	for _, worktree := range ordered {
		worktreeNotices, err := restoreLinkedWorktree(runner, repository, worktree)
		if err != nil {
			return nil, fmt.Errorf("restore linked worktree %s: %w", worktree.Metadata.Path, err)
		}
		notices = append(notices, worktreeNotices...)
	}
	return notices, nil
}

// listedWorktree is one entry of `git worktree list --porcelain`.
type listedWorktree struct {
	path   string
	branch *string
	locked bool
}

func listWorktrees(runner GitCommandRunner, repository string) ([]listedWorktree, error) {
	listed, err := gitBytes(runner, repository, []string{"worktree", "list", "--porcelain"}, nil, "list restore worktrees")
	if err != nil {
		return nil, err
	}
	var worktrees []listedWorktree
	for _, line := range bytes.Split(listed, []byte("\n")) {
		if raw, ok := bytes.CutPrefix(line, []byte("worktree ")); ok {
			path, err := core.FromGitBytes(raw)
			if err != nil {
				return nil, err
			}
			worktrees = append(worktrees, listedWorktree{path: path})
			continue
		}
		if len(worktrees) == 0 {
			continue
		}
		worktree := &worktrees[len(worktrees)-1]
		if branch, ok := bytes.CutPrefix(line, []byte("branch refs/heads/")); ok {
			name, err := gitDecodeUTF8(branch)
			if err != nil {
				return nil, fmt.Errorf("decode a worktree branch name: %w", err)
			}
			worktree.branch = &name
		} else if string(line) == "locked" || bytes.HasPrefix(line, []byte("locked ")) {
			worktree.locked = true
		}
	}
	return worktrees, nil
}

func restoreLinkedWorktree(runner GitCommandRunner, repository string, worktree *LinkedWorktreeSnapshot) ([]string, error) {
	metadata := &worktree.Metadata
	if err := validateWorktreeRelativePath(metadata.Path); err != nil {
		return nil, err
	}
	if err := ensureNoSymlinkAncestors(repository, metadata.Path); err != nil {
		return nil, err
	}
	directory := gitJoin(repository, metadata.Path)
	if _, err := gitBytes(runner, repository, []string{"cat-file", "-e", metadata.HeadCommit + "^{commit}"}, nil,
		"verify the linked worktree's commit"); err != nil {
		return nil, err
	}
	_, err := os.Lstat(directory)
	switch {
	case err == nil:
		// A retry finds what an earlier attempt made; anything else there is
		// not ours to overwrite.
		restored, err := isRestoredLinkedWorktree(runner, repository, directory, metadata.HeadCommit)
		if err != nil {
			return nil, err
		}
		if !restored {
			return nil, fmt.Errorf("%s already exists and is not a worktree of this repository at %s",
				directory, metadata.HeadCommit)
		}
		return []string{}, nil
	case errors.Is(err, fs.ErrNotExist):
	default:
		return nil, fmt.Errorf("inspect %s: %w", directory, err)
	}

	canonicalRepository, err := gitCanonicalize(repository)
	if err != nil {
		return nil, fmt.Errorf("resolve %s: %w", repository, err)
	}
	target := gitJoin(canonicalRepository, metadata.Path)
	listed, err := listWorktrees(runner, repository)
	if err != nil {
		return nil, err
	}
	// A registration whose directory is gone is reused rather than refused.
	arguments := []string{"worktree", "add"}
	if stale := slices.IndexFunc(listed, func(entry listedWorktree) bool {
		return gitComparePaths(entry.path, target) == 0
	}); stale >= 0 {
		arguments = append(arguments, "-f")
		if listed[stale].locked {
			arguments = append(arguments, "-f")
		}
	}
	// branchArguments is nil for a detached worktree, empty for a new branch
	// made from the commit, and the branch name for an existing branch.
	var branchArguments []string
	detached := true
	if metadata.Branch != nil {
		branch := *metadata.Branch
		if _, err := gitBytes(runner, repository, []string{"check-ref-format", "--branch", branch}, nil,
			"validate the linked worktree's branch"); err != nil {
			return nil, err
		}
		current, err := runGit(runner, repository,
			[]string{"rev-parse", "--verify", "--quiet", "refs/heads/" + branch + "^{commit}"}, nil)
		if err != nil {
			return nil, err
		}
		var existing *string
		switch current.Status {
		case 0:
			commit, err := trimOutput(current.Stdout, "decode a worktree branch commit")
			if err != nil {
				return nil, err
			}
			existing = &commit
		case 1:
		default:
			return nil, gitFailure("read the linked worktree's branch", &current)
		}
		heldElsewhere := slices.ContainsFunc(listed, func(entry listedWorktree) bool {
			return gitComparePaths(entry.path, target) != 0 && entry.branch != nil && *entry.branch == branch
		})
		switch {
		case heldElsewhere:
			slog.Warn("linked worktree restored detached: its branch is checked out in another worktree",
				"worktree", metadata.Path, "branch", branch)
		case existing != nil && *existing != metadata.HeadCommit:
			slog.Warn("linked worktree restored detached: its branch moved since the checkpoint",
				"worktree", metadata.Path, "branch", branch)
		case existing != nil:
			detached, branchArguments = false, []string{branch}
		default:
			detached = false
			arguments = append(arguments, "-b", branch)
		}
	}
	arguments = append(arguments, directory)
	switch {
	case detached:
		arguments = append(arguments, "--detach", metadata.HeadCommit)
	case len(branchArguments) > 0:
		// An existing branch is named; a new one is made from the commit.
		arguments = append(arguments, branchArguments...)
	default:
		arguments = append(arguments, metadata.HeadCommit)
	}
	if _, err := gitSuccess(runner, repository, &GitCommand{Arguments: arguments}, "add the linked worktree"); err != nil {
		return nil, err
	}
	// All or nothing: a worktree left half-restored would be taken for a
	// finished one by a retry, which skips it and loses its dirty state.
	notices, err := restoreLinkedWorktreeState(runner, repository, directory, worktree)
	if err != nil {
		_, removal := gitSuccess(runner, repository, &GitCommand{
			Arguments: []string{"worktree", "remove", "--force", "--force", directory},
		}, "remove the partly restored linked worktree")
		if removal != nil {
			return nil, fmt.Errorf("also failed to clean up: %v: %w", removal, err)
		}
		return nil, err
	}
	return notices, nil
}

// restoreLinkedWorktreeState applies a linked worktree's dirty state,
// operation and lock to its fresh checkout of the recorded commit.
func restoreLinkedWorktreeState(
	runner GitCommandRunner,
	repository string,
	directory string,
	worktree *LinkedWorktreeSnapshot,
) ([]string, error) {
	notices := []string{}
	metadata := &worktree.Metadata
	if len(worktree.StagedPatch) > 0 {
		if _, err := gitBytes(runner, directory, []string{"apply", "--binary", "--index", "-"}, worktree.StagedPatch,
			"restore the linked worktree's staged Git patch"); err != nil {
			return nil, err
		}
	}
	if len(worktree.UnstagedPatch) > 0 {
		if _, err := gitBytes(runner, directory, []string{"apply", "--binary", "-"}, worktree.UnstagedPatch,
			"restore the linked worktree's unstaged Git patch"); err != nil {
			return nil, err
		}
	}
	if err := restoreUntrackedTar(directory, worktree.UntrackedTar); err != nil {
		return nil, err
	}
	switch operation, payloads := metadata.GitOperation, worktree.OperationPayloads; {
	case operation == nil && payloads == nil:
	case operation != nil && payloads != nil:
		notice, err := operationRestore(directory, operation, payloads)
		if err != nil {
			return nil, err
		}
		notices = append(notices, notice)
	default:
		return nil, errors.New("the linked worktree's Git operation metadata and payloads disagree")
	}
	if metadata.LockReason != nil {
		arguments := []string{"worktree", "lock"}
		if reason := *metadata.LockReason; reason != "" {
			arguments = append(arguments, "--reason", reason)
		}
		arguments = append(arguments, directory)
		if _, err := gitSuccess(runner, repository, &GitCommand{Arguments: arguments}, "lock the linked worktree"); err != nil {
			return nil, err
		}
	}
	return notices, nil
}

// isRestoredLinkedWorktree reports whether `directory` is already a worktree
// of `repository` at `headCommit`.
func isRestoredLinkedWorktree(runner GitCommandRunner, repository string, directory string, headCommit string) (bool, error) {
	_, common, err := gitDirectories(runner, directory)
	if err != nil {
		return false, nil
	}
	_, expected, err := gitDirectories(runner, repository)
	if err != nil {
		return false, err
	}
	same, err := sameDirectory(common, expected)
	if err != nil || !same {
		return false, err
	}
	head, err := runGit(runner, directory, []string{"rev-parse", "--verify", "HEAD^{commit}"}, nil)
	if err != nil {
		return false, err
	}
	if head.Status != 0 {
		return false, nil
	}
	commit, err := trimOutput(head.Stdout, "decode worktree HEAD")
	if err != nil {
		return false, err
	}
	return commit == headCommit, nil
}

// restoreRemoteWorkspaceConfiguration rebuilds the Git configuration that
// makes a managed network workspace resumable. This runs after the checkout
// has been selected so a caller that moved a session onto a different branch
// keeps that branch while still inheriting the archived push destinations
// and immutable launch base.
func restoreRemoteWorkspaceConfiguration(runner GitCommandRunner, repository string, metadata *core.RepositoryMetadata) error {
	if !metadata.RemoteWorkspace {
		return nil
	}
	if metadata.Origin == "" {
		return errors.New("managed network workspace has no Git origin")
	}
	if metadata.BaseCommit == "" {
		return errors.New("managed network workspace has no immutable base commit")
	}
	if _, err := gitSuccess(runner, repository, &GitCommand{
		Arguments: []string{"config", "--local", "--replace-all", "remote.origin.url", metadata.Origin},
	}, "restore Git origin"); err != nil {
		return err
	}

	unset, err := runGit(runner, repository, []string{"config", "--local", "--unset-all", "remote.origin.pushurl"}, nil)
	if err != nil {
		return err
	}
	if unset.Status != 0 && unset.Status != 1 && unset.Status != 5 {
		return gitFailure("clear restored Git push URLs", &unset)
	}
	pushRefspec, err := runGit(runner, repository, []string{"config", "--local", "--unset-all", "remote.origin.push"}, nil)
	if err != nil {
		return err
	}
	if pushRefspec.Status != 0 && pushRefspec.Status != 1 && pushRefspec.Status != 5 {
		return gitFailure("clear restored Git push refspec", &pushRefspec)
	}
	for _, pushURL := range metadata.PushURLs {
		if _, err := gitSuccess(runner, repository, &GitCommand{
			Arguments: []string{"config", "--local", "--add", "remote.origin.pushurl", pushURL},
		}, "restore Git push URL"); err != nil {
			return err
		}
	}
	// Older archives could retain the recorded base without carrying its
	// object. Adding a GC root must not turn those existing restores into
	// failures; their marker/config behavior remains unchanged.
	retainBase, err := ManagedBaseIsAvailable(runner, repository, metadata.BaseCommit)
	if err != nil {
		return err
	}
	if !retainBase {
		slog.Warn("legacy checkpoint base object is absent; preserving its recorded base without a GC ref",
			"repository", repository, "base_commit", metadata.BaseCommit)
	}
	return core.FinalizeManagedCheckout(metadata.BaseCommit, retainBase, func(arguments []string) error {
		_, err := gitSuccess(runner, repository, &GitCommand{Arguments: slices.Clone(arguments)},
			"restore managed Git configuration")
		return err
	})
}

// ensureBranchAvailableForCheckout refuses to move a checkout onto a branch
// owned by another worktree.
//
// `git checkout -B` permits this on some Git versions, leaving one branch
// attached to two worktrees. The restore path must detect that before it
// fetches or changes the destination checkout.
func ensureBranchAvailableForCheckout(runner GitCommandRunner, repository string, branch string) error {
	current, err := runGit(runner, repository, []string{"symbolic-ref", "--quiet", "--short", "HEAD"}, nil)
	if err != nil {
		return err
	}
	if current.Status == 0 {
		name, err := trimOutput(current.Stdout, "decode current restore branch")
		if err != nil {
			return err
		}
		if name == branch {
			return nil
		}
	} else if current.Status != 1 {
		return gitFailure("read current restore branch", &current)
	}

	// Git 2.31 has no `worktree list -z`. A branch line holds a ref name,
	// which cannot contain a newline.
	worktrees, err := runGit(runner, repository, []string{"worktree", "list", "--porcelain"}, nil)
	if err != nil {
		return err
	}
	if worktrees.Status != 0 {
		return gitFailure("list restore worktrees", &worktrees)
	}
	branchField := "branch refs/heads/" + branch
	for _, field := range bytes.Split(worktrees.Stdout, []byte("\n")) {
		if string(field) == branchField {
			return fmt.Errorf("restore committed branch: branch %s is checked out in another worktree",
				gitRustDebugString(branch))
		}
	}
	return nil
}

// checkoutAdvice names the missing commit and how to make it reachable
// again: a snapshot without a bundle relies on origin for its committed
// history.
func checkoutAdvice(snapshot *RepositorySnapshot) string {
	headCommit := snapshot.Metadata.HeadCommit
	if len(snapshot.CommittedBundle) == 0 {
		return fmt.Sprintf("restore commit %s: the archive carries no committed bundle, so this commit must be reachable from the repository's origin; fetch the origin ref that contains it and retry", headCommit)
	}
	return fmt.Sprintf("restore commit %s", headCommit)
}

func runGit(runner GitCommandRunner, repository string, arguments []string, stdin []byte) (GitOutput, error) {
	return runner.Run(repository, &GitCommand{Arguments: arguments, Stdin: stdin})
}

func gitSuccess(runner GitCommandRunner, repository string, command *GitCommand, action string) ([]byte, error) {
	output, err := runner.Run(repository, command)
	if err != nil {
		return nil, err
	}
	if output.Status != 0 {
		return nil, gitFailure(action, &output)
	}
	return output.Stdout, nil
}

func gitBytes(runner GitCommandRunner, repository string, arguments []string, stdin []byte, action string) ([]byte, error) {
	return gitSuccess(runner, repository, &GitCommand{Arguments: arguments, Stdin: stdin}, action)
}

// gitBytesOwned is gitBytes without stdin; Rust separates the two only by
// argument type.
func gitBytesOwned(runner GitCommandRunner, repository string, arguments []string, action string) ([]byte, error) {
	return gitBytes(runner, repository, arguments, nil, action)
}

// HiddenTrackedPaths lists the tracked paths whose index flags
// (skip-worktree, assume-unchanged) can hide tracked edits from Git's stash
// machinery.
func HiddenTrackedPaths(runner GitCommandRunner, checkout string) ([]string, error) {
	listed, err := gitBytes(runner, checkout, []string{"ls-files", "-v", "-z"}, nil, "inspect hidden tracked files")
	if err != nil {
		return nil, err
	}
	paths := []string{}
	for _, entry := range bytes.Split(listed, []byte{0}) {
		if len(entry) == 0 || (entry[0] != 'S' && !(entry[0] >= 'a' && entry[0] <= 'z')) {
			continue
		}
		if len(entry) < 2 || entry[1] != ' ' {
			return nil, errors.New("malformed Git index flag entry")
		}
		path, err := core.FromGitBytes(entry[2:])
		if err != nil {
			return nil, err
		}
		paths = append(paths, path)
	}
	return paths, nil
}

func gitText(runner GitCommandRunner, repository string, arguments []string) (string, error) {
	output, err := gitBytes(runner, repository, arguments, nil, "run Git command")
	if err != nil {
		return "", err
	}
	return trimOutput(output, "decode Git output")
}

func trimOutput(output []byte, action string) (string, error) {
	text, err := gitDecodeUTF8(output)
	if err != nil {
		return "", fmt.Errorf("%s: %w", action, err)
	}
	return strings.TrimSpace(text), nil
}

func gitFailure(action string, output *GitOutput) error {
	stderr := gitFromUTF8Lossy(output.Stderr)
	return fmt.Errorf("%s failed with status %d: %s", action, output.Status, strings.TrimSpace(stderr))
}

// gitDecodeUTF8 is Rust's `std::str::from_utf8`, failing with the text of
// Rust's Utf8Error ("invalid utf-8 sequence of 1 bytes from index 3",
// "incomplete utf-8 byte sequence from index 3").
func gitDecodeUTF8(output []byte) (string, error) {
	for index := 0; index < len(output); {
		valid, complete := gitUTF8Sequence(output[index:])
		switch {
		case valid > 0 && complete:
			index += valid
		case index+valid == len(output) && valid > 0:
			return "", fmt.Errorf("incomplete utf-8 byte sequence from index %d", index)
		default:
			return "", fmt.Errorf("invalid utf-8 sequence of %d bytes from index %d", max(valid, 1), index)
		}
	}
	return string(output), nil
}

// gitUTF8Sequence measures the UTF-8 sequence text starts with: valid is
// how many of its leading bytes form a valid sequence or a valid prefix of
// one, and complete whether that is a whole character. An invalid first
// byte gives zero.
func gitUTF8Sequence(text []byte) (valid int, complete bool) {
	first := text[0]
	width := 0
	switch {
	case first < 0x80:
		return 1, true
	case first >= 0xC2 && first <= 0xDF:
		width = 2
	case first >= 0xE0 && first <= 0xEF:
		width = 3
	case first >= 0xF0 && first <= 0xF4:
		width = 4
	default:
		return 0, false
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
		if offset >= len(text) {
			return offset, false
		}
		next := text[offset]
		if offset == 1 && (next < low || next > high) || offset > 1 && (next < 0x80 || next > 0xBF) {
			return offset, false
		}
	}
	return width, true
}

// gitFromUTF8Lossy is Rust's `String::from_utf8_lossy`, which replaces each
// maximal invalid subsequence with one U+FFFD.
func gitFromUTF8Lossy(output []byte) string {
	if utf8.Valid(output) {
		return string(output)
	}
	var text strings.Builder
	for index := 0; index < len(output); {
		valid, complete := gitUTF8Sequence(output[index:])
		if complete {
			text.Write(output[index : index+valid])
			index += valid
			continue
		}
		text.WriteString("�")
		index += max(valid, 1)
	}
	return text.String()
}

// gitRustDebugString renders text as Rust's `{:?}` does for a str.
func gitRustDebugString(text string) string {
	var quoted strings.Builder
	quoted.WriteByte('"')
	for index, r := range text {
		switch {
		case r == '\t':
			quoted.WriteString(`\t`)
		case r == '\r':
			quoted.WriteString(`\r`)
		case r == '\n':
			quoted.WriteString(`\n`)
		case r == '\\':
			quoted.WriteString(`\\`)
		case r == '"':
			quoted.WriteString(`\"`)
		case r == 0:
			quoted.WriteString(`\0`)
		case r == utf8.RuneError && !strings.HasPrefix(text[index:], "�"):
			// Invalid UTF-8 cannot occur in a Rust str; keep the byte visible.
			fmt.Fprintf(&quoted, `\u{%x}`, text[index])
		case !unicode.IsGraphic(r) || unicode.In(r, unicode.Mn, unicode.Me):
			// Rust escapes non-printable and grapheme-extending characters.
			fmt.Fprintf(&quoted, `\u{%x}`, r)
		default:
			quoted.WriteRune(r)
		}
	}
	quoted.WriteByte('"')
	return quoted.String()
}

// gitParseUint is Rust's `str::parse` for an unsigned integer of `bits`
// bits, failing with the text of Rust's ParseIntError.
func gitParseUint(text string, bits int) (uint64, error) {
	if text == "" {
		return 0, errors.New("cannot parse integer from empty string")
	}
	digits := text
	if text[0] == '+' {
		if len(text) == 1 {
			return 0, errors.New("invalid digit found in string")
		}
		digits = text[1:]
	}
	for index := 0; index < len(digits); index++ {
		if digits[index] < '0' || digits[index] > '9' {
			return 0, errors.New("invalid digit found in string")
		}
	}
	value, err := strconv.ParseUint(digits, 10, bits)
	if err != nil {
		return 0, errors.New("number too large to fit in target type")
	}
	return value, nil
}

// gitFields is Rust's `split_whitespace().collect()`, with nil for none.
func gitFields(text string) []string {
	fields := strings.Fields(text)
	if len(fields) == 0 {
		return nil
	}
	return fields
}

// gitSortedKeys are a map's keys in order, as a Rust BTreeMap iterates them.
func gitSortedKeys[V any](values map[string]V) []string {
	return slices.Sorted(maps.Keys(values))
}

// gitComparePaths orders paths as Rust's Path Ord does: by component.
func gitComparePaths(left string, right string) int {
	return slices.CompareFunc(archivePathComponents(left), archivePathComponents(right),
		func(left, right archivePathComponent) int {
			// Component's derived order: by kind, then by text.
			if order := cmp.Compare(left.kind, right.kind); order != 0 {
				return order
			}
			return strings.Compare(left.text, right.text)
		})
}

// gitStripPrefix is Rust's Path::strip_prefix: the components of path after
// base's, when base's components are a prefix of path's.
func gitStripPrefix(path string, base string) (string, bool) {
	if !archivePathStartsWith(path, base) {
		return "", false
	}
	return gitJoinComponents(archivePathComponents(path)[len(archivePathComponents(base)):]), true
}

// gitJoin is Rust's Path::join: path pushed onto base without cleaning
// either, so a path reads in messages, and resolves, as Rust has it.
func gitJoin(base string, path string) string {
	switch {
	case filepath.IsAbs(path):
		return path
	case base != "" && !os.IsPathSeparator(base[len(base)-1]):
		return base + string(filepath.Separator) + path
	default:
		return base + path
	}
}

// gitCollectComponents is Rust's `path.components().collect::<PathBuf>()`.
func gitCollectComponents(path string) string {
	return gitJoinComponents(archivePathComponents(path))
}

// gitJoinComponents pushes each component onto an empty PathBuf.
func gitJoinComponents(components []archivePathComponent) string {
	joined := ""
	for _, component := range components {
		switch {
		case joined == "" || os.IsPathSeparator(joined[len(joined)-1]):
			joined += component.text
		default:
			joined += string(filepath.Separator) + component.text
		}
	}
	return joined
}

// gitComponentKey names a path by its components, so two spellings of one
// path, such as `a//b` and `a/b`, are the same key as Rust's Path equality
// has them.
func gitComponentKey(path string) string {
	components := archivePathComponents(path)
	texts := make([]string, len(components))
	for index, component := range components {
		texts[index] = component.text
	}
	return strings.Join(texts, "/")
}

// gitLines splits text as Rust's `str::lines` does: on "\n", dropping one
// "\r" before it, with no empty last line for a trailing newline.
func gitLines(text string) []string {
	var lines []string
	for text != "" {
		line, rest, found := strings.Cut(text, "\n")
		if found {
			line = strings.TrimSuffix(line, "\r")
		}
		lines = append(lines, line)
		text = rest
	}
	return lines
}

// worktreeTar is which working-tree files a tar holds, which decides the
// rules it is built, checked and restored under.
type worktreeTar int

const (
	// worktreeTarUntracked holds untracked files. A symbolic link must point
	// inside the checkout, and a restore never replaces a file.
	worktreeTarUntracked worktreeTar = iota
	// worktreeTarConflicted holds the working-tree files of unmerged paths.
	// They are tracked content and travel as exactly as the patches carry the
	// rest of the tracked tree: nothing is left out, a link keeps its
	// target, and a restore replaces the file the checkout wrote at the
	// path.
	worktreeTarConflicted
)

func (k worktreeTar) label() string {
	switch k {
	case worktreeTarUntracked:
		return "untracked"
	case worktreeTarConflicted:
		return "conflicted"
	}
	panic(fmt.Sprintf("unknown worktree tar kind %d", int(k)))
}

// buildUntrackedTar archives the untracked files named in `nulPaths`.
// `linkedWorktrees` are the directories of this repository's linked
// worktrees, which are captured as entries of their own, so a walk skips
// them instead of refusing them as foreign nested repositories.
func buildUntrackedTar(
	repository string,
	nulPaths []byte,
	_ UntrackedFiles,
	linkedWorktrees map[string]struct{},
	progress func(GitSnapshotProgress) error,
) ([]byte, error) {
	var paths [][]byte
	for _, path := range bytes.Split(nulPaths, []byte{0}) {
		if len(path) > 0 {
			paths = append(paths, path)
		}
	}
	total := len(paths)
	var output bytes.Buffer
	builder := tar.NewWriter(&output)
	for index, raw := range paths {
		relative, err := core.FromGitBytes(raw)
		if err != nil {
			return nil, err
		}
		if err := validateWorktreeRelativePath(relative); err != nil {
			return nil, err
		}
		if err := progress(GitSnapshotProgressUntrackedFile{Current: index + 1, Total: total, Path: relative}); err != nil {
			return nil, err
		}
		source := gitJoin(repository, relative)
		if err := ensureNoSymlinkAncestors(repository, relative); err != nil {
			return nil, err
		}
		metadata, err := os.Lstat(source)
		if err != nil {
			return nil, fmt.Errorf("stat untracked path %s: %w", source, err)
		}
		if len(linkedWorktrees) > 0 && metadata.IsDir() {
			directory, err := gitCanonicalize(source)
			if err != nil {
				return nil, fmt.Errorf("resolve %s: %w", source, err)
			}
			if _, linked := linkedWorktrees[directory]; linked {
				continue
			}
		}
		if err := appendWorktreeEntry(builder, repository, relative, metadata, worktreeTarUntracked); err != nil {
			return nil, err
		}
	}
	if err := builder.Close(); err != nil {
		return nil, fmt.Errorf("finish untracked-file tar: %w", err)
	}
	return output.Bytes(), nil
}

// buildConflictedWorktreeTar archives the working-tree files of unmerged
// paths, and returns the tar and the paths that had no file.
func buildConflictedWorktreeTar(repository string, paths []string) (tarBytes []byte, missing []string, err error) {
	var output bytes.Buffer
	builder := tar.NewWriter(&output)
	for _, relative := range paths {
		if err := validateWorktreeRelativePath(relative); err != nil {
			return nil, nil, err
		}
		if err := ensureNoSymlinkAncestors(repository, relative); err != nil {
			return nil, nil, err
		}
		source := gitJoin(repository, relative)
		metadata, err := os.Lstat(source)
		if errors.Is(err, fs.ErrNotExist) {
			missing = append(missing, relative)
			continue
		}
		if err != nil {
			return nil, nil, fmt.Errorf("stat conflicted path %s: %w", source, err)
		}
		if err := appendWorktreeEntry(builder, repository, relative, metadata, worktreeTarConflicted); err != nil {
			return nil, nil, err
		}
	}
	if err := builder.Close(); err != nil {
		return nil, nil, fmt.Errorf("finish conflicted-file tar: %w", err)
	}
	return output.Bytes(), missing, nil
}

func isNestedRepository(source string, metadata fs.FileInfo) (bool, error) {
	if !metadata.IsDir() || metadata.Mode()&fs.ModeSymlink != 0 {
		return false, nil
	}
	// Rust's `try_exists` follows links and is false only for a missing
	// path.
	_, err := os.Stat(filepath.Join(source, ".git"))
	switch {
	case err == nil:
		return true, nil
	case errors.Is(err, fs.ErrNotExist):
		return false, nil
	default:
		return false, fmt.Errorf("inspect nested repository directory %s: %w", source, err)
	}
}

func appendWorktreeEntry(builder *tar.Writer, repository string, relative string, metadata fs.FileInfo, kind worktreeTar) error {
	source := gitJoin(repository, relative)
	mode := metadata.Mode()
	switch {
	case mode&fs.ModeSymlink != 0:
		target, err := os.Readlink(source)
		if err != nil {
			return fmt.Errorf("read symlink %s: %w", source, err)
		}
		if kind == worktreeTarUntracked {
			if err := ValidateSymlinkTarget(relative, target); err != nil {
				return err
			}
		}
		linkName, err := gitTarLinkName(target)
		if err != nil {
			return err
		}
		return builder.WriteHeader(&tar.Header{
			Typeflag: tar.TypeSymlink,
			Name:     gitTarName(relative),
			Linkname: linkName,
			Mode:     0o777,
			ModTime:  time.Unix(0, 0),
			Format:   tar.FormatGNU,
		})
	case mode.IsRegular():
		header, err := gitTarFileHeader(metadata)
		if err != nil {
			return err
		}
		header.Name = gitTarName(relative)
		file, err := os.Open(source)
		if err != nil {
			return fmt.Errorf("open %s file %s: %w", kind.label(), source, err)
		}
		defer file.Close()
		if err := builder.WriteHeader(header); err != nil {
			return err
		}
		_, err = io.Copy(builder, file)
		return err
	case metadata.IsDir():
		if kind == worktreeTarUntracked {
			nested, err := isNestedRepository(source, metadata)
			if err != nil {
				return err
			}
			if nested {
				return fmt.Errorf("%s%s: for a disposable scratch worktree, exclude this path using the outer "+
					"repository's .git/info/exclude; otherwise preserve its work separately",
					core.NestedRepositoryCheckpointError, source)
			}
		}
		return fmt.Errorf("unsupported %s directory at %s", kind.label(), source)
	default:
		// Rust's Debug for a file type that is none of the three.
		return fmt.Errorf("unsupported %s file type FileType { is_file: false, is_dir: false, is_symlink: false, .. } at %s",
			kind.label(), source)
	}
}

// gitTarName is a validated relative path as the tar crate's `set_path`
// writes it: its components joined by `/`.
func gitTarName(relative string) string {
	components := archivePathComponents(relative)
	texts := make([]string, len(components))
	for index, component := range components {
		texts[index] = filepath.ToSlash(component.text)
	}
	return strings.Join(texts, "/")
}

// gitTarLinkNameBytes is the size of a tar header's link name field.
// Rust's `Header::set_link_name` fills only this field, with no GNU long
// link entry, so a longer target cannot be archived.
const gitTarLinkNameBytes = 100

// gitTarLinkName is a symbolic link's target as the tar crate's
// `Header::set_link_name` writes it: its components joined by `/`, a root
// kept as `/`, and a trailing `/` kept, failing as Rust does when the
// result does not fit the header's link name field.
func gitTarLinkName(target string) (string, error) {
	var name strings.Builder
	emitted, needsSlash := false, false
	for _, component := range archivePathComponents(target) {
		text := filepath.ToSlash(component.text)
		if needsSlash {
			name.WriteByte('/')
		}
		name.WriteString(text)
		needsSlash = text != "/"
		emitted = true
	}
	if !emitted {
		return "", errors.New("paths in archives must have at least one component when setting link name for ")
	}
	if target != "" && os.IsPathSeparator(target[len(target)-1]) {
		name.WriteByte('/')
	}
	if strings.IndexByte(name.String(), 0) >= 0 {
		return "", errors.New("provided value contains a nul byte when setting link name for ")
	}
	if name.Len() > gitTarLinkNameBytes {
		return "", errors.New("provided value is too long when setting link name for ")
	}
	return name.String(), nil
}

// gitTarFileInfo leaves a tar header's user and group names empty, as the
// tar crate's `Header::set_metadata` does.
type gitTarFileInfo struct{ fs.FileInfo }

func (gitTarFileInfo) Uname() (string, error) { return "", nil }
func (gitTarFileInfo) Gname() (string, error) { return "", nil }

// gitTarFileHeader is a GNU header for a regular file as the tar crate's
// `Header::set_metadata` fills it: size, whole-second modification time,
// owner ids and mode (with the file type bits on Unix; 0644 or 0444 on
// Windows), and no access or change time.
func gitTarFileHeader(metadata fs.FileInfo) (*tar.Header, error) {
	header, err := tar.FileInfoHeader(gitTarFileInfo{metadata}, "")
	if err != nil {
		return nil, err
	}
	header.Format = tar.FormatGNU
	header.ModTime = time.Unix(metadata.ModTime().Unix(), 0)
	header.AccessTime, header.ChangeTime = time.Time{}, time.Time{}
	header.Uname, header.Gname = "", ""
	if runtime.GOOS == "windows" {
		header.Uid, header.Gid = 0, 0
		header.Mode = 0o644
		if metadata.Mode().Perm()&0o200 == 0 {
			header.Mode = 0o444
		}
	} else {
		const regularFile = 0o100000
		header.Mode |= regularFile
	}
	return header, nil
}

func validateUntrackedTar(data []byte) error {
	return validateUntrackedTarReader(bytes.NewReader(data))
}

func validateUntrackedTarReader(reader io.Reader) error {
	return validateWorktreeTarReader(reader, worktreeTarUntracked)
}

func validateConflictedWorktreeTar(data []byte) error {
	return validateConflictedWorktreeTarReader(bytes.NewReader(data))
}

func validateConflictedWorktreeTarReader(reader io.Reader) error {
	return validateWorktreeTarReader(reader, worktreeTarConflicted)
}

func validateWorktreeTarReader(reader io.Reader, kind worktreeTar) error {
	label := kind.label()
	archive := tar.NewReader(reader)
	for {
		header, err := archive.Next()
		if err == io.EOF {
			return nil
		}
		if err != nil {
			return fmt.Errorf("read %s-file tar entry: %w", label, err)
		}
		relative := header.Name
		if err := validateWorktreeRelativePath(relative); err != nil {
			return err
		}
		symlink := header.Typeflag == tar.TypeSymlink
		if !gitTarIsFile(header) && !symlink {
			return fmt.Errorf("%s tar contains unsupported entry type for '%s'", label, relative)
		}
		if symlink {
			if header.Linkname == "" {
				return fmt.Errorf("%s symlink '%s' has no target", label, relative)
			}
			if kind == worktreeTarUntracked {
				if err := ValidateSymlinkTarget(relative, header.Linkname); err != nil {
					return err
				}
			}
		}
	}
}

// gitTarIsFile is the tar crate's `EntryType::is_file`: a regular file,
// whether its type flag is `0` or the old NUL.
func gitTarIsFile(header *tar.Header) bool {
	return header.Typeflag == tar.TypeReg || header.Typeflag == '\x00'
}

func restoreUntrackedTar(repository string, data []byte) error {
	return restoreWorktreeTar(repository, data, worktreeTarUntracked)
}

func restoreWorktreeTar(repository string, data []byte, kind worktreeTar) error {
	label := kind.label()
	if err := validateWorktreeTarReader(bytes.NewReader(data), kind); err != nil {
		return err
	}
	archive := tar.NewReader(bytes.NewReader(data))
	for {
		header, err := archive.Next()
		if err == io.EOF {
			return nil
		}
		if err != nil {
			return fmt.Errorf("read %s-file tar entry: %w", label, err)
		}
		relative := header.Name
		if err := ensureNoSymlinkAncestors(repository, relative); err != nil {
			return err
		}
		destination := gitJoin(repository, relative)
		parent := filepath.Dir(destination)
		if err := os.MkdirAll(parent, 0o777); err != nil {
			return fmt.Errorf("create directory %s: %w", parent, err)
		}
		if err := ensureNoSymlinkAncestors(repository, relative); err != nil {
			return err
		}
		switch kind {
		case worktreeTarUntracked:
			if _, err := os.Stat(destination); err == nil {
				return fmt.Errorf("refusing to overwrite restored untracked path '%s'", destination)
			}
		case worktreeTarConflicted:
			// The checkout wrote the path's committed version, which the
			// conflicted file replaces.
			if err := removeWorktreeFile(destination); err != nil {
				return err
			}
		}
		if header.Typeflag == tar.TypeSymlink {
			if header.Linkname == "" {
				return fmt.Errorf("%s symlink '%s' has no target", label, relative)
			}
			if err := createSymlink(header.Linkname, destination); err != nil {
				return err
			}
			continue
		}
		output, err := os.OpenFile(destination, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o666)
		if err != nil {
			return fmt.Errorf("create %s file %s: %w", label, destination, err)
		}
		if _, err := io.Copy(output, archive); err != nil {
			_ = output.Close()
			return err
		}
		if err := output.Sync(); err != nil {
			_ = output.Close()
			return err
		}
		if err := output.Close(); err != nil {
			return err
		}
		if runtime.GOOS != "windows" {
			if err := os.Chmod(destination, fs.FileMode(header.Mode&0o777)); err != nil {
				return err
			}
		}
	}
}

// removeWorktreeFile removes the file or symbolic link at `path`, if there
// is one. A directory there is an error: the restored tree disagrees with
// the archived one.
func removeWorktreeFile(path string) error {
	metadata, err := os.Lstat(path)
	switch {
	case errors.Is(err, fs.ErrNotExist):
		return nil
	case err != nil:
		return fmt.Errorf("inspect %s: %w", path, err)
	case metadata.IsDir():
		return fmt.Errorf("refusing to replace directory '%s' with a conflicted file", path)
	}
	if err := os.Remove(path); err != nil {
		return fmt.Errorf("remove %s: %w", path, err)
	}
	return nil
}

// ensureNoSymlinkAncestors refuses a path whose already-existing
// intermediate directories include a symlink, so a restore can never be
// redirected out of `root`.
func ensureNoSymlinkAncestors(root string, relative string) error {
	current := root
	components := archivePathComponents(relative)
	for index := 0; index+1 < len(components); index++ {
		component := components[index]
		if component.kind != archiveComponentNormal {
			return fmt.Errorf("unsafe path '%s'", relative)
		}
		current = gitJoin(current, component.text)
		metadata, err := os.Lstat(current)
		switch {
		case err == nil:
			if metadata.Mode()&fs.ModeSymlink != 0 {
				return fmt.Errorf("path '%s' traverses a symlink", relative)
			}
		case errors.Is(err, fs.ErrNotExist):
			return nil
		default:
			return fmt.Errorf("inspect %s: %w", current, err)
		}
	}
	return nil
}

func ValidateSymlinkTarget(linkPath string, target string) error {
	if filepath.IsAbs(target) {
		return fmt.Errorf("symlink '%s' has an absolute target", linkPath)
	}
	// The depth of the link's parent: Path::parent drops a last component
	// that is normal, `.` or `..`, and there is none above a root.
	depth := 0
	if components := archivePathComponents(linkPath); len(components) > 0 {
		switch components[len(components)-1].kind {
		case archiveComponentNormal, archiveComponentCurDir, archiveComponentParentDir:
			depth = len(components) - 1
		}
	}
	for _, component := range archivePathComponents(target) {
		switch component.kind {
		case archiveComponentNormal:
			depth++
		case archiveComponentCurDir:
		case archiveComponentParentDir:
			if depth == 0 {
				return fmt.Errorf("symlink '%s' escapes the repository", linkPath)
			}
			depth--
		default:
			return fmt.Errorf("symlink '%s' escapes the repository", linkPath)
		}
	}
	return nil
}

// createSymlink makes `destination` a symbolic link to `target`. Rust uses
// `symlink_file` on Windows; os.Symlink makes a directory link there only
// when the target already resolves to a directory.
func createSymlink(target string, destination string) error {
	if err := os.Symlink(target, destination); err != nil {
		return fmt.Errorf("create symlink %s: %w", destination, err)
	}
	return nil
}

// ---------------------------------------------------------------------------
// Session export
// ---------------------------------------------------------------------------

// ExportRefusedExitCode is the exit code the worker's export subcommands use
// for a precondition the caller can fix, such as a repository with no push
// remote or a session with no recorded base. Anything else is an ordinary
// failure, and clap's exit code 2 stays reserved for a worker too old to
// know these subcommands at all.
const ExportRefusedExitCode = 3

// SessionExportError is why a session export could not be produced: a
// SessionExportErrorRefused or a SessionExportErrorFailed.
//
// A refusal is a precondition the caller can act on, and it survives the
// worker process boundary as ExportRefusedExitCode plus the reason on
// standard error, so the API can answer 409 with that reason instead of 500.
type SessionExportError interface {
	error
	isSessionExportError()
}

// SessionExportErrorRefused is a refusal with its reason. The conversion
// `SessionExportErrorRefused(reason)` is Rust's `SessionExportError::refused`.
type SessionExportErrorRefused string

// SessionExportErrorFailed is any other failure (Rust: From<anyhow::Error>).
type SessionExportErrorFailed struct {
	Err error
}

func (SessionExportErrorRefused) isSessionExportError() {}
func (SessionExportErrorFailed) isSessionExportError()  {}

func (e SessionExportErrorRefused) Error() string { return string(e) }
func (e SessionExportErrorFailed) Error() string  { return e.Err.Error() }
func (e SessionExportErrorFailed) Unwrap() error  { return e.Err }

// SessionDiff is the work a session did in one repository, as a unified
// diff.
//
// The comparison is the session's base commit against the working tree,
// tracked and untracked alike, so a caller sees what the agent produced
// whether or not it committed. The base is resolved in the order the caller
// can trust: an explicit base, then the immutable base a managed network
// workspace records, then the commit the session branch was created at.
type SessionDiff struct {
	Diff string `json:"diff"`
	Base string `json:"base"`
	Head string `json:"head"`
	// HeadDescendsFromBase reports whether the base is an ancestor of HEAD.
	// false means the session rewrote history the base was on, so the diff
	// carries changes the session did not make. nil is an older worker whose
	// JSON predates this field (serde default; written as null).
	HeadDescendsFromBase *bool `json:"head_descends_from_base"`
}

// UnmarshalJSON requires the fields serde requires.
func (d *SessionDiff) UnmarshalJSON(data []byte) error {
	type plain SessionDiff
	if err := gitRequireJSONFields(data, "diff", "base", "head"); err != nil {
		return err
	}
	return json.Unmarshal(data, (*plain)(d))
}

// gitRequireJSONFields fails as serde does when a JSON object lacks a field
// without a default.
func gitRequireJSONFields(data []byte, names ...string) error {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	for _, name := range names {
		if _, ok := fields[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	return nil
}

// SessionDiffText is Rust's `session_diff`, renamed because Go cannot name
// a function after the SessionDiff type: the diff alone.
func SessionDiffText(runner GitCommandRunner, repository string, base *string, branch *string) (string, SessionExportError) {
	details, err := SessionDiffDetails(runner, repository, base, branch)
	if err != nil {
		return "", err
	}
	return details.Diff, nil
}

func SessionDiffDetails(runner GitCommandRunner, repository string, base *string, branch *string) (SessionDiff, SessionExportError) {
	failed := func(err error) (SessionDiff, SessionExportError) {
		return SessionDiff{}, SessionExportErrorFailed{Err: err}
	}
	var requested string
	if base != nil {
		requested = strings.TrimSpace(*base)
		if requested == "" {
			return SessionDiff{}, SessionExportErrorRefused("diff base must not be empty")
		}
	} else {
		var recorded *string
		var err error
		if branch != nil {
			recorded, err = RemoteWorkspaceBase(runner, repository)
			if err != nil {
				return failed(err)
			}
			if recorded == nil {
				recorded, err = branchCreationCommit(runner, repository, *branch)
				if err != nil {
					return failed(err)
				}
			}
		} else {
			recorded, err = RemoteWorkspaceBase(runner, repository)
			if err != nil {
				return failed(err)
			}
		}
		if recorded == nil {
			return SessionDiff{}, SessionExportErrorRefused("no session base recorded")
		}
		requested = *recorded
	}
	resolved, err := runGit(runner, repository,
		[]string{"rev-parse", "--verify", "--end-of-options", requested + "^{commit}"}, nil)
	if err != nil {
		return failed(err)
	}
	if resolved.Status != 0 {
		return SessionDiff{}, SessionExportErrorRefused(
			fmt.Sprintf("diff base %s does not resolve to a commit", gitRustDebugString(requested)))
	}
	resolvedBase, err := trimOutput(resolved.Stdout, "resolve the session diff base")
	if err != nil {
		return failed(err)
	}
	head, err := gitText(runner, repository, []string{"rev-parse", "--verify", "HEAD^{commit}"})
	if err != nil {
		return failed(fmt.Errorf("resolve the session diff head: %w", err))
	}
	// A base that HEAD no longer descends from means the session rewrote the
	// history the base sits on, so the diff reverts commits the session never
	// touched. Status 1 is Git's plain "not an ancestor", not a failure.
	ancestry, err := runGit(runner, repository, []string{"merge-base", "--is-ancestor", resolvedBase, head}, nil)
	if err != nil {
		return failed(err)
	}
	var descends bool
	switch ancestry.Status {
	case 0:
		descends = true
	case 1:
		descends = false
	default:
		return failed(gitFailure("compare the session base with HEAD", &ancestry))
	}
	baseTree, err := gitText(runner, repository, []string{"rev-parse", resolvedBase + "^{tree}"})
	if err != nil {
		return failed(fmt.Errorf("resolve the session base tree: %w", err))
	}
	current, err := CaptureWorktreeTree(runner, repository)
	if err != nil {
		return failed(err)
	}
	diff, err := DiffBetweenTrees(runner, repository, &baseTree, current)
	if err != nil {
		return failed(err)
	}
	return SessionDiff{Diff: diff, Base: resolvedBase, Head: head, HeadDescendsFromBase: &descends}, nil
}

// branchCreationCommit is the commit a branch was created at, read from its
// reflog.
//
// The oldest reflog entry is the branch's creation, so its commit is where
// the session started. Reflogs expire, which is why a recorded base is
// preferred; this is the fallback for sessions created before one was
// recorded.
func branchCreationCommit(runner GitCommandRunner, repository string, branch string) (*string, error) {
	output, err := runGit(runner, repository, []string{"reflog", "show", "--format=%H", "refs/heads/" + branch}, nil)
	if err != nil {
		return nil, err
	}
	if output.Status != 0 {
		return nil, nil
	}
	text, err := gitDecodeUTF8(output.Stdout)
	if err != nil {
		return nil, fmt.Errorf("decode the branch reflog: %w", err)
	}
	lines := gitLines(text)
	for index := len(lines) - 1; index >= 0; index-- {
		if commit := strings.TrimSpace(lines[index]); commit != "" {
			return &commit, nil
		}
	}
	return nil, nil
}

// PushedBranch is a branch this push created or advanced, and the remote it
// reached.
type PushedBranch struct {
	Remote string `json:"remote"`
	Branch string `json:"branch"`
}

// UnmarshalJSON requires the fields serde requires.
func (b *PushedBranch) UnmarshalJSON(data []byte) error {
	type plain PushedBranch
	if err := gitRequireJSONFields(data, "remote", "branch"); err != nil {
		return err
	}
	return json.Unmarshal(data, (*plain)(b))
}

// PushBranchError is why a session branch could not be pushed: a
// PushBranchErrorNoRemote, PushBranchErrorInvalidBranch or
// PushBranchErrorFailed.
//
// A missing remote is separated from a failed push because it is the one
// outcome the caller can fix without looking at Git's output: nothing was
// attempted, and configuring a remote makes the same call work.
type PushBranchError interface {
	error
	isPushBranchError()
}

// PushBranchErrorNoRemote: the repository names neither
// `remote.pushDefault` nor `origin`.
type PushBranchErrorNoRemote struct{}

// PushBranchErrorInvalidBranch: Git would not accept the name as a branch.
type PushBranchErrorInvalidBranch string

// PushBranchErrorFailed is any other failure (Rust: From<anyhow::Error>).
type PushBranchErrorFailed struct {
	Err error
}

func (PushBranchErrorNoRemote) isPushBranchError()      {}
func (PushBranchErrorInvalidBranch) isPushBranchError() {}
func (PushBranchErrorFailed) isPushBranchError()        {}

func (PushBranchErrorNoRemote) Error() string { return "no push remote configured" }
func (e PushBranchErrorInvalidBranch) Error() string {
	return gitRustDebugString(string(e)) + " is not a valid branch name"
}
func (e PushBranchErrorFailed) Error() string { return e.Err.Error() }
func (e PushBranchErrorFailed) Unwrap() error { return e.Err }

// PushBranch pushes the session's current HEAD to `branch` on the
// repository's push remote.
//
// The refspec names HEAD rather than the session branch so this works the
// same way on a detached checkout, and the destination is spelled in full so
// a remote's push rules cannot redirect it somewhere else.
func PushBranch(runner GitCommandRunner, repository string, branch string) (PushedBranch, PushBranchError) {
	if strings.HasPrefix(branch, "-") {
		return PushedBranch{}, PushBranchErrorInvalidBranch(branch)
	}
	checked, err := runGit(runner, repository, []string{"check-ref-format", "--branch", branch}, nil)
	if err != nil {
		return PushedBranch{}, PushBranchErrorFailed{Err: err}
	}
	if checked.Status != 0 {
		return PushedBranch{}, PushBranchErrorInvalidBranch(branch)
	}
	remote, failure := pushRemote(runner, repository)
	if failure != nil {
		return PushedBranch{}, failure
	}
	refspec := "HEAD:refs/heads/" + branch
	pushed, err := runGit(runner, repository, []string{"push", remote, refspec}, nil)
	if err != nil {
		return PushedBranch{}, PushBranchErrorFailed{Err: err}
	}
	if pushed.Status != 0 {
		return PushedBranch{}, PushBranchErrorFailed{Err: gitFailure("push the session branch", &pushed)}
	}
	return PushedBranch{Remote: remote, Branch: branch}, nil
}

// pushRemote is the remote a push goes to: the configured default, else
// `origin` when the repository has one.
func pushRemote(runner GitCommandRunner, repository string) (string, PushBranchError) {
	configured, err := runGit(runner, repository, []string{"config", "--get", "remote.pushDefault"}, nil)
	if err != nil {
		return "", PushBranchErrorFailed{Err: err}
	}
	if configured.Status == 0 {
		name, err := trimOutput(configured.Stdout, "read the configured push remote")
		if err != nil {
			return "", PushBranchErrorFailed{Err: err}
		}
		if name != "" {
			return name, nil
		}
	}
	remotes, err := gitText(runner, repository, []string{"remote"})
	if err != nil {
		return "", PushBranchErrorFailed{Err: fmt.Errorf("list the repository remotes: %w", err)}
	}
	for _, remote := range gitLines(remotes) {
		if strings.TrimSpace(remote) == "origin" {
			return "origin", nil
		}
	}
	return "", PushBranchErrorNoRemote{}
}
