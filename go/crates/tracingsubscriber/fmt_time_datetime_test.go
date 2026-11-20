// Port of tracing-subscriber 0.3.23 src/fmt/time/datetime.rs

package tracingsubscriber

import (
	"math"
	"strings"
	"testing"
	"time"
)

func TestFmtTimeDatetime_test_datetime(t *testing.T) {
	cases := func(expected string, secs int64, micros uint32) {
		t.Helper()
		// The crate builds a SystemTime from the seconds; the conversion
		// reads them back as seconds and nanoseconds, which is what is
		// given here.
		if got := dateTimeFromUnix(secs, micros*1_000).String(); got != expected {
			t.Errorf("secs: %d, micros: %d\n  left: %q\n right: %q", secs, micros, expected, got)
		}
	}

	// Mostly generated with:
	//  - date -jur <secs> +"%Y-%m-%dT%H:%M:%S.000000Z"
	//  - http://unixtimestamp.50x.eu/

	cases("1970-01-01T00:00:00.000000Z", 0, 0)
	cases("1970-01-01T00:00:00.000001Z", 0, 1)
	cases("1970-01-01T00:00:00.500000Z", 0, 500_000)
	cases("1970-01-01T00:00:01.000001Z", 1, 1)
	cases("1970-01-01T00:01:01.000001Z", 60+1, 1)
	cases("1970-01-01T01:01:01.000001Z", 60*60+60+1, 1)
	cases("1970-01-02T01:01:01.000001Z", 24*60*60+60*60+60+1, 1)

	cases("1969-12-31T23:59:59.000000Z", -1, 0)
	cases("1969-12-31T23:59:59.000001Z", -1, 1)
	cases("1969-12-31T23:59:59.500000Z", -1, 500_000)
	cases("1969-12-31T23:58:59.000001Z", -60-1, 1)
	cases("1969-12-31T22:58:59.000001Z", -60*60-60-1, 1)
	cases("1969-12-30T22:58:59.000001Z", -24*60*60-60*60-60-1, 1)

	cases("2038-01-19T03:14:07.000000Z", math.MaxInt32, 0)
	cases("2038-01-19T03:14:08.000000Z", math.MaxInt32+1, 0)
	cases("1901-12-13T20:45:52.000000Z", math.MinInt32, 0)
	cases("1901-12-13T20:45:51.000000Z", math.MinInt32-1, 0)

	// Skipping these tests on windows as std::time::SystemTime range is low
	// on Windows compared with that of Unix which can cause the following
	// high date value tests to panic
	cases("+292277026596-12-04T15:30:07.000000Z", math.MaxInt64, 0)
	cases("+292277026596-12-04T15:30:06.000000Z", math.MaxInt64-1, 0)
	cases("-292277022657-01-27T08:29:53.000000Z", math.MinInt64+1, 0)

	cases("1900-01-01T00:00:00.000000Z", -2208988800, 0)
	cases("1899-12-31T23:59:59.000000Z", -2208988801, 0)
	cases("2345-06-07T08:09:01.000000Z", 11847456541, 0)

	// Skipping pre-1601 dates on Windows: as of Rust 1.94, SystemTime
	// subtraction panics when the result would be before the Windows
	// FILETIME epoch (1601-01-01). See Rust 1.94.0 compatibility notes.
	cases("1234-05-06T07:08:09.000000Z", -23215049511, 0)
	cases("0000-01-01T00:00:00.000000Z", -62167219200, 0)
	cases("-0001-12-31T23:59:59.000000Z", -62167219201, 0)
	cases("-1234-05-06T07:08:09.000000Z", -101097651111, 0)
	cases("-2345-06-07T08:09:01.000000Z", -136154620259, 0)
}

// The default timer writes the current UTC time with microseconds: what it
// writes falls between two readings of the clock taken around it.
func TestSystemTimeWritesNowAsRfc3339Microseconds(t *testing.T) {
	var buf strings.Builder
	before := time.Now().UTC().Truncate(time.Microsecond)
	if err := (FmtTimeSystemTime{}).FormatTime(FmtFormatWriterNew(&buf)); err != nil {
		t.Fatal(err)
	}
	after := time.Now().UTC()
	written, err := time.Parse("2006-01-02T15:04:05.000000Z", buf.String())
	if err != nil {
		t.Fatalf("%q does not have the form 2026-11-20T11:36:12.123456Z: %v", buf.String(), err)
	}
	assertTrue(t, !written.Before(before) && !written.After(after), buf.String())
	assertEq(t, buf.String(), dateTimeFromSystemTime(written).String())
}
