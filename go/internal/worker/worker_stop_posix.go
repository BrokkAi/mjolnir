// Port of the `cfg(any(target_os = "linux", target_os = "macos"))` parts of
// mj-worker/src/worker_stop.rs.

//go:build linux || darwin

package worker

import (
	"slices"
	"syscall"
	"time"
)

// workerStopKillWait is how long a stop waits for the tree to disappear after
// KILL. The former stop script waited three one-second rounds.
const workerStopKillWait = 3 * time.Second

// WorkerStopStop is worker_stop::stop: stop the worker for root and
// everything it started, then return. termGrace is how long a TERM may take
// before KILL; zero kills at once. A root with no worker returns at once.
func WorkerStopStop(root string, termGrace time.Duration) error {
	return stopWithin(root, termGrace, workerStopKillWait, workerStopRecordWait)
}

func stopWithin(root string, termGrace, killWait, recordWait time.Duration) error {
	panic("unported: mj-worker/src/worker_stop.rs stop_within")
}

// stopTree is the processes a stop must remove, as they were when it looked.
type stopTree struct {
	leader           uint32
	leaderLeadsGroup bool
	members          []processEntry
}

// stopTreeSnapshot is the leader, its group when it leads one, and every
// descendant of either.
func stopTreeSnapshot(table []processEntry, leader uint32) stopTree {
	leaderLeadsGroup := slices.ContainsFunc(table, func(entry processEntry) bool {
		return entry.pid == leader && entry.pgid == leader
	})
	return stopTreeCollect(table, leader, leaderLeadsGroup, func(entry *processEntry) bool {
		return entry.pid == leader || (leaderLeadsGroup && entry.pgid == leader)
	})
}

// refreshed is the members of this tree that are still the same processes,
// with everything that descends from them now.
func (t *stopTree) refreshed(table []processEntry) stopTree {
	return stopTreeCollect(table, t.leader, t.leaderLeadsGroup, func(entry *processEntry) bool {
		return slices.ContainsFunc(t.members, func(member processEntry) bool {
			return entry.isLiveInstanceOf(&member)
		}) || (t.leaderLeadsGroup && entry.pgid == t.leader)
	})
}

// stopTreeCollect is every entry of table that isSeed selects, and every
// descendant of one, except this process.
func stopTreeCollect(table []processEntry, leader uint32, leaderLeadsGroup bool, isSeed func(*processEntry) bool) stopTree {
	panic("unported: mj-worker/src/worker_stop.rs Tree::collect")
}

// stopSignalProcess sends signal to pid, recording any failure other than
// the process already being gone.
func stopSignalProcess(pid uint32, signal syscall.Signal, errors *[]string) {
	panic("unported: mj-worker/src/worker_stop.rs signal_process")
}

// stopSignalGroup sends signal to the process group group, recording any
// failure.
func stopSignalGroup(group uint32, signal syscall.Signal, errors *[]string) {
	panic("unported: mj-worker/src/worker_stop.rs signal_group")
}
