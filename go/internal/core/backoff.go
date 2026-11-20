// Port of mj-core/src/backoff.rs.

// The retry delay shared by background session policies.

package core

import (
	"math"
	"time"
)

// BackoffDelay is the retry shape every background session policy uses:
// base after the first failure, doubling per consecutive failure, never past
// cap.
//
// Capped rather than unbounded because a target that is broken rather than
// blipping must keep being probed, just rarely enough to cost nothing.
func BackoffDelay(base, cap time.Duration, consecutiveFailures uint32) time.Duration {
	doublings := uint32(0)
	if consecutiveFailures > 0 {
		doublings = min(consecutiveFailures-1, 31)
	}
	factor := int64(1) << doublings
	if base > 0 && int64(base) > math.MaxInt64/factor {
		return cap
	}
	return min(base*time.Duration(factor), cap)
}
