// Port of mj-client/src/executable.rs.

// Which executable file a running process is actually running.
//
// Two Mjolnir builds can carry the same version string, the same protocol
// number and the same path, and still be different code: a rebuild replaces
// the file while the processes started from the previous one keep running it.
// Nothing in the daemon protocol can tell those apart, so the question is
// answered from the operating system instead, and answered in one place
// because the restart path, `mj daemon status` and `mj doctor` all ask it.
//
// The platform halves live in executable_linux.go, executable_darwin.go,
// executable_windows.go and executable_other.go (Rust's #[cfg] blocks), and
// the file identity Linux and macOS share in executable_identity.go.

package client

import (
	"fmt"
)

// ProcessExecutablePath is the file a process is running, for a message a
// person reads.
//
// Linux names an unlinked executable with a ` (deleted)` suffix. That suffix
// is the fact the reader needs, so it is kept rather than trimmed.
func ProcessExecutablePath(pid uint32) (string, bool) { return processExecutablePath(pid) }

// RunningExecutablePath is the file this process is running, named the same
// way.
func RunningExecutablePath() (string, bool) { return runningExecutablePath() }

// BuildDescription is one Mjolnir build as a message names it: the file it
// runs and the version that build reports.
type BuildDescription struct {
	// Executable is the executable, when the operating system could name it.
	Executable *string
	// Version is the version that build reports.
	Version string
}

// DescribeExecutable names an executable for a message a person reads.
//
// Nil is the answer the operating system gave — the process is gone, its
// executable is unreadable, or the platform does not expose it — so the
// message says that rather than omitting the file.
func DescribeExecutable(path *string) string {
	if path == nil {
		return "an unknown file"
	}
	return *path
}

// DescribeDaemonAndClientBuilds is one sentence naming the daemon's build and
// this client's, for a message a person reads.
//
// Both halves carry a file and a version because either can differ. Two
// builds of one version are different files after a rebuild, and a client
// installed by `cargo install` stays first on PATH while a newer daemon runs
// from a build tree. The paths are what the reader acts on, so they are named
// even when only the versions disagree.
//
// The sentence carries no final period: every caller follows it with its own
// explanation or advice.
func DescribeDaemonAndClientBuilds(daemonPid uint32, daemon, client BuildDescription) string {
	return fmt.Sprintf("Daemon %d runs %s (version %s), while this client runs %s (version %s)",
		daemonPid, DescribeExecutable(daemon.Executable), daemon.Version,
		DescribeExecutable(client.Executable), client.Version)
}

// DescribeRunningDaemonAndClientBuilds is DescribeDaemonAndClientBuilds with
// both executables resolved from the operating system and this build's own
// version on the client side.
func DescribeRunningDaemonAndClientBuilds(daemonPid uint32, daemonVersion string) string {
	// A daemon publishes its revision and times as build metadata; name them
	// the way a person reads them, and this client's the same way.
	daemonBuild := daemonVersion
	if build, err := BuildIdentityParse(daemonVersion); err == nil {
		daemonBuild = build.Describe()
	}
	var daemonExecutable, clientExecutable *string
	if path, ok := processExecutablePath(daemonPid); ok {
		daemonExecutable = &path
	}
	if path, ok := runningExecutablePath(); ok {
		clientExecutable = &path
	}
	return DescribeDaemonAndClientBuilds(daemonPid,
		BuildDescription{Executable: daemonExecutable, Version: daemonBuild},
		BuildDescription{Executable: clientExecutable, Version: ThisBuild().Describe()})
}
