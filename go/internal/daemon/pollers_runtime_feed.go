// Port of mj-daemon/src/pollers/runtime_feed.rs (the dashboard's side so
// far).

package daemon

import (
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/sessionruntime"
)

// AlertBatch is one delivery of the alert stream: the daemon's alert
// sequence it reaches and the events it adds (Rust's
// `(u64, Vec<AlertEvent>)`).
type AlertBatch struct {
	Sequence uint64
	Events   []client.AlertEvent
}

// RemoteDashboardWorkerPoller is every feed a dashboard follows from the
// daemon's runtime publication.
type RemoteDashboardWorkerPoller struct {
	Alerts   *core.Unbounded[AlertBatch]
	Updates  *sessionruntime.SessionManagerUpdates
	Control  sessionruntime.SessionManagerControl
	Shutdown *sessionruntime.SessionManagerShutdown
	State    *core.Watch[RuntimeStateUpdate]
	// SubagentParents is the sending side: the dashboard says which
	// sessions' sub-agents it shows.
	SubagentParents *core.Watch[map[string]struct{}]
	// Reviews the daemon is running for this workspace's sessions.
	Reviews *core.Watch[[]client.RuntimeReviewView]
	// Notices are background events the daemon wants reported once, oldest
	// first.
	Notices *core.Watch[[]client.RuntimeNotice]
	// UIRequests are dashboard requests made on the person's behalf
	// (`mj ui`), oldest first.
	UIRequests *core.Watch[[]client.UiRequest]
	// Quotas is what the daemon knows about quota. The daemon is the only
	// prober.
	Quotas              *core.Watch[client.QuotaSnapshot]
	ProfileCapabilities *core.Watch[core.ProfileCapabilitiesSnapshot]
	Config              *core.Watch[RuntimeLaunchConfig]
	Health              *core.Watch[RuntimeFeedHealth]
}

// RuntimeLaunchConfig is the configuration and launch verdicts from the
// same capture.
type RuntimeLaunchConfig struct {
	Config       core.Config
	Availability core.SharedSnapshot[client.RuntimeAvailability]
}

// RuntimeLaunchConfigFrom is Rust's `From<&RuntimeMetadata>`.
func RuntimeLaunchConfigFrom(metadata *client.RuntimeMetadata) RuntimeLaunchConfig {
	panic("unported: mj-daemon/src/pollers/runtime_feed.rs RuntimeLaunchConfig::from")
}

// RuntimeStateUpdate carries records and lifecycle ownership, which must
// reach the surface in the same frame.
type RuntimeStateUpdate struct {
	Verdicts             core.SnapshotMap[string, client.SessionVerdict]
	SubagentParents      map[string]struct{}
	OperatorPresence     core.OperatorPresence
	SessionCPU           core.SnapshotMap[string, client.SessionCpuView]
	LastSubagentPolicies map[string]core.SubagentPolicy
	NativeAgents         core.SnapshotMap[string, core.NativeAgentView]
	WorkspaceNames       map[string]string
	Revision             uint64
	Records              core.SnapshotMap[string, core.SessionRecord]
	EnvironmentOwners    core.SnapshotMap[string, string]
	// Sessions are the live worker views, including the controller-owned
	// session config policy.
	Sessions   core.SnapshotMap[string, client.RuntimeSessionView]
	Lifecycles []client.RuntimeLifecycleView
	Moves      core.SnapshotMap[string, core.MoveOperation]
	// Subagents are the parent/child relations for the sessions in Records,
	// so a surface can keep a daemon-created child out of the real workspace
	// without a full state reload.
	Subagents core.SnapshotMap[string, core.SubagentMetadata]
	// LaunchRecency is the session creation history for project-list
	// ordering and the legacy fallback.
	LaunchRecency          []client.LaunchRecency
	NotificationCards      *core.SnapshotMap[string, client.NotificationCard]
	UnreadFrontiers        core.SnapshotMap[string, uint64]
	NotificationDismissals core.SnapshotMap[string, core.NotificationDismissal]
	FormHolders            core.SnapshotMap[string, string]
	SessionAttention       core.SnapshotMap[string, core.SessionAttention]
	RecentLaunch           *core.RecentLaunch
	// ContainerUpgradeSessions and FailedLaunchRetries are sets of session
	// ids.
	ContainerUpgradeSessions map[string]struct{}
	FailedLaunchRetries      map[string]struct{}
	HostCapacities           []client.RuntimeHostCapacity
	// Storage is the daemon storage owner's verdict per target host.
	Storage []core.TargetStorageView
	// ContainerCapacities is the daemon's reservation-adjusted container
	// headroom per deployment host. Terminal surfaces consume this instead
	// of recalculating it.
	ContainerCapacities map[string]client.RuntimeContainerCapacity
}
