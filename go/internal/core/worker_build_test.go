// Port of the tests in mj-core/src/worker_build.rs.
//
// The Rust module also tests mj-core/worker_build_inputs.py and the Cargo
// dependency closure in worker-build-inputs.txt; those belong to the Rust
// build's worker identity and have no Go counterpart.

package core

import (
	"bytes"
	"crypto/sha256"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"
)

func workerBuildSHA(data []byte) string {
	digest := sha256.Sum256(data)
	return LowerHex(digest[:])
}

// Hard-won: daemon heap profile: verification must handle stamps across bounded reads.
func TestReadsAForeignBinaryWithoutExecutingIt(t *testing.T) {
	path := filepath.Join(t.TempDir(), "linux-worker")
	for _, offset := range []int{
		readBytes - len(prefix) - 10,
		readBytes - len(prefix),
		readBytes - 2,
		readBytes + 3,
		150_000,
	} {
		data := bytes.Repeat([]byte{0xff}, offset)
		data = append(data, WorkerBuildStamp()...)
		data = append(data, bytes.Repeat([]byte{0xff}, readBytes)...)
		// Matching stamps in separate chunks are valid, too.
		data = append(data, WorkerBuildStamp()...)
		if err := os.WriteFile(path, data, 0o644); err != nil {
			t.Fatal(err)
		}
		build, err := WorkerBuildFromBytes(data)
		if err != nil || build != WorkerBuildID() {
			t.Fatalf("offset %d: %q %v", offset, build, err)
		}
		if err := VerifyWorkerBuild(path); err != nil {
			t.Fatalf("offset %d: %v", offset, err)
		}
	}
}

// Hard-won: b93657092: same-size rewrites with restored mtime kept the old digest and stamp.
func TestWorkerExecutableMemoRefreshesAfterRenameAndRewrite(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "worker")
	replacement := filepath.Join(directory, "replacement")
	modified := time.Unix(1_600_000_000, 0)
	write := func(path string, data []byte, modified time.Time) {
		t.Helper()
		file, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o644)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := file.Write(data); err != nil {
			t.Fatal(err)
		}
		if err := file.Close(); err != nil {
			t.Fatal(err)
		}
		if err := os.Chtimes(path, time.Time{}, modified); err != nil {
			t.Fatal(err)
		}
	}
	digest := func() string {
		t.Helper()
		value, err := WorkerExecutableDigest(path)
		if err != nil {
			t.Fatal(err)
		}
		return value
	}

	data := bytes.Repeat([]byte{0xff}, readBytes-len(prefix))
	data = append(data, WorkerBuildStamp()...)
	write(path, data, modified)
	if err := VerifyWorkerBuild(path); err != nil {
		t.Fatal(err)
	}
	var original *ExecutableContents
	if executableMemoSupported {
		var err error
		if original, err = executableContents(path); err != nil {
			t.Fatal(err)
		}
	}
	if digest() != workerBuildSHA(data) {
		t.Fatal("digest of the original")
	}
	if executableMemoSupported {
		again, err := executableContents(path)
		if err != nil {
			t.Fatal(err)
		}
		if again != original {
			t.Fatal("the memo read the unchanged file again")
		}
	}

	stale := bytes.Clone(data)
	stale[len(stale)-2] = 'a'
	write(replacement, stale, modified)
	if err := os.Rename(replacement, path); err != nil {
		t.Fatal(err)
	}
	if digest() != workerBuildSHA(stale) {
		t.Fatal("digest after rename")
	}
	if VerifyWorkerBuild(path) == nil {
		t.Fatal("stale stamp verified after rename")
	}

	// Rewrite the existing file with the same size and a new mtime.
	data[0] = 0xee
	write(path, data, modified.Add(4*time.Second))
	if err := VerifyWorkerBuild(path); err != nil {
		t.Fatal(err)
	}
	if digest() != workerBuildSHA(data) {
		t.Fatal("digest after rewrite")
	}

	if runtime.GOOS != "windows" {
		// Let ctime tick even on filesystems whose clock advances at
		// scheduler tick granularity.
		time.Sleep(20 * time.Millisecond)
		info, err := os.Stat(path)
		if err != nil {
			t.Fatal(err)
		}
		originalMtime := info.ModTime()
		write(path, stale, originalMtime)
		info, err = os.Stat(path)
		if err != nil {
			t.Fatal(err)
		}
		if !info.ModTime().Equal(originalMtime) {
			t.Fatal("mtime not restored")
		}
		if VerifyWorkerBuild(path) == nil {
			t.Fatal("stale stamp verified after an in-place rewrite")
		}
		if digest() != workerBuildSHA(stale) {
			t.Fatal("digest after an in-place rewrite")
		}
	}
}

// Hard-won: 740cb30c5: a layout change put diagnostic text and the bare build id after the prefix constant, read as a conflicting stamp.
func TestPrefixConstantFollowedByTextAndBuildIDIsNotAStamp(t *testing.T) {
	data := bytes.Clone(prefix)
	data = append(data, "read worker build stampconflicting worker build stampsmissing worker build stamp (legacy or invalid worker)"...)
	data = append(data, WorkerBuildID()...)
	data = append(data, WorkerBuildStamp()...)
	build, err := WorkerBuildFromBytes(data)
	if err != nil || build != WorkerBuildID() {
		t.Fatalf("%q %v", build, err)
	}
}

func TestRejectsMissingTruncatedStaleAndConflictingStamps(t *testing.T) {
	if _, err := WorkerBuildFromBytes([]byte("legacy worker")); err == nil {
		t.Fatal("legacy worker accepted")
	}
	if _, err := WorkerBuildFromBytes([]byte(strings.TrimRight(WorkerBuildStamp(), "\x00"))); err == nil {
		t.Fatal("truncated stamp accepted")
	}
	stale := "\x00MJ-WORKER-BUILD:0.1.0+" + strings.Repeat("a", 40) + "\x00"
	path := filepath.Join(t.TempDir(), "stale-worker")
	if err := os.WriteFile(path, []byte(stale), 0o644); err != nil {
		t.Fatal(err)
	}
	err := VerifyWorkerBuild(path)
	if err == nil || !strings.Contains(err.Error(), "0.1.0+") || !strings.Contains(err.Error(), WorkerBuildID()) || !strings.Contains(err.Error(), "stale-worker") {
		t.Fatal(err)
	}
	conflicting := WorkerBuildStamp() + stale
	if _, err := WorkerBuildFromBytes([]byte(conflicting)); err == nil || !strings.Contains(err.Error(), "conflicting") {
		t.Fatal(err)
	}
	data := []byte(WorkerBuildStamp())
	data = append(data, bytes.Repeat([]byte{0xff}, 2*readBytes-len(prefix)-len(data))...)
	data = append(data, stale...)
	if err := os.WriteFile(path, data, 0o644); err != nil {
		t.Fatal(err)
	}
	err = VerifyWorkerBuild(path)
	if err == nil || !strings.Contains(err.Error(), "conflicting") || !strings.Contains(err.Error(), "stale-worker") {
		t.Fatal(err)
	}
	data = data[:readBytes-2]
	for i := range len(WorkerBuildStamp()) {
		data[i] = 0xff
	}
	data = append(data, strings.TrimRight(WorkerBuildStamp(), "\x00")...)
	if err := os.WriteFile(path, data, 0o644); err != nil {
		t.Fatal(err)
	}
	if err := VerifyWorkerBuild(path); err == nil || !strings.Contains(err.Error(), "missing") {
		t.Fatal(err)
	}
}
