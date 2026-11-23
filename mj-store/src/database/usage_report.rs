//! One consistent store snapshot of parent usage and retained descendants.
use super::*;
use mj_core::usage::{
    AgentUsageEstimate, HOUR_MS, HourlyUsage, ModelUsageEstimate, SessionUsageEstimate,
    SessionUsageQuery, SessionUsageReport, USAGE_GRAPH_HOURS, UnknownCost, UsageAgentRole,
    UsageScope, add_cost, descending_cost, graph_start_ms, hour_start_ms,
};

use mj_core::usage::model_catalog;

// Match the expressions indexed by migration 125. Read the selected time
// window once, regardless of how many historical descendants its roots retain.
const REPORT_TURNS_SQL: &str = "SELECT u.session_id, u.body, s.model, s.effort, s.profile,
            COALESCE(s.category, a.category), a.kind
    FROM session_turn_usage u
    LEFT JOIN session_turn_selections s USING(session_id, command_id)
    LEFT JOIN subagent_accounting a ON a.child_session_id=u.session_id
    WHERE json_extract(u.body, '$.completed_at_ms') >= ?1
    AND json_extract(u.body, '$.completed_at_ms') <= ?2
    AND (?3 IS NULL OR s.profile=?3)";

const REPORT_ACTIVE_SQL: &str = "SELECT m.session_id FROM materialized_sessions m
    LEFT JOIN session_turn_selections s ON s.session_id=m.session_id
        AND s.command_id=json_extract(m.active_turn_json, '$.command_id')
    WHERE m.active_turn_json IS NOT NULL
    AND json_extract(m.active_turn_json, '$.started_at_ms') <= ?1
    AND (?2 IS NULL OR s.profile=?2)";

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

fn child_usage_role(
    member: &str,
    category: Option<&str>,
    kind: Option<&str>,
) -> Result<UsageAgentRole> {
    Ok(match (category, kind) {
        (Some("supervisor" | "orchestrator"), _) => UsageAgentRole::Orchestrator,
        (Some("subtle"), _) => UsageAgentRole::Subtle,
        (Some("straightforward"), _) => UsageAgentRole::Straightforward,
        (Some("oracle"), _) => UsageAgentRole::Oracle,
        (Some("reviewer" | "brief_writer"), _) => UsageAgentRole::Reviewer,
        (_, Some("oracle")) => UsageAgentRole::Oracle,
        (_, Some("reviewer")) => UsageAgentRole::Reviewer,
        (_, Some("worker")) => UsageAgentRole::Subagent,
        (_, None) => UsageAgentRole::UnclassifiedSubagent,
        (_, Some(kind)) => bail!("unknown usage agent kind {kind:?} for {member}"),
    })
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
    // The graph always spans the same hours; the rows cover one of them when
    // the query names it.
    let graph_start = graph_start_ms(now_ms);
    let (since_ms, until_ms) = match query.hour_start_ms {
        Some(hour) => {
            let hour = hour_start_ms(hour);
            (hour, now_ms.min(hour.saturating_add(HOUR_MS - 1)))
        }
        None => (graph_start, now_ms),
    };
    let mut hours = (0..USAGE_GRAPH_HOURS)
        .map(|n| HourlyUsage {
            start_ms: graph_start + n * HOUR_MS,
            ..Default::default()
        })
        .collect::<Vec<_>>();
    let mut report = SessionUsageReport {
        since_ms,
        until_ms,
        ..Default::default()
    };
    let mut roots = tx.prepare("SELECT session_id, title, session_title_override, acp_session_title, last_profile, state
        FROM sessions s WHERE NOT EXISTS(SELECT 1 FROM subagent_accounting a WHERE a.child_session_id=s.session_id) ORDER BY session_id")?;
    let roots = roots
        .query_map([], |row| {
            let id = row.get::<_, String>(0)?;
            let title = row.get::<_, String>(1)?;
            let title_override = row.get::<_, Option<String>>(2)?;
            let acp_title = row.get::<_, Option<String>>(3)?;
            // Named as the session list names it.
            let name = mj_core::state::listed_session_title(
                &id,
                &title,
                title_override.as_deref(),
                acp_title.as_deref(),
            )
            .to_owned();
            Ok((id, name, row.get::<_, String>(4)?, row.get::<_, String>(5)?))
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
    struct ReportEntry {
        estimate: SessionUsageEstimate,
        models: BTreeMap<Option<String>, ModelUsageEstimate>,
    }
    let mut entries = Vec::new();
    // One owner per retained member, decided from the same snapshot as usage.
    let mut owners = BTreeMap::new();
    let mut tree = tx.prepare("WITH RECURSIVE tree(id) AS (
        SELECT ?1 UNION SELECT a.child_session_id FROM subagent_accounting a JOIN tree t ON a.parent_session_id=t.id)
        SELECT id FROM tree")?;
    for (session_id, title, profile, state) in roots {
        let suspended = !SessionState::from_stored(&state)
            .context("unknown usage session state")?
            .is_active();
        if suspended && !query.include_suspended {
            continue;
        }
        let members = tree
            .query_map([&session_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let subagent_count = members.len().saturating_sub(1);
        let index = entries.len();
        owners.extend(members.into_iter().map(|member| (member, index)));
        entries.push(ReportEntry {
            estimate: SessionUsageEstimate {
                session_id,
                title,
                profile,
                suspended,
                subagent_count,
                ..Default::default()
            },
            models: BTreeMap::new(),
        });
    }
    let mut role_groups = BTreeMap::from([
        (UsageAgentRole::Primary, BTreeMap::new()),
        (UsageAgentRole::Subagent, BTreeMap::new()),
        (UsageAgentRole::Reviewer, BTreeMap::new()),
    ]);
    let mut turns = tx.prepare(REPORT_TURNS_SQL)?;
    let rows = turns.query_map(params![since_ms.min(graph_start), now_ms, wanted], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            UsageSelection {
                model: row.get(2)?,
                effort: row.get(3)?,
            },
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
        ))
    })?;
    for row in rows {
        let (member, body, selection, turn_profile, category, kind) = row?;
        let Some(&index) = owners.get(&member) else {
            continue;
        };
        let entry = &mut entries[index];
        let turn = deserialize_turn_outcome(&body)?;
        let in_rows = (since_ms..=until_ms).contains(&turn.completed_at_ms);
        let bar = usize::try_from((turn.completed_at_ms - graph_start).div_euclid(HOUR_MS))
            .ok()
            .filter(|bar| *bar < hours.len());
        if in_rows {
            entry.estimate.coverage.recorded_turns += 1;
        }
        match turn.usage {
            Some(usage) if usage.scope == UsageScope::Turn => {
                if in_rows {
                    entry.estimate.coverage.full_turn_reports += 1;
                }
                let role = in_rows
                    .then(|| {
                        if member == entry.estimate.session_id {
                            Ok(UsageAgentRole::Primary)
                        } else {
                            child_usage_role(&member, category.as_deref(), kind.as_deref())
                        }
                    })
                    .transpose()?;
                let mut attributed = BTreeMap::new();
                super::usage::attribute(&usage, selection, &mut attributed)?;
                for (mut selection, totals) in attributed {
                    selection.model = selection
                        .model
                        .map(|model| catalogs.priced_model(turn_profile.as_deref(), model));
                    let cost = super::usage::estimate_model_cost(
                        &usage,
                        selection.model.as_deref(),
                        &totals,
                    )?;
                    if let Some(bar) = bar {
                        let hour = &mut hours[bar];
                        match cost {
                            Some(cost) => {
                                hour.known_cost_usd_ticks = hour
                                    .known_cost_usd_ticks
                                    .checked_add(cost)
                                    .context("hour cost overflow")?
                            }
                            None => hour.has_unknown_cost = true,
                        }
                    }
                    let Some(role) = role else { continue };
                    for groups in [&mut entry.models, role_groups.entry(role).or_default()] {
                        let group = groups.entry(selection.model.clone()).or_insert_with(|| {
                            ModelUsageEstimate {
                                model: selection.model.clone(),
                                ..Default::default()
                            }
                        });
                        group.cost_usd_ticks =
                            add_cost(group.cost_usd_ticks, cost.ok_or(UnknownCost))
                                .context("model cost overflow")?;
                        super::usage::merge_totals(&mut group.totals, &totals)?;
                    }
                }
            }
            Some(usage) if usage.scope == UsageScope::LastRequest => {
                if in_rows {
                    entry.estimate.coverage.last_request_reports += 1
                }
            }
            Some(_) => {
                if in_rows {
                    entry.estimate.coverage.unspecified_reports += 1
                }
            }
            None => {
                if in_rows {
                    entry.estimate.coverage.missing_reports += 1
                }
            }
        }
    }
    // A turn already running at the start of the window can still consume in it.
    let mut active = tx.prepare(REPORT_ACTIVE_SQL)?;
    for member in active.query_map(params![until_ms, wanted], |row| row.get::<_, String>(0))? {
        if let Some(&index) = owners.get(&member?) {
            entries[index].estimate.coverage.unfinished_turns += 1;
        }
    }
    report.hours = hours;
    for entry in entries {
        let mut estimate = entry.estimate;
        // A profile selects its own parents and any parent whose tree ran a
        // turn on it in the period; only that profile's turns are counted.
        if wanted.is_some_and(|wanted| wanted != estimate.profile)
            && estimate.coverage.recorded_turns == 0
            && estimate.coverage.unfinished_turns == 0
        {
            continue;
        }
        estimate.models = entry.models.into_values().collect();
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
    for (role, groups) in role_groups {
        let mut models: Vec<_> = groups.into_values().collect();
        models.sort_by(|a, b| {
            descending_cost(&a.cost_usd_ticks, &b.cost_usd_ticks)
                .then_with(|| a.model.cmp(&b.model))
        });
        let cost_usd_ticks = models
            .iter()
            .try_fold(Ok(0), |total, model| add_cost(total, model.cost_usd_ticks))
            .context("agent role cost overflow")?;
        report.agent_usage.push(AgentUsageEstimate {
            role,
            models,
            cost_usd_ticks,
        });
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hard-won: 1f8ff4afc: an hourly read scanned every historical turn body.
    #[test]
    fn usage_report_time_index_migration_keeps_old_writes_and_skips_history() -> Result<()> {
        use rusqlite::StatementStatus;

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("usage-index.sqlite");
        save_session_to(&path, &super::super::tests::session("root", "project"))?;
        let connection = open(&path)?;
        // Reconstruct the previous revision, with a turn written before the index.
        super::super::legacy_schema::ensure_session_container_override_columns(&connection)?;
        connection.execute_batch(
            "DROP INDEX session_turn_usage_completed_time;
             DROP INDEX materialized_sessions_active_usage;
             DELETE FROM schema_migrations WHERE version>=125;
             UPDATE schema_compatibility SET minimum_compatible_version=124;
             PRAGMA user_version=124;",
        )?;
        let mut turn = super::super::turn_outcome::tests::legacy_turns().remove(0);
        turn.command_id = "recent".into();
        turn.completed_at_ms = 1_000;
        let body = serialize_turn_outcome(&turn)?;
        connection.execute(
            "INSERT INTO session_turn_usage(session_id,command_id,completed_ordinal,body)
             VALUES ('root','recent',1,?1)",
            [&body],
        )?;
        schema::migrate_schema(&connection)?;
        schema::read_schema_state(&connection)?
            .ensure_supported_by(super::super::SCHEMA_VERSION)?;

        let read = || -> Result<(Vec<String>, i32)> {
            let mut statement = connection.prepare(REPORT_TURNS_SQL)?;
            let bodies = statement
                .query_map(params![1_000, 2_000, Option::<String>::None], |row| {
                    row.get::<_, String>(1)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((bodies, statement.get_status(StatementStatus::VmStep)))
        };
        let (before, steps_before) = read()?;
        assert_eq!(before.as_slice(), std::slice::from_ref(&body));
        let read_active = || -> Result<(Vec<String>, i32)> {
            let mut statement = connection.prepare(REPORT_ACTIVE_SQL)?;
            let members = statement
                .query_map(params![2_000, Option::<String>::None], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((members, statement.get_status(StatementStatus::VmStep)))
        };
        let active_before = read_active()?;
        // Previous writers still use the same columns. SQLite maintains the
        // expression index for their inserts and updates without a new writer.
        connection.execute(
            "WITH RECURSIVE history(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM history WHERE n<4096)
             INSERT INTO session_turn_usage(session_id,command_id,completed_ordinal,body)
             SELECT 'root','old-' || n,n+1,json_set(?1,'$.completed_at_ms',0) FROM history",
            [&body],
        )?;
        let (after, steps_after) = read()?;
        assert_eq!(after, before);
        assert_eq!(
            steps_after, steps_before,
            "hourly query work grew with old turns"
        );
        connection.execute_batch(
            "CREATE TEMP TABLE idle_members AS
                 WITH RECURSIVE history(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM history WHERE n<4096)
                 SELECT 'idle-' || n AS id FROM history;
             INSERT INTO session_contexts(session_id,bundle_id,created_at)
                 SELECT id,'project','now' FROM idle_members;
             INSERT INTO sessions(session_id,title,harness_kind,last_profile,target_template_id,state,updated_at)
                 SELECT id,id,'codex','codex','local','stopped','now' FROM idle_members;
             INSERT INTO materialized_sessions(session_id) SELECT id FROM idle_members;",
        )?;
        assert_eq!(
            read_active()?,
            active_before,
            "active-turn query work grew with inactive sessions"
        );
        connection.execute(
            "UPDATE session_turn_usage SET body=json_set(body,'$.completed_at_ms',1500)
             WHERE session_id='root' AND command_id='old-1'",
            [],
        )?;
        assert_eq!(read()?.0.len(), 2);
        connection.execute(
            "UPDATE materialized_sessions SET active_turn_json=
                 json_object('command_id','recent','started_at_ms',1500)
             WHERE session_id='root'",
            [],
        )?;
        assert_eq!(read_active()?.0, ["root"]);
        Ok(())
    }

    // Hard-won: #1559: the accounting schema lost reviewer kinds on destruction.
    #[test]
    fn usage_kind_migration_backfills_live_children_and_preserves_legacy_writes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("usage-kind.sqlite");
        for id in [
            "root",
            "reviewer",
            "worker",
            "oracle",
            "destroyed",
            "legacy",
        ] {
            save_session_to(&path, &super::super::tests::session(id, "project"))?;
        }
        let connection = open(&path)?;
        connection.execute_batch(
            "DROP TRIGGER subagent_accounting_kind_insert;
             DROP TRIGGER subagent_accounting_kind_update;
             ALTER TABLE subagent_accounting DROP COLUMN kind;
             DELETE FROM schema_migrations WHERE version>134;
             UPDATE schema_compatibility SET minimum_compatible_version=132 WHERE singleton=1;
             PRAGMA user_version=134;
             CREATE TRIGGER stop_before_brief_fence BEFORE INSERT ON schema_migrations
                 WHEN NEW.version=136 BEGIN SELECT RAISE(ABORT,'fixture boundary before brief fence'); END;
             INSERT INTO subagent_accounting(child_session_id,parent_session_id,task_name)
                 VALUES ('reviewer','root','review'),('worker','root','work'),
                     ('oracle','root','consult'),('destroyed','root','old');
             INSERT INTO subagent_sessions(child_session_id,parent_session_id,request_key,record_json)
                 VALUES ('reviewer','root','review','{\"kind\":\"reviewer\"}'),
                     ('worker','root','work','{}'),
                     ('oracle','root','consult','{\"kind\":\"oracle\"}');",
        )?;
        let floor = schema::read_schema_state(&connection)?.minimum_compatible;
        // Test revision 135's compatibility before the separate brief fence.
        let error = schema::migrate_schema(&connection).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("fixture boundary before brief fence"),
            "{error:#}"
        );
        connection.execute_batch("ROLLBACK;")?;
        let at_135 = schema::read_schema_state(&connection)?;
        assert_eq!(at_135.revision, 135);
        assert_eq!(at_135.minimum_compatible, floor);
        at_135.ensure_supported_by(134)?;
        // An old daemon's conflict path must not erase the new accounting fact.
        connection.execute_batch(
            "INSERT INTO subagent_accounting(child_session_id,parent_session_id,task_name)
                 VALUES ('reviewer','root','review') ON CONFLICT(child_session_id) DO NOTHING;
             INSERT INTO subagent_accounting(child_session_id,parent_session_id,task_name)
                 VALUES ('legacy','root','legacy');
             INSERT INTO subagent_sessions(child_session_id,parent_session_id,request_key,record_json)
                 VALUES ('legacy','root','legacy','{\"kind\":\"reviewer\"}');
             UPDATE subagent_sessions
                 SET record_json=json_set(record_json, '$.noticed_turn', 3)
                 WHERE child_session_id='reviewer';",
        )?;
        connection.execute_batch("DROP TRIGGER stop_before_brief_fence;")?;
        drop(connection);
        delete_session_from(&path, "reviewer")?;
        delete_session_from(&path, "legacy")?;
        let connection = open(&path)?;
        let latest = schema::read_schema_state(&connection)?;
        assert_eq!(latest.revision, super::super::SCHEMA_VERSION);
        assert_eq!(
            latest.minimum_compatible,
            Some(schema::MINIMUM_COMPATIBLE_VERSION)
        );
        assert!(latest.ensure_supported_by(135).is_err());
        let retained: Vec<(String, Option<String>)> = connection
            .prepare(
                "SELECT child_session_id,kind FROM subagent_accounting ORDER BY child_session_id",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        assert_eq!(
            retained,
            vec![
                ("destroyed".into(), None),
                ("legacy".into(), Some("reviewer".into())),
                ("oracle".into(), Some("oracle".into())),
                ("reviewer".into(), Some("reviewer".into())),
                ("worker".into(), Some("worker".into())),
            ]
        );
        Ok(())
    }

    // Hard-won: #1559: destroying reviewers erased their cost classification.
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
        connection.execute_batch("INSERT INTO subagent_accounting(child_session_id,parent_session_id,task_name) VALUES ('child','root','child'),('grandchild','child','grandchild')")?;
        for (child, parent, kind) in [
            ("child", "root", mj_core::subagent::SubagentKind::Worker),
            (
                "grandchild",
                "child",
                mj_core::subagent::SubagentKind::Reviewer,
            ),
        ] {
            let record = mj_core::subagent::SubagentRecord {
                kind,
                reviewer_launch: None,
                child_session_id: child.into(),
                parent_session_id: parent.into(),
                task_name: child.into(),
                profile_id: state.sessions[child].last_profile.clone(),
                model: None,
                effort: None,
                working_directory: PathBuf::new(),
                initial_prompt: "Inspect usage".into(),
                request_key: child.into(),
                created_at: state.sessions[child].created_at.clone(),
                noticed_turn: None,
                reported_finish: None,
                handback_tool: false,
                supervisor: false,
            };
            connection.execute(
                "INSERT INTO subagent_sessions(child_session_id,parent_session_id,request_key,record_json)
                 VALUES (?1,?2,?3,?4)",
                params![child, parent, kind.request_key(child), serde_json::to_string(&record)?],
            )?;
        }
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
            now - 23 * 3_600_000,
            "global.openai.gpt-6-luna",
            inclusive.clone(),
        )?;
        insert(
            "root",
            2,
            now - 23 * 3_600_000 - 1,
            "gpt-6-sol",
            inclusive.clone(),
        )?;
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
        let role_cost = |report: &SessionUsageReport, role| {
            report
                .agent_usage
                .iter()
                .find(|a| a.role == role)
                .unwrap()
                .cost_usd_ticks
        };
        let before_destroy = load_from(&path, &SessionUsageQuery::default(), now)?;
        assert_eq!(
            role_cost(&before_destroy, UsageAgentRole::Primary),
            Ok(1_050_000_000)
        );
        assert_eq!(
            role_cost(&before_destroy, UsageAgentRole::Subagent),
            Ok(71_000_000_000)
        );
        assert_eq!(
            role_cost(&before_destroy, UsageAgentRole::Reviewer),
            Err(UnknownCost)
        );
        // Park/Close keep child records and their kind, even though the child
        // itself is no longer active. Only the root controls this report filter.
        let connection = open(&path)?;
        connection.execute(
            "UPDATE sessions SET state='stopped' WHERE session_id IN ('child','grandchild')",
            [],
        )?;
        drop(connection);
        assert_eq!(
            load_from(&path, &SessionUsageQuery::default(), now)?.agent_usage,
            before_destroy.agent_usage
        );
        let filtered = load_from(
            &path,
            &SessionUsageQuery {
                profile: Some("codex2".into()),
                ..Default::default()
            },
            now,
        )?;
        assert_eq!(role_cost(&filtered, UsageAgentRole::Primary), Ok(0));
        assert_eq!(
            role_cost(&filtered, UsageAgentRole::Subagent),
            Ok(71_000_000_000)
        );
        assert_eq!(
            role_cost(&filtered, UsageAgentRole::Reviewer),
            Err(UnknownCost)
        );
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
        assert_eq!(
            role_cost(&hourly, UsageAgentRole::Primary),
            Ok(1_050_000_000)
        );
        assert_eq!(hourly.agent_usage, before_destroy.agent_usage);

        // A profile counts the turns recorded on it anywhere in the tree, by
        // the profile at turn start, after the children are destroyed.
        let on = |profile: &str, hour_start_ms| {
            load_from(
                &path,
                &SessionUsageQuery {
                    profile: Some(profile.into()),
                    hour_start_ms,
                    include_suspended: true,
                },
                now,
            )
        };
        let codex2 = on("codex2", None)?;
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
        let codex = on("codex", None)?;
        assert_eq!(priced(&codex.sessions[0]) + priced(tree), priced(root));
        assert_eq!(codex.sessions[0].coverage.recorded_turns, 4);
        assert_eq!(codex.sessions[0].coverage.unfinished_turns, 1);

        // The default span is the graph's 24 clock hours; its first hour
        // starts inclusively, and an older or later turn stays out.
        assert_eq!(hourly.since_ms, now - 23 * HOUR_MS);
        assert_eq!(hourly.until_ms, now);
        let with_suspended = load_from(
            &path,
            &SessionUsageQuery {
                include_suspended: true,
                ..Default::default()
            },
            now,
        )?;
        assert_eq!(with_suspended.sessions.len(), 2);
        // An unknown cost sorts after every known one, even $0.
        assert_eq!(with_suspended.sessions[0].session_id, "suspended");
        assert_eq!(with_suspended.sessions[0].coverage.recorded_turns, 0);
        assert_eq!(with_suspended.sessions[0].cost_usd_ticks, Ok(0));
        assert_eq!(with_suspended.sessions[1], *root);
        assert_eq!(with_suspended.cost_usd_ticks, Err(UnknownCost));

        // One bar per hour; each holds the known cost of the turns that
        // completed in it, and together they match the default rows.
        let bar = |report: &SessionUsageReport, n: usize| report.hours[n];
        assert_eq!(hourly.hours.len(), 24);
        assert_eq!(bar(&hourly, 0).start_ms, now - 23 * HOUR_MS);
        assert_eq!(bar(&hourly, 23).start_ms, now);
        assert_eq!(
            hourly
                .hours
                .iter()
                .map(|hour| hour.known_cost_usd_ticks)
                .sum::<u64>(),
            priced(root)
        );
        assert!(bar(&hourly, 23).has_unknown_cost);
        assert!(!bar(&hourly, 0).has_unknown_cost);
        assert!(bar(&hourly, 0).known_cost_usd_ticks > 0);
        assert!(hourly.hours[1..23].iter().all(|hour| *hour
            == HourlyUsage {
                start_ms: hour.start_ms,
                ..Default::default()
            }));

        // Naming an hour narrows the rows to the turns completed in it, while
        // the bars keep showing the whole graph.
        let in_hour = |hour: i64| {
            load_from(
                &path,
                &SessionUsageQuery {
                    hour_start_ms: Some(hour),
                    ..Default::default()
                },
                now,
            )
        };
        let first = in_hour(now - 23 * HOUR_MS)?;
        assert_eq!(first.hours, hourly.hours);
        assert_eq!(
            (first.since_ms, first.until_ms),
            (now - 23 * HOUR_MS, now - 22 * HOUR_MS - 1)
        );
        assert_eq!(first.sessions.len(), 1);
        assert_eq!(first.sessions[0].coverage.recorded_turns, 1);
        assert_eq!(
            first.sessions[0].cost_usd_ticks,
            Ok(bar(&hourly, 0).known_cost_usd_ticks)
        );
        // The running turn started in the current hour, after this one ended.
        assert_eq!(first.sessions[0].coverage.unfinished_turns, 0);
        let last = in_hour(now)?;
        assert_eq!(last.until_ms, now);
        assert_eq!(last.sessions[0].coverage.unfinished_turns, 1);
        assert_eq!(last.sessions[0].coverage.recorded_turns, 5);
        assert_eq!(
            priced(&last.sessions[0]) + priced(&first.sessions[0]),
            priced(root)
        );
        assert!(
            in_hour(now + HOUR_MS)?
                .sessions
                .iter()
                .all(|s| s.coverage.recorded_turns == 0)
        );
        // Hours older than the graph are still readable, with no bar.
        let before_graph = in_hour(now - 24 * HOUR_MS)?;
        assert_eq!(before_graph.sessions[0].coverage.recorded_turns, 1);
        assert_eq!(priced(&before_graph.sessions[0]), 21_000_000_000);
        let two_days = load_from(
            &path,
            &SessionUsageQuery {
                profile: Some("claude".into()),
                hour_start_ms: Some(now - 2 * 86_400_000),
                include_suspended: true,
            },
            now,
        )?;
        // Root ran no turns on claude, so only the claude parent remains.
        // Cache-discounted Astra: $5 + $0.50 + $5.
        assert_eq!(two_days.sessions.len(), 1);
        assert_eq!(two_days.sessions[0].cost_usd_ticks, Ok(105_000_000_000));
        assert_eq!(two_days.sessions[0].coverage.recorded_turns, 1);
        assert!(two_days.sessions[0].suspended);
        assert!(
            two_days
                .hours
                .iter()
                .all(|hour| hour.known_cost_usd_ticks == 0)
        );

        // Renaming a profile carries destroyed children's turns with it.
        super::super::sessions::rename_profile_references_at(&path, "codex2", "codex-two")?;
        assert_eq!(on("codex-two", None)?.sessions, codex2.sessions);
        // A child destroyed before kind retention cannot be classified from
        // its task name. Keep its usage without guessing that it was a worker.
        open(&path)?.execute(
            "UPDATE subagent_accounting SET kind=NULL WHERE child_session_id='grandchild'",
            [],
        )?;
        let historical = load_from(&path, &SessionUsageQuery::default(), now)?;
        let mut unclassified = before_destroy
            .agent_usage
            .iter()
            .find(|group| group.role == UsageAgentRole::Reviewer)
            .unwrap()
            .clone();
        unclassified.role = UsageAgentRole::UnclassifiedSubagent;
        assert!(historical.agent_usage.contains(&unclassified));
        assert_eq!(role_cost(&historical, UsageAgentRole::Reviewer), Ok(0));
        assert_eq!(historical.sessions, hourly.sessions);
        Ok(())
    }

    #[test]
    fn usage_report_classifies_recorded_turns_and_legacy_children() -> Result<()> {
        use UsageAgentRole::*;

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("usage-roles.sqlite");
        let now = 86_400_000;
        let mut root = super::super::tests::session("root", "project");
        root.state = SessionState::Running;
        save_session_to(&path, &root)?;
        for id in ["orchestrator", "worker", "oracle", "reviewer", "destroyed"] {
            save_session_to(&path, &super::super::tests::session(id, "project"))?;
        }

        let connection = open(&path)?;
        connection.execute_batch(
            "INSERT INTO subagent_accounting(child_session_id,parent_session_id,task_name,kind,category)
             VALUES ('orchestrator','root','orchestrator','worker','orchestrator'),
                    ('worker','orchestrator','worker','worker',NULL),
                    ('oracle','orchestrator','oracle','oracle',NULL),
                    ('reviewer','orchestrator','reviewer','reviewer',NULL),
                    ('destroyed','orchestrator','destroyed',NULL,NULL);",
        )?;
        // Each turn reports a distinct token count so the totals prove both
        // category precedence and per-turn attribution within one session.
        let turns = [
            ("root", Some("subtle"), 1),
            ("orchestrator", Some("supervisor"), 2),
            ("orchestrator", None, 4),
            ("orchestrator", Some("subtle"), 8),
            ("worker", Some("straightforward"), 16),
            ("worker", Some("oracle"), 32),
            ("oracle", None, 64),
            ("worker", Some("reviewer"), 128),
            ("worker", Some("brief_writer"), 256),
            ("reviewer", None, 512),
            ("worker", None, 1024),
            ("worker", Some("future_category"), 2048),
            ("destroyed", None, 4096),
        ];
        for (index, (session, category, tokens)) in turns.into_iter().enumerate() {
            let ordinal = index + 1;
            let command = format!("turn-{ordinal}");
            let turn: MaterializedTurnOutcome = serde_json::from_value(serde_json::json!({
                "command_id": command, "completed_ordinal": ordinal, "completed_at_ms": now,
                "outcome": {"kind":"completed","stop_reason":"EndTurn"},
                "usage": {"scope":"turn", "input_tokens":tokens, "output_tokens":0, "total_tokens":tokens}
            }))?;
            connection.execute(
                "INSERT INTO session_turn_usage(session_id,command_id,completed_ordinal,body)
                 VALUES (?1,?2,?3,?4)",
                params![session, command, ordinal, serialize_turn_outcome(&turn)?],
            )?;
            connection.execute(
                "INSERT INTO session_turn_selections(session_id,command_id,category)
                 VALUES (?1,?2,?3)",
                params![session, command, category],
            )?;
        }
        drop(connection);
        delete_session_from(&path, "destroyed")?;

        let report = load_from(&path, &SessionUsageQuery::default(), now)?;
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].subagent_count, 5);
        let usage = report
            .agent_usage
            .iter()
            .map(|group| {
                let input = group
                    .models
                    .iter()
                    .map(|model| model.totals["input_tokens"].tokens)
                    .sum::<u64>();
                (group.role, input)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            usage,
            [
                (Primary, 1),
                (Orchestrator, 6),
                (Subtle, 8),
                (Straightforward, 16),
                (Oracle, 96),
                (Reviewer, 896),
                (Subagent, 3072),
                (UnclassifiedSubagent, 4096),
            ]
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

    // Store integration: Antigravity uncached-input turns and Grok Build
    // `*-build` model breakdowns both price in the session usage report.
    #[test]
    fn usage_report_prices_antigravity_and_grok_build_turns() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("usage-report.sqlite");
        let now = 86_400_000;
        let mut state = State::default();
        for (id, profile) in [("neon-root", "gemini"), ("grok-child", "grok")] {
            let mut session = super::super::tests::session(id, "project");
            session.last_profile = profile.into();
            session.state = SessionState::Running;
            state.sessions.insert(id.into(), session);
        }
        save_state_to(&path, &state)?;
        let connection = open(&path)?;
        connection.execute(
            "INSERT INTO subagent_accounting(child_session_id,parent_session_id,task_name,kind)
             VALUES ('grok-child','neon-root','grok-worker','worker')",
            [],
        )?;
        let insert = |session: &str,
                      seq: u64,
                      model: &str,
                      usage: serde_json::Value|
         -> Result<()> {
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
            Ok(())
        };
        // Antigravity reports uncached input; caches are separate tallies.
        insert(
            "neon-root",
            1,
            "gemini-4-neon",
            serde_json::json!({"scope":"turn","total_tokens":2_100_000,"input_tokens":1_000_000,
                "output_tokens":100_000,"thought_tokens":50_000,"cached_read_tokens":1_000_000,
                "cached_write_tokens":0}),
        )?;
        // Grok Build reports `grok-4.7-build` in model_usage when `grok-4.7` is selected,
        // and includes cached reads in input_tokens.
        for (seq, selected, reported) in [
            (2, "grok-4.7", "grok-4.7-build"),
            (3, "grok-4.7-build-fast", "grok-4.7-build-fast"),
        ] {
            let model_row = serde_json::json!({"scope":"turn","total_tokens":2_100_000,
                "input_tokens":2_000_000,"output_tokens":100_000,"thought_tokens":50_000,
                "cached_read_tokens":1_000_000,"cached_write_tokens":0});
            insert(
                "grok-child",
                seq,
                selected,
                serde_json::json!({"scope":"turn","total_tokens":2_100_000,"input_tokens":2_000_000,
                    "output_tokens":100_000,"thought_tokens":50_000,"cached_read_tokens":1_000_000,
                    "cached_write_tokens":0,"provider_details":{"model_usage":{reported:model_row}}}),
            )?;
        }
        drop(connection);

        let report = load_from(&path, &SessionUsageQuery::default(), now)?;
        let [root] = &report.sessions[..] else {
            panic!("expected one root session: {:?}", report.sessions);
        };
        assert_eq!(root.session_id, "neon-root");
        let models = root
            .models
            .iter()
            .map(|model| (model.model.as_deref().unwrap(), model.cost_usd_ticks))
            .collect::<Vec<_>>();
        assert_eq!(
            models,
            [
                ("grok-4.7-build-fast", Ok(62_000_000_000)), // $4 + $1.20 + $1.00
                ("gemini-4-neon", Ok(46_500_000_000)),       // $3 + $1.50 + $0.15
                ("grok-4.7-build", Ok(31_000_000_000)),      // $2 + $0.60 + $0.50
            ]
        );
        assert_eq!(root.cost_usd_ticks, Ok(139_500_000_000));
        assert_eq!(report.cost_usd_ticks, Ok(139_500_000_000));

        let primary = report
            .agent_usage
            .iter()
            .find(|group| group.role == UsageAgentRole::Primary)
            .expect("primary group");
        assert_eq!(primary.cost_usd_ticks, Ok(46_500_000_000));
        assert_eq!(primary.models[0].model.as_deref(), Some("gemini-4-neon"));

        let subagent = report
            .agent_usage
            .iter()
            .find(|group| group.role == UsageAgentRole::Subagent)
            .expect("subagent group");
        assert_eq!(subagent.cost_usd_ticks, Ok(93_000_000_000));
        Ok(())
    }
}
