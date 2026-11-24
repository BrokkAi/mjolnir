// Port of mj-server/src/server/api/types.rs (the request and response types
// so far; the handler-side helpers arrive with the Go daemon).

package server

import (
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/sessionruntime"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// MailboxEventRequest is an externally produced item waiting in a session's
// durable mailbox. Rust: deny_unknown_fields.
type MailboxEventRequest struct {
	// Key is the idempotency key chosen by the producer.
	Key string `json:"key"`
	// Text is untrusted event text shown to the session.
	Text string `json:"text"`
	Wake bool   `json:"wake" serde:"default"`
}

// MailboxEventResponse answers a MailboxEventRequest. Rust:
// deny_unknown_fields.
type MailboxEventResponse struct {
	Key string `json:"key"`
	// Inserted is false when this exact event was already accepted into the
	// outbox.
	Inserted bool `json:"inserted"`
}

// SessionMessageResponse is the result of delivering one message to a
// session: through its mailbox, as a queued prompt for an older worker, or,
// for a message from the user, as the user's prompt (`via: "turn"`). Rust:
// deny_unknown_fields.
type SessionMessageResponse struct {
	SessionID string `json:"session_id"`
	Via       string `json:"via"`
	// MailboxDelivery is worker-reported delivery timing for mailbox
	// messages. Omitted when the live worker has not published this
	// capability.
	MailboxDelivery *core.MailboxDeliveryMode `json:"mailbox_delivery,omitempty" serde:"default"`
	// Note is a human-readable explanation when delivery waits for the
	// recipient turn.
	Note   *string `json:"note,omitempty" serde:"default"`
	TurnID *uint64 `json:"turn_id,omitempty" serde:"default"`
	// ManagedChild is an internal routing detail used to preserve the
	// shipped child-tool result shape. It is not part of the HTTP response
	// contract (serde skip).
	ManagedChild bool `json:"-"`
}

// SessionMessageRequest is a message sent by the CLI or another
// authenticated API client. Rust: deny_unknown_fields.
type SessionMessageRequest struct {
	// RequestID is a stable identity across retries after an uncertain
	// response. For a message from the user it is the prompt's command ID.
	RequestID string `json:"request_id"`
	Text      string `json:"text"`
	// SenderSessionID is the session asserted by the local caller, when this
	// process runs inside a session worker. The daemon verifies that it
	// exists and is top-level. Without it the message is from the user and
	// is submitted as the user's prompt.
	SenderSessionID *string `json:"sender_session_id,omitempty" serde:"default"`
}

// GithubTokenResponse is a currently valid installation access token.
// Callers must keep it out of logs and persistent session state. Rust:
// deny_unknown_fields.
type GithubTokenResponse struct {
	Token string `json:"token"`
}

// ApiSession is one session as the API presents it. This is a narrower,
// more stable shape than the viewer's own session projection, which changes
// whenever the browser needs something new.
//
// PendingElicitations is written through v1_elicitations (the call ID of a
// permission form removed): an encoder still to port.
type ApiSession struct {
	Attention core.SessionAttention `json:"attention" serde:"default"`
	// BackgroundTasks are the tasks currently observed by the daemon,
	// including their stop capability.
	BackgroundTasks []ViewerBackgroundTask `json:"background_tasks" serde:"default"`
	Assessment      *core.Summary          `json:"assessment,omitempty" serde:"default"`
	Subagents       core.SubagentPolicy    `json:"subagents" serde:"default"`
	// At is the commit the workspace started checked out at, when one was
	// named.
	At *string `json:"at,omitempty" serde:"default"`
	// Branch is the branch created at At, or the existing branch checked out
	// without it.
	Branch *string `json:"branch,omitempty" serde:"default"`
	// Base is the diff base the session was started with; At unless another
	// was named.
	Base *string `json:"base,omitempty" serde:"default"`
	// NoLaterHistory is whether the workspace started with At, Base and their
	// ancestors only, as StartSessionRequest.NoLaterHistory asked.
	NoLaterHistory bool                                   `json:"no_later_history,omitempty" serde:"default"`
	BackgroundWork *sessionruntime.ApiBackgroundWork      `json:"background_work,omitempty" serde:"default"`
	ID             string                                 `json:"id"`
	WorkspaceID    string                                 `json:"workspace_id"`
	Title          string                                 `json:"title"`
	HarnessKind    string                                 `json:"harness_kind"`
	ProfileID      string                                 `json:"profile_id"`
	TargetID       string                                 `json:"target_id"`
	BundleID       string                                 `json:"bundle_id"`
	State          string                                 `json:"state"`
	Lifecycle      sessionruntime.ViewerLifecycleCategory `json:"lifecycle"`
	ChatPhase      ViewerChatPhase                        `json:"chat_phase"`
	IsIdle         bool                                   `json:"is_idle"`
	// ActivityState is what this session is doing, in more detail than
	// ChatPhase's four values allow: in particular it can say that the
	// daemon cannot see the worker and report what it last knew, rather than
	// claiming idleness it cannot prove.
	ActivityState *core.ActivityState `json:"activity_state,omitempty" serde:"default"`
	// Operation is the active lifecycle operation and its current stages, on
	// session detail.
	Operation *ViewerOperation `json:"operation,omitempty" serde:"default"`
	// Relay is the daemon's worker connection, on session detail when
	// observed.
	Relay    *RelayHealth `json:"relay,omitempty" serde:"default"`
	HasError bool         `json:"has_error"`
	// Error is a launch failure or a safe lifecycle failure, including a
	// failed Move. Raw runtime error text is deliberately not published for
	// a running session. Why a turn failed travels in LastTurnDiagnostic,
	// which a single-session query fills.
	Error *string `json:"error,omitempty" serde:"default"`
	// StorageProblem is why this session's target cannot take writes: its
	// disk is full.
	StorageProblem *string `json:"storage_problem,omitempty" serde:"default"`
	// ProfileLoginError is the profile login failure reported by the daemon,
	// when available.
	ProfileLoginError *string `json:"profile_login_error,omitempty" serde:"default"`
	CreatedAt         string  `json:"created_at"`
	UpdatedAt         string  `json:"updated_at"`
	// LastTurnOutcome is how the last finished prompt ended. Absent unless
	// the caller asked for one session by id or waited on it, because the
	// dashboard projection the list is built from does not carry turn
	// identity.
	LastTurnOutcome     *core.MaterializedTurnOutcome `json:"last_turn_outcome,omitempty" serde:"default"`
	LastTurnDiagnostic  *core.TurnDiagnostic          `json:"last_turn_diagnostic,omitempty" serde:"default"`
	ConfigOptions       []ViewerConfigOption          `json:"config_options" serde:"default"`
	TurnControls        *core.TurnControls            `json:"turn_controls,omitempty" serde:"default"`
	PendingElicitations []core.ElicitationRequest     `json:"pending_elicitations,omitempty" serde:"default"`
}

// StopBackgroundTaskRequest is the body of
// `POST /sessions/{id}/background-tasks/stop`. Rust: deny_unknown_fields.
type StopBackgroundTaskRequest struct {
	BackgroundTaskID string `json:"background_task_id"`
}

// SessionListResponse answers `GET /sessions`.
type SessionListResponse struct {
	Sessions []ApiSession `json:"sessions"`
}

// WorkspaceListResponse is the workspaces the daemon holds, newest opening
// first, exactly as the terminal's workspace tabs and the viewer's list see
// them.
type WorkspaceListResponse struct {
	Workspaces []core.WorkspaceRecord `json:"workspaces"`
}

// CreateWorkspaceRequest names the workspace to work in. The name is the
// identity: it is trimmed, at most 64 characters, and unique
// case-insensitively, so naming one that already exists returns it rather
// than making a second. Rust: deny_unknown_fields.
type CreateWorkspaceRequest struct {
	Name string `json:"name"`
}

// CreateWorkspaceResponse answers a CreateWorkspaceRequest.
type CreateWorkspaceResponse struct {
	Workspace core.WorkspaceRecord `json:"workspace"`
}

// StartSessionRequest creates a session and, optionally, sends its first
// prompt.
//
// ProfileID and TargetID may be omitted, and each resolves independently: a
// caller may name a profile and take the saved default target. When Model
// is supplied without ProfileID, the daemon considers all configured,
// usable profiles and ranks them by quota, using the saved default profile
// as the tie-breaking anchor. An omitted identifier otherwise comes from the
// pair the user last saved with the `mj go` workflow, which the first setup
// also becomes, so a caller that has never read `config.toml` can create a
// session by naming neither. The controller still receives two explicit
// identifiers, because a session whose profile was implicit would be a
// session nobody can explain later. Rust: deny_unknown_fields.
//
// Review (a core.SessionReview, nil when omitted) needs an encoder and the
// daemon's `deserialize_api_session_review` decoder still to port.
type StartSessionRequest struct {
	// RememberContainerSize saves this session's container size as the
	// default for its physical host. False preserves the existing saved size
	// or the baseline.
	RememberContainerSize bool  `json:"remember_container_size,omitempty" serde:"default"`
	CreateManagedWorktree *bool `json:"create_managed_worktree" serde:"default"`
	// At is the full commit object ID to start the workspace at: an exact
	// checkout of the bundle's primary repository, detached unless Branch is
	// given.
	At *string `json:"at,omitempty" serde:"default"`
	// Branch is, with At, the new branch created there; otherwise the
	// existing branch to check out in a new isolated workspace.
	Branch *string `json:"branch,omitempty" serde:"default"`
	// Base is the diff base, when it is not At. Without At, also the
	// starting revision for a raw managed worktree.
	Base *string `json:"base,omitempty" serde:"default"`
	// NoLaterHistory starts the workspace, with At, with At, Base and their
	// ancestors only: no `origin` remote, remote-tracking refs, tags, or
	// clone-cache objects. Base must then be a full commit object ID, and
	// the bundle must hold one repository. The session cannot be resumed or
	// moved.
	NoLaterHistory bool `json:"no_later_history,omitempty" serde:"default"`
	// Subagents omitted tries Mjolnir and may fall back to the selected
	// harness's default.
	Subagents    *core.SubagentPolicy `json:"subagents" serde:"default"`
	Oracle       *bool                `json:"oracle,omitempty" serde:"default"`
	OracleChoice *core.SubagentChoice `json:"oracle_choice,omitempty" serde:"default"`
	// Review for this session: `off`, `push` or `inherit`. Omitted (nil)
	// follows `[review]`.
	Review core.SessionReview `json:"review,omitempty" serde:"default"`
	// NoProjectMemory starts the session with no project memory: nothing
	// from the workspace's project memory is injected and the harness is
	// offered no memory directory. Omitted or false keeps the workspace's
	// project memory.
	NoProjectMemory bool `json:"no_project_memory,omitempty" serde:"default"`
	// NoGithubEvents excludes this session and its children from GitHub
	// watcher delivery targets.
	NoGithubEvents bool `json:"no_github_events,omitempty" serde:"default"`
	// NoGithubCredentials withholds GitHub credentials; inherited by
	// children.
	NoGithubCredentials bool `json:"no_github_credentials,omitempty" serde:"default"`
	// NotificationsIgnored starts the session with notifications ignored, as
	// if `Ignore Session` had been chosen at creation.
	NotificationsIgnored bool    `json:"notifications_ignored,omitempty" serde:"default"`
	WorkspaceID          *string `json:"workspace_id" serde:"default"`
	// ProfileID omitted follows the saved default, or anchors model-based
	// profile selection when Model is supplied.
	ProfileID *string `json:"profile_id" serde:"default"`
	// TargetID omitted follows the saved default. See the type's own
	// documentation.
	TargetID *string `json:"target_id" serde:"default"`
	BundleID *string `json:"bundle_id" serde:"default"`
	// ProjectDirectory is a directory on the bare target, or a checkout on
	// the controller host from which to discover a bundle for an isolated
	// target.
	ProjectDirectory *string `json:"project_directory" serde:"default"`
	Title            *string `json:"title" serde:"default"`
	Model            *string `json:"model" serde:"default"`
	Effort           *string `json:"effort" serde:"default"`
	Prompt           *string `json:"prompt" serde:"default"`
	// Cpus is the container CPU limit, at least 2. Omitted takes the
	// target's default size, the one the viewer's create form selects.
	// Container targets only.
	Cpus *uint64 `json:"cpus,omitempty" serde:"default"`
	// MemoryBytes is the container memory limit in bytes, at least 4 GiB,
	// defaulted like Cpus.
	MemoryBytes *uint64 `json:"memory_bytes,omitempty" serde:"default"`
}

// ResumeSessionRequest resumes a stopped, lost, or failed session. Every
// field is optional: the session's own record supplies what the caller does
// not name, which is what makes `POST .../resume` with no body the
// scriptable "continue this session" call. Rust: deny_unknown_fields.
type ResumeSessionRequest struct {
	// ProfileID is the profile to resume on. Defaults to the one the session
	// last ran.
	ProfileID *string `json:"profile_id" serde:"default"`
	// TargetID is the target template to provision. Defaults to the
	// session's own.
	TargetID *string `json:"target_id" serde:"default"`
	// WorkspaceID is the workspace the resumed session belongs to. Defaults
	// to its own.
	WorkspaceID *string `json:"workspace_id" serde:"default"`
	// Queue is whether prompts queued when the session stopped are started
	// or discarded. Defaults to `start`, which is what the terminal's own
	// resume wizard defaults to.
	Queue *core.ResumeQueueDisposition `json:"queue" serde:"default"`
}

// ResumeSessionResponse is what a resume was accepted as: the settings it
// will actually use, resolved from the request and the session's record.
type ResumeSessionResponse struct {
	SessionID   string `json:"session_id"`
	WorkspaceID string `json:"workspace_id"`
	ProfileID   string `json:"profile_id"`
	TargetID    string `json:"target_id"`
}

// SuspendSessionResponse is what a suspend was accepted as. A suspend stops
// the session's active Mjolnir sub-agents without a checkpoint and suspends
// the session alone.
type SuspendSessionResponse struct {
	SessionID string `json:"session_id" serde:"default"`
	// StoppedSubagents is how many active sub-agents the suspend stops.
	StoppedSubagents uint64 `json:"stopped_subagents" serde:"default"`
	// SubagentsNotHandedBack is how many of those have not handed back their
	// report.
	SubagentsNotHandedBack uint64 `json:"subagents_not_handed_back" serde:"default"`
	// Warning is "N sub-agents have not handed back; suspending stops
	// them", when any have not.
	Warning *string `json:"warning,omitempty" serde:"default"`
}

// StartSessionResponse answers a StartSessionRequest.
type StartSessionResponse struct {
	SessionID string `json:"session_id"`
	// TurnID is the turn the follow-up prompt was accepted as, once it has
	// been submitted. Creation answers before that, so it is usually absent.
	TurnID *uint64 `json:"turn_id,omitempty" serde:"default"`
	// Notice is something to tell the person about the session as created,
	// such as review being turned off because its folder has no Git
	// commits.
	Notice *string `json:"notice,omitempty" serde:"default"`
}

// PromptRequest is the body of `POST /sessions/{id}/prompt`. Rust:
// deny_unknown_fields.
type PromptRequest struct {
	Text string `json:"text"`
	// CommandID is the caller-chosen relay command identity for recovery
	// after a lost reply.
	CommandID *string `json:"command_id,omitempty" serde:"default"`
}

// ClearQueueRequest is the body of
// `POST /sessions/{id}/queued-prompts/clear`. Rust: deny_unknown_fields.
type ClearQueueRequest struct {
	CommandID *string `json:"command_id,omitempty" serde:"default"`
}

// PromptResponse answers a PromptRequest.
type PromptResponse struct {
	// TurnID is the relay acceptance ordinal for this prompt, which is what
	// `wait` takes as `turn_id`.
	TurnID uint64 `json:"turn_id"`
}

// WaitCapacityRetry is a worker-owned server retry a wait reports.
type WaitCapacityRetry struct {
	Attempt   uint32 `json:"attempt"`
	RetryAtMs int64  `json:"retry_at_ms"`
}

// WaitCapacityRetryFrom is Rust's `From<&CapacityRetry>`.
func WaitCapacityRetryFrom(retry *core.CapacityRetry) WaitCapacityRetry {
	return WaitCapacityRetry{Attempt: retry.Attempt, RetryAtMs: retry.RetryAtMs}
}

// RelayState is how the daemon's live view of a session's relay is doing
// (snake_case).
//
// This reports; it never decides an outcome. A caller that gets `timeout`
// needs to tell "the turn is still working" from "the daemon cannot see the
// worker at all", and those look identical without it.
type RelayState string

const (
	// RelayStateConnected: the daemon is attached to the worker and
	// following its events.
	RelayStateConnected RelayState = "connected"
	// RelayStateDisconnected: not attached, with no error recorded yet:
	// attaching, or between tries.
	RelayStateDisconnected RelayState = "disconnected"
	// RelayStateUnreachable: the worker could not be reached.
	RelayStateUnreachable RelayState = "unreachable"
	// RelayStateTargetMissing: the session's target is gone.
	RelayStateTargetMissing RelayState = "target_missing"
	// RelayStateProjectionIntegrity: the event stream did not line up with
	// what the daemon had projected.
	RelayStateProjectionIntegrity RelayState = "projection_integrity"
)

// RelayStateOf is the state of a view: a recorded error outranks the
// connection flag.
func RelayStateOf(connected bool, viewError *client.ViewError) RelayState {
	if viewError != nil {
		switch viewError.Kind {
		case client.ViewErrorKindUnreachable:
			return RelayStateUnreachable
		case client.ViewErrorKindTargetMissing:
			return RelayStateTargetMissing
		case client.ViewErrorKindProjectionIntegrity:
			return RelayStateProjectionIntegrity
		}
	}
	if connected {
		return RelayStateConnected
	}
	return RelayStateDisconnected
}

// RelayHealth is the health of the daemon's live view of a session.
type RelayHealth struct {
	State RelayState `json:"state"`
	// Detail is the view's own description of the problem, when it recorded
	// one.
	Detail *string `json:"detail,omitempty" serde:"default"`
}

// RelayHealthFrom is Rust's `From<&ManagedSessionView>`.
func RelayHealthFrom(view *client.ManagedSessionView) RelayHealth {
	state := RelayStateOf(view.Connected, view.Error)
	var detail *string
	switch {
	case view.Error != nil:
		text := view.Error.Detail()
		detail = &text
	case view.Connected && view.Snapshot != nil && view.Snapshot.SubagentQueueError != nil:
		text := *view.Snapshot.SubagentQueueError
		detail = &text
	}
	return RelayHealth{State: state, Detail: detail}
}

// WaitResponse answers `POST /sessions/{id}/wait`.
//
// PendingElicitations is written through v1_elicitations, as on
// ApiSession: an encoder still to port.
type WaitResponse struct {
	Diagnostic          *core.TurnDiagnostic       `json:"diagnostic,omitempty" serde:"default"`
	PendingElicitations []core.ElicitationRequest  `json:"pending_elicitations,omitempty" serde:"default"`
	Usage               *core.TokenUsage           `json:"usage,omitempty" serde:"default"`
	Outcome             sessionruntime.WaitOutcome `json:"outcome"`
	// StopReason is the harness's own stop reason, when the turn reached
	// one.
	StopReason *string `json:"stop_reason,omitempty" serde:"default"`
	// Message is why the wait ended this way, when there is something to
	// say.
	Message *string `json:"message,omitempty" serde:"default"`
	// FinalMessage is the agent's last message of the turn, flattened to
	// text. For a sub-agent child it is the report the child handed back,
	// when it did.
	FinalMessage *string `json:"final_message,omitempty" serde:"default"`
	// ReportSource is, for a sub-agent child, `handback` when FinalMessage
	// is the report the child handed back, `last_message` when it is the
	// turn's last message. Absent for every other session.
	ReportSource *string `json:"report_source,omitempty" serde:"default"`
	// TurnID is the turn this answer is about: the latest turn that ended at
	// or after the requested one, since a session keeps only its latest
	// turn's outcome.
	TurnID *uint64 `json:"turn_id,omitempty" serde:"default"`
	// RequestedTurnID is the turn the request named, so a caller can see
	// when TurnID is a later one.
	RequestedTurnID *uint64 `json:"requested_turn_id,omitempty" serde:"default"`
	// Next is what the session is doing as the wait answers, as the worker's
	// activity state, when that is not idle. A wait on a turn answers when
	// that turn ends, so this is where a turn the harness started itself,
	// background work or an expected continuation shows. Absent when the
	// session is idle. A top-level field because older v1 clients reject
	// unknown fields inside the shared turn type.
	Next *core.ActivityState `json:"next,omitempty" serde:"default"`
	// TurnNumber is the one-based position of this turn in the
	// conversation.
	TurnNumber *uint64 `json:"turn_number,omitempty" serde:"default"`
	ElapsedMs  *int64  `json:"elapsed_ms,omitempty" serde:"default"`
	// ToolCalls is how many tool calls the turn made. Absent when the wait
	// ended without a finished turn.
	ToolCalls *uint64 `json:"tool_calls,omitempty" serde:"default"`
	// CapacityRetry is the legacy alias for a worker-owned server retry,
	// retained for older clients.
	CapacityRetry          *WaitCapacityRetry  `json:"capacity_retry,omitempty" serde:"default"`
	ServerRetry            *WaitCapacityRetry  `json:"server_retry,omitempty" serde:"default"`
	RetryAssessmentPending bool                `json:"retry_assessment_pending" serde:"default"`
	QuotaRecovery          *core.QuotaRecovery `json:"quota_recovery,omitempty" serde:"default"`
	// Relay is the health of the daemon's live view of this session. Absent
	// when no live actor holds the session, because there is then no view to
	// report on and inventing one would be worse than saying nothing.
	Relay   *RelayHealth `json:"relay,omitempty" serde:"default"`
	Session ApiSession   `json:"session"`
}

// TranscriptItemView is one transcript item `GET /sessions/{id}/transcript`
// answers.
type TranscriptItemView struct {
	StableID          string `json:"stable_id"`
	ToolDetailTrimmed bool   `json:"tool_detail_trimmed" serde:"default"`
	Position          uint64 `json:"position"`
	// Seq is what to pass as the next `after_seq`. It is the position for
	// everything but an agent message, which carries the ordinal of its
	// latest content.
	Seq  uint64 `json:"seq"`
	Role string `json:"role"`
	// Text is the item flattened to text, which is what a reading caller
	// wants.
	Text            string `json:"text"`
	CreatedAtMs     int64  `json:"created_at_ms"`
	LastChangedAtMs int64  `json:"last_changed_at_ms"`
	// Body is the stored body, for a caller that needs the structure behind
	// the text.
	Body core.TranscriptBody `json:"body"`
}

// TranscriptResponse is a page of a session's transcript.
type TranscriptResponse struct {
	NextAfterSeq uint64 `json:"next_after_seq" serde:"default"`
	SessionID    string `json:"session_id"`
	// LatestSeq is the newest sequence in the whole transcript. A page whose
	// last item reaches this is up to date.
	LatestSeq uint64                          `json:"latest_seq"`
	Execution core.MaterializedExecutionState `json:"execution"`
	Items     []TranscriptItemView            `json:"items"`
}

// PushedBranch is a branch the daemon pushed on the caller's behalf.
type PushedBranch struct {
	Branch string `json:"branch"`
	Remote string `json:"remote"`
}

// ExportKind is what form the caller wants the session's work in
// (snake_case).
type ExportKind string

const (
	// ExportKindPatch is a unified diff, as `GET /diff` returns.
	ExportKindPatch ExportKind = "patch"
	// ExportKindBranch is a branch pushed to the repository's push remote.
	ExportKindBranch ExportKind = "branch"
	// ExportKindBundle is the git bundle of the session's committed work.
	ExportKindBundle ExportKind = "bundle"
)

// ExportRequest is the body of `POST /sessions/{id}/export`.
type ExportRequest struct {
	Kind ExportKind `json:"kind"`
	// Branch is the branch to push. Required when Kind is `branch`.
	Branch *string `json:"branch,omitempty" serde:"default"`
}

// StartReviewResponse is what `POST /sessions/{id}/review` was accepted as:
// the review of the turn the session just finished has started.
type StartReviewResponse struct {
	SessionID string `json:"session_id"`
	Started   bool   `json:"started"`
}

// ReviewStatusResponse is the turn review a session has open, as
// `GET /sessions/{id}/review` reports it: the same view a phone renders, or
// nil when no review is open (none was asked for, or the last one has
// closed).
type ReviewStatusResponse struct {
	Trigger   core.ReviewTrigger `json:"trigger"`
	SessionID string             `json:"session_id"`
	Review    *ViewerTurnReview  `json:"review" serde:"default"`
}

// FinishedReviewResponse is a finished review's per-reviewer output,
// readable after the reviewer sessions have been destroyed.
type FinishedReviewResponse struct {
	SessionID string                `json:"session_id"`
	Review    *store.FinishedReview `json:"review" serde:"default"`
}

// WikiRestoreBody is the fields of a start request a restore needs. The
// archived session decides the rest: its title, and the project it ran in
// when the caller names none. Rust: deny_unknown_fields.
type WikiRestoreBody struct {
	WorkspaceID      *string `json:"workspace_id" serde:"default"`
	ProfileID        string  `json:"profile_id"`
	TargetID         string  `json:"target_id"`
	ProjectDirectory *string `json:"project_directory" serde:"default"`
	Model            *string `json:"model" serde:"default"`
	Effort           *string `json:"effort" serde:"default"`
}

// LaunchOptions is what a caller may choose when it starts a session, and
// which pair to use when it chooses nothing.
//
// Served by `GET /api/v1/options` so a caller that has never read
// `config.toml` can enumerate the profiles and targets this daemon knows,
// learn whether each host answered its last check, and read the remembered
// default. It is the public projection in viewer_types, narrowed to what a
// launch decision needs: no harness home, no SSH host or key, no container
// environment, no AWS detail, and no controller-side path. Rust:
// deny_unknown_fields.
type LaunchOptions struct {
	// Revision is the projection revision these lists came from. Two reads
	// carrying the same revision describe the same configuration, so a
	// caller can build a form without mixing halves of two different worlds.
	Revision          uint64          `json:"revision"`
	PrimaryConfigured bool            `json:"primary_configured" serde:"default"`
	Profiles          []LaunchProfile `json:"profiles"`
	Targets           []LaunchTarget  `json:"targets"`
	Bundles           []LaunchBundle  `json:"bundles"`
	// Hosts has one entry per host that has published a capacity reading.
	// The label is how a person names the host; it is never a locator or an
	// address.
	Hosts []LaunchHost `json:"hosts,omitempty" serde:"default"`
	// Default is the pair the user last saved as their default, which the
	// first setup also becomes. Absent when nothing has ever been saved, and
	// never an error: a caller that cannot read a preference still has the
	// lists.
	Default *LaunchDefault `json:"default,omitempty" serde:"default"`
}

// LaunchProfile is one account this daemon can run work under. Rust:
// deny_unknown_fields.
type LaunchProfile struct {
	ID string `json:"id"`
	// Harness is the harness kind, such as `codex` or `claude`. Which account
	// it is, and where its credentials live, stay on the controller.
	Harness string `json:"harness"`
}

// LaunchTarget is one runtime template a session can run on. Rust:
// deny_unknown_fields.
type LaunchTarget struct {
	ID string `json:"id"`
	// Kind is the runtime kind, such as `local-bare` or `ssh-podman`.
	Kind string `json:"kind"`
	// RequiresProjectDirectory is whether this target needs an existing
	// project directory on its machine instead of provisioning repositories
	// into a workspace.
	RequiresProjectDirectory bool               `json:"requires_project_directory"`
	Availability             LaunchAvailability `json:"availability"`
	// UnavailableReason is the daemon's launch-availability reason.
	UnavailableReason *string `json:"unavailable_reason,omitempty" serde:"default"`
	// RuntimeMissing is whether the target's runtime (Docker, Podman) is not
	// installed on the daemon's host. That is permanent for the host, unlike
	// a host that did not answer its last check: a picker leaves such a
	// target out, and a request that names it is refused with
	// UnavailableReason.
	RuntimeMissing bool `json:"runtime_missing,omitempty" serde:"default"`
	// DefaultCandidate is whether the target is a default candidate Mjolnir
	// supplies, not one the user wrote in `config.toml`. With RuntimeMissing
	// it means the user has no such runtime and never asked for the target,
	// so a client should not list it.
	DefaultCandidate bool `json:"default_candidate,omitempty" serde:"default"`
	// Host is how a person names the host, when a reading covers this
	// target.
	Host *string `json:"host,omitempty" serde:"default"`
}

// LaunchAvailability is the launch verdict for a configured profile or
// target (lowercase).
type LaunchAvailability string

const (
	// LaunchAvailabilityReady: a check found the entry usable.
	LaunchAvailabilityReady LaunchAvailability = "ready"
	// LaunchAvailabilityStale: historical host-capacity status accepted when
	// reading older replies.
	LaunchAvailabilityStale LaunchAvailability = "stale"
	// LaunchAvailabilityUnavailable: a check found the entry missing or its
	// engine is absent.
	LaunchAvailabilityUnavailable LaunchAvailability = "unavailable"
	// LaunchAvailabilityUnknown: the current configuration has no completed
	// check. Offer the entry.
	LaunchAvailabilityUnknown LaunchAvailability = "unknown"
)

// LaunchBundle is one repository set a managed target can provision. Rust:
// deny_unknown_fields.
type LaunchBundle struct {
	ID                string             `json:"id"`
	PrimaryRepository string             `json:"primary_repository"`
	Repositories      []LaunchRepository `json:"repositories"`
}

// LaunchRepository is one repository of a LaunchBundle. Rust:
// deny_unknown_fields.
type LaunchRepository struct {
	ID string `json:"id"`
	// Github is the Git URL or GitHub shorthand, when this repository has
	// one. A repository sourced from a local directory publishes nothing
	// here, because that source is a path on the controller.
	Github      *string `json:"github,omitempty" serde:"default"`
	Destination string  `json:"destination"`
}

// LaunchHost is one host or fleet, as much as a caller needs to explain an
// unavailable target. The probe's own error text is deliberately absent.
// Rust: deny_unknown_fields.
type LaunchHost struct {
	ID    string `json:"id"`
	Label string `json:"label"`
	// Targets are the targets this reading covers.
	Targets    []string `json:"targets"`
	Stale      bool     `json:"stale"`
	Refreshing bool     `json:"refreshing"`
	HasError   bool     `json:"has_error"`
}

// ResolveLaunchChoices decides the profile and target of a new session from
// what the caller named, the saved default, and what is configured. The
// daemon's start route and `mj new` both call it, so a caller that is told
// what to name is told by the rule that would have chosen for it.
//
// A choice left unnamed takes the saved default, else the only candidate.
// Every choice that remains open is reported together (open, non-nil), each
// with its candidates, so the caller can fix them in one step. needProfile
// is false for a model-first request, where the profile is only a quota
// tie-break and the saved profile, when there is one, is returned as it
// stands.
func ResolveLaunchChoices(
	profile, target *string,
	needProfile bool,
	profiles, targets []string,
	saved *LaunchDefault,
) (profileID *string, targetID string, open []string) {
	choose := func(field string, named, saved *string, candidates []string) *string {
		if named != nil {
			value := *named
			return &value
		}
		if saved != nil {
			value := *saved
			return &value
		}
		if len(candidates) == 1 {
			only := candidates[0]
			return &only
		}
		if len(candidates) == 0 {
			open = append(open, "a "+field+" (none is configured)")
		} else {
			open = append(open, "a "+field+" (one of: "+strings.Join(candidates, ", ")+")")
		}
		return nil
	}
	var savedProfile, savedTarget *string
	if saved != nil {
		savedProfile, savedTarget = &saved.ProfileID, &saved.TargetID
	}
	if needProfile {
		profileID = choose("profile_id", profile, savedProfile, profiles)
	} else if profile != nil {
		value := *profile
		profileID = &value
	} else if savedProfile != nil {
		value := *savedProfile
		profileID = &value
	}
	chosen := choose("target_id", target, savedTarget, targets)
	if chosen != nil && len(open) == 0 {
		return profileID, *chosen, nil
	}
	return nil, "", open
}

// LaunchDefault is the pair a caller may leave unnamed when it starts a
// session. Rust: deny_unknown_fields.
type LaunchDefault struct {
	ProfileID string `json:"profile_id"`
	TargetID  string `json:"target_id"`
}
