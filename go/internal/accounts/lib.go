// Port of mj-accounts/src/lib.rs.

// Package accounts is Mjolnir's harness profile accounts: whether a profile
// is logged in, the terminal a login runs in, and each provider's quota and
// usage reader with the scheduling and health derived from them. It builds on
// the targets and store packages and knows nothing of sessions.
//
// This is a separate package so that the session runtime, the lifecycle
// operations and the daemon can each use it without rebuilding one another.
//
// The Rust crate's modules all live in this one package, one file per Rust
// module. Rust's crate-private re-exports (`mj_store::{database, upgrade}`,
// `mj_targets::{streamed_child, targets}`) have no Go equivalent: the files
// import package store and package targets directly.
package accounts

import (
	"context"
	"errors"
	"math"
	"net"
	"net/http"
	"strconv"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// The helpers below stand in for Rust library behaviour several modules of
// this crate rely on.

// reqwestStatusDisplay is the Display of an http::StatusCode, as reqwest
// errors and Mjolnir's messages print it: the code and its canonical
// reason, which for a few codes is worded differently from net/http's.
func reqwestStatusDisplay(status int) string {
	reason := http.StatusText(status)
	switch status {
	case http.StatusNonAuthoritativeInfo:
		reason = "Non Authoritative Information"
	case http.StatusRequestEntityTooLarge:
		reason = "Payload Too Large"
	case http.StatusRequestURITooLong:
		reason = "URI Too Long"
	case http.StatusRequestedRangeNotSatisfiable:
		reason = "Range Not Satisfiable"
	}
	if reason == "" {
		reason = "<unknown status code>"
	}
	return strconv.Itoa(status) + " " + reason
}

// accountsIsTimeout is reqwest's `Error::is_timeout`: the request's own
// deadline passed.
func accountsIsTimeout(err error) bool {
	if errors.Is(err, context.DeadlineExceeded) {
		return true
	}
	var timeout net.Error
	return errors.As(err, &timeout) && timeout.Timeout()
}

// The jv helpers read a value decoded by jsonvalue.Parse as serde_json's
// Value accessors do.

// jvGet is `Value::get(key)`: the member of an object, absent for any other
// value.
func jvGet(value any, key string) (any, bool) {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil, false
	}
	return object.Get(key)
}

// jvPointer is `Value::pointer` for a path of object keys and array
// indices.
func jvPointer(value any, path ...string) (any, bool) {
	for _, step := range path {
		switch current := value.(type) {
		case jsonvalue.Object:
			next, ok := current.Get(step)
			if !ok {
				return nil, false
			}
			value = next
		case []any:
			index, err := strconv.ParseUint(step, 10, 64)
			if err != nil || index >= uint64(len(current)) || (len(step) > 1 && step[0] == '0') {
				return nil, false
			}
			value = current[index]
		default:
			return nil, false
		}
	}
	return value, true
}

// jvStr is `Value::as_str`.
func jvStr(value any, present bool) (string, bool) {
	text, ok := value.(string)
	return text, present && ok
}

// jvU64 is `Value::as_u64`: a non-negative integer.
func jvU64(value any, present bool) (uint64, bool) {
	if !present {
		return 0, false
	}
	switch number := value.(type) {
	case int64:
		return uint64(number), number >= 0
	case uint64:
		return number, true
	}
	return 0, false
}

// jvI64 is `Value::as_i64`: an integer that fits i64.
func jvI64(value any, present bool) (int64, bool) {
	if !present {
		return 0, false
	}
	switch number := value.(type) {
	case int64:
		return number, true
	case uint64:
		return int64(number), number <= math.MaxInt64
	}
	return 0, false
}

// jvF64 is `Value::as_f64`: any number.
func jvF64(value any, present bool) (float64, bool) {
	if !present {
		return 0, false
	}
	switch number := value.(type) {
	case int64:
		return float64(number), true
	case uint64:
		return float64(number), true
	case float64:
		return number, true
	}
	return 0, false
}

// jvBool is `Value::as_bool`.
func jvBool(value any, present bool) (bool, bool) {
	flag, ok := value.(bool)
	return flag, present && ok
}

// saturatingU8 is Rust's `f64 as u8`: NaN becomes 0 and values outside the
// range saturate.
func saturatingU8(value float64) uint8 {
	switch {
	case math.IsNaN(value) || value <= 0:
		return 0
	case value >= math.MaxUint8:
		return math.MaxUint8
	}
	return uint8(value)
}
