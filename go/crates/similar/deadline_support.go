// Port of similar 2.7.0 src/deadline_support.rs

package similar

import (
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

// `Instant` is `std::time::Instant`, which is rstime.Instant.

// Checks if a deadline was exeeded.
func deadlineSupportDeadlineExceeded(deadline rs.Option[rstime.Instant]) bool {
	if deadline, ok := deadline.Get(); ok {
		return rstime.InstantNow().After(deadline)
	}
	return false
}

// Converst a duration into a deadline.  This can be a noop on wasm
func deadlineSupportDurationToDeadline(add time.Duration) rs.Option[rstime.Instant] {
	return rstime.InstantNow().CheckedAdd(add)
}
