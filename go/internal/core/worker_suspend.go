// Port of mj-core/src/worker_suspend.rs.

// Target-owned process quiescence requests and receipts for an accepted
// suspend.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
)

const (
	WorkerSuspendAccepted   = "suspend-operation.json"
	WorkerSuspendCapability = "suspend-worker.json"
	WorkerSuspendRequest    = "suspend-request.json"
	WorkerSuspendReceipt    = "suspend-receipt.json"
	WorkerSuspendProgress   = "suspend-progress.json"
	WorkerSuspendRestart    = "suspend-restart.json"
)

// WorkerIdentity names one worker process: its pid and its start time, so a
// reused pid is never taken for the worker.
type WorkerIdentity struct {
	Pid   uint32 `json:"pid"`
	Start uint64 `json:"start"`
}

// IsAlive reports whether the process is still this worker and has not
// exited. Linux and macOS only.
func (w WorkerIdentity) IsAlive() (bool, error) { return workerIdentityIsAlive(w) }

type SuspendRequest struct {
	Operation     string         `json:"operation"`
	Worker        WorkerIdentity `json:"worker"`
	Checkout      string         `json:"checkout"`
	ChildSessions []string       `json:"child_sessions"`
}

// MarshalJSON writes a nil ChildSessions as an empty list, as serde writes a
// Vec.
func (r SuspendRequest) MarshalJSON() ([]byte, error) {
	type plain SuspendRequest
	if r.ChildSessions == nil {
		r.ChildSessions = []string{}
	}
	return json.Marshal(plain(r))
}

// UnmarshalJSON requires the fields serde requires; child_sessions defaults
// to empty.
func (r *SuspendRequest) UnmarshalJSON(data []byte) error {
	var wire struct {
		Operation     *string         `json:"operation"`
		Worker        *WorkerIdentity `json:"worker"`
		Checkout      *string         `json:"checkout"`
		ChildSessions []string        `json:"child_sessions"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	switch {
	case wire.Operation == nil:
		return errors.New("missing field `operation`")
	case wire.Worker == nil:
		return errors.New("missing field `worker`")
	case wire.Checkout == nil:
		return errors.New("missing field `checkout`")
	}
	*r = SuspendRequest{Operation: *wire.Operation, Worker: *wire.Worker, Checkout: *wire.Checkout, ChildSessions: wire.ChildSessions}
	if r.ChildSessions == nil {
		r.ChildSessions = []string{}
	}
	return nil
}

type SuspendReceipt struct {
	Operation string  `json:"operation"`
	Error     *string `json:"error"`
}

type SuspendProgress struct {
	Operation     string   `json:"operation"`
	RemainingPids []uint32 `json:"remaining_pids"`
}

// MarshalJSON writes a nil RemainingPids as an empty list, as serde writes a
// Vec.
func (p SuspendProgress) MarshalJSON() ([]byte, error) {
	type plain SuspendProgress
	if p.RemainingPids == nil {
		p.RemainingPids = []uint32{}
	}
	return json.Marshal(plain(p))
}

// WorkerSuspendRead reads root/name; false when it does not exist.
func WorkerSuspendRead[T any](root, name string) (T, bool, error) {
	var value T
	path := filepath.Join(root, name)
	body, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return value, false, nil
	}
	if err != nil {
		return value, false, fmt.Errorf("read %s: %w", path, err)
	}
	if err := json.Unmarshal(body, &value); err != nil {
		return value, false, fmt.Errorf("decode %s: %w", path, err)
	}
	return value, true, nil
}

// WorkerSuspendWrite atomically replaces root/name with value.
func WorkerSuspendWrite(root, name string, value any) error {
	body, err := json.Marshal(value)
	if err != nil {
		return err
	}
	return AtomicWriteExisting(filepath.Join(root, name), body)
}
