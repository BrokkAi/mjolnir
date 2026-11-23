// Port of the tests in mj-worker-review/src/capture.rs (push_tests).

//go:build unix

package workerreview

import (
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
)

// versionGit reports a fixed `git version` and runs every other command
// with the host's Git.
type versionGit struct {
	version string
}

func (g versionGit) Run(root string, command *checkpoint.GitCommand) (checkpoint.GitOutput, error) {
	if len(command.Arguments) == 1 && command.Arguments[0] == "version" {
		return checkpoint.GitOutput{Status: 0, Stdout: []byte("git version " + g.version + "\n")}, nil
	}
	return checkpoint.SystemGit{}.Run(root, command)
}

// pushTestRepository is push_tests::repository: a temporary directory
// holding a bare origin.git and its clone, checkout, with one pushed commit
// on main and origin's HEAD set.
func pushTestRepository(t *testing.T) string {
	t.Helper()
	temp := t.TempDir()
	origin := filepath.Join(temp, "origin.git")
	root := filepath.Join(temp, "checkout")
	if err := os.Mkdir(origin, 0o777); err != nil {
		t.Fatal(err)
	}
	testsGit(origin, "init", "--bare", "-q", "-b", "main")
	testsGit(temp, "clone", "-q", origin, root)
	testsGit(root, "config", "user.name", "Review Test")
	testsGit(root, "config", "user.email", "review@example.test")
	pushTestCommit(t, root, "base")
	testsGit(root, "push", "-q", "origin", "main")
	testsGit(root, "remote", "set-head", "origin", "main")
	return temp
}

// pushTestCommit is push_tests::commit: it writes text to tracked.txt,
// commits it and returns the new HEAD.
func pushTestCommit(t *testing.T, root, text string) string {
	t.Helper()
	if err := os.WriteFile(filepath.Join(root, "tracked.txt"), []byte(text+"\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	testsGit(root, "add", "tracked.txt")
	testsGit(root, "commit", "-qm", text)
	return testsGit(root, "rev-parse", "HEAD")
}

func pushTestCapture(t *testing.T, root, start string) PushCapture {
	t.Helper()
	capture, err := CapturePushableCommits(checkpoint.SystemGit{}, root, start)
	if err != nil {
		t.Fatal(err)
	}
	return capture
}

func pushTestWrite(t *testing.T, path, contents string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(contents), 0o666); err != nil {
		t.Fatal(err)
	}
}

func pushTestRecordReviewed(t *testing.T, root, tree, commit string) {
	t.Helper()
	if err := checkpoint.RecordReviewedTree(checkpoint.SystemGit{}, root, tree, commit); err != nil {
		t.Fatal(err)
	}
}

// pushTestAmendLater amends HEAD with a committer date far in the future.
func pushTestAmendLater(t *testing.T, root string) {
	t.Helper()
	amended, err := checkpoint.SystemGit{}.Run(root, &checkpoint.GitCommand{
		Arguments: []string{"commit", "--amend", "--no-edit", "-q"},
		Env:       [][2]string{{"GIT_COMMITTER_DATE", "2030-01-01T00:00:00Z"}},
	})
	if err != nil || amended.Status != 0 {
		t.Fatalf("amend: %v %+v", err, amended)
	}
}

// pushTestMergeStatus runs `git merge main` and returns its exit status.
func pushTestMergeStatus(t *testing.T, root string) int {
	t.Helper()
	output, err := checkpoint.SystemGit{}.Run(root, &checkpoint.GitCommand{Arguments: []string{"merge", "main"}})
	if err != nil {
		t.Fatal(err)
	}
	return output.Status
}

func pushTestFilePaths(capture PushCapture) []string {
	var paths []string
	for _, file := range capture.Delta.Files {
		paths = append(paths, file.Path)
	}
	return paths
}

func pushTestBaseCommit(base PushBase) string {
	if commit := base.BaseCommit(); commit != nil {
		return *commit
	}
	return "<none>"
}

func TestPushCaptureRefusesDirtyTrackedPathsAndIgnoresUntracked(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	head := testsGit(root, "rev-parse", "HEAD")
	pushTestWrite(t, filepath.Join(root, "untracked"), "ignored")
	if patch := pushTestCapture(t, root, head).Delta.Patch; patch != "" {
		t.Fatalf("patch %q", patch)
	}
	testsGit(root, "mv", "tracked.txt", "renamed with spaces.txt")
	_, err := CapturePushableCommits(checkpoint.SystemGit{}, root, head)
	var dirty PushCaptureRefusalDirtyTracked
	if !errors.As(err, &dirty) || !slices.Equal(dirty.Files, []string{"renamed with spaces.txt"}) {
		t.Fatalf("got %v", err)
	}
}

func TestPushCaptureUsesOriginDefaultMergeBaseAndSharedDiffSummary(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	base := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "checkout", "-qb", "feature")
	head := pushTestCommit(t, root, "feature")
	capture := pushTestCapture(t, root, head)
	if want := (PushBaseMergeBase{DefaultBranch: "refs/remotes/origin/main", Commit: base}); capture.Base != PushBase(want) {
		t.Fatalf("base %#v", capture.Base)
	}
	if capture.HeadCommit != head {
		t.Fatalf("head %s", capture.HeadCommit)
	}
	if baseline := capture.Delta.BaselineTree; baseline == nil || *baseline != testsGit(root, "rev-parse", base+"^{tree}") {
		t.Fatalf("baseline %v", baseline)
	}
	if !strings.Contains(capture.Delta.Patch, "+feature") {
		t.Fatal(capture.Delta.Patch)
	}
	if capture.Delta.ChangedLines != 2 {
		t.Fatalf("changed lines %d", capture.Delta.ChangedLines)
	}
	if capture.Delta.Files[0].Path != "tracked.txt" {
		t.Fatalf("files %+v", capture.Delta.Files)
	}
}

func TestPushCaptureUsesReviewedDescendantAndHandlesHeadAsBase(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "checkout", "-qb", "feature")
	reviewed := pushTestCommit(t, root, "reviewed")
	tree := testsGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)
	if got := testsGit(root, "rev-parse", "refs/mj/reviewed/"+tree); got != reviewed {
		t.Fatalf("reviewed ref %s", got)
	}
	empty := pushTestCapture(t, root, start)
	if empty.Base != PushBase(PushBaseReviewPoint{Commit: reviewed}) {
		t.Fatalf("base %#v", empty.Base)
	}
	if empty.Delta.Patch != "" || len(empty.Delta.Files) != 0 || empty.Delta.ChangedLines != 0 {
		t.Fatalf("delta %+v", empty.Delta)
	}
	pushTestCommit(t, root, "later")
	if got := pushTestBaseCommit(pushTestCapture(t, root, start).Base); got != reviewed {
		t.Fatalf("base %s", got)
	}
}

// An overridden tree is pushable but is not a review point, so the next
// review still covers the whole change.
func TestPushCaptureIgnoresOverriddenCommitsAsABase(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "checkout", "-qb", "feature")
	overridden := pushTestCommit(t, root, "overridden")
	tree := testsGit(root, "rev-parse", "HEAD^{tree}")
	if err := checkpoint.RecordOverriddenTree(checkpoint.SystemGit{}, root, tree, overridden); err != nil {
		t.Fatal(err)
	}
	pushTestCommit(t, root, "fix")
	capture := pushTestCapture(t, root, start)
	if got := pushTestBaseCommit(capture.Base); got != start {
		t.Fatalf("the base stays at the merge base, not the overridden commit: %s", got)
	}
	if !strings.Contains(capture.Delta.Patch, "+overridden") && !strings.Contains(capture.Delta.Patch, "+fix") {
		t.Fatal(capture.Delta.Patch)
	}
	if listed := testsGit(root, "for-each-ref", "refs/mj/reviewed/"); listed != "" {
		t.Fatal(listed)
	}
}

func TestSetReviewBaseAcceptsReviewedAndOverriddenTreesAndRefusesOthers(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "checkout", "-qb", "feature")
	reviewed := pushTestCommit(t, root, "reviewed")
	reviewedTree := testsGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, reviewedTree, reviewed)
	pushTestCommit(t, root, "overridden")
	// Review points are ordered by committer date, which has one-second
	// resolution; give the overridden commit a clearly later one.
	pushTestAmendLater(t, root)
	overridden := testsGit(root, "rev-parse", "HEAD")
	overriddenTree := testsGit(root, "rev-parse", "HEAD^{tree}")
	if err := checkpoint.RecordOverriddenTree(checkpoint.SystemGit{}, root, overriddenTree, overridden); err != nil {
		t.Fatal(err)
	}
	unreviewed := pushTestCommit(t, root, "unreviewed")

	refused := func(revision, want string) {
		t.Helper()
		_, _, err := SetReviewBase(checkpoint.SystemGit{}, root, revision)
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Fatalf("%s: %v", revision, err)
		}
	}
	refused(unreviewed, "call request_review first")
	refused("no-such-revision", "is not a commit")
	refused("--output=x", "is not a commit")
	if listed := testsGit(root, "for-each-ref", "refs/mj/reviewed/"); len(captureLines(listed)) != 1 {
		t.Fatal(listed)
	}

	// A rejected explicit-ref write must not promote an overridden tree.
	testsGit(root, "update-ref", "refs/mj/push-base", reviewed)
	refused(overridden, "set the review base")
	if listed := testsGit(root, "for-each-ref", "--format=%(objectname)", "refs/mj/reviewed/"); listed != reviewed {
		t.Fatal(listed)
	}
	testsGit(root, "update-ref", "-d", "refs/mj/push-base")

	// Any revision works: HEAD~1 is the overridden commit. It becomes a
	// review point and the next capture starts there.
	commit, tree, err := SetReviewBase(checkpoint.SystemGit{}, root, "HEAD~1")
	if err != nil || commit != overridden || tree != overriddenTree {
		t.Fatalf("%s %s %v", commit, tree, err)
	}
	if got := testsGit(root, "rev-parse", "refs/mj/reviewed/"+overriddenTree); got != overridden {
		t.Fatal(got)
	}
	if got := pushTestBaseCommit(pushTestCapture(t, root, start).Base); got != overridden {
		t.Fatal(got)
	}
	// An abbreviated hash names a reviewed commit as well.
	if commit, _, err := SetReviewBase(checkpoint.SystemGit{}, root, reviewed[:10]); err != nil || commit != reviewed {
		t.Fatalf("%s %v", commit, err)
	}
}

func TestPushCaptureOrdersReviewedAncestorsByAncestry(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "checkout", "-qb", "feature")
	pushTestCommit(t, root, "reviewed with later clock")
	pushTestAmendLater(t, root)
	newerDate := testsGit(root, "rev-parse", "HEAD")
	tree := testsGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, newerDate)
	later := pushTestCommit(t, root, "reviewed with earlier clock")
	tree = testsGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, later)
	if got := pushTestBaseCommit(pushTestCapture(t, root, start).Base); got != later {
		t.Fatal(got)
	}
	pushTestAmendLater(t, root)
	later = testsGit(root, "rev-parse", "HEAD")
	pushTestRecordReviewed(t, root, tree, later)
	if got := pushTestBaseCommit(pushTestCapture(t, root, start).Base); got != later {
		t.Fatal(got)
	}
	testsGit(root, "checkout", "-qb", "sibling", start)
	sibling := pushTestCommit(t, root, "not an ancestor")
	tree = testsGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, sibling)
	testsGit(root, "checkout", "-q", "feature")
	if got := pushTestBaseCommit(pushTestCapture(t, root, start).Base); got != later {
		t.Fatal(got)
	}
}

func TestPushCaptureIgnoresReviewedHistoryBeforeCurrentMergeBase(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	old := testsGit(root, "rev-parse", "HEAD")
	tree := testsGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, old)
	testsGit(root, "checkout", "-qb", "feature")
	pushTestCommit(t, root, "feature before rebase")
	testsGit(root, "checkout", "-q", "main")
	pushTestWrite(t, filepath.Join(root, "default-only"), "advance default\n")
	testsGit(root, "add", "default-only")
	testsGit(root, "commit", "-qm", "advance default")
	mb := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "push", "-q", "origin", "main")
	testsGit(root, "checkout", "-q", "feature")
	testsGit(root, "rebase", "main")
	if got := pushTestBaseCommit(pushTestCapture(t, root, old).Base); got != mb {
		t.Fatal(got)
	}
}

func TestPushCaptureUsesReviewedMergeTreeForRenames(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "checkout", "-qb", "feature")
	pushTestWrite(t, filepath.Join(root, "tracked.txt"), "reviewed edit\n")
	testsGit(root, "add", "tracked.txt")
	testsGit(root, "commit", "-qm", "reviewed edit")
	reviewed := testsGit(root, "rev-parse", "HEAD")
	tree := testsGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)

	testsGit(root, "checkout", "-q", "main")
	testsGit(root, "mv", "tracked.txt", "renamed.txt")
	testsGit(root, "commit", "-qm", "rename upstream file")
	mb := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "push", "-q", "origin", "main")
	testsGit(root, "checkout", "-q", "feature")
	testsGit(root, "merge", "-q", "main")

	captured := pushTestCapture(t, root, start)
	if got := pushTestBaseCommit(captured.Base); got != reviewed {
		t.Fatal(got)
	}
	if !slices.Equal(captured.Delta.MergedIn, []string{mb}) {
		t.Fatal(captured.Delta.MergedIn)
	}
	if captured.Delta.Patch != "" || len(captured.Delta.Files) != 0 {
		t.Fatal(captured.Delta.Patch)
	}
}

func TestPushCaptureOldGitRecordsMergedWorkWithoutReview(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "checkout", "-qb", "feature")
	reviewed := pushTestCommit(t, root, "reviewed")
	tree := testsGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)
	testsGit(root, "checkout", "-q", "main")
	pushTestWrite(t, filepath.Join(root, "upstream-only"), "upstream\n")
	testsGit(root, "add", "upstream-only")
	testsGit(root, "commit", "-qm", "upstream change")
	testsGit(root, "push", "-q", "origin", "main")
	testsGit(root, "checkout", "-q", "feature")
	testsGit(root, "merge", "-q", "main")

	captured, err := CapturePushableCommits(versionGit{"2.34.1"}, root, start)
	if err != nil {
		t.Fatal(err)
	}
	if baseline := captured.Delta.BaselineTree; baseline == nil || *baseline != captured.Delta.CurrentTree {
		t.Fatalf("baseline %v current %s", baseline, captured.Delta.CurrentTree)
	}
	if captured.Delta.Patch != "" {
		t.Fatal(captured.Delta.Patch)
	}
	if warning := captured.Delta.PushReviewWarning; warning == nil || !strings.Contains(*warning, "Git 2.34.1") {
		t.Fatal(warning)
	}
	newGit, err := CapturePushableCommits(versionGit{"2.39.5"}, root, start)
	if err != nil {
		t.Fatal(err)
	}
	expected := testsGit(root, "merge-tree", "--write-tree", "--allow-unrelated-histories",
		reviewed, testsGit(root, "rev-parse", "main"))
	if baseline := newGit.Delta.BaselineTree; baseline == nil || *baseline != expected {
		t.Fatalf("baseline %v, want %s", baseline, expected)
	}
	if newGit.Delta.PushReviewWarning != nil {
		t.Fatal(*newGit.Delta.PushReviewWarning)
	}
}

func TestPushCaptureMergesUnrelatedReviewedAndDefaultHistories(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "checkout", "-qb", "feature")
	reviewed := pushTestCommit(t, root, "reviewed")
	tree := testsGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)
	testsGit(root, "checkout", "--orphan", "new-main")
	testsGit(root, "rm", "-qrf", ".")
	pushTestWrite(t, filepath.Join(root, "upstream-only"), "upstream\n")
	testsGit(root, "add", "upstream-only")
	testsGit(root, "commit", "-qm", "new upstream root")
	testsGit(root, "branch", "-f", "main")
	testsGit(root, "push", "-qf", "origin", "main")
	testsGit(root, "checkout", "-q", "feature")
	testsGit(root, "merge", "-q", "--allow-unrelated-histories", "main")
	captured := pushTestCapture(t, root, start)
	if _, ok := captured.Base.(PushBaseMergedReviewPoint); !ok {
		t.Fatalf("base %#v", captured.Base)
	}
	if captured.Delta.Patch != "" || captured.Delta.PushReviewWarning != nil {
		t.Fatalf("%+v", captured.Delta)
	}
}

func TestPushCaptureUsesMergedReviewBaselineAndKeepsLaterEdits(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "checkout", "-qb", "feature")
	reviewed := pushTestCommit(t, root, "reviewed")
	tree := testsGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)
	if _, _, err := SetReviewBase(checkpoint.SystemGit{}, root, reviewed); err != nil {
		t.Fatal(err)
	}

	testsGit(root, "checkout", "-q", "main")
	pushTestWrite(t, filepath.Join(root, "upstream-only"), "upstream\n")
	testsGit(root, "add", "upstream-only")
	testsGit(root, "commit", "-qm", "upstream change")
	mb := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "push", "-q", "origin", "main")
	testsGit(root, "checkout", "-q", "feature")
	testsGit(root, "merge", "-q", "main")

	merged := pushTestCapture(t, root, start)
	if got := pushTestBaseCommit(merged.Base); got != reviewed {
		t.Fatal(got)
	}
	if !slices.Equal(merged.Delta.MergedIn, []string{mb}) {
		t.Fatal(merged.Delta.MergedIn)
	}
	if merged.Delta.Patch != "" || len(merged.Delta.Files) != 0 {
		t.Fatal(merged.Delta.Patch)
	}

	pushTestCommit(t, root, "post-merge edit")
	later := pushTestCapture(t, root, start)
	if !slices.Equal(later.Delta.MergedIn, []string{mb}) {
		t.Fatal(later.Delta.MergedIn)
	}
	if !strings.Contains(later.Delta.Patch, "+post-merge edit") {
		t.Fatal(later.Delta.Patch)
	}
}

func TestPushCaptureMarksConflictsInMergedReviewBaseline(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "checkout", "-qb", "feature")
	pushTestWrite(t, filepath.Join(root, "tracked.txt"), "branch resolution\n")
	pushTestWrite(t, filepath.Join(root, "branch-only"), "reviewed elsewhere\n")
	testsGit(root, "add", "tracked.txt", "branch-only")
	testsGit(root, "commit", "-qm", "reviewed branch change")
	reviewed := testsGit(root, "rev-parse", "HEAD")
	tree := testsGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)

	testsGit(root, "checkout", "-q", "main")
	pushTestWrite(t, filepath.Join(root, "tracked.txt"), "upstream resolution\n")
	pushTestWrite(t, filepath.Join(root, "upstream-only"), "upstream\n")
	testsGit(root, "add", "tracked.txt", "upstream-only")
	testsGit(root, "commit", "-qm", "upstream conflict change")
	mb := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "push", "-q", "origin", "main")
	testsGit(root, "checkout", "-q", "feature")
	if pushTestMergeStatus(t, root) == 0 {
		t.Fatal("the merge did not conflict")
	}
	pushTestWrite(t, filepath.Join(root, "tracked.txt"), "hand resolved\n")
	testsGit(root, "add", "tracked.txt")
	testsGit(root, "commit", "-qm", "resolve merge")

	captured := pushTestCapture(t, root, start)
	if got := pushTestBaseCommit(captured.Base); got != reviewed {
		t.Fatal(got)
	}
	if !slices.Equal(captured.Delta.MergedIn, []string{mb}) {
		t.Fatal(captured.Delta.MergedIn)
	}
	if !strings.Contains(captured.Delta.Patch, "hand resolved") {
		t.Fatal(captured.Delta.Patch)
	}
	// Only the hand resolution is new: the base holds both sides inside
	// conflict markers, and neither side's other files.
	if files := pushTestFilePaths(captured); !slices.Equal(files, []string{"tracked.txt"}) {
		t.Fatal(files)
	}
	if !strings.Contains(captured.Delta.Patch, "-<<<<<<<") {
		t.Fatal(captured.Delta.Patch)
	}
}

func TestPushCaptureShowsNonTextConflictsResolvedToTheMergeTreeSide(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testsGit(root, "rev-parse", "HEAD")
	pushTestWrite(t, filepath.Join(root, "image.bin"), "\x00base")
	pushTestWrite(t, filepath.Join(root, "kept.txt"), "base\n")
	testsGit(root, "add", "image.bin", "kept.txt")
	testsGit(root, "commit", "-qm", "shared files")
	testsGit(root, "push", "-q", "origin", "main")
	testsGit(root, "checkout", "-qb", "feature")
	pushTestWrite(t, filepath.Join(root, "image.bin"), "\x00branch")
	pushTestWrite(t, filepath.Join(root, "kept.txt"), "branch edit\n")
	testsGit(root, "add", "image.bin", "kept.txt")
	testsGit(root, "commit", "-qm", "reviewed branch change")
	reviewed := testsGit(root, "rev-parse", "HEAD")
	tree := testsGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)

	testsGit(root, "checkout", "-q", "main")
	pushTestWrite(t, filepath.Join(root, "image.bin"), "\x00upstream")
	pushTestWrite(t, filepath.Join(root, "upstream-only"), "upstream\n")
	testsGit(root, "add", "image.bin", "upstream-only")
	testsGit(root, "rm", "-q", "kept.txt")
	testsGit(root, "commit", "-qm", "upstream conflicting change")
	testsGit(root, "push", "-q", "origin", "main")
	testsGit(root, "checkout", "-q", "feature")
	if pushTestMergeStatus(t, root) == 0 {
		t.Fatal("the merge did not conflict")
	}
	// Keep the branch's side of both conflicts: the version merge-tree
	// itself writes for a binary and a modify/delete conflict.
	testsGit(root, "checkout", "--ours", "--", "image.bin")
	testsGit(root, "add", "image.bin", "kept.txt")
	testsGit(root, "commit", "-qm", "resolve merge")

	captured := pushTestCapture(t, root, start)
	if got := pushTestBaseCommit(captured.Base); got != reviewed {
		t.Fatal(got)
	}
	if files := pushTestFilePaths(captured); !slices.Equal(files, []string{"image.bin", "kept.txt"}) {
		t.Fatalf("%v\n%s", files, captured.Delta.Patch)
	}
}

// Hard-won: 9be41400f: macOS rejects the non-UTF-8 filesystem fixture.
func TestPushCaptureShowsConflictedPathsThatAreNotUtf8(t *testing.T) {
	if runtime.GOOS == "darwin" {
		t.Skip("macOS rejects file names that are not UTF-8")
	}
	name := "image-\xff.bin"
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testsGit(root, "rev-parse", "HEAD")
	pushTestWrite(t, filepath.Join(root, name), "\x00base")
	testsGit(root, "add", "-A")
	testsGit(root, "commit", "-qm", "shared file")
	testsGit(root, "push", "-q", "origin", "main")
	testsGit(root, "checkout", "-qb", "feature")
	pushTestWrite(t, filepath.Join(root, name), "\x00branch")
	testsGit(root, "commit", "-qam", "reviewed branch change")
	reviewed := testsGit(root, "rev-parse", "HEAD")
	tree := testsGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)
	testsGit(root, "checkout", "-q", "main")
	pushTestWrite(t, filepath.Join(root, name), "\x00upstream")
	testsGit(root, "commit", "-qam", "upstream conflicting change")
	testsGit(root, "push", "-q", "origin", "main")
	testsGit(root, "checkout", "-q", "feature")
	if pushTestMergeStatus(t, root) == 0 {
		t.Fatal("the merge did not conflict")
	}
	pushTestWrite(t, filepath.Join(root, name), "\x00branch")
	testsGit(root, "add", "-A")
	testsGit(root, "commit", "-qm", "resolve merge")

	captured := pushTestCapture(t, root, start)
	if got := pushTestBaseCommit(captured.Base); got != reviewed {
		t.Fatal(got)
	}
	if len(captured.Delta.Files) != 1 {
		t.Fatal(captured.Delta.Patch)
	}
}

func TestPushCaptureShowsSubmoduleChangesThatDiffSettingsIgnore(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "config", "diff.ignoreSubmodules", "all")
	testsGit(root, "checkout", "-qb", "feature")
	testsGit(root, "update-index", "--add", "--cacheinfo", "160000,"+start+",vendored")
	testsGit(root, "commit", "-qm", "point at a submodule commit")

	captured := pushTestCapture(t, root, start)
	if files := pushTestFilePaths(captured); !slices.Equal(files, []string{"vendored"}) {
		t.Fatalf("%v\n%s", files, captured.Delta.Patch)
	}
	if !strings.Contains(captured.Delta.Patch, "Subproject commit") {
		t.Fatal(captured.Delta.Patch)
	}
}

func pushTestStartHead(t *testing.T, get func(checkpoint.GitCommandRunner, string, string) (*string, error), root, session string) string {
	t.Helper()
	head, err := get(checkpoint.SystemGit{}, root, session)
	if err != nil {
		t.Fatal(err)
	}
	if head == nil {
		return "<none>"
	}
	return *head
}

// Hard-won: f92186373: a first manual review with no recorded start
// pinned the current HEAD and reported the session's commits as reviewed.
func TestAReviewWithoutARecordedStartNeverReportsEarlierCommitsAsReviewed(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	mainCommit := testsGit(root, "rev-parse", "HEAD")
	pushTestCommit(t, root, "on main")
	testsGit(root, "push", "-q", "origin", "main")
	if _, err := ReviewStartHead(checkpoint.SystemGit{}, root, "unpinned"); err == nil || !strings.Contains(err.Error(), "No starting point was recorded") {
		t.Fatal(err)
	}
	head := testsGit(root, "rev-parse", "HEAD")
	if got := pushTestStartHead(t, SessionStartHead, root, "unpinned"); got != head {
		t.Fatalf("later reviews start at the commit recorded now: %s", got)
	}

	reviewed := testsGit(root, "rev-parse", "HEAD")
	tree := testsGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)
	pushTestCommit(t, root, "after a review")
	testsGit(root, "push", "-q", "origin", "main")
	if got := pushTestStartHead(t, ReviewStartHead, root, "reviewed"); got != reviewed {
		t.Fatalf("an earlier reviewed commit bounds the work after it: %s", got)
	}
	if got := pushTestStartHead(t, SessionStartHead, root, "reviewed"); got != "<none>" {
		t.Fatal(got)
	}

	testsGit(root, "checkout", "-qb", "feature", mainCommit)
	pushTestCommit(t, root, "on a branch")
	if got := pushTestStartHead(t, ReviewStartHead, root, "branch"); got != mainCommit {
		t.Fatalf("a branch is bounded by its merge base with the default branch: %s", got)
	}
	if got := pushTestStartHead(t, SessionStartHead, root, "branch"); got != "<none>" {
		t.Fatal(got)
	}
}

func TestPushCaptureOnDefaultBranchUsesSessionStart(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testsGit(root, "rev-parse", "HEAD")
	pushTestCommit(t, root, "later")
	testsGit(root, "push", "-q", "origin", "main")
	if base := pushTestCapture(t, root, start).Base; base != PushBase(PushBaseSessionStart{Commit: start}) {
		t.Fatalf("%#v", base)
	}
}

func TestPushCaptureWithoutDefaultBranchUsesSessionStart(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "remote", "remove", "origin")
	testsGit(root, "branch", "-m", "topic")
	pushTestCommit(t, root, "later")
	if base := pushTestCapture(t, root, start).Base; base != PushBase(PushBaseSessionStart{Commit: start}) {
		t.Fatalf("%#v", base)
	}
}

func TestPushCaptureWithoutOriginHeadPrefersRemoteMainFromLocalMaster(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "symbolic-ref", "--delete", "refs/remotes/origin/HEAD")
	testsGit(root, "branch", "master", start)
	testsGit(root, "checkout", "-q", "master")
	reviewed := pushTestCommit(t, root, "reviewed on master")
	reviewedTree := testsGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, reviewedTree, reviewed)
	testsGit(root, "checkout", "-q", "main")
	pushTestWrite(t, filepath.Join(root, "upstream-only"), "upstream\n")
	testsGit(root, "add", "upstream-only")
	testsGit(root, "commit", "-qm", "advance upstream")
	mb := testsGit(root, "rev-parse", "HEAD")
	testsGit(root, "push", "-q", "origin", "main")
	testsGit(root, "checkout", "-q", "master")
	testsGit(root, "merge", "-q", "origin/main")

	captured := pushTestCapture(t, root, start)
	want := PushBaseMergedReviewPoint{Commit: reviewed, MergeBase: mb, DefaultBranch: "refs/remotes/origin/main"}
	if captured.Base != PushBase(want) {
		t.Fatalf("%#v", captured.Base)
	}
	if !slices.Equal(captured.Delta.MergedIn, []string{mb}) {
		t.Fatal(captured.Delta.MergedIn)
	}
	if captured.Delta.Patch != "" {
		t.Fatal(captured.Delta.Patch)
	}
}

func TestSessionStartPinSurvivesRestartAndWorktreeMoves(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	ensure := func(root string) string {
		t.Helper()
		return pushTestStartHead(t, EnsureSessionStartHead, root, "session")
	}
	recorded := func(root string) string {
		t.Helper()
		return pushTestStartHead(t, SessionStartHead, root, "session")
	}

	start := testsGit(root, "rev-parse", "HEAD")
	if got := recorded(root); got != "<none>" {
		t.Fatal(got)
	}
	ensure(root)
	pushTestCommit(t, root, "later")
	if got := ensure(root); got != start {
		t.Fatal(got)
	}
	if got := recorded(root); got != start {
		t.Fatal(got)
	}
	ensure(root)
	if got := recorded(root); got != start {
		t.Fatal(got)
	}
	moved := filepath.Join(temp, "moved")
	testsGit(temp, "clone", "-q", root, moved)
	if got := recorded(moved); got != "<none>" {
		t.Fatal(got)
	}
	movedHead := testsGit(moved, "rev-parse", "HEAD")
	if got := ensure(moved); got != movedHead {
		t.Fatal(got)
	}
	ensure(moved)
	if got := ensure(moved); got != movedHead {
		t.Fatal(got)
	}
	linked := filepath.Join(temp, "linked")
	testsGit(root, "worktree", "add", "-qb", "linked", linked)
	linkedHead := testsGit(linked, "rev-parse", "HEAD")
	ensure(linked)
	if got := recorded(linked); got != linkedHead {
		t.Fatal(got)
	}
	if recorded(root) == recorded(linked) {
		t.Fatal("the main and linked worktrees share a start")
	}
	// An old advancing baseline cannot reveal its original start commit.
	reference, err := sessionStartRef(checkpoint.SystemGit{}, root, "session")
	if err != nil {
		t.Fatal(err)
	}
	testsGit(root, "update-ref", "-d", reference)
	restoredHead := testsGit(root, "rev-parse", "HEAD")
	ensure(root)
	if got := ensure(root); got != restoredHead {
		t.Fatal(got)
	}
}

func TestEnsuringSessionStartWaitsForAnUnbornHeadToBeCommitted(t *testing.T) {
	root := t.TempDir()
	testsGit(root, "init", "-q", "-b", "main")
	testsGit(root, "config", "user.name", "Review Test")
	testsGit(root, "config", "user.email", "review@example.test")
	if got := pushTestStartHead(t, EnsureSessionStartHead, root, "session"); got != "<none>" {
		t.Fatal(got)
	}
	head := pushTestCommit(t, root, "first commit")
	if got := pushTestStartHead(t, EnsureSessionStartHead, root, "session"); got != head {
		t.Fatal(got)
	}
	pushTestCommit(t, root, "later commit")
	if got := pushTestStartHead(t, EnsureSessionStartHead, root, "session"); got != head {
		t.Fatal(got)
	}
}
