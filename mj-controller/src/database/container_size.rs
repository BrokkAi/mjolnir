//! Breaking revision 132: one durable per-session container size.
use super::*;

/// Parse positive decimal engine sizes exactly, rounding up only the final unit.
fn decimal_units(text: &str, multiplier: u64) -> Option<u64> {
    let text = text.trim();
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let whole = if whole.is_empty() {
        0
    } else {
        whole.parse::<u64>().ok()?
    };
    let mut bytes = whole.checked_mul(multiplier)?;
    // Trim trailing zeros so long decimal representations remain exact.
    let fraction = fraction.trim_end_matches('0');
    if !fraction.is_empty() {
        let denominator = 10_u128.checked_pow(fraction.len().try_into().ok()?)?;
        let numerator = fraction
            .parse::<u128>()
            .ok()?
            .checked_mul(multiplier.into())?;
        let rounded = numerator / denominator + u128::from(numerator % denominator != 0);
        bytes = bytes.checked_add(rounded.try_into().ok()?)?;
    }
    (bytes > 0).then_some(bytes)
}

fn memory_bytes(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let multiplier = match text[split..].to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => return None,
    };
    decimal_units(&text[..split], multiplier)
}

fn fold(
    allocation: Option<SessionResourceAllocation>,
    cpus: Option<&str>,
    memory: Option<&str>,
) -> Option<SessionResourceAllocation> {
    if matches!(allocation, Some(SessionResourceAllocation::AwsEc2 { .. })) {
        return allocation;
    }
    let (old_cpus, old_memory) = match &allocation {
        Some(SessionResourceAllocation::Container { cpus, memory_bytes }) => {
            (Some(*cpus), Some(*memory_bytes))
        }
        _ => (None, None),
    };
    match (
        cpus.and_then(|text| decimal_units(text, 1)).or(old_cpus),
        memory.and_then(memory_bytes).or(old_memory),
    ) {
        (Some(cpus), Some(memory_bytes)) => {
            Some(SessionResourceAllocation::Container { cpus, memory_bytes })
        }
        _ => allocation,
    }
}

/// Use persisted type locations rather than interpreting arbitrary JSON keys
/// (a runtime environment map may itself contain a variable named container_cpus).
fn rewrite_embedded_sizes(
    value: &mut serde_json::Value,
    session_paths: &[&str],
    snapshot_paths: &[&str],
    changes: &mut Vec<serde_json::Value>,
) -> Result<()> {
    for path in session_paths {
        if let Some(serde_json::Value::Object(fields)) = value.pointer_mut(path) {
            fold_fields(fields, "container_cpus", "container_memory", path, changes)?;
        }
    }
    for path in snapshot_paths {
        if let Some(serde_json::Value::Object(fields)) = value.pointer_mut(path) {
            fold_fields(fields, "cpus", "memory", path, changes)?;
        }
    }
    Ok(())
}

fn fold_json_allocation(
    allocation: Option<&serde_json::Value>,
    cpus: Option<&serde_json::Value>,
    memory: Option<&serde_json::Value>,
) -> Result<Option<SessionResourceAllocation>> {
    let allocation = allocation
        .cloned()
        .map(serde_json::from_value::<Option<SessionResourceAllocation>>)
        .transpose()?
        .flatten();
    Ok(fold(
        allocation,
        cpus.and_then(serde_json::Value::as_str),
        memory.and_then(serde_json::Value::as_str),
    ))
}

/// Legacy failed-Move Resume restored this allocation together with the
/// recovery session's override strings. Preserve that effective source size.
fn rewrite_move_source(
    value: &mut serde_json::Value,
    inherited_cpus: Option<&str>,
    inherited_memory: Option<&str>,
    changes: &mut Vec<serde_json::Value>,
) -> Result<()> {
    let Some(record) = value.as_object_mut() else {
        return Ok(());
    };
    let recovery = record
        .get("recovery_session")
        .filter(|record| !record.is_null());
    // Without a recovery snapshot, legacy Resume kept the session row's strings.
    let (cpus, memory) = match recovery {
        Some(record) => (
            record.get("container_cpus").cloned(),
            record.get("container_memory").cloned(),
        ),
        None => (
            inherited_cpus.map(|s| serde_json::Value::String(s.into())),
            inherited_memory.map(|s| serde_json::Value::String(s.into())),
        ),
    };
    if cpus.is_none() && memory.is_none() {
        return Ok(());
    }
    let allocation = fold_json_allocation(
        record.get("source_resource_allocation"),
        cpus.as_ref(),
        memory.as_ref(),
    )?;
    record.insert(
        "source_resource_allocation".into(),
        serde_json::to_value(&allocation)?,
    );
    changes.push(serde_json::json!({"path":"/source_resource_allocation","old_cpus":cpus,"old_memory":memory,"allocation":allocation}));
    Ok(())
}

fn fold_fields(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    cpu_key: &str,
    memory_key: &str,
    path: &str,
    changes: &mut Vec<serde_json::Value>,
) -> Result<()> {
    let cpus = fields.remove(cpu_key);
    let memory = fields.remove(memory_key);
    if cpus.is_none() && memory.is_none() {
        return Ok(());
    }
    let allocation = fold_json_allocation(
        fields.get("resource_allocation"),
        cpus.as_ref(),
        memory.as_ref(),
    )?;
    fields.insert(
        "resource_allocation".into(),
        serde_json::to_value(&allocation)?,
    );
    changes.push(serde_json::json!({"path":path,"old_cpus":cpus,"old_memory":memory,"allocation":allocation}));
    Ok(())
}

pub(super) struct Conversion {
    table: &'static str,
    record_id: String,
    session_id: String,
    cpus: Option<String>,
    memory: Option<String>,
    allocation: Option<SessionResourceAllocation>,
    snapshot: Option<serde_json::Value>,
}

/// Return the changed/dropped rows after committing so no rolled-back change is reported.
pub(super) fn migrate(connection: &Connection) -> Result<Vec<Conversion>> {
    let tx = connection.unchecked_transaction()?;
    let rows = {
        let mut statement = tx.prepare("SELECT session_id, container_cpus, container_memory, resource_allocation, target_runtime_json FROM sessions")?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut changes = Vec::new();
    for (session_id, cpus, memory, allocation, runtime) in rows {
        let allocation = allocation
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        let allocation = fold(allocation, cpus.as_deref(), memory.as_deref());
        let mut runtime: Option<serde_json::Value> =
            runtime.as_deref().map(serde_json::from_str).transpose()?;
        let mut details = Vec::new();
        if let Some(runtime) = &mut runtime {
            rewrite_embedded_sizes(runtime, &[], &["/container_settings"], &mut details)?;
        }
        let snapshot = (!details.is_empty()).then(|| serde_json::Value::Array(details));
        if cpus.is_some() || memory.is_some() || snapshot.is_some() {
            tx.execute("UPDATE sessions SET resource_allocation=?2, target_runtime_json=?3 WHERE session_id=?1", params![session_id, allocation.as_ref().map(serde_json::to_string).transpose()?, runtime.as_ref().map(serde_json::to_string).transpose()?])?;
            changes.push(Conversion {
                table: "sessions",
                record_id: session_id.clone(),
                session_id,
                cpus,
                memory,
                allocation,
                snapshot,
            });
        }
    }
    // These durable records also embed SessionRecord or TargetRuntimeSettings.
    // Convert all copies before committing the new incompatible schema revision.
    for (table, id_column, session_column, json_column, session_paths, snapshot_paths) in [
        (
            "session_moves",
            "operation_id",
            "session_id",
            "operation_json",
            &["/recovery_session", "/workspace_transfer/source"][..],
            &[
                "/recovery_session/target_runtime/container_settings",
                "/workspace_transfer/source/target_runtime/container_settings",
                "/prepared_destination/runtime/container_settings",
            ][..],
        ),
        (
            "retained_move_sources",
            "operation_id",
            "session_id",
            "source_json",
            &[""][..],
            &["/target_runtime/container_settings"][..],
        ),
        (
            "environments",
            "environment_id",
            "owner_session_id",
            "placement_json",
            &[][..],
            &[
                "/runtime/container_settings",
                "/prepared_destination/destination/runtime/container_settings",
            ][..],
        ),
        (
            "environment_workers",
            "lease_id",
            "worker_session_id",
            "runtime_json",
            &[][..],
            &["/container_settings"][..],
        ),
    ] {
        let rows = {
            let mut statement = tx.prepare(&format!("SELECT {id_column}, {session_column}, {json_column} FROM {table} WHERE {json_column} IS NOT NULL"))?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (record_id, session_id, json) in rows {
            let mut value: serde_json::Value = serde_json::from_str(&json).with_context(|| {
                format!("read {table} row {record_id} for container size conversion")
            })?;
            let mut details = Vec::new();
            if table == "session_moves" {
                let (cpus, memory): (Option<String>, Option<String>) = if value
                    .get("recovery_session")
                    .is_none_or(serde_json::Value::is_null)
                {
                    tx.query_row(
                        "SELECT container_cpus,container_memory FROM sessions WHERE session_id=?1",
                        [&session_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?
                    .unwrap_or_default()
                } else {
                    (None, None)
                };
                rewrite_move_source(&mut value, cpus.as_deref(), memory.as_deref(), &mut details)?;
            }
            rewrite_embedded_sizes(&mut value, session_paths, snapshot_paths, &mut details)
                .with_context(|| format!("convert container sizes in {table} row {record_id}"))?;
            if !details.is_empty() {
                tx.execute(
                    &format!("UPDATE {table} SET {json_column}=?2 WHERE {id_column}=?1"),
                    params![record_id, serde_json::to_string(&value)?],
                )?;
                changes.push(Conversion {
                    table,
                    record_id,
                    session_id,
                    cpus: None,
                    memory: None,
                    allocation: None,
                    snapshot: Some(serde_json::Value::Array(details)),
                });
            }
        }
    }
    tx.execute_batch("ALTER TABLE sessions DROP COLUMN container_cpus;
        ALTER TABLE sessions DROP COLUMN container_memory;
        UPDATE schema_compatibility SET minimum_compatible_version=132 WHERE singleton=1;
        INSERT INTO schema_migrations(version,applied_at) VALUES(132,strftime('%Y-%m-%dT%H:%M:%fZ','now'));
        PRAGMA user_version=132;")?;
    tx.commit()?;
    Ok(changes)
}

pub(super) fn report(changes: Vec<Conversion>) {
    for change in changes {
        tracing::warn!(table = change.table, record_id = change.record_id, session_id = change.session_id, old_cpus = ?change.cpus, old_memory = ?change.memory, allocation = ?change.allocation, snapshot = ?change.snapshot, "converted or dropped legacy container size overrides");
    }
}
