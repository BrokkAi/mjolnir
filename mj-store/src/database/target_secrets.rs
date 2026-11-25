//! Moves resolved secret values out of recorded target runtime settings.
//!
//! Revision 157 records a target's secret-sourced environment entries as
//! references. Rows written before it hold the resolved values in
//! `environment`, and the database cannot tell which values were secrets.
//! The configuration can: this rewrites each recorded runtime so that every
//! key the session's target now defines as a reference becomes that reference.
//! It is idempotent, so it runs at every daemon start.
use super::*;
use mj_core::config::EnvironmentValue;
use serde_json::Value;
use std::collections::BTreeSet;

/// What the configuration says about secret-sourced target environment
/// entries.
#[derive(Debug, Default)]
pub struct TargetEnvironmentReferences {
    /// Each target's references, by target template id. Targets without one
    /// are omitted.
    by_target: BTreeMap<String, BTreeMap<String, EnvironmentValue>>,
    /// Keys some target gives a plain value.
    literal_keys: BTreeSet<String>,
}

impl TargetEnvironmentReferences {
    pub fn from_config(config: &mj_core::config::Config) -> Self {
        let mut references = Self::default();
        for (id, template) in &config.targets {
            let Some(container) = template.container() else {
                continue;
            };
            let mut named = BTreeMap::new();
            for (key, value) in container.environment.sources() {
                if value.is_reference() {
                    named.insert(key.clone(), value.clone());
                } else {
                    references.literal_keys.insert(key.clone());
                }
            }
            if !named.is_empty() {
                references.by_target.insert(id.clone(), named);
            }
        }
        references
    }

    /// References for a runtime whose target is not recorded beside it: a key
    /// counts only when every target that names it agrees it is the same
    /// reference, so a plain value another target gives it is never rewritten.
    fn without_a_target(&self) -> BTreeMap<String, EnvironmentValue> {
        let mut agreed: BTreeMap<String, Option<EnvironmentValue>> = BTreeMap::new();
        for references in self.by_target.values() {
            for (key, reference) in references {
                agreed
                    .entry(key.clone())
                    .and_modify(|known| {
                        if known.as_ref() != Some(reference) {
                            *known = None;
                        }
                    })
                    .or_insert_with(|| Some(reference.clone()));
            }
        }
        agreed
            .into_iter()
            .filter_map(|(key, reference)| Some((key, reference?)))
            .filter(|(key, _)| !self.literal_keys.contains(key))
            .collect()
    }
}

/// Move the entries `references` defines out of `runtime.environment` into
/// `runtime.environment_references`. Returns whether anything changed.
fn scrub_runtime(
    runtime: &mut Value,
    references: &BTreeMap<String, EnvironmentValue>,
) -> Result<bool> {
    let Some(runtime) = runtime.as_object_mut() else {
        return Ok(false);
    };
    let Some(environment) = runtime
        .get_mut("environment")
        .and_then(Value::as_object_mut)
    else {
        return Ok(false);
    };
    let keys = environment
        .keys()
        .filter(|key| references.contains_key(*key))
        .cloned()
        .collect::<Vec<_>>();
    if keys.is_empty() {
        return Ok(false);
    }
    for key in &keys {
        environment.remove(key);
    }
    let recorded = runtime
        .entry("environment_references")
        .or_insert_with(|| Value::Object(Default::default()));
    let Some(recorded) = recorded.as_object_mut() else {
        bail!("recorded environment_references is not a table");
    };
    for key in keys {
        let reference = serde_json::to_value(&references[&key])?;
        recorded.insert(key, reference);
    }
    Ok(true)
}

/// A runtime at `runtime_pointer`, and where beside it the row names its
/// target template (`None`: the row's own `row_template`, else unknown).
type RuntimeLocation = (&'static str, Option<&'static str>);

fn scrub_value(
    value: &mut Value,
    locations: &[RuntimeLocation],
    row_template: Option<&str>,
    references: &TargetEnvironmentReferences,
    without_a_target: &BTreeMap<String, EnvironmentValue>,
) -> Result<bool> {
    let mut changed = false;
    for (runtime_pointer, template_pointer) in locations {
        let template = match template_pointer {
            Some(pointer) => value.pointer(pointer).and_then(Value::as_str),
            None => row_template,
        }
        .map(str::to_owned);
        let named = match &template {
            Some(id) => match references.by_target.get(id) {
                Some(named) => named,
                None => continue,
            },
            None => without_a_target,
        };
        if let Some(runtime) = value.pointer_mut(runtime_pointer) {
            changed |= scrub_runtime(runtime, named)?;
        }
    }
    Ok(changed)
}

/// A table that persists a copy of `TargetRuntimeSettings` outside `sessions`
/// (the locations `container_size` converts): its key, JSON column, where each
/// runtime sits in that JSON, and an SQL expression for the target template of
/// runtimes that do not record one beside them.
struct RuntimeCopies {
    table: &'static str,
    id_column: &'static str,
    json_column: &'static str,
    locations: &'static [RuntimeLocation],
    row_template: &'static str,
}

const COPIES: [RuntimeCopies; 4] = [
    RuntimeCopies {
        table: "session_moves",
        id_column: "operation_id",
        json_column: "operation_json",
        locations: &[
            (
                "/recovery_session/target_runtime",
                Some("/recovery_session/target_template_id"),
            ),
            (
                "/workspace_transfer/source/target_runtime",
                Some("/workspace_transfer/source/target_template_id"),
            ),
            // A prepared destination belongs to the target the Move selected,
            // or to the source target for an in-place Move.
            ("/prepared_destination/runtime", None),
        ],
        row_template: "COALESCE(json_extract(operation_json,'$.selection.target_template_id'),
                                json_extract(operation_json,'$.source_target_template_id'))",
    },
    RuntimeCopies {
        table: "retained_move_sources",
        id_column: "operation_id",
        json_column: "source_json",
        locations: &[("/target_runtime", Some("/target_template_id"))],
        row_template: "NULL",
    },
    RuntimeCopies {
        table: "environments",
        id_column: "environment_id",
        json_column: "placement_json",
        locations: &[
            ("/runtime", Some("/target_template_id")),
            ("/prepared_destination/destination/runtime", None),
        ],
        row_template: "NULL",
    },
    RuntimeCopies {
        table: "environment_workers",
        id_column: "lease_id",
        json_column: "runtime_json",
        locations: &[("", None)],
        row_template: "(SELECT target_template_id FROM sessions
                         WHERE sessions.session_id=environment_workers.worker_session_id)",
    },
];

/// Rewrite every recorded runtime that still holds a value the current
/// configuration defines as a reference. Returns the rows changed.
pub fn scrub_resolved_target_secrets(config: &mj_core::config::Config) -> Result<usize> {
    let references = TargetEnvironmentReferences::from_config(config);
    if references.by_target.is_empty() {
        return Ok(0);
    }
    submit_database_write("scrub_resolved_target_secrets", move |connection| {
        scrub_resolved_target_secrets_in(connection, &references)
    })
}

fn scrub_resolved_target_secrets_in(
    connection: &mut Connection,
    references: &TargetEnvironmentReferences,
) -> Result<usize> {
    let without_a_target = references.without_a_target();
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut changed = 0;
    let sessions = tx
        .prepare(
            "SELECT session_id, target_template_id, target_runtime_json FROM sessions
             WHERE target_runtime_json IS NOT NULL",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (session_id, template, json) in sessions {
        let Some(named) = references.by_target.get(&template) else {
            continue;
        };
        let mut runtime: Value = serde_json::from_str(&json)
            .with_context(|| format!("read recorded target runtime of session {session_id}"))?;
        if scrub_runtime(&mut runtime, named)? {
            tx.execute(
                "UPDATE sessions SET target_runtime_json=?2 WHERE session_id=?1",
                params![session_id, serde_json::to_string(&runtime)?],
            )?;
            changed += 1;
        }
    }
    for copies in COPIES {
        let RuntimeCopies {
            table,
            id_column,
            json_column,
            locations,
            row_template,
        } = copies;
        let rows = tx
            .prepare(&format!(
                "SELECT {id_column}, {json_column}, {row_template} FROM {table}
                 WHERE {json_column} IS NOT NULL"
            ))?
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, json, row_template) in rows {
            let mut value: Value = serde_json::from_str(&json)
                .with_context(|| format!("read {table} row {id} to remove resolved secrets"))?;
            if scrub_value(
                &mut value,
                locations,
                row_template.as_deref(),
                references,
                &without_a_target,
            )? {
                tx.execute(
                    &format!("UPDATE {table} SET {json_column}=?2 WHERE {id_column}=?1"),
                    params![id, serde_json::to_string(&value)?],
                )?;
                changed += 1;
            }
        }
    }
    tx.commit()?;
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::state::{TargetConnection, TargetRuntimeSettings};

    fn runtime(environment: &[(&str, &str)]) -> TargetRuntimeSettings {
        TargetRuntimeSettings {
            kind: "local-podman".into(),
            connection: TargetConnection::Local,
            execution_policy: mj_core::config::ExecutionPolicy::ConfiguredApprovals,
            environment: environment
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            environment_references: Default::default(),
            tool_cache: None,
            container_settings: None,
        }
    }

    // Rows written before secrets were recorded as references hold the resolved
    // value in every copy of the runtime; the scrub must reach them all, leave
    // literals and other targets alone, and be a no-op the second time.
    #[test]
    fn scrub_moves_resolved_secrets_out_of_every_recorded_runtime_and_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        let mut session = super::super::tests::session("scrubbed", "project");
        session.target_template_id = "sandbox".into();
        session.target_runtime = Some(std::sync::Arc::new(runtime(&[
            ("API_KEY", "sk-secret-value"),
            ("MODE", "fast"),
        ])));
        let mut other = super::super::tests::session("other-target", "project");
        other.target_template_id = "elsewhere".into();
        other.target_runtime = Some(std::sync::Arc::new(runtime(&[("API_KEY", "literal-here")])));
        save_session_to(&path, &session).unwrap();
        save_session_to(&path, &other).unwrap();
        let mut connection = open(&path).unwrap();
        // Saving a session records its physical environment, with a copy of
        // the runtime, and a Move in flight records a prepared destination
        // whose target is not named beside it.
        let placement: String = connection
            .query_row(
                "SELECT placement_json FROM environments WHERE owner_session_id='scrubbed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let mut placement: serde_json::Value = serde_json::from_str(&placement).unwrap();
        placement["prepared_destination"] = serde_json::json!({
            "operation_id": "move-1",
            "destination": {"runtime": runtime(&[("API_KEY", "sk-secret-value")])},
        });
        connection
            .execute(
                "UPDATE environments SET placement_json=?1 WHERE owner_session_id='scrubbed'",
                [placement.to_string()],
            )
            .unwrap();
        let mut templates = TargetEnvironmentReferences {
            by_target: BTreeMap::from([(
                "sandbox".to_owned(),
                BTreeMap::from([(
                    "API_KEY".to_owned(),
                    EnvironmentValue::FromSecret("stored".into()),
                )]),
            )]),
            literal_keys: BTreeSet::new(),
        };
        let changed = scrub_resolved_target_secrets_in(&mut connection, &templates).unwrap();
        // The session row, its environment, and its worker lease.
        assert_eq!(changed, 3);
        let reference = EnvironmentValue::FromSecret("stored".into());
        let scrubbed = state_io::load_session_with(&connection, "scrubbed")
            .unwrap()
            .unwrap();
        let recorded = scrubbed.target_runtime.unwrap();
        assert_eq!(
            recorded.environment,
            BTreeMap::from([("MODE".to_owned(), "fast".to_owned())])
        );
        assert_eq!(
            recorded.environment_references,
            BTreeMap::from([("API_KEY".to_owned(), reference)])
        );
        let untouched = state_io::load_session_with(&connection, "other-target")
            .unwrap()
            .unwrap();
        assert_eq!(
            untouched.target_runtime.unwrap().environment["API_KEY"],
            "literal-here"
        );
        let stored: String = connection
            .query_row(
                "SELECT group_concat(target_runtime_json) FROM sessions",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!stored.contains("sk-secret-value"), "{stored}");
        let placement: String = connection
            .query_row(
                "SELECT placement_json FROM environments WHERE owner_session_id='scrubbed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!placement.contains("sk-secret-value"), "{placement}");
        assert_eq!(
            placement.matches("environment_references").count(),
            2,
            "{placement}"
        );
        let leases: String = connection
            .query_row(
                "SELECT group_concat(runtime_json) FROM environment_workers",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            leases.contains("environment_references") && !leases.contains("sk-secret-value"),
            "{leases}"
        );
        assert_eq!(
            scrub_resolved_target_secrets_in(&mut connection, &templates).unwrap(),
            0
        );
        // A key another target gives a plain value is never guessed at for a
        // runtime that does not name its target.
        templates.literal_keys.insert("API_KEY".into());
        assert!(templates.without_a_target().is_empty());
    }
}
