// Port of mj-worker/src/acp/verdict_client.rs.

// Optional hosted GLiDE classification. Failures leave activity alone.

package worker

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

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

// verdictResponseBound is the hosted proxy's response bound, 64 KiB.
const verdictResponseBound = 64 * 1024

func isGlideSizeRefusal(status int, body []byte) bool {
	value, err := jsonvalue.Parse(body)
	if err != nil {
		return false
	}
	failure, _ := acpFeaturesGet(value, "error")
	code, _ := acpFeaturesGet(failure, "code")
	return (status == http.StatusRequestEntityTooLarge && failure == "body_too_large") ||
		// Verified against Fastino with synthetic oversized input, 2026-11-12.
		(status == http.StatusBadRequest && code == "input_too_long")
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
	// headers are sent with every request (reqwest's default headers): the
	// Fastino key for the default source.
	headers http.Header
	// log is the decision log classifications record into; nil for none.
	log *core.DecisionLog
}

// verdictClientTimeout is the client's default request timeout, for a
// request that sets none of its own.
const verdictClientTimeout = 10 * time.Second

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
	logger := a.logger
	if logger == nil {
		logger = slog.Default()
	}
	logger.Info("Jev decision outcome", "target", "mj_jev", "request_id", a.id, "session", a.session,
		"generation", a.generation, "phase", verdictDebugVariant(string(a.phase)),
		"elapsed_ms", uint64(time.Since(a.started).Milliseconds()), "outcome", outcome, "reason", reason)
	if a.diagnostic != nil {
		status, action := a.diagnosticOutcome(outcome, reason)
		var applied any
		switch {
		case outcome == "applied" && reason == "inferred_finished":
			applied = "InferredFinished"
		case outcome == "applied" && a.decision != nil:
			applied = verdictDebugVariant(string(*a.decision))
		}
		technical, err := jsonvalue.Marshal(jsonvalue.Object{
			{Key: "outcome", Value: outcome},
			{Key: "reason", Value: reason},
			{Key: "applied_decision", Value: applied},
		})
		if err == nil {
			a.diagnostic.Update(nil, technical)
		}
		a.diagnostic.Finish(status, action)
	}
	a.finished = true
}

// diagnosticOutcome is the decision log's status and action for an attempt
// that ended with outcome and reason.
func (a *VerdictAttempt) diagnosticOutcome(outcome, reason string) (string, string) {
	switch {
	case outcome == "bounced" && reason == "handback_unfinished":
		return "bounced", "Handback was not delivered; the child was asked to continue unfinished assigned work."
	case outcome == "delivered" && reason == "handback_accepted":
		return "delivered", "Handback check allowed delivery to the parent."
	case reason == "assessment_uncertain":
		return "uncertain", "Jev was uncertain; Mj retained the result until relevant evidence changes."
	case reason == "authorized_continuation_ready":
		return "deferred", "Authorized unfinished work remains; continuation awaits atomic worker admission."
	case reason == "quota_resolution_required":
		return "deferred", "Subscription quota reached; the controller will resolve the provider reset deadline."
	case reason == "automatic_action_suppressed":
		return "suppressed", "User input, pause, or a budget limit prevents automatic work."
	case reason == "stale_turn":
		return "stale", "A newer turn or user action superseded this assessment."
	case reason == "request_failed":
		return "failed", "Jev request failed; mj kept the runtime status."
	case reason == "structured_request_open":
		return "suppressed", "The harness has a question open for the person; mj kept the turn waiting for the answer."
	case outcome == "discarded":
		return "stale", "New activity superseded this assessment; mj kept the runtime status."
	case outcome == "applied":
		switch {
		case reason == "inferred_finished":
			return "applied", "Mj marked the session ready."
		case reason == "server_retry_armed":
			return "applied", "Mj scheduled a retry for a transient provider failure."
		case a.decision != nil && *a.decision == core.DecisionInferIdle:
			return "applied", "Mj marked the session ready."
		case a.decision != nil && *a.decision == core.DecisionExpectContinuation:
			return "applied", "Mj is expecting the agent to follow up."
		}
		return "applied", "Mj marked the session as awaiting input."
	case outcome == "unchanged" && reason == "keep_current" && a.uncertain:
		return "uncertain", "The independent assessments did not meet the action thresholds; mj kept the runtime status."
	case outcome == "unchanged" && reason == "keep_current":
		return "unchanged", "This answer does not change the status in this phase."
	case outcome == "cancelled":
		return "cancelled", "Check cancelled before an assessment was applied."
	}
	return outcome, fmt.Sprintf("Mj kept the runtime status: %s.", strings.ReplaceAll(reason, "_", " "))
}

// Close finishes an unfinished attempt as cancelled (Rust Drop).
func (a *VerdictAttempt) Close() {
	if !a.finished {
		a.finish("cancelled", "request_or_owner_dropped")
	}
}

// verdictDebugVariant is a snake_case wire variant as Rust's derived Debug
// names it: `infer_idle` is `InferIdle`.
func verdictDebugVariant(value string) string {
	var name strings.Builder
	for _, word := range strings.Split(value, "_") {
		if word == "" {
			continue
		}
		name.WriteString(strings.ToUpper(word[:1]))
		name.WriteString(word[1:])
	}
	return name.String()
}

// verdictStatusDisplay is the Display of an http::StatusCode: the code and
// its canonical reason, which for a few codes is worded differently from
// net/http's.
func verdictStatusDisplay(status int) string {
	reason := http.StatusText(status)
	switch status {
	case http.StatusNonAuthoritativeInfo:
		reason = "Non Authoritative Information"
	case http.StatusRequestEntityTooLarge:
		reason = "Payload Too Large"
	case http.StatusRequestURITooLong:
		reason = "URI Too Long"
	case http.StatusRequestedRangeNotSatisfiable:
		reason = "Range Not Satisfiable"
	}
	if reason == "" {
		reason = "<unknown status code>"
	}
	return strconv.Itoa(status) + " " + reason
}

// hostedGlideURL is the proxy URL for a GLiDE route.
func (c VerdictClient) hostedGlideURL(route string) (*url.URL, error) {
	endpoint, err := url.Parse(c.endpoint)
	if err != nil {
		return nil, err
	}
	return endpoint.Parse(route)
}

// cancelOnClose ends a request's timeout once its body is closed, so the
// timeout covers reading the body as reqwest's does.
type cancelOnClose struct {
	io.ReadCloser
	cancel context.CancelFunc
}

func (c *cancelOnClose) Close() error {
	defer c.cancel()
	return c.ReadCloser.Close()
}

// post sends a POST with the client's headers; timeout covers the whole
// exchange, the response body included, until the body is closed.
func (c VerdictClient) post(ctx context.Context, endpoint *url.URL, timeout time.Duration, body []byte) (*http.Response, error) {
	if timeout <= 0 {
		timeout = verdictClientTimeout
	}
	requestCtx, cancel := context.WithTimeout(ctx, timeout)
	request, err := http.NewRequestWithContext(requestCtx, http.MethodPost, endpoint.String(), bytes.NewReader(body))
	if err != nil {
		cancel()
		return nil, err
	}
	for name, values := range c.headers {
		request.Header[name] = values
	}
	request.Header.Set("Content-Type", "application/json")
	response, err := c.client.Do(request)
	if err != nil {
		cancel()
		return nil, err
	}
	response.Body = &cancelOnClose{ReadCloser: response.Body, cancel: cancel}
	return response, nil
}

// verdictDropped reports whether err is the cancellation of ctx: the caller
// dropped the request before the service answered. Cancellation decides the
// record only when it interrupted the request. Once the service has answered,
// even with a refusal or a malformed body, that answer is the outcome, as when
// Rust's future completes before it is dropped; a cancellation arriving later
// must not turn it into a cancelled record.
func verdictDropped(ctx context.Context, err error) bool {
	return err != nil && ctx.Err() != nil && errors.Is(err, ctx.Err())
}

// verdictRetryAfter is a response's Retry-After header, nil when absent.
func verdictRetryAfter(response *http.Response) *string {
	values, ok := response.Header["Retry-After"]
	if !ok || len(values) == 0 {
		return nil
	}
	return &values[0]
}

// verdictTechnical encodes a decision log's technical details.
func verdictTechnical(members jsonvalue.Object) json.RawMessage {
	encoded, err := jsonvalue.Marshal(members)
	if err != nil {
		panic("classifier diagnostics serialize: " + err.Error())
	}
	return encoded
}

func (c VerdictClient) sameAfkQuestion(ctx context.Context, session string, currentQuestion any, previousQuestions []any) (bool, error) {
	// The ordinary egress redactor collapses all long identifiers. Keep their
	// equality within this comparison without sending their contents.
	anonymizeSecretChoices := func(form any, labels map[string]int) {
		fieldsValue, _ := acpFeaturesGet(form, "fields")
		fields, ok := fieldsValue.([]any)
		if !ok {
			panic("normalized form fields")
		}
		for _, value := range fields {
			field, ok := value.(jsonvalue.Object)
			if !ok {
				continue
			}
			// questionEvidence has already resolved inherited secrecy.
			if secret, _ := field.Get("secret"); secret != true {
				continue
			}
			for index := range field {
				if field[index].Key == "options" {
					core.AnonymizeJSONStrings(&field[index].Value, labels)
				}
			}
		}
	}
	labels := map[string]int{}
	previous := make([]any, len(previousQuestions))
	copy(previous, previousQuestions)
	state := jsonvalue.Object{
		{Key: "current_question", Value: currentQuestion},
		{Key: "previous_questions", Value: previous},
	}.Clone()
	current, _ := state.Get("current_question")
	anonymizeSecretChoices(current, labels)
	forms, _ := state.Get("previous_questions")
	for _, form := range forms.([]any) {
		anonymizeSecretChoices(form, labels)
	}
	var redacted any = state
	core.RedactJSONStringsDistinct(&redacted, labels)
	body := core.RequestBody(redacted, nil)
	var diagnostic *core.Attempt
	if c.log != nil {
		attempt := c.log.Start(session, "activity",
			"Does this question repeat one already declined while away?",
			"The question the agent just asked and the earlier questions declined for the same agent.")
		diagnostic = &attempt
		defer attempt.Release()
		attempt.Update(nil, verdictTechnical(jsonvalue.Object{
			{Key: "request", Value: body},
			{Key: "contract", Value: "afk-question-glide-v1"},
		}))
	}
	same, different, err := c.afkProbabilities(ctx, body)
	if verdictDropped(ctx, err) {
		// Dropped mid-request: the released attempt records its cancellation.
		return false, ctx.Err()
	}
	if diagnostic != nil {
		if err == nil {
			diagnostic.Update(nil, verdictTechnical(jsonvalue.Object{
				{Key: "probabilities", Value: jsonvalue.Object{{Key: "same", Value: same}, {Key: "different", Value: different}}},
			}))
			if different > same {
				diagnostic.Finish("declined_new",
					"Classified as a new decision; the worker declines it unless the state changed first.")
			} else {
				diagnostic.Finish("held_repeat", "Held the question for the person.")
			}
		} else {
			diagnostic.Update(nil, verdictTechnical(jsonvalue.Object{{Key: "error", Value: err.Error()}}))
			diagnostic.Finish("failed", "The check failed; the question is held for the person.")
		}
	}
	if err != nil {
		return false, err
	}
	// A tie does not establish a new decision: the frozen rule is `different`
	// only when its probability is strictly higher.
	return different <= same, nil
}

func (c VerdictClient) afkProbabilities(ctx context.Context, body json.RawMessage) (same float64, different float64, err error) {
	endpoint, err := c.hostedGlideURL("/v1/afk-question-glide")
	if err != nil {
		return 0, 0, err
	}
	// Fastino: "set your client read timeout above 170 seconds"
	// https://docs.fastino.ai/troubleshooting/retries (supersedes older 300s
	// notes).
	response, err := c.post(ctx, endpoint, 180*time.Second, body)
	if err != nil {
		return 0, 0, fmt.Errorf("classify AFK question: %w", err)
	}
	defer response.Body.Close()
	status := response.StatusCode
	if status >= 200 && status < 300 {
		core.ObserveResponse(status, nil, nil)
	}
	retryAfter := verdictRetryAfter(response)
	payload, err := io.ReadAll(response.Body)
	if err != nil {
		return 0, 0, fmt.Errorf("read AFK question verdict: %w", err)
	}
	if status == http.StatusTooManyRequests {
		core.ObserveResponse(status, retryAfter, payload)
	}
	if len(payload) > verdictResponseBound {
		return 0, 0, errors.New("AFK question verdict exceeds proxy response bound")
	}
	if status < 200 || status >= 300 {
		return 0, 0, fmt.Errorf("AFK question HTTP %s: %s", verdictStatusDisplay(status), fromUTF8Lossy(payload))
	}
	value, err := jsonvalue.Parse(payload)
	if err != nil {
		return 0, 0, err
	}
	scoresValue, _ := acpFeaturesGet(value, "probabilities")
	scores, ok := scoresValue.(jsonvalue.Object)
	if !ok {
		return 0, 0, errors.New("missing AFK question distribution")
	}
	if len(scores.Keys()) != 2 {
		return 0, 0, errors.New("invalid AFK question distribution")
	}
	probability := func(label string) (float64, error) {
		value, _ := scores.Get(label)
		p, ok := acpFeaturesAsF64(value)
		if !ok {
			return 0, fmt.Errorf("missing %s probability", label)
		}
		if p != p || p < 0 || p > 1 {
			return 0, errors.New("invalid AFK question probabilities")
		}
		return p, nil
	}
	if same, err = probability("same"); err != nil {
		return 0, 0, err
	}
	if different, err = probability("different"); err != nil {
		return 0, 0, err
	}
	if sum := same + different - 1; sum > 0.03 || sum < -0.03 {
		return 0, 0, errors.New("invalid AFK question probabilities")
	}
	return same, different, nil
}

func (c VerdictClient) logSingleQuestion(session string, request *core.Request, stableID string) {
	if c.log == nil {
		return
	}
	attempt := c.log.Start(session, "activity",
		"Which reply contains the open question?",
		"Only one assistant reply followed the latest user message.")
	defer attempt.Release()
	attempt.Update(nil, verdictTechnical(jsonvalue.Object{
		{Key: "state", Value: core.RequestBody(request.State, nil)},
		{Key: "selected_message", Value: stableID},
		{Key: "reason", Value: "single_reply"},
	}))
	attempt.Finish("applied", "Located the only assistant reply without a model request.")
}

func (c VerdictClient) locateQuestion(ctx context.Context, session string, request *core.Request) (index int, ok bool, err error) {
	count := len(request.State.Replies)
	body := core.RequestBody(request.State, nil)
	var diagnostic *core.Attempt
	if c.log != nil {
		attempt := c.log.Start(session, "activity",
			"Which reply contains the open question?",
			"Assistant replies since the latest user message.")
		diagnostic = &attempt
		defer attempt.Release()
		attempt.Update(nil, verdictTechnical(jsonvalue.Object{
			{Key: "request", Value: body},
			{Key: "contract", Value: "question-locator-glide-v1"},
		}))
	}
	index, ok, err = func() (int, bool, error) {
		endpoint, err := c.hostedGlideURL("/v1/question-locator-glide")
		if err != nil {
			return 0, false, err
		}
		// Fastino: "set your client read timeout above 170 seconds"
		// https://docs.fastino.ai/troubleshooting/retries (supersedes older
		// 300s notes).
		response, err := c.post(ctx, endpoint, 180*time.Second, body)
		if err != nil {
			return 0, false, fmt.Errorf("request question locator: %w", err)
		}
		defer response.Body.Close()
		status := response.StatusCode
		if status >= 200 && status < 300 {
			core.ObserveResponse(status, nil, nil)
		}
		retryAfter := verdictRetryAfter(response)
		payload, err := io.ReadAll(response.Body)
		if err != nil {
			return 0, false, fmt.Errorf("read question locator: %w", err)
		}
		if status == http.StatusTooManyRequests {
			core.ObserveResponse(status, retryAfter, payload)
		}
		if len(payload) > verdictResponseBound {
			return 0, false, errors.New("question locator response exceeds byte limit")
		}
		if status < 200 || status >= 300 {
			return 0, false, fmt.Errorf("question locator HTTP %s: %s", verdictStatusDisplay(status), fromUTF8Lossy(payload))
		}
		answer, err := jsonvalue.Parse(payload)
		if err != nil {
			return 0, false, fmt.Errorf("decode locator JSON: %w", err)
		}
		probabilities, _ := acpFeaturesGet(answer, "probabilities")
		index, ok, err := core.Parse(probabilities, count)
		if err != nil {
			return 0, false, err
		}
		if diagnostic != nil {
			var selected any
			if ok {
				selected = int64(index)
			}
			diagnostic.Update(nil, verdictTechnical(jsonvalue.Object{
				{Key: "answer", Value: answer},
				{Key: "selected_index", Value: selected},
			}))
		}
		return index, ok, nil
	}()
	if verdictDropped(ctx, err) {
		// Dropped mid-request: the released attempt records its cancellation.
		return 0, false, ctx.Err()
	}
	if diagnostic != nil {
		switch {
		case err == nil && ok:
			diagnostic.Finish("located", "GLiDE selected a reply for the current-assessment check.")
		case err == nil:
			diagnostic.Finish("unchanged", "No open request was located.")
		default:
			diagnostic.Update(nil, verdictTechnical(jsonvalue.Object{{Key: "error", Value: err.Error()}}))
			diagnostic.Finish("failed", "The question locator failed; no target was stored.")
		}
	}
	return index, ok, err
}

func verdictClientResolve(ctx context.Context, source VerdictSource) *VerdictClient {
	return verdictClientResolveBlocking(source)
}

func verdictClientResolveBlocking(source VerdictSource) *VerdictClient {
	if source == nil {
		if endpoint, ok := core.TestGlideEndpoint(); ok {
			source = VerdictSourceHosted{Endpoint: endpoint}
		}
	}
	return verdictClientResolveWith(source, core.DisabledByEnvironment())
}

func verdictClientResolveWith(source VerdictSource, jevDisabled bool) *VerdictClient {
	if source == nil && jevDisabled {
		slog.Info("Jev classifier disabled", "target", "mj_jev", "outcome", "disabled", "reason", "jev_switch_off")
		return nil
	}
	var key *string
	if source == nil {
		if value, ok := fastinoKey.Get(); ok {
			key = &value
		}
		source = VerdictSourceHosted{Endpoint: hostedProxy}
	}
	if _, disabled := source.(VerdictSourceDisabled); disabled {
		slog.Info("Jev classifier disabled", "target", "mj_jev", "outcome", "disabled", "reason", "explicit_disabled_source")
		return nil
	}
	client, err := verdictClientNewWithKey(source, key)
	if err != nil {
		slog.Warn("turn classifier unavailable", "target", "mj_jev", "error", err.Error())
		return nil
	}
	return client
}

func verdictClientNewWithKey(source VerdictSource, fastinoKey *string) (*VerdictClient, error) {
	hosted, ok := source.(VerdictSourceHosted)
	if !ok {
		return nil, errors.New("classification is disabled for this source")
	}
	headers := http.Header{}
	if fastinoKey != nil {
		value, err := core.FastinoHeader(*fastinoKey)
		if err != nil {
			return nil, err
		}
		headers.Set(core.FastinoKeyHeader, value)
	}
	client := &http.Client{
		// Each request sets its own timeout (see post).
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}
	return &VerdictClient{endpoint: hosted.Endpoint, client: client, headers: headers}, nil
}

func (c VerdictClient) withLog(log *core.DecisionLog) VerdictClient {
	c.log = log
	return c
}

func (c VerdictClient) newAttempt(diagnostic *core.Attempt, session string, generation uint64, phase core.TurnPhase) *VerdictAttempt {
	return &VerdictAttempt{
		diagnostic: diagnostic,
		id:         nextRequest.Add(1),
		session:    session,
		generation: generation,
		phase:      phase,
		started:    time.Now(),
		logger:     slog.Default(),
	}
}

// verdictElapsedError is tokio's Elapsed: the handback check ran out of its
// time. Callers tell a timeout from other failures with errors.Is(err,
// context.DeadlineExceeded), as Rust looks for Elapsed in the error chain.
type verdictElapsedError struct{}

func (verdictElapsedError) Error() string { return "deadline has elapsed" }

func (verdictElapsedError) Is(target error) bool { return target == context.DeadlineExceeded }

func (c VerdictClient) askHandbackLogged(ctx context.Context, session string, generation uint64, evidence *core.TurnEvidence) (*VerdictAttempt, core.HandbackProbabilities, error) {
	var diagnostic *core.Attempt
	if c.log != nil {
		attempt := c.log.Start(session, "activity",
			"Does this handback leave assigned work unfinished?",
			"Parent assignment and final report.")
		diagnostic = &attempt
	}
	body := core.RequestBody(evidence, nil)
	if diagnostic != nil {
		diagnostic.Update(nil, verdictTechnical(jsonvalue.Object{
			{Key: "request", Value: body},
			{Key: "contract", Value: "handback-glide-v1"},
			{Key: "model", Value: "fastino/GLiDE"},
			{Key: "handback", Value: true},
			{Key: "unfinished_share", Value: core.HandbackUnfinishedShare},
			{Key: "stop_probability_limit", Value: core.HandbackStopProbability},
		}))
	}
	attempt := c.newAttempt(diagnostic, session, generation, evidence.Phase)
	// User requires fail-open after a few seconds to save the parent's time.
	// Fresh 268-case GLiDE latency: p50 1.1s / p95 2.8s; 5s admits nearly all
	// normal calls.
	timeout := 5 * time.Second
	deadline, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	probabilities, err := func() (core.HandbackProbabilities, error) {
		endpoint, err := c.hostedGlideURL("/v1/handback-verdict")
		if err != nil {
			return core.HandbackProbabilities{}, err
		}
		if evidence.Authorization == nil {
			return core.HandbackProbabilities{}, errors.New("missing handback authorization")
		}
		response, err := c.postGlide(deadline, endpoint, *evidence.Authorization, true, timeout,
			func(history *core.ContextHistory) (any, error) {
				projected := *evidence
				authorization := *history
				projected.Authorization = &authorization
				return core.RequestBody(projected, nil), nil
			})
		if err != nil {
			return core.HandbackProbabilities{}, err
		}
		defer response.Body.Close()
		payload, err := io.ReadAll(response.Body)
		if err != nil {
			return core.HandbackProbabilities{}, err
		}
		answer, err := jsonvalue.Parse(payload)
		if err != nil {
			return core.HandbackProbabilities{}, err
		}
		distribution, _ := acpFeaturesGet(answer, "probabilities")
		return core.HandbackProbabilitiesParse(distribution)
	}()
	if err != nil && errors.Is(deadline.Err(), context.DeadlineExceeded) && ctx.Err() == nil {
		err = verdictElapsedError{}
	}
	if diagnostic != nil {
		if err == nil {
			diagnostic.Update(nil, verdictTechnical(jsonvalue.Object{{Key: "probabilities", Value: probabilities}}))
		} else {
			answer := "No usable GLiDE answer; deliver unchecked."
			diagnostic.Update(&answer, verdictTechnical(jsonvalue.Object{{Key: "error", Value: err.Error()}}))
		}
	}
	return attempt, probabilities, err
}

func (c VerdictClient) askGlideLogged(
	ctx context.Context,
	session string,
	generation uint64,
	evidence *core.TurnEvidence,
	facts *core.ActivityFacts,
	native int,
) (*VerdictAttempt, core.Probabilities, error) {
	var diagnostic *core.Attempt
	if c.log != nil {
		attempt := c.log.Start(session, "activity", "Assess the completed turn", "Authorization and runtime observations.")
		diagnostic = &attempt
	}
	attempt := c.newAttempt(diagnostic, session, generation, evidence.Phase)
	slog.Info("GLiDE classification requested", "target", "mj_jev", "session", session,
		"generation", generation, "summary_bytes", len(evidence.TranscriptSummary))
	answer, err := func() (core.Probabilities, error) {
		if evidence.Authorization == nil {
			return core.Probabilities{}, errors.New("missing completed-turn authorization")
		}
		history := *evidence.Authorization
		body, err := project(evidence, history, facts, native)
		if err != nil {
			return core.Probabilities{}, err
		}
		if diagnostic != nil {
			diagnostic.Update(nil, verdictTechnical(jsonvalue.Object{
				{Key: "request", Value: body},
				{Key: "contract", Value: "glide-turn-v1"},
			}))
		}
		answer, err := c.askGlideTurn(ctx, evidence, history, facts, native)
		if err != nil {
			return core.Probabilities{}, err
		}
		if diagnostic != nil {
			value, err := acpFeaturesValue(answer)
			if err != nil {
				return core.Probabilities{}, err
			}
			diagnostic.Update(nil, verdictTechnical(jsonvalue.Object{
				{Key: "probabilities", Value: value},
				{Key: "action", Value: string(core.Action(value))},
			}))
		}
		return answer, nil
	}()
	if err == nil {
		slog.Info("GLiDE classification received", "target", "mj_jev", "session", session,
			"generation", generation, "probabilities", fmt.Sprintf("%+v", answer))
	} else {
		slog.Warn("GLiDE classification failed", "target", "mj_jev", "session", session,
			"generation", generation, "error", err.Error())
	}
	return attempt, answer, err
}

func (c VerdictClient) postGlide(
	ctx context.Context,
	endpoint *url.URL,
	history core.ContextHistory,
	keepInstructions bool,
	timeout time.Duration,
	project func(history *core.ContextHistory) (any, error),
) (*http.Response, error) {
	projected, err := project(&history)
	if err != nil {
		return nil, err
	}
	body, err := jsonvalue.Marshal(projected)
	if err != nil {
		return nil, err
	}
	for {
		response, err := c.post(ctx, endpoint, timeout, body)
		if err != nil {
			return nil, fmt.Errorf("request GLiDE verdict: %w", err)
		}
		status := response.StatusCode
		if status >= 200 && status < 300 {
			core.ObserveResponse(status, nil, nil)
			return response, nil
		}
		retryAfter := verdictRetryAfter(response)
		payload, err := io.ReadAll(response.Body)
		response.Body.Close()
		if err != nil {
			return nil, fmt.Errorf("read GLiDE refusal: %w", err)
		}
		core.ObserveResponse(status, retryAfter, payload)
		if isGlideSizeRefusal(status, payload) {
			var smaller []byte
			for shrink(&history) {
				if keepInstructions && !history.AuthorizationComplete {
					return nil, fmt.Errorf("GLiDE refused the request size (%s); a smaller request would drop the parent's instructions",
						verdictStatusDisplay(status))
				}
				projected, err := project(&history)
				if err != nil {
					return nil, err
				}
				candidate, err := jsonvalue.Marshal(projected)
				if err != nil {
					return nil, err
				}
				if len(candidate) < len(body) {
					smaller = candidate
					break
				}
			}
			if smaller != nil {
				body = smaller
				continue
			}
		}
		return nil, fmt.Errorf("GLiDE verdict HTTP %s: %s", verdictStatusDisplay(status), fromUTF8Lossy(payload))
	}
}

func (c VerdictClient) askGlideTurn(
	ctx context.Context,
	evidence *core.TurnEvidence,
	authorization core.ContextHistory,
	facts *core.ActivityFacts,
	nativeAgentCount int,
) (core.Probabilities, error) {
	endpoint, err := c.hostedGlideURL("/v1/glide-turn-verdict")
	if err != nil {
		return core.Probabilities{}, err
	}
	// Fastino: "set your client read timeout above 170 seconds"
	// https://docs.fastino.ai/troubleshooting/retries (supersedes older 300s
	// notes).
	response, err := c.postGlide(ctx, endpoint, authorization, false, 180*time.Second,
		func(history *core.ContextHistory) (any, error) {
			return project(evidence, *history, facts, nativeAgentCount)
		})
	if err != nil {
		return core.Probabilities{}, err
	}
	defer response.Body.Close()
	payload, err := io.ReadAll(io.LimitReader(response.Body, verdictResponseBound+1))
	if err != nil {
		return core.Probabilities{}, fmt.Errorf("read GLiDE verdict: %w", err)
	}
	if len(payload) > verdictResponseBound {
		return core.Probabilities{}, errors.New("GLiDE response exceeds proxy response bound")
	}
	value, err := jsonvalue.Parse(payload)
	if err != nil {
		return core.Probabilities{}, fmt.Errorf("decode GLiDE verdict: %w", err)
	}
	probabilities, err := core.ProbabilitiesParse(value)
	if err != nil {
		return core.Probabilities{}, err
	}
	if probabilities.Attention == nil {
		slog.Warn("the verdict proxy returned no attention answer; Important attention stays off until the proxy is updated",
			"operation", "glide_turn_verdict")
	}
	return probabilities, nil
}
