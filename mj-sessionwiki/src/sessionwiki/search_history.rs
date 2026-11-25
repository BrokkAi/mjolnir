//! Unified FTS history search. Coordinates always come from indexed_messages.
use super::{
    history::{SessionLookup, resolve_lookup},
    indexed_messages,
    search_query::{self, QueryAtom},
    top_level::QueryVisibility,
};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use sessionwiki::index::SessionRow;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

pub(super) struct SearchHistoryParams {
    pub query: String,
    pub session_ids: Vec<String>,
    pub role: Option<String>,
    pub order: Option<String>,
    pub limit: usize,
    pub context_messages: usize,
    pub max_chars: usize,
    pub cursor: Option<String>,
}
impl Default for SearchHistoryParams {
    fn default() -> Self {
        Self {
            query: String::new(),
            session_ids: Vec::new(),
            role: None,
            order: None,
            limit: 20,
            context_messages: 0,
            max_chars: 16_000,
            cursor: None,
        }
    }
}
struct FileMeta {
    row: SessionRow,
}
impl FileMeta {
    /// Newest-first key. Live updates only raise `ended`, which moves a session
    /// toward the front, never back past a cursor already issued.
    fn recency(&self) -> &str {
        self.row
            .last_active
            .as_deref()
            .or(self.row.started.as_deref())
            .unwrap_or("")
    }
    /// Oldest-first key. `started` never changes, so a live `ended` update
    /// cannot move an already returned session past a cursor.
    fn age(&self) -> &str {
        self.row
            .started
            .as_deref()
            .or(self.row.last_active.as_deref())
            .unwrap_or("")
    }
    fn order_key(&self, newest: bool) -> &str {
        if newest { self.recency() } else { self.age() }
    }
}

// Kept local: this format belongs only to the opaque history cursor.
fn encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut result = String::new();
    let mut bits = 0u32;
    let mut count = 0;
    for byte in bytes {
        bits = (bits << 8) | u32::from(*byte);
        count += 8;
        while count >= 6 {
            count -= 6;
            result.push(ALPHABET[((bits >> count) & 63) as usize] as char);
        }
    }
    if count > 0 {
        result.push(ALPHABET[((bits << (6 - count)) & 63) as usize] as char);
    }
    result
}
fn decode(text: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut bits = 0u32;
    let mut count = 0;
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => bail!("malformed search_history cursor"),
        };
        bits = (bits << 6) | u32::from(v);
        count += 6;
        if count >= 8 {
            count -= 8;
            bytes.push((bits >> count) as u8);
        }
    }
    ensure!(encode(&bytes) == text, "malformed search_history cursor");
    Ok(bytes)
}
fn finish(mut value: Value, unresolved: Vec<Value>, visibility: &mut QueryVisibility<'_>) -> Value {
    if !unresolved.is_empty() {
        value["unresolved_sessions"] = json!(unresolved);
    }
    let notices = visibility.take_notices();
    if !notices.is_empty() {
        value["notices"] = json!(notices);
    }
    value
}

/// FTS is always the outer loop, including scoped diagnostic probes.
fn matching(
    connection: &rusqlite::Connection,
    fts: &str,
    scope: Option<&[String]>,
    role: Option<&str>,
    mut accept: impl FnMut(i64, String) -> bool,
) -> Result<()> {
    let role = match role {
        None => "",
        Some("user") => " AND m.role = 'user'",
        Some("assistant") => " AND m.role = 'assistant'",
        Some("tool") => " AND m.role NOT IN ('user', 'assistant')",
        _ => bail!("role must be user, assistant, or tool"),
    };
    let base = "SELECT m.id, m.session_id FROM msgs CROSS JOIN messages m ON m.id = msgs.rowid WHERE msgs MATCH ?1";
    if let Some(scope) = scope {
        let mut bounds =
            connection.prepare("SELECT MIN(id), MAX(id) FROM messages WHERE session_id = ?1")?;
        let mut statement = connection.prepare(&format!(
            "{base} AND msgs.rowid BETWEEN ?2 AND ?3 AND m.session_id = ?4{role}"
        ))?;
        for sid in scope {
            let (lo, hi): (Option<i64>, Option<i64>) =
                bounds.query_row([sid], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if let (Some(lo), Some(hi)) = (lo, hi) {
                let mut rows = statement.query(rusqlite::params![fts, lo, hi, sid])?;
                while let Some(row) = rows.next()? {
                    if !accept(row.get(0)?, row.get(1)?) {
                        return Ok(());
                    }
                }
            }
        }
    } else {
        let mut statement = connection.prepare(&format!("{base}{role}"))?;
        let mut rows = statement.query([fts])?;
        while let Some(row) = rows.next()? {
            if !accept(row.get(0)?, row.get(1)?) {
                break;
            }
        }
    }
    Ok(())
}
fn excerpt_value(
    message: &indexed_messages::IndexedMessage,
    offset: usize,
    budget: usize,
) -> Value {
    let excerpt = indexed_messages::excerpt(&message.text, offset, budget);
    let mut value = json!({"i": message.i, "role": message.role, "excerpt": excerpt.text, "excerpt_offset": excerpt.offset, "text_chars": excerpt.text_chars});
    if let Some(next) = excerpt.next_offset {
        value["next_offset"] = json!(next);
    }
    value
}
pub(super) fn execute(
    connection: &rusqlite::Connection,
    params: &SearchHistoryParams,
    caller_session_id: Option<&str>,
    visibility: &mut QueryVisibility<'_>,
) -> Result<Value> {
    let _snapshot = connection
        .is_autocommit()
        .then(|| connection.unchecked_transaction())
        .transpose()?;
    super::ensure_search_tokenizer(connection)?;
    let mut statement = connection.prepare("SELECT session_id, tool, path, project, title, started, ended, msg_count, kind, archived_at IS NOT NULL FROM files")?;
    let files: HashMap<String, FileMeta> = statement
        .query_map([], |r| {
            let row = SessionRow {
                session_id: r.get(0)?,
                tool: r.get(1)?,
                path: r.get(2)?,
                project: r.get(3)?,
                title: r.get(4)?,
                started: r.get(5)?,
                last_active: r.get(6)?,
                msg_count: r.get(7)?,
                kind: r.get(8)?,
                archived: r.get(9)?,
                preview: None,
                summary: None,
                tags: None,
                account: None,
            };
            Ok((row.session_id.clone(), FileMeta { row }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let parsed = search_query::parse_query(&params.query);
    if !parsed.ignored_terms.is_empty() {
        visibility.notice(format!(
            "ignored terms: {}",
            parsed.ignored_terms.join(", ")
        ));
    }
    if params.context_messages > 5 {
        visibility.notice(format!(
            "context_messages clamped from {} to 5",
            params.context_messages
        ));
    }
    let mut unresolved = Vec::new();
    let mut scope = Vec::new();
    let global = params.session_ids.is_empty();
    let mut seen_inputs = HashSet::new();
    for input in &params.session_ids {
        if !seen_inputs.insert(input) {
            continue;
        }
        let target = if input == "self" {
            caller_session_id
        } else {
            Some(input.as_str())
        };
        let lookup = match target {
            Some(target) => resolve_lookup(connection, target, visibility)?,
            None => SessionLookup::NotFound {
                input: input.clone(),
            },
        };
        let entry = match lookup {
            SessionLookup::Resolved(row) => {
                if !scope.contains(&row.session_id) {
                    scope.push(row.session_id);
                }
                continue;
            }
            SessionLookup::ChildNotIndexed {
                child_session_id,
                parent_session_id,
                report_dir,
                ..
            } => {
                let mut v = json!({"input": input, "status":"child_not_indexed", "child_session_id":child_session_id,"parent_session_id":parent_session_id});
                if let Some(dir) = report_dir {
                    v["report_dir"] = json!(dir);
                }
                v
            }
            SessionLookup::UnindexedLive { session_id, .. } => {
                json!({"input":input,"status":"unindexed_live","session_id":session_id})
            }
            SessionLookup::Ambiguous { candidates, .. } => {
                json!({"input":input,"status":"ambiguous","candidates":candidates.into_iter().map(|c| json!({"session_id":c.session_id,"title":c.title,"started":c.started,"ended":c.ended})).collect::<Vec<_>>()})
            }
            SessionLookup::NotFound { .. } => json!({"input":input,"status":"not_found"}),
        };
        unresolved.push(entry);
    }
    let Some(fts) = search_query::compile_fts_match(&parsed.groups) else {
        return Ok(finish(json!({"sessions":[]}), unresolved, visibility));
    };
    if !global && scope.is_empty() {
        return Ok(finish(json!({"sessions":[]}), unresolved, visibility));
    }
    let newest = match params.order.as_deref() {
        None | Some("newest") => true,
        Some("oldest") => false,
        _ => bail!("order must be newest or oldest"),
    };
    let order = if newest { "n" } else { "o" };
    let mut sorted_scope = scope.clone();
    sorted_scope.sort();
    let fingerprint = encode(&Sha256::digest(serde_json::to_vec(&(
        fts.as_str(),
        if global { None } else { Some(sorted_scope) },
        params.role.as_deref(),
        order,
        caller_session_id,
    ))?));
    let cursor = params
        .cursor
        .as_ref()
        .map(|c| -> Result<Value> {
            let c: Value = serde_json::from_slice(&decode(c)?)
                .map_err(|_| anyhow::anyhow!("malformed search_history cursor"))?;
            ensure!(
                c["v"] == 1
                    && c["s"].is_string()
                    && c["r"].is_string()
                    && (c["i"].is_null() || c["i"].as_u64().is_some()),
                "malformed search_history cursor"
            );
            ensure!(
                c["f"] == fingerprint && c["o"] == order,
                "search_history cursor query/scope/role/order/caller changed"
            );
            ensure!(
                global == c["i"].is_null(),
                "malformed search_history cursor mode"
            );
            // A scoped cursor freezes the remaining session order, starting
            // with its boundary session.
            if !global {
                let rem = c["rem"]
                    .as_array()
                    .filter(|rem| !rem.is_empty() && rem.len() <= 50)
                    .filter(|rem| rem.iter().all(Value::is_string))
                    .filter(|rem| rem[0] == c["s"]);
                ensure!(rem.is_some(), "malformed search_history cursor");
            }
            Ok(c)
        })
        .transpose()?;
    let scope_filter = (!global).then_some(scope.as_slice());
    let mut hits: HashMap<String, Vec<i64>> = HashMap::new();
    let mut omitted = 0usize;
    matching(
        connection,
        &fts,
        scope_filter,
        params.role.as_deref(),
        |id, sid| {
            if let Some(meta) = files.get(&sid)
                && !visibility.is_child_lenient(&meta.row)
            {
                if global && Some(sid.as_str()) == caller_session_id {
                    omitted += 1;
                } else {
                    hits.entry(sid).or_default().push(id);
                }
            }
            true
        },
    )?;
    let mut sessions: Vec<_> = hits.keys().cloned().collect();
    sessions.sort_by(|a, b| {
        let cmp = (files[a].order_key(newest), a).cmp(&(files[b].order_key(newest), b));
        if newest { cmp.reverse() } else { cmp }
    });
    if let Some(c) = &cursor {
        if global {
            let boundary = (c["r"].as_str().unwrap(), c["s"].as_str().unwrap());
            sessions.retain(|sid| {
                let key = (files[sid].order_key(newest), sid.as_str());
                if newest {
                    key < boundary
                } else {
                    key > boundary
                }
            });
        } else {
            // Resume in the order page one froze, so a live `ended` update on
            // any session cannot reorder, skip, or repeat one.
            sessions = c["rem"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|sid| sid.as_str())
                .filter(|sid| hits.contains_key(*sid))
                .map(str::to_owned)
                .collect();
        }
    }
    for ids in hits.values_mut() {
        ids.sort_unstable();
        if newest {
            ids.reverse();
        }
    }
    let max_chars = params.max_chars.clamp(1, 64_000);
    let min_usable = 200.min(max_chars);
    let limit = params
        .limit
        .clamp(1, 50)
        .min((max_chars / min_usable).max(1));
    let mut ids_by_session: HashMap<String, Vec<i64>> = HashMap::new();
    let mut selected: Vec<(String, Vec<i64>)> = Vec::new();
    let mut remaining = limit;
    let mut more = false;
    if global {
        for sid in sessions.iter().take(limit) {
            selected.push((sid.clone(), vec![hits[sid][0]]));
            remaining -= 1;
        }
        for round in 1..3 {
            for (sid, ids) in &mut selected {
                if remaining > 0
                    && let Some(id) = hits[sid].get(round)
                {
                    ids.push(*id);
                    remaining -= 1;
                }
            }
        }
        more = sessions.len() > selected.len();
    } else {
        for sid in &sessions {
            if remaining == 0 {
                more = true;
                break;
            }
            let all = indexed_messages::load_ids(connection, sid)?;
            let eligible: Vec<_> = hits[sid]
                .iter()
                .copied()
                .filter(|id| {
                    cursor.as_ref().is_none_or(|c| {
                        c["s"] != *sid || {
                            let Ok(i) = all.binary_search(id) else {
                                return false;
                            };
                            let boundary = c["i"].as_u64().unwrap() as usize;
                            if newest { i < boundary } else { i > boundary }
                        }
                    })
                })
                .collect();
            let take = eligible.len().min(remaining);
            remaining -= take;
            more |= take < eligible.len();
            if take > 0 {
                selected.push((sid.clone(), eligible[..take].to_vec()));
            }
            ids_by_session.insert(sid.clone(), all);
        }
    }
    let n: usize = selected.iter().map(|(_, ids)| ids.len()).sum();
    let ctx = params.context_messages.min(5);
    let hit_pool = if ctx == 0 {
        max_chars
    } else {
        (n * min_usable).max(max_chars * 70 / 100).min(max_chars)
    };
    let mut hit_budget = hit_pool;
    let mut left = n;
    let mut output = Vec::new();
    let mut last = None;
    let mut bodies: HashMap<String, HashMap<usize, indexed_messages::IndexedMessage>> =
        HashMap::new();
    for (sid, ids) in selected {
        if !ids_by_session.contains_key(&sid) {
            ids_by_session.insert(sid.clone(), indexed_messages::load_ids(connection, &sid)?);
        }
        let all = &ids_by_session[&sid];
        // Read only the hits and their context neighbors, never the whole session.
        let mut positions = Vec::with_capacity(ids.len());
        let mut wanted = Vec::new();
        for id in &ids {
            let Ok(i) = all.binary_search(id) else {
                bail!("a matching message left the index during the search; retry");
            };
            positions.push(i);
            let low = if ctx > 0 { i.saturating_sub(ctx) } else { i };
            let high = if ctx > 0 {
                (i + ctx).min(all.len() - 1)
            } else {
                i
            };
            wanted.extend((low..=high).map(|j| (all[j], j)));
        }
        wanted.sort_unstable_by_key(|(_, j)| *j);
        wanted.dedup();
        let messages = bodies
            .entry(sid.clone())
            .or_insert(indexed_messages::load_by_ids(connection, &wanted)?);
        let mut values = Vec::new();
        for (id, i) in ids.into_iter().zip(positions) {
            let Some(message) = messages.get(&i) else {
                bail!("a matching message left the index during the search; retry");
            };
            let budget = hit_budget / left;
            left -= 1;
            let highlighted: String = connection.query_row("SELECT highlight(msgs, 0, char(2), char(3)) FROM msgs WHERE msgs.rowid = ?1 AND msgs MATCH ?2", rusqlite::params![id,fts], |r| r.get(0))?;
            let redacted = sessionwiki::redact::redact(&highlighted);
            let mut reconstructed = String::new();
            let mut offset = None;
            let mut chars = 0usize;
            for c in redacted.chars() {
                match c {
                    '\u{2}' => {
                        if offset.is_none() {
                            offset = Some(chars);
                        }
                    }
                    '\u{3}' => {}
                    _ => {
                        reconstructed.push(c);
                        chars += 1;
                    }
                }
            }
            let start = if reconstructed == message.text {
                offset
                    .map(|p| {
                        p.saturating_sub(budget / 3)
                            .min(chars.saturating_sub(budget))
                    })
                    .unwrap_or(0)
            } else {
                0
            };
            let v = excerpt_value(message, start, budget);
            hit_budget -= v["excerpt"].as_str().unwrap().chars().count();
            values.push(v);
            last = Some((sid.clone(), i));
        }
        let row = &files[&sid].row;
        let mut v = json!({"session_id":sid,"title":row.title,"total_messages":all.len(),"hits_in_session":hits[&sid].len(),"hits":values});
        if let Some(t) = &row.started {
            v["started"] = json!(t);
        }
        if let Some(t) = &row.last_active {
            v["ended"] = json!(t);
        }
        output.push(v);
    }
    // Materialize every anchor before sharing the remaining space among all
    // neighbors. Short anchors and neighbors roll their unused space forward.
    if ctx > 0 {
        let hit_chars_used = hit_pool - hit_budget;
        let mut context_budget = max_chars - hit_chars_used;
        let mut slots_left: usize = output
            .iter()
            .map(|session| {
                let total = ids_by_session[session["session_id"].as_str().unwrap()].len();
                session["hits"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|hit| {
                        let i = hit["i"].as_u64().unwrap() as usize;
                        i.min(ctx) + ctx.min(total - i - 1)
                    })
                    .sum::<usize>()
            })
            .sum();
        for session in &mut output {
            let sid = session["session_id"].as_str().unwrap().to_owned();
            let total = ids_by_session[&sid].len();
            let messages = &bodies[&sid];
            for hit in session["hits"].as_array_mut().unwrap() {
                let i = hit["i"].as_u64().unwrap() as usize;
                for (name, range) in [
                    ("context_before", i.saturating_sub(ctx)..i),
                    ("context_after", (i + 1)..(i + ctx + 1).min(total)),
                ] {
                    let mut context = Vec::new();
                    for j in range {
                        if context_budget == 0 {
                            break;
                        }
                        let slot_budget = context_budget / slots_left;
                        slots_left -= 1;
                        if slot_budget == 0 {
                            continue;
                        }
                        let value = excerpt_value(&messages[&j], 0, slot_budget);
                        context_budget -= value["excerpt"].as_str().unwrap().chars().count();
                        context.push(value);
                    }
                    if !context.is_empty() {
                        hit[name] = json!(context);
                    }
                }
            }
        }
    }
    let mut result = json!({"sessions":output});
    if omitted > 0 {
        result["self_hits_omitted"] = json!(omitted);
    }
    if more && let Some((sid, i)) = last {
        let mut cursor = json!({"v":1,"o":order,"s":sid,"r":files[&sid].order_key(newest),"i":if global { None } else { Some(i) },"f":fingerprint});
        if !global {
            let at = sessions.iter().position(|s| *s == sid).unwrap_or(0);
            cursor["rem"] = json!(&sessions[at..]);
        }
        result["next_cursor"] = json!(encode(&serde_json::to_vec(&cursor)?));
    }
    if params.cursor.is_none()
        && output.is_empty()
        && (parsed.groups.len() >= 2
            || parsed
                .groups
                .iter()
                .any(|g| matches!(g.as_slice(), [QueryAtom::Term { prefix: false, .. }])))
    {
        let mut counts = Vec::new();
        let mut counted_groups = HashSet::new();
        for group in &parsed.groups {
            let term = search_query::compile_group_fts_match(group).unwrap();
            if !counted_groups.insert(term.clone()) {
                continue;
            }
            let mut count = |fts: &str| -> Result<usize> {
                let mut count = 0;
                matching(
                    connection,
                    fts,
                    scope_filter,
                    params.role.as_deref(),
                    |_, sid| {
                        if let Some(meta) = files.get(&sid)
                            && !(global && Some(sid.as_str()) == caller_session_id)
                            && !visibility.is_child_lenient(&meta.row)
                        {
                            count += 1;
                        }
                        count < 1001
                    },
                )?;
                Ok(count)
            };
            let n = count(&term)?;
            let label = group
                .iter()
                .map(|atom| match atom {
                    QueryAtom::Phrase(raw) => format!("\"{raw}\""),
                    QueryAtom::Term { raw, prefix: true } => format!("{raw}*"),
                    QueryAtom::Term { raw, prefix: false } => raw.clone(),
                })
                .collect::<Vec<_>>()
                .join(" OR ");
            let mut v = json!({"term":label,"hits":n.min(1000)});
            if n > 1000 {
                v["capped"] = json!(true);
            }
            if n == 0
                && let [QueryAtom::Term { raw, prefix: false }] = group.as_slice()
            {
                let prefix = search_query::compile_group_fts_match(&[QueryAtom::Term {
                    raw: raw.clone(),
                    prefix: true,
                }])
                .unwrap();
                let n = count(&prefix)?;
                if n > 0 {
                    v["prefix_hits"] = json!(n.min(1000));
                    if n > 1000 {
                        v["prefix_capped"] = json!(true);
                    }
                }
            }
            counts.push(v);
        }
        result["term_counts"] = json!(counts);
    }
    Ok(finish(result, unresolved, visibility))
}

#[cfg(test)]
mod tests {
    use super::super::{tags::testing, top_level::Snapshot};
    use super::*;
    fn add(db: &rusqlite::Connection, sid: &str, texts: &[String]) {
        testing::index_row(db, sid, super::super::TOOL);
        for text in texts {
            db.execute(
                "INSERT INTO messages(session_id,role,text) VALUES (?1,'user',?2)",
                rusqlite::params![sid, text],
            )
            .unwrap();
        }
        db.execute("INSERT INTO msgs(msgs) VALUES ('rebuild')", [])
            .unwrap();
    }
    fn run(db: &rusqlite::Connection, p: &SearchHistoryParams, caller: Option<&str>) -> Value {
        execute(
            db,
            p,
            caller,
            &mut QueryVisibility::new(&Snapshot::default(), &crate::import::NativeScanCache::new()),
        )
        .unwrap()
    }
    fn size(v: &Value) -> usize {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|s| s["hits"].as_array().unwrap())
            .map(|h| {
                h["excerpt"].as_str().unwrap().chars().count()
                    + ["context_before", "context_after"]
                        .iter()
                        .map(|k| {
                            h[*k].as_array().map_or(0, |v| {
                                v.iter()
                                    .map(|c| c["excerpt"].as_str().unwrap().chars().count())
                                    .sum::<usize>()
                            })
                        })
                        .sum::<usize>()
            })
            .sum()
    }
    #[test]
    fn search_history_global_breadth_and_scoped_cursors() {
        let _held = testing::lock();
        let (_dir, db) = testing::isolated_index();
        for i in 0..15 {
            add(
                &db,
                &format!("session-{i:02}"),
                &vec!["quokka passage".into(); 5],
            );
        }
        let mut p = SearchHistoryParams {
            query: "quokka".into(),
            limit: 10,
            ..Default::default()
        };
        let first = run(&db, &p, None);
        assert_eq!(first["sessions"].as_array().unwrap().len(), 10);
        assert!(
            first["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["hits"].as_array().unwrap().len() == 1)
        );
        p.cursor = Some(first["next_cursor"].as_str().unwrap().into());
        let second = run(&db, &p, None);
        assert_eq!(second["sessions"].as_array().unwrap().len(), 5);
        assert!(second.get("next_cursor").is_none());
        assert!(second["sessions"].as_array().unwrap().iter().all(|s| {
            !first["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["session_id"] == s["session_id"])
        }));
        p.cursor = None;
        p.session_ids = vec!["session-01".into()];
        p.limit = 2;
        let first = run(&db, &p, None);
        assert_eq!(first["sessions"][0]["hits_in_session"], 5);
        assert_eq!(first["sessions"][0]["hits"][0]["i"], 4);
        p.cursor = Some(first["next_cursor"].as_str().unwrap().into());
        let second = run(&db, &p, None);
        assert_eq!(second["sessions"][0]["hits"][0]["i"], 2);
        for change in 0..4 {
            let mut q = SearchHistoryParams {
                query: p.query.clone(),
                session_ids: p.session_ids.clone(),
                cursor: p.cursor.clone(),
                ..Default::default()
            };
            match change {
                0 => q.query = "passage".into(),
                1 => q.role = Some("user".into()),
                2 => q.session_ids = vec!["session-02".into()],
                _ => {}
            }
            let caller = if change == 3 { Some("caller") } else { None };
            assert!(
                execute(
                    &db,
                    &q,
                    caller,
                    &mut QueryVisibility::new(
                        &Snapshot::default(),
                        &crate::import::NativeScanCache::new()
                    )
                )
                .is_err()
            );
        }
        db.execute("DELETE FROM messages WHERE session_id='session-01' AND id NOT IN (SELECT MIN(id) FROM messages WHERE session_id='session-01')",[]).unwrap();
        db.execute("INSERT INTO msgs(msgs) VALUES ('rebuild')", [])
            .unwrap();
        let shrunk = run(&db, &p, None);
        assert_eq!(shrunk["sessions"][0]["hits"][0]["i"], 0);
    }
    fn stamp(db: &rusqlite::Connection, sid: &str, started: &str, ended: &str) {
        db.execute(
            "UPDATE files SET started = ?2, ended = ?3 WHERE session_id = ?1",
            rusqlite::params![sid, started, ended],
        )
        .unwrap();
    }
    fn session_ids(page: &Value) -> Vec<String> {
        page["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["session_id"].as_str().unwrap().to_owned())
            .collect()
    }
    #[test]
    fn scoped_cursor_keeps_its_frozen_session_order_when_the_boundary_session_advances() {
        let _held = testing::lock();
        let (_dir, db) = testing::isolated_index();
        add(&db, "newer", &vec!["quokka passage".into(); 3]);
        add(&db, "older", &vec!["quokka passage".into(); 3]);
        stamp(&db, "newer", "2026-02-01", "2026-02-02");
        stamp(&db, "older", "2026-01-01", "2026-01-02");
        let mut p = SearchHistoryParams {
            query: "quokka".into(),
            session_ids: vec!["newer".into(), "older".into()],
            limit: 2,
            ..Default::default()
        };
        let first = run(&db, &p, None);
        assert_eq!(session_ids(&first), ["newer"]);
        assert_eq!(first["sessions"][0]["hits"].as_array().unwrap().len(), 2);
        // A live append moves the boundary session's `ended` forward.
        stamp(&db, "newer", "2026-02-01", "2026-11-24");
        p.cursor = Some(first["next_cursor"].as_str().unwrap().into());
        p.limit = 10;
        let second = run(&db, &p, None);
        assert_eq!(session_ids(&second), ["newer", "older"]);
        assert_eq!(second["sessions"][0]["hits"].as_array().unwrap().len(), 1);
        assert_eq!(second["sessions"][0]["hits"][0]["i"], 0);
        assert_eq!(second["sessions"][1]["hits"].as_array().unwrap().len(), 3);
        assert!(second.get("next_cursor").is_none());
    }
    #[test]
    fn global_oldest_first_cursor_ignores_live_ended_updates_to_returned_sessions() {
        let _held = testing::lock();
        let (_dir, db) = testing::isolated_index();
        for (sid, month) in [("x", "01"), ("y", "02"), ("z", "03")] {
            add(&db, sid, &["quokka passage".into()]);
            stamp(
                &db,
                sid,
                &format!("2026-{month}-01"),
                &format!("2026-{month}-02"),
            );
        }
        let mut p = SearchHistoryParams {
            query: "quokka".into(),
            order: Some("oldest".into()),
            limit: 1,
            ..Default::default()
        };
        let first = run(&db, &p, None);
        assert_eq!(session_ids(&first), ["x"]);
        stamp(&db, "x", "2026-01-01", "2026-11-24");
        p.cursor = Some(first["next_cursor"].as_str().unwrap().into());
        let second = run(&db, &p, None);
        assert_eq!(session_ids(&second), ["y"]);
        p.cursor = Some(second["next_cursor"].as_str().unwrap().into());
        let third = run(&db, &p, None);
        assert_eq!(session_ids(&third), ["z"]);
    }
    #[test]
    fn search_history_coordinates_budget_context_and_caller() {
        let _held = testing::lock();
        let (_dir, db) = testing::isolated_index();
        let long = format!("{} quokka {}", "é🙂 ".repeat(2000), "tail ".repeat(1000));
        for sid in ["a", "b", "c", "caller"] {
            add(
                &db,
                sid,
                &[
                    "large neighbor ".repeat(2000),
                    long.clone(),
                    "neighbor".into(),
                    long.clone(),
                    long.clone(),
                ],
            );
        }
        let mut p = SearchHistoryParams {
            query: "quokka".into(),
            limit: 10,
            max_chars: 250,
            ..Default::default()
        };
        let v = run(&db, &p, Some("caller"));
        assert_eq!(v["self_hits_omitted"], 3);
        assert_eq!(v["sessions"].as_array().unwrap().len(), 1);
        assert!(v["next_cursor"].is_string());
        assert!(size(&v) <= 250);
        let hit = &v["sessions"][0]["hits"][0];
        let messages =
            indexed_messages::load(&db, v["sessions"][0]["session_id"].as_str().unwrap()).unwrap();
        let expected = indexed_messages::excerpt(
            &messages[hit["i"].as_u64().unwrap() as usize].text,
            hit["excerpt_offset"].as_u64().unwrap() as usize,
            250,
        );
        assert_eq!(hit["excerpt"], expected.text);
        assert!(hit.get("truncated").is_none());
        assert!(hit["excerpt_offset"].as_u64().unwrap() > 0);
        p.max_chars = 16000;
        let v = run(&db, &p, Some("caller"));
        assert_eq!(v["sessions"].as_array().unwrap().len(), 3);
        assert!(
            v["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["hits"].as_array().unwrap().len() == 3)
        );
        p.limit = 2;
        p.context_messages = 200;
        p.session_ids = vec!["a".into()];
        let v = run(&db, &p, None);
        assert_eq!(v["sessions"][0]["hits"].as_array().unwrap().len(), 2);
        assert!(
            v["notices"][0]
                .as_str()
                .unwrap()
                .contains("clamped from 200 to 5")
        );
        assert!(size(&v) <= 16000);
        assert!(v["sessions"][0]["hits"][0]["context_before"].is_array());
        assert!(
            v["sessions"][0]["hits"][1]["context_before"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| !c["excerpt"].as_str().unwrap().is_empty())
        );
        add(
            &db,
            "short",
            &[
                "neighbor ".repeat(3000),
                "quokka".into(),
                "quokka".into(),
                "neighbor ".repeat(3000),
            ],
        );
        p.session_ids = vec!["short".into()];
        let v = run(&db, &p, None);
        assert!(size(&v) <= p.max_chars);
        // Both short anchors leave most of the hit pool available to context.
        assert!(size(&v) > p.max_chars * 30 / 100 + 12);
        assert!(
            v["sessions"][0]["hits"][1]["context_after"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| !c["excerpt"].as_str().unwrap().is_empty())
        );
    }
    #[test]
    fn search_history_diagnostics_exclude_caller_and_cap_prefix_probes() {
        let _held = testing::lock();
        let (_dir, db) = testing::isolated_index();
        add(&db, "caller", &["privateword".into()]);
        add(&db, "other", &vec!["common a25b06ab12345678".into(); 1002]);
        let p = SearchHistoryParams {
            query: "privateword common".into(),
            ..Default::default()
        };
        let v = run(&db, &p, Some("caller"));
        assert_eq!(v["term_counts"][0]["hits"], 0);
        assert_eq!(v["term_counts"][1]["hits"], 1000);
        assert_eq!(v["term_counts"][1]["capped"], true);
        let p = SearchHistoryParams {
            query: "a25b06ab".into(),
            ..Default::default()
        };
        let v = run(&db, &p, None);
        assert_eq!(v["term_counts"][0]["hits"], 0);
        assert_eq!(v["term_counts"][0]["prefix_hits"], 1000);
        assert_eq!(v["term_counts"][0]["prefix_capped"], true);
        let p = SearchHistoryParams {
            query: "missing missing \"quoted phrase\" star* foo OR bar*".into(),
            ..Default::default()
        };
        let v = run(&db, &p, None);
        let labels: Vec<_> = v["term_counts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["term"].as_str().unwrap())
            .collect();
        assert_eq!(
            labels,
            ["missing", "\"quoted phrase\"", "star*", "foo OR bar*"]
        );
    }
    #[test]
    fn search_history_read_session_roundtrip_and_redaction_fallback() {
        use mj_core::history::{HistoryQuery, HistoryRequest};
        let _held = testing::lock();
        let (_dir, db) = testing::isolated_index();
        let token = format!("ghp_{}", "a".repeat(36));
        add(
            &db,
            "roundtrip",
            &[format!(
                "{} quokka {} tail {}",
                "é🙂 ".repeat(2000),
                token,
                "suffix ".repeat(1000)
            )],
        );
        // Highlighting part of a secret changes redaction's token boundary;
        // alignment must fall back to the beginning of the read_session text.
        let p = SearchHistoryParams {
            query: "ghp*".into(),
            max_chars: 250,
            ..Default::default()
        };
        let v = run(&db, &p, None);
        assert_eq!(v["sessions"][0]["hits"][0]["excerpt_offset"], 0);
        assert!(!v.to_string().contains(&token));
        let p = SearchHistoryParams {
            query: "quokka".into(),
            max_chars: 250,
            ..Default::default()
        };
        let v = run(&db, &p, None);
        let hit = &v["sessions"][0]["hits"][0];
        let ownership = Snapshot::default();
        let cache = crate::import::NativeScanCache::new();
        for offset in [
            hit["excerpt_offset"].as_u64().unwrap(),
            hit["next_offset"].as_u64().unwrap(),
        ] {
            let request = HistoryRequest {
                request_id: "roundtrip".into(),
                query: HistoryQuery::ReadSession {
                    session_id: "roundtrip".into(),
                    start: 0,
                    offset: offset as usize,
                    role: None,
                    limit: 1,
                    max_chars: 250,
                },
            };
            let read =
                super::super::history::query_in_with(&db, &request, &ownership, &cache).unwrap();
            let messages = indexed_messages::load(&db, "roundtrip").unwrap();
            let expected = indexed_messages::excerpt(&messages[0].text, offset as usize, 250);
            assert_eq!(read["messages"][0]["text"], expected.text);
            if offset == hit["excerpt_offset"].as_u64().unwrap() {
                assert_eq!(read["messages"][0]["text"], hit["excerpt"]);
            }
        }
        let mut global = Vec::new();
        matching(&db, "quokka", None, None, |id, sid| {
            if sid == "roundtrip" {
                global.push(id);
            }
            true
        })
        .unwrap();
        let mut scoped = Vec::new();
        matching(&db, "quokka", Some(&["roundtrip".into()]), None, |id, _| {
            scoped.push(id);
            true
        })
        .unwrap();
        assert_eq!(global, scoped);
    }

    #[test]
    fn search_history_mixed_scope_and_tool_roles() {
        let _held = testing::lock();
        let (_dir, db) = testing::isolated_index();
        add(&db, "parent", &["quokka".into()]);
        add(&db, "caller", &["quokka".into()]);
        db.execute(
            "UPDATE messages SET role='custom' WHERE session_id='caller'",
            [],
        )
        .unwrap();
        let ownership = Snapshot::for_test_child("child", "parent");
        let cache = crate::import::NativeScanCache::new();
        let mut p = SearchHistoryParams {
            query: "quokka".into(),
            session_ids: vec!["child".into(), "parent".into(), "self".into()],
            ..Default::default()
        };
        let v = execute(
            &db,
            &p,
            Some("caller"),
            &mut QueryVisibility::new(&ownership, &cache),
        )
        .unwrap();
        assert_eq!(v["sessions"].as_array().unwrap().len(), 2);
        assert_eq!(v["unresolved_sessions"][0]["status"], "child_not_indexed");
        assert_eq!(v["unresolved_sessions"][0]["parent_session_id"], "parent");
        p.role = Some("tool".into());
        let v = execute(
            &db,
            &p,
            Some("caller"),
            &mut QueryVisibility::new(&ownership, &cache),
        )
        .unwrap();
        assert_eq!(v["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(v["sessions"][0]["hits"][0]["role"], "tool");
    }
}
