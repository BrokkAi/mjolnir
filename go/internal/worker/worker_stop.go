// Port of mj-worker/src/worker_stop.rs (the parts every platform builds).

// Worker liveness and stop, decided by the lock on `<root>/worker.lock`.
//
// The worker holds that lock from before it writes anything until the kernel
// closes its descriptors, so "the lock cannot be taken" is the whole answer to
// "is a worker running for this root". It needs no process listing, and a
// recycled pid cannot confuse it. The controller asks
// `hel worker liveness|stop|holder` on the target, from the root's own `hel`
// when that build has them and otherwise from a control worker it stages in
// the root. It does not test the lock from a shell. Two measured reasons rule
// that out (#1550): minimal images such as ubi9-minimal have neither
// `flock(1)` nor Perl, and `/proc/locks` drops a lock's line once the process
// that took it has exited, even while a forked child still holds the
// descriptor (#1257), so it would report a live worker as free in any pid
// namespace other than the initial one.
//
// # The control contract
//
// Any build may be asked to stop or probe a worker that a different build
// started, so these commands are a frozen interface, not an implementation
// detail. The controller decides on the target, per command, which binary in
// the root serves it: one that exits 0 for `worker <subcommand> --help`
// (`targets::worker_daemon`). That detection is the only version logic.
//
//   - The names, arguments, stdout tokens and exit-status meanings of
//     `worker liveness`, `worker holder` and `worker stop [--kill]` (all with
//     `--root <dir>`) are frozen. `liveness` prints `dead`, `starting` or
//     `alive`; `holder` prints the holder's pid, or nothing when no worker
//     holds the lock; `stop` prints nothing; status 0 is success and any
//     other status is a failure.
//   - Never change an existing control subcommand's arguments. New behavior
//     is a new subcommand, which the same `--help` probe detects.
//   - `--help` exits 0 on each of these subcommands. That is how a build
//     proves it has them, and a build that predates them exits 2.
//   - What the commands read in the root is frozen and may change only by
//     addition: the exclusive flock on `worker.lock`, held for the worker's
//     life, and the startup-record fields stop uses (`pid` and
//     `process_birth` in the startup record). This replaces any rule that an
//     older build must not judge a newer worker: every build that has the
//     commands reads the same things the same way, so any of them may stop
//     any worker.
//
// The golden for the control scripts and the contract test in this
// package's `tests/` enforce this.

package worker

import (
	"encoding/json"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// workerStopPollInterval is how often a stop looks at the lock and the
// processes it is waiting for.
const workerStopPollInterval = 25 * time.Millisecond

// TermGrace is the graceful stop's TERM grace, which the former stop script
// gave the worker.
const TermGrace = 2 * time.Second

// workerStopRecordWait is how long the holder of the lock may take to write
// the startup record that names it. The worker writes it as its first act
// after taking the lock.
const workerStopRecordWait = 3 * time.Second

// Liveness is what `hel worker liveness` reports; its value is the frozen
// stdout token.
type Liveness string

const (
	// LivenessDead: no process holds the root's lock.
	LivenessDead Liveness = "dead"
	// LivenessStarting: a worker holds the lock but has not bound its
	// control socket.
	LivenessStarting Liveness = "starting"
	// LivenessAlive: a worker holds the lock and serves its control socket.
	LivenessAlive Liveness = "alive"
)

// Token is the frozen stdout token of `hel worker liveness`.
func (l Liveness) Token() string { return string(l) }

// WorkerStopLiveness is worker_stop::liveness: whether a worker holds root's
// lock, and whether it serves its control socket yet.
func WorkerStopLiveness(root string) (Liveness, error) {
	held, err := WorkerRootOwnerIsHeld(root)
	if err != nil {
		return "", err
	}
	if !held {
		return LivenessDead, nil
	}
	if workerControlSocketExists(root) {
		return LivenessAlive, nil
	}
	return LivenessStarting, nil
}

func workerControlSocketExists(root string) bool {
	info, err := os.Stat(filepath.Join(root, "control.sock"))
	if runtime.GOOS == "windows" {
		return err == nil
	}
	return err == nil && info.Mode()&fs.ModeSocket != 0
}

// Holder is the pid of the worker holding the root's lock; false when no
// worker does. A record that does not name a live process of the recorded
// birth is not an identity, so the wait goes on until the holder writes its
// own or releases the lock.
func Holder(root string) (uint32, bool, error) {
	return holderWithin(root, workerStopRecordWait)
}

func holderWithin(root string, wait time.Duration) (uint32, bool, error) {
	deadline := time.Now().Add(wait)
	for {
		held, err := WorkerRootOwnerIsHeld(root)
		if err != nil || !held {
			return 0, false, err
		}
		if pid, ok := recordedWorker(root); ok {
			return pid, true, nil
		}
		if !time.Now().Before(deadline) {
			return 0, false, fmt.Errorf("a process holds the worker lock in %s but no startup record identifies it", root)
		}
		time.Sleep(workerStopPollInterval)
	}
}

// recordedWorker is the pid in the worker's own startup record, when the
// process with that pid is the one that wrote it. A pid alone is not an
// identity.
func recordedWorker(root string) (uint32, bool) {
	data, err := os.ReadFile(filepath.Join(root, core.WorkerStartupFile))
	if err != nil {
		return 0, false
	}
	var record struct {
		Pid          *uint32 `json:"pid"`
		ProcessBirth *string `json:"process_birth"`
	}
	if json.Unmarshal(data, &record) != nil || record.Pid == nil || record.ProcessBirth == nil {
		return 0, false
	}
	birth, err := core.ProcessBirthIdentity(*record.Pid)
	if err != nil || birth != *record.ProcessBirth {
		return 0, false
	}
	return *record.Pid, true
}
