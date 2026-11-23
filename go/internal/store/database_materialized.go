// Port of mj-store/src/database/materialized.rs.

package store

import (
	"database/sql"
	"fmt"
	"log/slog"

	"github.com/BrokkAi/mjolnir/go/internal/core"
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

func loadMaterializedActivityFrom(path string) (map[string]MaterializedActivity, error) {
	panic("unported: mj-store/src/database/materialized.rs load_materialized_activity_from")
}

func loadMaterializedSessionSummaryFrom(path string, sessionID string) (*core.MaterializedSessionSummary, error) {
	panic("unported: mj-store/src/database/materialized.rs load_materialized_session_summary_from")
}

// readMaterializedUnreadOrdinals reads the summary's ordinal facts without
// deserializing transcript bodies: the agent messages' latest content
// ordinals, then the interruption event ordinals.
func readMaterializedUnreadOrdinals(connection Queryer, sessionID string) ([]uint64, []uint64, error) {
	panic("unported: mj-store/src/database/materialized.rs read_materialized_unread_ordinals")
}

func readLatestUnreadEventOrdinal(connection Queryer, sessionID string) (*uint64, error) {
	panic("unported: mj-store/src/database/materialized.rs read_latest_unread_event_ordinal")
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
	panic("unported: mj-store/src/database/materialized.rs materialized_user_message")
}

// lastMaterializedContextBoundary is where the newest /clear boundary sits,
// if the session has one.
func lastMaterializedContextBoundary(connection Queryer, sessionID string) (*uint64, error) {
	panic("unported: mj-store/src/database/materialized.rs last_materialized_context_boundary")
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
	panic("unported: mj-store/src/database/materialized.rs last_materialized_turn_start")
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
	panic("unported: mj-store/src/database/materialized.rs last_materialized_agent_message_in")
}

func lastMaterializedAgentItemIn(connection Queryer, sessionID string, afterPosition uint64, throughPosition *uint64, throughOrdinal *uint64) (*core.TranscriptItem, error) {
	panic("unported: mj-store/src/database/materialized.rs last_materialized_agent_item_in")
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
	panic("unported: mj-store/src/database/materialized.rs load_prompt_acceptance")
}

// LoadMaterializedAgentMessageThrough is the newest agent message whose
// content was complete at event ordinal through: what a ReadThrough
// notification dismissal names. One query, so the bound and the read are one
// decision and a newer reply cannot answer for an older card.
func LoadMaterializedAgentMessageThrough(sessionID string, through uint64) (string, bool, error) {
	panic("unported: mj-store/src/database/materialized.rs load_materialized_agent_message_through")
}

func LoadMaterializedAgentItemThrough(sessionID string, through uint64) (*core.TranscriptItem, error) {
	panic("unported: mj-store/src/database/materialized.rs load_materialized_agent_item_through")
}

// MaterializedAgentItemThrough is the newest agent message of the turn that
// completed at through, bounded to that turn: a turn that left no text has
// none, never an earlier turn's. Every reader of a notification entry without
// a message id asks this.
func MaterializedAgentItemThrough(connection Queryer, sessionID string, through uint64) (*core.TranscriptItem, error) {
	panic("unported: mj-store/src/database/materialized.rs materialized_agent_item_through")
}

// LoadMaterializedTranscriptItem: whether the stored projection holds a
// transcript item with this stable id. A prompt's item appears when the
// prompt starts, so this tells a started prompt from one that was only
// queued. Exact primary-key lookup; the runtime tail need not contain this
// message.
func LoadMaterializedTranscriptItem(sessionID string, stableID string) (*core.TranscriptItem, error) {
	panic("unported: mj-store/src/database/materialized.rs load_materialized_transcript_item")
}

func MaterializedTranscriptItemExists(sessionID string, stableID string) (bool, error) {
	panic("unported: mj-store/src/database/materialized.rs materialized_transcript_item_exists")
}

func loadMaterializedTurnOutcomeFrom(path string, sessionID string) (*MaterializedTurnState, error) {
	panic("unported: mj-store/src/database/materialized.rs load_materialized_turn_outcome_from")
}

// readMaterializedTurnState: turn observation does not need configuration,
// forms, or transcript bodies.
func readMaterializedTurnState(connection Queryer, sessionID string) (*MaterializedTurnState, error) {
	panic("unported: mj-store/src/database/materialized.rs read_materialized_turn_state")
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
	panic("unported: mj-store/src/database/materialized.rs load_child_answer_messages")
}

func loadMaterializedFinishedTurnMessageFrom(path string, sessionID string) (string, bool, error) {
	panic("unported: mj-store/src/database/materialized.rs load_materialized_finished_turn_message_from")
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
	panic("unported: mj-store/src/database/materialized.rs load_materialized_turn_summary_from")
}

func LoadMaterializedTranscriptFiltered(sessionID string, afterSeq uint64, limit int, roles []core.TranscriptRole, finishedOnly bool) (*core.TranscriptPage, error) {
	return loadMaterializedTranscriptFilteredFrom(DatabasePath(), sessionID, afterSeq, limit, roles, finishedOnly)
}

// hydrateTerminalOutputs joins parked output for presentation without
// changing stored bodies or page boundaries. An item it changes is replaced
// by a changed copy (Rust's Arc::make_mut), so shared items stay unchanged.
func hydrateTerminalOutputs(connection Queryer, sessionID string, items []*core.TranscriptItem) error {
	panic("unported: mj-store/src/database/materialized.rs hydrate_terminal_outputs")
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
	panic("unported: mj-store/src/database/materialized.rs load_materialized_transcript_filtered_from")
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
	panic("unported: mj-store/src/database/materialized.rs spill_and_trim_settled_tools_through_from")
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
	panic("unported: mj-store/src/database/materialized.rs compact_materialized_transcript_in")
}

// compactMaterializedTranscriptAllIn is test-only in Rust.
func compactMaterializedTranscriptAllIn(path string, sessionID string, eventFrontier uint64, batchSize int) (core.TranscriptRetention, error) {
	panic("unported: mj-store/src/database/materialized.rs compact_materialized_transcript_all_in")
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
	panic("unported: mj-store/src/database/materialized.rs submit_tool_detail_batch")
}

const spillableTools = `settled = 1 AND detail_offset IS NULL
    AND item_kind IN ('tool', 'terminal_output')`

// NextToolDetailBackfillSession finds the next session with work at its
// durable frontier, returning its id and applied event ordinal. A scan
// timeout or shutdown needs no persisted cursor: committed rows are excluded
// on retry.
func NextToolDetailBackfillSession(after string) (string, uint64, bool, error) {
	panic("unported: mj-store/src/database/materialized.rs next_tool_detail_backfill_session")
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
	panic("unported: mj-store/src/database/materialized.rs spill_tool_detail_batch")
}

// LoadToolCallDetail reads the current detail by stable identity without
// paging the transcript. A nil result is no such item.
func LoadToolCallDetail(sessionID string, stableID string) (core.ToolCallDetailResult, error) {
	return LoadToolCallDetailFrom(DatabasePath(), sessionID, stableID)
}

func LoadToolCallDetailFrom(path string, sessionID string, stableID string) (core.ToolCallDetailResult, error) {
	panic("unported: mj-store/src/database/materialized.rs load_tool_call_detail_from")
}

// hydrateFullToolTerminalOutputs joins authoritative standalone terminal rows
// referenced by a full tool detail, resolving their .mjtd cold frames when
// spilled independently. A non-nil reason is Rust's inner Err: the detail is
// unavailable for that reason.
func hydrateFullToolTerminalOutputs(connection Queryer, dataDir string, sessionID string, stableID string, body *core.TranscriptBody) (*string, error) {
	panic("unported: mj-store/src/database/materialized.rs hydrate_full_tool_terminal_outputs")
}

// ReadStoredToolDetail reads the spill associated with a specific projected
// revision. A newer inline update or an already-trimmed legacy row has no
// matching detail (nil).
func ReadStoredToolDetail(sessionID string, item *core.TranscriptItem) (core.TranscriptBody, error) {
	return ReadStoredToolDetailFrom(DatabasePath(), sessionID, item)
}

func ReadStoredToolDetailFrom(path string, sessionID string, item *core.TranscriptItem) (core.TranscriptBody, error) {
	panic("unported: mj-store/src/database/materialized.rs read_stored_tool_detail_from")
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

func loadMaterializedActorProjectionFrom(path string, sessionID string) (*core.MaterializedSession, *core.ProjectionWindow, error) {
	panic("unported: mj-store/src/database/materialized.rs load_materialized_actor_projection_from")
}

func LoadTranscriptHistory(sessionID string, before *core.TranscriptCursor, limit int) (*core.TranscriptHistoryPage, error) {
	return loadTranscriptHistoryFrom(DatabasePath(), sessionID, before, limit)
}

func loadTranscriptHistoryFrom(path string, sessionID string, before *core.TranscriptCursor, limit int) (*core.TranscriptHistoryPage, error) {
	panic("unported: mj-store/src/database/materialized.rs load_transcript_history_from")
}

func readTranscriptRange(connection Queryer, sessionID string, start uint64, before *core.TranscriptCursor, limit *int) ([]*core.TranscriptItem, error) {
	panic("unported: mj-store/src/database/materialized.rs read_transcript_range")
}

// LoadProjectionReferences rehydrates referenced historical tools and
// possible raw-result terminal matches before deriving a delta. A window is a
// cache, never evidence that a durable tool does not exist.
func LoadProjectionReferences(projection *core.MaterializedSession, events []core.RelayEvent) ([]*core.TranscriptItem, error) {
	return loadProjectionReferencesFrom(DatabasePath(), projection, events)
}

func loadProjectionReferencesFrom(path string, projection *core.MaterializedSession, events []core.RelayEvent) ([]*core.TranscriptItem, error) {
	panic("unported: mj-store/src/database/materialized.rs load_projection_references_from")
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
	panic("unported: mj-store/src/database/materialized.rs load_materialized_projection_tail_from")
}

// MaterializedEventFrontier reads only the projection's event frontier
// (ordinal and digest). Deciding whether a stored projection already matches
// an archive costs one row this way, instead of deserializing every
// transcript item to compare two integers.
func MaterializedEventFrontier(sessionID string) (uint64, string, bool, error) {
	return materializedEventFrontierFrom(DatabasePath(), sessionID)
}

func materializedEventFrontierFrom(path string, sessionID string) (uint64, string, bool, error) {
	panic("unported: mj-store/src/database/materialized.rs materialized_event_frontier_from")
}

// ReplaceMaterializedQueuedPrompts replaces a session's durable prompt queue
// without touching its transcript or event frontier. Resume uses this when it
// keeps the stored projection but still has to drop the queue the archive
// carried.
func ReplaceMaterializedQueuedPrompts(sessionID string, queuedPrompts []core.MaterializedQueuedPrompt) error {
	panic("unported: mj-store/src/database/materialized.rs replace_materialized_queued_prompts")
}

func replaceMaterializedQueuedPromptsIn(path string, sessionID string, queuedPrompts []core.MaterializedQueuedPrompt) error {
	panic("unported: mj-store/src/database/materialized.rs replace_materialized_queued_prompts_in")
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
	panic("unported: mj-store/src/database/materialized.rs load_transcribed_session_activity_from")
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

func readAllMaterializedQueuedPrompts(path string, invalidQueue func(sessionID string, err error) error) (map[string][]core.MaterializedQueuedPrompt, error) {
	panic("unported: mj-store/src/database/materialized.rs read_all_materialized_queued_prompts")
}

func LoadMaterializedSessionFrom(path string, sessionID string) (*core.MaterializedSession, error) {
	panic("unported: mj-store/src/database/materialized.rs load_materialized_session_from")
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
	panic("unported: mj-store/src/database/materialized.rs load_materialized_recent_turns_from")
}

func loadMaterializedSessionBounded(connection *sql.Tx, sessionID string, transcriptLimit *int) (*core.MaterializedSession, error) {
	panic("unported: mj-store/src/database/materialized.rs load_materialized_session_bounded")
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
	panic("unported: mj-store/src/database/materialized.rs read_materialized_session_fields")
}

// readMaterializedTranscript reads a session's transcript, oldest first. A
// non-nil limit reads only that many items from the end, walking the
// materialized_transcript_position index backwards so the read costs the rows
// it returns.
func readMaterializedTranscript(connection Queryer, sessionID string, limit *int) ([]*core.TranscriptItem, error) {
	panic("unported: mj-store/src/database/materialized.rs read_materialized_transcript")
}

func readMaterializedQueuedPrompts(connection Queryer, sessionID string) ([]core.MaterializedQueuedPrompt, error) {
	panic("unported: mj-store/src/database/materialized.rs read_materialized_queued_prompts")
}

// SaveMaterializedSession replaces a complete projection, primarily when
// seeding a restored checkpoint. Operational SessionRecord metadata and read
// receipts are not modified.
func SaveMaterializedSession(materialized *core.MaterializedSession) error {
	panic("unported: mj-store/src/database/materialized.rs save_materialized_session")
}

// RepairMaterializedSessionIfCurrent repairs only the projection whose exact
// cursor the caller observed. A concurrent projector owns its committed
// history, even when repair rewinds.
func RepairMaterializedSessionIfCurrent(materialized *core.MaterializedSession, expectedOrdinal uint64, expectedDigest string) (bool, error) {
	panic("unported: mj-store/src/database/materialized.rs repair_materialized_session_if_current")
}

func repairMaterializedSessionIfCurrentTo(path string, materialized *core.MaterializedSession, expectedOrdinal uint64, expectedDigest string) (bool, error) {
	panic("unported: mj-store/src/database/materialized.rs repair_materialized_session_if_current_to")
}

func SaveMaterializedSessionTo(path string, materialized *core.MaterializedSession) error {
	panic("unported: mj-store/src/database/materialized.rs save_materialized_session_to")
}

// InstallLiveSessionBaseline installs one current-state cut only before any
// attachment establishes history. The notice, live owner state, and both
// native replay phases commit together.
func InstallLiveSessionBaseline(materialized *core.MaterializedSession, baseline core.LiveSessionBaseline) (bool, error) {
	panic("unported: mj-store/src/database/materialized.rs install_live_session_baseline")
}

func installLiveSessionBaselineTo(path string, materialized *core.MaterializedSession, baseline *core.LiveSessionBaseline) (bool, error) {
	panic("unported: mj-store/src/database/materialized.rs install_live_session_baseline_to")
}

func installLiveSessionBaselineWith(tx *sql.Tx, materialized *core.MaterializedSession, baseline *core.LiveSessionBaseline) (bool, error) {
	panic("unported: mj-store/src/database/materialized.rs install_live_session_baseline_with")
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
	panic("unported: mj-store/src/database/materialized.rs ProjectionPage::apply")
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
	panic("unported: mj-store/src/database/materialized.rs ProjectionPage::flush")
}

// ApplyProjectionPage applies one relay page in a single transaction. fill
// feeds the page's events through ProjectionPage.Apply; the projection
// changes and the event frontier commit together only when fill succeeds, so
// callers may acknowledge the page's last ordinal to the relay after this
// returns.
func ApplyProjectionPage[T any](sessionID string, fill func(*ProjectionPage) (T, error)) (T, error) {
	panic("unported: mj-store/src/database/materialized.rs apply_projection_page")
}

// applyProjectionPageTo is test-only in Rust.
func applyProjectionPageTo[T any](path string, sessionID string, fill func(*ProjectionPage) (T, error)) (T, error) {
	panic("unported: mj-store/src/database/materialized.rs apply_projection_page_to")
}

func applyProjectionPageWith[T any](connection *Connection, sessionID string, fill func(*ProjectionPage) (T, error)) (T, error) {
	panic("unported: mj-store/src/database/materialized.rs apply_projection_page_with")
}

func commitProjectionPage(page *ProjectionPage) error {
	panic("unported: mj-store/src/database/materialized.rs commit_projection_page")
}

func loadProjectionPage(transaction *sql.Tx, sessionID string) (*ProjectionPage, error) {
	panic("unported: mj-store/src/database/materialized.rs load_projection_page")
}

// ApplyProjectionEvent applies exactly one relay event, as a page of one.
func ApplyProjectionEvent(sessionID string, eventOrdinal uint64, previousEventDigest string, eventDigest string, mutation *core.MaterializedSessionMutation) (core.ProjectionApplyOutcome, error) {
	panic("unported: mj-store/src/database/materialized.rs apply_projection_event")
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
	panic("unported: mj-store/src/database/materialized.rs advance_viewed_through_event_ordinal")
}

func advanceViewedThroughEventOrdinalTo(path string, sessionID string, through uint64) (uint64, error) {
	panic("unported: mj-store/src/database/materialized.rs advance_viewed_through_event_ordinal_to")
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
	panic("unported: mj-store/src/database/materialized.rs load_continuation_evidence_from")
}
