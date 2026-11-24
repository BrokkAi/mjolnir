// Port of mj-server/src/server/viewer_types.rs (only the types the API
// session and review views carry so far).

package server

// LaunchingState is the state a session shows while it launches.
const LaunchingState = "launching"

// ViewerBackgroundTask is one command the active agent left running in the
// background. Rust: deny_unknown_fields.
type ViewerBackgroundTask struct {
	ID          string `json:"id"`
	Command     string `json:"command"`
	StartedAtMs int64  `json:"started_at_ms"`
	CanStop     bool   `json:"can_stop"`
}

// ViewerOperationKind is the kind of a running operation (kebab-case).
type ViewerOperationKind string

const (
	ViewerOperationKindCreate  ViewerOperationKind = "create"
	ViewerOperationKindResume  ViewerOperationKind = "resume"
	ViewerOperationKindMove    ViewerOperationKind = "move"
	ViewerOperationKindSuspend ViewerOperationKind = "suspend"
	ViewerOperationKindDestroy ViewerOperationKind = "destroy"
	// ViewerOperationKindStop: a sub-agent stopped because its parent is
	// being suspended.
	ViewerOperationKindStop       ViewerOperationKind = "stop"
	ViewerOperationKindCleanup    ViewerOperationKind = "cleanup"
	ViewerOperationKindCheckpoint ViewerOperationKind = "checkpoint"
)

// ViewerOperationStage is one stage of a running operation, with the clock
// it started on. Rust: deny_unknown_fields.
type ViewerOperationStage struct {
	Label                 string `json:"label"`
	StartedAtEpochSeconds uint64 `json:"started_at_epoch_seconds"`
}

// ViewerOperation is a provision, resume, stop or checkpoint the controller
// is running now.
//
// A phone that asked for one of these got `202 Accepted` and an identifier
// rather than a result, because the work outlives the request. This is how
// it finds out what happened. Rust: deny_unknown_fields.
type ViewerOperation struct {
	ID                    string                 `json:"id"`
	SessionID             string                 `json:"session_id"`
	Kind                  ViewerOperationKind    `json:"kind"`
	StartedAtEpochSeconds uint64                 `json:"started_at_epoch_seconds"`
	Stages                []ViewerOperationStage `json:"stages,omitempty" serde:"default"`
	// Notice is controller-authored and already meant for a person to read,
	// unlike the error text this projection keeps on the controller.
	Notice      *string `json:"notice,omitempty" serde:"default"`
	Cancellable bool    `json:"cancellable"`
}

// ViewerChatPhase is what the agent is doing, mirroring
// `RelayExecutionState` (kebab-case; the zero value is not a phase, the
// default is ViewerChatPhaseIdle).
type ViewerChatPhase string

const (
	ViewerChatPhaseIdle    ViewerChatPhase = "idle"
	ViewerChatPhaseRunning ViewerChatPhase = "running"
	ViewerChatPhaseClosing ViewerChatPhase = "closing"
	ViewerChatPhaseClosed  ViewerChatPhase = "closed"
)

// ViewerConfigChoice is one value a harness setting accepts. Rust:
// deny_unknown_fields.
type ViewerConfigChoice struct {
	Value       string  `json:"value"`
	Name        string  `json:"name"`
	Description *string `json:"description,omitempty" serde:"default"`
}

// ViewerConfigOption is one setting the harness advertised, with the values
// it will accept.
//
// The browser completes `/model` and `/effort` from this rather than from a
// list of its own, so a harness that offers something new needs no viewer
// change, and a viewer can never offer a value the harness would refuse.
// Rust: deny_unknown_fields.
type ViewerConfigOption struct {
	Key     string               `json:"key"`
	Label   string               `json:"label"`
	Current *string              `json:"current,omitempty" serde:"default"`
	Choices []ViewerConfigChoice `json:"choices"`
}

// ViewerTurnReview is a turn review as a phone renders it. Rust:
// deny_unknown_fields.
type ViewerTurnReview struct {
	ChildSessionID *string `json:"child_session_id,omitempty" serde:"default"`
	// Status is what the review is doing, in one line.
	Status string `json:"status"`
	// Roles has one row per reviewing agent: its label and where it has got
	// to.
	Roles []ViewerReviewRole `json:"roles,omitempty" serde:"default"`
	// Verdict is present once the review has reached a verdict the user must
	// answer.
	Verdict *ViewerReviewVerdict `json:"verdict,omitempty" serde:"default"`
}

// ViewerReviewRole is one reviewing agent of a ViewerTurnReview. Rust:
// deny_unknown_fields.
type ViewerReviewRole struct {
	Label string `json:"label"`
	// State is `pending`, `running`, `done`, `findings`, or `failed`.
	State          string  `json:"state"`
	ChildSessionID *string `json:"child_session_id,omitempty" serde:"default"`
}

// ViewerReviewVerdict is a review's verdict. Rust: deny_unknown_fields.
type ViewerReviewVerdict struct {
	// Kind is `clean`, `findings`, or `failed`.
	Kind string `json:"kind"`
	// Text is the findings, or the failure's reason.
	Text string `json:"text"`
}
