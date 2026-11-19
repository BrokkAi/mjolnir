//! The file names of the checkpoint archives Mjolnir manages.

use std::ffi::OsStr;

/// The session and checkpoint generation a managed archive file name names.
///
/// Managed checkpoints are named `<session_id>-<frontier>-archive-<32 hex>`,
/// so the file name alone says which session a checkpoint belongs to and which
/// of that session's checkpoints is the newest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedCheckpointArchiveName {
    pub session_id: String,
    pub frontier: u64,
}

/// Parse a managed checkpoint archive file name, or `None` when the name is
/// not one Mjolnir wrote.
pub fn managed_checkpoint_archive_name(name: &OsStr) -> Option<ManagedCheckpointArchiveName> {
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
