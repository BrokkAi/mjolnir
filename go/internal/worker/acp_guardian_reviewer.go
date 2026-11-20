// Port of mj-worker/src/acp/guardian/reviewer.rs.

// The reviewer side of the Mjolnir guardian.
//
// A Guardian belongs to one bridge, and runBridge stops its reviewer after
// that bridge's own teardown on every exit path. On its first review it
// starts a second process from the session's own launch: the same harness,
// staged home and credentials, so the same harness configuration and model.
// Each review opens a fresh session there, so no review inherits another's
// history.
//
// The reviewer's launch is narrowed for it (see reviewerLaunch): an OpenCode
// reviewer gets none of the session's Mjolnir plugins, so it can never
// drain the session's mailbox, and its hook and rules let it read and
// nothing else. Every harness's reviewer runs under the session's execution
// enforcement, its connection advertises no client terminal, file system,
// or elicitation, Mjolnir passes it no MCP servers, and every permission
// request it raises is refused once.

package worker

import (
	"context"
	"sync"
	"sync/atomic"
	"time"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// reviewTimeout is Codex's `REVIEW_TIMEOUT`: one deadline for a whole
// review, including starting the reviewer and every attempt. Waiting while
// another review starts the shared reviewer is a queue for it, not this
// review's own work, so the deadline starts after that.
const reviewTimeout = 90 * time.Second

// maxReviewAttempts is Codex's `MAX_REVIEW_ATTEMPTS`: reviewer prompts per
// review, counting a retry on a fresh reviewer and a reminder for a reply
// without a verdict.
const maxReviewAttempts = 3

// reviewerExitGrace is how long a retiring reviewer bridge gets to reap its
// process group after its stdin closes, the grace runBridge gives the
// session's own bridge. Tests shorten it to 100 milliseconds (Rust
// cfg(test)).
var reviewerExitGrace = 5 * time.Second

// reviewerSupervisorSpec is the reviewer's supervisor spec, beside the
// session's (Unix workers only).
const reviewerSupervisorSpec = "acp-supervisor-guardian.json"

// HostSlot is a held lock on a guardian's reviewer host (Rust a tokio
// MutexGuard over `Option<Arc<ReviewerHost>>`). Release it when done.
type HostSlot struct {
	guardian *Guardian
}

// host is the slot's reviewer, nil when there is none.
func (s *HostSlot) host() **ReviewerHost {
	return &s.guardian.host
}

// release unlocks the slot.
func (s *HostSlot) release() {
	<-s.guardian.hostLock
}

// Guardian is the guardian for one bridge: the reviewer host it starts on
// demand and the launch it reviews for.
type Guardian struct {
	spec *LaunchSpec
	// hostLock holds one token while a review owns host; a review keeps it
	// across its awaits (Rust tokio::sync::Mutex).
	hostLock chan struct{}
	host     *ReviewerHost
}

// guardianForLaunch is the guardian for spec, when its harness relies on
// Mjolnir's, its target keeps configured approvals, and its profile has not
// turned the guardian off; nil otherwise. The guardian keeps its own copy
// of the launch.
func guardianForLaunch(spec *LaunchSpec) *Guardian {
	if !spec.Harness.MjolnirGuardianReviews(spec.ExecutionPolicy, spec.ApprovalChoices) {
		return nil
	}
	clone := spec.Clone()
	return &Guardian{spec: &clone, hostLock: make(chan struct{}, 1)}
}

// lockHost waits for the host slot, or for ctx to end.
func (g *Guardian) lockHost(ctx context.Context) (*HostSlot, error) {
	select {
	case g.hostLock <- struct{}{}:
		return &HostSlot{guardian: g}, nil
	case <-ctx.Done():
		return nil, ctx.Err()
	}
}

// review reviews one permission request. A nil input is no input.
func (g *Guardian) review(ctx context.Context, request *schema.RequestPermissionRequest, input any, nativeChild bool) GuardianDecision {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs Guardian::review")
}

// decide reviews action against the evidence as it stands. If the person
// wrote while it was reviewed, the verdict may rest on stale authorization,
// so it is reviewed again on what they now say, as Codex discards a stale
// verdict (`review_request.rs`). Each repeat follows a new message; the
// review deadline bounds them.
func (g *Guardian) decide(ctx context.Context, first *HostSlot, action any) (Assessment, error) {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs Guardian::decide")
}

// assess runs the review's attempts and returns the first verdict. Every
// retry follows a change: a reviewer whose process died is replaced, and a
// reply without a verdict is asked for only the verdict. Any other failure
// would repeat, so it ends the review. *slot is the held host slot, or nil
// once released.
func (g *Guardian) assess(ctx context.Context, slot **HostSlot, prompt []schema.ContentBlock, attempts *int) (Assessment, error) {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs Guardian::assess")
}

// assessOn is one reviewer session's attempts: the prompt, then the verdict
// reminder while replies carry none and attempts remain.
func (g *Guardian) assessOn(ctx context.Context, host *ReviewerHost, prompt []schema.ContentBlock, attempts *int) (Assessment, error) {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs Guardian::assess_on")
}

// currentHost is the live reviewer in slot, started now if there is none.
func (g *Guardian) currentHost(ctx context.Context, slot *HostSlot) (*ReviewerHost, error) {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs Guardian::current_host")
}

// discard forgets host if it is still the current reviewer, and stops it.
func (g *Guardian) discard(ctx context.Context, host *ReviewerHost) {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs Guardian::discard")
}

// shutdown stops the reviewer with the bridge it belongs to.
func (g *Guardian) shutdown(ctx context.Context) {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs Guardian::shutdown")
}

// ReviewerLaunch is how the reviewer's bridge is started.
type ReviewerLaunch struct {
	command     string
	args        []string
	environment map[string]string
}

// reviewerLaunch is the session's launch, narrowed for a reviewer. A
// production bridge is the worker's ACP supervisor, which takes the harness
// environment from its spec file, so the reviewer gets a spec of its own
// beside the session's.
func reviewerLaunch(spec *LaunchSpec) (ReviewerLaunch, error) {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs reviewer_launch")
}

// reviewerEnvironment is the harness environment a reviewer runs with.
func reviewerEnvironment(harness core.HarnessKind, environment map[string]string) (map[string]string, error) {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs reviewer_environment")
}

// Replies holds the text each reviewer session has sent as its latest agent
// message. The zero value is ready to use.
type Replies struct {
	mu      sync.Mutex
	replies map[string]*ReviewerReply
}

type ReviewerReply struct {
	messageID *string
	text      string
}

// observe records a session update (a JSON value) of a reviewer session.
func (r *Replies) observe(sessionID string, update any) {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs Replies::observe")
}

func (r *Replies) open(sessionID string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.replies == nil {
		r.replies = map[string]*ReviewerReply{}
	}
	r.replies[sessionID] = &ReviewerReply{}
}

func (r *Replies) take(sessionID string) string {
	r.mu.Lock()
	defer r.mu.Unlock()
	reply, ok := r.replies[sessionID]
	if !ok {
		return ""
	}
	reply.messageID = nil
	text := reply.text
	reply.text = ""
	return text
}

func (r *Replies) close(sessionID string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	delete(r.replies, sessionID)
}

// ReviewerHost is a running reviewer bridge and its ACP connection. Close
// stops a host dropped without shutdown: its task closes the transport and
// reaps the bridge, logging what fails (Rust Drop).
type ReviewerHost struct {
	connection    *acp.Connection
	supportsClose bool
	replies       *Replies
	// stopped is done once the host is told to stop; stop tells it.
	stopped context.Context
	stop    context.CancelFunc
	// taskMu guards task, the bridge's supervising task until shutdown takes
	// it.
	taskMu sync.Mutex
	task   *core.JoinHandle[struct{}]
	// gone is done once the bridge or its connection is gone, by whichever
	// observer sees it first (see reviewerHostUntilExit); markGone records
	// it. The one owner of whether this reviewer is alive: a request to a
	// dead bridge may never be answered, so every request races it.
	gone     context.Context
	markGone context.CancelFunc
}

// reviewerHostStart starts the reviewer bridge for spec and initializes its
// connection.
func reviewerHostStart(ctx context.Context, spec *LaunchSpec) (*ReviewerHost, error) {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs ReviewerHost::start")
}

func (h *ReviewerHost) alive() bool {
	return h.gone.Err() == nil
}

// reviewerHostUntilExit runs work against host, failing it if the bridge
// exits.
//
// A request can learn that the bridge is gone before this host's task does:
// the connection fails it with "incoming transport closed" first. That
// failure is recorded in gone before it is returned, so the decision to
// retry, which reads only gone, sees the death whichever observer saw it
// first.
func reviewerHostUntilExit[T any](ctx context.Context, host *ReviewerHost, work func(ctx context.Context) (T, error)) (T, error) {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs ReviewerHost::until_exit")
}

// openSession opens a reviewer session configured like the reviewed one.
func (h *ReviewerHost) openSession(ctx context.Context, spec *LaunchSpec) (*ReviewerSession, error) {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs ReviewerHost::open_session")
}

func (h *ReviewerHost) shutdown(ctx context.Context) {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs ReviewerHost::shutdown")
}

// initializeReviewer initializes ACP without client terminal, file system,
// or elicitation, and reports whether the bridge can close sessions.
func initializeReviewer(ctx context.Context, connection *acp.Connection, spec *LaunchSpec) (bool, error) {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs initialize_reviewer")
}

// Close stops the host (Rust Drop).
func (h *ReviewerHost) Close() {
	h.stop()
}

// ReviewerSession is one review's session on the reviewer bridge. Closing
// it cancels a turn still running, as a cancelled or timed-out review leaves
// one, and closes the session in the background, outside any review's
// deadline (Rust Drop).
type ReviewerSession struct {
	host      *ReviewerHost
	id        schema.SessionId
	prompting atomic.Bool
}

// prompt sends one prompt and returns the reviewer's latest message.
func (s *ReviewerSession) prompt(ctx context.Context, blocks []schema.ContentBlock) (string, error) {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs ReviewerSession::prompt")
}

func (s *ReviewerSession) Close() {
	panic("unported: mj-worker/src/acp/guardian/reviewer.rs ReviewerSession::drop")
}
