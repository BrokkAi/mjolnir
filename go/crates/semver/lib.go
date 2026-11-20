// Port of semver 1.0.28 src/lib.rs

package semver

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

// **SemVer version** as defined by <https://semver.org>.
//
// # Syntax
//
//   - The major, minor, and patch numbers may be any integer 0 through u64::MAX.
//     When representing a SemVer version as a string, each number is written as
//     a base 10 integer. For example, `1.0.119`.
//
//   - Leading zeros are forbidden in those positions. For example `1.01.00` is
//     invalid as a SemVer version.
//
//   - The pre-release identifier, if present, must conform to the syntax
//     documented for [`Prerelease`].
//
//   - The build metadata, if present, must conform to the syntax documented for
//     [`BuildMetadata`].
//
//   - Whitespace is not allowed anywhere in the version.
//
// # Total ordering
//
// Given any two SemVer versions, one is less than, greater than, or equal to
// the other. Versions may be compared against one another using Rust's usual
// comparison operators.
//
//   - The major, minor, and patch number are compared numerically from left to
//     right, lexicographically ordered as a 3-tuple of integers. So for example
//     version `1.5.0` is less than version `1.19.0`, despite the fact that
//     "1.19.0" < "1.5.0" as ASCIIbetically compared strings and 1.19 < 1.5
//     as real numbers.
//
//   - When major, minor, and patch are equal, a pre-release version is
//     considered less than the ordinary release: version `1.0.0-alpha.1` is
//     less than version `1.0.0`.
//
//   - Two pre-releases of the same major, minor, patch are compared by
//     lexicographic ordering of dot-separated components of the pre-release
//     string.
//
//   - Identifiers consisting of only digits are compared
//     numerically: `1.0.0-pre.8` is less than `1.0.0-pre.12`.
//
//   - Identifiers that contain a letter or hyphen are compared in ASCII sort
//     order: `1.0.0-pre12` is less than `1.0.0-pre8`.
//
//   - Any numeric identifier is always less than any non-numeric
//     identifier: `1.0.0-pre.1` is less than `1.0.0-pre.x`.
//
// Example: `1.0.0-alpha` < `1.0.0-alpha.1` < `1.0.0-alpha.beta` < `1.0.0-beta` < `1.0.0-beta.2` < `1.0.0-beta.11` < `1.0.0-rc.1` < `1.0.0`
//
// In Go the comparison operators of `#[derive(Eq, PartialEq, Ord,
// PartialOrd)]` are rs.Eq and rs.Cmp, which compare the fields in order and
// use the Compare methods of Prerelease and BuildMetadata; Equal and Compare
// below are the same comparisons written out.
type Version struct {
	Major uint64        `rs:"major"`
	Minor uint64        `rs:"minor"`
	Patch uint64        `rs:"patch"`
	Pre   Prerelease    `rs:"pre"`
	Build BuildMetadata `rs:"build"`
}

var _ = rs.RegisterStruct[Version]("Version")

// Optional pre-release identifier on a version string. This comes after `-` in
// a SemVer version, like `1.0.0-alpha.1`
//
// # Examples
//
// Some real world pre-release idioms drawn from crates.io:
//
//   - **[mio]** 0.7.0-alpha.1 — the most common style
//     for numbering pre-releases.
//
//   - **[pest]** 1.0.0-beta.8, 1.0.0-rc.0
//     — this crate makes a distinction between betas and release
//     candidates.
//
//   - **[sassers]** 0.11.0-shitshow — ???.
//
//   - **[atomic-utils]** 0.0.0-reserved — a squatted
//     crate name.
//
// *Tip:* Be aware that if you are planning to number your own pre-releases,
// you should prefer to separate the numeric part from any non-numeric
// identifiers by using a dot in between. That is, prefer pre-releases
// `alpha.1`, `alpha.2`, etc rather than `alpha1`, `alpha2` etc. The SemVer
// spec's rule for pre-release precedence has special treatment of numeric
// components in the pre-release string, but only if there are no non-digit
// characters in the same dot-separated component. So you'd have `alpha.2` <
// `alpha.11` as intended, but `alpha11` < `alpha2`.
//
// # Syntax
//
// Pre-release strings are a series of dot separated identifiers immediately
// following the patch version. Identifiers must comprise only ASCII
// alphanumerics and hyphens: `0-9`, `A-Z`, `a-z`, `-`. Identifiers must not be
// empty. Numeric identifiers must not include leading zeros.
//
// # Total ordering
//
// Pre-releases have a total order defined by the SemVer spec. It uses
// lexicographic ordering of dot-separated components. Identifiers consisting
// of only digits are compared numerically. Otherwise, identifiers are compared
// in ASCII sort order. Any numeric identifier is always less than any
// non-numeric identifier.
//
// Example: `alpha` < `alpha.85` < `alpha.90` < `alpha.200` < `alpha.0a` < `alpha.1a0` < `alpha.a` < `beta`
//
// The zero value is `Prerelease::EMPTY`, which is also its Default.
type Prerelease struct {
	identifier identifierIdentifier `rs:"identifier"`
}

var _ = rs.RegisterStruct[Prerelease]("Prerelease")

// Optional build metadata identifier. This comes after `+` in a SemVer
// version, as in `0.8.1+zstd.1.5.0`.
//
// # Examples
//
// Some real world build metadata idioms drawn from crates.io:
//
//   - **[libgit2-sys]** 0.12.20+1.1.0 — for this
//     crate, the build metadata indicates the version of the C libgit2 library
//     that the Rust crate is built against.
//
//   - **[mashup]** 0.1.13+deprecated — just the word
//     "deprecated" for a crate that has been superseded by another. Eventually
//     people will take notice of this in Cargo's build output where it lists the
//     crates being compiled.
//
//   - **[google-bigquery2]** 2.0.4+20210327 — this
//     library is automatically generated from an official API schema, and the
//     build metadata indicates the date on which that schema was last captured.
//
//   - **[fbthrift-git]** 0.0.6+c7fcc0e — this crate is
//     published from snapshots of a big company monorepo. In monorepo
//     development, there is no concept of versions, and all downstream code is
//     just updated atomically in the same commit that breaking changes to a
//     library are landed. Therefore for crates.io purposes, every published
//     version must be assumed to be incompatible with the previous. The build
//     metadata provides the source control hash of the snapshotted code.
//
// # Syntax
//
// Build metadata is a series of dot separated identifiers immediately
// following the patch or pre-release version. Identifiers must comprise only
// ASCII alphanumerics and hyphens: `0-9`, `A-Z`, `a-z`, `-`. Identifiers must
// not be empty. Leading zeros *are* allowed, unlike any other place in the
// SemVer grammar.
//
// # Total ordering
//
// Build metadata is ignored in evaluating `VersionReq`; it plays no role in
// whether a `Version` matches any one of the comparison operators.
//
// However for comparing build metadatas among one another, they do have a
// total order which is determined by lexicographic ordering of dot-separated
// components. Identifiers consisting of only digits are compared numerically.
// Otherwise, identifiers are compared in ASCII sort order. Any numeric
// identifier is always less than any non-numeric identifier.
//
// Example: `demo` < `demo.85` < `demo.90` < `demo.090` < `demo.200` < `demo.1a0` < `demo.a` < `memo`
//
// The zero value is `BuildMetadata::EMPTY`, which is also its Default.
type BuildMetadata struct {
	identifier identifierIdentifier `rs:"identifier"`
}

var _ = rs.RegisterStruct[BuildMetadata]("BuildMetadata")

// Create `Version` with an empty pre-release and build metadata.
//
// Equivalent to:
//
//	Version{
//		Major: major,
//		Minor: minor,
//		Patch: patch,
//		Pre:   PrereleaseEmpty,
//		Build: BuildMetadataEmpty,
//	}
func VersionNew(major uint64, minor uint64, patch uint64) Version {
	return Version{
		Major: major,
		Minor: minor,
		Patch: patch,
		Pre:   PrereleaseEmpty,
		Build: BuildMetadataEmpty,
	}
}

// Create `Version` by parsing from string representation.
//
// # Errors
//
// Possible reasons for the parse to fail include:
//
//   - `1.0` — too few numeric components. A SemVer version must have
//     exactly three. If you are looking at something that has fewer than
//     three numbers in it, it's possible it is a `VersionReq` instead (with
//     an implicit default `^` comparison operator).
//
//   - `1.0.01` — a numeric component has a leading zero.
//
//   - `1.0.unknown` — unexpected character in one of the components.
//
//   - `1.0.0-` or `1.0.0+` — the pre-release or build metadata are
//     indicated present but empty.
//
//   - `1.0.0-alpha_123` — pre-release or build metadata have something
//     outside the allowed characters, which are `0-9`, `A-Z`, `a-z`, `-`,
//     and `.` (dot).
//
//   - `23456789999999999999.0.0` — overflow of a u64.
//
// The error is a *Error.
func VersionParse(text string) (Version, error) {
	return VersionFromStr(text)
}

// Compare the major, minor, patch, and pre-release value of two versions,
// disregarding build metadata. Versions that differ only in build metadata
// are considered equal. This comparison is what the SemVer spec refers to
// as "precedence".
func (self *Version) CmpPrecedence(other *Version) rs.Ordering {
	// Ord::cmp(&(self.major, self.minor, self.patch, &self.pre), &(other.major, ..))
	if self.Major != other.Major {
		return versionCmpU64(self.Major, other.Major)
	}
	if self.Minor != other.Minor {
		return versionCmpU64(self.Minor, other.Minor)
	}
	if self.Patch != other.Patch {
		return versionCmpU64(self.Patch, other.Patch)
	}
	return rs.OrderingOf(self.Pre.Compare(&other.Pre))
}

func versionCmpU64(a uint64, b uint64) rs.Ordering {
	switch {
	case a < b:
		return rs.Ordering_Less
	case a > b:
		return rs.Ordering_Greater
	default:
		return rs.Ordering_Equal
	}
}

// Compare is the derived `Ord` of Version: major, minor, patch, pre-release
// and build metadata, in that order. It is negative, zero or positive.
func (self *Version) Compare(other *Version) int {
	if order := self.CmpPrecedence(other); order != rs.Ordering_Equal {
		return int(order)
	}
	return self.Build.Compare(&other.Build)
}

// Equal is the derived `PartialEq` of Version: every field equal.
func (self *Version) Equal(other *Version) bool {
	return self.Major == other.Major &&
		self.Minor == other.Minor &&
		self.Patch == other.Patch &&
		self.Pre.Equal(&other.Pre) &&
		self.Build.Equal(&other.Build)
}

// `Prerelease::EMPTY`.
var PrereleaseEmpty = Prerelease{
	identifier: identifierIdentifierEmpty(),
}

// PrereleaseNew is `Prerelease::new`. The error is a *Error.
func PrereleaseNew(text string) (Prerelease, error) {
	return PrereleaseFromStr(text)
}

// AsStr is `Prerelease::as_str`.
func (self *Prerelease) AsStr() string {
	return self.identifier.asStr()
}

// IsEmpty is `Prerelease::is_empty`.
func (self *Prerelease) IsEmpty() bool {
	return self.identifier.isEmpty()
}

// Equal is the derived `PartialEq` of Prerelease: the same text.
func (self *Prerelease) Equal(other *Prerelease) bool {
	return self.identifier == other.identifier
}

// `BuildMetadata::EMPTY`.
var BuildMetadataEmpty = BuildMetadata{
	identifier: identifierIdentifierEmpty(),
}

// BuildMetadataNew is `BuildMetadata::new`. The error is a *Error.
func BuildMetadataNew(text string) (BuildMetadata, error) {
	return BuildMetadataFromStr(text)
}

// AsStr is `BuildMetadata::as_str`.
func (self *BuildMetadata) AsStr() string {
	return self.identifier.asStr()
}

// IsEmpty is `BuildMetadata::is_empty`.
func (self *BuildMetadata) IsEmpty() bool {
	return self.identifier.isEmpty()
}

// Equal is the derived `PartialEq` of BuildMetadata: the same text.
func (self *BuildMetadata) Equal(other *BuildMetadata) bool {
	return self.identifier == other.identifier
}
