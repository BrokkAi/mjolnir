// Port of mj-worker/src/worker_runtime/unix/requests.rs.
//
// Relay requests answered on the connection: durable relay requests, review
// capture, elicitation answers, background-task stops, credentials, skills
// and project memory, and the framing of every response.

//go:build unix

package worker

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"math"
	"path/filepath"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// handleRequest answers one relay request, keeping the relay lock for
// exactly as long as the request needs it.
//
// Every request but attach is a bounded mutation of durable state and is
// served under the lock. Attach is not: validating a controller's cursor can
// decompress a sealed segment and the reply carries up to a
// core.RelayReplayByteBudget page read from disk, and a controller catching
// up over a long offline history asks for page after page. Holding the lock
// across that stalls the coordinator's recordRuntimeEvent, and once its
// bounded event channel fills the agent's turn stalls with it. So attach
// captures a plan under the lock and does its reading on a blocking
// goroutine with the lock released.
func handleRequest(ctx context.Context, relay *RelayCell, envelope core.RelayRequestEnvelope, harnessPreparation core.HarnessPreparation) (core.RelayResponseEnvelope, error) {
	deferred, response, handled := func() (*DeferredRelayAttach, core.RelayResponseEnvelope, bool) {
		guard := relay.Lock()
		defer guard.Unlock()
		if deferred := guard.TakeDeferredAttach(&envelope); deferred != nil {
			return deferred, core.RelayResponseEnvelope{}, false
		}
		return nil, guard.Handle(envelope), true
	}()
	if handled {
		return response, nil
	}
	for range handleRequestReplanAttempts {
		deferred.SetHarnessPreparation(harnessPreparation)
		generation := deferred.JournalGeneration()
		finishing := deferred
		response, err := core.SpawnBlocking(func() core.RelayResponseEnvelope {
			return finishing.Finish()
		}).Join(ctx)
		if err != nil {
			return core.RelayResponseEnvelope{}, fmt.Errorf("assemble relay replay page: %w", err)
		}
		if _, failed := response.Body.(core.RelayResponseBodyError); !failed {
			guard := relay.Lock()
			if guard.JournalGeneration() == generation {
				guard.RememberReplayCursor(&response)
			}
			guard.Unlock()
			return response, nil
		}
		next, unmoved := func() (*DeferredRelayAttach, bool) {
			guard := relay.Lock()
			defer guard.Unlock()
			if guard.JournalGeneration() == generation {
				// The journal never moved, so this really is the answer.
				return nil, true
			}
			return guard.TakeDeferredAttach(&envelope), false
		}()
		if unmoved {
			return response, nil
		}
		if next == nil {
			break
		}
		deferred = next
	}
	return DeferredRelayAttachStaleJournalResponse(envelope.RequestID, envelope.ProtocolVersion), nil
}

// handleRequestReplanAttempts is how often handleRequest plans an attach
// again after sealing or garbage collection moved the segments a plan named
// while it was being read. That is not the controller's fault and not its
// problem. The journal only moves that way once per sealed megabyte or per
// acknowledgement, so a couple of attempts always outrun it.
const handleRequestReplanAttempts = 3

func reviewCaptureResponse(ctx context.Context, envelope core.RelayRequestEnvelope, capture *reviewCapture) core.RelayResponseEnvelope {
	var body core.RelayResponseBody
	if capture == nil {
		body = compactionError(core.RelayErrorCodeInvalidState, "review capture is unavailable")
	} else {
		reviewer, ok := envelope.Request.(core.RelayRequestReviewer)
		if !ok {
			panic("reviewCaptureResponse only serves capture requests")
		}
		answered, err := capture.Handle(ctx, reviewer.Request)
		if err != nil {
			body = compactionError(core.RelayErrorCodeInvalidState, err.Error())
		} else {
			body = answered
		}
	}
	return core.RelayResponseEnvelope{
		RequestID:       envelope.RequestID,
		ProtocolVersion: envelope.ProtocolVersion,
		Body:            body,
	}
}

// compactionError is a non-retryable relay error body.
func compactionError(code core.RelayErrorCode, message string) core.RelayResponseBody {
	return core.RelayResponseBodyError{Error: core.RelayProtocolError{Code: code, Message: message}}
}

// awaitCommandResolution waits for the ACP runtime to resolve a command sent
// with a oneshot reply. stopped is true when the runtime went away first,
// which is when Rust's oneshot receiver sees its sender dropped.
func awaitCommandResolution(ctx context.Context, commands *commandChannel, resolution <-chan error) (err error, stopped bool) {
	select {
	case err := <-resolution:
		return err, false
	case <-commands.closed:
		// A runtime that resolved before it stopped still answered.
		select {
		case err := <-resolution:
			return err, false
		default:
			return nil, true
		}
	case <-ctx.Done():
		return nil, true
	}
}

// elicitationResponse hands a form answer to the ACP runtime and waits for
// its resolution.
func elicitationResponse(ctx context.Context, envelope core.RelayRequestEnvelope, commands *commandChannel) core.RelayResponseEnvelope {
	request, ok := envelope.Request.(core.RelayRequestRespondElicitation)
	if !ok {
		panic("elicitationResponse only serves form answers")
	}
	var body core.RelayResponseBody
	if commands == nil {
		body = compactionError(core.RelayErrorCodeInvalidState,
			"session is closed; no ACP runtime can answer the elicitation")
	} else {
		resolution := make(chan error, 1)
		err := commands.send(ctx, CommandRequestResolveElicitation{
			ElicitationID: request.ElicitationID,
			Response:      request.Response,
			Resolved:      resolution,
		})
		if err != nil {
			body = compactionError(core.RelayErrorCodeInternal,
				"ACP runtime stopped before accepting the elicitation answer")
		} else {
			refusal, stopped := awaitCommandResolution(ctx, commands, resolution)
			switch {
			case stopped:
				body = compactionError(core.RelayErrorCodeInternal,
					"ACP runtime stopped before resolving the elicitation")
			case refusal != nil:
				body = compactionError(core.RelayErrorCodeInvalidState, refusal.Error())
			default:
				body = core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadElicitationResolved{
					ElicitationID: request.ElicitationID,
				}}
			}
		}
	}
	return core.RelayResponseEnvelope{
		RequestID:       envelope.RequestID,
		ProtocolVersion: envelope.ProtocolVersion,
		Body:            body,
	}
}

// backgroundTaskStopResponse asks the ACP runtime to stop a background task
// and waits for it to accept the stop.
func backgroundTaskStopResponse(ctx context.Context, envelope core.RelayRequestEnvelope, commands *commandChannel, relay *RelayCell) core.RelayResponseEnvelope {
	request, ok := envelope.Request.(core.RelayRequestStopBackgroundTask)
	if !ok {
		panic("backgroundTaskStopResponse only serves task stops")
	}
	target, targetErr := func() (core.BackgroundTaskStopTarget, error) {
		guard := relay.Lock()
		defer guard.Unlock()
		return guard.BackgroundTaskStopTarget(request.BackgroundTaskID)
	}()
	var body core.RelayResponseBody
	switch {
	case commands == nil:
		body = compactionError(core.RelayErrorCodeInvalidState,
			"session is closed; no ACP runtime can stop the background task")
	case targetErr != nil:
		body = compactionError(core.RelayErrorCodeInvalidState, targetErr.Error())
	default:
		// The relay expects the adapter's acknowledgement once a Claude stop
		// is requested; forget it if the stop never gets there.
		stopNotSent := func() {
			if task, ok := target.(core.BackgroundTaskStopTargetAirAsyncTask); ok {
				guard := relay.Lock()
				guard.ClaudeStopNotSent(task.SessionID, task.TaskID)
				guard.Unlock()
			}
		}
		resolution := make(chan error, 1)
		if err := commands.send(ctx, CommandRequestStopBackgroundTask{Target: target, Resolved: resolution}); err != nil {
			stopNotSent()
			body = compactionError(core.RelayErrorCodeInternal,
				"ACP runtime stopped before accepting the background task stop")
			break
		}
		refusal, stopped := awaitCommandResolution(ctx, commands, resolution)
		switch {
		case stopped:
			stopNotSent()
			body = compactionError(core.RelayErrorCodeInternal,
				"ACP runtime stopped before resolving the background task stop")
		case refusal != nil:
			stopNotSent()
			body = compactionError(core.RelayErrorCodeInvalidState, refusal.Error())
		default:
			body = core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadBackgroundTaskStopRequested{
				BackgroundTaskID: request.BackgroundTaskID,
			}}
		}
	}
	return core.RelayResponseEnvelope{
		RequestID:       envelope.RequestID,
		ProtocolVersion: envelope.ProtocolVersion,
		Body:            body,
	}
}

// credentialResponse serves a credential or skills request against this
// relay's own harness home. File work runs on a blocking goroutine so
// neither the socket task nor the ACP coordinator is stalled by filesystem
// I/O.
func credentialResponse(ctx context.Context, envelope core.RelayRequestEnvelope, credentials credentialEndpointResult, relayRoot string, loginIdentity *ClaudeLoginIdentity) core.RelayResponseEnvelope {
	var body core.RelayResponseBody
	if credentials.err != nil {
		body = core.RelayResponseBodyError{Error: core.RelayProtocolError{
			Code:    core.RelayErrorCodeInvalidState,
			Message: credentials.err.Error(),
		}}
	} else {
		endpoint := credentials.endpoint
		githubTokenPath := filepath.Join(relayRoot, "github-token")
		var payload core.RelayResponsePayload
		var err, joinErr error
		switch request := envelope.Request.(type) {
		case core.RelayRequestCredentialState:
			release := loginIdentity.installation()
			var snapshot core.CredentialSnapshot
			snapshot, err = readHeldLogin(ctx, &endpoint, loginIdentity)
			release()
			if err == nil {
				payload = credentialStatePayload(&snapshot)
			}
		case core.RelayRequestInstallCredentials:
			payload, err = applyInstallCredentials(ctx, &endpoint, request.Data, loginIdentity)
		default:
			type applied struct {
				payload core.RelayResponsePayload
				err     error
			}
			var result applied
			result, joinErr = core.SpawnBlocking(func() applied {
				payload, err := applyCredentialRequestAt(&endpoint, githubTokenPath, request)
				return applied{payload, err}
			}).Join(ctx)
			payload, err = result.payload, result.err
		}
		switch {
		case joinErr != nil:
			body = core.RelayResponseBodyError{Error: core.RelayProtocolError{
				Code:      core.RelayErrorCodeInternal,
				Message:   "credential task stopped: " + joinErr.Error(),
				Retryable: true,
			}}
		case err != nil:
			body = core.RelayResponseBodyError{Error: core.RelayProtocolError{
				Code:    core.RelayErrorCodeInvalidRequest,
				Message: err.Error(),
			}}
		default:
			body = core.RelayResponseBodyOk{Payload: payload}
		}
	}
	return core.RelayResponseEnvelope{
		RequestID:       envelope.RequestID,
		ProtocolVersion: envelope.ProtocolVersion,
		Body:            body,
	}
}

func applyInstallCredentials(ctx context.Context, endpoint *CredentialEndpoint, data string, loginIdentity *ClaudeLoginIdentity) (core.RelayResponsePayload, error) {
	if len(data) > core.MaxCredentialBytes*2 {
		return nil, fmt.Errorf("credential payload is above the %d byte limit", core.MaxCredentialBytes)
	}
	decoded, err := base64.StdEncoding.DecodeString(data)
	if err != nil {
		return nil, fmt.Errorf("decode credential payload: %w", err)
	}
	if err := core.ValidateCredentialPayload(endpoint.Harness, decoded); err != nil {
		return nil, err
	}
	// File commit and identity publication have the same owner and order,
	// including when several clients install credentials concurrently.
	release := loginIdentity.installation()
	defer release()
	var written error
	if endpoint.Harness.KeepsLoginsInDatabase() {
		marker, harness := endpoint.Marker, endpoint.Harness
		result, joinErr := core.SpawnBlocking(func() error {
			return core.WriteCredentialFile(harness, marker, decoded)
		}).Join(ctx)
		if joinErr != nil {
			written = fmt.Errorf("credential write task stopped: %w", joinErr)
		} else {
			written = result
		}
	} else {
		_, written = core.InstallSessionCredentials(ctx, endpoint.Harness, endpoint.Marker, decoded)
	}
	snapshot, readErr := readHeldLogin(ctx, endpoint, loginIdentity)
	if written != nil {
		return nil, fmt.Errorf("install credentials at %s: %w", endpoint.Marker, written)
	}
	if readErr != nil {
		return nil, readErr
	}
	return credentialStatePayload(&snapshot), nil
}

// readHeldLogin reads what the harness holds under the same worker lock as
// installation. A failed read clears the observation so a previously held
// login cannot masquerade as evidence about the result of this install.
func readHeldLogin(ctx context.Context, endpoint *CredentialEndpoint, loginIdentity *ClaudeLoginIdentity) (core.CredentialSnapshot, error) {
	marker, kind := endpoint.Marker, endpoint.Harness
	type held struct {
		snapshot core.CredentialSnapshot
		bytes    []byte
		err      error
	}
	read, joinErr := core.SpawnBlocking(func() held {
		snapshot, bytes, err := core.ReadCredentialFile(kind, marker)
		return held{snapshot, bytes, err}
	}).Join(ctx)
	err := read.err
	if joinErr != nil {
		err = fmt.Errorf("credential read-back task stopped: %w", joinErr)
	}
	var fingerprint *string
	if kind == core.HarnessKindClaude {
		if err == nil {
			if value, ok := core.ClaudeAccessTokenFingerprint(read.bytes); ok {
				fingerprint = &value
			}
		}
		loginIdentity.installed(fingerprint)
	} else {
		if err == nil && read.snapshot.Present {
			value := read.snapshot.Fingerprint
			fingerprint = &value
		}
		loginIdentity.observeFile(fingerprint)
	}
	if err != nil {
		return core.CredentialSnapshot{}, err
	}
	return read.snapshot, nil
}

func observeInitialLogin(ctx context.Context, endpoint *CredentialEndpoint, loginIdentity *ClaudeLoginIdentity) error {
	release := loginIdentity.installation()
	defer release()
	_, err := readHeldLogin(ctx, endpoint, loginIdentity)
	return err
}

// projectMemoryIOOutcome is a finished project memory operation: its value
// and error, or joinErr when its goroutine panicked (Rust: JoinError).
type projectMemoryIOOutcome[T any] struct {
	value   T
	err     error
	joinErr error
}

// runSerializedProjectMemoryIO snapshots and installs project memory off the
// socket task. These payloads are connection-only and are never journaled as
// conversation history. projectMemoryIO is the endpoint's one-slot
// semaphore; ctx cancels the wait for it, and a read (but not a mutating
// write) once admitted. False (Rust's None) when cancelled.
func runSerializedProjectMemoryIO[T any](ctx context.Context, projectMemoryIO chan struct{}, mutatesMemory bool, operation func() (T, error)) (projectMemoryIOOutcome[T], bool) {
	// Cancellation is checked first, as Rust's biased select does.
	if ctx.Err() != nil {
		return projectMemoryIOOutcome[T]{}, false
	}
	select {
	case <-ctx.Done():
		return projectMemoryIOOutcome[T]{}, false
	case projectMemoryIO <- struct{}{}:
	}
	// Acquiring the semaphore admits the operation. Nothing separates that
	// decision from starting its goroutine. A stalled mutating write holding
	// the checkpoint barrier is the true filesystem state, not something to
	// time out.
	work := core.SpawnBlocking(func() projectMemoryIOOutcome[T] {
		defer func() { <-projectMemoryIO }()
		value, err := operation()
		return projectMemoryIOOutcome[T]{value: value, err: err}
	})
	joined := func() projectMemoryIOOutcome[T] {
		outcome, joinErr := work.Join(context.Background())
		if joinErr != nil {
			return projectMemoryIOOutcome[T]{joinErr: joinErr}
		}
		return outcome
	}
	if mutatesMemory {
		return joined(), true
	}
	// A disconnected reader can leave this goroutine running, but its permit
	// and transfer state stay with it until its filesystem work finishes.
	if ctx.Err() == nil {
		select {
		case <-work.Done():
			return joined(), true
		case <-ctx.Done():
		}
	}
	// Teardown need not wait for a read, but its completion still has an
	// observer that reports task failures and panics.
	go func() {
		outcome := joined()
		switch {
		case outcome.joinErr != nil:
			slog.Warn("cancelled project-memory read task stopped", "error", outcome.joinErr)
		case outcome.err != nil:
			slog.Warn("cancelled project-memory read failed", "error", outcome.err.Error())
		}
	}()
	return projectMemoryIOOutcome[T]{}, false
}

// unixNotifyOne is tokio's Notify::notify_one on a one-slot channel: at most one
// pending signal is kept.
func unixNotifyOne(signal chan struct{}) {
	if signal == nil {
		return
	}
	select {
	case signal <- struct{}{}:
	default:
	}
}

// projectMemoryResponse answers a project memory request; nil when ctx
// cancelled it before it was admitted (the connection went away).
func projectMemoryResponse(ctx context.Context, envelope core.RelayRequestEnvelope, endpoint *projectMemoryEndpoint, transfer *connectionProjectMemoryTransfer) *core.RelayResponseEnvelope {
	var body core.RelayResponseBody
	if endpoint.config == nil {
		body = core.RelayResponseBodyError{Error: core.RelayProtocolError{
			Code:    core.RelayErrorCodeInvalidState,
			Message: "this session has no project memory endpoint",
		}}
	} else {
		memory := endpoint.config
		request := envelope.Request
		requestID := envelope.RequestID
		protocolVersion := envelope.ProtocolVersion
		connectionTransfer := *transfer
		*transfer = connectionProjectMemoryTransfer{}
		mutatesMemory := false
		switch request := request.(type) {
		case core.RelayRequestInstallProjectMemorySnapshot, core.RelayRequestReplaceProjectMemoryTree:
			mutatesMemory = true
		case core.RelayRequestProjectMemoryTransfer:
			_, mutatesMemory = request.Action.(core.ProjectMemoryTransferActionCommitUpload)
		}
		unixNotifyOne(endpoint.ioRequested)
		ioBlock := endpoint.ioBlock
		type answered struct {
			payload  core.RelayResponsePayload
			err      error
			transfer connectionProjectMemoryTransfer
		}
		outcome, admitted := runSerializedProjectMemoryIO(ctx, endpoint.io, mutatesMemory, func() (answered, error) {
			if ioBlock != nil {
				unixNotifyOne(ioBlock.started)
				released, open := <-ioBlock.release
				if !open {
					return answered{}, errors.New("test I/O gate closed: receiving on a closed channel")
				}
				if released != nil {
					return answered{}, released
				}
				if ioBlock.panicAfterRelease {
					panic("controlled read panic")
				}
			}
			var payload core.RelayResponsePayload
			var err error
			if transferRequest, ok := request.(core.RelayRequestProjectMemoryTransfer); ok {
				payload, err = applyProjectMemoryTransferRequest(memory, &connectionTransfer, transferRequest.Action, requestID, protocolVersion)
			} else {
				payload, err = applyProjectMemoryRequest(memory, request)
			}
			if err != nil {
				slog.Warn("project-memory request failed",
					"error", err.Error(),
					"project_key", memory.ProjectKey,
					"operation", core.RelayRequestMethodName(request))
			}
			return answered{payload: payload, err: err, transfer: connectionTransfer}, nil
		})
		if !admitted {
			return nil
		}
		switch {
		case outcome.joinErr != nil:
			body = core.RelayResponseBodyError{Error: core.RelayProtocolError{
				Code:      core.RelayErrorCodeInternal,
				Message:   "project memory task stopped: " + outcome.joinErr.Error(),
				Retryable: true,
			}}
		case outcome.err != nil:
			body = core.RelayResponseBodyError{Error: core.RelayProtocolError{
				Code:      core.RelayErrorCodeInternal,
				Message:   outcome.err.Error(),
				Retryable: true,
			}}
		default:
			*transfer = outcome.value.transfer
			if outcome.value.err != nil {
				body = core.RelayResponseBodyError{Error: core.RelayProtocolError{
					Code:    core.RelayErrorCodeInvalidRequest,
					Message: outcome.value.err.Error(),
				}}
			} else {
				body = core.RelayResponseBodyOk{Payload: outcome.value.payload}
			}
		}
	}
	return &core.RelayResponseEnvelope{
		RequestID:       envelope.RequestID,
		ProtocolVersion: envelope.ProtocolVersion,
		Body:            body,
	}
}

// connectionProjectMemoryTransfer is the project memory transfer one
// connection owns.
type connectionProjectMemoryTransfer struct {
	nextTransferID uint64
	// active is nil (Rust's ActiveProjectMemoryTransfer::None) when the
	// connection owns no transfer.
	active activeProjectMemoryTransfer
}

// activeProjectMemoryTransfer is a connection's transfer in progress; nil is
// Rust's None variant, the default. Variants are held as pointers, so a page
// read or an appended chunk updates the active transfer in place.
type activeProjectMemoryTransfer interface{ isActiveProjectMemoryTransfer() }

type activeProjectMemoryTransferDownload struct {
	transferID uint64
	download   *core.FrozenSnapshotDownload
}

type activeProjectMemoryTransferUpload struct {
	transferID uint64
	kind       core.ProjectMemoryUploadKind
	totalBytes uint64
	bytes      []byte
}

func (*activeProjectMemoryTransferDownload) isActiveProjectMemoryTransfer() {}
func (*activeProjectMemoryTransferUpload) isActiveProjectMemoryTransfer()   {}

func (t *connectionProjectMemoryTransfer) allocateID() (uint64, error) {
	if t.nextTransferID == math.MaxUint64 {
		return 0, errors.New("project-memory transfer ID space exhausted for this connection")
	}
	t.nextTransferID++
	return t.nextTransferID, nil
}

func applyProjectMemoryTransferRequest(memory *core.ProjectMemoryLaunchConfig, transfer *connectionProjectMemoryTransfer, action core.ProjectMemoryTransferAction, requestID string, protocolVersion uint32) (core.RelayResponsePayload, error) {
	transferPayload := func(result core.ProjectMemoryTransferResult) core.RelayResponsePayload {
		return core.RelayResponsePayloadProjectMemoryTransfer{Result: result}
	}
	switch action := action.(type) {
	case core.ProjectMemoryTransferActionBeginSnapshot:
		if transfer.active != nil {
			return nil, errors.New("this connection already owns a project-memory transfer; abort it first")
		}
		baselineStore := core.NewProjectMemoryStore(memory.BaselineRoot)
		baseline, err := baselineStore.Snapshot()
		if err != nil {
			return nil, err
		}
		replicaStore := core.NewProjectMemoryStore(memory.Root)
		replica, err := replicaStore.Snapshot()
		if err != nil {
			return nil, err
		}
		encoded, err := core.ProjectMemoryWireSnapshotEncode(baseline, replica, protocolVersion)
		if err != nil {
			return nil, err
		}
		totalBytes := uint64(len(encoded))
		transferID, err := transfer.allocateID()
		if err != nil {
			return nil, err
		}
		transfer.active = &activeProjectMemoryTransferDownload{
			transferID: transferID,
			download:   core.NewFrozenSnapshotDownload(encoded),
		}
		return transferPayload(core.ProjectMemoryTransferResultSnapshotStarted{
			TransferID: transferID,
			TotalBytes: totalBytes,
		}), nil
	case core.ProjectMemoryTransferActionReadSnapshotPage:
		download, ok := transfer.active.(*activeProjectMemoryTransferDownload)
		if !ok {
			return nil, errors.New("this connection has no project-memory snapshot transfer")
		}
		if action.TransferID != download.transferID {
			return nil, errors.New("project-memory snapshot transfer ID does not match this connection")
		}
		capacity, err := core.ProjectMemoryTransferResponseChunkBytes(protocolVersion, requestID)
		if err != nil {
			return nil, err
		}
		page, err := download.download.Read(action.Offset, capacity)
		if err != nil {
			return nil, err
		}
		id := download.transferID
		if page.Complete {
			transfer.active = nil
		}
		return transferPayload(core.ProjectMemoryTransferResultSnapshotPage{
			TransferID: id,
			Offset:     page.Offset,
			NextOffset: page.NextOffset,
			Complete:   page.Complete,
			Chunk:      page.Chunk,
		}), nil
	case core.ProjectMemoryTransferActionBeginUpload:
		if transfer.active != nil {
			return nil, errors.New("this connection already owns a project-memory transfer; abort it first")
		}
		if action.TotalBytes > uint64(math.MaxInt) {
			return nil, errors.New("project-memory upload exceeds addressable memory: out of range integral type conversion attempted")
		}
		transferID, err := transfer.allocateID()
		if err != nil {
			return nil, err
		}
		transfer.active = &activeProjectMemoryTransferUpload{
			transferID: transferID,
			kind:       action.Kind,
			totalBytes: action.TotalBytes,
			bytes:      []byte{},
		}
		return transferPayload(core.ProjectMemoryTransferResultUploadStarted{TransferID: transferID}), nil
	case core.ProjectMemoryTransferActionAppendUpload:
		upload, ok := transfer.active.(*activeProjectMemoryTransferUpload)
		if !ok {
			return nil, errors.New("this connection has no project-memory upload transfer")
		}
		if action.TransferID != upload.transferID {
			return nil, errors.New("project-memory upload transfer ID does not match this connection")
		}
		expectedOffset := uint64(len(upload.bytes))
		if action.Offset != expectedOffset {
			return nil, fmt.Errorf("project-memory upload offset mismatch: expected %d, got %d", expectedOffset, action.Offset)
		}
		decoded, err := base64.StdEncoding.DecodeString(action.Chunk)
		if err != nil {
			return nil, fmt.Errorf("decode project-memory upload chunk: %w", err)
		}
		if len(decoded) == 0 {
			return nil, errors.New("project-memory upload chunk is empty")
		}
		newLen := len(upload.bytes) + len(decoded)
		if uint64(newLen) > upload.totalBytes {
			return nil, errors.New("project-memory upload exceeds its declared byte length")
		}
		upload.bytes = append(upload.bytes, decoded...)
		return transferPayload(core.ProjectMemoryTransferResultUploadProgress{
			TransferID: upload.transferID,
			NextOffset: uint64(newLen),
		}), nil
	case core.ProjectMemoryTransferActionCommitUpload:
		upload, ok := transfer.active.(*activeProjectMemoryTransferUpload)
		if !ok {
			// Rust takes the active transfer before matching it, so a
			// download is dropped here.
			transfer.active = nil
			return nil, errors.New("this connection has no project-memory upload transfer")
		}
		if action.TransferID != upload.transferID {
			return nil, errors.New("project-memory upload transfer ID does not match this connection")
		}
		if uint64(len(upload.bytes)) != upload.totalBytes {
			return nil, errors.New("project-memory upload is incomplete")
		}
		transfer.active = nil
		tree, err := core.UnmarshalProjectMemoryWireTreeJSON(upload.bytes)
		if err != nil {
			return nil, fmt.Errorf("parse uploaded project-memory tree: %w", err)
		}
		switch kind := upload.kind.(type) {
		case core.ProjectMemoryUploadKindReplaceTree:
			payload, err := applyProjectMemoryRequest(memory, core.RelayRequestReplaceProjectMemoryTree{
				ExpectedReplica: kind.ExpectedReplica,
				Tree:            tree,
			})
			if err != nil {
				return nil, err
			}
			replaced, ok := payload.(core.RelayResponsePayloadProjectMemoryTreeReplaced)
			if !ok {
				return nil, errors.New("project-memory replacement returned an unexpected result")
			}
			return transferPayload(core.ProjectMemoryTransferResultReplaced{Outcome: replaced.Outcome}), nil
		default:
			if _, err := applyProjectMemoryRequest(memory, core.RelayRequestInstallProjectMemorySnapshot{Snapshot: tree}); err != nil {
				return nil, err
			}
			return transferPayload(core.ProjectMemoryTransferResultInstalled{}), nil
		}
	case core.ProjectMemoryTransferActionAbort:
		var activeID uint64
		switch active := transfer.active.(type) {
		case *activeProjectMemoryTransferDownload:
			activeID = active.transferID
		case *activeProjectMemoryTransferUpload:
			activeID = active.transferID
		default:
			return nil, errors.New("this connection has no project-memory transfer to abort")
		}
		if action.TransferID != activeID {
			return nil, errors.New("project-memory transfer ID does not match this connection")
		}
		transfer.active = nil
		return transferPayload(core.ProjectMemoryTransferResultAborted{TransferID: activeID}), nil
	}
	return nil, fmt.Errorf("unknown project-memory transfer action %T", action)
}

func applyProjectMemoryRequest(memory *core.ProjectMemoryLaunchConfig, request core.RelayRequest) (core.RelayResponsePayload, error) {
	replica := core.NewProjectMemoryStore(memory.Root)
	baseline := core.NewProjectMemoryStore(memory.BaselineRoot)
	switch request := request.(type) {
	case core.RelayRequestProjectMemorySnapshot:
		baselineSnapshot, err := baseline.Snapshot()
		if err != nil {
			return nil, err
		}
		baselineTree, err := core.ProjectMemoryWireTreeEncode(baselineSnapshot, core.RelayProtocolVersion)
		if err != nil {
			return nil, err
		}
		replicaSnapshot, err := replica.Snapshot()
		if err != nil {
			return nil, err
		}
		replicaTree, err := core.ProjectMemoryWireTreeEncode(replicaSnapshot, core.RelayProtocolVersion)
		if err != nil {
			return nil, err
		}
		return core.RelayResponsePayloadProjectMemorySnapshot{Baseline: baselineTree, Replica: replicaTree}, nil
	case core.RelayRequestInstallProjectMemorySnapshot:
		snapshot, err := core.ProjectMemoryWireTreeDecode(request.Snapshot)
		if err != nil {
			return nil, err
		}
		if _, err := replica.InstallSnapshot(&snapshot); err != nil {
			return nil, err
		}
		if _, err := baseline.InstallSnapshot(&snapshot); err != nil {
			return nil, err
		}
		return core.RelayResponsePayloadProjectMemorySnapshotInstalled{}, nil
	case core.RelayRequestReplaceProjectMemoryTree:
		currentReplica, err := replica.Snapshot()
		if err != nil {
			return nil, err
		}
		if currentReplica.Version() != request.ExpectedReplica {
			return core.RelayResponsePayloadProjectMemoryTreeReplaced{
				Outcome: core.ReplicaReplaceOutcomeReplicaChanged,
			}, nil
		}
		tree, err := core.ProjectMemoryWireTreeDecode(request.Tree)
		if err != nil {
			return nil, err
		}
		if _, err := replica.ReplaceTree(&tree); err != nil {
			return nil, err
		}
		if _, err := baseline.ReplaceTree(&tree); err != nil {
			return nil, err
		}
		return core.RelayResponsePayloadProjectMemoryTreeReplaced{
			Outcome: core.ReplicaReplaceOutcomeReplaced,
		}, nil
	}
	return nil, fmt.Errorf("%s is not a project memory request", core.RelayRequestMethodName(request))
}

// applyCredentialRequest applies request with the GitHub token in the
// harness home (Rust: cfg(test)).
func applyCredentialRequest(endpoint *CredentialEndpoint, request core.RelayRequest) (core.RelayResponsePayload, error) {
	return applyCredentialRequestAt(endpoint, filepath.Join(endpoint.Home, "github-token"), request)
}

func applyCredentialRequestAt(endpoint *CredentialEndpoint, githubTokenPath string, request core.RelayRequest) (core.RelayResponsePayload, error) {
	slopcopLoginPath := filepath.Join(endpoint.Home, core.LoginFile)
	stateOf := func(snapshot core.CredentialSnapshot, err error) (core.RelayResponsePayload, error) {
		if err != nil {
			return nil, err
		}
		return credentialStatePayload(&snapshot), nil
	}
	switch request := request.(type) {
	case core.RelayRequestAwsCredentialsState:
		return stateOf(core.AwsCredentialsState(endpoint.Home))
	case core.RelayRequestAwsCredentialsScope:
		scope, err := core.AwsCredentialsScope(endpoint.Home)
		if err != nil {
			return nil, err
		}
		profile, staged, err := core.AwsCredentialsStagedProfile(endpoint.Home)
		if err != nil {
			return nil, err
		}
		payload := core.RelayResponsePayloadAwsCredentialsScope{Scope: scope}
		if staged {
			payload.Profile = &profile
		}
		return payload, nil
	case core.RelayRequestRemoveAwsCredentials:
		return stateOf(core.AwsCredentialsRemove(endpoint.Home))
	case core.RelayRequestSlopcopLoginState:
		snapshot, _, err := core.ReadLogin(slopcopLoginPath)
		return stateOf(snapshot, err)
	case core.RelayRequestInstallSlopcopLogin:
		if len(request.Data) > core.MaxLoginBytes*2 {
			return nil, fmt.Errorf("slopcop login is above the %d byte limit", core.MaxLoginBytes)
		}
		decoded, err := base64.StdEncoding.DecodeString(request.Data)
		if err != nil {
			return nil, fmt.Errorf("decode slopcop login payload: %w", err)
		}
		return stateOf(core.WriteLogin(slopcopLoginPath, decoded))
	case core.RelayRequestRemoveSlopcopLogin:
		if err := core.RemoveLogin(slopcopLoginPath); err != nil {
			return nil, err
		}
		return stateOf(core.CredentialSnapshotAbsent(), nil)
	case core.RelayRequestInstallAwsCredentials:
		if len(request.Data) > core.MaxCredentialBytes*2 {
			return nil, fmt.Errorf("AWS credential payload is above the %d byte limit", core.MaxCredentialBytes)
		}
		decoded, err := base64.StdEncoding.DecodeString(request.Data)
		if err != nil {
			return nil, fmt.Errorf("decode AWS credential payload: %w", err)
		}
		credentials, err := core.AwsCredentialsDecode(decoded)
		if err != nil {
			return nil, err
		}
		return stateOf(credentials.WithScope(request.Scope).Install(endpoint.Home, endpoint.Home))
	case core.RelayRequestCredentialState:
		snapshot, _, err := core.ReadCredentialFile(endpoint.Harness, endpoint.Marker)
		return stateOf(snapshot, err)
	case core.RelayRequestReadCredentials:
		snapshot, data, err := core.ReadCredentialFile(endpoint.Harness, endpoint.Marker)
		if err != nil {
			return nil, err
		}
		if !snapshot.Present {
			return nil, fmt.Errorf("session has no %s credentials", endpoint.Marker)
		}
		return core.RelayResponsePayloadCredentials{Data: base64.StdEncoding.EncodeToString(data)}, nil
	case core.RelayRequestInstallCredentials:
		if !endpoint.Harness.KeepsLoginsInDatabase() {
			return nil, errors.New("file credential installs must use the async destination-owned install path")
		}
		if len(request.Data) > core.MaxCredentialBytes*2 {
			return nil, fmt.Errorf("credential payload is above the %d byte limit", core.MaxCredentialBytes)
		}
		decoded, err := base64.StdEncoding.DecodeString(request.Data)
		if err != nil {
			return nil, fmt.Errorf("decode credential payload: %w", err)
		}
		if err := core.WriteCredentialFile(endpoint.Harness, endpoint.Marker, decoded); err != nil {
			return nil, err
		}
		return stateOf(core.CredentialSnapshotOf(endpoint.Harness, decoded), nil)
	case core.RelayRequestSkillsState:
		archive, err := core.CollectSkills(endpoint.Harness, endpoint.Home)
		if err != nil {
			return nil, err
		}
		state := archive.State()
		return skillsStatePayload(&state), nil
	case core.RelayRequestInstallSkills:
		// Base64 inflates by a third; rejecting early keeps a hostile
		// controller from making the worker buffer an endless frame.
		if len(request.Data) > core.MaxSkillsArchiveBytes*2 {
			return nil, fmt.Errorf("skills payload is above the %d byte archive limit", core.MaxSkillsArchiveBytes)
		}
		decoded, err := base64.StdEncoding.DecodeString(request.Data)
		if err != nil {
			return nil, fmt.Errorf("decode skills payload: %w", err)
		}
		archive, err := core.DecodeSkillsArchive(decoded)
		if err != nil {
			return nil, err
		}
		if err := core.InstallSkills(endpoint.Harness, endpoint.Home, &archive); err != nil {
			return nil, err
		}
		installed, err := core.CollectSkills(endpoint.Harness, endpoint.Home)
		if err != nil {
			return nil, err
		}
		state := installed.State()
		return skillsStatePayload(&state), nil
	case core.RelayRequestGithubTokenState:
		snapshot, _, err := core.ReadGithubToken(githubTokenPath)
		if err != nil {
			return nil, err
		}
		return githubTokenStatePayload(&snapshot), nil
	case core.RelayRequestInstallGithubToken:
		if endpoint.NoGithubCredentials {
			return nil, errors.New("GitHub credentials are disabled for this session")
		}
		if len(request.Data) > core.MaxGithubTokenBytes*2 {
			return nil, fmt.Errorf("GitHub token is above the %d byte limit", core.MaxGithubTokenBytes)
		}
		decoded, err := base64.StdEncoding.DecodeString(request.Data)
		if err != nil {
			return nil, fmt.Errorf("decode GitHub token payload: %w", err)
		}
		snapshot, err := core.WriteGithubToken(githubTokenPath, decoded)
		if err != nil {
			return nil, err
		}
		return githubTokenStatePayload(&snapshot), nil
	case core.RelayRequestRemoveGithubToken:
		if err := core.RemoveGithubToken(githubTokenPath); err != nil {
			return nil, err
		}
		absent := core.GithubTokenSnapshotAbsent()
		return githubTokenStatePayload(&absent), nil
	}
	return nil, fmt.Errorf("%s is not a credential, GitHub token, or skills request", core.RelayRequestMethodName(request))
}

func skillsStatePayload(state *core.SkillsSyncState) core.RelayResponsePayload {
	return core.RelayResponsePayloadSkillsState{Present: state.Present, Fingerprint: state.Fingerprint}
}

func credentialStatePayload(snapshot *core.CredentialSnapshot) core.RelayResponsePayload {
	return core.RelayResponsePayloadCredentialState{
		Present:          snapshot.Present,
		Fingerprint:      snapshot.Fingerprint,
		FreshnessEpochMs: snapshot.FreshnessEpochMs,
	}
}

func githubTokenStatePayload(snapshot *core.GithubTokenSnapshot) core.RelayResponsePayload {
	return core.RelayResponsePayloadGithubTokenState{Present: snapshot.Present, Fingerprint: snapshot.Fingerprint}
}

// writeResponse writes one response frame: its JSON, as serde_json writes
// it, and a newline.
func writeResponse(writer io.Writer, response *core.RelayResponseEnvelope) error {
	var encoded bytes.Buffer
	encoder := json.NewEncoder(&encoded)
	encoder.SetEscapeHTML(false)
	// Encode ends the frame with its newline.
	if err := encoder.Encode(response); err != nil {
		return err
	}
	if encoded.Len() > core.MaxFrameBytes {
		return errors.New("relay response frame is too large")
	}
	_, err := writer.Write(encoded.Bytes())
	return err
}

// writeLoggedResponse writes a response, logging it when it is an error.
// Protocol rejections are ordinary responses rather than Go errors, so the
// socket loop must log them explicitly. Keep this next to the write boundary
// so every request route (including credentials and old protocol methods)
// gets the same session and operation context.
func writeLoggedResponse(writer io.Writer, response *core.RelayResponseEnvelope, sessionID, operation string) error {
	if failure, ok := response.Body.(core.RelayResponseBodyError); ok {
		slog.Warn("relay request returned an error",
			"session_id", sessionID,
			"operation", operation,
			"request_id", response.RequestID,
			"protocol_version", response.ProtocolVersion,
			"relay_error_code", string(failure.Error.Code),
			"relay_retryable", failure.Error.Retryable,
			"error_message", failure.Error.Message)
	}
	return writeResponse(writer, response)
}
