// Port of mj-core/src/clock.rs.

// Small, dependency-free helpers for reading the wall clock as Unix epoch
// values. Kept free of other modules so anything can use it.

package core

import "time"

// EpochSeconds is the number of seconds since the Unix epoch. Reads before
// the epoch report as 0.
func EpochSeconds() uint64 {
	seconds := time.Now().Unix()
	if seconds < 0 {
		return 0
	}
	return uint64(seconds)
}

// EpochMillis is the number of milliseconds since the Unix epoch, saturating
// at math.MaxInt64 (time.Time.UnixMilli cannot exceed it). Reads before the
// epoch report as 0.
func EpochMillis() int64 {
	millis := time.Now().UnixMilli()
	if millis < 0 {
		return 0
	}
	return millis
}
