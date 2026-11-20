// Port of mj-client/src/daemon/connector.rs.

// Process-owned daemon transport. Remote credentials never enter local
// metadata.

package client

import (
	"context"
	"net/netip"
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
	panic("unported: mj-client/src/daemon/connector.rs DaemonConnector::shared")
}

func (c *DaemonConnector) IsSSH() bool {
	return c.placement.Runtime == core.DaemonRuntimeSsh
}

// ValidateViewerURL validates a browser/HTTP destination against the
// transport's machine origin.
func (c *DaemonConnector) ValidateViewerURL(ctx context.Context, viewerURL string) error {
	panic("unported: mj-client/src/daemon/connector.rs DaemonConnector::validate_viewer_url")
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
	panic("unported: mj-client/src/daemon/connector.rs DaemonConnector::remote")
}
