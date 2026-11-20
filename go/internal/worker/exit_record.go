// Port of mj-worker/src/exit_record.rs.

// The worker's exit record, written so that a full disk cannot silence it.
//
// A worker stops when it can no longer write its journal, and on a full disk
// that is exactly when creating a new file fails too. So once the worker has
// recovered its durable state it reserves the record: `worker-exit.json`
// holding `null` padded with spaces, which every reader takes as "no exit
// record". At exit the reason is written over those bytes in place, which
// needs no new blocks on ext4, XFS and similar filesystems. Before the
// reservation exists (a worker that fails during startup) the record is
// created the ordinary way.

package worker

import (
	"bytes"
	"fmt"
	"path/filepath"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ReservedExitRecordBytes is room for a reason with a bridge's stderr tail
// after it.
const ReservedExitRecordBytes = 16 * 1024

// Reserve reserves the record's space while the disk still has some.
func Reserve(root string) error {
	body := append([]byte("null"), bytes.Repeat([]byte{' '}, ReservedExitRecordBytes-len("null"))...)
	body[ReservedExitRecordBytes-1] = '\n'
	path := filepath.Join(root, core.WorkerExitFile)
	if err := core.AtomicWrite(path, body); err != nil {
		return fmt.Errorf("reserve the worker exit record %s: %w", path, err)
	}
	return nil
}

// Write writes the exit record: over a reservation in place, or as a new
// file. refusal is nil when there is none.
func Write(root string, reason string, refusal *string) error {
	return WriteWithRecentLog(root, reason, refusal, nil, nil)
}

// WriteWithRecentLog writes the exit record with the newest diagnostics,
// fitting escaped JSON into the existing blocks.
func WriteWithRecentLog(
	root string,
	reason string,
	refusal *string,
	startupFailure *core.WorkerHarnessStartupFailure,
	recentLog []string,
) error {
	panic("unported: mj-worker/src/exit_record.rs write_with_recent_log")
}
