//! The worker's exit record, written so that a full disk cannot silence it.
//!
//! A worker stops when it can no longer write its journal, and on a full disk
//! that is exactly when creating a new file fails too. So once the worker has
//! recovered its durable state it reserves the record: `worker-exit.json`
//! holding `null` padded with spaces, which every reader takes as "no exit
//! record". At exit the reason is written over those bytes in place, which
//! needs no new blocks on ext4, XFS and similar filesystems. Before the
//! reservation exists (a worker that fails during startup) the record is
//! created the ordinary way.

use std::io::{Seek, Write};
use std::path::Path;

use anyhow::{Context, Result};

/// Room for a reason with a bridge's stderr tail after it.
pub const RESERVED_EXIT_RECORD_BYTES: usize = 16 * 1024;

/// Reserve the record's space while the disk still has some.
pub fn reserve(root: &Path) -> Result<()> {
    let mut body = b"null".to_vec();
    body.resize(RESERVED_EXIT_RECORD_BYTES, b' ');
    body[RESERVED_EXIT_RECORD_BYTES - 1] = b'\n';
    let path = root.join(mj_core::relay::WORKER_EXIT_FILE);
    mj_core::config::atomic_write(&path, &body)
        .with_context(|| format!("reserve the worker exit record {}", path.display()))
}

/// Write the exit record: over a reservation in place, or as a new file.
pub fn write(root: &Path, reason: &str, refusal: Option<&str>) -> Result<()> {
    let path = root.join(mj_core::relay::WORKER_EXIT_FILE);
    let reserved = std::fs::metadata(&path)
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len() as usize)
        .filter(|length| *length > 0);
    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let body = |reason: &str| -> Result<Vec<u8>> {
        Ok(serde_json::to_vec_pretty(&serde_json::json!({
            "reason": reason,
            "refusal": refusal,
            "at": at,
            "version": env!("CARGO_PKG_VERSION"),
        }))?)
    };
    let Some(reserved) = reserved else {
        // Atomic, like the startup record: the controller parses what it reads.
        return mj_core::config::atomic_write(&path, &body(reason)?)
            .with_context(|| format!("write the worker exit record {}", path.display()));
    };
    // The reason's start says what failed; shorten the tail to fit.
    let mut kept = reason.len();
    let mut bytes = body(reason)?;
    while bytes.len() > reserved && kept > 0 {
        kept = reason.floor_char_boundary(kept.saturating_sub(bytes.len() - reserved + 8));
        bytes = body(&format!("{}…", &reason[..kept]))?;
    }
    bytes.resize(reserved, b' ');
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .with_context(|| format!("open the reserved worker exit record {}", path.display()))?;
    file.rewind()?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_data())
        .with_context(|| format!("write the reserved worker exit record {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Record {
        reason: String,
    }

    // Hard-won: 540c920: A full target disk left workers unreachable without persisting the reason into their reserved exit record.
    #[test]
    fn a_reservation_reads_as_no_record_and_takes_the_reason_in_place() {
        let root = tempfile::tempdir().unwrap();
        reserve(root.path()).unwrap();
        let path = root.path().join(mj_core::relay::WORKER_EXIT_FILE);
        let reserved: Option<Record> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            reserved.is_none(),
            "a reservation must read as no exit record"
        );

        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&path).unwrap().ino()
        };
        write(
            root.path(),
            "relay coordinator failed: No space left on device (os error 28)",
            None,
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), RESERVED_EXIT_RECORD_BYTES);
        let record: Option<Record> = serde_json::from_slice(&bytes).unwrap();
        assert!(record.unwrap().reason.contains("No space left on device"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().ino(),
                inode,
                "the record must be written in place, not as a new file"
            );
        }
    }

    #[test]
    fn a_long_reason_is_shortened_to_fit_the_reservation() {
        let root = tempfile::tempdir().unwrap();
        reserve(root.path()).unwrap();
        let reason = format!("journal write failed: {}", "é".repeat(20_000));
        write(root.path(), &reason, None).unwrap();
        let bytes = std::fs::read(root.path().join(mj_core::relay::WORKER_EXIT_FILE)).unwrap();
        assert_eq!(bytes.len(), RESERVED_EXIT_RECORD_BYTES);
        let record: Option<Record> = serde_json::from_slice(&bytes).unwrap();
        let record = record.unwrap();
        assert!(record.reason.starts_with("journal write failed: "));
        assert!(record.reason.ends_with('…'));
    }
}
