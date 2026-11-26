//! Rewrite a fenced staged store as one SQLite transaction.

use super::*;
use mj_core::config::{MachineId, SshConnection, migration_paths};
use mj_core::state::{
    LegacyMoveOperation, ManagedWorktreeTarget, MoveOperation, PreparedDestinationState,
    PreparedMoveDestination, TargetConnection, TargetRuntimeSettings,
};

pub type SourceTemplateMap = BTreeMap<String, BTreeMap<String, String>>;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MigrationRewriteSpec {
    pub expected_fence: StoreFenceRecord,
    pub old_data_root: PathBuf,
    pub new_data_root: PathBuf,
    pub old_config_root: PathBuf,
    pub new_config_root: PathBuf,
    pub old_home: PathBuf,
    pub source_ssh: SshConnection,
    pub source_machine_id: MachineId,
    /// Old template ID -> recorded local kind -> new SSH template ID.
    pub source_templates: SourceTemplateMap,
    pub pinned_memory_keys: BTreeMap<String, String>,
    pub staged_sessions_dir: PathBuf,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct StoreRewriteSummary {
    pub session_targets: usize,
    pub environment_workers: usize,
    pub sessions: usize,
    pub environments: usize,
    pub moves: usize,
    pub pinned_memory_keys: usize,
}

fn local_kind(kind: &str) -> bool {
    matches!(
        kind,
        "local-bare" | "local-podman" | "local-docker" | "apple-container"
    )
}

fn rewritten_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "local-bare" => Some("ssh-host-bare"),
        "local-podman" => Some("ssh-podman"),
        "local-docker" => Some("ssh-docker"),
        "apple-container" => Some("ssh-apple-container"),
        _ => None,
    }
}

fn rewrite_template(id: &mut String, kind: &str, spec: &MigrationRewriteSpec) -> Result<()> {
    *id = spec
        .source_templates
        .get(id)
        .and_then(|kinds| kinds.get(kind))
        .with_context(|| format!("no migrated template for {id:?} with kind {kind:?}"))?
        .clone();
    Ok(())
}

fn rewrite_unambiguous_template(
    id: &mut String,
    spec: &MigrationRewriteSpec,
    kinds: &BTreeMap<String, BTreeSet<String>>,
    resolved: bool,
) -> Result<()> {
    if let Some(kinds) = kinds.get(id) {
        if kinds.len() > 1 {
            ensure!(
                resolved,
                "historical template {id:?} has multiple local kinds and no recorded locator"
            );
        } else if let Some(kind) = kinds.first() {
            rewrite_template(id, kind, spec)?;
        }
    }
    Ok(())
}

pub(super) fn local_locator_kind(target: &TargetLocator) -> Option<&'static str> {
    match target {
        TargetLocator::LocalBare { .. } => Some("local-bare"),
        TargetLocator::LocalPodman { .. } => Some("local-podman"),
        TargetLocator::LocalDocker { .. } => Some("local-docker"),
        TargetLocator::AppleContainer { .. } => Some("apple-container"),
        _ => None,
    }
}

fn rewrite_locator(locator: &mut TargetLocator, spec: &MigrationRewriteSpec) -> bool {
    let replacement = match locator {
        TargetLocator::LocalBare { .. } => TargetLocator::SshHostBare {
            host: spec.source_ssh.host.clone(),
            data_root: spec.old_data_root.clone(),
        },
        TargetLocator::LocalPodman {
            container_id,
            workspace_storage,
            borrowed_from,
        } => TargetLocator::SshPodman {
            host: spec.source_ssh.host.clone(),
            container_id: container_id.clone(),
            workspace_storage: workspace_storage.clone(),
            borrowed_from: borrowed_from.clone(),
        },
        TargetLocator::LocalDocker {
            container_id,
            borrowed_from,
        } => TargetLocator::SshDocker {
            host: spec.source_ssh.host.clone(),
            container_id: container_id.clone(),
            borrowed_from: borrowed_from.clone(),
        },
        TargetLocator::AppleContainer {
            container_id,
            borrowed_from,
        } => TargetLocator::SshAppleContainer {
            host: spec.source_ssh.host.clone(),
            container_id: container_id.clone(),
            borrowed_from: borrowed_from.clone(),
        },
        _ => return false,
    };
    *locator = replacement;
    true
}

fn check_locator_root(
    locator: &TargetLocator,
    session_id: &str,
    spec: &MigrationRewriteSpec,
) -> Result<()> {
    if let TargetLocator::LocalBare { worker_root } = locator {
        ensure!(
            *worker_root == DataLayout::at(&spec.old_data_root).worker_root(session_id),
            "local worker root for {session_id} does not match the source layout"
        );
    }
    Ok(())
}

fn rewrite_runtime(
    runtime: &mut TargetRuntimeSettings,
    local_kind: Option<&str>,
    spec: &MigrationRewriteSpec,
) {
    if let Some(kind) = local_kind {
        runtime.kind = rewritten_kind(kind).expect("local locator kind").to_owned();
        runtime.connection = TargetConnection::Ssh {
            ssh: spec.source_ssh.clone(),
        };
    }
    match &mut runtime.connection {
        TargetConnection::Local => {}
        TargetConnection::Ssh { ssh } => {
            if let Some(path) = &mut ssh.identity_file {
                migration_paths::relocate_ec2_path(
                    path,
                    &spec.old_config_root,
                    &spec.new_config_root,
                );
            }
            migration_paths::relocate_ec2_ssh_args(
                &mut ssh.extra_args,
                &spec.old_config_root,
                &spec.new_config_root,
            );
        }
        TargetConnection::Aws {
            identity_file,
            ssh_args,
            ..
        } => {
            if let Some(path) = identity_file {
                migration_paths::relocate_ec2_path(
                    path,
                    &spec.old_config_root,
                    &spec.new_config_root,
                );
            }
            migration_paths::relocate_ec2_ssh_args(
                ssh_args,
                &spec.old_config_root,
                &spec.new_config_root,
            );
        }
    }
}

fn rewrite_optional_runtime(
    runtime: &mut Option<TargetRuntimeSettings>,
    kind: Option<&str>,
    spec: &MigrationRewriteSpec,
) {
    if runtime.is_none()
        && let Some(kind) = kind
    {
        let mut synthesized =
            TargetRuntimeSettings::from(&mj_core::config::TargetTemplate::LocalBare);
        if kind != "local-bare" {
            synthesized.execution_policy = mj_core::config::ExecutionPolicy::Unconstrained;
        }
        *runtime = Some(synthesized);
    }
    if let Some(runtime) = runtime {
        rewrite_runtime(runtime, kind, spec);
    }
}

fn rewrite_worktree(worktree: &mut ManagedWorktree, spec: &MigrationRewriteSpec) -> bool {
    match &mut worktree.target {
        ManagedWorktreeTarget::Local => {
            let mut ssh = mj_core::targets::SshTarget::from(&spec.source_ssh);
            migration_paths::relocate_ec2_ssh_args(
                &mut ssh.ssh_args,
                &spec.old_config_root,
                &spec.new_config_root,
            );
            worktree.target = ManagedWorktreeTarget::Ssh {
                destination: ssh.destination,
                ssh_args: ssh.ssh_args,
            };
            true
        }
        ManagedWorktreeTarget::Ssh { ssh_args, .. } => {
            migration_paths::relocate_ec2_ssh_args(
                ssh_args,
                &spec.old_config_root,
                &spec.new_config_root,
            );
            false
        }
    }
}

fn rewrite_archive(path: &mut PathBuf, spec: &MigrationRewriteSpec) {
    let old = spec.old_data_root.join("sessions");
    if path.parent() == Some(old.as_path())
        && let Some(name) = path.file_name()
    {
        *path = spec.new_data_root.join("sessions").join(name);
    }
}

fn rewrite_session(session: &mut SessionRecord, spec: &MigrationRewriteSpec) -> Result<()> {
    if let Some(target) = session.target.as_deref() {
        check_locator_root(target, &session.id, spec)?;
    }
    let kind = session
        .target
        .as_deref()
        .and_then(local_locator_kind)
        .map(str::to_owned)
        .or_else(|| {
            session
                .target
                .is_none()
                .then_some(session.target_runtime.as_ref())
                .flatten()
                .filter(|runtime| {
                    local_kind(&runtime.kind)
                        && matches!(runtime.connection, TargetConnection::Local)
                })
                .map(|runtime| runtime.kind.clone())
        });
    if let Some(target) = session.target.as_deref_mut() {
        rewrite_locator(target, spec);
    }
    if let Some(kind) = &kind {
        rewrite_template(&mut session.target_template_id, kind, spec)?;
    }
    let mut runtime = session.target_runtime.as_deref().cloned();
    rewrite_optional_runtime(&mut runtime, kind.as_deref(), spec);
    session.target_runtime = runtime.map(Arc::new);
    if let Some(worktree) = &mut session.managed_worktree {
        rewrite_worktree(worktree, spec);
    }
    if let Some(checkpoint) = &mut session.checkpoint {
        rewrite_archive(&mut checkpoint.archive_path, spec);
    }
    Ok(())
}

pub(super) fn prepared_kind(prepared: &PreparedMoveDestination) -> Option<String> {
    prepared
        .target()
        .and_then(local_locator_kind)
        .map(str::to_owned)
        .or_else(|| {
            (prepared.target().is_none()
                && local_kind(&prepared.runtime.kind)
                && matches!(prepared.runtime.connection, TargetConnection::Local))
            .then(|| prepared.runtime.kind.clone())
        })
}

fn rewrite_prepared(
    prepared: &mut PreparedMoveDestination,
    spec: &MigrationRewriteSpec,
) -> Option<String> {
    let kind = prepared_kind(prepared);
    match &mut prepared.state {
        PreparedDestinationState::Checked { target }
        | PreparedDestinationState::Adopted { target } => {
            rewrite_locator(target, spec);
        }
        _ => {}
    }
    rewrite_runtime(&mut prepared.runtime, kind.as_deref(), spec);
    kind
}

fn rewrite_move(
    op: &mut MoveOperation,
    spec: &MigrationRewriteSpec,
    kinds: &BTreeMap<String, BTreeSet<String>>,
) -> Result<()> {
    let resolved = op.is_resolved();
    for target in [op.source_target.as_ref(), op.destination_target.as_ref()]
        .into_iter()
        .flatten()
    {
        check_locator_root(target, &op.selection.session_id, spec)?;
    }
    if let Some(prepared) = &op.prepared_destination
        && let Some(target) = prepared.target()
    {
        check_locator_root(target, &op.selection.session_id, spec)?;
    }
    let source_kind = op
        .source_target
        .as_ref()
        .and_then(local_locator_kind)
        .map(str::to_owned);
    let mut destination_kind = op
        .destination_target
        .as_ref()
        .and_then(local_locator_kind)
        .map(str::to_owned);
    if let Some(target) = &mut op.source_target {
        rewrite_locator(target, spec);
    }
    if let Some(target) = &mut op.destination_target {
        rewrite_locator(target, spec);
    }
    if let Some(kind) = &source_kind {
        rewrite_template(&mut op.source_target_template_id, kind, spec)?;
    } else if op.source_target.is_none() {
        rewrite_unambiguous_template(&mut op.source_target_template_id, spec, kinds, resolved)?;
    }
    if let Some(prepared) = &mut op.prepared_destination {
        let prepared_kind = rewrite_prepared(prepared, spec);
        destination_kind = destination_kind.or(prepared_kind);
    }
    if let Some(id) = &mut op.selection.target_template_id {
        if let Some(kind) = &destination_kind {
            rewrite_template(id, kind, spec)?;
        } else if op.destination_target.is_none() && op.prepared_destination.is_none() {
            rewrite_unambiguous_template(id, spec, kinds, resolved)?;
        }
    }
    if let Some(preparation) = &mut op.accepted_preparation {
        if let Some(kind) = &source_kind {
            rewrite_template(&mut preparation.source_target_template_id, kind, spec)?;
        } else if op.source_target.is_none() {
            rewrite_unambiguous_template(
                &mut preparation.source_target_template_id,
                spec,
                kinds,
                resolved,
            )?;
        }
        if let Some(id) = &mut preparation.selection.target_template_id {
            if let Some(kind) = &destination_kind {
                rewrite_template(id, kind, spec)?;
            } else if op.destination_target.is_none() && op.prepared_destination.is_none() {
                rewrite_unambiguous_template(id, spec, kinds, resolved)?;
            }
        }
    }
    if let Some(checkpoint) = &mut op.checkpoint {
        rewrite_archive(&mut checkpoint.archive_path, spec);
    }
    if let Some(session) = &mut op.recovery_session {
        rewrite_session(session, spec)?;
    }
    for grant in &mut op.granted_keys {
        if let Some(path) = &mut grant.private_key_path {
            migration_paths::relocate_ec2_path(path, &spec.old_config_root, &spec.new_config_root);
        }
        if let Some(path) = &mut grant.known_hosts_path {
            migration_paths::relocate_ec2_path(path, &spec.old_config_root, &spec.new_config_root);
        }
        if let Some(ssh) = &mut grant.ssh_target {
            migration_paths::relocate_ec2_ssh_args(
                &mut ssh.ssh_args,
                &spec.old_config_root,
                &spec.new_config_root,
            );
        }
    }
    Ok(())
}

fn rewrite_legacy_move(
    op: &mut LegacyMoveOperation,
    session_id: &str,
    spec: &MigrationRewriteSpec,
    kinds: &BTreeMap<String, BTreeSet<String>>,
) -> Result<()> {
    let resolved = op.is_resolved();
    for target in [op.source_target.as_ref(), op.destination_target.as_ref()]
        .into_iter()
        .flatten()
    {
        check_locator_root(target, session_id, spec)?;
    }
    if let Some(prepared) = &op.prepared_destination
        && let Some(target) = prepared.target()
    {
        check_locator_root(target, session_id, spec)?;
    }
    let source_kind = op
        .source_target
        .as_ref()
        .and_then(local_locator_kind)
        .map(str::to_owned);
    let mut destination_kind = op
        .destination_target
        .as_ref()
        .and_then(local_locator_kind)
        .map(str::to_owned);
    if let Some(target) = &mut op.source_target {
        rewrite_locator(target, spec);
    }
    if let Some(target) = &mut op.destination_target {
        rewrite_locator(target, spec);
    }
    if let Some(kind) = &source_kind {
        rewrite_template(&mut op.source_target_template_id, kind, spec)?;
    } else if op.source_target.is_none() {
        rewrite_unambiguous_template(&mut op.source_target_template_id, spec, kinds, resolved)?;
    }
    if let Some(prepared) = &mut op.prepared_destination {
        let prepared_kind = rewrite_prepared(prepared, spec);
        destination_kind = destination_kind.or(prepared_kind);
    }
    if let Some(selection) = &mut op.selection
        && let Some(id) = &mut selection.target_template_id
    {
        if let Some(kind) = &destination_kind {
            rewrite_template(id, kind, spec)?;
        } else if op.destination_target.is_none() && op.prepared_destination.is_none() {
            rewrite_unambiguous_template(id, spec, kinds, resolved)?;
        }
    }
    if let Some(checkpoint) = &mut op.checkpoint {
        rewrite_archive(&mut checkpoint.archive_path, spec);
    }
    if let Some(handoff) = &mut op.handoff {
        rewrite_archive(&mut handoff.archive_path, spec);
    }
    if let Some(session) = &mut op.recovery_session {
        rewrite_session(session, spec)?;
    }
    Ok(())
}

/// Rewrite the fenced destination copy. The source fence is never cleared here.
pub fn apply_store_migration(
    path: &Path,
    spec: &MigrationRewriteSpec,
) -> Result<StoreRewriteSummary> {
    ensure!(
        spec.source_ssh.host != "local",
        "source host alias 'local' is reserved; choose another --source-host alias for host A"
    );
    let mut connection = super::super::schema::open_writable_ignoring_fence(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let fence = read_store_fence(&tx)?.context("staged store is not fenced")?;
    ensure!(
        fence.destination == spec.expected_fence.destination
            && fence.fenced_at == spec.expected_fence.fenced_at
            && fence.fenced_host == spec.expected_fence.fenced_host
            && fence.fenced_data_dir == spec.expected_fence.fenced_data_dir,
        "staged store fence does not match migration source"
    );
    let preconditions = inspect_migration_preconditions(&tx, &spec.old_data_root)?;
    ensure!(
        preconditions.blockers.is_empty(),
        "migration blocked: {}",
        preconditions.blockers.join("; ")
    );
    let template_kinds = inspect_migration_inventory(&tx)?.local_template_kinds;
    let max_discovery_seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(sequence),0) FROM project_discovery_changes",
        [],
        |row| row.get(0),
    )?;
    let mut summary = StoreRewriteSummary::default();

    let targets: BTreeMap<String, TargetLocator> = super::super::state_io::read_targets(&tx, None)?;
    let local_sessions: BTreeMap<String, String> = targets
        .iter()
        .filter_map(|(id, target)| {
            local_locator_kind(target).map(|kind| (id.clone(), kind.to_owned()))
        })
        .collect();

    let sessions = {
        let mut query = tx.prepare("SELECT session_id,target_template_id,target_runtime_json,managed_worktree FROM sessions")?;
        let rows = query.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, mut template, runtime, worktree) in sessions {
        let mut runtime: Option<TargetRuntimeSettings> = runtime
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .with_context(|| format!("decode runtime for {id}"))?;
        let kind = local_sessions.get(&id).cloned().or_else(|| {
            (!targets.contains_key(&id))
                .then_some(runtime.as_ref())
                .flatten()
                .filter(|runtime| {
                    local_kind(&runtime.kind)
                        && matches!(runtime.connection, TargetConnection::Local)
                })
                .map(|runtime| runtime.kind.clone())
        });
        if let Some(kind) = &kind {
            rewrite_template(&mut template, kind, spec)?;
        }
        rewrite_optional_runtime(&mut runtime, kind.as_deref(), spec);
        let mut worktree: Option<ManagedWorktree> = worktree
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .with_context(|| format!("decode worktree for {id}"))?;
        if let Some(worktree) = &mut worktree {
            rewrite_worktree(worktree, spec);
        }
        tx.execute("UPDATE sessions SET target_template_id=?2,target_runtime_json=?3,managed_worktree=?4 WHERE session_id=?1", params![id,template,runtime.as_ref().map(serde_json::to_string).transpose()?,worktree.as_ref().map(serde_json::to_string).transpose()?])?;
        summary.sessions += 1;
    }

    for id in local_sessions.keys() {
        let target = &targets[id];
        let mut migrated = target.clone();
        rewrite_locator(&mut migrated, spec);
        super::super::state_io::validate_target_for_store(&migrated, id)?;
        let fields = super::super::state_io::stored_target_values(&migrated)?;
        tx.execute("UPDATE session_targets SET kind=?2,host=?3,resource_id=?4,address=?5,workspace=?6,worker_id=?7,workspace_storage=?8,borrowed_from=?9,data_root=?10,machine_id=?11 WHERE session_id=?1", params![id,fields.kind,fields.host,fields.resource,fields.address,fields.workspace,fields.worker_id,fields.workspace_storage,fields.borrowed_from,fields.data_root,spec.source_machine_id.as_str()])?;
        summary.session_targets += 1;
    }

    let leases = {
        let mut query = tx.prepare(
            "SELECT lease_id,kind,target_template_id,runtime_json FROM environment_workers",
        )?;
        let rows = query.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let local_leases: BTreeMap<String, String> = leases
        .iter()
        .filter(|(_, kind, _, _)| local_kind(kind))
        .map(|(lease, kind, _, _)| (lease.clone(), kind.clone()))
        .collect();
    for (lease, kind, mut template, runtime) in leases {
        let local = local_kind(&kind);
        if local {
            rewrite_template(&mut template, &kind, spec)?;
        }
        let mut runtime: Option<TargetRuntimeSettings> = runtime
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .with_context(|| format!("decode runtime for lease {lease}"))?;
        rewrite_optional_runtime(&mut runtime, local.then_some(kind.as_str()), spec);
        if let Some(kind) = rewritten_kind(&kind) {
            tx.execute("UPDATE environment_workers SET kind=?2,host=?3,workspace=CASE WHEN ?2='ssh-host-bare' THEN NULL ELSE workspace END,data_root=CASE WHEN ?2='ssh-host-bare' THEN ?4 ELSE data_root END,machine_id=?5,target_template_id=?6,runtime_json=?7 WHERE lease_id=?1", params![lease,kind,spec.source_ssh.host,path_to_blob(&spec.old_data_root),spec.source_machine_id.as_str(),template,runtime.as_ref().map(serde_json::to_string).transpose()?])?;
        } else {
            tx.execute(
                "UPDATE environment_workers SET runtime_json=?2 WHERE lease_id=?1",
                params![
                    lease,
                    runtime.as_ref().map(serde_json::to_string).transpose()?
                ],
            )?;
        }
        summary.environment_workers += 1;
    }

    let environments = {
        let mut query = tx.prepare(
            "SELECT e.environment_id,e.placement_json,e.target_lease_id,e.owner_session_id FROM environments e",
        )?;
        let rows = query.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, json, lease, owner) in environments {
        let mut placement: EnvironmentPlacement =
            serde_json::from_str(&json).with_context(|| format!("decode environment {id}"))?;
        let kind = lease
            .as_ref()
            .and_then(|lease| local_leases.get(lease))
            .cloned()
            .or_else(|| {
                if lease.is_none() {
                    placement
                        .runtime
                        .as_ref()
                        .filter(|r| {
                            local_kind(&r.kind) && matches!(r.connection, TargetConnection::Local)
                        })
                        .map(|r| r.kind.clone())
                } else {
                    None
                }
            });
        if let Some(kind) = &kind {
            rewrite_template(&mut placement.target_template_id, kind, spec)?;
        }
        rewrite_optional_runtime(&mut placement.runtime, kind.as_deref(), spec);
        if let Some(worktree) = &mut placement.managed_worktree {
            rewrite_worktree(worktree, spec);
        }
        if let Some(prepared) = &mut placement.prepared_destination {
            if let Some(target) = prepared.destination.target() {
                check_locator_root(target, &owner, spec)?;
            }
            let _ = rewrite_prepared(&mut prepared.destination, spec);
        }
        tx.execute(
            "UPDATE environments SET placement_json=?2 WHERE environment_id=?1",
            params![id, serde_json::to_string(&placement)?],
        )?;
        summary.environments += 1;
    }

    let moves = {
        let mut query = tx.prepare("SELECT session_id,operation_json FROM session_moves")?;
        let rows = query.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, json) in moves {
        let normalized = super::super::session_move::normalize_stored_move(&tx, &id, &json)?;
        let new_json = match StoredMoveOperation::decode_json(&normalized)
            .with_context(|| format!("decode Move for {id}"))?
        {
            StoredMoveOperation::Current(mut op) => {
                rewrite_move(&mut op, spec, &template_kinds)?;
                serde_json::to_string(&op)?
            }
            StoredMoveOperation::Legacy(mut op) => {
                rewrite_legacy_move(&mut op, &id, spec, &template_kinds)?;
                serde_json::to_string(&op)?
            }
        };
        tx.execute(
            "UPDATE session_moves SET operation_json=?2 WHERE session_id=?1",
            params![id, new_json],
        )?;
        summary.moves += 1;
    }

    let restarts = {
        let mut query = tx.prepare("SELECT session_id,target_json FROM worker_restart_intents")?;
        let rows = query.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, json) in restarts {
        let mut target: TargetLocator = serde_json::from_str(&json)
            .with_context(|| format!("decode worker restart target for {id}"))?;
        if let TargetLocator::LocalBare { worker_root } = &target {
            ensure!(
                *worker_root == DataLayout::at(&spec.old_data_root).worker_root(&id),
                "worker restart for {id} has a nonconforming local worker root"
            );
        }
        rewrite_locator(&mut target, spec);
        tx.execute(
            "UPDATE worker_restart_intents SET target_json=?2 WHERE session_id=?1",
            params![id, serde_json::to_string(&target)?],
        )?;
    }

    let retained = {
        let mut query = tx.prepare("SELECT operation_id,source_json FROM retained_move_sources")?;
        let rows = query.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, json) in retained {
        let mut session: SessionRecord =
            serde_json::from_str(&json).with_context(|| format!("decode retained source {id}"))?;
        rewrite_session(&mut session, spec)?;
        tx.execute(
            "UPDATE retained_move_sources SET source_json=?2 WHERE operation_id=?1",
            params![id, serde_json::to_string(&session)?],
        )?;
    }

    tx.execute("DELETE FROM host_snapshot_cache WHERE host='local'", [])?;
    let caches = {
        let mut query =
            tx.prepare("SELECT id,host FROM host_snapshot_cache WHERE host LIKE 'ssh:%'")?;
        let rows = query.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, host) in caches {
        let encoded = host
            .strip_prefix("ssh:")
            .with_context(|| format!("invalid SSH snapshot cache key {id}"))?;
        let mut ssh: mj_core::targets::SshTarget = serde_json::from_str(encoded)
            .with_context(|| format!("decode SSH snapshot cache key {id}"))?;
        migration_paths::relocate_ec2_ssh_args(
            &mut ssh.ssh_args,
            &spec.old_config_root,
            &spec.new_config_root,
        );
        tx.execute(
            "UPDATE host_snapshot_cache SET host=?2 WHERE id=?1",
            params![id, format!("ssh:{}", serde_json::to_string(&ssh)?)],
        )?;
    }
    for table in ["host_container_sizes", "project_locations"] {
        tx.execute(
            &format!("UPDATE OR IGNORE {table} SET host=?1 WHERE host='local'"),
            [&spec.source_ssh.host],
        )?;
        // A colliding destination row already owns this key. Keep it and
        // remove the old alias, which would otherwise name host B after cutover.
        tx.execute(&format!("DELETE FROM {table} WHERE host='local'"), [])?;
    }
    for (old_host, new_host) in [
        ("local", spec.source_ssh.host.clone()),
        ("project:local", format!("project:{}", spec.source_ssh.host)),
    ] {
        tx.execute(
            "UPDATE OR IGNORE mount_history SET host=?2 WHERE host=?1",
            params![old_host, new_host],
        )?;
        let remaining = {
            let mut query =
                tx.prepare("SELECT source FROM mount_history WHERE host=?1 ORDER BY ordinal")?;
            let rows = query.query_map([old_host], |row| row.get::<_, Vec<u8>>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for source in remaining {
            tx.execute("INSERT OR IGNORE INTO mount_history(host,source,ordinal) VALUES (?1,?2,(SELECT COALESCE(MAX(ordinal)+1,0) FROM mount_history WHERE host=?1))",params![new_host,source])?;
        }
        tx.execute("DELETE FROM mount_history WHERE host=?1", [old_host])?;
    }
    let changes = {
        let mut query = tx.prepare("SELECT d.sequence,d.session_id,d.target_template_id,d.managed_worktree, d.sequence <= (SELECT sequence FROM project_discovery_progress WHERE singleton=1) AND NOT EXISTS(SELECT 1 FROM project_discovery_failures f WHERE f.sequence=d.sequence) FROM project_discovery_changes d WHERE d.sequence<=?1")?;
        let rows = query.query_map([max_discovery_seq], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, bool>(4)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (sequence, id, mut template, worktree, processed) in changes {
        let mut worktree: Option<ManagedWorktree> =
            worktree.as_deref().map(serde_json::from_str).transpose()?;
        if let Some(worktree) = &mut worktree {
            rewrite_worktree(worktree, spec);
        }
        if let Some(kind) = local_sessions.get(&id).map(String::as_str) {
            rewrite_template(&mut template, kind, spec)?;
        } else if !targets.contains_key(&id) {
            rewrite_unambiguous_template(&mut template, spec, &template_kinds, processed)?;
        }
        tx.execute("UPDATE project_discovery_changes SET target_template_id=?2,managed_worktree=?3 WHERE sequence=?1",params![sequence,template,worktree.as_ref().map(serde_json::to_string).transpose()?])?;
    }
    tx.execute(
        "DELETE FROM project_discovery_changes WHERE sequence>?1",
        [max_discovery_seq],
    )?;
    for table in ["project_seed_homes", "project_seed_failures"] {
        let homes = {
            let mut query = tx.prepare(&format!("SELECT DISTINCT home FROM {table}"))?;
            let rows = query.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for home in homes {
            let path = blob_to_path(&home);
            if path.starts_with(&spec.old_home) || path.starts_with(&spec.old_config_root) {
                tx.execute(&format!("DELETE FROM {table} WHERE home=?1"), [home])?;
            }
        }
    }
    tx.execute("UPDATE detached_drafts SET owner_pid=NULL", [])?;
    for (id, key) in &spec.pinned_memory_keys {
        pin_session_memory_key_with(&tx, id, key)?;
        summary.pinned_memory_keys += 1;
    }
    let archives = {
        let mut query = tx.prepare("SELECT session_id,archive_path FROM session_checkpoints")?;
        let rows = query.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, archive) in archives {
        let path = blob_to_path(&archive);
        let mut components = path.components();
        if let (Some(std::path::Component::Normal(name)), None) =
            (components.next(), components.next())
        {
            let staged = spec.staged_sessions_dir.join(name);
            ensure!(
                staged.is_file(),
                "managed checkpoint for {id} is missing: {}",
                staged.display()
            );
        }
    }
    tx.commit()?;
    let (busy, _, _): (i64, i64, i64) =
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    ensure!(
        busy == 0,
        "could not truncate staged store WAL: another connection holds it"
    );
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::state::{MovePhase, MoveSourceSnapshot};

    fn assert_recorded_target(connection: &Connection, id: &str, expected_kind: &str) {
        let target = super::super::super::state_io::read_targets(connection, Some(id))
            .unwrap()
            .remove(id)
            .unwrap();
        let json: String = connection
            .query_row(
                "SELECT target_runtime_json FROM sessions WHERE session_id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        let runtime: TargetRuntimeSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(runtime.kind, expected_kind);
        mj_core::targets::TargetLocator::try_from(mj_core::targets::RecordedTarget {
            locator: &target,
            runtime: Some(&runtime),
            session_id: id,
        })
        .unwrap();
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, MigrationRewriteSpec) {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("old-data");
        let old_config = root.path().join("old-home/config");
        let new_config = root.path().join("new-home/config");
        let staged = root.path().join("staged");
        std::fs::create_dir_all(staged.join("sessions")).unwrap();
        let path = staged.join("mj.sqlite3");
        let fence = StoreFenceRecord {
            destination: "b".into(),
            fenced_at: "now".into(),
            fenced_host: "a".into(),
            fenced_data_dir: old.clone(),
            note: None,
        };
        let spec = MigrationRewriteSpec {
            expected_fence: fence,
            old_data_root: old,
            new_data_root: root.path().join("new-data"),
            old_config_root: old_config.clone(),
            new_config_root: new_config,
            old_home: root.path().join("old-home"),
            source_ssh: SshConnection {
                host: "source".into(),
                user: Some("alice".into()),
                identity_file: Some(old_config.join("ec2/source/key")),
                extra_args: vec![],
            },
            source_machine_id: MachineId::new("source").unwrap(),
            source_templates: BTreeMap::from([(
                "local".into(),
                BTreeMap::from([("local-bare".into(), "source-bare".into())]),
            )]),
            pinned_memory_keys: BTreeMap::from([("one".into(), "a".repeat(64))]),
            staged_sessions_dir: staged.join("sessions"),
        };
        (root, path, spec)
    }

    #[test]
    fn migration_rewrite_is_fenced_and_rolls_back_missing_managed_checkpoint() {
        let (_root, path, spec) = fixture();
        let transferred: MigrationRewriteSpec =
            serde_json::from_slice(&serde_json::to_vec(&spec).unwrap()).unwrap();
        assert_eq!(transferred.expected_fence, spec.expected_fence);
        assert_eq!(transferred.source_templates, spec.source_templates);
        let mut session = super::super::super::tests::session("one", "project");
        session.checkpoint = None;
        session.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: DataLayout::at(&spec.old_data_root).worker_root("one"),
        }));
        session.target_runtime = Some(Arc::new(TargetRuntimeSettings::from(
            &mj_core::config::TargetTemplate::LocalBare,
        )));
        session.project_directory = Some("/work/project".into());
        save_session_to(&path, &session).unwrap();
        let connection = Connection::open(&path).unwrap();
        let read_kind = || {
            connection
                .query_row(
                    "SELECT kind FROM session_targets WHERE session_id='one'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap()
        };
        assert!(apply_store_migration(&path, &spec).is_err());
        assert_eq!(read_kind(), "local-bare");
        set_store_fence(&connection, &spec.expected_fence).unwrap();
        let mut wrong = spec.clone();
        wrong.expected_fence.destination = "wrong".into();
        assert!(apply_store_migration(&path, &wrong).is_err());
        assert_eq!(read_kind(), "local-bare");
        connection.execute("INSERT INTO session_checkpoints(session_id,archive_path,sha256,created_at,event_frontier) VALUES ('one',?1,?2,'now',0)",params![path_to_blob(Path::new("archive.hel.zip")),"a".repeat(64)]).unwrap();
        assert!(apply_store_migration(&path, &spec).is_err());
        assert_eq!(read_kind(), "local-bare");
        assert_eq!(
            read_pinned_memory_key_with(&connection, "one").unwrap(),
            None
        );
        std::fs::write(spec.staged_sessions_dir.join("archive.hel.zip"), b"archive").unwrap();
        drop(connection);
        let summary = apply_store_migration(&path, &spec).unwrap();
        assert_eq!(summary.session_targets, 1);
        let connection = Connection::open(&path).unwrap();
        let (kind,host,workspace,data_root,machine):(String,String,Option<Vec<u8>>,Vec<u8>,String) = connection.query_row("SELECT kind,host,workspace,data_root,machine_id FROM session_targets WHERE session_id='one'",[],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).unwrap();
        assert_eq!(
            (kind.as_str(), host.as_str(), machine.as_str()),
            ("ssh-host-bare", "source", "source")
        );
        assert!(workspace.is_none());
        assert_eq!(blob_to_path(&data_root), spec.old_data_root);
        assert_recorded_target(&connection, "one", "ssh-host-bare");
        assert_eq!(
            read_pinned_memory_key_with(&connection, "one").unwrap(),
            Some("a".repeat(64))
        );
        assert!(read_store_fence(&connection).unwrap().is_some());
        let discovery: i64 = connection
            .query_row(
                "SELECT count(*) FROM project_discovery_changes",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            discovery, 1,
            "session UPDATE must not create discovery work"
        );
    }

    #[test]
    fn migration_preconditions_distinguish_resolved_and_pending_ambiguous_templates() {
        let (_root, path, mut spec) = fixture();
        spec.source_templates
            .entry("local".into())
            .or_default()
            .insert("local-docker".into(), "source-docker".into());
        let mut session = super::super::super::tests::session("one", "project");
        session.checkpoint = None;
        session.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: DataLayout::at(&spec.old_data_root).worker_root("one"),
        }));
        save_session_to(&path, &session).unwrap();
        let mut legacy_session = super::super::super::tests::session("legacy", "project");
        legacy_session.checkpoint = None;
        legacy_session.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: DataLayout::at(&spec.old_data_root).worker_root("legacy"),
        }));
        save_session_to(&path, &legacy_session).unwrap();
        let connection = Connection::open(&path).unwrap();
        let placement = EnvironmentPlacement::from_session(&session);
        connection.execute("INSERT INTO environments(environment_id,owner_session_id,phase,placement_json) VALUES ('env','one','reclaimed',?1)",[serde_json::to_string(&placement).unwrap()]).unwrap();
        connection.execute("INSERT INTO environment_workers(lease_id,environment_id,worker_session_id,kind,resource_id,target_template_id) VALUES ('old','env','one','local-docker','old-container','local')",[]).unwrap();
        let mut op = super::super::super::session_move::tests::operation(&session);
        op.source_target = None;
        op.destination_target = None;
        op.recovery_session = None;
        op.phase = MovePhase::Completed;
        connection.execute("INSERT INTO session_moves(session_id,operation_id,operation_json) VALUES ('one','move-one',?1)",[serde_json::to_string(&op).unwrap()]).unwrap();
        let mut legacy = serde_json::to_value(super::super::super::session_move::tests::operation(
            &legacy_session,
        ))
        .unwrap();
        legacy["operation_id"] = "legacy-move".into();
        legacy["phase"] = "completed".into();
        legacy["source_target"] = serde_json::Value::Null;
        legacy["selection"] = serde_json::Value::Null;
        connection.execute("INSERT INTO session_moves(session_id,operation_id,operation_json) VALUES ('legacy','legacy-move',?1)",[legacy.to_string()]).unwrap();
        connection.execute("INSERT INTO project_discovery_changes(session_id,target_template_id) VALUES ('gone','local')",[]).unwrap();
        let first: i64 = connection.last_insert_rowid();
        connection
            .execute(
                "UPDATE project_discovery_progress SET sequence=?1 WHERE singleton=1",
                [first],
            )
            .unwrap();
        let inspect = || inspect_migration_preconditions(&connection, &spec.old_data_root).unwrap();
        let ready = inspect();
        assert!(ready.blockers.is_empty(), "{:?}", ready.blockers);
        assert_eq!(ready.carried_moves, 2);
        set_store_fence(&connection, &spec.expected_fence).unwrap();
        op.phase = MovePhase::Failed;
        connection
            .execute(
                "UPDATE session_moves SET operation_json=?1 WHERE session_id='one'",
                [serde_json::to_string(&op).unwrap()],
            )
            .unwrap();
        assert!(
            inspect()
                .blockers
                .iter()
                .any(|b| b.contains("Move move-one") && b.contains("ambiguous"))
        );
        assert!(
            apply_store_migration(&path, &spec)
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT kind FROM session_targets WHERE session_id='one'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "local-bare"
        );
        op.phase = MovePhase::Completed;
        connection
            .execute(
                "UPDATE session_moves SET operation_json=?1 WHERE session_id='one'",
                [serde_json::to_string(&op).unwrap()],
            )
            .unwrap();
        connection.execute("INSERT INTO project_discovery_changes(session_id,target_template_id) VALUES ('gone-again','local')",[]).unwrap();
        let second = connection.last_insert_rowid();
        assert!(
            inspect()
                .blockers
                .iter()
                .any(|b| b.contains(&format!("change {second}")) && b.contains("ambiguous"))
        );
        connection
            .execute(
                "UPDATE project_discovery_progress SET sequence=?1 WHERE singleton=1",
                [second],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO project_discovery_failures(sequence,error) VALUES (?1,'retry')",
                [second],
            )
            .unwrap();
        assert!(
            inspect()
                .blockers
                .iter()
                .any(|b| b.contains(&format!("change {second}")) && b.contains("ambiguous"))
        );
        connection
            .execute(
                "DELETE FROM project_discovery_failures WHERE sequence=?1",
                [second],
            )
            .unwrap();
        assert!(inspect().blockers.is_empty());
        drop(connection);
        apply_store_migration(&path, &spec).unwrap();
        let connection = Connection::open(path).unwrap();
        let op: MoveOperation = serde_json::from_str(
            &connection
                .query_row(
                    "SELECT operation_json FROM session_moves WHERE session_id='one'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(op.source_target_template_id, "local");
        assert_eq!(op.selection.target_template_id.as_deref(), Some("local"));
        let legacy: LegacyMoveOperation = serde_json::from_str(
            &connection
                .query_row(
                    "SELECT operation_json FROM session_moves WHERE session_id='legacy'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(legacy.source_target_template_id, "local");
        let ids: Vec<String> = {
            let mut query = connection
                .prepare(
                    "SELECT target_template_id FROM project_discovery_changes ORDER BY sequence",
                )
                .unwrap();
            query
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(ids, vec!["local", "local"]);
    }

    #[test]
    fn migration_rewrites_retained_moves_leases_and_host_history() {
        let (_root, path, mut spec) = fixture();
        spec.source_templates
            .entry("local".into())
            .or_default()
            .insert("local-docker".into(), "source-docker".into());
        spec.source_templates
            .entry("podman".into())
            .or_default()
            .insert("local-podman".into(), "source-podman".into());
        let mut session = super::super::super::tests::session("one", "project");
        session.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: DataLayout::at(&spec.old_data_root).worker_root("one"),
        }));
        session.target_runtime = Some(Arc::new(TargetRuntimeSettings::from(
            &mj_core::config::TargetTemplate::LocalBare,
        )));
        session.managed_worktree = Some(Box::new(ManagedWorktree {
            kind: mj_core::state::ManagedCheckoutKind::Clone,
            source_project_directory: "/work/project".into(),
            source_repository: "/work/project".into(),
            worktree_root: "/work/project/.mj/clones/one".into(),
            branch: "mj/one".into(),
            target: ManagedWorktreeTarget::Local,
            base_commit: None,
        }));
        let archive = spec.old_data_root.join("sessions/archive.hel.zip");
        session.checkpoint.as_mut().unwrap().archive_path = archive.clone();
        save_session_to(&path, &session).unwrap();
        let mut remote = super::super::super::tests::session("remote", "project");
        remote.checkpoint = None;
        remote.target_template_id = "remote-b".into();
        remote.target = Some(Box::new(TargetLocator::SshBare {
            host: "destination-b".into(),
            workspace: "/home/b/work/remote".into(),
            worker_id: None,
        }));
        let mut remote_runtime =
            TargetRuntimeSettings::from(&mj_core::config::TargetTemplate::LocalBare);
        remote_runtime.kind = "ssh-bare".into();
        remote_runtime.connection = TargetConnection::Ssh {
            ssh: SshConnection {
                host: "destination-b".into(),
                user: None,
                identity_file: Some(spec.old_config_root.join("ec2/id/key")),
                extra_args: vec![format!(
                    "UserKnownHostsFile={}",
                    spec.old_config_root.join("ec2/id/known_hosts").display()
                )],
            },
        };
        remote.target_runtime = Some(Arc::new(remote_runtime));
        save_session_to(&path, &remote).unwrap();
        let mut legacy_session = super::super::super::tests::session("legacy", "project");
        legacy_session.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: DataLayout::at(&spec.old_data_root).worker_root("legacy"),
        }));
        legacy_session.checkpoint = None;
        save_session_to(&path, &legacy_session).unwrap();
        let mut podman = super::super::super::tests::session("podman", "project");
        podman.checkpoint = None;
        podman.target_template_id = "podman".into();
        podman.target_runtime = None;
        save_session_to(&path, &podman).unwrap();
        let connection = Connection::open(&path).unwrap();
        let inventory = inspect_migration_inventory(&connection).unwrap();
        assert!(
            inventory
                .unidentified_placements
                .iter()
                .any(|placement| placement.session_id == "remote" && placement.references_ec2_path)
        );
        let placement = EnvironmentPlacement::from_session(&session);
        connection.execute("INSERT INTO environments(environment_id,owner_session_id,phase,placement_json) VALUES ('env','one','active',?1)",[serde_json::to_string(&placement).unwrap()]).unwrap();
        connection.execute("INSERT INTO environment_workers(lease_id,environment_id,worker_session_id,kind,workspace,target_template_id,runtime_json) VALUES ('lease','env','one','local-bare',?1,'local',?2)",params![path_to_blob(&DataLayout::at(&spec.old_data_root).worker_root("one")),serde_json::to_string(&session.target_runtime).unwrap()]).unwrap();
        let mut historical_runtime = session.target_runtime.as_ref().unwrap().as_ref().clone();
        historical_runtime.kind = "local-docker".into();
        connection.execute("INSERT INTO environment_workers(lease_id,environment_id,worker_session_id,kind,resource_id,target_template_id,runtime_json) VALUES ('historical','env','one','local-docker','container-old','local',?1)",[serde_json::to_string(&historical_runtime).unwrap()]).unwrap();
        let mut reclaimed_placement = placement.clone();
        reclaimed_placement.runtime = None;
        connection.execute("INSERT INTO environments(environment_id,owner_session_id,phase,placement_json) VALUES ('reclaimed','one','reclaimed',?1)",[serde_json::to_string(&reclaimed_placement).unwrap()]).unwrap();
        connection.execute("INSERT INTO environment_workers(lease_id,environment_id,worker_session_id,kind,workspace,target_template_id,runtime_json) VALUES ('reclaimed-lease','reclaimed','one','local-bare',?1,'local',NULL)",[path_to_blob(&DataLayout::at(&spec.old_data_root).worker_root("one"))]).unwrap();
        connection.execute("UPDATE environments SET target_lease_id='reclaimed-lease' WHERE environment_id='reclaimed'",[]).unwrap();
        let mut remote_placement = EnvironmentPlacement::from_session(&remote);
        remote_placement.runtime = Some(TargetRuntimeSettings::from(
            &mj_core::config::TargetTemplate::LocalBare,
        ));
        connection.execute("INSERT INTO environments(environment_id,owner_session_id,phase,placement_json) VALUES ('remote-env','remote','active',?1)",[serde_json::to_string(&remote_placement).unwrap()]).unwrap();
        connection.execute("INSERT INTO environment_workers(lease_id,environment_id,worker_session_id,kind,host,workspace,target_template_id,runtime_json) VALUES ('remote-lease','remote-env','remote','ssh-bare','destination-b',?1,'remote-b',?2)",params![path_to_blob(Path::new("/home/b/work/remote")),serde_json::to_string(&remote.target_runtime).unwrap()]).unwrap();
        connection.execute("UPDATE environments SET target_lease_id='remote-lease' WHERE environment_id='remote-env'",[]).unwrap();
        connection
            .execute(
                "UPDATE environments SET target_lease_id='lease' WHERE environment_id='env'",
                [],
            )
            .unwrap();
        let mut op = super::super::super::session_move::tests::operation(&session);
        op.phase = MovePhase::Completed;
        op.destination_target = session.target.as_deref().cloned();
        op.source_snapshot = Some(Box::new(MoveSourceSnapshot {
            archive_path: archive.clone(),
            sha256: "b".repeat(64),
            manifest: None,
            queued_prompts: vec![],
            mailbox: None,
        }));
        connection.execute("INSERT INTO session_moves(session_id,operation_id,operation_json) VALUES ('one','move-one',?1)",[serde_json::to_string(&op).unwrap()]).unwrap();
        let mut legacy_op = serde_json::to_value(
            super::super::super::session_move::tests::operation(&legacy_session),
        )
        .unwrap();
        legacy_op["operation_id"] = "legacy-move".into();
        legacy_op["phase"] = "completed".into();
        legacy_op["selection"] = serde_json::Value::Null;
        assert!(matches!(
            StoredMoveOperation::decode_json(&legacy_op.to_string()).unwrap(),
            StoredMoveOperation::Legacy(_)
        ));
        connection.execute("INSERT INTO session_moves(session_id,operation_id,operation_json) VALUES ('legacy','legacy-move',?1)",[legacy_op.to_string()]).unwrap();
        connection.execute("INSERT INTO worker_restart_intents(session_id,operation_id,target_json,desired_build,phase) VALUES ('one','restart',?1,'build','prepared')",[serde_json::to_string(session.target.as_ref().unwrap()).unwrap()]).unwrap();
        connection.execute("INSERT INTO retained_move_sources(operation_id,session_id,source_json,exclusions_json,created_at) VALUES ('retained','one',?1,'[\"cache\"]','now')",[serde_json::to_string(&session).unwrap()]).unwrap();
        let old_key = spec.old_config_root.join("ec2/id/key");
        let ssh = mj_core::targets::SshTarget {
            destination: "ec2".into(),
            ssh_args: vec!["-i".into(), old_key.to_string_lossy().into_owned()],
        };
        connection.execute("INSERT INTO host_snapshot_cache(session_id,host,sha256,path,written_at) VALUES ('one','local','a','/a','now'),('one',?1,'b','/b','now')",[format!("ssh:{}",serde_json::to_string(&ssh).unwrap())]).unwrap();
        connection.execute("INSERT INTO host_container_sizes(host,cpus,memory_bytes) VALUES ('local',1,1),('source',2,2)",[]).unwrap();
        connection.execute("INSERT INTO mount_history(host,source,ordinal) VALUES ('local',X'2F61',0),('source',X'2F62',0),('project:local',X'2F63',0)",[]).unwrap();
        connection.execute("INSERT INTO project_locations(host,directory,checkout_root,repository_root,identity_json,seen_at) VALUES ('local',X'2F61',X'2F61',X'2F61','{}','old'),('source',X'2F61',X'2F61',X'2F61','{}','existing')",[]).unwrap();
        connection
            .execute(
                "INSERT INTO project_seed_homes(harness,home) VALUES ('codex',?1)",
                [path_to_blob(&spec.old_home.join(".codex"))],
            )
            .unwrap();
        connection.execute("INSERT INTO project_seed_failures(harness,home,directory,error,source_file) VALUES ('codex',?1,X'2F61','error',0)",[path_to_blob(&spec.old_home.join(".codex"))]).unwrap();
        connection.execute("INSERT INTO detached_drafts(draft_id,workspace_id,session_id,source,owner_pid,saved_at,text,revision) VALUES ('draft','default','one','terminal',123,'now','draft',1)",[]).unwrap();
        let inventory = inspect_migration_inventory(&connection).unwrap();
        assert_eq!(
            inventory.local_template_kinds["local"],
            BTreeSet::from(["local-bare".into(), "local-docker".into()])
        );
        set_store_fence(&connection, &spec.expected_fence).unwrap();
        drop(connection);
        apply_store_migration(&path, &spec).unwrap();
        let connection = Connection::open(path).unwrap();
        assert_recorded_target(&connection, "one", "ssh-host-bare");
        assert_recorded_target(&connection, "legacy", "ssh-host-bare");
        assert_recorded_target(&connection, "podman", "ssh-podman");
        let reclaimed_runtime: TargetRuntimeSettings = serde_json::from_str(
            &connection
                .query_row(
                    "SELECT runtime_json FROM environment_workers WHERE lease_id='reclaimed-lease'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        assert!(
            matches!(&reclaimed_runtime.connection, TargetConnection::Ssh { ssh } if ssh.identity_file.as_deref() == Some(spec.new_config_root.join("ec2/source/key").as_path()))
        );
        let reclaimed_target = TargetLocator::SshHostBare {
            host: spec.source_ssh.host.clone(),
            data_root: spec.old_data_root.clone(),
        };
        mj_core::targets::TargetLocator::try_from(mj_core::targets::RecordedTarget {
            locator: &reclaimed_target,
            runtime: Some(&reclaimed_runtime),
            session_id: "one",
        })
        .unwrap();
        let reclaimed: EnvironmentPlacement = serde_json::from_str(
            &connection
                .query_row(
                    "SELECT placement_json FROM environments WHERE environment_id='reclaimed'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(reclaimed.runtime.unwrap().kind, "ssh-host-bare");
        let (kind,machine,template):(String,String,String) = connection.query_row("SELECT kind,machine_id,target_template_id FROM environment_workers WHERE lease_id='lease'",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        assert_eq!(
            (kind.as_str(), machine.as_str(), template.as_str()),
            ("ssh-host-bare", "source", "source-bare")
        );
        let placement: EnvironmentPlacement = serde_json::from_str(
            &connection
                .query_row(
                    "SELECT placement_json FROM environments WHERE environment_id='env'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(placement.target_template_id, "source-bare");
        assert!(
            matches!(placement.managed_worktree.unwrap().target,ManagedWorktreeTarget::Ssh { ref destination,.. } if destination == "alice@source")
        );
        let worktree: ManagedWorktree = serde_json::from_str(
            &connection
                .query_row(
                    "SELECT managed_worktree FROM sessions WHERE session_id='one'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        assert!(
            matches!(worktree.target,ManagedWorktreeTarget::Ssh { ref destination,.. } if destination == "alice@source")
        );
        assert_eq!(
            worktree.worktree_root,
            PathBuf::from("/work/project/.mj/clones/one")
        );
        let (discovery_count,discovery_template):(i64,String) = connection.query_row("SELECT count(*),max(target_template_id) FROM project_discovery_changes WHERE session_id='one'",[],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!(
            (discovery_count, discovery_template.as_str()),
            (1, "source-bare")
        );
        let (historical,host,template):(String,String,String) = connection.query_row("SELECT kind,host,target_template_id FROM environment_workers WHERE lease_id='historical'",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        assert_eq!(
            (historical.as_str(), host.as_str(), template.as_str()),
            ("ssh-docker", "source", "source-docker")
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT kind FROM environment_workers WHERE lease_id='reclaimed-lease'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "ssh-host-bare"
        );
        let runtime: TargetRuntimeSettings = serde_json::from_str(
            &connection
                .query_row(
                    "SELECT runtime_json FROM environment_workers WHERE lease_id='historical'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(runtime.kind, "ssh-docker");
        assert!(matches!(runtime.connection, TargetConnection::Ssh { .. }));
        let (remote_kind, remote_host): (String, String) = connection
            .query_row(
                "SELECT kind,host FROM session_targets WHERE session_id='remote'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (remote_kind.as_str(), remote_host.as_str()),
            ("ssh-bare", "destination-b")
        );
        let remote_placement: EnvironmentPlacement = serde_json::from_str(
            &connection
                .query_row(
                    "SELECT placement_json FROM environments WHERE environment_id='remote-env'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(remote_placement.target_template_id, "remote-b");
        assert!(matches!(
            remote_placement.runtime.unwrap().connection,
            TargetConnection::Local
        ));
        let runtime: TargetRuntimeSettings = serde_json::from_str(
            &connection
                .query_row(
                    "SELECT target_runtime_json FROM sessions WHERE session_id='remote'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        let TargetConnection::Ssh { ssh } = runtime.connection else {
            panic!("SSH runtime expected")
        };
        assert_eq!(
            ssh.identity_file.unwrap(),
            spec.new_config_root.join("ec2/id/key")
        );
        assert!(ssh.extra_args[0].contains(spec.new_config_root.to_str().unwrap()));
        assert_eq!(
            connection
                .query_row(
                    "SELECT owner_pid FROM detached_drafts WHERE draft_id='draft'",
                    [],
                    |row| row.get::<_, Option<i64>>(0)
                )
                .unwrap(),
            None
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT exclusions_json FROM retained_move_sources",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "[\"cache\"]"
        );
        let op: MoveOperation = serde_json::from_str(
            &connection
                .query_row(
                    "SELECT operation_json FROM session_moves WHERE session_id='one'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            op.checkpoint.unwrap().archive_path,
            spec.new_data_root.join("sessions/archive.hel.zip")
        );
        assert_eq!(op.source_snapshot.unwrap().archive_path, archive);
        let legacy: LegacyMoveOperation = serde_json::from_str(
            &connection
                .query_row(
                    "SELECT operation_json FROM session_moves WHERE session_id='legacy'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        let embedded = legacy.recovery_session.as_ref().unwrap();
        mj_core::targets::TargetLocator::try_from(mj_core::targets::RecordedTarget {
            locator: embedded.target.as_deref().unwrap(),
            runtime: embedded.target_runtime.as_deref(),
            session_id: &embedded.id,
        })
        .unwrap();
        assert!(matches!(
            legacy.source_target,
            Some(TargetLocator::SshHostBare { .. })
        ));
        assert_eq!(legacy.source_target_template_id, "source-bare");
        assert_eq!(
            op.recovery_session
                .unwrap()
                .checkpoint
                .unwrap()
                .archive_path,
            spec.new_data_root.join("sessions/archive.hel.zip")
        );
        let restart: TargetLocator = serde_json::from_str(
            &connection
                .query_row(
                    "SELECT target_json FROM worker_restart_intents",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        assert!(matches!(restart, TargetLocator::SshHostBare { .. }));
        let retained: SessionRecord = serde_json::from_str(
            &connection
                .query_row("SELECT source_json FROM retained_move_sources", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            retained.checkpoint.unwrap().archive_path,
            spec.new_data_root.join("sessions/archive.hel.zip")
        );
        let cache: String = connection
            .query_row("SELECT host FROM host_snapshot_cache", [], |row| row.get(0))
            .unwrap();
        assert!(cache.contains(spec.new_config_root.to_str().unwrap()));
        let sizes: Vec<(String, i64)> = {
            let mut q = connection
                .prepare("SELECT host,cpus FROM host_container_sizes ORDER BY host")
                .unwrap();
            q.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert_eq!(sizes, vec![("source".into(), 2)]);
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM mount_history", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM mount_history WHERE host='source'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            2
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM mount_history WHERE host='project:source'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM project_locations", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT seen_at FROM project_locations WHERE host='source'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "existing"
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM project_seed_homes", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM project_seed_failures", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            0
        );
    }
}
