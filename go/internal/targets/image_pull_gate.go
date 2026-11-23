// Port of mj-targets/src/image_pull_gate.rs.
//
// Coordination between the daemon's background image download and a session
// launch that needs the same image.
//
// The daemon downloads every configured container image shortly after it
// starts. A person who creates a session during that download must not
// start a second download of the same image: the two would compete for the
// same bandwidth and the same layer store. Instead the launch waits for the
// one already running, and then finds the image present.
//
// There is one lock per (host, image) pair. Whoever is allowed to download
// holds it; whoever needs the image waits on it.
//
// The lock also holds a file lock in the instance's data directory, because
// `mj doctor --smoke` runs its smoke test in its own process while the
// daemon may be downloading the same image. Two engines pulling one image at
// once can fail to extract a layer.

package targets

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"time"
	"weak"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// pollInterval is how long a waiter sleeps between attempts on the lock.
//
// A download runs for minutes, so polling this slowly costs nothing and
// keeps the wait cancellable without a condition variable: both the
// refresher and a launch run on blocking goroutines through the synchronous
// CommandExecutor, and either can be asked to stop while it waits.
const pollInterval = 250 * time.Millisecond

var (
	imagePullLocksMu sync.Mutex
	// imagePullLocks holds the live locks by "<host label>|<image>"; an
	// entry nobody holds any more is dropped on the next lookup.
	imagePullLocks = map[string]weak.Pointer[ImagePullLock]{}
)

// ImagePullMutex is the lock covering downloads of one image on one host.
//
// Keyed by host label and image reference, which is what identifies a copy
// of an image: two targets naming the same image on the same host share one
// download, and the same image on two hosts does not.
func ImagePullMutex(host *core.ImageHost, image string) *ImagePullLock {
	key := host.Label() + "|" + image
	imagePullLocksMu.Lock()
	defer imagePullLocksMu.Unlock()
	for entry, lock := range imagePullLocks {
		if lock.Value() == nil {
			delete(imagePullLocks, entry)
		}
	}
	// Fixture processes with an isolated store still use the cross-process
	// lock. Other in-process tests must keep the live store untouched.
	unisolatedFixture := core.TestFixturesEnabled() &&
		core.EnsureMayControlStore(core.DataDir(), "lock an image download") != nil
	var file *string
	if !cfgTest && !unisolatedFixture {
		path := lockFilePath(key)
		file = &path
	}
	if lock := imagePullLocks[key].Value(); lock != nil {
		return lock
	}
	lock := &ImagePullLock{file: file}
	imagePullLocks[key] = weak.Make(lock)
	return lock
}

// lockFilePath is the file other Mjolnir processes of this instance lock for
// the same (host, image) pair. The name is a hash, because an image
// reference and a host label can hold characters a file name cannot.
func lockFilePath(key string) string {
	digest := sha256.Sum256([]byte(key))
	name := hex.EncodeToString(digest[:12])
	return filepath.Join(core.DataDir(), "image-pulls", name+".lock")
}

// ImagePullLock is the right to download one image on one host: a lock for
// the goroutines of this process and, when file is set, a file lock for
// other processes.
type ImagePullLock struct {
	local sync.Mutex
	file  *string
}

// ImagePullGuard is a held ImagePullLock. Release (Rust's Drop) releases
// both locks.
type ImagePullGuard struct {
	local *sync.Mutex
	file  *core.FileLock
}

// Release gives up both locks, in Rust's field drop order: the process lock,
// then the file lock.
func (g *ImagePullGuard) Release() {
	if g.local != nil {
		g.local.Unlock()
		g.local = nil
	}
	if g.file != nil {
		g.file.Drop()
		g.file = nil
	}
}

// tryLockFile tries the file lock once. A nil lock means another process
// holds it.
func tryLockFile(path string) (*core.FileLock, error) {
	if parent := filepath.Dir(path); parent != "" {
		if err := os.MkdirAll(parent, 0o777); err != nil {
			return nil, fmt.Errorf("create image download lock directory %s: %w", parent, err)
		}
	}
	file, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY, 0o666)
	if err != nil {
		return nil, fmt.Errorf("open image download lock %s: %w", path, err)
	}
	locked, err := libTryLockFile(file)
	if err != nil {
		_ = file.Close()
		return nil, fmt.Errorf("lock image download lock %s: %w", path, err)
	}
	if !locked {
		_ = file.Close()
		return nil, nil
	}
	return core.NewFileLock(file), nil
}

// HoldImagePull takes the right to download one image, waiting for whoever
// holds it.
//
// onWait runs once, the first time the lock is found busy, so a caller can
// tell the user it is waiting without saying anything in the common case
// where nothing is downloading. isCancelled is polled while waiting so a
// quitting daemon or a cancelled Create does not sit here for minutes.
func HoldImagePull(lock *ImagePullLock, isCancelled func() bool, onWait func()) (*ImagePullGuard, error) {
	for {
		if lock.local.TryLock() {
			if lock.file == nil {
				return &ImagePullGuard{local: &lock.local}, nil
			}
			file, err := tryLockFile(*lock.file)
			if err != nil {
				lock.local.Unlock()
				return nil, err
			}
			if file != nil {
				return &ImagePullGuard{local: &lock.local, file: file}, nil
			}
			// Another process is downloading; let this process's other
			// waiters see the same state while this one sleeps.
			lock.local.Unlock()
		}
		if onWait != nil {
			wait := onWait
			onWait = nil
			wait()
		}
		if isCancelled() {
			return nil, errors.New("cancelled while waiting for image download")
		}
		time.Sleep(pollInterval)
	}
}

// WithImageReady runs work with nothing downloading this target's image
// underneath it.
//
// For a container target this waits for a background download of the same
// image on the same host. Contention is reported as queue time; once this
// caller owns the lock, work runs under the "Pull image" stage. A target
// that runs no image just runs work.
func WithImageReady[T any](target *core.TargetsTargetTemplate, executor core.CommandExecutor, work func() (T, error)) (T, error) {
	host, container, ok := target.ImageHost()
	if !ok {
		return work()
	}
	image := container.Image
	lock := ImagePullMutex(&host, image)
	var waiting *core.ProvisionStageGuard
	guard, err := HoldImagePull(lock, executor.CancellationRequested, func() {
		waiting = core.NewProvisionStageGuard(executor, core.ProvisionStage{Kind: core.ProvisionStageWaitingForImagePull})
		executor.NotifyNotice(fmt.Sprintf("Waiting for image %s to finish downloading", image))
	})
	if waiting != nil {
		waiting.End()
	}
	if err != nil {
		var zero T
		return zero, err
	}
	pulling := core.NewProvisionStageGuard(executor, core.ProvisionStage{Kind: core.ProvisionStagePullingImage})
	defer pulling.End()
	defer guard.Release()
	return work()
}
