// Package semver stands in for the Rust crate `semver` 1.0.28, Cargo's
// flavour of Semantic Versioning, built as Mjolnir builds it: with the
// default `std` feature and without `serde`.
//
// Mjolnir parses versions and orders them:
//
//   - mj-client/src/build_identity.rs parses a daemon's published
//     `build_version`, takes its build metadata out
//     (`std::mem::replace(&mut version.build, BuildMetadata::EMPTY)`), reads
//     it with `as_str`, prints the version and orders two builds with
//     `cmp_precedence`.
//   - mj-controller/src/controller/update.rs parses release tags and package
//     versions (after trimming a leading `v`), keeps the newest with `>` and
//     `<=`, and prints them. Its error text reaches the user through anyhow
//     context.
//   - mj-build-cache/src/mbx.rs and src/mbx/install.rs parse the version an
//     mbx binary reports and compare it with the pinned one.
//
// What a user sees is the parse error text, the printed version and which
// of two versions wins, so the package is a port of the crate's source.
//
// # Names
//
//	semver::Version::parse(s)              semver.VersionParse(s)
//	s.parse::<Version>()                   semver.VersionFromStr(s)
//	Version::new(1, 2, 3)                  semver.VersionNew(1, 2, 3)
//	version.major, .pre, .build            version.Major, .Pre, .Build
//	a.cmp_precedence(&b)                   a.CmpPrecedence(&b) (an rs.Ordering)
//	a.cmp(&b), a < b, a == b               rs.Cmp(a, b), rs.Cmp(a, b) < 0, rs.Eq(a, b)
//	version.to_string()                    version.String()
//	Prerelease::new(s), Prerelease::EMPTY  semver.PrereleaseNew(s), semver.PrereleaseEmpty
//	BuildMetadata::new(s), ::EMPTY         semver.BuildMetadataNew(s), semver.BuildMetadataEmpty
//	build.as_str(), build.is_empty()       build.AsStr(), build.IsEmpty()
//	&*build (Deref to str), build.len()    build.Deref(), len(build.Deref())
//	semver::Error                          *semver.Error, returned as error
//
// Version, Prerelease and BuildMetadata are not Copy in Rust, so their
// methods have pointer receivers. The derived and hand-written comparisons
// are the methods Compare and Equal, which rs.Cmp and rs.Eq call; Version's
// are the derived field-by-field order: major, minor, patch, pre-release,
// then build metadata. The zero value of Prerelease and BuildMetadata is
// EMPTY, which is also their Default. The Display logic of every type is its
// String method; FmtDisplay and FmtDebug give rs.Format the crate's output,
// with Version's own padding.
//
// # Covered
//
// Everything of the crate but version requirements:
//
//   - src/lib.rs: Version, Prerelease, BuildMetadata and their constructors,
//     constants and accessors; Version::cmp_precedence.
//   - src/parse.rs: FromStr for Version, Prerelease and BuildMetadata, with
//     every error the crate reports for them.
//   - src/error.rs: Error, its Display text and its Debug form
//     `Error("...")`, the position names, and the quoting of the offending
//     character as `{:?}` prints a char (`'\0'` for NUL).
//   - src/display.rs: Display and Debug of Version, Prerelease and
//     BuildMetadata, including Version's handling of width, fill and
//     alignment.
//   - src/impls.rs: Ord of Prerelease and BuildMetadata (numeric
//     identifiers compared by value, build metadata's leading zeros), Deref
//     to str.
//   - src/identifier.rs: Identifier is the string it holds. Its inline and
//     heap representations and pointer comparisons are invisible from
//     outside and are not reproduced.
//
// # Not covered
//
//   - VersionReq, Comparator and Op: their parsing (src/parse.rs), matching
//     (src/eval.rs), Display and FromIterator. Mjolnir never uses a version
//     requirement. tests/test_version_req.rs and the requirement helpers of
//     tests/util/mod.rs are therefore not ported, and the error kinds only
//     requirements produce (ExpectedCommaFound, WildcardNotTheOnlyComparator,
//     UnexpectedAfterWildcard, ExcessiveComparators) are declared with their
//     text but nothing returns them.
//   - src/serde.rs: Mjolnir does not enable the `serde` feature.
//
// A Rust `&str` is always UTF-8 and a Go string need not be. A byte that is
// not part of a valid encoding is named in an error as U+FFFD, the character
// Go decodes it as; nothing else depends on the encoding, because every
// valid version is ASCII.
//
// # Behaviour to know about
//
//   - Build metadata takes part in Version's Ord and Eq: 1.2.3+a and 1.2.3
//     are different, and 1.2.3 < 1.2.3+a. Only cmp_precedence ignores it.
//     update.rs keeps the newest release with `>` and `<=`, which do look at
//     build metadata.
//   - Numeric parts of build metadata compare by value and then by length:
//     0 < 00 < 1 < 01 < 001 < 2. Pre-release identifiers cannot have leading
//     zeros.
//   - Version's Display honours width, fill and alignment (left by default)
//     and nothing else: `{:012}` pads with spaces on the right, and a
//     precision does not truncate. Prerelease, BuildMetadata and Error
//     ignore width altogether.
//   - In `{:8?}` the width reaches each number of Version's Debug form,
//     `Version { major:        1, .. }`, because Rust's debug_struct passes
//     the formatter on to the fields.
//
// # Tests
//
// The crate's integration tests for what is covered are ported
// (it_test_version_test.go, it_test_identifier_test.go,
// it_test_autotrait_test.go, with tests/util/mod.rs in it_util_test.go), and
// the documentation examples in lib_test.go. oracle_test.go compares the
// package with the real crate over two recorded corpora: parsing (every
// numeric boundary, identifier shapes, every Latin-1 character and a sample
// of all Unicode in each position an error names), every comparison and
// sorting; and, through rs.Formatter, Display with width, fill, alignment
// and precision and Debug alone and inside a derived Debug.
package semver
