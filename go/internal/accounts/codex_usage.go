// Port of mj-accounts/src/codex_usage.rs.

// Codex subscription quota querying through `codex app-server`.
//
// Codex exposes ChatGPT subscription rate limits through its local
// app-server protocol rather than a one-shot CLI command. Keep the JSONL
// client isolated from the UI so protocol parsing and unavailable states
// remain testable.

package accounts

import (
	"bufio"
	"context"
	"fmt"
	"strings"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// A cold Codex app-server start can be slow on busy machines. The client is
// reused after initialization, so this primarily bounds the initial probe.
const codexUsageRequestTimeout = 60 * time.Second

const codexUsageMaxResponseBytes = 256 * 1024

// Enough of app-server's stderr to explain a failure, such as a rejected
// token refresh, without holding its whole log.
const (
	codexStderrTailLines = 20
	codexStderrLineBytes = 2 * 1024
)

// codexStderrDrainTimeout is how long a stopped app-server's stderr gets to
// reach end of stream.
const codexStderrDrainTimeout = time.Second

// CodexUsageStatus is the outcome of a Codex quota poll: Available or
// Unavailable.
type CodexUsageStatus interface{ isCodexUsageStatus() }

type CodexUsageStatusAvailable struct{ Report CodexUsageReport }

type CodexUsageStatusUnavailable struct{ Reason string }

func (CodexUsageStatusAvailable) isCodexUsageStatus()   {}
func (CodexUsageStatusUnavailable) isCodexUsageStatus() {}

type CodexUsageReport struct {
	MonthlyPriceUsd *uint32
	PaidCredits     *client.PaidCredits
	BankedResets    *uint64
	Primary         *CodexUsageWindow
	Secondary       *CodexUsageWindow
}

type CodexUsageWindow struct {
	Label            string
	RemainingPercent uint8
	DurationSeconds  *int64
	ResetsAt         *int64
}

// CodexLauncher is where a Codex quota check starts its app-server. The zero
// value is CodexLauncherHost.
type CodexLauncher struct {
	// program is Rust's test-only `Program` variant: a program the caller
	// chose. Empty is Host.
	program string
}

// CodexLauncherHost is this host's installed CLI, otherwise the pinned
// managed installation.
var CodexLauncherHost = CodexLauncher{}

// codexLauncherProgram is Rust's `CodexLauncher::Program` (tests only).
func codexLauncherProgram(command string) CodexLauncher {
	return CodexLauncher{program: command}
}

func (l CodexLauncher) prepare(ctx context.Context, env map[string]string) (*quotaProgram, error) {
	if l.program == "" {
		return locate(ctx, core.HarnessKindCodex, env)
	}
	command, err := commandText(l.program)
	if err != nil {
		return nil, err
	}
	return &quotaProgram{command: command, environment: map[string]string{}}, nil
}

// CodexUsageClient is a running `codex app-server` the quota poller reuses.
type CodexUsageClient struct {
	child *targets.ProxyChild
	// launcher is which launcher started this client; a poll that asks for
	// another one replaces it.
	launcher     CodexLauncher
	program      *quotaProgram
	stdin        targets.ProxyWriter
	stdout       *bufio.Reader
	stderr       *codexStderrTail
	stderrReader *core.JoinHandle[struct{}]
	nextID       uint64
	initialized  bool
}

func (c *CodexUsageClient) matchesLauncher(launcher CodexLauncher) bool {
	return c.launcher == launcher
}

// preparedProgram reuses the selected program and leases while its
// app-server is alive; a changed or damaged installation cannot invalidate a
// running client.
func (c *CodexUsageClient) preparedProgram(launcher CodexLauncher) *quotaProgram {
	if !c.matchesLauncher(launcher) {
		return nil
	}
	return c.program
}

func spawnCodexUsageClient(ctx context.Context, cwd string, env map[string]string, launcher CodexLauncher, program *quotaProgram) (*CodexUsageClient, error) {
	panic("unported: mj-accounts/src/codex_usage.rs CodexUsageClient::spawn")
}

func (c *CodexUsageClient) initialize(ctx context.Context) error {
	panic("unported: mj-accounts/src/codex_usage.rs CodexUsageClient::initialize")
}

// refreshToken asks app-server to rotate the stored login now.
//
// `account/read` with `refreshToken: true` "requests a proactive token
// refresh before returning; in managed auth mode this triggers the normal
// refresh-token flow; in external auth mode this flag is ignored". An
// app-server too old to know the flag answers `-32601`, which means there is
// nothing to do here rather than a failed poll.
//
// The request succeeds even when the refresh does not: app-server records
// the failure and answers with no account. Only a ChatGPT account in the
// answer means the login is still usable.
func (c *CodexUsageClient) refreshToken(ctx context.Context) error {
	panic("unported: mj-accounts/src/codex_usage.rs CodexUsageClient::refresh_token")
}

func (c *CodexUsageClient) query(ctx context.Context) (CodexUsageReport, error) {
	panic("unported: mj-accounts/src/codex_usage.rs CodexUsageClient::query")
}

func (c *CodexUsageClient) sendRequest(ctx context.Context, method string, params any) (uint64, error) {
	id := c.nextID
	if c.nextID != ^uint64(0) {
		c.nextID++
	}
	if err := c.writeMessage(ctx, map[string]any{"method": method, "id": id, "params": params}); err != nil {
		return 0, err
	}
	return id, nil
}

func (c *CodexUsageClient) writeMessage(ctx context.Context, message any) error {
	panic("unported: mj-accounts/src/codex_usage.rs CodexUsageClient::write_message")
}

func (c *CodexUsageClient) readResult(ctx context.Context, expectedID uint64) (any, error) {
	panic("unported: mj-accounts/src/codex_usage.rs CodexUsageClient::read_result")
}

// shutdownWithDiagnostics stops app-server and returns the end of its
// stderr. Waiting for the reader to reach end of stream keeps lines that
// were still in the pipe when the failed answer arrived.
func (c *CodexUsageClient) shutdownWithDiagnostics(ctx context.Context) []string {
	panic("unported: mj-accounts/src/codex_usage.rs CodexUsageClient::shutdown_with_diagnostics")
}

// Shutdown stops the app-server and reaps it.
func (c *CodexUsageClient) Shutdown(ctx context.Context) {
	c.stop(ctx)
}

// stop stops the process and hands back its stderr reader, which ends on
// its own once the pipe closes.
func (c *CodexUsageClient) stop(ctx context.Context) *core.JoinHandle[struct{}] {
	panic("unported: mj-accounts/src/codex_usage.rs CodexUsageClient::stop")
}

// codexStderrTail is the last lines app-server wrote to stderr, cleaned of
// terminal controls and anything that looks like a token.
type codexStderrTail struct {
	mu    sync.Mutex
	lines []string
}

// captureCodexStderrTail keeps reading stderr until app-server closes it.
// Reading also keeps a chatty process from blocking on a full pipe.
func captureCodexStderrTail(stderr targets.ProxyReader) (*codexStderrTail, *core.JoinHandle[struct{}]) {
	panic("unported: mj-accounts/src/codex_usage.rs StderrTail::capture")
}

func (t *codexStderrTail) push(line []byte) {
	text := strings.TrimSpace(core.RedactSecrets(core.SanitizeTerminalText(strings.ToValidUTF8(string(line), "�"))))
	if text == "" {
		return
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	if len(t.lines) == codexStderrTailLines {
		t.lines = t.lines[1:]
	}
	t.lines = append(t.lines, text)
}

func (t *codexStderrTail) snapshot() []string {
	t.mu.Lock()
	defer t.mu.Unlock()
	return append([]string(nil), t.lines...)
}

// codexUsageReadBoundedFrame reads one app-server frame; ok is false at the
// end of the stream.
func codexUsageReadBoundedFrame(reader *bufio.Reader) ([]byte, bool, error) {
	frame, err := core.ReadBoundedFrame(reader, codexUsageMaxResponseBytes)
	if err != nil {
		if _, tooLarge := err.(core.BoundedFrameErrorTooLarge); tooLarge {
			return nil, false, codexQueryError{kind: codexQueryProtocol, protocol: codexProtocolTooLarge}
		}
		return nil, false, codexQueryError{kind: codexQueryProtocol, protocol: codexProtocolIo}
	}
	switch frame := frame.(type) {
	case core.BoundedFrameLine:
		return frame.Bytes, true, nil
	case core.BoundedFrameTruncated:
		return nil, false, codexQueryError{kind: codexQueryProtocol, protocol: codexProtocolClosed}
	}
	return nil, false, nil
}

// codexParseResponse is the result of the response to expectedID, or ok
// false for any other message.
func codexParseResponse(message any, expectedID uint64) (any, bool, error) {
	panic("unported: mj-accounts/src/codex_usage.rs parse_response")
}

// codexUsageStart starts the client when there is none for launcher.
// Installation is preparation, outside the app-server request deadline.
func codexUsageStart(ctx context.Context, client **CodexUsageClient, cwd string, env map[string]string, launcher CodexLauncher, prepared **quotaProgram) error {
	panic("unported: mj-accounts/src/codex_usage.rs start")
}

func codexUsagePrepare(ctx context.Context, client **CodexUsageClient) (*CodexUsageClient, error) {
	ready := *client
	if ready == nil {
		panic("client started before the request deadline")
	}
	if err := ready.initialize(ctx); err != nil {
		return nil, err
	}
	return ready, nil
}

// discardFailedClient drops a client the failure says is no longer usable,
// logging what its app-server wrote to stderr so the cause is not lost with
// the process.
func discardFailedClient(ctx context.Context, client **CodexUsageClient, replaceable bool) {
	panic("unported: mj-accounts/src/codex_usage.rs discard_failed_client")
}

// refreshWithCacheRoot refreshes a persistent app-server client, recreating
// it after transport or protocol failures. Calls are made serially by the
// session worker.
func refreshWithCacheRoot(ctx context.Context, client **CodexUsageClient, cwd string, env map[string]string, launcher CodexLauncher, prepared **quotaProgram) CodexUsageStatus {
	panic("unported: mj-accounts/src/codex_usage.rs refresh_with_cache_root")
}

// LoginRefreshFailure is why refreshLoginWithCacheRoot could not rotate the
// login.
type LoginRefreshFailure struct {
	// Detail is the failure in full, for the log.
	Detail string
	// Reason is the failure as refreshWithCacheRoot would report it to the
	// user.
	Reason string
}

func (f *LoginRefreshFailure) Error() string { return f.Detail }

// refreshLoginWithCacheRoot rotates the profile's Codex login ahead of its
// expiry, reusing the cached client the way refreshWithCacheRoot does.
//
// Codex refresh tokens are single use. A host and a container that reach
// expiry at the same instant both try to spend the same token, one wins, and
// the loser's turn dies. Rotating early on the host, so the sync can push
// the new file, keeps container copies away from that instant.
func refreshLoginWithCacheRoot(ctx context.Context, client **CodexUsageClient, cwd string, env map[string]string, launcher CodexLauncher, prepared **quotaProgram) *LoginRefreshFailure {
	panic("unported: mj-accounts/src/codex_usage.rs refresh_login_with_cache_root")
}

// spawnAppServer starts `codex app-server` from launcher, with the
// environment a quota check gives it.
func spawnAppServer(launcher, cwd string, env map[string]string) (*targets.ProxyStdio, error) {
	panic("unported: mj-accounts/src/codex_usage.rs spawn_app_server")
}

type codexQueryErrorKind int

const (
	codexQueryNotInstalled codexQueryErrorKind = iota
	codexQueryLaunch
	codexQueryNotSignedIn
	codexQueryUnsupportedAccount
	codexQueryUnsupported
	codexQueryNoData
	codexQueryProtocol
)

// codexQueryError is Rust's QueryError: expected and detail belong to
// NotInstalled, detail also to Launch, protocol to Protocol.
type codexQueryError struct {
	kind     codexQueryErrorKind
	expected *string
	detail   *string
	protocol codexProtocolError
}

type codexProtocolError int

const (
	codexProtocolIo codexProtocolError = iota
	codexProtocolClosed
	codexProtocolInvalidResponse
	codexProtocolTooLarge
	codexProtocolRemoteError
)

// String is the Rust Debug name of the variant.
func (e codexProtocolError) String() string {
	return [...]string{"Io", "Closed", "InvalidResponse", "TooLarge", "RemoteError"}[e]
}

// needsFreshClient is whether the cached app-server must be replaced before
// the next call.
//
// A transport or protocol break leaves the stream out of step. An
// authentication failure can be state inside app-server rather than in the
// credential file: once a token refresh fails, app-server reports no account
// until it restarts, even after the file is fixed (#1224). A new process
// reads the file again. While the login stays invalid each poll starts one
// process, so the poll interval bounds the cost.
func (e codexQueryError) needsFreshClient() bool {
	switch e.kind {
	case codexQueryProtocol, codexQueryUnsupported, codexQueryNotSignedIn, codexQueryUnsupportedAccount:
		return true
	}
	return false
}

func (e codexQueryError) unavailableReason() string {
	if e.kind == codexQueryLaunch || e.kind == codexQueryNotInstalled {
		return e.Error()
	}
	return e.userReason()
}

func (e codexQueryError) userReason() string {
	return [...]string{
		codexQueryNotInstalled:       "Codex CLI is unavailable",
		codexQueryLaunch:             "could not start Codex CLI",
		codexQueryNotSignedIn:        "not signed in with ChatGPT",
		codexQueryUnsupportedAccount: "ChatGPT subscription quota is not available for this account",
		codexQueryUnsupported:        "installed Codex does not support quota queries",
		codexQueryNoData:             "no rate-limit data returned",
		codexQueryProtocol:           "Codex quota request failed",
	}[e.kind]
}

func (e codexQueryError) Error() string {
	switch e.kind {
	case codexQueryLaunch:
		detail := ""
		if e.detail != nil {
			detail = *e.detail
		}
		return "could not start Codex CLI: " + detail
	case codexQueryProtocol:
		return fmt.Sprintf("Codex app-server protocol error (%s)", e.protocol)
	case codexQueryNotInstalled:
		text := e.userReason()
		if e.expected != nil {
			text += "; expected installation at " + *e.expected
		}
		if e.detail != nil {
			text += ": " + *e.detail
		}
		return text
	}
	return e.userReason()
}

func classifyAccount(result any) error {
	panic("unported: mj-accounts/src/codex_usage.rs classify_account")
}

func parseCodexReport(result any, accountPlan *string) (CodexUsageReport, error) {
	panic("unported: mj-accounts/src/codex_usage.rs parse_report")
}

func parseCodexSnapshot(snapshot any, accountPlan *string) *CodexUsageReport {
	panic("unported: mj-accounts/src/codex_usage.rs parse_snapshot")
}

// planMonthlyPrice is the US list price of a Codex plan. Codex 0.170.0's
// status tests identify the personal plan labels:
// https://github.com/openai/codex/blob/rust-v0.170.0/codex-rs/tui/src/status/helpers.rs#L187-L215
func planMonthlyPrice(plan string) (uint32, bool) {
	switch plan {
	// ChatGPT Go includes a Codex allowance at a US list price of $8:
	// https://openai.com/index/introducing-chatgpt-go/
	case "go":
		return 8, true
	// The weight stands for the plan's allowance. Standard Business
	// (reported as `team` or `self_serve_business_usage_based`) has the same
	// five-hour Codex estimates as Plus:
	// https://learn.chatgpt.com/docs/pricing#included-usage
	// Enterprise and Edu have no documented equivalent.
	case "plus", "team", "self_serve_business_usage_based":
		return 20, true
	// Business Premium lists at $100 per seat per month (annual billing):
	// https://help.openai.com/en/articles/11391654-chatgpt-business-release-notes
	case "self_serve_business_prolite", "prolite":
		return 100, true
	case "pro":
		return 200, true
	case "promax":
		return 500, true
	}
	return 0, false
}

func parseCodexPaidCredits(value any) *client.PaidCredits {
	panic("unported: mj-accounts/src/codex_usage.rs parse_paid_credits")
}

func parseCodexWindow(value any) *CodexUsageWindow {
	panic("unported: mj-accounts/src/codex_usage.rs parse_window")
}

func codexUsageWindowLabel(minutes *int64) string {
	if minutes == nil {
		return "limit"
	}
	switch value := *minutes; {
	case value == 300:
		return "5H"
	case value == 10_080:
		return "Week"
	case value > 0 && value < 60:
		return fmt.Sprintf("%dm", value)
	case value > 0 && value%1_440 == 0:
		return fmt.Sprintf("%dd", value/1_440)
	case value > 0 && value%60 == 0:
		return fmt.Sprintf("%dH", value/60)
	}
	return "limit"
}
