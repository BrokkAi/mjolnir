// Port of mj-core/src/state/session_move.rs.
//
// Durable intent for a verified stop followed by destination restoration.

package core

import "fmt"

// BareTargetFixedResources is the refusal for a resource allocation aimed at
// a bare target. Callers match on it to add their own remedy (a flag, a
// checkbox).
const BareTargetFixedResources = "bare targets have fixed host resources"

type ResumeQueueDisposition string

const (
	ResumeQueueDispositionStart   ResumeQueueDisposition = "start"
	ResumeQueueDispositionDiscard ResumeQueueDisposition = "discard"
)

type MoveSelection struct {
	Workspace               WorkspaceSelection        `json:"workspace"`
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
	panic("unported: mj-core/src/state/session_move.rs RawConversionPreview::summary_line")
}

// WarningLines is what a person needs to read before the checkout moves:
// which uncommitted work travels, which commits travel, and what stays
// behind.
//
// Every surface renders these in its own warning style, so the wording
// lives here rather than in each of the TUI, the CLI, and the browser.
func (p *RawConversionPreview) WarningLines() []string {
	panic("unported: mj-core/src/state/session_move.rs RawConversionPreview::warning_lines")
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
	DestinationChecks DestinationChecks    `json:"destination_checks"`
	Workspace         *WorkspaceAssessment `json:"workspace,omitempty"`
	SourceUnavailable bool                 `json:"source_unavailable"`
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
	Fingerprint    string                     `json:"fingerprint"`
	OperationID    string                     `json:"operation_id"`
}

func (p *MovePreparation) UnmarshalJSON(data []byte) error {
	type plain MovePreparation
	p.DestinationChecks = DestinationChecksChecked
	return stateDecodeStrict(data, (*plain)(p))
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
	MovePhasePreparing           MovePhase = "preparing"
	MovePhaseClosingSource       MovePhase = "closing_source"
	MovePhaseResumingDestination MovePhase = "resuming_destination"
	MovePhaseStartingQueue       MovePhase = "starting_queue"
	MovePhaseCompleted           MovePhase = "completed"
	MovePhaseFailed              MovePhase = "failed"
	MovePhaseCancelled           MovePhase = "cancelled"
)

// QuotaMoveContinuation is the continuation chosen once on the destination
// before queue admission.
type QuotaMoveContinuation string

const (
	QuotaMoveContinuationPrompt     QuotaMoveContinuation = "prompt"
	QuotaMoveContinuationResumeGoal QuotaMoveContinuation = "resume_goal"
)

type MoveOperation struct {
	// QuotaStopReason is the daemon-owned continuation after an automatic
	// quota profile switch.
	QuotaStopReason         *string                  `json:"quota_stop_reason,omitempty"`
	QuotaContinuation       *QuotaMoveContinuation   `json:"quota_continuation,omitempty"`
	PreparedDestination     *PreparedMoveDestination `json:"prepared_destination,omitempty"`
	AcceptedPreparation     *MovePreparation         `json:"accepted_preparation,omitempty"`
	AcknowledgeInterruption bool                     `json:"acknowledge_interruption"`
	WorkspaceTransfer       *WorkspaceTransfer       `json:"workspace_transfer,omitempty"`
	// Handoff is Move-owned session state, never a portable workspace checkpoint.
	Handoff *CheckpointMetadata `json:"handoff,omitempty"`
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
	// destination when selection says keep. Additive shape within unshipped
	// breaking store revision 132; no new migration.
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
	panic("unported: mj-core/src/state/session_move.rs MoveOperation::queue_command_ids")
}

// CaptureContainerSettings captures the two independently owned facts at
// acceptance of each attempt.
func (o *MoveOperation) CaptureContainerSettings(source *SessionRecord) {
	panic("unported: mj-core/src/state/session_move.rs MoveOperation::capture_container_settings")
}

// IsResolved: explicit Resume resolves a cancelled Move by finishing its
// queue without ever admitting it. Its archives are then historical, just
// as for a completed Move, and cannot make a later Move a retry.
func (o *MoveOperation) IsResolved() bool {
	return o.Phase == MovePhaseCompleted ||
		(o.Phase == MovePhaseCancelled && !o.QueueAdmissionStarted && o.QueueAdmissionFinished)
}

func (o *MoveOperation) RetainsSourceEnvironment() bool {
	return o.InPlace || o.WorkspaceTransfer != nil
}

func (o *MoveOperation) RestoreArtifact() *CheckpointMetadata {
	if o.Handoff != nil {
		return o.Handoff
	}
	return o.Checkpoint
}

func (o *MoveOperation) RetainsCheckpoint() bool {
	return (!o.IsResolved() && (o.Handoff != nil || o.Phase != MovePhaseCancelled)) ||
		(o.QueueAdmissionStarted && !o.QueueAdmissionFinished)
}

// RetainedArchives is every checkpoint archive this Move keeps on disk: its
// handoff and the source's last full checkpoint. The startup archive sweep
// and the superseded-checkpoint prune both ask this one question, so
// neither can delete an archive the Move still restores from.
func (o *MoveOperation) RetainedArchives() []*CheckpointMetadata {
	if !o.RetainsCheckpoint() {
		return nil
	}
	var archives []*CheckpointMetadata
	for _, archive := range []*CheckpointMetadata{o.Handoff, o.Checkpoint} {
		if archive != nil {
			archives = append(archives, archive)
		}
	}
	return archives
}

// CheckpointRetained reports whether an unfinished Move still has the
// checkpoint a retry restores.
//
// The Move record owns this fact. When its archive is found missing, the
// reference is removed from the record, so the recovery guidance, the API
// and the retry admission all read the loss from here.
func (o *MoveOperation) CheckpointRetained() bool {
	return !o.IsResolved() && o.RestoreArtifact() != nil
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
	case MovePhasePreparing, MovePhaseClosingSource, MovePhaseResumingDestination, MovePhaseStartingQueue:
		return true
	}
	return false
}
