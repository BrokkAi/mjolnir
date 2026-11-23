// Port of fake_worker_dispatcher in mj-build-id/src/lib.rs and the stand-in
// build.rs stamps. Rust builds it on Unix whenever the workspace fixture is
// present, no longer only under the removed `test-hooks` feature.

//go:build unix

package buildid

import (
	"crypto/sha256"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"syscall"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// FakeWorkerDispatcher is a worker stand-in carrying this build's worker
// stamp, for a test in which a real program verifies the worker: core's
// checked-in fake command (testdata/test_hooks/fake-command.sh) with
// "\n#MJ-BUILD-ID:<WorkerBuildID>\n#\x00MJ-WORKER-BUILD:<WorkerBuildID>\x00\n"
// appended, mode 0755. Rust writes it at build time so executing it cannot
// race a test's writer (ETXTBSY); it must be written before tests start
// executing commands.
func FakeWorkerDispatcher() string { return fakeWorkerDispatcher() }

var fakeWorkerDispatcher = sync.OnceValue(func() string {
	script, err := os.ReadFile(core.FakeCommandDispatcher())
	if err != nil {
		panic(fmt.Sprintf("fake command fixture: %v", err))
	}
	identity := WorkerBuildID()
	stamped := fmt.Sprintf("%s\n#MJ-BUILD-ID:%s\n#\x00MJ-WORKER-BUILD:%s\x00\n", script, identity, identity)
	sum := sha256.Sum256([]byte(stamped))
	path := filepath.Join(os.TempDir(), "mj-go-build-fake-worker-"+core.LowerHex(sum[:8])+".sh")
	// The name is the content's digest, so a file already there was
	// written complete by an earlier run.
	if existing, err := os.ReadFile(path); err == nil && string(existing) == stamped {
		return path
	}
	// Rust generates this file at build time, before any test runs. Here it
	// is written with the fork lock held, so no child started meanwhile
	// inherits the write descriptor (ETXTBSY), and renamed into place
	// complete.
	syscall.ForkLock.Lock()
	defer syscall.ForkLock.Unlock()
	temporary := fmt.Sprintf("%s.%d.tmp", path, os.Getpid())
	if err := os.WriteFile(temporary, []byte(stamped), 0o755); err != nil {
		panic(fmt.Sprintf("write stamped fake worker: %v", err))
	}
	if err := os.Chmod(temporary, 0o755); err != nil {
		panic(fmt.Sprintf("make fake worker executable: %v", err))
	}
	if err := os.Rename(temporary, path); err != nil {
		panic(fmt.Sprintf("install stamped fake worker: %v", err))
	}
	return path
})
