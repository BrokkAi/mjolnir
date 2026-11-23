// Port of mj-store/src/database/session_restart.rs.

// Durable intent for one daemon-owned session restart.

package store

import (
	"fmt"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

type SessionRestartPhase int

const (
	SessionRestartPhaseStopping SessionRestartPhase = iota
	SessionRestartPhaseRestoringInPlace
	SessionRestartPhaseRecreatingContainer
	SessionRestartPhaseCancelRequested
	SessionRestartPhaseFallback
)

// String is the Rust Debug name, which error messages show.
func (p SessionRestartPhase) String() string {
	switch p {
	case SessionRestartPhaseStopping:
		return "Stopping"
	case SessionRestartPhaseRestoringInPlace:
		return "RestoringInPlace"
	case SessionRestartPhaseRecreatingContainer:
		return "RecreatingContainer"
	case SessionRestartPhaseCancelRequested:
		return "CancelRequested"
	case SessionRestartPhaseFallback:
		return "Fallback"
	}
	return fmt.Sprintf("SessionRestartPhase(%d)", int(p))
}

// asStr is the phase's stored text.
func (p SessionRestartPhase) asStr() string {
	switch p {
	case SessionRestartPhaseStopping:
		return "stopping"
	case SessionRestartPhaseRestoringInPlace:
		return "restoring_in_place"
	case SessionRestartPhaseRecreatingContainer:
		return "recreating_container"
	case SessionRestartPhaseCancelRequested:
		return "cancel_requested"
	case SessionRestartPhaseFallback:
		return "fallback"
	}
	panic(fmt.Sprintf("unknown session restart phase %d", int(p)))
}

func sessionRestartPhaseParse(value string) (SessionRestartPhase, error) {
	switch value {
	case "stopping":
		return SessionRestartPhaseStopping, nil
	case "restoring_in_place":
		return SessionRestartPhaseRestoringInPlace, nil
	case "recreating_container":
		return SessionRestartPhaseRecreatingContainer, nil
	case "cancel_requested":
		return SessionRestartPhaseCancelRequested, nil
	case "fallback":
		return SessionRestartPhaseFallback, nil
	}
	return 0, fmt.Errorf("unknown session restart phase %q", value)
}

func (p SessionRestartPhase) canFollow(next SessionRestartPhase) bool {
	if p == next || next == SessionRestartPhaseCancelRequested {
		return true
	}
	switch p {
	case SessionRestartPhaseStopping:
		return next == SessionRestartPhaseRestoringInPlace || next == SessionRestartPhaseRecreatingContainer || next == SessionRestartPhaseFallback
	case SessionRestartPhaseRestoringInPlace:
		return next == SessionRestartPhaseFallback
	case SessionRestartPhaseRecreatingContainer:
		return next == SessionRestartPhaseRestoringInPlace
	}
	return false
}

type SessionRestartIntent struct {
	SessionID                       string
	OperationID                     string
	Phase                           SessionRestartPhase
	CheckpointSHA256                *string
	Handoff                         *core.CheckpointMetadata
	CheckpointStarted               bool
	ContainerRecreationAcknowledged bool
	// Automatic is the historical automatic recreation intent. New daemons
	// never create one; recovery retains this marker to reconcile operations
	// accepted by older builds.
	Automatic        bool
	ContainerUpgrade *targets.RetainedContainer
}

// BeginSessionRestart creates one intent, or returns the unfinished intent
// the session already owns.
func BeginSessionRestart(sessionID, operationID string, cancelled *atomic.Bool, containerRecreationAcknowledged bool) (SessionRestartIntent, error) {
	return beginSessionRestartWith(sessionID, operationID, cancelled, nil, nil)
}

func BeginContainerUpgrade(sessionID, operationID string, cancelled *atomic.Bool, plan targets.RetainedContainer, settings *core.ContainerSettingsChange) (SessionRestartIntent, error) {
	return beginSessionRestartWith(sessionID, operationID, cancelled, &plan, settings)
}

func beginSessionRestartWith(sessionID, operationID string, cancelled *atomic.Bool, containerUpgrade *targets.RetainedContainer, settings *core.ContainerSettingsChange) (SessionRestartIntent, error) {
	panic("unported: mj-store/src/database/session_restart.rs begin_session_restart_with")
}

// LoadRestartAndSession reads a session's restart intent and its record from
// one snapshot; ok is false when the session has no intent.
func LoadRestartAndSession(sessionID string) (intent SessionRestartIntent, session core.SessionRecord, ok bool, err error) {
	panic("unported: mj-store/src/database/session_restart.rs load_restart_and_session")
}

func LoadSessionRestarts() ([]SessionRestartIntent, error) {
	panic("unported: mj-store/src/database/session_restart.rs load_session_restarts")
}

func RecordSessionRestartCheckpoint(sessionID, operationID, checkpointSHA256 string) error {
	panic("unported: mj-store/src/database/session_restart.rs record_session_restart_checkpoint")
}

func MarkSessionRestartCheckpointStarted(sessionID, operationID string) error {
	panic("unported: mj-store/src/database/session_restart.rs mark_session_restart_checkpoint_started")
}

func AdvanceSessionRestart(sessionID, operationID string, phase SessionRestartPhase) error {
	panic("unported: mj-store/src/database/session_restart.rs advance_session_restart")
}

// RequestSessionRestartCancel retains the settings lock while cancellation
// unwinds, but never recovers accepted cancellation as another Restart after
// a daemon crash.
func RequestSessionRestartCancel(sessionID, operationID string) error {
	panic("unported: mj-store/src/database/session_restart.rs request_session_restart_cancel")
}

// RecoverCancelledSessionRestart keeps a cancelled Restart from becoming an
// ordinary pending suspension on startup: it retains its environment and
// settles the recovery row atomically.
func RecoverCancelledSessionRestart(sessionID, operationID string) error {
	panic("unported: mj-store/src/database/session_restart.rs recover_cancelled_session_restart")
}

func FinishSessionRestart(sessionID, operationID string) error {
	panic("unported: mj-store/src/database/session_restart.rs finish_session_restart")
}

func CancelSessionRestart(sessionID string) error {
	panic("unported: mj-store/src/database/session_restart.rs cancel_session_restart")
}

func loadSessionRestartWith(conn Queryer, sessionID string) (*SessionRestartIntent, error) {
	panic("unported: mj-store/src/database/session_restart.rs load_session_restart_with")
}

// PrepareContainerUpgrade persists the immutable prepared image before
// stopping the source.
func PrepareContainerUpgrade(sessionID, operationID string, plan targets.RetainedContainer) error {
	panic("unported: mj-store/src/database/session_restart.rs prepare_container_upgrade")
}

// ensureContainerParentAvailable refuses while member's environment is being
// replaced. Child startup and replacement share the database writer's
// decision. A parked child's stale target cannot be woken after replacement
// commits.
func ensureContainerParentAvailable(conn Queryer, member string) error {
	panic("unported: mj-store/src/database/session_restart.rs ensure_container_parent_available")
}

// ensureNoPeerContainerUpgrade refuses while another member's replacement
// owns the environment. An environment replacement may boot its own worker
// before releasing its intent. Other members of the environment must wait
// for that release.
func ensureNoPeerContainerUpgrade(conn Queryer, sessionID string) error {
	panic("unported: mj-store/src/database/session_restart.rs ensure_no_peer_container_upgrade")
}

type environmentPeer struct {
	id               string
	state            core.SessionState
	hasTarget        bool
	restartingWorker bool
	kind             *string
	upgradePhase     *string
}

// BoundEnvironmentMemberIDs is the operational sessions currently bound to
// the same environment as the member.
func BoundEnvironmentMemberIDs(member string) ([]string, error) {
	panic("unported: mj-store/src/database/session_restart.rs bound_environment_member_ids")
}

func environmentPeers(conn Queryer, member string) ([]environmentPeer, error) {
	panic("unported: mj-store/src/database/session_restart.rs environment_peers")
}

type ContainerUpgradeCommit interface {
	isContainerUpgradeCommit()
	IsCommitted() bool
}

type ContainerUpgradeCommitCommitted struct{}

// ContainerUpgradeCommitWaiting is the descriptions captured by the same
// transaction that refused replacement.
type ContainerUpgradeCommitWaiting struct{ Value []string }

func (ContainerUpgradeCommitCommitted) isContainerUpgradeCommit() {}
func (ContainerUpgradeCommitWaiting) isContainerUpgradeCommit()   {}

func (ContainerUpgradeCommitCommitted) IsCommitted() bool { return true }
func (ContainerUpgradeCommitWaiting) IsCommitted() bool   { return false }

func TryCommitContainerUpgrade(sessionID, operationID string) (ContainerUpgradeCommit, error) {
	panic("unported: mj-store/src/database/session_restart.rs try_commit_container_upgrade")
}

// ContainerUpgradeOwnsTarget is whether a retained-container intent owns the
// session's environment. The intent is the sole authority over its
// environment, including after an engine failure or daemon replacement.
func ContainerUpgradeOwnsTarget(sessionID string) (bool, error) {
	panic("unported: mj-store/src/database/session_restart.rs container_upgrade_owns_target")
}

func containerUpgradeOwnsTargetWith(conn Queryer, sessionID string) (bool, error) {
	peers, err := environmentPeers(conn, sessionID)
	if err != nil {
		return false, err
	}
	for _, peer := range peers {
		if peer.upgradePhase != nil {
			return true, nil
		}
	}
	return false, nil
}

// RecordSessionRestartHandoff records the handoff the Restart owns before the
// relay can be sealed. Its digest and archive reference change in one
// database-writer decision.
func RecordSessionRestartHandoff(sessionID, operationID string, handoff *core.CheckpointMetadata) error {
	panic("unported: mj-store/src/database/session_restart.rs record_session_restart_handoff")
}

func LoadSessionRestart(sessionID string) (*SessionRestartIntent, error) {
	panic("unported: mj-store/src/database/session_restart.rs load_session_restart")
}
