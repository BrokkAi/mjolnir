// Port of mj-cli/src/api_client.rs.
//
// A typed client for the daemon's documented `/api/v1` routes.
//
// The CLI subcommands are thin: they resolve the viewer URL and the bearer
// token, serialize the request structs package server exports (Rust:
// `mj_daemon::server::api`), and print what comes back. Keeping the wire
// shapes in one package means the CLI and the server cannot disagree about
// them.
//
// reqwest becomes net/http. A reqwest RequestBuilder is an apiRequest here,
// so a refused request can be sent again as Rust's `try_clone` allows. Rust's
// generic `get_json`/`post_json`/`decode` are the package functions
// apiGetJSON, apiPostJSON and apiDecode, since Go methods take no type
// parameters. Async methods take a context.Context first: ending it
// abandons the request, as dropping the Rust future does.

package cli

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/netip"
	"net/url"
	"os"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/server"
	"github.com/BrokkAi/mjolnir/go/internal/sessionruntime"
)

// requestTimeout is how long an ordinary request may take. Every route but
// `wait` and the exports answers from memory or from SQLite, so this only
// has to outlast a busy daemon.
const requestTimeout = 120 * time.Second

// exportTimeout is how long an export may take. The daemon runs git on the
// target, which for a large checkout over SSH is slow; its own ceiling is
// five minutes.
const exportTimeout = 600 * time.Second

// reviewStartTimeout is how long starting a review may take. The daemon
// answers once the review has opened: it waits for a recovery copy or worker
// upgrade holding the session, then chooses and starts a reviewer, which an
// Auto choice may try on several profiles.
const reviewStartTimeout = 600 * time.Second

// waitSlack is how much longer than the wait itself the HTTP request may
// take, so a wait that reaches its deadline answers `timeout` rather than
// failing the client.
const waitSlack = 30 * time.Second

// busyRetry is how a request the daemon refused with 429 (all of its
// concurrent action slots are taken) is retried. A 429 is answered before
// the daemon admits the action, so sending it again cannot repeat any work.
type busyRetry struct {
	firstDelay time.Duration
	maxDelay   time.Duration
	// limit is how long to keep retrying before reporting the refusal.
	limit time.Duration
}

// defaultBusyRetry is Rust's `BusyRetry::DEFAULT`. Session creation holds a
// slot until provisioning ends, which over SSH takes minutes, so the client
// waits about as long as one provision.
var defaultBusyRetry = busyRetry{
	firstDelay: 2 * time.Second,
	maxDelay:   15 * time.Second,
	limit:      600 * time.Second,
}

// apiClient is a client for one daemon's API (Rust `ApiClient`).
//
// The fields are named apart from the accessor methods Rust gives the same
// names (base_url, daemon_instance, daemon_on_client_filesystem).
type apiClient struct {
	base               string
	instance           *string
	onClientFilesystem bool
	token              string
	busyRetry          busyRetry
	// waitNotice is where the one "waiting for the daemon" line goes;
	// stderr outside tests.
	waitNotice func(line string)
	http       *http.Client
}

// apiRequest is one request to send, as a reqwest RequestBuilder holds it.
// A request with a single-use stream body cannot be sent twice, as
// `try_clone` refuses a streaming body.
type apiRequest struct {
	method string
	url    string
	query  url.Values
	header http.Header
	// body is a replayable body; stream a single-use one.
	body          []byte
	stream        io.Reader
	contentLength *uint64
	// timeout is the total timeout, or 0 for none.
	timeout time.Duration
}

// tryClone is reqwest's `try_clone`: a copy to send again, or false for a
// streaming body.
func (r *apiRequest) tryClone() (*apiRequest, bool) {
	panic("unported: mj-cli/src/api_client.rs RequestBuilder::try_clone")
}

// apiClientConnect resolves the daemon's viewer URL and the bearer token,
// starting the daemon if it is not running (Rust `ApiClient::connect`).
//
// Every step here is a read. When an automatic upgrade replaces the daemon
// between them, the steps are asked of the new daemon: a client that
// started during a handoff then works instead of failing on the old
// daemon's closed connection.
func apiClientConnect(ctx context.Context) (*apiClient, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::connect")
}

// apiClientConnectRunning connects to the daemon only if one is already
// running. Read-only diagnostics use this so they never start or replace a
// daemon.
func apiClientConnectRunning(ctx context.Context) (*apiClient, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::connect_running")
}

func apiClientConnectTo(ctx context.Context, daemon *client.DaemonClient) (*apiClient, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::connect_to")
}

// newAPIClient is the test constructor (Rust `#[cfg(test)] ApiClient::new`).
func newAPIClient(baseURL, token string) (*apiClient, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::new")
}

func apiClientWithHTTP(baseURL, token string, httpClient *http.Client) *apiClient {
	return &apiClient{
		base:       strings.TrimRight(baseURL, "/"),
		token:      token,
		busyRetry:  defaultBusyRetry,
		waitNotice: func(line string) { fmt.Fprintln(os.Stderr, line) },
		http:       httpClient,
	}
}

func (c *apiClient) daemonInstance() (string, bool) {
	if c.instance == nil {
		return "", false
	}
	return *c.instance, true
}

func (c *apiClient) daemonOnClientFilesystem() bool {
	return c.onClientFilesystem
}

func (c *apiClient) launchOptions(ctx context.Context) (server.LaunchOptions, error) {
	return apiGetJSON[server.LaunchOptions](ctx, c, "/options")
}

func (c *apiClient) imageStart(ctx context.Context, request *client.ImageRequest, status bool) (client.ImageStarted, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::image_start")
}

// imageFollow is nil when the daemon knows no such operation.
func (c *apiClient) imageFollow(ctx context.Context, id string, offset int) (*client.ImageFollow, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::image_follow")
}

func (c *apiClient) imageCancel(ctx context.Context, id string) error {
	panic("unported: mj-cli/src/api_client.rs ApiClient::image_cancel")
}

func (c *apiClient) baseURL() string {
	return c.base
}

func (c *apiClient) url(path string) string {
	return c.base + "/api/v1" + path
}

func (c *apiClient) send(ctx context.Context, request *apiRequest) (*http.Response, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::send")
}

// sendOptional sends a request whose subject may simply not exist: a 404 is
// a nil response rather than an error, and every other refusal is reported
// as send reports it.
func (c *apiClient) sendOptional(ctx context.Context, request *apiRequest) (*http.Response, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::send_optional")
}

// dispatch sends one request, separating "the API refused it" (a non-nil
// *apiError) from "the API could not be reached or does not speak this
// contract" (the error). Only the refusal is something a caller may
// interpret.
//
// A 429 means the daemon is already running as many actions as it allows
// and did not admit this one. The request is sent again with growing delays
// for up to busyRetry.limit, and the first wait is reported once on stderr
// so the person sees what the command is waiting for.
func (c *apiClient) dispatch(ctx context.Context, request *apiRequest) (*http.Response, *apiError, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::dispatch")
}

func (c *apiClient) dispatchOnce(ctx context.Context, request *apiRequest) (*http.Response, *apiError, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::dispatch_once")
}

// apiGetJSON is Rust's `ApiClient::get_json`.
func apiGetJSON[T any](ctx context.Context, c *apiClient, path string) (T, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::get_json")
}

func (c *apiClient) githubToken(ctx context.Context, owner *string, repositories []string) (string, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::github_token")
}

// apiPostJSON is Rust's `ApiClient::post_json`.
func apiPostJSON[T any](ctx context.Context, c *apiClient, path string, body any, timeout time.Duration) (T, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::post_json")
}

// requestUI asks the attached dashboards to change what they show (`mj ui`).
func (c *apiClient) requestUI(ctx context.Context, body *client.UiRequestBody) (client.UiRequestReceipt, error) {
	return apiPostJSON[client.UiRequestReceipt](ctx, c, "/ui/requests", body, requestTimeout)
}

func (c *apiClient) workspaces(ctx context.Context) (server.WorkspaceListResponse, error) {
	return apiGetJSON[server.WorkspaceListResponse](ctx, c, "/workspaces")
}

// createWorkspace creates the named workspace, or gets the one that already
// carries the name. The route is idempotent, so a script may call it every
// run.
func (c *apiClient) createWorkspace(ctx context.Context, name string) (server.CreateWorkspaceResponse, error) {
	return apiPostJSON[server.CreateWorkspaceResponse](ctx, c, "/workspaces", &server.CreateWorkspaceRequest{Name: name}, requestTimeout)
}

func (c *apiClient) sessionsInWorkspace(ctx context.Context, workspaceID *string, all bool) (server.SessionListResponse, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::sessions_in_workspace")
}

func (c *apiClient) message(ctx context.Context, sessionID string, request *server.SessionMessageRequest) (server.SessionMessageResponse, error) {
	return apiPostJSON[server.SessionMessageResponse](ctx, c, "/sessions/"+sessionID+"/message", request, requestTimeout)
}

func (c *apiClient) models(ctx context.Context, profile string, model *string, unrestricted, withDisabled bool) (core.ProfileConfig, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::models")
}

func (c *apiClient) profileLoginErrors(ctx context.Context) (map[string]string, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::profile_login_errors")
}

func (c *apiClient) setConfig(ctx context.Context, session string, request *server.SetConfigRequest) (server.ApiSession, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::set_config")
}

// sessionIfKnown is one session, or nil when the daemon knows no session
// with that id. An id that names nothing here may still name a SessionWiki
// row.
func (c *apiClient) sessionIfKnown(ctx context.Context, sessionID string) (*server.ApiSession, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::session_if_known")
}

// wikiSession is what the SessionWiki index knows about one row, or nil
// when the index holds no session with that id.
func (c *apiClient) wikiSession(ctx context.Context, wikiID string) (*client.WikiSessionInfo, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::wiki_session")
}

// wikiRestore starts a new session carrying a hand-off compacted from an
// indexed one.
func (c *apiClient) wikiRestore(ctx context.Context, wikiID string, request *server.WikiRestoreBody) (server.StartSessionResponse, error) {
	return apiPostJSON[server.StartSessionResponse](ctx, c, "/wiki/sessions/"+wikiID+"/restore", request, 660*time.Second)
}

func (c *apiClient) start(ctx context.Context, request *server.StartSessionRequest) (server.StartSessionResponse, error) {
	return apiPostJSON[server.StartSessionResponse](ctx, c, "/sessions", request, 660*time.Second)
}

func (c *apiClient) prompt(ctx context.Context, sessionID, text string) (server.PromptResponse, error) {
	return c.promptWithID(ctx, sessionID, text, nil)
}

func (c *apiClient) promptWithID(ctx context.Context, sessionID, text string, commandID *string) (server.PromptResponse, error) {
	return apiPostJSON[server.PromptResponse](ctx, c, "/sessions/"+sessionID+"/prompt", &server.PromptRequest{Text: text, CommandID: commandID}, requestTimeout)
}

func (c *apiClient) clearQueue(ctx context.Context, sessionID string, commandID *string) (server.PromptResponse, error) {
	return apiPostJSON[server.PromptResponse](ctx, c, "/sessions/"+sessionID+"/queued-prompts/clear", &server.ClearQueueRequest{CommandID: commandID}, requestTimeout)
}

func (c *apiClient) enqueueEvent(ctx context.Context, sessionID string, request *server.MailboxEventRequest) (server.MailboxEventResponse, error) {
	return apiPostJSON[server.MailboxEventResponse](ctx, c, "/sessions/"+sessionID+"/events", request, requestTimeout)
}

// wait blocks until a turn ends. The HTTP timeout outlasts the wait itself,
// so a deadline is reported by the server as a `timeout` outcome rather
// than by the client as a dropped request.
func (c *apiClient) wait(ctx context.Context, sessionID string, request *sessionruntime.WaitRequest) (server.WaitResponse, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::wait")
}

func (c *apiClient) transcript(
	ctx context.Context,
	sessionID string,
	afterSeq *uint64,
	limit *int,
	roles []core.TranscriptRole,
	finishedOnly bool,
) (server.TranscriptResponse, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::transcript")
}

// events opens the server-sent event stream of `GET /events`. The caller
// reads and closes the response body.
func (c *apiClient) events(ctx context.Context, filter *core.ApiEventFilter, afterSeq *uint64) (*http.Response, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::events")
}

func (c *apiClient) usage(ctx context.Context, sessionID string, afterSeq *uint64, limit *int) (core.UsagePage, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::usage")
}

func (c *apiClient) usageTree(ctx context.Context, parent string) (core.UsageTree, error) {
	return apiGetJSON[core.UsageTree](ctx, c, "/sessions/"+parent+"/usage/tree")
}

func (c *apiClient) usageReport(ctx context.Context, query *core.SessionUsageQuery) (core.SessionUsageReport, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::usage_report")
}

func (c *apiClient) diff(ctx context.Context, sessionID string, base *string, asJSON bool) (string, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::diff")
}

// putFile uploads body, a single-use stream, to path in the session's
// workspace.
func (c *apiClient) putFile(
	ctx context.Context,
	sessionID, path string,
	body io.Reader,
	contentLength *uint64,
	overwrite bool,
) (server.WriteFileResponse, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::put_file")
}

func (c *apiClient) elicitations(ctx context.Context, sessionID string) ([]core.ElicitationRequest, error) {
	return apiGetJSON[[]core.ElicitationRequest](ctx, c, "/sessions/"+sessionID+"/elicitations")
}

func (c *apiClient) respondElicitation(ctx context.Context, sessionID, elicitationID string, response *core.ElicitationResponse) error {
	panic("unported: mj-cli/src/api_client.rs ApiClient::respond_elicitation")
}

// export exports the session's work. A branch push answers JSON; a patch and
// a bundle answer bytes, so the caller gets both and uses the one its kind
// produced.
func (c *apiClient) export(ctx context.Context, sessionID string, request *server.ExportRequest) (exportResult, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::export")
}

// apiClientSuspendPreflightQuery is Rust's
// `ApiClient::suspend_preflight_query`.
func apiClientSuspendPreflightQuery(dryRun, withoutCheckpoint bool) string {
	switch {
	case dryRun && withoutCheckpoint:
		return "?dry_run=true&without_checkpoint=true"
	case dryRun:
		return "?dry_run=true"
	case withoutCheckpoint:
		return "?without_checkpoint=true"
	}
	return ""
}

func (c *apiClient) suspendPreflight(ctx context.Context, sessionID string, dryRun, withoutCheckpoint bool) (core.SuspendPreflight, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::suspend_preflight")
}

// suspend asks the daemon to suspend a session. The answer says how many
// sub-agents the suspend stops, and warns when some of them have not handed
// back.
func (c *apiClient) suspend(ctx context.Context, sessionID string, consent *core.SuspendConsent) (server.SuspendSessionResponse, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::suspend")
}

// purgeUntracked has no total or read timeout. Ctrl-C ends this process,
// which closes the connection, and the daemon stops the batch on that
// disconnect.
func (c *apiClient) purgeUntracked(ctx context.Context, request *server.PurgeUntrackedRequest) (server.PurgeUntrackedResponse, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::purge_untracked")
}

func (c *apiClient) destroy(ctx context.Context, sessionID string) error {
	panic("unported: mj-cli/src/api_client.rs ApiClient::destroy")
}

func (c *apiClient) notices(ctx context.Context, sessionID string) ([]client.RuntimeNotice, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::notices")
}

// resume asks the daemon to resume a stopped session. It answers as soon as
// the resume is admitted, so this does not wait for the session to come up.
func (c *apiClient) resume(ctx context.Context, sessionID string, request *server.ResumeSessionRequest) (server.ResumeSessionResponse, error) {
	return apiPostJSON[server.ResumeSessionResponse](ctx, c, "/sessions/"+sessionID+"/resume", request, requestTimeout)
}

func (c *apiClient) interruptTurn(ctx context.Context, sessionID string) error {
	panic("unported: mj-cli/src/api_client.rs ApiClient::interrupt_turn")
}

// restartSession answers the daemon's JSON (a serde_json::Value), or
// `{"session_id": ..., "restarted": true}` when it answered nothing
// readable.
func (c *apiClient) restartSession(ctx context.Context, sessionID string) (any, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::restart_session")
}

// startReview starts a one-off review of the turn the session just
// finished. The daemon answers once the review has opened, which includes
// choosing and starting a reviewer, so this may take minutes.
func (c *apiClient) startReview(ctx context.Context, sessionID string) (server.StartReviewResponse, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::start_review")
}

// reviewStatus is the review the session has open, if any.
func (c *apiClient) reviewStatus(ctx context.Context, sessionID string) (server.ReviewStatusResponse, error) {
	return apiGetJSON[server.ReviewStatusResponse](ctx, c, "/sessions/"+sessionID+"/review")
}

// finishedReview is a finished review's per-reviewer output, or the latest
// when reviewID is nil.
func (c *apiClient) finishedReview(ctx context.Context, sessionID string, reviewID *string) (server.FinishedReviewResponse, error) {
	panic("unported: mj-cli/src/api_client.rs ApiClient::finished_review")
}

func (c *apiClient) stopBackgroundTask(ctx context.Context, sessionID, taskID string) error {
	panic("unported: mj-cli/src/api_client.rs ApiClient::stop_background_task")
}

// viewerResolver sends connections for host to address while keeping host
// for HTTP Host and TLS SNI (reqwest's `ClientBuilder::resolve`).
type viewerResolver struct {
	host    string
	address netip.AddrPort
}

// httpClient is the HTTP client for the daemon API. With a pin it trusts
// only the certificate the daemon published; without one (plain HTTP, or a
// publicly trusted Tailscale certificate) it verifies as any HTTPS client
// does.
func httpClient(certificateSha256 *string, resolver *viewerResolver) (*http.Client, error) {
	return buildHTTPClient(certificateSha256, resolver, nil)
}

// buildHTTPClient builds the API client: user agent `mj/<version>`, no
// environment proxy unless proxy is given, no redirects, the resolver and
// the certificate pin.
func buildHTTPClient(certificateSha256 *string, resolver *viewerResolver, proxy func(*http.Request) (*url.URL, error)) (*http.Client, error) {
	panic("unported: mj-cli/src/api_client.rs build_http_client")
}

// routeViewerToLocalSocket preserves a DNS authority for HTTP Host and TLS
// SNI while sending the connection to the daemon-published local listener.
func routeViewerToLocalSocket(viewerURL string, localAddress netip.AddrPort) (string, *viewerResolver, error) {
	panic("unported: mj-cli/src/api_client.rs route_viewer_to_local_socket")
}

// probeAPI proves the route exists before a token is read: an
// unauthenticated versioned 401 is enough.
func probeAPI(ctx context.Context, httpClient *http.Client, baseURL string) error {
	panic("unported: mj-cli/src/api_client.rs probe_api")
}

// actionSlots is how full the daemon says its action pool is.
type actionSlots struct {
	running int
	limit   int
}

// apiError is a request the API refused, kept apart from a transport
// failure so a caller can tell "there is no such thing" from "the call did
// not work".
type apiError struct {
	status  int
	message string
	// busy is set when the daemon says how full its action pool is.
	busy *actionSlots
	// code is the daemon's name for the reason, when it gave one. The
	// message stays free of instructions for one surface; this client adds
	// its own.
	code *string
}

// daemonHandoff is the error of a daemon answering that an automatic
// upgrade is replacing it (Rust `DaemonHandoff`). The request was refused
// before it was accepted, or was a read the handoff ended, so a caller that
// can follow the daemon asks the next one.
type daemonHandoff struct{}

func (daemonHandoff) Error() string {
	return "the Mjolnir daemon is being replaced by an upgrade"
}

// isDaemonHandoff is whether err is, or wraps, a daemonHandoff.
func isDaemonHandoff(err error) bool {
	var handoff daemonHandoff
	return errors.As(err, &handoff)
}

// isHandoffRefusal is whether the daemon refused response because an
// upgrade is replacing it.
func isHandoffRefusal(response *http.Response) bool {
	return response.StatusCode == http.StatusServiceUnavailable &&
		response.Header.Get(server.UpgradeHeader) == "pending"
}

// handoffFollowLimit is how long a client follows one daemon replacement
// before it reports it. A daemon that closed admission exits in seconds;
// this only bounds a fault.
const handoffFollowLimit = 300 * time.Second

// apiClientConnectAfterHandoff connects to the daemon that replaces one
// being upgraded. Until the old daemon has exited, connecting can still
// reach it; apiClientConnect asks again until the new one answers.
func apiClientConnectAfterHandoff(ctx context.Context) (*apiClient, error) {
	timer := time.NewTimer(250 * time.Millisecond)
	defer timer.Stop()
	select {
	case <-timer.C:
	case <-ctx.Done():
		return nil, ctx.Err()
	}
	return apiClientConnect(ctx)
}

// intoError is the error a refusal reports. A handoff is a daemonHandoff;
// a sub-agent choice the daemon cannot make names where to configure it.
func (e *apiError) intoError() error {
	panic("unported: mj-cli/src/api_client.rs ApiError::into_error")
}

// statusDisplay is reqwest's StatusCode Display: the code and the http
// crate's canonical reason, or `<unknown status code>`. Some of the http
// crate's reasons differ from net/http's StatusText (413 "Payload Too
// Large", 414 "URI Too Long", 416 "Range Not Satisfiable").
func statusDisplay(status int) string {
	panic("unported: mj-cli/src/api_client.rs StatusCode Display")
}

// waitingNotice is the one line printed when a command starts waiting for
// the daemon to admit it. A daemon that does not report its counts gets the
// same line without them.
func waitingNotice(busy *actionSlots) string {
	if busy != nil {
		return fmt.Sprintf("Waiting for the daemon: %d of %d actions are in use (sessions still starting). Retrying until one frees up.", busy.running, busy.limit)
	}
	return "Waiting for the daemon: all of its actions are in use (sessions still starting). Retrying until one frees up."
}

// exportResult is what an export answered with: exportResultBranch or
// exportResultBytes.
type exportResult interface{ isExportResult() }

type exportResultBranch struct{ branch server.PushedBranch }

type exportResultBytes struct{ bytes []byte }

func (exportResultBranch) isExportResult() {}
func (exportResultBytes) isExportResult()  {}

// checkVersion refuses a response that does not name this contract, or
// names another major version of it. A client that parsed such a body would
// be guessing.
func checkVersion(response *http.Response) error {
	values := response.Header[http.CanonicalHeaderKey(server.APIVersionHeader)]
	// HeaderValue::to_str accepts only visible ASCII and tab.
	visible := len(values) > 0 && !strings.ContainsFunc(values[0], func(r rune) bool {
		return r != '\t' && (r < 0x20 || r > 0x7e)
	})
	if !visible {
		return fmt.Errorf("the server at this URL did not answer with %s; it is not a Mjolnir API", server.APIVersionHeader)
	}
	version := strings.TrimSpace(values[0])
	major, _, _ := strings.Cut(version, ".")
	if major != server.APIVersion {
		return fmt.Errorf("this daemon speaks Mjolnir API version %s; this `mj` speaks %s. Upgrade whichever is older.", version, server.APIVersion)
	}
	return nil
}

// apiDecode reads response's body as T, naming the first 200 characters of
// a body that does not decode.
func apiDecode[T any](response *http.Response) (T, error) {
	panic("unported: mj-cli/src/api_client.rs decode")
}
