// Port of mj-build-id/src/lib.rs and the identity half of mj-build-id/build.rs.
//
// The identity of the build a Mjolnir program was compiled from: its commit,
// that commit's time, and the digest of the worker's inputs.
//
// Only the programs (`mj` and the worker) import this package. A program
// installs Identity first thing in `main` (core.Install) and library code
// reads it from there, which is what keeps a commit from recompiling the
// libraries.
//
// Rust computes these values in build.rs. Go records the commit and its time
// itself (runtime/debug.ReadBuildInfo, `vcs.revision` and `vcs.time`); the
// rest comes from go/scripts/build.sh with `-ldflags -X`: the worker input
// digest, the release channel, and the MJ_BUILD_REVISION and
// MJ_BUILD_COMMIT_TIME overrides of a source tree without Git. build.sh also
// replaces stamp.go through `go build -overlay`, because `-X` cannot carry
// the NUL bytes of the worker stamp.

package buildid

import (
	"runtime/debug"
	"sync"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Values go/scripts/build.sh sets with `-ldflags -X` (Rust build.rs's
// `cargo:rustc-env` outputs). Each is empty in a plain `go build`.
var (
	// buildRevision is MJ_BUILD_REVISION: the full 40-character commit of a
	// source tree without Git. Empty: the binary's `vcs.revision`.
	buildRevision string
	// buildCommitTime is MJ_BUILD_COMMIT_TIME: the commit's Unix time in
	// seconds, what `git log -1 --format=%ct` prints. Empty: the binary's
	// `vcs.time`, when it belongs to the same revision.
	buildCommitTime string
	// workerInputsID is the 40-digit digest of the worker's committed inputs
	// (or MJ_WORKER_INPUTS_ID). Empty when the build had no Git: the revision
	// then stands in, as in Rust.
	workerInputsID string
	// buildChannel is MJ_BUILD_CHANNEL; "release" marks a release build.
	buildChannel string
)

// workerStampPrefix opens the bytes that mark a worker executable with its
// worker build identity (core's stamp parser looks for the same prefix).
const workerStampPrefix = "\x00MJ-WORKER-BUILD:"

// resolvedIdentity is the build's identity facts, validated once.
type resolvedIdentity struct {
	// revision is the build's commit in 40 lowercase hexadecimal digits.
	revision string
	// commitTime is the commit's time in Unix seconds, or empty when the
	// build had neither Git nor MJ_BUILD_COMMIT_TIME.
	commitTime string
	// workerInputs is the worker input digest in 40 lowercase hexadecimal
	// digits, or the revision when the build had no Git.
	workerInputs string
}

// resolved decides the identity once. It panics, with the message Rust's
// build script fails with, when the build has no revision: a Go binary
// cannot refuse to build, so it refuses to start.
var resolved = sync.OnceValue(resolveIdentity)

// resolveIdentity reads the `-X` values and the binary's VCS settings and
// validates them as build.rs does (`MJ_BUILD_REVISION must be a full
// 40-character Git commit`, `worker input identity must be a full
// 40-character digest`, and without any revision `worker build identity
// needs Git; for a source archive set MJ_BUILD_REVISION to its full Git
// commit`). The commit time is the `-X` value when set, else `vcs.time` (as
// Unix seconds) only when `vcs.revision` is the resolved revision: the time
// belongs to that commit or is unknown, never guessed.
func resolveIdentity() resolvedIdentity {
	panic("unported: mj-build-id/build.rs main")
}

// vcsSettings returns the binary's `vcs.revision` and `vcs.time` build
// settings, each empty when absent (`go run`, `go test`, or a build outside
// Git).
func vcsSettings() (revision, time string) {
	info, ok := debug.ReadBuildInfo()
	if !ok {
		return "", ""
	}
	for _, setting := range info.Settings {
		switch setting.Key {
		case "vcs.revision":
			revision = setting.Value
		case "vcs.time":
			time = setting.Value
		}
	}
	return revision, time
}

// isFullDigest reports whether text is 40 hexadecimal digits, the shape of
// a Git commit and of a worker input digest.
func isFullDigest(text string) bool {
	if len(text) != 40 {
		return false
	}
	for i := 0; i < len(text); i++ {
		c := text[i]
		if !('0' <= c && c <= '9' || 'a' <= c && c <= 'f' || 'A' <= c && c <= 'F') {
			return false
		}
	}
	return true
}

// BuildID is the identity of this build: package version plus commit. It
// orders daemon replacements and remains visible to users and clients.
func BuildID() string { return core.PackageVersion + "+" + resolved().revision }

// WorkerBuildID is the compatibility identity of workers built from the same
// inputs. The digest is based on Git trees at `HEAD`, so uncommitted changes
// are not reflected, matching BuildID.
func WorkerBuildID() string { return core.PackageVersion + "+" + resolved().workerInputs }

// BuildCommitTime is the committer time, in Unix seconds, of the revision in
// BuildID, or empty when the build had neither Git nor
// `MJ_BUILD_COMMIT_TIME`.
func BuildCommitTime() string { return resolved().commitTime }

// WorkerBuildStamp is the bytes that mark a worker executable with
// WorkerBuildID. The worker's entry point references this so the linker
// cannot discard it. It panics in a build that carries no stamp (a plain
// `go build`, not go/scripts/build.sh) or whose stamp disagrees with
// WorkerBuildID: such a binary must not run as a worker.
func WorkerBuildStamp() string {
	if workerBuildStampLiteral != workerStampPrefix+WorkerBuildID()+"\x00" {
		panic("this build carries no worker stamp for " + WorkerBuildID() + "; build it with go/scripts/build.sh")
	}
	return workerBuildStampLiteral
}

// ReleaseImageCarriesWorker reports whether this is a release build whose
// container image carries its own worker. The release workflow publishes
// that image with the worker built from the same commit, whose identity
// equals this build's, so a container on it needs no worker from the host.
// A development build is not one, and neither is a build whose worker
// identity fell back to the commit, which can never match the image's.
func ReleaseImageCarriesWorker() bool {
	identity := resolved()
	return buildChannel == "release" && identity.workerInputs != identity.revision
}

// Identity is what a program installs at startup.
func Identity() core.BuildIdentity {
	return core.BuildIdentity{
		BuildID:                   BuildID(),
		WorkerBuildID:             WorkerBuildID(),
		CommitTime:                BuildCommitTime(),
		ReleaseImageCarriesWorker: ReleaseImageCarriesWorker(),
	}
}
