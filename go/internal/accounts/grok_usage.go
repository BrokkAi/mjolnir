// Port of mj-accounts/src/grok_usage.rs.

// Grok Build subscription usage querying through its ACP billing extension.
//
// Grok Build publishes no HTTP quota endpoint. Its own `/usage` view polls
// an ACP ext request, `_x.ai/billing`, served by `grok agent stdio`. Keep the
// JSON-RPC exchange isolated from the UI so the response parser stays
// testable against captured payloads without spawning `grok`.

package accounts

import (
	"bufio"
	"context"
	"encoding/json"
	"log/slog"
	"math"
	"strings"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// grokUsageRequestTimeout covers a cold agent start plus one round trip to
// the billing backend. Generous because the alternative — reporting a
// timeout on a slow machine — reads to the user as a broken account.
const grokUsageRequestTimeout = 30 * time.Second

const grokUsageMaxResponseBytes = 256 * 1024

// grokBillingMethod is the ext method Grok Build serves for billing. The `_`
// prefix is the ACP ext framing, which is how the method appears on the
// wire.
const grokBillingMethod = "_x.ai/billing"

type GrokUsageReport struct {
	// PeriodLabel is the window name for the quota row: `Week` or `Month`,
	// matching the period the account is billed on.
	PeriodLabel string
	// UsedPercent is included-credit usage as a percentage of the allowance,
	// 0.0 to 100.0.
	UsedPercent float64
	// ResetsAt is epoch seconds when the current period ends.
	ResetsAt *int64
}

// RemainingPercent is the share of the allowance still available, rounded
// for the quota bar.
func (r *GrokUsageReport) RemainingPercent() uint8 {
	remaining := math.Round(100 - r.UsedPercent)
	if math.IsNaN(remaining) {
		return 0
	}
	return uint8(min(max(remaining, 0), 100))
}

// GrokUsageErrorKind is which GrokUsageError happened.
type GrokUsageErrorKind int

const (
	GrokUsageErrorLaunch GrokUsageErrorKind = iota
	GrokUsageErrorTimedOut
	// GrokUsageErrorClosed: the agent exited before answering. Grok Build
	// tears the process down when its stdin closes, so this also catches an
	// early hangup.
	GrokUsageErrorClosed
	GrokUsageErrorProtocol
	// GrokUsageErrorRejected: the agent answered the billing request with a
	// JSON-RPC error.
	GrokUsageErrorRejected
	// GrokUsageErrorNoData: the agent answered, but with no billing
	// configuration in it.
	GrokUsageErrorNoData
)

// GrokUsageError is why a Grok usage query failed. Detail is the payload of
// Launch, Protocol and Rejected. Comparable, as Rust's PartialEq is.
type GrokUsageError struct {
	Kind   GrokUsageErrorKind
	Detail string
}

func (e GrokUsageError) Error() string {
	switch e.Kind {
	case GrokUsageErrorLaunch:
		return "launch grok agent stdio: " + e.Detail
	case GrokUsageErrorTimedOut:
		return "grok usage request timed out"
	case GrokUsageErrorClosed:
		return "grok agent stdio exited before answering the usage request"
	case GrokUsageErrorProtocol:
		return "grok usage response was unreadable: " + e.Detail
	case GrokUsageErrorRejected:
		return "grok refused the usage request: " + e.Detail
	}
	return "grok reported no billing configuration"
}

// GrokUsageQuery asks a profile's Grok Build for its current billing
// period. The error is a GrokUsageError.
//
// Cancelling ctx stops the agent as dropping the Rust future does, and
// returns ctx's error.
func GrokUsageQuery(ctx context.Context, cwd string, env map[string]string) (GrokUsageReport, error) {
	session, err := spawnGrokUsageSession(ctx, cwd, env)
	if err != nil {
		return GrokUsageReport{}, err
	}
	// The exchange is bounded as a whole: a stalled agent must not hold the
	// quota refresh open, and its child must still be reaped.
	type outcome struct {
		report GrokUsageReport
		err    error
	}
	exchanged := make(chan outcome, 1)
	go func() {
		report, err := session.exchange(ctx)
		exchanged <- outcome{report: report, err: err}
	}()
	timer := time.NewTimer(grokUsageRequestTimeout)
	defer timer.Stop()
	var result outcome
	select {
	case result = <-exchanged:
	case <-timer.C:
		result = outcome{err: GrokUsageError{Kind: GrokUsageErrorTimedOut}}
	case <-ctx.Done():
		result = outcome{err: ctx.Err()}
	}
	session.shutdown(ctx)
	return result.report, result.err
}

type grokUsageSession struct {
	child   *targets.ProxyChild
	program *quotaProgram
	// stdin is held for the whole exchange on purpose: Grok Build stops when
	// its stdin closes, so closing this early kills the agent before it
	// answers.
	stdin  targets.ProxyWriter
	stdout *bufio.Reader
	// stderr drains stderr so a chatty agent cannot block on a full pipe.
	stderr *core.JoinHandle[struct{}]
	// stderrPipe is the pipe stderr drains; closing it aborts the drain
	// (Rust: aborting the task).
	stderrPipe targets.ProxyReader
}

func spawnGrokUsageSession(ctx context.Context, cwd string, env map[string]string) (*grokUsageSession, error) {
	program, err := locate(ctx, core.HarnessKindGrok, env)
	if err != nil {
		return nil, GrokUsageError{Kind: GrokUsageErrorLaunch, Detail: err.Error()}
	}
	spec := core.NewCommandSpec(program.command, []string{"agent", "stdio"})
	spec.Cwd = &cwd
	spec.Env = make(map[string]string, len(env)+len(program.environment))
	for name, value := range env {
		spec.Env[name] = value
	}
	for name, value := range program.environment {
		spec.Env[name] = value
	}
	stdio, err := targets.SpawnProxy(&spec)
	if err != nil {
		program.release()
		return nil, GrokUsageError{Kind: GrokUsageErrorLaunch, Detail: err.Error()}
	}
	stderrPipe := stdio.Stderr
	stderr := core.Spawn(func() struct{} {
		_, _ = targets.CollectBounded(stderrPipe, 64*1024)
		return struct{}{}
	})
	return &grokUsageSession{
		child:      stdio.Child,
		program:    program,
		stdin:      stdio.Stdin,
		stdout:     bufio.NewReader(stdio.Stdout),
		stderr:     stderr,
		stderrPipe: stderrPipe,
	}, nil
}

func (s *grokUsageSession) exchange(ctx context.Context) (GrokUsageReport, error) {
	if err := s.writeMessage(map[string]any{
		"jsonrpc": "2.0",
		"id":      1,
		"method":  "initialize",
		"params":  map[string]any{"protocolVersion": 1, "clientCapabilities": map[string]any{}},
	}); err != nil {
		return GrokUsageReport{}, err
	}
	if _, err := s.readResult(1); err != nil {
		return GrokUsageReport{}, err
	}
	if err := s.writeMessage(map[string]any{
		"jsonrpc": "2.0",
		"id":      2,
		"method":  grokBillingMethod,
		"params":  map[string]any{},
	}); err != nil {
		return GrokUsageReport{}, err
	}
	billing, err := s.readResult(2)
	if err != nil {
		return GrokUsageReport{}, err
	}
	return GrokUsageParse(billing)
}

func (s *grokUsageSession) writeMessage(message any) error {
	encoded, err := json.Marshal(message)
	if err != nil {
		return GrokUsageError{Kind: GrokUsageErrorProtocol, Detail: err.Error()}
	}
	encoded = append(encoded, '\n')
	if _, err := s.stdin.Write(encoded); err != nil {
		return GrokUsageError{Kind: GrokUsageErrorClosed}
	}
	return nil
}

// readResult reads frames until the answer to expectedID arrives.
// Everything else on the stream is the agent's own startup notifications.
func (s *grokUsageSession) readResult(expectedID uint64) (any, error) {
	for {
		frame, ok, err := grokUsageReadBoundedFrame(s.stdout)
		if err != nil {
			return nil, err
		}
		if !ok {
			return nil, GrokUsageError{Kind: GrokUsageErrorClosed}
		}
		message, err := jsonvalue.Parse(frame)
		if err != nil {
			// A login-shell banner or other non-JSON noise is not fatal on
			// its own; the answer may still be coming.
			continue
		}
		if id, ok := jvU64(jvGet(message, "id")); !ok || id != expectedID {
			continue
		}
		if failure, ok := jvGet(message, "error"); ok {
			detail, ok := jvStr(jvGet(failure, "message"))
			if !ok {
				detail = "no message"
			}
			return nil, GrokUsageError{Kind: GrokUsageErrorRejected, Detail: detail}
		}
		result, ok := jvGet(message, "result")
		if !ok {
			return nil, GrokUsageError{Kind: GrokUsageErrorProtocol, Detail: "response carries no result"}
		}
		return result, nil
	}
}

func (s *grokUsageSession) shutdown(ctx context.Context) {
	_ = s.stdin.Close()
	if exited, _ := s.child.TryWait(); exited == nil {
		if err := s.child.StartKill(); err != nil && !strings.Contains(err.Error(), "can't kill an exited process") {
			slog.Warn("could not stop the Grok usage process", "error", err)
		}
	}
	if _, err := s.child.Wait(context.WithoutCancel(ctx)); err != nil {
		slog.Warn("could not reap the Grok usage process", "error", err)
	}
	_ = s.stderrPipe.Close()
	s.child.Close()
	// The installation's leases stay held until the child has stopped.
	s.program.release()
}

// grokUsageReadBoundedFrame reads one frame; ok is false at the end of the
// stream.
func grokUsageReadBoundedFrame(reader *bufio.Reader) ([]byte, bool, error) {
	frame, err := core.ReadBoundedFrame(reader, grokUsageMaxResponseBytes)
	if err != nil {
		if _, tooLarge := err.(core.BoundedFrameErrorTooLarge); tooLarge {
			return nil, false, GrokUsageError{Kind: GrokUsageErrorProtocol, Detail: "response frame is too large"}
		}
		return nil, false, GrokUsageError{Kind: GrokUsageErrorProtocol, Detail: err.Error()}
	}
	switch frame := frame.(type) {
	case core.BoundedFrameLine:
		return frame.Bytes, true, nil
	case core.BoundedFrameTruncated:
		return frame.Bytes, true, nil
	}
	return nil, false, nil
}

// GrokUsageParse projects Grok Build's billing response into one quota
// window.
//
// The top level is snake_case, but `config` is camelCase, matching the two
// serde shapes in grok-build. proto3 JSON omits zero scalars, so an absent
// `creditUsagePercent` means zero usage rather than missing data. The
// deprecated `monthlyLimit`/`used`/`billingPeriod*` fields are read only
// when their replacements are absent, exactly as grok's own struct
// documents. The error is a GrokUsageError.
//
// result is a value decoded by jsonvalue.Parse.
func GrokUsageParse(result any) (GrokUsageReport, error) {
	config, ok := jvGet(result, "config")
	if !ok || config == nil {
		return GrokUsageReport{}, GrokUsageError{Kind: GrokUsageErrorNoData}
	}
	periodType, _ := jvStr(jvPointer(config, "currentPeriod", "type"))
	_, hasCurrentPeriod := jvGet(config, "currentPeriod")
	var periodLabel string
	switch {
	case strings.HasSuffix(periodType, "WEEKLY"):
		periodLabel = "Week"
	case strings.HasSuffix(periodType, "MONTHLY") || !hasCurrentPeriod:
		// The legacy shape only ever described a monthly credit budget.
		periodLabel = "Month"
	default:
		periodLabel = "Period"
	}
	usedPercent, ok := jvF64(jvGet(config, "creditUsagePercent"))
	if !ok {
		// Deprecated fallback: derive the share from the cent amounts.
		usedPercent = 0
		if limit, ok := grokUsageCents(jvGet(config, "monthlyLimit")); ok && limit > 0 {
			used, _ := grokUsageCents(jvGet(config, "used"))
			usedPercent = float64(used) * 100.0 / float64(limit)
		}
	}
	usedPercent = grokClamp(usedPercent, 0, 100)
	end, ok := jvPointer(config, "currentPeriod", "end")
	if !ok {
		end, ok = jvGet(config, "billingPeriodEnd")
	}
	var resetsAt *int64
	if text, ok := jvStr(end, ok); ok {
		if seconds, ok := grokUsageEpochSeconds(text); ok {
			resetsAt = &seconds
		}
	}
	return GrokUsageReport{PeriodLabel: periodLabel, UsedPercent: usedPercent, ResetsAt: resetsAt}, nil
}

// grokClamp is Rust's `f64::clamp`: NaN stays NaN.
func grokClamp(value, low, high float64) float64 {
	if value < low {
		return low
	}
	if value > high {
		return high
	}
	return value
}

// grokUsageCents reads a proto3 `Cent` message. `{}` is a valid zero, and so
// is an absent field; ok is false only when value is absent.
func grokUsageCents(value any, present bool) (int64, bool) {
	if !present {
		return 0, false
	}
	cents, ok := jvI64(jvGet(value, "val"))
	if !ok {
		return 0, true
	}
	return cents, true
}

func grokUsageEpochSeconds(timestamp string) (int64, bool) {
	moment, ok := core.ParseChronoRFC3339(timestamp)
	if !ok {
		return 0, false
	}
	return moment.Unix(), true
}
