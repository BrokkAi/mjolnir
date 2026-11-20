// Port of chrono 0.4.44 src/offset/mod.rs

package chrono

// The time zone, which calculates offsets from the local time to UTC.
//
// There are four operations provided by the TimeZone trait:
//
// 1. Converting the local NaiveDateTime to DateTime<Tz>
// 2. Converting the UTC NaiveDateTime to DateTime<Tz>
// 3. Converting DateTime<Tz> to the local NaiveDateTime
// 4. Constructing DateTime<Tz> objects from various offsets
//
// 1 is used for constructors. 2 is used for the WithTimezone method of date and time types.
// 3 is used for other methods, e.g. Year() or Format(), and provided by an associated type
// which implements Offset (which then passed to TimeZone for actual implementations).
// Technically speaking TimeZone has a total knowledge about given timescale,
// but Offset is used as a cache to avoid the repeated conversion
// and provides implementations for 1 and 3.
// An TimeZone instance can be reconstructed from the corresponding Offset instance.

import "github.com/BrokkAi/mjolnir/go/rs"

// MappedLocalTime is the result of mapping a local time to a concrete
// instant in a given time zone (Rust's `MappedLocalTime<T>`, the enum
// `LocalResult<T>`).
//
// The calculation to go from a local time (wall clock time) to an instant in
// UTC can end up in three cases:
//   - A single, simple result (*MappedLocalTime_Single).
//   - An ambiguous result when the clock is turned backwards during a
//     transition due to for example DST (*MappedLocalTime_Ambiguous).
//   - No result when the clock is turned forwards during a transition due to
//     for example DST (*MappedLocalTime_None).
//
// When the clock is turned backwards it creates a *fold* in local time,
// during which the local time is *ambiguous*. When the clock is turned
// forwards it creates a *gap* in local time, during which the local time is
// *missing*, or does not exist.
//
// Chrono does not return a default choice or invalid data during time zone
// transitions, but has the MappedLocalTime type to help deal with the result
// correctly.
//
// A value is always a pointer to one of the three variant structs.
type MappedLocalTime[T any] interface {
	isMappedLocalTime()
	// Single is `MappedLocalTime::single`: Some if the conversion has a
	// single, unambiguous result.
	Single() rs.Option[T]
	// Earliest is `MappedLocalTime::earliest`: the earliest possible result
	// of the time zone mapping, None if local time falls in a gap.
	Earliest() rs.Option[T]
	// Latest is `MappedLocalTime::latest`: the latest possible result of the
	// time zone mapping, None if local time falls in a gap.
	Latest() rs.Option[T]
	// Unwrap is `MappedLocalTime::unwrap`: the single unique conversion
	// result, or a panic "No such local time" or "Ambiguous local time,
	// ranging from {earliest:?} to {latest:?}".
	Unwrap() T
}

// MappedLocalTime_Single is `MappedLocalTime::Single`: the local time maps to
// a single unique result.
type MappedLocalTime_Single[T any] struct{ F0 T }

// MappedLocalTime_Ambiguous is `MappedLocalTime::Ambiguous`: the local time
// is ambiguous because there is a fold in the local time. It holds all
// possible results, from the earliest to the latest one.
type MappedLocalTime_Ambiguous[T any] struct {
	F0 T
	F1 T
}

// MappedLocalTime_None is `MappedLocalTime::None`: the local time does not
// exist because there is a gap in the local time. It is also returned to
// represent some other error, such as an out-of-range result.
type MappedLocalTime_None[T any] struct{}

// LocalResult is the old name of MappedLocalTime, which is still the enum's
// own name in Rust.
type LocalResult[T any] = MappedLocalTime[T]

// LocalResult_Single is `LocalResult::Single`.
type LocalResult_Single[T any] = MappedLocalTime_Single[T]

// LocalResult_Ambiguous is `LocalResult::Ambiguous`.
type LocalResult_Ambiguous[T any] = MappedLocalTime_Ambiguous[T]

// LocalResult_None is `LocalResult::None`.
type LocalResult_None[T any] = MappedLocalTime_None[T]

var _ = rs.RegisterEnum[MappedLocalTime[DateTime]]("LocalResult",
	rs.Variant[MappedLocalTime_Single[DateTime]]("Single", rs.ShapeTuple),
	rs.Variant[MappedLocalTime_Ambiguous[DateTime]]("Ambiguous", rs.ShapeTuple),
	rs.Variant[MappedLocalTime_None[DateTime]]("None", rs.ShapeUnit),
)

func (*MappedLocalTime_Single[T]) isMappedLocalTime()    {}
func (*MappedLocalTime_Ambiguous[T]) isMappedLocalTime() {}
func (*MappedLocalTime_None[T]) isMappedLocalTime()      {}

// Single is `MappedLocalTime::single`.
func (m *MappedLocalTime_Single[T]) Single() rs.Option[T] { return rs.Some(m.F0) }

// Single is `MappedLocalTime::single`.
func (m *MappedLocalTime_Ambiguous[T]) Single() rs.Option[T] { return rs.None[T]() }

// Single is `MappedLocalTime::single`.
func (m *MappedLocalTime_None[T]) Single() rs.Option[T] { return rs.None[T]() }

// Earliest is `MappedLocalTime::earliest`.
func (m *MappedLocalTime_Single[T]) Earliest() rs.Option[T] { return rs.Some(m.F0) }

// Earliest is `MappedLocalTime::earliest`.
func (m *MappedLocalTime_Ambiguous[T]) Earliest() rs.Option[T] { return rs.Some(m.F0) }

// Earliest is `MappedLocalTime::earliest`.
func (m *MappedLocalTime_None[T]) Earliest() rs.Option[T] { return rs.None[T]() }

// Latest is `MappedLocalTime::latest`.
func (m *MappedLocalTime_Single[T]) Latest() rs.Option[T] { return rs.Some(m.F0) }

// Latest is `MappedLocalTime::latest`.
func (m *MappedLocalTime_Ambiguous[T]) Latest() rs.Option[T] { return rs.Some(m.F1) }

// Latest is `MappedLocalTime::latest`.
func (m *MappedLocalTime_None[T]) Latest() rs.Option[T] { return rs.None[T]() }

// MappedLocalTimeMap is `MappedLocalTime::map`: the result with f applied to
// each value it holds.
func MappedLocalTimeMap[T, U any](m MappedLocalTime[T], f func(T) U) MappedLocalTime[U] {
	switch m := m.(type) {
	case *MappedLocalTime_None[T]:
		return &MappedLocalTime_None[U]{}
	case *MappedLocalTime_Single[T]:
		return &MappedLocalTime_Single[U]{F0: f(m.F0)}
	case *MappedLocalTime_Ambiguous[T]:
		min := f(m.F0)
		max := f(m.F1)
		return &MappedLocalTime_Ambiguous[U]{F0: min, F1: max}
	default:
		panic(rs.Unreachable(m))
	}
}

// mappedLocalTimeAndThen is `MappedLocalTime::and_then`: the result with f
// applied to each value it holds, and None as soon as f returns None.
func mappedLocalTimeAndThen[T, U any](m MappedLocalTime[T], f func(T) rs.Option[U]) MappedLocalTime[U] {
	switch m := m.(type) {
	case *MappedLocalTime_None[T]:
		return &MappedLocalTime_None[U]{}
	case *MappedLocalTime_Single[T]:
		if value, ok := f(m.F0).Get(); ok {
			return &MappedLocalTime_Single[U]{F0: value}
		}
		return &MappedLocalTime_None[U]{}
	case *MappedLocalTime_Ambiguous[T]:
		min, minOk := f(m.F0).Get()
		max, maxOk := f(m.F1).Get()
		if minOk && maxOk {
			return &MappedLocalTime_Ambiguous[U]{F0: min, F1: max}
		}
		return &MappedLocalTime_None[U]{}
	default:
		panic(rs.Unreachable(m))
	}
}

// Unwrap is `MappedLocalTime::unwrap`.
func (m *MappedLocalTime_Single[T]) Unwrap() T { return m.F0 }

// Unwrap is `MappedLocalTime::unwrap`. It panics: the local time is
// ambiguous.
func (m *MappedLocalTime_Ambiguous[T]) Unwrap() T {
	rs.PanicAt(1, "Ambiguous local time, ranging from "+debugString(m.F0)+" to "+debugString(m.F1))
	panic("unreachable")
}

// Unwrap is `MappedLocalTime::unwrap`. It panics: there is no such local
// time.
func (m *MappedLocalTime_None[T]) Unwrap() T {
	rs.PanicAt(1, "No such local time")
	panic("unreachable")
}

// Offset is the offset from the local time to UTC (Rust's `Offset` trait).
//
// Utc, FixedOffset and chronotz.TzOffset implement it. Rust requires an
// offset to be Debug, and formatting a DateTime also needs it to be Display;
// both forms are part of the Go interface, as text and as formatter methods.
type Offset interface {
	// Fix returns the fixed offset from UTC to the local time stored.
	Fix() FixedOffset
	// String is the Display form, which `%Z` prints: "UTC" for Utc, the
	// numeric offset "+09:00" for a FixedOffset (and so for Local), and the
	// zone abbreviation for a chronotz.TzOffset.
	String() string
	// DebugString is the Debug form, which the Debug form of a DateTime ends
	// with: "Z" for Utc.
	DebugString() string
	// FmtDisplay is the offset's `fmt::Display` impl.
	FmtDisplay(f *rs.Formatter) error
	// FmtDebug is the offset's `fmt::Debug` impl.
	FmtDebug(f *rs.Formatter) error
}

// TimeZone is the time zone (Rust's `TimeZone` trait): Utc, Local,
// FixedOffset and chronotz.Tz implement it.
//
// The methods of this trait are primarily constructors for the DateTime
// type. In Rust the zone is DateTime's type parameter; in Go a DateTime
// carries its zone as a value of this interface (see DateTime).
//
// The trait's provided methods are part of the interface; an implementation
// that keeps the default body calls the matching TimeZoneDefault function.
// The deprecated methods that build the deprecated `Date<Tz>` type (`ymd`,
// `yo`, `isoywd`, `from_local_date`, `from_utc_date`) and the deprecated
// `timestamp`, `timestamp_millis` and `datetime_from_str` are not ported.
type TimeZone interface {
	// WithYmdAndHms makes a new DateTime from year, month, day, time
	// components and current time zone.
	//
	// This assumes the proleptic Gregorian calendar, with the year 0 being 1
	// BCE.
	//
	// It returns *MappedLocalTime_None on invalid input data.
	WithYmdAndHms(year int32, month, day, hour, min, sec uint32) MappedLocalTime[DateTime]

	// TimestampOpt makes a new DateTime from the number of non-leap seconds
	// since January 1, 1970 0:00:00 UTC (aka "UNIX timestamp") and the number
	// of nanoseconds since the last whole non-leap second.
	//
	// The nanosecond part can exceed 1,000,000,000 in order to represent a
	// leap second, but only when `secs % 60 == 59`. (The true "UNIX
	// timestamp" cannot represent a leap second unambiguously.)
	//
	// It returns *MappedLocalTime_None on out-of-range number of seconds
	// and/or invalid nanosecond, otherwise always returns
	// *MappedLocalTime_Single.
	TimestampOpt(secs int64, nsecs uint32) MappedLocalTime[DateTime]

	// TimestampMillisOpt makes a new DateTime from the number of non-leap
	// milliseconds since January 1, 1970 0:00:00 UTC (aka "UNIX timestamp").
	//
	// It returns *MappedLocalTime_None on out-of-range number of
	// milliseconds, otherwise always returns *MappedLocalTime_Single.
	TimestampMillisOpt(millis int64) MappedLocalTime[DateTime]

	// TimestampNanos makes a new DateTime from the number of non-leap
	// nanoseconds since January 1, 1970 0:00:00 UTC (aka "UNIX timestamp").
	//
	// Unlike TimestampMillisOpt, this never fails.
	TimestampNanos(nanos int64) DateTime

	// TimestampMicros makes a new DateTime from the number of non-leap
	// microseconds since January 1, 1970 0:00:00 UTC (aka "UNIX timestamp").
	TimestampMicros(micros int64) MappedLocalTime[DateTime]

	// FromOffset reconstructs the time zone from the offset. In Rust it is an
	// associated function of the zone's type; in Go it is called on any
	// value of that type.
	FromOffset(offset Offset) TimeZone

	// OffsetFromLocalDate creates the offset(s) for given local NaiveDate if
	// possible.
	OffsetFromLocalDate(local NaiveDate) MappedLocalTime[Offset]

	// OffsetFromLocalDatetime creates the offset(s) for given local
	// NaiveDateTime if possible.
	OffsetFromLocalDatetime(local NaiveDateTime) MappedLocalTime[Offset]

	// FromLocalDatetime converts the local NaiveDateTime to the timezone-aware
	// DateTime if possible.
	FromLocalDatetime(local NaiveDateTime) MappedLocalTime[DateTime]

	// OffsetFromUtcDate creates the offset for given UTC NaiveDate. This
	// cannot fail.
	OffsetFromUtcDate(utc NaiveDate) Offset

	// OffsetFromUtcDatetime creates the offset for given UTC NaiveDateTime.
	// This cannot fail.
	OffsetFromUtcDatetime(utc NaiveDateTime) Offset

	// FromUtcDatetime converts the UTC NaiveDateTime to the local time. The
	// UTC is continuous and thus this cannot fail (but can give the duplicate
	// local time).
	FromUtcDatetime(utc NaiveDateTime) DateTime
}

// TimeZoneDefaultWithYmdAndHms is the default body of
// `TimeZone::with_ymd_and_hms`.
func TimeZoneDefaultWithYmdAndHms(tz TimeZone, year int32, month, day, hour, min, sec uint32) MappedLocalTime[DateTime] {
	date, ok := NaiveDateFromYmdOpt(year, month, day).Get()
	if ok {
		if dt, ok := date.AndHmsOpt(hour, min, sec).Get(); ok {
			return tz.FromLocalDatetime(dt)
		}
	}
	return &MappedLocalTime_None[DateTime]{}
}

// TimeZoneDefaultTimestampOpt is the default body of
// `TimeZone::timestamp_opt`.
func TimeZoneDefaultTimestampOpt(tz TimeZone, secs int64, nsecs uint32) MappedLocalTime[DateTime] {
	if dt, ok := DateTimeFromTimestamp(secs, nsecs).Get(); ok {
		return &MappedLocalTime_Single[DateTime]{F0: tz.FromUtcDatetime(dt.NaiveUtc())}
	}
	return &MappedLocalTime_None[DateTime]{}
}

// TimeZoneDefaultTimestampMillisOpt is the default body of
// `TimeZone::timestamp_millis_opt`.
func TimeZoneDefaultTimestampMillisOpt(tz TimeZone, millis int64) MappedLocalTime[DateTime] {
	if dt, ok := DateTimeFromTimestampMillis(millis).Get(); ok {
		return &MappedLocalTime_Single[DateTime]{F0: tz.FromUtcDatetime(dt.NaiveUtc())}
	}
	return &MappedLocalTime_None[DateTime]{}
}

// TimeZoneDefaultTimestampNanos is the default body of
// `TimeZone::timestamp_nanos`.
func TimeZoneDefaultTimestampNanos(tz TimeZone, nanos int64) DateTime {
	return tz.FromUtcDatetime(DateTimeFromTimestampNanos(nanos).NaiveUtc())
}

// TimeZoneDefaultTimestampMicros is the default body of
// `TimeZone::timestamp_micros`.
func TimeZoneDefaultTimestampMicros(tz TimeZone, micros int64) MappedLocalTime[DateTime] {
	if dt, ok := DateTimeFromTimestampMicros(micros).Get(); ok {
		return &MappedLocalTime_Single[DateTime]{F0: tz.FromUtcDatetime(dt.NaiveUtc())}
	}
	return &MappedLocalTime_None[DateTime]{}
}

// TimeZoneDefaultFromLocalDatetime is the default body of
// `TimeZone::from_local_datetime`.
func TimeZoneDefaultFromLocalDatetime(tz TimeZone, local NaiveDateTime) MappedLocalTime[DateTime] {
	return mappedLocalTimeAndThen(tz.OffsetFromLocalDatetime(local), func(off Offset) rs.Option[DateTime] {
		dt, ok := local.CheckedSubOffset(off.Fix()).Get()
		if !ok {
			return rs.None[DateTime]()
		}
		return rs.Some(DateTimeFromNaiveUtcAndOffset(dt, off, tz))
	})
}

// TimeZoneDefaultFromUtcDatetime is the default body of
// `TimeZone::from_utc_datetime`.
func TimeZoneDefaultFromUtcDatetime(tz TimeZone, utc NaiveDateTime) DateTime {
	return DateTimeFromNaiveUtcAndOffset(utc, tz.OffsetFromUtcDatetime(utc), tz)
}
