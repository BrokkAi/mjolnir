// Port of mj-core/src/retry_after.rs.

// The Retry-After header of an HTTP reply.

package core

import (
	"net/http"
	"net/mail"
	"strconv"
	"strings"
	"time"
)

// ParseRetryAfter reads a Retry-After value: a number of seconds, or an HTTP
// date. A date that has passed, or anything else, says nothing.
func ParseRetryAfter(value string, now time.Time) (time.Duration, bool) {
	value = strings.TrimSpace(value)
	if seconds, err := strconv.ParseUint(value, 10, 64); err == nil {
		return time.Duration(seconds) * time.Second, true
	}
	// chrono's parse_from_rfc2822 accepts the RFC 5322 date forms.
	date, err := mail.ParseDate(value)
	if err != nil {
		return 0, false
	}
	wait := date.Sub(now)
	if wait < 0 {
		return 0, false
	}
	return wait, true
}

// RetryAfterHeader reads the Retry-After header of an HTTP reply.
func RetryAfterHeader(headers http.Header) (time.Duration, bool) {
	values := headers.Values("Retry-After")
	if len(values) == 0 {
		return 0, false
	}
	return ParseRetryAfter(values[0], time.Now())
}
