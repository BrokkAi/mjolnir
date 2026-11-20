// Port of mj-core/src/storage.rs.

// Data exchanged with the controller store, independent of its SQLite
// implementation.
//
// Rust serializes an empty Vec as `[]` and an empty map as `{}`, where Go's
// encoding/json writes `null` for a nil slice or map, so the MarshalJSON
// methods here write empty collections for nil ones.

package core

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"slices"
)

// ProjectionIntegrityError is a deterministic projection integrity
// violation. Retrying cannot fix it, so callers must report it separately
// from transport failures.
type ProjectionIntegrityError string

func (e ProjectionIntegrityError) Error() string { return string(e) }

// StoreSchemaMismatch is a store this build cannot safely read and write.
//
// Carried as a typed cause rather than a message so the daemon can tell a
// store that moved underneath it from a transport failure. It survives every
// wrapping hop to the caller, which finds it with errors.As.
type StoreSchemaMismatch struct {
	Found     int64
	Supported int64
	Reason    StoreSchemaMismatchReason
}

// StoreSchemaMismatchReason says why the schema does not match. Its variants
// are comparable, so StoreSchemaMismatch compares with ==.
type StoreSchemaMismatchReason interface {
	isStoreSchemaMismatchReason()
}

type StoreSchemaMismatchReasonNeedsMigration struct{}

type StoreSchemaMismatchReasonIncompatible struct{ MinimumCompatible int64 }

type StoreSchemaMismatchReasonInvalidCompatibilityMetadata struct{}

type StoreSchemaMismatchReasonRollback struct{ Previous int64 }

func (StoreSchemaMismatchReasonNeedsMigration) isStoreSchemaMismatchReason()               {}
func (StoreSchemaMismatchReasonIncompatible) isStoreSchemaMismatchReason()                 {}
func (StoreSchemaMismatchReasonInvalidCompatibilityMetadata) isStoreSchemaMismatchReason() {}
func (StoreSchemaMismatchReasonRollback) isStoreSchemaMismatchReason()                     {}

func (m StoreSchemaMismatch) Error() string {
	switch reason := m.Reason.(type) {
	case StoreSchemaMismatchReasonIncompatible:
		return fmt.Sprintf("Mjolnir database schema %d requires at least build schema %d for reads and writes; this build supports %d; upgrade Mjolnir, run this build with an isolated data directory (--instance NAME or MJ_DATA_DIR), or restore a backup made by the older build", m.Found, reason.MinimumCompatible, m.Supported)
	case StoreSchemaMismatchReasonNeedsMigration:
		return fmt.Sprintf("Mjolnir database schema %d is not the supported schema %d; start the Mjolnir daemon to migrate it", m.Found, m.Supported)
	case StoreSchemaMismatchReasonInvalidCompatibilityMetadata:
		return fmt.Sprintf("Mjolnir database schema %d has missing or invalid compatibility metadata; refusing access from build schema %d", m.Found, m.Supported)
	case StoreSchemaMismatchReasonRollback:
		return fmt.Sprintf("Mjolnir database schema rolled back from %d to %d underneath this writer; refusing writes from build schema %d", reason.Previous, m.Found, m.Supported)
	}
	return fmt.Sprintf("Mjolnir database schema %d does not match build schema %d", m.Found, m.Supported)
}

// HistoryScope is how far a prompt search reaches. Wider scopes list the
// asking session's matches first, then the rest of the project's, then other
// projects', each newest first.
type HistoryScope int

const (
	// HistoryScopeSession is this session's prompts and nothing else.
	HistoryScopeSession HistoryScope = iota
	// HistoryScopeProject is this session's prompts, then other sessions in
	// the project.
	HistoryScopeProject
	// HistoryScopeAll is this session, then the project, then every other
	// project.
	HistoryScopeAll
)

type PromptHistoryEntry struct {
	ID        int64
	SessionID string
	Text      string
}

type ProjectionApplyOutcome int

const (
	ProjectionApplyOutcomeApplied ProjectionApplyOutcome = iota
	ProjectionApplyOutcomeAlreadyApplied
)

// TranscriptMutation is one change to a session's transcript.
type TranscriptMutation interface {
	isTranscriptMutation()
}

type TranscriptMutationUpsert struct{ Item TranscriptItem }

type TranscriptMutationRemove struct{ StableID string }

func (TranscriptMutationUpsert) isTranscriptMutation() {}
func (TranscriptMutationRemove) isTranscriptMutation() {}

// MaterializedConfigResult is one entry of
// MaterializedSessionMutation.ConfigResults: Rust's (String, Option<String>).
type MaterializedConfigResult struct {
	ConfigID string
	Value    *string
}

// MaterializedSessionMutation is the changes derived from one relay event.
// nil leaves a scalar untouched; the double pointer on SessionTitle permits
// explicitly clearing it.
type MaterializedSessionMutation struct {
	NativeAgent *RelayEvent
	// LastActivityAtMs is the relay receipt time for this event. Persistence
	// and the actor cache both take a monotonic maximum so removing detail
	// rows cannot move activity backwards.
	LastActivityAtMs    *int64
	Execution           *MaterializedExecutionState
	SessionTitle        **string
	Configuration       *SessionConfiguration
	Transcript          []TranscriptMutation
	QueuedPrompts       *[]MaterializedQueuedPrompt
	PendingElicitations *[]ElicitationRequest
	// ActiveTurn: the double pointer distinguishes "unchanged" (nil) from
	// "cleared" (pointer to nil), which is how a completed turn removes the
	// running turn.
	ActiveTurn **MaterializedTurn
	// ClearTurnOutcome: explicit context resets retire the last turn without
	// deleting its history.
	ClearTurnOutcome bool
	// ClearMailbox discards pending mailbox delivery when a new conversation
	// is adopted.
	ClearMailbox    bool
	LastTurnOutcome *MaterializedTurnOutcome
	ConfigResults   []MaterializedConfigResult
	ProviderCost    *ProviderCost
	APIEvents       []ApiEventData
}

// ClientSessionState is one viewer's stored state for one session.
type ClientSessionState struct {
	Draft               string
	ThroughEventOrdinal uint64
}

// DetachedSessionDraft is a terminal's current composer and the shared value
// it originally inherited. The inherited value is retired on detach, never
// replaced by client-local text.
type DetachedSessionDraft struct {
	Text           string  `json:"text"`
	InheritedInput *string `json:"inherited_input"`
}

// TurnSummary is what one turn produced, without loading the transcript
// around it.
type TurnSummary struct {
	// TurnNumber is the one-based count of turn starts up to and including
	// this one, which is what a caller means by "turn 3 of this session".
	TurnNumber      uint64
	TurnStartedAtMs int64
	// LastChangedAtMs is the newest change at or after the turn start, so
	// the caller can measure how long the turn took.
	LastChangedAtMs int64
	// FinalMessage is the last nonempty agent message the turn produced,
	// flattened to text.
	FinalMessage *string
	// ToolCalls is how many tool calls the turn made.
	ToolCalls uint64
}

// TranscriptPage is one page of a session's transcript, ordered by the
// sequence a reader pages by rather than by creation order.
type TranscriptPage struct {
	Items []*TranscriptItem
	// LatestSeq is the newest sequence in the whole transcript, so a caller
	// can tell whether this page reached the end without asking for another
	// one.
	LatestSeq    uint64
	NextAfterSeq uint64
	Execution    MaterializedExecutionState
}

// TranscriptCursor is an exclusive chronological cursor; update sequences
// are not creation order.
type TranscriptCursor struct {
	Position uint64 `json:"position"`
	StableID string `json:"stable_id"`
}

func TranscriptCursorOf(item *TranscriptItem) TranscriptCursor {
	return TranscriptCursor{Position: item.Position, StableID: item.StableID}
}

// TranscriptHistoryPage is a bounded historical read, independent of the
// live projection window.
type TranscriptHistoryPage struct {
	Items    []*TranscriptItem `json:"items"`
	Before   *TranscriptCursor `json:"before"`
	Frontier uint64            `json:"frontier"`
}

func (p TranscriptHistoryPage) MarshalJSON() ([]byte, error) {
	type plain TranscriptHistoryPage
	p.Items = storageNonNilSlice(p.Items)
	return json.Marshal(plain(p))
}

// TranscriptRetention is what one retention pass reclaimed.
type TranscriptRetention struct {
	Items int
	Bytes int
	// Remaining: rows this pass left for the next one, because of
	// RETENTION_BATCH_ITEMS.
	Remaining bool
}

// DeclinedFinding is a declined finding and the repositories whose review
// range it covers. An entry stored before repositories were recorded has
// none and covers every repository until any review base moves.
//
// The embedded ReviewOverride is serde's `#[serde(flatten)] entry`, and its
// fields are promoted as Rust's Deref exposes them.
type DeclinedFinding struct {
	ReviewOverride
	Repositories []string
}

// MarshalJSON writes the entry's members, then `repositories` when there
// are any.
func (f DeclinedFinding) MarshalJSON() ([]byte, error) {
	entry, err := json.Marshal(f.ReviewOverride)
	if err != nil {
		return nil, err
	}
	if len(f.Repositories) == 0 {
		return entry, nil
	}
	repositories, err := json.Marshal(map[string][]string{"repositories": f.Repositories})
	if err != nil {
		return nil, err
	}
	return storageMergeJSONObjects(entry, repositories)
}

// UnmarshalJSON reads the flattened entry and the optional repositories.
func (f *DeclinedFinding) UnmarshalJSON(data []byte) error {
	panic("unported: mj-core/src/storage.rs DeclinedFinding deserialize (flatten)")
}

// Covers is whether a review of root must be told not to repeat this
// finding.
func (f *DeclinedFinding) Covers(root string) bool {
	return len(f.Repositories) == 0 || slices.Contains(f.Repositories, root)
}

// Equal compares by value, as Rust's PartialEq does.
func (f *DeclinedFinding) Equal(other *DeclinedFinding) bool {
	return f.ReviewOverride == other.ReviewOverride && slices.Equal(f.Repositories, other.Repositories)
}

// EqualReviewOverride is Rust's PartialEq<ReviewOverride>.
func (f *DeclinedFinding) EqualReviewOverride(other *ReviewOverride) bool {
	return f.ReviewOverride == *other
}

// FinishedReviewResolution is how a review ended, recorded beside its
// per-reviewer output so the output remains readable after the reviewer
// sessions are destroyed.
type FinishedReviewResolution string

const (
	// FinishedReviewResolutionCompleted: every reviewer settled and the
	// verdict was delivered.
	FinishedReviewResolutionCompleted FinishedReviewResolution = "completed"
	// FinishedReviewResolutionFailed: the review could not reach a usable
	// verdict.
	FinishedReviewResolutionFailed FinishedReviewResolution = "failed"
	// FinishedReviewResolutionCancelled: the user stopped the review before
	// it resolved.
	FinishedReviewResolutionCancelled FinishedReviewResolution = "cancelled"
)

// UnmarshalJSON accepts only the resolutions serde accepts.
func (r *FinishedReviewResolution) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch resolution := FinishedReviewResolution(text); resolution {
	case FinishedReviewResolutionCompleted, FinishedReviewResolutionFailed, FinishedReviewResolutionCancelled:
		*r = resolution
		return nil
	}
	return fmt.Errorf("unknown variant `%s`, expected one of `completed`, `failed`, `cancelled`", text)
}

// TurnReviewState is durable review work, findings and declined findings
// for this session.
type TurnReviewState struct {
	// DeclinedFindings are findings the author declined to fix with
	// `override_review`, newest last. Every before-push review of this
	// session tells its reviewers not to report them again in those
	// repositories. A baseline move removes that repository from their
	// coverage. Bounded to DeclinedFindingsBytes.
	DeclinedFindings []DeclinedFinding `json:"declined_findings"`
	// Active is a review that was running when the daemon stopped. On
	// recovery it is cleared without advancing the baseline.
	Active *string `json:"active"`
	// PushFindings is the exact trees covered by the last before-push
	// findings verdict.
	PushFindings *PushFindingsRecord `json:"push_findings,omitempty"`
	// ActiveGroup: a reviewer group survives daemon handoff with its exact
	// commands and children.
	ActiveGroup *ActiveReviewGroup `json:"active_group,omitempty"`
}

func (s TurnReviewState) MarshalJSON() ([]byte, error) {
	type plain TurnReviewState
	s.DeclinedFindings = storageNonNilSlice(s.DeclinedFindings)
	return json.Marshal(plain(s))
}

// DeclinedFindingsBytes is how many bytes of declined findings and reasons
// a session keeps. The oldest entries go first.
const DeclinedFindingsBytes = 24 * 1024

// AdvanceReviewBases stops covering the repositories whose review base
// moved, dropping entries left with none. An entry without recorded
// repositories is dropped.
func (s *TurnReviewState) AdvanceReviewBases(moved []string) {
	if len(moved) == 0 {
		return
	}
	kept := s.DeclinedFindings[:0]
	for _, entry := range s.DeclinedFindings {
		entry.Repositories = slices.DeleteFunc(entry.Repositories, func(root string) bool {
			return slices.Contains(moved, root)
		})
		if len(entry.Repositories) > 0 {
			kept = append(kept, entry)
		}
	}
	s.DeclinedFindings = kept
}

// DeclineFindings adds findings the author declined, then drops the oldest
// entries until the list fits DeclinedFindingsBytes.
func (s *TurnReviewState) DeclineFindings(entries []ReviewOverride, repositories []string) {
	panic("unported: mj-core/src/storage.rs TurnReviewState::decline_findings")
}

// ActiveReviewGroup is a reviewer group that survives daemon handoff.
type ActiveReviewGroup struct {
	Driver        ReviewDriverSnapshot
	Reviewers     []ResolvedReviewSettings
	Registrations []ReviewerRegistration
	// BriefWriter is the resolved brief writer, when this review has one.
	// Its registration is in Registrations under BRIEF_WRITER_ROLE.
	BriefWriter *ResolvedReviewSettings
}

// activeReviewGroupInput is what an ActiveReviewGroup is read from (serde
// `from`): older records have one `reviewer` for every role.
type activeReviewGroupInput struct {
	Driver        ReviewDriverSnapshot     `json:"driver"`
	Reviewers     []ResolvedReviewSettings `json:"reviewers"`
	Reviewer      *ResolvedReviewSettings  `json:"reviewer"`
	Registrations []ReviewerRegistration   `json:"registrations"`
	BriefWriter   *ResolvedReviewSettings  `json:"brief_writer"`
}

// UnmarshalJSON reads the current form and the older one-reviewer form.
func (g *ActiveReviewGroup) UnmarshalJSON(data []byte) error {
	var input activeReviewGroupInput
	if err := json.Unmarshal(data, &input); err != nil {
		return err
	}
	reviewers := input.Reviewers
	if len(reviewers) == 0 {
		reviewers = []ResolvedReviewSettings{}
		if input.Reviewer != nil {
			for range input.Driver.Roles {
				reviewers = append(reviewers, *input.Reviewer)
			}
		}
	}
	*g = ActiveReviewGroup{
		Driver:        input.Driver,
		Reviewers:     reviewers,
		Registrations: input.Registrations,
		BriefWriter:   input.BriefWriter,
	}
	return nil
}

// MarshalJSON writes the persisted form. Scope is a persisted compatibility
// field, never an orchestration decision.
func (g ActiveReviewGroup) MarshalJSON() ([]byte, error) {
	type activeReviewGroupOutput struct {
		Driver        ReviewDriverSnapshot     `json:"driver"`
		Reviewers     []ResolvedReviewSettings `json:"reviewers"`
		Registrations []ReviewerRegistration   `json:"registrations"`
		Scope         string                   `json:"scope"`
		BriefWriter   *ResolvedReviewSettings  `json:"brief_writer,omitempty"`
	}
	return json.Marshal(activeReviewGroupOutput{
		Driver:        g.Driver,
		Reviewers:     storageNonNilSlice(g.Reviewers),
		Registrations: storageNonNilSlice(g.Registrations),
		Scope:         "pushable_commits",
		BriefWriter:   g.BriefWriter,
	})
}

type ReviewerRegistration struct {
	Role           string  `json:"role"`
	RequestKey     string  `json:"request_key"`
	ChildSessionID *string `json:"child_session_id,omitempty"`
}

type PushFindingsRecord struct {
	ReviewID string `json:"review_id"`
	// Trees maps each repository to its pair of trees, a two-element JSON
	// array as serde writes a tuple.
	Trees     map[string][2]string `json:"trees"`
	Synthesis string               `json:"synthesis"`
}

func (r PushFindingsRecord) MarshalJSON() ([]byte, error) {
	type plain PushFindingsRecord
	r.Trees = storageNonNilMap(r.Trees)
	return json.Marshal(plain(r))
}

// MinHistoryQueryChars: prompt-history queries shorter than this many
// characters match nothing: the trigram index behind the search cannot
// answer them.
const MinHistoryQueryChars = 3

// HistoryCursor is where a prompt search resumes: the tier it reached,
// counted from the asking session outward; the newest-row position below
// which it continues; and the newest history row when the search began, past
// which it reads nothing.
type HistoryCursor struct {
	Tier    uint8
	Before  int64
	Through int64
}

// PromptHistoryPage is one page of a prompt search. Next is nil once nothing
// more matches.
//
// Without Next a caller cannot tell eight matches from the first eight of
// many, and would present a partial answer as a whole one.
type PromptHistoryPage struct {
	Entries []PromptHistoryEntry
	Next    *HistoryCursor
}

type UsageCoverage struct {
	RecordedTurns      uint64 `json:"recorded_turns"`
	FullTurnReports    uint64 `json:"full_turn_reports"`
	LastRequestReports uint64 `json:"last_request_reports"`
	UnspecifiedReports uint64 `json:"unspecified_reports"`
	MissingReports     uint64 `json:"missing_reports"`
	// UnfinishedTurns is started turns for which no completion report has
	// been recorded.
	UnfinishedTurns    uint64 `json:"unfinished_turns"`
	ProviderCostTurns  uint64 `json:"provider_cost_turns"`
	EstimatedCostTurns uint64 `json:"estimated_cost_turns"`
	MissingCostTurns   uint64 `json:"missing_cost_turns"`
}

type UsageCounterTotal struct {
	Tokens uint64 `json:"tokens"`
	// ReportedTurns is the number of full-turn reports supplying this
	// particular counter.
	ReportedTurns uint64 `json:"reported_turns"`
}

type UsagePage struct {
	SessionID    string                    `json:"session_id"`
	Turns        []MaterializedTurnOutcome `json:"turns"`
	NextAfterSeq uint64                    `json:"next_after_seq"`
	LatestSeq    uint64                    `json:"latest_seq"`
	// Totals include only reports whose scope is known to be a whole turn.
	Totals              map[string]UsageCounterTotal `json:"totals"`
	Coverage            UsageCoverage                `json:"coverage"`
	ProviderSessionCost *ProviderCost                `json:"provider_session_cost,omitempty"`
	// TurnCostUsdTicks is the sum of priced whole turns, in 10^-10 USD.
	// Partial when cost coverage is missing.
	TurnCostUsdTicks uint64                    `json:"turn_cost_usd_ticks"`
	TurnSelections   map[string]UsageSelection `json:"turn_selections"`
	ByModel          []UsageModelTotal         `json:"by_model"`
}

func (p UsagePage) MarshalJSON() ([]byte, error) {
	type plain UsagePage
	p.Turns = storageNonNilSlice(p.Turns)
	p.Totals = storageNonNilMap(p.Totals)
	p.TurnSelections = storageNonNilMap(p.TurnSelections)
	p.ByModel = storageNonNilSlice(p.ByModel)
	return json.Marshal(plain(p))
}

// UsageSelection is the configuration observed when a turn started. nil
// means unknown, not zero. Selections are ordered by Compare.
type UsageSelection struct {
	Model  *string `json:"model"`
	Effort *string `json:"effort"`
}

// Compare orders selections as Rust's derived Ord does: by model, then
// effort, with nil before any value.
func (s UsageSelection) Compare(other UsageSelection) int {
	compare := func(a, b *string) int {
		switch {
		case a == nil && b == nil:
			return 0
		case a == nil:
			return -1
		case b == nil:
			return 1
		}
		return bytes.Compare([]byte(*a), []byte(*b))
	}
	if order := compare(s.Model, other.Model); order != 0 {
		return order
	}
	return compare(s.Effort, other.Effort)
}

// UsageModelTotal is the totals of one selection; the selection's members
// are flattened into the object.
type UsageModelTotal struct {
	Selection UsageSelection
	Totals    map[string]UsageCounterTotal
}

func (t UsageModelTotal) MarshalJSON() ([]byte, error) {
	selection, err := json.Marshal(t.Selection)
	if err != nil {
		return nil, err
	}
	totals, err := json.Marshal(map[string]map[string]UsageCounterTotal{"totals": storageNonNilMap(t.Totals)})
	if err != nil {
		return nil, err
	}
	return storageMergeJSONObjects(selection, totals)
}

func (t *UsageModelTotal) UnmarshalJSON(data []byte) error {
	var totals struct {
		Totals *map[string]UsageCounterTotal `json:"totals"`
	}
	if err := json.Unmarshal(data, &totals); err != nil {
		return err
	}
	if totals.Totals == nil {
		return errors.New("missing field `totals`")
	}
	var selection UsageSelection
	if err := json.Unmarshal(data, &selection); err != nil {
		return err
	}
	*t = UsageModelTotal{Selection: selection, Totals: *totals.Totals}
	return nil
}

type UsageSummary struct {
	Totals              map[string]UsageCounterTotal `json:"totals"`
	Coverage            UsageCoverage                `json:"coverage"`
	ByModel             []UsageModelTotal            `json:"by_model"`
	ProviderSessionCost *ProviderCost                `json:"provider_session_cost,omitempty"`
	// TurnCostUsdTicks is the sum of priced whole turns, in 10^-10 USD.
	// Partial when cost coverage is missing.
	TurnCostUsdTicks uint64 `json:"turn_cost_usd_ticks"`
}

func (s UsageSummary) MarshalJSON() ([]byte, error) {
	type plain UsageSummary
	s.Totals = storageNonNilMap(s.Totals)
	s.ByModel = storageNonNilSlice(s.ByModel)
	return json.Marshal(plain(s))
}

// UsageTreeSession is one session of a usage tree; the summary's members are
// flattened into the object.
type UsageTreeSession struct {
	SessionID                 string
	ParentSessionID           *string
	TaskName                  *string
	OperationalSessionPresent bool
	Summary                   UsageSummary
}

type usageTreeSessionFields struct {
	SessionID                 string  `json:"session_id"`
	ParentSessionID           *string `json:"parent_session_id"`
	TaskName                  *string `json:"task_name"`
	OperationalSessionPresent bool    `json:"operational_session_present"`
}

func (s UsageTreeSession) MarshalJSON() ([]byte, error) {
	fields, err := json.Marshal(usageTreeSessionFields{
		SessionID: s.SessionID, ParentSessionID: s.ParentSessionID,
		TaskName: s.TaskName, OperationalSessionPresent: s.OperationalSessionPresent,
	})
	if err != nil {
		return nil, err
	}
	summary, err := json.Marshal(s.Summary)
	if err != nil {
		return nil, err
	}
	return storageMergeJSONObjects(fields, summary)
}

func (s *UsageTreeSession) UnmarshalJSON(data []byte) error {
	var fields usageTreeSessionFields
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	var summary UsageSummary
	if err := json.Unmarshal(data, &summary); err != nil {
		return err
	}
	*s = UsageTreeSession{
		SessionID: fields.SessionID, ParentSessionID: fields.ParentSessionID,
		TaskName: fields.TaskName, OperationalSessionPresent: fields.OperationalSessionPresent,
		Summary: summary,
	}
	return nil
}

// UsageTree is the usage of a session and its descendants; the summary's
// members are flattened into the object.
type UsageTree struct {
	ParentSessionID string
	Sessions        []UsageTreeSession
	Summary         UsageSummary
}

type usageTreeFields struct {
	ParentSessionID string             `json:"parent_session_id"`
	Sessions        []UsageTreeSession `json:"sessions"`
}

func (t UsageTree) MarshalJSON() ([]byte, error) {
	fields, err := json.Marshal(usageTreeFields{
		ParentSessionID: t.ParentSessionID, Sessions: storageNonNilSlice(t.Sessions),
	})
	if err != nil {
		return nil, err
	}
	summary, err := json.Marshal(t.Summary)
	if err != nil {
		return nil, err
	}
	return storageMergeJSONObjects(fields, summary)
}

func (t *UsageTree) UnmarshalJSON(data []byte) error {
	var fields usageTreeFields
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	var summary UsageSummary
	if err := json.Unmarshal(data, &summary); err != nil {
		return err
	}
	*t = UsageTree{ParentSessionID: fields.ParentSessionID, Sessions: fields.Sessions, Summary: summary}
	return nil
}

// ApiEvent is one recorded API event; the event's `type` and `data` are
// flattened into the object.
type ApiEvent struct {
	Seq          uint64
	SessionID    string
	RecordedAtMs int64
	Event        ApiEventData
}

type apiEventFields struct {
	Seq          uint64 `json:"seq"`
	SessionID    string `json:"session_id"`
	RecordedAtMs int64  `json:"recorded_at_ms"`
}

func (e ApiEvent) MarshalJSON() ([]byte, error) {
	fields, err := json.Marshal(apiEventFields{Seq: e.Seq, SessionID: e.SessionID, RecordedAtMs: e.RecordedAtMs})
	if err != nil {
		return nil, err
	}
	event, err := MarshalApiEventData(e.Event)
	if err != nil {
		return nil, err
	}
	return storageMergeJSONObjects(fields, event)
}

func (e *ApiEvent) UnmarshalJSON(data []byte) error {
	var fields apiEventFields
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	event, err := UnmarshalApiEventData(data)
	if err != nil {
		return err
	}
	*e = ApiEvent{Seq: fields.Seq, SessionID: fields.SessionID, RecordedAtMs: fields.RecordedAtMs, Event: event}
	return nil
}

// ApiEventData is the payload of an API event. It serializes as serde's
// adjacently tagged enum: `{"type":"turn_started","data":{...}}`.
type ApiEventData interface {
	// Kind is the event's `type`.
	Kind() string
}

type ApiEventDataRuntimeResolved struct {
	Receipt RuntimeReceipt `json:"receipt"`
}

type ApiEventDataTurnStarted struct {
	Turn MaterializedTurn `json:"turn"`
}

type ApiEventDataTurnEnded struct {
	Turn ApiTurnOutcome `json:"turn"`
}

// ApiEventDataCommandEnded's data is the flattened command result.
type ApiEventDataCommandEnded struct {
	Result CommandResult
}

type ApiEventDataSessionFault struct {
	Reason    OutcomeReason `json:"reason"`
	Message   string        `json:"message"`
	CommandID *string       `json:"command_id"`
}

type ApiEventDataLegacyNotice struct {
	OriginalType string  `json:"original_type"`
	Message      string  `json:"message"`
	CommandID    *string `json:"command_id"`
}

type ApiEventDataInputRequired struct {
	// Request is absent when a completed turn asks for unstructured user
	// input.
	Request *ElicitationRequest `json:"request,omitempty"`
	TurnID  *uint64             `json:"turn_id"`
}

type ApiEventDataInputResolved struct {
	ElicitationID string  `json:"elicitation_id"`
	TurnID        *uint64 `json:"turn_id"`
	Action        string  `json:"action"`
}

type ApiEventDataActivityChanged struct {
	Activity ApiActivityState `json:"activity"`
}

func (ApiEventDataRuntimeResolved) Kind() string { return "runtime_resolved" }
func (ApiEventDataTurnStarted) Kind() string     { return "turn_started" }
func (ApiEventDataTurnEnded) Kind() string       { return "turn_ended" }
func (ApiEventDataCommandEnded) Kind() string    { return "command_ended" }
func (ApiEventDataSessionFault) Kind() string    { return "session_fault" }
func (ApiEventDataLegacyNotice) Kind() string    { return "legacy_notice" }
func (ApiEventDataInputRequired) Kind() string   { return "input_required" }
func (ApiEventDataInputResolved) Kind() string   { return "input_resolved" }
func (ApiEventDataActivityChanged) Kind() string { return "activity_changed" }

// MarshalJSON writes the flattened command result as the event's data.
func (d ApiEventDataCommandEnded) MarshalJSON() ([]byte, error) { return json.Marshal(d.Result) }

func (d *ApiEventDataCommandEnded) UnmarshalJSON(data []byte) error {
	return json.Unmarshal(data, &d.Result)
}

// MarshalApiEventData writes `{"type":<kind>,"data":<variant>}`.
func MarshalApiEventData(event ApiEventData) ([]byte, error) {
	data, err := json.Marshal(event)
	if err != nil {
		return nil, err
	}
	return json.Marshal(struct {
		Type string          `json:"type"`
		Data json.RawMessage `json:"data"`
	}{Type: event.Kind(), Data: data})
}

// UnmarshalApiEventData reads `{"type":<kind>,"data":<variant>}`.
func UnmarshalApiEventData(data []byte) (ApiEventData, error) {
	panic("unported: mj-core/src/storage.rs ApiEventData deserialize")
}

// ApiActivityState is the same structured facts rendered by the web and
// terminal UIs.
type ApiActivityState struct {
	State           string              `json:"state"`
	Details         *ApiActivityDetails `json:"details"`
	IsIdle          bool                `json:"is_idle"`
	WaitingForInput bool                `json:"waiting_for_input"`
	CapacityRetry   bool                `json:"capacity_retry"`
}

// ApiActivityDetails decodes with serde's deny_unknown_fields.
type ApiActivityDetails struct {
	Kind                  ApiActivityKind `json:"kind"`
	TurnStartedAtMs       *int64          `json:"turn_started_at_ms,omitempty"`
	StepStartedAtMs       *int64          `json:"step_started_at_ms,omitempty"`
	BackgroundStartedAtMs *int64          `json:"background_started_at_ms,omitempty"`
	IdleSinceMs           *int64          `json:"idle_since_ms,omitempty"`
	// LastActivityAtMs is when anything last arrived from the harness, while
	// a turn or a tool is in flight. A surface subtracts it from the current
	// time to show how long a running session has been quiet. Mjolnir never
	// ends a turn for this on its own; see activity's silence_note.
	LastActivityAtMs *int64  `json:"last_activity_at_ms,omitempty"`
	Label            *string `json:"label,omitempty"`
}

// UnmarshalJSON refuses unknown fields, as serde's deny_unknown_fields does.
func (d *ApiActivityDetails) UnmarshalJSON(data []byte) error {
	type plain ApiActivityDetails
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	var decoded plain
	if err := decoder.Decode(&decoded); err != nil {
		return err
	}
	*d = ApiActivityDetails(decoded)
	return nil
}

type ApiActivityKind string

const (
	ApiActivityKindTurn       ApiActivityKind = "turn"
	ApiActivityKindStep       ApiActivityKind = "step"
	ApiActivityKindBackground ApiActivityKind = "background"
	ApiActivityKindIdle       ApiActivityKind = "idle"
	ApiActivityKindLifecycle  ApiActivityKind = "lifecycle"
)

// UnmarshalJSON accepts only the kinds serde accepts.
func (k *ApiActivityKind) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch kind := ApiActivityKind(text); kind {
	case ApiActivityKindTurn, ApiActivityKindStep, ApiActivityKindBackground, ApiActivityKindIdle, ApiActivityKindLifecycle:
		*k = kind
		return nil
	}
	return fmt.Errorf("unknown variant `%s`, expected one of `turn`, `step`, `background`, `idle`, `lifecycle`", text)
}

type ApiEventFilter struct {
	SessionID   *string `json:"session_id"`
	WorkspaceID *string `json:"workspace_id"`
}

type ApiEventPage struct {
	Events       []ApiEvent `json:"events"`
	NextAfterSeq uint64     `json:"next_after_seq"`
	LatestSeq    uint64     `json:"latest_seq"`
}

func (p ApiEventPage) MarshalJSON() ([]byte, error) {
	type plain ApiEventPage
	p.Events = storageNonNilSlice(p.Events)
	return json.Marshal(plain(p))
}

// storageNonNilSlice is items, or an empty slice for nil, so it encodes as
// `[]` the way serde writes an empty Vec.
func storageNonNilSlice[T any](items []T) []T {
	if items == nil {
		return []T{}
	}
	return items
}

// storageNonNilMap is entries, or an empty map for nil, so it encodes as
// `{}` the way serde writes an empty map.
func storageNonNilMap[K comparable, V any](entries map[K]V) map[K]V {
	if entries == nil {
		return map[K]V{}
	}
	return entries
}

// storageMergeJSONObjects joins encoded JSON objects into one, members in
// order, as serde writes a struct with flattened fields.
func storageMergeJSONObjects(parts ...[]byte) ([]byte, error) {
	merged := []byte{'{'}
	for _, part := range parts {
		part = bytes.TrimSpace(part)
		if len(part) < 2 || part[0] != '{' || part[len(part)-1] != '}' {
			return nil, fmt.Errorf("flattened value is not a JSON object: %s", part)
		}
		members := bytes.TrimSpace(part[1 : len(part)-1])
		if len(members) == 0 {
			continue
		}
		if len(merged) > 1 {
			merged = append(merged, ',')
		}
		merged = append(merged, members...)
	}
	return append(merged, '}'), nil
}
