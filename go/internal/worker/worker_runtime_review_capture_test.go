// Port of the tests in mj-worker/src/worker_runtime/review_capture.rs.

//go:build unix

package worker

import (
	"context"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// reviewCaptureTestRoot is the canonical temporary directory: Git reports
// resolved paths, and macOS temporary directories sit behind a symlink.
func reviewCaptureTestRoot(t *testing.T) string {
	t.Helper()
	root, err := workerRuntimeCanonicalize(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	return root
}

func reviewCaptureTestRepository(t *testing.T, path, branch string) {
	t.Helper()
	if err := os.MkdirAll(path, 0o777); err != nil {
		t.Fatal(err)
	}
	testSupportGit(path, "init", "-q", "-b", branch)
	testSupportGit(path, "config", "user.name", "Review Test")
	testSupportGit(path, "config", "user.email", "review@example.invalid")
}

func reviewCaptureTestWrite(t *testing.T, path, body string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(body), 0o666); err != nil {
		t.Fatal(err)
	}
}

func newTestReviewCapture(t *testing.T, sessionID, cwd, relayDirectory string) *reviewCapture {
	t.Helper()
	relay, err := DurableRelayOpen(relayDirectory, sessionID, "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	return &reviewCapture{SessionID: sessionID, Cwd: cwd, PrimaryRelay: NewRelayCell(relay)}
}

func reviewCaptureDeltas(t *testing.T, body core.RelayResponseBody) []core.RepoDelta {
	t.Helper()
	ok, isOk := body.(core.RelayResponseBodyOk)
	if !isOk {
		t.Fatalf("missing delta: %#v", body)
	}
	delta, isDelta := ok.Payload.(core.RelayResponsePayloadReviewDelta)
	if !isDelta {
		t.Fatalf("missing delta: %#v", ok.Payload)
	}
	return delta.Repositories
}

func reviewCaptureFilePaths(delta core.RepoDelta) []string {
	var paths []string
	for _, file := range delta.Files {
		paths = append(paths, file.Path)
	}
	return paths
}

func reviewCaptureCapture(repositories *[]string) core.ReviewerRequestCaptureDelta {
	return core.ReviewerRequestCaptureDelta{Repositories: repositories, Baselines: map[string]string{}, Scope: core.CaptureScopePushableCommits}
}

func reviewCaptureHasCommit(value *string, expected string) bool {
	return value != nil && *value == expected
}

// The review host reserves a root before it changes refs, so resolving a
// path must name the same root a capture does, without capturing: no start
// ref is written, and uncommitted tracked edits do not refuse it.
func TestResolvingRepositoriesNamesRootsAndWritesNoRefs(t *testing.T) {
	ctx := context.Background()
	tempRoot := reviewCaptureTestRoot(t)
	root := filepath.Join(tempRoot, "root")
	reviewCaptureTestRepository(t, root, "main")
	reviewCaptureTestWrite(t, filepath.Join(root, "tracked.rs"), "base\n")
	testSupportGit(root, "add", "tracked.rs")
	testSupportGit(root, "commit", "-qm", "base")
	worktree := filepath.Join(tempRoot, "linked")
	testSupportGit(root, "worktree", "add", "-qb", "feature", worktree)
	reviewCaptureTestWrite(t, filepath.Join(root, "tracked.rs"), "dirty\n")
	refsBefore := testSupportGit(root, "for-each-ref")
	relayDirectory := filepath.Join(tempRoot, "relay")
	if err := os.Mkdir(relayDirectory, 0o777); err != nil {
		t.Fatal(err)
	}
	capture := newTestReviewCapture(t, "resolve-test", root, relayDirectory)
	result, err := capture.Handle(ctx, core.ReviewerRequestResolveRepositories{
		Paths: []string{"../linked", ".", worktree},
	})
	if err != nil {
		t.Fatal(err)
	}
	ok, isOk := result.(core.RelayResponseBodyOk)
	resolved, isRoots := ok.Payload.(core.RelayResponsePayloadReviewRepositories)
	if !isOk || !isRoots {
		t.Fatalf("missing roots: %#v", result)
	}
	if !slices.Equal(resolved.Roots, []string{worktree, root}) {
		t.Fatalf("roots = %q", resolved.Roots)
	}
	if refs := testSupportGit(root, "for-each-ref"); refs != refsBefore {
		t.Fatalf("resolving writes no ref: %q became %q", refsBefore, refs)
	}
}

// Hard-won: 10e9fd277: explicit commit reviews were refused on child workers without turn baselines.
func TestScopedPushCaptureIgnoresDirtyRootAndAcceptsLinkedWorktree(t *testing.T) {
	ctx := context.Background()
	tempRoot := reviewCaptureTestRoot(t)
	root := filepath.Join(tempRoot, "root")
	reviewCaptureTestRepository(t, root, "main")
	reviewCaptureTestWrite(t, filepath.Join(root, "tracked.rs"), "base\n")
	testSupportGit(root, "add", "tracked.rs")
	testSupportGit(root, "commit", "-qm", "base")
	base := testSupportGit(root, "rev-parse", "HEAD")
	worktree := filepath.Join(tempRoot, "linked")
	testSupportGit(root, "worktree", "add", "-qb", "feature", worktree)
	reviewCaptureTestWrite(t, filepath.Join(worktree, "tracked.rs"), "feature\n")
	testSupportGit(worktree, "commit", "-qam", "feature")
	reviewCaptureTestWrite(t, filepath.Join(root, "tracked.rs"), "dirty\n")
	relayDirectory := filepath.Join(tempRoot, "relay")
	if err := os.Mkdir(relayDirectory, 0o777); err != nil {
		t.Fatal(err)
	}
	capture := newTestReviewCapture(t, "scoped-test", root, relayDirectory)
	reviewedTree := ""
	for _, path := range []string{worktree, "../linked"} {
		result, err := capture.Handle(ctx, reviewCaptureCapture(&[]string{path}))
		if err != nil {
			t.Fatal(err)
		}
		repositories := reviewCaptureDeltas(t, result)
		if len(repositories) != 1 {
			t.Fatalf("repositories = %#v", repositories)
		}
		if repositories[0].Root != worktree {
			t.Fatalf("root = %s", repositories[0].Root)
		}
		reviewedTree = repositories[0].CurrentTree
		if !reviewCaptureHasCommit(repositories[0].BaseCommit, strings.TrimSpace(base)) {
			t.Fatalf("base = %v", repositories[0].BaseCommit)
		}
		if !reviewCaptureHasCommit(repositories[0].HeadCommit, strings.TrimSpace(testSupportGit(worktree, "rev-parse", "HEAD"))) {
			t.Fatalf("head = %v", repositories[0].HeadCommit)
		}
	}
	head := testSupportGit(worktree, "rev-parse", "HEAD")
	if _, err := capture.Handle(ctx, core.ReviewerRequestRecordReviewed{Trees: map[string][2]string{
		worktree: {reviewedTree, strings.TrimSpace(head)},
	}}); err != nil {
		t.Fatal(err)
	}
	if _, err := InstallPrePushHook(checkpoint.SystemGit{}, worktree); err != nil {
		t.Fatal(err)
	}
	remote := filepath.Join(tempRoot, "remote.git")
	if err := os.Mkdir(remote, 0o777); err != nil {
		t.Fatal(err)
	}
	testSupportGit(remote, "init", "-q", "--bare")
	testSupportGit(worktree, "remote", "add", "origin", remote)
	output, err := checkpoint.SystemGit{}.Run(worktree, &checkpoint.GitCommand{
		Arguments: []string{"push", "origin", "HEAD:refs/heads/feature"},
		Env:       [][2]string{{"MJ_REVIEW_GATE", "1"}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 {
		t.Fatalf("push failed: %s", output.Stderr)
	}
	unrelated := filepath.Join(tempRoot, "unrelated")
	if err := os.Mkdir(unrelated, 0o777); err != nil {
		t.Fatal(err)
	}
	testSupportGit(unrelated, "init", "-q")
	_, err = capture.Handle(ctx, reviewCaptureCapture(&[]string{unrelated}))
	if err == nil || !strings.Contains(err.Error(), "not a worktree of a workspace repository") {
		t.Fatalf("err = %v", err)
	}
}

// TestScopedPushCaptureStartsANewWorktreeAtItsForkPoint: a worktree made
// after the session started has no start of its own, and a repository
// without main, master or origin/HEAD has no default branch to bound it. Its
// first review still covers the commits it already holds.
func TestScopedPushCaptureStartsANewWorktreeAtItsForkPoint(t *testing.T) {
	ctx := context.Background()
	tempRoot := reviewCaptureTestRoot(t)
	root := filepath.Join(tempRoot, "root")
	reviewCaptureTestRepository(t, root, "trunk")
	reviewCaptureTestWrite(t, filepath.Join(root, "tracked.rs"), "base\n")
	testSupportGit(root, "add", "tracked.rs")
	testSupportGit(root, "commit", "-qm", "base")
	base := testSupportGit(root, "rev-parse", "HEAD")
	if _, err := EnsureSessionStartHead(checkpoint.SystemGit{}, root, "fork-test"); err != nil {
		t.Fatal(err)
	}
	worktree := filepath.Join(tempRoot, "lane")
	testSupportGit(root, "worktree", "add", "-qb", "lane", worktree)
	reviewCaptureTestWrite(t, filepath.Join(worktree, "tracked.rs"), "lane\n")
	testSupportGit(worktree, "commit", "-qam", "lane work")
	reviewCaptureTestWrite(t, filepath.Join(root, "other.rs"), "root\n")
	testSupportGit(root, "add", "other.rs")
	testSupportGit(root, "commit", "-qm", "root moves on")
	relayDirectory := filepath.Join(tempRoot, "relay")
	if err := os.Mkdir(relayDirectory, 0o777); err != nil {
		t.Fatal(err)
	}
	capture := newTestReviewCapture(t, "fork-test", root, relayDirectory)
	result, err := capture.Handle(ctx, reviewCaptureCapture(&[]string{"../lane"}))
	if err != nil {
		t.Fatal(err)
	}
	repositories := reviewCaptureDeltas(t, result)
	if len(repositories) != 1 {
		t.Fatalf("repositories = %#v", repositories)
	}
	delta := repositories[0]
	if delta.Root != worktree {
		t.Fatalf("root = %s", delta.Root)
	}
	if !reviewCaptureHasCommit(delta.BaseCommit, strings.TrimSpace(base)) {
		t.Fatalf("base = %v", delta.BaseCommit)
	}
	if !reviewCaptureHasCommit(delta.HeadCommit, strings.TrimSpace(testSupportGit(worktree, "rev-parse", "HEAD"))) {
		t.Fatalf("head = %v", delta.HeadCommit)
	}
	if paths := reviewCaptureFilePaths(delta); !slices.Equal(paths, []string{"tracked.rs"}) {
		t.Fatalf("files = %q", paths)
	}
}

// TestScopedPushCaptureReviewsAnOrphanWorktreeFromTheEmptyTree: an orphan
// branch shares no history with the session start or the default branch,
// so every commit it holds is reviewed.
func TestScopedPushCaptureReviewsAnOrphanWorktreeFromTheEmptyTree(t *testing.T) {
	ctx := context.Background()
	tempRoot := reviewCaptureTestRoot(t)
	root := filepath.Join(tempRoot, "root")
	reviewCaptureTestRepository(t, root, "main")
	reviewCaptureTestWrite(t, filepath.Join(root, "tracked.rs"), "base\n")
	testSupportGit(root, "add", "tracked.rs")
	testSupportGit(root, "commit", "-qm", "base")
	if _, err := EnsureSessionStartHead(checkpoint.SystemGit{}, root, "orphan-test"); err != nil {
		t.Fatal(err)
	}
	worktree := filepath.Join(tempRoot, "solo")
	testSupportGit(root, "worktree", "add", "-q", "--detach", worktree)
	testSupportGit(worktree, "checkout", "-q", "--orphan", "solo")
	testSupportGit(worktree, "rm", "-rqf", ".")
	reviewCaptureTestWrite(t, filepath.Join(worktree, "solo.rs"), "solo\n")
	testSupportGit(worktree, "add", "solo.rs")
	testSupportGit(worktree, "commit", "-qm", "orphan work")
	relayDirectory := filepath.Join(tempRoot, "relay")
	if err := os.Mkdir(relayDirectory, 0o777); err != nil {
		t.Fatal(err)
	}
	capture := newTestReviewCapture(t, "orphan-test", root, relayDirectory)
	result, err := capture.Handle(ctx, reviewCaptureCapture(&[]string{worktree}))
	if err != nil {
		t.Fatal(err)
	}
	repositories := reviewCaptureDeltas(t, result)
	if len(repositories) != 1 {
		t.Fatalf("repositories = %#v", repositories)
	}
	delta := repositories[0]
	if delta.Root != worktree {
		t.Fatalf("root = %s", delta.Root)
	}
	if delta.BaseCommit != nil {
		t.Fatalf("base = %v", *delta.BaseCommit)
	}
	if !reviewCaptureHasCommit(delta.BaseKind, "no shared history") {
		t.Fatalf("base kind = %v", delta.BaseKind)
	}
	if !reviewCaptureHasCommit(delta.HeadCommit, strings.TrimSpace(testSupportGit(worktree, "rev-parse", "HEAD"))) {
		t.Fatalf("head = %v", delta.HeadCommit)
	}
	if paths := reviewCaptureFilePaths(delta); !slices.Equal(paths, []string{"solo.rs"}) {
		t.Fatalf("files = %q", paths)
	}
	if !strings.Contains(delta.Patch, "+solo") {
		t.Fatal(delta.Patch)
	}
}

// Hard-won: #1531: a separately reviewed merge parent erased an accepted review baseline.
func TestSetReviewBaselineSurvivesMergingIndependentReviewedHistories(t *testing.T) {
	ctx := context.Background()
	temp := reviewCaptureTestRoot(t)
	repository := filepath.Join(temp, "repository")
	reviewCaptureTestRepository(t, repository, "main")
	reviewCaptureTestWrite(t, filepath.Join(repository, "feature.rs"), "base\n")
	testSupportGit(repository, "add", "feature.rs")
	testSupportGit(repository, "commit", "-qm", "base")
	worker := filepath.Join(temp, "worker")
	if err := os.Mkdir(worker, 0o777); err != nil {
		t.Fatal(err)
	}
	capture := newTestReviewCapture(t, "baseline-1531", repository, worker)

	testSupportGit(repository, "checkout", "-qb", "feature")
	reviewCaptureTestWrite(t, filepath.Join(repository, "feature.rs"), "reviewed feature\n")
	testSupportGit(repository, "commit", "-qam", "feature")
	baseline := testSupportGit(repository, "rev-parse", "HEAD")
	baselineTree := testSupportGit(repository, "rev-parse", "HEAD^{tree}")
	testSupportGit(repository, "checkout", "-qb", "sibling", "main")
	for _, name := range []string{"gate.rs", "classification.tsv"} {
		reviewCaptureTestWrite(t, filepath.Join(repository, name), "reviewed sibling\n")
		testSupportGit(repository, "add", name)
	}
	testSupportGit(repository, "commit", "-qm", "sibling")
	sibling := testSupportGit(repository, "rev-parse", "HEAD")
	siblingTree := testSupportGit(repository, "rev-parse", "HEAD^{tree}")
	for _, pair := range [][2]string{{baselineTree, baseline}, {siblingTree, sibling}} {
		if _, err := capture.Handle(ctx, core.ReviewerRequestRecordReviewed{Trees: map[string][2]string{repository: pair}}); err != nil {
			t.Fatal(err)
		}
	}
	testSupportGit(repository, "checkout", "-q", "feature")
	testSupportGit(repository, "merge", "-q", "--no-edit", "sibling")
	accepted, err := capture.Handle(ctx, core.ReviewerRequestSetReviewBase{Commit: baseline})
	if err != nil {
		t.Fatal(err)
	}
	if set, ok := reviewCaptureBaseSet(accepted); !ok || set.Commit != baseline || !set.Changed {
		t.Fatalf("accepted = %#v", accepted)
	}

	request := func() core.ReviewerRequestCaptureDelta { return reviewCaptureCapture(nil) }
	result, err := capture.Handle(ctx, request())
	if err != nil {
		t.Fatal(err)
	}
	repositories := reviewCaptureDeltas(t, result)
	if !reviewCaptureHasCommit(repositories[0].BaseCommit, baseline) {
		t.Fatalf("base = %v", repositories[0].BaseCommit)
	}
	if !reviewCaptureHasCommit(repositories[0].BaseKind, "review point") {
		t.Fatalf("base kind = %v", repositories[0].BaseKind)
	}
	if paths := reviewCaptureFilePaths(repositories[0]); !slices.Equal(paths, []string{"classification.tsv", "gate.rs"}) {
		t.Fatalf("files = %q", paths)
	}

	// Linked worktrees share reviewed refs, but each explicit selection is
	// its own.
	linked := filepath.Join(temp, "linked")
	testSupportGit(repository, "worktree", "add", "-q", "--detach", linked)
	if _, err := capture.Handle(ctx, core.ReviewerRequestSetReviewBase{Path: &linked, Commit: sibling}); err != nil {
		t.Fatal(err)
	}
	for _, pair := range [][2]string{{repository, baseline}, {linked, sibling}} {
		result, err := capture.Handle(ctx, reviewCaptureCapture(&[]string{pair[0]}))
		if err != nil {
			t.Fatal(err)
		}
		repositories := reviewCaptureDeltas(t, result)
		if !reviewCaptureHasCommit(repositories[0].BaseCommit, pair[1]) {
			t.Fatalf("%s base = %v", pair[0], repositories[0].BaseCommit)
		}
	}

	// Reviewing the merge advances past both parents without another setter
	// call.
	head := testSupportGit(repository, "rev-parse", "HEAD")
	tree := testSupportGit(repository, "rev-parse", "HEAD^{tree}")
	if _, err := capture.Handle(ctx, core.ReviewerRequestRecordReviewed{Trees: map[string][2]string{repository: {tree, head}}}); err != nil {
		t.Fatal(err)
	}
	result, err = capture.Handle(ctx, request())
	if err != nil {
		t.Fatal(err)
	}
	repositories = reviewCaptureDeltas(t, result)
	if !reviewCaptureHasCommit(repositories[0].BaseCommit, head) {
		t.Fatalf("base = %v", repositories[0].BaseCommit)
	}
	if len(repositories[0].Files) != 0 {
		t.Fatalf("files = %#v", repositories[0].Files)
	}

	// The completed review remains the base across another independently
	// reviewed merge.
	testSupportGit(linked, "checkout", "-qb", "next", "main")
	reviewCaptureTestWrite(t, filepath.Join(linked, "followup.rs"), "next reviewed branch\n")
	testSupportGit(linked, "add", "followup.rs")
	testSupportGit(linked, "commit", "-qm", "next")
	next := testSupportGit(linked, "rev-parse", "HEAD")
	tree = testSupportGit(linked, "rev-parse", "HEAD^{tree}")
	if _, err := capture.Handle(ctx, core.ReviewerRequestRecordReviewed{Trees: map[string][2]string{linked: {tree, next}}}); err != nil {
		t.Fatal(err)
	}
	testSupportGit(repository, "merge", "-q", "--no-edit", "next")
	result, err = capture.Handle(ctx, request())
	if err != nil {
		t.Fatal(err)
	}
	repositories = reviewCaptureDeltas(t, result)
	if !reviewCaptureHasCommit(repositories[0].BaseCommit, head) {
		t.Fatalf("base = %v", repositories[0].BaseCommit)
	}
	if paths := reviewCaptureFilePaths(repositories[0]); !slices.Equal(paths, []string{"followup.rs"}) {
		t.Fatalf("files = %q", paths)
	}
}

// reviewCaptureBaseSet is the ReviewBaseSet payload of an answer.
func reviewCaptureBaseSet(body core.RelayResponseBody) (core.RelayResponsePayloadReviewBaseSet, bool) {
	ok, isOk := body.(core.RelayResponseBodyOk)
	if !isOk {
		return core.RelayResponsePayloadReviewBaseSet{}, false
	}
	set, isSet := ok.Payload.(core.RelayResponsePayloadReviewBaseSet)
	return set, isSet
}

func TestAnOverridePinsOnlyTheOverriddenRefAndSetReviewBasePromotesIt(t *testing.T) {
	ctx := context.Background()
	temp := reviewCaptureTestRoot(t)
	repository := filepath.Join(temp, "repository")
	reviewCaptureTestRepository(t, repository, "main")
	reviewCaptureTestWrite(t, filepath.Join(repository, "tracked.rs"), "before\n")
	testSupportGit(repository, "add", "tracked.rs")
	testSupportGit(repository, "commit", "-qm", "base")
	reviewCaptureTestWrite(t, filepath.Join(repository, "tracked.rs"), "after\n")
	testSupportGit(repository, "commit", "-qam", "change")
	head := strings.TrimSpace(testSupportGit(repository, "rev-parse", "HEAD"))
	tree := testSupportGit(repository, "rev-parse", "HEAD^{tree}")
	worker := filepath.Join(temp, "worker")
	if err := os.Mkdir(worker, 0o777); err != nil {
		t.Fatal(err)
	}
	capture := newTestReviewCapture(t, "override-test", repository, worker)
	_, err := capture.Handle(ctx, core.ReviewerRequestSetReviewBase{Commit: head})
	if err == nil || !strings.Contains(err.Error(), "call request_review first") {
		t.Fatalf("err = %v", err)
	}
	if _, err := capture.Handle(ctx, core.ReviewerRequestRecordOverridden{Trees: map[string][2]string{repository: {tree, head}}}); err != nil {
		t.Fatal(err)
	}
	if refs := testSupportGit(repository, "for-each-ref", "refs/mj/reviewed/"); refs != "" {
		t.Fatalf("reviewed refs = %s", refs)
	}
	if got := testSupportGit(repository, "rev-parse", "refs/mj/overridden/"+tree); got != head {
		t.Fatalf("overridden = %s", got)
	}
	result, err := capture.Handle(ctx, core.ReviewerRequestSetReviewBase{Commit: "HEAD"})
	if err != nil {
		t.Fatal(err)
	}
	set, ok := reviewCaptureBaseSet(result)
	if !ok || !set.Changed {
		t.Fatalf("set_review_base did not answer with the base: %#v", result)
	}
	if set.Root != repository || set.Commit != head || set.Tree != tree {
		t.Fatalf("set = %#v", set)
	}
	if got := testSupportGit(repository, "rev-parse", "refs/mj/reviewed/"+tree); got != head {
		t.Fatalf("reviewed = %s", got)
	}
	// Hard-won: 9ebd246b2: setting an older reviewed base must be a no-op.
	reviewCaptureTestWrite(t, filepath.Join(repository, "tracked.rs"), "later\n")
	testSupportGit(repository, "commit", "-qam", "later reviewed commit")
	later := testSupportGit(repository, "rev-parse", "HEAD")
	laterTree := testSupportGit(repository, "rev-parse", "HEAD^{tree}")
	if _, err := capture.Handle(ctx, core.ReviewerRequestRecordReviewed{Trees: map[string][2]string{repository: {laterTree, later}}}); err != nil {
		t.Fatal(err)
	}
	unchanged, err := capture.Handle(ctx, core.ReviewerRequestSetReviewBase{Commit: head})
	if err != nil {
		t.Fatal(err)
	}
	if set, ok := reviewCaptureBaseSet(unchanged); !ok || set.Changed || set.Commit != later {
		t.Fatalf("unchanged = %#v", unchanged)
	}
	outsidePath := temp
	_, err = capture.Handle(ctx, core.ReviewerRequestSetReviewBase{Path: &outsidePath, Commit: "HEAD"})
	if err == nil || !strings.Contains(err.Error(), "not in a Git repository") {
		t.Fatalf("err = %v", err)
	}
}

func TestPushCaptureAndRecordingUseTheCommittedGitTree(t *testing.T) {
	ctx := context.Background()
	temp := t.TempDir()
	repository := filepath.Join(temp, "repository")
	reviewCaptureTestRepository(t, repository, "main")
	reviewCaptureTestWrite(t, filepath.Join(repository, "tracked.rs"), "before\n")
	testSupportGit(repository, "add", "tracked.rs")
	testSupportGit(repository, "commit", "-qm", "base")
	start := testSupportGit(repository, "rev-parse", "HEAD")
	if _, err := EnsureSessionStartHead(checkpoint.SystemGit{}, repository, "push-test"); err != nil {
		t.Fatal(err)
	}
	testSupportGit(repository, "checkout", "-qb", "feature")
	reviewCaptureTestWrite(t, filepath.Join(repository, "tracked.rs"), "after\n")
	testSupportGit(repository, "commit", "-qam", "change")
	reviewCaptureTestWrite(t, filepath.Join(repository, "untracked.txt"), "not committed")
	worker := filepath.Join(temp, "worker")
	if err := os.Mkdir(worker, 0o777); err != nil {
		t.Fatal(err)
	}
	capture := newTestReviewCapture(t, "push-test", repository, worker)
	result, err := capture.Handle(ctx, reviewCaptureCapture(nil))
	if err != nil {
		t.Fatal(err)
	}
	repositories := reviewCaptureDeltas(t, result)
	if len(repositories) != 1 {
		t.Fatalf("repositories = %#v", repositories)
	}
	delta := repositories[0]
	if !reviewCaptureHasCommit(delta.BaseCommit, strings.TrimSpace(start)) {
		t.Fatalf("base = %v", delta.BaseCommit)
	}
	if !reviewCaptureHasCommit(delta.BaseKind, "merge base with refs/heads/main") {
		t.Fatalf("base kind = %v", delta.BaseKind)
	}
	if paths := reviewCaptureFilePaths(delta); !slices.Equal(paths, []string{"tracked.rs"}) {
		t.Fatalf("files = %q", paths)
	}
	head := strings.TrimSpace(testSupportGit(repository, "rev-parse", "HEAD"))
	if !reviewCaptureHasCommit(delta.HeadCommit, head) {
		t.Fatalf("head = %v", delta.HeadCommit)
	}
	if _, err := capture.Handle(ctx, core.ReviewerRequestRecordReviewed{Trees: map[string][2]string{
		repository: {delta.CurrentTree, head},
	}}); err != nil {
		t.Fatal(err)
	}
	if got := strings.TrimSpace(testSupportGit(repository, "rev-parse", "refs/mj/reviewed/"+delta.CurrentTree)); got != head {
		t.Fatalf("reviewed = %s", got)
	}
	reviewCaptureTestWrite(t, filepath.Join(repository, "tracked.rs"), "dirty\n")
	_, err = capture.Handle(ctx, reviewCaptureCapture(nil))
	if err == nil {
		t.Fatal("a dirty tracked file must refuse the capture")
	}
	if !strings.Contains(err.Error(), "commit or stash these tracked changes first") || !strings.Contains(err.Error(), "tracked.rs") {
		t.Fatalf("err = %v", err)
	}
}

func TestAReviewCaptureNeedsTheProtocolThatIntroducedIt(t *testing.T) {
	request := core.RelayRequestReviewer{Request: core.ReviewerRequestCaptureDelta{Scope: core.CaptureScopePushableCommits, Baselines: map[string]string{}}}
	if got := core.RelayRequestMinimumProtocol(request); got != 43 {
		t.Fatalf("minimum protocol = %d", got)
	}
	if core.RelayRequestSupportedAt(request, 5) {
		t.Fatal("protocol 5 predates review capture")
	}
	if !core.RelayRequestSupportedAt(request, core.RelayProtocolVersion) {
		t.Fatal("the current protocol supports review capture")
	}
	if got := core.RelayRequestMethodName(request); got != "reviewer_capture_delta" {
		t.Fatalf("method = %s", got)
	}
}

func TestThePlainRelayRefusesReviewerRequests(t *testing.T) {
	relay, err := DurableRelayOpen(t.TempDir(), "capture-test", "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	response := relay.Handle(core.RelayRequestEnvelope{
		RequestID:       "req-1",
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         core.RelayRequestReviewer{Request: core.ReviewerRequestCaptureDelta{Scope: core.CaptureScopePushableCommits, Baselines: map[string]string{}}},
	})
	failure, ok := response.Body.(core.RelayResponseBodyError)
	if !ok {
		t.Fatalf("unexpected response: %#v", response)
	}
	if !strings.Contains(failure.Error.Message, "live relay transport") {
		t.Fatalf("%#v", response)
	}
}
