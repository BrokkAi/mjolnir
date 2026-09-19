//! Bounded, read-only history operations for target-side MCP clients.
use anyhow::{Context, Result, bail, ensure};
use mj_core::history::{HistoryQuery, HistoryRequest, HistoryResult};
use serde_json::{Value, json};
use sessionwiki::{index, model::Session};

pub(crate) async fn execute(request: HistoryRequest) -> HistoryResult {
    static PERMITS: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    let id = request.request_id.clone();
    let permits = PERMITS
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(4)))
        .clone();
    let work = async {
        let permit = permits
            .acquire_owned()
            .await
            .context("history service stopped")?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            query(&request)
        })
        .await
        .context("history query task failed")?
    };
    match tokio::time::timeout(mj_core::history::QUERY_TIMEOUT, work).await {
        Ok(Ok(value)) => match serde_json::to_vec(&value) {
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
        Ok(Err(error)) => HistoryResult::failed(id, format!("{error:#}")),
        Err(_) => HistoryResult::failed(id, "history query timed out; retry or narrow the query"),
    }
}

fn resolve(connection: &rusqlite::Connection, id: &str) -> Result<index::SessionRow> {
    ensure!(!id.is_empty() && id.len() <= 256, "invalid session id");
    let mut rows = index::resolve(connection, id)?;
    rows.retain(|row| row.session_id.starts_with(id));
    if let Some(position) = rows.iter().position(|row| row.session_id == id) {
        return Ok(rows.remove(position));
    }
    match rows.len() {
        0 => bail!("session {id:?} was not found in the index"),
        1 => Ok(rows.remove(0)),
        _ => bail!("session prefix {id:?} is ambiguous; use an id from search results"),
    }
}

fn limit(value: usize) -> usize {
    value.clamp(1, 100)
}
fn budget(value: usize) -> usize {
    value.clamp(1, 64_000)
}

fn query(request: &HistoryRequest) -> Result<Value> {
    request.query.validate()?;
    ensure!(
        super::index_is_isolated(),
        "session index location has not been configured"
    );
    ensure!(
        !super::index_version_mismatch(),
        "SessionWiki index version mismatch; the index was not modified"
    );
    let connection = super::open_readonly()?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;
    let value = query_in(&connection, request)?;
    Ok(json!({
        "data": value,
        "index_state": super::index_state(),
        "source": "indexed_snapshot",
        "coverage": "Indexed history can lag live sessions. Retained transcripts may omit original timestamps, tool output, or older messages. File evidence discarded before provenance indexing cannot be recovered.",
    }))
}

fn query_in(connection: &rusqlite::Connection, request: &HistoryRequest) -> Result<Value> {
    use HistoryQuery::*;
    match &request.query {
        SearchSessions {
            query,
            limit: count,
        } => {
            let rows = super::query_rows(query, limit(*count), &Default::default())?;
            Ok(json!({"sessions":rows,"limit":limit(*count)}))
        }
        TraceFile { path, limit: count } => {
            let rows =
                index::sessions_for_file(connection, &path.replace('\\', "/"), limit(*count))?;
            Ok(
                json!({"sessions":rows.into_iter().map(|(row, matched)| json!({"session":row,"matched_path":matched})).collect::<Vec<_>>() }),
            )
        }
        SessionFiles {
            session_id,
            start,
            limit: count,
        } => {
            let row = resolve(connection, session_id)?;
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
        ),
        GetSessionBrief {
            session_id,
            max_chars,
        } => {
            let row = resolve(connection, session_id)?;
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
            let row = resolve(connection, session_id)?;
            let session = index::session_from_index(connection, &row)?;
            Ok(read_page(
                &session,
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
            let row = resolve(connection, session_id)?;
            let session = index::session_from_index(connection, &row)?;
            let found = sessionwiki::grep::grep_session(
                &session,
                query,
                &sessionwiki::grep::GrepOpts {
                    context_messages: (*context).min(5),
                    chars: 2000,
                    ..Default::default()
                },
            );
            let mut remaining = budget(*max_chars);
            let mut hits = Vec::new();
            let mut next = None;
            for hit in found.hits.into_iter().filter(|hit| hit.i >= *start) {
                if hits.len() == limit(*count) || remaining == 0 {
                    next = Some(hit.i);
                    break;
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
    session: &Session,
    start: usize,
    offset: usize,
    role: Option<&str>,
    count: usize,
    chars: usize,
) -> Value {
    let mut remaining = chars;
    let mut messages = Vec::new();
    let mut next = None;
    for (i, message) in session.messages.iter().enumerate().skip(start) {
        if role.is_some_and(|role| role != super::role_name(message.role)) {
            continue;
        }
        if remaining == 0 || messages.len() == count {
            next = Some(json!({"start":i,"offset":0}));
            break;
        }
        let text = sessionwiki::redact::redact(&message.text);
        let skip = if i == start { offset } else { 0 };
        let available = text.chars().count().saturating_sub(skip);
        let part: String = text.chars().skip(skip).take(remaining).collect();
        let taken = part.chars().count();
        remaining -= taken;
        let truncated = taken < available;
        messages.push(json!({"i":i,"role":message.role,"ts":message.ts,"text":part,"offset":skip,"truncated":truncated}));
        if truncated {
            next = Some(json!({"start":i,"offset":skip+taken}));
            break;
        }
    }
    json!({"session_id":session.id,"messages":messages,"next":next,"total_messages":session.messages.len()})
}

fn blame(
    connection: &rusqlite::Connection,
    evidence: &mj_core::history::BlameEvidence,
) -> Result<Value> {
    ensure!(
        evidence.porcelain.len() <= mj_core::history::MAX_BLAME_BYTES,
        "Git blame evidence exceeds its byte budget"
    );
    let query = evidence.relative_path.to_string_lossy().replace('\\', "/");
    // sessions_touching uses SQL LIKE; intersect with the literal-path matcher
    // so '%' and '_' in a real file name cannot introduce unrelated candidates.
    let ids: std::collections::BTreeSet<_> = index::sessions_for_file(connection, &query, 10_000)?
        .into_iter()
        .map(|(row, _)| row.session_id)
        .collect();
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
        assert!(
            resolve(&connection, "session-")
                .err()
                .unwrap()
                .to_string()
                .contains("ambiguous")
        );
        assert!(
            resolve(&connection, "absent")
                .err()
                .unwrap()
                .to_string()
                .contains("not found")
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
        let result = blame(&connection, &evidence).unwrap();
        assert_eq!(result["runs"][0]["status"], "confident");
        assert_eq!(result["runs"][1]["status"], "unattributed");
    }
    #[test]
    fn transcript_pages_preserve_unicode_indices_and_continue_inside_large_messages() {
        let session = Session {
            id: "id".into(),
            tool: "mjolnir",
            path: "/s".into(),
            project: "p".into(),
            title: "t".into(),
            started: None,
            ended: None,
            subagent: false,
            touched: vec![],
            edits: vec![],
            messages: vec![sessionwiki::model::Message {
                role: sessionwiki::model::Role::User,
                text: "é🙂abcdef".into(),
                ts: None,
            }],
        };
        let first = read_page(&session, 0, 0, None, 20, 3);
        assert_eq!(first["messages"][0]["text"], "é🙂a");
        assert_eq!(first["next"], json!({"start":0,"offset":3}));
        let second = read_page(&session, 0, 3, Some("user"), 20, 20);
        assert_eq!(second["messages"][0]["text"], "bcdef");
        assert!(second["next"].is_null());
    }
}
