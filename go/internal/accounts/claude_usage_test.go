// Port of the tests in mj-accounts/src/claude_usage.rs.

package accounts

import (
	"context"
	"errors"
	"fmt"
	"io"
	"maps"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// claudeCLIUsage is captured verbatim from
// `claude -p /usage --no-session-persistence`.
const claudeCLIUsage = `You are currently using your subscription to power your Claude Code usage

Current session: 6% used · resets Sep 18 at 6:20pm (Europe/Paris)
Current week (all models): 30% used · resets Sep 20 at 1:59pm (Europe/Paris)
Current week (Fable): 40% used · resets Sep 20 at 2pm (Europe/Paris)

What's contributing to your limits usage?
Approximate, based on local sessions on this machine — does not include other devices or claude.ai. Behaviors are independent characteristics, not a breakdown.

Last 24h · 2108 requests · 13 sessions
  57% of your usage was at >150k context
  41% of your usage was while 4+ sessions ran in parallel
  31% of your usage came from subagent-heavy sessions
  Top skills: /code-review 3%
  Top MCP servers: claude-in-chrome 1%

Last 7d · 5966 requests · 78 sessions
  52% of your usage was at >150k context
  28% of your usage came from subagent-heavy sessions
  28% of your usage was while 4+ sessions ran in parallel
  Top skills: /code-review 2%
  Top subagents: Explore 2%, Plan 1%
  Top MCP servers: claude-in-chrome 2%
`

func claudeParse(t *testing.T, text string) any {
	t.Helper()
	value, err := jsonvalue.Parse([]byte(text))
	if err != nil {
		t.Fatalf("%s: %v", text, err)
	}
	return value
}

func TestBankedResetsCountUnspentGrantsWithoutRequiringImmediateUse(t *testing.T) {
	payload := claudeParse(t, `{"cedar_ember": {"eligible": true, "grants": [
		{"resets_left": 1, "usable_now": false, "use_requires_limit": true},
		{"resets_left": 2, "starts_at": "1970-01-01T00:01:40Z", "ends_at": "1970-01-01T00:03:20Z"},
		{"resets_left": 5, "ends_at": "1970-01-01T00:01:40Z"},
		{"resets_left": 7, "starts_at": "1970-01-01T00:03:20Z"}
	]}}`)
	if banked := parseBankedResets(payload, 100); banked == nil || *banked != 3 {
		t.Fatalf("banked = %v", banked)
	}
	if banked := parseBankedResets(claudeParse(t, `{"cedar_ember": {"eligible": false}}`), 100); banked == nil || *banked != 0 {
		t.Fatalf("banked = %v", banked)
	}
	if banked := parseBankedResets(claudeParse(t, `{"cedar_ember": {"eligible": true, "grants": []}}`), 100); banked == nil || *banked != 0 {
		t.Fatalf("banked = %v", banked)
	}
}

func TestUnknownResetMetadataDoesNotDiscardClaudeUsage(t *testing.T) {
	for _, block := range []string{
		`null`,
		`{}`,
		`{"eligible": true}`,
		`{"eligible": true, "grants": [{"resets_left": -1}]}`,
		`{"eligible": true, "grants": [{"resets_left": "1"}]}`,
		`{"eligible": true, "grants": [{"resets_left": 1, "ends_at": "invalid"}]}`,
	} {
		report := parseAPIUsage(claudeParse(t, `{"five_hour": {"utilization": 25}, "cedar_ember": `+block+`}`))
		if report == nil {
			t.Fatalf("%s: the usage was discarded", block)
		}
		if report.BankedResets != nil {
			t.Fatalf("%s: banked = %d", block, *report.BankedResets)
		}
		if report.FiveHour.RemainingPercent != 75 {
			t.Fatalf("%s: five hour = %#v", block, report.FiveHour)
		}
	}
}

func TestCliUsageReportsBothWindowsAndTheBindingWeeklyLimit(t *testing.T) {
	report := parseCLIUsage(claudeCLIUsage)
	if report == nil {
		t.Fatal("the captured output parses")
	}

	fiveHour := report.FiveHour
	if fiveHour == nil || fiveHour.RemainingPercent != 94 || fiveHour.ResetContext == nil ||
		*fiveHour.ResetContext != "Sep 18 at 6:20pm (Europe/Paris)" {
		t.Fatalf("session window = %#v", fiveHour)
	}
	// The all-models limit remains distinct from the Fable limit.
	week := report.Week
	if week == nil || week.RemainingPercent != 70 || week.ResetContext == nil ||
		*week.ResetContext != "Sep 20 at 1:59pm (Europe/Paris)" {
		t.Fatalf("weekly window = %#v", week)
	}
	if len(report.ScopedWeeks) != 1 {
		t.Fatalf("scoped weeks = %#v", report.ScopedWeeks)
	}
	scoped := report.ScopedWeeks[0]
	if scoped.Scope != "fable" || scoped.Window.RemainingPercent != 60 ||
		scoped.Window.DurationSeconds == nil || *scoped.Window.DurationSeconds != 604_800 {
		t.Fatalf("scoped week = %#v", scoped)
	}
}

func TestAPIUsageKeepsAllAndScopedWeeklyWindowsSeparate(t *testing.T) {
	report := parseAPIUsage(claudeParse(t, `{
		"limits": [
			{"kind": "weekly_all", "percent": 30},
			{"kind": "weekly_scoped", "scope": {"model": {"display_name": "Fable"}}, "percent": 40},
			{"kind": "weekly_scoped", "scope": {"model": {"display_name": "Mythos"}}, "percent": 50}
		]
	}`))
	if report == nil || report.Week.RemainingPercent != 70 {
		t.Fatalf("report = %#v", report)
	}
	if len(report.ScopedWeeks) != 2 || report.ScopedWeeks[0].Scope != "fable" ||
		report.ScopedWeeks[0].Window.RemainingPercent != 60 || report.ScopedWeeks[1].Scope != "mythos" {
		t.Fatalf("scoped weeks = %#v", report.ScopedWeeks)
	}

	legacy := parseAPIUsage(claudeParse(t, `{
		"seven_day": {"utilization": 25},
		"seven_day_fable": {"utilization": 100}
	}`))
	if legacy == nil || legacy.Week.RemainingPercent != 75 {
		t.Fatalf("legacy = %#v", legacy)
	}
	if legacy.ScopedWeeks[0].Scope != "fable" || legacy.ScopedWeeks[0].Window.RemainingPercent != 0 {
		t.Fatalf("legacy scoped weeks = %#v", legacy.ScopedWeeks)
	}
}

// The later sections are full of percentages that are not quota.
func TestCliUsageIgnoresPercentagesThatAreNotAWindow(t *testing.T) {
	report := parseCLIUsage("Last 24h · 2108 requests · 13 sessions\n  " +
		"57% of your usage was at >150k context\n  " +
		"Top skills: /code-review 3%\n")
	if report != nil {
		t.Fatalf("report = %#v", report)
	}
}

func TestCliUsageSurvivesTerminalColouring(t *testing.T) {
	coloured := "\x1b[1mCurrent session:\x1b[0m \x1b[32m6% used\x1b[0m · resets Sep 18 at 6:20pm\n"
	report := parseCLIUsage(coloured)
	if report == nil || report.FiveHour.RemainingPercent != 94 {
		t.Fatalf("coloured output parses: %#v", report)
	}
}

// A window whose number stops meaning "used" must read as unparsed rather
// than as its own complement.
func TestCliUsageRefusesAPercentageItCannotInterpret(t *testing.T) {
	if report := parseCLIUsage("Current session: 6% left · resets Sep 18 at 6:20pm\n"); report != nil {
		t.Fatalf("report = %#v", report)
	}
}

// Hard-won: #1485: discarded native failures hid the reason while raw Axios output can expose tokens.
func TestRefreshFailureCategoriesNeverDiscloseNativeOutput(t *testing.T) {
	for _, failure := range []struct{ message, category string }{
		{"Login failed: Request failed with status code 400", ": HTTP 400 rejection"},
		{"AxiosError: Request failed with status code 401", ": HTTP 401 rejection"},
		{"Login failed: Request failed with status code 403", ": HTTP 403 rejection"},
		{"Login failed: getaddrinfo ENOTFOUND private.example", ": DNS lookup failure"},
		{"Login failed: getaddrinfo EAI_AGAIN private.example", ": DNS lookup failure"},
		{"Login failed: connect ECONNREFUSED 127.0.0.1:8443", ": network connection refused"},
		{
			"Private organization refused sign-in\nThe refresh token in CLAUDE_CODE_OAUTH_REFRESH_TOKEN was used for this attempt and cannot be used again; issue a new one to retry.",
			": native sign-in policy refusal after token exchange",
		},
		{"Login failed: Request failed with status code 4010", ""},
		{"Login failed: provider sent private response 401", ""},
		{"Login failed: unknown private failure", ""},
		{"Login failed: Request failed with status code 500", ""},
	} {
		// Simulate Axios's token-bearing request dump and independent
		// stdout, including arbitrary bytes that must never be echoed.
		dump := []byte(fmt.Sprintf("%s\nconfig: {data: '{\"refresh_token\":\"SECRET_REFRESH\"}', headers: {Authorization: 'Bearer SECRET_ACCESS'}}\n", failure.message))
		dump = append(dump, []byte("\xffFAILED_STDERR_SENTINEL")...)
		for _, stderrFirst := range []bool{true, false} {
			sentinel := []byte("FAILED_STDOUT_SENTINEL")
			stdout, stderr := sentinel, dump
			if !stderrFirst {
				stdout, stderr = dump, sentinel
			}
			err := successfulRefreshOutput(core.CommandOutput{Status: 7, Stdout: stdout, Stderr: stderr})
			want := ClaudeUsageErrorRefresh{Value: "Claude auth login exited with status 7" + failure.category}
			if err != want {
				t.Fatalf("classification must return only fixed text for %q: %v", failure.message, err)
			}
		}
	}
	if err := successfulRefreshOutput(core.CommandOutput{
		Status: 0,
		Stdout: []byte("Login failed: Request failed with status code 401"),
		Stderr: []byte("SECRET_SUCCESS_SENTINEL"),
	}); err != nil {
		t.Fatal(err)
	}
}

// claudeFakeExecutor is a hand-written CommandExecutor whose commands run
// through execute; it has no deadline and no cancellation.
type claudeFakeExecutor struct {
	core.CommandExecutorDefaults
	execute func(command *core.CommandSpec) (core.CommandOutput, error)
}

func (e *claudeFakeExecutor) Execute(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.execute(command)
}

func (e *claudeFakeExecutor) ExecuteCleanup(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e *claudeFakeExecutor) ExecuteCleanupWithTimeout(command *core.CommandSpec, _ time.Duration) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e *claudeFakeExecutor) ExecuteWithoutDeadline(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e *claudeFakeExecutor) ExecuteWithStdinWithoutDeadline(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	return e.ExecuteWithStdin(command, input)
}

func (e *claudeFakeExecutor) CancellationRequestedWithoutDeadline() bool {
	return e.CancellationRequested()
}

func (e *claudeFakeExecutor) PauseDeadline() *core.DeadlinePause { return core.DeadlinePauseNone() }

// claudeRefreshExecutor stands in for `claude auth login`: it checks the
// command and the native locks, records the command, and writes the
// replacement login when there is one.
type claudeRefreshExecutor struct {
	t           *testing.T
	home        string
	replacement any
	status      int
	mu          sync.Mutex
	commands    []core.CommandSpec
}

func (r *claudeRefreshExecutor) run(command *core.CommandSpec) (core.CommandOutput, error) {
	if !slices.Equal(command.Args, []string{"auth", "login"}) {
		r.t.Errorf("args = %q", command.Args)
	}
	if got := command.Env["CLAUDE_CONFIG_DIR"]; got != r.home {
		r.t.Errorf("CLAUDE_CONFIG_DIR = %q", got)
	}
	if info, err := os.Stat(filepath.Join(r.home, ".oauth_refresh.lock")); err != nil || !info.IsDir() {
		r.t.Errorf("the refresh lock is not held: %v", err)
	}
	canonical, err := filepath.EvalSymlinks(r.home)
	if err != nil {
		r.t.Error(err)
	}
	if info, err := os.Stat(canonical + ".lock"); err != nil || !info.IsDir() {
		r.t.Errorf("the legacy lock is not held: %v", err)
	}
	r.mu.Lock()
	recorded := *command
	recorded.Env = maps.Clone(command.Env)
	r.commands = append(r.commands, recorded)
	r.mu.Unlock()
	if r.replacement != nil {
		body, err := jsonvalue.Marshal(r.replacement)
		if err != nil {
			return core.CommandOutput{}, err
		}
		if err := os.WriteFile(filepath.Join(r.home, ".credentials.json"), body, 0o666); err != nil {
			return core.CommandOutput{}, err
		}
	}
	return core.CommandOutput{Status: r.status, Stdout: []byte("Approximate local usage")}, nil
}

func (r *claudeRefreshExecutor) recorded() []core.CommandSpec {
	r.mu.Lock()
	defer r.mu.Unlock()
	return slices.Clone(r.commands)
}

func (r *claudeRefreshExecutor) executor() *claudeFakeExecutor {
	return &claudeFakeExecutor{execute: r.run}
}

// claudeUsageServer is the OAuth usage endpoint: it checks each request and
// records its authorization, refusing the first when asked to.
type claudeUsageServer struct {
	t              *testing.T
	rejectFirst    bool
	mu             sync.Mutex
	authorizations []string
}

func (s *claudeUsageServer) ServeHTTP(response http.ResponseWriter, request *http.Request) {
	if request.URL.Path != "/usage" {
		http.NotFound(response, request)
		return
	}
	query := request.URL.Query()
	if query.Get("cedar_ember") != "1" || query.Has("skip_spend") {
		s.t.Errorf("query = %q", request.URL.RawQuery)
	}
	if got := request.Header.Get("User-Agent"); got != usageUserAgent {
		s.t.Errorf("user agent = %q", got)
	}
	s.mu.Lock()
	reject := s.rejectFirst && len(s.authorizations) == 0
	s.authorizations = append(s.authorizations, request.Header.Get("Authorization"))
	s.mu.Unlock()
	response.Header().Set("Content-Type", "application/json")
	if reject {
		response.WriteHeader(http.StatusUnauthorized)
		io.WriteString(response, `{}`)
		return
	}
	io.WriteString(response, `{
		"five_hour": {"utilization": 25.0, "resets_at": "2026-08-23T00:00:00Z"},
		"seven_day": {"utilization": 40.0, "resets_at": "2026-08-29T00:00:00Z"},
		"cedar_ember": {"eligible": true, "grants": [{"resets_left": 1}]}
	}`)
}

func (s *claudeUsageServer) seen() []string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return slices.Clone(s.authorizations)
}

func spawnClaudeUsageServer(t *testing.T, rejectFirst bool) (string, *claudeUsageServer) {
	state := &claudeUsageServer{t: t, rejectFirst: rejectFirst}
	server := httptest.NewServer(state)
	t.Cleanup(server.Close)
	return server.URL + "/usage", state
}

func claudeCredentials(token string, refresh *string, expiresAt int64) jsonvalue.Object {
	oauth := jsonvalue.Object{
		{Key: "accessToken", Value: token},
		{Key: "expiresAt", Value: expiresAt},
		{Key: "scopes", Value: []any{"user:profile", "user:inference"}},
	}
	if refresh != nil {
		oauth.Set("refreshToken", *refresh)
	}
	return jsonvalue.Object{{Key: "claudeAiOauth", Value: oauth}}
}

func claudeWriteCredentials(t *testing.T, home string, credentials any) {
	t.Helper()
	body, err := jsonvalue.Marshal(credentials)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, ".credentials.json"), body, 0o666); err != nil {
		t.Fatal(err)
	}
}

// claudeSetOAuth sets one field of a login's claudeAiOauth object.
func claudeSetOAuth(credentials jsonvalue.Object, key string, value any) {
	oauth, _ := credentials.Get("claudeAiOauth")
	object := oauth.(jsonvalue.Object)
	object.Set(key, value)
	credentials.Set("claudeAiOauth", object)
}

// claudeQueryWith is Rust's test-only `query_with`: the runtime query with
// the `claude` on PATH as its launcher.
func claudeQueryWith(ctx context.Context, home string, environment map[string]string, usageEndpoint string, executor core.CommandExecutor) (ClaudeUsageReport, error) {
	return queryWithRuntime(ctx, home, environment, usageEndpoint, executor, ClaudeLauncherProgram{Value: "claude"})
}

func TestPaidUsageReadsWalletAndRespectsEnabledStatusAndSpendingCap(t *testing.T) {
	float := func(value float64) *float64 { return &value }
	for _, paid := range []struct {
		funds        int64
		cap          *int64
		spent        int64
		enabled      bool
		walletStatus int
		expected     *float64
	}{
		{1200, quotaI64(2000), 500, true, http.StatusOK, float(1200)},
		{5000, quotaI64(2000), 500, true, http.StatusOK, float(1500)},
		{5000, quotaI64(2000), 2100, true, http.StatusOK, float(0)},
		{5000, nil, 500, true, http.StatusOK, float(5000)},
		{0, quotaI64(2000), 500, true, http.StatusOK, float(0)},
		{5000, quotaI64(2000), 500, false, http.StatusOK, nil},
		{5000, quotaI64(2000), 500, true, http.StatusForbidden, nil},
	} {
		var walletReads atomic.Int64
		mux := http.NewServeMux()
		mux.HandleFunc("GET /usage", func(response http.ResponseWriter, request *http.Request) {
			var monthlyLimit any
			if paid.cap != nil {
				monthlyLimit = *paid.cap
			}
			body, _ := jsonvalue.Marshal(jsonvalue.Object{
				{Key: "five_hour", Value: jsonvalue.Object{{Key: "utilization", Value: int64(100)}}},
				{Key: "extra_usage", Value: jsonvalue.Object{
					{Key: "is_enabled", Value: paid.enabled},
					{Key: "monthly_limit", Value: monthlyLimit},
					{Key: "used_credits", Value: paid.spent},
					{Key: "currency", Value: "USD"},
				}},
			})
			response.Header().Set("Content-Type", "application/json")
			response.Write(body)
		})
		mux.HandleFunc("GET /api/oauth/profile", func(response http.ResponseWriter, request *http.Request) {
			if got := request.Header.Get("Authorization"); got != "Bearer test-token" {
				t.Errorf("authorization = %q", got)
			}
			response.Header().Set("Content-Type", "application/json")
			io.WriteString(response, `{"organization": {"uuid": "test-org"}}`)
		})
		mux.HandleFunc("GET /api/oauth/organizations/test-org/prepaid/credits", func(response http.ResponseWriter, request *http.Request) {
			walletReads.Add(1)
			if got := request.Header.Get("Authorization"); got != "Bearer test-token" {
				t.Errorf("authorization = %q", got)
			}
			if got := request.Header.Get("x-organization-uuid"); got != "test-org" {
				t.Errorf("organization = %q", got)
			}
			response.Header().Set("Content-Type", "application/json")
			response.WriteHeader(paid.walletStatus)
			fmt.Fprintf(response, `{"amount": %d, "currency": "USD"}`, paid.funds)
		})
		server := httptest.NewServer(mux)
		report, err := queryAPI(t.Context(), &http.Client{}, server.URL+"/usage", "test-token", jsonvalue.Object{})
		server.Close()
		if err != nil {
			t.Fatal(err)
		}
		if report.FiveHour.RemainingPercent != 0 {
			t.Fatalf("five hour = %#v", report.FiveHour)
		}
		var expected *client.PaidCredits
		if paid.expected != nil {
			expected = client.NewPaidCreditsLimited(*paid.expected)
		}
		if !reflect.DeepEqual(report.PaidCredits, expected) {
			t.Fatalf("%+v: paid credits = %#v", paid, report.PaidCredits)
		}
		reads := int64(0)
		if paid.enabled {
			reads = 1
		}
		if got := walletReads.Load(); got != reads {
			t.Fatalf("%+v: wallet reads = %d", paid, got)
		}
	}
}

func TestA429IsARateLimitThatCarriesTheServersRetryAfter(t *testing.T) {
	home := t.TempDir()
	claudeWriteCredentials(t, home, claudeCredentials("token", nil, core.EpochMillis()+3_600_000))
	twoMinutes := 120 * time.Second
	for _, limit := range []struct {
		header   *string
		expected *time.Duration
	}{
		{quotaStr("120"), &twoMinutes},
		{nil, nil},
		{quotaStr("not a delay"), nil},
	} {
		server := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
			if limit.header != nil {
				response.Header().Set("Retry-After", *limit.header)
			}
			response.WriteHeader(http.StatusTooManyRequests)
		}))
		refresh := &claudeRefreshExecutor{t: t, home: home}
		_, err := claudeQueryWith(t.Context(), home, map[string]string{}, server.URL+"/usage", refresh.executor())
		server.Close()
		if !reflect.DeepEqual(err, ClaudeUsageErrorRateLimited{RetryAfter: limit.expected}) {
			t.Fatalf("%v: error = %#v", limit.header, err)
		}
		if err.Error() != "rate limited" {
			t.Fatalf("error = %q", err)
		}
	}
}

// Hard-won: 958aba5ce: successful /usage stopped renewing expired OAuth tokens.
func TestExpiredLoginExchangesItsRefreshTokenWithoutStartingASession(t *testing.T) {
	home := t.TempDir()
	old := claudeCredentials("old", quotaStr("refresh"), 1)
	claudeSetOAuth(old, "rateLimitTier", "default_claude_max_5x")
	claudeSetOAuth(old, "clientId", "custom-client")
	claudeWriteCredentials(t, home, old)
	fresh := claudeCredentials("fresh", quotaStr("rotated"), core.EpochMillis()+60_000)
	claudeSetOAuth(fresh, "rateLimitTier", "default_claude_max_20x")
	refresh := &claudeRefreshExecutor{t: t, home: home, replacement: fresh}
	usageURL, server := spawnClaudeUsageServer(t, false)
	environment := map[string]string{"CLAUDE_CONFIG_DIR": home}

	report, err := claudeQueryWith(t.Context(), home, maps.Clone(environment), usageURL, refresh.executor())
	if err != nil {
		t.Fatal(err)
	}

	if report.MonthlyPriceUsd == nil || *report.MonthlyPriceUsd != 200 {
		t.Fatalf("price = %v", report.MonthlyPriceUsd)
	}
	if report.BankedResets == nil || *report.BankedResets != 1 {
		t.Fatalf("banked = %v", report.BankedResets)
	}
	if report.FiveHour.RemainingPercent != 75 {
		t.Fatalf("five hour = %#v", report.FiveHour)
	}
	commands := refresh.recorded()
	if len(commands) != 1 || !slices.Equal(commands[0].Args, []string{"auth", "login"}) {
		t.Fatalf("commands = %#v", commands)
	}
	environment["CLAUDE_CODE_OAUTH_REFRESH_TOKEN"] = "refresh"
	environment["CLAUDE_CODE_OAUTH_SCOPES"] = "user:profile user:inference"
	environment["CLAUDE_CODE_OAUTH_CLIENT_ID"] = "custom-client"
	if !maps.Equal(commands[0].Env, environment) {
		t.Fatalf("env = %q", commands[0].Env)
	}
	if seen := server.seen(); !slices.Equal(seen, []string{"Bearer fresh"}) {
		t.Fatalf("authorizations = %q", seen)
	}
}

func TestAuthoritativeRejectionRefreshesOnceAndRetriesWithNewCredentials(t *testing.T) {
	home := t.TempDir()
	freshExpiry := core.EpochMillis() + 60_000
	claudeWriteCredentials(t, home, claudeCredentials("old", quotaStr("refresh"), freshExpiry))
	refresh := &claudeRefreshExecutor{t: t, home: home, replacement: claudeCredentials("fresh", quotaStr("rotated"), freshExpiry)}
	usageURL, server := spawnClaudeUsageServer(t, true)

	if _, err := claudeQueryWith(t.Context(), home, map[string]string{}, usageURL, refresh.executor()); err != nil {
		t.Fatal(err)
	}

	commands := refresh.recorded()
	if len(commands) != 1 || commands[0].Env["CLAUDE_CODE_OAUTH_CLIENT_ID"] != "" {
		t.Fatalf("commands = %#v", commands)
	}
	if _, set := commands[0].Env["CLAUDE_CODE_OAUTH_CLIENT_ID"]; !set {
		t.Fatal("an empty client ID overrides an ambient one")
	}
	if seen := server.seen(); !slices.Equal(seen, []string{"Bearer old", "Bearer fresh"}) {
		t.Fatalf("authorizations = %q", seen)
	}
}

// claudeBlockedRefresh holds `claude auth login` until finish is signalled,
// after reporting that it started.
type claudeBlockedRefresh struct {
	inner   *claudeRefreshExecutor
	started chan struct{}
	once    sync.Once
	finish  chan struct{}
}

func (b *claudeBlockedRefresh) run(command *core.CommandSpec) (core.CommandOutput, error) {
	b.once.Do(func() { close(b.started) })
	select {
	case <-b.finish:
	case <-time.After(10 * time.Second):
		return core.CommandOutput{}, errors.New("timed out waiting on a recv operation")
	}
	return b.inner.run(command)
}

func newClaudeBlockedRefresh(inner *claudeRefreshExecutor) *claudeBlockedRefresh {
	return &claudeBlockedRefresh{inner: inner, started: make(chan struct{}), finish: make(chan struct{}, 1)}
}

func (b *claudeBlockedRefresh) executor() *claudeFakeExecutor {
	return &claudeFakeExecutor{execute: b.run}
}

func TestConcurrentQuotaReadersReuseTheTokenRenewedUnderClaudesLock(t *testing.T) {
	home := t.TempDir()
	claudeWriteCredentials(t, home, claudeCredentials("old", quotaStr("refresh"), 1))
	refresh := &claudeRefreshExecutor{t: t, home: home, replacement: claudeCredentials("fresh", quotaStr("rotated"), core.EpochMillis()+60_000)}
	blocked := newClaudeBlockedRefresh(refresh)
	executor := blocked.executor()
	usageURL, server := spawnClaudeUsageServer(t, false)
	first := make(chan error, 1)
	go func() {
		_, err := claudeQueryWith(t.Context(), home, map[string]string{}, usageURL, executor)
		first <- err
	}()
	<-blocked.started
	second := make(chan error, 1)
	go func() {
		_, err := claudeQueryWith(t.Context(), home, map[string]string{}, usageURL, executor)
		second <- err
	}()
	// Poll the second reader while the first remains inside its exchange.
	select {
	case err := <-second:
		t.Fatalf("the second reader finished inside the first's exchange: %v", err)
	case <-time.After(150 * time.Millisecond):
	}
	blocked.finish <- struct{}{}
	if err := <-first; err != nil {
		t.Fatal(err)
	}
	if err := <-second; err != nil {
		t.Fatal(err)
	}
	if commands := refresh.recorded(); len(commands) != 1 {
		t.Fatalf("%d renewals", len(commands))
	}
	if seen := server.seen(); !slices.Equal(seen, []string{"Bearer fresh", "Bearer fresh"}) {
		t.Fatalf("authorizations = %q", seen)
	}
}

func TestCancellingAQuotaReaderKeepsItsRefreshLockUntilTheChildFinishes(t *testing.T) {
	home := t.TempDir()
	claudeWriteCredentials(t, home, claudeCredentials("old", quotaStr("refresh"), 1))
	blocked := newClaudeBlockedRefresh(&claudeRefreshExecutor{t: t, home: home, status: 1})
	ctx, cancel := context.WithCancel(t.Context())
	task := make(chan error, 1)
	go func() {
		_, err := claudeQueryWith(ctx, home, map[string]string{}, "http://127.0.0.1:1/unused", blocked.executor())
		task <- err
	}()
	<-blocked.started
	cancel()
	if err := <-task; !errors.Is(err, context.Canceled) {
		t.Fatalf("the reader was cancelled: %v", err)
	}
	lock := filepath.Join(home, ".oauth_refresh.lock")
	if info, err := os.Stat(lock); err != nil || !info.IsDir() {
		t.Fatalf("the cancelled wait still has a running child: %v", err)
	}
	blocked.finish <- struct{}{}
	deadline := time.Now().Add(5 * time.Second)
	for {
		if _, err := os.Stat(lock); errors.Is(err, os.ErrNotExist) {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("the lock outlived the child")
		}
		time.Sleep(10 * time.Millisecond)
	}
}

// Hard-won: 85b96a6d5: full-suite load could delay lock acquisition past the test's 150ms wait.
func TestALegacyClaudeLockDelaysRefreshAndTheWaiterRereadsTheRenewedToken(t *testing.T) {
	home := t.TempDir()
	path := filepath.Join(home, ".credentials.json")
	claudeWriteCredentials(t, home, claudeCredentials("old", quotaStr("refresh"), 1))
	canonical, err := filepath.EvalSymlinks(home)
	if err != nil {
		t.Fatal(err)
	}
	legacy := canonical + ".lock"
	if err := os.Mkdir(legacy, 0o777); err != nil {
		t.Fatal(err)
	}
	refresh := &claudeRefreshExecutor{t: t, home: home}
	usageURL, server := spawnClaudeUsageServer(t, false)
	query := make(chan error, 1)
	go func() {
		_, err := claudeQueryWith(t.Context(), home, map[string]string{}, usageURL, refresh.executor())
		query <- err
	}()
	lock := filepath.Join(home, ".oauth_refresh.lock")
	deadline := time.Now().Add(5 * time.Second)
	for {
		select {
		case err := <-query:
			t.Fatalf("quota reader finished while the legacy lock was held: %v", err)
		default:
		}
		if info, err := os.Stat(lock); err == nil && info.IsDir() {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("quota reader did not acquire its first refresh lock")
		}
		time.Sleep(10 * time.Millisecond)
	}
	select {
	case err := <-query:
		t.Fatalf("quota reader finished while the legacy lock was held: %v", err)
	case <-time.After(150 * time.Millisecond):
	}
	if commands := refresh.recorded(); len(commands) != 0 {
		t.Fatalf("commands = %#v", commands)
	}
	if info, err := os.Stat(lock); err != nil || !info.IsDir() {
		t.Fatalf("lock = %v", err)
	}
	body, err := jsonvalue.Marshal(claudeCredentials("fresh", quotaStr("rotated"), core.EpochMillis()+60_000))
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, body, 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(legacy); err != nil {
		t.Fatal(err)
	}
	if err := <-query; err != nil {
		t.Fatal(err)
	}
	if commands := refresh.recorded(); len(commands) != 0 {
		t.Fatalf("commands = %#v", commands)
	}
	if seen := server.seen(); !slices.Equal(seen, []string{"Bearer fresh"}) {
		t.Fatalf("authorizations = %q", seen)
	}
}

// Hard-won: 958aba5ce: a successful CLI exit did not prove that credentials were renewed.
func TestSuccessfulCliWithoutTokenRenewalIsReportedAsARefreshFailure(t *testing.T) {
	for _, expiresAt := range []int64{1, core.EpochMillis() + 60_000} {
		home := t.TempDir()
		claudeWriteCredentials(t, home, claudeCredentials("old", quotaStr("refresh"), expiresAt))
		refresh := &claudeRefreshExecutor{t: t, home: home}
		usageURL, _ := spawnClaudeUsageServer(t, true)
		_, err := claudeQueryWith(t.Context(), home, map[string]string{}, usageURL, refresh.executor())
		if _, ok := err.(ClaudeUsageErrorRefresh); !ok {
			t.Fatalf("%v", err)
		}
		if _, err := os.Stat(filepath.Join(home, ".oauth_refresh.lock")); err == nil {
			t.Fatal("the refresh lock outlived the renewal")
		}
	}
}

func TestValidCredentialsQueryAuthoritativeUsageWithoutLaunchingClaude(t *testing.T) {
	home := t.TempDir()
	refresh := &claudeRefreshExecutor{t: t, home: home}
	executor := refresh.executor()
	usageURL, server := spawnClaudeUsageServer(t, false)
	price := func(value uint32) *uint32 { return &value }
	// Real OAuth tier metadata must survive the HTTP query, including a plan
	// change between polls. Unknown plans must not invent a price.
	for _, tier := range []struct {
		name     string
		expected *uint32
	}{
		{"default_claude_max_5x", price(100)},
		{"default_claude_max_20x", price(200)},
		{"default_claude_pro", price(20)},
		{"future-plan", nil},
	} {
		login := claudeCredentials("current", quotaStr("refresh"), core.EpochMillis()+60_000)
		claudeSetOAuth(login, "rateLimitTier", tier.name)
		claudeWriteCredentials(t, home, login)
		report, err := claudeQueryWith(t.Context(), home, map[string]string{}, usageURL, executor)
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(report.MonthlyPriceUsd, tier.expected) {
			t.Fatalf("%s: price = %v", tier.name, report.MonthlyPriceUsd)
		}
	}
	if commands := refresh.recorded(); len(commands) != 0 {
		t.Fatalf("commands = %#v", commands)
	}
	if seen := server.seen(); !slices.Equal(seen, []string{"Bearer current", "Bearer current", "Bearer current", "Bearer current"}) {
		t.Fatalf("authorizations = %q", seen)
	}
}

func TestFailedRefreshOfARejectedTokenIsNotMislabeledLoginExpired(t *testing.T) {
	home := t.TempDir()
	claudeWriteCredentials(t, home, claudeCredentials("rejected", quotaStr("refresh"), core.EpochMillis()+60_000))
	refresh := &claudeRefreshExecutor{t: t, home: home, status: 1}
	usageURL, server := spawnClaudeUsageServer(t, true)

	_, err := claudeQueryWith(t.Context(), home, map[string]string{}, usageURL, refresh.executor())

	if _, ok := err.(ClaudeUsageErrorRefresh); !ok {
		t.Fatalf("%v", err)
	}
	if seen := server.seen(); !slices.Equal(seen, []string{"Bearer rejected"}) {
		t.Fatalf("authorizations = %q", seen)
	}
}

func TestFailedCliIsAcceptedWhenCredentialsWereRefreshedConcurrently(t *testing.T) {
	home := t.TempDir()
	claudeWriteCredentials(t, home, claudeCredentials("old", quotaStr("refresh"), 1))
	refresh := &claudeRefreshExecutor{
		t:           t,
		home:        home,
		replacement: claudeCredentials("fresh", quotaStr("rotated"), core.EpochMillis()+60_000),
		status:      1,
	}
	usageURL, _ := spawnClaudeUsageServer(t, false)

	report, err := claudeQueryWith(t.Context(), home, map[string]string{}, usageURL, refresh.executor())
	if err != nil {
		t.Fatal(err)
	}

	if report.Week.RemainingPercent != 60 {
		t.Fatalf("week = %#v", report.Week)
	}
}

func TestExpiredOauthAccessTokenIsLoginExpired(t *testing.T) {
	credentials := claudeParse(t, `{"claudeAiOauth": {"accessToken": "sk-ant-oat01-test", "expiresAt": 1000}}`)
	for _, now := range []int64{1_001, 1_000} {
		if _, err := oauthAccessToken(credentials, now); err != (ClaudeUsageErrorLoginExpired{}) {
			t.Fatalf("%d: %v", now, err)
		}
	}
}

func TestCurrentOauthAccessTokenIsUsable(t *testing.T) {
	credentials := claudeParse(t, `{"claudeAiOauth": {"accessToken": "sk-ant-oat01-test", "expiresAt": 2000}}`)
	if token, err := oauthAccessToken(credentials, 1_999); err != nil || token != "sk-ant-oat01-test" {
		t.Fatalf("token = %q, %v", token, err)
	}
}

// claudeFakeMac is a macOS Keychain and the `claude` that writes it, in one
// fake: `security` answers with the stored login, `claude auth login` swaps
// in the renewed one. It checks what a Keychain renewal must look like to
// Claude Code: no `CLAUDE_CONFIG_DIR`, and the `~/.claude` locks held.
type claudeFakeMac struct {
	t              *testing.T
	mu             sync.Mutex
	login          any
	securityStatus int
	renewed        any
	// lockHome is the directory Claude Code takes its refresh locks under.
	lockHome   string
	claudeRuns []core.CommandSpec
}

func (m *claudeFakeMac) run(command *core.CommandSpec) (core.CommandOutput, error) {
	if command.Program == "security" {
		if len(command.Args) < 3 || command.Args[0] != "find-generic-password" || command.Args[1] != "-a" ||
			!slices.Equal(command.Args[3:], []string{"-s", "Claude Code-credentials", "-w"}) {
			m.t.Errorf("security args = %q", command.Args)
		}
		m.mu.Lock()
		defer m.mu.Unlock()
		var stdout []byte
		if m.login != nil {
			stdout, _ = jsonvalue.Marshal(m.login)
		}
		return core.CommandOutput{Status: m.securityStatus, Stdout: stdout}, nil
	}
	if !slices.Equal(command.Args, []string{"auth", "login"}) {
		m.t.Errorf("args = %q", command.Args)
	}
	if _, set := command.Env["CLAUDE_CONFIG_DIR"]; set {
		m.t.Error("CLAUDE_CONFIG_DIR reaches a Keychain renewal")
	}
	if !slices.Contains(command.EnvRemove, "CLAUDE_CONFIG_DIR") {
		m.t.Errorf("env_remove = %q", command.EnvRemove)
	}
	if got := command.Env["USER"]; got != ClaudeKeychainAccount() {
		m.t.Errorf("the renewal writes the account the daemon reads: USER = %q", got)
	}
	if info, err := os.Stat(filepath.Join(m.lockHome, ".oauth_refresh.lock")); err != nil || !info.IsDir() {
		m.t.Errorf("the native lock is not held: %v", err)
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	recorded := *command
	recorded.Env = maps.Clone(command.Env)
	m.claudeRuns = append(m.claudeRuns, recorded)
	m.login = m.renewed
	return core.CommandOutput{}, nil
}

func (m *claudeFakeMac) runs() []core.CommandSpec {
	m.mu.Lock()
	defer m.mu.Unlock()
	return slices.Clone(m.claudeRuns)
}

func newClaudeFakeMac(t *testing.T, directory string, securityStatus int, expiresInMs int64) *claudeFakeMac {
	now := core.EpochMillis()
	lockHome, err := filepath.EvalSymlinks(directory)
	if err != nil {
		t.Fatal(err)
	}
	return &claudeFakeMac{
		t:              t,
		login:          claudeCredentials("access-1", quotaStr("refresh-1"), now+expiresInMs),
		securityStatus: securityStatus,
		renewed:        claudeCredentials("access-2", quotaStr("refresh-2"), now+8*3_600_000),
		lockHome:       lockHome,
	}
}

func claudeKeychainSource(directory string, mac *claudeFakeMac) LoginSource {
	return LoginSourceKeychain{
		LockMarker: filepath.Join(directory, ".credentials.json"),
		Security:   &claudeFakeExecutor{execute: mac.run},
	}
}

const claudeOneHourMs = int64(3_600_000)

func renewKeychainLogin(ctx context.Context, source LoginSource, mac *claudeFakeMac, rejectedLogin *string) (GrantRefresh, error) {
	// An ambient `CLAUDE_CONFIG_DIR`, as a profile's own environment may
	// set, must not reach the renewal. Nor may a `USER` from that
	// environment, which would move the renewed login to another Keychain
	// account.
	environment := map[string]string{
		"CLAUDE_CONFIG_DIR": "/elsewhere",
		"USER":              "someone-else",
	}
	rejected := map[string]struct{}{}
	if rejectedLogin != nil {
		rejected[*rejectedLogin] = struct{}{}
	}
	return RefreshGrantIfDue(ctx, source, environment, &claudeFakeExecutor{execute: mac.run},
		ClaudeLauncherProgram{Value: "claude"}, core.NewGate(),
		GrantRefreshTrigger{NowMs: core.EpochMillis(), RejectedLogins: rejected})
}

// TestADueKeychainLoginIsRenewedWithNoConfigDirUnderTheLock: #1491: a
// Keychain login is renewed through the Keychain as the source, under the
// locks Claude Code takes for `~/.claude`, with `CLAUDE_CONFIG_DIR` unset so
// `claude auth login` writes the item the sessions' copies come from; the
// next look finds it current.
func TestADueKeychainLoginIsRenewedWithNoConfigDirUnderTheLock(t *testing.T) {
	directory := t.TempDir()
	mac := newClaudeFakeMac(t, directory, 0, claudeOneHourMs)

	first, err := renewKeychainLogin(t.Context(), claudeKeychainSource(directory, mac), mac, nil)
	if err != nil {
		t.Fatal(err)
	}
	if renewed, ok := first.(GrantRefreshRenewed); !ok || !renewed.ByThisCall {
		t.Fatalf("%#v", first)
	}
	runs := mac.runs()
	if len(runs) != 1 || runs[0].Env["CLAUDE_CODE_OAUTH_REFRESH_TOKEN"] != "refresh-1" {
		t.Fatalf("runs = %#v", runs)
	}
	if _, err := os.Stat(filepath.Join(directory, ".oauth_refresh.lock")); err == nil {
		t.Fatal("the lock outlived the renewal")
	}

	second, err := renewKeychainLogin(t.Context(), claudeKeychainSource(directory, mac), mac, nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := second.(GrantRefreshCurrent); !ok {
		t.Fatalf("%#v", second)
	}
	if runs := mac.runs(); len(runs) != 1 {
		t.Fatalf("%d runs", len(runs))
	}
}

// TestAWaitingKeychainRenewalFollowsTheOneThatHeldTheLock: #1491: profiles
// on one Mac share the Keychain item. A renewal that waits for another's
// lock rereads the item under it, finds it renewed, and spends no second
// refresh token.
func TestAWaitingKeychainRenewalFollowsTheOneThatHeldTheLock(t *testing.T) {
	directory := t.TempDir()
	mac := newClaudeFakeMac(t, directory, 0, claudeOneHourMs)
	held, err := core.CredentialWriteGuardAcquire(t.Context(), core.HarnessKindClaude, filepath.Join(directory, ".credentials.json"))
	if err != nil {
		t.Fatal(err)
	}
	type renewal struct {
		outcome GrantRefresh
		err     error
	}
	follower := make(chan renewal, 1)
	go func() {
		outcome, err := renewKeychainLogin(t.Context(), claudeKeychainSource(directory, mac), mac, nil)
		follower <- renewal{outcome, err}
	}()
	select {
	case <-follower:
		t.Fatal("the lock is held")
	case <-time.After(150 * time.Millisecond):
	}
	// The holder finishes its renewal, then lets go.
	mac.mu.Lock()
	mac.login = mac.renewed
	mac.mu.Unlock()
	held.Release()

	result := <-follower
	if result.err != nil {
		t.Fatal(result.err)
	}
	if renewed, ok := result.outcome.(GrantRefreshRenewed); !ok || renewed.ByThisCall {
		t.Fatalf("%#v", result.outcome)
	}
	if runs := mac.runs(); len(runs) != 0 {
		t.Fatalf("%d runs", len(runs))
	}
}

// A delayed refusal must adopt the login another daemon renewed under the lock.
func TestARefusedUnexpiredKeychainLoginIsRereadUnderTheNativeLock(t *testing.T) {
	directory := t.TempDir()
	mac := newClaudeFakeMac(t, directory, 0, 7*claudeOneHourMs)
	login, err := jsonvalue.Marshal(mac.login)
	if err != nil {
		t.Fatal(err)
	}
	refused, ok := core.ClaudeAccessTokenFingerprint(login)
	if !ok {
		t.Fatal("the login has an access token")
	}
	held, err := core.CredentialWriteGuardAcquire(t.Context(), core.HarnessKindClaude, filepath.Join(directory, ".credentials.json"))
	if err != nil {
		t.Fatal(err)
	}
	source := claudeKeychainSource(directory, mac)
	type renewal struct {
		outcome GrantRefresh
		err     error
	}
	follower := make(chan renewal, 1)
	go func() {
		outcome, err := renewKeychainLogin(t.Context(), source, mac, &refused)
		follower <- renewal{outcome, err}
	}()
	select {
	case <-follower:
		t.Fatal("even an unexpired refusal must take the native lock")
	case <-time.After(150 * time.Millisecond):
	}
	mac.mu.Lock()
	mac.login = mac.renewed
	mac.mu.Unlock()
	held.Release()
	result := <-follower
	if result.err != nil {
		t.Fatal(result.err)
	}
	if renewed, ok := result.outcome.(GrantRefreshRenewed); !ok || renewed.ByThisCall {
		t.Fatalf("%#v", result.outcome)
	}
	if runs := mac.runs(); len(runs) != 0 {
		t.Fatalf("%d runs", len(runs))
	}
}

// TestALockedKeychainStartsNoRenewal: #1491: a Keychain that cannot be read
// (locked) is not a missing login. The exchange spends the single-use
// refresh token, so none may start while the result could not be read back;
// the failure is reported.
func TestALockedKeychainStartsNoRenewal(t *testing.T) {
	directory := t.TempDir()
	mac := newClaudeFakeMac(t, directory, 36, claudeOneHourMs)

	_, err := renewKeychainLogin(t.Context(), claudeKeychainSource(directory, mac), mac, nil)

	if refresh, ok := err.(ClaudeUsageErrorRefresh); !ok || !strings.Contains(refresh.Value, "Keychain") {
		t.Fatalf("%v", err)
	}
	if runs := mac.runs(); len(runs) != 0 {
		t.Fatalf("%d runs", len(runs))
	}
	// And no login in the Keychain is simply nothing to renew.
	absent := newClaudeFakeMac(t, directory, 44, claudeOneHourMs)
	outcome, err := renewKeychainLogin(t.Context(), claudeKeychainSource(directory, absent), absent, nil)
	if err != nil {
		t.Fatal(err)
	}
	if outcome != (GrantRefreshNotRefreshable{}) {
		t.Fatalf("%#v", outcome)
	}
}
