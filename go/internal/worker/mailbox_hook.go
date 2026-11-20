// Port of mj-worker/src/mailbox_hook.rs.

// Harness-hook client for draining the worker-owned session mailbox.

package worker

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"runtime"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// MailboxHookRequest is mailbox_hook::request: let a harness retain leased
// input before acknowledging it. It reads one mailbox drain or
// acknowledgement request from stdin and writes the worker's answer to
// stdout, reusing the same framed IPC, request deadline, and long Unix socket
// path handling as hooks.
func MailboxHookRequest(socket string) error {
	input, err := io.ReadAll(os.Stdin)
	if err != nil {
		return err
	}
	request, err := core.UnmarshalRelayRequestJSON(input)
	if err != nil {
		return err
	}
	switch request.(type) {
	case core.RelayRequestDrainMailbox, core.RelayRequestAckMailbox:
	default:
		return errors.New("expected a mailbox drain or acknowledgement")
	}
	payload, err := mailboxHookSendRequest(socket, request)
	if err != nil {
		return err
	}
	encoded, err := json.Marshal(payload)
	if err != nil {
		return err
	}
	value, err := jsonvalue.Parse(encoded)
	if err != nil {
		return err
	}
	return mailboxHookWriteStdout(value)
}

// MailboxHookRun is mailbox_hook::run: consume one harness hook request,
// write its response, then acknowledge the relay lease. Relay unavailability
// is deliberately fail-open: an unacked lease returns to the mailbox after
// its timeout or a worker restart.
func MailboxHookRun(socket string, hookEvent string) error {
	input, err := io.ReadAll(os.Stdin)
	if err != nil {
		return mailboxHookWriteEmptyWithDiagnostic(fmt.Sprintf("could not consume hook input: %s", core.IOErrorDisplay(err)))
	}
	// Input that is not a hook object reads as a hook without an agent: only
	// a read failure is a reason not to drain.
	var hookInput mailboxHookInput
	if json.Unmarshal(input, &hookInput) != nil {
		hookInput = mailboxHookInput{}
	}
	if isSubagentHookInput(&hookInput) {
		return mailboxHookWriteStdout(jsonvalue.Object{})
	}

	text, count, leaseID, err := mailboxHookDrain(socket, hookEvent)
	switch {
	case err != nil:
		return mailboxHookWriteEmptyWithDiagnostic(fmt.Sprintf("could not drain mailbox: %v", err))
	case text != nil && count > 0 && leaseID != nil:
		if err := mailboxHookWriteStdout(jsonvalue.Object{
			{Key: "hookSpecificOutput", Value: jsonvalue.Object{
				{Key: "hookEventName", Value: hookEvent},
				{Key: "additionalContext", Value: *text},
			}},
		}); err != nil {
			return err
		}
		if err := mailboxHookAcknowledge(socket, *leaseID); err != nil {
			core.DiagnosticStderr("Mjolnir mailbox hook: could not acknowledge delivered events: %v", err)
		}
		return nil
	case text == nil && count == 0 && leaseID == nil:
		return mailboxHookWriteStdout(jsonvalue.Object{})
	default:
		return mailboxHookWriteEmptyWithDiagnostic("worker returned an incomplete mailbox lease")
	}
}

// mailboxHookInput is the part of a harness hook's input the mailbox hook
// reads.
type mailboxHookInput struct {
	AgentID *string `json:"agent_id"`
}

// isSubagentHookInput reports whether the hook fired inside a sub-agent,
// which must not drain the parent's mailbox.
func isSubagentHookInput(input *mailboxHookInput) bool {
	return input.AgentID != nil && *input.AgentID != ""
}

// mailboxHookDrain leases the pending mailbox text for hookEvent: the text,
// the event count and the lease id, each absent when there is nothing.
func mailboxHookDrain(socket string, hookEvent string) (text *string, count int, leaseID *string, err error) {
	if hookEvent != "PostToolUse" && hookEvent != "PostToolBatch" {
		return nil, 0, nil, errors.New("unsupported mailbox hook event")
	}
	payload, err := mailboxHookSendRequest(socket, core.RelayRequestDrainMailbox{HookEvent: hookEvent})
	if err != nil {
		return nil, 0, nil, err
	}
	drained, ok := payload.(core.RelayResponsePayloadMailboxDrained)
	if !ok {
		return nil, 0, nil, fmt.Errorf("worker refused mailbox hook request: %s", relayResponsePayloadDebug(payload))
	}
	return drained.Text, drained.Count, drained.LeaseID, nil
}

func mailboxHookAcknowledge(socket string, leaseID string) error {
	payload, err := mailboxHookSendRequest(socket, core.RelayRequestAckMailbox{LeaseID: leaseID})
	if err != nil {
		return err
	}
	acknowledged, ok := payload.(core.RelayResponsePayloadMailboxAcknowledged)
	switch {
	case ok && acknowledged.Acknowledged:
		return nil
	case ok:
		return errors.New("worker no longer holds the mailbox lease; it may be delivered again")
	}
	return fmt.Errorf("worker refused mailbox acknowledgement: %s", relayResponsePayloadDebug(payload))
}

// mailboxHookSendRequest sends one relay request over the worker's control
// socket and returns its payload. Workers run on Unix; elsewhere this says
// plainly that the hook cannot run.
func mailboxHookSendRequest(socket string, request core.RelayRequest) (core.RelayResponsePayload, error) {
	if runtime.GOOS == "windows" {
		return nil, fmt.Errorf("the mailbox hook socket %s needs a Unix platform", socket)
	}
	requestTimeout := time.Duration(core.MailboxHookRequestTimeoutSecs) * time.Second
	requestID, err := core.NewSessionID()
	if err != nil {
		return nil, err
	}
	stream, err := core.ConnectUnixStream(socket)
	if err != nil {
		return nil, fmt.Errorf("connect to worker control socket %s: %w", socket, err)
	}
	defer stream.Close()
	connection := timeoutConn{conn: stream, read: requestTimeout, write: requestTimeout}
	envelope, err := json.Marshal(core.RelayRequestEnvelope{
		RequestID:       requestID,
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         request,
	})
	if err != nil {
		return nil, err
	}
	if _, err := connection.Write(append(envelope, '\n')); err != nil {
		return nil, err
	}
	var line []byte
	read, complete, err := core.ReadBoundedLine(bufio.NewReader(connection), &line, core.MaxFrameBytes)
	if err != nil {
		return nil, err
	}
	if read == 0 {
		return nil, errors.New("worker closed the mailbox hook connection")
	}
	if !complete {
		return nil, errors.New("worker returned an incomplete mailbox hook frame")
	}
	var response core.RelayResponseEnvelope
	if err := json.Unmarshal(line, &response); err != nil {
		return nil, err
	}
	if response.RequestID != requestID {
		return nil, errors.New("mailbox hook reply identity mismatch")
	}
	if ok, isOk := response.Body.(core.RelayResponseBodyOk); isOk {
		return ok.Payload, nil
	}
	return nil, fmt.Errorf("worker refused mailbox hook request: %s", relayResponseBodyDebug(response.Body))
}

func mailboxHookWriteStdout(value any) error {
	output, err := jsonvalue.Marshal(value)
	if err != nil {
		return err
	}
	_, err = os.Stdout.Write(append(output, '\n'))
	return err
}

func mailboxHookWriteEmptyWithDiagnostic(message string) error {
	core.DiagnosticStderr("Mjolnir mailbox hook: %s", message)
	return mailboxHookWriteStdout(jsonvalue.Object{})
}
