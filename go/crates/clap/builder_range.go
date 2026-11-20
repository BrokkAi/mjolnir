// Port of clap_builder 4.6.0 src/builder/range.rs

package clap

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// BuilderValueRange is clap::builder::ValueRange: values per occurrence for
// an argument.
//
// A `usize` is an int here, and rs.UsizeMax stands for usize::MAX: an
// unbounded range ends there.
type BuilderValueRange struct {
	startInclusive int
	endInclusive   int
}

// BuilderValueRangeEmpty is ValueRange::EMPTY: nor argument values, or a
// flag.
var BuilderValueRangeEmpty = BuilderValueRange{startInclusive: 0, endInclusive: 0}

// BuilderValueRangeSingle is ValueRange::SINGLE: a single argument value,
// the most common case for options.
var BuilderValueRangeSingle = BuilderValueRange{startInclusive: 1, endInclusive: 1}

var builderValueRangeOptional = BuilderValueRange{startInclusive: 0, endInclusive: 1}

var builderValueRangeFull = BuilderValueRange{startInclusive: 0, endInclusive: rs.UsizeMax}

// BuilderValueRangeNew is ValueRange::new: create a range.
//
// Panics if the end is less than the start (debug builds).
func BuilderValueRangeNew(r BuilderValueRange) BuilderValueRange {
	return r
}

func builderValueRangeRaw(startInclusive, endInclusive int) BuilderValueRange {
	if debugAssertions && !(startInclusive <= endInclusive) {
		rs.PanicAt(2, "assertion failed: start_inclusive <= end_inclusive")
	}
	return BuilderValueRange{startInclusive: startInclusive, endInclusive: endInclusive}
}

// MinValues is ValueRange::min_values: fewest number of values the argument
// accepts.
func (r BuilderValueRange) MinValues() int { return r.startInclusive }

// MaxValues is ValueRange::max_values: most number of values the argument
// accepts.
func (r BuilderValueRange) MaxValues() int { return r.endInclusive }

// TakesValues is ValueRange::takes_values: report whether the argument takes
// any values (ie is a flag).
func (r BuilderValueRange) TakesValues() bool { return r.endInclusive != 0 }

func (r BuilderValueRange) isUnbounded() bool { return r.endInclusive == rs.UsizeMax }

func (r BuilderValueRange) isFixed() bool { return r.startInclusive == r.endInclusive }

func (r BuilderValueRange) isMultiple() bool {
	return r.startInclusive != r.endInclusive || 1 < r.startInclusive
}

func (r BuilderValueRange) numValues() rs.Option[int] {
	if r.isFixed() {
		return rs.Some(r.startInclusive)
	}
	return rs.None[int]()
}

func (r BuilderValueRange) acceptsMore(current int) bool {
	return current < r.endInclusive
}

// BuilderValueRangeDefault is `impl Default for ValueRange`.
func BuilderValueRangeDefault() BuilderValueRange { return BuilderValueRangeSingle }

// BuilderValueRangeFromUsize is `impl From<usize> for ValueRange`: exactly
// that many values.
func BuilderValueRangeFromUsize(fixed int) BuilderValueRange {
	return BuilderValueRangeFromRangeInclusive(fixed, fixed)
}

// BuilderValueRangeFromRange is `impl From<Range<usize>> for ValueRange`:
// `start..end`.
func BuilderValueRangeFromRange(start, end int) BuilderValueRange {
	startInclusive := start
	endInclusive := end - 1
	if end == 0 {
		endInclusive = 0
	}
	return builderValueRangeRaw(startInclusive, endInclusive)
}

// BuilderValueRangeFromRangeFull is `impl From<RangeFull> for ValueRange`:
// `..`.
func BuilderValueRangeFromRangeFull() BuilderValueRange {
	return builderValueRangeFull
}

// BuilderValueRangeFromRangeFrom is `impl From<RangeFrom<usize>> for
// ValueRange`: `start..`.
func BuilderValueRangeFromRangeFrom(start int) BuilderValueRange {
	startInclusive := start
	endInclusive := rs.UsizeMax
	return builderValueRangeRaw(startInclusive, endInclusive)
}

// BuilderValueRangeFromRangeTo is `impl From<RangeTo<usize>> for
// ValueRange`: `..end`.
func BuilderValueRangeFromRangeTo(end int) BuilderValueRange {
	startInclusive := 0
	endInclusive := end - 1
	if end == 0 {
		endInclusive = 0
	}
	return builderValueRangeRaw(startInclusive, endInclusive)
}

// BuilderValueRangeFromRangeInclusive is `impl From<RangeInclusive<usize>>
// for ValueRange`: `start..=end`.
func BuilderValueRangeFromRangeInclusive(start, end int) BuilderValueRange {
	startInclusive := start
	endInclusive := end
	return builderValueRangeRaw(startInclusive, endInclusive)
}

// BuilderValueRangeFromRangeToInclusive is `impl From<RangeToInclusive<usize>>
// for ValueRange`: `..=end`.
func BuilderValueRangeFromRangeToInclusive(end int) BuilderValueRange {
	startInclusive := 0
	endInclusive := end
	return builderValueRangeRaw(startInclusive, endInclusive)
}

// String is `impl Display for ValueRange`.
func (r BuilderValueRange) String() string {
	out := usizeDisplay(r.startInclusive)
	if r.isFixed() {
	} else if r.endInclusive == rs.UsizeMax {
		out += ".."
	} else {
		out += "..="
		out += usizeDisplay(r.endInclusive)
	}
	return out
}

// FmtDisplay is `impl Display for ValueRange`.
func (r BuilderValueRange) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(r.String()) }

// FmtDebug is `impl Debug for ValueRange`.
func (r BuilderValueRange) FmtDebug(f *rs.Formatter) error { return f.WriteStr(r.String()) }

// usizeDisplay is the Display form of a usize held as an int: rs.UsizeMax
// stands for usize::MAX and prints as that number does on a 64-bit target.
func usizeDisplay(n int) string {
	if n == rs.UsizeMax {
		return strconv.FormatUint(^uint64(0), 10)
	}
	return strconv.Itoa(n)
}
