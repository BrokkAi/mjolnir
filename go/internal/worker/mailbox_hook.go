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
	"strings"
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
	return mailboxHookWriteOutput(os.Stdout, value)
}

// HookOutputFormat is the output format expected by the calling harness's
// hook contract.
type HookOutputFormat int

const (
	HookOutputFormatStandard HookOutputFormat = iota
	HookOutputFormatAntigravity
)

// MailboxHookRun is mailbox_hook::run: consume one harness hook request,
// write its response, then acknowledge the relay lease. Relay unavailability
// is deliberately fail-open: an unacked lease returns to the mailbox after
// its timeout or a worker restart.
func MailboxHookRun(socket string, hookEvent string) error {
	return MailboxHookRunWithFormat(socket, hookEvent, HookOutputFormatStandard)
}

// MailboxHookRunWithFormat consumes one harness hook request using the
// specified hook output format.
func MailboxHookRunWithFormat(socket string, hookEvent string, format HookOutputFormat) error {
	return mailboxHookRun(socket, hookEvent, format, os.Stdin, os.Stdout)
}

func mailboxHookRun(socket string, hookEvent string, format HookOutputFormat, stdin io.Reader, stdout io.Writer) error {
	input, err := io.ReadAll(stdin)
	if err != nil {
		return mailboxHookWriteEmptyWithDiagnostic(stdout, fmt.Sprintf("could not consume hook input: %s", core.IOErrorDisplay(err)))
	}
	// Input that is not a hook object reads as a hook without an agent: only
	// a read failure is a reason not to drain.
	var hookInput mailboxHookInput
	if json.Unmarshal(input, &hookInput) != nil {
		hookInput = mailboxHookInput{}
	}
	if isSubagentHookInput(&hookInput) {
		return mailboxHookWriteOutput(stdout, jsonvalue.Object{})
	}

	if format == HookOutputFormatAntigravity && hookEvent == "Stop" {
		payload, err := mailboxHookSendRequest(socket, core.RelayRequestAntigravityStop{
			ConversationID:    hookInput.ConversationID,
			FullyIdle:         hookInput.FullyIdle,
			TerminationReason: hookInput.TerminationReason,
		})
		switch {
		case err != nil:
			return mailboxHookWriteEmptyWithDiagnostic(stdout, fmt.Sprintf("could not observe Stop: %v", err))
		default:
			if _, observed := payload.(core.RelayResponsePayloadAntigravityStopObserved); observed {
				return mailboxHookWriteOutput(stdout, jsonvalue.Object{})
			}
			return mailboxHookWriteEmptyWithDiagnostic(stdout, fmt.Sprintf("worker refused Stop: %s", mailboxHookPayloadDebug(payload)))
		}
	}

	var conversationID *string
	var initialNumSteps *int64
	if format == HookOutputFormatAntigravity {
		conversationID, initialNumSteps = hookInput.ConversationID, hookInput.InitialNumSteps
	}

	text, count, leaseID, err := mailboxHookDrain(socket, hookEvent, conversationID, initialNumSteps)
	switch {
	case err != nil:
		return mailboxHookWriteEmptyWithDiagnostic(stdout, fmt.Sprintf("could not drain mailbox: %v", err))
	case text != nil && count > 0 && leaseID != nil:
		output := jsonvalue.Object{
			{Key: "hookSpecificOutput", Value: jsonvalue.Object{
				{Key: "hookEventName", Value: hookEvent},
				{Key: "additionalContext", Value: *text},
			}},
		}
		if format == HookOutputFormatAntigravity {
			output = jsonvalue.Object{
				{Key: "injectSteps", Value: []any{
					jsonvalue.Object{
						{Key: "systemMessage", Value: jsonvalue.Object{
							{Key: "systemMessage", Value: *text},
						}},
					},
				}},
			}
		}
		if err := mailboxHookWriteOutput(stdout, output); err != nil {
			return err
		}
		if err := mailboxHookAcknowledge(socket, *leaseID); err != nil {
			core.DiagnosticStderr("Mjolnir mailbox hook: could not acknowledge delivered events: %v", err)
		}
		return nil
	case text == nil && count == 0 && leaseID == nil:
		return mailboxHookWriteOutput(stdout, jsonvalue.Object{})
	default:
		return mailboxHookWriteEmptyWithDiagnostic(stdout, "worker returned an incomplete mailbox lease")
	}
}

// mailboxHookInput is the part of a harness hook's input the mailbox hook
// reads.
type mailboxHookInput struct {
	AgentID           *string `json:"agent_id"`
	ConversationID    *string `json:"conversationId"`
	InitialNumSteps   *int64  `json:"initialNumSteps"`
	FullyIdle         *bool   `json:"fullyIdle"`
	TerminationReason *string `json:"terminationReason"`
}

// isSubagentHookInput reports whether the hook fired inside a sub-agent,
// which must not drain the parent's mailbox.
func isSubagentHookInput(input *mailboxHookInput) bool {
	return input.AgentID != nil && *input.AgentID != ""
}

// mailboxHookDrain leases the pending mailbox text for hookEvent: the text,
// the event count and the lease id, each absent when there is nothing.
func mailboxHookDrain(socket string, hookEvent string, conversationID *string, initialNumSteps *int64) (text *string, count int, leaseID *string, err error) {
	if hookEvent != "PostToolUse" && hookEvent != "PostToolBatch" {
		return nil, 0, nil, errors.New("unsupported mailbox hook event")
	}
	payload, err := mailboxHookSendRequest(socket, core.RelayRequestDrainMailbox{
		HookEvent:       hookEvent,
		ConversationID:  conversationID,
		InitialNumSteps: initialNumSteps,
	})
	if err != nil {
		return nil, 0, nil, err
	}
	drained, ok := payload.(core.RelayResponsePayloadMailboxDrained)
	if !ok {
		return nil, 0, nil, fmt.Errorf("worker refused mailbox hook request: %s", mailboxHookPayloadDebug(payload))
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
	return fmt.Errorf("worker refused mailbox acknowledgement: %s", mailboxHookPayloadDebug(payload))
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
	return nil, fmt.Errorf("worker refused mailbox hook request: %s", mailboxHookBodyDebug(response.Body))
}

func mailboxHookWriteOutput(stdout io.Writer, value any) error {
	output, err := jsonvalue.Marshal(value)
	if err != nil {
		return err
	}
	_, err = stdout.Write(append(output, '\n'))
	return err
}

func mailboxHookWriteEmptyWithDiagnostic(stdout io.Writer, message string) error {
	core.DiagnosticStderr("Mjolnir mailbox hook: %s", message)
	return mailboxHookWriteOutput(stdout, jsonvalue.Object{})
}

// mailboxHookBodyDebug renders a relay response body as Rust's `{:?}` shows
// it, for the refusals the mailbox hook client reports.
func mailboxHookBodyDebug(body core.RelayResponseBody) string {
	switch body := body.(type) {
	case core.RelayResponseBodyError:
		detail := "None"
		if body.Error.Detail != nil {
			detail = fmt.Sprintf("Some(%+v)", body.Error.Detail)
		}
		return fmt.Sprintf("Error { error: RelayProtocolError { code: %s, message: %s, retryable: %t, detail: %s } }",
			relayCommandKindDebug(core.RelayCommandKind(body.Error.Code)), mailboxHookDebugString(body.Error.Message), body.Error.Retryable, detail)
	case core.RelayResponseBodyOk:
		return fmt.Sprintf("Ok { payload: %s }", mailboxHookPayloadDebug(body.Payload))
	}
	return fmt.Sprintf("%+v", body)
}

// mailboxHookPayloadDebug names a payload as Rust's `{:?}` does, with its
// fields in Go's form.
func mailboxHookPayloadDebug(payload core.RelayResponsePayload) string {
	name := fmt.Sprintf("%T", payload)
	name = name[strings.LastIndexByte(name, '.')+1:]
	name = strings.TrimPrefix(name, "RelayResponsePayload")
	return name + " " + fmt.Sprintf("%+v", payload)
}

// mailboxHookDebugString is a string as Rust's `{:?}` shows it.
func mailboxHookDebugString(text string) string {
	var quoted strings.Builder
	quoted.WriteByte('"')
	for _, r := range text {
		switch r {
		case '"':
			quoted.WriteString(`\"`)
		case '\\':
			quoted.WriteString(`\\`)
		case '\n':
			quoted.WriteString(`\n`)
		case '\r':
			quoted.WriteString(`\r`)
		case '\t':
			quoted.WriteString(`\t`)
		case 0:
			quoted.WriteString(`\0`)
		default:
			if r < 0x20 || r == 0x7f {
				fmt.Fprintf(&quoted, `\u{%x}`, r)
			} else {
				quoted.WriteRune(r)
			}
		}
	}
	quoted.WriteByte('"')
	return quoted.String()
}
