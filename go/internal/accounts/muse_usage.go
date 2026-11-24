// Port of mj-accounts/src/muse_usage.rs.

// Muse subscription usage queried from the native mint endpoint.
//
// Muse keeps the access token in each profile's `auth.json`. The endpoint is
// intentionally queried directly here so each profile stays isolated and no
// minted API key is persisted or passed through the rest of the controller.
//
// The daemon keeps that login fresh itself, as it does for Kimi: a token
// near its expiry, or one the endpoint rejects, is refreshed under the lock
// Muse takes for `auth.json`, so the quota does not depend on Muse having
// run.
//
// While no five-hour window is open, Meta reports no usage at all, weekly
// included. That idle state is a reading of its own (MuseUsageIdle);
// idleWindows carries the weekly figure across it from stored probes.

package accounts

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"math"
	"net/http"
	"net/mail"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const mintPath = "/muse-code/key"

const museUsageRequestTimeout = 10 * time.Second

// tokenPath and oauthClientID are the token endpoint and public client id
// `muse login` uses (observed on the wire from Muse 1.4.12). A refresh is
// the standard `refresh_token` grant against the same endpoint.
const (
	tokenPath     = "/oidc/device/token/"
	oauthClientID = "1031625952748946"
)

// refreshAhead is how long before the recorded expiry a login is refreshed.
// It spans two quota polls, so the credential sync hands the new login to
// running sessions before their copies run out and have to spend the same
// refresh token themselves.
const refreshAhead = 20 * time.Minute

// loginExpired is the dashboard's marker for a rejected login; it renders
// this text as is.
const loginExpired = "login expired"

// minRefreshInterval: the mint endpoint is the only source of Muse usage and
// it is rate limited per account, so every caller in this process shares one
// reading rather than minting a key of its own.
const minRefreshInterval = 10 * time.Minute

// initialRateLimitBackoff is the first wait after a 429 that carries no
// `Retry-After`.
const initialRateLimitBackoff = 30 * time.Minute

const maxRateLimitBackoff = 6 * time.Hour

// RateLimitedPrefix lets the dashboard render a rate-limited profile
// honestly instead of calling it unavailable.
const RateLimitedPrefix = "rate limited"

type MuseUsageWindow struct {
	Label            string
	RemainingPercent uint8
	DurationSeconds  *int64
	ResetsAt         *int64
}

// MuseUsage is what Meta's key endpoint said about the subscription's
// usage.
type MuseUsage interface {
	isMuseUsage()
}

// MuseUsageOpen: a five-hour window is open, and Meta reported the windows
// it has.
type MuseUsageOpen struct{ Value []MuseUsageWindow }

// MuseUsageIdle: the subscription is active and no five-hour window is open.
// Meta then leaves `subs_usage` out entirely, the weekly window included,
// until the next request opens a window. Muse's own `muse usage` reports the
// same state as `server_sent_no_usage`.
type MuseUsageIdle struct{}

func (MuseUsageOpen) isMuseUsage() {}
func (MuseUsageIdle) isMuseUsage() {}

// MuseUsageReport is one usage reading, plus a short note when it is the
// last good reading served while Meta is rate limiting this account.
type MuseUsageReport struct {
	Usage MuseUsage
	Note  *string
}

// museRateLimited is a 429 from the mint endpoint, with whatever wait it
// asked for (Rust's private `RateLimited`). Carried as *museRateLimited.
type museRateLimited struct {
	retryAfter *time.Duration
}

func (*museRateLimited) Error() string {
	return "Muse usage request returned HTTP 429 Too Many Requests"
}

type gateEntry struct {
	// reading is nil until a reading was taken.
	reading               MuseUsage
	fetchedAt             *time.Time
	nextAllowedAt         *time.Time
	consecutiveRateLimits uint32
	rateLimitReason       *string
}

// museGate is the per-home rate-limit and minimum-interval state (Rust's
// `Mutex<HashMap<PathBuf, GateEntry>>`).
type museGate struct {
	mu      sync.Mutex
	entries map[string]*gateEntry
}

// entry is key's entry, created empty when it has none. Call it with the
// gate locked.
func (g *museGate) entry(key string) *gateEntry {
	if g.entries == nil {
		g.entries = map[string]*gateEntry{}
	}
	entry, ok := g.entries[key]
	if !ok {
		entry = &gateEntry{}
		g.entries[key] = entry
	}
	return entry
}

var museSharedGate = &museGate{entries: map[string]*gateEntry{}}

func sharedGate() *museGate {
	return museSharedGate
}

// MuseUsageQuery queries the Muse subscription usage for one profile.
func MuseUsageQuery(ctx context.Context, home string, environment map[string]string) (MuseUsageReport, error) {
	return queryGated(ctx, home, core.MuseQuotaEndpointsOf(environment), museUsageRequestTimeout, sharedGate(), time.Now())
}

// queryGated is the rate-limit and minimum-interval gate above the network
// step. now is supplied by the caller so tests can drive the clock without
// sleeping.
func queryGated(ctx context.Context, home string, endpoints core.MuseQuotaEndpoints, timeout time.Duration, gate *museGate, now time.Time) (MuseUsageReport, error) {
	key := gateKey(home)
	cached, err := cachedReport(gate, key, now)
	if err != nil {
		return MuseUsageReport{}, err
	}
	if cached != nil {
		return *cached, nil
	}
	usage, err := queryWithTimeout(ctx, home, endpoints, timeout)
	if err == nil {
		gate.mu.Lock()
		defer gate.mu.Unlock()
		entry := gate.entry(key)
		fetchedAt := now
		entry.reading = usage
		entry.fetchedAt = &fetchedAt
		entry.nextAllowedAt = nil
		entry.consecutiveRateLimits = 0
		entry.rateLimitReason = nil
		return MuseUsageReport{Usage: usage}, nil
	}
	var limited *museRateLimited
	if !errors.As(err, &limited) {
		// A transient failure neither clears the last reading nor blocks
		// the next attempt.
		return MuseUsageReport{}, err
	}
	gate.mu.Lock()
	defer gate.mu.Unlock()
	entry := gate.entry(key)
	backoff := exponentialBackoff(entry.consecutiveRateLimits)
	if limited.retryAfter != nil {
		backoff = *limited.retryAfter
	}
	nextAllowedAt := now.Add(backoff)
	if entry.consecutiveRateLimits < math.MaxUint32 {
		entry.consecutiveRateLimits++
	}
	reason := rateLimitReason(nextAllowedAt)
	entry.nextAllowedAt = &nextAllowedAt
	entry.rateLimitReason = &reason
	if entry.reading != nil {
		note := RateLimitedPrefix
		return MuseUsageReport{Usage: entry.reading, Note: &note}, nil
	}
	return MuseUsageReport{}, errors.New(reason)
}

// cachedReport is the cached answer, when the gate says no network call is
// due. A nil report and nil error mean the caller should query the endpoint.
func cachedReport(gate *museGate, key string, now time.Time) (*MuseUsageReport, error) {
	gate.mu.Lock()
	defer gate.mu.Unlock()
	entry, ok := gate.entries[key]
	if !ok {
		return nil, nil
	}
	if entry.nextAllowedAt != nil && now.Before(*entry.nextAllowedAt) {
		if entry.reading != nil {
			note := RateLimitedPrefix
			return &MuseUsageReport{Usage: entry.reading, Note: &note}, nil
		}
		reason := ""
		if entry.rateLimitReason != nil {
			reason = *entry.rateLimitReason
		}
		return nil, errors.New(reason)
	}
	fresh := false
	if entry.fetchedAt != nil {
		if age := now.Sub(*entry.fetchedAt); age >= 0 && age < minRefreshInterval {
			fresh = true
		}
	}
	if fresh && entry.reading != nil {
		return &MuseUsageReport{Usage: entry.reading}, nil
	}
	return nil, nil
}

// gateKey is home with its symlinks resolved, or home itself when that
// fails (Rust's `canonicalize().unwrap_or_else(...)`).
func gateKey(home string) string {
	resolved, err := filepath.EvalSymlinks(home)
	if err != nil {
		return home
	}
	absolute, err := filepath.Abs(resolved)
	if err != nil {
		return home
	}
	return absolute
}

func exponentialBackoff(consecutiveRateLimits uint32) time.Duration {
	return min(initialRateLimitBackoff*time.Duration(uint64(1)<<min(consecutiveRateLimits, 8)), maxRateLimitBackoff)
}

func rateLimitReason(nextAllowedAt time.Time) string {
	return fmt.Sprintf("%s by Meta; next check after %s", RateLimitedPrefix, nextAllowedAt.Local().Format("15:04"))
}

// parseRetryAfter reads `Retry-After` as either a delay in seconds or an
// HTTP date.
func parseRetryAfter(value string, now time.Time) *time.Duration {
	value = strings.TrimSpace(value)
	// Rust's u64 parse takes one leading `+`.
	if digits := strings.TrimPrefix(value, "+"); digits != "" && !strings.HasPrefix(digits, "+") {
		if seconds, err := strconv.ParseUint(digits, 10, 64); err == nil {
			wait := time.Duration(math.MaxInt64)
			if seconds <= uint64(math.MaxInt64/int64(time.Second)) {
				wait = time.Duration(seconds) * time.Second
			}
			return &wait
		}
	}
	at, err := mail.ParseDate(value)
	if err != nil {
		return nil
	}
	wait := at.Sub(now)
	if wait < 0 {
		return nil
	}
	return &wait
}

func queryWithTimeout(ctx context.Context, home string, endpoints core.MuseQuotaEndpoints, timeout time.Duration) (MuseUsage, error) {
	httpClient := &http.Client{
		Timeout: timeout,
		CheckRedirect: func(*http.Request, []*http.Request) error {
			return http.ErrUseLastResponse
		},
	}
	token, err := freshAccessToken(ctx, httpClient, home, endpoints.Auth, nil)
	if err != nil {
		return nil, err
	}
	usage, err := mint(ctx, httpClient, endpoints.Mint, token)
	// The endpoint, not the recorded expiry, is the judge of a token.
	if errors.As(err, new(loginRejected)) {
		refreshed, err := freshAccessToken(ctx, httpClient, home, endpoints.Auth, &token)
		if err != nil {
			return nil, err
		}
		usage, err = mint(ctx, httpClient, endpoints.Mint, refreshed)
		if errors.As(err, new(loginRejected)) {
			return nil, errors.New(loginExpired)
		}
		return usage, err
	}
	return usage, err
}

// loginRejected: Meta refused the login: the mint endpoint rejected the
// access token, or the OAuth host rejected the refresh token.
type loginRejected struct{}

func (loginRejected) Error() string { return loginExpired }

// museIsTimeout is reqwest's Error::is_timeout.
func museIsTimeout(err error) bool {
	return claudeIsTimeout(err)
}

func mint(ctx context.Context, httpClient *http.Client, baseURL string, token string) (MuseUsage, error) {
	url := strings.TrimRight(baseURL, "/") + mintPath
	request, err := http.NewRequestWithContext(ctx, http.MethodPost, url, strings.NewReader(`{"onboard":false}`))
	if err != nil {
		// Do not include the error: it can expose request details when a
		// malformed configured endpoint is involved.
		return nil, errors.New("Muse usage request failed")
	}
	request.Header.Set("Authorization", "Bearer "+token)
	request.Header.Set("Content-Type", "application/json")
	response, err := httpClient.Do(request)
	if err != nil {
		if museIsTimeout(err) {
			return nil, errors.New("Muse usage request timed out")
		}
		// Do not include the transport's error: it can expose request
		// details when a malformed configured endpoint is involved.
		return nil, errors.New("Muse usage request failed")
	}
	defer response.Body.Close()
	status := response.StatusCode
	if status == http.StatusUnauthorized || status == http.StatusForbidden {
		return nil, loginRejected{}
	}
	if status == http.StatusTooManyRequests {
		limited := &museRateLimited{}
		if values := response.Header.Values("Retry-After"); len(values) > 0 {
			limited.retryAfter = parseRetryAfter(values[0], time.Now())
		}
		return nil, limited
	}
	if !quotaStatusSuccess(status) {
		return nil, fmt.Errorf("Muse usage request returned HTTP %s", quotaHTTPStatus(status))
	}
	body, err := readBoundedBody(response)
	if err != nil {
		return nil, err
	}
	payload, err := jsonvalue.Parse(body)
	if err != nil {
		return nil, fmt.Errorf("decode Muse usage response: %w", err)
	}
	return museUsageParse(payload)
}

// museLogin is the Meta login in a profile's `auth.json`, with the document
// it came from so a refresh rewrites only the token fields.
type museLogin struct {
	document     any
	accessToken  string
	refreshToken *string
	// expiresAt is epoch seconds; nil for a stored API key.
	expiresAt *int64
}

func (l *museLogin) refreshDue(now int64) bool {
	return l.expiresAt != nil && quotaSaturatingSubInt64(*l.expiresAt, now) < int64(refreshAhead/time.Second)
}

// freshAccessToken hands back an access token to query with, refreshing the
// stored login when it is near expiry or when rejected names the token the
// endpoint just refused. The refresh runs under Muse's own `auth.json` lock
// and re-reads the login once the lock is held, so a refresh Muse finished
// meanwhile is used instead of spending its refresh token a second time.
func freshAccessToken(ctx context.Context, httpClient *http.Client, home string, authBaseURL string, rejected *string) (string, error) {
	initial, err := readLogin(ctx, home)
	if err != nil {
		return "", err
	}
	now := int64(core.EpochSeconds())
	if rejected == nil && !initial.refreshDue(now) {
		return initial.accessToken, nil
	}
	if initial.refreshToken == nil {
		// A stored API key, or a login without a refresh grant: nothing
		// here can renew it.
		if rejected != nil {
			return "", loginRejected{}
		}
		return initial.accessToken, nil
	}

	// Held until this function returns.
	lock, err := core.CredentialWriteGuardAcquire(ctx, core.HarnessKindMuse, filepath.Join(home, "auth.json"))
	if err != nil {
		return "", err
	}
	defer lock.Release()
	active, err := readLogin(ctx, home)
	if err != nil {
		return "", err
	}
	var renewedMeanwhile bool
	if rejected != nil {
		renewedMeanwhile = active.accessToken != *rejected
	} else {
		renewedMeanwhile = !active.refreshDue(now)
	}
	if renewedMeanwhile {
		return active.accessToken, nil
	}
	if active.refreshToken == nil {
		return "", loginRejected{}
	}
	grant, err := refreshGrant(ctx, httpClient, authBaseURL, *active.refreshToken)
	if err != nil {
		// Ahead of expiry the current token still works; the failure is
		// logged and the refresh is tried again on the next poll.
		if rejected == nil && !errors.As(err, new(loginRejected)) {
			slog.Warn("could not refresh the Muse login ahead of expiry",
				"home", quotaDisplay(home),
				"error", quotaDisplay(err.Error()),
			)
			return active.accessToken, nil
		}
		return "", err
	}
	if err := saveLogin(home, active.document, &grant, now); err != nil {
		return "", err
	}
	return grant.accessToken, nil
}

type tokenGrant struct {
	accessToken  string
	refreshToken *string
	expiresIn    *int64
}

func refreshGrant(ctx context.Context, httpClient *http.Client, authBaseURL string, refreshToken string) (tokenGrant, error) {
	url := strings.TrimRight(authBaseURL, "/") + tokenPath
	body := quotaFormEncode([][2]string{
		{"grant_type", "refresh_token"},
		{"refresh_token", refreshToken},
		{"client_id", oauthClientID},
	})
	request, err := http.NewRequestWithContext(ctx, http.MethodPost, url, strings.NewReader(body))
	if err != nil {
		return tokenGrant{}, errors.New("Muse login refresh failed")
	}
	request.Header.Set("Accept", "application/json")
	request.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	response, err := httpClient.Do(request)
	if err != nil {
		if museIsTimeout(err) {
			return tokenGrant{}, errors.New("Muse login refresh timed out")
		}
		return tokenGrant{}, errors.New("Muse login refresh failed")
	}
	defer response.Body.Close()
	status := response.StatusCode
	answer, err := readBoundedBody(response)
	if err != nil {
		return tokenGrant{}, err
	}
	payload, parseErr := jsonvalue.Parse(answer)
	parsed := parseErr == nil
	if !quotaStatusSuccess(status) {
		code, hasCode := "", false
		if parsed {
			code, hasCode = quotaJSONGetString(payload, "error")
		}
		if hasCode && code == "invalid_grant" {
			return tokenGrant{}, loginRejected{}
		}
		// Anything else is reported as it is: it may mean the request itself
		// is wrong, which "login expired" would hide.
		if hasCode {
			return tokenGrant{}, fmt.Errorf("Muse login refresh returned HTTP %s (%s)", quotaHTTPStatus(status), code)
		}
		return tokenGrant{}, fmt.Errorf("Muse login refresh returned HTTP %s", quotaHTTPStatus(status))
	}
	if !parsed {
		return tokenGrant{}, errors.New("Muse login refresh returned invalid JSON")
	}
	accessToken, ok := quotaJSONGetString(payload, "access_token")
	if !ok || accessToken == "" || strings.ContainsFunc(accessToken, unicode.IsSpace) {
		return tokenGrant{}, errors.New("Muse login refresh returned no access token")
	}
	grant := tokenGrant{accessToken: accessToken}
	if refreshed, ok := quotaJSONGetString(payload, "refresh_token"); ok && refreshed != "" {
		grant.refreshToken = &refreshed
	}
	if seconds, ok := quotaJSONI64(quotaJSONIndex(payload, "expires_in")); ok && seconds > 0 {
		grant.expiresIn = &seconds
	}
	return grant, nil
}

// saveLogin writes the refreshed grant into `auth.json`, keeping every other
// field Muse stored. The caller holds Muse's lock.
func saveLogin(home string, document any, grant *tokenGrant, now int64) error {
	root, isObject := document.(jsonvalue.Object)
	providers, providersOK := quotaJSONIndex(root, "providers").(jsonvalue.Object)
	meta, metaOK := quotaJSONIndex(providers, "meta").(jsonvalue.Object)
	if !isObject || !providersOK || !metaOK {
		return errors.New("Muse credentials have no Meta login")
	}
	meta = meta.Clone()
	meta.Set("access_token", grant.accessToken)
	if grant.refreshToken != nil {
		meta.Set("refresh_token", *grant.refreshToken)
	}
	if grant.expiresIn != nil {
		meta.Set("expires_at", museSaturatingAddInt64(now, *grant.expiresIn))
	} else {
		// A stale expiry would ask for a refresh on every poll. serde_json's
		// order-preserving map removes by swapping in the last member.
		meta.SwapDelete("expires_at")
	}
	providers = providers.Clone()
	providers.Set("meta", meta)
	root = root.Clone()
	root.Set("providers", providers)
	body, err := museMarshalPretty(root)
	if err != nil {
		return err
	}
	body = append(body, '\n')
	if err := core.AtomicWrite(filepath.Join(home, "auth.json"), body); err != nil {
		return fmt.Errorf("save the refreshed Muse login: %w", err)
	}
	return nil
}

// museMarshalPretty writes value as serde_json::to_vec_pretty does: two-space
// indentation, `": "` between a name and its value, and `[]`/`{}` for empty
// containers.
func museMarshalPretty(value any) ([]byte, error) {
	compact, err := jsonvalue.Marshal(value)
	if err != nil {
		return nil, err
	}
	var pretty bytes.Buffer
	if err := json.Indent(&pretty, compact, "", "  "); err != nil {
		return nil, err
	}
	return pretty.Bytes(), nil
}

// museSaturatingAddInt64 is Rust's i64::saturating_add.
func museSaturatingAddInt64(a, b int64) int64 {
	sum := a + b
	if b > 0 && sum < a {
		return math.MaxInt64
	}
	if b < 0 && sum > a {
		return math.MinInt64
	}
	return sum
}

func readLogin(ctx context.Context, home string) (museLogin, error) {
	file, err := os.Open(filepath.Join(home, "auth.json"))
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			return museLogin{}, errors.New("Muse login required: auth.json is missing; sign in to Muse")
		}
		return museLogin{}, errors.New("Muse credentials are unavailable")
	}
	defer file.Close()
	contents, err := io.ReadAll(io.LimitReader(file, core.MaxCredentialBytes+1))
	if err != nil {
		return museLogin{}, errors.New("Muse credentials are unavailable")
	}
	if len(contents) > core.MaxCredentialBytes {
		return museLogin{}, fmt.Errorf("Muse credentials exceed the %d byte limit", core.MaxCredentialBytes)
	}
	document, err := jsonvalue.Parse(contents)
	if err != nil {
		return museLogin{}, errors.New("Muse credentials are invalid; sign in to Muse")
	}
	meta, hasMeta := quotaJSONPointer(document, "/providers/meta")
	field := func(name string) (string, bool) {
		if !hasMeta {
			return "", false
		}
		value, ok := quotaJSONGetString(meta, name)
		if !ok || strings.TrimSpace(value) == "" {
			return "", false
		}
		return value, true
	}
	accessToken, ok := field("access_token")
	if !ok {
		return museLogin{}, errors.New("Muse access token is missing; sign in to Muse")
	}
	if strings.ContainsFunc(accessToken, unicode.IsSpace) {
		return museLogin{}, errors.New("Muse access token is invalid; sign in to Muse")
	}
	login := museLogin{document: document, accessToken: accessToken}
	if refreshToken, ok := field("refresh_token"); ok {
		login.refreshToken = &refreshToken
	}
	if hasMeta {
		if expiresAt, ok := quotaJSONI64(quotaJSONIndex(meta, "expires_at")); ok {
			login.expiresAt = &expiresAt
		}
	}
	return login, nil
}

func readBoundedBody(response *http.Response) ([]byte, error) {
	body, err := io.ReadAll(io.LimitReader(response.Body, core.MaxCredentialBytes+1))
	if err != nil {
		if museIsTimeout(err) {
			return nil, errors.New("Muse usage request timed out")
		}
		return nil, errors.New("read Muse usage response")
	}
	if len(body) > core.MaxCredentialBytes {
		return nil, fmt.Errorf("Muse usage response exceeds the %d byte limit", core.MaxCredentialBytes)
	}
	return body, nil
}

// museUsageParse parses the native endpoint's subscription usage object
// (Rust's `muse_usage::parse`).
//
// A window is shown only when the endpoint supplied the fields needed to
// describe it. Absent usage on an active subscription is the idle state, not
// a reading: idleWindows decides what can still be said about it. Invalid
// percentages are rejected so corrupt data cannot look like a healthy quota.
func museUsageParse(payload any) (MuseUsage, error) {
	usage, _ := quotaJSONGet(payload, "subs_usage")
	var subsUsage jsonvalue.Object
	switch usage := usage.(type) {
	case nil:
		active, _ := quotaJSONGet(payload, "is_subs_active")
		switch active {
		case true:
			return MuseUsageIdle{}, nil
		case false:
			return nil, errors.New("Muse reports no active subscription")
		}
		return nil, errors.New("Muse subscription usage is unavailable")
	case jsonvalue.Object:
		subsUsage = usage
	default:
		return nil, errors.New("Muse subscription usage is malformed")
	}
	var windows []MuseUsageWindow
	weekly, err := optionalWindow(subsUsage, "weekly")
	if err != nil {
		return nil, err
	}
	if weekly != nil {
		remaining, err := parseRemainingPercent(quotaJSONIndex(weekly, "used_percent"))
		if err != nil {
			return nil, err
		}
		week := int64(7 * 24 * 60 * 60)
		windows = append(windows, MuseUsageWindow{
			Label:            "Week",
			RemainingPercent: remaining,
			DurationSeconds:  &week,
			ResetsAt:         museResetsAt(weekly),
		})
	}
	window, err := optionalWindow(subsUsage, "window")
	if err != nil {
		return nil, err
	}
	if window != nil {
		remaining, err := parseRemainingPercent(quotaJSONIndex(window, "used_percent"))
		if err != nil {
			return nil, err
		}
		duration, err := parseDuration(quotaJSONIndex(window, "window_duration_mins"))
		if err != nil {
			return nil, err
		}
		var seconds *int64
		if duration <= math.MaxInt64/60 {
			value := duration * 60
			seconds = &value
		}
		windows = append(windows, MuseUsageWindow{
			Label:            museUsageWindowLabel(duration),
			RemainingPercent: remaining,
			DurationSeconds:  seconds,
			ResetsAt:         museResetsAt(window),
		})
	}
	if len(windows) == 0 {
		return nil, errors.New("Muse subscription usage is unavailable")
	}
	return MuseUsageOpen{Value: windows}, nil
}

// museResetsAt is a window's `resets_at`, when it is an integer.
func museResetsAt(window jsonvalue.Object) *int64 {
	reset, ok := quotaJSONI64(quotaJSONIndex(window, "resets_at"))
	if !ok {
		return nil
	}
	return &reset
}

// usageWindow is Meta's short usage window: its plans state the allowance
// per five hours, and the endpoint reports `window_duration_mins: 300`.
const usageWindow = 5 * time.Hour

// idleContinuity is how recent the last observed probe must be for an idle
// reading to keep its weekly figure. A probe's time can trail Meta's answer
// it carries by up to minRefreshInterval (the gate serves a cached answer
// that long), so that much is taken off the window to keep the bound exact.
const idleContinuity = usageWindow - minRefreshInterval

// idleHistorySince is the earliest probe idleWindows can use for an idle
// reading at now.
func idleHistorySince(now uint64) uint64 {
	continuity := uint64(idleContinuity / time.Second)
	if now < continuity {
		return 0
	}
	return now - continuity
}

// idleWindows is what an idle reading at now can show, from this profile's
// earlier probes (newest first).
//
// Meta withholds the weekly window while idle, but the week cannot move
// without a request, and a request opens a five-hour window that Meta
// reports until it closes. So when the last observed probe is recent enough
// that no window can have opened and closed since, its weekly figure still
// holds. Past its reset, the week is untouched: full, next reset unknown.
// Without a recent enough probe the week is unknown and nothing is shown.
// The five-hour window is left out, as Claude leaves out an idle one.
//
// A probe served from the gate during a rate limit observed nothing, and a
// failed one neither, so neither counts.
func idleWindows(history []client.ProfileQuota, now uint64) []client.QuotaWindow {
	var observed *client.ProfileQuota
	for i := range history {
		report := &history[i]
		if report.Error == nil && (report.Extra == nil || !strings.HasPrefix(*report.Extra, RateLimitedPrefix)) {
			observed = report
			break
		}
	}
	if observed == nil {
		return nil
	}
	if quotaSaturatingSubUint64(now, observed.RefreshedAtEpochSeconds) >= uint64(idleContinuity/time.Second) {
		return nil
	}
	weekly := observed.WeeklyWindow()
	if weekly == nil {
		return nil
	}
	week := *weekly
	if reset := week.ResetsAtEpochSeconds; reset != nil && (*reset < 0 || uint64(*reset) <= now) {
		full := uint8(100)
		week.RemainingPercent = &full
		week.Used = nil
		week.Resets = nil
		week.ResetsAtEpochSeconds = nil
	}
	return []client.QuotaWindow{week}
}

// optionalWindow is subsUsage[key] as an object, nil when absent or null.
func optionalWindow(subsUsage jsonvalue.Object, key string) (jsonvalue.Object, error) {
	value, _ := subsUsage.Get(key)
	switch window := value.(type) {
	case nil:
		return nil, nil
	case jsonvalue.Object:
		return window, nil
	default:
		return nil, fmt.Errorf("Muse %s usage window is malformed", key)
	}
}

// parseDuration reads a window's `window_duration_mins`; value is nil when
// the field is absent or null.
func parseDuration(value any) (int64, error) {
	if value == nil {
		return 0, errors.New("Muse usage window duration is missing")
	}
	duration, ok := quotaJSONI64(value)
	if !ok || duration <= 0 {
		return 0, errors.New("Muse usage returned an invalid window duration")
	}
	return duration, nil
}

// parseRemainingPercent reads a window's `used_percent`; value is nil when
// the field is absent or null.
func parseRemainingPercent(value any) (uint8, error) {
	if value == nil {
		return 0, errors.New("Muse usage percentage is missing")
	}
	usedPercent, ok := quotaJSONF64(value)
	if !ok || math.IsInf(usedPercent, 0) || math.IsNaN(usedPercent) || usedPercent < 0 {
		return 0, errors.New("Muse usage returned an invalid percentage")
	}
	return quotaF64ToUint8(math.Round(100 - min(usedPercent, 100))), nil
}

// museUsageWindowLabel is Rust's `muse_usage::window_label`.
func museUsageWindowLabel(minutes int64) string {
	switch {
	case minutes == 300:
		return "5H"
	case minutes%1_440 == 0:
		return fmt.Sprintf("%dd", minutes/1_440)
	case minutes%60 == 0:
		return fmt.Sprintf("%dH", minutes/60)
	default:
		return fmt.Sprintf("%dm", minutes)
	}
}
