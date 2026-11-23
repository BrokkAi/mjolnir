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
	"fmt"
	"math"
	"net/http"
	"net/url"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"time"

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
	panic("unported: mj-accounts/src/claude_usage.rs runs_cli")
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
	panic("unported: mj-accounts/src/claude_usage.rs query")
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
	panic("unported: mj-accounts/src/claude_usage.rs query_cli_with")
}

func queryWithRuntime(ctx context.Context, home string, environment map[string]string, usageEndpoint string, executor core.CommandExecutor, launcher ClaudeLauncher) (ClaudeUsageReport, error) {
	panic("unported: mj-accounts/src/claude_usage.rs query_with_runtime")
}

// readCredentials is the profile's `.credentials.json` as a JSON value, or
// ClaudeUsageErrorNotSignedIn when it is missing or not JSON.
func readCredentials(ctx context.Context, home string) (any, error) {
	panic("unported: mj-accounts/src/claude_usage.rs read_credentials")
}

func queryAPI(ctx context.Context, httpClient *http.Client, usageEndpoint string, token string, credentials any) (ClaudeUsageReport, error) {
	panic("unported: mj-accounts/src/claude_usage.rs query_api")
}

func queryOauthJSON(ctx context.Context, httpClient *http.Client, endpoint *url.URL, token string, timeout time.Duration, organization *string) (any, error) {
	panic("unported: mj-accounts/src/claude_usage.rs query_oauth_json")
}

func queryPaidCredits(ctx context.Context, httpClient *http.Client, usageEndpoint string, token string, extra any) (client.PaidCredits, error) {
	panic("unported: mj-accounts/src/claude_usage.rs query_paid_credits")
}

func refreshAndRetry(ctx context.Context, httpClient *http.Client, usageEndpoint string, home string, environment map[string]string, executor core.CommandExecutor, rejectedToken *string, launcher ClaudeLauncher) (ClaudeUsageReport, error) {
	panic("unported: mj-accounts/src/claude_usage.rs refresh_and_retry")
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
	panic("unported: mj-accounts/src/claude_usage.rs refresh_grant_if_due")
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
	panic("unported: mj-accounts/src/claude_usage.rs LoginSource::read")
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
// Drop).
type renewal struct {
	locks     *core.CredentialWriteGuard
	admission *core.UpgradeWork
	program   claudeProgram
}

func (r *renewal) release() {
	panic("unported: mj-accounts/src/claude_usage.rs Renewal drop")
}

// renewLocked runs `claude auth login` with the stored refresh token while
// the caller holds the native locks, and returns the credentials it left.
// Fails when the access token did not change.
func renewLocked(ctx context.Context, active any, source LoginSource, environment map[string]string, executor core.CommandExecutor, held renewal) (any, error) {
	panic("unported: mj-accounts/src/claude_usage.rs renew_locked")
}

// runClaudeUsage reads Claude's quota, on macOS, through its Keychain-aware
// CLI.
func runClaudeUsage(ctx context.Context, environment map[string]string, executor core.CommandExecutor, purpose string, program claudeProgram) (core.CommandOutput, error) {
	panic("unported: mj-accounts/src/claude_usage.rs run_claude_usage")
}

func runClaudeRefresh(ctx context.Context, credentials any, source LoginSource, environment map[string]string, executor core.CommandExecutor, held renewal) error {
	panic("unported: mj-accounts/src/claude_usage.rs run_claude_refresh")
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
	panic("unported: mj-accounts/src/claude_usage.rs claude_refresh_failure_category")
}

func oauthAccessToken(credentials any, nowMs int64) (string, error) {
	panic("unported: mj-accounts/src/claude_usage.rs oauth_access_token")
}

func oauthExpiresAt(oauth any) (int64, bool) {
	panic("unported: mj-accounts/src/claude_usage.rs oauth_expires_at")
}

func oauthHasRefreshToken(credentials any) bool {
	panic("unported: mj-accounts/src/claude_usage.rs oauth_has_refresh_token")
}

// monthlyPriceUsd is the monthly list price, deliberately independent of the
// advertised usage multipliers: Max's weekly allowance does not scale with
// its five-hour one.
func monthlyPriceUsd(credentials any) *uint32 {
	panic("unported: mj-accounts/src/claude_usage.rs monthly_price_usd")
}

func parseAPIUsage(payload any) *ClaudeUsageReport {
	panic("unported: mj-accounts/src/claude_usage.rs parse_api_usage")
}

func parseBankedResets(payload any, now int64) *uint64 {
	panic("unported: mj-accounts/src/claude_usage.rs parse_banked_resets")
}

func apiWindow(value any, percentKey string) *ClaudeUsageWindow {
	panic("unported: mj-accounts/src/claude_usage.rs api_window")
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
	panic("unported: mj-accounts/src/claude_usage.rs parse_cli_usage")
}

// cliWindow is one window from the text after a `/usage` line's label.
func cliWindow(rest string) *ClaudeUsageWindow {
	panic("unported: mj-accounts/src/claude_usage.rs cli_window")
}

// stripANSI removes SGR escapes: Claude Code paints `/usage` for a terminal,
// so its output can carry them even in print mode.
func stripANSI(input string) string {
	panic("unported: mj-accounts/src/claude_usage.rs strip_ansi")
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
	panic("unported: mj-accounts/src/claude_usage.rs api_scope_name")
}
