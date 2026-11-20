// Port of semver 1.0.28 tests/test_autotrait.rs

package semver_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/semver"
)

func TestItTestAutotrait_test(t *testing.T) {
	// The Rust test asks the compiler whether the crate's types are Send and
	// Sync. Go has no such property to ask about. The types ported here
	// (BuildMetadata, Error, Prerelease, Version) hold only numbers and
	// strings and are never changed by their methods, so values shared
	// between goroutines are read safely; Comparator, VersionReq and Op are
	// not ported (doc.go). What can be checked is that several goroutines
	// parse, print and compare the same values at once.
	version := itUtilVersion(t, "1.2.3-alpha.1+build.5")
	done := make(chan string)
	for range 4 {
		go func() {
			parsed, err := semver.VersionParse(version.String())
			if err != nil || parsed.Compare(&version) != 0 || !parsed.Equal(&version) {
				done <- "mismatch"
				return
			}
			done <- parsed.Pre.AsStr() + "+" + parsed.Build.AsStr()
		}()
	}
	for range 4 {
		itUtilAssertEq(t, <-done, "alpha.1+build.5")
	}
}
