// Port of core 1.99.1 src/time.rs (impl fmt::Debug for Duration)

package rstime

import (
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Duration's Debug output has one implementation, the formatter's
// (go/rs/fmt_time.go), which prints a time.Duration with Rust's algorithm
// wherever one is formatted: `rs.Format("{:?}", d)`, `{:.2?}`, `{:>9?}` and
// the rest. The functions here are that, under Rust's names, for code that
// calls Debug directly.

// DurationDebug is `format!("{d:?}")`: the duration in the largest unit that
// keeps its whole part non-zero, as "1.5s", "250ms", "7.1µs" or "0ns".
func DurationDebug(d time.Duration) string {
	check(d)
	return rs.DebugString(d)
}

// DurationFmtDebug is `fmt::Debug::fmt(&d, f)`: it writes the duration to f
// under f's own width, precision, fill, alignment and sign.
func DurationFmtDebug(d time.Duration, f *rs.Formatter) error {
	check(d)
	return rs.DebugFmt(d, f)
}
