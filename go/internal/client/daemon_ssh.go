// Port of mj-client/src/daemon/ssh.rs.

// SSH endpoint discovery, independent of CLI startup and local schema
// stores.

package client

import (
	"context"
	"crypto/hmac"
	"crypto/rand"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/netip"
	"os"
	"os/exec"
	"runtime"
	"strings"
	"time"
	"unicode"

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
	if placement.Host == nil {
		return DaemonEndpoint{}, errors.New("[daemon] host is required for SSH placement")
	}
	host := *placement.Host
	if strings.HasPrefix(host, "-") {
		return DaemonEndpoint{}, errors.New("[daemon] host cannot be an SSH option")
	}
	args := []string{
		placement.RemoteCommandOrDefault(),
		"--instance",
		remoteInstance,
		"daemon",
		"endpoint",
		"--json",
	}
	if intent == DiscoveryIntentExisting {
		args = append(args, "--existing")
	}
	sshArgs := append([]string{"-o", "BatchMode=yes"}, placement.SSHArgs...)
	sshArgs = append(sshArgs, host, core.DaemonLoginCommand(args))
	output, err := core.Capture(ctx, exec.Command("ssh", sshArgs...))
	if err != nil {
		return DaemonEndpoint{}, fmt.Errorf("discover SSH daemon: %w", err)
	}
	stderr := strings.TrimSpace(strings.ToValidUTF8(string(output.Stderr), "\uFFFD"))
	if output.Status.ExitCode() == 127 {
		return DaemonEndpoint{}, fmt.Errorf("[daemon] remote_command %s was not found on %s; install mj remotely with scripts/install.sh (or install the same release). SSH: %s",
			sshDebugString(placement.RemoteCommandOrDefault()), host, stderr)
	}
	if !output.Status.Success() {
		return DaemonEndpoint{}, fmt.Errorf("SSH daemon discovery failed (%s): %s", daemonExitStatus(output.Status), stderr)
	}
	var endpoint DaemonEndpoint
	if err := json.Unmarshal(output.Stdout, &endpoint); err != nil {
		return DaemonEndpoint{}, fmt.Errorf("parse SSH daemon endpoint JSON: %w", err)
	}
	remote, err := BuildIdentityParse(endpoint.BuildVersion)
	if err != nil {
		return DaemonEndpoint{}, err
	}
	if this := ThisBuild(); !this.SameSource(remote) {
		return DaemonEndpoint{}, fmt.Errorf("SSH daemon build %s differs from client build %s; run scripts/dev-remote.ps1 or install the same release on both machines",
			remote.Describe(), this.Describe())
	}
	return endpoint, nil
}

// sshDebugString formats text as Rust's `{:?}` does for a str.
func sshDebugString(text string) string {
	var builder strings.Builder
	builder.WriteByte('"')
	for _, r := range text {
		switch {
		case r == '"':
			builder.WriteString(`\"`)
		case r == '\\':
			builder.WriteString(`\\`)
		case r == '\n':
			builder.WriteString(`\n`)
		case r == '\r':
			builder.WriteString(`\r`)
		case r == '\t':
			builder.WriteString(`\t`)
		case r == 0:
			builder.WriteString(`\0`)
		case !unicode.IsGraphic(r) || unicode.In(r, unicode.Mn, unicode.Me):
			fmt.Fprintf(&builder, `\u{%x}`, r)
		default:
			builder.WriteRune(r)
		}
	}
	builder.WriteByte('"')
	return builder.String()
}

// sshLines is Rust's str::lines: split at `\n`, drop one `\r` before it,
// and yield no empty line after a final newline.
func sshLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(text, "\n"), "\n")
	for i, line := range lines {
		lines[i] = strings.TrimSuffix(line, "\r")
	}
	return lines
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
	errCancelled := errors.New("SSH daemon connection cancelled")
	opening, stop := context.WithCancelCause(ctx)
	defer stop(nil)
	go func() {
		select {
		case <-cancel:
			stop(errCancelled)
		case <-opening.Done():
		}
	}()
	lease, err := openWithPort(opening, placement, remoteInstance, intent, selectPort)
	if err != nil {
		if ctx.Err() == nil && errors.Is(context.Cause(opening), errCancelled) {
			return nil, errCancelled
		}
		return nil, err
	}
	return lease, nil
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
	endpoint, err := Discover(ctx, placement, remoteInstance, intent)
	if err != nil {
		return nil, err
	}
	for {
		selected, err := port()
		if err != nil {
			return nil, err
		}
		localAddr := netip.AddrPortFrom(netip.AddrFrom4([4]byte{127, 0, 0, 1}), selected)
		if placement.Host == nil {
			return nil, errors.New("missing SSH host")
		}
		// The tunnel needs its own connection. Through a shared ControlMaster
		// connection the master would own the listener, and stopping this
		// child would not remove the forward. OpenSSH keeps the first value of
		// an option, so these override the user's `ssh_args` and ssh_config.
		args := []string{
			"-N",
			"-o", "ControlMaster=no",
			"-o", "ControlPath=none",
			"-o", "ExitOnForwardFailure=yes",
			"-o", "BatchMode=yes",
		}
		args = append(args, placement.SSHArgs...)
		args = append(args,
			"-L", fmt.Sprintf("127.0.0.1:%d:%s", localAddr.Port(), forwardDestination(endpoint.ControlAddress)),
			*placement.Host)
		child, err := core.SupervisedChildSpawn(exec.Command("ssh", args...))
		if err != nil {
			return nil, err
		}
		lease := &SshLease{LocalAddr: localAddr, Endpoint: endpoint, child: child}
		fresh, exited, err := sshTunnelReady(ctx, placement, remoteInstance, intent, lease)
		if err != nil {
			lease.Close()
			return nil, err
		}
		switch {
		case exited != nil:
			stderr := strings.ToValidUTF8(string(lease.child.Stderr()), "\uFFFD")
			collision := false
			var shown []string
			for _, line := range sshLines(stderr) {
				if (strings.Contains(line, "bind") && strings.Contains(line, "Address already in use")) ||
					strings.Contains(line, "cannot listen to port") {
					collision = true
				}
				if !strings.HasPrefix(line, "debug") && !strings.HasPrefix(line, "OpenSSH") {
					shown = append(shown, line)
				}
			}
			lease.Close()
			if collision {
				continue
			}
			return nil, fmt.Errorf("SSH daemon tunnel exited (%s): %s", daemonExitStatus(exited), strings.Join(shown, "\n"))
		case fresh != nil:
			// The daemon was replaced after discovery: the new endpoint is
			// the state change that restarts the tunnel.
			if err := lease.Terminate(ctx); err != nil {
				return nil, err
			}
			endpoint = *fresh
		default:
			// An unreachable lease stops and reaps its SSH, as dropping the
			// last Rust Arc does; a replaced lease stays alive while any
			// client still holds it.
			runtime.AddCleanup(lease, func(child *core.SupervisedChild) { child.Close() }, lease.child)
			return lease, nil
		}
	}
}

// sshTunnelReady waits until the tunnel's listener proves it reaches the
// discovered daemon, the daemon turns out to have been replaced (fresh), or
// ssh exits (exited), whichever comes first; the other wait is cancelled.
//
// Readiness sends no token: proveListener sends only a nonce and checks the
// daemon's HMAC over it. There is no deadline. A silent listener that is not
// ssh ends when ssh exits on its failed bind, and a slow link only makes the
// proof slower.
func sshTunnelReady(
	ctx context.Context,
	placement core.DaemonConfig,
	remoteInstance string,
	intent DiscoveryIntent,
	lease *SshLease,
) (fresh *DaemonEndpoint, exited *os.ProcessState, err error) {
	waiting, stop := context.WithCancel(ctx)
	defer stop()
	type readiness struct {
		fresh *DaemonEndpoint
		err   error
	}
	ready := make(chan readiness, 1)
	go func() {
		fresh, err := sshProbeUntilReady(waiting, placement, remoteInstance, intent, lease)
		ready <- readiness{fresh: fresh, err: err}
	}()
	type exit struct {
		state *os.ProcessState
		err   error
	}
	exits := make(chan exit, 1)
	go func() {
		state, err := lease.child.Wait(waiting)
		exits <- exit{state: state, err: err}
	}()
	select {
	case outcome := <-ready:
		return outcome.fresh, nil, outcome.err
	case outcome := <-exits:
		// The probe is abandoned; wait for it so nothing still dials or
		// discovers once this tunnel is gone.
		stop()
		<-ready
		if outcome.err != nil {
			return nil, nil, outcome.err
		}
		return nil, outcome.state, nil
	}
}

// sshProbeUntilReady proves the listener until it answers for the
// discovered daemon (nil), or until discovery names a replacement daemon.
func sshProbeUntilReady(
	ctx context.Context,
	placement core.DaemonConfig,
	remoteInstance string,
	intent DiscoveryIntent,
	lease *SshLease,
) (*DaemonEndpoint, error) {
	// Bounds only the pause between probes, never the work.
	const maxProbeDelay = 2 * time.Second
	delay := 100 * time.Millisecond
	for {
		proven, err := proveListener(ctx, lease.LocalAddr, lease.Endpoint.Token)
		switch {
		case err == nil && proven:
			return nil, nil
		case err != nil && daemonConnectionRefused(err):
			// Nothing listens yet: ssh is still connecting.
		default:
			if ctx.Err() != nil {
				return nil, ctx.Err()
			}
			// A wrong or garbled proof, or a dropped connection: either the
			// daemon was replaced after discovery, or the listener is
			// another local process. Discovery tells them apart without
			// sending the token anywhere.
			fresh, err := Discover(ctx, placement, remoteInstance, intent)
			if err != nil {
				return nil, err
			}
			if fresh.InstanceID != lease.Endpoint.InstanceID || fresh.ControlAddress != lease.Endpoint.ControlAddress {
				return &fresh, nil
			}
			delay = min(delay*2, maxProbeDelay)
		}
		if err := daemonSleep(ctx, delay); err != nil {
			return nil, err
		}
	}
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
//
// Cancelling ctx closes stream, as dropping the Rust future that owns it
// does.
func proveStream(ctx context.Context, stream net.Conn, token string) (bool, error) {
	random := make([]byte, 32)
	if _, err := rand.Read(random); err != nil {
		return false, err
	}
	nonce := core.LowerHex(random)
	release := daemonWatch(ctx, stream)
	err := WriteFrame(stream, RequestEnvelope{
		ProtocolVersion: ProtocolVersion,
		RequestID:       0,
		Token:           "",
		Action:          DaemonActionProveIdentity{Nonce: nonce},
	})
	if err != nil {
		return false, daemonAbandon(ctx, stream, release, err)
	}
	response, err := ReadResponse(stream)
	if err != nil {
		return false, daemonAbandon(ctx, stream, release, err)
	}
	if !release() {
		_ = stream.Close()
		return false, ctx.Err()
	}
	proof, ok := response.Result.Ok.(DaemonReplyIdentityProof)
	if response.Result.Err != nil || !ok {
		return false, nil
	}
	return hmac.Equal(IdentityProof(token, nonce).Sum(nil), proof.Mac), nil
}
