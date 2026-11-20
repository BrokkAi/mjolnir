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
	"encoding/json"
	"errors"
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
	panic("unported: mj-worker/src/worker_runtime/unix/requests.rs handle_request")
}

// handleRequestReplanAttempts is how often handleRequest plans an attach
// again after sealing or garbage collection moved the segments a plan named
// while it was being read. That is not the controller's fault and not its
// problem. The journal only moves that way once per sealed megabyte or per
// acknowledgement, so a couple of attempts always outrun it.
const handleRequestReplanAttempts = 3

func reviewCaptureResponse(ctx context.Context, envelope core.RelayRequestEnvelope, capture *reviewCapture) core.RelayResponseEnvelope {
	panic("unported: mj-worker/src/worker_runtime/unix/requests.rs review_capture_response")
}

// compactionError is a non-retryable relay error body.
func compactionError(code core.RelayErrorCode, message string) core.RelayResponseBody {
	return core.RelayResponseBodyError{Error: core.RelayProtocolError{Code: code, Message: message}}
}

// elicitationResponse hands a form answer to the ACP runtime and waits for
// its resolution.
func elicitationResponse(ctx context.Context, envelope core.RelayRequestEnvelope, commands *commandChannel) core.RelayResponseEnvelope {
	panic("unported: mj-worker/src/worker_runtime/unix/requests.rs elicitation_response")
}

// backgroundTaskStopResponse asks the ACP runtime to stop a background task
// and waits for it to accept the stop.
func backgroundTaskStopResponse(ctx context.Context, envelope core.RelayRequestEnvelope, commands *commandChannel, relay *RelayCell) core.RelayResponseEnvelope {
	panic("unported: mj-worker/src/worker_runtime/unix/requests.rs background_task_stop_response")
}

// credentialResponse serves a credential or skills request against this
// relay's own harness home. File work runs on a blocking goroutine so
// neither the socket task nor the ACP coordinator is stalled by filesystem
// I/O.
func credentialResponse(ctx context.Context, envelope core.RelayRequestEnvelope, credentials credentialEndpointResult, relayRoot string, loginIdentity *ClaudeLoginIdentity) core.RelayResponseEnvelope {
	panic("unported: mj-worker/src/worker_runtime/unix/requests.rs credential_response")
}

func applyInstallCredentials(ctx context.Context, endpoint *CredentialEndpoint, data string, loginIdentity *ClaudeLoginIdentity) (core.RelayResponsePayload, error) {
	panic("unported: mj-worker/src/worker_runtime/unix/requests.rs apply_install_credentials")
}

// readHeldLogin reads what the harness holds under the same worker lock as
// installation. A failed read clears the observation so a previously held
// login cannot masquerade as evidence about the result of this install.
func readHeldLogin(ctx context.Context, endpoint *CredentialEndpoint, loginIdentity *ClaudeLoginIdentity) (core.CredentialSnapshot, error) {
	panic("unported: mj-worker/src/worker_runtime/unix/requests.rs read_held_login")
}

func observeInitialLogin(ctx context.Context, endpoint *CredentialEndpoint, loginIdentity *ClaudeLoginIdentity) error {
	panic("unported: mj-worker/src/worker_runtime/unix/requests.rs observe_initial_login")
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
	panic("unported: mj-worker/src/worker_runtime/unix/requests.rs run_serialized_project_memory_io")
}

// projectMemoryResponse answers a project memory request; nil when ctx
// cancelled it before it was admitted (the connection went away).
func projectMemoryResponse(ctx context.Context, envelope core.RelayRequestEnvelope, endpoint *projectMemoryEndpoint, transfer *connectionProjectMemoryTransfer) *core.RelayResponseEnvelope {
	panic("unported: mj-worker/src/worker_runtime/unix/requests.rs project_memory_response")
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
	panic("unported: mj-worker/src/worker_runtime/unix/requests.rs apply_project_memory_transfer_request")
}

func applyProjectMemoryRequest(memory *core.ProjectMemoryLaunchConfig, request core.RelayRequest) (core.RelayResponsePayload, error) {
	panic("unported: mj-worker/src/worker_runtime/unix/requests.rs apply_project_memory_request")
}

// applyCredentialRequest applies request with the GitHub token in the
// harness home (Rust: cfg(test)).
func applyCredentialRequest(endpoint *CredentialEndpoint, request core.RelayRequest) (core.RelayResponsePayload, error) {
	return applyCredentialRequestAt(endpoint, filepath.Join(endpoint.Home, "github-token"), request)
}

func applyCredentialRequestAt(endpoint *CredentialEndpoint, githubTokenPath string, request core.RelayRequest) (core.RelayResponsePayload, error) {
	panic("unported: mj-worker/src/worker_runtime/unix/requests.rs apply_credential_request_at")
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
