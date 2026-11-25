//! Bounded, read-only history operations for target-side MCP clients.
use anyhow::{Context, Result, anyhow, bail, ensure};
use mj_core::history::{HistoryQuery, HistoryRequest, HistoryResult};
use serde_json::{Value, json};
use sessionwiki::index;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::indexed_messages;
use super::top_level::QueryVisibility;

/// How long the blocking thread may run past the query's own deadline before
/// the waiting task gives up on it. SQLite stops itself at the deadline; this
/// only covers work outside SQLite (reading native transcripts, say).
const BACKSTOP: Duration = Duration::from_secs(5);
const TIMED_OUT: &str = "history query timed out; retry or narrow the query";

/// Run one history request. `caller_session_id` is the Mjolnir session that
/// owns the worker asking; the controller supplies it, never the request.
pub async fn execute(request: HistoryRequest, caller_session_id: Option<String>) -> HistoryResult {
    static PERMITS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let permits = PERMITS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)))
        .clone();
    execute_with(&permits, request, caller_session_id, query).await
}

type Work = fn(&HistoryRequest, &QueryBudget, Option<&str>) -> Result<Value>;

async fn execute_with(
    permits: &Arc<tokio::sync::Semaphore>,
    request: HistoryRequest,
    caller_session_id: Option<String>,
    work: Work,
) -> HistoryResult {
    let id = request.request_id.clone();
    // A full-text query waits for a tokenizer migration before it takes one of
    // the shared permits, so waiting searches never starve the other tools.
    if reads_full_text(&request)
        && let Err(error) = wait_for_tokenizer_migration(mj_core::history::QUERY_TIMEOUT).await
    {
        return HistoryResult::failed(id, format!("{error:#}"));
    }
    let outcome = run_bounded(permits, mj_core::history::QUERY_TIMEOUT, move |budget| {
        work(&request, &budget, caller_session_id.as_deref())
    })
    .await;
    match outcome {
        Ok(value) => match serde_json::to_vec(&value) {
            Ok(bytes) if bytes.len() <= mj_core::history::MAX_RESPONSE_BYTES => HistoryResult {
                request_id: id,
                value,
                is_error: false,
            },
            _ => HistoryResult::failed(
                id,
                "history response exceeds its byte budget; narrow the query",
            ),
        },
        Err(error) => HistoryResult::failed(id, format!("{error:#}")),
    }
}

/// What stops a query: its deadline, or the caller going away. SQLite polls
/// both between VM steps, so the thread frees its permit when either fires
/// instead of finishing work nobody will read.
struct QueryBudget {
    cancel: Arc<AtomicBool>,
    deadline: Instant,
}

impl QueryBudget {
    /// Make `connection` abort its running statement once the budget is spent.
    fn install(&self, connection: &mut rusqlite::Connection) -> Result<()> {
        let cancel = self.cancel.clone();
        let deadline = self.deadline;
        connection
            .progress_handler(
                1000,
                Some(move || cancel.load(Ordering::Relaxed) || Instant::now() >= deadline),
            )
            .context("install the history query deadline")
    }
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Run `work` on a blocking thread under one of `permits`. The deadline starts
/// once the permit is held, so time spent queueing never shortens the query.
/// The thread owns the permit and returns it the moment `work` returns, which
/// SQLite forces at the deadline or when this future is dropped.
async fn run_bounded<T: Send + 'static>(
    permits: &Arc<tokio::sync::Semaphore>,
    timeout: Duration,
    work: impl FnOnce(QueryBudget) -> Result<T> + Send + 'static,
) -> Result<T> {
    let permit = permits
        .clone()
        .acquire_owned()
        .await
        .context("history service stopped")?;
    let cancel = Arc::new(AtomicBool::new(false));
    let _cancel_when_dropped = CancelOnDrop(cancel.clone());
    let budget = QueryBudget {
        cancel,
        deadline: Instant::now() + timeout,
    };
    let task = mj_core::runtime::spawn_blocking(move || {
        let _permit = permit;
        work(budget)
    });
    match tokio::time::timeout(timeout + BACKSTOP, task).await {
        Ok(joined) => joined
            .context("history query task failed")?
            .map_err(|error| {
                if was_interrupted(&error) {
                    anyhow!(TIMED_OUT)
                } else {
                    error
                }
            }),
        Err(_) => bail!(TIMED_OUT),
    }
}

fn was_interrupted(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(failure, _))
                if failure.code == rusqlite::ErrorCode::OperationInterrupted
        )
    })
}

/// How a session id from a client resolves. Only `Resolved` has a transcript to
/// read; every other arm says what the client can do next.
#[allow(
    clippy::large_enum_variant,
    reason = "a lookup is built and consumed once per request"
)]
pub(super) enum SessionLookup {
    Resolved(index::SessionRow),
    /// A sub-agent: its transcript is deliberately not indexed.
    ChildNotIndexed {
        input: String,
        child_session_id: String,
        parent_session_id: String,
        report_dir: Option<String>,
    },
    /// A known top-level session the index has not caught up with.
    UnindexedLive {
        input: String,
        session_id: String,
    },
    Ambiguous {
        input: String,
        candidates: Vec<AmbiguousSession>,
    },
    NotFound {
        input: String,
    },
}

pub(super) struct AmbiguousSession {
    pub session_id: String,
    pub title: String,
    pub started: Option<String>,
    pub ended: Option<String>,
}

impl AmbiguousSession {
    fn of(row: &index::SessionRow) -> Self {
        Self {
            session_id: row.session_id.clone(),
            title: row.title.clone(),
            started: row.started.clone(),
            ended: row.last_active.clone(),
        }
    }
}

fn child_report_dir(child_session_id: &str) -> Option<String> {
    match crate::database::load_subagent_report(child_session_id) {
        Ok(report) => report.report_dir,
        Err(error) => {
            tracing::warn!(child_session_id, error = %format!("{error:#}"),
                "could not read a sub-agent's report directory");
            None
        }
    }
}

fn child_not_indexed(input: &str, pointer: &super::top_level::ChildPointer) -> SessionLookup {
    SessionLookup::ChildNotIndexed {
        input: input.to_owned(),
        child_session_id: pointer.child_session_id.clone(),
        parent_session_id: pointer.parent_session_id.clone(),
        report_dir: child_report_dir(&pointer.child_session_id),
    }
}

/// `sessionwiki::index::resolve` does not select `files.ended`, leaving
/// `SessionRow.last_active` as `None`. Fill it by primary key when absent so
/// session headers and ambiguous-prefix diagnostics show real end times.
fn fill_ended(connection: &rusqlite::Connection, row: &mut index::SessionRow) -> Result<()> {
    use rusqlite::OptionalExtension;
    if row.last_active.is_none() {
        row.last_active = connection
            .query_row(
                "SELECT ended FROM files WHERE path = ?1",
                [&row.path],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .context("read session end time")?
            .flatten();
    }
    Ok(())
}

/// Resolve `id` (a Mjolnir id or prefix, or a native UUID) to one visible
/// indexed session. A sub-agent never resolves, and a child id never hides a
/// top-level session: children are consulted only when no visible row matched.
pub(super) fn resolve_lookup(
    connection: &rusqlite::Connection,
    id: &str,
    visibility: &mut QueryVisibility<'_>,
) -> Result<SessionLookup> {
    ensure!(!id.is_empty() && id.len() <= 256, "invalid session id");
    let ownership = visibility.ownership();
    if let Some(pointer) = ownership.exact_child_pointer(id) {
        return Ok(child_not_indexed(id, pointer));
    }
    let mut candidates = index::resolve(connection, id)?;
    for session_id in ownership.native_session_ids(id) {
        candidates.extend(
            index::resolve(connection, session_id)?
                .into_iter()
                .filter(|row| row.session_id == session_id),
        );
    }
    let mut seen = std::collections::HashSet::new();
    let mut visible: Vec<_> = candidates
        .into_iter()
        .filter(|row| !visibility.is_child_lenient(row) && seen.insert(row.session_id.clone()))
        .collect();
    if let Some(position) = visible.iter().position(|row| row.session_id == id) {
        let mut row = visible.remove(position);
        fill_ended(connection, &mut row)?;
        return Ok(SessionLookup::Resolved(row));
    }
    // An exact native UUID outranks other rows that merely share its prefix.
    let wanted = Some(id.to_ascii_lowercase());
    if visible.iter().any(|row| {
        index::native_id_of(&row.path) == wanted
            || ownership.exact_native_session_id(id) == Some(row.session_id.as_str())
    }) {
        visible.retain(|row| {
            index::native_id_of(&row.path) == wanted
                || ownership.exact_native_session_id(id) == Some(row.session_id.as_str())
        });
    }
    if visible.len() > 1
        && visible.iter().all(|row| {
            index::native_id_of(&row.path) == wanted
                || ownership.exact_native_session_id(id) == Some(row.session_id.as_str())
        })
    {
        let preferred: Vec<_> = visible
            .iter()
            .enumerate()
            .filter(|(_, row)| row.tool == super::TOOL)
            .map(|(i, _)| i)
            .collect();
        if preferred.len() == 1 {
            let mut row = visible.remove(preferred[0]);
            fill_ended(connection, &mut row)?;
            return Ok(SessionLookup::Resolved(row));
        }
    }
    match visible.len() {
        0 => {}
        1 => {
            let mut row = visible.remove(0);
            fill_ended(connection, &mut row)?;
            return Ok(SessionLookup::Resolved(row));
        }
        _ => {
            for row in &mut visible {
                fill_ended(connection, row)?;
            }
            return Ok(SessionLookup::Ambiguous {
                input: id.to_owned(),
                candidates: visible.iter().map(AmbiguousSession::of).collect(),
            });
        }
    }
    if let Some(pointer) = ownership.child_pointer(id) {
        return Ok(child_not_indexed(id, pointer));
    }
    Ok(
        match ownership
            .resolve_native_session_id(id)
            .or_else(|| ownership.known_top_level(id))
        {
            Some(session_id) => SessionLookup::UnindexedLive {
                input: id.to_owned(),
                session_id: session_id.to_owned(),
            },
            None => SessionLookup::NotFound {
                input: id.to_owned(),
            },
        },
    )
}

/// The one visible session `id` names, or an error saying what to do instead.
fn resolve(
    connection: &rusqlite::Connection,
    id: &str,
    visibility: &mut QueryVisibility<'_>,
) -> Result<index::SessionRow> {
    match resolve_lookup(connection, id, visibility)? {
        SessionLookup::Resolved(row) => Ok(row),
        SessionLookup::ChildNotIndexed {
            input,
            child_session_id,
            parent_session_id,
            report_dir,
        } => {
            let report = match report_dir {
                Some(dir) => format!("its report details are in {dir}"),
                None => "no report directory is recorded for it".to_owned(),
            };
            bail!(
                "session {input:?} is sub-agent {child_session_id}, whose transcript is not indexed; \
                 read its parent session {parent_session_id} instead, and {report}"
            )
        }
        SessionLookup::UnindexedLive { input, session_id } if input == session_id => {
            bail!("session {input:?} exists but is not in the index")
        }
        SessionLookup::UnindexedLive { input, session_id } => {
            bail!("session {input:?} (Mjolnir session {session_id}) exists but is not in the index")
        }
        SessionLookup::Ambiguous { input, candidates } => {
            let listed: Vec<String> = candidates
                .iter()
                .map(|candidate| {
                    format!(
                        "{} {:?} ({} to {})",
                        candidate.session_id,
                        candidate.title,
                        candidate.started.as_deref().unwrap_or("?"),
                        candidate.ended.as_deref().unwrap_or("?")
                    )
                })
                .collect();
            bail!(
                "session prefix {input:?} is ambiguous; use one of these ids: {}",
                listed.join("; ")
            )
        }
        SessionLookup::NotFound { input } => bail!(
            "session {input:?} was not found in the index (sub-agent and reviewer transcripts are never indexed; \
             if this is a child id, check its parent session or report directory)"
        ),
    }
}

/// Replace the `"self"` alias with the calling session's id. `None` means the
/// alias was used with no calling session to stand for.
pub(super) fn substitute_self<'a>(
    input: &'a str,
    caller_session_id: Option<&'a str>,
) -> Option<&'a str> {
    if mj_core::history::is_self_alias(input) {
        caller_session_id
    } else {
        Some(input)
    }
}

fn limit(value: usize) -> usize {
    value.clamp(1, 100)
}
fn budget(value: usize) -> usize {
    value.clamp(1, 64_000)
}

fn query(
    request: &HistoryRequest,
    budget: &QueryBudget,
    caller_session_id: Option<&str>,
) -> Result<Value> {
    request.query.validate()?;
    let mut connection = super::readable_index()?.with_context(super::unreadable_reason)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    budget.install(&mut connection)?;
    let ownership = super::top_level::current_snapshot()?;
    let cache = crate::import::NativeScanCache::shared();
    let value = query_in_for(&connection, request, &ownership, &cache, caller_session_id)?;
    // The constant source and coverage prose lives in the `search_history` and
    // `read_session` tool descriptions, so their responses omit it.
    if matches!(
        request.query,
        HistoryQuery::SearchHistory { .. } | HistoryQuery::ReadSession { .. }
    ) {
        return Ok(json!({"data": value, "index_state": super::index_state()}));
    }
    Ok(json!({
        "data": value,
        "index_state": super::index_state(),
        "source": "indexed_snapshot",
        "coverage": "Indexed history can lag live sessions. Retained transcripts may omit original timestamps, tool output, or older messages.",
    }))
}

/// Only the full-text tools read the FTS table, so only they wait for a
/// tokenizer migration. The rest work on a trigram index at once.
fn reads_full_text(request: &HistoryRequest) -> bool {
    matches!(
        request.query,
        HistoryQuery::SearchHistory { .. } | HistoryQuery::SearchSessions { .. }
    )
}

/// Wait until the index leaves the trigram tokenizer, polling with a short
/// blocking probe so no thread or transaction is held across the sleep.
pub(super) async fn wait_for_tokenizer_migration(timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let migrating = mj_core::runtime::spawn_blocking(super::tokenizer_is_migrating)
            .await
            .context("tokenizer probe task failed")??;
        if !migrating {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("session history index is migrating to porter unicode61; retry shortly");
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(250))).await;
    }
}

#[cfg(test)]
fn query_in(connection: &rusqlite::Connection, request: &HistoryRequest) -> Result<Value> {
    query_in_with(
        connection,
        request,
        &super::top_level::Snapshot::default(),
        &crate::import::NativeScanCache::new(),
    )
}

#[cfg(test)]
pub(super) fn query_in_with(
    connection: &rusqlite::Connection,
    request: &HistoryRequest,
    ownership: &super::top_level::Snapshot,
    cache: &crate::import::NativeScanCache,
) -> Result<Value> {
    query_in_for(connection, request, ownership, cache, None)
}

fn query_in_for(
    connection: &rusqlite::Connection,
    request: &HistoryRequest,
    ownership: &super::top_level::Snapshot,
    cache: &crate::import::NativeScanCache,
    caller_session_id: Option<&str>,
) -> Result<Value> {
    let mut visibility = QueryVisibility::new(ownership, cache);
    let mut value = run_query(connection, request, &mut visibility, caller_session_id)?;
    let notices = visibility.take_notices();
    if !notices.is_empty() {
        value["notices"] = json!(notices);
    }
    Ok(value)
}

fn run_query(
    connection: &rusqlite::Connection,
    request: &HistoryRequest,
    visibility: &mut QueryVisibility<'_>,
    caller_session_id: Option<&str>,
) -> Result<Value> {
    use HistoryQuery::*;
    match &request.query {
        SearchHistory {
            query,
            session_ids,
            session_id,
            role,
            order,
            limit,
            context_messages,
            max_chars,
            cursor,
        } => super::search_history::execute(
            connection,
            &super::search_history::SearchHistoryParams {
                query: query.clone(),
                session_ids: mj_core::history::merged_search_history_session_ids(
                    session_ids.as_deref(),
                    session_id.as_deref(),
                ),
                role: role.clone(),
                order: order.clone(),
                limit: *limit,
                context_messages: *context_messages,
                max_chars: *max_chars,
                cursor: cursor.clone(),
            },
            caller_session_id,
            visibility,
        ),
        SearchSessions {
            query,
            limit: count,
        } => {
            let rows = super::query_rows_from(
                connection,
                query,
                limit(*count),
                &Default::default(),
                true,
                visibility,
            )?;
            Ok(json!({"sessions":rows,"limit":limit(*count)}))
        }
        GetSessionBrief {
            session_id,
            max_chars,
        } => {
            let row = resolve(connection, session_id, visibility)?;
            let session = index::session_from_index(connection, &row)?;
            let text = sessionwiki::commands::brief_markdown(&session, budget(*max_chars), true);
            Ok(
                json!({"session_id":row.session_id,"text":text.chars().take(budget(*max_chars)).collect::<String>(),"truncated":text.chars().count() > budget(*max_chars)}),
            )
        }
        ReadSession {
            session_id,
            start,
            offset,
            role,
            limit: count,
            max_chars,
            tail,
            before,
            max_message_chars,
        } => {
            let target = substitute_self(session_id, caller_session_id)
                .ok_or_else(|| anyhow!("session \"self\" requires a calling session"))?;
            let row = resolve(connection, target, visibility)?;
            let messages = indexed_messages::load(connection, &row.session_id)?;
            Ok(read_page(
                &row,
                &messages,
                &PageRequest {
                    start: *start,
                    offset: *offset,
                    role: role.as_deref(),
                    count: limit(*count),
                    chars: budget(*max_chars),
                    backward: *tail || before.is_some(),
                    before: *before,
                    per_message: max_message_chars.filter(|&n| n > 0).map(budget),
                },
            ))
        }
        SearchSession {
            session_id,
            query,
            start,
            context,
            limit: count,
            max_chars,
        } => {
            let row = resolve(connection, session_id, visibility)?;
            let session = index::session_from_index(connection, &row)?;
            let context = (*context).min(5);
            let found = super::transcript_grep::grep_session(
                &session,
                query,
                &super::transcript_grep::GrepOpts {
                    context_messages: context,
                    chars: 2000,
                    ..Default::default()
                },
            );
            let mut remaining = budget(*max_chars);
            let mut hits = Vec::new();
            let mut next = None;
            // `limit` counts matching messages. Context rows ride along, so
            // leading context can never use up the limit before an anchor.
            let mut anchors = 0;
            let mut last_anchor = 0;
            for hit in found.hits.into_iter().filter(|hit| hit.i >= *start) {
                let anchor = !hit.matches.is_empty();
                let past_limit = anchors == limit(*count)
                    && (anchor || hit.omitted_before > 0 || hit.i > last_anchor + context);
                if past_limit || remaining == 0 {
                    next = Some(hit.i);
                    break;
                }
                if anchor {
                    anchors += 1;
                    last_anchor = hit.i;
                }
                let text: String = hit.text.chars().take(remaining).collect();
                remaining = remaining.saturating_sub(text.chars().count());
                let matches: Vec<_> = hit
                    .matches
                    .into_iter()
                    .filter(|(_, end)| *end <= text.len())
                    .collect();
                hits.push(json!({"i":hit.i,"role":hit.role,"ts":hit.ts,"text":text,
                    "matches":matches,"truncated":hit.truncated || text.len() < hit.text.len(),"omitted_before":hit.omitted_before}));
            }
            Ok(
                json!({"session_id":row.session_id,"hits":hits,"next_start":next,"total_messages":session.messages.len()}),
            )
        }
    }
}

/// What one `read_session` page asks for, after validation and clamping.
struct PageRequest<'a> {
    start: usize,
    offset: usize,
    role: Option<&'a str>,
    count: usize,
    chars: usize,
    /// `tail`, or `before` was given: page backward from `before` (or the end).
    backward: bool,
    before: Option<usize>,
    per_message: Option<usize>,
}

/// The single owner of what a role filter admits; `dialogue` is user plus assistant.
fn matches_role(filter: Option<&str>, message_role: &str) -> bool {
    match filter {
        None => true,
        Some("dialogue") => matches!(message_role, "user" | "assistant"),
        Some(wanted) => wanted == message_role,
    }
}

/// Slice one message and describe it. `next_offset` appears only when cut.
fn take_message(
    message: &indexed_messages::IndexedMessage,
    skip: usize,
    take: usize,
) -> (Value, indexed_messages::Excerpt) {
    let part = indexed_messages::excerpt(&message.text, skip, take);
    let mut value = json!({
        "i": message.i,
        "role": message.role,
        "text": &part.text,
        "offset": part.offset,
        "text_chars": part.text_chars,
        "truncated": part.next_offset.is_some(),
    });
    if let Some(next_offset) = part.next_offset {
        value["next_offset"] = json!(next_offset);
    }
    (value, part)
}

/// The page envelope: a redacted session header plus the page and its cursors.
fn session_response(
    row: &index::SessionRow,
    total_messages: usize,
    messages: Vec<Value>,
    next: Option<Value>,
    earlier: Option<Value>,
) -> Value {
    use sessionwiki::redact::redact;
    let mut value = json!({
        "session_id": row.session_id,
        "title": redact(&row.title),
        "tool": redact(&row.tool),
    });
    if !row.project.trim().is_empty() {
        value["project"] = json!(redact(&row.project));
    }
    if let Some(started) = &row.started {
        value["started"] = json!(started);
    }
    if let Some(ended) = &row.last_active {
        value["ended"] = json!(ended);
    }
    value["total_messages"] = json!(total_messages);
    value["messages"] = json!(messages);
    value["next"] = json!(next);
    value["earlier"] = json!(earlier);
    value
}

fn read_page(
    row: &index::SessionRow,
    messages: &[indexed_messages::IndexedMessage],
    request: &PageRequest<'_>,
) -> Value {
    let matching: Vec<&indexed_messages::IndexedMessage> = messages
        .iter()
        .filter(|message| matches_role(request.role, message.role))
        .collect();
    // The first message of a skim page always fits in `chars`.
    let cap = request.per_message.map(|cap| cap.min(request.chars));
    let (page, next, earlier) = if request.backward {
        backward_page(&matching, request, cap)
    } else {
        forward_page(&matching, request, cap)
    };
    session_response(row, messages.len(), page, next, earlier)
}

type PageParts = (Vec<Value>, Option<Value>, Option<Value>);

/// The newest matching messages before `before` (or the end), in order.
fn backward_page(
    matching: &[&indexed_messages::IndexedMessage],
    request: &PageRequest<'_>,
    cap: Option<usize>,
) -> PageParts {
    let end = matching.partition_point(|message| message.i < request.before.unwrap_or(usize::MAX));
    let Some(last_pos) = end.checked_sub(1) else {
        return (Vec::new(), None, None);
    };
    let mut remaining = request.chars;
    let mut next = None;
    let last = matching[last_pos];
    let (value, part) = take_message(last, 0, cap.unwrap_or(remaining));
    remaining -= part.text.chars().count();
    let mut selected = vec![value];
    let mut first_pos = last_pos;
    match part.next_offset {
        // An oversized final message is read on from where it was cut.
        Some(next_offset) if cap.is_none() => {
            next = Some(json!({"start": last.i, "offset": next_offset}));
        }
        _ => {
            for pos in (0..last_pos).rev() {
                if selected.len() == request.count {
                    break;
                }
                let message = matching[pos];
                let text_chars = message.text.chars().count();
                let wanted = cap.map_or(text_chars, |cap| text_chars.min(cap));
                if wanted > remaining {
                    break;
                }
                let (value, part) = take_message(message, 0, wanted);
                remaining -= part.text.chars().count();
                selected.push(value);
                first_pos = pos;
            }
        }
    }
    selected.reverse();
    let earlier = (first_pos > 0).then(|| json!({"before": matching[first_pos].i}));
    (selected, next, earlier)
}

/// Matching messages from `start`, stopping on whole-message boundaries when
/// skimming and inside the first oversized message otherwise.
fn forward_page(
    matching: &[&indexed_messages::IndexedMessage],
    request: &PageRequest<'_>,
    cap: Option<usize>,
) -> PageParts {
    let pos0 = matching.partition_point(|message| message.i < request.start);
    let mut remaining = request.chars;
    let mut page = Vec::new();
    let mut next = None;
    for message in &matching[pos0..] {
        if page.len() == request.count || (!page.is_empty() && remaining == 0) {
            next = Some(json!({"start": message.i, "offset": 0}));
            break;
        }
        let skip = if message.i == request.start {
            request.offset
        } else {
            0
        };
        if let Some(cap) = cap {
            let wanted = message.text.chars().count().saturating_sub(skip).min(cap);
            if !page.is_empty() && wanted > remaining {
                next = Some(json!({"start": message.i, "offset": 0}));
                break;
            }
            let (value, part) = take_message(message, skip, wanted);
            remaining -= part.text.chars().count();
            page.push(value);
        } else {
            let (value, part) = take_message(message, skip, remaining);
            remaining -= part.text.chars().count();
            page.push(value);
            if let Some(next_offset) = part.next_offset {
                next = Some(json!({"start": message.i, "offset": next_offset}));
                break;
            }
        }
    }
    let earlier = (!page.is_empty() && pos0 > 0).then(|| json!({"before": matching[pos0].i}));
    (page, next, earlier)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(query: HistoryQuery) -> HistoryRequest {
        HistoryRequest {
            request_id: "test".into(),
            query,
        }
    }

    #[test]
    fn indexed_history_supports_search_drilldown_archives_and_resolution() {
        let _held = super::super::tags::testing::lock();
        let (_directory, connection) = super::super::tags::testing::isolated_index();
        for id in ["session-one", "session-two"] {
            super::super::tags::testing::index_row(&connection, id, "mjolnir");
        }
        connection.execute("UPDATE files SET archived_at = '2026-09-19', started = '2026-09-19T00:00:00Z', ended = '2026-09-19T01:00:00Z' WHERE session_id = 'session-one'", []).unwrap();
        connection
            .execute(
                "INSERT INTO messages(session_id, role, text) VALUES ('session-one', 'user', ?1)",
                [format!(
                    "We chose zebraprotocol for reliable delivery. {}",
                    "é🙂".repeat(40_000)
                )],
            )
            .unwrap();
        connection
            .execute("INSERT INTO msgs(msgs) VALUES ('rebuild')", [])
            .unwrap();
        let hits = query_in(
            &connection,
            &request(HistoryQuery::SearchSessions {
                query: "zebraprotocol".into(),
                limit: 20,
            }),
        )
        .unwrap();
        assert_eq!(hits["sessions"][0]["id"], "session-one");
        let page = query_in(
            &connection,
            &request(HistoryQuery::ReadSession {
                session_id: "session-one".into(),
                start: 0,
                offset: 0,
                role: None,
                limit: 20,
                max_chars: 64_000,
                tail: false,
                before: None,
                max_message_chars: None,
            }),
        )
        .unwrap();
        assert_eq!(
            page["messages"][0]["text"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            64_000
        );
        assert_eq!(page["next"]["offset"], 64_000);
        let hits = query_in(
            &connection,
            &request(HistoryQuery::SearchSession {
                session_id: "session-one".into(),
                query: "zebraprotocol".into(),
                start: 0,
                context: 0,
                limit: 20,
                max_chars: 16000,
            }),
        )
        .unwrap();
        assert_eq!(hits["hits"][0]["i"], 0);
        let ownership = super::super::top_level::Snapshot::default();
        let cache = crate::import::NativeScanCache::new();
        let mut visibility = QueryVisibility::new(&ownership, &cache);
        assert!(
            resolve(&connection, "session-", &mut visibility)
                .err()
                .unwrap()
                .to_string()
                .contains("ambiguous")
        );
        assert!(
            resolve(&connection, "absent", &mut visibility)
                .err()
                .unwrap()
                .to_string()
                .contains("not found")
        );
        let known = super::super::top_level::Snapshot::for_test_top_level("fresh");
        assert_eq!(
            resolve(
                &connection,
                "fresh",
                &mut QueryVisibility::new(&known, &cache)
            )
            .err()
            .unwrap()
            .to_string(),
            "session \"fresh\" exists but is not in the index"
        );
    }
    fn add_messages(connection: &rusqlite::Connection, id: &str, texts: &[&str]) {
        for text in texts {
            connection
                .execute(
                    "INSERT INTO messages(session_id, role, text) VALUES (?1, 'user', ?2)",
                    [id, text],
                )
                .unwrap();
        }
    }

    fn read_request(id: &str) -> HistoryRequest {
        request(HistoryQuery::ReadSession {
            session_id: id.into(),
            start: 0,
            offset: 0,
            role: None,
            limit: 20,
            max_chars: 1000,
            tail: false,
            before: None,
            max_message_chars: None,
        })
    }

    fn add_roled(connection: &rusqlite::Connection, id: &str, messages: &[(&str, &str)]) {
        for (role, text) in messages {
            connection
                .execute(
                    "INSERT INTO messages(session_id, role, text) VALUES (?1, ?2, ?3)",
                    [id, role, text],
                )
                .unwrap();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        connection: &rusqlite::Connection,
        id: &str,
        role: Option<&str>,
        tail: bool,
        before: Option<usize>,
        start: usize,
        offset: usize,
        max_chars: usize,
        max_message_chars: Option<usize>,
        limit: usize,
    ) -> Value {
        query_in(
            connection,
            &request(HistoryQuery::ReadSession {
                session_id: id.into(),
                start,
                offset,
                role: role.map(str::to_owned),
                limit,
                max_chars,
                tail,
                before,
                max_message_chars,
            }),
        )
        .unwrap()
    }

    fn indices(page: &Value) -> Vec<u64> {
        page["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["i"].as_u64().unwrap())
            .collect()
    }

    #[test]
    fn transcript_pages_preserve_unicode_indices_and_continue_inside_large_messages() {
        let _held = super::super::tags::testing::lock();
        let (_directory, connection) = super::super::tags::testing::isolated_index();
        super::super::tags::testing::index_row(&connection, "unicode", "mjolnir");
        add_messages(&connection, "unicode", &["é🙂abcdef"]);
        let first = read(&connection, "unicode", None, false, None, 0, 0, 3, None, 20);
        assert_eq!(first["messages"][0]["text"], "é🙂a");
        assert_eq!(first["messages"][0]["text_chars"], 8);
        assert_eq!(first["messages"][0]["next_offset"], 3);
        assert_eq!(first["next"], json!({"start":0,"offset":3}));
        let second = read(
            &connection,
            "unicode",
            Some("user"),
            false,
            None,
            0,
            3,
            20,
            None,
            20,
        );
        assert_eq!(second["messages"][0]["text"], "bcdef");
        assert!(second["next"].is_null());
        assert!(second["messages"][0].get("next_offset").is_none());
    }

    #[test]
    fn read_session_walks_backward_through_dialogue_with_earlier_cursors_to_the_start() {
        let _held = super::super::tags::testing::lock();
        let (_directory, connection) = super::super::tags::testing::isolated_index();
        super::super::tags::testing::index_row(&connection, "walk", "mjolnir");
        // 0 user, 1 tool, 2 assistant, 3 tool, 4 user, 5 tool, 6 assistant
        add_roled(
            &connection,
            "walk",
            &[
                ("user", "u0"),
                ("tool", "t1"),
                ("assistant", "a2"),
                ("tool", "t3"),
                ("user", "u4"),
                ("tool", "t5"),
                ("assistant", "a6"),
            ],
        );
        let dialogue = Some("dialogue");
        let page = read(
            &connection,
            "walk",
            dialogue,
            true,
            None,
            0,
            0,
            1000,
            None,
            2,
        );
        assert_eq!(indices(&page), [4, 6]);
        assert_eq!(page["earlier"], json!({"before": 4}));
        assert!(page["next"].is_null());
        let page = read(
            &connection,
            "walk",
            dialogue,
            false,
            Some(4),
            0,
            0,
            1000,
            None,
            2,
        );
        assert_eq!(indices(&page), [0, 2]);
        assert!(page["earlier"].is_null(), "index 0 is the start");
        // Without a role filter, `before` still counts every message.
        let page = read(
            &connection,
            "walk",
            None,
            false,
            Some(2),
            0,
            0,
            1000,
            None,
            20,
        );
        assert_eq!(indices(&page), [0, 1]);
        assert_eq!(page["total_messages"], 7);
        // Nothing precedes the first message.
        let page = read(
            &connection,
            "walk",
            dialogue,
            false,
            Some(0),
            0,
            0,
            1000,
            None,
            20,
        );
        assert!(page["messages"].as_array().unwrap().is_empty());
        assert!(page["earlier"].is_null() && page["next"].is_null());
        // A page budget that cannot fit the earlier message stops before it.
        let page = read(&connection, "walk", dialogue, true, None, 0, 0, 3, None, 20);
        assert_eq!(indices(&page), [6]);
        assert_eq!(page["earlier"], json!({"before": 6}));
    }

    #[test]
    fn an_oversized_final_message_is_cut_with_a_forward_cursor_and_an_earlier_cursor() {
        let _held = super::super::tags::testing::lock();
        let (_directory, connection) = super::super::tags::testing::isolated_index();
        super::super::tags::testing::index_row(&connection, "big", "mjolnir");
        let long = "é🙂".repeat(50);
        add_roled(&connection, "big", &[("user", "ask"), ("assistant", &long)]);
        let page = read(&connection, "big", None, true, None, 0, 0, 10, None, 20);
        assert_eq!(indices(&page), [1]);
        assert_eq!(page["messages"][0]["text_chars"], 100);
        assert_eq!(page["next"], json!({"start": 1, "offset": 10}));
        assert_eq!(page["earlier"], json!({"before": 1}));
        // The forward cursor continues where the tail page stopped.
        let more = read(&connection, "big", None, false, None, 1, 10, 10, None, 20);
        assert_eq!(more["messages"][0]["offset"], 10);
        assert_eq!(
            more["messages"][0]["text"],
            long.chars().skip(10).take(10).collect::<String>()
        );
    }

    #[test]
    fn skimming_pages_on_whole_messages_and_treats_zero_as_uncapped() {
        let _held = super::super::tags::testing::lock();
        let (_directory, connection) = super::super::tags::testing::isolated_index();
        super::super::tags::testing::index_row(&connection, "skim", "mjolnir");
        let long = "x".repeat(500);
        add_roled(
            &connection,
            "skim",
            &[
                ("user", &long),
                ("assistant", &long),
                ("user", "short"),
                ("assistant", &long),
            ],
        );
        // 120-char cap: the first message is shown cut, and its continuation is advertised.
        let page = read(
            &connection,
            "skim",
            None,
            false,
            None,
            0,
            0,
            1000,
            Some(120),
            20,
        );
        assert_eq!(indices(&page), [0, 1, 2, 3]);
        assert_eq!(page["messages"][0]["text"].as_str().unwrap().len(), 120);
        assert_eq!(page["messages"][0]["text_chars"], 500);
        assert_eq!(page["messages"][0]["next_offset"], 120);
        assert_eq!(page["messages"][2]["text"], "short");
        assert!(page["next"].is_null());
        // A budget of 241 fits two capped messages; the 5-char third no longer fits and starts a whole next page.
        let page = read(
            &connection,
            "skim",
            None,
            false,
            None,
            0,
            0,
            241,
            Some(120),
            20,
        );
        assert_eq!(indices(&page), [0, 1]);
        assert_eq!(page["next"], json!({"start": 2, "offset": 0}));
        // A cap above the page budget is clamped, so the first message still fits.
        let page = read(
            &connection,
            "skim",
            None,
            false,
            None,
            0,
            0,
            100,
            Some(5000),
            20,
        );
        assert_eq!(indices(&page), [0]);
        assert_eq!(page["messages"][0]["text"].as_str().unwrap().len(), 100);
        assert_eq!(page["next"], json!({"start": 1, "offset": 0}));
        // Zero is "no cap", exactly like omitting it.
        let zero = read(
            &connection,
            "skim",
            None,
            false,
            None,
            0,
            0,
            600,
            Some(0),
            20,
        );
        let none = read(&connection, "skim", None, false, None, 0, 0, 600, None, 20);
        assert_eq!(zero, none);
        assert_eq!(zero["next"], json!({"start": 1, "offset": 100}));
        // Backward skim stops before a message that no longer fits.
        let page = read(
            &connection,
            "skim",
            None,
            true,
            None,
            0,
            0,
            200,
            Some(120),
            20,
        );
        assert_eq!(indices(&page), [2, 3]);
        assert_eq!(page["earlier"], json!({"before": 2}));
    }

    #[test]
    fn read_session_header_is_redacted_resolves_self_and_omits_constant_prose() {
        let _held = super::super::tags::testing::lock();
        let (_directory, connection) = super::super::tags::testing::isolated_index();
        super::super::tags::testing::index_row(&connection, "caller1", "mjolnir");
        connection
            .execute(
                "UPDATE files SET title = 'deploy AKIAIOSFODNN7EXAMPLE', project = '',
                 started = '2026-11-01T00:00:00Z', ended = '2026-11-01T01:00:00Z'
                 WHERE session_id = 'caller1'",
                [],
            )
            .unwrap();
        add_roled(
            &connection,
            "caller1",
            &[("user", "hello"), ("assistant", "hi")],
        );
        let ownership = super::super::top_level::Snapshot::default();
        let cache = crate::import::NativeScanCache::new();
        let page = run_query(
            &connection,
            &request(HistoryQuery::ReadSession {
                session_id: "self".into(),
                start: 0,
                offset: 0,
                role: None,
                limit: 20,
                max_chars: 1000,
                tail: true,
                before: None,
                max_message_chars: None,
            }),
            &mut QueryVisibility::new(&ownership, &cache),
            Some("caller1"),
        )
        .unwrap();
        assert_eq!(page["session_id"], "caller1");
        assert_eq!(page["tool"], "mjolnir");
        assert!(
            page["title"].as_str().unwrap().contains("[redacted"),
            "{page}"
        );
        assert!(page.get("project").is_none(), "an empty project is omitted");
        assert_eq!(page["started"], "2026-11-01T00:00:00Z");
        assert_eq!(page["ended"], "2026-11-01T01:00:00Z");
        assert_eq!(page["total_messages"], 2);
        assert!(page["messages"][0].get("ts").is_none());
        // "self" with no calling session is an error, not a lookup for the word.
        let error = run_query(
            &connection,
            &request(HistoryQuery::ReadSession {
                session_id: "self".into(),
                start: 0,
                offset: 0,
                role: None,
                limit: 20,
                max_chars: 1000,
                tail: false,
                before: None,
                max_message_chars: None,
            }),
            &mut QueryVisibility::new(&ownership, &cache),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("calling session"), "{error:#}");
        // A sub-agent reading "self" is pointed at its parent.
        let child = super::super::top_level::Snapshot::for_test_child("caller9", "caller1");
        let error = run_query(
            &connection,
            &request(HistoryQuery::ReadSession {
                session_id: "self".into(),
                start: 0,
                offset: 0,
                role: None,
                limit: 20,
                max_chars: 1000,
                tail: false,
                before: None,
                max_message_chars: None,
            }),
            &mut QueryVisibility::new(&child, &cache),
            Some("caller9"),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("sub-agent caller9") && message.contains("parent session caller1"),
            "{message}"
        );
        // An unknown id explains why child and reviewer transcripts are missing.
        let error = run_query(
            &connection,
            &read_request("absent"),
            &mut QueryVisibility::new(&ownership, &cache),
            None,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("never indexed")
                && error.to_string().contains("report directory"),
            "{error:#}"
        );
    }

    #[test]
    fn the_retired_session_brief_still_executes_for_older_workers() {
        let _held = super::super::tags::testing::lock();
        let (_directory, connection) = super::super::tags::testing::isolated_index();
        super::super::tags::testing::index_row(&connection, "old1", "mjolnir");
        add_roled(&connection, "old1", &[("user", "hello brief")]);
        let brief = query_in(
            &connection,
            &request(HistoryQuery::GetSessionBrief {
                session_id: "old1".into(),
                max_chars: 4000,
            }),
        )
        .unwrap();
        assert_eq!(brief["session_id"], "old1");
        assert!(brief["text"].as_str().unwrap().contains("hello brief"));
    }

    // Hard-won: live-audit: a native UUID matched only through files.path was discarded for not prefixing the Mjolnir id.
    #[test]
    fn native_uuid_in_the_indexed_path_resolves_its_session() {
        let _held = super::super::tags::testing::lock();
        let (_directory, connection) = super::super::tags::testing::isolated_index();
        let native = "019a0000-0000-7000-8000-00000000abcd";
        super::super::tags::testing::index_row(&connection, "ffff0001", "codex");
        connection
            .execute(
                "UPDATE files SET path = ?1",
                [format!("/home/user/.codex/sessions/rollout-{native}.jsonl")],
            )
            .unwrap();
        add_messages(&connection, "ffff0001", &["hello native"]);
        for input in [native, &native[..13]] {
            let page = query_in(&connection, &read_request(input)).unwrap();
            assert_eq!(page["session_id"], "ffff0001", "{input}");
            assert_eq!(page["messages"][0]["text"], "hello native");
        }
    }

    #[test]
    fn native_prefix_combines_owned_and_independently_indexed_sessions() {
        let _held = super::super::tags::testing::lock();
        let (_dir, connection) = super::super::tags::testing::isolated_index();
        let ownership = super::super::top_level::Snapshot::for_test_native(
            "owned",
            "019a1111-0000-7000-8000-00000000aaaa",
        );
        let cache = crate::import::NativeScanCache::new();
        for id in ["owned", "independent"] {
            super::super::tags::testing::index_row(&connection, id, "mjolnir");
        }
        connection.execute("UPDATE files SET path='/native/019a1111-0000-7000-8000-00000000bbbb.jsonl' WHERE session_id='independent'",[]).unwrap();
        let lookup = || {
            resolve_lookup(
                &connection,
                "019a1111-0000",
                &mut QueryVisibility::new(&ownership, &cache),
            )
            .unwrap()
        };
        match lookup() {
            SessionLookup::Ambiguous { candidates, .. } => assert_eq!(candidates.len(), 2),
            _ => panic!("expected both candidates"),
        }
        connection
            .execute("DELETE FROM files WHERE session_id='owned'", [])
            .unwrap();
        match lookup() {
            SessionLookup::Resolved(row) => assert_eq!(row.session_id, "independent"),
            _ => panic!("expected readable independent session"),
        }
    }

    #[test]
    fn lookups_point_children_to_parents_and_name_every_ambiguous_candidate() {
        let _held = super::super::tags::testing::lock();
        let (_directory, connection) = super::super::tags::testing::isolated_index();
        let cache = crate::import::NativeScanCache::new();
        let native = "019a1111-0000-7000-8000-00000000beef";
        for id in ["aaaa0001", "aaaa0002"] {
            super::super::tags::testing::index_row(&connection, id, "mjolnir");
        }
        add_messages(&connection, "aaaa0001", &["mjolnir native"]);
        let error = |ownership: &super::super::top_level::Snapshot, id: &str| {
            query_in_with(&connection, &read_request(id), ownership, &cache)
                .err()
                .map(|error| format!("{error:#}"))
                .unwrap_or_default()
        };
        // A Mjolnir session answers to its native UUID, which its path never holds.
        let native_owner = super::super::top_level::Snapshot::for_test_native("aaaa0001", native);
        let page =
            query_in_with(&connection, &read_request(native), &native_owner, &cache).unwrap();
        assert_eq!(page["session_id"], "aaaa0001");
        // A child names its parent instead of "not found", and a child id never
        // hides a visible top-level session that shares the prefix.
        let child = super::super::top_level::Snapshot::for_test_child("aaaa0009", "aaaa0001");
        let message = error(&child, "aaaa0009");
        assert!(
            message.contains("sub-agent aaaa0009") && message.contains("parent session aaaa0001"),
            "{message}"
        );
        let message = error(&child, "aaaa000");
        assert!(message.contains("ambiguous"), "{message}");
        assert!(
            message.contains("aaaa0001") && message.contains("aaaa0002"),
            "{message}"
        );
        assert!(message.contains("an indexed session"), "{message}");
        let message = error(
            &super::super::top_level::Snapshot::for_test_top_level("bbbb0001"),
            "bbbb",
        );
        assert!(message.contains("not in the index"), "{message}");
    }

    // Hard-won: live-audit: leading context rows used up `limit` before any matching message was returned.
    #[test]
    fn search_session_limit_counts_matching_messages_not_context() {
        let _held = super::super::tags::testing::lock();
        let (_directory, connection) = super::super::tags::testing::isolated_index();
        super::super::tags::testing::index_row(&connection, "session-ctx", "mjolnir");
        let texts = [
            "filler 0",
            "filler 1",
            "filler 2",
            "filler 3",
            "filler 4",
            "filler 5",
            "filler 6",
            "needle seven",
            "needle eight",
            "needle nine",
        ];
        add_messages(&connection, "session-ctx", &texts);
        let hits = query_in(
            &connection,
            &request(HistoryQuery::SearchSession {
                session_id: "session-ctx".into(),
                query: "needle".into(),
                start: 0,
                context: 5,
                limit: 2,
                max_chars: 16_000,
            }),
        )
        .unwrap();
        let hits_list = hits["hits"].as_array().unwrap();
        let anchors: Vec<_> = hits_list
            .iter()
            .filter(|hit| !hit["matches"].as_array().unwrap().is_empty())
            .map(|hit| hit["i"].as_u64().unwrap())
            .collect();
        assert_eq!(anchors, [7, 8]);
        assert_eq!(hits["next_start"], 9);
        assert_eq!(
            hits_list[0]["i"], 2,
            "context before the first anchor stays"
        );
    }

    #[test]
    fn an_unclassifiable_session_is_skipped_with_one_notice_instead_of_failing_the_query() {
        let _held = super::super::tags::testing::lock();
        let (_directory, connection) = super::super::tags::testing::isolated_index();
        let home = tempfile::tempdir().unwrap();
        let broken = home
            .path()
            .join("rollout-019a0000-0000-7000-8000-00000000dead.jsonl");
        std::fs::write(&broken, "this is not JSON\n").unwrap();
        for id in ["good", "broken"] {
            super::super::tags::testing::index_row(&connection, id, "mjolnir");
            connection
                .execute(
                    "INSERT INTO messages(session_id, role, text) VALUES (?1, 'user', 'shared needle')",
                    [id],
                )
                .unwrap();
        }
        connection
            .execute("INSERT INTO msgs(msgs) VALUES ('rebuild')", [])
            .unwrap();
        connection
            .execute(
                "UPDATE files SET tool = 'codex', path = ?1 WHERE session_id = 'broken'",
                [broken.to_string_lossy()],
            )
            .unwrap();
        let value = query_in(
            &connection,
            &request(HistoryQuery::SearchHistory {
                query: "needle".into(),
                session_ids: None,
                session_id: None,
                role: None,
                order: None,
                limit: 20,
                context_messages: 0,
                max_chars: 16_000,
                cursor: None,
            }),
        )
        .unwrap();
        let sessions = value["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session_id"], "good");
        let notices = value["notices"].as_array().unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].as_str().unwrap().contains("broken"));
    }

    const ENDLESS: &str =
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c";

    fn endless(budget: QueryBudget) -> Result<i64> {
        let mut connection = rusqlite::Connection::open_in_memory()?;
        budget.install(&mut connection)?;
        Ok(connection.query_row(ENDLESS, [], |row| row.get(0))?)
    }

    // Hard-won: live-audit: a timed-out query kept spinning in SQLite and held its permit, starving later queries.
    #[tokio::test]
    async fn a_spent_deadline_or_dropped_caller_stops_sqlite_and_frees_the_permit() {
        let permits = Arc::new(tokio::sync::Semaphore::new(4));
        let started = Instant::now();
        let mut running = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let permits = permits.clone();
            running.spawn(async move {
                run_bounded(&permits, Duration::from_millis(100), endless).await
            });
        }
        while let Some(done) = running.join_next().await {
            let error = done.unwrap().unwrap_err();
            assert_eq!(error.to_string(), TIMED_OUT);
        }
        // SQLite stopped itself: the five-second backstop never had to.
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_eq!(permits.available_permits(), 4);

        let abandoned = {
            let permits = permits.clone();
            mj_core::runtime::spawn(async move {
                run_bounded(&permits, Duration::from_secs(600), endless).await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(permits.available_permits(), 3);
        abandoned.abort();
        let all = tokio::time::timeout(Duration::from_secs(3), permits.acquire_many(4))
            .await
            .expect("the dropped query released its permit");
        drop(all);
    }

    fn test_work(
        request: &HistoryRequest,
        _budget: &QueryBudget,
        caller_session_id: Option<&str>,
    ) -> Result<Value> {
        let connection = super::super::readable_index()?.context("index is unreadable")?;
        query_in_for(
            &connection,
            request,
            &super::super::top_level::Snapshot::default(),
            &crate::import::NativeScanCache::new(),
            caller_session_id,
        )
    }

    #[tokio::test]
    #[allow(
        clippy::await_holding_lock,
        reason = "the test owns SESSIONWIKI_DATA for its whole duration"
    )]
    async fn full_text_searches_wait_for_the_migration_without_holding_history_permits() {
        let _held = super::super::tags::testing::lock();
        let (_directory, connection) = super::super::tags::testing::isolated_index();
        super::super::tags::testing::index_row(&connection, "aaaa0001", super::super::TOOL);
        connection
            .execute(
                "INSERT INTO messages(session_id, role, text) VALUES ('aaaa0001', 'user', 'hello')",
                [],
            )
            .unwrap();
        connection
            .execute("INSERT INTO msgs(msgs) VALUES ('rebuild')", [])
            .unwrap();
        sessionwiki::index::set_tokenizer_spec(&connection, "trigram").unwrap();
        let permits = Arc::new(tokio::sync::Semaphore::new(4));
        let search = |id: &str| HistoryRequest {
            request_id: id.into(),
            query: HistoryQuery::SearchHistory {
                query: "hello".into(),
                session_ids: None,
                session_id: None,
                role: None,
                order: None,
                limit: 20,
                context_messages: 0,
                max_chars: 16_000,
                cursor: None,
            },
        };
        // More waiting searches than permits.
        let waiting: Vec<_> = (0..5)
            .map(|n| {
                let permits = permits.clone();
                let request = search(&format!("search-{n}"));
                mj_core::runtime::spawn(async move {
                    execute_with(&permits, request, None, test_work).await
                })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(waiting.iter().all(|task| !task.is_finished()));
        assert_eq!(permits.available_permits(), 4);
        // A tool that does not read the FTS table runs at once on the same index.
        let read = tokio::time::timeout(
            Duration::from_secs(3),
            execute_with(&permits, read_request("aaaa0001"), None, test_work),
        )
        .await
        .expect("a read_session call must not wait for the migration");
        assert!(!read.is_error, "{:?}", read.value);
        assert_eq!(read.value["messages"][0]["text"], "hello");
        super::super::migrate_tokenizer(&connection).unwrap();
        for task in waiting {
            let result = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("a waiting search finishes after the migration")
                .unwrap();
            assert!(!result.is_error, "{:?}", result.value);
        }
        assert_eq!(permits.available_permits(), 4);
    }
}
