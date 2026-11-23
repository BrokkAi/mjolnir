// Port of mj-worker/src/worker_stop/suspension.rs (the shared part).

// The live worker owns the stop and reports its result even across daemon
// replacement. A receipt is published only after the harness and every
// attributable descendant have exited.

//go:build linux || darwin

package worker

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"os/signal"
	"slices"
	"strings"
	"syscall"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SuspendSignal receives a durable suspend request addressed to this worker
// process.
type SuspendSignal struct {
	root    string
	worker  core.WorkerIdentity
	signals chan os.Signal
}

// InstallSuspendSignal installs the receiver and then publishes this
// worker's capability. It returns nil where checkout quiescence cannot be
// supported (a Linux worker that is not a subreaper).
func InstallSuspendSignal(root string) (*SuspendSignal, error) {
	if !suspendSupported() {
		return nil, nil
	}
	signals := make(chan os.Signal, 1)
	signal.Notify(signals, syscall.SIGUSR1)
	pid := uint32(os.Getpid())
	entry, found, err := processTableLookup(pid)
	if err != nil {
		signal.Stop(signals)
		return nil, err
	}
	if !found {
		signal.Stop(signals)
		return nil, errors.New("read this worker's process identity")
	}
	worker := core.WorkerIdentity{Pid: pid, Start: entry.start}
	if err := core.WorkerSuspendWrite(root, core.WorkerSuspendCapability, worker); err != nil {
		signal.Stop(signals)
		return nil, err
	}
	return &SuspendSignal{root: root, worker: worker, signals: signals}, nil
}

func (s *SuspendSignal) wait(ctx context.Context) (core.SuspendRequest, error) {
	for {
		request, found, err := core.WorkerSuspendRead[core.SuspendRequest](s.root, core.WorkerSuspendRequest)
		if err != nil {
			return core.SuspendRequest{}, err
		}
		if found && request.Worker == s.worker {
			return request, nil
		}
		// The request is durable before the sender wakes us. Poll its
		// record too so a killed sender cannot leave the worker waiting.
		timer := time.NewTimer(time.Second)
		select {
		case <-s.signals:
		case <-timer.C:
		case <-ctx.Done():
			timer.Stop()
			return core.SuspendRequest{}, ctx.Err()
		}
		timer.Stop()
	}
}

// RequestSuspend starts (or redelivers) a request for this exact worker
// process. The worker's capability is published only after installing the
// signal receiver.
func RequestSuspend(root string, operation string, checkout string, childSessions []string) error {
	receipt, found, err := core.WorkerSuspendRead[core.SuspendReceipt](root, core.WorkerSuspendReceipt)
	if err != nil {
		return err
	}
	if found && receipt.Operation == operation {
		return nil
	}
	identity, found, err := core.WorkerSuspendRead[core.WorkerIdentity](root, core.WorkerSuspendCapability)
	if err != nil {
		return err
	}
	if !found {
		return errors.New("this worker does not support managed checkout quiescence")
	}
	worker, found, err := processTableLookup(identity.Pid)
	if err != nil {
		return err
	}
	if !found || worker.start != identity.Start || worker.zombie {
		return errors.New("worker exited before confirming checkout quiescence")
	}
	holder, held, err := Holder(root)
	if err != nil {
		return err
	}
	if !held || holder != worker.pid {
		return errors.New("worker no longer owns the requested checkout")
	}
	request, found, err := core.WorkerSuspendRead[core.SuspendRequest](root, core.WorkerSuspendRequest)
	if err != nil {
		return err
	}
	if !found || request.Operation != operation || request.Worker != identity {
		if err := core.WorkerSuspendWrite(root, core.WorkerSuspendRequest, core.SuspendRequest{
			Operation:     operation,
			Worker:        identity,
			Checkout:      checkout,
			ChildSessions: childSessions,
		}); err != nil {
			return err
		}
	}
	if err := signalVerified(&worker, syscall.SIGUSR1, func(*processEntry) bool { return true }); err != nil {
		slog.Warn("could not wake worker for its durable suspend request", "error", err)
	}
	return nil
}

// disappeared treats a process that is already gone as signalled.
func disappeared(err error) error {
	if errors.Is(err, syscall.ESRCH) {
		return nil
	}
	return err
}

func terminateSessionProcesses(root string, sessionID string, request *core.SuspendRequest) error {
	workerPID := uint32(os.Getpid())
	sessionIDs := append([]string{sessionID}, request.ChildSessions...)
	marked := func(pid uint32) bool {
		return slices.ContainsFunc(sessionIDs, func(id string) bool { return markedProcess(pid, id) })
	}
	treeContains := func(process *processEntry) bool {
		// Walk to this worker while validating each live parent. A
		// descendant reparented mid-scan is found on the next pass.
		parent := process.ppid
		for parent > 1 {
			if parent == workerPID {
				return true
			}
			entry, found, err := processTableLookup(parent)
			if err != nil || !found {
				return false
			}
			if entry.start > process.start || entry.ppid == parent {
				return false
			}
			parent = entry.ppid
		}
		return false
	}

	var waitingFor []uint32
	for {
		noChildren, err := suspendNoChildren()
		if err != nil {
			return err
		}
		table, err := processTableList()
		if err != nil {
			return err
		}
		var survivors []uint32
		for i := range table {
			entry := &table[i]
			if entry.pid == workerPID || entry.zombie || !(marked(entry.pid) || treeContains(entry)) {
				continue
			}
			if err := signalVerified(entry, syscall.SIGKILL, func(current *processEntry) bool {
				return marked(current.pid) || treeContains(current)
			}); err != nil {
				return fmt.Errorf("terminate session process %d: %w", entry.pid, err)
			}
			survivors = append(survivors, entry.pid)
		}
		if noChildren && len(survivors) == 0 {
			break
		}
		slices.Sort(survivors)
		if !slices.Equal(waitingFor, survivors) {
			progress := core.SuspendProgress{Operation: request.Operation, RemainingPids: survivors}
			if err := core.WorkerSuspendWrite(root, core.WorkerSuspendProgress, progress); err != nil {
				return err
			}
			waitingFor = progress.RemainingPids
		}
		time.Sleep(workerStopPollInterval)
	}
	holders, err := checkoutHolders(request.Checkout)
	if err != nil {
		return err
	}
	if len(holders) != 0 {
		return fmt.Errorf("Mjolnir cannot prove these processes belong to this session; stop them, then retry: %s",
			strings.Join(holders, "; "))
	}
	return nil
}

// waitForSuspend waits for a suspend request; it never returns without one
// (or ctx ending) when signal is nil.
func waitForSuspend(ctx context.Context, signal *SuspendSignal) (core.SuspendRequest, error) {
	if signal == nil {
		<-ctx.Done()
		return core.SuspendRequest{}, ctx.Err()
	}
	return signal.wait(ctx)
}

// finishSuspend stops every process of the session and its child sessions,
// then publishes the receipt.
func finishSuspend(ctx context.Context, root string, sessionID string, request core.SuspendRequest) error {
	sealSuspendSpawns()
	written, joinErr := core.SpawnBlocking(func() error {
		outcome := terminateSessionProcesses(root, sessionID, &request)
		receipt := core.SuspendReceipt{Operation: request.Operation}
		if outcome != nil {
			message := outcome.Error()
			receipt.Error = &message
		}
		return core.WorkerSuspendWrite(root, core.WorkerSuspendReceipt, receipt)
	}).Join(ctx)
	if joinErr != nil {
		return fmt.Errorf("join worker quiescence: %w", joinErr)
	}
	return written
}
