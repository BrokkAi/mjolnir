// Port of mj-accounts/src/quota.rs.

// One-pane quota collection for Mjolnir harness profiles.

package accounts

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"net/http"
	"os"
	"slices"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// The quota types and labels this module re-exports from mj-client (Rust's
// `pub use mj_client::quota::{...}`).
type (
	ProfileQuota = client.ProfileQuota
	QuotaWindow  = client.QuotaWindow
)

const (
	APILabel         = client.APILabel
	IdleLabel        = client.IdleLabel
	NotReportedLabel = client.NotReportedLabel
)

// ProjectsExhaustion re-exports client.ProjectsExhaustion.
func ProjectsExhaustion(window *client.QuotaWindow, now uint64) bool {
	return client.ProjectsExhaustion(window, now)
}

// The Codex quota source types this module re-exports from mj-core (Rust's
// `pub use mj_core::config::{CodexQuotaSource, ProviderCredential}`).
type (
	CodexQuotaSource   = core.CodexQuotaSource
	ProviderCredential = core.ProviderCredential
)

// RetryAfterHeader re-exports core.RetryAfterHeader (Rust's
// `pub use mj_core::retry_after::retry_after_header`).
func RetryAfterHeader(headers http.Header) (time.Duration, bool) {
	return core.RetryAfterHeader(headers)
}

// SurfaceQuotaReport clones a daemon report for a user-facing quota surface,
// retaining only unscoped windows, Claude's Fable allowance and Antigravity's
// pools. Internal selection and health keep the complete report.
func SurfaceQuotaReport(report *client.ProfileQuota) client.ProfileQuota {
	visible := *report
	if report.Harness == core.HarnessKindAntigravity {
		visible.Windows = slices.Clone(report.Windows)
		return visible
	}
	fableWeekly := report.FableWeeklyWindow()
	visible.Windows = make([]client.QuotaWindow, 0, len(report.Windows)+1)
	for _, window := range report.Windows {
		if window.ModelScope == nil {
			visible.Windows = append(visible.Windows, window)
		}
	}
	if fableWeekly != nil {
		visible.Windows = append(visible.Windows, *fableWeekly)
	}
	return visible
}

type QuotaRefreshRequest struct {
	ProfileID   string
	Harness     core.HarnessKind
	AuthScheme  core.AuthScheme
	SourceHome  string
	Environment map[string]string
	Cwd         string
	// CodexSource is where a Codex profile's quota comes from. Other
	// harnesses ignore it.
	CodexSource core.CodexQuotaSource
	// OpencodeUsagePriced is whether an OpenCode profile signs in through an
	// environment-supplied provider key rather than a login OpenCode itself
	// stores, so it is billed by usage the way Dsh and a Codex custom
	// provider are. Other harnesses ignore it.
	OpencodeUsagePriced bool
}

// KindMayNeedManagedQuotaHarness is whether a quota check for this harness
// can call the managed installer at all (a host harness or the pinned
// Antigravity), decided from the kind alone, before reading any profile
// file. Whether one profile does (QuotaRefreshRequest.RunsHostHarness) also
// depends on its login and provider. Rust declares it on Unix only.
func KindMayNeedManagedQuotaHarness(kind core.HarnessKind) bool {
	switch kind {
	case core.HarnessKindCodex, core.HarnessKindGrok, core.HarnessKindClaude, core.HarnessKindAntigravity:
		return true
	default:
		return false
	}
}

// CacheIdentity hashes the request's configuration, never writing
// credentials into the cache key.
func (r *QuotaRefreshRequest) CacheIdentity() string {
	panic("unported: mj-accounts/src/quota.rs cache_identity")
}

// RunsHostHarness is whether this profile's check or login renewal would
// call `quota_harness::locate`: the question the daemon's prewarm asks, so
// it installs nothing a check would not. It reads the Claude login file, so
// call it off any event loop. Claude's decision is runsCLI; Codex and Grok
// resolve their program before reading any login, so the kind and Codex's
// ChatGPT source decide. Rust declares it on Unix only.
func (r *QuotaRefreshRequest) RunsHostHarness() bool {
	if !KindMayNeedManagedQuotaHarness(r.Harness) {
		return false
	}
	switch r.Harness {
	case core.HarnessKindClaude:
		_, aws := r.AuthScheme.(core.AuthSchemeAwsCredentialChain)
		return !aws && runsCLI(r.SourceHome)
	case core.HarnessKindCodex:
		_, chatGPT := r.CodexSource.(core.CodexQuotaSourceChatGptLogin)
		return chatGPT
	default:
		// Grok and Antigravity resolve their program before reading any
		// login.
		return true
	}
}

// QuotaRefreshRequestForProfile builds the request for one configured
// profile. The harness home environment is composed here so every caller
// asks for quota the same way, and so a provider key is read from exactly
// one place.
func QuotaRefreshRequestForProfile(profileID string, profile *core.HarnessProfile, cwd string) QuotaRefreshRequest {
	panic("unported: mj-accounts/src/quota.rs QuotaRefreshRequest::for_profile")
}

// DefaultRateLimitHold is how long a profile's probes are held when the
// provider answers 429 without saying how long.
const DefaultRateLimitHold = 15 * time.Minute

// RateLimited is a provider's 429 (too many requests). Carried, as
// *RateLimited, as the error of a failed probe so the refresh can turn it
// into a hold.
type RateLimited struct {
	RetryAfter *time.Duration
}

func (*RateLimited) Error() string { return "rate limited" }

// RateLimitHold is the hold for a 429: what the provider asked for, kept
// between one minute and six hours so a bad header can neither spin the
// poller nor park a profile for days.
func RateLimitHold(retryAfter *time.Duration) time.Duration {
	hold := DefaultRateLimitHold
	if retryAfter != nil {
		hold = *retryAfter
	}
	return min(max(hold, time.Minute), 6*time.Hour)
}

// QuotaRefreshInterval is how often each profile's quota is read again.
const QuotaRefreshInterval = 10 * time.Minute

// RefusedCheckSchedule is, for a check refused at attempt: when the
// provider's hold ends, and when the next check runs. A refused check does
// not shorten the regular cadence: the next one is an interval after this
// attempt, or the hold's end if later.
func RefusedCheckSchedule(attempt uint64, retryAfter *time.Duration) (until uint64, nextCheck uint64) {
	until = attempt + uint64(RateLimitHold(retryAfter)/time.Second)
	nextCheck = max(until, attempt+uint64(QuotaRefreshInterval/time.Second))
	return until, nextCheck
}

// HoldReport is what to publish after a probe was rate limited: the last
// good reading with the hold attached, so its numbers and age stay visible.
// With no good reading to keep, the rate-limit report itself.
func HoldReport(previous *client.ProfileQuota, limited client.ProfileQuota) client.ProfileQuota {
	if previous == nil || previous.Error != nil {
		return limited
	}
	good := *previous
	good.Windows = slices.Clone(previous.Windows)
	good.RateLimitedUntilEpochSeconds = limited.RateLimitedUntilEpochSeconds
	good.NextCheckAtEpochSeconds = limited.NextCheckAtEpochSeconds
	return good
}

type QuotaRefreshOutcome struct {
	// CacheIdentity is captured from the request that produced this outcome.
	CacheIdentity      string
	Report             client.ProfileQuota
	CredentialsChanged bool
}

// QuotaReportPublication is one profile's authoritative quota publication.
// A nil Report means that its current identity has not completed a check
// yet.
type QuotaReportPublication struct {
	// LoginError is the renewal failure, independent of the usage API
	// result.
	LoginError    *string
	CacheIdentity string
	// Report is the latest probe, including a failure. Display surfaces keep
	// showing that failure through EffectiveReport.
	Report *client.ProfileQuota
	// LastGood is the newest probe with no error. Scheduling uses this when
	// Report failed, so a network error cannot turn a known-exhausted
	// profile into Unknown. Seeded from `quota_reset_cache` and updated only
	// when a probe succeeds.
	LastGood *client.ProfileQuota
}

func (p *QuotaReportPublication) ObserveClaudeGrant(profileID string, check ClaudeGrantCheck, rejected bool) bool {
	var loginError *string
	if rejected {
		failed := false
		switch check := check.(type) {
		case ClaudeGrantCheckFailed:
			failed = true
		case ClaudeGrantCheckWaiting:
			failed = check.DueAtMs == nil
		}
		if failed {
			message := client.RefusedLoginMessage(profileID)
			loginError = &message
		}
	}
	if (p.LoginError == nil) == (loginError == nil) && (loginError == nil || *p.LoginError == *loginError) {
		return false
	}
	p.LoginError = loginError
	return true
}

func (p *QuotaReportPublication) EffectiveReport(profileID string) *client.ProfileQuota {
	var report client.ProfileQuota
	switch {
	case p.Report != nil:
		report = *p.Report
		report.Windows = slices.Clone(p.Report.Windows)
	case p.LoginError == nil:
		return nil
	default:
		report = client.ProfileQuota{
			ProfileID:               profileID,
			Harness:                 core.HarnessKindClaude,
			Windows:                 []client.QuotaWindow{},
			RefreshedAtEpochSeconds: core.EpochSeconds(),
		}
	}
	if p.LoginError != nil {
		message := *p.LoginError
		report.Error = &message
	}
	return &report
}

type QuotaReportMap = map[string]QuotaReportPublication

// QuotaManager's zero value is ready to use (Rust's Default).
type QuotaManager struct {
	codexClients map[string]*CodexUsageClient
	reports      map[string]client.ProfileQuota
	cacheRoot    *string
}

func (m *QuotaManager) Reports() map[string]client.ProfileQuota {
	return m.reports
}

// Report is the latest report for one profile, if there is one.
func (m *QuotaManager) Report(profileID string) *client.ProfileQuota {
	report, ok := m.reports[profileID]
	if !ok {
		return nil
	}
	return &report
}

// Seed adopts a report read from the store, so the profile counts as probed
// at the report's time.
func (m *QuotaManager) Seed(report client.ProfileQuota) {
	if m.reports == nil {
		m.reports = map[string]client.ProfileQuota{}
	}
	m.reports[report.ProfileID] = report
}

// Forget forgets a profile's report and stops its cached client, for a
// profile whose configuration changed. The client's `codex app-server` was
// started with the old home and environment.
func (m *QuotaManager) Forget(ctx context.Context, profileID string) {
	panic("unported: mj-accounts/src/quota.rs QuotaManager::forget")
}

// RetainProfiles keeps only the profiles in keep: it drops the reports of
// the others and stops their cached clients.
func (m *QuotaManager) RetainProfiles(ctx context.Context, keep map[string]struct{}) {
	for profileID := range m.reports {
		if _, kept := keep[profileID]; !kept {
			delete(m.reports, profileID)
		}
	}
	m.stopClientsOutsideBatch(ctx, keep)
}

// RefreshProfiles refreshes each profile independently so one slow harness
// cannot delay the others. onReport runs per profile in completion order, so
// fast harnesses report without waiting for the slowest one in the batch.
// The batch is the whole configured set: profiles outside it are dropped.
func (m *QuotaManager) RefreshProfiles(ctx context.Context, requests []QuotaRefreshRequest, onReport func(QuotaRefreshOutcome)) {
	batch := make(map[string]struct{}, len(requests))
	for _, request := range requests {
		batch[request.ProfileID] = struct{}{}
	}
	for profileID := range m.reports {
		if _, kept := batch[profileID]; !kept {
			delete(m.reports, profileID)
		}
	}
	m.Probe(ctx, requests, onReport)
	m.stopClientsOutsideBatch(ctx, batch)
}

// Probe probes exactly these profiles and leaves every other report alone.
func (m *QuotaManager) Probe(ctx context.Context, requests []QuotaRefreshRequest, onReport func(QuotaRefreshOutcome)) {
	panic("unported: mj-accounts/src/quota.rs QuotaManager::probe")
}

// stopClientsOutsideBatch stops the cached clients whose profiles are not in
// keep. Every batch carries the whole configured set, so a client left over
// from an earlier batch belongs to a profile the configuration no longer
// has. Each one owns a live `codex app-server` child that nothing would ever
// hand back to a refresh again, so it would run until the controller exits.
func (m *QuotaManager) stopClientsOutsideBatch(ctx context.Context, keep map[string]struct{}) {
	panic("unported: mj-accounts/src/quota.rs QuotaManager::stop_clients_outside_batch")
}

func (m *QuotaManager) Shutdown(ctx context.Context) {
	panic("unported: mj-accounts/src/quota.rs QuotaManager::shutdown")
}

// logQuotaChange logs why a profile's quota could not be read, and when it
// can be read again.
//
// A failed probe becomes the report's `error`, and the panes show only a
// short label for it ("unavailable", "login expired", "rate limited"), so
// without this line the reason was recorded nowhere (R12-2). A profile that
// keeps failing the same way is logged once, not on every refresh.
func logQuotaChange(previous *client.ProfileQuota, report *client.ProfileQuota) {
	panic("unported: mj-accounts/src/quota.rs log_quota_change")
}

// ClaudeGrantCheck is what the daemon's early Claude grant refresh decided
// for one profile.
type ClaudeGrantCheck interface {
	isClaudeGrantCheck()
}

// ClaudeGrantCheckNotApplicable: not a profile whose grant the daemon renews
// (another harness, or one that does not sign in with the harness's own
// login).
type ClaudeGrantCheckNotApplicable struct{}

// ClaudeGrantCheckWaiting: nothing to do now. DueAtMs is when the grant
// needs renewing, from the login as it is now (after a renewal, the new
// one). Nil when the login is missing, holds no refresh token, or states no
// expiry.
type ClaudeGrantCheckWaiting struct{ DueAtMs *int64 }

// ClaudeGrantCheckDeferred: a handoff is draining admission; the next daemon
// checks the login.
type ClaudeGrantCheckDeferred struct{}

// ClaudeGrantCheckRenewed: a replacement is ready for immediate credential
// delivery.
type ClaudeGrantCheckRenewed struct{ DueAtMs *int64 }

// ClaudeGrantCheckFailed: the refresh failed. The error is already logged
// with its safe category.
type ClaudeGrantCheckFailed struct{}

func (ClaudeGrantCheckNotApplicable) isClaudeGrantCheck() {}
func (ClaudeGrantCheckWaiting) isClaudeGrantCheck()       {}
func (ClaudeGrantCheckDeferred) isClaudeGrantCheck()      {}
func (ClaudeGrantCheckRenewed) isClaudeGrantCheck()       {}
func (ClaudeGrantCheckFailed) isClaudeGrantCheck()        {}

// RefreshClaudeGrant renews a Claude profile's grant ahead of its expiry
// (#1490). The daemon is the only refresher: session copies carry no refresh
// token. This is the decision's one owner and runs apart from the usage
// probe, so a 429 hold on the usage API never postpones it.
func RefreshClaudeGrant(ctx context.Context, request *QuotaRefreshRequest, nowMs int64, launcher ClaudeLauncher, gate *core.Gate, rejectedLogins map[string]struct{}) ClaudeGrantCheck {
	panic("unported: mj-accounts/src/quota.rs refresh_claude_grant")
}

func refreshProfileWithCacheRoot(ctx context.Context, request QuotaRefreshRequest, codexClient *CodexUsageClient, cacheRoot *string) (QuotaRefreshOutcome, *CodexUsageClient) {
	panic("unported: mj-accounts/src/quota.rs refresh_profile_with_cache_root")
}

// recentQuotaHistory is this profile's probes saved since since, newest
// first. Only the process that saves probes reads them back: elsewhere there
// is no history, and a failed read is logged and treated as none, which
// shows less rather than something stale.
func recentQuotaHistory(ctx context.Context, identity string, profileID string, since uint64) []client.ProfileQuota {
	panic("unported: mj-accounts/src/quota.rs recent_quota_history")
}

// codexMinimumRefreshMarginMs is the shortest gap to expiry Hel will leave a
// Codex login sitting at. A token with a long life gets a proportionally
// wider margin, because the poll interval buys nothing once the whole life
// is short.
const codexMinimumRefreshMarginMs int64 = 60 * 60 * 1000

// codexLoginIsNearExpiry is whether the profile's Codex login is close
// enough to expiry that a container copy of it could reach the single-use
// refresh race before the next poll.
func codexLoginIsNearExpiry(ctx context.Context, marker string) bool {
	bytes, err := os.ReadFile(marker)
	if err != nil || len(bytes) > core.MaxCredentialBytes {
		return false
	}
	var expiry, refreshed *int64
	if value, ok := core.CredentialExpiry(core.HarnessKindCodex, bytes); ok {
		expiry = &value
	}
	if value, ok := core.CredentialFreshness(core.HarnessKindCodex, bytes); ok {
		refreshed = &value
	}
	return codexLoginNeedsRefresh(expiry, refreshed, time.Now().UnixMilli())
}

// codexLoginNeedsRefresh: the margin is the larger of one hour and a tenth
// of the token's life, where the life is what the last refresh bought. A
// credential that says nothing about its own age falls back to the flat
// hour.
func codexLoginNeedsRefresh(expiryMillis *int64, lastRefreshMillis *int64, nowMillis int64) bool {
	if expiryMillis == nil {
		return false
	}
	expiry := *expiryMillis
	var lifetime int64
	if lastRefreshMillis != nil {
		lifetime = quotaSaturatingSubInt64(expiry, *lastRefreshMillis)
	}
	margin := max(codexMinimumRefreshMarginMs, lifetime/10)
	return quotaSaturatingSubInt64(expiry, nowMillis) < margin
}

func credentialMarkerFingerprint(ctx context.Context, path string) (*string, error) {
	panic("unported: mj-accounts/src/quota.rs credential_marker_fingerprint")
}

func queryKimi(ctx context.Context, home string, environment map[string]string) ([]client.QuotaWindow, *string, error) {
	panic("unported: mj-accounts/src/quota.rs query_kimi")
}

const kimiOauthClientID = "17e5f671-d194-4dfb-9706-5516cb48c098"

// kimiCredentials is Kimi Code's stored OAuth pair. Each field also accepts
// its camelCase alias when decoding; encoding writes the snake_case names in
// this order, as serde does.
type kimiCredentials struct {
	AccessToken  string `json:"access_token"`
	RefreshToken string `json:"refresh_token"`
	ExpiresAt    int64  `json:"expires_at"`
	Scope        string `json:"scope"`
	TokenType    string `json:"token_type"`
	ExpiresIn    int64  `json:"expires_in"`
}

// UnmarshalJSON decodes as serde's derive does: `access_token` is required,
// the others default, each snake_case name also accepts its camelCase alias,
// and a name given twice (directly and by alias) or a null value is an
// error.
func (c *kimiCredentials) UnmarshalJSON(data []byte) error {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	if fields == nil {
		return errors.New("invalid type: null, expected struct KimiCredentials")
	}
	decoded := kimiCredentials{}
	for _, field := range []struct {
		name, alias string
		target      any
		required    bool
	}{
		{"access_token", "accessToken", &decoded.AccessToken, true},
		{"refresh_token", "refreshToken", &decoded.RefreshToken, false},
		{"expires_at", "expiresAt", &decoded.ExpiresAt, false},
		{"scope", "", &decoded.Scope, false},
		{"token_type", "tokenType", &decoded.TokenType, false},
		{"expires_in", "expiresIn", &decoded.ExpiresIn, false},
	} {
		raw, present := fields[field.name]
		if field.alias != "" {
			if aliased, hasAlias := fields[field.alias]; hasAlias {
				if present {
					return fmt.Errorf("duplicate field `%s`", field.name)
				}
				raw, present = aliased, true
			}
		}
		if !present {
			if field.required {
				return fmt.Errorf("missing field `%s`", field.name)
			}
			continue
		}
		if bytes.Equal(bytes.TrimSpace(raw), []byte("null")) {
			return fmt.Errorf("invalid type: null for field `%s`", field.name)
		}
		if err := json.Unmarshal(raw, field.target); err != nil {
			return err
		}
	}
	*c = decoded
	return nil
}

// differsFrom is whether this is a different pair from other. A refresh
// rotates the access token, the refresh token and the expiry together, so
// those three fields are what tells two pairs apart; the rest only describes
// them.
func (c *kimiCredentials) differsFrom(other *kimiCredentials) bool {
	return c.AccessToken != other.AccessToken ||
		c.RefreshToken != other.RefreshToken ||
		c.ExpiresAt != other.ExpiresAt
}

func (c *kimiCredentials) needsRefresh() bool {
	if c.ExpiresAt == 0 {
		return false
	}
	now := time.Now().Unix()
	threshold := max(int64(300), c.ExpiresIn/2)
	return c.ExpiresAt-now < threshold
}

func readKimiCredentials(ctx context.Context, path string) (kimiCredentials, error) {
	bytes, err := os.ReadFile(path)
	if err != nil {
		return kimiCredentials{}, fmt.Errorf("Kimi Code credentials are unavailable: %w", err)
	}
	var credentials kimiCredentials
	if err := json.Unmarshal(bytes, &credentials); err != nil {
		return kimiCredentials{}, fmt.Errorf("Kimi Code credentials are invalid: %w", err)
	}
	if credentials.AccessToken == "" {
		return kimiCredentials{}, errors.New("Kimi Code access token is missing")
	}
	return credentials, nil
}

// fetchBearerWithAuthRetry asks authenticate for a token, and once more
// with force set and the refused token when the server answers 401.
func fetchBearerWithAuthRetry(ctx context.Context, httpClient *http.Client, url string, authenticate func(ctx context.Context, force bool, rejectedToken *string) (string, error)) (*http.Response, error) {
	token, err := authenticate(ctx, false, nil)
	if err != nil {
		return nil, err
	}
	response, err := fetchBearer(ctx, httpClient, url, token)
	if err != nil || response.StatusCode != http.StatusUnauthorized {
		return response, err
	}
	response.Body.Close()
	refreshed, err := authenticate(ctx, true, &token)
	if err != nil {
		return nil, err
	}
	return fetchBearer(ctx, httpClient, url, refreshed)
}

func fetchBearer(ctx context.Context, httpClient *http.Client, url string, token string) (*http.Response, error) {
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, fmt.Errorf("query quota: %w", err)
	}
	request.Header.Set("Authorization", "Bearer "+token)
	request.Header.Set("Accept", "application/json")
	response, err := httpClient.Do(request)
	if err != nil {
		return nil, fmt.Errorf("query quota: %w", err)
	}
	return response, nil
}

// EnsureFreshKimiToken hands back a usable Kimi Code access token,
// refreshing the stored pair when it is stale or when the server rejected
// it. The refresh runs under the lock the Kimi Code CLI also takes, and the
// pair is re-read once the lock is held, so a refresh another process just
// finished is used instead of spending its new refresh token again.
func EnsureFreshKimiToken(ctx context.Context, httpClient *http.Client, auth *KimiRuntimeAuth, force bool, rejectedToken *string) (string, error) {
	panic("unported: mj-accounts/src/quota.rs ensure_fresh_kimi_token")
}

func saveKimiCredentials(path string, credentials *kimiCredentials) error {
	var body bytes.Buffer
	encoder := json.NewEncoder(&body)
	encoder.SetEscapeHTML(false)
	encoder.SetIndent("", "  ")
	// Encode ends the document with the newline Rust appends.
	if err := encoder.Encode(credentials); err != nil {
		return err
	}
	if err := core.AtomicWrite(path, body.Bytes()); err != nil {
		return fmt.Errorf("save refreshed Kimi Code credentials: %w", err)
	}
	return nil
}

func requiredString(payload any, key string, contextText string) (string, error) {
	if object, ok := payload.(map[string]any); ok {
		if value, ok := object[key].(string); ok && value != "" {
			return value, nil
		}
	}
	return "", fmt.Errorf("%s is missing %s", contextText, key)
}

func parseKimiUsage(payload any) ([]client.QuotaWindow, *string) {
	panic("unported: mj-accounts/src/quota.rs parse_kimi_usage")
}

func kimiLimitLabel(item any, index int) string {
	panic("unported: mj-accounts/src/quota.rs kimi_limit_label")
}

func parseKimiWindow(value any, fallback string) *client.QuotaWindow {
	panic("unported: mj-accounts/src/quota.rs parse_kimi_window")
}

func parseKimiRatioWindow(value any, label string) *client.QuotaWindow {
	panic("unported: mj-accounts/src/quota.rs parse_kimi_ratio_window")
}

func canonicalKimiLabel(label string) string {
	lowercase := quotaASCIILower(label)
	switch {
	case strings.Contains(lowercase, "5h") || strings.Contains(lowercase, "5-hour") || strings.Contains(lowercase, "5 hour"):
		return "5H"
	case strings.Contains(lowercase, "week") || strings.Contains(lowercase, "7d"):
		return "Week"
	case strings.Contains(lowercase, "month") && strings.Contains(lowercase, "code"):
		return "Month code"
	case strings.Contains(lowercase, "month"):
		return "Month"
	default:
		return label
	}
}

// kimiResetValue is the first reset field present in value, null included.
func kimiResetValue(value any) (any, bool) {
	object, ok := value.(map[string]any)
	if !ok {
		return nil, false
	}
	for _, key := range []string{"resetAt", "reset_at", "resetTime", "reset_time"} {
		if reset, ok := object[key]; ok {
			return reset, true
		}
	}
	return nil, false
}

func valueI64(value any) (int64, bool) {
	panic("unported: mj-accounts/src/quota.rs value_i64")
}

func valueF64(value any) (float64, bool) {
	panic("unported: mj-accounts/src/quota.rs value_f64")
}

func normalizeKimiReset(value any) *string {
	panic("unported: mj-accounts/src/quota.rs normalize_kimi_reset")
}

func kimiResetEpochSeconds(value any) *int64 {
	panic("unported: mj-accounts/src/quota.rs kimi_reset_epoch_seconds")
}

// FormatResetLocal formats a Unix reset timestamp as wall-clock time in the
// machine's local time zone. Accepts seconds or milliseconds and rejects
// non-finite or out-of-range values.
func FormatResetLocal(epoch float64) *string {
	panic("unported: mj-accounts/src/quota.rs format_reset_local")
}

func FormatResetLocalSeconds(epoch int64) *string {
	return FormatResetLocal(float64(epoch))
}

// NormalizeResetText normalizes a provider's textual reset value to the
// compact 24-hour form used by the dashboard. A time-only value is the next
// occurrence of that wall-clock time; Claude Code uses this shape for its
// five-hour window.
func NormalizeResetText(value string) *string {
	reset, ok := NormalizeResetAt(value, quotaLocalFixedNow())
	if !ok {
		return nil
	}
	label := formatResetLabel(reset)
	return &label
}

func NormalizeResetEpochSeconds(value string) *int64 {
	reset, ok := NormalizeResetAt(value, quotaLocalFixedNow())
	if !ok {
		return nil
	}
	seconds := reset.Unix()
	return &seconds
}

// NormalizeResetAt reads a provider's reset text against now. now carries a
// fixed offset (Rust's DateTime<FixedOffset>): a reset without its own zone
// is placed in now's offset, not in a zone with daylight-saving rules.
func NormalizeResetAt(value string, now time.Time) (time.Time, bool) {
	panic("unported: mj-accounts/src/quota.rs normalize_reset_at")
}

// formatResetLabel is the pure formatter split from local-zone discovery for
// deterministic tests.
func formatResetLabel(reset time.Time) string {
	return reset.Format("15:04 Jan 2")
}

// quotaLocalFixedNow is the current time in the local zone's present offset
// (Rust's `Local::now().fixed_offset()`).
func quotaLocalFixedNow() time.Time {
	now := time.Now()
	_, offset := now.Zone()
	return now.In(time.FixedZone("", offset))
}

// quotaSaturatingSubInt64 is Rust's i64::saturating_sub.
func quotaSaturatingSubInt64(a, b int64) int64 {
	difference := a - b
	if b > 0 && difference > a {
		return math.MinInt64
	}
	if b < 0 && difference < a {
		return math.MaxInt64
	}
	return difference
}

// quotaASCIILower is Rust's str::to_ascii_lowercase: only ASCII letters
// change.
func quotaASCIILower(text string) string {
	return strings.Map(func(r rune) rune {
		if 'A' <= r && r <= 'Z' {
			return r + ('a' - 'A')
		}
		return r
	}, text)
}
