// Port of mj-worker/src/acp/verdict_client.rs.

// Optional hosted GLiDE classification. Failures leave activity alone.

package worker

import (
	"context"
	"encoding/json"
	"log/slog"
	"net/http"
	"net/url"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// nextRequest numbers classifier requests, from 1 (Rust NEXT_REQUEST).
var nextRequest atomic.Uint64

// fastinoKeyCell is a set-once string (Rust `OnceLock<String>`).
type fastinoKeyCell struct {
	mu    sync.Mutex
	value *string
}

// Set stores the key; false when it was already set.
func (c *fastinoKeyCell) Set(value string) bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.value != nil {
		return false
	}
	c.value = &value
	return true
}

// Get is the key, when set.
func (c *fastinoKeyCell) Get() (string, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.value == nil {
		return "", false
	}
	return *c.value, true
}

// fastinoKey is the Fastino key the worker entry point sets once; hosted
// Jev requests from the default source carry it.
var fastinoKey fastinoKeyCell

func isGlideSizeRefusal(status int, body []byte) bool {
	panic("unported: mj-worker/src/acp/verdict_client.rs is_glide_size_refusal")
}

// hostedProxy is the hosted classifier proxy. It holds the provider keys, so
// every classifier request goes through it.
const hostedProxy = "https://mj-jev-proxy.eng-admin-a63.workers.dev"

// VerdictSource is where a launch's classifier requests go.
type VerdictSource interface {
	isVerdictSource()
}

// VerdictSourceHosted is the proxy at Endpoint; routes are joined onto its
// origin.
type VerdictSourceHosted struct{ Endpoint string }

// VerdictSourceDisabled: classification is off for this launch; used by
// isolated tests.
type VerdictSourceDisabled struct{}

func (VerdictSourceHosted) isVerdictSource()   {}
func (VerdictSourceDisabled) isVerdictSource() {}

// String is the Rust Debug form, which leaves out everything but the
// endpoint.
func (s VerdictSourceHosted) String() string {
	return "Hosted { endpoint: " + sessionConfigDebugString(s.Endpoint) + ", .. }"
}

func (VerdictSourceDisabled) String() string { return "Disabled" }

// VerdictClient is a hosted classifier client. It is a value: copies share
// its HTTP client (Rust Clone).
type VerdictClient struct {
	endpoint string
	client   *http.Client
	// log is the decision log classifications record into; nil for none.
	log *core.DecisionLog
}

// VerdictAttempt is one classifier request's diagnostics, kept alive
// through application so cancellation has a terminal log event too. Close
// it when its owner is done (Rust Drop).
type VerdictAttempt struct {
	diagnostic *core.Attempt
	decision   *core.Decision
	uncertain  bool
	id         uint64
	session    string
	generation uint64
	phase      core.TurnPhase
	started    time.Time
	finished   bool
	// logger is the logger current when the attempt started (Rust keeps the
	// tracing dispatcher).
	logger *slog.Logger
}

// finish records the attempt's outcome.
func (a *VerdictAttempt) finish(outcome, reason string) {
	panic("unported: mj-worker/src/acp/verdict_client.rs VerdictAttempt::finish")
}

// Close finishes an unfinished attempt as cancelled (Rust Drop).
func (a *VerdictAttempt) Close() {
	if !a.finished {
		a.finish("cancelled", "request_or_owner_dropped")
	}
}

// hostedGlideURL is the proxy URL for a GLiDE route.
func (c VerdictClient) hostedGlideURL(route string) (*url.URL, error) {
	endpoint, err := url.Parse(c.endpoint)
	if err != nil {
		return nil, err
	}
	return endpoint.Parse(route)
}

// sameAfkQuestion compares complete forms. It returns true when
// currentQuestion repeats earlier ones (hold it for the person) and false
// when GLiDE finds a new decision (decline it for now). The request has no
// size cap and no truncation: an oversized request or any other failure is
// an error and the question is held.
func (c VerdictClient) sameAfkQuestion(ctx context.Context, session string, currentQuestion any, previousQuestions []any) (bool, error) {
	panic("unported: mj-worker/src/acp/verdict_client.rs VerdictClient::same_afk_question")
}

// afkProbabilities posts the redacted state to the hosted route and returns
// (same, different).
func (c VerdictClient) afkProbabilities(ctx context.Context, body json.RawMessage) (same float64, different float64, err error) {
	panic("unported: mj-worker/src/acp/verdict_client.rs VerdictClient::afk_probabilities")
}

func (c VerdictClient) logSingleQuestion(session string, request *core.Request, stableID string) {
	panic("unported: mj-worker/src/acp/verdict_client.rs VerdictClient::log_single_question")
}

// locateQuestion asks GLiDE which reply holds the open question. The frozen
// rule picks the most probable of the replies and `none` (core.Parse, Rust
// question_locator::parse); ok is false for `none`.
//
// The state builder keeps the request far below the proxy's body bound, so
// an explicit size refusal is a failure like any other: no retry, no target
// stored.
func (c VerdictClient) locateQuestion(ctx context.Context, session string, request *core.Request) (index int, ok bool, err error) {
	panic("unported: mj-worker/src/acp/verdict_client.rs VerdictClient::locate_question")
}

// verdictClientResolve is the client for source, or nil when classification
// is off or unavailable. A nil source is the default hosted proxy.
func verdictClientResolve(ctx context.Context, source VerdictSource) *VerdictClient {
	panic("unported: mj-worker/src/acp/verdict_client.rs VerdictClient::resolve")
}

func verdictClientResolveBlocking(source VerdictSource) *VerdictClient {
	return verdictClientResolveWith(source, core.DisabledByEnvironment())
}

// verdictClientResolveWith: jevDisabled is `[jev] enabled = false` as the
// launch passed it. The switch covers the default source only: an explicit
// source is a test's own endpoint.
func verdictClientResolveWith(source VerdictSource, jevDisabled bool) *VerdictClient {
	panic("unported: mj-worker/src/acp/verdict_client.rs VerdictClient::resolve_with")
}

// verdictClientNewWithKey builds a client for a hosted source. A nil
// fastinoKey sends no key.
func verdictClientNewWithKey(source VerdictSource, fastinoKey *string) (*VerdictClient, error) {
	panic("unported: mj-worker/src/acp/verdict_client.rs VerdictClient::new_with_key")
}

func (c VerdictClient) withLog(log *core.DecisionLog) VerdictClient {
	c.log = log
	return c
}

func (c VerdictClient) askHandbackLogged(ctx context.Context, session string, generation uint64, evidence *core.TurnEvidence) (*VerdictAttempt, core.HandbackProbabilities, error) {
	panic("unported: mj-worker/src/acp/verdict_client.rs VerdictClient::ask_handback_logged")
}

func (c VerdictClient) askGlideLogged(
	ctx context.Context,
	session string,
	generation uint64,
	evidence *core.TurnEvidence,
	facts *core.ActivityFacts,
	native int,
) (*VerdictAttempt, core.Probabilities, error) {
	panic("unported: mj-worker/src/acp/verdict_client.rs VerdictClient::ask_glide_logged")
}

// postGlide posts a GLiDE request, retrying only an explicit size refusal,
// and only with a strictly smaller body. With keepInstructions, a refusal
// that only a history without some of the person's instruction text could
// answer fails instead of judging against partial instructions.
func (c VerdictClient) postGlide(
	ctx context.Context,
	endpoint *url.URL,
	history core.ContextHistory,
	keepInstructions bool,
	timeout time.Duration,
	project func(history *core.ContextHistory) (any, error),
) (*http.Response, error) {
	panic("unported: mj-worker/src/acp/verdict_client.rs VerdictClient::post_glide")
}

// askGlideTurn is the completed-turn transport only; callers retain
// ownership of action and logging.
func (c VerdictClient) askGlideTurn(
	ctx context.Context,
	evidence *core.TurnEvidence,
	authorization core.ContextHistory,
	facts *core.ActivityFacts,
	nativeAgentCount int,
) (core.Probabilities, error) {
	panic("unported: mj-worker/src/acp/verdict_client.rs VerdictClient::ask_glide_turn")
}
