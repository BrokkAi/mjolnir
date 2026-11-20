// Port of semver 1.0.28 tests/test_version.rs

package semver_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/semver"
	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestItTestVersion_test_parse(t *testing.T) {
	err := itUtilVersionErr(t, "")
	itUtilAssertToString(t, err, "empty string, expected a semver version")

	err = itUtilVersionErr(t, "  ")
	itUtilAssertToString(t,
		err,
		"unexpected character ' ' while parsing major version number",
	)

	err = itUtilVersionErr(t, "1")
	itUtilAssertToString(t,
		err,
		"unexpected end of input while parsing major version number",
	)

	err = itUtilVersionErr(t, "1.2")
	itUtilAssertToString(t,
		err,
		"unexpected end of input while parsing minor version number",
	)

	err = itUtilVersionErr(t, "1.2.3-")
	itUtilAssertToString(t, err, "empty identifier segment in pre-release identifier")

	err = itUtilVersionErr(t, "a.b.c")
	itUtilAssertToString(t,
		err,
		"unexpected character 'a' while parsing major version number",
	)

	err = itUtilVersionErr(t, "1.2.3 abc")
	itUtilAssertToString(t, err, "unexpected character ' ' after patch version number")

	err = itUtilVersionErr(t, "1.2.3-01")
	itUtilAssertToString(t, err, "invalid leading zero in pre-release identifier")

	err = itUtilVersionErr(t, "1.2.3++")
	itUtilAssertToString(t, err, "empty identifier segment in build metadata")

	err = itUtilVersionErr(t, "07")
	itUtilAssertToString(t, err, "invalid leading zero in major version number")

	err = itUtilVersionErr(t, "111111111111111111111.0.0")
	itUtilAssertToString(t, err, "value of major version number exceeds u64::MAX")

	err = itUtilVersionErr(t, "8\x00")
	itUtilAssertToString(t, err, "unexpected character '\\0' after major version number")

	parsed := itUtilVersion(t, "1.2.3")
	expected := semver.VersionNew(1, 2, 3)
	itUtilAssertEq(t, parsed, expected)
	expected = semver.Version{
		Major: 1,
		Minor: 2,
		Patch: 3,
		Pre:   semver.PrereleaseEmpty,
		Build: semver.BuildMetadataEmpty,
	}
	itUtilAssertEq(t, parsed, expected)

	parsed = itUtilVersion(t, "1.2.3-alpha1")
	expected = semver.Version{
		Major: 1,
		Minor: 2,
		Patch: 3,
		Pre:   itUtilPrerelease(t, "alpha1"),
		Build: semver.BuildMetadataEmpty,
	}
	itUtilAssertEq(t, parsed, expected)

	parsed = itUtilVersion(t, "1.2.3+build5")
	expected = semver.Version{
		Major: 1,
		Minor: 2,
		Patch: 3,
		Pre:   semver.PrereleaseEmpty,
		Build: itUtilBuildMetadata(t, "build5"),
	}
	itUtilAssertEq(t, parsed, expected)

	parsed = itUtilVersion(t, "1.2.3+5build")
	expected = semver.Version{
		Major: 1,
		Minor: 2,
		Patch: 3,
		Pre:   semver.PrereleaseEmpty,
		Build: itUtilBuildMetadata(t, "5build"),
	}
	itUtilAssertEq(t, parsed, expected)

	parsed = itUtilVersion(t, "1.2.3-alpha1+build5")
	expected = semver.Version{
		Major: 1,
		Minor: 2,
		Patch: 3,
		Pre:   itUtilPrerelease(t, "alpha1"),
		Build: itUtilBuildMetadata(t, "build5"),
	}
	itUtilAssertEq(t, parsed, expected)

	parsed = itUtilVersion(t, "1.2.3-1.alpha1.9+build5.7.3aedf")
	expected = semver.Version{
		Major: 1,
		Minor: 2,
		Patch: 3,
		Pre:   itUtilPrerelease(t, "1.alpha1.9"),
		Build: itUtilBuildMetadata(t, "build5.7.3aedf"),
	}
	itUtilAssertEq(t, parsed, expected)

	parsed = itUtilVersion(t, "1.2.3-0a.alpha1.9+05build.7.3aedf")
	expected = semver.Version{
		Major: 1,
		Minor: 2,
		Patch: 3,
		Pre:   itUtilPrerelease(t, "0a.alpha1.9"),
		Build: itUtilBuildMetadata(t, "05build.7.3aedf"),
	}
	itUtilAssertEq(t, parsed, expected)

	parsed = itUtilVersion(t, "0.4.0-beta.1+0851523")
	expected = semver.Version{
		Major: 0,
		Minor: 4,
		Patch: 0,
		Pre:   itUtilPrerelease(t, "beta.1"),
		Build: itUtilBuildMetadata(t, "0851523"),
	}
	itUtilAssertEq(t, parsed, expected)

	// for https://nodejs.org/dist/index.json, where some older npm versions are "1.1.0-beta-10"
	parsed = itUtilVersion(t, "1.1.0-beta-10")
	expected = semver.Version{
		Major: 1,
		Minor: 1,
		Patch: 0,
		Pre:   itUtilPrerelease(t, "beta-10"),
		Build: semver.BuildMetadataEmpty,
	}
	itUtilAssertEq(t, parsed, expected)
}

func TestItTestVersion_test_eq(t *testing.T) {
	itUtilAssertEq(t, itUtilVersion(t, "1.2.3"), itUtilVersion(t, "1.2.3"))
	itUtilAssertEq(t, itUtilVersion(t, "1.2.3-alpha1"), itUtilVersion(t, "1.2.3-alpha1"))
	itUtilAssertEq(t, itUtilVersion(t, "1.2.3+build.42"), itUtilVersion(t, "1.2.3+build.42"))
	itUtilAssertEq(t, itUtilVersion(t, "1.2.3-alpha1+42"), itUtilVersion(t, "1.2.3-alpha1+42"))
}

func TestItTestVersion_test_ne(t *testing.T) {
	itUtilAssertNe(t, itUtilVersion(t, "0.0.0"), itUtilVersion(t, "0.0.1"))
	itUtilAssertNe(t, itUtilVersion(t, "0.0.0"), itUtilVersion(t, "0.1.0"))
	itUtilAssertNe(t, itUtilVersion(t, "0.0.0"), itUtilVersion(t, "1.0.0"))
	itUtilAssertNe(t, itUtilVersion(t, "1.2.3-alpha"), itUtilVersion(t, "1.2.3-beta"))
	itUtilAssertNe(t, itUtilVersion(t, "1.2.3+23"), itUtilVersion(t, "1.2.3+42"))
}

func TestItTestVersion_test_display(t *testing.T) {
	// Version's String has a pointer receiver, so each version is held in a
	// variable first.
	version := itUtilVersion(t, "1.2.3")
	itUtilAssertToString(t, &version, "1.2.3")
	version = itUtilVersion(t, "1.2.3-alpha1")
	itUtilAssertToString(t, &version, "1.2.3-alpha1")
	version = itUtilVersion(t, "1.2.3+build.42")
	itUtilAssertToString(t, &version, "1.2.3+build.42")
	version = itUtilVersion(t, "1.2.3-alpha1+42")
	itUtilAssertToString(t, &version, "1.2.3-alpha1+42")
}

// `a < b` on versions is rs.Cmp(a, b) < 0 in Go, which reaches Version's
// Compare method, the port of the derived Ord.

func TestItTestVersion_test_lt(t *testing.T) {
	lt := func(a, b string) bool { return rs.Cmp(itUtilVersion(t, a), itUtilVersion(t, b)) < 0 }
	itUtilAssert(t, lt("0.0.0", "1.2.3-alpha2"), "")
	itUtilAssert(t, lt("1.0.0", "1.2.3-alpha2"), "")
	itUtilAssert(t, lt("1.2.0", "1.2.3-alpha2"), "")
	itUtilAssert(t, lt("1.2.3-alpha1", "1.2.3"), "")
	itUtilAssert(t, lt("1.2.3-alpha1", "1.2.3-alpha2"), "")
	itUtilAssert(t, !(lt("1.2.3-alpha2", "1.2.3-alpha2")), "")
	itUtilAssert(t, lt("1.2.3+23", "1.2.3+42"), "")
}

func TestItTestVersion_test_le(t *testing.T) {
	le := func(a, b string) bool { return rs.Cmp(itUtilVersion(t, a), itUtilVersion(t, b)) <= 0 }
	itUtilAssert(t, le("0.0.0", "1.2.3-alpha2"), "")
	itUtilAssert(t, le("1.0.0", "1.2.3-alpha2"), "")
	itUtilAssert(t, le("1.2.0", "1.2.3-alpha2"), "")
	itUtilAssert(t, le("1.2.3-alpha1", "1.2.3-alpha2"), "")
	itUtilAssert(t, le("1.2.3-alpha2", "1.2.3-alpha2"), "")
	itUtilAssert(t, le("1.2.3+23", "1.2.3+42"), "")
}

func TestItTestVersion_test_gt(t *testing.T) {
	gt := func(a, b string) bool { return rs.Cmp(itUtilVersion(t, a), itUtilVersion(t, b)) > 0 }
	itUtilAssert(t, gt("1.2.3-alpha2", "0.0.0"), "")
	itUtilAssert(t, gt("1.2.3-alpha2", "1.0.0"), "")
	itUtilAssert(t, gt("1.2.3-alpha2", "1.2.0"), "")
	itUtilAssert(t, gt("1.2.3-alpha2", "1.2.3-alpha1"), "")
	itUtilAssert(t, gt("1.2.3", "1.2.3-alpha2"), "")
	itUtilAssert(t, !(gt("1.2.3-alpha2", "1.2.3-alpha2")), "")
	itUtilAssert(t, !(gt("1.2.3+23", "1.2.3+42")), "")
}

func TestItTestVersion_test_ge(t *testing.T) {
	ge := func(a, b string) bool { return rs.Cmp(itUtilVersion(t, a), itUtilVersion(t, b)) >= 0 }
	itUtilAssert(t, ge("1.2.3-alpha2", "0.0.0"), "")
	itUtilAssert(t, ge("1.2.3-alpha2", "1.0.0"), "")
	itUtilAssert(t, ge("1.2.3-alpha2", "1.2.0"), "")
	itUtilAssert(t, ge("1.2.3-alpha2", "1.2.3-alpha1"), "")
	itUtilAssert(t, ge("1.2.3-alpha2", "1.2.3-alpha2"), "")
	itUtilAssert(t, !(ge("1.2.3+23", "1.2.3+42")), "")
}

func TestItTestVersion_test_spec_order(t *testing.T) {
	vs := []string{
		"1.0.0-alpha",
		"1.0.0-alpha.1",
		"1.0.0-alpha.beta",
		"1.0.0-beta",
		"1.0.0-beta.2",
		"1.0.0-beta.11",
		"1.0.0-rc.1",
		"1.0.0",
	}
	i := 1
	for i < len(vs) {
		a := itUtilVersion(t, vs[i-1])
		b := itUtilVersion(t, vs[i])
		// The Rust message prints both versions with `{:?}`; the plain text
		// says the same without the formatter.
		itUtilAssert(t, rs.Cmp(a, b) < 0, "nope "+a.String()+" < "+b.String())
		i += 1
	}
}

func TestItTestVersion_test_align(t *testing.T) {
	itUtilRequireFormatter(t)
	version := itUtilVersion(t, "1.2.3-rc1")
	itUtilAssertEq(t, "1.2.3-rc1           ", rs.Format("{:20}", &version))
	itUtilAssertEq(t, "*****1.2.3-rc1******", rs.Format("{:*^20}", &version))
	itUtilAssertEq(t, "           1.2.3-rc1", rs.Format("{:>20}", &version))
}
