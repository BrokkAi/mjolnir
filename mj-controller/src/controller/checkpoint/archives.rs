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

pub(super) fn is_managed_checkpoint_archive_name(name: &OsStr) -> bool {
    let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".hel.zip")) else {
        return false;
    };
    let Some((frontier_prefix, nonce)) = stem.rsplit_once("-archive-") else {
        return false;
    };
    if nonce.len() != 32
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return false;
    }
    let Some((session_id, frontier)) = frontier_prefix.rsplit_once('-') else {
        return false;
    };
    !session_id.is_empty()
        && frontier.parse::<u64>().is_ok()
        && mj_core::config::validate_id("session", session_id).is_ok()
}
