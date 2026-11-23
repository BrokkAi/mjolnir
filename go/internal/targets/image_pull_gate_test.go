// Port of the tests in mj-targets/src/image_pull_gate.rs.

package targets

import (
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// A cancelled Create stops waiting instead of sitting behind a
// multi-gigabyte download.
func TestAWaitingCreateStopsWhenCancelled(t *testing.T) {
	lock := &ImagePullLock{}
	lock.local.Lock()
	defer lock.local.Unlock()

	_, err := HoldImagePull(lock, func() bool { return true }, func() {})
	if err == nil {
		t.Fatal("a cancelled wait must not return a lock it never took")
	}
	if !strings.Contains(err.Error(), "cancelled while waiting for image download") {
		t.Fatal(err)
	}
}

// The same image on the same host is one download; a different host or a
// different image is not.
func TestThePullLockIsSharedPerHostAndImage(t *testing.T) {
	podman := core.ImageHost{Kind: core.ImageHostLocalPodman}
	docker := core.ImageHost{Kind: core.ImageHostLocalDocker}
	first := ImagePullMutex(&podman, "ghcr.io/example/dev:latest")
	again := ImagePullMutex(&podman, "ghcr.io/example/dev:latest")
	if first != again {
		t.Fatal("the same image on the same host must share one lock")
	}

	otherImage := ImagePullMutex(&podman, "ghcr.io/example/other:latest")
	if first == otherImage {
		t.Fatal("another image must have its own lock")
	}

	otherHost := ImagePullMutex(&docker, "ghcr.io/example/dev:latest")
	if first == otherHost {
		t.Fatal("another host must have its own lock")
	}
}

// `mj doctor --smoke` runs its smoke test in its own process while the
// daemon may be downloading the same image. The file lock makes it wait, as
// a goroutine of the daemon would: two locks with the same file stand for
// the two processes.
//
// Hard-won: ae03cbf5: doctor/setup smoke raced the daemon image unpack and failed while the same image was downloading.
func TestASmokeTestInAnotherProcessWaitsForTheDaemonSDownload(t *testing.T) {
	path := filepath.Join(t.TempDir(), "pulls", "image.lock")
	daemon := &ImagePullLock{file: &path}
	setup := &ImagePullLock{file: &path}
	var holding, released atomic.Bool
	var waits atomic.Int64
	var group sync.WaitGroup
	group.Add(1)
	go func() {
		defer group.Done()
		held, err := HoldImagePull(daemon, func() bool { return false }, func() {})
		if err != nil {
			t.Error(err)
			holding.Store(true)
			return
		}
		holding.Store(true)
		time.Sleep(400 * time.Millisecond)
		released.Store(true)
		held.Release()
	}()
	for !holding.Load() {
		time.Sleep(10 * time.Millisecond)
	}
	guard, err := HoldImagePull(setup, func() bool { return false }, func() { waits.Add(1) })
	if err != nil {
		t.Fatal(err)
	}
	if !released.Load() {
		t.Error("the smoke test took the lock while the daemon held it")
	}
	guard.Release()
	group.Wait()
	if got := waits.Load(); got != 1 {
		t.Fatalf("waits = %d, want 1", got)
	}
}
