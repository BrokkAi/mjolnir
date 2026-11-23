// Port of mj-worker/src/worker_runtime/history.rs.

// Transient history requests. No history payload is written to worker
// state.

//go:build unix

package worker

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// historyPending is one history query waiting for the controller's answer.
type historyPending struct {
	request core.HistoryRequest
	expires time.Time
	// reply is the oneshot sender (buffered for one result); closed is
	// closed once the waiting caller has gone (the receiver dropped).
	reply  chan core.HistoryResult
	closed <-chan struct{}
}

// replyClosed is Rust's oneshot::Sender::is_closed.
func (p *historyPending) replyClosed() bool {
	select {
	case <-p.closed:
		return true
	default:
		return false
	}
}

// historyEndpoint holds the history queries waiting for the controller.
// Copies of the pointer share it, as Rust's Clone of the Arcs does.
type historyEndpoint struct {
	mu      sync.Mutex
	pending map[string]*historyPending
	// permits is a semaphore of core.MaxPending tokens.
	permits chan struct{}
	// queryTimeout is core.QueryTimeout. A field only so the test that Rust
	// runs on tokio's paused clock can shorten it.
	queryTimeout time.Duration
}

// defaultHistoryEndpoint is Rust's Default.
func defaultHistoryEndpoint() *historyEndpoint {
	return &historyEndpoint{
		pending:      map[string]*historyPending{},
		permits:      make(chan struct{}, core.MaxPending),
		queryTimeout: core.QueryTimeout,
	}
}

// retainLive drops expired and abandoned queries. The caller holds mu.
func (e *historyEndpoint) retainLive() {
	now := time.Now()
	for id, entry := range e.pending {
		if !entry.expires.After(now) || entry.replyClosed() {
			delete(e.pending, id)
		}
	}
}

// Requests are the live queries for the controller, after dropping expired
// and abandoned ones.
func (e *historyEndpoint) Requests() []core.HistoryRequest {
	e.mu.Lock()
	defer e.mu.Unlock()
	e.retainLive()
	requests := make([]core.HistoryRequest, 0, len(e.pending))
	for _, id := range slices.Sorted(maps.Keys(e.pending)) {
		requests = append(requests, e.pending[id].request)
	}
	return requests
}

// Complete delivers the controller's answer to its waiting query.
func (e *historyEndpoint) Complete(result core.HistoryResult) {
	e.mu.Lock()
	entry, ok := e.pending[result.RequestID]
	delete(e.pending, result.RequestID)
	e.mu.Unlock()
	if !ok {
		return
	}
	if entry.replyClosed() {
		slog.Debug("history caller disconnected before its response")
		return
	}
	select {
	case entry.reply <- result:
	default:
		slog.Debug("history caller disconnected before its response")
	}
}

// Query queues a history query for the controller and waits for its answer.
func (e *historyEndpoint) Query(ctx context.Context, query core.HistoryQuery, cwd string) (core.HistoryResult, error) {
	if err := query.Validate(); err != nil {
		return core.HistoryResult{}, err
	}
	select {
	case e.permits <- struct{}{}:
	default:
		return core.HistoryResult{}, errors.New("too many pending history queries; retry shortly")
	}
	defer func() { <-e.permits }()
	id, err := core.NewSessionID()
	if err != nil {
		return core.HistoryResult{}, err
	}
	deadline := time.Now().Add(e.queryTimeout)
	var blame *core.BlameEvidence
	if file, ok := query.(core.HistoryQueryBlameFile); ok {
		blameContext, cancel := context.WithDeadline(ctx, deadline)
		evidence, err := captureBlame(blameContext, cwd, file.Path, file.StartLine, file.EndLine)
		timedOut := errors.Is(blameContext.Err(), context.DeadlineExceeded)
		cancel()
		if timedOut {
			return core.HistoryResult{}, errors.New("target Git blame timed out")
		}
		if err != nil {
			return core.HistoryResult{}, err
		}
		blame = &evidence
	}
	reply := make(chan core.HistoryResult, 1)
	closed := make(chan struct{})
	e.mu.Lock()
	e.retainLive()
	if len(e.pending) >= core.MaxPending {
		e.mu.Unlock()
		return core.HistoryResult{}, errors.New("too many pending history queries; retry shortly")
	}
	e.pending[id] = &historyPending{
		request: core.HistoryRequest{RequestID: id, Query: query, Blame: blame},
		expires: deadline,
		reply:   reply,
		closed:  closed,
	}
	e.mu.Unlock()
	// The caller leaving, by answer, timeout or cancellation, drops its
	// pending entry (Rust's Cleanup guard and the receiver's drop).
	defer func() {
		close(closed)
		e.mu.Lock()
		delete(e.pending, id)
		e.mu.Unlock()
	}()
	timer := time.NewTimer(time.Until(deadline))
	defer timer.Stop()
	select {
	case result := <-reply:
		return result, nil
	case <-timer.C:
		return core.HistoryResult{}, errors.New("history query timed out; controller may be disconnected")
	case <-ctx.Done():
		return core.HistoryResult{}, ctx.Err()
	}
}

func historyGit(ctx context.Context, cwd string, args []string) (string, error) {
	command := exec.Command("git", append([]string{"--no-pager", "-c", "core.fsmonitor=", "-c", "core.hooksPath="}, args...)...)
	command.Dir = cwd
	var environment []string
	for _, entry := range os.Environ() {
		if !strings.HasPrefix(entry, "GIT_") {
			environment = append(environment, entry)
		}
	}
	command.Env = append(environment, "GIT_CONFIG_NOSYSTEM=1", "GIT_TERMINAL_PROMPT=0", "GIT_PAGER=cat")
	output, err := core.RunBounded(ctx, command, core.MaxBlameBytes, 20*time.Second)
	if err != nil {
		return "", err
	}
	if !output.Status.Success() {
		return "", fmt.Errorf("git failed: %s", fromUTF8Lossy(output.Stderr))
	}
	if !utf8.Valid(output.Stdout) {
		return "", errors.New("Git output is not UTF-8")
	}
	return string(output.Stdout), nil
}

func captureBlame(ctx context.Context, cwd string, path string, start int, end int) (core.BlameEvidence, error) {
	if !filepath.IsAbs(path) {
		path = filepath.Join(cwd, path)
	}
	path, err := workerRuntimeCanonicalize(path)
	if err != nil {
		return core.BlameEvidence{}, fmt.Errorf("resolve target file for blame: %w", err)
	}
	parent, ok := workerRuntimePathParent(path)
	if !ok {
		return core.BlameEvidence{}, errors.New("file has no parent")
	}
	root, err := historyGit(ctx, parent, []string{"rev-parse", "--show-toplevel"})
	if err != nil {
		return core.BlameEvidence{}, err
	}
	repository := strings.TrimSpace(root)
	relativePath, ok := workerRuntimeStripPrefix(path, repository)
	if !ok {
		return core.BlameEvidence{}, errors.New("file is outside Git repository")
	}
	porcelain, err := historyGit(ctx, repository, []string{
		"blame", "--line-porcelain", "-M", "-C", fmt.Sprintf("-L%d,%d", start, end), "--", relativePath,
	})
	if err != nil {
		return core.BlameEvidence{}, err
	}
	return core.BlameEvidence{Repository: repository, RelativePath: relativePath, Porcelain: porcelain}, nil
}
