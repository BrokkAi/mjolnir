//! Reading the sessions Mjolnir manages out of a container engine's listing.

use super::*;

/// Managed session IDs in a container listing, each with the instance label
/// that created it when the container carries one.
pub fn managed_sessions_from_container_json(
    stdout: &[u8],
) -> Result<Vec<(String, Option<String>)>> {
    let values = serde_json::Deserializer::from_slice(stdout)
        .into_iter::<serde_json::Value>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse container list JSON")?;
    let mut sessions = Vec::new();
    for value in &values {
        collect_managed_sessions(value, &mut sessions);
    }
    sessions.sort();
    sessions.dedup();
    Ok(sessions)
}

fn collect_managed_sessions(
    value: &serde_json::Value,
    sessions: &mut Vec<(String, Option<String>)>,
) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                collect_managed_sessions(value, sessions);
            }
        }
        serde_json::Value::Object(object) => {
            for label_key in ["Labels", "labels"] {
                if let Some(labels) = object.get(label_key) {
                    let managed =
                        label_value(labels, MANAGED_LABEL).is_some_and(|value| value == "true");
                    if managed && let Some(session) = label_value(labels, SESSION_LABEL) {
                        sessions.push((session, label_value(labels, INSTANCE_LABEL)));
                    }
                }
            }
            for value in object.values() {
                collect_managed_sessions(value, sessions);
            }
        }
        _ => {}
    }
}

fn label_value(labels: &serde_json::Value, key: &str) -> Option<String> {
    match labels {
        serde_json::Value::Object(object) => object.get(key)?.as_str().map(str::to_owned),
        serde_json::Value::String(text) => text
            .split(',')
            .find_map(|label| {
                label
                    .trim()
                    .split_once('=')
                    .filter(|(name, _)| *name == key)
            })
            .map(|(_, value)| value.to_owned()),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetAvailability {
    Reachable,
    Missing,
    Unreachable,
}

/// Check the exact retained Apple container ID. Resume may start it once
/// presence is confirmed; read-only probes only list retained containers.
pub fn apple_container_availability(
    endpoint: ContainerEndpoint<'_>,
    executor: &impl CommandExecutor,
    start: bool,
) -> Result<TargetAvailability> {
    use TargetAvailability::{Missing, Reachable, Unreachable};

    let execute = |args: &[&str], purpose| -> Result<Option<Vec<u8>>> {
        let command = endpoint.host_command(
            SshTrafficClass::Control,
            CommandSpec::new("container", args.iter().copied()).purpose(purpose),
        );
        let output = executor
            .execute(&command)
            .with_context(|| command.purpose.clone())?;
        if output.status == 255 && endpoint.ssh.is_some() {
            return Ok(None);
        }
        ensure!(
            output.status == 0,
            "{} failed with status {}: {}",
            command.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(Some(output.stdout))
    };
    let list = |args: &[&str], purpose| -> Result<TargetAvailability> {
        let Some(stdout) = execute(args, purpose)? else {
            return Ok(Unreachable);
        };
        let listed = std::str::from_utf8(&stdout).context("decode Apple container list")?;
        Ok(
            if listed.lines().any(|id| id.trim() == endpoint.container_id) {
                Reachable
            } else {
                Missing
            },
        )
    };
    let retained = list(
        &["list", "--all", "--quiet"],
        "check retained Apple container before Restart",
    )?;
    if !start || retained != Reachable {
        return Ok(retained);
    }

    // The default list includes only running containers.
    let running = || {
        list(
            &["list", "--quiet"],
            "list running retained Apple containers for Resume",
        )
    };
    let current = running()?;
    if current != Missing {
        return Ok(current);
    }
    if execute(
        &["start", endpoint.container_id],
        "start retained Apple container for Resume",
    )?
    .is_none()
    {
        return Ok(Unreachable);
    }
    let started = running()?;
    ensure!(
        started != Missing,
        "retained Apple container did not become running"
    );
    Ok(started)
}
