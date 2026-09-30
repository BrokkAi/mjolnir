use super::*;
#[cfg(test)]
use mj_core::usage::ProviderCost;
use mj_core::usage::{TokenUsage, UsageScope};

pub(super) fn record_child_accounting(
    connection: &Connection,
    child: &SubagentRecord,
) -> Result<()> {
    connection.execute(
        "INSERT INTO subagent_accounting(child_session_id, parent_session_id, task_name)
        VALUES (?1, ?2, ?3) ON CONFLICT(child_session_id) DO NOTHING",
        params![
            child.child_session_id,
            child.parent_session_id,
            child.task_name
        ],
    )?;
    let stored: (String, String) = connection.query_row(
        "SELECT parent_session_id, task_name FROM subagent_accounting WHERE child_session_id=?1",
        [&child.child_session_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        stored == (child.parent_session_id.clone(), child.task_name.clone()),
        "accounting identity changed for child {}",
        child.child_session_id
    );
    Ok(())
}

pub(super) fn record_turn_selection(
    connection: &Connection,
    session: &str,
    command: &str,
    configuration: &mj_core::state::SessionConfiguration,
) -> Result<()> {
    let surface =
        mj_core::acp::surface::AcpSessionSurface::from_configuration(&configuration.values);
    connection.execute(
        "INSERT INTO session_turn_selections(session_id, command_id, model, effort)
        VALUES (?1, ?2, ?3, ?4) ON CONFLICT(session_id, command_id) DO NOTHING",
        params![
            session,
            command,
            surface.current_model(),
            surface.current_effort()
        ],
    )?;
    Ok(())
}

pub fn load_session_usage(
    session_id: &str,
    after_seq: u64,
    limit: usize,
) -> Result<Option<UsagePage>> {
    load_session_usage_from(&database_path(), session_id, after_seq, limit)
}

fn context_exists(connection: &Connection, session: &str) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_contexts WHERE session_id=?1)",
        [session],
        |row| row.get(0),
    )?)
}

fn load_session_usage_from(
    path: &Path,
    session_id: &str,
    after_seq: u64,
    limit: usize,
) -> Result<Option<UsagePage>> {
    let mut connection = open_reader(path)?;
    let tx = connection.transaction()?;
    if !context_exists(&tx, session_id)? {
        return Ok(None);
    }
    let latest_seq: u64 = tx.query_row(
        "SELECT COALESCE(MAX(completed_ordinal), 0) FROM session_turn_usage WHERE session_id=?1",
        [session_id],
        |row| row.get(0),
    )?;
    let mut statement = tx.prepare("SELECT body FROM session_turn_usage WHERE session_id=?1 AND completed_ordinal > ?2 ORDER BY completed_ordinal LIMIT ?3")?;
    let turns = statement
        .query_map(
            params![session_id, after_seq, limit.clamp(1, 1000) as i64],
            |row| row.get::<_, String>(0),
        )?
        .map(|row| Ok(serde_json::from_str::<MaterializedTurnOutcome>(&row?)?))
        .collect::<Result<Vec<_>>>()?;
    let mut turn_selections = BTreeMap::new();
    for turn in &turns {
        turn_selections.insert(
            turn.command_id.clone(),
            selection_for(&tx, session_id, &turn.command_id)?,
        );
    }
    let summary = session_summary(&tx, session_id)?;
    Ok(Some(UsagePage {
        session_id: session_id.into(),
        next_after_seq: turns
            .last()
            .map_or(latest_seq.max(after_seq), |turn| turn.completed_ordinal),
        latest_seq,
        turns,
        totals: summary.totals,
        coverage: summary.coverage,
        provider_session_cost: summary.provider_session_cost,
        turn_selections,
        by_model: summary.by_model,
    }))
}

fn selection_for(connection: &Connection, session: &str, command: &str) -> Result<UsageSelection> {
    Ok(connection.query_row("SELECT model, effort FROM session_turn_selections WHERE session_id=?1 AND command_id=?2",
        params![session, command], |row| Ok(UsageSelection { model: row.get(0)?, effort: row.get(1)? })).optional()?.unwrap_or_default())
}

fn counters(usage: &TokenUsage) -> BTreeMap<String, u64> {
    let mut values = BTreeMap::from([
        ("total_tokens".into(), usage.total_tokens),
        ("input_tokens".into(), usage.input_tokens),
        ("output_tokens".into(), usage.output_tokens),
    ]);
    for (name, value) in [
        ("thought_tokens", usage.thought_tokens),
        ("cached_read_tokens", usage.cached_read_tokens),
        ("cached_write_tokens", usage.cached_write_tokens),
    ] {
        if let Some(value) = value {
            values.insert(name.into(), value);
        }
    }
    values
}

fn add_counters(
    totals: &mut BTreeMap<String, UsageCounterTotal>,
    values: &BTreeMap<String, u64>,
) -> Result<()> {
    for (name, tokens) in values {
        let total = totals.entry(name.clone()).or_insert(UsageCounterTotal {
            tokens: 0,
            reported_turns: 0,
        });
        total.tokens = total
            .tokens
            .checked_add(*tokens)
            .context("usage token total overflow")?;
        total.reported_turns = total
            .reported_turns
            .checked_add(1)
            .context("usage turn total overflow")?;
    }
    Ok(())
}

/// Attribute each counter once. Partial or inconsistent breakdowns retain an
/// unknown remainder instead of assigning it to the configured model.
fn attribute(
    usage: &TokenUsage,
    selected: UsageSelection,
    groups: &mut BTreeMap<UsageSelection, BTreeMap<String, UsageCounterTotal>>,
) -> Result<()> {
    let values = counters(usage);
    let breakdown = usage
        .provider_details
        .as_ref()
        .map(|details| &details.model_usage)
        .filter(|models| !models.is_empty());
    let Some(models) = breakdown else {
        return add_counters(groups.entry(selected).or_default(), &values);
    };
    let mut assigned: BTreeMap<UsageSelection, BTreeMap<String, u64>> = BTreeMap::new();
    for (counter, total) in values {
        let parts = models
            .iter()
            .map(|(model, report)| (model, report.scope, counters(report).get(&counter).copied()))
            .collect::<Vec<_>>();
        let sum = parts.iter().try_fold(0u64, |sum, (_, scope, count)| {
            if *scope != UsageScope::Turn {
                return None;
            }
            sum.checked_add((*count)?)
        });
        if let Some(sum) = sum.filter(|sum| *sum <= total) {
            for (model, _, count) in parts {
                let selection = UsageSelection {
                    model: Some(model.clone()),
                    effort: (selected.model.as_ref() == Some(model))
                        .then(|| selected.effort.clone())
                        .flatten(),
                };
                assigned
                    .entry(selection)
                    .or_default()
                    .insert(counter.clone(), count.expect("validated counter"));
            }
            if sum < total {
                assigned
                    .entry(UsageSelection::default())
                    .or_default()
                    .insert(counter, total - sum);
            }
        } else {
            assigned
                .entry(UsageSelection::default())
                .or_default()
                .insert(counter, total);
        }
    }
    for (selection, values) in assigned {
        add_counters(groups.entry(selection).or_default(), &values)?;
    }
    Ok(())
}

fn session_summary(connection: &Connection, session: &str) -> Result<UsageSummary> {
    let mut summary = UsageSummary::default();
    let mut groups = BTreeMap::new();
    let mut statement = connection.prepare("SELECT u.body, s.model, s.effort FROM session_turn_usage u
        LEFT JOIN session_turn_selections s USING(session_id, command_id) WHERE u.session_id=?1 ORDER BY u.completed_ordinal")?;
    let rows = statement.query_map([session], |row| {
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
        let turn: MaterializedTurnOutcome = serde_json::from_str(&body)?;
        summary.coverage.recorded_turns += 1;
        match turn.usage {
            Some(usage) if usage.scope == UsageScope::Turn => {
                summary.coverage.full_turn_reports += 1;
                add_counters(&mut summary.totals, &counters(&usage))?;
                attribute(&usage, selection, &mut groups)?;
            }
            Some(usage) if usage.scope == UsageScope::LastRequest => {
                summary.coverage.last_request_reports += 1
            }
            Some(_) => summary.coverage.unspecified_reports += 1,
            None => summary.coverage.missing_reports += 1,
        }
    }
    summary.coverage.unfinished_turns = connection.query_row("SELECT COUNT(*) FROM session_turn_selections s
        WHERE session_id=?1 AND NOT EXISTS(SELECT 1 FROM session_turn_usage u WHERE u.session_id=s.session_id AND u.command_id=s.command_id)",
        [session], |row| row.get(0))?;
    summary.by_model = groups
        .into_iter()
        .map(|(selection, totals)| UsageModelTotal { selection, totals })
        .collect();
    let cost: Option<String> = connection
        .query_row(
            "SELECT body FROM session_provider_cost WHERE session_id=?1",
            [session],
            |row| row.get(0),
        )
        .optional()?;
    summary.provider_session_cost = cost.map(|body| serde_json::from_str(&body)).transpose()?;
    Ok(summary)
}

pub fn load_usage_tree(parent: &str) -> Result<Option<UsageTree>> {
    load_usage_tree_from(&database_path(), parent)
}

fn load_usage_tree_from(path: &Path, parent: &str) -> Result<Option<UsageTree>> {
    let mut connection = open_reader(path)?;
    let tx = connection.transaction()?;
    if !context_exists(&tx, parent)? {
        return Ok(None);
    }
    let mut statement = tx.prepare("WITH RECURSIVE tree(id) AS (
        SELECT ?1 UNION SELECT a.child_session_id FROM subagent_accounting a JOIN tree t ON a.parent_session_id=t.id)
        SELECT t.id, a.parent_session_id, a.task_name, EXISTS(SELECT 1 FROM sessions s WHERE s.session_id=t.id)
        FROM tree t LEFT JOIN subagent_accounting a ON a.child_session_id=t.id ORDER BY t.id")?;
    let identities = statement
        .query_map([parent], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut summary = UsageSummary::default();
    let mut groups: BTreeMap<UsageSelection, BTreeMap<String, UsageCounterTotal>> = BTreeMap::new();
    let mut sessions = Vec::new();
    for (session_id, parent_session_id, task_name, operational_session_present) in identities {
        let member = session_summary(&tx, &session_id)?;
        merge_summary(&mut summary, &member)?;
        for group in &member.by_model {
            merge_totals(
                groups.entry(group.selection.clone()).or_default(),
                &group.totals,
            )?;
        }
        sessions.push(UsageTreeSession {
            session_id,
            parent_session_id,
            task_name,
            operational_session_present,
            summary: member,
        });
    }
    summary.by_model = groups
        .into_iter()
        .map(|(selection, totals)| UsageModelTotal { selection, totals })
        .collect();
    Ok(Some(UsageTree {
        parent_session_id: parent.into(),
        sessions,
        summary,
    }))
}

fn merge_totals(
    target: &mut BTreeMap<String, UsageCounterTotal>,
    source: &BTreeMap<String, UsageCounterTotal>,
) -> Result<()> {
    for (name, value) in source {
        let total = target.entry(name.clone()).or_insert(UsageCounterTotal {
            tokens: 0,
            reported_turns: 0,
        });
        total.tokens = total
            .tokens
            .checked_add(value.tokens)
            .context("usage tree token overflow")?;
        total.reported_turns = total
            .reported_turns
            .checked_add(value.reported_turns)
            .context("usage tree turn overflow")?;
    }
    Ok(())
}

fn merge_summary(target: &mut UsageSummary, source: &UsageSummary) -> Result<()> {
    merge_totals(&mut target.totals, &source.totals)?;
    let a = &mut target.coverage;
    let b = &source.coverage;
    for (target, value) in [
        (&mut a.recorded_turns, b.recorded_turns),
        (&mut a.full_turn_reports, b.full_turn_reports),
        (&mut a.last_request_reports, b.last_request_reports),
        (&mut a.unspecified_reports, b.unspecified_reports),
        (&mut a.missing_reports, b.missing_reports),
        (&mut a.unfinished_turns, b.unfinished_turns),
    ] {
        *target = target
            .checked_add(value)
            .context("usage coverage overflow")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::state::TurnOutcomeKind;
    use mj_core::usage::{TokenUsage, UsageScope};

    fn completed(command: &str, ordinal: u64, input: u64) -> MaterializedTurnOutcome {
        serde_json::from_value(serde_json::json!({
            "command_id":command, "completed_ordinal":ordinal, "completed_at_ms":100,
            "outcome":{"kind":"completed","stop_reason":"EndTurn"},
            "usage":{"scope":"turn","total_tokens":input+10,"input_tokens":input,"output_tokens":10}
        }))
        .unwrap()
    }

    fn configuration(model: &str, effort: &str) -> mj_core::state::SessionConfiguration {
        serde_json::from_value(serde_json::json!({"model":model,"effort":effort})).unwrap()
    }

    #[test]
    fn turn_selections_follow_event_order_and_survive_cleanup_and_replay() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("usage.sqlite");
        save_session_to(&path, &super::super::tests::session("root", "project"))?;
        save_session_to(&path, &super::super::tests::session("child", "project"))?;
        let connection = open(&path)?;
        connection.execute(
            "INSERT INTO subagent_accounting VALUES ('child','root','investigate')",
            [],
        )?;
        drop(connection);
        let events = [
            MaterializedSessionMutation {
                configuration: Some(configuration("sol", "high")),
                active_turn: Some(Some(MaterializedTurn {
                    command_id: "one".into(),
                    accepted_ordinal: None,
                    turn_start_position: 1,
                    started_at_ms: 1,
                    steered_into: None,
                })),
                ..Default::default()
            },
            MaterializedSessionMutation {
                active_turn: Some(None),
                last_turn_outcome: Some(completed("one", 2, 20)),
                ..Default::default()
            },
            MaterializedSessionMutation {
                configuration: Some(configuration("luna", "low")),
                active_turn: Some(Some(MaterializedTurn {
                    command_id: "two".into(),
                    accepted_ordinal: None,
                    turn_start_position: 3,
                    started_at_ms: 3,
                    steered_into: None,
                })),
                ..Default::default()
            },
            MaterializedSessionMutation {
                active_turn: Some(None),
                last_turn_outcome: Some(completed("two", 4, 40)),
                ..Default::default()
            },
            MaterializedSessionMutation {
                active_turn: Some(Some(MaterializedTurn {
                    command_id: "crashed".into(),
                    accepted_ordinal: None,
                    turn_start_position: 5,
                    started_at_ms: 5,
                    steered_into: None,
                })),
                ..Default::default()
            },
        ];
        for _ in 0..2 {
            apply_projection_page_to(&path, "child", |page| {
                for (i, event) in events.iter().enumerate() {
                    page.apply(
                        i as u64 + 1,
                        &format!("{:064x}", i),
                        &format!("{:064x}", i + 1),
                        event,
                    )?;
                }
                Ok(())
            })?;
        }
        let before = load_session_usage_from(&path, "child", 0, 1)?.unwrap();
        assert_eq!(before.turn_selections["one"].model.as_deref(), Some("sol"));
        assert_eq!(before.by_model.len(), 2);
        assert_eq!(before.coverage.unfinished_turns, 1);
        assert_eq!(before.by_model[0].selection.effort.as_deref(), Some("low"));
        let later = load_session_usage_from(&path, "child", 2, 1)?.unwrap();
        assert_eq!(later.turn_selections["two"].model.as_deref(), Some("luna"));
        assert_eq!(before.totals["input_tokens"].tokens, 60);
        delete_session_from(&path, "child")?;
        delete_session_from(&path, "root")?;
        assert_eq!(
            before,
            load_session_usage_from(&path, "child", 0, 1)?.unwrap()
        );
        let tree = load_usage_tree_from(&path, "root")?.unwrap();
        assert_eq!(tree.sessions.len(), 2);
        assert_eq!(tree.summary.totals, before.totals);
        assert_eq!(tree.summary.by_model, before.by_model);
        assert_eq!(tree.summary.coverage.unfinished_turns, 1);
        assert!(tree.sessions.iter().all(|s| !s.operational_session_present));
        assert_eq!(tree.sessions[0].task_name.as_deref(), Some("investigate"));
        Ok(())
    }

    #[test]
    fn tree_keeps_registered_descendants_when_operational_state_is_removed() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("tree.sqlite");
        let mut state = State::default();
        for id in ["root", "child", "grandchild"] {
            state
                .sessions
                .insert(id.into(), super::super::tests::session(id, "project"));
        }
        {
            let (child, parent) = ("child", "root");
            let relation: SubagentRecord = serde_json::from_value(serde_json::json!({
                "child_session_id":child,"parent_session_id":parent,"task_name":child,
                "profile_id":"codex","working_directory":"","initial_prompt":"probe",
                "request_key":child,"created_at":"2026-09-30T00:00:00Z"
            }))?;
            state.subagents.insert(child.into(), relation);
        }
        save_state_to(&path, &state)?;
        // Operational children cannot delegate today; retained accounting
        // still traverses deeper historical/imported relationships.
        open(&path)?.execute(
            "INSERT INTO subagent_accounting VALUES ('grandchild','child','nested')",
            [],
        )?;
        let before = load_usage_tree_from(&path, "root")?.unwrap();
        assert_eq!(before.sessions.len(), 3);
        save_state_to(&path, &State::default())?;
        let after = load_usage_tree_from(&path, "root")?.unwrap();
        assert_eq!(after.sessions.len(), 3);
        assert!(
            after
                .sessions
                .iter()
                .all(|s| !s.operational_session_present)
        );
        Ok(())
    }

    #[test]
    fn provider_breakdowns_partition_counters_without_double_counting_or_guessing() -> Result<()> {
        use mj_core::usage::ProviderTurnUsage;
        let mut usage = completed("one", 1, 100).usage.unwrap();
        let mut first = usage.clone();
        first.input_tokens = 30;
        first.output_tokens = 4;
        first.total_tokens = 34;
        let mut second = first.clone();
        second.input_tokens = 60;
        second.output_tokens = 6;
        second.total_tokens = 66;
        usage.provider_details = Some(Box::new(ProviderTurnUsage {
            model_usage: BTreeMap::from([("a".into(), first), ("b".into(), second)]),
            ..Default::default()
        }));
        let selected = UsageSelection {
            model: Some("a".into()),
            effort: Some("high".into()),
        };
        let mut groups = BTreeMap::new();
        attribute(&usage, selected.clone(), &mut groups)?;
        assert_eq!(groups[&selected]["input_tokens"].tokens, 30);
        assert_eq!(
            groups[&UsageSelection {
                model: Some("b".into()),
                effort: None
            }]["input_tokens"]
                .tokens,
            60
        );
        assert_eq!(
            groups[&UsageSelection::default()]["input_tokens"].tokens,
            10
        );
        for (counter, total) in counters(&usage) {
            assert_eq!(
                groups
                    .values()
                    .filter_map(|v| v.get(&counter))
                    .map(|v| v.tokens)
                    .sum::<u64>(),
                total
            );
        }
        usage
            .provider_details
            .as_mut()
            .unwrap()
            .model_usage
            .get_mut("b")
            .unwrap()
            .scope = UsageScope::LastRequest;
        groups.clear();
        attribute(&usage, selected, &mut groups)?;
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[&UsageSelection::default()]["input_tokens"].tokens,
            100
        );
        Ok(())
    }

    #[test]
    fn usage_survives_reopening_and_replay_with_honest_coverage() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("usage.sqlite");
        let conn = open(&path)?;
        // Use the same projection setup as the materialized database tests.
        drop(conn);
        save_session_to(&path, &super::super::tests::session("usage", "project"))?;

        let mut turns = Vec::new();
        for (i, scope) in [
            Some(UsageScope::Turn),
            Some(UsageScope::Turn),
            Some(UsageScope::LastRequest),
            None,
            Some(UsageScope::Unspecified),
        ]
        .into_iter()
        .enumerate()
        {
            let turn = MaterializedTurnOutcome {
                diagnostic: Some(mj_core::diagnostic::TurnDiagnostic {
                    message: "Usage limit exceeded".into(),
                    code: Some("provider.auth_error".into()),
                    http_status: Some(403),
                    reset_at: None,
                }),
                command_id: format!("p{i}"),
                accepted_ordinal: None,
                turn_start_position: Some(i as u64 + 1),
                completed_ordinal: i as u64 + 1,
                completed_at_ms: 100,
                outcome: TurnOutcomeKind::Completed {
                    stop_reason: "EndTurn".into(),
                },
                usage: scope.map(|scope| TokenUsage {
                    provider_details: Some(Box::new(mj_core::usage::ProviderTurnUsage {
                        cost: Some(mj_core::usage::ProviderTurnCost::from_usd_ticks(
                            88_767_200, false,
                        )),
                        model_calls: Some(2),
                        api_duration_ms: Some(1700),
                        elapsed_ms: Some(2377),
                        model_usage: BTreeMap::new(),
                    })),
                    scope,
                    total_tokens: 30,
                    input_tokens: 20,
                    output_tokens: 10,
                    thought_tokens: None,
                    cached_read_tokens: Some(0),
                    cached_write_tokens: None,
                }),
            };
            turns.push(turn);
        }
        for _ in 0..2 {
            apply_projection_page_to(&path, "usage", |page| {
                for turn in &turns {
                    let prior = if turn.completed_ordinal == 1 {
                        mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.into()
                    } else {
                        format!("{:064x}", turn.completed_ordinal - 1)
                    };
                    page.apply(
                        turn.completed_ordinal,
                        &prior,
                        &format!("{:064x}", turn.completed_ordinal),
                        &MaterializedSessionMutation {
                            last_turn_outcome: Some(turn.clone()),
                            provider_cost: Some(ProviderCost {
                                amount: 1.25,
                                currency: "USD".into(),
                                observed_at_ms: 100,
                            }),
                            ..Default::default()
                        },
                    )?;
                }
                Ok(())
            })?;
        }
        let first = load_session_usage_from(&path, "usage", 0, 2)?.unwrap();
        assert_eq!(first.turns.len(), 2);
        let details = first.turns[0]
            .usage
            .as_ref()
            .unwrap()
            .provider_details
            .as_ref()
            .unwrap();
        assert_eq!(details.cost.as_ref().unwrap().usd, "0.0088767200");
        assert_eq!(details.elapsed_ms, Some(2377));
        assert_eq!(
            first.turns[0].diagnostic.as_ref().unwrap().http_status,
            Some(403)
        );
        assert_eq!(first.provider_session_cost.as_ref().unwrap().amount, 1.25);
        assert_eq!(first.coverage.recorded_turns, 5);
        assert_eq!(first.coverage.full_turn_reports, 2);
        assert_eq!(first.coverage.last_request_reports, 1);
        assert_eq!(first.coverage.unspecified_reports, 1);
        assert_eq!(first.coverage.missing_reports, 1);
        // Only the two whole-turn reports are summed: the last-request and
        // unspecified turns stay out of the totals.
        assert_eq!(first.totals["total_tokens"].tokens, 60);
        assert_eq!(first.totals["total_tokens"].reported_turns, 2);
        assert_eq!(first.totals["cached_read_tokens"].tokens, 0);
        assert!(!first.totals.contains_key("thought_tokens"));
        let next = load_session_usage_from(&path, "usage", first.next_after_seq, 2)?.unwrap();
        assert_eq!(next.turns.len(), 2);
        assert_eq!(next.latest_seq, 5);
        assert_eq!(next.totals, first.totals);
        let last = load_session_usage_from(&path, "usage", next.next_after_seq, 2)?.unwrap();
        assert_eq!(last.turns.len(), 1);
        assert_eq!(
            last.turns[0].usage.as_ref().unwrap().scope,
            UsageScope::Unspecified
        );
        assert_eq!(last.next_after_seq, last.latest_seq);
        assert_eq!(last.totals, first.totals);
        Ok(())
    }
}
