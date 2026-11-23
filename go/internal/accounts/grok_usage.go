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
	"math"
	"time"

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
func GrokUsageQuery(ctx context.Context, cwd string, env map[string]string) (GrokUsageReport, error) {
	panic("unported: mj-accounts/src/grok_usage.rs query")
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
}

func spawnGrokUsageSession(ctx context.Context, cwd string, env map[string]string) (*grokUsageSession, error) {
	panic("unported: mj-accounts/src/grok_usage.rs GrokUsageSession::spawn")
}

func (s *grokUsageSession) exchange(ctx context.Context) (GrokUsageReport, error) {
	panic("unported: mj-accounts/src/grok_usage.rs GrokUsageSession::exchange")
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
	panic("unported: mj-accounts/src/grok_usage.rs GrokUsageSession::read_result")
}

func (s *grokUsageSession) shutdown(ctx context.Context) {
	panic("unported: mj-accounts/src/grok_usage.rs GrokUsageSession::shutdown")
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
func GrokUsageParse(result any) (GrokUsageReport, error) {
	panic("unported: mj-accounts/src/grok_usage.rs parse")
}

// grokUsageCents reads a proto3 `Cent` message. `{}` is a valid zero, and so
// is an absent field; ok is false only when value is absent.
func grokUsageCents(value any, present bool) (int64, bool) {
	panic("unported: mj-accounts/src/grok_usage.rs cents")
}

func grokUsageEpochSeconds(timestamp string) (int64, bool) {
	moment, ok := core.ParseChronoRFC3339(timestamp)
	if !ok {
		return 0, false
	}
	return moment.Unix(), true
}
