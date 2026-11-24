// Port of mj-daemon/src/pollers/remote.rs (only the entry point so far).

package daemon

import "github.com/BrokkAi/mjolnir/go/internal/client"

// SpawnRemoteDashboardWorkerPoller starts the remote session manager and the
// tasks that follow the daemon's runtime feed for workspaceID, starting from
// bootstrap, and hands back every feed they publish.
func SpawnRemoteDashboardWorkerPoller(workspaceID string, bootstrap client.RuntimeReplica) (RemoteDashboardWorkerPoller, error) {
	panic("unported: mj-daemon/src/pollers/remote.rs spawn_remote_dashboard_worker_poller")
}
