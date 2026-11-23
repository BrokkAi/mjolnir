// Port of mj-core/src/worker_build.rs.

// Daemon and worker compatibility identities shared by the workspace.

package core

import (
	"bytes"
	"crypto/sha256"
	"errors"
	"fmt"
	"io"
	"os"
	"strings"
	"sync"
	"testing"
	"unicode/utf8"
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
	// Generate the dummy digest so no literal test revision is linked into a
	// program that uses the shared identity accessors.
	id := fmt.Sprintf("%s+%040x", PackageVersion, 0)
	return BuildIdentity{BuildID: id, WorkerBuildID: id}
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
	if !strings.Contains(testWorkerBuildStamp, testRevision) {
		panic("the test worker stamp names the test revision")
	}
	// Keep the stamp's bytes in the executable: only a test that calls this
	// links it.
	testWorkerStampSink = testWorkerBuildStamp
	path, err := os.Executable()
	if err != nil {
		panic(fmt.Sprintf("locate the running test executable: %v", err))
	}
	return path
}

// testWorkerStampSink holds the test stamp so the linker keeps it, as Rust's
// black_box does.
var testWorkerStampSink string

func installed() BuildIdentity {
	installedIdentity.mu.Lock()
	defer installedIdentity.mu.Unlock()
	if installedIdentity.identity == nil {
		// A test binary has no `main` of ours to install one.
		if !testing.Testing() && !TestFixturesEnabled() {
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
	builds := []string{}
	for offset := 0; ; {
		index := bytes.Index(data[offset:], prefix)
		if index < 0 {
			return builds
		}
		start := offset + index + len(prefix)
		offset = start
		tail := data[start:]
		window := tail[:min(stampBytes, len(tail))]
		end := bytes.IndexByte(window, 0)
		if end < 0 {
			continue
		}
		if build := tail[:end]; utf8.Valid(build) && IsWorkerBuild(string(build)) {
			builds = append(builds, string(build))
		}
	}
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

type ExecutableContents struct {
	// digest is nil only for a stamp-only read of an unpublished file.
	digest *string
	// stamp is the build found, or stampErr when there is none or several.
	stamp    string
	stampErr error
}

func executableContentsRead(input *os.File, hash bool) (*ExecutableContents, error) {
	// Read each byte once for both the digest and build stamp. Only the
	// stamp parser needs to see bytes from the previous chunk again.
	overlap := len(prefix) + stampBytes
	window := make([]byte, 0, readBytes+overlap)
	chunk := make([]byte, readBytes)
	digest := sha256.New()
	found := ""
	var stampErr error
	for {
		n, err := io.ReadFull(input, chunk)
		if err != nil && err != io.EOF && err != io.ErrUnexpectedEOF {
			return nil, err
		}
		if n == 0 {
			break
		}
		window = append(window, chunk[:n]...)
		if hash {
			digest.Write(chunk[:n])
		}
		if stampErr == nil {
			for _, build := range workerBuildStamps(window) {
				if found == "" {
					found = build
				} else if found != build {
					stampErr = errors.New("conflicting worker build stamps")
					break
				}
			}
		}
		keepFrom := max(0, len(window)-overlap)
		window = window[:copy(window, window[keepFrom:])]
	}
	contents := &ExecutableContents{stamp: found, stampErr: stampErr}
	if stampErr == nil && found == "" {
		contents.stampErr = errors.New("missing worker build stamp (legacy or invalid worker)")
	}
	if hash {
		sum := LowerHex(digest.Sum(nil))
		contents.digest = &sum
	}
	return contents, nil
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
	if m.byPath == nil {
		m.byPath = map[string]ExecutableIdentity{}
		m.byIdentity = map[ExecutableIdentity]*MemoizedExecutable{}
	}
	if previous, ok := m.byPath[path]; !ok || previous != identity {
		m.byPath[path] = identity
		if ok {
			entry, indexed := m.byIdentity[previous]
			if !indexed {
				panic("indexed executable")
			}
			entry.paths--
			if entry.paths == 0 {
				delete(m.byIdentity, previous)
			}
		}
		entry, present := m.byIdentity[identity]
		if !present {
			entry = &MemoizedExecutable{read: &ExecutableRead{}}
			m.byIdentity[identity] = entry
		}
		entry.paths++
	}
	return m.byIdentity[identity].read
}

func executableContents(path string) (*ExecutableContents, error) {
	input, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("open worker executable %s: %w", path, err)
	}
	defer input.Close()
	var contents *ExecutableContents
	if executableMemoSupported {
		contents, err = memoizedExecutableContents(path, input)
	} else {
		// Path, size and mtime cannot distinguish same-size replacements
		// with preserved timestamps on Windows. Read the file each time
		// there.
		contents, err = executableContentsRead(input, true)
	}
	if err != nil {
		return nil, fmt.Errorf("read worker executable %s: %w", path, err)
	}
	return contents, nil
}

// executableMemo is the static `MEMO` inside Rust's
// `memoized_executable_contents`.
var executableMemo struct {
	mu   sync.Mutex
	memo ExecutableMemo
}

func memoizedExecutableContents(path string, input *os.File) (*ExecutableContents, error) {
	// Stat the open file we will read. Publication by rename supplies a new
	// inode. An in-place write updates ctime even if the writer restores
	// mtime. Keep both timestamps at their full nanosecond resolution.
	info, err := input.Stat()
	if err != nil {
		return nil, err
	}
	identity := executableIdentityFromMetadata(info)
	executableMemo.mu.Lock()
	read := executableMemo.memo.lookup(path, identity)
	executableMemo.mu.Unlock()
	// Concurrent launches of this file share the read. Other binaries can be
	// opened and read independently.
	read.mu.Lock()
	defer read.mu.Unlock()
	if read.contents != nil {
		return read.contents, nil
	}
	scanned, err := executableContentsRead(input, true)
	if err != nil {
		return nil, err
	}
	after, err := input.Stat()
	if err != nil {
		return nil, err
	}
	if identity != executableIdentityFromMetadata(after) {
		return nil, errors.New("worker executable changed while being read")
	}
	read.contents = scanned
	return scanned, nil
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
