// Port of mj-worker/src/worker_runtime/review_capture.rs.

// Worker-owned capture operations for child-session reviews.

//go:build unix

package worker

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// reviewCapture answers a reviewer's capture requests against the primary
// session's workspace.
type reviewCapture struct {
	SessionID             string
	Cwd                   string
	AdditionalDirectories []string
	PrimaryRelay          *RelayCell
	// Operation is tokio's Mutex<()>: one capture operation at a time.
	Operation sync.Mutex
}

// reviewCaptureOutcome is one capture operation's result, carried out of
// its task.
type reviewCaptureOutcome struct {
	body core.RelayResponseBody
	err  error
}

// Handle runs one reviewer request on its own task, logging a failure. The
// task finishes the operation even when the caller stops waiting, as a
// spawned Rust task does.
func (c *reviewCapture) Handle(ctx context.Context, request core.ReviewerRequest) (core.RelayResponseBody, error) {
	task := core.Spawn(func() reviewCaptureOutcome {
		body, err := c.run(context.WithoutCancel(ctx), request)
		if err != nil {
			slog.Warn("review capture operation failed", "error", err)
		}
		return reviewCaptureOutcome{body: body, err: err}
	})
	outcome, err := task.Join(ctx)
	if err != nil {
		if ctx.Err() != nil {
			return nil, ctx.Err()
		}
		return nil, fmt.Errorf("review capture task stopped: %w", err)
	}
	return outcome.body, outcome.err
}

func (c *reviewCapture) run(ctx context.Context, request core.ReviewerRequest) (core.RelayResponseBody, error) {
	c.Operation.Lock()
	defer c.Operation.Unlock()
	admission, err := reviewerAdmissionAcquire(c.PrimaryRelay, "review capture")
	if err != nil {
		return nil, err
	}
	defer admission.Release()
	git := checkpoint.SystemGit{}
	switch request := request.(type) {
	case core.ReviewerRequestCaptureDelta:
		return c.captureDelta(ctx, request.Repositories)
	case core.ReviewerRequestRecordReviewed:
		for _, root := range slices.Sorted(maps.Keys(request.Trees)) {
			tree, commit := request.Trees[root][0], request.Trees[root][1]
			if err := RecordReviewBase(git, root, tree, commit); err != nil {
				return nil, err
			}
		}
		return core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadReviewBaselineAdvanced{}}, nil
	case core.ReviewerRequestRecordOverridden:
		for _, root := range slices.Sorted(maps.Keys(request.Trees)) {
			tree, commit := request.Trees[root][0], request.Trees[root][1]
			if err := checkpoint.RecordOverriddenTree(git, root, tree, commit); err != nil {
				return nil, err
			}
		}
		return core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadReviewBaselineAdvanced{}}, nil
	case core.ReviewerRequestSetReviewBase:
		return c.setReviewBase(ctx, request.Path, request.Commit)
	}
	return nil, fmt.Errorf("unknown reviewer request %T", request)
}

func (c *reviewCapture) roots() []string {
	roots := []string{c.Cwd}
	return append(roots, c.AdditionalDirectories...)
}

// captureDelta captures committed work since the push review base in each
// repository.
//
// Git work is blocking and can take a moment on a large tree, so it runs
// on the blocking pool rather than on the runtime that also serves the
// primary session's relay.
func (c *reviewCapture) captureDelta(ctx context.Context, requested *[]string) (core.RelayResponseBody, error) {
	git := checkpoint.SystemGit{}
	discovered := DiscoverRepositories(git, c.roots())
	repositories, origins := discovered, map[string]string{}
	if requested != nil {
		var err error
		repositories, origins, err = scopedRepositories(c.Cwd, discovered, *requested)
		if err != nil {
			return nil, err
		}
	}
	deltas := []core.RepoDelta{}
	for _, root := range repositories {
		var start *string
		var err error
		if origin, ok := origins[root]; ok {
			start, err = LinkedWorktreeStart(git, root, origin, c.SessionID)
		} else {
			start, err = ReviewStartHead(git, root, c.SessionID)
		}
		if err != nil {
			return nil, err
		}
		if start == nil {
			return nil, errors.New("the repository has no HEAD commit")
		}
		capture, err := CapturePushableCommits(git, root, *start)
		if err != nil {
			return nil, err
		}
		delta := capture.Delta
		delta.BaseCommit = capture.Base.BaseCommit()
		head := capture.HeadCommit
		delta.HeadCommit = &head
		deltas = append(deltas, delta)
	}
	return core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadReviewDelta{Repositories: deltas}}, nil
}

// setReviewBase makes a reviewed or overridden commit the base of the next
// push review.
func (c *reviewCapture) setReviewBase(ctx context.Context, path *string, commit string) (core.RelayResponseBody, error) {
	git := checkpoint.SystemGit{}
	discovered := DiscoverRepositories(git, c.roots())
	selected := c.Cwd
	if path != nil {
		selected = *path
	}
	repositories, _, err := scopedRepositories(c.Cwd, discovered, []string{selected})
	if err != nil {
		return nil, err
	}
	if len(repositories) == 0 {
		return nil, errors.New("the path names no repository")
	}
	root := repositories[0]
	laterCommit, laterTree, found, err := LaterReviewBase(git, root, commit)
	if err != nil {
		return nil, err
	}
	if found {
		if _, _, err := SetReviewBase(git, root, laterCommit); err != nil {
			return nil, err
		}
		return core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadReviewBaseSet{
			Root: root, Commit: laterCommit, Tree: laterTree, Changed: false,
		}}, nil
	}
	setCommit, setTree, err := SetReviewBase(git, root, commit)
	if err != nil {
		return nil, err
	}
	return core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadReviewBaseSet{
		Root: root, Commit: setCommit, Tree: setTree, Changed: true,
	}}, nil
}

// scopedRepositories returns the requested repositories, each with the
// workspace checkout of the same repository it belongs to when it is not
// itself a workspace root.
func scopedRepositories(cwd string, discovered []string, paths []string) ([]string, map[string]string, error) {
	git := checkpoint.SystemGit{}
	common := func(root string) (string, error) {
		output, err := checkedGit(git, root, []string{"rev-parse", "--path-format=absolute", "--git-common-dir"})
		if err != nil {
			return "", err
		}
		if !utf8.Valid(output) {
			return "", errors.New("Git common directory is not UTF-8")
		}
		path, err := workerRuntimeCanonicalize(strings.TrimSpace(string(output)))
		if err != nil {
			return "", fmt.Errorf("resolve Git common directory for %s: %w", root, err)
		}
		return path, nil
	}
	type allowedRepository struct {
		common string
		root   string
	}
	var allowed []allowedRepository
	for _, root := range discovered {
		repository, err := common(root)
		if err != nil {
			return nil, nil, err
		}
		allowed = append(allowed, allowedRepository{common: repository, root: root})
	}
	selected := []string{}
	origins := map[string]string{}
	for _, path := range paths {
		directory := path
		if !filepath.IsAbs(path) {
			directory = core.Join(cwd, path)
		}
		// Resolved as discovery resolves a root, so an override's recapture
		// reports the same repository keys the verdict recorded.
		root, ok := captureRepositoryRoot(git, directory)
		if !ok {
			return nil, nil, fmt.Errorf("%s is not in a Git repository", path)
		}
		repository, err := common(root)
		if err != nil {
			return nil, nil, err
		}
		index := slices.IndexFunc(allowed, func(candidate allowedRepository) bool {
			return workerRuntimePathEqual(candidate.common, repository)
		})
		if index < 0 {
			return nil, nil, fmt.Errorf("%s is not a worktree of a workspace repository", path)
		}
		sameRoot := func(candidate string) bool { return workerRuntimePathEqual(candidate, root) }
		if !slices.ContainsFunc(discovered, sameRoot) {
			origins[root] = allowed[index].root
		}
		if !slices.ContainsFunc(selected, sameRoot) {
			selected = append(selected, root)
		}
	}
	return selected, origins, nil
}
