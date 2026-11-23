// Port of mj-store/src/database/materialized.rs.

package store

import (
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"iter"
	"log/slog"
	"maps"
	"math"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// LoadMaterializedSession loads a session's whole projection, transcript and
// all.
//
// Crate-private on purpose in Rust. The cost of this call is everything that
// has ever happened in the conversation, and the callers that made that a
// visible problem — the runtime poll and the resume reply — were both outside
// this crate. What they wanted was LoadMaterializedProjectionTail; what they
// reached for was this, because it was public and its name did not say
// otherwise. The remaining caller owns a live projection and genuinely needs
// all of it.
func LoadMaterializedSession(sessionID string) (*core.MaterializedSession, error) {
	return LoadMaterializedSessionFrom(DatabasePath(), sessionID)
}

// LoadMaterializedSessionSummary loads only the projection fields needed by
// dashboard session summaries. Transcript bodies for tools, plans, thoughts,
// and old messages stay in SQLite, which keeps dashboard startup independent
// of transcript size.
func LoadMaterializedSessionSummary(sessionID string) (*core.MaterializedSessionSummary, error) {
	return loadMaterializedSessionSummaryFrom(DatabasePath(), sessionID)
}

// MaterializedActivity is one session's durable activity watermark and
// execution state: Rust's `(Option<i64>, MaterializedExecutionState)`.
type MaterializedActivity struct {
	LastActivityAtMs *int64
	Execution        core.MaterializedExecutionState
}

// LoadMaterializedActivity loads durable activity for every materialized
// session so startup does not read transcripts.
func LoadMaterializedActivity() (map[string]MaterializedActivity, error) {
	return loadMaterializedActivityFrom(DatabasePath())
}

// openReadSnapshot opens a reader on path and begins its read transaction:
// Rust's `open_reader(path)?.transaction()?`. done rolls the snapshot back
// and returns the reader.
func openReadSnapshot(path string) (*sql.Tx, func(), error) {
	reader, err := openReader(path)
	if err != nil {
		return nil, nil, err
	}
	connection, err := reader.Begin()
	if err != nil {
		reader.Close()
		return nil, nil, err
	}
	return connection, func() {
		_ = connection.Rollback()
		reader.Close()
	}, nil
}

func loadMaterializedActivityFrom(path string) (map[string]MaterializedActivity, error) {
	connection, done, err := openReadSnapshot(path)
	if err != nil {
		return nil, err
	}
	defer done()
	type stored struct {
		sessionID          string
		lastActivityAtMs   *int64
		executionState     string
		runningStartedAtMs *int64
	}
	var rows []stored
	err = func() error {
		result, err := connection.Query(`SELECT session_id, last_activity_at_ms, execution_state, running_started_at_ms
         FROM materialized_sessions`)
		if err != nil {
			return err
		}
		defer result.Close()
		for result.Next() {
			var row stored
			if err := result.Scan(&row.sessionID, &row.lastActivityAtMs, &row.executionState, &row.runningStartedAtMs); err != nil {
				return err
			}
			rows = append(rows, row)
		}
		return result.Err()
	}()
	if err != nil {
		return nil, err
	}
	activity := make(map[string]MaterializedActivity, len(rows))
	for _, row := range rows {
		execution, err := parseMaterializedExecution(row.executionState, row.runningStartedAtMs)
		if err != nil {
			return nil, err
		}
		activity[row.sessionID] = MaterializedActivity{LastActivityAtMs: row.lastActivityAtMs, Execution: execution}
	}
	return activity, nil
}

func loadMaterializedSessionSummaryFrom(path string, sessionID string) (*core.MaterializedSessionSummary, error) {
	connection, done, err := openReadSnapshot(path)
	if err != nil {
		return nil, err
	}
	defer done()
	var appliedEventOrdinal uint64
	var lastActivityAtMs, runningStartedAtMs *int64
	var execution string
	var sessionTitle *string
	err = connection.QueryRow(`SELECT applied_event_ordinal, last_activity_at_ms, execution_state,
                    running_started_at_ms, session_title
             FROM materialized_sessions WHERE session_id = ?1`, sessionID).
		Scan(&appliedEventOrdinal, &lastActivityAtMs, &execution, &runningStartedAtMs, &sessionTitle)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	// Rust's cfg(test) hook between the frontier and transcript reads.
	afterMaterializedFrontierRead()
	// `/clear` starts a new conversation; the summary describes only that
	// conversation, never the one before the boundary.
	conversationStart, err := lastMaterializedContextBoundary(connection, sessionID)
	if err != nil {
		return nil, err
	}
	userPosition, userText, hasUser, err := lastMaterializedUserMessage(connection, sessionID)
	if err != nil {
		return nil, err
	}
	if hasUser && conversationStart != nil && userPosition <= *conversationStart {
		hasUser = false
	}
	after := uint64(0)
	if conversationStart != nil {
		after = *conversationStart
	}
	agentPosition, agentText, hasAgent, err := lastMaterializedAgentMessageIn(connection, sessionID, after, nil, nil)
	if err != nil {
		return nil, err
	}
	followsUser := hasAgent && (!hasUser || agentPosition > userPosition)
	agentOrdinals, interruptionOrdinals, err := readMaterializedUnreadOrdinals(connection, sessionID)
	if err != nil {
		return nil, err
	}
	parsed, err := parseMaterializedExecution(execution, runningStartedAtMs)
	if err != nil {
		return nil, err
	}
	summary := &core.MaterializedSessionSummary{
		SessionID:                         sessionID,
		AppliedEventOrdinal:               appliedEventOrdinal,
		LastActivityAtMs:                  lastActivityAtMs,
		Execution:                         parsed,
		SessionTitle:                      sessionTitle,
		LastAgentMessageFollowsLastUser:   followsUser,
		AgentMessageLatestContentOrdinals: agentOrdinals,
		InterruptionEventOrdinals:         interruptionOrdinals,
	}
	if hasAgent {
		summary.LastAgentMessage = &agentText
	}
	if hasUser {
		summary.LastUserMessage = &userText
	}
	return summary, nil
}

// queryUnsignedColumn collects the single unsigned integer column of every row.
func queryUnsignedColumn(connection Queryer, query string, args ...any) ([]uint64, error) {
	rows, err := connection.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	values := []uint64{}
	for rows.Next() {
		var value uint64
		if err := rows.Scan(&value); err != nil {
			return nil, err
		}
		values = append(values, value)
	}
	return values, rows.Err()
}

// readMaterializedUnreadOrdinals reads the summary's ordinal facts without
// deserializing transcript bodies: the agent messages' latest content
// ordinals, then the interruption event ordinals.
func readMaterializedUnreadOrdinals(connection Queryer, sessionID string) ([]uint64, []uint64, error) {
	agentOrdinals, err := queryUnsignedColumn(connection, `SELECT latest_content_event_ordinal
         FROM materialized_transcript_items
         WHERE session_id = ?1
           AND latest_content_event_ordinal IS NOT NULL
           AND EXISTS (
               SELECT 1 FROM json_each(
                   CASE
                       WHEN latest_content_event_ordinal IS NOT NULL
                           AND json_valid(body_json)
                       THEN body_json
                       ELSE '{}'
                   END,
                   '$.chunks'
               ) AS chunk
               WHERE json_extract(chunk.value, '$.content.type') IS NOT NULL
                 AND (
                     json_extract(chunk.value, '$.content.type') <> 'text'
                     OR trim(coalesce(json_extract(chunk.value, '$.content.text'), '')) <> ''
                 )
           )
         ORDER BY position, stable_id`, sessionID)
	if err != nil {
		return nil, nil, err
	}
	interruptions, err := queryUnsignedColumn(connection, `SELECT position
         FROM materialized_transcript_items
         WHERE session_id = ?1 AND stable_id GLOB ?2
         ORDER BY position, stable_id`, sessionID, core.WorkInterruptedItemPrefix+"*")
	if err != nil {
		return nil, nil, err
	}
	var outcome *string
	if err := connection.QueryRow("SELECT last_turn_outcome_json FROM materialized_sessions WHERE session_id = ?1", sessionID).Scan(&outcome); err != nil {
		return nil, nil, err
	}
	if outcome != nil {
		turn, err := deserializeTurnOutcome(*outcome)
		if err != nil {
			return nil, nil, err
		}
		if ordinal, ok := turn.InterruptionOrdinal(); ok {
			interruptions = append(interruptions, ordinal)
		}
	}
	slices.Sort(interruptions)
	interruptions = slices.Compact(interruptions)
	return agentOrdinals, interruptions, nil
}

func readLatestUnreadEventOrdinal(connection Queryer, sessionID string) (*uint64, error) {
	agent, interruptions, err := readMaterializedUnreadOrdinals(connection, sessionID)
	if err != nil {
		return nil, err
	}
	var latest *uint64
	for _, ordinal := range append(agent, interruptions...) {
		if latest == nil || ordinal > *latest {
			value := ordinal
			latest = &value
		}
	}
	return latest, nil
}

// firstMaterializedUserMessage is the oldest visible user message, which is
// where a session's provisional title comes from. It sits at the head of the
// transcript, so a projection loaded as a tail cannot find it by scanning;
// this reads it directly. It returns the item's position and text.
func firstMaterializedUserMessage(connection Queryer, sessionID string) (uint64, string, bool, error) {
	return materializedUserMessage(connection, sessionID, true)
}

func lastMaterializedUserMessage(connection Queryer, sessionID string) (uint64, string, bool, error) {
	return materializedUserMessage(connection, sessionID, false)
}

func materializedUserMessage(connection Queryer, sessionID string, oldestFirst bool) (uint64, string, bool, error) {
	order := "ORDER BY position DESC, stable_id DESC"
	if oldestFirst {
		order = "ORDER BY position, stable_id"
	}
	rows, err := connection.Query(`SELECT position, body_json
         FROM materialized_transcript_items
         WHERE session_id = ?1
           AND json_extract(
               CASE
                   WHEN stable_id GLOB 'user:*' OR stable_id GLOB 'user-*'
                   THEN body_json
                   ELSE '{}'
               END,
               '$.kind'
           ) = 'user'
         `+order, sessionID)
	if err != nil {
		return 0, "", false, err
	}
	defer rows.Close()
	for rows.Next() {
		var position uint64
		var bodyJSON string
		if err := rows.Scan(&position, &bodyJSON); err != nil {
			return 0, "", false, err
		}
		body, err := core.DecodeTranscriptBody([]byte(bodyJSON))
		if err != nil {
			return 0, "", false, fmt.Errorf("parse materialized user message for session %s: %w", sessionID, err)
		}
		user, ok := body.(core.TranscriptBodyUser)
		if !ok {
			continue
		}
		text := core.MaterializedContentText(user.Content)
		if !isRustBlank(text) {
			return position, text, true, nil
		}
	}
	return 0, "", false, rows.Err()
}

// lastMaterializedContextBoundary is where the newest /clear boundary sits,
// if the session has one.
func lastMaterializedContextBoundary(connection Queryer, sessionID string) (*uint64, error) {
	var position *uint64
	err := connection.QueryRow(`SELECT MAX(position) FROM materialized_transcript_items
         WHERE session_id = ?1 AND stable_id GLOB ?2`, sessionID, core.ContextBoundaryPrefix+"*").Scan(&position)
	return position, err
}

// turnStartSQL is the SQL form of TranscriptItem::is_turn_start: a user
// prompt (a user item that is not an elicitation reply) or a harness-started
// turn marker. Every store query that finds turn boundaries uses this, so
// loaded windows agree with ProjectionWindow::trim.
func turnStartSQL() string {
	return fmt.Sprintf(
		"(stable_id GLOB '%s*' OR (stable_id NOT GLOB '%s*' AND item_kind='user'))",
		core.HarnessTurnItemPrefix,
		core.ElicitationReplyItemPrefix,
	)
}

// lastMaterializedTurnStart is where the newest turn began: a user message,
// or the marker for a turn the harness started on its own. This is the
// recovery boundary; it reads a position only, scanning back from the newest
// item.
func lastMaterializedTurnStart(connection Queryer, sessionID string) (*uint64, error) {
	var position uint64
	err := connection.QueryRow(fmt.Sprintf(`SELECT position FROM materialized_transcript_items
                 WHERE session_id = ?1 AND %s
                 ORDER BY position DESC, stable_id DESC
                 LIMIT 1`, turnStartSQL()), sessionID).Scan(&position)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	return &position, nil
}

// lastMaterializedAgentMessageWithin is the newest nonempty agent message
// inside one turn, flattened to text.
//
// The turn is a span, not a starting point. A harness can put an agent
// message in the transcript after the turn it answered has ended — a resume
// notice is one — and reading everything after the turn's start would return
// that notice as the turn's answer.
func lastMaterializedAgentMessageWithin(connection Queryer, sessionID string, afterPosition uint64, throughPosition uint64) (*string, error) {
	_, text, ok, err := lastMaterializedAgentMessageIn(connection, sessionID, afterPosition, &throughPosition, nil)
	if err != nil || !ok {
		return nil, err
	}
	return &text, nil
}

func lastMaterializedAgentMessageIn(connection Queryer, sessionID string, afterPosition uint64, throughPosition *uint64, throughOrdinal *uint64) (uint64, string, bool, error) {
	item, err := lastMaterializedAgentItemIn(connection, sessionID, afterPosition, throughPosition, throughOrdinal)
	if err != nil || item == nil {
		return 0, "", false, err
	}
	agent, ok := item.Body.(core.TranscriptBodyAgent)
	if !ok {
		return 0, "", false, nil
	}
	text := core.MaterializedChunksText(agent.Chunks)
	if isRustBlank(text) {
		return 0, "", false, nil
	}
	return item.Position, text, true, nil
}

// scanStoredTranscriptItem reads the stable_id, position,
// latest_content_event_ordinal, created_at_ms, last_changed_at_ms and
// body_json columns of a transcript row.
func scanStoredTranscriptItem(scan func(dest ...any) error, sessionID string) (*core.TranscriptItem, error) {
	item := new(core.TranscriptItem)
	var body string
	if err := scan(&item.StableID, &item.Position, &item.LatestContentEventOrdinal, &item.CreatedAtMs, &item.LastChangedAtMs, &body); err != nil {
		return nil, err
	}
	decoded, err := decodeTranscriptBody(body, sessionID)
	if err != nil {
		return nil, err
	}
	item.Body = decoded
	return item, nil
}

// queryStoredTranscriptItems decodes every row of a query selecting the transcript
// item columns scanStoredTranscriptItem reads, in order.
func queryStoredTranscriptItems(connection Queryer, sessionID string, query string, args ...any) ([]*core.TranscriptItem, error) {
	rows, err := connection.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	type stored struct {
		item core.TranscriptItem
		body string
	}
	var bodies []stored
	for rows.Next() {
		var row stored
		if err := rows.Scan(&row.item.StableID, &row.item.Position, &row.item.LatestContentEventOrdinal,
			&row.item.CreatedAtMs, &row.item.LastChangedAtMs, &row.body); err != nil {
			return nil, err
		}
		bodies = append(bodies, row)
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}
	items := make([]*core.TranscriptItem, 0, len(bodies))
	for i := range bodies {
		body, err := decodeTranscriptBody(bodies[i].body, sessionID)
		if err != nil {
			return nil, err
		}
		item := bodies[i].item
		item.Body = body
		items = append(items, &item)
	}
	return items, nil
}

func lastMaterializedAgentItemIn(connection Queryer, sessionID string, afterPosition uint64, throughPosition *uint64, throughOrdinal *uint64) (*core.TranscriptItem, error) {
	item, err := scanStoredTranscriptItem(connection.QueryRow(`SELECT stable_id, position, latest_content_event_ordinal, created_at_ms, last_changed_at_ms, body_json
             FROM materialized_transcript_items
             WHERE session_id = ?1
               AND position > ?2
               AND (?3 IS NULL OR position <= ?3)
               AND (?4 IS NULL OR latest_content_event_ordinal <= ?4)
               AND latest_content_event_ordinal IS NOT NULL
               AND EXISTS (
                   SELECT 1 FROM json_each(
                       CASE
                           WHEN latest_content_event_ordinal IS NOT NULL
                               AND json_valid(body_json)
                           THEN body_json
                           ELSE '{}'
                       END,
                       '$.chunks'
                   ) AS chunk
                   WHERE json_extract(chunk.value, '$.content.type') IS NOT NULL
                     AND (
                         json_extract(chunk.value, '$.content.type') <> 'text'
                         OR trim(coalesce(json_extract(chunk.value, '$.content.text'), '')) <> ''
                     )
               )
             ORDER BY position DESC, stable_id DESC
             LIMIT 1`, sessionID, afterPosition, throughPosition, throughOrdinal).Scan, sessionID)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	return item, err
}

// MaterializedTurnState is the execution state, the running turn, and the
// last finished turn's outcome.
type MaterializedTurnState struct {
	Execution       core.MaterializedExecutionState
	ActiveTurn      *core.MaterializedTurn
	LastTurnOutcome *core.MaterializedTurnOutcome
}

// LoadMaterializedTurnOutcome is where a session stands turn by turn: what is
// running now, and how the last finished prompt ended. Returns nil when the
// session has no projection row. The API's wait loop reads this for sessions
// whose actor is gone.
func LoadMaterializedTurnOutcome(sessionID string) (*MaterializedTurnState, error) {
	return loadMaterializedTurnOutcomeFrom(DatabasePath(), sessionID)
}

// LoadPromptAcceptance reconciles a replayed input after the worker has
// collected its command ledger. Queued, active and historical turns are read
// from one projection transaction.
func LoadPromptAcceptance(sessionID string, commandID string) (uint64, bool, error) {
	connection, done, err := openReadSnapshot(DatabasePath())
	if err != nil {
		return 0, false, err
	}
	defer done()
	fields, err := readMaterializedSessionFields(connection, sessionID)
	if err != nil {
		return 0, false, err
	}
	if fields != nil {
		if turn := fields.activeTurn; turn != nil && turn.CommandID == commandID {
			if turn.AcceptedOrdinal == nil {
				return 0, false, errors.New("accepted prompt has no acceptance ordinal")
			}
			return *turn.AcceptedOrdinal, true, nil
		}
		if turn := fields.lastTurnOutcome; turn != nil && turn.CommandID == commandID {
			if turn.AcceptedOrdinal == nil {
				return 0, false, errors.New("completed prompt has no acceptance ordinal")
			}
			return *turn.AcceptedOrdinal, true, nil
		}
	}
	var ordinal *uint64
	err = connection.QueryRow(`SELECT accepted_ordinal FROM materialized_queued_prompts WHERE session_id=?1 AND command_id=?2
         UNION ALL SELECT json_extract(body, '$.accepted_ordinal') FROM session_turn_usage
         WHERE session_id=?1 AND command_id=?2 LIMIT 1`, sessionID, commandID).Scan(&ordinal)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return 0, false, err
	}
	if ordinal != nil {
		return *ordinal, true, nil
	}
	var started bool
	if err := connection.QueryRow("SELECT EXISTS(SELECT 1 FROM materialized_transcript_items WHERE session_id=?1 AND stable_id IN (?2, ?3))",
		sessionID, "user:"+commandID, core.ContextBoundaryPrefix+commandID).Scan(&started); err != nil {
		return 0, false, err
	}
	if started {
		return 0, false, errors.New("input was delivered but its acceptance ordinal is unavailable; refusing to send it twice")
	}
	return 0, false, nil
}

// LoadMaterializedAgentMessageThrough is the newest agent message whose
// content was complete at event ordinal through: what a ReadThrough
// notification dismissal names. One query, so the bound and the read are one
// decision and a newer reply cannot answer for an older card.
func LoadMaterializedAgentMessageThrough(sessionID string, through uint64) (string, bool, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return "", false, err
	}
	defer connection.Close()
	_, text, ok, err := lastMaterializedAgentMessageIn(connection, sessionID, 0, nil, &through)
	return text, ok, err
}

func LoadMaterializedAgentItemThrough(sessionID string, through uint64) (*core.TranscriptItem, error) {
	connection, done, err := openReadSnapshot(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer done()
	return MaterializedAgentItemThrough(connection, sessionID, through)
}

// MaterializedAgentItemThrough is the newest agent message of the turn that
// completed at through, bounded to that turn: a turn that left no text has
// none, never an earlier turn's. Every reader of a notification entry without
// a message id asks this.
func MaterializedAgentItemThrough(connection Queryer, sessionID string, through uint64) (*core.TranscriptItem, error) {
	var historicalStart *uint64
	err := connection.QueryRow(`SELECT turn_start_position FROM session_turn_usage
             WHERE session_id = ?1 AND completed_ordinal = ?2`, sessionID, through).Scan(&historicalStart)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return nil, err
	}
	start := historicalStart
	if start == nil {
		// Restoring a canonical snapshot can supply a last outcome without
		// its historical usage row. Its completion must match exactly too.
		state, err := readMaterializedTurnState(connection, sessionID)
		if err != nil {
			return nil, err
		}
		if state != nil && state.LastTurnOutcome != nil && state.LastTurnOutcome.CompletedOrdinal == through {
			start = state.LastTurnOutcome.TurnStartPosition
		}
	}
	if start == nil {
		return nil, nil
	}
	return lastMaterializedAgentItemIn(connection, sessionID, *start, &through, &through)
}

// LoadMaterializedTranscriptItem: whether the stored projection holds a
// transcript item with this stable id. A prompt's item appears when the
// prompt starts, so this tells a started prompt from one that was only
// queued. Exact primary-key lookup; the runtime tail need not contain this
// message.
func LoadMaterializedTranscriptItem(sessionID string, stableID string) (*core.TranscriptItem, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	item := &core.TranscriptItem{StableID: stableID}
	var body string
	err = connection.QueryRow(`SELECT position, latest_content_event_ordinal, created_at_ms, last_changed_at_ms, body_json
         FROM materialized_transcript_items WHERE session_id=?1 AND stable_id=?2`, sessionID, stableID).
		Scan(&item.Position, &item.LatestContentEventOrdinal, &item.CreatedAtMs, &item.LastChangedAtMs, &body)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	if item.Body, err = decodeTranscriptBody(body, sessionID); err != nil {
		return nil, err
	}
	return item, nil
}

func MaterializedTranscriptItemExists(sessionID string, stableID string) (bool, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return false, err
	}
	defer connection.Close()
	var exists bool
	err = connection.QueryRow("SELECT EXISTS(SELECT 1 FROM materialized_transcript_items WHERE session_id=?1 AND stable_id=?2)",
		sessionID, stableID).Scan(&exists)
	return exists, err
}

func loadMaterializedTurnOutcomeFrom(path string, sessionID string) (*MaterializedTurnState, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return readMaterializedTurnState(connection, sessionID)
}

// decodeMaterializedTurn decodes an active turn column, with the context its
// readers give a malformed one.
func decodeMaterializedTurn(text *string, sessionID string) (*core.MaterializedTurn, error) {
	if text == nil {
		return nil, nil
	}
	turn := new(core.MaterializedTurn)
	if err := json.Unmarshal([]byte(*text), turn); err != nil {
		return nil, fmt.Errorf("parse active turn for session %s: %w", sessionID, err)
	}
	return turn, nil
}

// decodeLastTurnOutcome decodes a last turn outcome column, with the context
// its readers give a malformed one.
func decodeLastTurnOutcome(text *string, sessionID string) (*core.MaterializedTurnOutcome, error) {
	if text == nil {
		return nil, nil
	}
	turn, err := deserializeTurnOutcome(*text)
	if err != nil {
		return nil, fmt.Errorf("parse last turn outcome for session %s: %w", sessionID, err)
	}
	return &turn, nil
}

// readMaterializedTurnState: turn observation does not need configuration,
// forms, or transcript bodies.
func readMaterializedTurnState(connection Queryer, sessionID string) (*MaterializedTurnState, error) {
	var execution string
	var started *int64
	var active, last *string
	err := connection.QueryRow(`SELECT execution_state, running_started_at_ms, active_turn_json, last_turn_outcome_json
         FROM materialized_sessions WHERE session_id=?1`, sessionID).Scan(&execution, &started, &active, &last)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	state := &MaterializedTurnState{}
	if state.Execution, err = parseMaterializedExecution(execution, started); err != nil {
		return nil, err
	}
	if state.ActiveTurn, err = decodeMaterializedTurn(active, sessionID); err != nil {
		return nil, err
	}
	if state.LastTurnOutcome, err = decodeLastTurnOutcome(last, sessionID); err != nil {
		return nil, err
	}
	return state, nil
}

// LoadMaterializedFinishedTurnMessage is the answer a session's last
// finished turn ended with.
//
// This is what a finished child session reports back to the agent that
// delegated to it, so it has to be that turn's own last agent message. The
// session-wide last agent message is not the same thing: a harness records
// messages of its own outside any turn, and a resume notice arriving after
// the child finished would then stand in for the child's report.
//
// false means the session has no projection row, or no finished turn whose
// span is recorded, and the caller decides what to show instead.
func LoadMaterializedFinishedTurnMessage(sessionID string) (string, bool, error) {
	return loadMaterializedFinishedTurnMessageFrom(DatabasePath(), sessionID)
}

// ChildAnswerSpan is one child and the (start, end) positions of the turn
// whose completion was observed: Rust's `(String, Option<(u64, u64)>)`.
type ChildAnswerSpan struct {
	SessionID string
	Span      *[2]uint64
}

// LoadChildAnswerMessages reads report text within the turn bounds that
// decided completion, even if the child has started another turn since that
// observation.
func LoadChildAnswerMessages(children []ChildAnswerSpan) (map[string]*string, error) {
	connection, done, err := openReadSnapshot(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer done()
	answers := make(map[string]*string, len(children))
	for _, child := range children {
		var finished *string
		if child.Span != nil {
			finished, err = lastMaterializedAgentMessageWithin(connection, child.SessionID, child.Span[0], child.Span[1])
			if err != nil {
				return nil, err
			}
		}
		answers[child.SessionID] = finished
	}
	return answers, nil
}

func loadMaterializedFinishedTurnMessageFrom(path string, sessionID string) (string, bool, error) {
	connection, done, err := openReadSnapshot(path)
	if err != nil {
		return "", false, err
	}
	defer done()
	fields, err := readMaterializedSessionFields(connection, sessionID)
	if err != nil || fields == nil {
		return "", false, err
	}
	turn := fields.lastTurnOutcome
	if turn == nil || turn.TurnStartPosition == nil {
		return "", false, nil
	}
	message, err := lastMaterializedAgentMessageWithin(connection, sessionID, *turn.TurnStartPosition, turn.CompletedOrdinal)
	if err != nil || message == nil {
		return "", false, err
	}
	return *message, true, nil
}

// LoadMaterializedTurnSummary summarizes the turn that ran from
// turnStartPosition to turnCompletedPosition.
//
// Both bounds come from the turn's own record: a transcript item's position
// and a completed turn's completed_ordinal are the same relay ordinal, so the
// completion ordinal is the last position the turn can own. Anything the
// session records afterwards belongs to no turn, or to the next one.
func LoadMaterializedTurnSummary(sessionID string, turnStartPosition uint64, turnCompletedPosition uint64) (core.TurnSummary, error) {
	return loadMaterializedTurnSummaryFrom(DatabasePath(), sessionID, turnStartPosition, turnCompletedPosition)
}

func loadMaterializedTurnSummaryFrom(path string, sessionID string, turnStartPosition uint64, turnCompletedPosition uint64) (core.TurnSummary, error) {
	connection, done, err := openReadSnapshot(path)
	if err != nil {
		return core.TurnSummary{}, err
	}
	defer done()
	var summary core.TurnSummary
	if err := connection.QueryRow(`SELECT COUNT(*)
         FROM materialized_transcript_items
         WHERE session_id = ?1
           AND position <= ?3
           AND (
               stable_id GLOB ?2
               OR json_extract(
                   CASE
                       WHEN stable_id GLOB 'user:*' OR stable_id GLOB 'user-*'
                       THEN body_json
                       ELSE '{}'
                   END,
                   '$.kind'
               ) = 'user'
           )`, sessionID, core.HarnessTurnItemPrefix+"*", turnStartPosition).Scan(&summary.TurnNumber); err != nil {
		return core.TurnSummary{}, err
	}
	if err := connection.QueryRow(`SELECT COALESCE(MIN(created_at_ms), 0), COALESCE(MAX(last_changed_at_ms), 0)
         FROM materialized_transcript_items
         WHERE session_id = ?1 AND position >= ?2 AND position <= ?3`,
		sessionID, turnStartPosition, turnCompletedPosition).Scan(&summary.TurnStartedAtMs, &summary.LastChangedAtMs); err != nil {
		return core.TurnSummary{}, err
	}
	if summary.FinalMessage, err = lastMaterializedAgentMessageWithin(connection, sessionID, turnStartPosition, turnCompletedPosition); err != nil {
		return core.TurnSummary{}, err
	}
	if err := connection.QueryRow(`SELECT COUNT(*)
         FROM materialized_transcript_items
         WHERE session_id = ?1 AND position >= ?2 AND position <= ?3
           AND item_kind = 'tool'`, sessionID, turnStartPosition, turnCompletedPosition).Scan(&summary.ToolCalls); err != nil {
		return core.TurnSummary{}, err
	}
	return summary, nil
}

func LoadMaterializedTranscriptFiltered(sessionID string, afterSeq uint64, limit int, roles []core.TranscriptRole, finishedOnly bool) (*core.TranscriptPage, error) {
	return loadMaterializedTranscriptFilteredFrom(DatabasePath(), sessionID, afterSeq, limit, roles, finishedOnly)
}

// hydrateTerminalOutputs joins parked output for presentation without
// changing stored bodies or page boundaries. An item it changes is replaced
// by a changed copy (Rust's Arc::make_mut), so shared items stay unchanged.
func hydrateTerminalOutputs(connection Queryer, sessionID string, items []*core.TranscriptItem) error {
	held := core.TerminalOutputsOf(derefTranscriptItems(items))
	idSet := map[string]struct{}{}
	for _, id := range held.ReferencedTerminalIDs() {
		idSet["terminal:"+id] = struct{}{}
	}
	if len(idSet) == 0 {
		return nil
	}
	statement, err := connection.Prepare("SELECT body_json FROM materialized_transcript_items WHERE session_id=?1 AND stable_id=?2")
	if err != nil {
		return err
	}
	defer statement.Close()
	var context []core.TranscriptItem
	for _, id := range slices.Sorted(maps.Keys(idSet)) {
		var body string
		err := statement.QueryRow(sessionID, id).Scan(&body)
		if errors.Is(err, sql.ErrNoRows) {
			continue
		}
		if err != nil {
			return err
		}
		decoded, err := decodeTranscriptBody(body, sessionID)
		if err != nil {
			return err
		}
		context = append(context, core.TranscriptItem{StableID: id, Body: decoded})
	}
	outputs := core.TerminalOutputsOf(append(derefTranscriptItems(items), context...))
	for i, item := range items {
		tool, ok := item.Body.(core.TranscriptBodyTool)
		if !ok {
			continue
		}
		tool.TerminalOutputs = outputs.ForTool(item.StableID)
		copied := *item
		copied.Body = tool
		items[i] = &copied
	}
	return nil
}

const filteredTranscriptMatch = `session_id = ?1 AND update_ordinal > ?2
    AND (json_array_length(?4) = 0 OR item_kind IN (SELECT value FROM json_each(?4)))
    AND (?5 = 0 OR (settled = 1 AND (?6 IS NULL OR update_ordinal < ?6)))`

func filteredTranscriptPageSQL() string {
	return `SELECT stable_id, position, latest_content_event_ordinal, created_at_ms,
                last_changed_at_ms, body_json
         FROM materialized_transcript_items
         WHERE ` + filteredTranscriptMatch + `
           AND update_ordinal <= (
               SELECT MAX(update_ordinal) FROM (
                   SELECT update_ordinal FROM materialized_transcript_items
                   WHERE ` + filteredTranscriptMatch + `
                   ORDER BY update_ordinal, position, stable_id LIMIT ?3
               )
           )
         ORDER BY update_ordinal, position, stable_id`
}

func loadMaterializedTranscriptFilteredFrom(path string, sessionID string, afterSeq uint64, limit int, roles []core.TranscriptRole, finishedOnly bool) (*core.TranscriptPage, error) {
	connection, done, err := openReadSnapshot(path)
	if err != nil {
		return nil, err
	}
	defer done()
	fields, err := readMaterializedSessionFields(connection, sessionID)
	if err != nil || fields == nil {
		return nil, err
	}
	roleKinds := []string{}
	if finishedOnly {
		roleKinds = []string{"agent"}
	} else {
		for _, role := range roles {
			roleKinds = append(roleKinds, role.StorageKind())
		}
	}
	roleKindsJSON, err := databaseMarshalJSON(roleKinds)
	if err != nil {
		return nil, err
	}
	finished := int64(0)
	if finishedOnly {
		finished = 1
	}
	var openAgentSeq *uint64
	if finishedOnly {
		if err := connection.QueryRow(`SELECT MIN(update_ordinal) FROM materialized_transcript_items
             WHERE session_id = ?1 AND update_ordinal > ?2 AND item_kind = 'agent' AND settled = 0`,
			sessionID, afterSeq).Scan(&openAgentSeq); err != nil {
			return nil, err
		}
	}
	items, err := queryStoredTranscriptItems(connection, sessionID, filteredTranscriptPageSQL(),
		sessionID, afterSeq, int64(min(max(limit, 1), 1000)), string(roleKindsJSON), finished, openAgentSeq)
	if err != nil {
		return nil, err
	}
	// A sequence is indivisible as a cursor. Preserve the historical
	// stable-ID order within each tie while reading both ranges in index
	// order.
	slices.SortStableFunc(items, func(a, b *core.TranscriptItem) int {
		if a.Seq() != b.Seq() {
			if a.Seq() < b.Seq() {
				return -1
			}
			return 1
		}
		return strings.Compare(a.StableID, b.StableID)
	})
	var latestSeq uint64
	if err := connection.QueryRow(`SELECT COALESCE(MAX(update_ordinal), 0)
         FROM materialized_transcript_items WHERE session_id = ?1`, sessionID).Scan(&latestSeq); err != nil {
		return nil, err
	}
	lastSeq := afterSeq
	if len(items) != 0 {
		lastSeq = items[len(items)-1].Seq()
	}
	var more bool
	if err := connection.QueryRow(`SELECT EXISTS(SELECT 1 FROM materialized_transcript_items
                  WHERE `+filteredTranscriptMatch+`)`,
		sessionID, lastSeq, nil, string(roleKindsJSON), finished, openAgentSeq).Scan(&more); err != nil {
		return nil, err
	}
	var nextAfterSeq uint64
	switch {
	case more:
		nextAfterSeq = lastSeq
	case finishedOnly && openAgentSeq != nil:
		open := *openAgentSeq
		if open > 0 {
			open--
		}
		nextAfterSeq = max(open, afterSeq)
	default:
		nextAfterSeq = max(latestSeq, afterSeq)
	}
	if len(roles) != 0 && !slices.Contains(roles, core.TranscriptRoleTerminal) {
		if err := hydrateTerminalOutputs(connection, sessionID, items); err != nil {
			return nil, err
		}
	}
	return &core.TranscriptPage{
		NextAfterSeq: nextAfterSeq,
		Items:        items,
		LatestSeq:    latestSeq,
		Execution:    fields.execution,
	}, nil
}

// retentionBatchItems: use the same bounded pass for turn completion,
// checkpoint retention, and backfill.
const retentionBatchItems = 32

// retentionBodyFloorBytes: smaller rows already cost little to keep inline.
const retentionBodyFloorBytes = 4 * 1024

// SpilledToolBody is one compacted tool body: Rust's
// `(String, TranscriptBody)`, the stable id and the trimmed body.
type SpilledToolBody struct {
	StableID string
	Body     core.TranscriptBody
}

func SpillAndTrimSettledToolsThrough(sessionID string, frontierOrdinal uint64) ([]SpilledToolBody, error) {
	items, err := SpillAndTrimSettledToolsVersioned(sessionID, frontierOrdinal)
	if err != nil {
		return nil, err
	}
	return spilledBodies(items), nil
}

func SpillAndTrimSettledToolsThroughFrom(path string, sessionID string, frontierOrdinal uint64) ([]SpilledToolBody, error) {
	connection, err := open(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	batch, err := drainToolDetailBatches(func(after *ToolDetailCursor) (spilledToolBatch, error) {
		return spillToolDetailBatch(connection, dataDirFrom(path), sessionID, frontierOrdinal, after, retentionBatchItems)
	})
	if err != nil {
		return nil, err
	}
	return spilledBodies(batch.items), nil
}

func spilledBodies(items []core.TranscriptItem) []SpilledToolBody {
	bodies := make([]SpilledToolBody, 0, len(items))
	for _, item := range items {
		bodies = append(bodies, SpilledToolBody{StableID: item.StableID, Body: item.Body})
	}
	return bodies
}

// SpillAndTrimSettledToolsVersioned returns the durable identity and version
// with each changed body so actors can reconcile the result with their own
// committed window.
func SpillAndTrimSettledToolsVersioned(sessionID string, frontierOrdinal uint64) ([]core.TranscriptItem, error) {
	batch, err := drainToolDetailBatches(func(after *ToolDetailCursor) (spilledToolBatch, error) {
		return submitToolDetailBatch(sessionID, frontierOrdinal, after.clone())
	})
	if err != nil {
		return nil, err
	}
	return batch.items, nil
}

func CompactMaterializedTranscriptThrough(sessionID string, eventFrontier uint64) (core.TranscriptRetention, error) {
	batch, err := submitToolDetailBatch(sessionID, eventFrontier, nil)
	if err != nil {
		return core.TranscriptRetention{}, err
	}
	return batch.retention, nil
}

func CompactMaterializedTranscriptAllThrough(sessionID string, eventFrontier uint64) (core.TranscriptRetention, error) {
	batch, err := drainToolDetailBatches(func(after *ToolDetailCursor) (spilledToolBatch, error) {
		batch, err := submitToolDetailBatch(sessionID, eventFrontier, after.clone())
		batch.items = nil
		return batch, err
	})
	if err != nil {
		return core.TranscriptRetention{}, err
	}
	return batch.retention, nil
}

// compactMaterializedTranscriptIn is test-only in Rust.
func compactMaterializedTranscriptIn(path string, sessionID string, eventFrontier uint64) (core.TranscriptRetention, error) {
	connection, err := open(path)
	if err != nil {
		return core.TranscriptRetention{}, err
	}
	defer connection.Close()
	batch, err := spillToolDetailBatch(connection, dataDirFrom(path), sessionID, eventFrontier, nil, retentionBatchItems)
	if err != nil {
		return core.TranscriptRetention{}, err
	}
	return batch.retention, nil
}

// compactMaterializedTranscriptAllIn is test-only in Rust.
func compactMaterializedTranscriptAllIn(path string, sessionID string, eventFrontier uint64, batchSize int) (core.TranscriptRetention, error) {
	connection, err := open(path)
	if err != nil {
		return core.TranscriptRetention{}, err
	}
	defer connection.Close()
	batch, err := drainToolDetailBatches(func(after *ToolDetailCursor) (spilledToolBatch, error) {
		return spillToolDetailBatch(connection, dataDirFrom(path), sessionID, eventFrontier, after, batchSize)
	})
	if err != nil {
		return core.TranscriptRetention{}, err
	}
	return batch.retention, nil
}

// ToolDetailCursor is where a tool detail backfill resumes: Rust's
// `(u64, String)`, the position and stable id of the last row it read.
type ToolDetailCursor struct {
	Position uint64
	StableID string
}

// clone is Rust's Option<&ToolDetailCursor>::cloned.
func (c *ToolDetailCursor) clone() *ToolDetailCursor {
	if c == nil {
		return nil
	}
	copied := *c
	return &copied
}

type spilledToolBatch struct {
	retention core.TranscriptRetention
	items     []core.TranscriptItem
	next      *ToolDetailCursor
}

func drainToolDetailBatches(spill func(after *ToolDetailCursor) (spilledToolBatch, error)) (spilledToolBatch, error) {
	var total spilledToolBatch
	for {
		batch, err := spill(total.next)
		if err != nil {
			return spilledToolBatch{}, err
		}
		total.retention.Items += batch.retention.Items
		total.retention.Bytes += batch.retention.Bytes
		total.items = append(total.items, batch.items...)
		total.next = batch.next
		if total.next == nil {
			return total, nil
		}
	}
}

func submitToolDetailBatch(sessionID string, frontierOrdinal uint64, after *ToolDetailCursor) (spilledToolBatch, error) {
	return submitDatabaseWrite("spill_tool_details", func(connection *Connection) (spilledToolBatch, error) {
		return spillToolDetailBatch(connection, dataDirFrom(DatabasePath()), sessionID, frontierOrdinal, after, retentionBatchItems)
	})
}

const spillableTools = `settled = 1 AND detail_offset IS NULL
    AND item_kind IN ('tool', 'terminal_output')`

// NextToolDetailBackfillSession finds the next session with work at its
// durable frontier, returning its id and applied event ordinal. A scan
// timeout or shutdown needs no persisted cursor: committed rows are excluded
// on retry.
func NextToolDetailBackfillSession(after string) (string, uint64, bool, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return "", 0, false, err
	}
	defer connection.Close()
	var sessionID string
	var frontier uint64
	err = connection.QueryRow(`SELECT session_id, applied_event_ordinal FROM materialized_sessions AS s
         WHERE session_id > ?1 AND EXISTS (
             SELECT 1 FROM materialized_transcript_items AS i
             WHERE i.session_id = s.session_id AND `+spillableTools+` AND length(body_json) > ?2
         )
         ORDER BY session_id LIMIT 1`, after, retentionBodyFloorBytes).Scan(&sessionID, &frontier)
	if errors.Is(err, sql.ErrNoRows) {
		return "", 0, false, nil
	}
	if err != nil {
		return "", 0, false, err
	}
	return sessionID, frontier, true, nil
}

// BackfillToolDetailsBatch spills one maintenance batch, releasing the writer
// before scanning again.
func BackfillToolDetailsBatch(sessionID string, frontier uint64, after *ToolDetailCursor) (*ToolDetailCursor, error) {
	batch, err := submitToolDetailBatch(sessionID, frontier, after)
	if err != nil {
		return nil, err
	}
	return batch.next, nil
}

func spillToolDetailBatch(connection *Connection, dataDir string, sessionID string, frontier uint64, after *ToolDetailCursor, batchSize int) (spilledToolBatch, error) {
	// Hold the SQLite write reservation across selection and append even when
	// working against an explicit database without the daemon's writer queue.
	transaction, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return spilledToolBatch{}, err
	}
	defer transaction.Rollback()
	var unsettledTool bool
	if err := transaction.QueryRow(`SELECT EXISTS(SELECT 1 FROM materialized_transcript_items
         WHERE session_id = ?1 AND settled = 0 AND item_kind = 'tool')`, sessionID).Scan(&unsettledTool); err != nil {
		return spilledToolBatch{}, err
	}
	// A still-open fallback terminal tool (`tool:hel-terminal:...`) may yet be
	// claimed on close by a completed provider tool whose rawOutput matches
	// (e.g. Kimi Bash). Defer spilling until that fallback terminal finishes.
	if unsettledTool {
		var fallbackOpen bool
		if err := transaction.QueryRow(`SELECT EXISTS(SELECT 1 FROM materialized_transcript_items
             WHERE session_id = ?1 AND settled = 0 AND item_kind = 'tool'
               AND stable_id LIKE 'tool:hel-terminal:%')`, sessionID).Scan(&fallbackOpen); err != nil {
			return spilledToolBatch{}, err
		}
		if fallbackOpen {
			return spilledToolBatch{}, nil
		}
	}
	type candidate struct {
		item core.TranscriptItem
		body string
	}
	var candidates []candidate
	err = func() error {
		afterPosition, afterID := uint64(0), ""
		if after != nil {
			afterPosition, afterID = after.Position, after.StableID
		}
		rows, err := transaction.Query(`SELECT stable_id, position, latest_content_event_ordinal, created_at_ms,
                    last_changed_at_ms, body_json
             FROM materialized_transcript_items
             WHERE session_id = ?1 AND position <= ?2 AND update_ordinal <= ?2
               AND `+spillableTools+` AND length(body_json) > ?3 AND (?4 = 0 OR item_kind = 'tool')
               AND (position, stable_id) > (?5, ?6)
             ORDER BY position, stable_id LIMIT ?7`,
			sessionID, frontier, retentionBodyFloorBytes, unsettledTool, afterPosition, afterID, int64(batchSize+1))
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			var c candidate
			if err := rows.Scan(&c.item.StableID, &c.item.Position, &c.item.LatestContentEventOrdinal,
				&c.item.CreatedAtMs, &c.item.LastChangedAtMs, &c.body); err != nil {
				return err
			}
			candidates = append(candidates, c)
		}
		return rows.Err()
	}()
	if err != nil {
		return spilledToolBatch{}, err
	}
	var batch spilledToolBatch
	if len(candidates) > batchSize {
		candidates = candidates[:len(candidates)-1]
		last := candidates[batchSize-1]
		batch.next = &ToolDetailCursor{Position: last.item.Position, StableID: last.item.StableID}
		batch.retention.Remaining = true
	}
	type pendingSpill struct {
		item      core.TranscriptItem
		bodyJSON  string
		compacted string
	}
	var pending []pendingSpill
	for _, c := range candidates {
		body, err := core.DecodeTranscriptBody([]byte(c.body))
		if err != nil {
			slog.Warn("skipping unreadable transcript body", "session_id", sessionID, "stable_id", c.item.StableID, "error", err.Error())
			continue
		}
		if core.TranscriptBodyIsLegacyCompactedTool(body) || !transcript.CompactTranscriptBodyForRetention(&body) {
			continue
		}
		compacted, err := databaseMarshalJSON(body)
		if err != nil {
			return spilledToolBatch{}, fmt.Errorf("serialize compacted transcript body %s: %w", c.item.StableID, err)
		}
		if len(compacted) < len(c.body) {
			item := c.item
			item.Body = body
			pending = append(pending, pendingSpill{item: item, bodyJSON: c.body, compacted: string(compacted)})
		}
	}
	bodies := make([]ToolDetailBody, 0, len(pending))
	for _, spill := range pending {
		bodies = append(bodies, ToolDetailBody{StableID: spill.item.StableID, BodyJSON: spill.bodyJSON})
	}
	frames, err := AppendToolDetailsBatch(dataDir, sessionID, bodies)
	if err != nil {
		return spilledToolBatch{}, err
	}
	for i, spill := range pending {
		if i >= len(frames) {
			break
		}
		frame := frames[i]
		changed, err := changedRows(transaction.Exec(`UPDATE materialized_transcript_items SET body_json = ?1, detail_offset = ?2, detail_len = ?3
             WHERE session_id = ?4 AND stable_id = ?5 AND last_changed_at_ms = ?6
               AND latest_content_event_ordinal IS ?7`,
			spill.compacted, frame.Offset, frame.Len, sessionID, spill.item.StableID,
			spill.item.LastChangedAtMs, spill.item.LatestContentEventOrdinal))
		if err != nil {
			return spilledToolBatch{}, err
		}
		if changed == 1 {
			batch.retention.Items++
			batch.retention.Bytes += len(spill.bodyJSON) - len(spill.compacted)
			batch.items = append(batch.items, spill.item)
		}
	}
	if err := transaction.Commit(); err != nil {
		return spilledToolBatch{}, err
	}
	return batch, nil
}

// LoadToolCallDetail reads the current detail by stable identity without
// paging the transcript. A nil result is no such item.
func LoadToolCallDetail(sessionID string, stableID string) (core.ToolCallDetailResult, error) {
	return LoadToolCallDetailFrom(DatabasePath(), sessionID, stableID)
}

func LoadToolCallDetailFrom(path string, sessionID string, stableID string) (core.ToolCallDetailResult, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	var summaryJSON string
	var offset *uint64
	var length *uint32
	err = connection.QueryRow(`SELECT body_json, detail_offset, detail_len FROM materialized_transcript_items
             WHERE session_id = ?1 AND stable_id = ?2`, sessionID, stableID).Scan(&summaryJSON, &offset, &length)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	dataDir := dataDirFrom(path)
	summary, err := core.DecodeTranscriptBody([]byte(summaryJSON))
	if err != nil {
		return nil, err
	}
	var body core.TranscriptBody
	switch {
	case offset != nil && length != nil:
		body, err = ReadToolDetail(dataDir, sessionID, stableID, *offset, *length)
		if err != nil {
			return core.ToolCallDetailResultUnavailable{Summary: summary, Reason: err.Error()}, nil
		}
	case offset == nil && !core.TranscriptBodyHasTrimmedDetail(summary):
		body = summary
	default:
		return core.ToolCallDetailResultUnavailable{Summary: summary, Reason: "full tool detail is not stored locally"}, nil
	}
	reason, err := hydrateFullToolTerminalOutputs(connection, dataDir, sessionID, stableID, &body)
	if err != nil {
		return nil, err
	}
	if reason != nil {
		return core.ToolCallDetailResultUnavailable{Summary: summary, Reason: *reason}, nil
	}
	return core.ToolCallDetailResultFull{Body: body}, nil
}

// hydrateFullToolTerminalOutputs joins authoritative standalone terminal rows
// referenced by a full tool detail, resolving their .mjtd cold frames when
// spilled independently. A non-nil reason is Rust's inner Err: the detail is
// unavailable for that reason.
func hydrateFullToolTerminalOutputs(connection Queryer, dataDir string, sessionID string, stableID string, body *core.TranscriptBody) (*string, error) {
	tool, ok := (*body).(core.TranscriptBodyTool)
	if !ok {
		return nil, nil
	}
	idSet := map[string]struct{}{}
	for _, id := range tool.TerminalRefs {
		idSet["terminal:"+id] = struct{}{}
	}
	for _, record := range tool.TerminalOutputs {
		idSet["terminal:"+record.TerminalID] = struct{}{}
	}
	if len(idSet) == 0 {
		return nil, nil
	}
	statement, err := connection.Prepare(`SELECT body_json, detail_offset, detail_len FROM materialized_transcript_items
         WHERE session_id = ?1 AND stable_id = ?2`)
	if err != nil {
		return nil, err
	}
	defer statement.Close()
	var context []core.TranscriptItem
	for _, id := range slices.Sorted(maps.Keys(idSet)) {
		var inlineJSON string
		var offset *uint64
		var length *uint32
		err := statement.QueryRow(sessionID, id).Scan(&inlineJSON, &offset, &length)
		if errors.Is(err, sql.ErrNoRows) {
			continue
		}
		if err != nil {
			return nil, err
		}
		var terminalBody core.TranscriptBody
		if offset != nil && length != nil {
			terminalBody, err = ReadToolDetail(dataDir, sessionID, id, *offset, *length)
			if err != nil {
				reason := err.Error()
				return &reason, nil
			}
		} else {
			inline, err := decodeTranscriptBody(inlineJSON, sessionID)
			if err != nil {
				return nil, err
			}
			if core.TranscriptBodyHasTrimmedDetail(inline) {
				reason := "full referenced terminal detail is not stored locally"
				return &reason, nil
			}
			terminalBody = inline
		}
		context = append(context, core.TranscriptItem{StableID: id, Body: terminalBody})
	}
	if len(context) == 0 {
		return nil, nil
	}
	toolItem := core.TranscriptItem{StableID: stableID, Body: tool}
	outputs := core.TerminalOutputsOf(append([]core.TranscriptItem{toolItem}, context...))
	tool.TerminalOutputs = outputs.ForTool(stableID)
	*body = tool
	return nil, nil
}

// ReadStoredToolDetail reads the spill associated with a specific projected
// revision. A newer inline update or an already-trimmed legacy row has no
// matching detail (nil).
func ReadStoredToolDetail(sessionID string, item *core.TranscriptItem) (core.TranscriptBody, error) {
	return ReadStoredToolDetailFrom(DatabasePath(), sessionID, item)
}

func ReadStoredToolDetailFrom(path string, sessionID string, item *core.TranscriptItem) (core.TranscriptBody, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	var offset uint64
	var length uint32
	err = connection.QueryRow(`SELECT detail_offset, detail_len FROM materialized_transcript_items
         WHERE session_id = ?1 AND stable_id = ?2 AND last_changed_at_ms = ?3
           AND latest_content_event_ordinal IS ?4 AND detail_offset IS NOT NULL`,
		sessionID, item.StableID, item.LastChangedAtMs, item.LatestContentEventOrdinal).Scan(&offset, &length)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	return ReadToolDetail(dataDirFrom(path), sessionID, item.StableID, offset, length)
}

const actorUnsettledItemsSQL = `SELECT position FROM materialized_transcript_items INDEXED BY materialized_transcript_unsettled
     WHERE session_id = ?1 AND position < ?2 AND settled = 0`

func actorUnsettledTurnsSQL() string {
	turnStart := turnStartSQL()
	return `WITH unsettled AS (` + actorUnsettledItemsSQL + `)
         SELECT COALESCE((
                    SELECT MAX(position) FROM materialized_transcript_items
                    WHERE session_id = ?1 AND position <= u.position AND ` + turnStart + `
                ), 0),
                COALESCE((
                    SELECT MIN(position) FROM materialized_transcript_items
                    WHERE session_id = ?1 AND position > u.position AND ` + turnStart + `
                ), ?2)
         FROM unsettled u`
}

// ProjectionTailItems is how many transcript items a polled projection
// carries.
//
// Both the browser and TUI page older history inline from the store. Keep
// twice the TUI's 256 seed entries for the initial window; the browser's
// independent 1,000 rendered-line budget may clip this window further.
const ProjectionTailItems = 512

// LoadMaterializedActorProjection loads the same safe window the live
// projector maintains, without first deserializing all of the conversation.
// Read metadata and bodies together. Both results are nil when the session
// has no projection.
func LoadMaterializedActorProjection(sessionID string) (*core.MaterializedSession, *core.ProjectionWindow, error) {
	return loadMaterializedActorProjectionFrom(DatabasePath(), sessionID)
}

// materializedProvisionalTitle is the provisional title derived from the oldest user
// message, if any.
func materializedProvisionalTitle(connection Queryer, sessionID string) (*string, error) {
	_, text, ok, err := firstMaterializedUserMessage(connection, sessionID)
	if err != nil || !ok {
		return nil, err
	}
	title, ok := core.ProvisionalSessionTitle(text)
	if !ok {
		return nil, nil
	}
	return &title, nil
}

// materializedFromFields assembles a projection from its stored fields,
// transcript and queue.
func materializedFromFields(sessionID string, fields *materializedSessionFields, transcript []*core.TranscriptItem, queuedPrompts []core.MaterializedQueuedPrompt) core.MaterializedSession {
	return core.MaterializedSession{
		SessionID:           sessionID,
		AppliedEventOrdinal: fields.appliedEventOrdinal,
		AppliedEventDigest:  fields.appliedEventDigest,
		LastActivityAtMs:    fields.lastActivityAtMs,
		Execution:           fields.execution,
		SessionTitle:        fields.sessionTitle,
		Configuration:       fields.configuration,
		Transcript:          transcript,
		QueuedPrompts:       queuedPrompts,
		PendingElicitations: fields.pendingElicitations,
		ActiveTurn:          fields.activeTurn,
		LastTurnOutcome:     fields.lastTurnOutcome,
	}
}

func loadMaterializedActorProjectionFrom(path string, sessionID string) (*core.MaterializedSession, *core.ProjectionWindow, error) {
	connection, done, err := openReadSnapshot(path)
	if err != nil {
		return nil, nil, err
	}
	defer done()
	fields, err := readMaterializedSessionFields(connection, sessionID)
	if err != nil || fields == nil {
		return nil, nil, err
	}
	latestTurn, err := lastMaterializedTurnStart(connection, sessionID)
	if err != nil {
		return nil, nil, err
	}
	var desired uint64
	err = connection.QueryRow(`SELECT position FROM materialized_transcript_items WHERE session_id=?1
         ORDER BY position DESC, stable_id DESC LIMIT 1 OFFSET ?2`, sessionID, ProjectionTailItems-1).Scan(&desired)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return nil, nil, err
	}
	boundary := desired
	if latestTurn == nil {
		boundary = 0
	} else {
		boundary = min(boundary, *latestTurn)
	}
	var start uint64
	if err := connection.QueryRow(fmt.Sprintf(`SELECT COALESCE(MAX(position),0) FROM materialized_transcript_items
             WHERE session_id=?1 AND position<=?2 AND %s`, turnStartSQL()), sessionID, boundary).Scan(&start); err != nil {
		return nil, nil, err
	}
	// Find old unsettled turns, then read only their distinct ranges.
	type span struct{ begin, end uint64 }
	rangeSet := map[span]struct{}{}
	err = func() error {
		rows, err := connection.Query(actorUnsettledTurnsSQL(), sessionID, start)
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			var s span
			if err := rows.Scan(&s.begin, &s.end); err != nil {
				return err
			}
			rangeSet[s] = struct{}{}
		}
		return rows.Err()
	}()
	if err != nil {
		return nil, nil, err
	}
	ranges := slices.SortedFunc(maps.Keys(rangeSet), func(a, b span) int {
		if a.begin != b.begin {
			if a.begin < b.begin {
				return -1
			}
			return 1
		}
		if a.end < b.end {
			return -1
		}
		if a.end > b.end {
			return 1
		}
		return 0
	})
	var transcriptItems []*core.TranscriptItem
	for _, r := range ranges {
		before := &core.TranscriptCursor{Position: r.end, StableID: ""}
		items, err := readTranscriptRange(connection, sessionID, r.begin, before, nil)
		if err != nil {
			return nil, nil, err
		}
		transcriptItems = append(transcriptItems, items...)
	}
	tail, err := readTranscriptRange(connection, sessionID, start, nil, nil)
	if err != nil {
		return nil, nil, err
	}
	transcriptItems = append(transcriptItems, tail...)
	var total int
	if err := connection.QueryRow("SELECT COUNT(*) FROM materialized_transcript_items WHERE session_id=?1", sessionID).Scan(&total); err != nil {
		return nil, nil, err
	}
	window := &core.ProjectionWindow{
		OmittedItems:            total - len(transcriptItems),
		LatestTurnStartPosition: latestTurn,
	}
	if window.LatestUnreadEventOrdinal, err = readLatestUnreadEventOrdinal(connection, sessionID); err != nil {
		return nil, nil, err
	}
	if window.ProvisionalTitle, err = materializedProvisionalTitle(connection, sessionID); err != nil {
		return nil, nil, err
	}
	queued, err := readMaterializedQueuedPrompts(connection, sessionID)
	if err != nil {
		return nil, nil, err
	}
	materialized := materializedFromFields(sessionID, fields, transcriptItems, queued)
	if err := materialized.Validate(); err != nil {
		return nil, nil, err
	}
	return &materialized, window, nil
}

func LoadTranscriptHistory(sessionID string, before *core.TranscriptCursor, limit int) (*core.TranscriptHistoryPage, error) {
	return loadTranscriptHistoryFrom(DatabasePath(), sessionID, before, limit)
}

func loadTranscriptHistoryFrom(path string, sessionID string, before *core.TranscriptCursor, limit int) (*core.TranscriptHistoryPage, error) {
	connection, done, err := openReadSnapshot(path)
	if err != nil {
		return nil, err
	}
	defer done()
	fields, err := readMaterializedSessionFields(connection, sessionID)
	if err != nil || fields == nil {
		return nil, err
	}
	limit = min(max(limit, 1), 256)
	pageLimit := limit + 1
	items, err := readTranscriptRange(connection, sessionID, 0, before, &pageLimit)
	if err != nil {
		return nil, err
	}
	hasMore := len(items) > limit
	if hasMore {
		items = items[1:]
	}
	page := &core.TranscriptHistoryPage{Items: items, Frontier: fields.appliedEventOrdinal}
	if hasMore {
		cursor := core.TranscriptCursorOf(items[0])
		page.Before = &cursor
	}
	return page, nil
}

func readTranscriptRange(connection Queryer, sessionID string, start uint64, before *core.TranscriptCursor, limit *int) ([]*core.TranscriptItem, error) {
	position, stableID := uint64(math.MaxInt64), ""
	if before != nil {
		position, stableID = before.Position, before.StableID
	}
	rowLimit := int64(-1)
	if limit != nil {
		rowLimit = int64(*limit)
	}
	items, err := queryStoredTranscriptItems(connection, sessionID, `SELECT stable_id, position, latest_content_event_ordinal, created_at_ms,
                last_changed_at_ms, body_json FROM materialized_transcript_items
         WHERE session_id=?1 AND position>=?2
           AND (position,stable_id)<(?3,?4)
         ORDER BY position DESC, stable_id DESC LIMIT ?5`, sessionID, start, position, stableID, rowLimit)
	if err != nil {
		return nil, err
	}
	slices.Reverse(items)
	return items, nil
}

// LoadProjectionReferences rehydrates referenced historical tools and
// possible raw-result terminal matches before deriving a delta. A window is a
// cache, never evidence that a durable tool does not exist.
func LoadProjectionReferences(projection *core.MaterializedSession, events []core.RelayEvent) ([]*core.TranscriptItem, error) {
	return loadProjectionReferencesFrom(DatabasePath(), projection, events)
}

func loadProjectionReferencesFrom(path string, projection *core.MaterializedSession, events []core.RelayEvent) ([]*core.TranscriptItem, error) {
	references, err := transcript.HistoricalReferencesOf(events)
	if err != nil {
		return nil, err
	}
	retained := make(map[string]struct{}, len(projection.Transcript))
	for _, item := range projection.Transcript {
		retained[item.StableID] = struct{}{}
	}
	ids := make([]string, 0, len(references.IDs))
	for _, id := range references.IDs {
		if _, ok := retained[id]; !ok {
			ids = append(ids, id)
		}
	}
	if len(ids) == 0 {
		return []*core.TranscriptItem{}, nil
	}
	connection, done, err := openReadSnapshot(path)
	if err != nil {
		return nil, err
	}
	defer done()
	// Reconcile only explicit keys. Terminal owners are indexed in the live
	// projection; output for evicted owners stays parked until read or
	// claimed.
	encoded, err := databaseMarshalJSON(ids)
	if err != nil {
		return nil, err
	}
	return queryStoredTranscriptItems(connection, projection.SessionID, `SELECT stable_id, position, latest_content_event_ordinal, created_at_ms,
                last_changed_at_ms, body_json FROM materialized_transcript_items
         WHERE session_id=?1 AND stable_id IN (SELECT value FROM json_each(?2))
         ORDER BY position,stable_id`, projection.SessionID, string(encoded))
}

// LoadMaterializedProjectionTail loads a projection carrying only the end of
// its transcript.
//
// The steady-state poll reloads a session's projection every time anything
// about it moves. Loading the whole transcript to do that is work
// proportional to everything that has ever happened in the conversation —
// 635 MiB and 28,066 items on one measured session — for a view that shows
// the last few hundred entries. This reads the window instead, plus the two
// facts that live outside it, each with one indexed query. See
// core.ProjectionWindow. Both results are nil when the session has no
// projection.
func LoadMaterializedProjectionTail(sessionID string, transcriptLimit int) (*core.MaterializedSession, *core.ProjectionWindow, error) {
	return loadMaterializedProjectionTailFrom(DatabasePath(), sessionID, transcriptLimit)
}

func loadMaterializedProjectionTailFrom(path string, sessionID string, transcriptLimit int) (*core.MaterializedSession, *core.ProjectionWindow, error) {
	// Frontier, mutable transcript bodies, and window metadata must describe
	// one WAL snapshot even if the daemon commits between these queries.
	connection, done, err := openReadSnapshot(path)
	if err != nil {
		return nil, nil, err
	}
	defer done()
	fields, err := readMaterializedSessionFields(connection, sessionID)
	if err != nil || fields == nil {
		return nil, nil, err
	}
	transcriptItems, err := readMaterializedTranscript(connection, sessionID, &transcriptLimit)
	if err != nil {
		return nil, nil, err
	}
	var totalItems int
	if err := connection.QueryRow("SELECT COUNT(*) FROM materialized_transcript_items WHERE session_id = ?1", sessionID).Scan(&totalItems); err != nil {
		return nil, nil, err
	}
	window := &core.ProjectionWindow{OmittedItems: max(totalItems-len(transcriptItems), 0)}
	if window.LatestUnreadEventOrdinal, err = readLatestUnreadEventOrdinal(connection, sessionID); err != nil {
		return nil, nil, err
	}
	if window.ProvisionalTitle, err = materializedProvisionalTitle(connection, sessionID); err != nil {
		return nil, nil, err
	}
	if window.LatestTurnStartPosition, err = lastMaterializedTurnStart(connection, sessionID); err != nil {
		return nil, nil, err
	}
	queued, err := readMaterializedQueuedPrompts(connection, sessionID)
	if err != nil {
		return nil, nil, err
	}
	materialized := materializedFromFields(sessionID, fields, transcriptItems, queued)
	if err := materialized.Validate(); err != nil {
		return nil, nil, err
	}
	return &materialized, window, nil
}

// MaterializedEventFrontier reads only the projection's event frontier
// (ordinal and digest). Deciding whether a stored projection already matches
// an archive costs one row this way, instead of deserializing every
// transcript item to compare two integers.
func MaterializedEventFrontier(sessionID string) (uint64, string, bool, error) {
	return materializedEventFrontierFrom(DatabasePath(), sessionID)
}

func materializedEventFrontierFrom(path string, sessionID string) (uint64, string, bool, error) {
	connection, err := openReader(path)
	if err != nil {
		return 0, "", false, err
	}
	defer connection.Close()
	var ordinal uint64
	var digest string
	err = connection.QueryRow(`SELECT applied_event_ordinal, applied_event_digest
             FROM materialized_sessions WHERE session_id = ?1`, sessionID).Scan(&ordinal, &digest)
	if errors.Is(err, sql.ErrNoRows) {
		return 0, "", false, nil
	}
	if err != nil {
		return 0, "", false, err
	}
	return ordinal, digest, true, nil
}

// ReplaceMaterializedQueuedPrompts replaces a session's durable prompt queue
// without touching its transcript or event frontier. Resume uses this when it
// keeps the stored projection but still has to drop the queue the archive
// carried.
func ReplaceMaterializedQueuedPrompts(sessionID string, queuedPrompts []core.MaterializedQueuedPrompt) error {
	prompts := append([]core.MaterializedQueuedPrompt(nil), queuedPrompts...)
	_, err := submitDatabaseWrite("replace_materialized_queued_prompts", func(*Connection) (unitResult, error) {
		return unitResult{}, replaceMaterializedQueuedPromptsIn(DatabasePath(), sessionID, prompts)
	})
	return err
}

func replaceMaterializedQueuedPromptsIn(path string, sessionID string, queuedPrompts []core.MaterializedQueuedPrompt) error {
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	exists, err := sessionExists(tx, sessionID)
	if err != nil {
		return err
	}
	if !exists {
		return fmt.Errorf("unknown session %s", sessionID)
	}
	if err := replaceMaterializedQueue(tx, sessionID, queuedPrompts); err != nil {
		return err
	}
	return tx.Commit()
}

// LoadTranscribedSessionActivity is the activity watermark of every session
// whose projection holds at least one transcript item, by session id.
//
// This is a change token, not a projection: session indexing needs to know
// which live conversations have moved since it last looked, and loading each
// one's transcript to find out would cost the whole corpus every sync.
func LoadTranscribedSessionActivity() (map[string]*int64, error) {
	return loadTranscribedSessionActivityFrom(DatabasePath())
}

func loadTranscribedSessionActivityFrom(path string) (map[string]*int64, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	rows, err := connection.Query(`SELECT session_id, last_activity_at_ms
         FROM materialized_sessions s
         WHERE EXISTS (
             SELECT 1 FROM materialized_transcript_items i
             WHERE i.session_id = s.session_id
         )`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	activity := map[string]*int64{}
	for rows.Next() {
		var sessionID string
		var lastActivityAtMs *int64
		if err := rows.Scan(&sessionID, &lastActivityAtMs); err != nil {
			return nil, err
		}
		activity[sessionID] = lastActivityAtMs
	}
	return activity, rows.Err()
}

// LoadMaterializedQueuedPrompts loads only the durable prompt queues without
// deserializing transcript rows. Dashboard startup uses this path so work is
// proportional to queued prompts, not to the complete retained conversation
// history.
func LoadMaterializedQueuedPrompts() (map[string][]core.MaterializedQueuedPrompt, error) {
	return loadMaterializedQueuedPromptsFrom(DatabasePath())
}

func loadMaterializedQueuedPromptsFrom(path string) (map[string][]core.MaterializedQueuedPrompt, error) {
	return readAllMaterializedQueuedPrompts(path, func(_ string, err error) error { return err })
}

// LoadMaterializedQueuedPromptsForStartup seeds independent browser queues at
// daemon startup. A malformed queue stays in SQLite for recovery and does not
// hide the other sessions' queues.
func LoadMaterializedQueuedPromptsForStartup() (map[string][]core.MaterializedQueuedPrompt, error) {
	return readAllMaterializedQueuedPrompts(DatabasePath(), func(sessionID string, err error) error {
		slog.Warn("could not seed the session's stored prompt queue", "session_id", sessionID, "error", err)
		return nil
	})
}

// decodeQueuedPrompt decodes one materialized_queued_prompts row's columns.
func decodeQueuedPrompt(sessionID, commandID, kindJSON, contentJSON string, queuedAtMs int64, acceptedOrdinal *uint64) (core.MaterializedQueuedPrompt, error) {
	kind, err := core.UnmarshalQueuedCommandKindJSON([]byte(kindJSON))
	if err != nil {
		return core.MaterializedQueuedPrompt{}, fmt.Errorf("parse materialized queue entry kind for session %s: %w", sessionID, err)
	}
	var content []any
	if err := json.Unmarshal([]byte(contentJSON), &content); err != nil {
		return core.MaterializedQueuedPrompt{}, fmt.Errorf("parse materialized queued prompt for session %s: %w", sessionID, err)
	}
	if content == nil {
		return core.MaterializedQueuedPrompt{}, fmt.Errorf("parse materialized queued prompt for session %s: invalid type: null, expected a sequence", sessionID)
	}
	return core.MaterializedQueuedPrompt{
		CommandID:       commandID,
		Kind:            kind,
		Content:         content,
		QueuedAtMs:      queuedAtMs,
		AcceptedOrdinal: acceptedOrdinal,
	}, nil
}

func readAllMaterializedQueuedPrompts(path string, invalidQueue func(sessionID string, err error) error) (map[string][]core.MaterializedQueuedPrompt, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	rows, err := connection.Query(`SELECT session_id, command_id, kind_json, content_json, queued_at_ms, accepted_ordinal
         FROM materialized_queued_prompts
         ORDER BY session_id, ordinal`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	queues := map[string][]core.MaterializedQueuedPrompt{}
	var invalidSession *string
	for rows.Next() {
		row, err := scanSQLRow(rows, 6)
		if err != nil {
			return nil, err
		}
		sessionID, err := sqlRowText(row, 0)
		if err != nil {
			return nil, err
		}
		if invalidSession != nil && *invalidSession == sessionID {
			continue
		}
		prompt, err := func() (core.MaterializedQueuedPrompt, error) {
			commandID, err := sqlRowText(row, 1)
			if err != nil {
				return core.MaterializedQueuedPrompt{}, err
			}
			kindJSON, err := sqlRowText(row, 2)
			if err != nil {
				return core.MaterializedQueuedPrompt{}, err
			}
			contentJSON, err := sqlRowText(row, 3)
			if err != nil {
				return core.MaterializedQueuedPrompt{}, err
			}
			queuedAtMs, err := sqlRowInt64(row, 4)
			if err != nil {
				return core.MaterializedQueuedPrompt{}, err
			}
			var acceptedOrdinal *uint64
			if row[5] != nil {
				ordinal, err := sqlRowUint64(row, 5)
				if err != nil {
					return core.MaterializedQueuedPrompt{}, err
				}
				acceptedOrdinal = &ordinal
			}
			return decodeQueuedPrompt(sessionID, commandID, kindJSON, contentJSON, queuedAtMs, acceptedOrdinal)
		}()
		if err != nil {
			if err := invalidQueue(sessionID, err); err != nil {
				return nil, err
			}
			delete(queues, sessionID)
			id := sessionID
			invalidSession = &id
			continue
		}
		queues[sessionID] = append(queues[sessionID], prompt)
	}
	return queues, rows.Err()
}

func LoadMaterializedSessionFrom(path string, sessionID string) (*core.MaterializedSession, error) {
	connection, done, err := openReadSnapshot(path)
	if err != nil {
		return nil, err
	}
	defer done()
	return loadMaterializedSessionWith(connection, sessionID)
}

func loadMaterializedSessionWith(connection *sql.Tx, sessionID string) (*core.MaterializedSession, error) {
	return loadMaterializedSessionBounded(connection, sessionID, nil)
}

// LoadMaterializedRecentTurns is the projection with its transcript cut to
// the session's last turns user turns. Rendering a session's recent turns
// then reads those turns rather than everything that ever happened in it. A
// session with fewer turns is loaded whole.
func LoadMaterializedRecentTurns(sessionID string, turns int) (*core.MaterializedSession, error) {
	return LoadMaterializedRecentTurnsFrom(DatabasePath(), sessionID, turns)
}

func LoadMaterializedRecentTurnsFrom(path string, sessionID string, turns int) (*core.MaterializedSession, error) {
	connection, done, err := openReadSnapshot(path)
	if err != nil {
		return nil, err
	}
	defer done()
	var limit *int
	if turns == 0 {
		zero := 0
		limit = &zero
	} else {
		var position uint64
		var stableID string
		err := connection.QueryRow(`SELECT position, stable_id
                 FROM materialized_transcript_items
                 WHERE session_id = ?1
                   AND json_extract(
                       CASE
                           WHEN stable_id GLOB 'user:*' OR stable_id GLOB 'user-*'
                           THEN body_json
                           ELSE '{}'
                       END,
                       '$.kind'
                   ) = 'user'
                 ORDER BY position DESC, stable_id DESC
                 LIMIT 1 OFFSET ?2`, sessionID, int64(turns-1)).Scan(&position, &stableID)
		switch {
		case errors.Is(err, sql.ErrNoRows):
		case err != nil:
			return nil, err
		default:
			// The transcript is ordered by position, then stable id, so the
			// items from the start onward are exactly the newest this many.
			var count int
			if err := connection.QueryRow(`SELECT COUNT(*)
             FROM materialized_transcript_items
             WHERE session_id = ?1
               AND (position > ?2 OR (position = ?2 AND stable_id >= ?3))`,
				sessionID, int64(position), stableID).Scan(&count); err != nil {
				return nil, err
			}
			limit = &count
		}
	}
	return loadMaterializedSessionBounded(connection, sessionID, limit)
}

func loadMaterializedSessionBounded(connection *sql.Tx, sessionID string, transcriptLimit *int) (*core.MaterializedSession, error) {
	fields, err := readMaterializedSessionFields(connection, sessionID)
	if err != nil || fields == nil {
		return nil, err
	}
	transcriptItems, err := readMaterializedTranscript(connection, sessionID, transcriptLimit)
	if err != nil {
		return nil, err
	}
	queued, err := readMaterializedQueuedPrompts(connection, sessionID)
	if err != nil {
		return nil, err
	}
	materialized := materializedFromFields(sessionID, fields, transcriptItems, queued)
	if err := materialized.Validate(); err != nil {
		return nil, err
	}
	return &materialized, nil
}

// materializedSessionFields is everything a projection holds apart from its
// transcript and its queue.
type materializedSessionFields struct {
	appliedEventOrdinal uint64
	appliedEventDigest  string
	lastActivityAtMs    *int64
	execution           core.MaterializedExecutionState
	sessionTitle        *string
	configuration       core.SessionConfiguration
	pendingElicitations []core.ElicitationRequest
	activeTurn          *core.MaterializedTurn
	lastTurnOutcome     *core.MaterializedTurnOutcome
}

func readMaterializedSessionFields(connection Queryer, sessionID string) (*materializedSessionFields, error) {
	var fields materializedSessionFields
	var execution, configurationJSON, pendingElicitationsJSON string
	var runningStartedAtMs *int64
	var activeTurnJSON, lastTurnOutcomeJSON *string
	err := connection.QueryRow(`SELECT applied_event_ordinal, applied_event_digest, last_activity_at_ms,
                    execution_state, running_started_at_ms, session_title, configuration_json,
                    pending_elicitations_json, active_turn_json, last_turn_outcome_json
             FROM materialized_sessions WHERE session_id = ?1`, sessionID).
		Scan(&fields.appliedEventOrdinal, &fields.appliedEventDigest, &fields.lastActivityAtMs, &execution,
			&runningStartedAtMs, &fields.sessionTitle, &configurationJSON, &pendingElicitationsJSON,
			&activeTurnJSON, &lastTurnOutcomeJSON)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	// Rust's cfg(test) hook between the frontier and transcript reads.
	afterMaterializedFrontierRead()
	if fields.execution, err = parseMaterializedExecution(execution, runningStartedAtMs); err != nil {
		return nil, err
	}
	if err := json.Unmarshal([]byte(configurationJSON), &fields.configuration); err != nil {
		return nil, fmt.Errorf("parse materialized configuration for session %s: %w", sessionID, err)
	}
	if err := json.Unmarshal([]byte(pendingElicitationsJSON), &fields.pendingElicitations); err != nil {
		return nil, fmt.Errorf("parse pending elicitations for session %s: %w", sessionID, err)
	}
	if fields.activeTurn, err = decodeMaterializedTurn(activeTurnJSON, sessionID); err != nil {
		return nil, err
	}
	if fields.lastTurnOutcome, err = decodeLastTurnOutcome(lastTurnOutcomeJSON, sessionID); err != nil {
		return nil, err
	}
	return &fields, nil
}

// readMaterializedTranscript reads a session's transcript, oldest first. A
// non-nil limit reads only that many items from the end, walking the
// materialized_transcript_position index backwards so the read costs the rows
// it returns.
func readMaterializedTranscript(connection Queryer, sessionID string, limit *int) ([]*core.TranscriptItem, error) {
	if limit == nil {
		return queryStoredTranscriptItems(connection, sessionID, `SELECT stable_id, position, latest_content_event_ordinal, created_at_ms,
                    last_changed_at_ms, body_json
             FROM materialized_transcript_items
             WHERE session_id = ?1
             ORDER BY position, stable_id`, sessionID)
	}
	items, err := queryStoredTranscriptItems(connection, sessionID, `SELECT stable_id, position, latest_content_event_ordinal, created_at_ms,
                    last_changed_at_ms, body_json
             FROM materialized_transcript_items
             WHERE session_id = ?1
             ORDER BY position DESC, stable_id DESC
             LIMIT ?2`, sessionID, int64(*limit))
	if err != nil {
		return nil, err
	}
	// The bounded query walks the index backwards to bound what it reads;
	// every caller wants the transcript in the order it was written.
	slices.Reverse(items)
	return items, nil
}

func readMaterializedQueuedPrompts(connection Queryer, sessionID string) ([]core.MaterializedQueuedPrompt, error) {
	rows, err := connection.Query(`SELECT command_id, kind_json, content_json, queued_at_ms, accepted_ordinal
         FROM materialized_queued_prompts
         WHERE session_id = ?1
         ORDER BY ordinal`, sessionID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	type stored struct {
		commandID, kindJSON, contentJSON string
		queuedAtMs                       int64
		acceptedOrdinal                  *uint64
	}
	var storedRows []stored
	for rows.Next() {
		var row stored
		if err := rows.Scan(&row.commandID, &row.kindJSON, &row.contentJSON, &row.queuedAtMs, &row.acceptedOrdinal); err != nil {
			return nil, err
		}
		storedRows = append(storedRows, row)
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}
	prompts := make([]core.MaterializedQueuedPrompt, 0, len(storedRows))
	for _, row := range storedRows {
		prompt, err := decodeQueuedPrompt(sessionID, row.commandID, row.kindJSON, row.contentJSON, row.queuedAtMs, row.acceptedOrdinal)
		if err != nil {
			return nil, err
		}
		prompts = append(prompts, prompt)
	}
	return prompts, nil
}

// SaveMaterializedSession replaces a complete projection, primarily when
// seeding a restored checkpoint. Operational SessionRecord metadata and read
// receipts are not modified.
func SaveMaterializedSession(materialized *core.MaterializedSession) error {
	materializedCopy := *materialized
	_, err := submitDatabaseWrite("save_materialized_session", func(*Connection) (unitResult, error) {
		return unitResult{}, SaveMaterializedSessionTo(DatabasePath(), &materializedCopy)
	})
	return err
}

// RepairMaterializedSessionIfCurrent repairs only the projection whose exact
// cursor the caller observed. A concurrent projector owns its committed
// history, even when repair rewinds.
func RepairMaterializedSessionIfCurrent(materialized *core.MaterializedSession, expectedOrdinal uint64, expectedDigest string) (bool, error) {
	materializedCopy := *materialized
	return submitDatabaseWrite("repair_materialized_session_if_current", func(*Connection) (bool, error) {
		return repairMaterializedSessionIfCurrentTo(DatabasePath(), &materializedCopy, expectedOrdinal, expectedDigest)
	})
}

func repairMaterializedSessionIfCurrentTo(path string, materialized *core.MaterializedSession, expectedOrdinal uint64, expectedDigest string) (bool, error) {
	if err := materialized.Validate(); err != nil {
		return false, err
	}
	connection, err := open(path)
	if err != nil {
		return false, err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return false, err
	}
	defer tx.Rollback()
	currentOrdinal, currentDigest := uint64(0), core.RelayEventGenesisDigest
	err = tx.QueryRow("SELECT applied_event_ordinal, applied_event_digest FROM materialized_sessions WHERE session_id=?1",
		materialized.SessionID).Scan(&currentOrdinal, &currentDigest)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return false, err
	}
	if errors.Is(err, sql.ErrNoRows) {
		currentOrdinal, currentDigest = 0, core.RelayEventGenesisDigest
	}
	if currentOrdinal != expectedOrdinal || currentDigest != expectedDigest {
		return false, nil
	}
	// A connection may already owe an ACK for this durable frontier. That ACK
	// needs no database write, so even a successful CAS cannot authorize
	// rewind.
	if materialized.AppliedEventOrdinal < currentOrdinal {
		return false, fmt.Errorf("refusing projection repair from durable frontier %d:%s to older frontier %d:%s; the durable projection is unchanged",
			currentOrdinal, currentDigest, materialized.AppliedEventOrdinal, materialized.AppliedEventDigest)
	}
	exists, err := sessionExists(tx, materialized.SessionID)
	if err != nil {
		return false, err
	}
	if !exists {
		return false, fmt.Errorf("unknown session %s", materialized.SessionID)
	}
	if err := writeMaterializedSession(tx, materialized); err != nil {
		return false, err
	}
	if err := tx.Commit(); err != nil {
		return false, err
	}
	return true, nil
}

func SaveMaterializedSessionTo(path string, materialized *core.MaterializedSession) error {
	if err := materialized.Validate(); err != nil {
		return err
	}
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	exists, err := sessionExists(tx, materialized.SessionID)
	if err != nil {
		return err
	}
	if !exists {
		return fmt.Errorf("unknown session %s", materialized.SessionID)
	}
	if err := writeMaterializedSession(tx, materialized); err != nil {
		return err
	}
	return tx.Commit()
}

// InstallLiveSessionBaseline installs one current-state cut only before any
// attachment establishes history. The notice, live owner state, and both
// native replay phases commit together.
func InstallLiveSessionBaseline(materialized *core.MaterializedSession, baseline core.LiveSessionBaseline) (bool, error) {
	materializedCopy := *materialized
	return submitDatabaseWrite("install_live_session_baseline", func(*Connection) (bool, error) {
		return installLiveSessionBaselineTo(DatabasePath(), &materializedCopy, &baseline)
	})
}

func installLiveSessionBaselineTo(path string, materialized *core.MaterializedSession, baseline *core.LiveSessionBaseline) (bool, error) {
	connection, err := open(path)
	if err != nil {
		return false, err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return false, err
	}
	defer tx.Rollback()
	installed, err := installLiveSessionBaselineWith(tx, materialized, baseline)
	if err != nil {
		return false, err
	}
	if err := tx.Commit(); err != nil {
		return false, err
	}
	if installed && len(materialized.PendingElicitations) != 0 {
		NotifyMailboxOutboxChanged()
	}
	return installed, nil
}

func installLiveSessionBaselineWith(tx *sql.Tx, materialized *core.MaterializedSession, baseline *core.LiveSessionBaseline) (bool, error) {
	if err := materialized.Validate(); err != nil {
		return false, err
	}
	if materialized.SessionID != baseline.State.SessionID ||
		materialized.AppliedEventOrdinal != baseline.State.LatestOrdinal ||
		materialized.AppliedEventDigest != baseline.State.LatestDigest {
		return false, errors.New("live baseline projection does not match its worker cut")
	}
	exists, err := sessionExists(tx, materialized.SessionID)
	if err != nil {
		return false, err
	}
	if !exists {
		return false, fmt.Errorf("unknown session %s", materialized.SessionID)
	}
	var frontier uint64
	err = tx.QueryRow("SELECT applied_event_ordinal FROM materialized_sessions WHERE session_id=?1", materialized.SessionID).Scan(&frontier)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return false, err
	}
	if err == nil && frontier > 0 {
		return false, nil
	}
	cursor := core.RelayCursor{Ordinal: baseline.State.LatestOrdinal, Digest: baseline.State.LatestDigest}
	if err := seedAdoptedNativeAgents(tx, materialized.SessionID, baseline.State.NativeAgents, &cursor, 0); err != nil {
		return false, err
	}
	if staged := baseline.NativeAgentReplay; staged != nil {
		if _, err := tx.Exec("INSERT OR IGNORE INTO native_agent_replay(owner) VALUES (?1)", materialized.SessionID); err != nil {
			return false, err
		}
		if err := seedAdoptedNativeAgents(tx, materialized.SessionID, *staged, &cursor, 1); err != nil {
			return false, err
		}
	}
	if err := writeMaterializedSession(tx, materialized); err != nil {
		return false, err
	}
	var turnID *uint64
	if materialized.ActiveTurn != nil {
		turnID = materialized.ActiveTurn.AcceptedOrdinal
	}
	for _, request := range materialized.PendingElicitations {
		recordedAtMs := core.EpochMillis()
		if materialized.LastActivityAtMs != nil {
			recordedAtMs = *materialized.LastActivityAtMs
		}
		request := request
		if err := insertAPIEvent(tx, materialized.SessionID, recordedAtMs, core.ApiEventDataInputRequired{
			Request: &request,
			TurnID:  turnID,
		}); err != nil {
			return false, err
		}
	}
	return true, nil
}

// ProjectionPage is one relay page being applied inside a single write
// transaction. The relay retains everything past the last acknowledgement,
// so a page that fails part-way rolls back to the previous durable frontier
// and is simply redelivered. Only a committed page may be acknowledged.
type ProjectionPage struct {
	sessionID      string
	transaction    *sql.Tx
	appliedOrdinal uint64
	appliedDigest  string
	// inputRequiredEventFrontier survives a projection reset so historical
	// forms never become new mail.
	inputRequiredEventFrontier uint64
	operatorAwayEventFrontier  uint64
	dirty                      bool
	pending                    core.MaterializedSessionMutation
	// pendingTranscript is keyed by stable id; Flush writes it in key order,
	// as Rust's BTreeMap iterates.
	pendingTranscript map[string]*pendingTranscriptMutation
	pendingTurns      []core.MaterializedTurnOutcome
	// usageConfiguration: configuration changes are coalesced for the
	// session, but selection is captured at each turn start before later
	// events can overwrite it.
	usageConfiguration core.SessionConfiguration
	pendingEvents      []pendingAPIEvent
}

type pendingTranscriptMutation struct {
	finalMutation      core.TranscriptMutation
	removeBeforeUpsert bool
}

// pendingAPIEvent is one entry of ProjectionPage.pendingEvents: Rust's
// `(i64, ApiEventData)`, the receipt time and the event.
type pendingAPIEvent struct {
	recordedAtMs int64
	event        core.ApiEventData
}

// Apply applies the projection effects of the next relay event to the open
// page. The event must continue the chain the page has reached so far, which
// is the persisted frontier plus every event already applied to this page.
func (p *ProjectionPage) Apply(eventOrdinal uint64, previousEventDigest string, eventDigest string, mutation *core.MaterializedSessionMutation) (core.ProjectionApplyOutcome, error) {
	if eventOrdinal == 0 {
		return 0, errors.New("relay event ordinal must be positive")
	}
	// A v2 event carries no chain link (empty previous digest). Its
	// continuity to the projection frontier is proven by ordinal contiguity
	// plus the attach cursor the controller validated against the worker, not
	// by an in-record back-reference; divergence is caught there, before any
	// event is applied.
	chained := previousEventDigest != ""
	if chained {
		if err := core.ValidateRelayEventDigest(previousEventDigest, "previous relay event digest"); err != nil {
			return 0, err
		}
	}
	if err := core.ValidateRelayEventFrontier(eventOrdinal, eventDigest, "relay event frontier"); err != nil {
		return 0, err
	}
	sessionID := p.sessionID
	applied := p.appliedOrdinal
	if eventOrdinal < applied {
		return core.ProjectionApplyOutcomeAlreadyApplied, nil
	}
	if eventOrdinal == applied {
		if eventDigest != p.appliedDigest {
			return 0, fmt.Errorf("relay event digest mismatch for session %s at ordinal %d: projection has %s, received %s",
				sessionID, eventOrdinal, p.appliedDigest, eventDigest)
		}
		return core.ProjectionApplyOutcomeAlreadyApplied, nil
	}
	if applied == math.MaxUint64 {
		return 0, errors.New("materialized event ordinal overflow")
	}
	expected := applied + 1
	if eventOrdinal != expected {
		return 0, fmt.Errorf("relay event gap for session %s: expected ordinal %d, received %d", sessionID, expected, eventOrdinal)
	}
	if chained && previousEventDigest != p.appliedDigest {
		return 0, fmt.Errorf("relay event chain diverged for session %s before ordinal %d: projection has %s, event follows %s",
			sessionID, eventOrdinal, p.appliedDigest, previousEventDigest)
	}

	if mutation.NativeAgent != nil {
		if err := applyNativeAgentEvent(p.transaction, sessionID, mutation.NativeAgent); err != nil {
			return 0, err
		}
	}
	if mutation.ClearMailbox {
		if err := clearMailboxWith(p.transaction, sessionID, eventOrdinal); err != nil {
			return 0, err
		}
	}
	if activityAtMs := mutation.LastActivityAtMs; activityAtMs != nil {
		latest := *activityAtMs
		if p.pending.LastActivityAtMs != nil {
			latest = max(*p.pending.LastActivityAtMs, latest)
		}
		p.pending.LastActivityAtMs = &latest
	}
	if mutation.Execution != nil {
		execution := *mutation.Execution
		p.pending.Execution = &execution
	}
	if mutation.SessionTitle != nil {
		title := *mutation.SessionTitle
		if title != nil && isRustBlank(*title) {
			return 0, errors.New("materialized session title cannot be empty")
		}
		var copied *string
		if title != nil {
			text := *title
			copied = &text
		}
		p.pending.SessionTitle = &copied
	}
	if mutation.Configuration != nil {
		p.usageConfiguration = *mutation.Configuration
		configuration := *mutation.Configuration
		p.pending.Configuration = &configuration
	}
	if p.pendingTranscript == nil {
		p.pendingTranscript = map[string]*pendingTranscriptMutation{}
	}
	for _, itemMutation := range mutation.Transcript {
		switch change := itemMutation.(type) {
		case core.TranscriptMutationUpsert:
			item := change.Item
			if err := item.Validate(eventOrdinal); err != nil {
				return 0, err
			}
			entry, ok := p.pendingTranscript[item.StableID]
			if !ok {
				entry = &pendingTranscriptMutation{finalMutation: core.TranscriptMutationUpsert{Item: item}}
				p.pendingTranscript[item.StableID] = entry
			}
			if _, removed := entry.finalMutation.(core.TranscriptMutationRemove); removed {
				entry.removeBeforeUpsert = true
			}
			entry.finalMutation = core.TranscriptMutationUpsert{Item: item}
		case core.TranscriptMutationRemove:
			if isRustBlank(change.StableID) {
				return 0, errors.New("cannot remove a transcript item with an empty stable id")
			}
			removed := core.TranscriptMutationRemove{StableID: change.StableID}
			if entry, ok := p.pendingTranscript[change.StableID]; ok {
				entry.finalMutation = removed
			} else {
				p.pendingTranscript[change.StableID] = &pendingTranscriptMutation{finalMutation: removed}
			}
		}
	}
	if mutation.QueuedPrompts != nil {
		queued := append([]core.MaterializedQueuedPrompt(nil), (*mutation.QueuedPrompts)...)
		p.pending.QueuedPrompts = &queued
	}
	if mutation.PendingElicitations != nil {
		pending := append([]core.ElicitationRequest(nil), (*mutation.PendingElicitations)...)
		p.pending.PendingElicitations = &pending
	}
	p.pending.ConfigResults = append(p.pending.ConfigResults, mutation.ConfigResults...)
	if mutation.ActiveTurn != nil {
		turn := *mutation.ActiveTurn
		if turn != nil {
			if err := recordTurnSelection(p.transaction, sessionID, turn.CommandID, &p.usageConfiguration); err != nil {
				return 0, err
			}
		}
		var copied *core.MaterializedTurn
		if turn != nil {
			value := *turn
			copied = &value
		}
		p.pending.ActiveTurn = &copied
	}
	if mutation.ClearTurnOutcome {
		p.pending.ClearTurnOutcome = true
		p.pending.LastTurnOutcome = nil
	}
	if mutation.LastTurnOutcome != nil {
		outcome := *mutation.LastTurnOutcome
		p.pendingTurns = append(p.pendingTurns, outcome)
		p.pending.LastTurnOutcome = &outcome
	}
	if mutation.ProviderCost != nil {
		cost := *mutation.ProviderCost
		p.pending.ProviderCost = &cost
	}
	recordedAtMs := int64(0)
	if mutation.LastActivityAtMs != nil {
		recordedAtMs = *mutation.LastActivityAtMs
	}
	for _, event := range mutation.APIEvents {
		switch event.(type) {
		case core.ApiEventDataInputRequired:
			if eventOrdinal <= p.inputRequiredEventFrontier {
				continue
			}
		case core.ApiEventDataInputResolved:
			if eventOrdinal <= p.operatorAwayEventFrontier {
				continue
			}
		}
		p.pendingEvents = append(p.pendingEvents, pendingAPIEvent{recordedAtMs: recordedAtMs, event: event})
	}
	p.appliedOrdinal = eventOrdinal
	p.appliedDigest = eventDigest
	p.dirty = true
	return core.ProjectionApplyOutcomeApplied, nil
}

// SetResolvedTitle publishes a session title with the same SQLite commit as
// its relay page. A checkpoint can retire the actor immediately after this
// commit.
func (p *ProjectionPage) SetResolvedTitle(title *string) error {
	_, err := p.transaction.Exec(`UPDATE sessions SET acp_session_title = ?2
             WHERE session_id = ?1 AND acp_session_title IS NOT ?2`,
		p.sessionID, title)
	return err
}

// Flush persists the coalesced final state of this page. Intermediate event
// frontiers are useful only for chain validation: a page commits or rolls
// back as a unit, so writing them individually adds no recovery value.
func (p *ProjectionPage) Flush() error {
	if !p.dirty {
		return nil
	}
	tx := p.transaction
	sessionID := p.sessionID
	if p.pending.Execution != nil {
		state, startedAtMs := materializedExecutionColumns(*p.pending.Execution)
		if _, err := tx.Exec(`UPDATE materialized_sessions
                 SET execution_state = ?2, running_started_at_ms = ?3
                 WHERE session_id = ?1`, sessionID, state, startedAtMs); err != nil {
			return err
		}
	}
	if p.pending.SessionTitle != nil {
		if _, err := tx.Exec("UPDATE materialized_sessions SET session_title = ?2 WHERE session_id = ?1", sessionID, *p.pending.SessionTitle); err != nil {
			return err
		}
	}
	if p.pending.Configuration != nil {
		configuration, err := databaseMarshalJSON(p.pending.Configuration)
		if err != nil {
			return err
		}
		if _, err := tx.Exec("UPDATE materialized_sessions SET configuration_json = ?2 WHERE session_id = ?1", sessionID, string(configuration)); err != nil {
			return err
		}
	}
	for _, stableID := range slices.Sorted(maps.Keys(p.pendingTranscript)) {
		pending := p.pendingTranscript[stableID]
		switch change := pending.finalMutation.(type) {
		case core.TranscriptMutationUpsert:
			// A remove followed by an upsert deliberately starts a new item
			// identity. Preserve that boundary even though other repeated
			// updates are coalesced to one write.
			if pending.removeBeforeUpsert {
				if _, err := tx.Exec(`DELETE FROM materialized_transcript_items
                             WHERE session_id = ?1 AND stable_id = ?2`, sessionID, change.Item.StableID); err != nil {
					return err
				}
			}
			item := change.Item
			if err := upsertTranscriptItem(tx, sessionID, &item); err != nil {
				return err
			}
		case core.TranscriptMutationRemove:
			if _, err := tx.Exec(`DELETE FROM materialized_transcript_items
                         WHERE session_id = ?1 AND stable_id = ?2`, sessionID, change.StableID); err != nil {
				return err
			}
		}
	}
	if p.pending.QueuedPrompts != nil {
		if err := replaceMaterializedQueue(tx, sessionID, *p.pending.QueuedPrompts); err != nil {
			return err
		}
	}
	if p.pending.PendingElicitations != nil {
		pending, err := databaseMarshalJSON(nonNilSlice(*p.pending.PendingElicitations))
		if err != nil {
			return err
		}
		if _, err := tx.Exec(`UPDATE materialized_sessions
                 SET pending_elicitations_json = ?2 WHERE session_id = ?1`, sessionID, string(pending)); err != nil {
			return err
		}
	}
	for _, pending := range p.pendingEvents {
		if err := insertAPIEvent(tx, sessionID, pending.recordedAtMs, pending.event); err != nil {
			return err
		}
	}
	for i := range p.pendingTurns {
		turn := &p.pendingTurns[i]
		body, err := serializeTurnOutcome(turn)
		if err != nil {
			return err
		}
		if _, err := tx.Exec("INSERT OR REPLACE INTO session_turn_usage(session_id, command_id, completed_ordinal, turn_start_position, body) VALUES (?1, ?2, ?3, ?4, ?5)",
			sessionID, turn.CommandID, turn.CompletedOrdinal, turn.TurnStartPosition, body); err != nil {
			return err
		}
	}
	if p.pending.ProviderCost != nil {
		cost, err := databaseMarshalJSON(p.pending.ProviderCost)
		if err != nil {
			return err
		}
		if _, err := tx.Exec("INSERT OR REPLACE INTO session_provider_cost(session_id, body) VALUES (?1, ?2)", sessionID, string(cost)); err != nil {
			return err
		}
	}
	for _, result := range p.pending.ConfigResults {
		if _, err := tx.Exec("INSERT OR REPLACE INTO api_config_results(session_id, command_id, error) VALUES (?1, ?2, ?3)",
			sessionID, result.ConfigID, result.Value); err != nil {
			return err
		}
	}
	if p.pending.ActiveTurn != nil {
		activeTurn, err := optionalJSONColumn(*p.pending.ActiveTurn)
		if err != nil {
			return err
		}
		if _, err := tx.Exec("UPDATE materialized_sessions SET active_turn_json = ?2 WHERE session_id = ?1", sessionID, activeTurn); err != nil {
			return err
		}
	}
	if p.pending.ClearTurnOutcome {
		if _, err := tx.Exec("UPDATE materialized_sessions SET last_turn_outcome_json = NULL WHERE session_id = ?1", sessionID); err != nil {
			return err
		}
	}
	if p.pending.LastTurnOutcome != nil {
		body, err := serializeTurnOutcome(p.pending.LastTurnOutcome)
		if err != nil {
			return err
		}
		if _, err := tx.Exec(`UPDATE materialized_sessions
                 SET last_turn_outcome_json = ?2 WHERE session_id = ?1`, sessionID, body); err != nil {
			return err
		}
	}
	_, err := tx.Exec(`UPDATE materialized_sessions
             SET last_activity_at_ms = CASE
                     WHEN ?2 IS NULL THEN last_activity_at_ms
                     WHEN last_activity_at_ms IS NULL OR last_activity_at_ms < ?2 THEN ?2
                     ELSE last_activity_at_ms
                 END,
                 applied_event_ordinal = ?3,
                 applied_event_digest = ?4,
                 input_required_event_frontier = max(input_required_event_frontier, ?3),
                 operator_away_event_frontier = max(operator_away_event_frontier, ?3)
             WHERE session_id = ?1`,
		sessionID, p.pending.LastActivityAtMs, p.appliedOrdinal, p.appliedDigest)
	return err
}

// ApplyProjectionPage applies one relay page in a single transaction. fill
// feeds the page's events through ProjectionPage.Apply; the projection
// changes and the event frontier commit together only when fill succeeds, so
// callers may acknowledge the page's last ordinal to the relay after this
// returns.
func ApplyProjectionPage[T any](sessionID string, fill func(*ProjectionPage) (T, error)) (T, error) {
	return submitDatabaseWrite("apply_projection_page", func(connection *Connection) (T, error) {
		return applyProjectionPageWith(connection, sessionID, fill)
	})
}

// applyProjectionPageTo is test-only in Rust.
func applyProjectionPageTo[T any](path string, sessionID string, fill func(*ProjectionPage) (T, error)) (T, error) {
	connection, err := open(path)
	if err != nil {
		var zero T
		return zero, err
	}
	defer connection.Close()
	return applyProjectionPageWith(connection, sessionID, fill)
}

func applyProjectionPageWith[T any](connection *Connection, sessionID string, fill func(*ProjectionPage) (T, error)) (T, error) {
	var zero T
	transaction, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return zero, err
	}
	// Rolling back on failure leaves the projection at the frontier the relay
	// last saw acknowledged.
	defer transaction.Rollback()
	page, err := loadProjectionPage(transaction, sessionID)
	if err != nil {
		return zero, err
	}
	filled, err := fill(page)
	if err != nil {
		return zero, err
	}
	if err := page.Flush(); err != nil {
		return zero, err
	}
	if err := commitProjectionPage(page); err != nil {
		return zero, err
	}
	return filled, nil
}

func commitProjectionPage(page *ProjectionPage) error {
	questions := false
	for _, pending := range page.pendingEvents {
		switch event := pending.event.(type) {
		case core.ApiEventDataInputRequired:
			questions = questions || event.Request != nil
		case core.ApiEventDataInputResolved:
			questions = true
		}
	}
	if err := page.transaction.Commit(); err != nil {
		return err
	}
	if questions {
		NotifyMailboxOutboxChanged()
	}
	return nil
}

func loadProjectionPage(transaction *sql.Tx, sessionID string) (*ProjectionPage, error) {
	page := &ProjectionPage{sessionID: sessionID, transaction: transaction, pendingTranscript: map[string]*pendingTranscriptMutation{}}
	err := transaction.QueryRow(`SELECT applied_event_ordinal, applied_event_digest, input_required_event_frontier, operator_away_event_frontier
             FROM materialized_sessions WHERE session_id = ?1`, sessionID).
		Scan(&page.appliedOrdinal, &page.appliedDigest, &page.inputRequiredEventFrontier, &page.operatorAwayEventFrontier)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, fmt.Errorf("unknown session %s", sessionID)
	}
	if err != nil {
		return nil, err
	}
	if err := core.ValidateRelayEventFrontier(page.appliedOrdinal, page.appliedDigest, "persisted relay event frontier"); err != nil {
		return nil, err
	}
	fields, err := readMaterializedSessionFields(transaction, sessionID)
	if err != nil {
		return nil, err
	}
	if fields == nil {
		return nil, errors.New("projection session disappeared")
	}
	page.usageConfiguration = fields.configuration
	return page, nil
}

// ApplyProjectionEvent applies exactly one relay event, as a page of one.
func ApplyProjectionEvent(sessionID string, eventOrdinal uint64, previousEventDigest string, eventDigest string, mutation *core.MaterializedSessionMutation) (core.ProjectionApplyOutcome, error) {
	return submitDatabaseWrite("apply_projection_event", func(connection *Connection) (core.ProjectionApplyOutcome, error) {
		return applyProjectionPageWith(connection, sessionID, func(page *ProjectionPage) (core.ProjectionApplyOutcome, error) {
			return page.Apply(eventOrdinal, previousEventDigest, eventDigest, mutation)
		})
	})
}

// applyProjectionEventTo is test-only in Rust.
func applyProjectionEventTo(path string, sessionID string, eventOrdinal uint64, previousEventDigest string, eventDigest string, mutation *core.MaterializedSessionMutation) (core.ProjectionApplyOutcome, error) {
	return applyProjectionPageTo(path, sessionID, func(page *ProjectionPage) (core.ProjectionApplyOutcome, error) {
		return page.Apply(eventOrdinal, previousEventDigest, eventDigest, mutation)
	})
}

// AdvanceViewedThroughEventOrdinal advances the persisted detach/read receipt
// monotonically. A receipt cannot acknowledge an event the controller
// projection has not durably applied.
func AdvanceViewedThroughEventOrdinal(sessionID string, through uint64) (uint64, error) {
	return submitDatabaseWrite("advance_viewed_through_event_ordinal", func(*Connection) (uint64, error) {
		return advanceViewedThroughEventOrdinalTo(DatabasePath(), sessionID, through)
	})
}

func advanceViewedThroughEventOrdinalTo(path string, sessionID string, through uint64) (uint64, error) {
	connection, err := open(path)
	if err != nil {
		return 0, err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return 0, err
	}
	defer tx.Rollback()
	var applied uint64
	err = tx.QueryRow("SELECT applied_event_ordinal FROM materialized_sessions WHERE session_id = ?1", sessionID).Scan(&applied)
	if errors.Is(err, sql.ErrNoRows) {
		return 0, fmt.Errorf("unknown session %s", sessionID)
	}
	if err != nil {
		return 0, err
	}
	if through > applied {
		return 0, fmt.Errorf("cannot acknowledge event ordinal %d for session %s; projection is at %d", through, sessionID, applied)
	}
	if _, err := tx.Exec(`UPDATE sessions
         SET viewed_through_event_ordinal = max(viewed_through_event_ordinal, ?2)
         WHERE session_id = ?1`, sessionID, through); err != nil {
		return 0, err
	}
	var receipt uint64
	if err := tx.QueryRow("SELECT viewed_through_event_ordinal FROM sessions WHERE session_id = ?1", sessionID).Scan(&receipt); err != nil {
		return 0, err
	}
	if err := tx.Commit(); err != nil {
		return 0, err
	}
	return receipt, nil
}

// decodeTranscriptBody decodes a stored transcript body, merging runs of
// streamed text chunks.
//
// Rows written before streamed text was merged on the way in hold one chunk
// per token, which costs far more memory decoded than the text it carries.
// Collapsing them here shrinks long sessions without rewriting stored JSON.
func decodeTranscriptBody(bodyJSON string, sessionID string) (core.TranscriptBody, error) {
	body, err := core.DecodeTranscriptBody([]byte(bodyJSON))
	if err != nil {
		return nil, fmt.Errorf("parse materialized transcript body for session %s: %w", sessionID, err)
	}
	switch decoded := body.(type) {
	case core.TranscriptBodyAgent:
		core.CoalesceContentChunks(&decoded.Chunks)
		return decoded, nil
	case core.TranscriptBodyThought:
		core.CoalesceContentChunks(&decoded.Chunks)
		return decoded, nil
	}
	return body, nil
}

// LoadContinuationEvidence reads complete authorization and recent replies
// in one consistent snapshot. Runs on the continuation service's blocking
// pool, never a render/event loop.
func LoadContinuationEvidence(sessionID string, ordinal uint64, digest string) (core.ContinuationEvidence, error) {
	return loadContinuationEvidenceFrom(DatabasePath(), sessionID, ordinal, digest)
}

func loadContinuationEvidenceFrom(path string, sessionID string, ordinal uint64, digest string) (core.ContinuationEvidence, error) {
	connection, done, err := openReadSnapshot(path)
	if err != nil {
		return core.ContinuationEvidence{}, err
	}
	defer done()
	fields, err := readMaterializedSessionFields(connection, sessionID)
	if err != nil {
		return core.ContinuationEvidence{}, err
	}
	if fields == nil {
		return core.ContinuationEvidence{}, errors.New("continuation projection is missing")
	}
	if fields.appliedEventOrdinal != ordinal || fields.appliedEventDigest != digest {
		return core.ContinuationEvidence{}, errors.New("continuation projection changed")
	}
	rows, err := connection.Query(`SELECT stable_id, position, latest_content_event_ordinal, created_at_ms,
                last_changed_at_ms, body_json
         FROM materialized_transcript_items WHERE session_id = ?1
         AND (json_extract(body_json, '$.kind') IN ('user', 'agent')
              OR stable_id LIKE 'context-cleared:%')
         ORDER BY position DESC, stable_id DESC`, sessionID)
	if err != nil {
		return core.ContinuationEvidence{}, err
	}
	defer rows.Close()
	items := iter.Seq2[*core.TranscriptItem, error](func(yield func(*core.TranscriptItem, error) bool) {
		for rows.Next() {
			item := new(core.TranscriptItem)
			var bodyJSON string
			if err := rows.Scan(&item.StableID, &item.Position, &item.LatestContentEventOrdinal,
				&item.CreatedAtMs, &item.LastChangedAtMs, &bodyJSON); err != nil {
				yield(nil, err)
				return
			}
			// Refuse exceptionally large source messages rather than clip
			// consent.
			if len(bodyJSON) > 1024*1024 {
				yield(nil, errors.New("continuation source message exceeds budget"))
				return
			}
			body, err := decodeTranscriptBody(bodyJSON, sessionID)
			if err != nil {
				yield(nil, err)
				return
			}
			item.Body = body
			if !yield(item, nil) {
				return
			}
		}
		if err := rows.Err(); err != nil {
			yield(nil, err)
		}
	})
	return transcript.EvidenceFromItems(items)
}
