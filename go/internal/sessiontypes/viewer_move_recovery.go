// Port of mj-session-types/src/viewer_move_recovery.rs.
//
// The public view of an unresolved Move: what the viewer and the API show
// so a person can retry, resume or destroy the session.

package sessiontypes

import (
	"bytes"
	"encoding/json"
	"fmt"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ViewerMoveRecovery is serialized with serde's deny_unknown_fields; see
// MarshalJSON and UnmarshalJSON.
type ViewerMoveRecovery struct {
	// FailureNotice is the public diagnostic owned by this Move, without the
	// raw error chain.
	FailureNotice               *string              `json:"failure_notice,omitempty"`
	Oracle                      *bool                `json:"oracle,omitempty"`
	OracleChoice                *core.SubagentChoice `json:"oracle_choice,omitempty"`
	OperationID                 string               `json:"operation_id"`
	SourceProfileID             string               `json:"source_profile_id"`
	SourceTargetTemplateID      string               `json:"source_target_template_id"`
	DestinationProfileID        string               `json:"destination_profile_id"`
	DestinationTargetTemplateID string               `json:"destination_target_template_id"`
	Phase                       string               `json:"phase"`
	Queue                       string               `json:"queue"`
	ClearResourceAllocation     bool                 `json:"clear_resource_allocation"`
	// SourceAdditionalMounts and SourceResourceAllocation: the source
	// settings are retained so Resume cannot silently inherit a partially
	// converted destination record after a failed Move.
	SourceAdditionalMounts   []core.AdditionalMount         `json:"source_additional_mounts"`
	SourceResourceAllocation core.SessionResourceAllocation `json:"source_resource_allocation"`
	// DestinationAdditionalMounts and DestinationResourceAllocation: the
	// exact destination settings are needed when a queue admission
	// checkpoint pins retry to the already-provisioned destination.
	DestinationAdditionalMounts   []core.AdditionalMount         `json:"destination_additional_mounts"`
	DestinationResourceAllocation core.SessionResourceAllocation `json:"destination_resource_allocation"`
	// CheckpointRetained: the Move still has the checkpoint a retry
	// restores, so Retry Move is possible. Read from
	// core.MoveOperation.CheckpointRetained, the fact the published recovery
	// guidance is written from.
	CheckpointRetained bool `json:"checkpoint_retained"`
	// EnvironmentRetained: the Move holds the source environment for a
	// retry, so Resume is refused: only Retry Move (when the checkpoint is
	// retained) or Destroy can act on the session.
	EnvironmentRetained    bool `json:"environment_retained"`
	DestinationReady       bool `json:"destination_ready"`
	QueueAdmissionStarted  bool `json:"queue_admission_started"`
	QueueAdmissionFinished bool `json:"queue_admission_finished"`
}

// viewerMoveRecoveryFields is every serialized field in declaration order,
// as serde's deny_unknown_fields error lists them.
var viewerMoveRecoveryFields = []string{
	"failure_notice",
	"oracle",
	"oracle_choice",
	"operation_id",
	"source_profile_id",
	"source_target_template_id",
	"destination_profile_id",
	"destination_target_template_id",
	"phase",
	"queue",
	"clear_resource_allocation",
	"source_additional_mounts",
	"source_resource_allocation",
	"destination_additional_mounts",
	"destination_resource_allocation",
	"checkpoint_retained",
	"environment_retained",
	"destination_ready",
	"queue_admission_started",
	"queue_admission_finished",
}

// viewerMoveRecoveryRequired is the fields without `#[serde(default)]`, in
// declaration order.
var viewerMoveRecoveryRequired = []string{
	"operation_id",
	"source_profile_id",
	"source_target_template_id",
	"destination_profile_id",
	"destination_target_template_id",
	"phase",
	"queue",
	"clear_resource_allocation",
	"checkpoint_retained",
	"destination_ready",
	"queue_admission_started",
	"queue_admission_finished",
}

// MarshalJSON writes the present-but-empty mount lists as `[]` and absent
// allocations as `null`, as serde does, without HTML escapes.
func (r ViewerMoveRecovery) MarshalJSON() ([]byte, error) {
	type plain ViewerMoveRecovery
	if r.SourceAdditionalMounts == nil {
		r.SourceAdditionalMounts = []core.AdditionalMount{}
	}
	if r.DestinationAdditionalMounts == nil {
		r.DestinationAdditionalMounts = []core.AdditionalMount{}
	}
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(plain(r)); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// UnmarshalJSON applies serde's deny_unknown_fields, the required fields and
// the `#[serde(default)]` ones, and decodes the allocations through core's
// tagged-union decoder.
func (r *ViewerMoveRecovery) UnmarshalJSON(data []byte) error {
	keys, err := orderedObjectKeys(data)
	if err != nil {
		return err
	}
	for i, key := range keys {
		if !slices.Contains(viewerMoveRecoveryFields, key) {
			return fmt.Errorf("unknown field `%s`, expected one of %s", key, quotedFieldList(viewerMoveRecoveryFields))
		}
		if slices.Contains(keys[:i], key) {
			return fmt.Errorf("duplicate field `%s`", key)
		}
	}
	for _, name := range viewerMoveRecoveryRequired {
		if !slices.Contains(keys, name) {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	type plain ViewerMoveRecovery
	var decoded ViewerMoveRecovery
	aux := struct {
		*plain
		SourceResourceAllocation      json.RawMessage `json:"source_resource_allocation"`
		DestinationResourceAllocation json.RawMessage `json:"destination_resource_allocation"`
	}{plain: (*plain)(&decoded)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if decoded.SourceResourceAllocation, err = decodeOptionalAllocation(aux.SourceResourceAllocation); err != nil {
		return err
	}
	if decoded.DestinationResourceAllocation, err = decodeOptionalAllocation(aux.DestinationResourceAllocation); err != nil {
		return err
	}
	*r = decoded
	return nil
}

// decodeOptionalAllocation is serde's `Option<SessionResourceAllocation>`:
// absent and `null` are None.
func decodeOptionalAllocation(raw json.RawMessage) (core.SessionResourceAllocation, error) {
	if len(raw) == 0 || string(bytes.TrimSpace(raw)) == "null" {
		return nil, nil
	}
	return core.UnmarshalSessionResourceAllocationJSON(raw)
}

// orderedObjectKeys is a JSON object's member names in document order, so an
// unknown-field error names the first one as serde does.
func orderedObjectKeys(data []byte) ([]string, error) {
	decoder := json.NewDecoder(bytes.NewReader(data))
	token, err := decoder.Token()
	if err != nil {
		return nil, err
	}
	if delim, ok := token.(json.Delim); !ok || delim != '{' {
		return nil, fmt.Errorf("invalid type: expected struct ViewerMoveRecovery")
	}
	var keys []string
	for decoder.More() {
		token, err := decoder.Token()
		if err != nil {
			return nil, err
		}
		keys = append(keys, token.(string))
		var skip json.RawMessage
		if err := decoder.Decode(&skip); err != nil {
			return nil, err
		}
	}
	return keys, nil
}

// quotedFieldList is serde's `OneOf` list: "`a`, `b`, `c`".
func quotedFieldList(names []string) string {
	quoted := make([]string, len(names))
	for i, name := range names {
		quoted[i] = "`" + name + "`"
	}
	return strings.Join(quoted, ", ")
}

// WithPublicFailure joins only this Move's current public diagnostic.
// Resume and a new turn retire that diagnostic; a later lifecycle error must
// not replace it.
func (r ViewerMoveRecovery) WithPublicFailure(publicError *string) ViewerMoveRecovery {
	r.FailureNotice = nil
	if publicError != nil {
		message := *publicError
		if (r.Phase == "failed" || r.Phase == "cancelled") &&
			strings.HasPrefix(message, core.MoveFailurePrefix) &&
			strings.HasSuffix(message, "reference "+r.OperationID) {
			r.FailureNotice = &message
		}
	}
	return r
}

// ViewerMoveRecoveryFromOperation is Rust's `ViewerMoveRecovery::from_operation`:
// nil for a resolved Move.
func ViewerMoveRecoveryFromOperation(operation *core.MoveOperation) *ViewerMoveRecovery {
	if operation.IsResolved() {
		return nil
	}
	selection := &operation.Selection
	destinationProfileID := ""
	if selection.ProfileID != nil {
		destinationProfileID = *selection.ProfileID
	}
	destinationTargetTemplateID := ""
	if selection.TargetTemplateID != nil {
		destinationTargetTemplateID = *selection.TargetTemplateID
	}
	var destinationAdditionalMounts []core.AdditionalMount
	if selection.AdditionalMounts != nil {
		destinationAdditionalMounts = slices.Clone(*selection.AdditionalMounts)
	} else {
		destinationAdditionalMounts = slices.Clone(operation.InheritedAdditionalMounts)
	}
	var destinationResourceAllocation core.SessionResourceAllocation
	if !selection.ClearResourceAllocation {
		destinationResourceAllocation = selection.ResourceAllocation
		if destinationResourceAllocation == nil {
			destinationResourceAllocation = operation.InheritedResourceAllocation
		}
	}
	return &ViewerMoveRecovery{
		FailureNotice:                 nil,
		Oracle:                        cloneBoolPointer(selection.Oracle),
		OracleChoice:                  cloneSubagentChoicePointer(selection.OracleChoice),
		OperationID:                   operation.OperationID,
		SourceProfileID:               operation.SourceProfileID,
		SourceTargetTemplateID:        operation.SourceTargetTemplateID,
		DestinationProfileID:          destinationProfileID,
		DestinationTargetTemplateID:   destinationTargetTemplateID,
		Phase:                         viewerMovePhase(operation.Phase),
		Queue:                         viewerMoveQueue(operation.Queue),
		ClearResourceAllocation:       selection.ClearResourceAllocation,
		SourceAdditionalMounts:        slices.Clone(operation.SourceAdditionalMounts),
		SourceResourceAllocation:      operation.SourceResourceAllocation,
		DestinationAdditionalMounts:   destinationAdditionalMounts,
		DestinationResourceAllocation: destinationResourceAllocation,
		CheckpointRetained:            operation.CheckpointRetained(),
		EnvironmentRetained:           operation.HoldsSourceEnvironment(),
		DestinationReady: operation.DestinationTarget != nil &&
			operation.DestinationNativeSessionID != nil,
		QueueAdmissionStarted:  operation.QueueAdmissionStarted,
		QueueAdmissionFinished: operation.QueueAdmissionFinished,
	}
}

// viewerMovePhase is the public name of each Move phase, spelled out as
// Rust's exhaustive match does. Core rejects any other value when it
// decodes a phase, so the last line only names a value built in memory.
func viewerMovePhase(phase core.MovePhase) string {
	switch phase {
	case core.MovePhaseCapturing:
		return "capturing"
	case core.MovePhaseTransferring:
		return "transferring"
	case core.MovePhaseRestoring:
		return "restoring"
	case core.MovePhaseDestinationStarting:
		return "destination_starting"
	case core.MovePhaseDestinationOwns:
		return "destination_owns"
	case core.MovePhaseSourceRetired:
		return "source_retired"
	case core.MovePhaseQueueAdmitted:
		return "queue_admitted"
	case core.MovePhaseDispatchReleased:
		return "dispatch_released"
	case core.MovePhaseCompleted:
		return "completed"
	case core.MovePhaseFailed:
		return "failed"
	case core.MovePhaseCancelled:
		return "cancelled"
	}
	return string(phase)
}

// viewerMoveQueue is the public name of a Move's queue disposition, as
// Rust's exhaustive match spells it.
func viewerMoveQueue(queue core.ResumeQueueDisposition) string {
	switch queue {
	case core.ResumeQueueDispositionStart:
		return "start"
	case core.ResumeQueueDispositionDiscard:
		return "discard"
	}
	return string(queue)
}

// cloneBoolPointer is Rust's `Option<bool>::clone`.
func cloneBoolPointer(value *bool) *bool {
	if value == nil {
		return nil
	}
	copied := *value
	return &copied
}

// cloneSubagentChoicePointer is Rust's `Option<SubagentChoice>::clone`.
func cloneSubagentChoicePointer(choice *core.SubagentChoice) *core.SubagentChoice {
	if choice == nil {
		return nil
	}
	copied := *choice
	if choice.Effort != nil {
		effort := *choice.Effort
		copied.Effort = &effort
	}
	return &copied
}
