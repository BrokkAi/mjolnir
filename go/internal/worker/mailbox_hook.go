// Port of mj-worker/src/mailbox_hook.rs.

// Harness-hook client for draining the worker-owned session mailbox.

package worker

import (
	"os"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// MailboxHookRequest is mailbox_hook::request: let a harness retain leased
// input before acknowledging it. It reads one mailbox drain or
// acknowledgement request from stdin and writes the worker's answer to
// stdout, reusing the same framed IPC, request deadline, and long Unix socket
// path handling as hooks.
func MailboxHookRequest(socket string) error {
	panic("unported: mj-worker/src/mailbox_hook.rs request")
}

// MailboxHookRun is mailbox_hook::run: consume one harness hook request,
// write its response, then acknowledge the relay lease. Relay unavailability
// is deliberately fail-open: an unacked lease returns to the mailbox after
// its timeout or a worker restart.
func MailboxHookRun(socket string, hookEvent string) error {
	panic("unported: mj-worker/src/mailbox_hook.rs run")
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
	panic("unported: mj-worker/src/mailbox_hook.rs drain")
}

func mailboxHookAcknowledge(socket string, leaseID string) error {
	panic("unported: mj-worker/src/mailbox_hook.rs acknowledge")
}

// mailboxHookSendRequest sends one relay request over the worker's control
// socket and returns its payload. Workers run on Unix; elsewhere this says
// plainly that the hook cannot run.
func mailboxHookSendRequest(socket string, request core.RelayRequest) (core.RelayResponsePayload, error) {
	panic("unported: mj-worker/src/mailbox_hook.rs send_request")
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
