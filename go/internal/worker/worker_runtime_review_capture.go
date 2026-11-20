// Port of mj-worker/src/worker_runtime/review_capture.rs.

// Worker-owned capture operations for child-session reviews.

//go:build unix

package worker

import (
	"context"
	"sync"

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

// Handle runs one reviewer request on its own task, logging a failure.
func (c *reviewCapture) Handle(ctx context.Context, request core.ReviewerRequest) (core.RelayResponseBody, error) {
	panic("unported: mj-worker/src/worker_runtime/review_capture.rs ReviewCapture::handle")
}

func (c *reviewCapture) run(ctx context.Context, request core.ReviewerRequest) (core.RelayResponseBody, error) {
	panic("unported: mj-worker/src/worker_runtime/review_capture.rs ReviewCapture::run")
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
	panic("unported: mj-worker/src/worker_runtime/review_capture.rs ReviewCapture::capture_delta")
}

// setReviewBase makes a reviewed or overridden commit the base of the next
// push review.
func (c *reviewCapture) setReviewBase(ctx context.Context, path *string, commit string) (core.RelayResponseBody, error) {
	panic("unported: mj-worker/src/worker_runtime/review_capture.rs ReviewCapture::set_review_base")
}

// scopedRepositories returns the requested repositories, each with the
// workspace checkout of the same repository it belongs to when it is not
// itself a workspace root.
func scopedRepositories(cwd string, discovered []string, paths []string) ([]string, map[string]string, error) {
	panic("unported: mj-worker/src/worker_runtime/review_capture.rs scoped_repositories")
}
