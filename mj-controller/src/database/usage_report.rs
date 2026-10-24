//! One consistent store snapshot of parent usage and retained descendants.
use super::*;
use mj_core::usage::{
    ModelUsageEstimate, SessionUsageEstimate, SessionUsageQuery, SessionUsageReport, UnknownCost,
    UsageScope, add_cost, descending_cost,
};

mod model_catalog;
mod pricing;

/// `models` is each profile's newest advertised model list, used to price
/// model aliases.
pub fn load_session_usage_report(
    query: &SessionUsageQuery,
    models: BTreeMap<String, Vec<mj_core::acp::SessionConfigChoice>>,
) -> Result<SessionUsageReport> {
    load_with(
        &database_path(),
        query,
        chrono::Utc::now().timestamp_millis(),
        &model_catalog::ModelCatalogs(models),
    )
}

#[cfg(test)]
fn load_from(path: &Path, query: &SessionUsageQuery, now_ms: i64) -> Result<SessionUsageReport> {
    load_with(
        path,
        query,
        now_ms,
        &model_catalog::ModelCatalogs::default(),
    )
}

fn load_with(
    path: &Path,
    query: &SessionUsageQuery,
    now_ms: i64,
    catalogs: &model_catalog::ModelCatalogs,
) -> Result<SessionUsageReport> {
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
    let mut profiles = roots
        .iter()
        .map(|(_, _, profile, _)| profile.clone())
        .collect::<BTreeSet<_>>();
    let mut turn_profiles = tx.prepare(
        "SELECT DISTINCT profile FROM session_turn_selections WHERE profile IS NOT NULL",
    )?;
    for profile in turn_profiles.query_map([], |row| row.get::<_, String>(0))? {
        profiles.insert(profile?);
    }
    report.profiles = profiles.into_iter().collect();
    let wanted = query.profile.as_deref();
    for (session_id, title, profile, state) in roots {
        let suspended = !SessionState::from_stored(&state)
            .context("unknown usage session state")?
            .is_active();
        if suspended && !query.include_suspended {
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
                "SELECT u.body, s.model, s.effort, s.profile FROM session_turn_usage u
                LEFT JOIN session_turn_selections s USING(session_id, command_id)
                WHERE u.session_id=?1 AND json_extract(u.body, '$.completed_at_ms') >= ?2
                AND json_extract(u.body, '$.completed_at_ms') <= ?3
                AND (?4 IS NULL OR s.profile=?4) ORDER BY u.completed_ordinal",
            )?;
            let rows = turns.query_map(params![member, since_ms, now_ms, wanted], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    UsageSelection {
                        model: row.get(1)?,
                        effort: row.get(2)?,
                    },
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            for row in rows {
                let (body, selection, turn_profile) = row?;
                let turn = deserialize_turn_outcome(&body)?;
                estimate.coverage.recorded_turns += 1;
                match turn.usage {
                    Some(usage) if usage.scope == UsageScope::Turn => {
                        estimate.coverage.full_turn_reports += 1;
                        let mut attributed = BTreeMap::new();
                        super::usage::attribute(&usage, selection, &mut attributed)?;
                        for (mut selection, totals) in attributed {
                            selection.model = selection
                                .model
                                .map(|model| catalogs.priced_model(turn_profile.as_deref(), model));
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
                                        ..Default::default()
                                    }
                                });
                            group.cost_usd_ticks =
                                add_cost(group.cost_usd_ticks, cost.ok_or(UnknownCost))
                                    .context("model cost overflow")?;
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
            let unfinished: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM materialized_sessions m
                LEFT JOIN session_turn_selections s ON s.session_id=m.session_id
                    AND s.command_id=json_extract(m.active_turn_json, '$.command_id')
                WHERE m.session_id=?1 AND m.active_turn_json IS NOT NULL
                AND json_extract(m.active_turn_json, '$.started_at_ms') <= ?2
                AND (?3 IS NULL OR s.profile=?3))",
                params![member, now_ms, wanted],
                |row| row.get(0),
            )?;
            estimate.coverage.unfinished_turns += u64::from(unfinished);
        }
        // A profile selects its own parents and any parent whose tree ran a
        // turn on it in the period; only that profile's turns are counted.
        if wanted.is_some_and(|wanted| wanted != estimate.profile)
            && estimate.coverage.recorded_turns == 0
            && estimate.coverage.unfinished_turns == 0
        {
            continue;
        }
        estimate.models = groups.into_values().collect();
        estimate.models.sort_by(|a, b| {
            descending_cost(&a.cost_usd_ticks, &b.cost_usd_ticks)
                .then_with(|| a.model.cmp(&b.model))
        });
        estimate.cost_usd_ticks = estimate
            .models
            .iter()
            .try_fold(Ok(0), |total, model| add_cost(total, model.cost_usd_ticks))
            .context("session cost overflow")?;
        report.cost_usd_ticks = add_cost(report.cost_usd_ticks, estimate.cost_usd_ticks)
            .context("report cost overflow")?;
        report.sessions.push(estimate);
    }
    report.sessions.sort_by(|a, b| {
        descending_cost(&a.cost_usd_ticks, &b.cost_usd_ticks)
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
            session.last_profile = match id {
                "suspended" => "claude",
                "child" => "codex2",
                _ => "codex",
            }
            .into();
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
            let configuration =
                serde_json::from_value(serde_json::json!({"model":model,"effort":"high"}))?;
            super::super::usage::record_turn_selection(
                &connection,
                session,
                &command,
                &configuration,
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
        // A restart switches the grandchild's profile between its turns.
        connection.execute(
            "UPDATE sessions SET last_profile='codex2' WHERE session_id='grandchild'",
            [],
        )?;
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
        let running: mj_core::state::SessionConfiguration =
            serde_json::from_value(serde_json::json!({"model":"gpt-6-astra"}))?;
        super::super::usage::record_turn_selection(&connection, "root", "running", &running)?;
        let active = serde_json::json!({"command_id":"running","turn_start_position":9,"started_at_ms":now - 10});
        assert_eq!(
            connection.execute(
                "UPDATE materialized_sessions SET active_turn_json=?1 WHERE session_id='root'",
                [active.to_string()],
            )?,
            1
        );
        drop(connection);
        delete_session_from(&path, "grandchild")?;
        delete_session_from(&path, "child")?;

        // The known part of a cost that the unpriced models make unknown.
        let priced = |session: &SessionUsageEstimate| -> u64 {
            session
                .models
                .iter()
                .filter_map(|model| model.cost_usd_ticks.ok())
                .sum()
        };
        let hourly = load_from(&path, &SessionUsageQuery::default(), now)?;
        assert_eq!(hourly.sessions.len(), 1);
        let root = &hourly.sessions[0];
        assert_eq!(root.subagent_count, 2);
        // Custom-model has no rate, and gpt-6.1-sol's cache counters are unknown.
        assert_eq!(root.cost_usd_ticks, Err(UnknownCost));
        assert_eq!(priced(root), 72_050_000_000); // $7.10 + $0.105, with caches counted once.
        assert_eq!(root.models.len(), 5);
        assert!(
            root.models
                .iter()
                .find(|m| m.model.as_deref() == Some("gpt-6.1-sol"))
                .unwrap()
                .cost_usd_ticks
                .is_err()
        );
        assert!(root.models.last().unwrap().cost_usd_ticks.is_err());
        assert_eq!(root.coverage.full_turn_reports, 4);
        assert_eq!(root.coverage.last_request_reports, 1);
        assert_eq!(root.coverage.missing_reports, 1);
        assert!(root.is_partial());
        assert_eq!(root.coverage.unfinished_turns, 1);
        assert_eq!(hourly.profiles, ["claude", "codex", "codex2"]);
        assert_eq!(hourly.cost_usd_ticks, Err(UnknownCost));

        // A profile counts the turns recorded on it anywhere in the tree, by
        // the profile at turn start, after the children are destroyed.
        let on = |profile: &str, period| {
            load_from(
                &path,
                &SessionUsageQuery {
                    profile: Some(profile.into()),
                    period,
                    include_suspended: true,
                },
                now,
            )
        };
        let codex2 = on("codex2", UsagePeriod::Hourly)?;
        assert_eq!(codex2.sessions.len(), 1);
        let tree = &codex2.sessions[0];
        assert_eq!(
            (tree.session_id.as_str(), tree.profile.as_str()),
            ("root", "codex")
        );
        assert_eq!(tree.subagent_count, 2);
        assert_eq!(tree.cost_usd_ticks, Err(UnknownCost));
        assert_eq!(priced(tree), 71_000_000_000);
        assert_eq!(tree.coverage.recorded_turns, 2);
        assert_eq!(tree.coverage.unfinished_turns, 0);
        let codex = on("codex", UsagePeriod::Hourly)?;
        assert_eq!(priced(&codex.sessions[0]) + priced(tree), priced(root));
        assert_eq!(codex.sessions[0].coverage.recorded_turns, 4);
        assert_eq!(codex.sessions[0].coverage.unfinished_turns, 1);

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
        // An unknown cost sorts after every known one, even $0.
        assert_eq!(daily.sessions[0].session_id, "suspended");
        assert_eq!(daily.sessions[0].coverage.recorded_turns, 0);
        assert_eq!(daily.sessions[0].cost_usd_ticks, Ok(0));
        assert_eq!(daily.sessions[1].session_id, "root");
        assert_eq!(priced(&daily.sessions[1]), 93_050_000_000);
        assert_eq!(daily.cost_usd_ticks, Err(UnknownCost));

        let weekly_query = SessionUsageQuery {
            period: UsagePeriod::Weekly,
            include_suspended: true,
            profile: None,
        };
        let weekly = load_from(&path, &weekly_query, now)?;
        assert_eq!(weekly.sessions.len(), 2);
        // Cache-discounted Astra: $5 + $0.50 + $5 = $10.50, greater than root.
        assert_eq!(weekly.sessions[0].session_id, "suspended");
        assert_eq!(weekly.sessions[0].cost_usd_ticks, Ok(105_000_000_000));
        assert!(weekly.sessions[0].suspended);
        assert_eq!(priced(&weekly.sessions[1]), 93_050_000_000);
        assert_eq!(weekly.cost_usd_ticks, Err(UnknownCost));
        let filtered = on("codex", UsagePeriod::Weekly)?;
        assert_eq!(filtered.sessions.len(), 1);
        assert_eq!(priced(&filtered.sessions[0]), 22_050_000_000);
        let monthly = load_from(
            &path,
            &SessionUsageQuery {
                period: UsagePeriod::Monthly,
                profile: Some("claude".into()),
                ..weekly_query
            },
            now,
        )?;
        // Root ran no turns on claude, so only the claude parent remains.
        assert_eq!(monthly.sessions.len(), 1);
        assert_eq!(monthly.sessions[0].cost_usd_ticks, Ok(126_000_000_000));
        assert_eq!(monthly.cost_usd_ticks, Ok(126_000_000_000));
        assert_eq!(monthly.sessions[0].coverage.recorded_turns, 2);

        // Renaming a profile carries destroyed children's turns with it.
        super::super::sessions::rename_session_reference_at(
            &path,
            "last_profile",
            "codex2",
            "codex-two",
        )?;
        assert_eq!(
            on("codex-two", UsagePeriod::Hourly)?.sessions,
            codex2.sessions
        );
        Ok(())
    }

    // Store integration: Claude records the alias a turn selected; the
    // profile's discovered model list says which model that alias runs.
    #[test]
    fn usage_report_prices_claude_aliases_through_the_profile_catalog() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("usage-report.sqlite");
        let now = 86_400_000;
        let mut state = State::default();
        for id in ["aliases", "unresolved"] {
            let mut session = super::super::tests::session(id, "project");
            session.last_profile = "claude4".into();
            session.state = SessionState::Running;
            state.sessions.insert(id.into(), session);
        }
        save_state_to(&path, &state)?;
        let connection = open(&path)?;
        let catalogs = model_catalog::ModelCatalogs(BTreeMap::from([(
            "claude4".to_owned(),
            serde_json::from_value(serde_json::json!([
                {"value":"default","name":"Default (recommended)","description":"Sonnet"},
                {"value":"opus","name":"Opus 5.5","description":null},
                {"value":"claude-fable-5-1[1m]","name":"Fable 5.1","description":null},
                {"value":"claude-opus-5","name":"Opus 5","description":null}]))?,
        )]));
        // Claude reports uncached input; caches are separate tallies.
        let usage = serde_json::json!({"scope":"turn","total_tokens":2_100_000,"input_tokens":1_000_000,
            "output_tokens":100_000,"cached_read_tokens":1_000_000,"cached_write_tokens":0});
        for (seq, (session, model)) in [
            ("aliases", "opus"),
            ("aliases", "fable"),
            ("unresolved", "default"),
        ]
        .into_iter()
        .enumerate()
        {
            let command = format!("command-{seq}");
            let body = serde_json::json!({"command_id":command,"completed_ordinal":seq,"completed_at_ms":now,
                "outcome":{"kind":"completed","stop_reason":"EndTurn"},"usage":usage});
            let turn: MaterializedTurnOutcome = serde_json::from_value(body)?;
            connection.execute("INSERT INTO session_turn_usage(session_id,command_id,completed_ordinal,body) VALUES (?1,?2,?3,?4)", params![session, command, seq, serialize_turn_outcome(&turn)?])?;
            let configuration = serde_json::from_value(serde_json::json!({"model":model}))?;
            super::super::usage::record_turn_selection(
                &connection,
                session,
                &command,
                &configuration,
            )?;
        }
        drop(connection);

        let report = load_with(&path, &SessionUsageQuery::default(), now, &catalogs)?;
        let [aliases, unresolved] = &report.sessions[..] else {
            panic!("expected two sessions: {:?}", report.sessions);
        };
        assert_eq!(aliases.session_id, "aliases");
        let models = aliases
            .models
            .iter()
            .map(|model| (model.model.as_deref().unwrap(), model.cost_usd_ticks))
            .collect::<Vec<_>>();
        // `opus` is the listed Opus 5.5, not the older Opus 5 also listed;
        // `fable` matches the listed Fable 5.1 the way the bridge does.
        assert_eq!(
            models,
            [
                ("claude-fable-5-1", Ok(152_500_000_000)), // $10 + $5 + $0.25
                ("claude-opus-5-5", Ok(62_000_000_000)),   // $4 + $2 + $0.20
            ]
        );
        assert_eq!(aliases.cost_usd_ticks, Ok(214_500_000_000));
        // `default` names no versioned model, so its cost is unknown.
        assert_eq!(unresolved.models[0].model.as_deref(), Some("default"));
        assert_eq!(unresolved.cost_usd_ticks, Err(UnknownCost));
        assert_eq!(report.cost_usd_ticks, Err(UnknownCost));
        Ok(())
    }
}
