// Stand-ins for the mj-core items the mj-checkpoint checkpoint-rest sync
// needs at Rust 3dde0c11b and that other sync areas port: suspend_consent.rs
// (CaptureConsent), session_keep.rs (snapshot_keep_dir), worker_suspend.rs
// (the quiescence request and receipt), subprocess.rs
// (run_with_input_to_file) and confined_path.rs (write_atomically_from_reader
// and PublishError::LengthMismatch).
//
// They exist only so the checkpoint package compiles and its tests run
// before those areas are merged. The core-rest sync ports the real items in
// suspend_consent.go, session_keep.go, worker_suspend.go, subprocess.go and
// confined_path.go under the same names and signatures; when both are
// merged, delete this whole file.

package core

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
)

// ---- suspend_consent.rs ----

// CaptureConsent is typed consent for untracked files, keep-dir files, and
// in-progress operations. Stand-in: the core-rest port carries the fields.
type CaptureConsent struct{}

// ---- session_keep.rs ----

// SnapshotKeepDir is `<checkout>/.mj/keep` when sessionID is nil, otherwise
// `<checkout>/.mj/keep/<session-id>`.
func SnapshotKeepDir(checkout string, sessionID *string) (string, error) {
	path := rustPathPush(rustPathPush(checkout, ".mj"), "keep")
	if sessionID != nil {
		valid := *sessionID != ""
		for i := 0; i < len(*sessionID); i++ {
			b := (*sessionID)[i]
			if !(b >= 'a' && b <= 'z' || b >= 'A' && b <= 'Z' || b >= '0' && b <= '9' || b == '-' || b == '_') {
				valid = false
			}
		}
		if !valid {
			return "", fmt.Errorf("session id %s is not a single directory name", debugString(*sessionID))
		}
		path = rustPathPush(path, *sessionID)
	}
	return path, nil
}

// ---- worker_suspend.rs ----

const (
	WorkerSuspendRequest = "suspend-request.json"
	WorkerSuspendReceipt = "suspend-receipt.json"
)

// WorkerIdentity names one worker process: its pid and its start time.
type WorkerIdentity struct {
	Pid   uint32 `json:"pid"`
	Start uint64 `json:"start"`
}

// IsAlive reports whether the process is still this worker. Stand-in: the
// core-rest port reads the process table.
func (w WorkerIdentity) IsAlive() (bool, error) {
	return false, errors.New("worker process identity is not ported in this stand-in")
}

type SuspendRequest struct {
	Operation     string         `json:"operation"`
	Worker        WorkerIdentity `json:"worker"`
	Checkout      string         `json:"checkout"`
	ChildSessions []string       `json:"child_sessions"`
}

type SuspendReceipt struct {
	Operation string  `json:"operation"`
	Error     *string `json:"error"`
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

// ---- subprocess.rs ----

// RunWithInputToFile runs a child with stdin fed concurrently with stderr
// draining, writing stdout directly to output without buffering it in
// memory. A broken pipe on stdin is handled the same way as RunWithInput.
func RunWithInputToFile(command *exec.Cmd, input []byte, output *os.File) (ProcessOutput, error) {
	stdinRead, stdinWrite, err := os.Pipe()
	if err != nil {
		return ProcessOutput{}, fmt.Errorf("spawn child process: %w", err)
	}
	var stderr bytes.Buffer
	command.Stdin = stdinRead
	command.Stdout, command.Stderr = output, &stderr
	err = command.Start()
	_ = stdinRead.Close()
	if err != nil {
		_ = stdinWrite.Close()
		return ProcessOutput{}, fmt.Errorf("spawn child process: %w", err)
	}
	written := make(chan error, 1)
	go func() {
		_, err := stdinWrite.Write(input)
		_ = stdinWrite.Close()
		written <- err
	}()
	outcome := waitForExit(command)
	writeErr := <-written
	if outcome.err != nil {
		return ProcessOutput{}, fmt.Errorf("wait for child process: %w", outcome.err)
	}
	if writeErr != nil && !isBrokenPipe(writeErr) {
		return ProcessOutput{}, fmt.Errorf("write child process stdin: %w", writeErr)
	}
	return ProcessOutput{Status: outcome.state, Stdout: []byte{}, Stderr: stderr.Bytes()}, nil
}

// ---- confined_path.rs ----

// PublishErrorLengthMismatch: streamed input did not match the declared
// length.
type PublishErrorLengthMismatch struct {
	Expected uint64
	Received uint64
}

func (PublishErrorLengthMismatch) isPublishError() {}

func (e PublishErrorLengthMismatch) Error() string {
	return fmt.Sprintf("incomplete file upload: expected %d bytes, received %d", e.Expected, e.Received)
}

// WriteAtomicallyFromReader publishes reader's bytes as name. When
// expectedLen is set, it rejects short reads and trailing extra bytes before
// publishing. Stand-in: it reads the input into memory before publishing;
// the core-rest port streams it.
func (d *ConfinedDir) WriteAtomicallyFromReader(name string, reader io.Reader, expectedLen *uint64, publish Publish) (uint64, PublishError) {
	var body bytes.Buffer
	limited := reader
	if expectedLen != nil {
		limited = io.LimitReader(reader, int64(*expectedLen)+1)
	}
	if _, err := body.ReadFrom(limited); err != nil {
		return 0, PublishErrorFailed{fmt.Errorf("read input for %s: %w", rustPathPush(d.path, name), err)}
	}
	received := uint64(body.Len())
	if expectedLen != nil && received != *expectedLen {
		return 0, PublishErrorLengthMismatch{Expected: *expectedLen, Received: received}
	}
	if err := d.WriteAtomically(name, body.Bytes(), publish); err != nil {
		return 0, err
	}
	return received, nil
}
