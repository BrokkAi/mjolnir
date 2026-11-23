// Port of mj-store/src/database/turn_reviews.rs.

// Durable review state and the finished report each review leaves behind.
//
// The review host owns every decision about a review, including which text
// settled a role. This module stores those decisions; it never re-derives one
// from another table, because the reviewer child's mutable rows are pruned
// when its role ends and can be rewritten by a later command.

package store

import (
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// AcceptedReport is one reviewing role's report, as the review host settled
// it.
//
// The host passes the exact text it accepted, so a later handback from the
// same child cannot become this review's output. It names the review, so a
// report cannot land on another open review that has a role of the same
// name.
type AcceptedReport struct {
	ReviewID string
	Role     string
	Text     string
}

// FinishedReview is one finished review and the output of each reviewing
// role. It is stored separately from the reviewer child sessions, which are
// destroyed as soon as their role is over.
type FinishedReview struct {
	SessionID    string                        `json:"session_id"`
	ReviewID     string                        `json:"review_id"`
	FinishedAtMs int64                         `json:"finished_at_ms"`
	Resolution   core.FinishedReviewResolution `json:"resolution"`
	Roles        []FinishedReviewRole          `json:"roles"`
}

// MarshalJSON writes a nil Roles as [] (a present, empty Vec).
func (r FinishedReview) MarshalJSON() ([]byte, error) {
	type plain FinishedReview
	out := plain(r)
	if out.Roles == nil {
		out.Roles = []FinishedReviewRole{}
	}
	return json.Marshal(out)
}

// UnmarshalJSON requires every field, as serde does.
func (r *FinishedReview) UnmarshalJSON(data []byte) error {
	var raw struct {
		SessionID    *string                        `json:"session_id"`
		ReviewID     *string                        `json:"review_id"`
		FinishedAtMs *int64                         `json:"finished_at_ms"`
		Resolution   *core.FinishedReviewResolution `json:"resolution"`
		Roles        *[]FinishedReviewRole          `json:"roles"`
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	switch {
	case raw.SessionID == nil:
		return errors.New("missing field `session_id`")
	case raw.ReviewID == nil:
		return errors.New("missing field `review_id`")
	case raw.FinishedAtMs == nil:
		return errors.New("missing field `finished_at_ms`")
	case raw.Resolution == nil:
		return errors.New("missing field `resolution`")
	case raw.Roles == nil:
		return errors.New("missing field `roles`")
	}
	*r = FinishedReview{
		SessionID:    *raw.SessionID,
		ReviewID:     *raw.ReviewID,
		FinishedAtMs: *raw.FinishedAtMs,
		Resolution:   *raw.Resolution,
		Roles:        *raw.Roles,
	}
	return nil
}

// FinishedReviewRole is one role's output from a finished review, including
// the turn it ran for usage. Turn is read from the accounting retained after
// session cleanup.
type FinishedReviewRole struct {
	Role           string  `json:"role"`
	Label          string  `json:"label"`
	CommandID      string  `json:"command_id"`
	ChildSessionID *string `json:"child_session_id,omitempty"`
	Profile        string  `json:"profile"`
	Model          *string `json:"model,omitempty"`
	Effort         *string `json:"effort,omitempty"`
	// Handback is the text this role's outcome was settled from, when it
	// handed back.
	Handback *string                       `json:"handback,omitempty"`
	Verdict  core.ReviewVerdict            `json:"verdict,omitempty"`
	Turn     *core.MaterializedTurnOutcome `json:"turn,omitempty"`
}

// UnmarshalJSON requires the fields serde requires and decodes the verdict
// through its owner's decoder.
func (r *FinishedReviewRole) UnmarshalJSON(data []byte) error {
	var raw struct {
		Role           *string                       `json:"role"`
		Label          *string                       `json:"label"`
		CommandID      *string                       `json:"command_id"`
		ChildSessionID *string                       `json:"child_session_id"`
		Profile        *string                       `json:"profile"`
		Model          *string                       `json:"model"`
		Effort         *string                       `json:"effort"`
		Handback       *string                       `json:"handback"`
		Verdict        json.RawMessage               `json:"verdict"`
		Turn           *core.MaterializedTurnOutcome `json:"turn"`
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	switch {
	case raw.Role == nil:
		return errors.New("missing field `role`")
	case raw.Label == nil:
		return errors.New("missing field `label`")
	case raw.CommandID == nil:
		return errors.New("missing field `command_id`")
	case raw.Profile == nil:
		return errors.New("missing field `profile`")
	}
	role := FinishedReviewRole{
		Role:           *raw.Role,
		Label:          *raw.Label,
		CommandID:      *raw.CommandID,
		ChildSessionID: raw.ChildSessionID,
		Profile:        *raw.Profile,
		Model:          raw.Model,
		Effort:         raw.Effort,
		Handback:       raw.Handback,
		Turn:           raw.Turn,
	}
	if raw.Verdict != nil && string(raw.Verdict) != "null" {
		verdict, err := core.UnmarshalReviewVerdict(raw.Verdict)
		if err != nil {
			return err
		}
		role.Verdict = verdict
	}
	*r = role
	return nil
}

// TurnReviewState is how far sessionID has been reviewed, or a fresh state
// when it has never been reviewed.
func TurnReviewState(sessionID string) (core.TurnReviewState, error) {
	return turnReviewStateIn(DatabasePath(), sessionID)
}

// ActiveReviewGroupState is a session with review groups whose reviewer
// workers must be reattached after daemon handoff (Rust's (String,
// TurnReviewState)).
type ActiveReviewGroupState struct {
	SessionID string
	State     core.TurnReviewState
}

// ActiveReviewGroups is the groups whose reviewer workers must be reattached
// after daemon handoff.
func ActiveReviewGroups() ([]ActiveReviewGroupState, error) {
	return activeReviewGroupsIn(DatabasePath())
}

func activeReviewGroupsIn(path string) ([]ActiveReviewGroupState, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	ids, err := valuesQueryMap(connection, `SELECT session_id FROM turn_review_state
             WHERE EXISTS(SELECT 1 FROM json_each(open_reviews)
                          WHERE json_extract(value, '$.group') IS NOT NULL)
             ORDER BY session_id`, nil, func(rows *sql.Rows) (string, error) {
		var id string
		err := rows.Scan(&id)
		return id, err
	})
	connection.Close()
	if err != nil {
		return nil, err
	}
	groups := make([]ActiveReviewGroupState, 0, len(ids))
	for _, id := range ids {
		state, err := turnReviewStateIn(path, id)
		if err != nil {
			return nil, err
		}
		groups = append(groups, ActiveReviewGroupState{SessionID: id, State: state})
	}
	return groups, nil
}

func turnReviewStateIn(path, sessionID string) (core.TurnReviewState, error) {
	connection, err := openReader(path)
	if err != nil {
		return core.TurnReviewState{}, err
	}
	defer connection.Close()
	var openReviews string
	var pushFindings, declinedFindings *string
	found, err := valuesOptional(connection.QueryRow("SELECT open_reviews, push_findings, declined_findings FROM turn_review_state WHERE session_id = ?1", sessionID).
		Scan(&openReviews, &pushFindings, &declinedFindings))
	if err != nil {
		return core.TurnReviewState{}, err
	}
	if !found {
		return core.TurnReviewState{}, nil
	}
	var state core.TurnReviewState
	if err := json.Unmarshal([]byte(openReviews), &state.OpenReviews); err != nil {
		return core.TurnReviewState{}, fmt.Errorf("parse stored open reviews: %w", err)
	}
	if pushFindings != nil {
		if err := json.Unmarshal([]byte(*pushFindings), &state.PushFindings); err != nil {
			return core.TurnReviewState{}, fmt.Errorf("parse stored push findings: %w", err)
		}
	}
	if declinedFindings != nil {
		if err := json.Unmarshal([]byte(*declinedFindings), &state.DeclinedFindings); err != nil {
			return core.TurnReviewState{}, fmt.Errorf("parse stored declined findings: %w", err)
		}
	}
	return state, nil
}

// storedOpenReviews is the stored open_reviews list for state: the reviews
// the row still has something to say about.
func storedOpenReviews(state *core.TurnReviewState) (string, error) {
	open := make([]*core.OpenReview, 0, len(state.OpenReviews))
	for index := range state.OpenReviews {
		if state.OpenReviews[index].IsStored() {
			open = append(open, &state.OpenReviews[index])
		}
	}
	encoded, err := databaseMarshalJSON(open)
	if err != nil {
		return "", err
	}
	return string(encoded), nil
}

// SaveTurnReviewState records review state and atomically queues newly
// settled findings while other reviewers are pending.
func SaveTurnReviewState(sessionID string, state *core.TurnReviewState) error {
	_, err := submitDatabaseWrite("save_turn_review_state", func(*Connection) (struct{}, error) {
		return struct{}{}, saveTurnReviewStateIn(DatabasePath(), sessionID, state)
	})
	return err
}

// SaveReviewerOutcome records review state together with the reports the
// host settled roles from. A save that carries no reports keeps the text
// already stored.
func SaveReviewerOutcome(sessionID string, state *core.TurnReviewState, reports []AcceptedReport) error {
	_, err := submitDatabaseWrite("save_reviewer_outcome", func(*Connection) (struct{}, error) {
		return struct{}{}, saveReviewerOutcomeIn(DatabasePath(), sessionID, state, reports)
	})
	return err
}

// CloseTurnReviewState clears the review reviewID from the row and records
// its per-role output in the same transaction. The resolution is explicit
// because a cancelled review's stored group can still carry the verdict it
// was about to deliver.
func CloseTurnReviewState(sessionID, reviewID string, state *core.TurnReviewState, resolution core.FinishedReviewResolution) error {
	_, err := submitDatabaseWrite("close_turn_review_state", func(*Connection) (struct{}, error) {
		return struct{}{}, closeTurnReviewStateIn(DatabasePath(), sessionID, reviewID, state, resolution)
	})
	return err
}

func saveTurnReviewStateIn(path, sessionID string, state *core.TurnReviewState) error {
	return saveTurnReviewStateInWith(path, sessionID, state, nil, nil)
}

// saveReviewerOutcomeIn is SaveReviewerOutcome at an explicit store path.
func saveReviewerOutcomeIn(path, sessionID string, state *core.TurnReviewState, reports []AcceptedReport) error {
	return saveTurnReviewStateInWith(path, sessionID, state, nil, reports)
}

func closeTurnReviewStateIn(path, sessionID, reviewID string, state *core.TurnReviewState, resolution core.FinishedReviewResolution) error {
	return saveTurnReviewStateInWith(path, sessionID, state, &reviewClose{reviewID: reviewID, resolution: resolution}, nil)
}

// reviewClose is the review a save closes and how it ended.
type reviewClose struct {
	reviewID   string
	resolution core.FinishedReviewResolution
}

func saveTurnReviewStateInWith(path, sessionID string, state *core.TurnReviewState, close *reviewClose, reports []AcceptedReport) error {
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	transaction, err := connection.Begin()
	if err != nil {
		return err
	}
	defer transaction.Rollback()
	previous, err := previousActiveGroups(transaction, sessionID)
	if err != nil {
		return err
	}
	var events []core.MailboxEvent
	for index := range state.OpenReviews {
		current := state.OpenReviews[index].ActiveGroup
		if current == nil {
			continue
		}
		var prior *core.ActiveReviewGroup
		for groupIndex := range previous {
			if previous[groupIndex].Driver.ReviewID == current.Driver.ReviewID {
				prior = &previous[groupIndex]
				break
			}
		}
		events = append(events, PartialReviewEvents(sessionID, prior, current)...)
	}
	if err := recordReviewReports(transaction, sessionID, previous, state, close, reports); err != nil {
		return err
	}
	if err := saveTurnReviewStateWith(transaction, sessionID, state); err != nil {
		return err
	}
	queued := false
	if len(events) > 0 {
		var unpark bool
		if err := transaction.QueryRow("SELECT EXISTS(SELECT 1 FROM subagent_sessions WHERE child_session_id=?1)", sessionID).Scan(&unpark); err != nil {
			return err
		}
		for _, event := range events {
			encoded, err := databaseMarshalJSON(event)
			if err != nil {
				return err
			}
			enqueued, err := EnqueueMailboxEventWith(transaction, event.Key, sessionID, string(encoded), event.Wake, unpark)
			if err != nil {
				return err
			}
			queued = queued || enqueued.Inserted()
		}
	}
	if err := transaction.Commit(); err != nil {
		return err
	}
	if queued {
		NotifyMailboxOutboxChanged()
	}
	return nil
}

func previousActiveGroups(conn Queryer, sessionID string) ([]core.ActiveReviewGroup, error) {
	var previous string
	found, err := valuesOptional(conn.QueryRow("SELECT open_reviews FROM turn_review_state WHERE session_id=?1", sessionID).Scan(&previous))
	if err != nil {
		return nil, err
	}
	if !found {
		return []core.ActiveReviewGroup{}, nil
	}
	var reviews []core.OpenReview
	if err := json.Unmarshal([]byte(previous), &reviews); err != nil {
		return nil, fmt.Errorf("parse previous open reviews: %w", err)
	}
	groups := []core.ActiveReviewGroup{}
	for _, review := range reviews {
		if review.ActiveGroup != nil {
			groups = append(groups, *review.ActiveGroup)
		}
	}
	return groups, nil
}

// recordReviewReports keeps the durable per-role report in step with review
// state.
//
// reports carries the text the host settled each role from, with the review
// it belongs to. The store writes exactly that and nothing else: it never
// reads the reviewer child's mutable handback row, whose latest entry can
// belong to another command and would pair unrelated text with a settled
// verdict.
func recordReviewReports(conn Queryer, sessionID string, previous []core.ActiveReviewGroup, state *core.TurnReviewState, close *reviewClose, reports []AcceptedReport) error {
	var current []*core.ActiveReviewGroup
	for index := range state.OpenReviews {
		if group := state.OpenReviews[index].ActiveGroup; group != nil {
			current = append(current, group)
		}
	}
	// A stored group is finished when the saved state no longer lists it. A
	// close decision describes the review it names; a group that left without
	// being named, such as one a restart abandoned, has only its own driver
	// to answer.
	for index := range previous {
		prior := &previous[index]
		stillOpen := false
		for _, group := range current {
			if group.Driver.ReviewID == prior.Driver.ReviewID {
				stillOpen = true
				break
			}
		}
		if stillOpen {
			continue
		}
		if err := syncReviewReport(conn, sessionID, prior, nil); err != nil {
			return err
		}
		if err := finalizeReviewReport(conn, sessionID, prior, previousResolution(prior, close)); err != nil {
			return err
		}
	}
	for _, group := range current {
		var settled []*AcceptedReport
		for index := range reports {
			if reports[index].ReviewID == group.Driver.ReviewID {
				settled = append(settled, &reports[index])
			}
		}
		if err := syncReviewReport(conn, sessionID, group, settled); err != nil {
			return err
		}
		// The close decision, not the presence of an earlier row, decides
		// that this review is finished: a first save may already be it.
		if close != nil && close.reviewID == group.Driver.ReviewID {
			if err := finalizeReviewReport(conn, sessionID, group, close.resolution); err != nil {
				return err
			}
		}
	}
	return nil
}

// previousResolution is the resolution for a review that is being cleared.
// The host's explicit decision wins: a cancelled review's stored group can
// still carry the verdict it was about to deliver, and a delivery failure can
// arrive after a clean verdict was recorded. Only a close with no decision,
// which no production path produces, reads the driver's own outcome.
func previousResolution(previous *core.ActiveReviewGroup, close *reviewClose) core.FinishedReviewResolution {
	if close != nil && close.reviewID == previous.Driver.ReviewID {
		return close.resolution
	}
	return inferredResolution(previous)
}

// inferredResolution reads a review whose state is cleared without an
// explicit close: a delivered verdict means completed or failed, and no
// verdict means the review was cancelled.
func inferredResolution(group *core.ActiveReviewGroup) core.FinishedReviewResolution {
	if resolved, ok := group.Driver.Phase.(core.TurnReviewPhaseResolved); ok && resolved.Resolution == core.ResolutionCancelled {
		return core.FinishedReviewResolutionCancelled
	}
	switch group.Driver.LastVerdict.(type) {
	case nil:
		return core.FinishedReviewResolutionCancelled
	case core.ReviewVerdictFailed:
		return core.FinishedReviewResolutionFailed
	default:
		return core.FinishedReviewResolutionCompleted
	}
}

func syncReviewReport(conn Queryer, sessionID string, group *core.ActiveReviewGroup, reports []*AcceptedReport) error {
	now := core.EpochMillis()
	for ordinal, slot := range group.Driver.Roles {
		var child *string
		for _, registration := range group.Registrations {
			if registration.Role == slot.Role {
				if registration.ChildSessionID != nil && *registration.ChildSessionID != "" {
					child = registration.ChildSessionID
				}
				break
			}
		}
		profile := ""
		var model, effort *string
		if ordinal < len(group.Reviewers) {
			reviewer := group.Reviewers[ordinal]
			profile, model, effort = reviewer.Profile, reviewer.Main.Model, reviewer.Main.Effort
		}
		var handback *string
		for _, report := range reports {
			if report.Role == slot.Role {
				handback = &report.Text
				break
			}
		}
		var verdict *string
		if slot.Outcome != nil {
			encoded, err := databaseMarshalJSON(slot.Outcome)
			if err != nil {
				return err
			}
			text := string(encoded)
			verdict = &text
		}
		if _, err := conn.Exec(`INSERT INTO review_reports(
                 parent_session_id, review_id, role, ordinal, command_id,
                 child_session_id, profile, model, effort, handback, verdict,
                 updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(parent_session_id, review_id, role) DO UPDATE SET
                 ordinal = excluded.ordinal,
                 command_id = excluded.command_id,
                 child_session_id = COALESCE(excluded.child_session_id, review_reports.child_session_id),
                 profile = CASE
                     WHEN excluded.profile <> '' THEN excluded.profile
                     ELSE review_reports.profile
                 END,
                 model = COALESCE(excluded.model, review_reports.model),
                 effort = COALESCE(excluded.effort, review_reports.effort),
                 handback = COALESCE(excluded.handback, review_reports.handback),
                 verdict = COALESCE(excluded.verdict, review_reports.verdict),
                 updated_at_ms = excluded.updated_at_ms`,
			sessionID, group.Driver.ReviewID, slot.Role, int64(ordinal), slot.CommandID,
			child, profile, model, effort, handback, verdict, now); err != nil {
			return err
		}
	}
	return nil
}

func finalizeReviewReport(conn Queryer, sessionID string, group *core.ActiveReviewGroup, resolution core.FinishedReviewResolution) error {
	_, err := conn.Exec(`UPDATE review_reports SET finished_at_ms=?3, resolution=?4
         WHERE parent_session_id=?1 AND review_id=?2 AND finished_at_ms IS NULL`,
		sessionID, group.Driver.ReviewID, core.EpochMillis(), resolutionText(resolution))
	return err
}

func resolutionText(resolution core.FinishedReviewResolution) string {
	switch resolution {
	case core.FinishedReviewResolutionCompleted:
		return "completed"
	case core.FinishedReviewResolutionFailed:
		return "failed"
	case core.FinishedReviewResolutionCancelled:
		return "cancelled"
	}
	panic(fmt.Sprintf("unknown review resolution %q", string(resolution)))
}

func resolutionFromText(text string) (core.FinishedReviewResolution, error) {
	switch text {
	case "completed":
		return core.FinishedReviewResolutionCompleted, nil
	case "failed":
		return core.FinishedReviewResolutionFailed, nil
	case "cancelled":
		return core.FinishedReviewResolutionCancelled, nil
	}
	return "", fmt.Errorf("invalid review report resolution %q", text)
}

// FinishedReviewOf is the latest finished review for sessionID, or the one
// reviewID names (Rust's finished_review, named apart from the type).
func FinishedReviewOf(sessionID string, reviewID *string) (*FinishedReview, error) {
	return finishedReviewIn(DatabasePath(), sessionID, reviewID)
}

func finishedReviewIn(path, sessionID string, reviewID *string) (*FinishedReview, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	review := &FinishedReview{SessionID: sessionID}
	var resolution string
	found, err := valuesOptional(connection.QueryRow(`SELECT review_id, MAX(finished_at_ms), resolution
             FROM review_reports
             WHERE parent_session_id=?1
               AND finished_at_ms IS NOT NULL
               AND (?2 IS NULL OR review_id=?2)
             GROUP BY review_id
             ORDER BY MAX(finished_at_ms) DESC, review_id DESC
             LIMIT 1`, sessionID, reviewID).Scan(&review.ReviewID, &review.FinishedAtMs, &resolution))
	if err != nil || !found {
		return nil, err
	}
	type roleRow struct {
		role, commandID, profile                string
		ordinal                                 int64
		child, model, effort, handback, verdict *string
	}
	rows, err := valuesQueryMap(connection, `SELECT role, ordinal, command_id, child_session_id, profile, model,
                effort, handback, verdict
         FROM review_reports
         WHERE parent_session_id=?1 AND review_id=?2
         ORDER BY ordinal, role`, []any{sessionID, review.ReviewID}, func(rows *sql.Rows) (roleRow, error) {
		var row roleRow
		err := rows.Scan(&row.role, &row.ordinal, &row.commandID, &row.child, &row.profile, &row.model,
			&row.effort, &row.handback, &row.verdict)
		return row, err
	})
	if err != nil {
		return nil, err
	}
	review.Roles = []FinishedReviewRole{}
	for _, row := range rows {
		role := FinishedReviewRole{
			Role:           row.role,
			Label:          fmt.Sprintf("Reviewer %d", row.ordinal+1),
			CommandID:      row.commandID,
			ChildSessionID: row.child,
			Profile:        row.profile,
			Model:          row.model,
			Effort:         row.effort,
			Handback:       row.handback,
		}
		if row.verdict != nil {
			verdict, err := core.UnmarshalReviewVerdict([]byte(*row.verdict))
			if err != nil {
				return nil, fmt.Errorf("parse stored reviewer verdict: %w", err)
			}
			role.Verdict = verdict
		}
		if role.Turn, err = reviewerTurn(connection, row.child); err != nil {
			return nil, err
		}
		review.Roles = append(review.Roles, role)
	}
	if review.Resolution, err = resolutionFromText(resolution); err != nil {
		return nil, err
	}
	return review, nil
}

// reviewerTurn is the one review turn a reviewer child ran. Accounting
// outlives the child's session row, which is why this is still readable
// after the child is destroyed.
func reviewerTurn(conn Queryer, child *string) (*core.MaterializedTurnOutcome, error) {
	if child == nil {
		return nil, nil
	}
	var body string
	found, err := valuesOptional(conn.QueryRow(`SELECT body FROM session_turn_usage
             WHERE session_id=?1
             ORDER BY completed_ordinal DESC
             LIMIT 1`, *child).Scan(&body))
	if err != nil || !found {
		return nil, err
	}
	turn, err := deserializeTurnOutcome(body)
	if err != nil {
		return nil, err
	}
	return &turn, nil
}

func saveTurnReviewStateWith(conn Queryer, sessionID string, state *core.TurnReviewState) error {
	openReviews, err := storedOpenReviews(state)
	if err != nil {
		return err
	}
	var pushFindings, declinedFindings *string
	if len(state.PushFindings) > 0 {
		encoded, err := databaseMarshalJSON(state.PushFindings)
		if err != nil {
			return err
		}
		text := string(encoded)
		pushFindings = &text
	}
	if len(state.DeclinedFindings) > 0 {
		encoded, err := databaseMarshalJSON(state.DeclinedFindings)
		if err != nil {
			return err
		}
		text := string(encoded)
		declinedFindings = &text
	}
	_, err = conn.Exec(`INSERT INTO turn_review_state(
             session_id, baselines, reviewed_through_ordinal, open_reviews,
             pending_forward, child_session_id, push_findings, pending_followup, forward_attempts,
             declined_findings
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(session_id) DO UPDATE SET
             baselines = excluded.baselines,
             reviewed_through_ordinal = excluded.reviewed_through_ordinal,
             open_reviews = excluded.open_reviews,
             pending_forward = excluded.pending_forward,
             child_session_id = excluded.child_session_id,
             push_findings = excluded.push_findings,
             pending_followup = 0,
             forward_attempts = excluded.forward_attempts,
             declined_findings = excluded.declined_findings`,
		sessionID, "{}", 0, openReviews, nil, nil, pushFindings, 0, 0, declinedFindings)
	return err
}

// ClearInterruptedTurnReviews clears legacy interrupted reviews while
// retaining durable reviewer groups.
//
// Durable push groups resume independently. Preparation markers without a
// group are cleared and reported so the user can request review again.
func ClearInterruptedTurnReviews() ([]string, error) {
	return submitDatabaseWrite("clear_interrupted_turn_reviews", func(*Connection) ([]string, error) {
		return clearInterruptedTurnReviewsIn(DatabasePath())
	})
}

func clearInterruptedTurnReviewsIn(path string) ([]string, error) {
	connection, err := open(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	interrupted, err := valuesQueryMap(connection, `SELECT session_id FROM turn_review_state
                 WHERE pending_forward IS NOT NULL
                    OR EXISTS(SELECT 1 FROM json_each(open_reviews)
                              WHERE json_extract(value, '$.group') IS NULL)`, nil, func(rows *sql.Rows) (string, error) {
		var id string
		err := rows.Scan(&id)
		return id, err
	})
	if err != nil {
		return nil, err
	}
	if interrupted == nil {
		interrupted = []string{}
	}
	if _, err := connection.Exec(`UPDATE turn_review_state
             SET open_reviews = (SELECT json_group_array(json(value)) FROM json_each(open_reviews)
                                 WHERE json_extract(value, '$.group') IS NOT NULL)
             WHERE EXISTS(SELECT 1 FROM json_each(open_reviews)
                          WHERE json_extract(value, '$.group') IS NULL)`); err != nil {
		return nil, err
	}
	return interrupted, nil
}
