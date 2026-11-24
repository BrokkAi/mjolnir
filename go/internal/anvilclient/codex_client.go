// Port of brokk-anvil-client 0.28.6 src/codex_client.rs (the part Mjolnir uses).

// ChatGPT-subscription-backed LLM backend.
//
// Talks to `https://chatgpt.com/backend-api/codex/responses` (the Responses
// API, not Chat Completions) using the OAuth `access_token` and
// `chatgpt_account_id` stored in a Codex `auth.json`. This is the same
// endpoint Codex CLI hits after `codex login` with a ChatGPT account, so
// usage counts against the ChatGPT subscription rather than an API key.
//
// The `Authorization` and `ChatGPT-Account-ID` headers mirror what Codex CLI
// sends, and `originator: codex_cli_rs` with a matching User-Agent presents
// the client as Codex CLI: a compatibility choice, since the user
// authenticates with their own OAuth tokens.

package anvilclient

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/cookiejar"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

const (
	// chatgptResponsesURL is the streaming completions endpoint.
	chatgptResponsesURL = "https://chatgpt.com/backend-api/codex/responses"
	// chatgptModelsURL lists the slugs the user's ChatGPT plan can route
	// to.
	chatgptModelsURL = "https://chatgpt.com/backend-api/codex/models"
	// codexOriginator is the originator Codex CLI sends; the server gates
	// ChatGPT-subscription usage on it alongside the OAuth token.
	codexOriginator = "codex_cli_rs"
	// reasoningEncryptedContentInclude makes the backend return reasoning
	// items with their encrypted payload attached.
	reasoningEncryptedContentInclude = "reasoning.encrypted_content"
	// fallbackChatgptModel is the last-resort model if `/models` is
	// unreachable.
	fallbackChatgptModel = "gpt-5-codex"
	// fallbackCodexCompatClientVersion is the `client_version` reported
	// if Codex's published model catalog cannot be fetched. The server
	// filters out any model whose minimal_client_version exceeds it.
	fallbackCodexCompatClientVersion = "0.144.0"
	// codexModelsManifestURL is Codex's checked-in model catalog; its
	// newest visible minimal_client_version is the `/codex/models`
	// client_version.
	codexModelsManifestURL      = "https://raw.githubusercontent.com/openai/codex/main/codex-rs/models-manager/models.json"
	codexModelsManifestTimeout  = 2 * time.Second
	codexModelsManifestCacheTTL = 6 * time.Hour
)

// codexCompatClientVersionCache holds the manifest's client_version for
// its TTL, process-wide.
var codexCompatClientVersionCache struct {
	mu        sync.Mutex
	version   string
	fetchedAt time.Time
}

// codexPromptCacheIdentity is the per-conversation identity the ChatGPT
// backend uses to route a turn back to the prompt cache its predecessor
// warmed. It is a transport-level routing hint only.
type codexPromptCacheIdentity struct {
	sessionID string
	threadID  string
}

func mintCodexPromptCacheIdentity() codexPromptCacheIdentity {
	return codexPromptCacheIdentity{sessionID: newUUIDv4(), threadID: newUUIDv4()}
}

// responsesContinuation is what the prefix match found for a turn: the
// identity to route on, and how far into the messages the match reached
// (nil when nothing matched), which is also how far stored reasoning items
// may be replayed.
type responsesContinuation struct {
	identity                codexPromptCacheIdentity
	reasoningReplayBoundary *int
}

// chatgptCredentials are the credentials extracted from auth.json.
type chatgptCredentials struct {
	accessToken string
	accountID   string
}

func chatgptCredentialsFromAuth(auth *AuthDotJSON) (chatgptCredentials, error) {
	if auth.Tokens == nil {
		return chatgptCredentials{}, errors.New("auth.json has no `tokens` block; run /setup codex again")
	}
	if auth.Tokens.AccessToken == "" {
		return chatgptCredentials{}, errors.New("auth.json `tokens.access_token` is empty")
	}
	if auth.Tokens.AccountID == "" {
		return chatgptCredentials{}, errors.New("auth.json `tokens.account_id` is empty")
	}
	return chatgptCredentials{accessToken: auth.Tokens.AccessToken, accountID: auth.Tokens.AccountID}, nil
}

func isChatgptMode(auth *AuthDotJSON) bool {
	return auth.AuthMode != nil && *auth.AuthMode == "chatgpt"
}

// applyChatgptAuth applies the account-bound authentication headers shared
// by ChatGPT backend endpoints.
func applyChatgptAuth(request *http.Request, credentials *chatgptCredentials) {
	request.Header.Set("Authorization", "Bearer "+credentials.accessToken)
	request.Header.Set("ChatGPT-Account-ID", credentials.accountID)
}

// CodexClient talks to the ChatGPT Responses API with a Codex login. It
// reads the auth file on every request and transparently refreshes the
// OAuth tokens when they go stale.
type CodexClient struct {
	http     *http.Client
	authPath string
	// refreshMu serializes concurrent token-refresh attempts. Without it,
	// two in-flight prompts hitting a 401 race each other into the refresh
	// endpoint and one of the resulting refresh tokens is invalidated by the
	// server's rotation policy.
	refreshMu        sync.Mutex
	noticesMu        sync.Mutex
	discoveryNotices []ModelDiscoveryNotice
	responsesURL     string
	// identitiesMu guards promptCacheIdentities, the content-keyed cache
	// mapping a hash of a message context to the identity that context was
	// last sent under.
	identitiesMu          sync.Mutex
	promptCacheIdentities *responsesChainCache[codexPromptCacheIdentity]
}

// rustOS is Rust's std::env::consts::OS for this platform.
func rustOS() string {
	if runtime.GOOS == "darwin" {
		return "macos"
	}
	return runtime.GOOS
}

// userAgentTransport sets the client's User-Agent on every request, as
// reqwest's builder does.
type userAgentTransport struct {
	base      http.RoundTripper
	userAgent string
}

func (t userAgentTransport) RoundTrip(request *http.Request) (*http.Response, error) {
	if request.Header.Get("User-Agent") == "" {
		request = request.Clone(request.Context())
		request.Header.Set("User-Agent", t.userAgent)
	}
	return t.base.RoundTrip(request)
}

// NewCodexClientWithAuthPath builds a client that reads and refreshes the
// login at authPath. The ChatGPT backend sits behind Cloudflare, so the
// client keeps a per-client cookie jar.
func NewCodexClientWithAuthPath(authPath string) *CodexClient {
	jar, err := cookiejar.New(nil)
	if err != nil {
		panic(fmt.Sprintf("failed to build HTTP client: %v", err))
	}
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.DialContext = (&net.Dialer{Timeout: 10 * time.Second}).DialContext
	return &CodexClient{
		http: &http.Client{
			Transport: userAgentTransport{
				base:      transport,
				userAgent: fmt.Sprintf("%s/%s (brokk-acp; %s)", codexOriginator, anvilVersion, rustOS()),
			},
			Timeout: 600 * time.Second,
			Jar:     jar,
		},
		authPath:              authPath,
		responsesURL:          chatgptResponsesURL,
		promptCacheIdentities: newResponsesChainCache[codexPromptCacheIdentity](responsesChainCacheCap),
	}
}

// loadCredentials loads fresh credentials from disk, refreshing the OAuth
// tokens if they're past Codex's staleness window. The fast path skips the
// refresh lock; the lock is taken only when a refresh is warranted, and the
// file is re-read under it in case another request refreshed first.
func (c *CodexClient) loadCredentials(ctx context.Context) (chatgptCredentials, error) {
	auth, err := ReadAuthDotJSONAt(c.authPath)
	if err != nil {
		return chatgptCredentials{}, err
	}
	if auth == nil {
		return chatgptCredentials{}, errors.New("~/.codex/auth.json not found; run /setup codex to authenticate")
	}
	if !isChatgptMode(auth) {
		return chatgptCredentials{}, fmt.Errorf("auth.json has auth_mode=%s (need \"chatgpt\" for subscription routing); "+
			"re-run /setup codex (apikey-mode auth.json is auto-detected by the "+
			"OPENAI_API_KEY backend at startup -- restart the server to switch)", debugOptionalString(auth.AuthMode))
	}
	if !isStale(auth) {
		return chatgptCredentialsFromAuth(auth)
	}
	c.refreshMu.Lock()
	defer c.refreshMu.Unlock()
	auth, err = ReadAuthDotJSONAt(c.authPath)
	if err != nil {
		return chatgptCredentials{}, err
	}
	if auth == nil {
		return chatgptCredentials{}, errors.New("~/.codex/auth.json disappeared while waiting for refresh lock")
	}
	if !isChatgptMode(auth) {
		return chatgptCredentials{}, errors.New("auth.json switched out of chatgpt mode while waiting for refresh lock")
	}
	if isStale(auth) {
		if _, err := refreshIfStaleAt(ctx, c.authPath, auth); err != nil {
			slog.Warn(fmt.Sprintf("proactive token refresh failed (will retry on 401): %v", err))
		}
	}
	return chatgptCredentialsFromAuth(auth)
}

// debugOptionalString is Rust's `{:?}` of an Option<String>.
func debugOptionalString(value *string) string {
	if value == nil {
		return "None"
	}
	return "Some(" + strconv.Quote(*value) + ")"
}

// forceRefresh forces a fresh token regardless of `last_refresh`, after a
// 401. The backdated marker stays in memory: writing it would let other
// workers pile into the refresh endpoint themselves.
func (c *CodexClient) forceRefresh(ctx context.Context) (chatgptCredentials, error) {
	c.refreshMu.Lock()
	defer c.refreshMu.Unlock()
	auth, err := ReadAuthDotJSONAt(c.authPath)
	if err != nil {
		return chatgptCredentials{}, err
	}
	if auth == nil {
		return chatgptCredentials{}, errors.New("~/.codex/auth.json disappeared between requests")
	}
	if !isChatgptMode(auth) {
		return chatgptCredentials{}, errors.New("auth.json no longer in chatgpt mode")
	}
	backdated := time.Now().UTC().Add(-30 * 24 * time.Hour)
	auth.LastRefresh = &backdated
	if _, err := refreshIfStaleAt(ctx, c.authPath, auth); err != nil {
		return chatgptCredentials{}, withContext(err, "forced token refresh failed")
	}
	return chatgptCredentialsFromAuth(auth)
}

// continuationFor recovers the identity this conversation has been using,
// or mints a new one, together with how far the matched prefix reached.
func (c *CodexClient) continuationFor(messages []ChatMessage) responsesContinuation {
	c.identitiesMu.Lock()
	defer c.identitiesMu.Unlock()
	boundary, identity, ok := findResponsesContinuation(messages, c.promptCacheIdentities.get)
	if ok {
		return responsesContinuation{identity: identity, reasoningReplayBoundary: &boundary}
	}
	return responsesContinuation{identity: mintCodexPromptCacheIdentity()}
}

func (c *CodexClient) rememberPromptCacheIdentity(messages []ChatMessage, identity codexPromptCacheIdentity) {
	c.identitiesMu.Lock()
	defer c.identitiesMu.Unlock()
	c.promptCacheIdentities.insert(hashResponsesContext(messages), identity)
}

func (c *CodexClient) ListModels(ctx context.Context) ([]string, error) {
	metadata, err := c.ListModelMetadata(ctx)
	if err != nil {
		return nil, err
	}
	ids := make([]string, 0, len(metadata))
	for _, model := range metadata {
		ids = append(ids, model.ID)
	}
	return ids, nil
}

// ListModelMetadata discovers usable models from the ChatGPT backend. On
// any error it falls back to a single known slug so a caller still has
// something to offer.
func (c *CodexClient) ListModelMetadata(ctx context.Context) ([]ModelMetadata, error) {
	c.noticesMu.Lock()
	c.discoveryNotices = nil
	c.noticesMu.Unlock()
	fallback := []ModelMetadata{ModelMetadataIDOnly(fallbackChatgptModel)}
	credentials, err := c.loadCredentials(ctx)
	if err != nil {
		slog.Warn(fmt.Sprintf("skipping ChatGPT model discovery (credentials not ready): %v", err))
		return fallback, nil
	}
	models, notice, err := fetchChatgptModels(ctx, c.http, &credentials)
	switch {
	case err != nil:
		slog.Warn(fmt.Sprintf("ChatGPT model discovery failed (%s): %v; falling back to %s", chatgptModelsURL, err, fallbackChatgptModel))
		return fallback, nil
	case len(models) == 0:
		slog.Warn(fmt.Sprintf("ChatGPT /models endpoint returned no slugs; falling back to %s", fallbackChatgptModel))
		return fallback, nil
	}
	if notice != nil {
		c.noticesMu.Lock()
		c.discoveryNotices = append(c.discoveryNotices, ModelDiscoveryNotice{Source: "Codex", Message: *notice})
		c.noticesMu.Unlock()
	}
	return models, nil
}

// SupportsNativeStructuredOutput is the trait default.
func (c *CodexClient) SupportsNativeStructuredOutput() bool { return false }

func (c *CodexClient) TakeModelDiscoveryNotices() []ModelDiscoveryNotice {
	c.noticesMu.Lock()
	defer c.noticesMu.Unlock()
	notices := c.discoveryNotices
	c.discoveryNotices = nil
	return notices
}

// chatgptHTTPError is a typed HTTP error, so the 401 retry matches on the
// status code rather than scanning the formatted message.
type chatgptHTTPError struct {
	status int
	body   string
}

func (e *chatgptHTTPError) Error() string {
	return fmt.Sprintf("ChatGPT backend returned HTTP %s: %s", httpStatusText(e.status), e.body)
}

func chatgptHTTPErrorFor(status int, body string) error {
	message := fmt.Sprintf("ChatGPT backend returned HTTP %s: %s", httpStatusText(status), body)
	failure := &chatgptHTTPError{status: status, body: body}
	if containsGatewayTransientMarker(body) {
		return withContext(&markedError{marker: RetryableLlmErrorGatewayTransient("gateway transient response body"), source: failure}, message)
	}
	if containsStandardTransientMarker(body) {
		// Same tier as the Responses path and the status path: these
		// markers are body-borne 5xx/429 equivalents.
		return withContext(&markedError{marker: RetryableLlmErrorGatewayTransient("standard transient response body"), source: failure}, message)
	}
	return failure
}

func isUnauthorized(err error) bool {
	var failure *chatgptHTTPError
	return errors.As(err, &failure) && failure.status == http.StatusUnauthorized
}

func (c *CodexClient) StreamChat(ctx context.Context, request StreamChatRequest) (LlmResponse, error) {
	credentials, err := c.loadCredentials(ctx)
	if err != nil {
		return nil, err
	}
	continuation := c.continuationFor(request.Messages)
	body, err := codexBuildResponsesRequest(request.Model, request.Messages, request.Tools, request.ReasoningEffort,
		request.ServiceTier, request.StructuredOutput, &continuation)
	if err != nil {
		return nil, err
	}
	response, err := c.sendResponsesRequest(ctx, &credentials, &continuation.identity, body, &request)
	if err != nil && isUnauthorized(err) {
		slog.Info("ChatGPT backend returned 401; refreshing tokens and retrying once")
		credentials, err = c.forceRefresh(ctx)
		if err != nil {
			return nil, err
		}
		response, err = c.sendResponsesRequest(ctx, &credentials, &continuation.identity, body, &request)
	}
	// Record the identity only for a turn the server actually answered:
	// that is the turn whose prefix is now warm.
	if err == nil {
		c.rememberPromptCacheIdentity(request.Messages, continuation.identity)
	}
	return response, err
}

func (c *CodexClient) sendResponsesRequest(ctx context.Context, credentials *chatgptCredentials, identity *codexPromptCacheIdentity, body []byte, request *StreamChatRequest) (LlmResponse, error) {
	response, err := sendWithRetries(ctx, "posting Responses API request", c.http, func(attemptCtx context.Context) (*http.Request, error) {
		outgoing, err := http.NewRequestWithContext(attemptCtx, http.MethodPost, c.responsesURL, bytes.NewReader(body))
		if err != nil {
			return nil, err
		}
		outgoing.Header.Set("originator", codexOriginator)
		outgoing.Header.Set("Accept", "text/event-stream")
		outgoing.Header.Set("Content-Type", "application/json")
		applyChatgptAuth(outgoing, credentials)
		// These two are what actually steer this backend's prompt cache.
		outgoing.Header.Set("session-id", identity.sessionID)
		outgoing.Header.Set("thread-id", identity.threadID)
		return outgoing, nil
	}, request.IdleTimeouts.FirstProgress)
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode > 299 {
		text, _ := io.ReadAll(response.Body)
		return nil, chatgptHTTPErrorFor(response.StatusCode, strings.TrimSpace(string(text)))
	}
	return codexDriveResponsesSSEStream(ctx, response.Body, request.OnToken, request.OnThought, request.IdleTimeouts)
}

// fetchChatgptModels lists the slugs the user's plan can route to, sorted
// by the server's priority (descending). client_version gates per-model
// rollout.
func fetchChatgptModels(ctx context.Context, client *http.Client, credentials *chatgptCredentials) ([]ModelMetadata, *string, error) {
	clientVersion, notice := resolveCodexCompatClientVersion(ctx, client)
	url := chatgptModelsURL + "?client_version=" + urlencode(clientVersion)
	response, err := sendWithRetries(ctx, "GET /models", client, func(attemptCtx context.Context) (*http.Request, error) {
		request, err := http.NewRequestWithContext(attemptCtx, http.MethodGet, url, nil)
		if err != nil {
			return nil, err
		}
		request.Header.Set("originator", codexOriginator)
		request.Header.Set("Accept", "application/json")
		applyChatgptAuth(request, credentials)
		return request, nil
	}, 0)
	if err != nil {
		return nil, notice, err
	}
	defer response.Body.Close()
	contentType := response.Header.Get("Content-Type")
	body, err := io.ReadAll(response.Body)
	if err != nil {
		return nil, notice, withContext(err, "reading /models response body")
	}
	if response.StatusCode < 200 || response.StatusCode > 299 {
		shown := contentType
		if shown == "" {
			shown = "(none)"
		}
		return nil, notice, fmt.Errorf("ChatGPT /models returned HTTP %s (content-type: %s): %s",
			httpStatusText(response.StatusCode), shown, bodyExcerpt(body, 256))
	}
	// Cloudflare challenges come back 200 OK with text/html. Fail loud
	// rather than try to parse them as JSON.
	if contentType != "" && !strings.Contains(contentType, "json") {
		return nil, notice, fmt.Errorf("ChatGPT /models returned non-JSON response (content-type: %s): %s", contentType, bodyExcerpt(body, 256))
	}
	var parsed struct {
		Models []chatgptModelEntry `json:"models"`
	}
	if err := json.Unmarshal(body, &parsed); err != nil {
		return nil, notice, fmt.Errorf("parsing /models JSON (excerpt: %s): %w", bodyExcerpt(body, 256), err)
	}
	// Codex CLI puts only `list` models in its picker; `hide` and `none`
	// (automation-only) slugs stay out.
	models := slices.DeleteFunc(parsed.Models, func(model chatgptModelEntry) bool {
		return model.Visibility == nil || *model.Visibility != "list"
	})
	// Higher priority first; ties keep server order.
	slices.SortStableFunc(models, func(left, right chatgptModelEntry) int { return right.Priority - left.Priority })
	slugs := make([]string, 0, len(models))
	for _, model := range models {
		slugs = append(slugs, model.Slug)
	}
	slog.Info(fmt.Sprintf("ChatGPT /models returned %d slugs after filtering: %q", len(models), slugs))
	metadata := make([]ModelMetadata, 0, len(models))
	for _, model := range models {
		levels := []ReasoningLevelPreset{}
		for _, level := range model.SupportedReasoningLevels {
			levels = append(levels, ReasoningLevelPreset{Effort: level.Effort, Description: level.Description})
		}
		tiers := []ModelServiceTier{}
		for _, tier := range model.ServiceTiers {
			tiers = append(tiers, ModelServiceTier{ID: tier.ID, Name: tier.Name, Description: tier.Description})
		}
		metadata = append(metadata, ModelMetadata{
			ID:                       model.Slug,
			DefaultReasoningLevel:    model.DefaultReasoningLevel,
			SupportedReasoningLevels: levels,
			ServiceTiers:             tiers,
			// ChatGPT's `/models` exposes no context window.
		})
	}
	return metadata, notice, nil
}

type chatgptModelEntry struct {
	Slug       string  `json:"slug"`
	Visibility *string `json:"visibility"`
	// Priority is the server's ordering hint; higher is more prominent.
	Priority                 int     `json:"priority"`
	DefaultReasoningLevel    *string `json:"default_reasoning_level"`
	SupportedReasoningLevels []struct {
		Effort      string `json:"effort"`
		Description string `json:"description"`
	} `json:"supported_reasoning_levels"`
	ServiceTiers []struct {
		ID          string `json:"id"`
		Name        string `json:"name"`
		Description string `json:"description"`
	} `json:"service_tiers"`
}

// bodyExcerpt renders up to limit bytes of body for an error message.
func bodyExcerpt(body []byte, limit int) string {
	slice := body
	if len(body) > limit {
		slice = body[:limit]
	}
	text := strings.ReplaceAll(strings.ToValidUTF8(string(slice), "�"), "\n", " ")
	if len(body) > limit {
		return fmt.Sprintf("%s... (truncated, %d total bytes)", text, len(body))
	}
	return text
}

// resolveCodexCompatClientVersion is the client_version for model
// discovery and, when it had to fall back, a notice saying why.
func resolveCodexCompatClientVersion(ctx context.Context, client *http.Client) (string, *string) {
	codexCompatClientVersionCache.mu.Lock()
	cached, fetchedAt := codexCompatClientVersionCache.version, codexCompatClientVersionCache.fetchedAt
	codexCompatClientVersionCache.mu.Unlock()
	if cached != "" && time.Since(fetchedAt) < codexModelsManifestCacheTTL {
		slog.Debug("using cached Codex models manifest client_version for ChatGPT model discovery", "client_version", cached)
		return cached, nil
	}
	manifestCtx, cancel := context.WithTimeout(ctx, codexModelsManifestTimeout)
	defer cancel()
	version, err := fetchCodexModelsManifestClientVersion(manifestCtx, client)
	switch {
	case err == nil && version != nil:
		codexCompatClientVersionCache.mu.Lock()
		codexCompatClientVersionCache.version, codexCompatClientVersionCache.fetchedAt = *version, time.Now()
		codexCompatClientVersionCache.mu.Unlock()
		slog.Info("using Codex models manifest client_version for ChatGPT model discovery", "client_version", *version)
		return *version, nil
	case err == nil:
		return fallbackCodexCompatClientVersionFor("Codex models manifest did not advertise any visible minimal_client_version")
	case manifestCtx.Err() == context.DeadlineExceeded:
		slog.Warn("timed out fetching Codex models manifest; using fallback client_version",
			"timeout_secs", codexModelsManifestTimeout.Seconds(), "fallback", fallbackCodexCompatClientVersion)
		return fallbackCodexCompatClientVersionFor("Timed out fetching Codex models manifest")
	}
	slog.Warn("failed to fetch Codex models manifest; using fallback client_version",
		"e", err, "fallback", fallbackCodexCompatClientVersion)
	return fallbackCodexCompatClientVersionFor("Could not fetch Codex models manifest")
}

func fallbackCodexCompatClientVersionFor(reason string) (string, *string) {
	message := fmt.Sprintf("%s; using fallback client_version %s.", reason, fallbackCodexCompatClientVersion)
	slog.Warn(message)
	return fallbackCodexCompatClientVersion, &message
}

func fetchCodexModelsManifestClientVersion(ctx context.Context, client *http.Client) (*string, error) {
	response, err := sendWithRetries(ctx, "GET Codex models manifest", client, func(attemptCtx context.Context) (*http.Request, error) {
		request, err := http.NewRequestWithContext(attemptCtx, http.MethodGet, codexModelsManifestURL, nil)
		if err != nil {
			return nil, err
		}
		request.Header.Set("Accept", "application/json")
		return request, nil
	}, 0)
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	body, err := io.ReadAll(response.Body)
	if err != nil {
		return nil, withContext(err, "reading Codex models manifest response body")
	}
	if response.StatusCode < 200 || response.StatusCode > 299 {
		return nil, fmt.Errorf("Codex models manifest returned HTTP %s: %s", httpStatusText(response.StatusCode), bodyExcerpt(body, 256))
	}
	var parsed struct {
		Models []codexManifestModelEntry `json:"models"`
	}
	if err := json.Unmarshal(body, &parsed); err != nil {
		return nil, fmt.Errorf("parsing Codex models manifest JSON (excerpt: %s): %w", bodyExcerpt(body, 256), err)
	}
	return latestVisibleMinimalClientVersion(parsed.Models), nil
}

type codexManifestModelEntry struct {
	Visibility           *string `json:"visibility"`
	MinimalClientVersion *string `json:"minimal_client_version"`
}

func latestVisibleMinimalClientVersion(models []codexManifestModelEntry) *string {
	newest := func(keep func(*codexManifestModelEntry) bool) *string {
		var best *[3]uint64
		for index := range models {
			model := &models[index]
			if !keep(model) || model.MinimalClientVersion == nil {
				continue
			}
			version, ok := parseVersionTriple(*model.MinimalClientVersion)
			if ok && (best == nil || compareTriple(version, *best) > 0) {
				best = &version
			}
		}
		if best == nil {
			return nil
		}
		text := fmt.Sprintf("%d.%d.%d", best[0], best[1], best[2])
		return &text
	}
	if version := newest(func(model *codexManifestModelEntry) bool {
		return model.Visibility != nil && *model.Visibility == "list"
	}); version != nil {
		return version
	}
	// Be tolerant if Codex changes the manifest's visibility vocabulary: a
	// newer gate on an unknown visible-looking entry is safer than an older
	// hardcoded client_version.
	return newest(func(model *codexManifestModelEntry) bool {
		return model.Visibility == nil || *model.Visibility != "hide" && *model.Visibility != "none"
	})
}

func compareTriple(left, right [3]uint64) int {
	for index := range left {
		if left[index] != right[index] {
			if left[index] < right[index] {
				return -1
			}
			return 1
		}
	}
	return 0
}

func parseVersionTriple(input string) ([3]uint64, bool) {
	parts := strings.Split(strings.TrimSpace(input), ".")
	if len(parts) < 3 {
		return [3]uint64{}, false
	}
	major, err := strconv.ParseUint(parts[0], 10, 64)
	if err != nil {
		return [3]uint64{}, false
	}
	minor, err := strconv.ParseUint(parts[1], 10, 64)
	if err != nil {
		return [3]uint64{}, false
	}
	digits := parts[2]
	for index := 0; index < len(digits); index++ {
		if digits[index] < '0' || digits[index] > '9' {
			digits = digits[:index]
			break
		}
	}
	if digits == "" {
		return [3]uint64{}, false
	}
	patch, err := strconv.ParseUint(digits, 10, 64)
	if err != nil {
		return [3]uint64{}, false
	}
	return [3]uint64{major, minor, patch}, true
}

// Responses API request shape (the ChatGPT backend's).

type codexResponsesRequest struct {
	Model             string            `json:"model"`
	Instructions      *string           `json:"instructions,omitempty"`
	Input             []json.RawMessage `json:"input"`
	Tools             []json.RawMessage `json:"tools,omitempty"`
	ToolChoice        *string           `json:"tool_choice,omitempty"`
	ParallelToolCalls *bool             `json:"parallel_tool_calls,omitempty"`
	Stream            bool              `json:"stream"`
	// Store is always false: the backend rejects `store: true`.
	Store          bool                       `json:"store"`
	Include        []string                   `json:"include,omitempty"`
	PromptCacheKey *string                    `json:"prompt_cache_key,omitempty"`
	Reasoning      *reasoningConfig           `json:"reasoning,omitempty"`
	ServiceTier    *string                    `json:"service_tier,omitempty"`
	Text           *responsesTextConfigFormat `json:"text,omitempty"`
}

// responsesTextConfigFormat is `{"format": {"type": "json_schema", ...}}`.
type responsesTextConfigFormat struct {
	Format struct {
		Type   string          `json:"type"`
		Name   string          `json:"name"`
		Schema json.RawMessage `json:"schema"`
		Strict bool            `json:"strict"`
	} `json:"format"`
}

func responsesTextFormat(structured *StructuredOutputRequest) (*responsesTextConfigFormat, error) {
	if structured == nil {
		return nil, nil
	}
	format := nativeResponseFormat(structured)
	schema, err := jsonvalue.Marshal(format.Schema)
	if err != nil {
		return nil, err
	}
	text := &responsesTextConfigFormat{}
	text.Format.Type = "json_schema"
	text.Format.Name = format.Name
	text.Format.Schema = schema
	text.Format.Strict = format.Strict
	return text, nil
}

// responsesInputItem encodes one typed input item, its `type` first.
func responsesInputItem(members jsonvalue.Object) (json.RawMessage, error) {
	return jsonvalue.Marshal(members)
}

func responsesContentParts(parts []ChatContentPart, textType string) []any {
	values := make([]any, 0, len(parts))
	for _, part := range parts {
		switch part := part.(type) {
		case ChatContentPartText:
			values = append(values, jsonvalue.Object{{Key: "type", Value: textType}, {Key: "text", Value: part.Text}})
		case ChatContentPartImage:
			values = append(values, jsonvalue.Object{{Key: "type", Value: "input_image"}, {Key: "image_url", Value: part.ImageURL}})
		}
	}
	return values
}

func responsesTools(tools []ToolDefinition) ([]json.RawMessage, error) {
	if tools == nil {
		return nil, nil
	}
	encoded := make([]json.RawMessage, 0, len(tools))
	for _, tool := range tools {
		parameters, err := toJSONValue(tool.Function.Parameters)
		if err != nil {
			return nil, err
		}
		item, err := jsonvalue.Marshal(jsonvalue.Object{
			{Key: "type", Value: "function"},
			{Key: "name", Value: tool.Function.Name},
			{Key: "description", Value: tool.Function.Description},
			{Key: "parameters", Value: parameters},
		})
		if err != nil {
			return nil, err
		}
		encoded = append(encoded, item)
	}
	return encoded, nil
}

// reasoningInputItem replays a stored reasoning item field for field.
// `content` and `summary` are never omitted, even when empty: the backend
// rejects a reasoning item without `summary`.
func reasoningInputItem(item *CodexReasoningItem) jsonvalue.Object {
	content := item.Content
	if content == nil {
		content = []any{}
	}
	summary := item.Summary
	if summary == nil {
		summary = []any{}
	}
	members := jsonvalue.Object{{Key: "type", Value: "reasoning"}, {Key: "id", Value: item.ID}, {Key: "content", Value: content}}
	if item.EncryptedContent != nil {
		members = append(members, jsonvalue.Member{Key: "encrypted_content", Value: *item.EncryptedContent})
	}
	return append(members, jsonvalue.Member{Key: "summary", Value: summary})
}

// codexBuildResponsesRequest converts Chat-Completions-shaped messages into
// the typed Responses-API input. System messages collapse into the
// top-level instructions, concatenated in arrival order. The whole
// conversation goes out on every turn; the continuation's identity routes
// it to the server's prompt cache, and a stored reasoning item is replayed
// only for a message within the matched prefix.
func codexBuildResponsesRequest(
	model string,
	messages []ChatMessage,
	tools []ToolDefinition,
	reasoningEffort *string,
	serviceTier *string,
	structuredOutput *StructuredOutputRequest,
	continuation *responsesContinuation,
) ([]byte, error) {
	var instructionsParts []string
	input := []json.RawMessage{}
	push := func(members jsonvalue.Object) error {
		item, err := responsesInputItem(members)
		if err != nil {
			return err
		}
		input = append(input, item)
		return nil
	}
	for index := range messages {
		message := &messages[index]
		switch message.Role {
		case "system":
			if text := message.ContentText(); text != "" {
				instructionsParts = append(instructionsParts, text)
			} else {
				slog.Debug("dropping system message with no text content when building Responses input")
			}
		case "user":
			if len(message.Content) == 0 {
				slog.Debug("dropping user message with no content when building Responses input")
				continue
			}
			if err := push(jsonvalue.Object{
				{Key: "type", Value: "message"},
				{Key: "role", Value: "user"},
				{Key: "content", Value: responsesContentParts(message.Content, "input_text")},
			}); err != nil {
				return nil, err
			}
		case "assistant":
			if reasoning := message.CodexReasoning; reasoning != nil {
				if boundary := continuation.reasoningReplayBoundary; boundary != nil && index <= *boundary {
					if err := push(reasoningInputItem(reasoning)); err != nil {
						return nil, err
					}
				} else {
					slog.Debug(fmt.Sprintf("dropping codex reasoning item at message index %d: outside the matched prefix (edited or compacted history), so the server never produced it from the context we're about to send", index))
				}
			}
			switch {
			case message.ToolCalls != nil:
				for _, call := range message.ToolCalls {
					if err := push(jsonvalue.Object{
						{Key: "type", Value: "function_call"},
						{Key: "name", Value: call.Function.Name},
						{Key: "arguments", Value: normalizeRequestToolArguments(call.Function.Arguments, call.Function.Name)},
						{Key: "call_id", Value: call.ID},
					}); err != nil {
						return nil, err
					}
				}
			case len(message.Content) != 0:
				if err := push(jsonvalue.Object{
					{Key: "type", Value: "message"},
					{Key: "role", Value: "assistant"},
					{Key: "content", Value: []any{jsonvalue.Object{{Key: "type", Value: "output_text"}, {Key: "text", Value: message.ContentText()}}}},
				}); err != nil {
					return nil, err
				}
			default:
				slog.Debug("dropping assistant message with neither tool_calls nor content when building Responses input")
			}
		case "tool":
			if message.ToolCallID == nil {
				slog.Warn(fmt.Sprintf("dropping malformed tool message when building Responses input: tool_call_id_present=%v content_present=%v",
					message.ToolCallID != nil, len(message.Content) != 0))
				continue
			}
			if err := push(jsonvalue.Object{
				{Key: "type", Value: "function_call_output"},
				{Key: "call_id", Value: *message.ToolCallID},
				{Key: "output", Value: message.ContentText()},
			}); err != nil {
				return nil, err
			}
		default:
			slog.Debug(fmt.Sprintf("dropping unknown role %q when building Responses input", message.Role))
		}
	}
	encodedTools, err := responsesTools(tools)
	if err != nil {
		return nil, err
	}
	text, err := responsesTextFormat(structuredOutput)
	if err != nil {
		return nil, err
	}
	sessionID := continuation.identity.sessionID
	request := codexResponsesRequest{
		Model:          model,
		Input:          input,
		Tools:          encodedTools,
		Stream:         true,
		Store:          false,
		Include:        []string{reasoningEncryptedContentInclude},
		PromptCacheKey: &sessionID,
		ServiceTier:    serviceTier,
		Text:           text,
	}
	if encodedTools != nil {
		parallel, auto := true, "auto"
		request.ParallelToolCalls, request.ToolChoice = &parallel, &auto
	}
	if len(instructionsParts) != 0 {
		instructions := strings.Join(instructionsParts, "\n\n")
		request.Instructions = &instructions
	}
	if reasoningEffort != nil {
		request.Reasoning = &reasoningConfig{Effort: *reasoningEffort}
	}
	return marshalSerdeJSON(request)
}

// Responses API SSE parsing.

// responsesStreamEvent is the subset of a Responses stream event consumed
// here.
type responsesStreamEvent struct {
	Kind     *string             `json:"type"`
	Delta    *string             `json:"delta"`
	Item     json.RawMessage     `json:"item"`
	Response *responsesFinalBody `json:"response"`
}

// responsesFinalBody is the body of a completed, failed or incomplete
// response.
type responsesFinalBody struct {
	Error *struct {
		Code    *string `json:"code"`
		Message *string `json:"message"`
	} `json:"error"`
	Usage             *responsesUsage `json:"usage"`
	IncompleteDetails *struct {
		Reason *string `json:"reason"`
	} `json:"incomplete_details"`
}

type responsesUsage struct {
	InputTokens        uint64 `json:"input_tokens"`
	OutputTokens       uint64 `json:"output_tokens"`
	InputTokensDetails *struct {
		CachedTokens uint64 `json:"cached_tokens"`
	} `json:"input_tokens_details"`
	OutputTokensDetails *struct {
		ReasoningTokens uint64 `json:"reasoning_tokens"`
	} `json:"output_tokens_details"`
}

func (u *responsesUsage) intoUsage() TokenUsage {
	var cached, reasoning uint64
	if u.InputTokensDetails != nil {
		cached = u.InputTokensDetails.CachedTokens
	}
	if u.OutputTokensDetails != nil {
		reasoning = u.OutputTokensDetails.ReasoningTokens
	}
	sub := func(left, right uint64) uint64 {
		if right > left {
			return 0
		}
		return left - right
	}
	return TokenUsage{
		InputTokens:      sub(u.InputTokens, cached),
		OutputTokens:     sub(u.OutputTokens, reasoning),
		ThoughtTokens:    reasoning,
		CachedReadTokens: cached,
	}
}

// responsesOutputItem is an item of `response.output_item.done`: a
// message, a function call, a reasoning item, or another kind ignored.
type responsesOutputItem struct {
	kind             string
	role             *string
	texts            []string
	id               *string
	name             string
	arguments        string
	callID           *string
	reasoningID      string
	encryptedContent *string
	content          []any
	summary          []any
}

// parseResponsesOutputItem decodes an output item as serde's tagged enum
// does; false when it does not decode.
func parseResponsesOutputItem(raw json.RawMessage) (responsesOutputItem, bool) {
	value, err := jsonvalue.Parse(raw)
	if err != nil {
		return responsesOutputItem{}, false
	}
	kind, ok := jsonField(value, "type").(string)
	if !ok {
		return responsesOutputItem{}, false
	}
	optionalString := func(key string) (*string, bool) {
		member, present := jsonMember(value, key)
		if !present || member == nil {
			return nil, true
		}
		text, ok := member.(string)
		return &text, ok
	}
	values := func(key string) ([]any, bool) {
		member, present := jsonMember(value, key)
		if !present {
			return []any{}, true
		}
		list, ok := member.([]any)
		return list, ok
	}
	item := responsesOutputItem{kind: kind}
	switch kind {
	case "message":
		if item.role, ok = optionalString("role"); !ok {
			return item, false
		}
		content, ok := values("content")
		if !ok {
			return item, false
		}
		for _, part := range content {
			partKind, ok := jsonField(part, "type").(string)
			if !ok {
				return item, false
			}
			if partKind != "output_text" {
				continue
			}
			text := ""
			if member, present := jsonMember(part, "text"); present {
				if text, ok = member.(string); !ok {
					return item, false
				}
			}
			item.texts = append(item.texts, text)
		}
	case "function_call":
		name, nameOK := jsonField(value, "name").(string)
		arguments, argumentsOK := jsonField(value, "arguments").(string)
		if !nameOK || !argumentsOK {
			return item, false
		}
		item.name, item.arguments = name, arguments
		if item.id, ok = optionalString("id"); !ok {
			return item, false
		}
		if item.callID, ok = optionalString("call_id"); !ok {
			return item, false
		}
	case "reasoning":
		if member, present := jsonMember(value, "id"); present {
			if item.reasoningID, ok = member.(string); !ok {
				return item, false
			}
		}
		if item.encryptedContent, ok = optionalString("encrypted_content"); !ok {
			return item, false
		}
		if item.content, ok = values("content"); !ok {
			return item, false
		}
		if item.summary, ok = values("summary"); !ok {
			return item, false
		}
	}
	return item, true
}

// jsonField is serde_json's `value.get(key)`, nil when absent.
func jsonField(value any, key string) any {
	member, _ := jsonMember(value, key)
	return member
}

// codexDriveResponsesSSEStream drives a Responses-API SSE byte stream until
// `response.completed` or ctx is cancelled. Text deltas go to onToken as
// they arrive; function calls are collected from `response.output_item.done`
// events. The idle-timeout posture matches driveSSEStream.
func codexDriveResponsesSSEStream(ctx context.Context, body io.ReadCloser, onToken, onThought TokenSink, idle IdleTimeouts) (LlmResponse, error) {
	done := make(chan struct{})
	defer close(done)
	defer body.Close()
	reads := readStream(body, done)
	emitToken := func(text string) {
		if onToken != nil {
			onToken(text)
		}
	}
	var fullText strings.Builder
	var toolCalls []ToolCall
	var rawBuffer []byte
	deadline := time.NewTimer(idle.FirstProgress)
	defer deadline.Stop()
	sawProgress := false
	completed := false
	var failure error
	usage := TokenUsage{}
	// At most one reasoning item is expected per response, ahead of its
	// text or function calls; a second overwrites the first with a warning.
	var pendingReasoning *CodexReasoningItem
	// deltasReceived tells "no deltas yet" from "deltas that were empty",
	// so the output_item.done backfill never duplicates the text.
	deltasReceived := false

loop:
	for {
		select {
		case <-ctx.Done():
			slog.Info("Codex streaming cancelled by client")
			break loop
		case <-deadline.C:
			if sawProgress {
				return nil, RetryableLlmErrorWith(
					fmt.Sprintf("Codex Responses stream stalled mid-stream for %ds; aborting", int64(idle.InterChunk/time.Second)),
					RetryableLlmErrorFast("Codex Responses stream stalled mid-stream"))
			}
			return nil, RetryableLlmErrorWith(
				fmt.Sprintf("Codex Responses stream made no first token for %ds; aborting", int64(idle.FirstProgress/time.Second)),
				RetryableLlmErrorFast("Codex Responses stream made no first token"))
		case read := <-reads:
			eofAfterBuffer := false
			switch {
			case read.err != nil && !errors.Is(read.err, io.EOF):
				return nil, retryableLlmContext(read.err, "Codex stream read error", RetryableLlmErrorFast("Codex stream read error"))
			case read.err == nil:
				rawBuffer = append(rawBuffer, read.data...)
			case len(rawBuffer) == 0:
				break loop
			default:
				rawBuffer = append(rawBuffer, '\n')
				eofAfterBuffer = true
			}
			madeProgress := false
		lines:
			for {
				position := bytes.IndexByte(rawBuffer, '\n')
				if position < 0 {
					break
				}
				lineBytes := rawBuffer[:position+1]
				rawBuffer = rawBuffer[position+1:]
				line := strings.TrimSpace(strings.ToValidUTF8(string(lineBytes), "�"))
				// Blank lines, SSE comments, and `event:` markers carry
				// nothing: the JSON rides on `data:` lines with its own
				// `type` discriminator.
				if line == "" || strings.HasPrefix(line, ":") || strings.HasPrefix(line, "event:") {
					continue
				}
				var data string
				if stripped, ok := strings.CutPrefix(line, "data: "); ok {
					data = strings.TrimSpace(stripped)
				} else if stripped, ok := strings.CutPrefix(line, "data:"); ok {
					data = strings.TrimSpace(stripped)
				} else {
					continue
				}
				if data == "[DONE]" {
					continue
				}
				var event responsesStreamEvent
				if err := json.Unmarshal([]byte(data), &event); err != nil || event.Kind == nil {
					slog.Debug("skipping unparseable Responses SSE chunk: " + data)
					continue
				}
				switch *event.Kind {
				case "response.output_text.delta":
					if event.Delta != nil {
						madeProgress = true
						deltasReceived = true
						emitToken(*event.Delta)
						fullText.WriteString(*event.Delta)
					}
				case "response.output_item.done":
					if len(event.Item) == 0 {
						continue
					}
					item, ok := parseResponsesOutputItem(event.Item)
					if !ok {
						continue
					}
					switch item.kind {
					case "message":
						// Some servers send the whole assistant message via
						// output_item.done without deltas; backfill only then.
						if item.role != nil && *item.role == "assistant" && !deltasReceived {
							for _, text := range item.texts {
								emitToken(text)
								fullText.WriteString(text)
							}
						}
						madeProgress = true
					case "function_call":
						// `call_id` is the persistent identifier the tool
						// result correlates on; `id` is the server item id.
						resolvedID := fmt.Sprintf("call_%d", len(toolCalls))
						if item.callID != nil {
							resolvedID = *item.callID
						} else if item.id != nil {
							resolvedID = *item.id
						}
						arguments, err := normalizeStreamedToolArguments(resolvedID, item.name, item.arguments, "Codex Responses SSE")
						if err != nil {
							return nil, err
						}
						toolCalls = append(toolCalls, ToolCall{ID: resolvedID, Type: "function", Function: FunctionCall{Name: item.name, Arguments: arguments}})
						madeProgress = true
					case "reasoning":
						if pendingReasoning != nil {
							slog.Warn("Codex response emitted more than one reasoning item; keeping only the latest one for replay")
						}
						pendingReasoning = &CodexReasoningItem{
							ID:               item.reasoningID,
							EncryptedContent: item.encryptedContent,
							Content:          item.content,
							Summary:          item.summary,
						}
						madeProgress = true
					}
				case "response.completed":
					if event.Response != nil && event.Response.Usage != nil {
						usage = event.Response.Usage.intoUsage()
					}
					completed = true
					break lines
				case "response.failed":
					message := "unknown error"
					if event.Response != nil && event.Response.Error != nil {
						code, text := "unknown", ""
						if event.Response.Error.Code != nil {
							code = *event.Response.Error.Code
						}
						if event.Response.Error.Message != nil {
							text = *event.Response.Error.Message
						}
						message = code + ": " + text
					}
					failure = RetryableLlmErrorForResponsesFailure("Codex Responses stream failed: "+message, message)
					completed = true
					break lines
				case "response.incomplete":
					if event.Response == nil {
						slog.Warn("Codex Responses stream ended with incomplete response without body")
						continue
					}
					if event.Response.Usage != nil {
						usage = event.Response.Usage.intoUsage()
					}
					reason := "unknown"
					if details := event.Response.IncompleteDetails; details != nil && details.Reason != nil {
						reason = *details.Reason
					}
					if reason == "max_output_tokens" {
						if strings.TrimSpace(fullText.String()) == "" && len(toolCalls) == 0 {
							failure = OutputBudgetExhaustedError{}
						} else {
							slog.Warn("Codex Responses stream became incomplete after emitting output; returning truncated content", "reason", reason)
						}
						completed = true
						break lines
					}
					slog.Warn("Codex Responses stream ended with incomplete response", "reason", reason)
				case "response.reasoning_text.delta", "response.reasoning_summary_text.delta":
					// Chain-of-thought deltas go to the thought sink.
					if event.Delta != nil {
						madeProgress = true
						if onThought != nil {
							onThought(*event.Delta)
						}
					}
				default:
					// Other events count as activity so the idle timer
					// doesn't fire mid-think.
					madeProgress = true
				}
			}
			if completed {
				break loop
			}
			if madeProgress {
				sawProgress = true
				if !deadline.Stop() {
					select {
					case <-deadline.C:
					default:
					}
				}
				deadline.Reset(idle.InterChunk)
			}
			if eofAfterBuffer {
				break loop
			}
		}
	}

	if failure != nil {
		return nil, failure
	}
	if ctx.Err() != nil {
		return LlmResponseText{Text: fullText.String(), Usage: usage, CodexReasoning: pendingReasoning}, nil
	}
	if !completed {
		return nil, NewIncompleteStreamError("Codex Responses SSE", "response.completed")
	}
	if len(toolCalls) == 0 {
		return LlmResponseText{Text: fullText.String(), Usage: usage, CodexReasoning: pendingReasoning}, nil
	}
	return LlmResponseToolCalls{Text: fullText.String(), Calls: toolCalls, Usage: usage, CodexReasoning: pendingReasoning}, nil
}
