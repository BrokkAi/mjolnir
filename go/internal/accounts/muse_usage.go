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
	"context"
	"fmt"
	"net/http"
	"path/filepath"
	"sync"
	"time"

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
	panic("unported: mj-accounts/src/muse_usage.rs query_gated")
}

// cachedReport is the cached answer, when the gate says no network call is
// due. A nil report and nil error mean the caller should query the endpoint.
func cachedReport(gate *museGate, key string, now time.Time) (*MuseUsageReport, error) {
	panic("unported: mj-accounts/src/muse_usage.rs cached_report")
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
	panic("unported: mj-accounts/src/muse_usage.rs parse_retry_after")
}

func queryWithTimeout(ctx context.Context, home string, endpoints core.MuseQuotaEndpoints, timeout time.Duration) (MuseUsage, error) {
	panic("unported: mj-accounts/src/muse_usage.rs query_with_timeout")
}

// loginRejected: Meta refused the login: the mint endpoint rejected the
// access token, or the OAuth host rejected the refresh token.
type loginRejected struct{}

func (loginRejected) Error() string { return loginExpired }

func mint(ctx context.Context, httpClient *http.Client, baseURL string, token string) (MuseUsage, error) {
	panic("unported: mj-accounts/src/muse_usage.rs mint")
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
	panic("unported: mj-accounts/src/muse_usage.rs fresh_access_token")
}

type tokenGrant struct {
	accessToken  string
	refreshToken *string
	expiresIn    *int64
}

func refreshGrant(ctx context.Context, httpClient *http.Client, authBaseURL string, refreshToken string) (tokenGrant, error) {
	panic("unported: mj-accounts/src/muse_usage.rs refresh_grant")
}

// saveLogin writes the refreshed grant into `auth.json`, keeping every other
// field Muse stored. The caller holds Muse's lock.
func saveLogin(home string, document any, grant *tokenGrant, now int64) error {
	panic("unported: mj-accounts/src/muse_usage.rs save_login")
}

func readLogin(ctx context.Context, home string) (museLogin, error) {
	panic("unported: mj-accounts/src/muse_usage.rs read_login")
}

func readBoundedBody(response *http.Response) ([]byte, error) {
	panic("unported: mj-accounts/src/muse_usage.rs read_bounded_body")
}

// museUsageParse parses the native endpoint's subscription usage object
// (Rust's `muse_usage::parse`).
//
// A window is shown only when the endpoint supplied the fields needed to
// describe it. Absent usage on an active subscription is the idle state, not
// a reading: idleWindows decides what can still be said about it. Invalid
// percentages are rejected so corrupt data cannot look like a healthy quota.
func museUsageParse(payload any) (MuseUsage, error) {
	panic("unported: mj-accounts/src/muse_usage.rs parse")
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
	panic("unported: mj-accounts/src/muse_usage.rs idle_windows")
}

// optionalWindow is subsUsage[key] as an object, nil when absent or null.
func optionalWindow(subsUsage map[string]any, key string) (map[string]any, error) {
	switch window := subsUsage[key].(type) {
	case nil:
		return nil, nil
	case map[string]any:
		return window, nil
	default:
		return nil, fmt.Errorf("Muse %s usage window is malformed", key)
	}
}

// parseDuration reads a window's `window_duration_mins`; value is nil when
// the field is absent or null.
func parseDuration(value any) (int64, error) {
	panic("unported: mj-accounts/src/muse_usage.rs parse_duration")
}

// parseRemainingPercent reads a window's `used_percent`; value is nil when
// the field is absent or null.
func parseRemainingPercent(value any) (uint8, error) {
	panic("unported: mj-accounts/src/muse_usage.rs parse_remaining_percent")
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
