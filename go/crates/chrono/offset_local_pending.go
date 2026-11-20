package chrono

import "github.com/BrokkAi/mjolnir/go/rs"

// Staging only: replaced by the per-platform files before this package is
// finished.

func localInnerOffsetFromUtcDatetime(utc NaiveDateTime) MappedLocalTime[FixedOffset] {
	panic(rs.Todo("chrono::offset::local::inner::offset_from_utc_datetime"))
}

func localInnerOffsetFromLocalDatetime(local NaiveDateTime) MappedLocalTime[FixedOffset] {
	panic(rs.Todo("chrono::offset::local::inner::offset_from_local_datetime"))
}
