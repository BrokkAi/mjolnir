use super::*;

/// Stable id of the standalone item that holds a terminal's output until a
/// tool call refers to it.
pub(super) fn terminal_item_id(terminal_id: &str) -> String {
    format!("terminal:{terminal_id}")
}

/// The terminal ids one stored ACP tool call refers to. This is the only place
/// that reads terminal content out of a call, so attaching output and
/// consuming a parked item cannot disagree about what a call refers to.
///
/// Content hel cannot read as an ACP block names no terminal; the renderer
/// already reports such a call as invalid, so this hides no failure.
pub(crate) fn tool_call_terminal_ids(call: &Value) -> Vec<String> {
    let Some(Value::Array(content)) = call.get("content") else {
        return Vec::new();
    };
    content
        .iter()
        .filter_map(|value| match ToolCallContent::deserialize(value) {
            Ok(ToolCallContent::Terminal(terminal)) => Some(terminal.terminal_id.0.to_string()),
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "ignoring malformed tool-call content while locating terminal output"
                );
                None
            }
        })
        .collect()
}

/// The output already recorded for `terminal_id`, wherever it is parked.
pub(super) fn find_terminal_record(
    index: &ProjectionIndex,
    terminal_id: &str,
) -> Option<TerminalOutputRecord> {
    index
        .get(&terminal_item_id(terminal_id))
        .and_then(|item| match &item.body {
            TranscriptBody::TerminalOutput { record } if record.terminal_id == terminal_id => {
                Some(record.clone())
            }
            _ => None,
        })
}

pub(super) fn replace_or_push_terminal_record(
    records: &mut Vec<TerminalOutputRecord>,
    record: TerminalOutputRecord,
) {
    match records
        .iter_mut()
        .find(|existing| existing.terminal_id == record.terminal_id)
    {
        Some(existing) => *existing = record,
        None => records.push(record),
    }
}

pub(super) fn fallback_terminal_tool_item_id(terminal_id: &str) -> String {
    format!(
        "tool:{}",
        mj_core::acp::fallback_terminal_tool_call_id(terminal_id)
    )
}

pub(super) fn fallback_tool_item(item: &TranscriptItem) -> Result<bool> {
    let TranscriptBody::Tool { call, .. } = &item.body else {
        return Ok(false);
    };
    let call = serde_json::from_value(call.clone()).context("parse fallback terminal tool call")?;
    Ok(mj_core::acp::is_fallback_terminal_tool_call(&call))
}

/// The one provider tool demonstrably owning a result through its raw value.
/// Identical concurrent results are deliberately left with the fallback tool.
pub(super) fn uniquely_matching_raw_tool(
    index: &ProjectionIndex,
    record: &TerminalOutputRecord,
) -> Option<Arc<TranscriptItem>> {
    let mut matching = index.transcript.values().filter_map(|item| {
        let TranscriptBody::Tool { call, .. } = &item.body else {
            return None;
        };
        if fallback_tool_item(item).ok()? {
            return None;
        }
        record
            .matches_tool_raw_result(call)
            .then(|| Arc::clone(item))
    });
    let item = matching.next()?;
    matching.next().is_none().then_some(item)
}

/// A provider may publish its real tool before it asks the client to create
/// the terminal. In that ordering the existing call already provides the
/// durable start item, so the compatibility call would only duplicate it.
pub(super) fn fallback_terminal_already_claimed(
    index: &ProjectionIndex,
    call: &ToolCall,
) -> Result<bool> {
    if !mj_core::acp::is_fallback_terminal_tool_call(call) {
        return Ok(false);
    }
    let value = serde_json::to_value(call)?;
    Ok(tool_call_terminal_ids(&value)
        .into_iter()
        .any(|terminal_id| {
            index.terminal_referrers(&terminal_id).any(|item| {
                let TranscriptBody::Tool { call, .. } = &item.body else {
                    return false;
                };
                serde_json::from_value::<ToolCall>(call.clone())
                    .is_ok_and(|call| !mj_core::acp::is_fallback_terminal_tool_call(&call))
            })
        }))
}

/// Replace Hel's interim terminal tool with the real ACP tool once the agent
/// supplies one. Output may already have landed on the interim item, so move
/// it along. A newly created real item can also adopt the interim item's start
/// identity; an existing tool keeps its immutable position and timestamp.
pub(super) fn consume_fallback_terminal_tools(
    index: &ProjectionIndex,
    mutation: &mut MaterializedSessionMutation,
    item: &mut TranscriptItem,
    may_adopt_identity: bool,
) -> Result<()> {
    let TranscriptBody::Tool {
        call,
        terminal_outputs,
        terminal_refs,
        ..
    } = &mut item.body
    else {
        return Ok(());
    };
    let materialized: ToolCall = serde_json::from_value(call.clone())
        .context("parse ACP tool call while claiming fallback terminal tools")?;
    if mj_core::acp::is_fallback_terminal_tool_call(&materialized) {
        return Ok(());
    }

    let mut terminal_ids = tool_call_terminal_ids(call);
    terminal_ids.extend(terminal_refs.iter().cloned());
    for terminal_id in terminal_ids {
        let stable_id = fallback_terminal_tool_item_id(&terminal_id);
        if stable_id == item.stable_id {
            continue;
        }
        let Some(fallback) = index.get(&stable_id) else {
            continue;
        };
        let TranscriptBody::Tool {
            call: fallback_call,
            terminal_outputs: fallback_outputs,
            terminal_refs: fallback_refs,
            ..
        } = &fallback.body
        else {
            continue;
        };
        let fallback_call: ToolCall = serde_json::from_value(fallback_call.clone())
            .context("parse fallback terminal tool call")?;
        if !mj_core::acp::is_fallback_terminal_tool_call(&fallback_call) {
            continue;
        }
        for record in fallback_outputs {
            replace_or_push_terminal_record(terminal_outputs, record.clone());
        }
        for terminal_ref in fallback_refs {
            if !terminal_refs.contains(terminal_ref) {
                terminal_refs.push(terminal_ref.clone());
            }
        }
        if !terminal_refs.contains(&terminal_id) {
            terminal_refs.push(terminal_id);
        }
        if may_adopt_identity {
            item.position = item.position.min(fallback.position);
            item.created_at_ms = item.created_at_ms.min(fallback.created_at_ms);
        }
        item.last_changed_at_ms = item.last_changed_at_ms.max(fallback.last_changed_at_ms);
        mutation
            .transcript
            .push(TranscriptMutation::Remove { stable_id });
    }
    Ok(())
}

/// Terminal close is a Hel event rather than an ACP tool update. Complete only
/// the interim call Hel created; a provider-owned call keeps its own status.
pub(super) fn finalize_fallback_terminal_tool(item: &mut TranscriptItem) -> Result<()> {
    let TranscriptBody::Tool {
        call,
        terminal_outputs,
        ..
    } = &mut item.body
    else {
        return Ok(());
    };
    if terminal_outputs.is_empty() {
        return Ok(());
    }
    let mut materialized: ToolCall = serde_json::from_value(call.clone())
        .context("parse ACP tool call while finalizing fallback terminal tool")?;
    if !mj_core::acp::is_fallback_terminal_tool_call(&materialized) {
        return Ok(());
    }
    materialized.status = if terminal_outputs
        .iter()
        .all(TerminalOutputRecord::exited_cleanly)
    {
        ToolCallStatus::Completed
    } else {
        ToolCallStatus::Failed
    };
    *call = serde_json::to_value(materialized)?;
    Ok(())
}

/// The one parked terminal result an ACP tool demonstrably owns through its
/// raw result. Ambiguous identical results stay standalone rather than being
/// assigned to an arbitrary concurrent tool.
pub(super) fn uniquely_matching_raw_terminal(
    current: &MaterializedSession,
    call: &Value,
) -> Option<String> {
    let mut matching = current.transcript.iter().filter_map(|item| {
        let record = match &item.body {
            TranscriptBody::TerminalOutput { record } => record,
            TranscriptBody::Tool {
                terminal_outputs, ..
            } if fallback_tool_item(item).ok()? => terminal_outputs.first()?,
            _ => return None,
        };
        record
            .matches_tool_raw_result(call)
            .then(|| record.terminal_id.clone())
    });
    let terminal_id = matching.next()?;
    matching.next().is_none().then_some(terminal_id)
}

/// Remember every terminal the current call refers to, move any parked output
/// for the terminals `item` has ever referred to into the item, and remove the
/// standalone items it consumed. An exact provider raw result also claims its
/// one matching parked terminal: Kimi supplies that result but no ACP terminal
/// reference. Output that arrives before the tool call therefore ends up
/// exactly where output that arrives after it does, and a call that drops its
/// terminal reference on a later content update still owns the terminal it
/// started.
pub(super) fn attach_terminal_outputs(
    current: &MaterializedSession,
    index: &ProjectionIndex,
    mutation: &mut MaterializedSessionMutation,
    item: &mut TranscriptItem,
) {
    let TranscriptBody::Tool {
        call,
        terminal_outputs,
        terminal_refs,
        ..
    } = &mut item.body
    else {
        return;
    };
    for terminal_id in tool_call_terminal_ids(call) {
        if !terminal_refs.contains(&terminal_id) {
            terminal_refs.push(terminal_id);
        }
    }
    if terminal_refs.is_empty()
        && let Some(terminal_id) = uniquely_matching_raw_terminal(current, call)
        && !terminal_refs.contains(&terminal_id)
    {
        terminal_refs.push(terminal_id);
    }
    let mut consumed = Vec::new();
    for terminal_id in terminal_refs.iter() {
        let Some(record) = find_terminal_record(index, terminal_id) else {
            continue;
        };
        replace_or_push_terminal_record(terminal_outputs, record);
        consumed.push(terminal_item_id(terminal_id));
    }
    for stable_id in consumed {
        mutation
            .transcript
            .push(TranscriptMutation::Remove { stable_id });
    }
}

pub(super) fn canonical_terminal_output(record: &TerminalOutputRecord) -> CanonicalTerminalOutput {
    CanonicalTerminalOutput {
        terminal_id: record.terminal_id.clone(),
        output: record.output.clone(),
        truncated: record.truncated,
        exit_code: record.exit_code,
        signal: record.signal.clone(),
    }
}

pub(super) fn materialized_terminal_output(
    record: &CanonicalTerminalOutput,
) -> TerminalOutputRecord {
    TerminalOutputRecord {
        terminal_id: record.terminal_id.clone(),
        output: record.output.clone(),
        truncated: record.truncated,
        exit_code: record.exit_code,
        signal: record.signal.clone(),
    }
}
