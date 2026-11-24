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
	"bytes"
	"context"
	"crypto/tls"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/netip"
	"net/url"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

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
	// query is the pairs reqwest's `query` appends to the URL, in order.
	query  [][2]string
	header http.Header
	// body is a replayable body; stream a single-use one.
	body          []byte
	stream        io.Reader
	contentLength *uint64
	// timeout is the total timeout, or 0 for none.
	timeout time.Duration
}

// newAPIRequest is `self.http.<method>(url)`.
func newAPIRequest(method, url string) *apiRequest {
	return &apiRequest{method: method, url: url, header: http.Header{}}
}

// withQuery is reqwest's `query`: the pairs are appended in order.
func (r *apiRequest) withQuery(pairs ...[2]string) *apiRequest {
	r.query = append(r.query, pairs...)
	return r
}

// withTimeout is reqwest's `timeout`: the whole request, body included.
func (r *apiRequest) withTimeout(timeout time.Duration) *apiRequest {
	r.timeout = timeout
	return r
}

// withJSON is reqwest's `json`: the body serde_json writes, as
// `application/json`.
func (r *apiRequest) withJSON(body any) (*apiRequest, error) {
	encoded, err := core.SerdeJSON(body)
	if err != nil {
		return nil, err
	}
	r.body = encoded
	r.header.Set("Content-Type", "application/json")
	return r, nil
}

// tryClone is reqwest's `try_clone`: a copy to send again, or false for a
// streaming body.
func (r *apiRequest) tryClone() (*apiRequest, bool) {
	if r.stream != nil {
		return nil, false
	}
	clone := *r
	clone.query = append([][2]string(nil), r.query...)
	clone.header = r.header.Clone()
	if r.body != nil {
		clone.body = append([]byte(nil), r.body...)
	}
	if r.contentLength != nil {
		length := *r.contentLength
		clone.contentLength = &length
	}
	return &clone, true
}

// fullURL is the URL with the query appended as reqwest appends it: pairs
// form-urlencoded after any query the URL already has, and no `?` for an
// empty query.
func (r *apiRequest) fullURL() string {
	if len(r.query) == 0 {
		return r.url
	}
	encoded := make([]string, len(r.query))
	for index, pair := range r.query {
		encoded[index] = formURLEncode(pair[0]) + "=" + formURLEncode(pair[1])
	}
	separator := "?"
	if strings.Contains(r.url, "?") {
		separator = "&"
		if strings.HasSuffix(r.url, "?") || strings.HasSuffix(r.url, "&") {
			separator = ""
		}
	}
	return r.url + separator + strings.Join(encoded, "&")
}

// formURLEncode is form_urlencoded::byte_serialize, as reqwest's `query`
// writes each name and value.
func formURLEncode(text string) string {
	var encoded strings.Builder
	for index := 0; index < len(text); index++ {
		switch b := text[index]; {
		case 'a' <= b && b <= 'z', 'A' <= b && b <= 'Z', '0' <= b && b <= '9', b == '*', b == '-', b == '.', b == '_':
			encoded.WriteByte(b)
		case b == ' ':
			encoded.WriteByte('+')
		default:
			fmt.Fprintf(&encoded, "%%%02X", b)
		}
	}
	return encoded.String()
}

// apiClientConnect resolves the daemon's viewer URL and the bearer token,
// starting the daemon if it is not running (Rust `ApiClient::connect`).
//
// Every step here is a read. When an automatic upgrade replaces the daemon
// between them, the steps are asked of the new daemon: a client that
// started during a handoff then works instead of failing on the old
// daemon's closed connection.
func apiClientConnect(ctx context.Context) (*apiClient, error) {
	deadline := time.Now().Add(handoffFollowLimit)
	replaced := 0
	for {
		started := time.Now()
		daemon, err := ConnectOrStart(ctx)
		if err != nil {
			return nil, err
		}
		identity, err := daemon.Identity(ctx)
		if err != nil {
			daemon.Close()
			return nil, err
		}
		instanceID := identity.InstanceID
		daemonReady := time.Since(started)
		connected, err := apiClientConnectTo(ctx, daemon)
		daemon.Close()
		if err == nil {
			// A slow command after a handoff has to say whether it waited
			// for the daemon or for its web API.
			total := time.Since(started)
			if total >= 250*time.Millisecond {
				slog.Info("connected to the daemon's API",
					"instance_id", instanceID,
					"daemon_ms", daemonReady.Milliseconds(),
					"api_ms", (total - daemonReady).Milliseconds())
			}
			return connected, nil
		}
		// A daemon handing off refuses until it exits; one that went away
		// under this client was replaced, or failed, in which case the
		// retries are few.
		var follow bool
		if isDaemonHandoff(err) {
			follow = time.Now().Before(deadline)
		} else {
			replaced++
			follow = replaced <= 3 && daemonWasReplaced(ctx, instanceID)
		}
		if !follow {
			return nil, err
		}
		slog.Info("the daemon was replaced while this client connected; connecting to its replacement",
			"error", err, "instance_id", instanceID)
		if err := sleepContext(ctx, 250*time.Millisecond); err != nil {
			return nil, err
		}
	}
}

// sleepContext waits for duration, or for ctx to end (tokio's sleep in a
// future the caller may drop).
func sleepContext(ctx context.Context, duration time.Duration) error {
	timer := time.NewTimer(duration)
	defer timer.Stop()
	select {
	case <-timer.C:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

// apiClientConnectRunning connects to the daemon only if one is already
// running. Read-only diagnostics use this so they never start or replace a
// daemon.
func apiClientConnectRunning(ctx context.Context) (*apiClient, error) {
	daemon, err := client.ConnectExisting(ctx)
	if err != nil {
		return nil, err
	}
	defer daemon.Close()
	return apiClientConnectTo(ctx, daemon)
}

func apiClientConnectTo(ctx context.Context, daemon *client.DaemonClient) (*apiClient, error) {
	access, err := WaitForWebAccess(ctx, daemon)
	if err != nil {
		return nil, err
	}
	ready, ok := access.(client.WebViewerAccessReady)
	if !ok {
		panic("wait_for_web_access returns only Ready")
	}
	viewerURL := ready.ViewerURL
	var resolver *viewerResolver
	if connector := daemon.Connector(); connector != nil && connector.IsSSH() {
		if err := connector.ValidateViewerURL(ctx, viewerURL); err != nil {
			return nil, err
		}
	} else {
		if ready.LocalAddress == nil {
			return nil, errors.New("the daemon did not publish a local web viewer address; restart the daemon")
		}
		viewerURL, resolver, err = routeViewerToLocalSocket(ready.ViewerURL, *ready.LocalAddress)
		if err != nil {
			return nil, err
		}
	}
	http, err := httpClient(ready.CertificateSha256, resolver)
	if err != nil {
		return nil, err
	}
	if err := probeAPI(ctx, http, viewerURL); err != nil {
		return nil, err
	}
	token, err := daemon.APICredential(ctx)
	if err != nil {
		return nil, err
	}
	api := apiClientWithHTTP(viewerURL, token, http)
	identity, err := daemon.Identity(ctx)
	if err != nil {
		return nil, err
	}
	api.onClientFilesystem = daemon.SharesClientFilesystem(identity)
	instance := identity.InstanceID
	api.instance = &instance
	return api, nil
}

// newAPIClient is the test constructor (Rust `#[cfg(test)] ApiClient::new`).
func newAPIClient(baseURL, token string) (*apiClient, error) {
	http, err := httpClient(nil, nil)
	if err != nil {
		return nil, err
	}
	return apiClientWithHTTP(baseURL, token, http), nil
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
	path := "/images/build"
	if status {
		path = "/images/status"
	}
	return apiPostJSON[client.ImageStarted](ctx, c, path, request, requestTimeout)
}

// imageFollow is nil when the daemon knows no such operation.
func (c *apiClient) imageFollow(ctx context.Context, id string, offset int) (*client.ImageFollow, error) {
	request := newAPIRequest(http.MethodGet, c.url("/images/operations/"+id)).
		withQuery([2]string{"offset", strconv.Itoa(offset)})
	response, err := c.sendOptional(ctx, request)
	if err != nil || response == nil {
		return nil, err
	}
	follow, err := apiDecode[client.ImageFollow](response)
	if err != nil {
		return nil, err
	}
	return &follow, nil
}

func (c *apiClient) imageCancel(ctx context.Context, id string) error {
	response, err := c.send(ctx, newAPIRequest(http.MethodDelete, c.url("/images/operations/"+id)))
	if err != nil {
		return err
	}
	return response.Body.Close()
}

func (c *apiClient) baseURL() string {
	return c.base
}

func (c *apiClient) url(path string) string {
	return c.base + "/api/v1" + path
}

func (c *apiClient) send(ctx context.Context, request *apiRequest) (*http.Response, error) {
	response, failure, err := c.dispatch(ctx, request)
	if err != nil {
		return nil, err
	}
	if failure != nil {
		return nil, failure.intoError()
	}
	return response, nil
}

// sendOptional sends a request whose subject may simply not exist: a 404 is
// a nil response rather than an error, and every other refusal is reported
// as send reports it.
func (c *apiClient) sendOptional(ctx context.Context, request *apiRequest) (*http.Response, error) {
	response, failure, err := c.dispatch(ctx, request)
	if err != nil {
		return nil, err
	}
	if failure != nil {
		if failure.status == http.StatusNotFound {
			return nil, nil
		}
		return nil, failure.intoError()
	}
	return response, nil
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
	started := time.Now()
	delay := c.busyRetry.firstDelay
	announced := false
	for {
		retry, replayable := request.tryClone()
		response, failure, err := c.dispatchOnce(ctx, request)
		if err != nil {
			return nil, nil, err
		}
		if failure == nil || failure.status != http.StatusTooManyRequests {
			return response, failure, nil
		}
		if !replayable {
			return nil, failure, nil
		}
		if time.Since(started)+delay > c.busyRetry.limit {
			return nil, failure, nil
		}
		if !announced {
			announced = true
			c.waitNotice(waitingNotice(failure.busy))
			slog.Info("the daemon's action slots are full; retrying", "busy", failure.busy.debug())
		}
		if err := sleepContext(ctx, delay); err != nil {
			return nil, nil, err
		}
		delay = min(delay*2, c.busyRetry.maxDelay)
		request = retry
	}
}

func (c *apiClient) dispatchOnce(ctx context.Context, request *apiRequest) (*http.Response, *apiError, error) {
	response, err := c.do(ctx, request, true)
	if err != nil {
		return nil, nil, fmt.Errorf("reach the Mjolnir API: %w", err)
	}
	status := response.StatusCode
	// Checked before the version: the admission layer refuses a request
	// during a handoff before the API's own layers would mark it.
	if isHandoffRefusal(response) {
		response.Body.Close()
		code := server.DaemonHandoffCode
		return nil, &apiError{
			status:  status,
			message: "the Mjolnir daemon is being replaced by an upgrade",
			code:    &code,
		}, nil
	}
	if err := checkVersion(response); err != nil {
		response.Body.Close()
		return nil, nil, err
	}
	if status >= 200 && status < 300 {
		return response, nil, nil
	}
	// An unreadable body reads as empty, as Rust's unwrap_or_default.
	body, _ := io.ReadAll(response.Body)
	response.Body.Close()
	parsed, parseErr := jsonvalue.Parse(body)
	object, isObject := parsed.(jsonvalue.Object)
	isObject = isObject && parseErr == nil
	message := strings.TrimSpace(core.LossyUTF8(body))
	if isObject {
		if value, ok := object.Get("error"); ok {
			if text, ok := value.(string); ok {
				message = text
			}
		}
	}
	var busy *actionSlots
	if isObject {
		running, runningOK := jsonUint(object, "running_actions")
		limit, limitOK := jsonUint(object, "action_limit")
		if runningOK && limitOK {
			busy = &actionSlots{running: int(running), limit: int(limit)}
		}
	}
	var code *string
	if isObject {
		if value, ok := object.Get("code"); ok {
			if text, ok := value.(string); ok {
				code = &text
			}
		}
	}
	return nil, &apiError{status: status, message: message, busy: busy, code: code}, nil
}

// jsonUint is serde_json's `get(key)?.as_u64()?.try_into().ok()` for a
// usize: a non-negative integer member.
func jsonUint(object jsonvalue.Object, key string) (uint64, bool) {
	value, ok := object.Get(key)
	if !ok {
		return 0, false
	}
	switch number := value.(type) {
	case int64:
		if number >= 0 {
			return uint64(number), true
		}
	case uint64:
		return number, true
	}
	return 0, false
}

// do sends request over c.http, with the bearer token when authenticated.
// A request timeout covers reading the body too, as reqwest's does: it ends
// when the caller closes the body.
func (c *apiClient) do(ctx context.Context, request *apiRequest, authenticated bool) (*http.Response, error) {
	cancel := context.CancelFunc(func() {})
	if request.timeout > 0 {
		ctx, cancel = context.WithTimeout(ctx, request.timeout)
	}
	var body io.Reader
	switch {
	case request.stream != nil:
		body = request.stream
	case request.body != nil:
		body = bytes.NewReader(request.body)
	}
	built, err := http.NewRequestWithContext(ctx, request.method, request.fullURL(), body)
	if err != nil {
		cancel()
		return nil, err
	}
	for name, values := range request.header {
		built.Header[name] = append([]string(nil), values...)
	}
	if request.contentLength != nil {
		built.ContentLength = int64(*request.contentLength)
	} else if request.stream != nil {
		// A stream of unknown length goes chunked.
		built.ContentLength = -1
	}
	if authenticated {
		built.Header.Set("Authorization", "Bearer "+c.token)
	}
	response, err := c.http.Do(built)
	if err != nil {
		cancel()
		return nil, err
	}
	response.Body = &cancelOnClose{ReadCloser: response.Body, cancel: cancel}
	return response, nil
}

// cancelOnClose ends a request's timeout once its body is closed.
type cancelOnClose struct {
	io.ReadCloser
	cancel context.CancelFunc
}

func (b *cancelOnClose) Close() error {
	err := b.ReadCloser.Close()
	b.cancel()
	return err
}

// apiGetJSON is Rust's `ApiClient::get_json`.
func apiGetJSON[T any](ctx context.Context, c *apiClient, path string) (T, error) {
	response, err := c.send(ctx, newAPIRequest(http.MethodGet, c.url(path)).withTimeout(requestTimeout))
	if err != nil {
		var zero T
		return zero, err
	}
	return apiDecode[T](response)
}

func (c *apiClient) githubToken(ctx context.Context, owner *string, repositories []string) (string, error) {
	query := make([][2]string, 0, len(repositories)+1)
	if owner != nil {
		query = append(query, [2]string{"owner", *owner})
	}
	for _, repository := range repositories {
		query = append(query, [2]string{"repo", repository})
	}
	request := newAPIRequest(http.MethodGet, c.url("/github-token")).
		withQuery(query...).
		withTimeout(requestTimeout)
	response, err := c.send(ctx, request)
	if err != nil {
		return "", err
	}
	decoded, err := apiDecode[server.GithubTokenResponse](response)
	if err != nil {
		return "", err
	}
	return decoded.Token, nil
}

// apiPostJSON is Rust's `ApiClient::post_json`.
func apiPostJSON[T any](ctx context.Context, c *apiClient, path string, body any, timeout time.Duration) (T, error) {
	var zero T
	request, err := newAPIRequest(http.MethodPost, c.url(path)).withTimeout(timeout).withJSON(body)
	if err != nil {
		return zero, err
	}
	response, err := c.send(ctx, request)
	if err != nil {
		return zero, err
	}
	return apiDecode[T](response)
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
	// serde_urlencoded leaves an absent workspace out.
	request := newAPIRequest(http.MethodGet, c.url("/sessions"))
	if workspaceID != nil {
		request.withQuery([2]string{"workspace_id", *workspaceID})
	}
	request.withQuery([2]string{"all", strconv.FormatBool(all)}).withTimeout(requestTimeout)
	response, err := c.send(ctx, request)
	if err != nil {
		return server.SessionListResponse{}, err
	}
	return apiDecode[server.SessionListResponse](response)
}

func (c *apiClient) message(ctx context.Context, sessionID string, request *server.SessionMessageRequest) (server.SessionMessageResponse, error) {
	return apiPostJSON[server.SessionMessageResponse](ctx, c, "/sessions/"+sessionID+"/message", request, requestTimeout)
}

func (c *apiClient) models(ctx context.Context, profile string, model *string, unrestricted, withDisabled bool) (core.ProfileConfig, error) {
	request := newAPIRequest(http.MethodGet, c.url("/profiles/"+profile+"/config")).
		withTimeout(330 * time.Second)
	if model != nil {
		request.withQuery([2]string{"model", *model})
	}
	if unrestricted {
		request.withQuery([2]string{"unrestricted", "true"})
	}
	if withDisabled {
		request.withQuery([2]string{"with_disabled", "true"})
	}
	response, err := c.send(ctx, request)
	if err != nil {
		return core.ProfileConfig{}, err
	}
	return apiDecode[core.ProfileConfig](response)
}

func (c *apiClient) profileLoginErrors(ctx context.Context) (map[string]string, error) {
	response, err := c.send(ctx, newAPIRequest(http.MethodGet, c.url("/profiles/login-errors")).withTimeout(requestTimeout))
	if err != nil {
		return nil, err
	}
	return apiDecode[map[string]string](response)
}

func (c *apiClient) setConfig(ctx context.Context, session string, request *server.SetConfigRequest) (server.ApiSession, error) {
	built, err := newAPIRequest(http.MethodPatch, c.url("/sessions/"+session+"/config")).withJSON(request)
	if err != nil {
		return server.ApiSession{}, err
	}
	response, err := c.send(ctx, built.withTimeout(requestTimeout))
	if err != nil {
		return server.ApiSession{}, err
	}
	return apiDecode[server.ApiSession](response)
}

// sessionIfKnown is one session, or nil when the daemon knows no session
// with that id. An id that names nothing here may still name a SessionWiki
// row.
func (c *apiClient) sessionIfKnown(ctx context.Context, sessionID string) (*server.ApiSession, error) {
	request := newAPIRequest(http.MethodGet, c.url("/sessions/"+sessionID)).withTimeout(requestTimeout)
	response, err := c.sendOptional(ctx, request)
	if err != nil || response == nil {
		return nil, err
	}
	session, err := apiDecode[server.ApiSession](response)
	if err != nil {
		return nil, err
	}
	return &session, nil
}

// wikiSession is what the SessionWiki index knows about one row, or nil
// when the index holds no session with that id.
func (c *apiClient) wikiSession(ctx context.Context, wikiID string) (*client.WikiSessionInfo, error) {
	request := newAPIRequest(http.MethodGet, c.url("/wiki/sessions/"+wikiID)).withTimeout(requestTimeout)
	response, err := c.sendOptional(ctx, request)
	if err != nil || response == nil {
		return nil, err
	}
	info, err := apiDecode[client.WikiSessionInfo](response)
	if err != nil {
		return nil, err
	}
	return &info, nil
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
	seconds := uint64(600)
	if request.TimeoutSecs != nil {
		seconds = *request.TimeoutSecs
	}
	timeout := time.Duration(seconds)*time.Second + waitSlack
	return apiPostJSON[server.WaitResponse](ctx, c, "/sessions/"+sessionID+"/wait", request, timeout)
}

func (c *apiClient) transcript(
	ctx context.Context,
	sessionID string,
	afterSeq *uint64,
	limit *int,
	roles []core.TranscriptRole,
	finishedOnly bool,
) (server.TranscriptResponse, error) {
	var query []string
	for _, role := range roles {
		query = append(query, "role="+role.AsStr())
	}
	if finishedOnly {
		query = append(query, "finished_only=true")
	}
	if afterSeq != nil {
		query = append(query, "after_seq="+strconv.FormatUint(*afterSeq, 10))
	}
	if limit != nil {
		query = append(query, "limit="+strconv.Itoa(*limit))
	}
	path := "/sessions/" + sessionID + "/transcript"
	if len(query) > 0 {
		path += "?" + strings.Join(query, "&")
	}
	return apiGetJSON[server.TranscriptResponse](ctx, c, path)
}

// events opens the server-sent event stream of `GET /events`. The caller
// reads and closes the response body.
func (c *apiClient) events(ctx context.Context, filter *core.ApiEventFilter, afterSeq *uint64) (*http.Response, error) {
	request := newAPIRequest(http.MethodGet, c.url("/events"))
	request.header.Set("Accept", "text/event-stream")
	// serde_urlencoded leaves absent members out.
	if filter.SessionID != nil {
		request.withQuery([2]string{"session_id", *filter.SessionID})
	}
	if filter.WorkspaceID != nil {
		request.withQuery([2]string{"workspace_id", *filter.WorkspaceID})
	}
	if afterSeq != nil {
		request.withQuery([2]string{"after_seq", strconv.FormatUint(*afterSeq, 10)})
	}
	response, err := c.send(ctx, request)
	if err != nil {
		return nil, err
	}
	if !strings.HasPrefix(headerText(response.Header, "Content-Type"), "text/event-stream") {
		response.Body.Close()
		return nil, errors.New("API events endpoint did not return an event stream")
	}
	return response, nil
}

// headerText is reqwest's `headers().get(name).and_then(|v| v.to_str().ok())`
// with "" for a missing or non-visible-ASCII value.
func headerText(header http.Header, name string) string {
	values := header.Values(name)
	if len(values) == 0 || !headerValueIsVisible(values[0]) {
		return ""
	}
	return values[0]
}

// headerValueIsVisible is whether HeaderValue::to_str accepts value: only
// visible ASCII and tab.
func headerValueIsVisible(value string) bool {
	return !strings.ContainsFunc(value, func(r rune) bool {
		return r != '\t' && (r < 0x20 || r > 0x7e)
	})
}

func (c *apiClient) usage(ctx context.Context, sessionID string, afterSeq *uint64, limit *int) (core.UsagePage, error) {
	after := uint64(0)
	if afterSeq != nil {
		after = *afterSeq
	}
	count := 200
	if limit != nil {
		count = *limit
	}
	return apiGetJSON[core.UsagePage](ctx, c, fmt.Sprintf("/sessions/%s/usage?after_seq=%d&limit=%d", sessionID, after, count))
}

func (c *apiClient) usageTree(ctx context.Context, parent string) (core.UsageTree, error) {
	return apiGetJSON[core.UsageTree](ctx, c, "/sessions/"+parent+"/usage/tree")
}

func (c *apiClient) usageReport(ctx context.Context, query *core.SessionUsageQuery) (core.SessionUsageReport, error) {
	request := newAPIRequest(http.MethodGet, c.url("/usage/report"))
	// serde_urlencoded leaves absent members out.
	if query.Profile != nil {
		request.withQuery([2]string{"profile", *query.Profile})
	}
	if query.HourStartMs != nil {
		request.withQuery([2]string{"hour_start_ms", strconv.FormatInt(*query.HourStartMs, 10)})
	}
	request.withQuery([2]string{"include_suspended", strconv.FormatBool(query.IncludeSuspended)}).
		withTimeout(requestTimeout)
	response, err := c.send(ctx, request)
	if err != nil {
		return core.SessionUsageReport{}, err
	}
	return apiDecode[core.SessionUsageReport](response)
}

func (c *apiClient) diff(ctx context.Context, sessionID string, base *string, asJSON bool) (string, error) {
	request := newAPIRequest(http.MethodGet, c.url("/sessions/"+sessionID+"/diff"))
	if base != nil {
		request.withQuery([2]string{"base", *base})
	}
	if asJSON {
		request.withQuery([2]string{"json", "true"})
	}
	response, err := c.send(ctx, request.withTimeout(exportTimeout))
	if err != nil {
		return "", err
	}
	defer response.Body.Close()
	body, err := io.ReadAll(response.Body)
	if err != nil {
		return "", fmt.Errorf("read the session diff: %w", err)
	}
	return core.LossyUTF8(body), nil
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
	overwriteText := "false"
	if overwrite {
		overwriteText = "true"
	}
	request := newAPIRequest(http.MethodPut, c.url("/sessions/"+sessionID+"/files")).
		withQuery([2]string{"path", path}, [2]string{"overwrite", overwriteText})
	request.header.Set("Content-Type", "application/octet-stream")
	if contentLength != nil {
		length := *contentLength
		request.contentLength = &length
	}
	request.stream = body
	response, err := c.send(ctx, request)
	if err != nil {
		return server.WriteFileResponse{}, err
	}
	defer response.Body.Close()
	var written server.WriteFileResponse
	if err := json.NewDecoder(response.Body).Decode(&written); err != nil {
		return server.WriteFileResponse{}, fmt.Errorf("read file injection result: %w", err)
	}
	return written, nil
}

func (c *apiClient) elicitations(ctx context.Context, sessionID string) ([]core.ElicitationRequest, error) {
	return apiGetJSON[[]core.ElicitationRequest](ctx, c, "/sessions/"+sessionID+"/elicitations")
}

func (c *apiClient) respondElicitation(ctx context.Context, sessionID, elicitationID string, response *core.ElicitationResponse) error {
	request, err := newAPIRequest(http.MethodPost, c.url("/sessions/"+sessionID+"/elicitations/"+elicitationID)).withJSON(response)
	if err != nil {
		return err
	}
	answered, err := c.send(ctx, request.withTimeout(requestTimeout))
	if err != nil {
		return err
	}
	return answered.Body.Close()
}

// export exports the session's work. A branch push answers JSON; a patch and
// a bundle answer bytes, so the caller gets both and uses the one its kind
// produced.
func (c *apiClient) export(ctx context.Context, sessionID string, request *server.ExportRequest) (exportResult, error) {
	built, err := newAPIRequest(http.MethodPost, c.url("/sessions/"+sessionID+"/export")).
		withTimeout(exportTimeout).
		withJSON(request)
	if err != nil {
		return nil, err
	}
	response, err := c.send(ctx, built)
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	contentType := headerText(response.Header, "Content-Type")
	bytes, err := io.ReadAll(response.Body)
	if err != nil {
		return nil, fmt.Errorf("read the export: %w", err)
	}
	if strings.HasPrefix(contentType, "application/json") {
		var pushed server.PushedBranch
		if err := json.Unmarshal(bytes, &pushed); err != nil {
			return nil, fmt.Errorf("read the pushed branch: %w", err)
		}
		return exportResultBranch{branch: pushed}, nil
	}
	return exportResultBytes{bytes: bytes}, nil
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
	query := apiClientSuspendPreflightQuery(dryRun, withoutCheckpoint)
	request := newAPIRequest(http.MethodGet, c.url("/sessions/"+sessionID+"/suspend-preflight"+query)).
		withTimeout(requestTimeout)
	response, err := c.send(ctx, request)
	if err != nil {
		return core.SuspendPreflight{}, err
	}
	defer response.Body.Close()
	body, err := io.ReadAll(response.Body)
	if err != nil {
		return core.SuspendPreflight{}, fmt.Errorf("read the suspend preflight answer: %w", err)
	}
	var preflight core.SuspendPreflight
	if err := json.Unmarshal(body, &preflight); err != nil {
		return core.SuspendPreflight{}, fmt.Errorf("decode the suspend preflight answer: %w", err)
	}
	return preflight, nil
}

// suspend asks the daemon to suspend a session. The answer says how many
// sub-agents the suspend stops, and warns when some of them have not handed
// back.
func (c *apiClient) suspend(ctx context.Context, sessionID string, consent *core.SuspendConsent) (server.SuspendSessionResponse, error) {
	payload := jsonvalue.Object{
		{Key: "discard_untracked", Value: consent.DiscardUntracked},
		{Key: "abort_operations", Value: consent.AbortOperations},
		{Key: "without_checkpoint", Value: consent.WithoutCheckpoint},
	}
	if consent.ConfirmNoCheckpoint {
		payload.Set("confirm_no_checkpoint", true)
	}
	encoded, err := jsonvalue.Marshal(payload)
	if err != nil {
		return server.SuspendSessionResponse{}, err
	}
	request := newAPIRequest(http.MethodPost, c.url("/sessions/"+sessionID+"/suspend")).withTimeout(requestTimeout)
	request.body = encoded
	request.header.Set("Content-Type", "application/json")
	response, err := c.send(ctx, request)
	if err != nil {
		return server.SuspendSessionResponse{}, err
	}
	defer response.Body.Close()
	body, err := io.ReadAll(response.Body)
	if err != nil {
		return server.SuspendSessionResponse{}, fmt.Errorf("read the suspend answer: %w", err)
	}
	if len(body) == 0 {
		return server.SuspendSessionResponse{SessionID: sessionID}, nil
	}
	var answer server.SuspendSessionResponse
	if err := json.Unmarshal(body, &answer); err != nil {
		return server.SuspendSessionResponse{}, fmt.Errorf("read the suspend answer: %w", err)
	}
	return answer, nil
}

// purgeUntracked has no total or read timeout. Ctrl-C ends this process,
// which closes the connection, and the daemon stops the batch on that
// disconnect.
func (c *apiClient) purgeUntracked(ctx context.Context, request *server.PurgeUntrackedRequest) (server.PurgeUntrackedResponse, error) {
	built, err := newAPIRequest(http.MethodPost, c.url("/checkpoints/purge-untracked")).withJSON(request)
	if err != nil {
		return server.PurgeUntrackedResponse{}, err
	}
	response, err := c.send(ctx, built)
	if err != nil {
		return server.PurgeUntrackedResponse{}, err
	}
	return apiDecode[server.PurgeUntrackedResponse](response)
}

func (c *apiClient) destroy(ctx context.Context, sessionID string) error {
	request := newAPIRequest(http.MethodPost, c.url("/sessions/"+sessionID+"/destroy")).withTimeout(requestTimeout)
	request.body = []byte("{}")
	request.header.Set("Content-Type", "application/json")
	response, err := c.send(ctx, request)
	if err != nil {
		return err
	}
	return response.Body.Close()
}

func (c *apiClient) notices(ctx context.Context, sessionID string) ([]client.RuntimeNotice, error) {
	response, err := c.send(ctx, newAPIRequest(http.MethodGet, c.url("/sessions/"+sessionID+"/notices")).withTimeout(requestTimeout))
	if err != nil {
		return nil, err
	}
	return apiDecode[[]client.RuntimeNotice](response)
}

// resume asks the daemon to resume a stopped session. It answers as soon as
// the resume is admitted, so this does not wait for the session to come up.
func (c *apiClient) resume(ctx context.Context, sessionID string, request *server.ResumeSessionRequest) (server.ResumeSessionResponse, error) {
	return apiPostJSON[server.ResumeSessionResponse](ctx, c, "/sessions/"+sessionID+"/resume", request, requestTimeout)
}

func (c *apiClient) interruptTurn(ctx context.Context, sessionID string) error {
	response, err := c.send(ctx, newAPIRequest(http.MethodPost, c.url("/sessions/"+sessionID+"/interrupt-turn")).withTimeout(requestTimeout))
	if err != nil {
		return err
	}
	return response.Body.Close()
}

// restartSession answers the daemon's JSON (a serde_json::Value, held as a
// jsonvalue value), or `{"session_id": ..., "restarted": true}` when it
// answered nothing readable.
func (c *apiClient) restartSession(ctx context.Context, sessionID string) (any, error) {
	response, err := c.send(ctx, newAPIRequest(http.MethodPost, c.url("/sessions/"+sessionID+"/restart")).withTimeout(requestTimeout))
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	body, err := io.ReadAll(response.Body)
	if err != nil {
		return nil, err
	}
	restarted := jsonvalue.Object{
		{Key: "session_id", Value: sessionID},
		{Key: "restarted", Value: true},
	}
	text := core.LossyUTF8(body)
	if strings.TrimSpace(text) == "" {
		return restarted, nil
	}
	parsed, err := jsonvalue.Parse([]byte(text))
	if err != nil {
		return restarted, nil
	}
	return parsed, nil
}

// startReview starts a one-off review of the turn the session just
// finished. The daemon answers once the review has opened, which includes
// choosing and starting a reviewer, so this may take minutes.
func (c *apiClient) startReview(ctx context.Context, sessionID string) (server.StartReviewResponse, error) {
	response, err := c.send(ctx, newAPIRequest(http.MethodPost, c.url("/sessions/"+sessionID+"/review")).withTimeout(reviewStartTimeout))
	if err != nil {
		return server.StartReviewResponse{}, err
	}
	return apiDecode[server.StartReviewResponse](response)
}

// reviewStatus is the review the session has open, if any.
func (c *apiClient) reviewStatus(ctx context.Context, sessionID string) (server.ReviewStatusResponse, error) {
	return apiGetJSON[server.ReviewStatusResponse](ctx, c, "/sessions/"+sessionID+"/review")
}

// finishedReview is a finished review's per-reviewer output, or the latest
// when reviewID is nil.
func (c *apiClient) finishedReview(ctx context.Context, sessionID string, reviewID *string) (server.FinishedReviewResponse, error) {
	request := newAPIRequest(http.MethodGet, c.url("/sessions/"+sessionID+"/reviews"))
	if reviewID != nil {
		request.withQuery([2]string{"review", *reviewID})
	}
	response, err := c.send(ctx, request)
	if err != nil {
		return server.FinishedReviewResponse{}, err
	}
	return apiDecode[server.FinishedReviewResponse](response)
}

func (c *apiClient) stopBackgroundTask(ctx context.Context, sessionID, taskID string) error {
	request, err := newAPIRequest(http.MethodPost, c.url("/sessions/"+sessionID+"/background-tasks/stop")).
		withJSON(&server.StopBackgroundTaskRequest{BackgroundTaskID: taskID})
	if err != nil {
		return err
	}
	response, err := c.send(ctx, request.withTimeout(requestTimeout))
	if err != nil {
		return err
	}
	return response.Body.Close()
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
//
// As in Rust, a given proxy is then cleared by `no_proxy`: the daemon API is
// always reached directly, and a test passes a proxy to show it is not used.
func buildHTTPClient(certificateSha256 *string, resolver *viewerResolver, proxy func(*http.Request) (*url.URL, error)) (*http.Client, error) {
	transport := &http.Transport{
		Proxy:             proxy,
		ForceAttemptHTTP2: true,
		MaxIdleConns:      100,
		IdleConnTimeout:   90 * time.Second,
	}
	// no_proxy: clear every proxy, including the system's.
	transport.Proxy = nil
	dialer := &net.Dialer{Timeout: 30 * time.Second, KeepAlive: 30 * time.Second}
	transport.DialContext = dialer.DialContext
	if resolver != nil {
		resolved := *resolver
		transport.DialContext = func(ctx context.Context, network, address string) (net.Conn, error) {
			host, port, err := net.SplitHostPort(address)
			if err == nil && strings.EqualFold(host, resolved.host) {
				// The URL's port is the listener's: route checked they match.
				address = net.JoinHostPort(resolved.address.Addr().String(), port)
			}
			return dialer.DialContext(ctx, network, address)
		}
	}
	if certificateSha256 != nil {
		config, err := pinnedClientConfig(*certificateSha256)
		if err != nil {
			return nil, err
		}
		transport.TLSClientConfig = config
	} else {
		transport.TLSClientConfig = &tls.Config{MinVersion: tls.VersionTLS12}
	}
	return &http.Client{
		Transport: userAgentTransport{inner: transport},
		CheckRedirect: func(*http.Request, []*http.Request) error {
			return http.ErrUseLastResponse
		},
	}, nil
}

// userAgentTransport names this client on every request, as reqwest's
// `user_agent` does.
type userAgentTransport struct {
	inner http.RoundTripper
}

func (t userAgentTransport) RoundTrip(request *http.Request) (*http.Response, error) {
	if request.Header.Get("User-Agent") == "" {
		request = request.Clone(request.Context())
		request.Header.Set("User-Agent", "mj/"+core.PackageVersion)
	}
	return t.inner.RoundTrip(request)
}

// routeViewerToLocalSocket preserves a DNS authority for HTTP Host and TLS
// SNI while sending the connection to the daemon-published local listener.
func routeViewerToLocalSocket(viewerURL string, localAddress netip.AddrPort) (string, *viewerResolver, error) {
	parsed, err := url.Parse(viewerURL)
	if err != nil || parsed.Scheme == "" || !parsed.IsAbs() {
		if err == nil {
			err = errors.New("relative URL without a base")
		}
		return "", nil, fmt.Errorf("parse web viewer URL: %w", err)
	}
	normalizeViewerURL(parsed)
	port, ok := urlPortOrKnownDefault(parsed)
	if !ok {
		return "", nil, errors.New("web viewer URL has no known port")
	}
	if port != localAddress.Port() {
		return "", nil, fmt.Errorf("web viewer URL port %d does not match its local listener port %d", port, localAddress.Port())
	}
	host := parsed.Hostname()
	if host == "" {
		return "", nil, errors.New("web viewer URL has no host")
	}
	local := localAddress.Addr()
	var resolver *viewerResolver
	if ip, err := netip.ParseAddr(host); err == nil && !strings.Contains(host, "%") {
		switch {
		case ip.Is4() && ip.IsUnspecified():
			if !local.Is4() {
				return "", nil, errors.New("web viewer wildcard host and local listener use different address families")
			}
			setURLIPHost(parsed, local)
		case ip.Is6() && ip.IsUnspecified():
			if !local.Is6() {
				return "", nil, errors.New("web viewer wildcard host and local listener use different address families")
			}
			setURLIPHost(parsed, local)
		default:
			if local != ip {
				return "", nil, errors.New("web viewer URL address does not match its local listener address")
			}
		}
	} else {
		resolver = &viewerResolver{host: host, address: localAddress}
	}
	return parsed.String(), resolver, nil
}

// urlSpecialPorts are the url crate's default ports of the special schemes.
var urlSpecialPorts = map[string]uint16{"http": 80, "https": 443, "ws": 80, "wss": 443, "ftp": 21}

// normalizeViewerURL serializes a parsed URL as the url crate does: a lower
// case domain, no default port, `/` for an empty path of a special scheme,
// and an IPv6 host in its canonical form.
func normalizeViewerURL(parsed *url.URL) {
	host := parsed.Hostname()
	port := parsed.Port()
	if defaultPort, special := urlSpecialPorts[strings.ToLower(parsed.Scheme)]; special {
		if port == strconv.Itoa(int(defaultPort)) {
			port = ""
		}
		if parsed.Path == "" && parsed.Opaque == "" {
			parsed.Path = "/"
		}
	}
	if ip, err := netip.ParseAddr(host); err == nil {
		host = ip.String()
		if ip.Is6() {
			host = "[" + host + "]"
		}
	} else {
		host = strings.ToLower(host)
	}
	if port != "" {
		parsed.Host = host + ":" + port
	} else {
		parsed.Host = host
	}
}

// urlPortOrKnownDefault is the url crate's `port_or_known_default`.
func urlPortOrKnownDefault(parsed *url.URL) (uint16, bool) {
	if text := parsed.Port(); text != "" {
		port, err := strconv.ParseUint(text, 10, 16)
		if err != nil {
			return 0, false
		}
		return uint16(port), true
	}
	port, ok := urlSpecialPorts[strings.ToLower(parsed.Scheme)]
	return port, ok
}

// setURLIPHost is the url crate's `set_ip_host`, keeping the port.
func setURLIPHost(parsed *url.URL, ip netip.Addr) {
	host := ip.String()
	if ip.Is6() {
		host = "[" + host + "]"
	}
	if port := parsed.Port(); port != "" {
		parsed.Host = host + ":" + port
	} else {
		parsed.Host = host
	}
}

// probeAPI proves the route exists before a token is read: an
// unauthenticated versioned 401 is enough.
func probeAPI(ctx context.Context, httpClient *http.Client, baseURL string) error {
	ctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, strings.TrimRight(baseURL, "/")+"/api/v1/sessions", nil)
	if err != nil {
		return fmt.Errorf("reach the daemon API; check `mj daemon status`: %w", err)
	}
	response, err := httpClient.Do(request)
	if err != nil {
		return fmt.Errorf("reach the daemon API; check `mj daemon status`: %w", err)
	}
	defer response.Body.Close()
	if isHandoffRefusal(response) {
		return daemonHandoff{}
	}
	if response.StatusCode == http.StatusNotFound && len(response.Header.Values(server.APIVersionHeader)) == 0 {
		return errors.New("this daemon predates the Mjolnir API; run `mj daemon restart`")
	}
	if err := checkVersion(response); err != nil {
		return err
	}
	if (response.StatusCode >= 200 && response.StatusCode < 300) || response.StatusCode == http.StatusUnauthorized {
		return nil
	}
	return fmt.Errorf("the daemon API is unavailable (%s); check `mj daemon status`", statusDisplay(response.StatusCode))
}

// actionSlots is how full the daemon says its action pool is.
type actionSlots struct {
	running int
	limit   int
}

// debug is Rust's Debug of the `Option<(usize, usize)>` it was.
func (s *actionSlots) debug() string {
	if s == nil {
		return "None"
	}
	return fmt.Sprintf("Some((%d, %d))", s.running, s.limit)
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
	values := response.Header.Values(server.UpgradeHeader)
	return response.StatusCode == http.StatusServiceUnavailable && len(values) > 0 && values[0] == "pending"
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
	if e.code != nil && *e.code == server.DaemonHandoffCode {
		return daemonHandoff{}
	}
	remedy := ""
	if e.code != nil && *e.code == core.ChoiceUnavailableCode {
		remedy = " Configure [subagents.straightforward] and optionally [subagents.subtle] in config.toml or TUI Settings → Sub-agents."
	}
	if e.message == "" {
		return fmt.Errorf("the Mjolnir API answered %s", statusDisplay(e.status))
	}
	return fmt.Errorf("the Mjolnir API answered %s: %s%s", statusDisplay(e.status), e.message, remedy)
}

// statusDisplay is reqwest's StatusCode Display: the code and the http
// crate's canonical reason, or `<unknown status code>`. Some of the http
// crate's reasons differ from net/http's StatusText (413 "Payload Too
// Large", 414 "URI Too Long", 416 "Range Not Satisfiable").
func statusDisplay(status int) string {
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
	values := response.Header.Values(server.APIVersionHeader)
	// HeaderValue::to_str accepts only visible ASCII and tab.
	if len(values) == 0 || !headerValueIsVisible(values[0]) {
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
// a body that does not decode. It closes the body.
func apiDecode[T any](response *http.Response) (T, error) {
	var value T
	defer response.Body.Close()
	body, err := io.ReadAll(response.Body)
	if err != nil {
		return value, fmt.Errorf("read the API response: %w", err)
	}
	if err := json.Unmarshal(body, &value); err != nil {
		text := []rune(core.LossyUTF8(body))
		if len(text) > 200 {
			text = text[:200]
		}
		return value, fmt.Errorf("read the API response body: %s: %w", string(text), err)
	}
	return value, nil
}
