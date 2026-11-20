// Port of semver 1.0.28 tests/test_identifier.rs

package semver_test

import (
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/semver"
	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestItTestIdentifier_test_new(t *testing.T) {
	test := func(identifier semver.Prerelease, expected string) {
		t.Helper()
		itUtilAssertEq(t, identifier.IsEmpty(), expected == "")
		// `identifier.len()` is str::len through Deref.
		itUtilAssertEq(t, len(identifier.Deref()), len(expected))
		itUtilAssertEq(t, identifier.AsStr(), expected)
		itUtilAssertEq(t, identifier, identifier)
		itUtilAssertEq(t, identifier, rs.Clone(identifier))
	}

	var string_ strings.Builder
	limit := 280 // the Rust test uses 40 under miri, which is slow
	for range limit {
		test(itUtilPrerelease(t, string_.String()), string_.String())
		string_.WriteByte('1')
	}

	{
		string_ := strings.Repeat(string_.String(), 20000)
		test(itUtilPrerelease(t, string_), string_)
	}
}

func TestItTestIdentifier_test_eq(t *testing.T) {
	itUtilAssertEq(t, itUtilPrerelease(t, "-"), itUtilPrerelease(t, "-"))
	itUtilAssertNe(t, itUtilPrerelease(t, "a"), itUtilPrerelease(t, "aa"))
	itUtilAssertNe(t, itUtilPrerelease(t, "aa"), itUtilPrerelease(t, "a"))
	itUtilAssertNe(t, itUtilPrerelease(t, "aaaaaaaaa"), itUtilPrerelease(t, "a"))
	itUtilAssertNe(t, itUtilPrerelease(t, "a"), itUtilPrerelease(t, "aaaaaaaaa"))
	itUtilAssertNe(t, itUtilPrerelease(t, "aaaaaaaaa"), itUtilPrerelease(t, "bbbbbbbbb"))
	itUtilAssertNe(t, itUtilBuildMetadata(t, "1"), itUtilBuildMetadata(t, "001"))
}

func TestItTestIdentifier_test_prerelease(t *testing.T) {
	err := itUtilPrereleaseErr(t, "1.b\x00")
	itUtilAssertToString(t, err, "unexpected character in pre-release identifier")
}
