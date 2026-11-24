// Port of mj-accounts/src/claude_usage.rs.

// Claude Code quota polling, OAuth renewal, and `/usage` parsing.
//
// The Claude ACP agent exposes token usage over ACP, but the subscription
// quota shown by Claude Code lives behind its local `/usage` command. Keep
// this module independent from the UI state machine so the parser can be
// tested against captured command output without spawning `claude`.

package accounts

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"maps"
	"math"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const usageTimeout = 20 * time.Second

// cliUsageTimeout is how long macOS lets Claude Code's own `/usage` run. It
// is the bound the whole check ran under before the login exchange stopped
// sharing it.
const cliUsageTimeout = 60 * time.Second

const usageURL = "https://api.anthropic.com/api/oauth/usage"

// usageUserAgent: the reset-grant endpoint uses the CLI surface to determine
// eligibility.
const usageUserAgent = "claude-cli/2.1.283 (external, cli)"

// LoginExpired is the quota-row copy when the stored Claude OAuth access
// token is past `expiresAt`.
const LoginExpired = "login expired"

type ClaudeUsageReport struct {
	MonthlyPriceUsd *uint32
	PaidCredits     *client.PaidCredits
	BankedResets    *uint64
	FiveHour        *ClaudeUsageWindow
	Week            *ClaudeUsageWindow
	ScopedWeeks     []ClaudeScopedWeek
}

// ClaudeScopedWeek is one model-scoped weekly window, by its lowercase
// scope name (Rust's `(String, ClaudeUsageWindow)` pair).
type ClaudeScopedWeek struct {
	Scope  string
	Window ClaudeUsageWindow
}

type ClaudeUsageWindow struct {
	RemainingPercent uint8
	DurationSeconds  *int64
	// ResetContext is the text following `reset` in Claude Code output,
	// without the word itself.
	ResetContext *string
}

// ClaudeUsageError is why a Claude usage query or login renewal failed.
// Every variant is an error whose text is Rust's Display.
type ClaudeUsageError interface {
	error
	isClaudeUsageError()
}

type ClaudeUsageErrorTimedOut struct{}
type ClaudeUsageErrorNotSignedIn struct{}
type ClaudeUsageErrorLoginExpired struct{}
type ClaudeUsageErrorRefresh struct{ Value string }
type ClaudeUsageErrorQuery struct{ Value string }
type ClaudeUsageErrorParse struct{}

// ClaudeUsageErrorRateLimited: the endpoint answered 429. RetryAfter is what
// `Retry-After` asked for, when it said.
type ClaudeUsageErrorRateLimited struct{ RetryAfter *time.Duration }

func (ClaudeUsageErrorTimedOut) isClaudeUsageError()     {}
func (ClaudeUsageErrorNotSignedIn) isClaudeUsageError()  {}
func (ClaudeUsageErrorLoginExpired) isClaudeUsageError() {}
func (ClaudeUsageErrorRefresh) isClaudeUsageError()      {}
func (ClaudeUsageErrorQuery) isClaudeUsageError()        {}
func (ClaudeUsageErrorParse) isClaudeUsageError()        {}
func (ClaudeUsageErrorRateLimited) isClaudeUsageError()  {}

func (ClaudeUsageErrorTimedOut) Error() string    { return "claude /usage timed out" }
func (ClaudeUsageErrorNotSignedIn) Error() string { return "Claude Code is not signed in" }
func (ClaudeUsageErrorLoginExpired) Error() string {
	return LoginExpired
}
func (e ClaudeUsageErrorRefresh) Error() string { return "refresh Claude login: " + e.Value }
func (e ClaudeUsageErrorQuery) Error() string   { return "query Claude usage: " + e.Value }
func (ClaudeUsageErrorParse) Error() string     { return "could not parse claude /usage output" }
func (ClaudeUsageErrorRateLimited) Error() string {
	return "rate limited"
}

// queriesThroughCLI is whether the usage query asks the CLI directly:
// macOS, where the login lives in the Keychain.
func queriesThroughCLI() bool {
	return runtime.GOOS == "darwin"
}

// runsCLI is whether a usage query or grant renewal for the profile at home
// can resolve the `claude` program: always where the query goes through the
// CLI, otherwise only with a stored login that has a refresh token (the one
// renewal that needs the CLI; without one, queryWithRuntime and
// RefreshGrantIfDue finish without it). Reads the login file. Rust declares
// it on Unix only.
func runsCLI(home string) bool {
	if queriesThroughCLI() {
		return true
	}
	bytes, err := os.ReadFile(filepath.Join(home, ".credentials.json"))
	if err != nil {
		return false
	}
	credentials, err := jsonvalue.Parse(bytes)
	return err == nil && oauthHasRefreshToken(credentials)
}

// ClaudeUsageQuery queries the same OAuth usage endpoint as Claude Code's
// interactive `/usage`, except on macOS, where Claude Code is asked
// directly.
//
// The endpoint is authenticated with the access token from the profile
// home's `.credentials.json`. On macOS that file does not exist: Claude Code
// keeps the credentials in the Keychain, where only Claude Code itself reads
// them. The API has no token to send there, so its print-mode `/usage`
// output is the only quota macOS can report.
func ClaudeUsageQuery(ctx context.Context, home string, environment map[string]string) (ClaudeUsageReport, error) {
	if queriesThroughCLI() {
		program, err := ClaudeLauncherManaged{}.locate(ctx, environment)
		if err != nil {
			return ClaudeUsageReport{}, err
		}
		executor := core.CancellableProcessExecutorWithTimeout(cliUsageTimeout)
		cancel := executor.CancelOnDrop()
		defer cancel.Cancel()
		report, err := queryCLIWith(ctx, environment, executor, program)
		if err != nil {
			return ClaudeUsageReport{}, err
		}
		source := LoginSourceKeychain{
			LockMarker: filepath.Join(home, ".credentials.json"),
			Security:   executor,
		}
		credentials, err := source.read(ctx)
		if err != nil {
			slog.Warn("Claude subscription price unavailable", "error", quotaDisplay(err.Error()))
		} else {
			report.MonthlyPriceUsd = monthlyPriceUsd(credentials)
		}
		return report, nil
	}
	// The login exchange runs under no deadline of mj's and survives this
	// call being cancelled: see runClaudeRefresh.
	return queryWithRuntime(ctx, home, environment, usageURL, core.ProcessExecutor{}, ClaudeLauncherManaged{})
}

// ClaudeLauncher is where a host renewal gets its `claude`.
type ClaudeLauncher interface {
	isClaudeLauncher()
	// locate resolves the program, installing the pinned Claude Code when
	// this host has none yet. A failure says why no `claude` can run, so it
	// is reported on every renewal that needs it.
	locate(ctx context.Context, environment map[string]string) (claudeProgram, error)
}

// ClaudeLauncherManaged is this host's Claude Code, or the pinned
// installation prepared by the same resolver as bare workers.
type ClaudeLauncherManaged struct{}

// ClaudeLauncherProgram is a program chosen by the caller (tests).
type ClaudeLauncherProgram struct{ Value string }

func (ClaudeLauncherManaged) isClaudeLauncher() {}
func (ClaudeLauncherProgram) isClaudeLauncher() {}

func (ClaudeLauncherManaged) locate(ctx context.Context, environment map[string]string) (claudeProgram, error) {
	program, err := locate(ctx, core.HarnessKindClaude, environment)
	if err != nil {
		return claudeProgram{}, ClaudeUsageErrorRefresh{Value: err.Error()}
	}
	return *program, nil
}

func (l ClaudeLauncherProgram) locate(ctx context.Context, environment map[string]string) (claudeProgram, error) {
	command, err := commandText(l.Value)
	if err != nil {
		return claudeProgram{}, ClaudeUsageErrorRefresh{Value: err.Error()}
	}
	return claudeProgram{command: command, environment: map[string]string{}}, nil
}

type claudeProgram = quotaProgram

// admitRenewal admits one login renewal, which the daemon's handoff waits
// for once it has begun: Anthropic spends the refresh token before
// `claude auth login` persists the new one, so the exchange must not be cut
// short. Refused while a handoff is waiting; the next daemon redoes the
// renewal from the file.
func admitRenewal(gate *core.Gate) (*core.UpgradeWork, error) {
	work, err := gate.EnterUnlessDraining("Claude login renewal")
	if err != nil {
		return nil, ClaudeUsageErrorRefresh{Value: err.Error()}
	}
	return work, nil
}

// queryCLIWith reads the quota out of Claude Code's own `/usage` output.
func queryCLIWith(ctx context.Context, environment map[string]string, executor core.CommandExecutor, program claudeProgram) (ClaudeUsageReport, error) {
	output, err := runClaudeUsage(ctx, environment, executor, "read Claude usage", program)
	if err != nil {
		return ClaudeUsageReport{}, ClaudeUsageErrorQuery{Value: err.Error()}
	}
	// Claude Code reports a missing login on stdout and exits zero, so both
	// streams are read before the status is judged.
	text := strings.ToValidUTF8(string(output.Stdout), "�") + strings.ToValidUTF8(string(output.Stderr), "�")
	// A report that parses is the answer. Classifying first would let the
	// free-form sections below the windows decide the result: a skill or MCP
	// server whose name happens to contain "unauthorized" is not a login
	// failure.
	if output.Status == 0 {
		if report := parseCLIUsage(text); report != nil {
			return *report, nil
		}
	}
	if isAuthenticationError(text) {
		return ClaudeUsageReport{}, ClaudeUsageErrorNotSignedIn{}
	}
	if output.Status != 0 {
		return ClaudeUsageReport{}, ClaudeUsageErrorQuery{Value: fmt.Sprintf("claude /usage exited with status %d", output.Status)}
	}
	return ClaudeUsageReport{}, ClaudeUsageErrorParse{}
}

func queryWithRuntime(ctx context.Context, home string, environment map[string]string, usageEndpoint string, executor core.CommandExecutor, launcher ClaudeLauncher) (ClaudeUsageReport, error) {
	httpClient := &http.Client{Timeout: usageTimeout}
	credentials, err := readCredentials(ctx, home)
	if err != nil {
		return ClaudeUsageReport{}, err
	}
	token, err := oauthAccessToken(credentials, core.EpochMillis())
	if err == nil {
		report, err := queryAPI(ctx, httpClient, usageEndpoint, token, credentials)
		if _, expired := err.(ClaudeUsageErrorLoginExpired); expired && oauthHasRefreshToken(credentials) {
			return refreshAndRetry(ctx, httpClient, usageEndpoint, home, environment, executor, &token, launcher)
		}
		return report, err
	}
	if _, expired := err.(ClaudeUsageErrorLoginExpired); expired && oauthHasRefreshToken(credentials) {
		return refreshAndRetry(ctx, httpClient, usageEndpoint, home, environment, executor, nil, launcher)
	}
	return ClaudeUsageReport{}, err
}

// readCredentials is the profile's `.credentials.json` as a JSON value, or
// ClaudeUsageErrorNotSignedIn when it is missing or not JSON.
func readCredentials(ctx context.Context, home string) (any, error) {
	bytes, err := os.ReadFile(filepath.Join(home, ".credentials.json"))
	if err != nil {
		return nil, ClaudeUsageErrorNotSignedIn{}
	}
	credentials, err := jsonvalue.Parse(bytes)
	if err != nil {
		return nil, ClaudeUsageErrorNotSignedIn{}
	}
	return credentials, nil
}

func queryAPI(ctx context.Context, httpClient *http.Client, usageEndpoint string, token string, credentials any) (ClaudeUsageReport, error) {
	endpoint, err := url.Parse(usageEndpoint)
	if err != nil {
		return ClaudeUsageReport{}, ClaudeUsageErrorQuery{Value: err.Error()}
	}
	if endpoint.RawQuery != "" {
		endpoint.RawQuery += "&"
	}
	endpoint.RawQuery += "cedar_ember=1"
	payload, err := queryOauthJSON(ctx, httpClient, endpoint, token, usageTimeout, nil)
	if err != nil {
		return ClaudeUsageReport{}, err
	}
	report := parseAPIUsage(payload)
	if report == nil {
		return ClaudeUsageReport{}, ClaudeUsageErrorParse{}
	}
	report.MonthlyPriceUsd = monthlyPriceUsd(credentials)
	if extra, ok := quotaJSONGet(payload, "extra_usage"); ok {
		if enabled, _ := quotaJSONGet(extra, "is_enabled"); enabled == true {
			credits, err := queryPaidCredits(ctx, httpClient, usageEndpoint, token, extra)
			if err != nil {
				slog.Warn("Claude paid-credit balance unavailable", "error", quotaDisplay(err.Error()))
			} else {
				report.PaidCredits = &credits
			}
		}
	}
	return *report, nil
}

func queryOauthJSON(ctx context.Context, httpClient *http.Client, endpoint *url.URL, token string, timeout time.Duration, organization *string) (any, error) {
	requestCtx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	request, err := http.NewRequestWithContext(requestCtx, http.MethodGet, endpoint.String(), nil)
	if err != nil {
		return nil, ClaudeUsageErrorQuery{Value: err.Error()}
	}
	request.Header.Set("User-Agent", usageUserAgent)
	request.Header.Set("Authorization", "Bearer "+token)
	request.Header.Set("anthropic-beta", "oauth-2025-04-20")
	request.Header.Set("anthropic-version", "2023-06-01")
	if organization != nil {
		request.Header.Set("x-organization-uuid", *organization)
	}
	response, err := httpClient.Do(request)
	if err != nil {
		if claudeIsTimeout(err) {
			return nil, ClaudeUsageErrorTimedOut{}
		}
		return nil, ClaudeUsageErrorQuery{Value: err.Error()}
	}
	defer response.Body.Close()
	switch {
	case response.StatusCode == http.StatusUnauthorized || response.StatusCode == http.StatusForbidden:
		return nil, ClaudeUsageErrorLoginExpired{}
	case response.StatusCode == http.StatusTooManyRequests:
		limited := ClaudeUsageErrorRateLimited{}
		if retryAfter, ok := RetryAfterHeader(response.Header); ok {
			limited.RetryAfter = &retryAfter
		}
		return nil, limited
	case !quotaStatusSuccess(response.StatusCode):
		return nil, ClaudeUsageErrorQuery{Value: "HTTP " + quotaHTTPStatus(response.StatusCode)}
	}
	body, err := io.ReadAll(response.Body)
	if err != nil {
		if claudeIsTimeout(err) {
			return nil, ClaudeUsageErrorTimedOut{}
		}
		return nil, ClaudeUsageErrorQuery{Value: err.Error()}
	}
	payload, err := jsonvalue.Parse(body)
	if err != nil {
		return nil, ClaudeUsageErrorQuery{Value: err.Error()}
	}
	return payload, nil
}

// claudeIsTimeout is reqwest's Error::is_timeout.
func claudeIsTimeout(err error) bool {
	if errors.Is(err, context.DeadlineExceeded) {
		return true
	}
	var timeout net.Error
	return errors.As(err, &timeout) && timeout.Timeout()
}

func queryPaidCredits(ctx context.Context, httpClient *http.Client, usageEndpoint string, token string, extra any) (client.PaidCredits, error) {
	invalid := ClaudeUsageErrorQuery{Value: "invalid paid-credit balance or spending cap"}
	endpoint, err := url.Parse(usageEndpoint)
	if err != nil {
		return client.PaidCredits{}, invalid
	}
	endpoint.Path, endpoint.RawPath, endpoint.RawQuery, endpoint.ForceQuery = "/api/oauth/profile", "", "", false
	profile, err := queryOauthJSON(ctx, httpClient, endpoint, token, 5*time.Second, nil)
	if err != nil {
		return client.PaidCredits{}, err
	}
	uuid, _ := quotaJSONPointer(profile, "/organization/uuid")
	organization, ok := uuid.(string)
	if !ok || organization == "" {
		return client.PaidCredits{}, invalid
	}
	endpoint.Path = "/api/oauth/organizations/" + organization + "/prepaid/credits"
	endpoint.RawPath = "/api/oauth/organizations/" + claudePathSegment(organization) + "/prepaid/credits"
	wallet, err := queryOauthJSON(ctx, httpClient, endpoint, token, 5*time.Second, &organization)
	if err != nil {
		return client.PaidCredits{}, err
	}
	amount, _ := quotaJSONGet(wallet, "amount")
	balance, ok := quotaJSONF64(amount)
	if !ok {
		return client.PaidCredits{}, invalid
	}
	limit, hasLimit := quotaJSONGet(extra, "monthly_limit")
	switch {
	case hasLimit && limit != nil:
		cap, ok := quotaJSONF64(limit)
		if !ok {
			return client.PaidCredits{}, invalid
		}
		spent, _ := quotaJSONGet(extra, "used_credits")
		used, ok := quotaJSONF64(spent)
		if !ok {
			return client.PaidCredits{}, invalid
		}
		balance = min(balance, cap-used)
	case !hasLimit:
		return client.PaidCredits{}, invalid
	}
	spend, spendOK := quotaJSONGetString(extra, "currency")
	funds, fundsOK := quotaJSONGetString(wallet, "currency")
	if spendOK && fundsOK && quotaASCIILower(spend) != quotaASCIILower(funds) {
		return client.PaidCredits{}, ClaudeUsageErrorQuery{Value: "paid-credit currencies disagree"}
	}
	credits := client.NewPaidCreditsLimited(balance)
	if credits == nil {
		return client.PaidCredits{}, invalid
	}
	return *credits, nil
}

// claudePathSegment percent-encodes one path segment as the url crate's
// `path_segments_mut().push` does for an http URL.
func claudePathSegment(segment string) string {
	var out strings.Builder
	for _, b := range []byte(segment) {
		if b < 0x20 || b >= 0x7F || strings.IndexByte(" \"#<>?`{}/%\\", b) >= 0 {
			fmt.Fprintf(&out, "%%%02X", b)
		} else {
			out.WriteByte(b)
		}
	}
	return out.String()
}

func refreshAndRetry(ctx context.Context, httpClient *http.Client, usageEndpoint string, home string, environment map[string]string, executor core.CommandExecutor, rejectedToken *string, launcher ClaudeLauncher) (ClaudeUsageReport, error) {
	program, err := launcher.locate(ctx, environment)
	if err != nil {
		return ClaudeUsageReport{}, err
	}
	// Held until the renewal has persisted the new login, even if this call
	// is cancelled (see runClaudeRefresh).
	admission, err := admitRenewal(core.UpgradeGate())
	if err != nil {
		program.release()
		return ClaudeUsageReport{}, err
	}
	held := &renewal{admission: admission, program: program}
	defer held.release()
	source := LoginSourceFile{Home: home}
	locks, err := source.lock(ctx)
	if err != nil {
		return ClaudeUsageReport{}, err
	}
	held.locks = locks
	active, err := readCredentials(ctx, home)
	if err != nil {
		return ClaudeUsageReport{}, err
	}
	if token, err := oauthAccessToken(active, core.EpochMillis()); err == nil && (rejectedToken == nil || *rejectedToken != token) {
		held.locks.Release()
		held.locks = nil
		return queryAPI(ctx, httpClient, usageEndpoint, token, active)
	}
	credentials, err := renewLocked(ctx, active, source, environment, executor, held.take())
	if err != nil {
		return ClaudeUsageReport{}, err
	}
	token, err := oauthAccessToken(credentials, core.EpochMillis())
	if err != nil {
		return ClaudeUsageReport{}, ClaudeUsageErrorRefresh{Value: err.Error()}
	}
	return queryAPI(ctx, httpClient, usageEndpoint, token, credentials)
}

// GrantRefreshLead: Claude access tokens last 8 hours (observed: `expiresAt`
// is the issue time plus 8 h). The daemon refreshes when less than half of
// that remains, which leaves every session copy, none of which can refresh,
// 4 hours of valid token while no daemon runs (#1490).
const GrantRefreshLead = 4 * time.Hour

// GrantDueAt is when a login expiring at expiresAtMs is due for renewal.
func GrantDueAt(expiresAtMs int64) int64 {
	leadMs := GrantRefreshLead.Milliseconds()
	if expiresAtMs < math.MinInt64+leadMs {
		return math.MinInt64
	}
	return expiresAtMs - leadMs
}

// GrantRefresh is what RefreshGrantIfDue found.
type GrantRefresh interface {
	isGrantRefresh()
}

// GrantRefreshNotRefreshable: nothing to refresh: no login here, or it
// carries no refresh token.
type GrantRefreshNotRefreshable struct{}

// GrantRefreshCurrent: enough lifetime remains. The login becomes due at
// this epoch time.
type GrantRefreshCurrent struct{ DueAtMs int64 }

// GrantRefreshDeferred: due, but a daemon handoff is waiting. The renewal
// did not start; the next daemon decides again from the file.
type GrantRefreshDeferred struct{}

// GrantRefreshRenewed: the login was renewed, by this call or by another
// daemon that held the native lock first. ExpiresAtMs is the new expiry.
type GrantRefreshRenewed struct {
	ByThisCall  bool
	ExpiresAtMs *int64
}

func (GrantRefreshNotRefreshable) isGrantRefresh() {}
func (GrantRefreshCurrent) isGrantRefresh()        {}
func (GrantRefreshDeferred) isGrantRefresh()       {}
func (GrantRefreshRenewed) isGrantRefresh()        {}

type GrantRefreshTrigger struct {
	NowMs int64
	// RejectedLogins is the identity of the access token used by the
	// authentication-failed turn.
	RejectedLogins map[string]struct{}
}

// RefreshGrantIfDue renews the profile's login when refused or when less
// than GrantRefreshLead remains, under the native locks with a reread, so
// only one of several daemons sharing the home exchanges the refresh token.
// This is the grant's only refresher: copies handed to harnesses carry no
// refresh token. The decision does not consult the usage API's rate-limit
// hold, which covers usage requests only.
func RefreshGrantIfDue(ctx context.Context, source LoginSource, environment map[string]string, executor core.CommandExecutor, launcher ClaudeLauncher, gate *core.Gate, trigger GrantRefreshTrigger) (GrantRefresh, error) {
	nowMs, rejectedLogins := trigger.NowMs, trigger.RejectedLogins
	dueAt := func(credentials any) (int64, bool) {
		if !oauthHasRefreshToken(credentials) {
			return 0, false
		}
		oauth, ok := quotaJSONGet(credentials, "claudeAiOauth")
		if !ok {
			return 0, false
		}
		expiresAt, ok := oauthExpiresAt(oauth)
		if !ok {
			return 0, false
		}
		return GrantDueAt(expiresAt), true
	}
	expiresAt := func(credentials any) *int64 {
		oauth, ok := quotaJSONGet(credentials, "claudeAiOauth")
		if !ok {
			return nil
		}
		expires, ok := oauthExpiresAt(oauth)
		if !ok {
			return nil
		}
		return &expires
	}
	credentials, err := source.read(ctx)
	if err != nil {
		if _, missing := err.(ClaudeUsageErrorNotSignedIn); missing {
			return GrantRefreshNotRefreshable{}, nil
		}
		return nil, err
	}
	refused := func(credentials any) bool {
		token, err := oauthAccessToken(credentials, nowMs)
		if err != nil {
			return false
		}
		_, rejected := rejectedLogins[core.CredentialFingerprint([]byte(token))]
		return rejected
	}
	dueAtMs, ok := dueAt(credentials)
	if !ok {
		return GrantRefreshNotRefreshable{}, nil
	}
	if dueAtMs > nowMs && !refused(credentials) {
		return GrantRefreshCurrent{DueAtMs: dueAtMs}, nil
	}
	// Due. Find a runnable `claude` first: with none, say so on every
	// attempt rather than strip copies nothing can renew.
	program, err := launcher.locate(ctx, environment)
	if err != nil {
		return nil, err
	}
	// Admission covers the exchange only, from here until the new login is
	// persisted (it moves into the subprocess's goroutine with the locks). A
	// waiting handoff is not delayed by a renewal that has not begun.
	admission, err := admitRenewal(gate)
	if err != nil {
		program.release()
		slog.Debug("deferring the Claude login renewal until after the daemon handoff", "error", quotaDisplay(err.Error()))
		return GrantRefreshDeferred{}, nil
	}
	held := &renewal{admission: admission, program: program}
	defer held.release()
	locks, err := source.lock(ctx)
	if err != nil {
		return nil, err
	}
	held.locks = locks
	active, err := source.read(ctx)
	if err != nil {
		return nil, err
	}
	activeDueAt, ok := dueAt(active)
	if !ok {
		return GrantRefreshNotRefreshable{}, nil
	}
	if activeDueAt > nowMs && !refused(active) {
		// Another daemon or Claude Code renewed it while this one waited.
		return GrantRefreshRenewed{ByThisCall: false, ExpiresAtMs: expiresAt(active)}, nil
	}
	renewed, err := renewLocked(ctx, active, source, environment, executor, held.take())
	if err != nil {
		return nil, err
	}
	return GrantRefreshRenewed{ByThisCall: true, ExpiresAtMs: expiresAt(renewed)}, nil
}

// LoginSource is where the login a renewal works on is kept, and how Claude
// Code is told to find it. Built from core.HarnessKind.LoginStore.
type LoginSource interface {
	isLoginSource()
	// lock takes the native refresh locks every writer of this login takes.
	lock(ctx context.Context) (*core.CredentialWriteGuard, error)
	// read is the login as it is now. A Keychain that cannot be read is an
	// error, never "not signed in": a renewal spends the refresh token, and
	// one started on a login that merely could not be read would lose the
	// grant.
	read(ctx context.Context) (any, error)
	// configureEnvironment sets the variables `claude auth login` needs to
	// find this login.
	configureEnvironment(environment map[string]string)
	// confineCommand points a Keychain renewal at the item the daemon reads
	// and writes. An inherited `CLAUDE_CONFIG_DIR` would rename the item, and
	// a `USER` from the profile's environment would change its account
	// (Claude Code names the account after `USER`): either way the renewed
	// login would land where nothing reads it, after the old refresh token
	// was spent.
	confineCommand(command *core.CommandSpec)
}

// LoginSourceFile is `<home>/.credentials.json`, with `CLAUDE_CONFIG_DIR`
// pointing at Home.
type LoginSourceFile struct{ Home string }

// LoginSourceKeychain is the macOS Keychain item every profile shares.
// `claude auth login` must run with `CLAUDE_CONFIG_DIR` unset, which is the
// only way it writes that item. LockMarker is the credential file under
// `~/.claude` whose directory carries the locks Claude Code itself takes;
// Security reads the item.
type LoginSourceKeychain struct {
	LockMarker string
	Security   core.CommandExecutor
}

func (LoginSourceFile) isLoginSource()     {}
func (LoginSourceKeychain) isLoginSource() {}

func (s LoginSourceFile) lock(ctx context.Context) (*core.CredentialWriteGuard, error) {
	guard, err := core.CredentialWriteGuardAcquire(ctx, core.HarnessKindClaude, filepath.Join(s.Home, ".credentials.json"))
	if err != nil {
		return nil, ClaudeUsageErrorRefresh{Value: err.Error()}
	}
	return guard, nil
}

func (s LoginSourceKeychain) lock(ctx context.Context) (*core.CredentialWriteGuard, error) {
	guard, err := core.CredentialWriteGuardAcquire(ctx, core.HarnessKindClaude, s.LockMarker)
	if err != nil {
		return nil, ClaudeUsageErrorRefresh{Value: err.Error()}
	}
	return guard, nil
}

func (s LoginSourceFile) read(ctx context.Context) (any, error) {
	return readCredentials(ctx, s.Home)
}

func (s LoginSourceKeychain) read(ctx context.Context) (any, error) {
	login, err := ReadClaudeKeychainLogin(s.Security)
	if err != nil {
		return nil, ClaudeUsageErrorRefresh{Value: err.Error()}
	}
	switch login := login.(type) {
	case KeychainLoginMissing:
		return nil, ClaudeUsageErrorNotSignedIn{}
	case KeychainLoginUnreadable:
		return nil, ClaudeUsageErrorRefresh{Value: fmt.Sprintf(
			"the macOS Keychain could not be read (security exited with status %d); unlock it and the renewal will be retried",
			login.Status,
		)}
	case KeychainLoginPresent:
		credentials, err := jsonvalue.Parse(login.Login)
		if err != nil {
			return nil, ClaudeUsageErrorNotSignedIn{}
		}
		return credentials, nil
	}
	return nil, ClaudeUsageErrorRefresh{Value: fmt.Sprintf("unknown Keychain login %T", login)}
}

func (s LoginSourceFile) configureEnvironment(environment map[string]string) {
	environment["CLAUDE_CONFIG_DIR"] = s.Home
}

func (LoginSourceKeychain) configureEnvironment(environment map[string]string) {
	delete(environment, "CLAUDE_CONFIG_DIR")
}

func (LoginSourceFile) confineCommand(*core.CommandSpec) {}

func (LoginSourceKeychain) confineCommand(command *core.CommandSpec) {
	delete(command.Env, "CLAUDE_CONFIG_DIR")
	command.EnvRemove = append(command.EnvRemove, "CLAUDE_CONFIG_DIR")
	command.EnvRemove = slices.DeleteFunc(command.EnvRemove, func(name string) bool { return name == "USER" })
	if command.Env == nil {
		command.Env = map[string]string{}
	}
	command.Env["USER"] = ClaudeKeychainAccount()
}

// renewal is what one login renewal holds from before its first lock until
// the child has exited: the native locks, the handoff admission, and the
// installation of the `claude` it runs. release gives all three up (Rust's
// Drop); take hands them to a new owner.
type renewal struct {
	locks     *core.CredentialWriteGuard
	admission *core.UpgradeWork
	program   claudeProgram
}

func (r *renewal) release() {
	if r.locks != nil {
		r.locks.Release()
		r.locks = nil
	}
	if r.admission != nil {
		r.admission.Release()
		r.admission = nil
	}
	r.program.release()
}

// take moves what r holds into a new renewal, leaving r holding nothing.
func (r *renewal) take() *renewal {
	moved := *r
	*r = renewal{}
	return &moved
}

// renewLocked runs `claude auth login` with the stored refresh token while
// the caller holds the native locks, and returns the credentials it left.
// Fails when the access token did not change.
func renewLocked(ctx context.Context, active any, source LoginSource, environment map[string]string, executor core.CommandExecutor, held *renewal) (any, error) {
	var before *string
	if token, ok := quotaJSONPointer(active, "/claudeAiOauth/accessToken"); ok {
		if text, ok := token.(string); ok {
			before = &text
		}
	}
	environment = maps.Clone(environment)
	if environment == nil {
		environment = map[string]string{}
	}
	source.configureEnvironment(environment)
	refreshErr := runClaudeRefresh(ctx, active, source, environment, executor, held)
	if refreshErr != nil && ctx.Err() != nil && errors.Is(refreshErr, ctx.Err()) {
		// Cancelled while the exchange runs on: it finishes on its own.
		return nil, refreshErr
	}
	credentials, err := source.read(ctx)
	if err != nil {
		if refreshErr != nil {
			return nil, refreshErr
		}
		return nil, err
	}
	token, err := oauthAccessToken(credentials, core.EpochMillis())
	if err != nil {
		if refreshErr != nil {
			return nil, refreshErr
		}
		return nil, ClaudeUsageErrorRefresh{Value: fmt.Sprintf("Claude auth login left no usable access token: %s", err)}
	}
	if before != nil && *before == token {
		if refreshErr != nil {
			return nil, refreshErr
		}
		return nil, ClaudeUsageErrorRefresh{Value: "Claude auth login did not renew the access token"}
	}
	return credentials, nil
}

// runClaudeUsage reads Claude's quota, on macOS, through its Keychain-aware
// CLI.
func runClaudeUsage(ctx context.Context, environment map[string]string, executor core.CommandExecutor, purpose string, program claudeProgram) (core.CommandOutput, error) {
	command := core.NewCommandSpec(program.command, []string{"-p", "/usage", "--no-session-persistence"})
	command.Purpose = purpose
	maps.Copy(command.Env, environment)
	maps.Copy(command.Env, program.environment)
	type executed struct {
		output core.CommandOutput
		err    error
	}
	done := make(chan executed, 1)
	go func() {
		var result executed
		func() {
			defer func() {
				if failure := recover(); failure != nil {
					result = executed{err: fmt.Errorf("worker failed: %v", failure)}
				}
			}()
			output, err := executor.Execute(&command)
			result = executed{output: output, err: err}
		}()
		// The installation stays leased until the child has been reaped.
		program.release()
		done <- result
	}()
	select {
	case result := <-done:
		return result.output, result.err
	case <-ctx.Done():
		return core.CommandOutput{}, ctx.Err()
	}
}

func runClaudeRefresh(ctx context.Context, credentials any, source LoginSource, environment map[string]string, executor core.CommandExecutor, held *renewal) error {
	oauth := quotaJSONIndex(credentials, "claudeAiOauth")
	refresh, ok := quotaJSONIndex(oauth, "refreshToken").(string)
	if !ok || refresh == "" {
		held.release()
		return ClaudeUsageErrorRefresh{Value: "stored refresh token is missing"}
	}
	var scopes []string
	if items, ok := quotaJSONIndex(oauth, "scopes").([]any); ok && len(items) != 0 {
		for _, item := range items {
			scope, ok := item.(string)
			if !ok || scope == "" {
				scopes = nil
				break
			}
			scopes = append(scopes, scope)
		}
	}
	if len(scopes) == 0 {
		held.release()
		return ClaudeUsageErrorRefresh{Value: "stored OAuth scopes are missing or invalid"}
	}
	environment["CLAUDE_CODE_OAUTH_REFRESH_TOKEN"] = refresh
	environment["CLAUDE_CODE_OAUTH_SCOPES"] = strings.Join(scopes, " ")
	// An empty client ID selects Claude's default and overrides an ambient
	// ID belonging to another account's automation.
	clientID, _ := quotaJSONIndex(oauth, "clientId").(string)
	environment["CLAUDE_CODE_OAUTH_CLIENT_ID"] = clientID
	program := held.program
	command := core.NewCommandSpec(program.command, []string{"auth", "login"})
	maps.Copy(command.Env, environment)
	maps.Copy(command.Env, program.environment)
	source.confineCommand(&command)
	command.Purpose = "refresh Claude login"
	command.SensitiveOutput = true
	// Once the child starts it runs to its own end. Anthropic spends the
	// refresh token on the first request, so a kill (a mj deadline, or the
	// caller being cancelled) before the new login is written loses the
	// grant. Claude Code bounds its own network steps: the token request
	// (`fn(30000, ...)` in `Npe`, CC 2.1.321) and the profile lookup after
	// it (`fn(1e4, ...)`) each time out. The executor therefore has no
	// deadline and no cancellation, and the lock wait is counted apart
	// (`ClaudeRefreshLockWait`).
	type executed struct {
		output core.CommandOutput
		err    error
	}
	done := make(chan executed, 1)
	go func() {
		var result executed
		func() {
			defer func() {
				if failure := recover(); failure != nil {
					result = executed{err: ClaudeUsageErrorRefresh{Value: fmt.Sprintf("worker failed: %v", failure)}}
				}
			}()
			output, err := executor.Execute(&command)
			if err != nil {
				err = ClaudeUsageErrorRefresh{Value: err.Error()}
			}
			result = executed{output: output, err: err}
		}()
		// Keep the locks, the installation and the handoff admission until
		// the executor has reaped its child, even if the caller is
		// cancelled while this goroutine is still running.
		held.release()
		done <- result
	}()
	select {
	case result := <-done:
		if result.err != nil {
			return result.err
		}
		return successfulRefreshOutput(result.output)
	case <-ctx.Done():
		return ctx.Err()
	}
}

func successfulRefreshOutput(output core.CommandOutput) error {
	if output.Status == 0 {
		return nil
	}
	detail := ""
	for _, stream := range [][]byte{output.Stderr, output.Stdout} {
		if category, ok := claudeRefreshFailureCategory(stream); ok {
			detail = ": " + category
			break
		}
	}
	return ClaudeUsageErrorRefresh{Value: fmt.Sprintf("Claude auth login exited with status %d%s", output.Status, detail)}
}

// claudeRefreshFailureCategory is a fixed description of a failed native
// auth login, never captured text: Axios can dump request bodies containing
// tokens.
func claudeRefreshFailureCategory(bytes []byte) (string, bool) {
	// Native auth login prints `Login failed: ${error.message}`. Match
	// complete status messages so token text or e.g. 4010 is not a status.
	for _, line := range claudeLines(strings.ToValidUTF8(string(bytes), "�")) {
		line = strings.TrimSpace(line)
		message, ok := strings.CutPrefix(line, "Login failed: ")
		if !ok {
			message, ok = strings.CutPrefix(line, "AxiosError: ")
		}
		if ok {
			switch {
			case message == "Request failed with status code 400":
				return "HTTP 400 rejection", true
			case message == "Request failed with status code 401":
				return "HTTP 401 rejection", true
			case message == "Request failed with status code 403":
				return "HTTP 403 rejection", true
			case strings.HasPrefix(message, "getaddrinfo ENOTFOUND ") || strings.HasPrefix(message, "getaddrinfo EAI_AGAIN "):
				return "DNS lookup failure", true
			case strings.HasPrefix(message, "connect ECONNREFUSED "):
				return "network connection refused", true
			}
		}
		// Native auth login appends this fixed warning after a post-exchange
		// policy refusal. The surrounding organization details stay private.
		if line == "The refresh token in CLAUDE_CODE_OAUTH_REFRESH_TOKEN was used for this attempt and cannot be used again; issue a new one to retry." {
			return "native sign-in policy refusal after token exchange", true
		}
	}
	return "", false
}

// claudeLines is Rust's str::lines: split at `\n`, drop a `\r` before it,
// and no empty line after a final newline.
func claudeLines(text string) []string {
	var lines []string
	for text != "" {
		line, rest, newline := strings.Cut(text, "\n")
		if newline {
			line = strings.TrimSuffix(line, "\r")
		}
		lines = append(lines, line)
		text = rest
	}
	return lines
}

func oauthAccessToken(credentials any, nowMs int64) (string, error) {
	oauth, ok := quotaJSONGet(credentials, "claudeAiOauth")
	if !ok {
		return "", ClaudeUsageErrorNotSignedIn{}
	}
	token, ok := quotaJSONGetString(oauth, "accessToken")
	if !ok || token == "" {
		return "", ClaudeUsageErrorNotSignedIn{}
	}
	if expiresAt, ok := oauthExpiresAt(oauth); ok && expiresAt <= nowMs {
		return "", ClaudeUsageErrorLoginExpired{}
	}
	return token, nil
}

func oauthExpiresAt(oauth any) (int64, bool) {
	value, ok := quotaJSONGet(oauth, "expiresAt")
	if !ok {
		return 0, false
	}
	return valueI64(value)
}

func oauthHasRefreshToken(credentials any) bool {
	token, _ := quotaJSONPointer(credentials, "/claudeAiOauth/refreshToken")
	text, ok := token.(string)
	return ok && text != ""
}

// monthlyPriceUsd is the monthly list price, deliberately independent of the
// advertised usage multipliers: Max's weekly allowance does not scale with
// its five-hour one.
func monthlyPriceUsd(credentials any) *uint32 {
	tier, _ := quotaJSONPointer(credentials, "/claudeAiOauth/rateLimitTier")
	var price uint32
	switch tier {
	case "default_claude_max_5x":
		price = 100
	case "default_claude_max_20x":
		price = 200
	case "default_claude_pro":
		price = 20
	default:
		return nil
	}
	return &price
}

func parseAPIUsage(payload any) *ClaudeUsageReport {
	var fiveHour, week *ClaudeUsageWindow
	var scopedWeeks []ClaudeScopedWeek
	sevenDays := int64(7 * 24 * 60 * 60)

	if limits, ok := quotaJSONGet(payload, "limits"); ok && claudeIsArray(limits) {
		for _, limit := range limits.([]any) {
			kind, ok := quotaJSONGetString(limit, "kind")
			if !ok {
				continue
			}
			window := apiWindow(limit, "percent")
			if window == nil {
				continue
			}
			switch kind {
			case "session":
				fiveHours := int64(5 * 60 * 60)
				window.DurationSeconds = &fiveHours
				fiveHour = window
			case "weekly_all":
				window.DurationSeconds = &sevenDays
				week = window
			case "weekly_scoped":
				if scope := apiScopeName(limit); scope != nil {
					window.DurationSeconds = &sevenDays
					scopedWeeks = append(scopedWeeks, ClaudeScopedWeek{Scope: *scope, Window: *window})
				}
			}
		}
	} else {
		legacy := func(key string) *ClaudeUsageWindow {
			value, ok := quotaJSONGet(payload, key)
			if !ok || value == nil {
				return nil
			}
			return apiWindow(value, "utilization")
		}
		fiveHour = legacy("five_hour")
		if window := legacy("seven_day"); window != nil {
			window.DurationSeconds = &sevenDays
			week = window
		}
		if window := legacy("seven_day_fable"); window != nil {
			window.DurationSeconds = &sevenDays
			scopedWeeks = append(scopedWeeks, ClaudeScopedWeek{Scope: "fable", Window: *window})
		}
	}

	extraEnabled, _ := quotaJSONPointer(payload, "/extra_usage/is_enabled")
	if fiveHour == nil && week == nil && len(scopedWeeks) == 0 && extraEnabled != true {
		return nil
	}
	if scopedWeeks == nil {
		scopedWeeks = []ClaudeScopedWeek{}
	}
	return &ClaudeUsageReport{
		BankedResets: parseBankedResets(payload, core.EpochMillis()/1000),
		FiveHour:     fiveHour,
		Week:         week,
		ScopedWeeks:  scopedWeeks,
	}
}

func claudeIsArray(value any) bool {
	_, ok := value.([]any)
	return ok
}

func parseBankedResets(payload any, now int64) *uint64 {
	block, ok := quotaJSONGet(payload, "cedar_ember")
	if !ok || block == nil {
		return nil
	}
	parsed := func() (uint64, bool) {
		eligibleValue, ok := quotaJSONGet(block, "eligible")
		if !ok {
			return 0, false
		}
		eligible, ok := eligibleValue.(bool)
		if !ok {
			return 0, false
		}
		if !eligible {
			return 0, true
		}
		grantsValue, ok := quotaJSONGet(block, "grants")
		if !ok {
			return 0, false
		}
		grants, ok := grantsValue.([]any)
		if !ok {
			return 0, false
		}
		var total uint64
		for _, grant := range grants {
			left, ok := quotaJSONGet(grant, "resets_left")
			if !ok {
				return 0, false
			}
			remaining, ok := quotaJSONU64(left)
			if !ok {
				return 0, false
			}
			// timestamp is false for a malformed value; a missing or null
			// one is no time (nil).
			timestamp := func(key string) (*int64, bool) {
				value, ok := quotaJSONGet(grant, key)
				if !ok || value == nil {
					return nil, true
				}
				text, ok := value.(string)
				if !ok {
					return nil, false
				}
				date, ok := core.ParseChronoRFC3339(text)
				if !ok {
					return nil, false
				}
				seconds := date.Unix()
				return &seconds, true
			}
			start, ok := timestamp("starts_at")
			if !ok {
				return 0, false
			}
			if start != nil && *start > now {
				continue
			}
			end, ok := timestamp("ends_at")
			if !ok {
				return 0, false
			}
			if end != nil && *end <= now {
				continue
			}
			// A banked reset can require reaching a limit before use.
			// `usable_now` is not the balance of unused resets.
			if total > math.MaxUint64-remaining {
				return 0, false
			}
			total += remaining
		}
		return total, true
	}
	total, ok := parsed()
	if !ok {
		slog.Warn("Claude usage returned malformed reset-grant metadata")
		return nil
	}
	return &total
}

func apiWindow(value any, percentKey string) *ClaudeUsageWindow {
	percent, ok := quotaJSONGet(value, percentKey)
	if !ok {
		return nil
	}
	used, ok := quotaJSONF64(percent)
	if !ok {
		return nil
	}
	window := &ClaudeUsageWindow{
		RemainingPercent: 100 - quotaF64ToUint8(min(max(math.Round(used), 0), 100)),
	}
	if reset, ok := quotaJSONGetString(value, "resets_at"); ok {
		window.ResetContext = &reset
	}
	return window
}

// parseCLIUsage scrapes `claude -p /usage` for the two windows the quota row
// shows.
//
// The command prints one line per window, each naming its own percentage:
//
//	Current session: 6% used · resets Sep 18 at 6:20pm (Europe/Paris)
//	Current week (all models): 30% used · resets Sep 20 at 1:59pm (Europe/Paris)
//	Current week (Fable): 40% used · resets Sep 20 at 2pm (Europe/Paris)
//
// The sections below those quote unrelated percentages ("57% of your usage
// was at >150k context", "Top skills: /code-review 3%"), so only a line whose
// label is one of the windows is read, and only when the number is followed
// by `used`. A percentage that means something else is worth reporting as
// unparsed rather than reporting backwards.
func parseCLIUsage(output string) *ClaudeUsageReport {
	var fiveHour, week *ClaudeUsageWindow
	scopedWeeks := []ClaudeScopedWeek{}
	sevenDays := int64(7 * 24 * 60 * 60)

	for _, line := range claudeLines(stripANSI(output)) {
		label, rest, ok := strings.Cut(line, ":")
		if !ok {
			continue
		}
		label = quotaASCIILower(strings.TrimSpace(label))
		if label != "current session" && !strings.HasPrefix(label, "current week") {
			continue
		}
		window := cliWindow(rest)
		if window == nil {
			continue
		}
		switch {
		case label == "current session":
			fiveHours := int64(5 * 60 * 60)
			window.DurationSeconds = &fiveHours
			fiveHour = window
		case label == "current week (all models)" || label == "current week":
			window.DurationSeconds = &sevenDays
			week = window
		default:
			scope, ok := strings.CutPrefix(label, "current week (")
			if !ok {
				continue
			}
			scope, ok = strings.CutSuffix(scope, ")")
			if !ok {
				continue
			}
			window.DurationSeconds = &sevenDays
			scopedWeeks = append(scopedWeeks, ClaudeScopedWeek{Scope: scope, Window: *window})
		}
	}

	if fiveHour == nil && week == nil && len(scopedWeeks) == 0 {
		return nil
	}
	return &ClaudeUsageReport{FiveHour: fiveHour, Week: week, ScopedWeeks: scopedWeeks}
}

// cliWindow is one window from the text after a `/usage` line's label.
func cliWindow(rest string) *ClaudeUsageWindow {
	usedText, tail, ok := strings.Cut(strings.TrimLeftFunc(rest, unicode.IsSpace), "%")
	if !ok {
		return nil
	}
	used, ok := quotaParseF64(strings.TrimSpace(usedText))
	if !ok {
		return nil
	}
	if !strings.HasPrefix(strings.TrimLeftFunc(tail, unicode.IsSpace), "used") {
		return nil
	}
	window := &ClaudeUsageWindow{
		RemainingPercent: 100 - quotaF64ToUint8(min(max(math.Round(used), 0), 100)),
	}
	if _, context, ok := strings.Cut(tail, "resets "); ok {
		if context = strings.TrimSpace(context); context != "" {
			window.ResetContext = &context
		}
	}
	return window
}

// stripANSI removes SGR escapes: Claude Code paints `/usage` for a terminal,
// so its output can carry them even in print mode.
func stripANSI(input string) string {
	var output strings.Builder
	output.Grow(len(input))
	for len(input) > 0 {
		character, size := utf8.DecodeRuneInString(input)
		input = input[size:]
		if character != '\x1b' {
			output.WriteRune(character)
			continue
		}
		// CSI sequences end at their final byte; anything else ends at the
		// next character.
		next, size := utf8.DecodeRuneInString(input)
		if size == 0 {
			continue
		}
		input = input[size:]
		if next != '[' {
			continue
		}
		for len(input) > 0 {
			character, size := utf8.DecodeRuneInString(input)
			input = input[size:]
			if character < utf8.RuneSelf && (character >= 'a' && character <= 'z' || character >= 'A' && character <= 'Z') {
				break
			}
		}
	}
	return output.String()
}

func isAuthenticationError(detail string) bool {
	lower := quotaASCIILower(detail)
	for _, needle := range []string{
		"not logged in",
		"not signed in",
		"unauthenticated",
		"unauthorized",
		"please log in",
		"please login",
		"invalid api key",
	} {
		if strings.Contains(lower, needle) {
			return true
		}
	}
	return false
}

func apiScopeName(value any) *string {
	name, ok := quotaJSONPointer(value, "/scope/model/display_name")
	if !ok {
		return nil
	}
	text, ok := name.(string)
	if !ok {
		return nil
	}
	lower := quotaASCIILower(text)
	return &lower
}
