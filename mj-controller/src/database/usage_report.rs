//! One consistent store snapshot of parent usage and retained descendants.
use super::*;
use mj_core::usage::{
    ModelUsageEstimate, SessionUsageEstimate, SessionUsageQuery, SessionUsageReport, UsageScope,
};

mod pricing;

pub fn load_session_usage_report(query: &SessionUsageQuery) -> Result<SessionUsageReport> {
    load_from(
        &database_path(),
        query,
        chrono::Utc::now().timestamp_millis(),
    )
}

fn load_from(path: &Path, query: &SessionUsageQuery, now_ms: i64) -> Result<SessionUsageReport> {
    let mut connection = open_reader(path)?;
    let tx = connection.transaction()?;
    let since_ms = now_ms.saturating_sub(query.period.duration_ms());
    let mut report = SessionUsageReport {
        since_ms,
        until_ms: now_ms,
        ..Default::default()
    };
    let mut roots = tx.prepare("SELECT session_id, COALESCE(session_title_override, acp_session_title, title), last_profile, state
        FROM sessions s WHERE NOT EXISTS(SELECT 1 FROM subagent_accounting a WHERE a.child_session_id=s.session_id) ORDER BY session_id")?;
    let roots = roots
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    report.profiles = roots
        .iter()
        .map(|(_, _, profile, _)| profile.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for (session_id, title, profile, state) in roots {
        let suspended = !SessionState::from_stored(&state)
            .context("unknown usage session state")?
            .is_active();
        if (suspended && !query.include_suspended)
            || query
                .profile
                .as_ref()
                .is_some_and(|wanted| wanted != &profile)
        {
            continue;
        }
        let mut tree = tx.prepare("WITH RECURSIVE tree(id) AS (
            SELECT ?1 UNION SELECT a.child_session_id FROM subagent_accounting a JOIN tree t ON a.parent_session_id=t.id)
            SELECT id FROM tree ORDER BY id")?;
        let members = tree
            .query_map([&session_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut estimate = SessionUsageEstimate {
            session_id,
            title,
            profile,
            suspended,
            subagent_count: members.len().saturating_sub(1),
            ..Default::default()
        };
        let mut groups: BTreeMap<Option<String>, ModelUsageEstimate> = BTreeMap::new();
        for member in members {
            let mut turns = tx.prepare(
                "SELECT u.body, s.model, s.effort FROM session_turn_usage u
                LEFT JOIN session_turn_selections s USING(session_id, command_id)
                WHERE u.session_id=?1 AND json_extract(u.body, '$.completed_at_ms') >= ?2
                AND json_extract(u.body, '$.completed_at_ms') <= ?3 ORDER BY u.completed_ordinal",
            )?;
            let rows = turns.query_map(params![member, since_ms, now_ms], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    UsageSelection {
                        model: row.get(1)?,
                        effort: row.get(2)?,
                    },
                ))
            })?;
            for row in rows {
                let (body, selection) = row?;
                let turn = deserialize_turn_outcome(&body)?;
                estimate.coverage.recorded_turns += 1;
                match turn.usage {
                    Some(usage) if usage.scope == UsageScope::Turn => {
                        estimate.coverage.full_turn_reports += 1;
                        let mut attributed = BTreeMap::new();
                        super::usage::attribute(&usage, selection, &mut attributed)?;
                        for (selection, totals) in attributed {
                            // An unattributed cache counter is unknown, not zero.
                            // Pricing the input as uncached would overstate its cost.
                            let caches_known = (usage.cached_read_tokens.is_none()
                                || totals.contains_key("cached_read_tokens"))
                                && (usage.cached_write_tokens.is_none()
                                    || totals.contains_key("cached_write_tokens"));
                            let cost = if caches_known {
                                pricing::estimate(selection.model.as_deref(), &totals)?
                            } else {
                                None
                            };
                            let group =
                                groups.entry(selection.model.clone()).or_insert_with(|| {
                                    ModelUsageEstimate {
                                        model: selection.model,
                                        cost_usd_ticks: Some(0),
                                        ..Default::default()
                                    }
                                });
                            group.cost_usd_ticks = match (group.cost_usd_ticks, cost) {
                                (Some(a), Some(b)) => {
                                    Some(a.checked_add(b).context("model cost overflow")?)
                                }
                                _ => None,
                            };
                            super::usage::merge_totals(&mut group.totals, &totals)?;
                        }
                    }
                    Some(usage) if usage.scope == UsageScope::LastRequest => {
                        estimate.coverage.last_request_reports += 1
                    }
                    Some(_) => estimate.coverage.unspecified_reports += 1,
                    None => estimate.coverage.missing_reports += 1,
                }
            }
            // A turn already running at the start of the window can still consume in it.
            let unfinished: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM materialized_sessions WHERE session_id=?1
                AND active_turn_json IS NOT NULL AND json_extract(active_turn_json, '$.started_at_ms') <= ?2)", params![member, now_ms], |row| row.get(0))?;
            estimate.coverage.unfinished_turns += u64::from(unfinished);
        }
        estimate.models = groups.into_values().collect();
        estimate.models.sort_by(|a, b| {
            b.cost_usd_ticks
                .cmp(&a.cost_usd_ticks)
                .then_with(|| a.model.cmp(&b.model))
        });
        estimate.cost_usd_ticks = estimate
            .models
            .iter()
            .filter_map(|model| model.cost_usd_ticks)
            .try_fold(0u64, |total, cost| {
                total.checked_add(cost).context("session cost overflow")
            })?;
        report.cost_usd_ticks = report
            .cost_usd_ticks
            .checked_add(estimate.cost_usd_ticks)
            .context("report cost overflow")?;
        report.sessions.push(estimate);
    }
    report.sessions.sort_by(|a, b| {
        b.cost_usd_ticks
            .cmp(&a.cost_usd_ticks)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::usage::UsagePeriod;

    // Store/schema integration: retained descendants, inclusive time boundaries,
    // model switches and incomplete reports must agree with the report reader.
    #[test]
    fn usage_report_preserves_retained_trees_and_filters_completed_usage() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("usage-report.sqlite");
        let now = 31 * 86_400_000;
        let mut state = State::default();
        for id in ["root", "child", "grandchild", "suspended"] {
            let mut session = super::super::tests::session(id, "project");
            session.last_profile = if id == "suspended" { "claude" } else { "codex" }.into();
            session.state = if id == "suspended" {
                SessionState::Stopped
            } else {
                SessionState::Running
            };
            state.sessions.insert(id.into(), session);
        }
        save_state_to(&path, &state)?;
        let connection = open(&path)?;
        connection.execute_batch("INSERT INTO subagent_accounting VALUES ('child','root','child'),('grandchild','child','grandchild')")?;
        let insert = |session: &str,
                      seq: u64,
                      timestamp: i64,
                      model: &str,
                      usage: serde_json::Value|
         -> Result<()> {
            let command = format!("command-{seq}");
            let body = serde_json::json!({"command_id":command,"completed_ordinal":seq,"completed_at_ms":timestamp,
                "outcome":{"kind":"completed","stop_reason":"EndTurn"},"usage":usage});
            let turn: MaterializedTurnOutcome = serde_json::from_value(body)?;
            connection.execute("INSERT INTO session_turn_usage(session_id,command_id,completed_ordinal,body) VALUES (?1,?2,?3,?4)", params![session, command, seq, serialize_turn_outcome(&turn)?])?;
            connection.execute(
                "INSERT INTO session_turn_selections VALUES (?1,?2,?3,'high')",
                params![session, command, model],
            )?;
            Ok(())
        };
        let inclusive = serde_json::json!({"scope":"turn","total_tokens":1_100_000,"input_tokens":1_000_000,"output_tokens":100_000,"cached_read_tokens":500_000,"cached_write_tokens":0});
        insert(
            "root",
            1,
            now - 3_600_000,
            "global.openai.gpt-6-luna",
            inclusive.clone(),
        )?;
        insert("root", 2, now - 3_600_001, "gpt-6-sol", inclusive.clone())?;
        insert("root", 3, now + 1, "gpt-6-astra", inclusive.clone())?;
        insert(
            "child",
            1,
            now,
            "claude-opus-5-5-20260922",
            serde_json::json!({"scope":"turn","total_tokens":1_800_000,"input_tokens":1_000_000,"output_tokens":100_000,"cached_read_tokens":500_000,"cached_write_tokens":200_000}),
        )?;
        insert("grandchild", 1, now, "custom-model", inclusive.clone())?;
        let mut missing_cache_breakdown = inclusive.clone();
        missing_cache_breakdown["provider_details"] = serde_json::json!({"model_usage":{"gpt-6.1-sol":{
            "scope":"turn","total_tokens":1_100_000,"input_tokens":1_000_000,"output_tokens":100_000
        }}});
        insert(
            "grandchild",
            2,
            now,
            "custom-model",
            missing_cache_breakdown,
        )?;
        insert(
            "root",
            4,
            now,
            "gpt-6-astra",
            serde_json::json!({"scope":"last_request","total_tokens":1_100_000,"input_tokens":1_000_000,"output_tokens":100_000}),
        )?;
        insert("root", 5, now, "gpt-6-astra", serde_json::Value::Null)?;
        insert(
            "suspended",
            1,
            now - 2 * 86_400_000,
            "gpt-6-astra",
            inclusive.clone(),
        )?;
        insert(
            "suspended",
            2,
            now - 8 * 86_400_000,
            "gpt-6-sol",
            inclusive.clone(),
        )?;
        insert(
            "suspended",
            3,
            now - 31 * 86_400_000,
            "gpt-6-astra",
            inclusive,
        )?;
        drop(connection);
        delete_session_from(&path, "grandchild")?;
        delete_session_from(&path, "child")?;

        let hourly = load_from(&path, &SessionUsageQuery::default(), now)?;
        assert_eq!(hourly.sessions.len(), 1);
        let root = &hourly.sessions[0];
        assert_eq!(root.subagent_count, 2);
        assert_eq!(root.cost_usd_ticks, 72_050_000_000); // $7.10 + $0.105, with caches counted once.
        assert_eq!(root.models.len(), 5);
        assert!(
            root.models
                .iter()
                .find(|m| m.model.as_deref() == Some("gpt-6.1-sol"))
                .unwrap()
                .cost_usd_ticks
                .is_none()
        );
        assert!(root.models.last().unwrap().cost_usd_ticks.is_none());
        assert_eq!(root.coverage.full_turn_reports, 4);
        assert_eq!(root.coverage.last_request_reports, 1);
        assert_eq!(root.coverage.missing_reports, 1);
        assert!(root.is_partial());
        assert_eq!(hourly.profiles, ["claude", "codex"]);
        assert_eq!(hourly.cost_usd_ticks, root.cost_usd_ticks);

        let daily = load_from(
            &path,
            &SessionUsageQuery {
                period: UsagePeriod::Daily,
                include_suspended: true,
                ..Default::default()
            },
            now,
        )?;
        assert_eq!(daily.since_ms, now - 86_400_000);
        assert_eq!(daily.sessions.len(), 2);
        assert_eq!(daily.sessions[0].session_id, "root");
        assert_eq!(daily.cost_usd_ticks, 93_050_000_000);
        assert_eq!(daily.sessions[1].session_id, "suspended");
        assert_eq!(daily.sessions[1].coverage.recorded_turns, 0);

        let weekly_query = SessionUsageQuery {
            period: UsagePeriod::Weekly,
            include_suspended: true,
            profile: None,
        };
        let weekly = load_from(&path, &weekly_query, now)?;
        assert_eq!(weekly.sessions.len(), 2);
        // Cache-discounted Astra: $5 + $0.50 + $5 = $10.50, greater than root.
        assert_eq!(weekly.sessions[0].session_id, "suspended");
        assert_eq!(weekly.sessions[0].cost_usd_ticks, 105_000_000_000);
        assert!(weekly.sessions[0].suspended);
        assert_eq!(weekly.sessions[1].cost_usd_ticks, 93_050_000_000);
        assert_eq!(weekly.cost_usd_ticks, 198_050_000_000);
        let filtered = load_from(
            &path,
            &SessionUsageQuery {
                profile: Some("codex".into()),
                ..weekly_query.clone()
            },
            now,
        )?;
        assert_eq!(filtered.sessions, [weekly.sessions[1].clone()]);
        let monthly = load_from(
            &path,
            &SessionUsageQuery {
                period: UsagePeriod::Monthly,
                profile: Some("claude".into()),
                ..weekly_query
            },
            now,
        )?;
        assert_eq!(monthly.sessions[0].cost_usd_ticks, 126_000_000_000);
        assert_eq!(monthly.sessions[0].coverage.recorded_turns, 2);
        Ok(())
    }
}
