// Port of mj-client/src/session.rs.

// The session operations used by interactive control surfaces.

package client

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ViewErrorKind is the kind of a ViewError.
type ViewErrorKind string

const (
	ViewErrorKindUnreachable         ViewErrorKind = "unreachable"
	ViewErrorKindTargetMissing       ViewErrorKind = "target_missing"
	ViewErrorKindProjectionIntegrity ViewErrorKind = "projection_integrity"
)

// ViewError is why a session view is not live. Rust's enum (Unreachable,
// TargetMissing, ProjectionIntegrity, each carrying a detail string) is a
// single struct here, adjacently tagged by "kind" with the detail under
// "detail", in snake_case.
type ViewError struct {
	Kind    ViewErrorKind
	Message string
}

// Detail is the error's detail text.
func (e ViewError) Detail() string { return e.Message }

func (e ViewError) MarshalJSON() ([]byte, error) {
	body, err := daemonMarshalJSON(e.Message)
	if err != nil {
		return nil, err
	}
	return daemonAdjacentJSON("kind", "detail", string(e.Kind), body)
}

func (e *ViewError) UnmarshalJSON(data []byte) error {
	name, body, err := daemonSplitAdjacent(data, "kind", "detail")
	if err != nil {
		return err
	}
	switch kind := ViewErrorKind(name); kind {
	case ViewErrorKindUnreachable, ViewErrorKindTargetMissing, ViewErrorKindProjectionIntegrity:
		var message string
		if err := daemonDecodeRequired(body, "detail", &message); err != nil {
			return err
		}
		*e = ViewError{Kind: kind, Message: message}
		return nil
	}
	return daemonUnknownVariant(name, []string{"unreachable", "target_missing", "projection_integrity"})
}

type ManagedSessionView struct {
	Attention core.SessionAttention
	Snapshot  *core.ManagedSessionSnapshot
	Connected bool
	Error     *ViewError
}

type RelayAttachment struct {
	State          core.SharedSnapshot[core.RelayOperationalState] `json:"state"`
	Events         []core.RelayEvent                               `json:"events"`
	ThroughOrdinal uint64                                          `json:"through_ordinal"`
	ThroughDigest  string                                          `json:"through_digest"`
}

func (v RelayAttachment) MarshalJSON() ([]byte, error) {
	type plain RelayAttachment
	return daemonMarshalStruct(plain(v))
}

func (v *RelayAttachment) UnmarshalJSON(data []byte) error {
	type plain RelayAttachment
	return daemonDecodeStruct(data, (*plain)(v), false)
}

// ReviewerAction is one reviewer operation on the worker. Externally tagged
// in snake_case.
type ReviewerAction interface {
	isReviewerAction()
	// OperationName names the operation in worker requests.
	OperationName() string
}

type ReviewerActionCaptureDelta struct {
	// Repositories is nil for every repository.
	Repositories *[]string `json:"repositories,omitempty" serde:"default"`
}

type ReviewerActionRecordReviewed struct {
	// Trees maps each repository path to its pair of trees.
	Trees map[string][2]string `json:"trees"`
}

// ReviewerActionRecordOverridden records trees as overridden: pushable, but
// not the next review base.
type ReviewerActionRecordOverridden struct {
	Trees map[string][2]string `json:"trees"`
}

// ReviewerActionSetReviewBase makes a reviewed or overridden commit the next
// push review base.
type ReviewerActionSetReviewBase struct {
	Path   *string `json:"path"`
	Commit string  `json:"commit"`
}

func (ReviewerActionCaptureDelta) isReviewerAction()     {}
func (ReviewerActionRecordReviewed) isReviewerAction()   {}
func (ReviewerActionRecordOverridden) isReviewerAction() {}
func (ReviewerActionSetReviewBase) isReviewerAction()    {}

func (ReviewerActionCaptureDelta) OperationName() string     { return "reviewer_capture_delta" }
func (ReviewerActionRecordReviewed) OperationName() string   { return "reviewer_record_reviewed" }
func (ReviewerActionRecordOverridden) OperationName() string { return "reviewer_record_overridden" }
func (ReviewerActionSetReviewBase) OperationName() string    { return "reviewer_set_review_base" }

func (v ReviewerActionCaptureDelta) MarshalJSON() ([]byte, error) {
	type plain ReviewerActionCaptureDelta
	return sessionExternalStruct("capture_delta", plain(v))
}

func (v ReviewerActionRecordReviewed) MarshalJSON() ([]byte, error) {
	type plain ReviewerActionRecordReviewed
	return sessionExternalStruct("record_reviewed", plain(v))
}

func (v ReviewerActionRecordOverridden) MarshalJSON() ([]byte, error) {
	type plain ReviewerActionRecordOverridden
	return sessionExternalStruct("record_overridden", plain(v))
}

func (v ReviewerActionSetReviewBase) MarshalJSON() ([]byte, error) {
	type plain ReviewerActionSetReviewBase
	return sessionExternalStruct("set_review_base", plain(v))
}

// UnmarshalReviewerActionJSON decodes serde's externally tagged form.
func UnmarshalReviewerActionJSON(data []byte) (ReviewerAction, error) {
	name, body, err := daemonSplitExternal(data)
	if err != nil {
		return nil, err
	}
	switch name {
	case "capture_delta":
		return sessionExternalVariant[ReviewerActionCaptureDelta, ReviewerAction](body)
	case "record_reviewed":
		return sessionExternalVariant[ReviewerActionRecordReviewed, ReviewerAction](body)
	case "record_overridden":
		return sessionExternalVariant[ReviewerActionRecordOverridden, ReviewerAction](body)
	case "set_review_base":
		return sessionExternalVariant[ReviewerActionSetReviewBase, ReviewerAction](body)
	}
	return nil, daemonUnknownVariant(name, []string{"capture_delta", "record_reviewed", "record_overridden", "set_review_base"})
}

// ReviewerOutcome is a reviewer operation's result. Externally tagged in
// snake_case.
type ReviewerOutcome interface {
	isReviewerOutcome()
}

type ReviewerOutcomeDelta struct {
	Repositories []core.RepoDelta `json:"repositories"`
}

type ReviewerOutcomeBaselineAdvanced struct{}

type ReviewerOutcomeReviewBaseSet struct {
	Root    string `json:"root"`
	Commit  string `json:"commit"`
	Tree    string `json:"tree"`
	Changed bool   `json:"changed"`
}

func (ReviewerOutcomeDelta) isReviewerOutcome()            {}
func (ReviewerOutcomeBaselineAdvanced) isReviewerOutcome() {}
func (ReviewerOutcomeReviewBaseSet) isReviewerOutcome()    {}

func (v ReviewerOutcomeDelta) MarshalJSON() ([]byte, error) {
	type plain ReviewerOutcomeDelta
	return sessionExternalStruct("delta", plain(v))
}

func (ReviewerOutcomeBaselineAdvanced) MarshalJSON() ([]byte, error) {
	return daemonExternalJSON("baseline_advanced", nil)
}

func (v ReviewerOutcomeReviewBaseSet) MarshalJSON() ([]byte, error) {
	type plain ReviewerOutcomeReviewBaseSet
	return sessionExternalStruct("review_base_set", plain(v))
}

// UnmarshalReviewerOutcomeJSON decodes serde's externally tagged form.
func UnmarshalReviewerOutcomeJSON(data []byte) (ReviewerOutcome, error) {
	name, body, err := daemonSplitExternal(data)
	if err != nil {
		return nil, err
	}
	switch name {
	case "delta":
		return sessionExternalVariant[ReviewerOutcomeDelta, ReviewerOutcome](body)
	case "baseline_advanced":
		return ReviewerOutcomeBaselineAdvanced{}, nil
	case "review_base_set":
		return sessionExternalVariant[ReviewerOutcomeReviewBaseSet, ReviewerOutcome](body)
	}
	return nil, daemonUnknownVariant(name, []string{"delta", "baseline_advanced", "review_base_set"})
}

// sessionExternalStruct writes an externally tagged struct variant;
// fields is a method-less copy of the variant.
func sessionExternalStruct(name string, fields any) ([]byte, error) {
	body, err := daemonMarshalStruct(fields)
	if err != nil {
		return nil, err
	}
	return daemonExternalJSON(name, body)
}

// sessionExternalVariant decodes the content of an externally tagged struct
// variant into T.
func sessionExternalVariant[T any, I any](body []byte) (I, error) {
	var value T
	var zero I
	if body == nil {
		return zero, errors.New("invalid type: unit variant, expected struct variant")
	}
	if err := daemonDecodeStruct(body, &value, false); err != nil {
		return zero, err
	}
	return any(value).(I), nil
}

// SubmitFailure preserves whether a submit failed before delivery or lost its
// acknowledgement.
type SubmitFailure struct {
	Message     string
	Unconfirmed bool
	// Refused: the worker itself read the command and rejected it. Other
	// definite failures (a full queue, a worker that could not be reached)
	// were never looked at and may succeed on a later attempt.
	Refused bool
}

// SubmitFailureFrom is a definite failure with message (Rust's
// From<String>).
func SubmitFailureFrom(message string) SubmitFailure {
	return SubmitFailure{Message: message}
}

// Refused: the worker read the command and rejected it; resending it
// unchanged will be rejected again.
type Refused struct{}

func (Refused) Error() string { return "the worker refused the command" }

// DeliveryUnconfirmed: submission lost its acknowledgement; callers must
// reconcile before retrying.
type DeliveryUnconfirmed struct{}

func (DeliveryUnconfirmed) Error() string { return "delivery unconfirmed" }

// PendingRelaySubmit is an accepted submit whose relay ordinal is still to
// come.
type PendingRelaySubmit struct {
	completion func(context.Context) (uint64, error)
}

func NewPendingRelaySubmit(completion func(context.Context) (uint64, error)) PendingRelaySubmit {
	return PendingRelaySubmit{completion: completion}
}

func (p PendingRelaySubmit) Wait(ctx context.Context) (uint64, error) {
	return p.completion(ctx)
}

// PendingRelaySync is an accepted sync still to complete.
type PendingRelaySync struct {
	completion func(context.Context) error
}

func NewPendingRelaySync(completion func(context.Context) error) PendingRelaySync {
	return PendingRelaySync{completion: completion}
}

func (p PendingRelaySync) Wait(ctx context.Context) error {
	return p.completion(ctx)
}

// SessionHandleBackend is what a SessionHandle drives. Rust's default
// methods are the optional interfaces TranscriptHistoryBackend and
// GithubTokenBackend; a backend without them gets Rust's defaults.
type SessionHandleBackend interface {
	// SearchPrompts is one page of prompts matching query, resuming from
	// after.
	SearchPrompts(
		ctx context.Context,
		bundleID string,
		scope core.HistoryScope,
		query string,
		after *core.HistoryCursor,
		limit int,
	) (core.PromptHistoryPage, error)
	// SessionPrompts is this session's stored prompts, newest first, each
	// text once.
	SessionPrompts(ctx context.Context) ([]core.PromptHistoryEntry, error)
	// ConfigResult is the outcome of a configuration command: done false
	// while it is pending; otherwise the rejection, nil for success.
	ConfigResult(ctx context.Context, commandID string) (rejection *string, done bool, err error)

	CloneBox() SessionHandleBackend
	SessionID() string
	View() ManagedSessionView
	IsStopped() bool
	HasChanged() (bool, error)
	Changed(ctx context.Context) (ManagedSessionView, error)
	EnqueueSubmit(ctx context.Context, commandID string, command core.RelayCommand) (PendingRelaySubmit, error)
	EnqueueSync(ctx context.Context) (PendingRelaySync, error)
	RespondElicitation(ctx context.Context, elicitationID string, response core.ElicitationResponse) error
	StopBackgroundTask(ctx context.Context, backgroundTaskID string) error
	Reviewer(ctx context.Context, role *string, action ReviewerAction) (ReviewerOutcome, error)
}

// TranscriptHistoryBackend is the optional SessionHandleBackend method that
// pages earlier conversation history. Without it the history is unavailable.
type TranscriptHistoryBackend interface {
	TranscriptHistory(ctx context.Context, before *core.TranscriptCursor) (core.TranscriptHistoryPage, error)
}

// GithubTokenBackend is the optional SessionHandleBackend method that
// replaces the worker's ephemeral GitHub token before a controller-owned
// operation that needs the newest installation credential. Without it the
// refresh is unavailable.
type GithubTokenBackend interface {
	InstallGithubToken(ctx context.Context, token string) error
}

// SessionHandle is one live session as a control surface drives it.
type SessionHandle struct {
	backend SessionHandleBackend
}

func NewSessionHandle(backend SessionHandleBackend) SessionHandle {
	return SessionHandle{backend: backend}
}

func (h SessionHandle) TranscriptHistory(ctx context.Context, before *core.TranscriptCursor) (core.TranscriptHistoryPage, error) {
	if history, ok := h.backend.(TranscriptHistoryBackend); ok {
		return history.TranscriptHistory(ctx, before)
	}
	return core.TranscriptHistoryPage{}, errors.New("earlier conversation history is unavailable")
}

func (h SessionHandle) SearchPrompts(
	ctx context.Context,
	bundleID string,
	scope core.HistoryScope,
	query string,
	after *core.HistoryCursor,
	limit int,
) (core.PromptHistoryPage, error) {
	return h.backend.SearchPrompts(ctx, bundleID, scope, query, after, limit)
}

func (h SessionHandle) SessionPrompts(ctx context.Context) ([]core.PromptHistoryEntry, error) {
	return h.backend.SessionPrompts(ctx)
}

func (h SessionHandle) SessionID() string { return h.backend.SessionID() }

func (h SessionHandle) View() ManagedSessionView { return h.backend.View() }

func (h SessionHandle) IsStopped() bool { return h.backend.IsStopped() }

func (h SessionHandle) HasChanged() (bool, error) { return h.backend.HasChanged() }

func (h SessionHandle) Changed(ctx context.Context) (ManagedSessionView, error) {
	return h.backend.Changed(ctx)
}

// Clone is a handle with its own change tracking, as Rust's Clone gives.
func (h SessionHandle) Clone() SessionHandle {
	return SessionHandle{backend: h.backend.CloneBox()}
}

func (h SessionHandle) Submit(ctx context.Context, commandID string, command core.RelayCommand) (uint64, error) {
	pending, err := h.EnqueueSubmit(ctx, commandID, command)
	if err != nil {
		return 0, err
	}
	return pending.Wait(ctx)
}

// SetConfig applies a setting and waits for its durable success or
// rejection.
func (h SessionHandle) SetConfig(ctx context.Context, key, value string) error {
	commandID, err := NewCommandID("set-config")
	if err != nil {
		return err
	}
	_, err = h.SetConfigWithID(ctx, commandID, key, value)
	return err
}

// SetConfigWithID applies a setting under a caller-chosen command id and
// returns the ordinal at which the relay accepted it. A caller that retries
// with the same id after a lost reply gets the original ordinal back.
func (h SessionHandle) SetConfigWithID(ctx context.Context, commandID, key, value string) (uint64, error) {
	return h.applyConfigurationCommand(ctx, commandID, core.RelayCommandSetConfig{Key: key, Value: value})
}

func (h SessionHandle) applyConfigurationCommand(ctx context.Context, commandID string, command core.RelayCommand) (uint64, error) {
	ordinal, err := h.Submit(ctx, commandID, command)
	if err != nil {
		return 0, err
	}
	wait, cancel := context.WithTimeout(ctx, 60*time.Second)
	defer cancel()
	timedOut := func(err error) error {
		return sessionTimedOut(ctx, wait, "configuration command did not complete within 60 seconds", err)
	}
	for {
		rejection, done, err := h.backend.ConfigResult(wait, commandID)
		if err != nil {
			return 0, timedOut(err)
		}
		if done {
			if rejection != nil {
				return 0, errors.New(*rejection)
			}
			if err := h.SyncNow(wait); err != nil {
				return 0, timedOut(err)
			}
			return ordinal, nil
		}
		if h.IsStopped() {
			return 0, errors.New("session stopped while applying configuration")
		}
		if viewError := h.View().Error; viewError != nil {
			return 0, fmt.Errorf("configuration connection failed: %s", viewError.Detail())
		}
		if err := daemonSleep(wait, 50*time.Millisecond); err != nil {
			return 0, timedOut(err)
		}
	}
}

// sessionTimedOut is Rust's `tokio::time::timeout(..).context(message)`: a
// failure after the wait's own deadline passed is that deadline, reported
// with message; a cancelled parent and any other failure pass through.
func sessionTimedOut(parent, wait context.Context, message string, err error) error {
	if parent.Err() == nil && errors.Is(wait.Err(), context.DeadlineExceeded) {
		return fmt.Errorf("%s: %w", message, wait.Err())
	}
	return err
}

// ApplyPlanControl applies a plan control. A follow-up prompt must wait for
// the mode change, not just admission.
func (h SessionHandle) ApplyPlanControl(ctx context.Context, commandID string, control core.PlanControl) error {
	switch control := control.(type) {
	case core.PlanControlRestoreExecutionMode:
		_, err := h.applyConfigurationCommand(ctx, commandID, core.RelayCommandRestoreExecutionMode{})
		return err
	case core.PlanControlSetConfig:
		_, err := h.SetConfigWithID(ctx, commandID, control.Key, control.Value)
		return err
	case core.PlanControlSetSessionMode:
		if _, err := h.Submit(ctx, commandID, core.RelayCommandSetSessionMode{ModeID: control.ModeID}); err != nil {
			return err
		}
		wait, cancel := context.WithTimeout(ctx, 60*time.Second)
		defer cancel()
		timedOut := func(err error) error {
			return sessionTimedOut(ctx, wait, "session mode change did not complete within 60 seconds", err)
		}
		for {
			if err := h.SyncNow(wait); err != nil {
				return timedOut(err)
			}
			if snapshot := h.View().Snapshot; snapshot != nil {
				if modes := snapshot.Operational.Get().Modes; modes != nil && string(modes.CurrentModeID) == control.ModeID {
					return nil
				}
			}
			if h.IsStopped() {
				return errors.New("session stopped while changing mode")
			}
			if err := daemonSleep(wait, 50*time.Millisecond); err != nil {
				return timedOut(err)
			}
		}
	}
	panic(fmt.Sprintf("unknown PlanControl %T", control))
}

func (h SessionHandle) EnqueueSubmit(ctx context.Context, commandID string, command core.RelayCommand) (PendingRelaySubmit, error) {
	return h.backend.EnqueueSubmit(ctx, commandID, command)
}

func (h SessionHandle) SyncNow(ctx context.Context) error {
	pending, err := h.EnqueueSync(ctx)
	if err != nil {
		return err
	}
	return pending.Wait(ctx)
}

func (h SessionHandle) EnqueueSync(ctx context.Context) (PendingRelaySync, error) {
	return h.backend.EnqueueSync(ctx)
}

func (h SessionHandle) RespondElicitation(ctx context.Context, elicitationID string, response core.ElicitationResponse) error {
	return h.backend.RespondElicitation(ctx, elicitationID, response)
}

func (h SessionHandle) StopBackgroundTask(ctx context.Context, backgroundTaskID string) error {
	return h.backend.StopBackgroundTask(ctx, backgroundTaskID)
}

func (h SessionHandle) InstallGithubToken(ctx context.Context, token string) error {
	if github, ok := h.backend.(GithubTokenBackend); ok {
		return github.InstallGithubToken(ctx, token)
	}
	return errors.New("GitHub token refresh is unavailable")
}

func (h SessionHandle) Reviewer(ctx context.Context, action ReviewerAction) (ReviewerOutcome, error) {
	return h.backend.Reviewer(ctx, nil, action)
}

// GoString is Rust's Debug: the session id and nothing else.
func (h SessionHandle) GoString() string {
	return fmt.Sprintf("SessionHandle { session_id: %q, .. }", h.SessionID())
}

// ParentMessageRoute is how a parent's message reaches a session. The
// session manager owns launch registration before a relay actor attaches.
// A sealed interface over ParentMessageRouteStarting,
// ParentMessageRoutePublished and ParentMessageRouteUnmanaged.
type ParentMessageRoute interface {
	isParentMessageRoute()
}

type ParentMessageRouteStarting struct{}

type ParentMessageRoutePublished struct {
	// RelayProtocolVersion is the published relay's protocol, when it
	// reported one.
	RelayProtocolVersion *uint32
}

type ParentMessageRouteUnmanaged struct{}

func (ParentMessageRouteStarting) isParentMessageRoute()  {}
func (ParentMessageRoutePublished) isParentMessageRoute() {}
func (ParentMessageRouteUnmanaged) isParentMessageRoute() {}

// SessionControlBackend resolves sessions for a SessionControl. Rust's
// default methods are the optional interfaces ParentMessageRouteBackend and
// SessionSnapshotBackend; a backend without them gets Rust's defaults, built
// on Session.
type SessionControlBackend interface {
	Session(ctx context.Context, sessionID string) (SessionHandle, error)
}

// ParentMessageRouteBackend is the optional SessionControlBackend method
// that routes a parent's message.
type ParentMessageRouteBackend interface {
	ParentMessageRoute(ctx context.Context, sessionID string) (ParentMessageRoute, error)
}

// SessionSnapshotBackend is the optional SessionControlBackend method that
// reads a session's materialized transcript.
type SessionSnapshotBackend interface {
	Snapshot(ctx context.Context, sessionID string) (core.MaterializedSession, error)
}

// SessionControl resolves live sessions for control surfaces.
type SessionControl struct {
	backend SessionControlBackend
}

func NewSessionControl(backend SessionControlBackend) SessionControl {
	return SessionControl{backend: backend}
}

func (c SessionControl) ParentMessageRoute(ctx context.Context, sessionID string) (ParentMessageRoute, error) {
	if routes, ok := c.backend.(ParentMessageRouteBackend); ok {
		return routes.ParentMessageRoute(ctx, sessionID)
	}
	handle, err := c.backend.Session(ctx, sessionID)
	if err != nil {
		return ParentMessageRouteUnmanaged{}, nil
	}
	var version *uint32
	if snapshot := handle.View().Snapshot; snapshot != nil {
		version = snapshot.Operational.Get().RelayProtocolVersion
	}
	return ParentMessageRoutePublished{RelayProtocolVersion: version}, nil
}

func (c SessionControl) Snapshot(ctx context.Context, sessionID string) (core.MaterializedSession, error) {
	if snapshots, ok := c.backend.(SessionSnapshotBackend); ok {
		return snapshots.Snapshot(ctx, sessionID)
	}
	handle, err := c.backend.Session(ctx, sessionID)
	if err != nil {
		return core.MaterializedSession{}, err
	}
	if err := handle.SyncNow(ctx); err != nil {
		return core.MaterializedSession{}, err
	}
	snapshot := handle.View().Snapshot
	if snapshot == nil {
		return core.MaterializedSession{}, errors.New("session has no transcript yet")
	}
	return snapshot.Materialized, nil
}

func (c SessionControl) Session(ctx context.Context, sessionID string) (SessionHandle, error) {
	return c.backend.Session(ctx, sessionID)
}

// WaitForSession waits up to timeout for the session's actor to be
// available.
func (c SessionControl) WaitForSession(ctx context.Context, sessionID string, timeout time.Duration) (SessionHandle, error) {
	wait, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	for {
		handle, err := c.Session(wait, sessionID)
		if err == nil {
			return handle, nil
		}
		// tracing's trace level, below slog's Debug.
		slog.Log(wait, slog.LevelDebug-4, "waiting for session actor", "session_id", sessionID, "error", err.Error())
		if err := daemonSleep(wait, 25*time.Millisecond); err != nil {
			return SessionHandle{}, sessionTimedOut(ctx, wait,
				fmt.Sprintf("session %s did not become available within %d seconds", sessionID, int64(timeout/time.Second)), err)
		}
	}
}

// GoString is Rust's Debug.
func (c SessionControl) GoString() string { return "SessionControl(..)" }

// NewCommandID is a fresh command id: prefix, a dash, and 32 random hex
// digits. Rust re-exports mj_core::relay::new_command_id here.
func NewCommandID(prefix string) (string, error) { return core.NewCommandID(prefix) }

// ReplacementSessionTestFixture is a stopped session and a manager that
// resolves its live replacement.
//
// Chat's cross-package tests use this hand-written client fake to verify
// actor replacement without depending on the controller implementation.
type ReplacementSessionTestFixture struct {
	Stopped         SessionHandle
	Control         SessionControl
	Submitted       *core.Unbounded[core.RelayCommand]
	ReplacementView *core.Watch[ManagedSessionView]
}

// replacementTestSession is the fixture's SessionHandleBackend.
type replacementTestSession struct {
	// history, when set, receives prompt searches for a test to answer
	// (test-only in Rust).
	history         *core.Unbounded[historyTestRequest]
	sessionID       string
	stopped         bool
	acceptedOrdinal uint64
	submitted       *core.Unbounded[core.RelayCommand]
	view            *core.Watch[ManagedSessionView]
	// seen is the view's change signal as of this handle's last
	// observation; it is closed once the view changes after that.
	seen <-chan struct{}
	// viewClosed: the view's sender is gone (the session manager stopped).
	viewClosed bool
}

func (s *replacementTestSession) SearchPrompts(
	ctx context.Context,
	bundleID string,
	scope core.HistoryScope,
	query string,
	after *core.HistoryCursor,
	limit int,
) (core.PromptHistoryPage, error) {
	if s.history != nil {
		response := make(chan historyTestResponse, 1)
		err := s.history.Send(historyTestRequest{bundleID: bundleID, scope: scope, query: query, response: response})
		if err != nil {
			return core.PromptHistoryPage{}, errors.New("history backend closed")
		}
		select {
		case answer, ok := <-response:
			if !ok {
				return core.PromptHistoryPage{}, errors.New("history response dropped")
			}
			return answer.page, answer.err
		case <-ctx.Done():
			return core.PromptHistoryPage{}, ctx.Err()
		}
	}
	return core.PromptHistoryPage{Entries: []core.PromptHistoryEntry{}}, nil
}

func (s *replacementTestSession) SessionPrompts(ctx context.Context) ([]core.PromptHistoryEntry, error) {
	return []core.PromptHistoryEntry{}, nil
}

func (s *replacementTestSession) ConfigResult(ctx context.Context, commandID string) (*string, bool, error) {
	return nil, false, nil
}

func (s *replacementTestSession) CloneBox() SessionHandleBackend {
	clone := *s
	return &clone
}

func (s *replacementTestSession) SessionID() string { return s.sessionID }

func (s *replacementTestSession) View() ManagedSessionView { return s.view.Borrow() }

func (s *replacementTestSession) IsStopped() bool { return s.stopped }

func (s *replacementTestSession) HasChanged() (bool, error) {
	if s.viewClosed {
		return false, errors.New("session manager stopped")
	}
	select {
	case <-s.seen:
		return true, nil
	default:
		return false, nil
	}
}

func (s *replacementTestSession) Changed(ctx context.Context) (ManagedSessionView, error) {
	if s.viewClosed {
		return ManagedSessionView{}, errors.New("session manager stopped")
	}
	select {
	case <-s.seen:
	case <-ctx.Done():
		return ManagedSessionView{}, ctx.Err()
	}
	// Observing the change marks the current value seen, as tokio's
	// changed() does.
	s.seen = s.view.Changed()
	return s.View(), nil
}

func (s *replacementTestSession) EnqueueSubmit(ctx context.Context, commandID string, command core.RelayCommand) (PendingRelaySubmit, error) {
	if s.stopped {
		return PendingRelaySubmit{}, errors.New("session manager stopped")
	}
	if s.submitted == nil {
		return PendingRelaySubmit{}, errors.New("unsupported test operation")
	}
	if err := s.submitted.Send(command); err != nil {
		return PendingRelaySubmit{}, fmt.Errorf("test submit observer stopped: %w", err)
	}
	ordinal := s.acceptedOrdinal
	return NewPendingRelaySubmit(func(context.Context) (uint64, error) { return ordinal, nil }), nil
}

func (s *replacementTestSession) EnqueueSync(ctx context.Context) (PendingRelaySync, error) {
	if s.stopped {
		return PendingRelaySync{}, errors.New("session manager stopped")
	}
	return NewPendingRelaySync(func(context.Context) error { return nil }), nil
}

func (s *replacementTestSession) RespondElicitation(ctx context.Context, elicitationID string, response core.ElicitationResponse) error {
	return errors.New("unsupported test operation")
}

func (s *replacementTestSession) StopBackgroundTask(ctx context.Context, backgroundTaskID string) error {
	return errors.New("unsupported test operation")
}

func (s *replacementTestSession) Reviewer(ctx context.Context, role *string, action ReviewerAction) (ReviewerOutcome, error) {
	return nil, errors.New("unsupported test operation")
}

// replacementTestControl is the fixture's SessionControlBackend.
type replacementTestControl struct {
	sessionID   string
	replacement SessionHandle
}

func (c replacementTestControl) Session(ctx context.Context, sessionID string) (SessionHandle, error) {
	if sessionID != c.sessionID {
		return SessionHandle{}, fmt.Errorf("session %s is not managed", sessionID)
	}
	return c.replacement.Clone(), nil
}

// NewReplacementSessionTestFixture builds the fixture (Rust's
// replacement_session_test_fixture; renamed because the type holds the
// name).
func NewReplacementSessionTestFixture(sessionID string, acceptedOrdinal uint64) ReplacementSessionTestFixture {
	// The stopped session's view has lost its sender: it never changes, and
	// waiting for a change fails.
	stoppedView := core.NewWatch(ManagedSessionView{})
	stopped := NewSessionHandle(&replacementTestSession{
		sessionID:       sessionID,
		stopped:         true,
		acceptedOrdinal: acceptedOrdinal,
		view:            stoppedView,
		seen:            stoppedView.Changed(),
		viewClosed:      true,
	})

	view := core.NewWatch(ManagedSessionView{})
	submitted := core.NewUnbounded[core.RelayCommand]()
	replacement := NewSessionHandle(&replacementTestSession{
		sessionID:       sessionID,
		acceptedOrdinal: acceptedOrdinal,
		submitted:       submitted,
		view:            view,
		seen:            view.Changed(),
	})
	control := NewSessionControl(replacementTestControl{sessionID: sessionID, replacement: replacement})
	return ReplacementSessionTestFixture{
		Stopped:         stopped,
		Control:         control,
		Submitted:       submitted,
		ReplacementView: view,
	}
}

// historyTestRequest is one prompt search a test answers (test-only in
// Rust).
type historyTestRequest struct {
	bundleID string
	scope    core.HistoryScope
	query    string
	response chan historyTestResponse
}

type historyTestResponse struct {
	page core.PromptHistoryPage
	err  error
}
