// Port of mj-client/src/daemon/ssh.rs.

// SSH endpoint discovery, independent of CLI startup and local schema
// stores.

package client

import (
	"context"
	"fmt"
	"net"
	"net/netip"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// DiscoveryIntent says whether discovery may start the remote daemon.
type DiscoveryIntent int

const (
	DiscoveryIntentStart DiscoveryIntent = iota
	DiscoveryIntentExisting
)

// Discover discovers process-private remote credentials. No client-side
// daemon or store is consulted. Cancelling ctx cancels the SSH process
// group.
func Discover(ctx context.Context, placement core.DaemonConfig, remoteInstance string, intent DiscoveryIntent) (DaemonEndpoint, error) {
	panic("unported: mj-client/src/daemon/ssh.rs discover")
}

// SshLease is process-private transport ownership. Closing the lease stops
// and reaps SSH; it never stops the remote daemon.
type SshLease struct {
	LocalAddr netip.AddrPort
	Endpoint  DaemonEndpoint
	child     *core.SupervisedChild
}

// Metadata is the credentials for the forwarded connection, never persisted
// to disk. PID zero deliberately carries no local process authority.
func (l *SshLease) Metadata() DaemonMetadata {
	return DaemonMetadata{
		ProtocolVersion: l.Endpoint.ProtocolVersion,
		Pid:             0,
		Address:         l.LocalAddr,
		Token:           l.Endpoint.Token,
		BuildVersion:    l.Endpoint.BuildVersion,
		InstanceID:      l.Endpoint.InstanceID,
		Placement:       DaemonPlacement{Kind: DaemonPlacementKindHost},
	}
}

func (l *SshLease) Terminate(ctx context.Context) error {
	_, err := l.child.Terminate(ctx)
	return err
}

// Close stops and reaps SSH, as dropping the Rust lease does.
func (l *SshLease) Close() {
	l.child.Close()
}

// Open discovers and authenticates a supervised tunnel. Cancellation (ctx,
// or cancel firing; a nil cancel never fires) releases the process group
// without remote teardown.
func Open(
	ctx context.Context,
	placement core.DaemonConfig,
	remoteInstance string,
	intent DiscoveryIntent,
	cancel <-chan struct{},
) (*SshLease, error) {
	panic("unported: mj-client/src/daemon/ssh.rs open")
}

// forwardDestination is the `-L` destination for the discovered control
// address, as seen from the remote host. A daemon listening on a wildcard is
// reached through loopback of the same family; IPv6 needs brackets in
// OpenSSH forwarding syntax.
func forwardDestination(address netip.AddrPort) string {
	ip := address.Addr()
	if ip.IsUnspecified() {
		if ip.Is4() {
			ip = netip.AddrFrom4([4]byte{127, 0, 0, 1})
		} else {
			ip = netip.IPv6Loopback()
		}
	}
	if ip.Is4() {
		return fmt.Sprintf("%s:%d", ip, address.Port())
	}
	return fmt.Sprintf("[%s]:%d", ip, address.Port())
}

func selectPort() (uint16, error) {
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		return 0, err
	}
	defer listener.Close()
	return uint16(listener.Addr().(*net.TCPAddr).Port), nil
}

func openWithPort(
	ctx context.Context,
	placement core.DaemonConfig,
	remoteInstance string,
	intent DiscoveryIntent,
	port func() (uint16, error),
) (*SshLease, error) {
	panic("unported: mj-client/src/daemon/ssh.rs open_with_port")
}

func proveListener(ctx context.Context, address netip.AddrPort, token string) (bool, error) {
	var dialer net.Dialer
	stream, err := dialer.DialContext(ctx, "tcp", address.String())
	if err != nil {
		return false, err
	}
	defer stream.Close()
	return proveStream(ctx, stream, token)
}

// proveStream asks the peer on stream to prove it holds token, sending only a
// nonce. Every SSH-placement connection runs this before its first
// token-bearing request: a proof covers only the TCP connection that carried
// it.
func proveStream(ctx context.Context, stream net.Conn, token string) (bool, error) {
	panic("unported: mj-client/src/daemon/ssh.rs prove_stream")
}
