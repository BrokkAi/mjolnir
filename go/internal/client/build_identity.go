// Port of mj-client/src/build_identity.rs.

// Which of two Mjolnir builds is newer.
//
// The release version orders releases. Development builds, and two
// installations of one release, share a version string, a wire protocol and
// a schema, so the version alone cannot say which build a running daemon
// should give way to. Each build therefore also carries the Git revision it
// was built from, that commit's time, and when its executable file was
// written. The commit time orders different revisions; the file time orders
// two builds of one revision, which differ only in uncommitted changes or
// build settings.
//
// A daemon publishes these facts as semver build metadata on the
// `build_version` it already reports, for example
// `2.24.0+19485f17….c1790824791.m1790830000123456789`. `daemon.json` and the
// status reply reject unknown fields, so a new field would break every older
// client that reads them. Build metadata is invisible to older clients: they
// compare versions with `cmp_precedence`, which ignores it.

package client

import (
	"sync"
)

// BuildIdentity is one build of Mjolnir, as far as it can be ordered against
// another.
type BuildIdentity struct {
	// version is the release version without build metadata, in semver
	// form (`2.24.0`, `2.24.0-rc.1`), as Rust's semver::Version displays it.
	version string
	// revision is the full Git revision. Nil for a daemon that predates
	// publishing it.
	revision *string
	// commitTime is the revision's committer time in Unix seconds, when the
	// build knew it.
	commitTime *int64
	// executableModified is when the executable file was last written, in
	// Unix nanoseconds (Rust holds a u128; a uint64 lasts until 2554).
	executableModified *uint64
}

const (
	commitTimePrefix         = "c"
	executableModifiedPrefix = "m"
)

// BuildIdentityParse reads a published `build_version`. A plain version, as
// every daemon before this one published, has no revision or times.
// Unrecognized build metadata is ignored so a later build can publish more.
func BuildIdentityParse(published string) (BuildIdentity, error) {
	panic("unported: mj-client/src/build_identity.rs BuildIdentity::parse")
}

// Published is the `build_version` a daemon running this build publishes.
func (b BuildIdentity) Published() string {
	panic("unported: mj-client/src/build_identity.rs BuildIdentity::published")
}

// Describe is the build for a message a person reads, such as
// `2.24.0+19485f17, committed 2026-09-30 15:31:22 UTC, built 2026-09-30 16:02:09 UTC`.
func (b BuildIdentity) Describe() string {
	panic("unported: mj-client/src/build_identity.rs BuildIdentity::describe")
}

// SameSource is whether b and other were built from the same release
// version and Git revision, whatever their executable files' times. That is
// what identifies a daemon running an image's own copy of this build.
func (b BuildIdentity) SameSource(other BuildIdentity) bool {
	return b.version == other.version &&
		b.revision != nil && other.revision != nil && *b.revision == *other.revision
}

// Version is the release version, without build metadata.
func (b BuildIdentity) Version() string { return b.version }

// thisBuild decides this process's identity once.
var thisBuild = sync.OnceValue(thisBuildIdentity)

// ThisBuild is the identity of the build this process runs.
//
// The file time is read once per process, from the running inode on Linux,
// so replacing the file on disk later does not change what this process says
// about itself. A daemon publishes exactly this value.
func ThisBuild() *BuildIdentity { return thisBuild() }

// thisBuildIdentity reads this build's version, revision, commit time and
// executable time (the body of Rust's OnceLock initializer).
func thisBuildIdentity() *BuildIdentity {
	panic("unported: mj-client/src/build_identity.rs this_build")
}

func runningExecutableModified() *uint64 {
	panic("unported: mj-client/src/build_identity.rs running_executable_modified")
}

// DaemonBuildOrder is how a running daemon's build stands against a
// client's.
type DaemonBuildOrder int

const (
	// DaemonBuildOrderOlder: the client's build is newer: the client
	// replaces the daemon.
	DaemonBuildOrderOlder DaemonBuildOrder = iota
	// DaemonBuildOrderSame: the same build: the client uses the daemon.
	DaemonBuildOrderSame
	// DaemonBuildOrderNewer: the daemon's build is newer: the client must
	// never replace it.
	DaemonBuildOrderNewer
	// DaemonBuildOrderUnknown: the facts do not establish an order: the
	// client must not replace it.
	DaemonBuildOrderUnknown
)

// daemonBuildOrderFromDaemonOrdering maps the daemon's ordering against the
// client (negative, zero, positive, as cmp.Compare returns it).
func daemonBuildOrderFromDaemonOrdering(ordering int) DaemonBuildOrder {
	switch {
	case ordering < 0:
		return DaemonBuildOrderOlder
	case ordering == 0:
		return DaemonBuildOrderSame
	default:
		return DaemonBuildOrderNewer
	}
}

// CompareDaemonBuild orders daemon against client.
//
// Releases are ordered by version. Within one release, different revisions
// are ordered by commit time, and two builds of one revision by when their
// executables were written. A daemon that publishes no revision was built
// before any build published one, so it is the older build. Anything else
// that is missing, or a tie between different revisions, leaves the order
// unknown.
func CompareDaemonBuild(daemon, client *BuildIdentity) DaemonBuildOrder {
	panic("unported: mj-client/src/build_identity.rs compare_daemon_build")
}
