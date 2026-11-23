// Port of mj-core/src/github_rate_limit.rs.

// Reading GitHub's rate-limit refusals, shared by every GitHub API caller.

package core

import (
	"math"
	"net/http"
	"net/mail"
	"strconv"
	"time"
)

// GithubRateLimitRetryAt is, when a GitHub API response is a rate-limit
// refusal, the moment it may be retried; false when it is not one. Covers
// the primary limit (remaining requests exhausted), HTTP 429, and secondary
// limits, which GitHub signals with `Retry-After` while requests remain.
func GithubRateLimitRetryAt(status int, headers http.Header, now time.Time) (time.Time, bool) {
	var reset *time.Time
	if values := headers.Values("x-ratelimit-reset"); len(values) > 0 && jevVisibleASCII(values[0]) {
		if seconds, err := strconv.ParseInt(values[0], 10, 64); err == nil {
			at := time.Unix(seconds, 0).UTC()
			reset = &at
		}
	}
	var retryAfter *time.Time
	if values := headers.Values("Retry-After"); len(values) > 0 && jevVisibleASCII(values[0]) {
		value := values[0]
		if seconds, err := strconv.ParseUint(value, 10, 64); err == nil {
			// chrono::Duration::seconds of more than i64::MAX milliseconds
			// panics in Rust; a header that large is not a real deadline, so
			// it saturates here.
			wait := time.Duration(math.MaxInt64)
			if seconds < uint64(math.MaxInt64/int64(time.Second)) {
				wait = time.Duration(seconds) * time.Second
			}
			at := now.Add(wait)
			retryAfter = &at
		} else if date, err := mail.ParseDate(value); err == nil {
			at := date.UTC()
			retryAfter = &at
		}
	}
	exhausted := false
	if values := headers.Values("x-ratelimit-remaining"); len(values) > 0 && jevVisibleASCII(values[0]) {
		exhausted = values[0] == "0"
	}
	if !exhausted && status != http.StatusTooManyRequests && retryAfter == nil {
		return time.Time{}, false
	}
	var latest *time.Time
	for _, candidate := range []*time.Time{retryAfter, reset} {
		if candidate != nil && (latest == nil || candidate.After(*latest)) {
			latest = candidate
		}
	}
	if latest == nil {
		return now.Add(time.Minute), true
	}
	return *latest, true
}
