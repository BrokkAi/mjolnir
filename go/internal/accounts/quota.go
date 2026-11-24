// Port of mj-accounts/src/quota.rs.

// One-pane quota collection for Mjolnir harness profiles.

package accounts

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"maps"
	"math"
	"net/http"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"time"
	// chrono-tz carries the IANA zone database; so does the Go build, so a
	// host without zoneinfo still reads a reset stated in a named zone.
	_ "time/tzdata"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
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
	hash := sha256.New()
	// serde_json writes the (profile id, harness, environment) tuple as an
	// array, the environment's keys in order.
	environment := make(map[string]any, len(r.Environment))
	for key, value := range r.Environment {
		environment[key] = value
	}
	configuration, err := jsonvalue.Marshal([]any{r.ProfileID, string(r.Harness), environment})
	if err != nil {
		panic(fmt.Sprintf("serializable profile: %v", err))
	}
	hash.Write(configuration)
	hash.Write([]byte(r.SourceHome))
	// The ChatGPT login hashes nothing more, so its stored reports stay
	// valid. Every other source gets its own key, so a report from a
	// different source (including one read through the ChatGPT login before
	// the source was decided correctly) is never reused.
	switch source := r.CodexSource.(type) {
	case core.CodexQuotaSourceChatGptLogin:
	case core.CodexQuotaSourceProvider:
		hash.Write([]byte(source.Value.Host))
		hash.Write([]byte(source.Value.APIKey))
	case core.CodexQuotaSourceUsagePriced:
		hash.Write([]byte("usage-priced\x00"))
		hash.Write([]byte(source.Provider))
	case core.CodexQuotaSourceUnreadable:
		hash.Write([]byte("unreadable\x00"))
	}
	if r.OpencodeUsagePriced {
		hash.Write([]byte{1})
	} else {
		hash.Write([]byte{0})
	}
	return core.LowerHex(hash.Sum(nil))
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
	environment := maps.Clone(profile.Environment.Resolved())
	if environment == nil {
		environment = map[string]string{}
	}
	profile.Kind.ConfigureProfileHomeEnvironment(profile.Home, core.HarnessHostCurrent(), environment)
	// A profile with no stored OpenCode login is usage-priced exactly when
	// the environment supplies a provider key (`mj doctor` and the sign-in
	// status recognize the same signal,
	// HarnessProfile.OpencodeEnvironmentLogins). A profile with a stored
	// login is usage-priced too when every stored credential is itself a
	// plain API key (`value.type == "key"`, whether exported from the
	// environment or typed into `opencode auth login`); any stored OAuth
	// login, for this provider or another, leaves billing unknown, because
	// OpenCode's own login database does not say which credential the
	// session actually uses.
	opencodeUsagePriced := false
	if profile.Kind == core.HarnessKindOpenCode {
		logins, err := ProfileLogins(profile)
		switch {
		case err != nil:
		case logins == nil:
			opencodeUsagePriced = len(profile.OpencodeEnvironmentLogins()) != 0
		default:
			parsed, err := jsonvalue.Parse(logins)
			items, isArray := parsed.([]any)
			if err == nil && isArray {
				opencodeUsagePriced = true
				for _, login := range items {
					value, _ := quotaJSONGet(login, "value")
					kind, _ := quotaJSONGet(value, "type")
					if text, ok := kind.(string); !ok || text != "key" {
						opencodeUsagePriced = false
						break
					}
				}
			}
		}
	}
	return QuotaRefreshRequest{
		ProfileID:           profileID,
		Harness:             profile.Kind,
		AuthScheme:          profile.AuthScheme(),
		SourceHome:          profile.Home,
		Environment:         environment,
		Cwd:                 cwd,
		CodexSource:         core.CodexQuotaSourceOfProfile(*profile),
		OpencodeUsagePriced: opencodeUsagePriced,
	}
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
	delete(m.reports, profileID)
	if client, ok := m.codexClients[profileID]; ok {
		delete(m.codexClients, profileID)
		client.Shutdown(ctx)
	}
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
	cacheRoot := m.cacheRoot

	type probed struct {
		outcome QuotaRefreshOutcome
		client  *CodexUsageClient
		// failure is a probe that panicked (Rust: the task's JoinError).
		failure any
	}
	results := make(chan probed, len(requests))
	for _, request := range requests {
		client := m.codexClients[request.ProfileID]
		delete(m.codexClients, request.ProfileID)
		go func() {
			defer func() {
				if failure := recover(); failure != nil {
					results <- probed{failure: failure}
				}
			}()
			outcome, client := refreshProfileWithCacheRoot(ctx, request, client, cacheRoot)
			results <- probed{outcome: outcome, client: client}
		}()
	}

	for range requests {
		result := <-results
		if result.failure != nil {
			slog.Warn("quota refresh task failed", "error", quotaDisplay(fmt.Sprint(result.failure)))
			continue
		}
		outcome := result.outcome
		if result.client != nil {
			if m.codexClients == nil {
				m.codexClients = map[string]*CodexUsageClient{}
			}
			m.codexClients[outcome.Report.ProfileID] = result.client
		}
		previous := m.Report(outcome.Report.ProfileID)
		if outcome.Report.RateLimitedUntilEpochSeconds != nil {
			outcome = QuotaRefreshOutcome{
				CacheIdentity:      outcome.CacheIdentity,
				Report:             HoldReport(previous, outcome.Report),
				CredentialsChanged: outcome.CredentialsChanged,
			}
		}
		logQuotaChange(previous, &outcome.Report)
		m.Seed(outcome.Report)
		onReport(outcome)
	}
}

// stopClientsOutsideBatch stops the cached clients whose profiles are not in
// keep. Every batch carries the whole configured set, so a client left over
// from an earlier batch belongs to a profile the configuration no longer
// has. Each one owns a live `codex app-server` child that nothing would ever
// hand back to a refresh again, so it would run until the controller exits.
func (m *QuotaManager) stopClientsOutsideBatch(ctx context.Context, keep map[string]struct{}) {
	var stranded []string
	for profileID := range m.codexClients {
		if _, kept := keep[profileID]; !kept {
			stranded = append(stranded, profileID)
		}
	}
	for _, profileID := range stranded {
		client := m.codexClients[profileID]
		delete(m.codexClients, profileID)
		slog.Info("stopping the quota client of a removed profile", "profile_id", profileID)
		client.Shutdown(ctx)
	}
}

func (m *QuotaManager) Shutdown(ctx context.Context) {
	clients := m.codexClients
	m.codexClients = nil
	for _, client := range clients {
		client.Shutdown(ctx)
	}
}

// logQuotaChange logs why a profile's quota could not be read, and when it
// can be read again.
//
// A failed probe becomes the report's `error`, and the panes show only a
// short label for it ("unavailable", "login expired", "rate limited"), so
// without this line the reason was recorded nowhere (R12-2). A profile that
// keeps failing the same way is logged once, not on every refresh.
func logQuotaChange(previous *client.ProfileQuota, report *client.ProfileQuota) {
	if report.RateLimitedUntilEpochSeconds != nil {
		until := *report.RateLimitedUntilEpochSeconds
		// A hold is logged once, when it starts. The provider's hold and the
		// next scheduled check differ when the hold is shorter than the
		// regular interval.
		if previous == nil || previous.RateLimitedUntilEpochSeconds == nil || *previous.RateLimitedUntilEpochSeconds != until {
			now := core.EpochSeconds()
			nextCheck, ok := report.NextCheckAt()
			if !ok {
				nextCheck = until
			}
			slog.Info("the provider rate limited the quota probe; holding this profile's probes",
				"profile_id", quotaDisplay(report.ProfileID),
				"harness", report.Harness.DisplayName(),
				"provider_hold_seconds", quotaSaturatingSubUint64(until, now),
				"next_check_in_seconds", quotaSaturatingSubUint64(nextCheck, now),
			)
		}
		return
	}
	var previousError *string
	if previous != nil {
		previousError = previous.Error
	}
	switch {
	case report.Error != nil && (previousError == nil || *previousError != *report.Error):
		shownAs, _ := report.ErrorLabel()
		slog.Info("could not read the profile's quota",
			"profile_id", quotaDisplay(report.ProfileID),
			"harness", report.Harness.DisplayName(),
			"shown_as", shownAs,
			"error", *report.Error,
		)
	case report.Error == nil && previousError != nil:
		slog.Info("the profile's quota can be read again",
			"profile_id", quotaDisplay(report.ProfileID),
			"harness", report.Harness.DisplayName(),
		)
	}
}

// quotaDisplay is a log field value written as Rust's tracing writes a `%`
// (Display) field: as it is, not quoted as a string field is.
type quotaDisplay string

func (d quotaDisplay) String() string { return string(d) }

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
	if request.Harness != core.HarnessKindClaude || !request.AuthScheme.UsesNativeLoginFile() {
		return ClaudeGrantCheckNotApplicable{}
	}
	environment := maps.Clone(request.Environment)
	if environment == nil {
		environment = map[string]string{}
	}
	var source LoginSource
	switch store := request.Harness.LoginStore(core.HarnessHostCurrent(), request.SourceHome).(type) {
	case core.HarnessLoginStoreMacKeychain:
		// A Keychain login is renewed by Claude Code on this host, never in
		// a container: the image cannot write the Keychain. A host that
		// cannot run Claude Code fails visibly in RefreshGrantIfDue.
		lockMarker, err := store.LockMarker()
		if err != nil {
			slog.Warn("could not find where Claude Code locks the Keychain login",
				"profile_id", quotaDisplay(request.ProfileID),
				"error", quotaDisplay(err.Error()),
			)
			return ClaudeGrantCheckFailed{}
		}
		source = LoginSourceKeychain{
			LockMarker: lockMarker,
			// `security` can wait on a Keychain prompt; the same bound as
			// the other host lookups. The login exchange itself runs under
			// no deadline.
			Security: core.NewBoundedProcessExecutor(KeychainReadTimeout),
		}
	default:
		source = LoginSourceFile{Home: request.SourceHome}
	}
	// No deadline and no cancellation: a started login exchange runs to its
	// own end (see runClaudeRefresh).
	executor := core.ProcessExecutor{}
	refresh, err := RefreshGrantIfDue(ctx, source, environment, executor, launcher, gate, GrantRefreshTrigger{
		NowMs:          nowMs,
		RejectedLogins: rejectedLogins,
	})
	if err != nil {
		slog.Warn("could not refresh the Claude login",
			"profile_id", quotaDisplay(request.ProfileID),
			"error", quotaDisplay(err.Error()),
		)
		return ClaudeGrantCheckFailed{}
	}
	switch refresh := refresh.(type) {
	case GrantRefreshNotRefreshable:
		return ClaudeGrantCheckWaiting{}
	case GrantRefreshDeferred:
		// Due, but a handoff is waiting: the next daemon decides again.
		return ClaudeGrantCheckDeferred{}
	case GrantRefreshCurrent:
		dueAt := refresh.DueAtMs
		return ClaudeGrantCheckWaiting{DueAtMs: &dueAt}
	case GrantRefreshRenewed:
		expires := "None"
		var dueAt *int64
		if refresh.ExpiresAtMs != nil {
			expires = fmt.Sprintf("Some(%d)", *refresh.ExpiresAtMs)
			due := GrantDueAt(*refresh.ExpiresAtMs)
			dueAt = &due
		}
		slog.Info("refreshed the Claude login",
			"profile_id", quotaDisplay(request.ProfileID),
			"new_expires_at_ms", quotaDisplay(expires),
			"refreshed_by_this_daemon", refresh.ByThisCall,
		)
		// The credential sync pushes the renewed login to sessions.
		return ClaudeGrantCheckRenewed{DueAtMs: dueAt}
	}
	panic(fmt.Sprintf("unknown grant refresh %T", refresh))
}

func refreshProfileWithCacheRoot(ctx context.Context, request QuotaRefreshRequest, codexClient *CodexUsageClient, cacheRoot *string) (QuotaRefreshOutcome, *CodexUsageClient) {
	cacheIdentity := request.CacheIdentity()
	var credentialPath string
	if request.Harness == core.HarnessKindKimi {
		runtime, err := KimiRuntimeAuthResolve(request.SourceHome, request.Environment)
		if err != nil {
			slog.Warn("could not resolve Kimi Code credentials",
				"profile_id", quotaDisplay(request.ProfileID),
				"error", quotaDisplay(err.Error()),
			)
			credentialPath = core.HarnessAuthenticationMarker(request.Harness, request.SourceHome)
		} else {
			credentialPath = runtime.CredentialsPath
		}
	} else {
		credentialPath = core.HarnessAuthenticationMarker(request.Harness, request.SourceHome)
	}
	// Only the ChatGPT login uses a Codex app-server. A client cached while
	// the profile used it is stopped here rather than handed back.
	if _, chatGPT := request.CodexSource.(core.CodexQuotaSourceChatGptLogin); !chatGPT && codexClient != nil {
		codexClient.Shutdown(ctx)
		codexClient = nil
	}
	credentialBefore, credentialBeforeErr := credentialMarkerFingerprint(ctx, credentialPath)
	profileID, harness, sourceHome, cwd := request.ProfileID, request.Harness, request.SourceHome, request.Cwd
	environment := maps.Clone(request.Environment)
	if environment == nil {
		environment = map[string]string{}
	}
	refreshedAtEpochSeconds := uint64(max(time.Now().Unix(), 0))
	// A service that publishes no quota endpoint bills by usage, so there is
	// no allowance to report. Saying "API" rather than raising an error
	// keeps the dashboard from showing the profile as unavailable and lets
	// the utility ranker treat it as healthy, which matches how it actually
	// behaves.
	labelled := func(label string) client.ProfileQuota {
		return client.ProfileQuota{
			ProfileID:               profileID,
			Harness:                 harness,
			Windows:                 []client.QuotaWindow{},
			Extra:                   &label,
			RefreshedAtEpochSeconds: refreshedAtEpochSeconds,
		}
	}
	usagePriced := func() client.ProfileQuota { return labelled(APILabel) }
	// A harness that never shares its quota has nothing to probe and nothing
	// that failed, so it says `n/a` instead of reading as unavailable.
	notReported := func() client.ProfileQuota { return labelled(NotReportedLabel) }
	reading := func(windows []client.QuotaWindow) client.ProfileQuota {
		if windows == nil {
			windows = []client.QuotaWindow{}
		}
		return client.ProfileQuota{
			ProfileID:               profileID,
			Harness:                 harness,
			Windows:                 windows,
			RefreshedAtEpochSeconds: refreshedAtEpochSeconds,
		}
	}

	var report client.ProfileQuota
	var err error
	switch harness {
	// A Codex profile that authenticates with an API key against a custom
	// provider has no ChatGPT login to refresh and no ChatGPT rate-limit
	// windows to read. Its quota, when the provider publishes one, comes
	// from the provider's own endpoint.
	case core.HarnessKindCodex:
		switch source := request.CodexSource.(type) {
		case core.CodexQuotaSourceProvider:
			if !ServesQuota(source.Value.Host) {
				report = usagePriced()
				break
			}
			var windows []ZaiUsageWindow
			windows, err = ZaiUsageQuery(ctx, source.Value.Host, source.Value.APIKey)
			if err == nil {
				quotaWindows := make([]client.QuotaWindow, 0, len(windows))
				for _, window := range windows {
					remaining := window.RemainingPercent
					quotaWindows = append(quotaWindows, client.QuotaWindow{
						Label:                window.Label,
						RemainingPercent:     &remaining,
						Used:                 window.Used,
						Limit:                window.Limit,
						Resets:               quotaFormatResetLocalSecondsOf(window.ResetsAt),
						ResetsAtEpochSeconds: window.ResetsAt,
					})
				}
				report = reading(quotaWindows)
			}
		case core.CodexQuotaSourceUsagePriced:
			report = usagePriced()
		case core.CodexQuotaSourceUnreadable:
			err = fmt.Errorf("could not read the Codex configuration: %s", source.Value)
		default:
			var codex CodexUsageReport
			codex, err = refreshCodexChatGPT(ctx, &codexClient, profileID, credentialPath, cwd, environment, cacheRoot)
			if err == nil {
				var windows []client.QuotaWindow
				for _, window := range []*CodexUsageWindow{codex.Primary, codex.Secondary} {
					if window == nil {
						continue
					}
					remaining := window.RemainingPercent
					windows = append(windows, client.QuotaWindow{
						Label:                window.Label,
						RemainingPercent:     &remaining,
						DurationSeconds:      window.DurationSeconds,
						Resets:               quotaFormatResetLocalSecondsOf(window.ResetsAt),
						ResetsAtEpochSeconds: window.ResetsAt,
					})
				}
				report = reading(windows)
				report.PaidCredits = codex.PaidCredits
				report.MonthlyPriceUsd = codex.MonthlyPriceUsd
				report.BankedResets = codex.BankedResets
			}
		}
	case core.HarnessKindClaude:
		// Bedrock bills through AWS and has no Claude subscription allowance.
		if _, aws := request.AuthScheme.(core.AuthSchemeAwsCredentialChain); aws {
			report = usagePriced()
			break
		}
		var claude ClaudeUsageReport
		claude, err = ClaudeUsageQuery(ctx, sourceHome, environment)
		if err != nil {
			var limited ClaudeUsageErrorRateLimited
			if errors.As(err, &limited) {
				err = &RateLimited{RetryAfter: limited.RetryAfter}
			} else {
				err = errors.New(err.Error())
			}
			break
		}
		var windows []client.QuotaWindow
		claudeWindow := func(label string, window *ClaudeUsageWindow, scope *string) client.QuotaWindow {
			remaining := window.RemainingPercent
			quotaWindow := client.QuotaWindow{
				Label:            label,
				RemainingPercent: &remaining,
				DurationSeconds:  window.DurationSeconds,
				ModelScope:       scope,
			}
			if window.ResetContext != nil {
				quotaWindow.Resets = NormalizeResetText(*window.ResetContext)
				quotaWindow.ResetsAtEpochSeconds = NormalizeResetEpochSeconds(*window.ResetContext)
			}
			return quotaWindow
		}
		if claude.FiveHour != nil {
			windows = append(windows, claudeWindow("5H", claude.FiveHour, nil))
		}
		if claude.Week != nil {
			windows = append(windows, claudeWindow("Week", claude.Week, nil))
		}
		for _, scoped := range claude.ScopedWeeks {
			labelScope := scoped.Scope
			if first, size := utf8.DecodeRuneInString(labelScope); size > 0 && 'a' <= first && first <= 'z' {
				labelScope = string(first-'a'+'A') + labelScope[size:]
			}
			scope := scoped.Scope
			windows = append(windows, claudeWindow(fmt.Sprintf("Week (%s)", labelScope), &scoped.Window, &scope))
		}
		report = reading(windows)
		report.PaidCredits = claude.PaidCredits
		report.MonthlyPriceUsd = claude.MonthlyPriceUsd
		report.BankedResets = claude.BankedResets
	case core.HarnessKindKimi:
		var windows []client.QuotaWindow
		var extra *string
		windows, extra, err = queryKimi(ctx, sourceHome, environment)
		if err == nil {
			report = reading(windows)
			report.Extra = extra
		}
	// Grok Build publishes no HTTP quota endpoint. Its own usage view polls
	// an ACP billing extension, and so does Mjolnir.
	case core.HarnessKindGrok:
		var grok GrokUsageReport
		grok, err = GrokUsageQuery(ctx, cwd, environment)
		if err != nil {
			err = errors.New(err.Error())
			break
		}
		remaining := grok.RemainingPercent()
		report = reading([]client.QuotaWindow{{
			Label:            grok.PeriodLabel,
			RemainingPercent: &remaining,
			// Grok Build reports a share of the allowance, not the credit
			// amounts behind it.
			Resets:               quotaFormatResetLocalSecondsOf(grok.ResetsAt),
			ResetsAtEpochSeconds: grok.ResetsAt,
		}})
	case core.HarnessKindMuse:
		var muse MuseUsageReport
		muse, err = MuseUsageQuery(ctx, sourceHome, environment)
		if err != nil {
			break
		}
		var windows []client.QuotaWindow
		switch usage := muse.Usage.(type) {
		case MuseUsageOpen:
			for _, window := range usage.Value {
				remaining := window.RemainingPercent
				windows = append(windows, client.QuotaWindow{
					Label:                window.Label,
					RemainingPercent:     &remaining,
					DurationSeconds:      window.DurationSeconds,
					Resets:               quotaFormatResetLocalSecondsOf(window.ResetsAt),
					ResetsAtEpochSeconds: window.ResetsAt,
				})
			}
		case MuseUsageIdle:
			history := recentQuotaHistory(ctx, cacheIdentity, profileID, idleHistorySince(refreshedAtEpochSeconds))
			windows = idleWindows(history, refreshedAtEpochSeconds)
		}
		report = reading(windows)
		report.Extra = muse.Note
		if report.Extra == nil && len(windows) == 0 {
			idle := IdleLabel
			report.Extra = &idle
		}
	// OpenCode reads no allowance from any provider: rate-limit headers
	// reach it only on failed requests. A profile signed in through an
	// environment-supplied provider key (OpenRouter and the like) is still
	// billed by usage even though OpenCode never shares it, so it gets the
	// same `API` label as Dsh rather than reading as unavailable.
	case core.HarnessKindOpenCode:
		if request.OpencodeUsagePriced {
			report = usagePriced()
		} else {
			report = notReported()
		}
	// dsh-acp talks to DeepSeek's API, which bills by usage.
	case core.HarnessKindDsh:
		report = usagePriced()
	case core.HarnessKindAntigravity:
		var windows []client.QuotaWindow
		windows, err = AntigravityUsageQuery(ctx, sourceHome, environment)
		if err == nil {
			report = reading(windows)
		}
	default:
		err = fmt.Errorf("unknown harness %q", harness)
	}
	if err != nil {
		report = client.ProfileQuota{
			ProfileID:               profileID,
			Harness:                 harness,
			Windows:                 []client.QuotaWindow{},
			RefreshedAtEpochSeconds: refreshedAtEpochSeconds,
		}
		message := err.Error()
		report.Error = &message
		var limited *RateLimited
		if errors.As(err, &limited) {
			until, nextCheck := RefusedCheckSchedule(refreshedAtEpochSeconds, limited.RetryAfter)
			report.RateLimitedUntilEpochSeconds = &until
			report.NextCheckAtEpochSeconds = &nextCheck
		}
	}
	// Only refresh attempts reach this point, not reports adopted from the
	// daemon's reset cache. A reader can still reuse its recent provider
	// data. Persist failures before the manager substitutes old windows for
	// display. Only the daemon owns the writer; other callers leave
	// persistence to it.
	if store.DatabaseWriterInstalled() {
		observed := report
		if err := store.SaveQuotaReport(cacheIdentity, &observed); err != nil {
			slog.Warn("could not preserve quota history and reset times",
				"profile_id", quotaDisplay(report.ProfileID),
				"error", quotaDisplay(err.Error()),
			)
		}
	} else {
		slog.Debug("this process has no database writer; leaving quota history and reset times to the daemon",
			"profile_id", quotaDisplay(report.ProfileID),
		)
	}
	credentialAfter, credentialAfterErr := credentialMarkerFingerprint(ctx, credentialPath)
	credentialsChanged := false
	switch {
	case credentialBeforeErr == nil && credentialAfterErr == nil:
		credentialsChanged = !quotaEqualOptional(credentialBefore, credentialAfter)
	default:
		failure := credentialBeforeErr
		if failure == nil {
			failure = credentialAfterErr
		}
		slog.Warn("could not fingerprint quota credentials",
			"path", quotaDisplay(credentialPath),
			"error", quotaDisplay(failure.Error()),
		)
	}
	return QuotaRefreshOutcome{
		CacheIdentity:      cacheIdentity,
		Report:             report,
		CredentialsChanged: credentialsChanged,
	}, codexClient
}

// refreshCodexChatGPT reads a Codex profile's ChatGPT allowance through its
// app-server, rotating a login near its expiry first (the ChatGPT-login arm
// of Rust's refresh_profile_with_cache_root).
func refreshCodexChatGPT(ctx context.Context, codexClient **CodexUsageClient, profileID, credentialPath, cwd string, environment map[string]string, cacheRoot *string) (CodexUsageReport, error) {
	launcher := CodexLauncherHost
	// Only tests set a cache root: they run the managed installation's
	// entrypoint under it directly.
	if cacheRoot != nil {
		launcher = codexLauncherProgram(filepath.Join(core.ManagedHarnessInstallDir(*cacheRoot, core.HarnessKindCodex), core.CodexCLIEntrypoint))
	}
	// Installation touches no credentials; finish it before reserving the
	// login slot used by credential synchronization. A program borrowed
	// from the cached client belongs to that client; one prepared here is
	// released unless a client this poll starts takes it.
	var prepared *quotaProgram
	owned := false
	if *codexClient != nil {
		prepared = (*codexClient).preparedProgram(launcher)
	}
	if prepared == nil {
		program, err := launcher.prepare(ctx, environment)
		if err != nil {
			return CodexUsageReport{}, err
		}
		prepared, owned = program, true
	}
	defer func() {
		if owned && prepared != nil {
			prepared.release()
		}
	}()
	// Serialize the expiry decision and every app-server request with other
	// daemons and credential syncs using this login slot. Waiting for
	// ownership is outside the RPC request timeout.
	credentialGuard, err := core.CredentialWriteGuardAcquire(ctx, core.HarnessKindCodex, credentialPath)
	if err != nil {
		return CodexUsageReport{}, err
	}
	defer func() {
		if credentialGuard != nil {
			credentialGuard.Release()
		}
	}()
	// A poll starts at most one app-server. A failed rotation drops its
	// client. When that client was cached from an earlier poll, a new one
	// may still read the quota, which is how a login that the old process
	// gave up on recovers (#1224). When this poll started it, another start
	// would only repeat the failure, so the next poll tries again instead.
	var failedRotation *string
	if codexLoginIsNearExpiry(ctx, credentialPath) {
		startedHere := *codexClient == nil
		failure := refreshLoginWithCacheRoot(ctx, codexClient, cwd, maps.Clone(environment), launcher, &prepared)
		if failure == nil {
			slog.Info("refreshed Codex login ahead of expiry", "profile_id", quotaDisplay(profileID))
		} else {
			slog.Warn("could not refresh the Codex login ahead of expiry",
				"profile_id", quotaDisplay(profileID),
				"error", quotaDisplay(failure.Detail),
			)
			if *codexClient == nil {
				if startedHere {
					reason := failure.Reason
					failedRotation = &reason
				} else {
					// The failed cached process has stopped. Drop its lease
					// and login admission before preparing a replacement,
					// which may need a fresh install.
					if owned && prepared != nil {
						prepared.release()
					}
					prepared, owned = nil, false
					credentialGuard.Release()
					credentialGuard = nil
					program, err := launcher.prepare(ctx, environment)
					if err != nil {
						return CodexUsageReport{}, err
					}
					prepared, owned = program, true
					credentialGuard, err = core.CredentialWriteGuardAcquire(ctx, core.HarnessKindCodex, credentialPath)
					if err != nil {
						return CodexUsageReport{}, err
					}
				}
			}
		}
	}
	var status CodexUsageStatus
	if failedRotation != nil {
		status = CodexUsageStatusUnavailable{Reason: *failedRotation}
	} else {
		status = refreshWithCacheRoot(ctx, codexClient, cwd, environment, launcher, &prepared)
	}
	credentialGuard.Release()
	credentialGuard = nil
	switch status := status.(type) {
	case CodexUsageStatusAvailable:
		return status.Report, nil
	case CodexUsageStatusUnavailable:
		return CodexUsageReport{}, errors.New(status.Reason)
	}
	panic(fmt.Sprintf("unknown Codex usage status %T", status))
}

// recentQuotaHistory is this profile's probes saved since since, newest
// first. Only the process that saves probes reads them back: elsewhere there
// is no history, and a failed read is logged and treated as none, which
// shows less rather than something stale.
func recentQuotaHistory(ctx context.Context, identity string, profileID string, since uint64) []client.ProfileQuota {
	if !store.DatabaseWriterInstalled() {
		return nil
	}
	history, err := store.LoadQuotaHistorySince(identity, profileID, since)
	if err != nil {
		slog.Warn("could not read the stored quota probes",
			"profile_id", profileID,
			"error", quotaDisplay(err.Error()),
		)
		return nil
	}
	return history
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
	info, err := os.Stat(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("inspect credential marker: %w", err)
	}
	if info.Size() > core.MaxCredentialBytes {
		return nil, fmt.Errorf("credential marker exceeds %d bytes", core.MaxCredentialBytes)
	}
	bytes, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read credential marker: %w", err)
	}
	if len(bytes) > core.MaxCredentialBytes {
		return nil, fmt.Errorf("credential marker exceeds %d bytes", core.MaxCredentialBytes)
	}
	fingerprint := core.CredentialFingerprint(bytes)
	return &fingerprint, nil
}

func queryKimi(ctx context.Context, home string, environment map[string]string) ([]client.QuotaWindow, *string, error) {
	runtime, err := KimiRuntimeAuthResolve(home, maps.Clone(environment))
	if err != nil {
		return nil, nil, err
	}
	httpClient := &http.Client{Timeout: 10 * time.Second}
	usageURL := runtime.BaseURL + "/usages"
	var response *http.Response
	if apiKey, ok := environment["KIMI_API_KEY"]; ok && strings.TrimSpace(apiKey) != "" {
		response, err = fetchBearer(ctx, httpClient, usageURL, strings.TrimSpace(apiKey))
	} else {
		response, err = fetchBearerWithAuthRetry(ctx, httpClient, usageURL, func(ctx context.Context, force bool, rejectedToken *string) (string, error) {
			return EnsureFreshKimiToken(ctx, httpClient, &runtime, force, rejectedToken)
		})
	}
	if err != nil {
		return nil, nil, err
	}
	defer response.Body.Close()
	if response.StatusCode == http.StatusTooManyRequests {
		limited := &RateLimited{}
		if retryAfter, ok := core.RetryAfterHeader(response.Header); ok {
			limited.RetryAfter = &retryAfter
		}
		return nil, nil, limited
	}
	if !quotaStatusSuccess(response.StatusCode) {
		return nil, nil, fmt.Errorf("Kimi Code quota returned HTTP %s", quotaHTTPStatus(response.StatusCode))
	}
	body, err := io.ReadAll(response.Body)
	if err != nil {
		return nil, nil, fmt.Errorf("decode Kimi Code quota: %w", err)
	}
	payload, err := jsonvalue.Parse(body)
	if err != nil {
		return nil, nil, fmt.Errorf("decode Kimi Code quota: %w", err)
	}
	windows, extra := parseKimiUsage(payload)
	return windows, extra, nil
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
	initial, err := readKimiCredentials(ctx, auth.CredentialsPath)
	if err != nil {
		return "", err
	}
	if !force && !initial.needsRefresh() {
		return initial.AccessToken, nil
	}

	// Held until this function returns.
	refreshLock, err := core.CredentialWriteGuardAcquire(ctx, core.HarnessKindKimi, auth.CredentialsPath)
	if err != nil {
		return "", err
	}
	defer refreshLock.Release()
	active, err := readKimiCredentials(ctx, auth.CredentialsPath)
	if err != nil {
		return "", err
	}
	changedWhileWaiting := active.differsFrom(&initial)
	if (!force && !active.needsRefresh()) ||
		(force && (changedWhileWaiting || (rejectedToken != nil && *rejectedToken != active.AccessToken))) {
		return active.AccessToken, nil
	}
	if active.RefreshToken == "" {
		return "", errors.New("Kimi Code refresh token is missing; run `kimi login`")
	}

	request, err := http.NewRequestWithContext(ctx, http.MethodPost, auth.OAuthHost+"/api/oauth/token", strings.NewReader(quotaFormEncode([][2]string{
		{"client_id", kimiOauthClientID},
		{"grant_type", "refresh_token"},
		{"refresh_token", active.RefreshToken},
	})))
	if err != nil {
		return "", fmt.Errorf("refresh Kimi Code access token: %w", err)
	}
	request.Header.Set("Accept", "application/json")
	request.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	response, err := httpClient.Do(request)
	if err != nil {
		return "", fmt.Errorf("refresh Kimi Code access token: %w", err)
	}
	defer response.Body.Close()
	if !quotaStatusSuccess(response.StatusCode) {
		status := response.StatusCode
		if status == http.StatusUnauthorized || status == http.StatusForbidden {
			select {
			case <-ctx.Done():
				return "", ctx.Err()
			case <-time.After(100 * time.Millisecond):
			}
			recovery, err := readKimiCredentials(ctx, auth.CredentialsPath)
			if err != nil {
				return "", err
			}
			if recovery.RefreshToken != active.RefreshToken && recovery.AccessToken != "" {
				return recovery.AccessToken, nil
			}
		}
		return "", fmt.Errorf("Kimi Code token refresh returned HTTP %s", quotaHTTPStatus(status))
	}

	body, err := io.ReadAll(response.Body)
	if err != nil {
		return "", fmt.Errorf("decode Kimi Code token refresh: %w", err)
	}
	payload, err := jsonvalue.Parse(body)
	if err != nil {
		return "", fmt.Errorf("decode Kimi Code token refresh: %w", err)
	}
	accessToken, err := requiredString(payload, "access_token", "Kimi Code token refresh")
	if err != nil {
		return "", err
	}
	refreshToken, err := requiredString(payload, "refresh_token", "Kimi Code token refresh")
	if err != nil {
		return "", err
	}
	expiresIn, ok := int64(0), false
	if value, present := quotaJSONGet(payload, "expires_in"); present {
		expiresIn, ok = valueI64(value)
	}
	if !ok || expiresIn <= 0 {
		return "", errors.New("Kimi Code token refresh is missing expires_in")
	}
	scope, _ := quotaJSONGetString(payload, "scope")
	tokenType, ok := quotaJSONGetString(payload, "token_type")
	if !ok {
		tokenType = "Bearer"
	}
	refreshed := kimiCredentials{
		AccessToken:  accessToken,
		RefreshToken: refreshToken,
		ExpiresAt:    time.Now().Unix() + expiresIn,
		Scope:        scope,
		TokenType:    tokenType,
		ExpiresIn:    expiresIn,
	}
	if err := saveKimiCredentials(auth.CredentialsPath, &refreshed); err != nil {
		return "", err
	}
	return refreshed.AccessToken, nil
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
	if value, ok := quotaJSONGetString(payload, key); ok && value != "" {
		return value, nil
	}
	return "", fmt.Errorf("%s is missing %s", contextText, key)
}

func parseKimiUsage(payload any) ([]client.QuotaWindow, *string) {
	var windows []client.QuotaWindow
	labels := map[string]bool{}
	if summary, ok := quotaJSONGet(payload, "usage"); ok {
		if window := parseKimiWindow(summary, "Weekly limit"); window != nil {
			labels[window.Label] = true
			windows = append(windows, *window)
		}
	}
	if limits, ok := quotaJSONGet(payload, "limits"); ok {
		if items, ok := limits.([]any); ok {
			for index, item := range items {
				detail, ok := quotaJSONGet(item, "detail")
				if !ok {
					detail = item
				}
				fallback := kimiLimitLabel(item, index)
				if window := parseKimiWindow(detail, fallback); window != nil && !labels[window.Label] {
					labels[window.Label] = true
					windows = append(windows, *window)
				}
			}
		}
	}
	if usages, ok := quotaJSONGet(payload, "usages"); ok {
		if usages, ok := usages.(jsonvalue.Object); ok {
			// `limits[]` carries absolute counts while `usages` carries
			// ratios for the same windows. Keep the more precise absolute row
			// when both name the same window.
			for _, ratio := range []struct {
				keys  []string
				label string
			}{
				{[]string{"limit_5h"}, "5H"},
				{[]string{"limit_7d"}, "Week"},
				{[]string{"limit_month_total", "month_total"}, "Month"},
				{[]string{"limit_month_code", "month_code"}, "Month code"},
			} {
				var value any
				found := false
				for _, key := range ratio.keys {
					if value, found = usages.Get(key); found {
						break
					}
				}
				if !found {
					continue
				}
				if window := parseKimiRatioWindow(value, ratio.label); window != nil && !labels[window.Label] {
					labels[window.Label] = true
					windows = append(windows, *window)
				}
			}
		}
	}
	var extra *string
	if amount, ok := quotaJSONPointer(payload, "/boosterWallet/balance/amountLeft"); ok {
		if value, ok := valueI64(amount); ok {
			text := fmt.Sprintf("booster %d remaining", value/1_000_000)
			extra = &text
		}
	}
	return windows, extra
}

func kimiLimitLabel(item any, index int) string {
	var duration *int64
	if value, ok := quotaJSONPointer(item, "/window/duration"); ok {
		if parsed, ok := valueI64(value); ok {
			duration = &parsed
		}
	}
	if duration == nil {
		if value, ok := quotaJSONGet(item, "duration"); ok {
			if parsed, ok := valueI64(value); ok {
				duration = &parsed
			}
		}
	}
	var unitValue any
	unitFound := false
	for _, lookup := range []func() (any, bool){
		func() (any, bool) { return quotaJSONPointer(item, "/window/timeUnit") },
		func() (any, bool) { return quotaJSONPointer(item, "/window/time_unit") },
		func() (any, bool) { return quotaJSONGet(item, "timeUnit") },
		func() (any, bool) { return quotaJSONGet(item, "time_unit") },
	} {
		if unitValue, unitFound = lookup(); unitFound {
			break
		}
	}
	unit := ""
	if unitFound {
		if text, ok := unitValue.(string); ok {
			unit = quotaASCIIUpper(text)
		}
	}
	if duration != nil {
		seconds := *duration
		switch unit {
		case "TIME_UNIT_MINUTE", "MINUTE", "MINUTES":
			seconds = quotaSaturatingMulInt64(seconds, 60)
		case "TIME_UNIT_HOUR", "HOUR", "HOURS":
			seconds = quotaSaturatingMulInt64(seconds, 3_600)
		case "TIME_UNIT_DAY", "DAY", "DAYS":
			seconds = quotaSaturatingMulInt64(seconds, 86_400)
		}
		switch seconds {
		case 18_000:
			return "5H"
		case 604_800:
			return "Week"
		}
	} else if index == 0 {
		return "5H"
	}
	return fmt.Sprintf("Limit #%d", index+1)
}

func parseKimiWindow(value any, fallback string) *client.QuotaWindow {
	var limit, used *int64
	if field, ok := quotaJSONGet(value, "limit"); ok {
		if parsed, ok := valueI64(field); ok {
			limit = &parsed
		}
	}
	if field, ok := quotaJSONGet(value, "used"); ok {
		if parsed, ok := valueI64(field); ok {
			used = &parsed
		}
	}
	if used == nil {
		if field, ok := quotaJSONGet(value, "remaining"); ok {
			if remaining, ok := valueI64(field); ok && limit != nil {
				difference := *limit - remaining
				used = &difference
			}
		}
	}
	if used == nil && limit == nil {
		return nil
	}
	providerLabel := fallback
	name, ok := quotaJSONGet(value, "name")
	if !ok {
		name, ok = quotaJSONGet(value, "title")
	}
	if ok {
		if text, isText := name.(string); isText {
			providerLabel = text
		}
	}
	window := &client.QuotaWindow{
		Label: canonicalKimiLabel(providerLabel),
		Used:  used,
		Limit: limit,
	}
	if reset, ok := kimiResetValue(value); ok {
		window.Resets = normalizeKimiReset(reset)
		window.ResetsAtEpochSeconds = kimiResetEpochSeconds(reset)
	}
	if used != nil && limit != nil && *limit > 0 {
		remaining := uint8(100 - min(max(*used, 0), *limit)*100 / *limit)
		window.RemainingPercent = &remaining
	}
	return window
}

func parseKimiRatioWindow(value any, label string) *client.QuotaWindow {
	field, ok := quotaJSONGet(value, "used_ratio")
	if !ok {
		field, ok = quotaJSONGet(value, "usedRatio")
	}
	if !ok {
		return nil
	}
	usedRatio, ok := valueF64(field)
	if !ok || math.IsInf(usedRatio, 0) || math.IsNaN(usedRatio) {
		return nil
	}
	usedRatio = min(max(usedRatio, 0), 1)
	usedPercent := quotaF64ToUint8(min(max(math.Ceil(usedRatio*100), 0), 100))
	remaining := 100 - usedPercent
	window := &client.QuotaWindow{
		Label:            label,
		RemainingPercent: &remaining,
	}
	if reset, ok := kimiResetValue(value); ok {
		window.Resets = normalizeKimiReset(reset)
		window.ResetsAtEpochSeconds = kimiResetEpochSeconds(reset)
	}
	return window
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
	for _, key := range []string{"resetAt", "reset_at", "resetTime", "reset_time"} {
		if reset, ok := quotaJSONGet(value, key); ok {
			return reset, true
		}
	}
	return nil, false
}

// valueI64 is an integer, or a string holding one.
func valueI64(value any) (int64, bool) {
	if number, ok := quotaJSONI64(value); ok {
		return number, true
	}
	if text, ok := value.(string); ok {
		number, err := strconv.ParseInt(text, 10, 64)
		return number, err == nil
	}
	return 0, false
}

// valueF64 is a number, or a string holding one.
func valueF64(value any) (float64, bool) {
	if number, ok := quotaJSONF64(value); ok {
		return number, true
	}
	if text, ok := value.(string); ok {
		return quotaParseF64(text)
	}
	return 0, false
}

func normalizeKimiReset(value any) *string {
	if epoch, ok := quotaJSONF64(value); ok {
		if label := FormatResetLocal(epoch); label != nil {
			return label
		}
	}
	if text, ok := value.(string); ok {
		return NormalizeResetText(text)
	}
	return nil
}

func kimiResetEpochSeconds(value any) *int64 {
	if epoch, ok := quotaJSONF64(value); ok {
		seconds := quotaEpochSeconds(epoch)
		return &seconds
	}
	if text, ok := value.(string); ok {
		return NormalizeResetEpochSeconds(text)
	}
	return nil
}

// quotaEpochSeconds reads a Unix timestamp given in seconds or milliseconds
// as whole seconds, truncated toward zero.
func quotaEpochSeconds(epoch float64) int64 {
	if math.Abs(epoch) >= 1_000_000_000_000 {
		return quotaF64ToInt64(math.Trunc(epoch / 1000))
	}
	return quotaF64ToInt64(math.Trunc(epoch))
}

// FormatResetLocal formats a Unix reset timestamp as wall-clock time in the
// machine's local time zone. Accepts seconds or milliseconds and rejects
// non-finite or out-of-range values.
func FormatResetLocal(epoch float64) *string {
	if math.IsInf(epoch, 0) || math.IsNaN(epoch) {
		return nil
	}
	local, ok := quotaLocalTimestamp(quotaEpochSeconds(epoch))
	if !ok {
		return nil
	}
	label := formatResetLabel(local)
	return &label
}

// quotaLocalTimestamp is the instant seconds after the epoch in the local
// zone's offset at that instant (Rust's `Local.timestamp_opt(seconds, 0)`
// then `fixed_offset()`); false outside chrono's representable dates.
func quotaLocalTimestamp(seconds int64) (time.Time, bool) {
	if seconds < quotaChronoMinTimestamp || seconds > quotaChronoMaxTimestamp {
		return time.Time{}, false
	}
	local := time.Unix(seconds, 0).In(time.Local)
	_, offset := local.Zone()
	fixed := local.In(time.FixedZone("", offset))
	if year := fixed.Year(); year < quotaChronoMinYear || year > quotaChronoMaxYear {
		return time.Time{}, false
	}
	return fixed, true
}

// chrono's NaiveDate range: years -262144 through 262143.
const (
	quotaChronoMinYear = -262144
	quotaChronoMaxYear = 262143
)

var (
	quotaChronoMinTimestamp = time.Date(quotaChronoMinYear, 1, 1, 0, 0, 0, 0, time.UTC).Unix()
	quotaChronoMaxTimestamp = time.Date(quotaChronoMaxYear, 12, 31, 23, 59, 59, 0, time.UTC).Unix()
)

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
	value = strings.TrimSpace(value)
	if value == "" {
		return time.Time{}, false
	}
	if index := strings.LastIndexByte(value, '('); index >= 0 {
		clock, zone := value[:index], value[index+1:]
		name, closed := strings.CutSuffix(zone, ")")
		if !closed {
			return time.Time{}, false
		}
		location, ok := quotaLoadZone(strings.TrimSpace(name))
		if !ok {
			return time.Time{}, false
		}
		parsed, ok := NormalizeResetAt(strings.TrimSpace(clock), quotaFixed(now.In(location)))
		if !ok {
			return time.Time{}, false
		}
		year, month, day := parsed.Date()
		hour, minute, second := parsed.Clock()
		reset, ok := quotaZoneLocal(location, year, month, day, hour, minute, second, parsed.Nanosecond())
		if !ok {
			return time.Time{}, false
		}
		return quotaFixed(reset), true
	}
	if index := strings.LastIndexByte(value, ' '); index >= 0 {
		clock, offset := value[:index], value[index+1:]
		if strings.HasPrefix(offset, "+") || strings.HasPrefix(offset, "-") {
			seconds, ok := quotaParseFixedOffset(offset)
			if !ok {
				return time.Time{}, false
			}
			return NormalizeResetAt(clock, now.In(time.FixedZone("", seconds)))
		}
	}
	if clock, ok := strings.CutSuffix(value, " UTC"); ok {
		return NormalizeResetAt(clock, now.In(time.FixedZone("", 0)))
	}
	if clock, ok := strings.CutSuffix(value, " GMT"); ok {
		return NormalizeResetAt(clock, now.In(time.FixedZone("", 0)))
	}
	if epoch, ok := quotaParseF64(value); ok {
		return quotaLocalTimestamp(quotaEpochSeconds(epoch))
	}
	if timestamp, ok := core.ParseChronoRFC3339(value); ok {
		return quotaFixed(timestamp.In(time.Local)), true
	}

	value = strings.TrimPrefix(value, "at ")
	value, _, _ = strings.Cut(value, "(")
	value = strings.TrimRight(strings.TrimSpace(value), ",")

	// Claude has used both `Aug 14 at 4am` and `Aug 14, 4am` across
	// releases. Keep the provider punctuation out of the date/time parsers.
	date, clock, dated := strings.Cut(value, " at ")
	if !dated {
		date, clock, dated = strings.Cut(value, ",")
		clock = strings.TrimSpace(clock)
	}
	if dated {
		hour, minute, ok := quotaParseClock(strings.TrimSpace(clock))
		if !ok {
			return time.Time{}, false
		}
		date = strings.TrimRight(strings.TrimSpace(date), ",")
		var year int
		var month time.Month
		var day int
		switch quotaASCIILower(date) {
		case "today":
			year, month, day = now.Date()
		case "tomorrow":
			year, month, day = now.AddDate(0, 0, 1).Date()
		default:
			year, month, day, ok = quotaParseMonthDay(strings.ReplaceAll(date, ",", ""), now.Year())
			if !ok {
				return time.Time{}, false
			}
		}
		return time.Date(year, month, day, hour, minute, 0, 0, now.Location()), true
	}

	hour, minute, ok := quotaParseClock(value)
	if !ok {
		return time.Time{}, false
	}
	year, month, day := now.Date()
	reset := time.Date(year, month, day, hour, minute, 0, 0, now.Location())
	if !reset.After(now) {
		reset = time.Date(year, month, day+1, hour, minute, 0, 0, now.Location())
	}
	return reset, true
}

// quotaFixed is t in the fixed offset its zone has at that instant (Rust's
// `fixed_offset()`).
func quotaFixed(t time.Time) time.Time {
	_, offset := t.Zone()
	return t.In(time.FixedZone("", offset))
}

// quotaLoadZone is an IANA zone by name, as chrono-tz parses one: neither the
// empty name (UTC to Go) nor "Local" names a zone.
func quotaLoadZone(name string) (*time.Location, bool) {
	if name == "" || name == "Local" {
		return nil, false
	}
	location, err := time.LoadLocation(name)
	return location, err == nil
}

// quotaZoneLocal is the one instant whose wall clock in location is the
// given one (Rust's `from_local_datetime(..).single()`): false when the
// clock falls in a gap or, at a fold, names two instants.
func quotaZoneLocal(location *time.Location, year int, month time.Month, day, hour, minute, second, nanosecond int) (time.Time, bool) {
	naive := time.Date(year, month, day, hour, minute, second, nanosecond, time.UTC)
	var found []time.Time
	offsets := map[int]bool{}
	for _, probe := range []time.Duration{-24 * time.Hour, 0, 24 * time.Hour} {
		_, offset := naive.Add(probe).In(location).Zone()
		if offsets[offset] {
			continue
		}
		offsets[offset] = true
		candidate := naive.Add(-time.Duration(offset) * time.Second).In(location)
		cy, cm, cd := candidate.Date()
		ch, cmin, cs := candidate.Clock()
		if cy == year && cm == month && cd == day && ch == hour && cmin == minute && cs == second &&
			!slices.ContainsFunc(found, candidate.Equal) {
			found = append(found, candidate)
		}
	}
	if len(found) != 1 {
		return time.Time{}, false
	}
	return found[0], true
}

// quotaParseFixedOffset reads a `±HH:MM`, `±HHMM` or `±HH MM` offset as
// chrono's FixedOffset parses one (text after the minutes is ignored).
func quotaParseFixedOffset(text string) (int, bool) {
	if text == "" {
		return 0, false
	}
	negative := false
	switch text[0] {
	case '+':
	case '-':
		negative = true
	default:
		return 0, false
	}
	text = text[1:]
	digits := func(text string) (int, bool) {
		if len(text) < 2 || text[0] < '0' || text[0] > '9' || text[1] < '0' || text[1] > '9' {
			return 0, false
		}
		return int(text[0]-'0')*10 + int(text[1]-'0'), true
	}
	hours, ok := digits(text)
	if !ok {
		return 0, false
	}
	text = strings.TrimLeftFunc(text[2:], func(r rune) bool { return r == ':' || unicode.IsSpace(r) })
	minutes, ok := digits(text)
	if !ok || minutes > 59 {
		return 0, false
	}
	seconds := hours*3600 + minutes*60
	if seconds >= 86_400 {
		return 0, false
	}
	if negative {
		seconds = -seconds
	}
	return seconds, true
}

// quotaParseClock reads a wall-clock time as Claude and Kimi print one:
// `4:49pm`, `4pm`, `16:49` (Rust tries chrono's `%I:%M%P`, `%I%P` and
// `%H:%M` after writing a bare `4pm` as `4:00pm`).
func quotaParseClock(value string) (int, int, bool) {
	value = strings.Map(func(r rune) rune {
		if unicode.IsSpace(r) {
			return -1
		}
		return r
	}, quotaASCIILower(value))
	for _, suffix := range []string{"am", "pm"} {
		if hour, ok := strings.CutSuffix(value, suffix); ok {
			if !strings.Contains(hour, ":") {
				value = hour + ":00" + suffix
			}
			break
		}
	}
	// number reads one or two digits.
	number := func(text string) (int, string, bool) {
		end := 0
		for end < len(text) && end < 2 && text[end] >= '0' && text[end] <= '9' {
			end++
		}
		if end == 0 {
			return 0, text, false
		}
		parsed, _ := strconv.Atoi(text[:end])
		return parsed, text[end:], true
	}
	meridiem := func(text string) (bool, bool) {
		switch text {
		case "am":
			return false, true
		case "pm":
			return true, true
		}
		return false, false
	}
	twelveHour := func(hour int, pm bool) int {
		hour %= 12
		if pm {
			hour += 12
		}
		return hour
	}
	// %I:%M%P
	if hour, rest, ok := number(value); ok && hour >= 1 && hour <= 12 {
		if rest, ok := strings.CutPrefix(rest, ":"); ok {
			if minute, rest, ok := number(rest); ok && minute <= 59 {
				if pm, ok := meridiem(rest); ok {
					return twelveHour(hour, pm), minute, true
				}
			}
		}
	}
	// %I%P
	if hour, rest, ok := number(value); ok && hour >= 1 && hour <= 12 {
		if pm, ok := meridiem(rest); ok {
			return twelveHour(hour, pm), 0, true
		}
	}
	// %H:%M
	if hour, rest, ok := number(value); ok && hour <= 23 {
		if rest, ok := strings.CutPrefix(rest, ":"); ok {
			if minute, rest, ok := number(rest); ok && minute <= 59 && rest == "" {
				return hour, minute, true
			}
		}
	}
	return 0, 0, false
}

// quotaParseMonthDay reads `Aug 14` or `August 14` (chrono's `%b %e` with
// year appended), case-insensitively; false for a date that does not exist.
func quotaParseMonthDay(text string, year int) (int, time.Month, int, bool) {
	lower := quotaASCIILower(text)
	for month := time.January; month <= time.December; month++ {
		long := strings.ToLower(month.String())
		rest, ok := strings.CutPrefix(lower, long)
		if !ok {
			rest, ok = strings.CutPrefix(lower, long[:3])
		}
		if !ok {
			continue
		}
		rest = strings.TrimLeftFunc(rest, unicode.IsSpace)
		end := 0
		for end < len(rest) && end < 2 && rest[end] >= '0' && rest[end] <= '9' {
			end++
		}
		if end == 0 || strings.TrimLeftFunc(rest[end:], unicode.IsSpace) != "" {
			return 0, 0, 0, false
		}
		day, _ := strconv.Atoi(rest[:end])
		if day < 1 || time.Date(year, month, day, 0, 0, 0, 0, time.UTC).Day() != day {
			return 0, 0, 0, false
		}
		return year, month, day, true
	}
	return 0, 0, 0, false
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

// quotaASCIIUpper is Rust's str::to_ascii_uppercase.
func quotaASCIIUpper(text string) string {
	return strings.Map(func(r rune) rune {
		if 'a' <= r && r <= 'z' {
			return r - ('a' - 'A')
		}
		return r
	}, text)
}

// quotaSaturatingSubUint64 is Rust's u64::saturating_sub.
func quotaSaturatingSubUint64(a, b uint64) uint64 {
	if b > a {
		return 0
	}
	return a - b
}

// quotaSaturatingMulInt64 is Rust's i64::saturating_mul.
func quotaSaturatingMulInt64(a, b int64) int64 {
	if a == 0 || b == 0 {
		return 0
	}
	product := a * b
	if product/b != a || (a == -1 && b == math.MinInt64) || (b == -1 && a == math.MinInt64) {
		if (a < 0) != (b < 0) {
			return math.MinInt64
		}
		return math.MaxInt64
	}
	return product
}

// quotaF64ToInt64 is Rust's `f64 as i64`: saturating, NaN as zero.
func quotaF64ToInt64(value float64) int64 {
	switch {
	case math.IsNaN(value):
		return 0
	case value >= math.MaxInt64:
		return math.MaxInt64
	case value <= math.MinInt64:
		return math.MinInt64
	}
	return int64(value)
}

// quotaF64ToUint8 is Rust's `f64 as u8`: saturating, NaN as zero.
func quotaF64ToUint8(value float64) uint8 {
	switch {
	case math.IsNaN(value) || value <= 0:
		return 0
	case value >= math.MaxUint8:
		return math.MaxUint8
	}
	return uint8(value)
}

// quotaParseF64 is Rust's `str::parse::<f64>`: decimal text with an
// optional sign and exponent, `inf`, `infinity` or `nan`; a magnitude past
// f64's range is infinity rather than an error.
func quotaParseF64(text string) (float64, bool) {
	body := text
	if strings.HasPrefix(body, "+") || strings.HasPrefix(body, "-") {
		body = body[1:]
	}
	if strings.ContainsRune(body, '_') || strings.HasPrefix(quotaASCIILower(body), "0x") {
		return 0, false
	}
	value, err := strconv.ParseFloat(text, 64)
	if err != nil {
		if errors.Is(err, strconv.ErrRange) {
			return value, true
		}
		return 0, false
	}
	return value, true
}

func quotaEqualOptional(a, b *string) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return *a == *b
}

// quotaFormatResetLocalSecondsOf is FormatResetLocalSeconds of an optional
// reset.
func quotaFormatResetLocalSecondsOf(epoch *int64) *string {
	if epoch == nil {
		return nil
	}
	return FormatResetLocalSeconds(*epoch)
}

// quotaStatusSuccess is http::StatusCode::is_success.
func quotaStatusSuccess(status int) bool { return status >= 200 && status < 300 }

// quotaHTTPStatus is the Display of an http::StatusCode: the code and its
// canonical reason, which for a few codes is worded differently from
// net/http's.
func quotaHTTPStatus(status int) string {
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

// quotaFormEncode writes pairs in order as reqwest's `.form()` does
// (application/x-www-form-urlencoded, as the url crate serializes it).
func quotaFormEncode(pairs [][2]string) string {
	encode := func(text string) string {
		var out strings.Builder
		for _, b := range []byte(text) {
			switch {
			case b >= 'a' && b <= 'z', b >= 'A' && b <= 'Z', b >= '0' && b <= '9',
				b == '*', b == '-', b == '.', b == '_':
				out.WriteByte(b)
			case b == ' ':
				out.WriteByte('+')
			default:
				fmt.Fprintf(&out, "%%%02X", b)
			}
		}
		return out.String()
	}
	parts := make([]string, len(pairs))
	for i, pair := range pairs {
		parts[i] = encode(pair[0]) + "=" + encode(pair[1])
	}
	return strings.Join(parts, "&")
}

// The JSON accessors below read a value decoded by jsonvalue.Parse as
// serde_json's Value accessors do.

// quotaJSONGet is Value::get with a key: the member of an object.
func quotaJSONGet(value any, key string) (any, bool) {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil, false
	}
	return object.Get(key)
}

// quotaJSONGetString is `value.get(key).and_then(Value::as_str)`.
func quotaJSONGetString(value any, key string) (string, bool) {
	member, _ := quotaJSONGet(value, key)
	text, ok := member.(string)
	return text, ok
}

// quotaJSONPointer is Value::pointer.
func quotaJSONPointer(value any, pointer string) (any, bool) {
	if pointer == "" {
		return value, true
	}
	if !strings.HasPrefix(pointer, "/") {
		return nil, false
	}
	for _, token := range strings.Split(pointer[1:], "/") {
		token = strings.ReplaceAll(strings.ReplaceAll(token, "~1", "/"), "~0", "~")
		switch current := value.(type) {
		case jsonvalue.Object:
			member, ok := current.Get(token)
			if !ok {
				return nil, false
			}
			value = member
		case []any:
			if strings.HasPrefix(token, "+") || (strings.HasPrefix(token, "0") && len(token) != 1) {
				return nil, false
			}
			index, err := strconv.ParseUint(token, 10, 64)
			if err != nil || index >= uint64(len(current)) {
				return nil, false
			}
			value = current[index]
		default:
			return nil, false
		}
	}
	return value, true
}

// quotaJSONIndex is serde_json's `value[key]`: the member, or null.
func quotaJSONIndex(value any, key string) any {
	member, _ := quotaJSONGet(value, key)
	return member
}

// quotaJSONI64 is Value::as_i64.
func quotaJSONI64(value any) (int64, bool) {
	number, ok := value.(int64)
	return number, ok
}

// quotaJSONU64 is Value::as_u64.
func quotaJSONU64(value any) (uint64, bool) {
	switch number := value.(type) {
	case int64:
		return uint64(number), number >= 0
	case uint64:
		return number, true
	}
	return 0, false
}

// quotaJSONF64 is Value::as_f64.
func quotaJSONF64(value any) (float64, bool) {
	switch number := value.(type) {
	case int64:
		return float64(number), true
	case uint64:
		return float64(number), true
	case float64:
		return number, true
	}
	return 0, false
}
