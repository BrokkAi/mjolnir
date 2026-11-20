// Port of semver 1.0.28 src/lib.rs and src/parse.rs (documentation examples)

package semver_test

import (
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/semver"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// The crate has no `#[test]` functions in src/. These are its documentation
// examples, which Rust runs as tests. The crate-level example evaluates a
// VersionReq, which this package does not port (doc.go).

// Version::new: equivalent to a struct with empty pre-release and build
// metadata.
func TestLib_doc_version_new(t *testing.T) {
	newVersion := func(major uint64, minor uint64, patch uint64) semver.Version {
		return semver.Version{
			Major: major,
			Minor: minor,
			Patch: patch,
			Pre:   semver.PrereleaseEmpty,
			Build: semver.BuildMetadataEmpty,
		}
	}
	itUtilAssertEq(t, semver.VersionNew(1, 2, 3), newVersion(1, 2, 3))
	itUtilAssertEq(t, semver.VersionNew(1, 2, 3), itUtilVersion(t, "1.2.3"))
}

// Version::cmp_precedence.
func TestLib_doc_cmp_precedence(t *testing.T) {
	parse := func(text string) semver.Version {
		// "..".parse::<Version>().unwrap()
		return rs.Unwrap(semver.VersionFromStr(text))
	}
	versions := []semver.Version{
		parse("1.20.0+c144a98"),
		parse("1.20.0"),
		parse("1.0.0"),
		parse("1.0.0-alpha"),
		parse("1.20.0+bc17664"),
	}

	// This is a stable sort, so it preserves the relative order of equal
	// elements. The three 1.20.0 versions differ only in build metadata so
	// they are not reordered relative to one another.
	slices.SortStableFunc(versions, func(a, b semver.Version) int { return int(a.CmpPrecedence(&b)) })
	itUtilAssertEq(t, versions, []semver.Version{
		parse("1.0.0-alpha"),
		parse("1.0.0"),
		parse("1.20.0+c144a98"),
		parse("1.20.0"),
		parse("1.20.0+bc17664"),
	})

	// Totally order the versions, including comparing the build metadata.
	slices.SortStableFunc(versions, rs.Cmp[semver.Version])
	itUtilAssertEq(t, versions, []semver.Version{
		parse("1.0.0-alpha"),
		parse("1.0.0"),
		parse("1.20.0"),
		parse("1.20.0+bc17664"),
		parse("1.20.0+c144a98"),
	})
}

// Error (src/parse.rs).
func TestLib_doc_error(t *testing.T) {
	err := itUtilVersionErr(t, "1.q.r")

	// "unexpected character 'q' while parsing minor version number"
	itUtilAssertToString(t, err, "unexpected character 'q' while parsing minor version number")
	itUtilAssertEq(t, err.Error(), "unexpected character 'q' while parsing minor version number")
}
