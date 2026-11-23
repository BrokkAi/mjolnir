// Port of mj-core/src/state/session_move.rs.
//
// Durable intent for a verified stop followed by destination restoration.

package core

import (
	"encoding/json"
	"fmt"
	"slices"
	"strings"
)

// BareTargetFixedResources is the refusal for a resource allocation aimed at
// a bare target. Callers match on it to add their own remedy (a flag, a
// checkbox).
const BareTargetFixedResources = "bare targets have fixed host resources"

type ResumeQueueDisposition string

const (
	ResumeQueueDispositionStart   ResumeQueueDisposition = "start"
	ResumeQueueDispositionDiscard ResumeQueueDisposition = "discard"
)

// TransferRoute is the route taken to move an archive (and optional
// `.mj/keep/` stream) between targets.
type TransferRoute string

const (
	TransferRouteSameHost     TransferRoute = "same_host"
	TransferRouteDirectPush   TransferRoute = "direct_push"
	TransferRouteDirectPull   TransferRoute = "direct_pull"
	TransferRouteDaemonStream TransferRoute = "daemon_stream"
)

func (r TransferRoute) IsDirect() bool {
	return r == TransferRouteDirectPush || r == TransferRouteDirectPull
}

func (r TransferRoute) IsPulled() bool { return r == TransferRouteDirectPull }

func (r TransferRoute) Label() string {
	switch r {
	case TransferRouteSameHost:
		return "same-host pipe"
	case TransferRouteDirectPush:
		return "direct push"
	case TransferRouteDirectPull:
		return "direct pull"
	}
	return "daemon stream"
}

func (r *TransferRoute) UnmarshalText(text []byte) error {
	switch value := TransferRoute(text); value {
	case TransferRouteSameHost, TransferRouteDirectPush, TransferRouteDirectPull, TransferRouteDaemonStream:
		*r = value
		return nil
	}
	return serdeUnknownVariant(string(text), "same_host", "direct_push", "direct_pull", "daemon_stream")
}

// RoutePreview is the read-only route preview for `--dry-run`. It never
// installs keys or AWS SG rules.
type RoutePreview string

const (
	RoutePreviewSameHost             RoutePreview = "same_host"
	RoutePreviewDirectOrDaemonStream RoutePreview = "direct_or_daemon_stream"
)

func (r RoutePreview) Label() string {
	if r == RoutePreviewSameHost {
		return "same-host pipe"
	}
	return "direct host-to-host (or daemon stream fallback)"
}

func (r *RoutePreview) UnmarshalText(text []byte) error {
	switch value := RoutePreview(text); value {
	case RoutePreviewSameHost, RoutePreviewDirectOrDaemonStream:
		*r = value
		return nil
	}
	return serdeUnknownVariant(string(text), "same_host", "direct_or_daemon_stream")
}

// GrantedMoveKey is the durable ownership record for an ephemeral Move SSH
// key / known_hosts stage.
type GrantedMoveKey struct {
	Host           string  `json:"host"`
	Comment        string  `json:"comment"`
	PrivateKeyPath *string `json:"private_key_path,omitempty"`
	KnownHostsPath *string `json:"known_hosts_path,omitempty"`
	// AuthorizedKeysPath is the remote `authorized_keys` file where the
	// ephemeral public key was installed.
	AuthorizedKeysPath *string `json:"authorized_keys_path,omitempty"`
	// WorkerBin is the host-side worker binary path used to run
	// `worker authorized-keys remove`.
	WorkerBin *string `json:"worker_bin,omitempty"`
	// SSHTarget is a self-contained SSH target so deferred revocation works
	// even after the Move retires its source or another Move runs on the
	// same session.
	SSHTarget *SshTarget `json:"ssh_target,omitempty"`
}

func (k *GrantedMoveKey) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "host", "comment"); err != nil {
		return err
	}
	type plain GrantedMoveKey
	return stateDecodeStrict(data, (*plain)(k))
}

// GrantedEc2Rule is the durable ownership record for a shared EC2
// security-group ingress grant used by a Move.
type GrantedEc2Rule struct {
	SecurityGroupID string `json:"security_group_id"`
	CidrIP          string `json:"cidr_ip"`
	Port            uint16 `json:"port"`
}

func (r *GrantedEc2Rule) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "security_group_id", "cidr_ip", "port"); err != nil {
		return err
	}
	type plain GrantedEc2Rule
	return stateDecodeStrict(data, (*plain)(r))
}

type MoveSelection struct {
	Consent SuspendConsent `json:"consent"`
	// SkipKeepPaths is the relative `.mj/keep/` paths excluded from
	// transfer.
	SkipKeepPaths           []string                  `json:"skip_keep_paths,omitempty"`
	ClearResourceAllocation bool                      `json:"clear_resource_allocation"`
	SessionID               string                    `json:"session_id"`
	ProfileID               *string                   `json:"profile_id"`
	TargetTemplateID        *string                   `json:"target_template_id"`
	AdditionalMounts        *[]AdditionalMount        `json:"additional_mounts"`
	ResourceAllocation      SessionResourceAllocation `json:"resource_allocation"`
	// Subagents is the delegation policy for the destination. Nil keeps the
	// session's own.
	Subagents *SubagentPolicy `json:"subagents,omitempty"`
	// Oracle is the Oracle choice for the destination; omitted retains the
	// session value.
	Oracle *bool `json:"oracle,omitempty"`
	// OracleChoice is the destination oracle model override; omitted retains
	// the session value.
	OracleChoice *SubagentChoice `json:"oracle_choice,omitempty"`
}

func (s *MoveSelection) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "session_id"); err != nil {
		return err
	}
	type plain MoveSelection
	aux := struct {
		*plain
		ResourceAllocation stateUnionJSON[SessionResourceAllocation] `json:"resource_allocation"`
	}{plain: (*plain)(s)}
	if err := stateDecodeStrict(data, &aux); err != nil {
		return err
	}
	s.ResourceAllocation = aux.ResourceAllocation.V
	return nil
}

// RawConversionPreview is what moving a local checkout into an isolated
// workspace will do, shown before anything is stopped or provisioned.
//
// Every field is read from the host checkout and its remote. The dirty
// counts are deliberately not part of a move fingerprint: a running local
// session has an agent editing files, so they change under the confirmation.
type RawConversionPreview struct {
	// Checkout is the checkout that is snapshotted: a managed worktree, or
	// the user's own directory when the session opened one directly.
	Checkout string `json:"checkout"`
	// Destination is where the checkout lands inside the target.
	Destination string `json:"destination"`
	// Branch is the branch the session continues on, or nil for a detached head.
	Branch   *string  `json:"branch"`
	FetchURL string   `json:"fetch_url"`
	PushURLs []string `json:"push_urls"`
	// DefaultBranch is the branch the remote's HEAD names, which is what a
	// fresh clone starts on before the session's own branch is restored.
	DefaultBranch string `json:"default_branch"`
	// UnpushedCommits are commits reachable from HEAD that are on no origin
	// ref, and so have to travel in the conversion archive.
	UnpushedCommits uint64 `json:"unpushed_commits"`
	StagedFiles     uint64 `json:"staged_files"`
	UnstagedFiles   uint64 `json:"unstaged_files"`
	UntrackedFiles  uint64 `json:"untracked_files"`
	UntrackedBytes  uint64 `json:"untracked_bytes"`
	// HostCheckoutRetained is true when the session opened the user's own
	// checkout, which stays on this machine untouched after the move.
	HostCheckoutRetained bool `json:"host_checkout_retained"`
}

func (p *RawConversionPreview) UnmarshalJSON(data []byte) error {
	type plain RawConversionPreview
	return stateDecodeStrict(data, (*plain)(p))
}

// SummaryLine is the one line every surface shows: what is cloned, where it
// lands, which branch the session continues on, and where git push goes.
func (p *RawConversionPreview) SummaryLine() string {
	push := p.FetchURL
	if len(p.PushURLs) > 0 {
		push = strings.Join(p.PushURLs, ", ")
	}
	branch := "a detached head"
	if p.Branch != nil {
		branch = *p.Branch
	}
	return fmt.Sprintf("Clone %s (default branch %s) into %s on branch %s; push to %s.",
		p.FetchURL, p.DefaultBranch, p.Destination, branch, push)
}

// WarningLines is what a person needs to read before the checkout moves:
// which uncommitted work travels, which commits travel, and what stays
// behind.
//
// Every surface renders these in its own warning style, so the wording
// lives here rather than in each of the TUI, the CLI, and the browser.
func (p *RawConversionPreview) WarningLines() []string {
	var lines []string
	dirty := p.StagedFiles + p.UnstagedFiles + p.UntrackedFiles
	if dirty > 0 {
		noun := "files"
		if dirty == 1 {
			noun = "file"
		}
		lines = append(lines, fmt.Sprintf("%d staged, %d unstaged, and %d untracked %s (%s) will be copied into the container. "+
			"Ignored files such as build output, .env, and node_modules will not.",
			p.StagedFiles, p.UnstagedFiles, p.UntrackedFiles, noun, formatConversionBytes(p.UntrackedBytes)))
	}
	if p.UnpushedCommits > 0 {
		noun, verb := "commits", "travel"
		if p.UnpushedCommits == 1 {
			noun, verb = "commit", "travels"
		}
		line := fmt.Sprintf("%d %s not on %s %s in the checkpoint.", p.UnpushedCommits, noun, p.FetchURL, verb)
		if p.UnpushedCommits > 200 {
			line += " That is a large history; consider pushing first."
		}
		lines = append(lines, line)
	}
	if p.HostCheckoutRetained {
		lines = append(lines, fmt.Sprintf("%s stays on this machine and will no longer track this session. "+
			"Edits made in the container do not come back automatically; "+
			"push the branch or move the session back.", p.Checkout))
	}
	return lines
}

// formatConversionBytes is untracked size as a person reads it. Only KB and
// MB appear: a checkout's untracked work is never usefully described in
// bytes, and anything above a gigabyte is already a warning in megabytes.
func formatConversionBytes(bytes uint64) string {
	const kb = 1024.0
	const mb = kb * 1024.0
	value := float64(bytes)
	if value < mb {
		return fmt.Sprintf("%.1f KB", value/kb)
	}
	return fmt.Sprintf("%.1f MB", value/mb)
}

const EC2MovePreparationNotice = "The EC2 instance will be created and checked after you confirm Move."

type DestinationChecks string

const (
	DestinationChecksChecked           DestinationChecks = "checked"
	DestinationChecksAfterProvisioning DestinationChecks = "after_provisioning"
)

// PreparedMoveDestination: the launch request is immutable for an attempt,
// including its client token.
type PreparedMoveDestination struct {
	LaunchArgs []string                 `json:"launch_args"`
	Runtime    TargetRuntimeSettings    `json:"runtime"`
	State      PreparedDestinationState `json:"state"`
}

func (d *PreparedMoveDestination) UnmarshalJSON(data []byte) error {
	type plain PreparedMoveDestination
	aux := struct {
		*plain
		State stateUnionJSON[PreparedDestinationState] `json:"state"`
	}{plain: (*plain)(d)}
	if err := stateDecodeStrict(data, &aux); err != nil {
		return err
	}
	if aux.State.V == nil {
		return fmt.Errorf("missing field `state`")
	}
	d.State = aux.State.V
	return nil
}

// PreparedDestinationState is tagged "phase", snake_case, with unknown fields
// denied.
type PreparedDestinationState interface {
	isPreparedDestinationState()
}

type PreparedDestinationStateLaunchPending struct{}

type PreparedDestinationStateCreated struct {
	InstanceID string `json:"instance_id"`
}

type PreparedDestinationStateChecked struct {
	Target TargetLocator `json:"target"`
}

type PreparedDestinationStateAdopted struct {
	Target TargetLocator `json:"target"`
}

type PreparedDestinationStateCleanupPending struct {
	InstanceID *string `json:"instance_id"`
}

type PreparedDestinationStateReleased struct{}

func (PreparedDestinationStateLaunchPending) isPreparedDestinationState()  {}
func (PreparedDestinationStateCreated) isPreparedDestinationState()        {}
func (PreparedDestinationStateChecked) isPreparedDestinationState()        {}
func (PreparedDestinationStateAdopted) isPreparedDestinationState()        {}
func (PreparedDestinationStateCleanupPending) isPreparedDestinationState() {}
func (PreparedDestinationStateReleased) isPreparedDestinationState()       {}

func (PreparedDestinationStateLaunchPending) MarshalJSON() ([]byte, error) {
	return stateMarshalTagged("phase", "launch_pending", nil)
}

func (v PreparedDestinationStateCreated) MarshalJSON() ([]byte, error) {
	type plain PreparedDestinationStateCreated
	return stateMarshalTagged("phase", "created", plain(v))
}

func (v PreparedDestinationStateChecked) MarshalJSON() ([]byte, error) {
	type plain PreparedDestinationStateChecked
	return stateMarshalTagged("phase", "checked", plain(v))
}

func (v PreparedDestinationStateAdopted) MarshalJSON() ([]byte, error) {
	type plain PreparedDestinationStateAdopted
	return stateMarshalTagged("phase", "adopted", plain(v))
}

func (v PreparedDestinationStateCleanupPending) MarshalJSON() ([]byte, error) {
	type plain PreparedDestinationStateCleanupPending
	return stateMarshalTagged("phase", "cleanup_pending", plain(v))
}

func (PreparedDestinationStateReleased) MarshalJSON() ([]byte, error) {
	return stateMarshalTagged("phase", "released", nil)
}

func unmarshalPreparedDestinationState(data []byte) (PreparedDestinationState, error) {
	tag, err := stateTag(data, "phase")
	if err != nil {
		return nil, err
	}
	var target struct {
		Target stateUnionJSON[TargetLocator] `json:"target"`
	}
	switch tag {
	case "launch_pending":
		return PreparedDestinationStateLaunchPending{}, stateDecodeStrict(data, &struct{}{}, "phase")
	case "created":
		var value PreparedDestinationStateCreated
		err := stateDecodeStrict(data, &value, "phase")
		return value, err
	case "checked":
		err := stateDecodeStrict(data, &target, "phase")
		return PreparedDestinationStateChecked{target.Target.V}, err
	case "adopted":
		err := stateDecodeStrict(data, &target, "phase")
		return PreparedDestinationStateAdopted{target.Target.V}, err
	case "cleanup_pending":
		var value PreparedDestinationStateCleanupPending
		err := stateDecodeStrict(data, &value, "phase")
		return value, err
	case "released":
		return PreparedDestinationStateReleased{}, stateDecodeStrict(data, &struct{}{}, "phase")
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

func (d *PreparedMoveDestination) Target() TargetLocator {
	switch state := d.State.(type) {
	case PreparedDestinationStateChecked:
		return state.Target
	case PreparedDestinationStateAdopted:
		return state.Target
	}
	return nil
}

func (d *PreparedMoveDestination) InstanceID() (string, bool) {
	switch state := d.State.(type) {
	case PreparedDestinationStateCreated:
		return state.InstanceID, true
	case PreparedDestinationStateCleanupPending:
		if state.InstanceID != nil {
			return *state.InstanceID, true
		}
	}
	if ec2, ok := d.Target().(TargetLocatorAwsEc2); ok {
		return ec2.InstanceID, true
	}
	return "", false
}

func (d *PreparedMoveDestination) OwnsResource() bool {
	_, released := d.State.(PreparedDestinationStateReleased)
	return !released
}

type MovePreparation struct {
	ContainerUpgrade  *ContainerUpgradePreview `json:"container_upgrade,omitempty"`
	DestinationChecks DestinationChecks        `json:"destination_checks"`
	RoutePreview      *RoutePreview            `json:"route_preview,omitempty"`
	SuspendPreflight  *SuspendPreflight        `json:"suspend_preflight,omitempty"`
	// SkipKeepSizes is the skipped `.mj/keep/` paths and their sizes in
	// bytes, for `--dry-run` and dialogs.
	SkipKeepSizes     []SkipKeepSize `json:"skip_keep_sizes,omitempty"`
	SourceUnavailable bool           `json:"source_unavailable"`
	// InPlace: the destination is the source target: only the harness is
	// replaced; the container or worker root and the workspace are kept.
	InPlace bool `json:"in_place"`
	// StopsSubagents: Move stops the parent's live sub-agents before it
	// closes the source. An in-place Move keeps them running when the
	// destination harness has the mj-agents parent tools and the source
	// worker can drain sub-agent requests.
	StopsSubagents bool `json:"stops_subagents"`
	// Conversion is present only when this move converts a local checkout
	// into an isolated workspace.
	Conversion *RawConversionPreview `json:"conversion,omitempty"`
	Selection  MoveSelection         `json:"selection"`
	// SizedByDestinationDefault: the selection's size is the destination's
	// default, which a clear request resolved to. The sealed Move owns this
	// fact so that retrying the same command compares against what this Move
	// decided rather than against host state other sessions keep changing.
	SizedByDestinationDefault bool   `json:"sized_by_destination_default,omitempty"`
	SourceProfileID           string `json:"source_profile_id"`
	SourceTargetTemplateID    string `json:"source_target_template_id"`
	// CrossHarness: the destination cannot continue the native
	// conversation, because its harness or model service differs (see
	// Config.NativeContinuity), so the transcript is handed over as
	// compacted text. Stored move records keep this name.
	CrossHarness   bool                       `json:"cross_harness"`
	Active         bool                       `json:"active"`
	QueuedCommands []MaterializedQueuedPrompt `json:"queued_commands"`
	// LockedQueue is the queue choice already locked by an earlier retry
	// whose queue admission started. Callers may omit `--queue` on retry and
	// inherit this choice.
	LockedQueue *ResumeQueueDisposition `json:"locked_queue,omitempty"`
	Fingerprint string                  `json:"fingerprint"`
	OperationID string                  `json:"operation_id"`
}

func (p *MovePreparation) UnmarshalJSON(data []byte) error {
	type plain MovePreparation
	p.DestinationChecks = DestinationChecksChecked
	return stateDecodeStrict(data, (*plain)(p))
}

// SkipKeepSize is one skipped `.mj/keep/` path and its size in bytes,
// serialized as serde writes a `(PathBuf, u64)` tuple: `[path, bytes]`.
type SkipKeepSize struct {
	Path  string
	Bytes uint64
}

func (s SkipKeepSize) MarshalJSON() ([]byte, error) {
	return json.Marshal([]any{s.Path, s.Bytes})
}

func (s *SkipKeepSize) UnmarshalJSON(data []byte) error {
	var tuple []json.RawMessage
	if err := json.Unmarshal(data, &tuple); err != nil {
		return err
	}
	if len(tuple) != 2 {
		return fmt.Errorf("invalid length %d, expected a tuple of size 2", len(tuple))
	}
	if err := json.Unmarshal(tuple[0], &s.Path); err != nil {
		return err
	}
	return json.Unmarshal(tuple[1], &s.Bytes)
}

type MoveSessionRequest struct {
	Preparation             MovePreparation         `json:"preparation"`
	Queue                   *ResumeQueueDisposition `json:"queue"`
	AcknowledgeInterruption bool                    `json:"acknowledge_interruption"`
}

func (r *MoveSessionRequest) UnmarshalJSON(data []byte) error {
	type plain MoveSessionRequest
	return stateDecodeStrict(data, (*plain)(r))
}

type MoveOutcome struct {
	OperationID      string  `json:"operation_id"`
	SessionID        string  `json:"session_id"`
	ProfileID        string  `json:"profile_id"`
	TargetTemplateID string  `json:"target_template_id"`
	Outcome          string  `json:"outcome"`
	Error            *string `json:"error"`
	Recovery         *string `json:"recovery"`
}

func (o *MoveOutcome) UnmarshalJSON(data []byte) error {
	type plain MoveOutcome
	return stateDecodeStrict(data, (*plain)(o))
}

type MovePhase string

const (
	MovePhaseCapturing           MovePhase = "capturing"
	MovePhaseTransferring        MovePhase = "transferring"
	MovePhaseRestoring           MovePhase = "restoring"
	MovePhaseDestinationStarting MovePhase = "destination_starting"
	MovePhaseDestinationOwns     MovePhase = "destination_owns"
	MovePhaseSourceRetired       MovePhase = "source_retired"
	MovePhaseQueueAdmitted       MovePhase = "queue_admitted"
	MovePhaseDispatchReleased    MovePhase = "dispatch_released"
	MovePhaseCompleted           MovePhase = "completed"
	MovePhaseFailed              MovePhase = "failed"
	MovePhaseCancelled           MovePhase = "cancelled"
)

func (p *MovePhase) UnmarshalText(text []byte) error {
	switch value := MovePhase(text); value {
	case MovePhaseCapturing, MovePhaseTransferring, MovePhaseRestoring, MovePhaseDestinationStarting,
		MovePhaseDestinationOwns, MovePhaseSourceRetired, MovePhaseQueueAdmitted, MovePhaseDispatchReleased,
		MovePhaseCompleted, MovePhaseFailed, MovePhaseCancelled:
		*p = value
		return nil
	}
	return serdeUnknownVariant(string(text), "capturing", "transferring", "restoring", "destination_starting",
		"destination_owns", "source_retired", "queue_admitted", "dispatch_released", "completed", "failed", "cancelled")
}

// DestinationOwns is true once the cross-target Move has crossed its commit
// point and the destination durably owns the session.
func (p MovePhase) DestinationOwns() bool {
	return p == MovePhaseDestinationOwns || p == MovePhaseSourceRetired || p == MovePhaseQueueAdmitted
}

// LegacyMovePhase is the pre-148 MovePhase variants kept solely for startup
// recovery of in-flight legacy MoveOperation records across an upgrade.
type LegacyMovePhase string

const (
	LegacyMovePhasePreparing           LegacyMovePhase = "preparing"
	LegacyMovePhaseClosingSource       LegacyMovePhase = "closing_source"
	LegacyMovePhaseResumingDestination LegacyMovePhase = "resuming_destination"
	LegacyMovePhaseStartingQueue       LegacyMovePhase = "starting_queue"
	LegacyMovePhaseCompleted           LegacyMovePhase = "completed"
	LegacyMovePhaseFailed              LegacyMovePhase = "failed"
	LegacyMovePhaseCancelled           LegacyMovePhase = "cancelled"
)

func (p *LegacyMovePhase) UnmarshalText(text []byte) error {
	switch value := LegacyMovePhase(text); value {
	case LegacyMovePhasePreparing, LegacyMovePhaseClosingSource, LegacyMovePhaseResumingDestination,
		LegacyMovePhaseStartingQueue, LegacyMovePhaseCompleted, LegacyMovePhaseFailed, LegacyMovePhaseCancelled:
		*p = value
		return nil
	}
	return serdeUnknownVariant(string(text), "preparing", "closing_source", "resuming_destination", "starting_queue",
		"completed", "failed", "cancelled")
}

func (p LegacyMovePhase) IsActive() bool {
	switch p {
	case LegacyMovePhasePreparing, LegacyMovePhaseClosingSource, LegacyMovePhaseResumingDestination,
		LegacyMovePhaseStartingQueue:
		return true
	}
	return false
}

// LegacyMoveSelection is the pre-149 MoveSelection fields preserved when
// converting a retryable legacy Move across an upgrade (ignores removed
// legacy fields such as `workspace`).
type LegacyMoveSelection struct {
	SessionID               string                    `json:"session_id"`
	ProfileID               *string                   `json:"profile_id"`
	TargetTemplateID        *string                   `json:"target_template_id"`
	AdditionalMounts        *[]AdditionalMount        `json:"additional_mounts"`
	ResourceAllocation      SessionResourceAllocation `json:"resource_allocation"`
	ClearResourceAllocation bool                      `json:"clear_resource_allocation"`
	Subagents               *SubagentPolicy           `json:"subagents"`
	Oracle                  *bool                     `json:"oracle"`
	OracleChoice            *SubagentChoice           `json:"oracle_choice"`
}

func (s *LegacyMoveSelection) UnmarshalJSON(data []byte) error {
	type plain LegacyMoveSelection
	aux := struct {
		*plain
		ResourceAllocation stateUnionJSON[SessionResourceAllocation] `json:"resource_allocation"`
	}{plain: (*plain)(s)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	s.ResourceAllocation = aux.ResourceAllocation.V
	return nil
}

// LegacyMoveOperation is the pre-149 MoveOperation shape decoded only during
// startup recovery so an upgraded daemon can roll back source-owned legacy
// moves, finish forward destination-owned legacy moves, or preserve
// retryable failed/cancelled moves.
type LegacyMoveOperation struct {
	Selection                   *LegacyMoveSelection      `json:"selection"`
	QuotaStopReason             *string                   `json:"quota_stop_reason"`
	QuotaContinuation           *QuotaMoveContinuation    `json:"quota_continuation"`
	PreparedDestination         *PreparedMoveDestination  `json:"prepared_destination"`
	AcknowledgeInterruption     bool                      `json:"acknowledge_interruption"`
	Handoff                     *CheckpointMetadata       `json:"handoff"`
	SourceCheckpointOnly        bool                      `json:"source_checkpoint_only"`
	InPlace                     bool                      `json:"in_place"`
	OperationID                 string                    `json:"operation_id"`
	SourceProfileID             string                    `json:"source_profile_id"`
	SourceTargetTemplateID      string                    `json:"source_target_template_id"`
	SourceTarget                TargetLocator             `json:"source_target"`
	SourceNativeSessionID       *string                   `json:"source_native_session_id"`
	InheritedResourceAllocation SessionResourceAllocation `json:"inherited_resource_allocation"`
	InheritedAdditionalMounts   []AdditionalMount         `json:"inherited_additional_mounts"`
	SourceAdditionalMounts      []AdditionalMount         `json:"source_additional_mounts"`
	SourceResourceAllocation    SessionResourceAllocation `json:"source_resource_allocation"`
	DestinationTarget           TargetLocator             `json:"destination_target"`
	DestinationNativeSessionID  *string                   `json:"destination_native_session_id"`
	DestinationStoreID          *string                   `json:"destination_store_id"`
	ConfigurationFingerprint    string                    `json:"configuration_fingerprint"`
	Checkpoint                  *CheckpointMetadata       `json:"checkpoint"`
	RecoverySession             *SessionRecord            `json:"recovery_session"`
	Queue                       ResumeQueueDisposition    `json:"queue"`
	Phase                       LegacyMovePhase           `json:"phase"`
	QueueAdmissionStarted       bool                      `json:"queue_admission_started"`
	QueueAdmissionFinished      bool                      `json:"queue_admission_finished"`
	CancellationRequested       bool                      `json:"cancellation_requested"`
	CreatedAt                   string                    `json:"created_at"`
	UpdatedAt                   string                    `json:"updated_at"`
	Error                       *string                   `json:"error"`
}

// MarshalJSON writes the legacy mount lists as [], as serde writes a Vec.
func (o LegacyMoveOperation) MarshalJSON() ([]byte, error) {
	type plain LegacyMoveOperation
	if o.InheritedAdditionalMounts == nil {
		o.InheritedAdditionalMounts = []AdditionalMount{}
	}
	if o.SourceAdditionalMounts == nil {
		o.SourceAdditionalMounts = []AdditionalMount{}
	}
	return json.Marshal(plain(o))
}

func (o *LegacyMoveOperation) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "operation_id", "source_profile_id", "source_target_template_id", "queue",
		"phase", "created_at", "updated_at"); err != nil {
		return err
	}
	type plain LegacyMoveOperation
	aux := struct {
		*plain
		SourceTarget                stateUnionJSON[TargetLocator]             `json:"source_target"`
		InheritedResourceAllocation stateUnionJSON[SessionResourceAllocation] `json:"inherited_resource_allocation"`
		SourceResourceAllocation    stateUnionJSON[SessionResourceAllocation] `json:"source_resource_allocation"`
		DestinationTarget           stateUnionJSON[TargetLocator]             `json:"destination_target"`
	}{plain: (*plain)(o)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	o.SourceTarget = aux.SourceTarget.V
	o.InheritedResourceAllocation = aux.InheritedResourceAllocation.V
	o.SourceResourceAllocation = aux.SourceResourceAllocation.V
	o.DestinationTarget = aux.DestinationTarget.V
	return nil
}

// RetainedArchives is every checkpoint archive this legacy Move still
// references for retry or recovery.
func (o *LegacyMoveOperation) RetainedArchives() []*CheckpointMetadata {
	if o.IsResolved() {
		return nil
	}
	var archives []*CheckpointMetadata
	if o.Handoff != nil {
		archives = append(archives, o.Handoff)
	}
	if o.Checkpoint != nil && !slices.ContainsFunc(archives, func(existing *CheckpointMetadata) bool {
		return existing.ArchivePath == o.Checkpoint.ArchivePath
	}) {
		archives = append(archives, o.Checkpoint)
	}
	return archives
}

// IsResolved reports whether this legacy Move has no remaining work,
// retryable archive, or prepared destination and can safely be deleted on
// upgrade.
func (o *LegacyMoveOperation) IsResolved() bool {
	switch o.Phase {
	case LegacyMovePhaseCompleted:
		return true
	case LegacyMovePhaseFailed, LegacyMovePhaseCancelled:
		return !o.QueueAdmissionStarted && o.Handoff == nil && o.Checkpoint == nil && o.PreparedDestination == nil
	}
	return false
}

// NeedsRecovery reports whether startup recovery must actively roll back or
// finish forward this legacy Move.
func (o *LegacyMoveOperation) NeedsRecovery() bool {
	return o.Phase.IsActive() || (o.QueueAdmissionStarted && !o.QueueAdmissionFinished)
}

// IntoRetryableMoveOperation converts a retryable inactive (Failed /
// Cancelled) legacy Move into a current MoveOperation so retry state and
// archive retention survive upgrade.
func (o *LegacyMoveOperation) IntoRetryableMoveOperation(sessionID string) MoveOperation {
	selection := MoveSelection{SessionID: sessionID}
	if sel := o.Selection; sel != nil {
		selection.ClearResourceAllocation = sel.ClearResourceAllocation
		if sel.SessionID != "" {
			selection.SessionID = sel.SessionID
		}
		selection.ProfileID = cloneStringPointer(sel.ProfileID)
		selection.TargetTemplateID = cloneStringPointer(sel.TargetTemplateID)
		if sel.AdditionalMounts != nil {
			mounts := slices.Clone(*sel.AdditionalMounts)
			selection.AdditionalMounts = &mounts
		}
		selection.ResourceAllocation = sel.ResourceAllocation
		selection.Subagents = sel.Subagents
		selection.Oracle = sel.Oracle
		selection.OracleChoice = sel.OracleChoice
	}
	checkpoint := o.Handoff
	if checkpoint == nil {
		checkpoint = o.Checkpoint
	}
	phase := MovePhaseFailed
	if o.Phase == LegacyMovePhaseCancelled {
		phase = MovePhaseCancelled
	}
	return MoveOperation{
		QuotaStopReason:             o.QuotaStopReason,
		QuotaContinuation:           o.QuotaContinuation,
		PreparedDestination:         o.PreparedDestination,
		AcknowledgeInterruption:     o.AcknowledgeInterruption,
		SourceCheckpointOnly:        o.SourceCheckpointOnly,
		InPlace:                     o.InPlace,
		OperationID:                 o.OperationID,
		Selection:                   selection,
		SourceProfileID:             o.SourceProfileID,
		SourceTargetTemplateID:      o.SourceTargetTemplateID,
		SourceTarget:                o.SourceTarget,
		SourceNativeSessionID:       o.SourceNativeSessionID,
		InheritedResourceAllocation: o.InheritedResourceAllocation,
		InheritedAdditionalMounts:   slices.Clone(o.InheritedAdditionalMounts),
		SourceAdditionalMounts:      slices.Clone(o.SourceAdditionalMounts),
		SourceResourceAllocation:    o.SourceResourceAllocation,
		DestinationTarget:           o.DestinationTarget,
		DestinationNativeSessionID:  o.DestinationNativeSessionID,
		DestinationStoreID:          o.DestinationStoreID,
		ConfigurationFingerprint:    o.ConfigurationFingerprint,
		Checkpoint:                  checkpoint,
		RecoverySession:             o.RecoverySession,
		Queue:                       o.Queue,
		Phase:                       phase,
		QueueAdmissionStarted:       o.QueueAdmissionStarted,
		QueueAdmissionFinished:      o.QueueAdmissionFinished,
		CancellationRequested:       o.CancellationRequested,
		CreatedAt:                   o.CreatedAt,
		UpdatedAt:                   o.UpdatedAt,
		Error:                       o.Error,
	}
}

// StoredMoveOperation is the result of decoding a stored MoveOperation JSON
// row: exactly one of Current and Legacy is set.
type StoredMoveOperation struct {
	Current *MoveOperation
	Legacy  *LegacyMoveOperation
}

// DecodeStoredMoveOperationJSON decodes a stored row as the current shape,
// else as the legacy one; when neither reads, the current shape's error is
// returned.
func DecodeStoredMoveOperationJSON(text string) (StoredMoveOperation, error) {
	var current MoveOperation
	currentErr := json.Unmarshal([]byte(text), &current)
	if currentErr == nil {
		return StoredMoveOperation{Current: &current}, nil
	}
	var legacy LegacyMoveOperation
	if err := json.Unmarshal([]byte(text), &legacy); err == nil {
		return StoredMoveOperation{Legacy: &legacy}, nil
	}
	return StoredMoveOperation{}, currentErr
}

func (s StoredMoveOperation) OperationID() string {
	if s.Current != nil {
		return s.Current.OperationID
	}
	return s.Legacy.OperationID
}

func (s StoredMoveOperation) IsActive() bool {
	if s.Current != nil {
		return s.Current.IsActive()
	}
	return s.Legacy.Phase.IsActive()
}

// QuotaMoveContinuation is the continuation chosen once on the destination
// before queue admission.
type QuotaMoveContinuation string

const (
	QuotaMoveContinuationPrompt     QuotaMoveContinuation = "prompt"
	QuotaMoveContinuationResumeGoal QuotaMoveContinuation = "resume_goal"
)

// MoveSourceSnapshot is the durable metadata for a cross-target Move archive
// that remains on the source target and streams directly to the
// destination (never installed on the daemon as `session.checkpoint`).
type MoveSourceSnapshot struct {
	// ArchivePath is the archive path on the source target.
	ArchivePath string `json:"archive_path"`
	// Sha256 is the verified SHA-256 digest of the source-target archive.
	Sha256 string `json:"sha256"`
	// Manifest is the archive manifest returned by the source worker at
	// export time.
	Manifest *ArchiveManifest `json:"manifest,omitempty"`
	// QueuedPrompts is the queued prompts captured at the source barrier for
	// post-commit admission.
	QueuedPrompts []CanonicalQueuedPrompt `json:"queued_prompts,omitempty"`
	// Mailbox is the pending worker mail captured at the source barrier for
	// post-commit admission.
	Mailbox *CanonicalMailbox `json:"mailbox,omitempty"`
}

func (s *MoveSourceSnapshot) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "archive_path", "sha256"); err != nil {
		return err
	}
	type plain MoveSourceSnapshot
	return stateDecodeStrict(data, (*plain)(s))
}

type MoveOperation struct {
	// QuotaStopReason is the daemon-owned continuation after an automatic
	// quota profile switch.
	QuotaStopReason         *string                  `json:"quota_stop_reason,omitempty"`
	QuotaContinuation       *QuotaMoveContinuation   `json:"quota_continuation,omitempty"`
	PreparedDestination     *PreparedMoveDestination `json:"prepared_destination,omitempty"`
	AcceptedPreparation     *MovePreparation         `json:"accepted_preparation,omitempty"`
	AcknowledgeInterruption bool                     `json:"acknowledge_interruption"`
	// TransferRoute is the route chosen for cross-target archive and
	// `.mj/keep/` transfer.
	TransferRoute *TransferRoute `json:"transfer_route,omitempty"`
	// Consent is the typed suspend consent captured for this Move.
	Consent SuspendConsent `json:"consent"`
	// SkipKeepPaths is the relative `.mj/keep/` paths excluded from
	// transfer.
	SkipKeepPaths []string `json:"skip_keep_paths,omitempty"`
	// GrantedKeys is the ephemeral SSH keys / known_hosts files owned by
	// this Move (persisted before install).
	GrantedKeys []GrantedMoveKey `json:"granted_keys,omitempty"`
	// GrantedEc2Rules is the shared EC2 security-group ingress rules used by
	// this Move (persisted before install).
	GrantedEc2Rules []GrantedEc2Rule `json:"granted_ec2_rules,omitempty"`
	// InPlaceExpectedStoreID: in-place Move: relay `store_id` captured
	// before holding dispatch.
	InPlaceExpectedStoreID *string `json:"in_place_expected_store_id,omitempty"`
	// InPlaceContinuationCommandID: in-place Move: operation-derived command
	// ID for `SubmitFront` continuation.
	InPlaceContinuationCommandID *string `json:"in_place_continuation_command_id,omitempty"`
	// InPlaceHeldCommandIDs: in-place Move: pending command IDs recorded
	// when `HoldInPlaceMoveDispatch` ran.
	InPlaceHeldCommandIDs []string `json:"in_place_held_command_ids,omitempty"`
	// InPlaceHarnessRestarted: in-place Move: whether
	// `RestartHarnessInPlace` has completed.
	InPlaceHarnessRestarted bool `json:"in_place_harness_restarted"`
	// DeferredEnvironmentChanges is the environment changes deferred until
	// the session's environment is next recreated.
	DeferredEnvironmentChanges []string `json:"deferred_environment_changes,omitempty"`
	// SourceSnapshot is the source-target Move snapshot streamed directly to
	// the destination without staging on daemon disk or overwriting
	// `session.checkpoint`.
	SourceSnapshot *MoveSourceSnapshot `json:"source_snapshot,omitempty"`
	// SourceCheckpointOnly keeps the source harness stopped across recovery
	// until destination restoration.
	SourceCheckpointOnly bool `json:"source_checkpoint_only"`
	// InPlace: the destination is the source target: only the harness is
	// replaced; the container or worker root and the workspace are kept.
	InPlace                bool          `json:"in_place"`
	OperationID            string        `json:"operation_id"`
	Selection              MoveSelection `json:"selection"`
	SourceProfileID        string        `json:"source_profile_id"`
	SourceTargetTemplateID string        `json:"source_target_template_id"`
	SourceTarget           TargetLocator `json:"source_target"`
	SourceNativeSessionID  *string       `json:"source_native_session_id"`
	// InheritedResourceAllocation is the frozen request inherited by the
	// destination when selection says keep.
	InheritedResourceAllocation SessionResourceAllocation `json:"inherited_resource_allocation,omitempty"`
	InheritedAdditionalMounts   []AdditionalMount         `json:"inherited_additional_mounts,omitempty"`
	// SourceAdditionalMounts and SourceResourceAllocation are applied source
	// settings, used only for source recovery.
	SourceAdditionalMounts     []AdditionalMount         `json:"source_additional_mounts"`
	SourceResourceAllocation   SessionResourceAllocation `json:"source_resource_allocation"`
	DestinationTarget          TargetLocator             `json:"destination_target"`
	DestinationNativeSessionID *string                   `json:"destination_native_session_id"`
	DestinationStoreID         *string                   `json:"destination_store_id"`
	ConfigurationFingerprint   string                    `json:"configuration_fingerprint"`
	Checkpoint                 *CheckpointMetadata       `json:"checkpoint"`
	// RecoverySession is the stopped identity retained across partially
	// written resume conversions.
	RecoverySession *SessionRecord         `json:"recovery_session"`
	Queue           ResumeQueueDisposition `json:"queue"`
	Phase           MovePhase              `json:"phase"`
	// QueueAdmissionStarted is a durable boundary: once set, never restore
	// or replay on another relay.
	QueueAdmissionStarted  bool    `json:"queue_admission_started"`
	QueueAdmissionFinished bool    `json:"queue_admission_finished"`
	CancellationRequested  bool    `json:"cancellation_requested"`
	CreatedAt              string  `json:"created_at"`
	UpdatedAt              string  `json:"updated_at"`
	Error                  *string `json:"error"`
}

func (o *MoveOperation) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "operation_id", "selection", "source_profile_id", "source_target_template_id",
		"source_additional_mounts", "configuration_fingerprint", "queue", "phase", "queue_admission_started",
		"queue_admission_finished", "cancellation_requested", "created_at", "updated_at"); err != nil {
		return err
	}
	type plain MoveOperation
	aux := struct {
		*plain
		SourceTarget                stateUnionJSON[TargetLocator]             `json:"source_target"`
		InheritedResourceAllocation stateUnionJSON[SessionResourceAllocation] `json:"inherited_resource_allocation"`
		SourceResourceAllocation    stateUnionJSON[SessionResourceAllocation] `json:"source_resource_allocation"`
		DestinationTarget           stateUnionJSON[TargetLocator]             `json:"destination_target"`
	}{plain: (*plain)(o)}
	if err := stateDecodeStrict(data, &aux); err != nil {
		return err
	}
	o.SourceTarget = aux.SourceTarget.V
	o.InheritedResourceAllocation = aux.InheritedResourceAllocation.V
	o.SourceResourceAllocation = aux.SourceResourceAllocation.V
	o.DestinationTarget = aux.DestinationTarget.V
	return nil
}

// QueueCommandIDs is every command a retained Move queue can resubmit,
// including quota continuation.
func (o *MoveOperation) QueueCommandIDs(canonical *CanonicalSessionSnapshot) map[string]struct{} {
	return o.QueueCommandIDsFromPrompts(canonical.QueuedPrompts)
}

func (o *MoveOperation) QueueCommandIDsFromPrompts(queuedPrompts []CanonicalQueuedPrompt) map[string]struct{} {
	ids := map[string]struct{}{}
	for _, queued := range queuedPrompts {
		ids[queued.CommandID] = struct{}{}
	}
	if o.QuotaStopReason != nil {
		ids[GeneratedPromptQuotaRetry.CommandID(o.OperationID)] = struct{}{}
	}
	return ids
}

// CaptureContainerSettings captures the two independently owned facts at
// acceptance of each attempt.
func (o *MoveOperation) CaptureContainerSettings(source *SessionRecord) {
	applied := AppliedContainerSettingsOf(source)
	o.SourceResourceAllocation = applied.ResourceAllocation
	o.SourceAdditionalMounts = append([]AdditionalMount{}, applied.AdditionalMounts...)
	if o.Selection.ResourceAllocation == nil && !o.Selection.ClearResourceAllocation {
		o.InheritedResourceAllocation = source.ResourceAllocation
	} else {
		o.InheritedResourceAllocation = nil
	}
	if o.Selection.AdditionalMounts == nil {
		o.InheritedAdditionalMounts = slices.Clone(source.AdditionalMounts)
	} else {
		o.InheritedAdditionalMounts = nil
	}
}

// IsResolved: explicit Resume resolves a cancelled Move by finishing its
// queue without ever admitting it. Its archives are then historical, just
// as for a completed Move, and cannot make a later Move a retry.
func (o *MoveOperation) IsResolved() bool {
	return o.Phase == MovePhaseCompleted ||
		(o.Phase == MovePhaseCancelled && !o.QueueAdmissionStarted && o.QueueAdmissionFinished)
}

// RetainsSourceEnvironment reports whether this Move retains its source
// environment before destination ownership commits. Both in-place Moves
// (which never recreate the target) and cross-target Moves before
// DestinationOwns keep the source intact so any pre-commit interruption or
// failure rolls back to the source instead of tearing it down before a
// destination exists.
func (o *MoveOperation) RetainsSourceEnvironment() bool {
	return o.InPlace || (!o.Phase.DestinationOwns() && !o.QueueAdmissionStarted)
}

func (o *MoveOperation) RestoreArtifact() *CheckpointMetadata {
	return o.Checkpoint
}

func (o *MoveOperation) RetainsCheckpoint() bool {
	return (!o.IsResolved() && o.Phase != MovePhaseCancelled) ||
		(o.QueueAdmissionStarted && !o.QueueAdmissionFinished)
}

// RetainedArchives is every checkpoint archive this Move keeps on disk. The
// startup archive sweep and the superseded-checkpoint prune both ask this
// one question, so neither can delete an archive the Move still restores
// from.
func (o *MoveOperation) RetainedArchives() []*CheckpointMetadata {
	if !o.RetainsCheckpoint() || o.Checkpoint == nil {
		return nil
	}
	return []*CheckpointMetadata{o.Checkpoint}
}

// CheckpointRetained reports whether an unfinished Move still has the
// checkpoint a retry restores.
//
// The Move record owns this fact. When its archive is found missing, the
// reference is removed from the record, so the recovery guidance, the API
// and the retry admission all read the loss from here.
func (o *MoveOperation) CheckpointRetained() bool {
	return !o.IsResolved() && (o.RestoreArtifact() != nil || o.SourceSnapshot != nil)
}

// OffersRetry reports whether a surface should offer to retry this failed
// or cancelled Move: it still has its checkpoint, or queued work already
// began on the destination and only this Move may finish admitting it.
func (o *MoveOperation) OffersRetry() bool {
	return (o.Phase == MovePhaseFailed || o.Phase == MovePhaseCancelled) &&
		(o.CheckpointRetained() || (o.QueueAdmissionStarted && !o.QueueAdmissionFinished))
}

// HoldsSourceEnvironment reports whether this unfinished Move holds the
// source environment (the target, its worker root and the checkout) for an
// explicit retry. While it does, Resume would recreate what the Move
// promised to keep, so only a retried Move or Destroy may act on the
// session.
func (o *MoveOperation) HoldsSourceEnvironment() bool {
	return o.RetainsSourceEnvironment() && !o.IsResolved() && o.RecoverySession != nil
}

func (o *MoveOperation) IsActive() bool {
	switch o.Phase {
	case MovePhaseCapturing, MovePhaseTransferring, MovePhaseRestoring, MovePhaseDestinationStarting,
		MovePhaseDestinationOwns, MovePhaseSourceRetired, MovePhaseQueueAdmitted, MovePhaseDispatchReleased:
		return true
	}
	return false
}
