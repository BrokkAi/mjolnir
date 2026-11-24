//go:build unix

// Port of the Unix-only tests in mj-accounts/src/quota/tests.rs.

package accounts

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"net"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// managedCodexCacheRoot is where a test's managed-install `codex
// app-server` stand-in lives.
func managedCodexCacheRoot(directory string) string {
	return filepath.Join(directory, "managed-cache")
}

// fakeCodexAppServer installs a managed-install `codex app-server` stand-in
// that logs each request line, so a test can assert the exact protocol
// exchange.
func fakeCodexAppServer(directory, script string) (map[string]string, string) {
	installFakeManagedCodex(managedCodexCacheRoot(directory), script)
	log := filepath.Join(directory, "requests.jsonl")
	environment := map[string]string{
		"PATH":                 directory,
		"CODEX_USAGE_TEST_LOG": log,
		"CODEX_AUTH_FILE":      filepath.Join(directory, "auth.json"),
	}
	return environment, log
}

// writeCodexAuth writes a Codex `auth.json` whose access token is a JWT
// expiring expiresIn from now, last refreshed refreshedAgo before now.
func writeCodexAuth(t *testing.T, home string, expiresIn, refreshedAgo time.Duration) {
	t.Helper()
	now := time.Now().UTC()
	expiry := now.Add(expiresIn).Unix()
	segment := func(value any) string {
		encoded, err := jsonvalue.Marshal(value)
		if err != nil {
			t.Fatal(err)
		}
		return base64.RawURLEncoding.EncodeToString(encoded)
	}
	accessToken := segment(jsonvalue.Object{{Key: "alg", Value: "RS256"}, {Key: "typ", Value: "JWT"}}) + "." +
		segment(jsonvalue.Object{{Key: "exp", Value: expiry}}) + ".signature-is-never-checked"
	body, err := jsonvalue.Marshal(jsonvalue.Object{
		{Key: "auth_mode", Value: "chatgpt"},
		{Key: "tokens", Value: jsonvalue.Object{
			{Key: "access_token", Value: accessToken},
			{Key: "refresh_token", Value: "refresh"},
			{Key: "id_token", Value: "id"},
			{Key: "account_id", Value: "account"},
		}},
		{Key: "last_refresh", Value: core.FormatChronoRFC3339(now.Add(-refreshedAgo))},
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, "auth.json"), body, 0o666); err != nil {
		t.Fatal(err)
	}
}

func codexRequestLog(t *testing.T, log string) []any {
	t.Helper()
	text, err := os.ReadFile(log)
	if err != nil {
		t.Fatal(err)
	}
	var messages []any
	for _, line := range claudeLines(string(text)) {
		message, err := jsonvalue.Parse([]byte(line))
		if err != nil {
			t.Fatalf("%q: %v", line, err)
		}
		messages = append(messages, message)
	}
	return messages
}

func codexQuotaRequest(directory string, environment map[string]string) QuotaRefreshRequest {
	return QuotaRefreshRequest{
		ProfileID:   "codex",
		Harness:     core.HarnessKindCodex,
		AuthScheme:  core.AuthSchemeNativeLogin{},
		SourceHome:  directory,
		Environment: environment,
		Cwd:         directory,
		CodexSource: core.CodexQuotaSourceChatGptLogin{},
	}
}

// codexStarts is how many times the fake app-server has started. Each start
// appends a line to `<request log>.starts`.
func codexStarts(log string) int {
	text, err := os.ReadFile(log + ".starts")
	if err != nil {
		return 0
	}
	return len(claudeLines(string(text)))
}

func pollCodexProfile(ctx context.Context, directory string, environment map[string]string) QuotaRefreshOutcome {
	root := managedCodexCacheRoot(directory)
	outcome, codexClient := refreshProfileWithCacheRoot(ctx, codexQuotaRequest(directory, environment), nil, &root)
	if codexClient != nil {
		codexClient.Shutdown(ctx)
	}
	return outcome
}

// codexMessage reads a pointer out of one logged request.
func codexMessage(message any, pointer string) any {
	value, _ := quotaJSONPointer(message, pointer)
	return value
}

// Hard-won: c94c68c0: host and container could spend the same Codex refresh token and kill a live turn
func TestACodexLoginNearExpiryIsRotatedBeforeTheUsageQuery(t *testing.T) {
	directory := t.TempDir()
	// Ten minutes left on a one-hour token: inside the one-hour margin.
	writeCodexAuth(t, directory, 600*time.Second, 3_000*time.Second)
	environment, log := fakeCodexAppServer(directory, `#!/bin/sh
read_and_log() {
IFS= read -r line || exit 1
printf '%s\n' "$line" >> "$CODEX_USAGE_TEST_LOG"
}
read_and_log
printf '%s\n' '{"id":1,"result":{}}'
read_and_log
read_and_log
printf '%s\n' '{"auth_mode":"chatgpt","tokens":{"access_token":"rotated"}}' > "$CODEX_AUTH_FILE"
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}'
read_and_log
printf '%s\n' '{"id":3,"result":{"account":{"type":"chatgpt"}}}'
read_and_log
printf '%s\n' '{"id":4,"result":{"rateLimitResetCredits":{"availableCount":1},"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300},"credits":{"hasCredits":true,"unlimited":false,"balance":"12.75"}}}}'
`)

	outcome := pollCodexProfile(t.Context(), directory, environment)

	if outcome.Report.Error != nil {
		t.Fatalf("error = %q", *outcome.Report.Error)
	}
	if outcome.Report.BankedResets == nil || *outcome.Report.BankedResets != 1 {
		t.Fatalf("banked resets = %v", outcome.Report.BankedResets)
	}
	if want := client.NewPaidCreditsLimited(12.75); outcome.Report.PaidCredits == nil || !jsonvalue.Equal(outcome.Report.PaidCredits.Balance, want.Balance) || outcome.Report.PaidCredits.Kind != want.Kind {
		t.Fatalf("paid credits = %#v", outcome.Report.PaidCredits)
	}
	if window := outcome.Report.FiveHourWindow(); window == nil || *window.RemainingPercent != 75 {
		t.Fatalf("five hour = %#v", window)
	}
	// The rotated file has to reach live sessions, which is what the
	// changed-credentials flag asks the daemon to do.
	if !outcome.CredentialsChanged {
		t.Fatal("the rotation is a credential change")
	}

	messages := codexRequestLog(t, log)
	if len(messages) != 5 {
		t.Fatalf("%d messages", len(messages))
	}
	for index, want := range []struct {
		pointer string
		value   any
	}{
		{"/method", "initialize"}, {"/method", "initialized"}, {"/method", "account/read"},
		{"/params/refreshToken", true}, {"/method", "account/read"}, {"/params/refreshToken", false},
		{"/method", "account/rateLimits/read"},
	} {
		message := messages[[]int{0, 1, 2, 2, 3, 3, 4}[index]]
		if got := codexMessage(message, want.pointer); got != want.value {
			t.Fatalf("message %d %s = %v", index, want.pointer, got)
		}
	}
}

// Hard-won: #1485: separate daemons could concurrently spend one Codex refresh token
func TestCodexQuotaProcessesShareRefreshOwnershipAndRecheckExpiry(t *testing.T) {
	const childHome = "MJ_CODEX_QUOTA_TRANSACTION_HOME"
	const childEnvironment = "MJ_CODEX_QUOTA_TRANSACTION_ENV"
	if home, ok := os.LookupEnv(childHome); ok {
		var environment map[string]string
		if err := json.Unmarshal([]byte(os.Getenv(childEnvironment)), &environment); err != nil {
			t.Fatal(err)
		}
		outcome := pollCodexProfile(t.Context(), home, environment)
		if outcome.Report.Error != nil {
			t.Fatalf("error = %q", *outcome.Report.Error)
		}
		return
	}

	// A short directory: the control socket's path must fit sun_path.
	directory, err := os.MkdirTemp("", "codex-quota")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(directory) })
	unrelated := t.TempDir()
	writeCodexAuth(t, directory, 600*time.Second, 3_000*time.Second)
	writeCodexAuth(t, unrelated, 10*3_600*time.Second, 3_600*time.Second)
	// The first app-server stops inside its refresh RPC. The socket gives
	// the parent ownership of the interleaving; no live provider is
	// contacted.
	socketPath := filepath.Join(directory, "refresh.sock")
	listener, err := net.Listen("unix", socketPath)
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	script := `#!/usr/bin/python3
import json, os, socket, sys
for line in sys.stdin:
    message = json.loads(line)
    with open(os.environ['CODEX_USAGE_TEST_LOG'], 'a') as log:
        log.write(line)
    method = message.get('method')
    if method == 'initialized':
        continue
    if method == 'account/read':
        if message['params']['refreshToken']:
            with socket.socket(socket.AF_UNIX) as control:
                control.connect(os.environ['CODEX_REFRESH_SOCKET'])
                control.sendall(b'R')
                assert control.recv(1) == b'G'
            with open(os.environ['CODEX_FRESH_AUTH']) as source:
                fresh = source.read()
            with open(os.environ['CODEX_AUTH_FILE'], 'w') as target:
                target.write(fresh)
        result = {'account': {'type': 'chatgpt'}}
    elif method == 'account/rateLimits/read':
        result = {'rateLimits': {'primary': {'usedPercent': 25, 'windowDurationMins': 300}}}
    else:
        result = {}
    print(json.dumps({'id': message['id'], 'result': result}), flush=True)
`
	// Fake commands are read by /bin/sh, so invoke Python explicitly rather
	// than relying on the source file's shebang.
	pythonScript := filepath.Join(directory, "quota-server.py")
	if err := os.WriteFile(pythonScript, []byte(script), 0o666); err != nil {
		t.Fatal(err)
	}
	launcher := "exec /usr/bin/python3 " + core.PosixQuote(pythonScript) + " \"$@\"\n"
	environment, firstLog := fakeCodexAppServer(directory, launcher)
	environment["CODEX_REFRESH_SOCKET"] = socketPath
	environment["CODEX_FRESH_AUTH"] = filepath.Join(unrelated, "auth.json")
	encoded, err := json.Marshal(environment)
	if err != nil {
		t.Fatal(err)
	}
	isolated := targets.NewIsolatedTest(targets.TestName("quota::tests", "TestCodexQuotaProcessesShareRefreshOwnershipAndRecheckExpiry")).
		Env(childHome, directory).
		Env(childEnvironment, string(encoded)).
		IsolatedStore(filepath.Join(directory, "child-instance"))
	childDone := make(chan core.ProcessOutput, 1)
	go func() { childDone <- isolated.Output() }()
	accepted := make(chan net.Conn, 1)
	go func() {
		connection, err := listener.Accept()
		if err == nil {
			accepted <- connection
		}
	}()
	var control net.Conn
	select {
	case control = <-accepted:
	case output := <-childDone:
		t.Fatalf("first quota process exited before its refresh: %s\n%s", output.Stdout, output.Stderr)
	case <-time.After(20 * time.Second):
		t.Fatal("first process entered its refresh")
	}
	defer control.Close()
	signal := make([]byte, 1)
	if _, err := control.Read(signal); err != nil || signal[0] != 'R' {
		t.Fatalf("signal = %q, %v", signal, err)
	}

	secondLog := filepath.Join(directory, "second.jsonl")
	environment["CODEX_USAGE_TEST_LOG"] = secondLog
	second := make(chan QuotaRefreshOutcome, 1)
	go func() { second <- pollCodexProfile(t.Context(), directory, environment) }()
	// Poll the contender while the first process is paused before writing.
	select {
	case <-second:
		t.Fatal("the contender finished while the first process held the login")
	case <-time.After(250 * time.Millisecond):
	}
	if _, err := os.Stat(secondLog); err == nil {
		t.Fatal("waiting must precede app-server RPCs")
	}

	unrelatedEnvironment, _ := fakeCodexAppServer(unrelated, launcher)
	independent := make(chan QuotaRefreshOutcome, 1)
	go func() { independent <- pollCodexProfile(t.Context(), unrelated, unrelatedEnvironment) }()
	select {
	case outcome := <-independent:
		if outcome.Report.Error != nil {
			t.Fatalf("independent error = %q", *outcome.Report.Error)
		}
	case <-time.After(20 * time.Second):
		t.Fatal("a different credential home must not wait")
	}

	if _, err := control.Write([]byte("G")); err != nil {
		t.Fatal(err)
	}
	select {
	case outcome := <-second:
		if outcome.Report.Error != nil {
			t.Fatalf("second error = %q", *outcome.Report.Error)
		}
	case <-time.After(20 * time.Second):
		t.Fatal("the waiting probe resumes after the first transaction")
	}
	output := <-childDone
	if output.Status == nil || !output.Status.Success() {
		t.Fatalf("first quota process failed: %s\n%s", output.Stdout, output.Stderr)
	}
	rotations := func(log string) int {
		count := 0
		for _, message := range codexRequestLog(t, log) {
			if codexMessage(message, "/params/refreshToken") == true {
				count++
			}
		}
		return count
	}
	if count := rotations(firstLog); count != 1 {
		t.Fatalf("first rotations = %d", count)
	}
	if count := rotations(secondLog); count != 0 {
		t.Fatalf("expiry must be reread after waiting: %d rotations", count)
	}
}

func TestACodexLoginFarFromExpiryIsPolledWithoutARotation(t *testing.T) {
	directory := t.TempDir()
	// Ten hours left on an eleven-hour token: outside both margins.
	writeCodexAuth(t, directory, 10*3_600*time.Second, 3_600*time.Second)
	environment, log := fakeCodexAppServer(directory, `#!/bin/sh
read_and_log() {
IFS= read -r line || exit 1
printf '%s\n' "$line" >> "$CODEX_USAGE_TEST_LOG"
}
read_and_log
printf '%s\n' '{"id":1,"result":{}}'
read_and_log
read_and_log
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}'
read_and_log
printf '%s\n' '{"id":3,"result":{"rateLimits":{"planType":"pro","primary":{"usedPercent":25,"windowDurationMins":300}}}}'
`)

	outcome := pollCodexProfile(t.Context(), directory, environment)

	if outcome.Report.Error != nil {
		t.Fatalf("error = %q", *outcome.Report.Error)
	}
	if outcome.Report.MonthlyPriceUsd == nil || *outcome.Report.MonthlyPriceUsd != 200 {
		t.Fatalf("price = %v", outcome.Report.MonthlyPriceUsd)
	}
	if len(outcome.Report.Windows) != 1 {
		t.Fatalf("expected one quota window: %#v", outcome.Report.Windows)
	}
	if window := outcome.Report.Windows[0]; window.Used != nil || window.Limit != nil {
		t.Fatalf("window = %#v", window)
	}
	if outcome.CredentialsChanged {
		t.Fatal("credentials changed")
	}

	messages := codexRequestLog(t, log)
	if len(messages) != 4 {
		t.Fatalf("%d messages", len(messages))
	}
	if codexMessage(messages[0], "/method") != "initialize" || codexMessage(messages[1], "/method") != "initialized" ||
		codexMessage(messages[2], "/params/refreshToken") != false || codexMessage(messages[3], "/method") != "account/rateLimits/read" {
		t.Fatalf("messages = %v", messages)
	}
}

func TestACodexAppServerWithoutTheRefreshFlagStillReportsQuota(t *testing.T) {
	directory := t.TempDir()
	writeCodexAuth(t, directory, 600*time.Second, 3_000*time.Second)
	environment, _ := fakeCodexAppServer(directory, `#!/bin/sh
IFS= read -r line || exit 1
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line || exit 1
IFS= read -r line || exit 1
printf '%s\n' '{"id":2,"error":{"code":-32601,"message":"unknown parameter"}}'
IFS= read -r line || exit 1
printf '%s\n' '{"id":3,"result":{"account":{"type":"chatgpt"}}}'
IFS= read -r line || exit 1
printf '%s\n' '{"id":4,"result":{"rateLimits":{"primary":{"usedPercent":40,"windowDurationMins":300}}}}'
`)

	outcome := pollCodexProfile(t.Context(), directory, environment)

	if outcome.Report.Error != nil {
		t.Fatalf("error = %q", *outcome.Report.Error)
	}
	if window := outcome.Report.FiveHourWindow(); window == nil || *window.RemainingPercent != 60 {
		t.Fatalf("five hour = %#v", window)
	}
}

// Hard-won: 7f4649508: re-preparing Codex rejected a healthy cached client after its CLI disappeared.
func TestCachedCodexKeepsPollingAfterItsLauncherDisappears(t *testing.T) {
	directory := t.TempDir()
	writeCodexAuth(t, directory, 36_000*time.Second, 3_600*time.Second)
	script := `#!/bin/sh
id=0
while IFS= read -r line; do
    case "$line" in
        *'"method":"initialized"'*) continue ;;
    esac
    id=$((id + 1))
    case "$line" in
        *'"method":"account/read"'*) result='{"account":{"type":"chatgpt"}}' ;;
        *'"method":"account/rateLimits/read"'*) result='{"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300}}}' ;;
        *) result='{}' ;;
    esac
    printf '{"id":%s,"result":%s}\n' "$id" "$result"
done
`
	core.InstallFakeCommand(directory, "codex", script)
	request := codexQuotaRequest(directory, map[string]string{
		"PATH": directory,
		// If lookup is retried after the executable disappears, preparation
		// fails deterministically without touching the host cache or
		// network.
		"XDG_CACHE_HOME": "relative-cache-is-unusable",
	})
	first, codexClient := refreshProfileWithCacheRoot(t.Context(), request, nil, nil)
	if first.Report.Error != nil {
		t.Fatalf("error = %q", *first.Report.Error)
	}
	if codexClient == nil {
		t.Fatal("the first poll caches its app-server")
	}
	if err := os.Remove(filepath.Join(directory, "codex")); err != nil {
		t.Fatal(err)
	}
	second, codexClient := refreshProfileWithCacheRoot(t.Context(), request, codexClient, nil)
	// Stop the child even if the assertion below detects the regression.
	if codexClient == nil {
		t.Fatal("cached app-server survives")
	}
	codexClient.Shutdown(t.Context())
	if second.Report.Error != nil {
		t.Fatalf("error = %q", *second.Report.Error)
	}
	if window := second.Report.FiveHourWindow(); window == nil || *window.RemainingPercent != 75 {
		t.Fatalf("five hour = %#v", window)
	}
	// Hard-won: ce1937477: a fresh installation failure lost its actionable cause in the quota report.
	unavailable, codexClient := refreshProfileWithCacheRoot(t.Context(), request, nil, nil)
	if codexClient != nil {
		t.Fatal("no client without a launcher")
	}
	if unavailable.Report.Error == nil {
		t.Fatal("the stopped client cannot serve this poll")
	}
	failure := *unavailable.Report.Error
	if !strings.Contains(failure, "could not prepare the pinned harness") ||
		!strings.Contains(failure, "managed harness cache root must be absolute") {
		t.Fatalf("error = %q", failure)
	}
}

// Hard-won: 7f4649508: an ambient CLI's non-UTF-8 directory was replaced before process startup.
func TestInheritedNativePathReportsANonUTF8QuotaHarness(t *testing.T) {
	// macOS filesystems reject the non-UTF-8 directory name this fixture
	// needs.
	if runtime.GOOS == "darwin" {
		t.Skip("macOS filesystems reject non-UTF-8 names")
	}
	const childHome = "MJ_NATIVE_QUOTA_PATH_HOME"
	if home, ok := os.LookupEnv(childHome); ok {
		request := codexQuotaRequest(home, map[string]string{"XDG_CACHE_HOME": "relative-cache-is-unusable"})
		outcome, codexClient := refreshProfileWithCacheRoot(t.Context(), request, nil, nil)
		if codexClient != nil {
			t.Fatal("no client from a path CommandSpec cannot carry")
		}
		if outcome.Report.Error == nil {
			t.Fatal("native path cannot enter CommandSpec")
		}
		failure := *outcome.Report.Error
		if !strings.Contains(failure, "quota harness path is not UTF-8:") || !strings.Contains(failure, "bin-") {
			t.Fatalf("error = %q", failure)
		}
		return
	}
	directory := t.TempDir()
	writeCodexAuth(t, directory, 36_000*time.Second, 3_600*time.Second)
	bin := filepath.Join(directory, "bin-\xff")
	if err := os.Mkdir(bin, 0o777); err != nil {
		t.Fatal(err)
	}
	core.InstallFakeCommand(bin, "codex", `#!/bin/sh
IFS= read -r line || exit 1
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line || exit 1
IFS= read -r line || exit 1
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}'
IFS= read -r line || exit 1
printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300}}}}'
IFS= read -r line
`)
	output := targets.NewIsolatedTest(targets.TestName("quota::tests", "TestInheritedNativePathReportsANonUTF8QuotaHarness")).
		Env(childHome, directory).
		Env("PATH", bin).
		IsolatedStore(filepath.Join(directory, "child-instance")).
		Output()
	if output.Status == nil || !output.Status.Success() {
		t.Fatalf("%s\n%s", output.Stdout, output.Stderr)
	}
}

// TestACachedAppServerStopsWhenTheProfileLeavesTheChatgptLogin: a profile
// switched off the ChatGPT login stops the app-server cached for it instead
// of handing it back to be kept until the daemon exits.
func TestACachedAppServerStopsWhenTheProfileLeavesTheChatgptLogin(t *testing.T) {
	directory := t.TempDir()
	writeCodexAuth(t, directory, 10*3_600*time.Second, 3_600*time.Second)
	environment, _ := fakeCodexAppServer(directory, `#!/bin/sh
IFS= read -r line || exit 1
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line || exit 1
IFS= read -r line || exit 1
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}'
IFS= read -r line || exit 1
printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":9,"windowDurationMins":300}}}}'
IFS= read -r line
`)
	request := codexQuotaRequest(directory, environment)
	root := managedCodexCacheRoot(directory)
	first, codexClient := refreshProfileWithCacheRoot(t.Context(), request, nil, &root)
	if first.Report.Error != nil {
		t.Fatalf("error = %q", *first.Report.Error)
	}
	if codexClient == nil {
		t.Fatal("the ChatGPT poll caches its app-server")
	}

	switched := request
	switched.CodexSource = core.CodexQuotaSourceUsagePriced{Provider: "ollama"}
	second, codexClient := refreshProfileWithCacheRoot(t.Context(), switched, codexClient, &root)
	if !second.Report.IsUsagePriced() {
		t.Fatalf("report = %#v", second.Report)
	}
	if codexClient != nil {
		t.Fatal("the unused app-server is stopped")
	}
}

// TestAFailedRotationInANewAppServerEndsThePoll: a rotation that fails in an
// app-server this poll started ends the poll: starting another one for the
// quota read would repeat the failure (#1224).
func TestAFailedRotationInANewAppServerEndsThePoll(t *testing.T) {
	directory := t.TempDir()
	writeCodexAuth(t, directory, 600*time.Second, 3_000*time.Second)
	environment, log := fakeCodexAppServer(directory, `#!/bin/sh
printf 'start\n' >> "$CODEX_USAGE_TEST_LOG.starts"
read_and_log() {
IFS= read -r line || exit 1
printf '%s\n' "$line" >> "$CODEX_USAGE_TEST_LOG"
}
read_and_log
printf '%s\n' '{"id":1,"result":{}}'
read_and_log
read_and_log
printf '%s\n' '{"id":2,"result":{"account":null}}'
IFS= read -r line
`)

	outcome := pollCodexProfile(t.Context(), directory, environment)

	if outcome.Report.Error == nil || *outcome.Report.Error != "not signed in with ChatGPT" {
		t.Fatalf("error = %v", outcome.Report.Error)
	}
	if starts := codexStarts(log); starts != 1 {
		t.Fatalf("starts = %d", starts)
	}
	messages := codexRequestLog(t, log)
	if len(messages) != 3 || codexMessage(messages[2], "/params/refreshToken") != true {
		t.Fatalf("messages = %v", messages)
	}
}

// TestAFailedRotationInACachedAppServerRecoversInTheSamePoll: the #1224
// case: a cached app-server whose rotation fails stays signed out until it
// restarts. The poll replaces it and reads the quota with the new one,
// which reads the credential file again.
//
// Hard-won: 4de4844fb: cached rotation recovery reused a disappeared CLI instead of preparing its replacement.
func TestAFailedRotationInACachedAppServerRecoversInTheSamePoll(t *testing.T) {
	directory := t.TempDir()
	writeCodexAuth(t, directory, 10*3_600*time.Second, 3_600*time.Second)
	script := `#!/bin/sh
starts="$CODEX_USAGE_TEST_LOG.starts"
started_before=no
[ -e "$starts" ] && started_before=yes
printf 'start\n' >> "$starts"
IFS= read -r line || exit 1
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line || exit 1
IFS= read -r line || exit 1
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}'
IFS= read -r line || exit 1
if [ "$started_before" = no ]; then
    printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":10,"windowDurationMins":300}}}}'
    IFS= read -r line || exit 1
    printf '%s\n' '{"id":4,"result":{"account":null}}'
else
    printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":29,"windowDurationMins":10080}}}}'
fi
IFS= read -r line
`
	environment, log := fakeCodexAppServer(directory, script)
	replacementBin := filepath.Join(directory, "replacement-bin")
	if err := os.Mkdir(replacementBin, 0o777); err != nil {
		t.Fatal(err)
	}
	core.InstallFakeCommand(directory, "codex", script)
	environment["PATH"] = strings.Join([]string{directory, replacementBin}, string(os.PathListSeparator))
	environment["XDG_CACHE_HOME"] = "relative-cache-is-unusable"

	first, codexClient := refreshProfileWithCacheRoot(t.Context(), codexQuotaRequest(directory, environment), nil, nil)
	if first.Report.Error != nil {
		t.Fatalf("error = %q", *first.Report.Error)
	}
	if codexClient == nil {
		t.Fatal("the first poll caches its app-server")
	}

	if err := os.Remove(filepath.Join(directory, "codex")); err != nil {
		t.Fatal(err)
	}
	core.InstallFakeCommand(replacementBin, "codex", script)
	writeCodexAuth(t, directory, 600*time.Second, 3_000*time.Second)
	second, codexClient := refreshProfileWithCacheRoot(t.Context(), codexQuotaRequest(directory, environment), codexClient, nil)
	if codexClient != nil {
		codexClient.Shutdown(t.Context())
	}

	if second.Report.Error != nil {
		t.Fatalf("error = %q", *second.Report.Error)
	}
	if window := second.Report.WeeklyWindow(); window == nil || *window.RemainingPercent != 71 {
		t.Fatalf("weekly = %#v", window)
	}
	if starts := codexStarts(log); starts != 2 {
		t.Fatalf("starts = %d", starts)
	}
}

func TestAnUnreachableGrokReportsTheFailureInsteadOfAZeroReading(t *testing.T) {
	directory := t.TempDir()
	core.InstallFakeCommand(directory, "grok", "#!/bin/sh\nexit 1\n")

	outcome, _ := refreshProfileWithCacheRoot(t.Context(), QuotaRefreshRequest{
		ProfileID:   "grok",
		Harness:     core.HarnessKindGrok,
		AuthScheme:  core.AuthSchemeNativeLogin{},
		SourceHome:  directory,
		Environment: map[string]string{"PATH": directory},
		Cwd:         directory,
		CodexSource: core.CodexQuotaSourceChatGptLogin{},
	}, nil, nil)
	report := outcome.Report

	if len(report.Windows) != 0 {
		t.Fatalf("windows = %#v", report.Windows)
	}
	if report.Error == nil || *report.Error != "grok agent stdio exited before answering the usage request" {
		t.Fatalf("error = %v", report.Error)
	}
}

func processIsGone(pid int) bool {
	// Signal 0 only probes whether the process exists.
	return syscall.Kill(pid, 0) != nil
}

func TestDroppingAProfileFromTheConfigurationStopsItsCodexQuotaClient(t *testing.T) {
	directory := t.TempDir()
	pidFile := filepath.Join(directory, "codex.pid")
	// A `codex app-server` stand-in: answer one quota refresh, then stay
	// alive on stdin the way the real one does between refreshes. The
	// managed launcher runs the script directly, so `$$` is the spawned
	// process.
	installFakeManagedCodex(managedCodexCacheRoot(directory), `#!/bin/sh
printf '%s\n' "$$" > "$CODEX_QUOTA_TEST_PID"
IFS= read -r line || exit 0
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line || exit 0
IFS= read -r line || exit 0
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}'
IFS= read -r line || exit 0
printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300}}}}'
while IFS= read -r line; do :; done
`)
	request := QuotaRefreshRequest{
		ProfileID:  "codex-1",
		Harness:    core.HarnessKindCodex,
		AuthScheme: core.AuthSchemeNativeLogin{},
		SourceHome: directory,
		Environment: map[string]string{
			"PATH":                 directory,
			"CODEX_QUOTA_TEST_PID": pidFile,
		},
		Cwd:         directory,
		CodexSource: core.CodexQuotaSourceChatGptLogin{},
	}

	root := managedCodexCacheRoot(directory)
	quotas := QuotaManager{cacheRoot: &root}
	quotas.RefreshProfiles(t.Context(), []QuotaRefreshRequest{request}, func(QuotaRefreshOutcome) {})

	if report := quotas.Reports()["codex-1"]; report.Error != nil {
		t.Fatalf("the stand-in app-server must answer the quota query: %q", *report.Error)
	}
	text, err := os.ReadFile(pidFile)
	if err != nil {
		t.Fatal(err)
	}
	pid, err := strconv.Atoi(strings.TrimSpace(string(text)))
	if err != nil {
		t.Fatal(err)
	}
	if processIsGone(pid) {
		t.Fatal("the app-server child is cached between refreshes")
	}

	// The profile leaves the configuration, so the next batch no longer
	// carries it.
	quotas.RefreshProfiles(t.Context(), nil, func(QuotaRefreshOutcome) {})

	if !processIsGone(pid) {
		t.Fatal("a profile removed from the configuration must not leave its `codex app-server` child running")
	}
	quotas.Shutdown(t.Context())
}
