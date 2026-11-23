// Port of the tests in mj-worker/src/mailbox_hook.rs.

//go:build unix

package worker

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"net"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestMailboxHookAntigravityStopObservesMainConversationAndStaysPassive(t *testing.T) {
	socket := filepath.Join(t.TempDir(), "control.sock")
	listener, err := net.Listen("unix", socket)
	if err != nil {
		t.Fatal(err)
	}
	unix := listener.(*net.UnixListener)
	served := make(chan error, 1)
	go func() {
		served <- func() error {
			if err := unix.SetDeadline(time.Now().Add(5 * time.Second)); err != nil {
				return err
			}
			stream, err := unix.Accept()
			if err != nil {
				return errors.New("no Stop request arrived")
			}
			defer stream.Close()
			if err := stream.SetReadDeadline(time.Now().Add(5 * time.Second)); err != nil {
				return err
			}
			line, err := bufio.NewReader(stream).ReadBytes('\n')
			if err != nil {
				return err
			}
			var envelope core.RelayRequestEnvelope
			if err := json.Unmarshal(line, &envelope); err != nil {
				return err
			}
			stop, ok := envelope.Request.(core.RelayRequestAntigravityStop)
			if !ok || stop.ConversationID == nil || *stop.ConversationID != "main" ||
				stop.FullyIdle == nil || *stop.FullyIdle ||
				stop.TerminationReason == nil || *stop.TerminationReason != "USER_CANCELED" {
				return errors.New("unexpected Stop request: " + strings.TrimSpace(string(line)))
			}
			response, err := json.Marshal(core.RelayResponseEnvelope{
				RequestID:       envelope.RequestID,
				ProtocolVersion: envelope.ProtocolVersion,
				Body:            core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadAntigravityStopObserved{}},
			})
			if err != nil {
				return err
			}
			_, err = stream.Write(append(response, '\n'))
			return err
		}()
	}()

	var output bytes.Buffer
	if err := mailboxHookRun(socket, "Stop", HookOutputFormatAntigravity,
		strings.NewReader(`{"conversationId":"main","fullyIdle":false,"terminationReason":"USER_CANCELED"}`), &output); err != nil {
		t.Fatal(err)
	}
	if output.String() != "{}\n" {
		t.Fatalf("output = %q", output.String())
	}
	if err := <-served; err != nil {
		t.Fatal(err)
	}

	// A sub-agent's Stop never reaches the worker.
	output.Reset()
	if err := mailboxHookRun(socket, "Stop", HookOutputFormatAntigravity,
		strings.NewReader(`{"conversationId":"main","fullyIdle":true,"agent_id":"subagent"}`), &output); err != nil {
		t.Fatal(err)
	}
	if output.String() != "{}\n" {
		t.Fatalf("output = %q", output.String())
	}
	if err := unix.SetDeadline(time.Now()); err != nil {
		t.Fatal(err)
	}
	if stream, err := unix.Accept(); err == nil {
		stream.Close()
		t.Fatal("a sub-agent Stop reached the worker")
	} else if !errors.Is(err, os.ErrDeadlineExceeded) {
		t.Fatal(err)
	}

	// With no worker listening the hook stays passive.
	listener.Close()
	output.Reset()
	if err := mailboxHookRun(socket, "Stop", HookOutputFormatAntigravity,
		strings.NewReader(`{"conversationId":"main","fullyIdle":false}`), &output); err != nil {
		t.Fatal(err)
	}
	if output.String() != "{}\n" {
		t.Fatalf("output = %q", output.String())
	}
}
