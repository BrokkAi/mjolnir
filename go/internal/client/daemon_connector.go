// Port of mj-client/src/daemon/connector.rs.

// Process-owned daemon transport. Remote credentials never enter local
// metadata.

package client

import (
	"context"
	"fmt"
	"net/netip"
	"net/url"
	"strings"
	"sync"
	"weak"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// DaemonConnector owns this process's way to its daemon: the placement read
// from configuration and, for an SSH placement, the tunnel lease.
type DaemonConnector struct {
	placement core.DaemonConfig
	// leaseMu is held through retirement and authenticated replacement of
	// lease (Rust's tokio Mutex held across awaits).
	leaseMu sync.Mutex
	lease   *SshLease
}

// sharedConnector is the process's connector while anyone holds it (Rust's
// static Mutex<Weak<DaemonConnector>>).
var sharedConnector struct {
	mu        sync.Mutex
	connector weak.Pointer[DaemonConnector]
}

// DaemonConnectorShared is the process's connector, loading the daemon
// placement once while any caller still holds it.
func DaemonConnectorShared(ctx context.Context) (*DaemonConnector, error) {
	sharedConnector.mu.Lock()
	defer sharedConnector.mu.Unlock()
	if connector := sharedConnector.connector.Value(); connector != nil {
		return connector, nil
	}
	placement, err := core.DaemonConfigLoadPlacement(core.ConfigPath())
	if err != nil {
		return nil, err
	}
	connector := &DaemonConnector{placement: placement}
	sharedConnector.connector = weak.Make(connector)
	return connector, nil
}

// daemonConnectorForPlacement is a connector of its own for placement
// (Rust's test-only for_placement).
func daemonConnectorForPlacement(placement core.DaemonConfig) *DaemonConnector {
	return &DaemonConnector{placement: placement}
}

func (c *DaemonConnector) IsSSH() bool {
	return c.placement.Runtime == core.DaemonRuntimeSsh
}

// ValidateViewerURL validates a browser/HTTP destination against the
// transport's machine origin.
func (c *DaemonConnector) ValidateViewerURL(ctx context.Context, viewerURL string) error {
	if !c.IsSSH() {
		return nil
	}
	parsed, err := url.Parse(viewerURL)
	if err != nil {
		return err
	}
	// url::Host: an IPv4 or IPv6 address, a domain, or no host at all.
	host := parsed.Hostname()
	loopback, wildcard := false, false
	if address, err := netip.ParseAddr(host); err == nil {
		address = address.WithZone("")
		if address.Is4() {
			loopback = address.IsLoopback() || address.IsUnspecified()
		} else {
			// Rust's Ipv6Addr checks: only ::1 and ::, never a mapped IPv4.
			loopback = address == netip.IPv6Loopback() || address == netip.IPv6Unspecified()
		}
		wildcard = address.IsUnspecified() && (address.Is4() || address == netip.IPv6Unspecified())
	} else if host != "" {
		loopback = strings.EqualFold(host, "localhost") || strings.HasSuffix(strings.ToLower(host), ".localhost")
	}
	if !loopback {
		return nil
	}
	detail := ""
	if wildcard {
		detail = " the advertised address is a wildcard; [phone] bind must name a reachable address or use Tailscale;"
	}
	c.leaseMu.Lock()
	path := "the remote config.toml"
	if c.lease != nil && c.lease.Endpoint.ConfigPath != nil {
		path = *c.lease.Endpoint.ConfigPath
	}
	c.leaseMu.Unlock()
	sshHost := "the SSH host"
	if c.placement.Host != nil {
		sshHost = *c.placement.Host
	}
	return fmt.Errorf("the viewer is unreachable from this computer;%s on %s enable [phone] tailscale_detect with working Tailscale, or set [phone] bind to a reachable address with tls_cert and tls_key; remote config: %s",
		detail, sshHost, path)
}

// ConnectOrStart connects through SSH for an SSH placement, and otherwise
// runs local: the CLI supplies local lifecycle policy without a client
// dependency on it.
func (c *DaemonConnector) ConnectOrStart(
	ctx context.Context,
	local func(context.Context) (*DaemonClient, error),
) (*DaemonClient, error) {
	if c.IsSSH() {
		return c.remote(ctx, DiscoveryIntentStart, nil)
	}
	return local(ctx)
}

func (c *DaemonConnector) ConnectExisting(ctx context.Context) (*DaemonClient, error) {
	if c.IsSSH() {
		return c.remote(ctx, DiscoveryIntentExisting, nil)
	}
	metadata, err := ReadMetadata()
	if err != nil {
		return nil, err
	}
	return DaemonClientConnect(ctx, metadata)
}

func (c *DaemonConnector) refresh(ctx context.Context, previous DaemonMetadata) (*DaemonClient, error) {
	if c.IsSSH() {
		stale := previous.Address
		return c.remote(ctx, DiscoveryIntentStart, &stale)
	}
	return c.ConnectExisting(ctx)
}

// remote holds the decision point through retirement and authenticated
// replacement. Callers holding an older endpoint (stale) reuse a replacement
// already published.
func (c *DaemonConnector) remote(ctx context.Context, intent DiscoveryIntent, stale *netip.AddrPort) (*DaemonClient, error) {
	c.leaseMu.Lock()
	defer c.leaseMu.Unlock()
	if current := c.lease; current != nil {
		metadata := current.Metadata()
		if stale == nil || *stale != metadata.Address {
			if client, err := daemonClientConnectProven(ctx, metadata); err == nil {
				client.connector = c
				client.sshLease = current
				return client, nil
			}
		}
	}
	var instance *string
	if name, ok := c.placement.RemoteInstance(); ok {
		instance = &name
	}
	// Replacing the published lease never stops the old tunnel: clients
	// still holding it keep it until they are gone (#1514).
	replacement, err := Open(ctx, c.placement, instance, intent, nil)
	if err != nil {
		return nil, err
	}
	metadata := replacement.Metadata()
	c.lease = replacement
	client, err := daemonClientConnectProven(ctx, metadata)
	if err != nil {
		return nil, err
	}
	client.connector = c
	client.sshLease = replacement
	return client, nil
}
