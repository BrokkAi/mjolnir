use super::*;

/// Remove checkpoint archives installed by a process that exited before its
/// database transaction committed. Call this only while holding the
/// machine-wide controller-store guard and before starting background work.
pub fn reconcile_managed_checkpoint_archives() -> Result<usize> {
    let state = crate::database::load_state()?;
    let operations = crate::database::load_move_operations()?;
    let removed = reconcile_managed_checkpoint_archives_in(&sessions_dir(), &state, &operations)?;
    // An archive a Move refers to can still be missing (removed by hand, or
    // by a daemon that predates this sweep keeping Move handoffs). The Move
    // record owns that fact; record it before anything reads the Move.
    crate::controller::move_session::record_missing_move_archives(&state, operations)?;
    // This scan is the last thing that removes an archive nothing references, so
    // it is also where a finished move's row stops being able to act. Sweeping
    // here means the startup load below this call already sees a clean store.
    super::reap_finished_move_intents();
    Ok(removed)
}

/// Every archive a session record or an unfinished Move refers to is kept.
/// The Move's references come from [`MoveOperation::retained_archives`], the
/// same answer the superseded-checkpoint prune reads.
pub(super) fn reconcile_managed_checkpoint_archives_in(
    directory: &Path,
    state: &State,
    operations: &[mj_core::state::MoveOperation],
) -> Result<usize> {
    if !directory.exists() {
        return Ok(0);
    }
    let referenced_names = state
        .sessions
        .values()
        .filter_map(|session| session.checkpoint.as_ref())
        .chain(
            operations
                .iter()
                .flat_map(mj_core::state::MoveOperation::retained_archives),
        )
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

/// Remove every checkpoint archive that belongs to a session being destroyed:
/// the one its record points at, and any other `<session_id>-<n>-archive-<hex>`
/// file in `directory`, such as the handoff archive a Move wrote and the record
/// no longer references. A file belongs to the session only when its parsed
/// session ID equals `session_id`, so a session whose ID is a prefix of this
/// one is never touched. Retained Move sources are workspaces and checkouts
/// recorded in the database, not archive files, so they are not affected.
pub(crate) fn remove_session_checkpoint_archives(
    directory: &Path,
    session_id: &str,
    current: Option<&CheckpointMetadata>,
) -> Result<()> {
    if let Some(checkpoint) = current
        && let Err(error) = std::fs::remove_file(&checkpoint.archive_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error).with_context(|| {
            format!(
                "remove session recovery archive {}",
                checkpoint.archive_path.display()
            )
        });
    }
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("scan checkpoint directory {}", directory.display()));
        }
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file()
            || managed_checkpoint_archive_name(&entry.file_name())
                .is_none_or(|name| name.session_id != session_id)
        {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("remove session recovery archive {}", entry.path().display())
                });
            }
        }
    }
    Ok(())
}
