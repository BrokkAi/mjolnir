// Port of mj-core/src/daemon_exit.rs.

// Exit status captured by the daemon's detached parent.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
)

const (
	DaemonExitSchema   uint32 = 1
	StoreOwnedExitCode int32  = 75
)

// DaemonExitRecord is the exit of one daemon process, as its waiter saw it.
type DaemonExitRecord struct {
	Schema     uint32  `json:"schema"`
	Pid        uint32  `json:"pid"`
	WaiterPid  *uint32 `json:"waiter_pid,omitempty"`
	ExitedAt   string  `json:"exited_at"`
	ExitCode   *int32  `json:"exit_code"`
	Signal     *int32  `json:"signal"`
	SignalName *string `json:"signal_name"`
	CoreDumped *bool   `json:"core_dumped"`
}

// UnmarshalJSON applies serde's defaults: schema defaults to
// DaemonExitSchema, pid is required, and unknown fields are ignored.
func (r *DaemonExitRecord) UnmarshalJSON(data []byte) error {
	var wire struct {
		Schema     *uint32 `json:"schema"`
		Pid        *uint32 `json:"pid"`
		WaiterPid  *uint32 `json:"waiter_pid"`
		ExitedAt   string  `json:"exited_at"`
		ExitCode   *int32  `json:"exit_code"`
		Signal     *int32  `json:"signal"`
		SignalName *string `json:"signal_name"`
		CoreDumped *bool   `json:"core_dumped"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	if wire.Pid == nil {
		return errors.New("missing field `pid`")
	}
	*r = DaemonExitRecord{
		Schema:     DaemonExitSchema,
		Pid:        *wire.Pid,
		WaiterPid:  wire.WaiterPid,
		ExitedAt:   wire.ExitedAt,
		ExitCode:   wire.ExitCode,
		Signal:     wire.Signal,
		SignalName: wire.SignalName,
		CoreDumped: wire.CoreDumped,
	}
	if wire.Schema != nil {
		r.Schema = *wire.Schema
	}
	return nil
}

func DaemonExitsDir(dataDir string) string {
	return filepath.Join(dataDir, "daemon-exits")
}

func WriteDaemonExitRecord(exitDir string, record *DaemonExitRecord) (string, error) {
	if err := CreateOwnerOnlyDirectory(exitDir); err != nil {
		return "", err
	}
	path := filepath.Join(exitDir, strconv.FormatUint(uint64(record.Pid), 10)+".json")
	body, err := json.Marshal(record)
	if err != nil {
		return "", err
	}
	if err := AtomicWriteExisting(path, body); err != nil {
		return "", err
	}
	return path, nil
}

func ReadDaemonExitRecord(path string) (DaemonExitRecord, error) {
	body, err := os.ReadFile(path)
	if err != nil {
		return DaemonExitRecord{}, fmt.Errorf("read %s: %w", path, err)
	}
	var record DaemonExitRecord
	if err := json.Unmarshal(body, &record); err != nil {
		return DaemonExitRecord{}, fmt.Errorf("parse %s: %w", path, err)
	}
	return record, nil
}

func FormatExitReason(record *DaemonExitRecord) string {
	if record.Signal != nil {
		signal := *record.Signal
		core := ""
		if record.CoreDumped != nil && *record.CoreDumped {
			core = ", core dumped"
		}
		switch {
		case record.SignalName != nil:
			return fmt.Sprintf("terminated by signal %s (%d%s)", *record.SignalName, signal, core)
		case core == "":
			return fmt.Sprintf("terminated by signal %d", signal)
		default:
			return fmt.Sprintf("terminated by signal %d (core dumped)", signal)
		}
	}
	if record.ExitCode != nil {
		return fmt.Sprintf("exited with status %d", *record.ExitCode)
	}
	return "exited with an unknown status"
}
