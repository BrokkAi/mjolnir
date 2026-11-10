//! Credential-slot ownership shared by sync and quota refreshers.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

use super::{
    CredentialSnapshot, SyncAction, read_credential_file, reconcile, refuse_symlinked_token,
    validate_credential_payload, write_credential_file,
};
use crate::config::HarnessKind;
use crate::file_lock::FileLock;

/// Shared Claude lock-acquisition budget, also included in quota query timeouts.
pub const CLAUDE_REFRESH_LOCK_WAIT: Duration = Duration::from_secs(10);

/// Owns the destination's native refresh locks, or Mjolnir's sidecar lock.
/// Codex processes outside Mjolnir do not participate in its sidecar protocol.
pub struct CredentialWriteGuard {
    _directories: Vec<OAuthRefreshLock>,
    _file: Option<FileLock>,
}

impl CredentialWriteGuard {
    pub async fn acquire(kind: HarnessKind, marker: &Path) -> Result<Self> {
        let marker = marker.to_owned();
        let home = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
            let parent = marker.parent().context("credential marker has no parent")?;
            std::fs::create_dir_all(parent)?;
            let parent = std::fs::canonicalize(parent).context("resolve credential home")?;
            if kind == HarnessKind::Kimi {
                Ok(parent
                    .parent()
                    .context("Kimi credentials have no home")?
                    .to_owned())
            } else {
                Ok(parent)
            }
        })
        .await
        .context("resolve credential slot task")??;
        let mut guard = Self {
            _directories: Vec::new(),
            _file: None,
        };
        match kind {
            HarnessKind::Claude => {
                // Claude 2.1.312 takes these proper-lockfile directory locks
                // in this order during refresh (explicit auth login does not).
                // Preserve the refresh protocol's 60s stale / 5s heartbeat
                // and Mjolnir's existing shared acquisition budget.
                let mut legacy = home.as_os_str().to_os_string();
                legacy.push(".lock");
                let deadline = tokio::time::Instant::now() + CLAUDE_REFRESH_LOCK_WAIT;
                for path in [home.join(".oauth_refresh.lock"), PathBuf::from(legacy)] {
                    guard._directories.push(
                        OAuthRefreshLock::acquire(
                            path,
                            deadline.saturating_duration_since(tokio::time::Instant::now()),
                            Duration::from_secs(60),
                            Duration::from_secs(5),
                        )
                        .await?,
                    );
                }
            }
            HarnessKind::Kimi => guard
                ._directories
                .push(acquire_kimi_lock(&home, KIMI_LOCK_WAIT).await?),
            HarnessKind::Muse => {
                // Muse serializes auth.json writes with an exclusive OS lock
                // on .auth.json.lock (native acquisition waits up to 5s).
                // Preserve Mjolnir's existing 10s acquisition budget.
                guard._file = Some(
                    acquire_file_lock(home.join(".auth.json.lock"), Some(Duration::from_secs(10)))
                        .await?,
                )
            }
            _ => guard._file = Some(acquire_file_lock(home.join(".mj-auth.lock"), None).await?),
        }
        Ok(guard)
    }
}

/// The snapshot actually retained by the destination after convergence.
pub struct CredentialInstall {
    pub snapshot: CredentialSnapshot,
    pub installed: bool,
}

/// Who owns the destination file, which decides what may replace it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum InstallPolicy {
    /// The profile's own login: only a strictly newer copy replaces it, and a
    /// copy without a refresh token never replaces one that has it, because
    /// that would drop the grant the daemon alone refreshes.
    Canonical,
    /// A harness's copy of the login. Also replaced by a same-expiry copy
    /// without a refresh token, which is how a copy staged before the daemon
    /// became the only refresher converges (#1490).
    SessionCopy,
}

fn replaces(
    policy: InstallPolicy,
    kind: HarnessKind,
    current: (&CredentialSnapshot, &[u8]),
    incoming: (&CredentialSnapshot, &[u8]),
) -> bool {
    if reconcile(current.0, incoming.0) == SyncAction::Pull {
        return policy == InstallPolicy::SessionCopy
            || !(super::credential_holds_refresh_token(kind, current.1)
                && !super::credential_holds_refresh_token(kind, incoming.1));
    }
    policy == InstallPolicy::SessionCopy
        && kind == HarnessKind::Claude
        && !super::credential_holds_refresh_token(kind, incoming.1)
        && current.0.present
        && current.0.freshness_epoch_ms.is_some()
        && current.0.freshness_epoch_ms == incoming.0.freshness_epoch_ms
        && current.0.fingerprint != incoming.0.fingerprint
}

/// Install a copy into the profile's own login, only when it is newer and does
/// not lose the refresh token the current one holds.
pub async fn install_credentials_if_newer(
    kind: HarnessKind,
    path: &Path,
    bytes: &[u8],
) -> Result<CredentialInstall> {
    install_credentials(kind, path, bytes, InstallPolicy::Canonical).await
}

/// Install a delivered copy into a harness's own credential file: newer
/// copies replace it, and so does a same-expiry copy without a refresh token.
/// A worker applies this to the controller's pushes.
pub async fn install_session_credentials(
    kind: HarnessKind,
    path: &Path,
    bytes: &[u8],
) -> Result<CredentialInstall> {
    install_credentials(kind, path, bytes, InstallPolicy::SessionCopy).await
}

async fn install_credentials(
    kind: HarnessKind,
    path: &Path,
    bytes: &[u8],
    policy: InstallPolicy,
) -> Result<CredentialInstall> {
    validate_credential_payload(kind, bytes)?;
    let guard = CredentialWriteGuard::acquire(kind, path).await?;
    let path = path.to_owned();
    let bytes = bytes.to_owned();
    tokio::task::spawn_blocking(move || {
        // Blocking writes cannot be cancelled: ownership follows the write.
        let _guard = guard;
        refuse_symlinked_token("credential", &path)?;
        let (current, current_bytes) = read_credential_file(kind, &path)?;
        let incoming = CredentialSnapshot::of(kind, &bytes);
        if replaces(
            policy,
            kind,
            (&current, &current_bytes),
            (&incoming, &bytes),
        ) {
            write_credential_file(kind, &path, &bytes)?;
            Ok(CredentialInstall {
                snapshot: incoming,
                installed: true,
            })
        } else {
            Ok(CredentialInstall {
                snapshot: current,
                installed: false,
            })
        }
    })
    .await
    .context("install credential task")?
}

async fn acquire_file_lock(path: PathBuf, wait: Option<Duration>) -> Result<FileLock> {
    let open_path = path.clone();
    let file = tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(open_path)
    })
    .await
    .context("open credential lock task")?
    .with_context(|| format!("open credential lock {}", path.display()))?;
    let deadline = wait.map(|wait| tokio::time::Instant::now() + wait);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(FileLock::new(file)),
            Err(std::fs::TryLockError::WouldBlock) => {
                if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                    bail!("timed out waiting for credential lock {}", path.display());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("lock credential {}", path.display()));
            }
        }
    }
}

// Kimi Code uses proper-lockfile on oauth/kimi-code (native stale: 5s).
// Preserve Mjolnir's existing 10s abandonment threshold and 1s heartbeat;
// frequent beats keep our live lease inside the CLI's shorter break window.
const KIMI_LOCK_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const KIMI_LOCK_STALE_AFTER: Duration = Duration::from_secs(10);
const KIMI_LOCK_WAIT: Duration = Duration::from_secs(60);
async fn acquire_kimi_lock(home: &Path, wait: Duration) -> Result<OAuthRefreshLock> {
    let oauth_dir = home.join("oauth");
    tokio::fs::create_dir_all(&oauth_dir)
        .await
        .context("prepare Kimi Code OAuth lock")?;
    // proper-lockfile requires the sentinel to exist before taking its lock.
    tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(oauth_dir.join("kimi-code"))
        .await
        .context("prepare Kimi Code OAuth lock sentinel")?;
    OAuthRefreshLock::acquire(
        oauth_dir.join("kimi-code.lock"),
        wait,
        KIMI_LOCK_STALE_AFTER,
        KIMI_LOCK_HEARTBEAT_INTERVAL,
    )
    .await
}

pub(crate) struct OAuthRefreshLock {
    // Serialize heartbeat and release so an in-flight beat cannot touch a
    // directory acquired by the next owner after our release.
    path: Arc<Mutex<Option<PathBuf>>>,
    heartbeat: tokio::task::JoinHandle<()>,
}

impl OAuthRefreshLock {
    pub(crate) async fn acquire(
        path: PathBuf,
        wait: Duration,
        stale_after: Duration,
        heartbeat_interval: Duration,
    ) -> Result<Self> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let attempt_path = path.clone();
            let runtime = tokio::runtime::Handle::current();
            // Create the lease and its Rust owner in one blocking operation.
            // Cancelling the waiter drops that owner instead of orphaning a
            // directory created by an already-running filesystem operation.
            let attempt = tokio::task::spawn_blocking(move || -> std::io::Result<Self> {
                std::fs::create_dir(&attempt_path)?;
                let path = Arc::new(Mutex::new(Some(attempt_path)));
                let heartbeat_path = path.clone();
                let heartbeat = runtime.spawn(async move {
                    loop {
                        tokio::time::sleep(heartbeat_interval).await;
                        let heartbeat_path = heartbeat_path.clone();
                        match tokio::task::spawn_blocking(move || {
                            let held = heartbeat_path.lock().unwrap();
                            let Some(path) = held.as_ref() else { return false };
                            if let Err(error) = touch(path, SystemTime::now()) {
                                tracing::warn!(path = %path.display(), %error, "heartbeat OAuth refresh lock");
                            }
                            true
                        }).await {
                            Ok(true) => {},
                            Ok(false) => break,
                            Err(error) => {
                                tracing::warn!(%error, "OAuth refresh heartbeat task failed");
                                break;
                            }
                        }
                    }
                });
                Ok(Self { path, heartbeat })
            }).await.context("acquire OAuth refresh lock task")?;
            match attempt {
                Ok(guard) => return Ok(guard),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if tokio::time::Instant::now() >= deadline {
                        bail!(
                            "timed out waiting for OAuth refresh lock {}",
                            path.display()
                        );
                    }
                    if !break_stale_lock(&path, stale_after).await {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
                Err(error) => return Err(error).context("acquire OAuth refresh lock"),
            }
        }
    }
}

impl Drop for OAuthRefreshLock {
    fn drop(&mut self) {
        self.heartbeat.abort();
        if let Some(path) = self.path.lock().unwrap().take() {
            match std::fs::remove_dir(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "release OAuth refresh lock")
                }
            }
        }
    }
}

pub(crate) fn mtime(path: &Path) -> std::io::Result<Option<SystemTime>> {
    match std::fs::metadata(path) {
        Ok(metadata) => metadata.modified().map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn touch(path: &Path, modified: SystemTime) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    options
        .open(path)?
        .set_times(std::fs::FileTimes::new().set_modified(modified))
}

async fn break_stale_lock(path: &Path, stale_after: Duration) -> bool {
    let modified = match mtime(path) {
        Ok(Some(modified)) => modified,
        Ok(None) => return true,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "inspect OAuth refresh lock");
            return false;
        }
    };
    // proper-lockfile can round its writes into the future.
    let Some(age) = SystemTime::now()
        .duration_since(modified)
        .ok()
        .filter(|age| *age >= stale_after)
    else {
        return false;
    };
    match tokio::fs::remove_dir(path).await {
        Ok(()) => {
            tracing::warn!(path = %path.display(), age_seconds = age.as_secs(), "removed an abandoned OAuth refresh lock");
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "remove stale OAuth refresh lock");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Backdate the lock directory the way a holder that stopped heartbeating
    /// leaves it behind.
    fn age_kimi_lock(path: &Path, age: Duration) {
        touch(path, SystemTime::now() - age).expect("backdate lock directory");
    }

    #[tokio::test]
    #[cfg_attr(windows, ignore = "#1299: Windows file locking and directory times")]
    async fn a_kimi_refresh_lock_left_by_a_crashed_holder_is_broken_and_reacquired() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("oauth/kimi-code.lock");
        std::fs::create_dir_all(&lock).unwrap();
        age_kimi_lock(&lock, KIMI_LOCK_STALE_AFTER + Duration::from_secs(60));

        let started = std::time::Instant::now();
        let held = acquire_kimi_lock(home.path(), Duration::from_secs(10))
            .await
            .expect("an orphaned lock must not block a refresh");
        let waited = started.elapsed();

        assert!(
            waited < Duration::from_secs(5),
            "acquisition waited {waited:?}"
        );
        drop(held);
        assert!(!lock.exists(), "the released lock must be gone");
    }

    #[tokio::test]
    async fn a_heartbeating_kimi_refresh_lock_is_not_broken_by_a_waiter() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("oauth/kimi-code.lock");
        std::fs::create_dir_all(&lock).unwrap();

        let error = acquire_kimi_lock(home.path(), Duration::from_millis(600))
            .await
            .err()
            .expect("a lock with a live holder must be waited out, not stolen");

        assert!(
            error.to_string().contains("kimi-code.lock"),
            "the timeout must name the lock: {error}"
        );
        assert!(lock.exists(), "a live holder's lock must survive a waiter");
    }

    /// The Kimi Code CLI breaks a lock whose modification time is more than
    /// five seconds old, so Mjolnir's beats have to be frequent enough that a
    /// stalled heartbeat task still cannot cost it a live lock.
    #[tokio::test]
    #[cfg_attr(windows, ignore = "#1299: Windows file locking and directory times")]
    async fn a_held_kimi_lock_republishes_its_mtime_several_times_per_cli_break_window() {
        let home = tempfile::tempdir().unwrap();
        let held = acquire_kimi_lock(home.path(), KIMI_LOCK_WAIT)
            .await
            .unwrap();
        let lock = home.path().join("oauth/kimi-code.lock");

        // Half the peer's break window: two beats have to land inside it, so
        // Mjolnir publishes at least four times per window and can miss several in
        // a row and still hold the lock.
        const KIMI_CLI_LOCK_STALE_AFTER: Duration = Duration::from_secs(5);
        let watched = KIMI_CLI_LOCK_STALE_AFTER / 2;
        let deadline = tokio::time::Instant::now() + watched;
        let mut published = vec![mtime(&lock).unwrap().expect("the created lock")];
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let observed = mtime(&lock).unwrap().expect("a held lock");
            if published.last() != Some(&observed) {
                published.push(observed);
            }
            assert_eq!(
                std::fs::read_dir(&lock).unwrap().count(),
                0,
                "the CLI releases this lock with a plain rmdir, so it must stay empty"
            );
        }

        assert!(
            published.len() >= 3,
            "the lock's modification time moved {} times in {watched:?}; the Kimi Code CLI breaks a lock after {KIMI_CLI_LOCK_STALE_AFTER:?} without a beat",
            published.len() - 1
        );
        drop(held);
    }

    // Child entry point for cross-process OS-lock compatibility and cancellation.
    #[test]
    fn credential_lock_peer() {
        let Some(root) = std::env::var_os("MJ_CREDENTIAL_LOCK_PEER") else {
            return;
        };
        let root = PathBuf::from(root);
        let lock_name = std::env::var("MJ_CREDENTIAL_LOCK_NAME").unwrap();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.join(lock_name))
            .unwrap();
        file.lock().unwrap();
        let _lock = FileLock::new(file);
        std::fs::write(root.join("ready"), b"ready").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !root.join("release").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "parent did not release peer"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::copy(root.join("refreshed.json"), root.join("auth.json")).unwrap();
    }

    // Hard-won: #1485: sync overwrote credentials refreshed while it waited.
    #[tokio::test]
    async fn destination_lock_rereads_after_process_refresh_and_cancelled_waiter() {
        for (kind, lock_name, incoming, refreshed) in [
            (
                HarnessKind::Codex,
                ".mj-auth.lock",
                r#"{"last_refresh":"2026-01-01T00:00:00Z"}"#,
                r#"{"last_refresh":"2026-02-01T00:00:00Z"}"#,
            ),
            (
                HarnessKind::Muse,
                ".auth.json.lock",
                r#"{"providers":{"meta":{"expires_at":100}}}"#,
                r#"{"providers":{"meta":{"expires_at":200}}}"#,
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("refreshed.json"), refreshed).unwrap();
            let child_root = root.path().to_owned();
            let child = tokio::task::spawn_blocking(move || {
                crate::subprocess::run_capturing_stdout(
                    std::process::Command::new(std::env::current_exe().unwrap())
                        .args([
                            "--exact",
                            "credentials::locking::tests::credential_lock_peer",
                            "--nocapture",
                        ])
                        .env("MJ_CREDENTIAL_LOCK_PEER", child_root)
                        .env("MJ_CREDENTIAL_LOCK_NAME", lock_name),
                )
                .unwrap()
            });
            tokio::time::timeout(Duration::from_secs(10), async {
                while !root.path().join("ready").exists() {
                    assert!(
                        !child.is_finished(),
                        "lock peer exited before acquiring lock"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let marker = root.path().join("auth.json");
            let cancelled_marker = marker.clone();
            let mut cancelled = tokio::spawn(async move {
                CredentialWriteGuard::acquire(kind, &cancelled_marker).await
            });
            assert!(
                tokio::time::timeout(Duration::from_millis(150), &mut cancelled)
                    .await
                    .is_err()
            );
            cancelled.abort();
            assert!(matches!(cancelled.await, Err(error) if error.is_cancelled()));
            let other = tempfile::tempdir().unwrap();
            let independent = tokio::time::timeout(
                Duration::from_secs(2),
                CredentialWriteGuard::acquire(kind, &other.path().join("auth.json")),
            )
            .await
            .unwrap()
            .unwrap();
            drop(independent);
            let install_marker = marker.clone();
            let mut install = tokio::spawn(async move {
                install_credentials_if_newer(kind, &install_marker, incoming.as_bytes())
                    .await
                    .unwrap()
            });
            assert!(
                tokio::time::timeout(Duration::from_millis(150), &mut install)
                    .await
                    .is_err()
            );
            std::fs::write(root.path().join("release"), b"release").unwrap();
            let output = child.await.unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            let result = tokio::time::timeout(Duration::from_secs(5), install)
                .await
                .unwrap()
                .unwrap();
            assert!(!result.installed);
            assert_eq!(
                result.snapshot,
                CredentialSnapshot::of(kind, refreshed.as_bytes())
            );
            assert_eq!(std::fs::read(marker).unwrap(), refreshed.as_bytes());
        }
    }

    // Hard-won: #1485: aliases must share the same credential-slot owner.
    #[cfg(unix)]
    #[tokio::test]
    async fn home_aliases_contend_without_replacing_auth_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&home, &alias).unwrap();
        let held = CredentialWriteGuard::acquire(HarnessKind::Codex, &home.join("auth.json"))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(150),
                CredentialWriteGuard::acquire(HarnessKind::Codex, &alias.join("auth.json"))
            )
            .await
            .is_err()
        );
        drop(held);
        let target = root.path().join("target.json");
        std::os::unix::fs::symlink(&target, home.join("auth.json")).unwrap();
        let earlier = br#"{"last_refresh":"2026-01-01T00:00:00Z"}"#.as_slice();
        let later = br#"{"last_refresh":"2026-02-01T00:00:00Z"}"#.as_slice();
        let unknown = b"{}".as_slice();
        // Refuse the linked destination before reading, regardless of whether
        // freshness would select a write, equality, retention, or no decision.
        for (existing, incoming) in [
            (unknown, earlier),
            (earlier, earlier),
            (later, earlier),
            (unknown, unknown),
        ] {
            std::fs::write(&target, existing).unwrap();
            let error = install_credentials_if_newer(
                HarnessKind::Codex,
                &alias.join("auth.json"),
                incoming,
            )
            .await
            .err()
            .unwrap();
            assert!(error.to_string().contains("symbolic link"));
            assert_eq!(std::fs::read(&target).unwrap(), existing);
            assert!(
                std::fs::symlink_metadata(home.join("auth.json"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }
}
