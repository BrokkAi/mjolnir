use super::*;

/// Remove checkpoint archives installed by a process that exited before its
/// database transaction committed. Call this only while holding the
/// machine-wide controller-store guard and before starting background work.
pub fn reconcile_managed_checkpoint_archives() -> Result<usize> {
    let mut state = crate::database::load_state()?;
    // Include operation-owned recovery copies even after a ready destination
    // installs a newer ordinary checkpoint.
    for operation in crate::database::load_move_operations()? {
        if operation.retains_checkpoint()
            && let Some(checkpoint) = operation.checkpoint
            && let Some(mut session) = state.sessions.get(&operation.selection.session_id).cloned()
        {
            session.checkpoint = Some(checkpoint);
            state
                .sessions
                .insert(format!("move:{}", operation.operation_id), session);
        }
    }
    reconcile_managed_checkpoint_archives_in(&sessions_dir(), &state)
}

pub(super) fn reconcile_managed_checkpoint_archives_in(
    directory: &Path,
    state: &State,
) -> Result<usize> {
    if !directory.exists() {
        return Ok(0);
    }
    let referenced_names = state
        .sessions
        .values()
        .filter_map(|session| session.checkpoint.as_ref())
        .filter_map(|checkpoint| checkpoint.archive_path.file_name())
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    let mut removed = 0;
    for entry in std::fs::read_dir(directory)
        .with_context(|| format!("scan checkpoint directory {}", directory.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file()
            || !is_managed_checkpoint_archive_name(&entry.file_name())
            || referenced_names.contains(&entry.file_name())
        {
            continue;
        }
        std::fs::remove_file(entry.path()).with_context(|| {
            format!(
                "remove unreferenced managed checkpoint {}",
                entry.path().display()
            )
        })?;
        removed += 1;
    }
    Ok(removed)
}

/// The session and checkpoint generation a managed archive file name names.
///
/// Managed checkpoints are named `<session_id>-<frontier>-archive-<32 hex>`,
/// so the file name alone says which session a checkpoint belongs to and which
/// of that session's checkpoints is the newest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedCheckpointArchiveName {
    pub session_id: String,
    pub frontier: u64,
}

/// Parse a managed checkpoint archive file name, or `None` when the name is
/// not one Mjolnir wrote.
pub(crate) fn managed_checkpoint_archive_name(
    name: &OsStr,
) -> Option<ManagedCheckpointArchiveName> {
    let stem = name
        .to_str()
        .and_then(|name| name.strip_suffix(".hel.zip"))?;
    let (frontier_prefix, nonce) = stem.rsplit_once("-archive-")?;
    if nonce.len() != 32
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return None;
    }
    let (session_id, frontier) = frontier_prefix.rsplit_once('-')?;
    if session_id.is_empty() || mj_core::config::validate_id("session", session_id).is_err() {
        return None;
    }
    Some(ManagedCheckpointArchiveName {
        session_id: session_id.to_owned(),
        frontier: frontier.parse::<u64>().ok()?,
    })
}

pub(super) fn is_managed_checkpoint_archive_name(name: &OsStr) -> bool {
    managed_checkpoint_archive_name(name).is_some()
}
