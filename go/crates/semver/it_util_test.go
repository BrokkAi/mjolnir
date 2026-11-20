// Port of semver 1.0.28 tests/util/mod.rs

package semver_test

import (
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/semver"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// The helpers for VersionReq and Comparator (req, req_err, comparator,
// comparator_err) are not ported: this package leaves out VersionReq (see
// doc.go).

func itUtilVersion(t *testing.T, text string) semver.Version {
	t.Helper()
	version, err := semver.VersionParse(text)
	if err != nil {
		t.Fatalf("called `Result::unwrap()` on an `Err` value: %s", err)
	}
	return version
}

func itUtilVersionErr(t *testing.T, text string) *semver.Error {
	t.Helper()
	version, err := semver.VersionParse(text)
	if err == nil {
		t.Fatalf("called `Result::unwrap_err()` on an `Ok` value: %s", version.String())
	}
	return itUtilAsError(t, err)
}

func itUtilPrerelease(t *testing.T, text string) semver.Prerelease {
	t.Helper()
	pre, err := semver.PrereleaseNew(text)
	if err != nil {
		t.Fatalf("called `Result::unwrap()` on an `Err` value: %s", err)
	}
	return pre
}

func itUtilPrereleaseErr(t *testing.T, text string) *semver.Error {
	t.Helper()
	pre, err := semver.PrereleaseNew(text)
	if err == nil {
		t.Fatalf("called `Result::unwrap_err()` on an `Ok` value: %s", pre.String())
	}
	return itUtilAsError(t, err)
}

func itUtilBuildMetadata(t *testing.T, text string) semver.BuildMetadata {
	t.Helper()
	build, err := semver.BuildMetadataNew(text)
	if err != nil {
		t.Fatalf("called `Result::unwrap()` on an `Err` value: %s", err)
	}
	return build
}

// itUtilAssertToString is `assert_to_string`: `value.to_string()` is the
// String method, which holds the Display logic of every type in the package.
func itUtilAssertToString(t *testing.T, value interface{ String() string }, expected string) {
	t.Helper()
	itUtilAssertEq(t, value.String(), expected)
}

// itUtilAsError checks that a parse error is the package's *Error, as the
// functions document.
func itUtilAsError(t *testing.T, err error) *semver.Error {
	t.Helper()
	semverError, ok := err.(*semver.Error)
	if !ok {
		t.Fatalf("error %q is a %T, not a *semver.Error", err, err)
	}
	return semverError
}

// itUtilAssertEq is `assert_eq!`: the values compare equal as Rust's
// PartialEq compares them.
func itUtilAssertEq[T any](t *testing.T, left T, right T) {
	t.Helper()
	if !rs.Eq(left, right) {
		t.Fatalf("assertion `left == right` failed\n  left: %#v\n right: %#v", left, right)
	}
}

// itUtilAssertNe is `assert_ne!`.
func itUtilAssertNe[T any](t *testing.T, left T, right T) {
	t.Helper()
	if rs.Eq(left, right) {
		t.Fatalf("assertion `left != right` failed\n  left: %#v\n right: %#v", left, right)
	}
}

// itUtilAssert is `assert!(cond, ..)`.
func itUtilAssert(t *testing.T, cond bool, message string) {
	t.Helper()
	if !cond {
		if message == "" {
			message = "assertion failed"
		}
		t.Fatal(message)
	}
}

// itUtilRequireFormatter skips a test that formats through rs.Format, which
// panics with "not yet ported" until go/rs has its formatter
// (go/rs/formatter_pending.go).
func itUtilRequireFormatter(t *testing.T) {
	t.Helper()
	if !itUtilFormatterPorted() {
		t.Skip("waiting for rs.Format and rs.Formatter (go/rs/formatter_pending.go)")
	}
}

func itUtilFormatterPorted() (ported bool) {
	defer func() {
		if recovered := recover(); recovered != nil {
			if strings.HasPrefix(rs.PanicInfoOf(recovered).Message, "not yet ported: core::fmt") {
				ported = false
				return
			}
			panic(recovered)
		}
	}()
	_ = rs.Format("{:>4}", "x")
	return true
}
