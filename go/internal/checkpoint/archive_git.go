// Port of mj-checkpoint/src/archive/git.rs.

package checkpoint

import (
	"archive/tar"
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"

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
	if ssh, ok := core.LocalGitSSHCommand(); ok {
		process.Env = append(process.Env, "GIT_SSH_COMMAND="+ssh)
	}
	output, err := core.RunWithInput(process, command.Stdin)
	if err != nil {
		return GitOutput{}, fmt.Errorf("run git in %s: %w", repository, err)
	}
	return GitOutput{Status: output.Status.ExitCode(), Stdout: output.Stdout, Stderr: output.Stderr}, nil
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
	// UntrackedFilesIgnoreNestedRepositories is explicit user consent to omit
	// untracked nested Git repositories.
	UntrackedFilesIgnoreNestedRepositories
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
	panic("unported: mj-checkpoint/src/archive/git.rs collect_git_snapshot_with_progress")
}

// CollectGitMetadataSnapshot collects only enough Git identity to associate
// native harness state with an existing project, together with its durable
// review ref names and object IDs. The project remains the recovery source
// for objects and working files.
func CollectGitMetadataSnapshot(runner GitCommandRunner, repository string, spec *GitCollectionSpec) (RepositorySnapshot, error) {
	panic("unported: mj-checkpoint/src/archive/git.rs collect_git_metadata_snapshot")
}

// CollectPlainDirectorySnapshot is a snapshot of a bare local session's
// directory when it is not a Git checkout.
//
// Nothing about the folder is captured: it is the user's own directory, not
// a managed checkout, so a restore never replaces its files. The metadata
// still names the directory so an archive describes the shape of the session
// it belongs to, and it carries no commit, so nothing claims Git provenance.
func CollectPlainDirectorySnapshot(spec *GitCollectionSpec) (RepositorySnapshot, error) {
	panic("unported: mj-checkpoint/src/archive/git.rs collect_plain_directory_snapshot")
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
	panic("unported: mj-checkpoint/src/archive/git.rs locate_checkout")
}

type collectedGitIdentity struct {
	origin     string
	pushURLs   []string
	headCommit string
	branch     *string
}

func collectGitIdentity(runner GitCommandRunner, repository string, originOverride *string) (collectedGitIdentity, error) {
	panic("unported: mj-checkpoint/src/archive/git.rs collect_git_identity")
}

// collectGitPushURLs reads only explicitly configured push destinations.
// `git remote get-url --push` falls back to the fetch URL when no push URL
// is configured, which would lose the distinction that lets a managed
// workspace keep its host's default push destination after restore.
func collectGitPushURLs(runner GitCommandRunner, repository string) ([]string, error) {
	panic("unported: mj-checkpoint/src/archive/git.rs collect_git_push_urls")
}

// RemoteWorkspaceBase reads the immutable launch base recorded on a managed
// network workspace. Returning nil for an absent or false marker keeps raw
// local DeltaFrom captures separate from the managed restore path.
func RemoteWorkspaceBase(runner GitCommandRunner, repository string) (*string, error) {
	panic("unported: mj-checkpoint/src/archive/git.rs remote_workspace_base")
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
	panic("unported: mj-checkpoint/src/archive/git.rs merge_base")
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
	panic("unported: mj-checkpoint/src/archive/git.rs session_start_scope")
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
	panic("unported: mj-checkpoint/src/archive/git.rs migrate_review_ref")
}

// gitDirectories returns the worktree's own Git directory and the
// repository's common one, which are the same directory in the main
// worktree.
func gitDirectories(runner GitCommandRunner, repository string) (gitDir string, commonDir string, err error) {
	output, err := gitText(runner, repository,
		[]string{"rev-parse", "--path-format=absolute", "--absolute-git-dir", "--git-common-dir"})
	if err != nil {
		return "", "", fmt.Errorf("locate the Git directories: %w", err)
	}
	lines := gitLines(output)
	if len(lines) < 2 {
		return "", "", fmt.Errorf("git rev-parse named no common directory: %s", output)
	}
	return lines[0], lines[1], nil
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
	panic("unported: mj-checkpoint/src/archive/git.rs ScratchIndex::create")
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
	panic("unported: mj-checkpoint/src/archive/git.rs capture_paths")
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
	panic("unported: mj-checkpoint/src/archive/git.rs record_review_point")
}

// ReadReviewPoint reads a review point, or a bare tree id from an earlier
// release as a point with no recorded HEAD.
func ReadReviewPoint(runner GitCommandRunner, repository string, id string) (ReviewPoint, error) {
	panic("unported: mj-checkpoint/src/archive/git.rs read_review_point")
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
	panic("unported: mj-checkpoint/src/archive/git.rs ReflogEntry::moves_only")
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
	panic("unported: mj-checkpoint/src/archive/git.rs read_head_reflog")
}

// TreeWithoutPaths is `tree` with `paths` removed, written as a new tree
// object through a scratch index. Paths `tree` does not hold are ignored.
func TreeWithoutPaths(runner GitCommandRunner, repository string, tree string, paths []string) (string, error) {
	panic("unported: mj-checkpoint/src/archive/git.rs tree_without_paths")
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
	panic("unported: mj-checkpoint/src/archive/git.rs merge_trees")
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
	panic("unported: mj-checkpoint/src/archive/git.rs unmerged_paths")
}

// resolveConflicts settles every unmerged path and returns the
// `update-index -z --index-info` input that records the results.
func resolveConflicts(runner GitCommandRunner, repository string, conflicts []unmergedPath, lines MergeConflicts) ([]byte, error) {
	panic("unported: mj-checkpoint/src/archive/git.rs resolve_conflicts")
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
	panic("unported: mj-checkpoint/src/archive/git.rs merge_file")
}

// readBlobs reads blobs in one `git cat-file --batch`, keyed by object id.
func readBlobs(runner GitCommandRunner, repository string, objects []string) (map[string][]byte, error) {
	panic("unported: mj-checkpoint/src/archive/git.rs read_blobs")
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
	panic("unported: mj-checkpoint/src/archive/git.rs capture_worktree_tree")
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
	panic("unported: mj-checkpoint/src/archive/git.rs diff_between_trees")
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
	panic("unported: mj-checkpoint/src/archive/git.rs collect_review_refs")
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
	panic("unported: mj-checkpoint/src/archive/git.rs collect_clone_refs")
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
	panic("unported: mj-checkpoint/src/archive/git.rs select_git_history")
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
	panic("unported: mj-checkpoint/src/archive/git.rs collect_git_contents")
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

// discoverLinkedWorktrees returns the linked worktrees of `repository`'s
// checkout that live inside it, at any depth, parents before children.
//
// A worktree is "ours" by Git's own definition of the same repository: the
// common Git directory that `git rev-parse` names inside it is the
// checkout's. A registered worktree outside the checkout, or whose directory
// is gone, holds nothing this checkout's session owns and is left out; this
// never prunes a registration.
func discoverLinkedWorktrees(runner GitCommandRunner, repository string) ([]linkedWorktreeDirectory, error) {
	panic("unported: mj-checkpoint/src/archive/git.rs discover_linked_worktrees")
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
	panic("unported: mj-checkpoint/src/archive/git.rs prepare_linked_worktrees")
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
	panic("unported: mj-checkpoint/src/archive/git.rs include_worktree_history")
}

func collectLinkedWorktreeContents(
	runner GitCommandRunner,
	worktrees []preparedWorktree,
	untrackedFiles UntrackedFiles,
	linkedWorktrees map[string]struct{},
	progress func(GitSnapshotProgress) error,
) ([]LinkedWorktreeSnapshot, error) {
	panic("unported: mj-checkpoint/src/archive/git.rs collect_linked_worktree_contents")
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
	panic("unported: mj-checkpoint/src/archive/git.rs relocated_review_refs")
}

// RestoreGitSnapshot restores a snapshot into `repository`, the directory
// its paths are named from. For a snapshot that records a
// `checkout_subdirectory` that is the destination checkout's top level: its
// untracked files are named from there, and `git apply` run from a
// subdirectory skips every path outside it.
func RestoreGitSnapshot(runner GitCommandRunner, repository string, snapshot *RepositorySnapshot) error {
	return restoreGitSnapshotForSession(runner, repository, snapshot, nil, true)
}

func restoreGitSnapshotForSession(
	runner GitCommandRunner,
	repository string,
	snapshot *RepositorySnapshot,
	session *string,
	restoreContents bool,
) error {
	panic("unported: mj-checkpoint/src/archive/git.rs restore_git_snapshot_for_session")
}

// restoreLinkedWorktrees recreates the checkout's linked worktrees, parents
// before children, each with the HEAD, index, files, operation and lock it
// was captured with.
func restoreLinkedWorktrees(runner GitCommandRunner, repository string, worktrees []LinkedWorktreeSnapshot) error {
	panic("unported: mj-checkpoint/src/archive/git.rs restore_linked_worktrees")
}

// listedWorktree is one entry of `git worktree list --porcelain`.
type listedWorktree struct {
	path   string
	branch *string
	locked bool
}

func listWorktrees(runner GitCommandRunner, repository string) ([]listedWorktree, error) {
	panic("unported: mj-checkpoint/src/archive/git.rs list_worktrees")
}

func restoreLinkedWorktree(runner GitCommandRunner, repository string, worktree *LinkedWorktreeSnapshot) error {
	panic("unported: mj-checkpoint/src/archive/git.rs restore_linked_worktree")
}

// restoreLinkedWorktreeState applies a linked worktree's dirty state,
// operation and lock to its fresh checkout of the recorded commit.
func restoreLinkedWorktreeState(
	runner GitCommandRunner,
	repository string,
	directory string,
	worktree *LinkedWorktreeSnapshot,
) error {
	panic("unported: mj-checkpoint/src/archive/git.rs restore_linked_worktree_state")
}

// isRestoredLinkedWorktree reports whether `directory` is already a worktree
// of `repository` at `headCommit`.
func isRestoredLinkedWorktree(runner GitCommandRunner, repository string, directory string, headCommit string) (bool, error) {
	panic("unported: mj-checkpoint/src/archive/git.rs is_restored_linked_worktree")
}

// restoreRemoteWorkspaceConfiguration rebuilds the Git configuration that
// makes a managed network workspace resumable. This runs after the checkout
// has been selected so a caller that moved a session onto a different branch
// keeps that branch while still inheriting the archived push destinations
// and immutable launch base.
func restoreRemoteWorkspaceConfiguration(runner GitCommandRunner, repository string, metadata *core.RepositoryMetadata) error {
	panic("unported: mj-checkpoint/src/archive/git.rs restore_remote_workspace_configuration")
}

// ensureBranchAvailableForCheckout refuses to move a checkout onto a branch
// owned by another worktree.
//
// `git checkout -B` permits this on some Git versions, leaving one branch
// attached to two worktrees. The restore path must detect that before it
// fetches or changes the destination checkout.
func ensureBranchAvailableForCheckout(runner GitCommandRunner, repository string, branch string) error {
	panic("unported: mj-checkpoint/src/archive/git.rs ensure_branch_available_for_checkout")
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
	panic("unported: mj-checkpoint/src/archive/git.rs std::str::from_utf8 (Utf8Error text)")
}

// gitFromUTF8Lossy is Rust's `String::from_utf8_lossy`, which replaces each
// maximal invalid subsequence with one U+FFFD.
func gitFromUTF8Lossy(output []byte) string {
	panic("unported: mj-checkpoint/src/archive/git.rs String::from_utf8_lossy")
}

// gitRustDebugString renders text as Rust's `{:?}` does for a str.
func gitRustDebugString(text string) string {
	panic("unported: mj-checkpoint/src/archive/git.rs {:?} for str")
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
	policy UntrackedFiles,
	linkedWorktrees map[string]struct{},
	progress func(GitSnapshotProgress) error,
) ([]byte, error) {
	panic("unported: mj-checkpoint/src/archive/git.rs build_untracked_tar")
}

// buildConflictedWorktreeTar archives the working-tree files of unmerged
// paths, and returns the tar and the paths that had no file.
func buildConflictedWorktreeTar(repository string, paths []string) (tarBytes []byte, missing []string, err error) {
	panic("unported: mj-checkpoint/src/archive/git.rs build_conflicted_worktree_tar")
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
	panic("unported: mj-checkpoint/src/archive/git.rs append_worktree_entry")
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
	panic("unported: mj-checkpoint/src/archive/git.rs validate_worktree_tar_reader")
}

func restoreUntrackedTar(repository string, data []byte) error {
	return restoreWorktreeTar(repository, data, worktreeTarUntracked)
}

func restoreWorktreeTar(repository string, data []byte, kind worktreeTar) error {
	panic("unported: mj-checkpoint/src/archive/git.rs restore_worktree_tar")
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
	panic("unported: mj-checkpoint/src/archive/git.rs ensure_no_symlink_ancestors")
}

func ValidateSymlinkTarget(linkPath string, target string) error {
	panic("unported: mj-checkpoint/src/archive/git.rs validate_symlink_target")
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
	panic("unported: mj-checkpoint/src/archive/git.rs session_diff_details")
}

// branchCreationCommit is the commit a branch was created at, read from its
// reflog.
//
// The oldest reflog entry is the branch's creation, so its commit is where
// the session started. Reflogs expire, which is why a recorded base is
// preferred; this is the fallback for sessions created before one was
// recorded.
func branchCreationCommit(runner GitCommandRunner, repository string, branch string) (*string, error) {
	panic("unported: mj-checkpoint/src/archive/git.rs branch_creation_commit")
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
	panic("unported: mj-checkpoint/src/archive/git.rs push_branch")
}

// pushRemote is the remote a push goes to: the configured default, else
// `origin` when the repository has one.
func pushRemote(runner GitCommandRunner, repository string) (string, PushBranchError) {
	panic("unported: mj-checkpoint/src/archive/git.rs push_remote")
}
