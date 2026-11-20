// Port of the tests in mj-worker/src/review/capture.rs (push_tests).

//go:build unix

package worker

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
	testSupportGit(origin, "init", "--bare", "-q", "-b", "main")
	testSupportGit(temp, "clone", "-q", origin, root)
	testSupportGit(root, "config", "user.name", "Review Test")
	testSupportGit(root, "config", "user.email", "review@example.test")
	pushTestCommit(t, root, "base")
	testSupportGit(root, "push", "-q", "origin", "main")
	testSupportGit(root, "remote", "set-head", "origin", "main")
	return temp
}

// pushTestCommit is push_tests::commit: it writes text to tracked.txt,
// commits it and returns the new HEAD.
func pushTestCommit(t *testing.T, root, text string) string {
	t.Helper()
	if err := os.WriteFile(filepath.Join(root, "tracked.txt"), []byte(text+"\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	testSupportGit(root, "add", "tracked.txt")
	testSupportGit(root, "commit", "-qm", text)
	return testSupportGit(root, "rev-parse", "HEAD")
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
	head := testSupportGit(root, "rev-parse", "HEAD")
	pushTestWrite(t, filepath.Join(root, "untracked"), "ignored")
	if patch := pushTestCapture(t, root, head).Delta.Patch; patch != "" {
		t.Fatalf("patch %q", patch)
	}
	testSupportGit(root, "mv", "tracked.txt", "renamed with spaces.txt")
	_, err := CapturePushableCommits(checkpoint.SystemGit{}, root, head)
	var dirty PushCaptureRefusalDirtyTracked
	if !errors.As(err, &dirty) || !slices.Equal(dirty.Files, []string{"renamed with spaces.txt"}) {
		t.Fatalf("got %v", err)
	}
}

func TestPushCaptureUsesOriginDefaultMergeBaseAndSharedDiffSummary(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	base := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "checkout", "-qb", "feature")
	head := pushTestCommit(t, root, "feature")
	capture := pushTestCapture(t, root, head)
	if want := (PushBaseMergeBase{DefaultBranch: "refs/remotes/origin/main", Commit: base}); capture.Base != PushBase(want) {
		t.Fatalf("base %#v", capture.Base)
	}
	if capture.HeadCommit != head {
		t.Fatalf("head %s", capture.HeadCommit)
	}
	if baseline := capture.Delta.BaselineTree; baseline == nil || *baseline != testSupportGit(root, "rev-parse", base+"^{tree}") {
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
	start := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "checkout", "-qb", "feature")
	reviewed := pushTestCommit(t, root, "reviewed")
	tree := testSupportGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)
	if got := testSupportGit(root, "rev-parse", "refs/mj/reviewed/"+tree); got != reviewed {
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
	start := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "checkout", "-qb", "feature")
	overridden := pushTestCommit(t, root, "overridden")
	tree := testSupportGit(root, "rev-parse", "HEAD^{tree}")
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
	if listed := testSupportGit(root, "for-each-ref", "refs/mj/reviewed/"); listed != "" {
		t.Fatal(listed)
	}
}

func TestSetReviewBaseAcceptsReviewedAndOverriddenTreesAndRefusesOthers(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "checkout", "-qb", "feature")
	reviewed := pushTestCommit(t, root, "reviewed")
	reviewedTree := testSupportGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, reviewedTree, reviewed)
	pushTestCommit(t, root, "overridden")
	// Review points are ordered by committer date, which has one-second
	// resolution; give the overridden commit a clearly later one.
	pushTestAmendLater(t, root)
	overridden := testSupportGit(root, "rev-parse", "HEAD")
	overriddenTree := testSupportGit(root, "rev-parse", "HEAD^{tree}")
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
	if listed := testSupportGit(root, "for-each-ref", "refs/mj/reviewed/"); len(captureLines(listed)) != 1 {
		t.Fatal(listed)
	}

	// A rejected explicit-ref write must not promote an overridden tree.
	testSupportGit(root, "update-ref", "refs/mj/push-base", reviewed)
	refused(overridden, "set the review base")
	if listed := testSupportGit(root, "for-each-ref", "--format=%(objectname)", "refs/mj/reviewed/"); listed != reviewed {
		t.Fatal(listed)
	}
	testSupportGit(root, "update-ref", "-d", "refs/mj/push-base")

	// Any revision works: HEAD~1 is the overridden commit. It becomes a
	// review point and the next capture starts there.
	commit, tree, err := SetReviewBase(checkpoint.SystemGit{}, root, "HEAD~1")
	if err != nil || commit != overridden || tree != overriddenTree {
		t.Fatalf("%s %s %v", commit, tree, err)
	}
	if got := testSupportGit(root, "rev-parse", "refs/mj/reviewed/"+overriddenTree); got != overridden {
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
	start := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "checkout", "-qb", "feature")
	pushTestCommit(t, root, "reviewed with later clock")
	pushTestAmendLater(t, root)
	newerDate := testSupportGit(root, "rev-parse", "HEAD")
	tree := testSupportGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, newerDate)
	later := pushTestCommit(t, root, "reviewed with earlier clock")
	tree = testSupportGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, later)
	if got := pushTestBaseCommit(pushTestCapture(t, root, start).Base); got != later {
		t.Fatal(got)
	}
	pushTestAmendLater(t, root)
	later = testSupportGit(root, "rev-parse", "HEAD")
	pushTestRecordReviewed(t, root, tree, later)
	if got := pushTestBaseCommit(pushTestCapture(t, root, start).Base); got != later {
		t.Fatal(got)
	}
	testSupportGit(root, "checkout", "-qb", "sibling", start)
	sibling := pushTestCommit(t, root, "not an ancestor")
	tree = testSupportGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, sibling)
	testSupportGit(root, "checkout", "-q", "feature")
	if got := pushTestBaseCommit(pushTestCapture(t, root, start).Base); got != later {
		t.Fatal(got)
	}
}

func TestPushCaptureIgnoresReviewedHistoryBeforeCurrentMergeBase(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	old := testSupportGit(root, "rev-parse", "HEAD")
	tree := testSupportGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, old)
	testSupportGit(root, "checkout", "-qb", "feature")
	pushTestCommit(t, root, "feature before rebase")
	testSupportGit(root, "checkout", "-q", "main")
	pushTestWrite(t, filepath.Join(root, "default-only"), "advance default\n")
	testSupportGit(root, "add", "default-only")
	testSupportGit(root, "commit", "-qm", "advance default")
	mb := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "push", "-q", "origin", "main")
	testSupportGit(root, "checkout", "-q", "feature")
	testSupportGit(root, "rebase", "main")
	if got := pushTestBaseCommit(pushTestCapture(t, root, old).Base); got != mb {
		t.Fatal(got)
	}
}

func TestPushCaptureUsesReviewedMergeTreeForRenames(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "checkout", "-qb", "feature")
	pushTestWrite(t, filepath.Join(root, "tracked.txt"), "reviewed edit\n")
	testSupportGit(root, "add", "tracked.txt")
	testSupportGit(root, "commit", "-qm", "reviewed edit")
	reviewed := testSupportGit(root, "rev-parse", "HEAD")
	tree := testSupportGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)

	testSupportGit(root, "checkout", "-q", "main")
	testSupportGit(root, "mv", "tracked.txt", "renamed.txt")
	testSupportGit(root, "commit", "-qm", "rename upstream file")
	mb := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "push", "-q", "origin", "main")
	testSupportGit(root, "checkout", "-q", "feature")
	testSupportGit(root, "merge", "-q", "main")

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
	start := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "checkout", "-qb", "feature")
	reviewed := pushTestCommit(t, root, "reviewed")
	tree := testSupportGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)
	testSupportGit(root, "checkout", "-q", "main")
	pushTestWrite(t, filepath.Join(root, "upstream-only"), "upstream\n")
	testSupportGit(root, "add", "upstream-only")
	testSupportGit(root, "commit", "-qm", "upstream change")
	testSupportGit(root, "push", "-q", "origin", "main")
	testSupportGit(root, "checkout", "-q", "feature")
	testSupportGit(root, "merge", "-q", "main")

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
	expected := testSupportGit(root, "merge-tree", "--write-tree", "--allow-unrelated-histories",
		reviewed, testSupportGit(root, "rev-parse", "main"))
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
	start := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "checkout", "-qb", "feature")
	reviewed := pushTestCommit(t, root, "reviewed")
	tree := testSupportGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)
	testSupportGit(root, "checkout", "--orphan", "new-main")
	testSupportGit(root, "rm", "-qrf", ".")
	pushTestWrite(t, filepath.Join(root, "upstream-only"), "upstream\n")
	testSupportGit(root, "add", "upstream-only")
	testSupportGit(root, "commit", "-qm", "new upstream root")
	testSupportGit(root, "branch", "-f", "main")
	testSupportGit(root, "push", "-qf", "origin", "main")
	testSupportGit(root, "checkout", "-q", "feature")
	testSupportGit(root, "merge", "-q", "--allow-unrelated-histories", "main")
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
	start := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "checkout", "-qb", "feature")
	reviewed := pushTestCommit(t, root, "reviewed")
	tree := testSupportGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)
	if _, _, err := SetReviewBase(checkpoint.SystemGit{}, root, reviewed); err != nil {
		t.Fatal(err)
	}

	testSupportGit(root, "checkout", "-q", "main")
	pushTestWrite(t, filepath.Join(root, "upstream-only"), "upstream\n")
	testSupportGit(root, "add", "upstream-only")
	testSupportGit(root, "commit", "-qm", "upstream change")
	mb := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "push", "-q", "origin", "main")
	testSupportGit(root, "checkout", "-q", "feature")
	testSupportGit(root, "merge", "-q", "main")

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
	start := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "checkout", "-qb", "feature")
	pushTestWrite(t, filepath.Join(root, "tracked.txt"), "branch resolution\n")
	pushTestWrite(t, filepath.Join(root, "branch-only"), "reviewed elsewhere\n")
	testSupportGit(root, "add", "tracked.txt", "branch-only")
	testSupportGit(root, "commit", "-qm", "reviewed branch change")
	reviewed := testSupportGit(root, "rev-parse", "HEAD")
	tree := testSupportGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)

	testSupportGit(root, "checkout", "-q", "main")
	pushTestWrite(t, filepath.Join(root, "tracked.txt"), "upstream resolution\n")
	pushTestWrite(t, filepath.Join(root, "upstream-only"), "upstream\n")
	testSupportGit(root, "add", "tracked.txt", "upstream-only")
	testSupportGit(root, "commit", "-qm", "upstream conflict change")
	mb := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "push", "-q", "origin", "main")
	testSupportGit(root, "checkout", "-q", "feature")
	if pushTestMergeStatus(t, root) == 0 {
		t.Fatal("the merge did not conflict")
	}
	pushTestWrite(t, filepath.Join(root, "tracked.txt"), "hand resolved\n")
	testSupportGit(root, "add", "tracked.txt")
	testSupportGit(root, "commit", "-qm", "resolve merge")

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
	start := testSupportGit(root, "rev-parse", "HEAD")
	pushTestWrite(t, filepath.Join(root, "image.bin"), "\x00base")
	pushTestWrite(t, filepath.Join(root, "kept.txt"), "base\n")
	testSupportGit(root, "add", "image.bin", "kept.txt")
	testSupportGit(root, "commit", "-qm", "shared files")
	testSupportGit(root, "push", "-q", "origin", "main")
	testSupportGit(root, "checkout", "-qb", "feature")
	pushTestWrite(t, filepath.Join(root, "image.bin"), "\x00branch")
	pushTestWrite(t, filepath.Join(root, "kept.txt"), "branch edit\n")
	testSupportGit(root, "add", "image.bin", "kept.txt")
	testSupportGit(root, "commit", "-qm", "reviewed branch change")
	reviewed := testSupportGit(root, "rev-parse", "HEAD")
	tree := testSupportGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)

	testSupportGit(root, "checkout", "-q", "main")
	pushTestWrite(t, filepath.Join(root, "image.bin"), "\x00upstream")
	pushTestWrite(t, filepath.Join(root, "upstream-only"), "upstream\n")
	testSupportGit(root, "add", "image.bin", "upstream-only")
	testSupportGit(root, "rm", "-q", "kept.txt")
	testSupportGit(root, "commit", "-qm", "upstream conflicting change")
	testSupportGit(root, "push", "-q", "origin", "main")
	testSupportGit(root, "checkout", "-q", "feature")
	if pushTestMergeStatus(t, root) == 0 {
		t.Fatal("the merge did not conflict")
	}
	// Keep the branch's side of both conflicts: the version merge-tree
	// itself writes for a binary and a modify/delete conflict.
	testSupportGit(root, "checkout", "--ours", "--", "image.bin")
	testSupportGit(root, "add", "image.bin", "kept.txt")
	testSupportGit(root, "commit", "-qm", "resolve merge")

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
	start := testSupportGit(root, "rev-parse", "HEAD")
	pushTestWrite(t, filepath.Join(root, name), "\x00base")
	testSupportGit(root, "add", "-A")
	testSupportGit(root, "commit", "-qm", "shared file")
	testSupportGit(root, "push", "-q", "origin", "main")
	testSupportGit(root, "checkout", "-qb", "feature")
	pushTestWrite(t, filepath.Join(root, name), "\x00branch")
	testSupportGit(root, "commit", "-qam", "reviewed branch change")
	reviewed := testSupportGit(root, "rev-parse", "HEAD")
	tree := testSupportGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)
	testSupportGit(root, "checkout", "-q", "main")
	pushTestWrite(t, filepath.Join(root, name), "\x00upstream")
	testSupportGit(root, "commit", "-qam", "upstream conflicting change")
	testSupportGit(root, "push", "-q", "origin", "main")
	testSupportGit(root, "checkout", "-q", "feature")
	if pushTestMergeStatus(t, root) == 0 {
		t.Fatal("the merge did not conflict")
	}
	pushTestWrite(t, filepath.Join(root, name), "\x00branch")
	testSupportGit(root, "add", "-A")
	testSupportGit(root, "commit", "-qm", "resolve merge")

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
	start := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "config", "diff.ignoreSubmodules", "all")
	testSupportGit(root, "checkout", "-qb", "feature")
	testSupportGit(root, "update-index", "--add", "--cacheinfo", "160000,"+start+",vendored")
	testSupportGit(root, "commit", "-qm", "point at a submodule commit")

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
	mainCommit := testSupportGit(root, "rev-parse", "HEAD")
	pushTestCommit(t, root, "on main")
	testSupportGit(root, "push", "-q", "origin", "main")
	if _, err := ReviewStartHead(checkpoint.SystemGit{}, root, "unpinned"); err == nil || !strings.Contains(err.Error(), "No starting point was recorded") {
		t.Fatal(err)
	}
	head := testSupportGit(root, "rev-parse", "HEAD")
	if got := pushTestStartHead(t, SessionStartHead, root, "unpinned"); got != head {
		t.Fatalf("later reviews start at the commit recorded now: %s", got)
	}

	reviewed := testSupportGit(root, "rev-parse", "HEAD")
	tree := testSupportGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, tree, reviewed)
	pushTestCommit(t, root, "after a review")
	testSupportGit(root, "push", "-q", "origin", "main")
	if got := pushTestStartHead(t, ReviewStartHead, root, "reviewed"); got != reviewed {
		t.Fatalf("an earlier reviewed commit bounds the work after it: %s", got)
	}
	if got := pushTestStartHead(t, SessionStartHead, root, "reviewed"); got != "<none>" {
		t.Fatal(got)
	}

	testSupportGit(root, "checkout", "-qb", "feature", mainCommit)
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
	start := testSupportGit(root, "rev-parse", "HEAD")
	pushTestCommit(t, root, "later")
	testSupportGit(root, "push", "-q", "origin", "main")
	if base := pushTestCapture(t, root, start).Base; base != PushBase(PushBaseSessionStart{Commit: start}) {
		t.Fatalf("%#v", base)
	}
}

func TestPushCaptureWithoutDefaultBranchUsesSessionStart(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "remote", "remove", "origin")
	testSupportGit(root, "branch", "-m", "topic")
	pushTestCommit(t, root, "later")
	if base := pushTestCapture(t, root, start).Base; base != PushBase(PushBaseSessionStart{Commit: start}) {
		t.Fatalf("%#v", base)
	}
}

func TestPushCaptureWithoutOriginHeadPrefersRemoteMainFromLocalMaster(t *testing.T) {
	temp := pushTestRepository(t)
	root := filepath.Join(temp, "checkout")
	start := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "symbolic-ref", "--delete", "refs/remotes/origin/HEAD")
	testSupportGit(root, "branch", "master", start)
	testSupportGit(root, "checkout", "-q", "master")
	reviewed := pushTestCommit(t, root, "reviewed on master")
	reviewedTree := testSupportGit(root, "rev-parse", "HEAD^{tree}")
	pushTestRecordReviewed(t, root, reviewedTree, reviewed)
	testSupportGit(root, "checkout", "-q", "main")
	pushTestWrite(t, filepath.Join(root, "upstream-only"), "upstream\n")
	testSupportGit(root, "add", "upstream-only")
	testSupportGit(root, "commit", "-qm", "advance upstream")
	mb := testSupportGit(root, "rev-parse", "HEAD")
	testSupportGit(root, "push", "-q", "origin", "main")
	testSupportGit(root, "checkout", "-q", "master")
	testSupportGit(root, "merge", "-q", "origin/main")

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

	start := testSupportGit(root, "rev-parse", "HEAD")
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
	testSupportGit(temp, "clone", "-q", root, moved)
	if got := recorded(moved); got != "<none>" {
		t.Fatal(got)
	}
	movedHead := testSupportGit(moved, "rev-parse", "HEAD")
	if got := ensure(moved); got != movedHead {
		t.Fatal(got)
	}
	ensure(moved)
	if got := ensure(moved); got != movedHead {
		t.Fatal(got)
	}
	linked := filepath.Join(temp, "linked")
	testSupportGit(root, "worktree", "add", "-qb", "linked", linked)
	linkedHead := testSupportGit(linked, "rev-parse", "HEAD")
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
	testSupportGit(root, "update-ref", "-d", reference)
	restoredHead := testSupportGit(root, "rev-parse", "HEAD")
	ensure(root)
	if got := ensure(root); got != restoredHead {
		t.Fatal(got)
	}
}

func TestEnsuringSessionStartWaitsForAnUnbornHeadToBeCommitted(t *testing.T) {
	root := t.TempDir()
	testSupportGit(root, "init", "-q", "-b", "main")
	testSupportGit(root, "config", "user.name", "Review Test")
	testSupportGit(root, "config", "user.email", "review@example.test")
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
