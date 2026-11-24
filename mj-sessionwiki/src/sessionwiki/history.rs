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

pub async fn execute(request: HistoryRequest) -> HistoryResult {
    static PERMITS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let id = request.request_id.clone();
    let permits = PERMITS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)))
        .clone();
    let outcome = run_bounded(&permits, mj_core::history::QUERY_TIMEOUT, move |budget| {
        query(&request, &budget)
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
    // The native UUID of a Mjolnir session appears nowhere in its indexed path.
    if let Some(session_id) = ownership.resolve_native_session_id(id) {
        for row in index::resolve(connection, session_id)? {
            if row.session_id == session_id && !visibility.is_child_lenient(&row) {
                return Ok(SessionLookup::Resolved(row));
            }
        }
        return Ok(SessionLookup::UnindexedLive {
            input: id.to_owned(),
            session_id: session_id.to_owned(),
        });
    }
    let mut visible = Vec::new();
    for row in index::resolve(connection, id)? {
        if !visibility.is_child_lenient(&row) {
            visible.push(row);
        }
    }
    if let Some(position) = visible.iter().position(|row| row.session_id == id) {
        return Ok(SessionLookup::Resolved(visible.remove(position)));
    }
    // An exact native UUID outranks other rows that merely share its prefix.
    let wanted = Some(id.to_ascii_lowercase());
    if visible
        .iter()
        .any(|row| index::native_id_of(&row.path) == wanted)
    {
        visible.retain(|row| index::native_id_of(&row.path) == wanted);
    }
    match visible.len() {
        0 => {}
        1 => return Ok(SessionLookup::Resolved(visible.remove(0))),
        _ => {
            return Ok(SessionLookup::Ambiguous {
                input: id.to_owned(),
                candidates: visible.iter().map(AmbiguousSession::of).collect(),
            });
        }
    }
    if let Some(pointer) = ownership.child_pointer(id) {
        return Ok(child_not_indexed(id, pointer));
    }
    Ok(match ownership.known_top_level(id) {
        Some(session_id) => SessionLookup::UnindexedLive {
            input: id.to_owned(),
            session_id: session_id.to_owned(),
        },
        None => SessionLookup::NotFound {
            input: id.to_owned(),
        },
    })
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
        SessionLookup::NotFound { input } => bail!("session {input:?} was not found in the index"),
    }
}

fn limit(value: usize) -> usize {
    value.clamp(1, 100)
}
fn budget(value: usize) -> usize {
    value.clamp(1, 64_000)
}

fn query(request: &HistoryRequest, budget: &QueryBudget) -> Result<Value> {
    request.query.validate()?;
    let mut connection = super::readable_index()?.with_context(super::unreadable_reason)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    super::wait_for_tokenizer(&connection, budget.deadline, &budget.cancel)?;
    budget.install(&mut connection)?;
    let ownership = super::top_level::current_snapshot()?;
    let cache = crate::import::NativeScanCache::shared();
    let value = query_in_with(&connection, request, &ownership, &cache)?;
    Ok(json!({
        "data": value,
        "index_state": super::index_state(),
        "source": "indexed_snapshot",
        "coverage": "Indexed history can lag live sessions. Retained transcripts may omit original timestamps, tool output, or older messages. File evidence discarded before provenance indexing cannot be recovered.",
    }))
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

pub(super) fn query_in_with(
    connection: &rusqlite::Connection,
    request: &HistoryRequest,
    ownership: &super::top_level::Snapshot,
    cache: &crate::import::NativeScanCache,
) -> Result<Value> {
    let mut visibility = QueryVisibility::new(ownership, cache);
    let mut value = run_query(connection, request, &mut visibility)?;
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
) -> Result<Value> {
    use HistoryQuery::*;
    match &request.query {
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
        TraceFile { path, limit: count } => {
            let candidates = index::sessions_for_file(
                connection,
                &path.replace('\\', "/"),
                super::MAX_WIKI_LIMIT,
            )?;
            let mut rows = Vec::with_capacity(limit(*count));
            for (row, matched) in candidates {
                if rows.len() >= limit(*count) {
                    break;
                }
                if !visibility.is_child_lenient(&row) {
                    rows.push(json!({"session":row,"matched_path":matched}));
                }
            }
            Ok(json!({"sessions":rows}))
        }
        SessionFiles {
            session_id,
            start,
            limit: count,
        } => {
            let row = resolve(connection, session_id, visibility)?;
            let files = index::files_for(connection, &row.session_id)?;
            let end = start.saturating_add(limit(*count)).min(files.len());
            Ok(
                json!({"session_id":row.session_id,"files":files.get(*start..end).unwrap_or_default(),
                "next_start":(end < files.len()).then_some(end),"total":files.len(),
                "provenance_indexed":row.tool != "mjolnir" || row.tags.as_deref().is_some_and(|tags| tags.split(',').any(|tag| tag == super::provenance::REVISION)) }),
            )
        }
        BlameFile { .. } => blame(
            connection,
            request
                .blame
                .as_ref()
                .context("target did not supply Git blame evidence")?,
            visibility,
        ),
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
        } => {
            let row = resolve(connection, session_id, visibility)?;
            let messages = indexed_messages::load(connection, &row.session_id)?;
            Ok(read_page(
                &row.session_id,
                &messages,
                *start,
                *offset,
                role.as_deref(),
                limit(*count),
                budget(*max_chars),
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

fn read_page(
    session_id: &str,
    messages: &[indexed_messages::IndexedMessage],
    start: usize,
    offset: usize,
    role: Option<&str>,
    count: usize,
    chars: usize,
) -> Value {
    let mut remaining = chars;
    let mut page = Vec::new();
    let mut next = None;
    for message in messages.iter().skip(start) {
        if role.is_some_and(|role| role != message.role) {
            continue;
        }
        if remaining == 0 || page.len() == count {
            next = Some(json!({"start":message.i,"offset":0}));
            break;
        }
        let skip = if message.i == start { offset } else { 0 };
        let part = indexed_messages::excerpt(&message.text, skip, remaining);
        remaining -= part.text.chars().count();
        let truncated = part.next_offset.is_some();
        page.push(json!({"i":message.i,"role":message.role,"ts":Value::Null,"text":part.text,"offset":part.offset,"truncated":truncated}));
        if let Some(next_offset) = part.next_offset {
            next = Some(json!({"start":message.i,"offset":next_offset}));
            break;
        }
    }
    json!({"session_id":session_id,"messages":page,"next":next,"total_messages":messages.len()})
}

fn blame(
    connection: &rusqlite::Connection,
    evidence: &mj_core::history::BlameEvidence,
    visibility: &mut QueryVisibility<'_>,
) -> Result<Value> {
    ensure!(
        evidence.porcelain.len() <= mj_core::history::MAX_BLAME_BYTES,
        "Git blame evidence exceeds its byte budget"
    );
    let query = evidence.relative_path.to_string_lossy().replace('\\', "/");
    // sessions_touching uses SQL LIKE; intersect with the literal-path matcher
    // so '%' and '_' in a real file name cannot introduce unrelated candidates.
    let mut ids = std::collections::BTreeSet::new();
    for (row, _) in index::sessions_for_file(connection, &query, 10_000)? {
        if !visibility.is_child_lenient(&row) {
            ids.insert(row.session_id);
        }
    }
    let candidates: Vec<_> = index::sessions_touching(connection, &query)?
        .into_iter()
        .filter(|candidate| ids.contains(&candidate.session_id))
        .collect();
    let runs = sessionwiki::blame::group_runs(&sessionwiki::blame::parse_line_porcelain(
        &evidence.porcelain,
    ));
    let runs: Vec<_> = runs.into_iter().map(|run| {
        let attribution = if run.commit.bytes().all(|byte| byte == b'0') {
            sessionwiki::blame::Attribution::Unattributed
        } else {
            sessionwiki::blame::attribute_commit(run.author_time, &evidence.repository.to_string_lossy(), &candidates)
        };
        let candidate = |s: sessionwiki::blame::TouchingSession| json!({"session_id":s.session_id,"tool":s.tool,"title":s.title,"project":s.project,"archived":s.archived});
        let (status, sessions) = match attribution {
            sessionwiki::blame::Attribution::Confident(session) => ("confident", vec![candidate(session)]),
            sessionwiki::blame::Attribution::Ambiguous(sessions) => ("ambiguous", sessions.into_iter().map(candidate).collect()),
            sessionwiki::blame::Attribution::Unattributed => ("unattributed", vec![]),
        };
        json!({"start":run.start,"end":run.end,"commit":run.commit,"status":status,"sessions":sessions})
    }).collect();
    Ok(
        json!({"path":evidence.relative_path,"runs":runs,"heuristic":true,"note":"Attribution uses recorded file edits and commit timing; verify against the conversation and Git history."}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(query: HistoryQuery) -> HistoryRequest {
        HistoryRequest {
            request_id: "test".into(),
            query,
            blame: None,
        }
    }

    #[test]
    fn indexed_history_supports_search_drilldown_archives_and_literal_file_evidence() {
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
            .execute(
                "INSERT INTO touched VALUES ('session-one', '/old/target/src/a_%.rs')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO touched VALUES ('session-two', '/different/src/axY.rs')",
                [],
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
        let trace = query_in(
            &connection,
            &request(HistoryQuery::TraceFile {
                path: "src/a_%.rs".into(),
                limit: 20,
            }),
        )
        .unwrap();
        assert_eq!(trace["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(trace["sessions"][0]["session"]["id"], "session-one");
        assert_eq!(trace["sessions"][0]["session"]["archived"], true);
        let files = query_in(
            &connection,
            &request(HistoryQuery::SessionFiles {
                session_id: "session-one".into(),
                start: 0,
                limit: 20,
            }),
        )
        .unwrap();
        assert_eq!(files["files"][0], "/old/target/src/a_%.rs");
        assert_eq!(files["provenance_indexed"], false);
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
        let evidence = mj_core::history::BlameEvidence {
            repository: "/src/project".into(),
            relative_path: "src/a_%.rs".into(),
            porcelain: format!(
                "{} 1 1 1\nauthor-time 1789777800\n\tline\n{} 2 2 1\nauthor-time 1789777800\n\tchanged\n",
                "a".repeat(40),
                "0".repeat(40)
            ),
        };
        let result = blame(&connection, &evidence, &mut visibility).unwrap();
        assert_eq!(result["runs"][0]["status"], "confident");
        assert_eq!(result["runs"][1]["status"], "unattributed");
    }
    #[test]
    fn transcript_pages_preserve_unicode_indices_and_continue_inside_large_messages() {
        let messages = [indexed_messages::IndexedMessage {
            id: 1,
            i: 0,
            role: "user",
            text: "é🙂abcdef".into(),
        }];
        let first = read_page("id", &messages, 0, 0, None, 20, 3);
        assert_eq!(first["messages"][0]["text"], "é🙂a");
        assert_eq!(first["next"], json!({"start":0,"offset":3}));
        let second = read_page("id", &messages, 0, 3, Some("user"), 20, 20);
        assert_eq!(second["messages"][0]["text"], "bcdef");
        assert!(second["next"].is_null());
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
        })
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
                .execute("INSERT INTO touched VALUES (?1, '/src/shared.rs')", [id])
                .unwrap();
        }
        connection
            .execute(
                "UPDATE files SET tool = 'codex', path = ?1 WHERE session_id = 'broken'",
                [broken.to_string_lossy()],
            )
            .unwrap();
        let value = query_in(
            &connection,
            &request(HistoryQuery::TraceFile {
                path: "src/shared.rs".into(),
                limit: 20,
            }),
        )
        .unwrap();
        let sessions = value["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session"]["id"], "good");
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
}
