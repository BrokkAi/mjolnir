// Port of the tests in mj-cli/src/dashboard/actions.rs.

package cli

import (
	"context"
	"net"
	"net/netip"
	"os"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func actionsOpenChat(sessionID string) *chat.ActiveChat {
	fixture := client.NewReplacementSessionTestFixture(sessionID, 1)
	return chat.ActiveChatOpen(
		fixture.Stopped,
		"bundle-1",
		nil,
		fixture.Control,
		chat.SessionHeaderIdentity{},
		"",
		chat.NewNotices(),
	)
}

// Stopping a session retires its relay actor, which closes the open chat's
// feed. The chat is told before that happens so it reads the closure as the
// expected end of a deliberate stop.
// Hard-won: 66c73159: an intentional stop triggered a misleading feed-loss warning and reconnect loop.
func TestStoppingTheOpenChatsSessionMarksThatChatRetiring(t *testing.T) {
	open := actionsOpenChat("session-open")
	if open.SessionRetiring() {
		t.Fatal("a fresh chat is not retiring")
	}

	markActiveChatRetiring(open, "session-open")

	if !open.SessionRetiring() {
		t.Fatal("the stopped session's chat must be marked retiring")
	}
}

func TestStoppingAnotherSessionLeavesTheOpenChatAlone(t *testing.T) {
	open := actionsOpenChat("session-open")

	markActiveChatRetiring(open, "session-other")

	if open.SessionRetiring() {
		t.Fatal("stopping another session must leave the open chat alone")
	}
}

// Hard-won: f2fc48f3: Restart removed the workspace and temporary volumes before restarting.
func TestRestartDispatchSendsOneDaemonRestartAction(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	address, err := netip.ParseAddrPort(listener.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	metadata := client.DaemonMetadata{
		ProtocolVersion: client.ProtocolVersion,
		Pid:             uint32(os.Getpid()),
		Address:         address,
		Token:           "restart-test",
		StartedAt:       "test",
		BuildVersion:    core.PackageVersion,
		InstanceID:      "",
		Placement:       client.DaemonPlacement{Kind: client.DaemonPlacementKindHost},
	}
	served := make(chan error, 1)
	go func() {
		served <- func() error {
			stream, err := listener.Accept()
			if err != nil {
				return err
			}
			defer stream.Close()
			request, err := client.ReadFrame[client.RequestEnvelope](stream)
			if err != nil {
				return err
			}
			restart, ok := request.Action.(client.DaemonActionRestartSession)
			if !ok || restart.SessionID != "session-1" || !restart.AcknowledgeContainerRecreation {
				t.Errorf("the daemon was asked %#v", request.Action)
			}
			return client.WriteFrame(stream, client.ResponseEnvelope{
				ProtocolVersion: request.ProtocolVersion,
				RequestID:       request.RequestID,
				Result:          client.DaemonOk[client.DaemonReply](client.DaemonReplyDone{}),
			})
		}()
	}()
	ctx := context.Background()
	daemonClient, err := client.DaemonClientConnect(ctx, metadata)
	if err != nil {
		t.Fatal(err)
	}
	defer daemonClient.Close()

	if err := dispatchRestart(ctx, daemonClient, "session-1", true); err != nil {
		t.Fatal(err)
	}
	if err := <-served; err != nil {
		t.Fatal(err)
	}
}
