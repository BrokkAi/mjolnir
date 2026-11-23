//! Locked `authorized_keys` editor for temporary direct-transfer SSH grants.
//!
//! Every edit to `~/.ssh/authorized_keys` (install, remove, reconcile) goes
//! through this module. It takes an exclusive `flock` on
//! `authorized_keys.mj-lock` beside `authorized_keys`, preserves `0700` on the
//! parent `.ssh` directory and `0600` on `authorized_keys`, writes to a
//! temporary file in the same directory, fsyncs, and atomically renames into
//! place.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use tempfile::NamedTempFile;

/// Comment prefix identifying a Mjolnir Move transfer key.
pub const MOVE_KEY_COMMENT_PREFIX: &str = "mj-move-";

/// Lock file name placed beside `authorized_keys`.
pub const LOCK_FILE_NAME: &str = "authorized_keys.mj-lock";

/// Format the canonical comment for a move operation's temporary key.
pub fn move_key_comment(operation_id: &str) -> Result<String> {
    validate_operation_id(operation_id)?;
    Ok(format!("{MOVE_KEY_COMMENT_PREFIX}{operation_id}"))
}

/// Validate an operation identifier before using it in a key comment.
pub fn validate_operation_id(operation_id: &str) -> Result<()> {
    ensure!(!operation_id.is_empty(), "operation_id must not be empty");
    ensure!(
        operation_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "operation_id must contain only ASCII alphanumeric characters, '-' or '_'"
    );
    Ok(())
}

/// Validate an SSH `authorized_keys` option value (`from` or `command`).
///
/// Rejects ASCII control characters (including CR/LF/NUL) as well as `"` and
/// `\` so the value cannot break out of double-quoted option syntax.
fn validate_option_value(label: &str, value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "{label} must not be empty");
    ensure!(
        !value
            .chars()
            .any(|c| c.is_control() || c == '"' || c == '\\'),
        "{label} contains forbidden control or quoting characters"
    );
    Ok(())
}

/// Validate an ed25519 public key line before embedding it in `authorized_keys`.
fn validate_ed25519_pubkey(public_key: &str) -> Result<String> {
    let trimmed = public_key.trim();
    ensure!(!trimmed.is_empty(), "public_key must not be empty");
    ensure!(
        !trimmed.chars().any(char::is_control),
        "public_key contains control characters"
    );
    let mut parts = trimmed.split_whitespace();
    let key_type = parts.next().context("public_key missing key type")?;
    let key_data = parts.next().context("public_key missing key data")?;
    ensure!(
        key_type == "ssh-ed25519",
        "expected ssh-ed25519 public key, got {key_type}"
    );
    ensure!(
        key_data
            .bytes()
            .all(|b| { b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=' }),
        "public_key contains invalid base64 characters"
    );
    Ok(format!("{key_type} {key_data}"))
}

/// Format a single restricted `authorized_keys` line for a Move transfer.
pub fn format_restricted_line(
    source_address: &str,
    restricted_worker_command: &str,
    ed25519_pubkey: &str,
    operation_id: &str,
) -> Result<String> {
    validate_option_value("source_address", source_address)?;
    validate_option_value("restricted_worker_command", restricted_worker_command)?;
    let pubkey = validate_ed25519_pubkey(ed25519_pubkey)?;
    let comment = move_key_comment(operation_id)?;
    Ok(format!(
        "restrict,from=\"{source_address}\",command=\"{restricted_worker_command}\" {pubkey} {comment}"
    ))
}

/// Extract the `mj-move-<id>` comment token ONLY if `line` matches the full
/// managed key shape emitted by [`format_restricted_line`]:
/// `restrict,from="...",command="..." ssh-ed25519 <base64> mj-move-<id>`.
///
/// Unrelated user keys (even those whose comment happens to end with
/// `mj-move-<id>`) return `None` and are never modified.
fn managed_move_key_comment(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if !trimmed.starts_with("restrict,from=\"") {
        return None;
    }
    let after_from_prefix = trimmed.strip_prefix("restrict,from=\"")?;
    let (_, after_from) = after_from_prefix.split_once("\",command=\"")?;
    let (_, rest) = after_from.split_once("\" ")?;
    let mut tokens = rest.split_whitespace();
    let key_type = tokens.next()?;
    let _key_data = tokens.next()?;
    let comment = tokens.next()?;
    if tokens.next().is_some() {
        return None;
    }
    if key_type != "ssh-ed25519" || !comment.starts_with(MOVE_KEY_COMMENT_PREFIX) {
        return None;
    }
    Some(comment)
}

/// Install (or idempotently replace) a restricted temporary key in `path`.
pub fn install_key(
    path: &Path,
    operation_id: &str,
    source_address: &str,
    restricted_worker_command: &str,
    ed25519_pubkey: &str,
) -> Result<()> {
    let comment = move_key_comment(operation_id)?;
    let formatted = format_restricted_line(
        source_address,
        restricted_worker_command,
        ed25519_pubkey,
        operation_id,
    )?;

    with_locked_authorized_keys(path, true, |lines| {
        lines.retain(|line| managed_move_key_comment(line) != Some(comment.as_str()));
        lines.push(formatted);
        Ok(true)
    })?;
    Ok(())
}

/// Remove only lines carrying the exact comment `mj-move-<operation_id>`.
///
/// Idempotent when the file or the key line does not exist. Returns `true` if
/// at least one line was removed.
pub fn remove_key(path: &Path, operation_id: &str) -> Result<bool> {
    let comment = move_key_comment(operation_id)?;
    let mut removed = false;
    with_locked_authorized_keys(path, false, |lines| {
        let before = lines.len();
        lines.retain(|line| managed_move_key_comment(line) != Some(comment.as_str()));
        removed = lines.len() != before;
        Ok(removed)
    })?;
    Ok(removed)
}

/// Remove any `mj-move-*` key lines whose operation ID is not in
/// `active_operations`. Unrelated user keys are never modified.
///
/// Returns the number of removed lines.
pub fn reconcile_keys(path: &Path, active_operations: &BTreeSet<String>) -> Result<usize> {
    for op in active_operations {
        validate_operation_id(op)?;
    }
    let mut removed = 0usize;
    with_locked_authorized_keys(path, false, |lines| {
        let before = lines.len();
        lines.retain(|line| {
            let Some(comment) = managed_move_key_comment(line) else {
                return true;
            };
            let Some(op_id) = comment.strip_prefix(MOVE_KEY_COMMENT_PREFIX) else {
                return true;
            };
            active_operations.contains(op_id)
        });
        removed = before - lines.len();
        Ok(removed > 0)
    })?;
    Ok(removed)
}

/// Run `edit` while holding an exclusive `flock` on `authorized_keys.mj-lock`
/// beside `path`. If `edit` returns `Ok(true)`, atomically rewrites `path` with
/// mode `0600`.
fn with_locked_authorized_keys(
    path: &Path,
    create_parent_if_missing: bool,
    edit: impl FnOnce(&mut Vec<String>) -> Result<bool>,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("authorized_keys path must have a parent directory")?;

    if !parent.exists() {
        if !create_parent_if_missing {
            return Ok(());
        }
        create_ssh_dir(parent)?;
    }

    let lock_path = parent.join(LOCK_FILE_NAME);
    let _lock = ExclusiveFileLock::acquire(&lock_path)?;

    if !path.exists() && !create_parent_if_missing {
        return Ok(());
    }

    let existing = if path.exists() {
        fs::read_to_string(path)
            .with_context(|| format!("read authorized_keys {}", path.display()))?
    } else {
        String::new()
    };

    let mut lines: Vec<String> = existing.lines().map(ToOwned::to_owned).collect();
    let changed = edit(&mut lines)?;
    if !changed {
        return Ok(());
    }

    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("create temp authorized_keys in {}", parent.display()))?;
    set_file_mode_0600(temp.as_file(), temp.path())?;
    for line in &lines {
        writeln!(temp, "{line}")
            .with_context(|| format!("write temp authorized_keys in {}", parent.display()))?;
    }
    temp.as_file()
        .sync_all()
        .with_context(|| format!("fsync temp authorized_keys in {}", parent.display()))?;
    temp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("rename temp authorized_keys into {}", path.display()))?;
    Ok(())
}

fn create_ssh_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create ssh directory {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 {}", dir.display()))?;
    }
    Ok(())
}

fn set_file_mode_0600(file: &File, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (file, path);
    }
    Ok(())
}

/// Exclusive `flock`-backed file lock that releases on drop.
struct ExclusiveFileLock {
    _file: File,
    _path: PathBuf,
}

impl ExclusiveFileLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open lock file {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            loop {
                // SAFETY: `file.as_raw_fd()` is a valid open descriptor owned by `file`.
                let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
                if rc == 0 {
                    break;
                }
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                anyhow::bail!("flock {} failed: {err}", path.display());
            }
        }
        Ok(Self {
            _file: file,
            _path: path.to_path_buf(),
        })
    }
}

impl Drop for ExclusiveFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: `self._file` is still open until after `drop` finishes.
            unsafe {
                libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    const SAMPLE_PUBKEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

    #[test]
    fn concurrent_installs_and_removals_preserve_grants_and_permissions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ssh_dir = dir.path().join(".ssh");
        let auth_keys = ssh_dir.join("authorized_keys");

        // Pre-populate with an unrelated user key.
        create_ssh_dir(&ssh_dir).expect("create .ssh");
        let user_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB user@host";
        fs::write(&auth_keys, format!("{user_key}\n")).expect("write user key");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&auth_keys, fs::Permissions::from_mode(0o600)).expect("chmod 0600");
        }

        // Pre-install 8 keys that will be removed concurrently while 8 new keys
        // are installed concurrently.
        for i in 0..8 {
            install_key(
                &auth_keys,
                &format!("remove-{i}"),
                "10.0.0.1",
                "mj-worker worker receive-move-stream",
                SAMPLE_PUBKEY,
            )
            .expect("pre-install");
        }

        let barrier = Arc::new(Barrier::new(16));
        let mut handles = Vec::new();
        for i in 0..8 {
            let path = auth_keys.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                install_key(
                    &path,
                    &format!("keep-{i}"),
                    "10.0.0.2",
                    "mj-worker worker receive-move-stream",
                    SAMPLE_PUBKEY,
                )
                .expect("concurrent install");
            }));
        }
        for i in 0..8 {
            let path = auth_keys.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                let removed = remove_key(&path, &format!("remove-{i}")).expect("concurrent remove");
                assert!(removed, "expected remove-{i} to be removed");
            }));
        }
        for handle in handles {
            handle.join().expect("thread join");
        }

        let contents = fs::read_to_string(&auth_keys).expect("read final authorized_keys");
        let lines: Vec<&str> = contents.lines().collect();
        assert!(lines.contains(&user_key), "unrelated user key must survive");
        for i in 0..8 {
            let keep_comment = format!("mj-move-keep-{i}");
            assert!(
                lines
                    .iter()
                    .any(|l| managed_move_key_comment(l) == Some(keep_comment.as_str())),
                "concurrently installed key {keep_comment} was lost"
            );
            let remove_comment = format!("mj-move-remove-{i}");
            assert!(
                !lines
                    .iter()
                    .any(|l| managed_move_key_comment(l) == Some(remove_comment.as_str())),
                "removed key {remove_comment} was resurrected"
            );
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&auth_keys)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "authorized_keys must stay 0600");
        }

        // Reconcile keeps only keep-0 and keep-1, leaving user_key untouched.
        let active: BTreeSet<String> = ["keep-0".to_owned(), "keep-1".to_owned()].into();
        let reconciled = reconcile_keys(&auth_keys, &active).expect("reconcile");
        assert_eq!(reconciled, 6);
        let final_contents = fs::read_to_string(&auth_keys).expect("read reconciled");
        assert!(final_contents.lines().any(|l| l == user_key));
        assert_eq!(final_contents.lines().count(), 3);
    }

    #[test]
    fn install_rejects_control_chars_and_unsafe_quoting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_keys = dir.path().join(".ssh/authorized_keys");

        assert!(
            install_key(
                &auth_keys,
                "op1",
                "10.0.0.1\",command=\"evil",
                "mj-worker",
                SAMPLE_PUBKEY,
            )
            .is_err()
        );
        assert!(
            install_key(
                &auth_keys,
                "op1",
                "10.0.0.1",
                "mj-worker\nssh-ed25519 AAAA",
                SAMPLE_PUBKEY,
            )
            .is_err()
        );
        assert!(
            install_key(
                &auth_keys,
                "op1",
                "10.0.0.1",
                "mj-worker",
                &format!("{SAMPLE_PUBKEY}\nextra"),
            )
            .is_err()
        );
        assert!(install_key(&auth_keys, "op 1", "10.0.0.1", "mj-worker", SAMPLE_PUBKEY,).is_err());
    }

    #[test]
    fn leaves_unrelated_key_whose_comment_ends_with_mj_move_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ssh_dir = dir.path().join(".ssh");
        let auth_keys = ssh_dir.join("authorized_keys");
        create_ssh_dir(&ssh_dir).expect("create .ssh");

        let unrelated = format!("{SAMPLE_PUBKEY} alice laptop mj-move-op1");
        fs::write(&auth_keys, format!("{unrelated}\n")).expect("write unrelated key");

        // Install, remove, and reconcile for `op1` must never touch `unrelated`.
        install_key(&auth_keys, "op1", "10.0.0.1", "mj-worker", SAMPLE_PUBKEY).expect("install");
        assert!(remove_key(&auth_keys, "op1").expect("remove"));
        assert_eq!(
            reconcile_keys(&auth_keys, &BTreeSet::new()).expect("reconcile"),
            0
        );

        let contents = fs::read_to_string(&auth_keys).expect("read final");
        assert_eq!(contents.trim(), unrelated);
    }
}
