// Port of the tests in mj-accounts/src/muse_usage.rs.

package accounts

import (
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type museRequest struct {
	method        string
	authorization string
	body          string
}

// museServer is the mint endpoint: it records each request and answers
// with the configured status and body, after the configured delay.
type museServer struct {
	mu       sync.Mutex
	requests []museRequest
	status   int
	body     string
	delay    time.Duration
}

func (s *museServer) ServeHTTP(response http.ResponseWriter, request *http.Request) {
	if request.URL.Path != "/muse-code/key" || request.Method != http.MethodPost {
		http.NotFound(response, request)
		return
	}
	if s.delay > 0 {
		time.Sleep(s.delay)
	}
	body, _ := io.ReadAll(request.Body)
	s.mu.Lock()
	s.requests = append(s.requests, museRequest{request.Method, request.Header.Get("Authorization"), string(body)})
	status, answer := s.status, s.body
	s.mu.Unlock()
	response.Header().Set("Content-Type", "application/json")
	response.WriteHeader(status)
	io.WriteString(response, answer)
}

func (s *museServer) respond(status int, body string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.status, s.body = status, body
}

func (s *museServer) seen() []museRequest {
	s.mu.Lock()
	defer s.mu.Unlock()
	return slices.Clone(s.requests)
}

func spawnMuseServer(t *testing.T, state *museServer) string {
	server := httptest.NewServer(state)
	t.Cleanup(server.Close)
	return server.URL
}

func museEndpoints(baseURL string) core.MuseQuotaEndpoints {
	return core.MuseQuotaEndpoints{Mint: baseURL, Auth: baseURL}
}

func writeMuseCredentials(t *testing.T, home, token string) {
	t.Helper()
	body, err := jsonvalue.Marshal(jsonvalue.Object{{Key: "providers", Value: jsonvalue.Object{
		{Key: "meta", Value: jsonvalue.Object{{Key: "access_token", Value: token}}},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, "auth.json"), body, 0o666); err != nil {
		t.Fatal(err)
	}
}

func museParse(t *testing.T, text string) any {
	t.Helper()
	value, err := jsonvalue.Parse([]byte(text))
	if err != nil {
		t.Fatal(err)
	}
	return value
}

func TestPreservesOptionalWindowsAndResetsButRejectsIncompleteUsage(t *testing.T) {
	usage, err := museUsageParse(museParse(t, `{"subs_usage": {"weekly": {"used_percent": 25}, "window": null}}`))
	if err != nil {
		t.Fatal(err)
	}
	open, ok := usage.(MuseUsageOpen)
	if !ok {
		t.Fatalf("usage was reported: %#v", usage)
	}
	windows := open.Value
	if len(windows) != 1 || windows[0].Label != "Week" || windows[0].RemainingPercent != 75 || windows[0].ResetsAt != nil {
		t.Fatalf("windows = %#v", windows)
	}
	if _, err := museUsageParse(museParse(t, `{}`)); err == nil || err.Error() != "Muse subscription usage is unavailable" {
		t.Fatalf("error = %v", err)
	}
	for _, window := range []string{`{}`, `{"used_percent": 1}`, `{"window_duration_mins": 300}`} {
		if _, err := museUsageParse(museParse(t, `{"subs_usage": {"weekly": {"used_percent": 25}, "window": `+window+`}}`)); err == nil {
			t.Fatalf("%s: an incomplete window was accepted", window)
		}
	}
}

func TestRejectsMissingInvalidAndOversizedCredentials(t *testing.T) {
	home := t.TempDir()
	if _, err := readLogin(t.Context(), home); err == nil || !strings.Contains(err.Error(), "login required") {
		t.Fatalf("error = %v", err)
	}
	for _, bytes := range [][]byte{
		[]byte("not-json"),
		[]byte("{}"),
		[]byte(strings.Repeat(" ", core.MaxCredentialBytes+1)),
	} {
		if err := os.WriteFile(filepath.Join(home, "auth.json"), bytes, 0o666); err != nil {
			t.Fatal(err)
		}
		if _, err := readLogin(t.Context(), home); err == nil {
			t.Fatalf("%.20q was accepted", bytes)
		}
	}
}

func TestPostsProfileTokenAndNativeBodyWithoutPersistingResponse(t *testing.T) {
	state := &museServer{status: http.StatusOK, body: `{"subs_usage":{"weekly":{"used_percent":1,"resets_at":1789344000}}}`}
	baseURL := spawnMuseServer(t, state)
	first := t.TempDir()
	second := t.TempDir()
	writeMuseCredentials(t, first, "first-token")
	writeMuseCredentials(t, second, "second-token")

	if _, err := MuseUsageQuery(t.Context(), first, map[string]string{"TBH_MINT_BASE_URL": baseURL}); err != nil {
		t.Fatal(err)
	}
	if _, err := MuseUsageQuery(t.Context(), second, map[string]string{"TBH_MINT_BASE_URL": baseURL}); err != nil {
		t.Fatal(err)
	}
	for home, token := range map[string]string{first: "first-token", second: "second-token"} {
		stored, err := os.ReadFile(filepath.Join(home, "auth.json"))
		if err != nil || !strings.Contains(string(stored), token) {
			t.Fatalf("auth.json = %s, %v", stored, err)
		}
	}
	requests := state.seen()
	if len(requests) != 2 {
		t.Fatalf("requests = %#v", requests)
	}
	if requests[0].method != http.MethodPost || requests[0].authorization != "Bearer first-token" ||
		requests[1].authorization != "Bearer second-token" || requests[0].body != `{"onboard":false}` {
		t.Fatalf("requests = %#v", requests)
	}
}

func museUsageBody() string {
	return `{"subs_usage":{"weekly":{"used_percent":1,"resets_at":1789344000}}}`
}

func museOpen(t *testing.T, usage MuseUsage) []MuseUsageWindow {
	t.Helper()
	open, ok := usage.(MuseUsageOpen)
	if !ok {
		t.Fatalf("expected an open window: %#v", usage)
	}
	return open.Value
}

func museNoteIs(t *testing.T, note *string, want *string) {
	t.Helper()
	if (note == nil) != (want == nil) || (note != nil && *note != *want) {
		t.Fatalf("note = %v; want %v", note, want)
	}
}

// Hard-won: fd350b04: a live Muse 429 outage was amplified by duplicate key-mint calls and the dashboard discarded its last good reading.
func TestGateServesLastReadingWhileRateLimitedAndDedupesRefreshes(t *testing.T) {
	home := t.TempDir()
	writeMuseCredentials(t, home, "profile-token")
	state := &museServer{status: http.StatusOK, body: museUsageBody()}
	baseURL := spawnMuseServer(t, state)
	gate := &museGate{}
	start := time.Unix(1_700_000_000, 0)

	report, err := queryGated(t.Context(), home, museEndpoints(baseURL), museUsageRequestTimeout, gate, start)
	if err != nil {
		t.Fatal(err)
	}
	if windows := museOpen(t, report.Usage); windows[0].RemainingPercent != 99 {
		t.Fatalf("windows = %#v", windows)
	}
	museNoteIs(t, report.Note, nil)

	// A second caller inside the minimum interval reuses the reading.
	report, err = queryGated(t.Context(), home, museEndpoints(baseURL), museUsageRequestTimeout, gate, start.Add(60*time.Second))
	if err != nil {
		t.Fatal(err)
	}
	museNoteIs(t, report.Note, nil)
	if count := len(state.seen()); count != 1 {
		t.Fatalf("%d requests", count)
	}

	// After the interval a 429 keeps the last windows and marks the note.
	state.respond(http.StatusTooManyRequests, "{}")
	limited := start.Add(minRefreshInterval + time.Second)
	report, err = queryGated(t.Context(), home, museEndpoints(baseURL), museUsageRequestTimeout, gate, limited)
	if err != nil {
		t.Fatal(err)
	}
	if windows := museOpen(t, report.Usage); windows[0].RemainingPercent != 99 {
		t.Fatalf("windows = %#v", windows)
	}
	museNoteIs(t, report.Note, quotaStr("rate limited"))
	if count := len(state.seen()); count != 2 {
		t.Fatalf("%d requests", count)
	}

	// No further network call until the backoff elapses.
	report, err = queryGated(t.Context(), home, museEndpoints(baseURL), museUsageRequestTimeout, gate, limited.Add(initialRateLimitBackoff-time.Second))
	if err != nil {
		t.Fatal(err)
	}
	museNoteIs(t, report.Note, quotaStr("rate limited"))
	if count := len(state.seen()); count != 2 {
		t.Fatalf("%d requests", count)
	}

	// Once it elapses the endpoint is tried again, and a success clears both
	// the note and the consecutive-429 count.
	state.respond(http.StatusOK, museUsageBody())
	recovered := limited.Add(initialRateLimitBackoff)
	report, err = queryGated(t.Context(), home, museEndpoints(baseURL), museUsageRequestTimeout, gate, recovered)
	if err != nil {
		t.Fatal(err)
	}
	museNoteIs(t, report.Note, nil)
	if count := len(state.seen()); count != 3 {
		t.Fatalf("%d requests", count)
	}
	gate.mu.Lock()
	consecutive := gate.entries[gateKey(home)].consecutiveRateLimits
	gate.mu.Unlock()
	if consecutive != 0 {
		t.Fatalf("consecutive rate limits = %d", consecutive)
	}
}

// Hard-won: fd350b04: the live Muse 429 case without cached usage was shown as unavailable and retried on every poll.
func TestRateLimitWithoutAPriorReadingReportsTheRateLimitedMarker(t *testing.T) {
	home := t.TempDir()
	writeMuseCredentials(t, home, "profile-token")
	state := &museServer{status: http.StatusTooManyRequests, body: "{}"}
	baseURL := spawnMuseServer(t, state)
	gate := &museGate{}
	start := time.Unix(1_700_000_000, 0)

	for _, at := range []time.Time{start, start.Add(60 * time.Second)} {
		_, err := queryGated(t.Context(), home, museEndpoints(baseURL), museUsageRequestTimeout, gate, at)
		if err == nil || !strings.HasPrefix(err.Error(), "rate limited") {
			t.Fatalf("error = %v", err)
		}
	}
	if count := len(state.seen()); count != 1 {
		t.Fatalf("%d requests", count)
	}
}

// Hard-won: fd350b04: Muse’s live rate limit required honoring Retry-After to stop repeated key-mint requests.
func TestRetryAfterSecondsSetTheNextAllowedCheck(t *testing.T) {
	home := t.TempDir()
	writeMuseCredentials(t, home, "profile-token")
	server := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		response.Header().Set("Retry-After", "120")
		response.WriteHeader(http.StatusTooManyRequests)
		io.WriteString(response, "{}")
	}))
	defer server.Close()
	gate := &museGate{}
	start := time.Unix(1_700_000_000, 0)

	if _, err := queryGated(t.Context(), home, museEndpoints(server.URL), museUsageRequestTimeout, gate, start); err == nil {
		t.Fatal("a 429 without a reading is an error")
	}
	gate.mu.Lock()
	next := gate.entries[gateKey(home)].nextAllowedAt
	gate.mu.Unlock()
	if next == nil || !next.Equal(start.Add(120*time.Second)) {
		t.Fatalf("next allowed = %v", next)
	}
}

// Hard-won: fd350b04: the live Muse 429 fix needed bounded clock-based backoff and Retry-After date/seconds handling.
func TestRetryAfterAcceptsSecondsAndHTTPDatesAndBackoffIsCapped(t *testing.T) {
	now := time.Unix(1_700_000_000, 0)
	if wait := parseRetryAfter("30", now); wait == nil || *wait != 30*time.Second {
		t.Fatalf("wait = %v", wait)
	}
	if wait := parseRetryAfter("Tue, 14 Nov 2023 22:23:20 GMT", now); wait == nil || *wait != 600*time.Second {
		t.Fatalf("wait = %v", wait)
	}
	if wait := parseRetryAfter("soon", now); wait != nil {
		t.Fatalf("wait = %v", *wait)
	}
	if backoff := exponentialBackoff(0); backoff != initialRateLimitBackoff {
		t.Fatalf("backoff = %v", backoff)
	}
	if backoff := exponentialBackoff(1); backoff != initialRateLimitBackoff*2 {
		t.Fatalf("backoff = %v", backoff)
	}
	if backoff := exponentialBackoff(9); backoff != maxRateLimitBackoff {
		t.Fatalf("backoff = %v", backoff)
	}
}

func TestReturnsAuthStatusAndTimeoutErrorsWithoutResponseDetails(t *testing.T) {
	home := t.TempDir()
	writeMuseCredentials(t, home, "secret-token")
	rejecting := &museServer{status: http.StatusUnauthorized, body: "secret-payload"}
	_, err := queryWithTimeout(t.Context(), home, museEndpoints(spawnMuseServer(t, rejecting)), time.Second)
	if err == nil || err.Error() != loginExpired {
		t.Fatalf("error = %v", err)
	}

	delayed := &museServer{status: http.StatusOK, body: "{}", delay: time.Second}
	_, err = queryWithTimeout(t.Context(), home, museEndpoints(spawnMuseServer(t, delayed)), 20*time.Millisecond)
	if err == nil || err.Error() != "Muse usage request timed out" {
		t.Fatalf("error = %v", err)
	}
}

// spawnMeta is a Meta endpoint pair: the mint accepts only `fresh-access`,
// and the token endpoint records each refresh it is asked for.
func spawnMeta(t *testing.T, refreshes, mintedWith *[]string) string {
	var mu sync.Mutex
	mux := http.NewServeMux()
	mux.HandleFunc("POST /muse-code/key", func(response http.ResponseWriter, request *http.Request) {
		authorization := request.Header.Get("Authorization")
		mu.Lock()
		*mintedWith = append(*mintedWith, authorization)
		mu.Unlock()
		status := http.StatusUnauthorized
		if strings.HasSuffix(authorization, "-access") && authorization != "Bearer stale-access" {
			status = http.StatusOK
		}
		response.WriteHeader(status)
		io.WriteString(response, museUsageBody())
	})
	mux.HandleFunc("POST /oidc/device/token/", func(response http.ResponseWriter, request *http.Request) {
		body, _ := io.ReadAll(request.Body)
		mu.Lock()
		*refreshes = append(*refreshes, string(body))
		mu.Unlock()
		io.WriteString(response, `{"access_token":"fresh-access","refresh_token":"fresh-refresh","expires_in":3600,"token_type":"Bearer"}`)
	})
	server := httptest.NewServer(mux)
	t.Cleanup(server.Close)
	return server.URL
}

// writeMuseLogin writes the `auth.json` shape `muse login` writes (Muse
// 1.4.12).
func writeMuseLogin(t *testing.T, home, accessToken string, expiresAt int64) {
	t.Helper()
	body, err := museMarshalPretty(jsonvalue.Object{
		{Key: "schema_version", Value: int64(1)},
		{Key: "providers", Value: jsonvalue.Object{{Key: "meta", Value: jsonvalue.Object{
			{Key: "access_token", Value: accessToken},
			{Key: "api_base_url", Value: "https://api.meta.ai"},
			{Key: "api_key", Value: "minted-key"},
			{Key: "expires_at", Value: expiresAt},
			{Key: "mechanism", Value: "oauth"},
			{Key: "obtained_via", Value: "device_code"},
			{Key: "refresh_token", Value: "old-refresh"},
		}}}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, "auth.json"), body, 0o666); err != nil {
		t.Fatal(err)
	}
}

func TestARejectedLoginIsRefreshedOnceUnderMusesLockAndTheQueryRetried(t *testing.T) {
	var refreshes, mintedWith []string
	baseURL := spawnMeta(t, &refreshes, &mintedWith)
	farFuture := int64(core.EpochSeconds()) + 24*60*60

	// The recorded expiry says the token is good; the endpoint disagrees.
	home := t.TempDir()
	writeMuseLogin(t, home, "stale-access", farFuture)
	usage, err := queryWithTimeout(t.Context(), home, museEndpoints(baseURL), museUsageRequestTimeout)
	if err != nil {
		t.Fatal(err)
	}
	if windows := museOpen(t, usage); windows[0].RemainingPercent != 99 {
		t.Fatalf("windows = %#v", windows)
	}
	if !slices.Equal(mintedWith, []string{"Bearer stale-access", "Bearer fresh-access"}) {
		t.Fatalf("minted with %q", mintedWith)
	}
	if want := []string{fmt.Sprintf("grant_type=refresh_token&refresh_token=old-refresh&client_id=%s", oauthClientID)}; !slices.Equal(refreshes, want) {
		t.Fatalf("refreshes = %q", refreshes)
	}
	stored, err := os.ReadFile(filepath.Join(home, "auth.json"))
	if err != nil {
		t.Fatal(err)
	}
	saved := museParse(t, string(stored))
	meta, _ := quotaJSONPointer(saved, "/providers/meta")
	for key, want := range map[string]any{
		"access_token":  "fresh-access",
		"refresh_token": "fresh-refresh",
		"api_key":       "minted-key",
		"mechanism":     "oauth",
	} {
		if got := quotaJSONIndex(meta, key); got != want {
			t.Fatalf("%s = %v", key, got)
		}
	}
	if version := quotaJSONIndex(saved, "schema_version"); version != int64(1) {
		t.Fatalf("schema_version = %v", version)
	}
	if expires, ok := quotaJSONI64(quotaJSONIndex(meta, "expires_at")); !ok || expires <= int64(core.EpochSeconds()) {
		t.Fatalf("expires_at = %v", quotaJSONIndex(meta, "expires_at"))
	}

	// Muse renews the login while the daemon waits for its lock: the daemon
	// uses that login and never spends the refresh token Muse already
	// spent.
	refreshes, mintedWith = nil, nil
	baseURL = spawnMeta(t, &refreshes, &mintedWith)
	raced := t.TempDir()
	writeMuseLogin(t, raced, "stale-access", 1_700_000_000)
	lockFile, err := os.OpenFile(filepath.Join(raced, ".auth.json.lock"), os.O_CREATE|os.O_WRONLY, 0o666)
	if err != nil {
		t.Fatal(err)
	}
	muse, err := core.AcquireFileLockCancellable(lockFile, func() bool { return false })
	if err != nil {
		t.Fatal(err)
	}
	type answer struct {
		token string
		err   error
	}
	query := make(chan answer, 1)
	go func() {
		token, err := freshAccessToken(t.Context(), &http.Client{}, raced, baseURL, nil)
		query <- answer{token, err}
	}()
	select {
	case <-query:
		t.Fatal("the daemon must wait for Muse's lock")
	case <-time.After(300 * time.Millisecond):
	}
	writeMuseLogin(t, raced, "muse-renewed-access", farFuture)
	if err := muse.Release(); err != nil {
		t.Fatal(err)
	}
	lockFile.Close()
	result := <-query
	if result.err != nil || result.token != "muse-renewed-access" {
		t.Fatalf("token = %q, %v", result.token, result.err)
	}
	if len(refreshes) != 0 || len(mintedWith) != 0 {
		t.Fatalf("refreshes = %q, minted with %q", refreshes, mintedWith)
	}
}
