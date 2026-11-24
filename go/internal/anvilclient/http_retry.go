// Port of brokk-anvil-client 0.28.6 src/http_retry.rs (the part Mjolnir uses).

package anvilclient

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"math/rand/v2"
	"net/http"
	"strconv"
	"strings"
	"time"
)

const LlmMaxAttempts uint64 = 4

const (
	llmGatewayTransientMaxAttempts uint64 = 12
	requestRetryBaseDelay                 = 200 * time.Millisecond
	gatewayTransientRetryBaseDelay        = time.Second
	gatewayTransientRetryMaxDelay         = 45 * time.Second
	noResponseHeadersWithin               = "request produced no response headers within"
	dailyQuotaMarker                      = "-tpd:"
)

// LlmRetryTier is how patiently a transient failure is retried.
type LlmRetryTier int

const (
	LlmRetryTierFast LlmRetryTier = iota
	LlmRetryTierGatewayTransient
)

func (t LlmRetryTier) MaxAttempts() uint64 {
	if t == LlmRetryTierGatewayTransient {
		return llmGatewayTransientMaxAttempts
	}
	return LlmMaxAttempts
}

// String is the Rust Debug name.
func (t LlmRetryTier) String() string {
	if t == LlmRetryTierGatewayTransient {
		return "GatewayTransient"
	}
	return "Fast"
}

// RetryableLlmError marks a failure that is worth retrying, and how
// patiently.
type RetryableLlmError struct {
	tier   LlmRetryTier
	reason string
}

func NewRetryableLlmError(tier LlmRetryTier, reason string) *RetryableLlmError {
	return &RetryableLlmError{tier: tier, reason: reason}
}

func RetryableLlmErrorFast(reason string) *RetryableLlmError {
	return NewRetryableLlmError(LlmRetryTierFast, reason)
}

func RetryableLlmErrorGatewayTransient(reason string) *RetryableLlmError {
	return NewRetryableLlmError(LlmRetryTierGatewayTransient, reason)
}

func (e *RetryableLlmError) Tier() LlmRetryTier { return e.tier }

func (e *RetryableLlmError) Error() string {
	return fmt.Sprintf("retryable LLM error (%v): %s", e.tier, e.reason)
}

// fatalLlmQuotaError is a provider quota that cannot clear before the run
// ends.
//
// A per-day token quota does not reset on any timescale a retry loop can
// wait out, so treating its 429 as "rate limited, back off and try again"
// burns the whole retry budget and then hands the caller a failure that is
// indistinguishable from a transient one. This marker says it.
type fatalLlmQuotaError struct {
	quota string
}

func (e *fatalLlmQuotaError) Error() string {
	return fmt.Sprintf("provider daily token quota exhausted (%s); this quota resets on a day boundary, so retrying cannot recover it -- failing the run instead of continuing without a model", e.quota)
}

func isFatalLlmQuotaError(err error) bool {
	var fatal *fatalLlmQuotaError
	return errors.As(err, &fatal)
}

// contextError is anyhow's `.context(message)`: Error prints the context
// before its source, as Rust's alternate Display (`{:#}`) does.
type contextError struct {
	message string
	source  error
}

func (e *contextError) Error() string { return e.message + ": " + e.source.Error() }
func (e *contextError) Unwrap() error { return e.source }

func withContext(err error, message string) error {
	return &contextError{message: message, source: err}
}

// markedError is anyhow's `error.context(marker)`: the marker printed
// before the error, both reachable through errors.As.
type markedError struct {
	marker error
	source error
}

func (e *markedError) Error() string   { return e.marker.Error() + ": " + e.source.Error() }
func (e *markedError) Unwrap() []error { return []error{e.marker, e.source} }

// dailyQuotaID extracts the daily-quota identifier from a provider
// rejection body, if it names one, e.g.
// `input-tpd:842609633142:openai.gpt-5.6-sol`.
func dailyQuotaID(body string) (string, bool) {
	marker := strings.Index(body, dailyQuotaMarker)
	if marker < 0 {
		return "", false
	}
	isIDChar := func(c byte) bool {
		return c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' ||
			c == '-' || c == '_' || c == '.' || c == ':'
	}
	// Walk back over the metric prefix and forward over the account/model
	// suffix, stopping at whatever delimiter the provider used.
	start := marker
	for start > 0 && isIDChar(body[start-1]) {
		start--
	}
	end := marker
	for end < len(body) && isIDChar(body[end]) {
		end++
	}
	return body[start:end], true
}

// fatalDailyQuotaError classifies a rejection body as a fatal daily-quota
// exhaustion, or nil if it names no per-day quota. Checked ahead of every
// retryable classification so no status code or body marker can launder it
// back into a retry.
func fatalDailyQuotaError(message, body string) error {
	quota, ok := dailyQuotaID(body)
	if !ok {
		return nil
	}
	return withContext(&fatalLlmQuotaError{quota: quota}, message)
}

func RetryableLlmErrorWith(message string, marker *RetryableLlmError) error {
	return withContext(marker, message)
}

func retryableLlmContext(err error, context string, marker *RetryableLlmError) error {
	return withContext(&markedError{marker: marker, source: err}, context)
}

func retryableLlmErrorForBody(message, body string) error {
	if err := fatalDailyQuotaError(message, body); err != nil {
		return err
	}
	if containsGatewayTransientMarker(body) {
		return RetryableLlmErrorWith(message, RetryableLlmErrorGatewayTransient("gateway transient response body"))
	}
	if containsStandardTransientMarker(body) {
		return RetryableLlmErrorWith(message, RetryableLlmErrorGatewayTransient("standard transient response body"))
	}
	return errors.New(message)
}

// retryableLlmErrorForStatusAndBody classifies a failed HTTP exchange,
// preferring the status code over the response body. A 5xx or 429 is
// retryable by definition, whatever prose the provider puts in the body.
// The one thing that outranks the status is a per-day quota id in the body.
func retryableLlmErrorForStatusAndBody(message string, status int, body string) error {
	if err := fatalDailyQuotaError(message, body); err != nil {
		return err
	}
	if status >= 500 && status <= 599 {
		return RetryableLlmErrorWith(message, RetryableLlmErrorGatewayTransient("server error status"))
	}
	if status == http.StatusTooManyRequests {
		return RetryableLlmErrorWith(message, RetryableLlmErrorGatewayTransient("rate limited status"))
	}
	return retryableLlmErrorForBody(message, body)
}

func RetryableLlmErrorForResponsesFailure(message, failure string) error {
	if err := fatalDailyQuotaError(message, failure); err != nil {
		return err
	}
	if containsStandardTransientMarker(failure) {
		// These markers are the body-borne equivalent of a 5xx/429 status,
		// so they earn the same patient tier the status path already gets.
		return RetryableLlmErrorWith(message, RetryableLlmErrorGatewayTransient("Responses stream transient failure"))
	}
	return errors.New(message)
}

func containsGatewayTransientMarker(message string) bool {
	for _, marker := range []string{
		"JSON-RPC error -32602",
		"Job registration failed",
		"Task submission failed",
		"Engine not found",
	} {
		if strings.Contains(message, marker) {
			return true
		}
	}
	return false
}

// containsStandardTransientMarker: body markers that mean the same thing a
// 5xx or a 429 status means: an overload or a throttle on the provider's
// side, never a defect in the request.
func containsStandardTransientMarker(message string) bool {
	lower := asciiLower(message)
	for _, marker := range []string{
		"server_error",
		"server_is_overloaded",
		"rate_limit_exceeded",
		"internal server error",
	} {
		if strings.Contains(lower, marker) {
			return true
		}
	}
	return false
}

// asciiLower is Rust's `to_ascii_lowercase`.
func asciiLower(text string) string {
	return strings.Map(func(character rune) rune {
		if character >= 'A' && character <= 'Z' {
			return character + ('a' - 'A')
		}
		return character
	}, text)
}

// errNoResponseHeaders cancels an attempt whose headers did not arrive in
// time.
var errNoResponseHeaders = errors.New("no response headers")

// cancelOnClose cancels a request's context once its body is closed, so a
// per-attempt context lives exactly as long as the response it carries.
type cancelOnClose struct {
	io.ReadCloser
	cancel context.CancelCauseFunc
}

func (b *cancelOnClose) Close() error {
	err := b.ReadCloser.Close()
	b.cancel(nil)
	return err
}

// sendWithRetries retries a request until it either succeeds, fails
// deterministically, or exhausts the Codex-compatible retry budget. This is
// intentionally limited to the pre-stream HTTP exchange: once an SSE body
// starts producing tokens, replaying here could duplicate user-visible
// output. cancel is the caller's cancellation, or nil for none;
// requestTimeout bounds each attempt's wait for response headers, or 0 for
// no bound.
func sendWithRetries(
	cancel context.Context,
	operation string,
	client *http.Client,
	makeRequest func(ctx context.Context) (*http.Request, error),
	requestTimeout time.Duration,
) (*http.Response, error) {
	parent := cancel
	if parent == nil {
		parent = context.Background()
	}
	for attempt := uint64(1); attempt <= LlmMaxAttempts; attempt++ {
		attemptCtx, cancelAttempt := context.WithCancelCause(parent)
		request, err := makeRequest(attemptCtx)
		if err != nil {
			cancelAttempt(nil)
			return nil, withContext(err, operation)
		}
		var timer *time.Timer
		if requestTimeout > 0 {
			timer = time.AfterFunc(requestTimeout, func() { cancelAttempt(errNoResponseHeaders) })
		}
		response, err := client.Do(request)
		if timer != nil {
			timer.Stop()
		}
		if err != nil {
			cause := context.Cause(attemptCtx)
			cancelAttempt(nil)
			switch {
			case cancel != nil && cancel.Err() != nil:
				return nil, fmt.Errorf("%s was cancelled while sending request", operation)
			case errors.Is(cause, errNoResponseHeaders):
				err = fmt.Errorf("%s %s", noResponseHeadersWithin, DurationDebug(requestTimeout))
			default:
				err = withContext(err, operation)
			}
			if attempt < LlmMaxAttempts && isRetryableSendError(err) {
				if err := sleepBeforeRetry(cancel, operation, attempt, err.Error()); err != nil {
					return nil, err
				}
				continue
			}
			return nil, err
		}
		response.Body = &cancelOnClose{ReadCloser: response.Body, cancel: cancelAttempt}
		if isRetryableStatus(response.StatusCode) && attempt < LlmMaxAttempts {
			response.Body.Close()
			if err := sleepBeforeRetry(cancel, operation, attempt, "HTTP "+httpStatusText(response.StatusCode)); err != nil {
				return nil, err
			}
			continue
		}
		return response, nil
	}
	panic("retry loop always returns on the last attempt")
}

func isRetryableStatus(status int) bool {
	return status == http.StatusRequestTimeout || status == http.StatusTooManyRequests || status >= 500 && status <= 599
}

// isRetryableSendError: every transport failure is worth another attempt
// except a redirect loop, as reqwest's builder, redirect, status and decode
// errors are not.
func isRetryableSendError(err error) bool {
	text := err.Error()
	if strings.Contains(text, noResponseHeadersWithin) {
		return true
	}
	redirectLoop := strings.Contains(text, "stopped after") && strings.Contains(text, "redirects")
	return !redirectLoop
}

// httpStatusText is reqwest's StatusCode Display: the code and its reason
// phrase, as `404 Not Found`.
func httpStatusText(status int) string {
	if reason := http.StatusText(status); reason != "" {
		return strconv.Itoa(status) + " " + reason
	}
	return strconv.Itoa(status)
}

func sleepBeforeRetry(cancel context.Context, operation string, attempt uint64, reason string) error {
	return SleepBeforeRetryForTier(cancel, operation, LlmRetryTierFast, attempt, reason)
}

// SleepBeforeRetryForTier waits out one retry's backoff, or fails when
// cancel (which may be nil) is cancelled first.
func SleepBeforeRetryForTier(cancel context.Context, operation string, tier LlmRetryTier, attempt uint64, reason string) error {
	delay := RetryBackoffForTier(tier, attempt)
	slog.Warn(fmt.Sprintf("%s failed (%s); retrying request (%d/%d) in %s",
		operation, reason, attempt, tier.MaxAttempts(), DurationDebug(delay)))
	timer := time.NewTimer(delay)
	defer timer.Stop()
	if cancel == nil {
		<-timer.C
		return nil
	}
	select {
	case <-cancel.Done():
		return fmt.Errorf("%s cancelled while waiting to retry", operation)
	case <-timer.C:
		return nil
	}
}

func RetryBackoffForTier(tier LlmRetryTier, attempt uint64) time.Duration {
	if tier == LlmRetryTierGatewayTransient {
		return gatewayTransientRetryBackoff(attempt)
	}
	return RetryBackoff(attempt)
}

// retryExponent is Rust's `2u64.saturating_pow(attempt.saturating_sub(1))`.
func retryExponent(attempt uint64) uint64 {
	if attempt == 0 {
		return 1
	}
	if attempt-1 >= 64 {
		return ^uint64(0)
	}
	return 1 << (attempt - 1)
}

func jitteredMillis(raw uint64) time.Duration {
	jitter := 0.9 + rand.Float64()*0.2
	return time.Duration(uint64(float64(raw)*jitter)) * time.Millisecond
}

func RetryBackoff(attempt uint64) time.Duration {
	exp := retryExponent(attempt)
	base := uint64(requestRetryBaseDelay / time.Millisecond)
	raw := base * exp
	if exp != 0 && raw/exp != base {
		raw = ^uint64(0)
	}
	return jitteredMillis(raw)
}

func gatewayTransientRetryBackoff(attempt uint64) time.Duration {
	exp := retryExponent(attempt)
	base := uint64(gatewayTransientRetryBaseDelay / time.Millisecond)
	raw := base * exp
	if exp != 0 && raw/exp != base {
		raw = ^uint64(0)
	}
	raw = min(raw, uint64(gatewayTransientRetryMaxDelay/time.Millisecond))
	return jitteredMillis(raw)
}

// DurationDebug formats a duration as Rust's Debug for Duration does:
// `120s`, `1.5s`, `200ms`, `5µs`, `0ns`.
func DurationDebug(duration time.Duration) string {
	if duration < 0 {
		duration = 0
	}
	nanos := uint64(duration)
	const (
		perSecond = uint64(time.Second)
		perMilli  = uint64(time.Millisecond)
		perMicro  = uint64(time.Microsecond)
	)
	decimal := func(integer, fraction, divisor uint64, suffix string) string {
		text := strconv.FormatUint(integer, 10)
		if fraction != 0 {
			digits := ""
			for divisor > 0 && fraction > 0 {
				digits += strconv.FormatUint(fraction/divisor, 10)
				fraction %= divisor
				divisor /= 10
			}
			text += "." + strings.TrimRight(digits, "0")
		}
		return text + suffix
	}
	switch {
	case nanos >= perSecond:
		return decimal(nanos/perSecond, nanos%perSecond, perSecond/10, "s")
	case nanos >= perMilli:
		return decimal(nanos/perMilli, nanos%perMilli, perMilli/10, "ms")
	case nanos >= perMicro:
		return decimal(nanos/perMicro, nanos%perMicro, perMicro/10, "µs")
	}
	return decimal(nanos, 0, 1, "ns")
}
