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
	"os"
	"path/filepath"
	"slices"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"

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
	path := filepath.Join(root, core.WorkerExitFile)
	reserved := 0
	if info, err := os.Stat(path); err == nil && info.Mode().IsRegular() && info.Size() > 0 {
		reserved = int(info.Size())
	}
	at := time.Now().UTC().Format("2006-01-02T15:04:05Z")
	budget := ReservedExitRecordBytes
	if reserved > 0 {
		budget = reserved
	}
	recentLog = slices.Clone(recentLog)
	var failure *core.WorkerHarnessStartupFailure
	if startupFailure != nil {
		copied := *startupFailure
		failure = &copied
	}
	// Keep reason/refusal first, but reserve most of the record for the
	// tail. JSON escaping can make a bounded in-memory line six times larger
	// on disk.
	shorten := func(text *string, limit int) {
		if len(*text) > limit {
			*text = (*text)[:floorCharBoundary(*text, max(limit-3, 0))] + "…"
		}
	}
	// Even control characters in the marker must fit the reservation.
	if failure != nil {
		shorten(&failure.Message, budget/8)
	}
	if refusal != nil {
		copied := *refusal
		refusal = &copied
	}
	if len(recentLog) > 0 {
		shorten(&reason, budget/8)
		if refusal != nil {
			shorten(refusal, budget/8)
		}
	}
	body := func() ([]byte, error) {
		var refusalValue any
		if refusal != nil {
			refusalValue = *refusal
		}
		logValues := make([]any, len(recentLog))
		for i, line := range recentLog {
			logValues[i] = line
		}
		record := jsonvalue.Object{
			{Key: "reason", Value: reason},
			{Key: "refusal", Value: refusalValue},
			{Key: "at", Value: at},
			{Key: "version", Value: core.PackageVersion},
			{Key: "recent_log", Value: logValues},
		}
		if failure != nil {
			record = append(record, jsonvalue.Member{Key: "harness_startup_failure", Value: *failure})
		}
		return jsonvalue.Marshal(record)
	}
	encoded, err := body()
	if err != nil {
		return err
	}
	for len(encoded) > budget {
		switch {
		case len(recentLog) > 0:
			first := recentLog[0]
			excess := len(encoded) - budget
			// Trim by encoded JSON bytes, preserving the newest text even
			// when control characters expand to six bytes each.
			remove := len(first)
			counted := 0
			for index := 0; index < len(first); {
				character, width := utf8.DecodeRuneInString(first[index:])
				switch {
				case character == '"' || character == '\\' || character == '\b' || character == '\f' ||
					character == '\n' || character == '\r' || character == '\t':
					counted += 2
				case character <= 0x1f:
					counted += 6
				default:
					counted += width
				}
				index += width
				if counted >= excess {
					remove = index
					break
				}
			}
			first = first[remove:]
			if first == "" {
				recentLog = recentLog[1:]
			} else {
				recentLog[0] = first
			}
		case len(reason) > 3:
			shorten(&reason, max(len(reason)-max(len(encoded)-budget, 4), 0))
		case refusal != nil:
			if len(*refusal) > 3 {
				shorten(refusal, max(len(*refusal)-max(len(encoded)-budget, 4), 0))
			} else {
				refusal = nil
			}
		default:
			return fmt.Errorf("worker exit record reservation is too small (%d bytes)", budget)
		}
		if encoded, err = body(); err != nil {
			return err
		}
	}
	if reserved == 0 {
		if err := core.AtomicWrite(path, encoded); err != nil {
			return fmt.Errorf("write the worker exit record %s: %w", path, err)
		}
		return nil
	}
	padded := append(encoded, bytes.Repeat([]byte{' '}, reserved-len(encoded))...)
	file, err := os.OpenFile(path, os.O_WRONLY, 0)
	if err != nil {
		return fmt.Errorf("open the reserved worker exit record %s: %w", path, err)
	}
	defer file.Close()
	if _, err := file.WriteAt(padded, 0); err != nil {
		return fmt.Errorf("write the reserved worker exit record %s: %w", path, err)
	}
	if err := file.Sync(); err != nil {
		return fmt.Errorf("write the reserved worker exit record %s: %w", path, err)
	}
	return nil
}
