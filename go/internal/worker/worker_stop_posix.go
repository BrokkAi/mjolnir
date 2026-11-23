// Port of the `cfg(any(target_os = "linux", target_os = "macos"))` parts of
// mj-worker/src/worker_stop.rs.

//go:build linux || darwin

package worker

import (
	"fmt"
	"maps"
	"os"
	"slices"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
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
	leader, held, err := holderWithin(root, recordWait)
	if err != nil || !held {
		return err
	}
	// The harness and its helpers lead process groups of their own, so the
	// worker's group can empty while they still write into the root. The tree
	// is therefore snapshotted before any signal: the leader, its group, and
	// every descendant of either. A stop is finished when the lock is free
	// and none of them is left.
	table, err := processTableList()
	if err != nil {
		return fmt.Errorf("list processes: %w", err)
	}
	tree := stopTreeSnapshot(table, leader)
	ownGroup := uint32(syscall.Getpgrp())
	var signalErrors []string
	done := func(tree *stopTree) (bool, error) {
		held, err := WorkerRootOwnerIsHeld(root)
		if err != nil || held {
			return false, err
		}
		for i := range tree.members {
			member := &tree.members[i]
			now, found, err := processTableLookup(member.pid)
			if err != nil {
				return false, fmt.Errorf("look up process %d: %w", member.pid, err)
			}
			if found && now.isLiveInstanceOf(member) {
				return false, nil
			}
		}
		return true, nil
	}
	wait := func(tree *stopTree, limit time.Duration) (bool, error) {
		deadline := time.Now().Add(limit)
		for {
			finished, err := done(tree)
			if err != nil || finished {
				return finished, err
			}
			if !time.Now().Before(deadline) {
				return false, nil
			}
			time.Sleep(workerStopPollInterval)
		}
	}

	if termGrace != 0 {
		// The worker leads its own group, so TERM takes its agent down too.
		if tree.leaderLeadsGroup {
			stopSignalGroup(leader, syscall.SIGTERM, &signalErrors)
		} else {
			stopSignalProcess(leader, syscall.SIGTERM, &signalErrors)
		}
		finished, err := wait(&tree, termGrace)
		if err != nil || finished {
			return err
		}
		// Whatever ignored TERM, and whatever it started since, goes next.
		table, err := processTableList()
		if err != nil {
			return fmt.Errorf("list processes: %w", err)
		}
		tree = tree.refreshed(table)
	}
	groups := map[uint32]bool{}
	for _, member := range tree.members {
		stopSignalProcess(member.pid, syscall.SIGKILL, &signalErrors)
		// Group 0 and 1 are not groups of ours to signal, and our own group
		// holds this stop.
		if member.pgid > 1 && member.pgid != ownGroup {
			groups[member.pgid] = true
		}
	}
	for _, group := range slices.Sorted(maps.Keys(groups)) {
		stopSignalGroup(group, syscall.SIGKILL, &signalErrors)
	}
	finished, err := wait(&tree, killWait)
	if err != nil || finished {
		return err
	}
	var survivors []string
	for i := range tree.members {
		member := &tree.members[i]
		if now, found, err := processTableLookup(member.pid); err == nil && found && now.isLiveInstanceOf(member) {
			survivors = append(survivors, strconv.FormatUint(uint64(member.pid), 10))
		}
	}
	lockHeld, err := WorkerRootOwnerIsHeld(root)
	if err != nil {
		lockHeld = true
	}
	signalReport := ""
	if len(signalErrors) > 0 {
		signalReport = ", signal errors: " + strings.Join(signalErrors, "; ")
	}
	return fmt.Errorf(
		"worker process tree still running after stop: leader %d, lock held: %t, live processes [%s]%s",
		leader, lockHeld, strings.Join(survivors, " "), signalReport)
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
	own := uint32(os.Getpid())
	children := map[uint32][]int{}
	for index := range table {
		children[table[index].ppid] = append(children[table[index].ppid], index)
	}
	included := make([]bool, len(table))
	var pending []int
	for index := range table {
		if table[index].pid != own && isSeed(&table[index]) {
			included[index] = true
			pending = append(pending, index)
		}
	}
	for len(pending) > 0 {
		index := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		for _, child := range children[table[index].pid] {
			if !included[child] && table[child].pid != own {
				included[child] = true
				pending = append(pending, child)
			}
		}
	}
	tree := stopTree{leader: leader, leaderLeadsGroup: leaderLeadsGroup}
	for index, entry := range table {
		if included[index] {
			tree.members = append(tree.members, entry)
		}
	}
	return tree
}

// stopSignalProcess sends signal to pid, recording any failure other than
// the process already being gone.
func stopSignalProcess(pid uint32, signal syscall.Signal, errors *[]string) {
	if err := syscall.Kill(int(int32(pid)), signal); err != nil {
		// A process that is already gone is what the signal was for.
		if err != syscall.ESRCH {
			*errors = append(*errors, fmt.Sprintf("signal %d to %d: %s", int(signal), pid, core.IOErrorDisplay(err)))
		}
	}
}

// stopSignalGroup sends signal to the process group group, recording any
// failure.
func stopSignalGroup(group uint32, signal syscall.Signal, errors *[]string) {
	if err := core.SignalProcessGroup(int(int32(group)), signal); err != nil {
		*errors = append(*errors, fmt.Sprintf("signal %d to group %d: %s", int(signal), group, core.IOErrorDisplay(err)))
	}
}
