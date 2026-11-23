// Port of the tests in mj-client/src/daemon/ssh.rs.

package client

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"io"
	"net"
	"os"
	"os/exec"
	"runtime"
	"strings"
	"testing"
	"weak"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Hard-won: 5159ed294: a forwarding diagnostic printed before bind() released the token to another listener
func TestCompetingListenerNeverReceivesControlToken(t *testing.T) {
	listener, address := daemonTestListener(t)
	const token = "private-control-token-that-must-never-cross-the-wire"
	wait := daemonTestServer(t, func() error {
		stream, err := listener.Accept()
		if err != nil {
			return err
		}
		defer stream.Close()
		var length [4]byte
		if _, err := io.ReadFull(stream, length[:]); err != nil {
			return err
		}
		body := make([]byte, binary.BigEndian.Uint32(length[:]))
		if _, err := io.ReadFull(stream, body); err != nil {
			return err
		}
		if strings.Contains(string(body), token) {
			return errors.New("the control token crossed the wire")
		}
		var request RequestEnvelope
		if err := json.Unmarshal(body, &request); err != nil {
			return err
		}
		if request.Token != "" {
			return errors.New("the proof request carries a token")
		}
		if _, ok := request.Action.(DaemonActionProveIdentity); !ok {
			return errors.New("the first request is not a proof request")
		}
		if _, err := stream.Write([]byte{0, 0, 0, 1, '!'}); err != nil {
			return err
		}
		remainder, err := io.ReadAll(stream)
		if err != nil {
			return err
		}
		if len(remainder) != 0 {
			return errors.New("the client kept talking to an unproven listener")
		}
		return nil
	})
	// Every SSH-placement connection goes through connect_proven, so a proof
	// made earlier on another connection never vouches for this one.
	metadata := DaemonMetadata{
		ProtocolVersion: ProtocolVersion,
		Pid:             0,
		Address:         address,
		Token:           token,
		Placement:       DaemonPlacement{Kind: DaemonPlacementKindHost},
	}
	client, err := daemonClientConnectProven(context.Background(), metadata)
	if err == nil {
		client.Close()
		t.Fatal("an unproven listener was accepted")
	}
	wait()
}

// Run against a disposable sshd with a real, matching mj, never a live
// instance. The driver supplies isolated remote config/data directories.
// (Rust marks it #[ignore]; here it runs only when the fixture is named.)
func TestRealSshEndpointDiscovery(t *testing.T) {
	path := os.Getenv("MJ_SSH_DISCOVERY_PLACEMENT")
	if path == "" {
		t.Skip("requires MJ_SSH_DISCOVERY_PLACEMENT pointing at a disposable sshd fixture")
	}
	ctx := context.Background()
	must := func(err error) {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
	}
	placement, err := core.DaemonConfigLoadPlacement(path)
	must(err)
	instance := "ssh-discovery-e2e"
	connectorPlacement := placement
	connectorPlacement.Instance = &instance
	connector := daemonConnectorForPlacement(connectorPlacement)
	connected, err := connector.ConnectOrStart(ctx, func(context.Context) (*DaemonClient, error) {
		t.Fatal("SSH must not run local startup")
		return nil, nil
	})
	must(err)
	firstIdentity, err := connected.Identity(ctx)
	must(err)
	oldLease := weak.Make(connected.sshLease)
	oldAddress := connected.metadata.Address
	secondClient, err := connector.ConnectExisting(ctx)
	must(err)
	secondIdentity, err := secondClient.Identity(ctx)
	must(err)
	if secondIdentity.InstanceID != firstIdentity.InstanceID {
		t.Fatalf("second client reached %s, first %s", secondIdentity.InstanceID, firstIdentity.InstanceID)
	}
	argv := []string{placement.RemoteCommandOrDefault(), "--instance", instance, "daemon", "restart"}
	restartArgs := append([]string{"-o", "BatchMode=yes"}, placement.SSHArgs...)
	restartArgs = append(restartArgs, *placement.Host, core.DaemonLoginCommand(argv))
	restarted, err := core.Capture(ctx, exec.Command("ssh", restartArgs...))
	must(err)
	if !restarted.Status.Success() {
		t.Fatal(string(restarted.Stderr))
	}
	type connection struct {
		client *DaemonClient
		err    error
	}
	connections := make(chan connection, 2)
	for range 2 {
		go func() {
			client, err := connector.ConnectExisting(ctx)
			connections <- connection{client: client, err: err}
		}()
	}
	left, right := <-connections, <-connections
	must(left.err)
	must(right.err)
	newIdentity, err := left.client.Identity(ctx)
	must(err)
	if newIdentity.InstanceID == firstIdentity.InstanceID {
		t.Fatal("the restarted daemon kept its identity")
	}
	rightIdentity, err := right.client.Identity(ctx)
	must(err)
	if rightIdentity.InstanceID != newIdentity.InstanceID {
		t.Fatalf("concurrent refreshes reached %s and %s", rightIdentity.InstanceID, newIdentity.InstanceID)
	}
	// Hard-won: #1514: refreshing one client killed another client's tunnel.
	if oldLease.Value() == nil {
		t.Fatal("the old lease is gone while clients still hold it")
	}
	oldConnection, err := net.Dial("tcp", oldAddress.String())
	must(err)
	_ = oldConnection.Close()
	connected.Close()
	secondClient.Close()
	connected, secondClient = nil, nil
	runtime.GC()
	if oldLease.Value() != nil {
		t.Fatal("the old lease outlived every client holding it")
	}
	connector = nil
	endpoint, err := Discover(ctx, placement, &instance, DiscoveryIntentStart)
	must(err)
	if !endpoint.ControlAddress.Addr().IsLoopback() {
		t.Fatalf("control address %s", endpoint.ControlAddress)
	}
	existing, err := Discover(ctx, placement, &instance, DiscoveryIntentExisting)
	must(err)
	if existing.Token != endpoint.Token || existing.InstanceID != endpoint.InstanceID {
		t.Fatal("existing discovery found another daemon")
	}
	lease, err := Open(ctx, placement, &instance, DiscoveryIntentExisting, nil)
	must(err)
	if lease.Endpoint.InstanceID != endpoint.InstanceID {
		t.Fatal("the tunnel reaches another daemon")
	}
	must(lease.Terminate(ctx))
	occupied, err := net.Listen("tcp", "127.0.0.1:0")
	must(err)
	defer occupied.Close()
	occupiedPort := uint16(occupied.Addr().(*net.TCPAddr).Port)
	first := true
	second, err := openWithPort(ctx, placement, &instance, DiscoveryIntentExisting, func() (uint16, error) {
		if first {
			first = false
			return occupiedPort, nil
		}
		return selectPort()
	})
	must(err)
	if first {
		t.Fatal("the occupied port was never tried")
	}
	if second.LocalAddr.Port() == occupiedPort {
		t.Fatal("the tunnel took the occupied port")
	}
	if second.Endpoint.InstanceID != endpoint.InstanceID {
		t.Fatal("the tunnel reaches another daemon")
	}
	must(second.Terminate(ctx))
	missing := placement
	missingCommand := "/missing/mj-discovery-test"
	missing.RemoteCommand = &missingCommand
	_, err = Discover(ctx, missing, &instance, DiscoveryIntentExisting)
	if err == nil {
		t.Fatal("a missing remote command was discovered")
	}
	message := err.Error()
	if !strings.Contains(message, "[daemon] remote_command") || !strings.Contains(message, "scripts/install.sh") {
		t.Fatal(message)
	}
}
