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
	"errors"
	"fmt"
	"log/slog"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

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
// running client. The caller holds the returned program (Rust: an `Arc`
// clone) and releases it once.
func (c *CodexUsageClient) preparedProgram(launcher CodexLauncher) *quotaProgram {
	if !c.matchesLauncher(launcher) {
		return nil
	}
	return c.program.retain()
}

// spawnCodexUsageClient starts app-server with program, which the client
// then holds.
func spawnCodexUsageClient(ctx context.Context, cwd string, env map[string]string, launcher CodexLauncher, program *quotaProgram) (*CodexUsageClient, error) {
	merged := make(map[string]string, len(env)+len(program.environment))
	for name, value := range env {
		merged[name] = value
	}
	for name, value := range program.environment {
		merged[name] = value
	}
	stdio, err := spawnAppServer(program.command, cwd, merged)
	if err != nil {
		return nil, err
	}
	tail, reader := captureCodexStderrTail(stdio.Stderr)
	return &CodexUsageClient{
		child:        stdio.Child,
		launcher:     launcher,
		program:      program,
		stdin:        stdio.Stdin,
		stdout:       bufio.NewReader(stdio.Stdout),
		stderr:       tail,
		stderrReader: reader,
		nextID:       1,
	}, nil
}

func (c *CodexUsageClient) initialize(ctx context.Context) error {
	if c.initialized {
		return nil
	}
	id, err := c.sendRequest(ctx, "initialize", map[string]any{
		"clientInfo": map[string]any{
			"name":    "mj",
			"title":   "Mjolnir",
			"version": core.PackageVersion,
		},
	})
	if err != nil {
		return err
	}
	if _, err := c.readResult(ctx, id); err != nil {
		return err
	}
	if err := c.writeMessage(ctx, map[string]any{"method": "initialized"}); err != nil {
		return err
	}
	c.initialized = true
	return nil
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
	id, err := c.sendRequest(ctx, "account/read", map[string]any{"refreshToken": true})
	if err != nil {
		return err
	}
	account, err := c.readResult(ctx, id)
	if err != nil {
		var failure codexQueryError
		if errors.As(err, &failure) && failure.kind == codexQueryUnsupported {
			return nil
		}
		return err
	}
	return classifyAccount(account)
}

func (c *CodexUsageClient) query(ctx context.Context) (CodexUsageReport, error) {
	accountID, err := c.sendRequest(ctx, "account/read", map[string]any{"refreshToken": false})
	if err != nil {
		return CodexUsageReport{}, err
	}
	account, err := c.readResult(ctx, accountID)
	if err != nil {
		return CodexUsageReport{}, err
	}
	if err := classifyAccount(account); err != nil {
		return CodexUsageReport{}, err
	}
	limitsID, err := c.sendRequest(ctx, "account/rateLimits/read", nil)
	if err != nil {
		return CodexUsageReport{}, err
	}
	limits, err := c.readResult(ctx, limitsID)
	if err != nil {
		return CodexUsageReport{}, err
	}
	var accountPlan *string
	if plan, ok := jvStr(jvPointer(account, "account", "planType")); ok {
		accountPlan = &plan
	}
	return parseCodexReport(limits, accountPlan)
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

// writeMessage writes message as one JSON line, its object keys in
// serde_json's sorted order.
func (c *CodexUsageClient) writeMessage(ctx context.Context, message any) error {
	encoded, err := jsonvalue.Marshal(message)
	if err != nil {
		return codexQueryError{kind: codexQueryProtocol, protocol: codexProtocolInvalidResponse}
	}
	encoded = append(encoded, '\n')
	if _, err := c.stdin.Write(encoded); err != nil {
		return codexQueryError{kind: codexQueryProtocol, protocol: codexProtocolIo}
	}
	return nil
}

func (c *CodexUsageClient) readResult(ctx context.Context, expectedID uint64) (any, error) {
	for {
		line, ok, err := codexUsageReadBoundedFrame(c.stdout)
		if err != nil {
			return nil, err
		}
		if !ok {
			return nil, codexQueryError{kind: codexQueryProtocol, protocol: codexProtocolClosed}
		}
		message, err := jsonvalue.Parse(line)
		if err != nil {
			return nil, codexQueryError{kind: codexQueryProtocol, protocol: codexProtocolInvalidResponse}
		}
		result, matched, err := codexParseResponse(message, expectedID)
		if err != nil {
			return nil, err
		}
		if matched {
			return result, nil
		}
	}
}

// shutdownWithDiagnostics stops app-server and returns the end of its
// stderr. Waiting for the reader to reach end of stream keeps lines that
// were still in the pipe when the failed answer arrived.
func (c *CodexUsageClient) shutdownWithDiagnostics(ctx context.Context) []string {
	tail := c.stderr
	reader := c.stop(ctx)
	// A process app-server started could still hold the pipe open.
	timer := time.NewTimer(codexStderrDrainTimeout)
	defer timer.Stop()
	select {
	case <-reader.Done():
	case <-timer.C:
	}
	return tail.snapshot()
}

// Shutdown stops the app-server and reaps it.
func (c *CodexUsageClient) Shutdown(ctx context.Context) {
	c.stop(ctx)
}

// stop stops the process and hands back its stderr reader, which ends on
// its own once the pipe closes.
func (c *CodexUsageClient) stop(ctx context.Context) *core.JoinHandle[struct{}] {
	// Closing stdin asks app-server to stop. The quota process is always
	// launched directly (never through npx), so killing the recorded child
	// is sufficient if it does not notice EOF promptly.
	_ = c.stdin.Close()
	if exited, _ := c.child.TryWait(); exited == nil {
		if err := c.child.StartKill(); err != nil && !strings.Contains(err.Error(), "can't kill an exited process") {
			slog.Warn("could not stop the Codex quota process", "error", err)
		}
	}
	if _, err := c.child.Wait(context.WithoutCancel(ctx)); err != nil {
		slog.Warn("could not reap the Codex quota process", "error", err)
	}
	c.child.Close()
	// The installation's leases stay held until the child has stopped.
	c.program.release()
	return c.stderrReader
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
	tail := &codexStderrTail{}
	reader := core.Spawn(func() struct{} {
		buffered := bufio.NewReader(stderr)
		line := []byte{}
		for {
			chunk, err := buffered.ReadSlice('\n')
			newline := len(chunk) > 0 && chunk[len(chunk)-1] == '\n'
			content := chunk
			if newline {
				content = chunk[:len(chunk)-1]
			}
			room := max(codexStderrLineBytes-len(line), 0)
			line = append(line, content[:min(len(content), room)]...)
			if newline {
				tail.push(line)
				line = line[:0]
			}
			if err != nil && !errors.Is(err, bufio.ErrBufferFull) {
				break
			}
		}
		tail.push(line)
		return struct{}{}
	})
	return tail, reader
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
	if id, ok := jvU64(jvGet(message, "id")); !ok || id != expectedID {
		return nil, false, nil
	}
	if failure, ok := jvGet(message, "error"); ok {
		if code, ok := jvI64(jvGet(failure, "code")); ok && code == -32601 {
			return nil, false, codexQueryError{kind: codexQueryUnsupported}
		}
		return nil, false, codexQueryError{kind: codexQueryProtocol, protocol: codexProtocolRemoteError}
	}
	result, ok := jvGet(message, "result")
	if !ok {
		return nil, false, codexQueryError{kind: codexQueryProtocol, protocol: codexProtocolInvalidResponse}
	}
	return result, true, nil
}

// codexUsageStart starts the client when there is none for launcher.
// Installation is preparation, outside the app-server request deadline.
// A prepared program is taken over by the client it starts.
func codexUsageStart(ctx context.Context, client **CodexUsageClient, cwd string, env map[string]string, launcher CodexLauncher, prepared **quotaProgram) error {
	if stale := *client; stale != nil && !stale.matchesLauncher(launcher) {
		*client = nil
		stale.Shutdown(ctx)
	}
	if *client != nil {
		return nil
	}
	program := *prepared
	*prepared = nil
	if program == nil {
		var err error
		program, err = launcher.prepare(ctx, env)
		if err != nil {
			detail := err.Error()
			return codexQueryError{kind: codexQueryLaunch, detail: &detail}
		}
	}
	started, err := spawnCodexUsageClient(ctx, cwd, env, launcher, program)
	if err != nil {
		program.release()
		return err
	}
	*client = started
	return nil
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
	stale := *client
	if !replaceable || stale == nil {
		return
	}
	*client = nil
	if diagnostics := stale.shutdownWithDiagnostics(ctx); len(diagnostics) > 0 {
		slog.Warn("Codex quota process diagnostics before restart", "stderr", strings.Join(diagnostics, "\n"))
	}
}

// codexRequestOutcome is how a bounded app-server request ended: err is its
// failure, timedOut that the deadline passed first.
type codexRequestOutcome struct {
	err      error
	timedOut bool
}

// codexBoundedRequest starts the client and runs request on it under the
// request deadline. Starting, which may install the harness, is outside
// the deadline. A request still running at the deadline is abandoned with
// its client, which the caller then discards; the client is stopped before
// this returns, so the abandoned request ends with its pipes.
func codexBoundedRequest(ctx context.Context, client **CodexUsageClient, cwd string, env map[string]string, launcher CodexLauncher, prepared **quotaProgram, request func(ctx context.Context, ready *CodexUsageClient) error) codexRequestOutcome {
	if err := codexUsageStart(ctx, client, cwd, env, launcher, prepared); err != nil {
		return codexRequestOutcome{err: err}
	}
	// The request works on its own copy of the slot, so discarding the
	// client at the deadline does not race with it.
	slot := *client
	done := make(chan error, 1)
	go func() {
		ready, err := codexUsagePrepare(ctx, &slot)
		if err == nil {
			err = request(ctx, ready)
		}
		done <- err
	}()
	timer := time.NewTimer(codexUsageRequestTimeout)
	defer timer.Stop()
	select {
	case err := <-done:
		return codexRequestOutcome{err: err}
	case <-timer.C:
	case <-ctx.Done():
	}
	discardFailedClient(ctx, client, true)
	// The stopped process closed the pipes the request was using.
	<-done
	return codexRequestOutcome{timedOut: true}
}

// refreshWithCacheRoot refreshes a persistent app-server client, recreating
// it after transport or protocol failures. Calls are made serially by the
// session worker.
func refreshWithCacheRoot(ctx context.Context, client **CodexUsageClient, cwd string, env map[string]string, launcher CodexLauncher, prepared **quotaProgram) CodexUsageStatus {
	var report CodexUsageReport
	outcome := codexBoundedRequest(ctx, client, cwd, env, launcher, prepared, func(ctx context.Context, ready *CodexUsageClient) error {
		var err error
		report, err = ready.query(ctx)
		return err
	})
	if outcome.timedOut {
		slog.Warn("codex quota query timed out")
		return CodexUsageStatusUnavailable{Reason: "request timed out"}
	}
	if outcome.err != nil {
		failure := codexAsQueryError(outcome.err)
		discardFailedClient(ctx, client, failure.needsFreshClient())
		slog.Warn("codex quota query failed: " + failure.Error())
		return CodexUsageStatusUnavailable{Reason: failure.unavailableReason()}
	}
	return CodexUsageStatusAvailable{Report: report}
}

// codexAsQueryError is err as the QueryError every client step returns.
func codexAsQueryError(err error) codexQueryError {
	var failure codexQueryError
	if errors.As(err, &failure) {
		return failure
	}
	detail := err.Error()
	return codexQueryError{kind: codexQueryLaunch, detail: &detail}
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
	outcome := codexBoundedRequest(ctx, client, cwd, env, launcher, prepared, func(ctx context.Context, ready *CodexUsageClient) error {
		return ready.refreshToken(ctx)
	})
	if outcome.timedOut {
		return &LoginRefreshFailure{Detail: "request timed out", Reason: "request timed out"}
	}
	if outcome.err != nil {
		failure := codexAsQueryError(outcome.err)
		discardFailedClient(ctx, client, failure.needsFreshClient())
		return &LoginRefreshFailure{Detail: failure.Error(), Reason: failure.unavailableReason()}
	}
	return nil
}

// spawnAppServer starts `codex app-server` from launcher, with the
// environment a quota check gives it.
func spawnAppServer(launcher, cwd string, env map[string]string) (*targets.ProxyStdio, error) {
	spec := core.NewCommandSpec(launcher, []string{"app-server", "--stdio"})
	spec.Cwd = &cwd
	spec.Env = make(map[string]string, len(env)+1)
	for name, value := range env {
		spec.Env[name] = value
	}
	// This probe reads a ChatGPT login's rate limits and refreshes that
	// login. An API key from the profile's environment or the daemon's own
	// would let Codex use the key instead (#1160).
	spec.EnvRemove = append([]string(nil), core.CodexCredentialEnvironment[:]...)
	// Its stderr is kept for diagnostics. At error level app-server reports
	// a rejected token refresh; a broader level inherited from the daemon
	// would add HTTP request bodies that carry the refresh token.
	spec.Env["RUST_LOG"] = "error"
	stdio, err := targets.SpawnProxy(&spec)
	if err != nil {
		if targets.IsMissingProgram(err) {
			detail := fmt.Sprintf("launcher %s could not be found", launcher)
			return nil, codexQueryError{kind: codexQueryNotInstalled, detail: &detail}
		}
		detail := fmt.Sprintf("could not start Codex launcher %s: %v", launcher, err)
		return nil, codexQueryError{kind: codexQueryLaunch, detail: &detail}
	}
	return stdio, nil
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
	account, ok := jvGet(result, "account")
	if !ok || account == nil {
		return codexQueryError{kind: codexQueryNotSignedIn}
	}
	if kind, ok := jvStr(jvGet(account, "type")); ok && kind == "chatgpt" {
		return nil
	}
	return codexQueryError{kind: codexQueryUnsupportedAccount}
}

func parseCodexReport(result any, accountPlan *string) (CodexUsageReport, error) {
	var report *CodexUsageReport
	if buckets, ok := jvGet(result, "rateLimitsByLimitId"); ok {
		if _, isObject := buckets.(jsonvalue.Object); isObject {
			if snapshot, ok := jvGet(buckets, "codex"); ok {
				report = parseCodexSnapshot(snapshot, accountPlan)
			}
		}
	}
	if report == nil {
		if snapshot, ok := jvGet(result, "rateLimits"); ok {
			report = parseCodexSnapshot(snapshot, accountPlan)
		}
	}
	if report == nil {
		return CodexUsageReport{}, codexQueryError{kind: codexQueryNoData}
	}
	if credits, ok := jvGet(result, "rateLimitResetCredits"); ok && credits != nil {
		if count, ok := jvU64(jvGet(credits, "availableCount")); ok {
			report.BankedResets = &count
		} else {
			slog.Warn("Codex usage returned malformed reset-credit metadata")
		}
	}
	return *report, nil
}

func parseCodexSnapshot(snapshot any, accountPlan *string) *CodexUsageReport {
	// Codex reports `"unknown"` when the quota snapshot lacks billing
	// information; treat it as missing and use the account's plan.
	plan := accountPlan
	if snapshotPlan, ok := jvStr(jvGet(snapshot, "planType")); ok && snapshotPlan != "unknown" {
		plan = &snapshotPlan
	}
	report := CodexUsageReport{}
	if plan != nil {
		if price, ok := planMonthlyPrice(*plan); ok {
			report.MonthlyPriceUsd = &price
		}
	}
	if credits, ok := jvGet(snapshot, "credits"); ok {
		report.PaidCredits = parseCodexPaidCredits(credits)
	}
	if primary, ok := jvGet(snapshot, "primary"); ok {
		report.Primary = parseCodexWindow(primary)
	}
	if secondary, ok := jvGet(snapshot, "secondary"); ok {
		report.Secondary = parseCodexWindow(secondary)
	}
	if report.Primary == nil && report.Secondary == nil && report.PaidCredits == nil {
		return nil
	}
	return &report
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
	if unlimited, ok := jvBool(jvGet(value, "unlimited")); ok && unlimited {
		return &client.PaidCredits{Kind: client.PaidCreditsUnlimited}
	}
	hasCredits, ok := jvBool(jvGet(value, "hasCredits"))
	if !ok {
		return nil
	}
	if !hasCredits {
		return client.NewPaidCreditsLimited(0.0)
	}
	balance, ok := jvGet(value, "balance")
	if !ok || balance == nil {
		return &client.PaidCredits{Kind: client.PaidCreditsAvailable}
	}
	text, ok := balance.(string)
	if !ok {
		return nil
	}
	amount, ok := codexParseF64(text)
	if !ok {
		return nil
	}
	return client.NewPaidCreditsLimited(amount)
}

// codexParseF64 is Rust's `str::parse::<f64>`: decimal digits with an
// optional sign, point and exponent, or `inf`, `infinity` and `nan`; no
// hexadecimal form and no digit separators.
func codexParseF64(text string) (float64, bool) {
	if strings.ContainsAny(text, "_xXpP") {
		return 0, false
	}
	value, err := strconv.ParseFloat(text, 64)
	if err != nil {
		var numeric *strconv.NumError
		if errors.As(err, &numeric) && errors.Is(numeric.Err, strconv.ErrRange) {
			return value, true
		}
		return 0, false
	}
	return value, true
}

func parseCodexWindow(value any) *CodexUsageWindow {
	used, ok := jvI64(jvGet(value, "usedPercent"))
	if !ok {
		return nil
	}
	used = min(max(used, 0), 100)
	var duration *int64
	if minutes, ok := jvI64(jvGet(value, "windowDurationMins")); ok {
		duration = &minutes
	}
	window := &CodexUsageWindow{
		Label:            codexUsageWindowLabel(duration),
		RemainingPercent: uint8(100 - used),
	}
	if duration != nil && *duration >= -153_722_867_280_912_930 && *duration <= 153_722_867_280_912_930 {
		seconds := *duration * 60
		window.DurationSeconds = &seconds
	}
	if resetsAt, ok := jvI64(jvGet(value, "resetsAt")); ok {
		window.ResetsAt = &resetsAt
	}
	return window
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
