//go:build unix

// Port of the tests and the test-only helper in mj-accounts/src/codex_usage.rs
// (Rust: `#[cfg(all(test, unix))]`).

package accounts

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// installFakeManagedCodex installs script as the managed Codex CLI under
// cacheRoot, with its lease and manifest, and returns the installation
// directory. The quota tests share it.
func installFakeManagedCodex(cacheRoot, script string) string {
	install := core.ManagedHarnessInstallDir(cacheRoot, core.HarnessKindCodex)
	launcher := filepath.Join(install, filepath.FromSlash(core.CodexCLIEntrypoint))
	launcherDirectory := filepath.Dir(launcher)
	if err := os.MkdirAll(launcherDirectory, 0o755); err != nil {
		panic("create managed install: " + err.Error())
	}
	core.InstallFakeCommand(launcherDirectory, "codex", script)
	if err := os.WriteFile(filepath.Join(install, core.LeaseFile), nil, 0o644); err != nil {
		panic("create managed install lease: " + err.Error())
	}
	manifest, err := json.Marshal(core.ManagedHarnessManifestForHarness(core.HarnessKindCodex))
	if err != nil {
		panic("serialize managed install manifest: " + err.Error())
	}
	if err := os.WriteFile(filepath.Join(install, core.ManifestFile), manifest, 0o644); err != nil {
		panic("write managed install manifest: " + err.Error())
	}
	return install
}

func codexTestCacheRoot(temp string) string {
	return filepath.Join(temp, "cache")
}

func fakeCodexEnv(temp, script string) (map[string]string, string) {
	installFakeManagedCodex(codexTestCacheRoot(temp), script)
	core.InstallFakeCommand(temp, "codex", script)
	log := filepath.Join(temp, "requests.jsonl")
	return map[string]string{"PATH": temp, "CODEX_USAGE_TEST_LOG": log}, log
}

func codexRefreshInTemp(client **CodexUsageClient, cwd string, env map[string]string) CodexUsageStatus {
	var prepared *quotaProgram
	return refreshWithCacheRoot(context.Background(), client, cwd, env, CodexLauncherHost, &prepared)
}

func codexRefreshLoginInTemp(client **CodexUsageClient, cwd string, env map[string]string) *LoginRefreshFailure {
	var prepared *quotaProgram
	return refreshLoginWithCacheRoot(context.Background(), client, cwd, env, CodexLauncherHost, &prepared)
}

func codexShutdown(t *testing.T, client **CodexUsageClient) {
	t.Helper()
	if *client == nil {
		t.Fatal("client")
	}
	(*client).Shutdown(context.Background())
	*client = nil
}

func codexUsageRequestLog(t *testing.T, log string) []any {
	t.Helper()
	requests, err := os.ReadFile(log)
	if err != nil {
		t.Fatalf("request log: %v", err)
	}
	messages := []any{}
	for _, line := range strings.Split(strings.TrimRight(string(requests), "\n"), "\n") {
		message, err := jsonvalue.Parse([]byte(line))
		if err != nil {
			t.Fatalf("request json: %v", err)
		}
		messages = append(messages, message)
	}
	return messages
}

func codexField(message any, path ...string) any {
	value, _ := jvPointer(message, path...)
	return value
}

func TestTheQuotaProbeStartsCodexWithoutAnAPIKey(t *testing.T) {
	temp := t.TempDir()
	seen := filepath.Join(temp, "seen")
	env, _ := fakeCodexEnv(temp, fmt.Sprintf(
		"#!/bin/sh\nprintf '%%s|%%s|%%s|%%s|%%s\\n' \"${OPENAI_API_KEY-unset}\" \"${CODEX_API_KEY-unset}\" \"${CODEX_ACCESS_TOKEN-unset}\" \"${OPENAI_BASE_URL-unset}\" \"${RUST_LOG-unset}\" > %s\nexit 1\n",
		core.PosixQuote(seen)))
	for _, name := range core.CodexCredentialEnvironment {
		env[name] = "sk-svcacct-test"
	}
	// Trace level would log request bodies that carry the refresh token.
	env["RUST_LOG"] = "trace"
	var codex *CodexUsageClient

	codexRefreshInTemp(&codex, temp, env)

	got, err := os.ReadFile(seen)
	if err != nil {
		t.Fatalf("the fake codex ran: %v", err)
	}
	if string(got) != "unset|unset|unset|unset|error\n" {
		t.Fatalf("%q", got)
	}
}

func TestRefreshUsesOneInitializedClientForRepeatedQueries(t *testing.T) {
	temp := t.TempDir()
	env, log := fakeCodexEnv(temp, `#!/bin/sh
read_and_log() {
    IFS= read -r line || exit 1
    printf '%s\n' "$line" >> "$CODEX_USAGE_TEST_LOG"
}
read_and_log
printf '%s\n' '{"method":"account/rateLimits/updated"}' '{"id":1,"result":{}}'
read_and_log
read_and_log
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt","planType":"plus"}}}'
read_and_log
printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300},"credits":{"hasCredits":true,"unlimited":false,"balance":"50.25"}}}}'
read_and_log
printf '%s\n' '{"id":4,"result":{"account":{"type":"chatgpt","planType":"plus"}}}'
read_and_log
printf '%s\n' '{"id":5,"result":{"rateLimitsByLimitId":{"codex":{"planType":"promax","primary":{"usedPercent":50,"windowDurationMins":300},"credits":{"hasCredits":false,"unlimited":false,"balance":"50.25"}}}}}'
read_and_log
printf '%s\n' '{"id":6,"result":{"account":{"type":"chatgpt","planType":"plus"}}}'
read_and_log
printf '%s\n' '{"id":7,"result":{"rateLimits":{"planType":"business","primary":{"usedPercent":50,"windowDurationMins":300}}}}'
read_and_log
printf '%s\n' '{"id":8,"result":{"account":{"type":"chatgpt","planType":"prolite"}}}'
read_and_log
printf '%s\n' '{"id":9,"result":{"rateLimits":{"primary":{"usedPercent":50,"windowDurationMins":300}}}}'
`)
	var codex *CodexUsageClient

	first := codexRefreshInTemp(&codex, temp, env)
	second := codexRefreshInTemp(&codex, temp, env)
	third := codexRefreshInTemp(&codex, temp, env)
	fourth := codexRefreshInTemp(&codex, temp, env)

	float := func(value float64) *float64 { return &value }
	price := func(value uint32) *uint32 { return &value }
	for index, expected := range []struct {
		status  CodexUsageStatus
		balance *float64
		price   *uint32
	}{
		{first, float(50.25), price(20)},
		{second, float(0.0), price(500)},
		{third, nil, nil},
		{fourth, nil, price(100)},
	} {
		available, ok := expected.status.(CodexUsageStatusAvailable)
		if !ok {
			t.Fatalf("poll %d: missing quota report: %#v", index+1, expected.status)
		}
		report := available.Report
		if (report.MonthlyPriceUsd == nil) != (expected.price == nil) ||
			(report.MonthlyPriceUsd != nil && *report.MonthlyPriceUsd != *expected.price) {
			t.Fatalf("poll %d: price %v", index+1, report.MonthlyPriceUsd)
		}
		var wanted *client.PaidCredits
		if expected.balance != nil {
			wanted = client.NewPaidCreditsLimited(*expected.balance)
		}
		if (report.PaidCredits == nil) != (wanted == nil) ||
			(wanted != nil && (report.PaidCredits.Kind != wanted.Kind || !jsonvalue.Equal(report.PaidCredits.Balance, wanted.Balance))) {
			t.Fatalf("poll %d: credits %#v, want %#v", index+1, report.PaidCredits, wanted)
		}
	}

	for index, expected := range []struct {
		status    CodexUsageStatus
		remaining uint8
	}{{first, 75}, {second, 50}} {
		report := expected.status.(CodexUsageStatusAvailable).Report
		if report.PaidCredits == nil || report.BankedResets != nil || report.Primary == nil ||
			report.Primary.RemainingPercent != expected.remaining {
			t.Fatalf("poll %d: %#v", index+1, report)
		}
	}

	messages := codexUsageRequestLog(t, log)
	if len(messages) != 10 {
		t.Fatalf("%d requests", len(messages))
	}
	for _, check := range []struct {
		message int
		field   string
		want    any
	}{
		{0, "method", "initialize"},
		{0, "id", int64(1)},
		{1, "method", "initialized"},
		{2, "method", "account/read"},
		{3, "method", "account/rateLimits/read"},
		{4, "id", int64(4)},
		{5, "id", int64(5)},
	} {
		if got := codexField(messages[check.message], check.field); !jsonvalue.Equal(got, check.want) {
			t.Fatalf("message %d %s = %v", check.message, check.field, got)
		}
	}

	codexShutdown(t, &codex)
}

func TestRefreshLoginAsksAppServerForAProactiveTokenRefresh(t *testing.T) {
	temp := t.TempDir()
	env, log := fakeCodexEnv(temp, `#!/bin/sh
read_and_log() {
    IFS= read -r line || exit 1
    printf '%s\n' "$line" >> "$CODEX_USAGE_TEST_LOG"
}
read_and_log
printf '%s\n' '{"id":1,"result":{}}'
read_and_log
read_and_log
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}'
`)
	var codex *CodexUsageClient

	if failure := codexRefreshLoginInTemp(&codex, temp, env); failure != nil {
		t.Fatalf("proactive refresh: %v", failure)
	}

	messages := codexUsageRequestLog(t, log)
	if len(messages) != 3 {
		t.Fatalf("%d requests", len(messages))
	}
	for _, check := range []struct {
		message int
		path    []string
		want    any
	}{
		{0, []string{"method"}, "initialize"},
		{1, []string{"method"}, "initialized"},
		{2, []string{"method"}, "account/read"},
		{2, []string{"params", "refreshToken"}, true},
	} {
		if got := codexField(messages[check.message], check.path...); !jsonvalue.Equal(got, check.want) {
			t.Fatalf("message %d %v = %v", check.message, check.path, got)
		}
	}

	codexShutdown(t, &codex)
}

func TestRefreshLoginKeepsTheClientWhenTheRefreshFlagIsUnsupported(t *testing.T) {
	temp := t.TempDir()
	env, _ := fakeCodexEnv(temp, `#!/bin/sh
read_and_log() {
    IFS= read -r line || exit 1
    printf '%s\n' "$line" >> "$CODEX_USAGE_TEST_LOG"
}
read_and_log
printf '%s\n' '{"id":1,"result":{}}'
read_and_log
read_and_log
printf '%s\n' '{"id":2,"error":{"code":-32601,"message":"unknown parameter"}}'
read_and_log
printf '%s\n' '{"id":3,"result":{"account":{"type":"chatgpt"}}}'
read_and_log
printf '%s\n' '{"id":4,"result":{"rateLimits":{"primary":{"usedPercent":10,"windowDurationMins":300}}}}'
`)
	var codex *CodexUsageClient

	if failure := codexRefreshLoginInTemp(&codex, temp, env); failure != nil {
		t.Fatalf("an app-server without the flag has nothing to refresh: %v", failure)
	}
	if codex == nil {
		t.Fatal("the client stays usable for the poll")
	}

	status := codexRefreshInTemp(&codex, temp, env)

	available, ok := status.(CodexUsageStatusAvailable)
	if !ok || available.Report.PaidCredits != nil || available.Report.BankedResets != nil ||
		available.Report.Primary == nil || available.Report.Primary.RemainingPercent != 90 {
		t.Fatalf("%#v", status)
	}

	codexShutdown(t, &codex)
}

func TestRefreshDiscardsClientWhenAppServerIsUnsupported(t *testing.T) {
	temp := t.TempDir()
	env, _ := fakeCodexEnv(temp, `#!/bin/sh
IFS= read -r line || exit 1
printf '%s\n' '{"id":1,"error":{"code":-32601,"message":"unknown method"}}'
`)
	var codex *CodexUsageClient

	status := codexRefreshInTemp(&codex, temp, env)

	if status != (CodexUsageStatusUnavailable{Reason: "installed Codex does not support quota queries"}) {
		t.Fatalf("%#v", status)
	}
	if codex != nil {
		t.Fatal("the unsupported client was kept")
	}
}

// codexSignedOutUntilRestart is a fake app-server that records each start
// and answers `account/read` with no account on its first start, as one
// whose token refresh failed does, and with a usable login on every later
// start.
const codexSignedOutUntilRestart = `#!/bin/sh
starts="$CODEX_USAGE_TEST_LOG.starts"
started_before=no
[ -e "$starts" ] && started_before=yes
printf 'start\n' >> "$starts"
IFS= read -r line || exit 1
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line || exit 1
IFS= read -r line || exit 1
if [ "$started_before" = no ]; then
    printf '%s\n' '{"id":2,"result":{"account":null}}'
else
    printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}'
    IFS= read -r line || exit 1
    printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":29,"windowDurationMins":10080}}}}'
fi
IFS= read -r line
`

func codexUsageStarts(log string) int {
	text, err := os.ReadFile(log + ".starts")
	if err != nil {
		return 0
	}
	return strings.Count(string(text), "\n")
}

// App-server answers a refresh whose token exchange failed with no account
// rather than an error (#1224).
func TestRefreshLoginReportsAMissingAccountAsAFailedRefresh(t *testing.T) {
	temp := t.TempDir()
	env, log := fakeCodexEnv(temp, codexSignedOutUntilRestart)
	var codex *CodexUsageClient

	failure := codexRefreshLoginInTemp(&codex, temp, env)
	if failure == nil {
		t.Fatal("no account means the refresh failed")
	}

	if failure.Reason != "not signed in with ChatGPT" {
		t.Fatalf("%q", failure.Reason)
	}
	if codex != nil {
		t.Fatal("the signed-out app-server is replaced")
	}
	if starts := codexUsageStarts(log); starts != 1 {
		t.Fatalf("%d starts", starts)
	}
}

// An app-server that has recorded an authentication failure keeps reporting
// it until it restarts, even once the credential file works again (#1224).
// The next poll starts a new one.
func TestRefreshRecoversAfterACachedAuthenticationFailure(t *testing.T) {
	temp := t.TempDir()
	env, log := fakeCodexEnv(temp, codexSignedOutUntilRestart)
	var codex *CodexUsageClient

	first := codexRefreshInTemp(&codex, temp, env)
	if first != (CodexUsageStatusUnavailable{Reason: "not signed in with ChatGPT"}) {
		t.Fatalf("%#v", first)
	}
	if codex != nil {
		t.Fatal("the signed-out client was kept")
	}

	second := codexRefreshInTemp(&codex, temp, env)
	available, ok := second.(CodexUsageStatusAvailable)
	if !ok || available.Report.Primary == nil || available.Report.Primary.RemainingPercent != 71 {
		t.Fatalf("%#v", second)
	}
	if starts := codexUsageStarts(log); starts != 2 {
		t.Fatalf("%d starts", starts)
	}

	codexShutdown(t, &codex)
}

// A login that is invalid in the file too costs one app-server start per
// call, never a retry inside the call.
func TestAnInvalidLoginStartsOneAppServerPerCall(t *testing.T) {
	temp := t.TempDir()
	env, log := fakeCodexEnv(temp, `#!/bin/sh
printf 'start\n' >> "$CODEX_USAGE_TEST_LOG.starts"
IFS= read -r line || exit 1
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line || exit 1
IFS= read -r line || exit 1
printf '%s\n' '{"id":2,"result":{"account":null}}'
IFS= read -r line
`)
	var codex *CodexUsageClient

	for poll := 1; poll <= 3; poll++ {
		status := codexRefreshInTemp(&codex, temp, env)
		if status != (CodexUsageStatusUnavailable{Reason: "not signed in with ChatGPT"}) {
			t.Fatalf("poll %d: %#v", poll, status)
		}
		if codex != nil {
			t.Fatalf("poll %d: the client was kept", poll)
		}
		if starts := codexUsageStarts(log); starts != poll {
			t.Fatalf("poll %d: %d starts", poll, starts)
		}
	}
}

// The tail is read only after app-server has stopped and its stderr has
// drained, so a line written just before the failed answer is kept.
func TestTheClientKeepsARedactedTailOfAppServerStderr(t *testing.T) {
	temp := t.TempDir()
	env, _ := fakeCodexEnv(temp, `#!/bin/sh
i=0
while [ "$i" -lt 25 ]; do
    printf 'noise %s\n' "$i" >&2
    i=$((i + 1))
done
IFS= read -r line || exit 1
printf '\033[31mERROR\033[0m failed to refresh token: refresh_token=rt_abcdefghijklmnopqrstuvwxyz0123 reused\n' >&2
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line
`)
	ctx := context.Background()
	var codex *CodexUsageClient
	var prepared *quotaProgram
	launcher := codexLauncherProgram(filepath.Join(
		core.ManagedHarnessInstallDir(codexTestCacheRoot(temp), core.HarnessKindCodex),
		filepath.FromSlash(core.CodexCLIEntrypoint)))
	if err := codexUsageStart(ctx, &codex, temp, env, launcher, &prepared); err != nil {
		t.Fatalf("started client: %v", err)
	}
	if _, err := codexUsagePrepare(ctx, &codex); err != nil {
		t.Fatalf("initialized client: %v", err)
	}

	stopped := codex
	codex = nil
	lines := stopped.shutdownWithDiagnostics(ctx)

	if len(lines) != codexStderrTailLines {
		t.Fatalf("%d lines: %q", len(lines), lines)
	}
	if lines[0] != "noise 6" {
		t.Fatalf("%q", lines[0])
	}
	if last := lines[len(lines)-1]; last != "ERROR failed to refresh token: refresh_token=[redacted] reused" {
		t.Fatalf("%q", last)
	}
}
