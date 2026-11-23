// Port of mj-worker-review/src/capture.rs.
//
// Worker-owned Git capture for push review.
//
// Names that collided with other mj-worker modules when this was part of
// the worker package keep this module's prefix: captureRun (Rust run).

package workerreview

import (
	"bytes"
	"crypto/sha256"
	"errors"
	"fmt"
	"os"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync/atomic"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/review"
)

// PushCapture is the committed work a push review covers.
type PushCapture struct {
	Delta      core.RepoDelta
	Base       PushBase
	HeadCommit string
}

// PushBase is where a push review's coverage starts. One of
// PushBaseReviewPoint, PushBaseMergeBase, PushBaseMergedReviewPoint,
// PushBaseSessionStart, PushBaseRoot. Variants are comparable values.
type PushBase interface {
	isPushBase()
	// BaseCommit is Rust's PushBase::commit: the commit coverage starts
	// after, or nil for PushBaseRoot.
	BaseCommit() *string
}

type PushBaseReviewPoint struct {
	Commit string
}

type PushBaseMergeBase struct {
	DefaultBranch string
	Commit        string
}

type PushBaseMergedReviewPoint struct {
	Commit        string
	MergeBase     string
	DefaultBranch string
}

type PushBaseSessionStart struct {
	Commit string
}

// PushBaseRoot: HEAD shares no history with the session start or a default
// branch, as on an orphan branch: every commit it holds is reviewed.
type PushBaseRoot struct{}

func (PushBaseReviewPoint) isPushBase()       {}
func (PushBaseMergeBase) isPushBase()         {}
func (PushBaseMergedReviewPoint) isPushBase() {}
func (PushBaseSessionStart) isPushBase()      {}
func (PushBaseRoot) isPushBase()              {}

func (b PushBaseReviewPoint) BaseCommit() *string       { return &b.Commit }
func (b PushBaseMergeBase) BaseCommit() *string         { return &b.Commit }
func (b PushBaseMergedReviewPoint) BaseCommit() *string { return &b.Commit }
func (b PushBaseSessionStart) BaseCommit() *string      { return &b.Commit }
func (PushBaseRoot) BaseCommit() *string                { return nil }

// PushCaptureRefusal is why committed work cannot be captured for push
// review. One of PushCaptureRefusalDirtyTracked, PushCaptureRefusalNoHead,
// PushCaptureRefusalGit. CapturePushableCommits returns it as its error.
type PushCaptureRefusal interface {
	error
	isPushCaptureRefusal()
}

type PushCaptureRefusalDirtyTracked struct {
	Files []string
}

type PushCaptureRefusalNoHead struct{}

// PushCaptureRefusalGit is Rust's From<anyhow::Error>: a Git failure.
type PushCaptureRefusalGit struct {
	Err error
}

func (PushCaptureRefusalDirtyTracked) isPushCaptureRefusal() {}
func (PushCaptureRefusalNoHead) isPushCaptureRefusal()       {}
func (PushCaptureRefusalGit) isPushCaptureRefusal()          {}

func (r PushCaptureRefusalDirtyTracked) Error() string {
	var text strings.Builder
	text.WriteString("commit or stash these tracked changes first:")
	for _, file := range r.Files {
		text.WriteString("\n  " + file)
	}
	return text.String()
}

func (PushCaptureRefusalNoHead) Error() string { return "the repository has no HEAD commit" }

func (r PushCaptureRefusalGit) Error() string { return r.Err.Error() }

func (r PushCaptureRefusalGit) Unwrap() error { return r.Err }

// CapturePushableCommits captures committed work only, using the last
// reviewed commit within the branch's current merge-base boundary. Its
// error is always a PushCaptureRefusal.
func CapturePushableCommits(runner checkpoint.GitCommandRunner, root string, sessionStartHead string) (PushCapture, error) {
	capture, err := capturePushableCommits(runner, root, sessionStartHead)
	if err == nil {
		return capture, nil
	}
	var refusal PushCaptureRefusal
	if errors.As(err, &refusal) {
		return PushCapture{}, refusal
	}
	return PushCapture{}, PushCaptureRefusalGit{Err: err}
}

func capturePushableCommits(runner checkpoint.GitCommandRunner, root string, sessionStartHead string) (PushCapture, error) {
	status, err := CheckedGit(runner, root, []string{"status", "--porcelain", "--untracked-files=no", "-z"})
	if err != nil {
		return PushCapture{}, err
	}
	entries := bytes.Split(status, []byte{0})
	var files []string
	for index := 0; index < len(entries); index++ {
		entry := entries[index]
		if len(entry) < 4 {
			continue
		}
		files = append(files, captureLossy(entry[3:]))
		// A rename or copy is followed by its original path.
		if slices.ContainsFunc(entry[:2], func(b byte) bool { return b == 'R' || b == 'C' }) {
			index++
		}
	}
	if len(files) > 0 {
		return PushCapture{}, PushCaptureRefusalDirtyTracked{Files: files}
	}
	resolved, err := resolveHead(runner, root)
	if err != nil {
		return PushCapture{}, err
	}
	if resolved == nil {
		return PushCapture{}, PushCaptureRefusalNoHead{}
	}
	head := *resolved
	var base PushBase = PushBaseRoot{}
	if shared, err := mergeBase(runner, root, sessionStartHead, head); err != nil {
		return PushCapture{}, err
	} else if shared != nil {
		base = PushBaseSessionStart{Commit: sessionStartHead}
	}
	branch, err := defaultBranch(runner, root)
	if err != nil {
		return PushCapture{}, err
	}
	if branch != nil {
		mb, err := mergeBase(runner, root, head, *branch)
		if err != nil {
			return PushCapture{}, err
		}
		if mb != nil && *mb != head {
			base = PushBaseMergeBase{DefaultBranch: *branch, Commit: *mb}
		}
	}
	// Git filters the reviewed refs to HEAD's history within the boundary,
	// then keeps the one that every other candidate leads to.
	filters := []string{"--merged=" + head}
	if boundary := base.BaseCommit(); boundary != nil {
		filters = append(filters, "--contains="+*boundary)
	}
	eligible, err := reviewedRefs(runner, root, filters)
	if err != nil {
		return PushCapture{}, err
	}
	commit, err := selectReviewedAncestor(runner, root, &eligible)
	if err != nil {
		return PushCapture{}, err
	}
	if commit != nil {
		base = PushBaseReviewPoint{Commit: *commit}
	}
	// Reviewed work merged with newer default-branch history: diff from Git's
	// merge of the two, so the reviewer sees only resolutions and later edits.
	var merged, pushReviewWarning *string
	if mergeBaseBase, ok := base.(PushBaseMergeBase); ok {
		eligible, err := reviewedRefs(runner, root, []string{"--merged=" + head, "--no-merged=" + mergeBaseBase.Commit})
		if err != nil {
			return PushCapture{}, err
		}
		reviewed, err := selectReviewedAncestor(runner, root, &eligible)
		if err != nil {
			return PushCapture{}, err
		}
		if reviewed != nil {
			version, err := checkedGitText(runner, root, []string{"version"})
			if err != nil {
				return PushCapture{}, err
			}
			found, parsed := core.ParseGitVersion(version)
			floor := core.MinimumGitVersion
			if parsed && core.CompareGitVersions(found, floor) >= 0 {
				tree, err := mergedReviewTree(runner, root, *reviewed, mergeBaseBase.Commit, head)
				if err != nil {
					return PushCapture{}, err
				}
				merged = &tree
			} else {
				trimmed := strings.TrimSpace(version)
				warning := fmt.Sprintf(
					"Pushed without review: Git %s in %s is older than %d.%d.%d or its version could not be read, so Mjolnir could not separate this merge's new changes from reviewed work. Upgrade Git to review merged work.",
					strings.TrimPrefix(trimmed, "git version "), root, floor[0], floor[1], floor[2])
				pushReviewWarning = &warning
			}
			base = PushBaseMergedReviewPoint{
				Commit:        *reviewed,
				MergeBase:     mergeBaseBase.Commit,
				DefaultBranch: mergeBaseBase.DefaultBranch,
			}
		}
	}
	var mergedIn []string
	if mergedBase, ok := base.(PushBaseMergedReviewPoint); ok {
		mergedIn = []string{mergedBase.MergeBase}
	}
	current, err := checkedGitText(runner, root, []string{"rev-parse", "--verify", head + "^{tree}"})
	if err != nil {
		return PushCapture{}, err
	}
	var baseline string
	switch commit := base.BaseCommit(); {
	case merged != nil:
		baseline = *merged
	case pushReviewWarning != nil:
		baseline = current
	case commit != nil:
		if baseline, err = checkedGitText(runner, root, []string{"rev-parse", "--verify", *commit + "^{tree}"}); err != nil {
			return PushCapture{}, err
		}
	default:
		if baseline, err = checkpoint.EmptyTreeID(runner, root); err != nil {
			return PushCapture{}, err
		}
	}
	patch, changed, err := captureDiff(runner, root, baseline, current)
	if err != nil {
		return PushCapture{}, err
	}
	delta := buildDelta(root, &baseline, current, patch, changed, mergedIn, nil)
	if commit := base.BaseCommit(); commit != nil {
		baseCommit := *commit
		delta.BaseCommit = &baseCommit
	}
	delta.PushReviewWarning = pushReviewWarning
	headCommit := head
	delta.HeadCommit = &headCommit
	var kind string
	switch base := base.(type) {
	case PushBaseReviewPoint:
		kind = "review point"
	case PushBaseMergeBase:
		kind = "merge base with " + base.DefaultBranch
	case PushBaseMergedReviewPoint:
		kind = "review point merged with " + base.DefaultBranch
	case PushBaseSessionStart:
		kind = "session start"
	case PushBaseRoot:
		kind = "no shared history"
	}
	delta.BaseKind = &kind
	return PushCapture{Delta: delta, Base: base, HeadCommit: head}, nil
}

// mergedReviewTree is the tree Git makes by merging the reviewed point with
// default-branch work, as the base for reviewing head.
//
// A text conflict leaves markers in that tree, so the diff to head shows its
// resolution. A binary, modify/delete or submodule conflict leaves one
// side's version instead; when head resolved it to that same version, the
// diff would show nothing. Such paths are removed from the base, so the
// reviewer sees each in full.
func mergedReviewTree(runner checkpoint.GitCommandRunner, root, reviewed, mergeBaseCommit, head string) (string, error) {
	output, err := captureRun(runner, root, []string{
		"merge-tree", "--write-tree", "--allow-unrelated-histories", "--no-messages", "--name-only", "-z",
		reviewed, mergeBaseCommit,
	})
	if err != nil {
		return "", err
	}
	// Status 1 means conflicts; the tree is still written.
	if output.Status != 0 && output.Status != 1 {
		return "", fmt.Errorf("git merge-tree failed with status %d: %s", output.Status, captureLossy(output.Stderr))
	}
	// Paths are raw bytes: a file name need not be UTF-8.
	var fields [][]byte
	for _, field := range bytes.Split(output.Stdout, []byte{0}) {
		if len(field) > 0 {
			fields = append(fields, field)
		}
	}
	if len(fields) == 0 {
		return "", errors.New("git merge-tree printed no tree id")
	}
	tree, err := captureFromUTF8(fields[0])
	if err != nil {
		return "", err
	}
	var conflicted []string
	for _, field := range fields[1:] {
		path, err := core.FromGitBytes(field)
		if err != nil {
			return "", err
		}
		conflicted = append(conflicted, path)
	}
	slices.Sort(conflicted)
	conflicted = slices.Compact(conflicted)
	if len(conflicted) == 0 {
		return tree, nil
	}
	mergedEntries, err := treeEntries(runner, root, tree, conflicted)
	if err != nil {
		return "", err
	}
	resolved, err := treeEntries(runner, root, head, conflicted)
	if err != nil {
		return "", err
	}
	var hidden []string
	for _, path := range conflicted {
		inMerge, mergedHas := mergedEntries[path]
		inHead, headHas := resolved[path]
		if mergedHas == headHas && bytes.Equal(inMerge, inHead) {
			hidden = append(hidden, path)
		}
	}
	if len(hidden) == 0 {
		return tree, nil
	}
	return checkpoint.TreeWithoutPaths(runner, root, tree, hidden)
}

// treeEntries is the mode and object of each of paths (sorted, as Rust's
// BTreeSet) that treeIsh holds, keyed by path.
func treeEntries(runner checkpoint.GitCommandRunner, root, treeIsh string, paths []string) (map[string][]byte, error) {
	arguments := append([]string{"--literal-pathspecs", "ls-tree", "-r", "-z", treeIsh, "--"}, paths...)
	output, err := runner.Run(root, &checkpoint.GitCommand{Arguments: arguments})
	if err != nil {
		return nil, err
	}
	if output.Status != 0 {
		return nil, fmt.Errorf("git ls-tree failed with status %d: %s", output.Status, captureLossy(output.Stderr))
	}
	entries := map[string][]byte{}
	for _, record := range bytes.Split(output.Stdout, []byte{0}) {
		tab := bytes.IndexByte(record, '\t')
		if tab < 0 {
			continue
		}
		path, err := core.FromGitBytes(record[tab+1:])
		if err != nil {
			return nil, err
		}
		entries[path] = slices.Clone(record[:tab])
	}
	return entries, nil
}

// defaultBranch is origin's default branch, or the first of the usual
// default branches that exists; nil when there is none.
func defaultBranch(runner checkpoint.GitCommandRunner, root string) (*string, error) {
	remote, err := captureRun(runner, root, []string{"symbolic-ref", "-q", "refs/remotes/origin/HEAD"})
	if err != nil {
		return nil, err
	}
	switch remote.Status {
	case 0:
		text, err := captureFromUTF8(remote.Stdout)
		if err != nil {
			return nil, err
		}
		branch := strings.TrimSpace(text)
		return &branch, nil
	case 1:
	default:
		return nil, fmt.Errorf("read origin's default branch: %s", captureLossy(remote.Stderr))
	}
	for _, branch := range []string{
		"refs/remotes/origin/main",
		"refs/remotes/origin/master",
		"refs/heads/main",
		"refs/heads/master",
	} {
		commit, err := resolveCommit(runner, root, branch)
		if err != nil {
			return nil, err
		}
		if commit != nil {
			return &branch, nil
		}
	}
	return nil, nil
}

// mergeBase is the best common ancestor of two commits, or nil when they
// share no history.
func mergeBase(runner checkpoint.GitCommandRunner, root, one, other string) (*string, error) {
	output, err := captureRun(runner, root, []string{"merge-base", one, other})
	if err != nil {
		return nil, err
	}
	switch output.Status {
	case 0:
		text, err := captureFromUTF8(output.Stdout)
		if err != nil {
			return nil, err
		}
		base := strings.TrimSpace(text)
		return &base, nil
	case 1:
		return nil, nil
	}
	return nil, fmt.Errorf("find a merge base: %s", captureLossy(output.Stderr))
}

// CheckedGit runs git in root and returns its standard output, failing
// when git exits unsuccessfully.
func CheckedGit(runner checkpoint.GitCommandRunner, root string, args []string) ([]byte, error) {
	output, err := captureRun(runner, root, args)
	if err != nil {
		return nil, err
	}
	if output.Status != 0 {
		return nil, fmt.Errorf("git %s: %s", strings.Join(args, " "), captureLossy(output.Stderr))
	}
	return output.Stdout, nil
}

// SetReviewBase makes revision the base of the next push review of root.
// The commit's tree must already be recorded as reviewed or overridden,
// which is what lets the agent move the base only to work a review has
// seen. Returns the commit and tree.
func SetReviewBase(runner checkpoint.GitCommandRunner, root string, revision string) (commit string, tree string, err error) {
	if revision == "" || strings.HasPrefix(revision, "-") {
		return "", "", fmt.Errorf("%s is not a commit; pass a commit hash from a review verdict", strconv.Quote(revision))
	}
	resolve := func(suffix string) (string, error) {
		resolved, err := checkedGitText(runner, root, []string{"rev-parse", "--verify", "--quiet", revision + suffix})
		if err != nil {
			return "", fmt.Errorf("%s is not a commit in %s; pass a commit hash from a review verdict", revision, root)
		}
		return resolved, nil
	}
	if commit, err = resolve("^{commit}"); err != nil {
		return "", "", err
	}
	if tree, err = resolve("^{tree}"); err != nil {
		return "", "", err
	}
	recorded, err := checkedGitText(runner, root, []string{
		"for-each-ref", "--format=%(refname)", "refs/mj/reviewed/" + tree, "refs/mj/overridden/" + tree,
	})
	if err != nil {
		return "", "", err
	}
	if recorded == "" {
		return "", "", fmt.Errorf("commit %s was never reviewed or overridden in %s; call request_review first, then call set_baseline once the verdict is clean or you have overridden it", commit, root)
	}
	if err := RecordReviewBase(runner, root, tree, commit); err != nil {
		return "", "", err
	}
	return commit, tree, nil
}

// RecordReviewBase records a clean review or an accepted explicit baseline
// in one transaction.
func RecordReviewBase(runner checkpoint.GitCommandRunner, root, tree, commit string) error {
	selected, err := worktreeReviewBaseRef(runner, root)
	if err != nil {
		return err
	}
	output, err := runner.Run(root, &checkpoint.GitCommand{
		Arguments: []string{"update-ref", "--stdin"},
		Stdin:     []byte(fmt.Sprintf("update refs/mj/reviewed/%s %s\nupdate %s %s\n", tree, commit, selected, commit)),
	})
	if err != nil {
		return err
	}
	if output.Status != 0 {
		return fmt.Errorf("set the review base: %s", captureLossy(output.Stderr))
	}
	return nil
}

func worktreeReviewBaseRef(runner checkpoint.GitCommandRunner, root string) (string, error) {
	scope, err := checkpoint.SessionStartScope(runner, root)
	if err != nil {
		return "", err
	}
	return checkpoint.PushReviewBaseRef(scope), nil
}

// ReviewedRefs (Rust's private struct keeps its name: the function
// reviewed_refs takes reviewedRefs) are reviewed commits and this
// worktree's selected base among them.
type ReviewedRefs struct {
	commits  []string
	selected *string
}

// reviewedRefs is the reviewed commits under refs/mj/reviewed/ that match
// the given for-each-ref filters, including this worktree's selected base.
func reviewedRefs(runner checkpoint.GitCommandRunner, root string, filters []string) (ReviewedRefs, error) {
	selected, err := worktreeReviewBaseRef(runner, root)
	if err != nil {
		return ReviewedRefs{}, err
	}
	args := []string{"for-each-ref", "--format=%(objectname) %(refname)"}
	args = append(args, filters...)
	args = append(args, "refs/mj/reviewed/", selected)
	listed, err := checkedGitText(runner, root, args)
	if err != nil {
		return ReviewedRefs{}, err
	}
	var refs ReviewedRefs
	for _, line := range captureLines(listed) {
		commit, name, ok := strings.Cut(line, " ")
		if !ok {
			return ReviewedRefs{}, errors.New("read the reviewed ref's name")
		}
		refs.commits = append(refs.commits, commit)
		if name == selected {
			refs.selected = &commit
		}
	}
	return refs, nil
}

// selectReviewedAncestor advances when every candidate leads to one
// reviewed commit. When reviewed histories are incomparable, it retains the
// worktree's eligible selected base.
func selectReviewedAncestor(runner checkpoint.GitCommandRunner, root string, candidates *ReviewedRefs) (*string, error) {
	if len(candidates.commits) == 0 {
		return nil, nil
	}
	tips, err := checkedGitText(runner, root, append([]string{"merge-base", "--independent"}, candidates.commits...))
	if err != nil {
		return nil, err
	}
	if lines := captureLines(tips); len(lines) == 1 {
		return &lines[0], nil
	}
	return candidates.selected, nil
}

// LaterReviewBase is the newest reviewed commit after revision in HEAD's
// history, with its tree; found is false when there is none.
func LaterReviewBase(runner checkpoint.GitCommandRunner, root string, revision string) (commit string, tree string, found bool, err error) {
	if revision == "" || strings.HasPrefix(revision, "-") {
		return "", "", false, errors.New("invalid review commit")
	}
	if commit, err = checkedGitText(runner, root, []string{"rev-parse", "--verify", revision + "^{commit}"}); err != nil {
		return "", "", false, err
	}
	head, err := checkedGitText(runner, root, []string{"rev-parse", "HEAD"})
	if err != nil {
		return "", "", false, err
	}
	later, err := reviewedRefs(runner, root, []string{"--contains=" + commit, "--merged=" + head})
	if err != nil {
		return "", "", false, err
	}
	candidate, err := selectReviewedAncestor(runner, root, &later)
	if err != nil {
		return "", "", false, err
	}
	if candidate == nil || *candidate == commit {
		return "", "", false, nil
	}
	if tree, err = checkedGitText(runner, root, []string{"rev-parse", *candidate + "^{tree}"}); err != nil {
		return "", "", false, err
	}
	return *candidate, tree, true, nil
}

func checkedGitText(runner checkpoint.GitCommandRunner, root string, args []string) (string, error) {
	output, err := CheckedGit(runner, root, args)
	if err != nil {
		return "", err
	}
	text, err := captureFromUTF8(output)
	if err != nil {
		return "", err
	}
	return strings.TrimSpace(text), nil
}

func captureDiff(runner checkpoint.GitCommandRunner, root, baseline, current string) (string, []core.FileLineChange, error) {
	patch, err := checkpoint.DiffBetweenTrees(runner, root, &baseline, current)
	if err != nil {
		return "", nil, fmt.Errorf("diff the captured trees of %s: %w", root, err)
	}
	numstat, err := checkpoint.NumstatBetweenTrees(runner, root, baseline, current)
	if err != nil {
		return "", nil, fmt.Errorf("count the changed lines of %s: %w", root, err)
	}
	return patch, parseNumstat(numstat), nil
}

func buildDelta(root string, baselineTree *string, currentTree string, patch string, files []core.FileLineChange, mergedIn []string, unattributed *string) core.RepoDelta {
	summary := review.RawDiffSummaryFromPatch(patch)
	return core.RepoDelta{
		Root:         root,
		BaselineTree: baselineTree,
		CurrentTree:  currentTree,
		Patch:        review.BoundReviewSection(patch, review.ReviewCaptureDiffLimit, "workspace diff"),
		Diffstat:     summary.Diffstat(),
		ChangedLines: summary.ChangedLineCount(),
		Files:        files,
		MergedIn:     mergedIn,
		Unattributed: unattributed,
	}
}

// resolveHead is the commit HEAD names, or nil on an unborn branch.
func resolveHead(git checkpoint.GitCommandRunner, repository string) (*string, error) {
	return resolveCommit(git, repository, "HEAD")
}

func resolveCommit(git checkpoint.GitCommandRunner, repository, reference string) (*string, error) {
	output, err := captureRun(git, repository, []string{"rev-parse", "--verify", "--quiet", reference + "^{commit}"})
	if err != nil {
		return nil, err
	}
	switch output.Status {
	case 0:
		commit := strings.TrimSpace(captureLossy(output.Stdout))
		return &commit, nil
	case 1:
		return nil, nil
	}
	return nil, fmt.Errorf("resolving %s of %s failed with status %d: %s",
		reference, repository, output.Status, strings.TrimSpace(captureLossy(output.Stderr)))
}

// captureRun is Rust's review::capture::run (prefixed: acp's run keeps the
// name). It runs one Git command in repository with no input.
func captureRun(git checkpoint.GitCommandRunner, repository string, arguments []string) (checkpoint.GitOutput, error) {
	return git.Run(repository, &checkpoint.GitCommand{Arguments: append([]string(nil), arguments...)})
}

// parseNumstat reads `git diff --numstat -z -M` output: `added\tdeleted\tpath\0`
// per file, or `added\tdeleted\t\0old path\0new path\0` for a rename or copy,
// with `-` for both counts on a binary file.
func parseNumstat(output []byte) []core.FileLineChange {
	fields := bytes.Split(output, []byte{0})
	var files []core.FileLineChange
	for index := 0; index < len(fields); index++ {
		record := fields[index]
		if len(record) == 0 {
			continue
		}
		parts := strings.SplitN(captureLossy(record), "\t", 3)
		if len(parts) < 3 {
			continue
		}
		added, deleted, path := parts[0], parts[1], parts[2]
		binary := added == "-" && deleted == "-"
		var oldPath *string
		if path == "" {
			if index+1 < len(fields) {
				index++
				old := captureLossy(fields[index])
				oldPath = &old
			}
			if index+1 < len(fields) {
				index++
				path = captureLossy(fields[index])
			}
		}
		files = append(files, core.FileLineChange{
			Path:       path,
			OldPath:    oldPath,
			Insertions: captureCount(added),
			Deletions:  captureCount(deleted),
			Binary:     binary,
		})
	}
	return files
}

// captureCount is a numstat count, or 0 for `-` or anything else that is not
// a count (Rust: parse().unwrap_or(0)).
func captureCount(text string) int {
	count, err := strconv.ParseUint(strings.TrimPrefix(text, "+"), 10, strconv.IntSize-1)
	if err != nil {
		return 0
	}
	return int(count)
}

func sessionStartRef(runner checkpoint.GitCommandRunner, root, session string) (string, error) {
	baseline, err := checkpoint.SessionStartScope(runner, root)
	if err != nil {
		return "", err
	}
	sessionDigest := sha256.Sum256([]byte(session))
	prefix := "refs/mj/session-start/" + core.LowerHex(sessionDigest[:])
	baselineDigest := sha256.Sum256([]byte(baseline))
	reference := prefix + "/" + core.LowerHex(baselineDigest[:])
	legacyDigest := sha256.Sum256([]byte(strings.Replace(baseline, "refs/mj/", "refs/hel/", 1)))
	legacy := prefix + "/" + core.LowerHex(legacyDigest[:])
	if err := checkpoint.MigrateReviewRef(runner, root, legacy, reference); err != nil {
		return "", err
	}
	return reference, nil
}

// SessionStartHead reads the immutable start commit, if this checkout has
// recorded one. Legacy checkouts and moves that omit private refs return
// nil.
func SessionStartHead(runner checkpoint.GitCommandRunner, root, session string) (*string, error) {
	reference, err := sessionStartRef(runner, root, session)
	if err != nil {
		return nil, err
	}
	return resolveCommit(runner, root, reference)
}

// LinkedWorktreeStart is where push coverage starts in a linked worktree the
// session did not start in: the fork point of its HEAD with the session's
// start in origin, a workspace checkout of the same repository. Pinning the
// worktree's own start at its current HEAD would leave commits made before
// its first review with nothing to review when no default branch bounds
// them. Without a shared history the session start is returned as is, and
// the capture reviews every commit HEAD holds.
func LinkedWorktreeStart(runner checkpoint.GitCommandRunner, worktree, origin, session string) (*string, error) {
	head, err := resolveHead(runner, worktree)
	if err != nil {
		return nil, err
	}
	start, err := EnsureSessionStartHead(runner, origin, session)
	if err != nil {
		return nil, err
	}
	if head == nil || start == nil {
		return EnsureSessionStartHead(runner, worktree, session)
	}
	base, err := mergeBase(runner, worktree, *head, *start)
	if err != nil || base != nil {
		return base, err
	}
	return start, nil
}

// ReviewStartHead is where a review's coverage starts. A recorded session
// start wins. Without one, the merge base with the default branch bounds the
// session's work when HEAD has moved past it, and then the newest reviewed
// commit in HEAD's history does. Otherwise nothing tells the session's
// commits from older history: the current HEAD is recorded as the start so
// later commits can be reviewed, and this review is refused instead of
// reporting the session's earlier commits as reviewed.
func ReviewStartHead(runner checkpoint.GitCommandRunner, root, session string) (*string, error) {
	if start, err := SessionStartHead(runner, root, session); err != nil || start != nil {
		return start, err
	}
	head, err := resolveHead(runner, root)
	if err != nil || head == nil {
		return nil, err
	}
	branch, err := defaultBranch(runner, root)
	if err != nil {
		return nil, err
	}
	if branch != nil {
		base, err := mergeBase(runner, root, *head, *branch)
		if err != nil {
			return nil, err
		}
		if base != nil && *base != *head {
			return base, nil
		}
	}
	// An earlier review of HEAD's history bounds the work that follows it.
	reviewed, err := reviewedRefs(runner, root, []string{"--merged=" + *head})
	if err != nil {
		return nil, err
	}
	if ancestor, err := selectReviewedAncestor(runner, root, &reviewed); err != nil || ancestor != nil {
		return ancestor, err
	}
	if _, err := EnsureSessionStartHead(runner, root, session); err != nil {
		return nil, err
	}
	return nil, fmt.Errorf("No starting point was recorded for this session's work in %s, so its commits cannot be told apart from earlier history. Mjolnir now records the current commit as the start; a later review covers the commits made after it.", root)
}

// EnsureSessionStartHead preserves a recorded start, or starts coverage at
// the current HEAD when an older session or a restored checkout has no start
// ref.
func EnsureSessionStartHead(runner checkpoint.GitCommandRunner, root, session string) (*string, error) {
	if start, err := SessionStartHead(runner, root, session); err != nil || start != nil {
		return start, err
	}
	head, err := resolveHead(runner, root)
	if err != nil || head == nil {
		return nil, err
	}
	reference, err := sessionStartRef(runner, root, session)
	if err != nil {
		return nil, err
	}
	// An empty old value creates the ref only if it does not exist.
	if _, err := CheckedGit(runner, root, []string{"update-ref", reference, *head, ""}); err != nil {
		// A concurrent initializer may have published the start first.
		start, startErr := SessionStartHead(runner, root, session)
		if startErr != nil || start != nil {
			return start, startErr
		}
		return nil, err
	}
	return head, nil
}

// DiscoverRepositories is the workspace repositories a review covers: the
// session's working directory and its additional roots, each resolved to the
// Git repository that contains it. A directory that is not in a repository
// is skipped, and two roots inside one repository collapse to a single
// entry.
func DiscoverRepositories(git checkpoint.GitCommandRunner, roots []string) []string {
	var discovered []string
	for _, root := range roots {
		toplevel, ok := RepositoryRoot(git, root)
		if ok && !captureContainsPath(discovered, toplevel) {
			discovered = append(discovered, toplevel)
		}
	}
	return discovered
}

// RepositoryRoot is Rust's capture::repository_root: the top level of the
// repository containing directory.
func RepositoryRoot(git checkpoint.GitCommandRunner, directory string) (string, bool) {
	if info, err := os.Stat(directory); err != nil || !info.IsDir() {
		return "", false
	}
	output, err := captureRun(git, directory, []string{"rev-parse", "--show-toplevel"})
	if err != nil || output.Status != 0 {
		return "", false
	}
	text, err := captureFromUTF8(output.Stdout)
	trimmed := strings.TrimSpace(text)
	if err != nil || trimmed == "" {
		return "", false
	}
	return trimmed, true
}

// WorkspaceStateTimeout bounds each Git command of push-hook preparation.
const WorkspaceStateTimeout = 120 * time.Second

// BoundedGit is a Git runner that gives every command a deadline.
//
// Push-hook preparation uses a deadline so a stuck Git command reports a
// failure.
type BoundedGit struct {
	timeout  time.Duration
	executor core.CancellableProcessExecutor
}

func NewBoundedGit(timeout time.Duration) *BoundedGit {
	return &BoundedGit{timeout: timeout, executor: core.NewCancellableProcessExecutor(new(atomic.Bool))}
}

// CancellationGuard cancels this runner's commands when the caller's wait
// ends; call Cancel on every exit path (Rust: Drop).
func (g *BoundedGit) CancellationGuard() *core.ProcessCancellationGuard {
	return g.executor.CancelOnDrop()
}

func (g *BoundedGit) Run(repository string, command *checkpoint.GitCommand) (checkpoint.GitOutput, error) {
	spec := core.NewCommandSpec("git", command.Arguments).WithPurpose("prepare push review")
	cwd := repository
	spec.Cwd = &cwd
	for _, pair := range checkpoint.NonInteractiveGitEnv {
		spec.Env[pair[0]] = pair[1]
	}
	for _, pair := range command.Env {
		spec.Env[pair[0]] = pair[1]
	}
	// The executor's pipe handler feeds input while it drains output, and
	// gives up on it at the deadline like any other wait.
	if len(command.Stdin) > 0 {
		spec = spec.WithSensitiveStdin(slices.Clone(command.Stdin))
	}
	executor := g.executor.WithDeadline(g.timeout)
	output, err := executor.Execute(&spec)
	if err != nil {
		if executor.IsCancelled() && !g.executor.IsCancelled() {
			return checkpoint.GitOutput{}, &core.CommandTimedOut{Program: spec.Program, Purpose: spec.Purpose, Timeout: g.timeout}
		}
		return checkpoint.GitOutput{}, err
	}
	return checkpoint.GitOutput{Status: output.Status, Stdout: output.Stdout, Stderr: output.Stderr}, nil
}

// capturePathStartsWith is Rust's Path::starts_with: base's components are
// the leading components of path.
func capturePathStartsWith(path, base string) bool {
	components, prefix := capturePathComponents(path), capturePathComponents(base)
	return len(prefix) <= len(components) && slices.Equal(components[:len(prefix)], prefix)
}

// captureContainsPath is Vec<PathBuf>::contains: paths compared by
// component.
func captureContainsPath(paths []string, path string) bool {
	wanted := capturePathComponents(path)
	return slices.ContainsFunc(paths, func(candidate string) bool {
		return slices.Equal(capturePathComponents(candidate), wanted)
	})
}

// capturePathComponents is a path's components as Rust's Path::components
// yields them: the root as "/", "." dropped except at the start, ".." kept.
func capturePathComponents(path string) []string {
	separator := func(r rune) bool { return r == '/' || (runtime.GOOS == "windows" && r == '\\') }
	var components []string
	if path != "" && separator(rune(path[0])) {
		components = append(components, "/")
	}
	for index, part := range strings.FieldsFunc(path, separator) {
		if part == "." && !(index == 0 && components == nil && strings.HasPrefix(path, ".")) {
			continue
		}
		components = append(components, part)
	}
	return components
}

// captureLines is Rust's str::lines: lines split at \n with a trailing \r
// removed, and no empty last line for a final newline.
func captureLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(text, "\n"), "\n")
	for i, line := range lines {
		lines[i] = strings.TrimSuffix(line, "\r")
	}
	return lines
}

// captureFromUTF8 is Rust's String::from_utf8, failing with the text of
// Rust's Utf8Error.
func captureFromUTF8(data []byte) (string, error) {
	for index := 0; index < len(data); {
		valid, complete := captureUTF8Sequence(data[index:])
		switch {
		case complete:
			index += valid
		case valid > 0 && index+valid == len(data):
			return "", fmt.Errorf("incomplete utf-8 byte sequence from index %d", index)
		default:
			return "", fmt.Errorf("invalid utf-8 sequence of %d bytes from index %d", max(valid, 1), index)
		}
	}
	return string(data), nil
}

// captureLossy is Rust's String::from_utf8_lossy: each maximal invalid
// subsequence becomes one U+FFFD.
func captureLossy(data []byte) string {
	if utf8.Valid(data) {
		return string(data)
	}
	var text strings.Builder
	for len(data) > 0 {
		valid, complete := captureUTF8Sequence(data)
		if complete {
			text.Write(data[:valid])
			data = data[valid:]
			continue
		}
		text.WriteRune(utf8.RuneError)
		data = data[max(valid, 1):]
	}
	return text.String()
}

// captureUTF8Sequence measures the UTF-8 sequence text starts with: valid
// is how many of its leading bytes form a valid sequence or a valid prefix
// of one, and complete whether that is a whole character. An invalid first
// byte gives zero.
func captureUTF8Sequence(text []byte) (valid int, complete bool) {
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
