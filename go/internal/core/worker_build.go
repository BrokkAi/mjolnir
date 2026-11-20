// Port of mj-core/src/worker_build.rs.

// Daemon and worker compatibility identities shared by the workspace.

package core

import (
	"bytes"
	"errors"
	"fmt"
	"os"
	"sync"
	"testing"
)

// PackageVersion is the workspace package version (Rust's
// `CARGO_PKG_VERSION` for mj-core); it tracks the Rust workspace.
const PackageVersion = "2.73.0"

// testRevision is the digest part of the identity a test binary reports: no
// commit has it. Keep it equal to `TEST_REVISION` in `build.rs`, which stamps
// the fake worker fixture with it.
const testRevision = "0000000000000000000000000000000000000000"

// testWorkerBuildStamp is the worker stamp of the test identity. Only
// TestWorkerExecutable names it, so its bytes are in a test executable that
// hands itself over as a worker and in nothing else.
const testWorkerBuildStamp = "\x00MJ-WORKER-BUILD:" + PackageVersion + "+0000000000000000000000000000000000000000\x00"

// BuildIdentity is the identities of the running program's build.
//
// A program installs this first thing in `main` (Install) and library code
// reads it through BuildID, WorkerBuildID and CommitTime. Library packages do
// not compile the values in: a value that changes with every commit would
// make each package that names it stale after every commit.
type BuildIdentity struct {
	// BuildID is the package version, `+`, and the commit: see BuildID.
	BuildID string
	// WorkerBuildID is the package version, `+`, and the worker input
	// digest: see WorkerBuildID.
	WorkerBuildID string
	// CommitTime is the commit's time in Unix seconds, or empty when unknown.
	CommitTime string
	// ReleaseImageCarriesWorker: see ReleaseImageCarriesWorker.
	ReleaseImageCarriesWorker bool
}

// buildIdentityForTests is what a test binary reports. It names no commit, so
// it is the same after every commit and a test build stays fresh; its commit
// time is unknown, as for a build without Git.
func buildIdentityForTests() BuildIdentity {
	return BuildIdentity{
		BuildID:       PackageVersion + "+" + testRevision,
		WorkerBuildID: PackageVersion + "+" + testRevision,
	}
}

// installedIdentity is Rust's `INSTALLED` (renamed beside the installed
// function).
var installedIdentity struct {
	mu       sync.Mutex
	identity *BuildIdentity
}

// Install records the identity of the running program. Call it once, first
// thing in `main`.
//
// It panics when a different identity is already installed: a process has
// one build.
func Install(identity BuildIdentity) {
	installedIdentity.mu.Lock()
	defer installedIdentity.mu.Unlock()
	if installedIdentity.identity == nil {
		installedIdentity.identity = &identity
	}
	if *installedIdentity.identity != identity {
		panic("a process cannot install two build identities")
	}
}

// TestWorkerExecutable is the running test executable, for a test that hands
// it to the code under test as the worker binary to verify and install.
//
// A worker is recognised by a build stamp in its file. A real worker carries
// its own, which its entry point keeps; a test executable has no such entry
// point, so it gets the test identity's stamp by calling this. The stamp must
// not be anywhere else (#1603).
func TestWorkerExecutable() string {
	panic("unported: mj-core/src/worker_build.rs test_worker_executable")
}

func installed() BuildIdentity {
	installedIdentity.mu.Lock()
	defer installedIdentity.mu.Unlock()
	if installedIdentity.identity == nil {
		// A test binary has no `main` of ours to install one.
		if !testing.Testing() {
			panic("the program did not install its build identity before using it")
		}
		identity := buildIdentityForTests()
		installedIdentity.identity = &identity
	}
	return *installedIdentity.identity
}

// BuildID is the identity of this daemon build: package version plus commit.
// It orders daemon replacements and remains visible to users and clients.
func BuildID() string { return installed().BuildID }

// WorkerBuildID is the compatibility identity of workers built from the same
// inputs: the package version plus a digest of the worker's inputs at the
// commit.
func WorkerBuildID() string { return installed().WorkerBuildID }

// CommitTime is the committer time, in Unix seconds, of the revision in
// BuildID, or empty when the build had neither Git nor
// `MJ_BUILD_COMMIT_TIME`.
func CommitTime() string { return installed().CommitTime }

// ReleaseImageCarriesWorker reports whether this is a release build whose
// container image (ReleaseContainerImage) carries its own worker. The release
// workflow publishes that image with the worker built from the same commit,
// whose identity equals this build's, so a container on it needs no worker
// from the host. Neither a development build nor a crates.io package, whose
// identity falls back to the commit, is.
func ReleaseImageCarriesWorker() bool { return installed().ReleaseImageCarriesWorker }

var workerBuildStampOnce = sync.OnceValue(func() string {
	return "\x00MJ-WORKER-BUILD:" + WorkerBuildID() + "\x00"
})

// WorkerBuildStamp is the bytes a worker executable carries so its build can
// be read from the file: WorkerBuildID between the stamp's markers.
func WorkerBuildStamp() string { return workerBuildStampOnce() }

var prefix = []byte("\x00MJ-WORKER-BUILD:")

const (
	// stampBytes preserves the existing stamp parser's search window,
	// including its terminator.
	stampBytes = 192
	readBytes  = 128 * 1024
)

// IsWorkerBuild reports whether build has the shape of a worker
// compatibility identity: a package version, `+`, and a 40-digit hexadecimal
// digest. The version must look like one (a digit first, then digits,
// letters, dots and hyphens), so unrelated text the linker places between the
// stamp prefix constant and the build id literal cannot read as a second
// stamp.
func IsWorkerBuild(build string) bool {
	index := bytes.LastIndexByte([]byte(build), '+')
	if index < 0 {
		return false
	}
	version, revision := build[:index], build[index+1:]
	if version == "" || version[0] < '0' || version[0] > '9' || len(revision) != 40 {
		return false
	}
	for i := 0; i < len(version); i++ {
		c := version[i]
		if !(harnessRuntimeIsASCIIAlphanumeric(c) || c == '.' || c == '-') {
			return false
		}
	}
	for i := 0; i < len(revision); i++ {
		c := revision[i]
		if !(('0' <= c && c <= '9') || ('a' <= c && c <= 'f') || ('A' <= c && c <= 'F')) {
			return false
		}
	}
	return true
}

func workerBuildStamps(data []byte) []string {
	panic("unported: mj-core/src/worker_build.rs worker_build_stamps")
}

func WorkerBuildFromBytes(data []byte) (string, error) {
	found := ""
	for _, build := range workerBuildStamps(data) {
		if found != "" && found != build {
			return "", errors.New("conflicting worker build stamps")
		}
		found = build
	}
	if found == "" {
		return "", errors.New("missing worker build stamp (legacy or invalid worker)")
	}
	return found, nil
}

// ExecutableIdentity is Unix-only in Rust.
type ExecutableIdentity struct {
	device   uint64
	inode    uint64
	size     uint64
	modified [2]int64
	changed  [2]int64
}

func executableIdentityFromMetadata(info os.FileInfo) ExecutableIdentity {
	panic("unported: mj-core/src/worker_build.rs ExecutableIdentity::from_metadata")
}

type ExecutableContents struct {
	// digest is nil only for a stamp-only read of an unpublished file.
	digest *string
	// stamp is the build found, or stampErr when there is none or several.
	stamp    string
	stampErr error
}

func executableContentsRead(input *os.File, hash bool) (*ExecutableContents, error) {
	panic("unported: mj-core/src/worker_build.rs ExecutableContents::read")
}

// ExecutableRead is Rust's `Arc<Mutex<Option<Arc<ExecutableContents>>>>`:
// concurrent launches of one file share a single read.
type ExecutableRead struct {
	mu       sync.Mutex
	contents *ExecutableContents
}

type MemoizedExecutable struct {
	read  *ExecutableRead
	paths int
}

type ExecutableMemo struct {
	byIdentity map[ExecutableIdentity]*MemoizedExecutable
	byPath     map[string]ExecutableIdentity
}

func (m *ExecutableMemo) lookup(path string, identity ExecutableIdentity) *ExecutableRead {
	panic("unported: mj-core/src/worker_build.rs ExecutableMemo::lookup")
}

func executableContents(path string) (*ExecutableContents, error) {
	panic("unported: mj-core/src/worker_build.rs executable_contents")
}

// executableMemo is the static `MEMO` inside Rust's
// `memoized_executable_contents`.
var executableMemo struct {
	mu   sync.Mutex
	memo ExecutableMemo
}

func memoizedExecutableContents(path string, input *os.File) (*ExecutableContents, error) {
	panic("unported: mj-core/src/worker_build.rs memoized_executable_contents")
}

func executableDigest(path string) (string, error) {
	contents, err := executableContents(path)
	if err != nil {
		return "", err
	}
	if contents.digest == nil {
		return "", errors.New("worker executable was read without its digest")
	}
	return *contents.digest, nil
}

func VerifyWorkerBuild(path string) error {
	contents, err := executableContents(path)
	if err != nil {
		return err
	}
	return checkWorkerStamp(path, contents.stamp, contents.stampErr)
}

// VerifyUnpublishedWorkerBuild is VerifyWorkerBuild for a file that is about
// to be published under another name, such as a download's temporary file:
// one stamp-only scan, outside the memo, so one-off paths are never recorded
// there.
func VerifyUnpublishedWorkerBuild(path string) error {
	input, err := os.Open(path)
	if err != nil {
		return fmt.Errorf("open worker executable %s: %w", path, err)
	}
	defer input.Close()
	contents, err := executableContentsRead(input, false)
	if err != nil {
		return fmt.Errorf("read worker executable %s: %w", path, err)
	}
	return checkWorkerStamp(path, contents.stamp, contents.stampErr)
}

func checkWorkerStamp(path, stamp string, stampErr error) error {
	if stampErr != nil {
		return fmt.Errorf("worker %s; expected build %s: %w", path, WorkerBuildID(), stampErr)
	}
	if stamp != WorkerBuildID() {
		return fmt.Errorf("worker %s has build %s; expected build %s", path, stamp, WorkerBuildID())
	}
	return nil
}
