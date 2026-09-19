use super::*;

/// Hand a Claude worker the profile's long-lived setup token, when it has one.
///
/// Claude Code reads `CLAUDE_CODE_OAUTH_TOKEN` ahead of the `/login`
/// credentials file, and a setup token does not rotate, so a container copy
/// cannot lose the single-use refresh race with the host. A profile that sets
/// the variable itself stays authoritative.
pub(in crate::controller) fn apply_claude_setup_token(
    environment: &mut std::collections::BTreeMap<String, String>,
    kind: mj_core::config::HarnessKind,
    token_path: &Path,
) {
    use mj_core::credentials::CLAUDE_OAUTH_TOKEN_ENV;

    if kind != mj_core::config::HarnessKind::Claude
        || environment.contains_key(CLAUDE_OAUTH_TOKEN_ENV)
    {
        return;
    }
    match mj_core::credentials::read_claude_oauth_token(token_path) {
        Ok(Some(token)) => {
            environment.insert(CLAUDE_OAUTH_TOKEN_ENV.to_owned(), token);
        }
        Ok(None) => {}
        // A stored token Hel cannot read is worth reporting, but the session
        // still starts on the synced credentials file.
        Err(error) => tracing::warn!(
            path = %token_path.display(),
            %error,
            "ignoring an unreadable Claude setup token"
        ),
    }
}

/// Kimi's runtime-aware engine cannot infer a runtime identity from an ACP
/// stdio server. Add Hel's server to the session-private profile instead,
/// where Kimi's native schema can bind it to the target's local runtime.
pub(super) fn configure_kimi_project_memory_mcp(
    profile_stage: &Path,
    worker_root: &str,
    memory: &ProjectMemoryLaunchConfig,
) -> Result<()> {
    let path = profile_stage.join("mcp.json");
    edit_staged_json_object(&path, "staged Kimi MCP configuration", |root| {
        let servers = root
            .entry("mcpServers")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .with_context(|| {
                format!(
                    "mcpServers in staged Kimi MCP configuration {} must be a JSON object",
                    path.display()
                )
            })?;

        let worker = Path::new(worker_root).join("hel");
        let server = if worker.is_absolute() && memory.root.is_absolute() {
            let mut args = vec![
                "worker".to_owned(),
                "memory-mcp".into(),
                "--root".into(),
                memory.root.to_string_lossy().into_owned(),
            ];
            if let Some(socket) = &memory.history_socket {
                args.extend([
                    "--history-socket".into(),
                    socket.to_string_lossy().into_owned(),
                ]);
            }
            serde_json::json!({
                "transport": "stdio",
                "command": worker,
                "args": args,
                "runtime_id": "local"
            })
        } else {
            let worker = worker.to_string_lossy();
            let memory_root = memory.root.to_string_lossy();
            let mut args = vec![
                "-c".to_owned(),
                "exec \"$HOME/$1\" worker memory-mcp --root \"$HOME/$2\"".into(),
                "mj-memory".into(),
                worker.into_owned(),
                memory_root.into_owned(),
            ];
            if let Some(socket) = &memory.history_socket {
                args[1].push_str(if socket.is_absolute() {
                    " --history-socket \"$3\""
                } else {
                    " --history-socket \"$HOME/$3\""
                });
                args.push(socket.to_string_lossy().into_owned());
            }
            serde_json::json!({
                "transport": "stdio",
                "command": "sh",
                "args": args,
                "runtime_id": "local"
            })
        };
        servers.insert("mj-memory".into(), server);
        Ok(())
    })
}

/// Claude reads MCP servers from its private profile rather than ACP. Parent
/// sessions always use an isolated staged profile, including on local bare
/// targets, so this never modifies the user's source profile.
pub(super) fn configure_claude_subagent_mcp(profile_stage: &Path, worker_root: &str) -> Result<()> {
    let path = profile_stage.join(".claude.json");
    edit_staged_json_object(&path, "staged Claude configuration", |root| {
        let servers = root
            .entry("mcpServers")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .with_context(|| {
                format!(
                    "mcpServers in staged Claude configuration {} must be a JSON object",
                    path.display()
                )
            })?;
        servers.insert(
            "mj-agents".into(),
            serde_json::json!({
                "type":"stdio",
                "command":Path::new(worker_root).join("hel"),
                "args":[
                    "worker",
                    "subagent-mcp",
                    "--socket",
                    Path::new(worker_root).join(mj_worker_socket_name()),
                    // Claude's own limit is on silence, which the server's
                    // progress notifications break, so this keeps the full
                    // advertised wait. Naming the harness anyway keeps both
                    // delivery paths explicit.
                    "--harness",
                    mj_core::config::HarnessKind::Claude.id()
                ]
            }),
        );
        Ok(())
    })
}

/// Write Mjolnir's managed skills into a staged profile home.
///
/// This is the launch half of the invariant that
/// [`mj_core::skills::session_skills`] defines: a managed skill overwrites a
/// user skill at the same path, so the staged tree fingerprints the same as
/// the archive the first credential sync pushes and nothing is wiped.
pub(super) fn stage_managed_skills(
    kind: mj_core::config::HarnessKind,
    profile_stage: &Path,
) -> Result<()> {
    for entry in mj_core::skills::managed_skills(kind) {
        let path = profile_stage.join(&entry.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create staged skills directory {}", parent.display()))?;
        }
        atomic_write(&path, &entry.bytes)
            .with_context(|| format!("write managed skill {}", path.display()))?;
    }
    Ok(())
}

/// Write the enforcement table's staged setting, if the harness has one. Muse
/// composes a session's permission profile from its settings file and nothing
/// on the ACP wire overrides that choice, so the profile has to be staged.
pub(super) fn apply_staged_execution_setting(
    kind: mj_core::config::HarnessKind,
    policy: mj_core::config::ExecutionPolicy,
    profile_stage: &Path,
) -> Result<()> {
    let Some(setting) = kind
        .execution_enforcement(policy)
        .and_then(mj_core::config::ExecutionEnforcement::staged_setting)
    else {
        return Ok(());
    };
    let path = profile_stage.join(setting.file);
    let label = format!("staged {} settings", kind.display_name());
    edit_staged_json_object(&path, &label, |root| {
        setting
            .apply(root)
            .with_context(|| format!("{label} {}", path.display()))
    })
}

/// Read a staged JSON settings file (treating a missing file as an empty
/// object), let `edit` change its root object, and write it back atomically.
/// `label` names the file in every error message.
pub(super) fn edit_staged_json_object(
    path: &Path,
    label: &str,
    edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> Result<()>,
) -> Result<()> {
    let mut document = match std::fs::read(path) {
        Ok(body) => serde_json::from_slice::<serde_json::Value>(&body)
            .with_context(|| format!("parse {label} {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            serde_json::Value::Object(serde_json::Map::new())
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read {label} {}", path.display()));
        }
    };
    let root = document
        .as_object_mut()
        .with_context(|| format!("{label} {} must contain a JSON object", path.display()))?;
    edit(root)?;
    let mut body = serde_json::to_vec_pretty(&document)?;
    body.push(b'\n');
    atomic_write(path, &body).with_context(|| format!("write {label} {}", path.display()))
}

pub(super) fn mj_worker_socket_name() -> &'static str {
    "subagents.sock"
}

pub(super) fn directory_has_files(path: &Path) -> Result<bool> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() || (metadata.is_dir() && directory_has_files(&entry.path())?) {
            return Ok(true);
        }
    }
    Ok(false)
}
