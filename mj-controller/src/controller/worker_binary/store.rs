//! Immutable target binaries. Session installation only creates new directory
//! entries; it never writes executable bytes through an existing inode.
use super::*;
use std::sync::{Arc, Mutex, Weak};

/// Beside the session roots, in their target filesystem (inside containers).
pub(super) fn store_root(locator: &targets::TargetLocator, session_id: &str) -> Result<PathBuf> {
    let root = PathBuf::from(targets::worker_root(locator, session_id)?);
    Ok(mj_core::target_path::join(
        root.parent().context("worker root has no parent")?,
        Path::new("worker-binaries"),
    ))
}

/// Key of one target store: this daemon's data directory, the target's command
/// prefix and the store path.
fn store_key(locator: &targets::TargetLocator, root: &str) -> String {
    let command = targets::locator_command(locator, vec!["true".into()]);
    format!(
        "{:?}:{}:{:?}:{root}",
        data_dir(),
        command.program,
        command.args
    )
}

/// One owner per `root` string in this daemon. Callers choose the string: the
/// store gate is never taken for a whole store. An upload takes `{store}/{digest}`
/// so one build is transferred once, and a session's staging registry takes its
/// own root. Cached installs of a published build take none of these. Other
/// daemons targeting the same host are protected by the upload's unique name
/// and its atomic publication. Waiting observes the executor cancellation.
pub(super) fn store_gate(locator: &targets::TargetLocator, root: &str) -> Arc<Mutex<()>> {
    type Gates = Mutex<HashMap<String, Weak<Mutex<()>>>>;
    static GATES: OnceLock<Gates> = OnceLock::new();
    let key = store_key(locator, root);
    let mut gates = GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    gates.retain(|_, gate| gate.strong_count() != 0);
    if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(Mutex::new(()));
    gates.insert(key, Arc::downgrade(&gate));
    gate
}

pub(super) fn lock_gate<'a>(
    executor: &impl CommandExecutor,
    gate: &'a Mutex<()>,
) -> Result<std::sync::MutexGuard<'a, ()>> {
    loop {
        match gate.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                if executor.cancellation_requested() {
                    executor.execute(
                        &CommandSpec::new("true", Vec::<String>::new())
                            .purpose("wait for the shared Mjolnir worker binary"),
                    )?;
                    bail!("worker binary store wait cancelled");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

fn script(
    locator: &targets::TargetLocator,
    body: &str,
    args: &[&str],
    purpose: &str,
) -> CommandSpec {
    let mut argv = vec![
        "sh".into(),
        "-c".into(),
        format!("{}\n{body}", super::upgrade::PROMOTE_WORKER_BINARY),
        "mj-worker-store".into(),
    ];
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    targets::locator_command(locator, argv).purpose(purpose)
}

// Rename an old entry out of the published namespace before deciding whether
// it is unreferenced. A concurrent installer either pins it first, or sees a
// missing path and retries. Never remove a directory that an uploader owns.
const COLLECT: &str = r#"set -eu
store=$1; keep=${2-}
[ -d "$store" ] || exit 0
# Timed-out transfers may not be able to run their cleanup command. All live
# transfers in this daemon share the owner held by collection.
for upload in "$store"/.upload-* "$store"/.transport-*; do
    [ -d "$upload" ] || continue
    [ -n "$(find "$upload" -prune -mmin +1440 -print)" ] || continue
    rm -rf -- "$upload"
done
for retired in "$store"/.gc-*; do
    [ -f "$retired/hel" ] || continue
    [ -n "$(find "$retired/hel" -type f -mmin +1440 -links 1 -print)" ] || continue
    rm -f -- "$retired/hel"
    rmdir -- "$retired"
done
for file in "$store"/*/hel; do
    [ -f "$file" ] || continue
    [ -n "$(find "$file" -type f -mmin +1440 -links 1 -print)" ] || continue
    dir=${file%/hel}; name=${dir##*/}
    case $name in *[!0123456789abcdef]*|'') continue;; esac
    [ ${#name} = 64 ] || continue
    [ "$name" != "$keep" ] || continue
    retired="$store/.gc-$name-$$"
    mv -- "$dir" "$retired" 2>/dev/null || continue
    if [ -n "$(find "$retired/hel" -type f -links 1 -print)" ]; then
        rm -f -- "$retired/hel"
    else
        mkdir -p -- "$dir"
        if [ ! -e "$dir/hel" ]; then
            ln -- "$retired/hel" "$dir/hel" || exit 1
        fi
        rm -f -- "$retired/hel"
    fi
    rmdir -- "$retired"
done"#;

fn collect(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    root: &str,
    keep: Option<&str>,
) -> Result<()> {
    execute_checked(
        executor,
        script(
            locator,
            COLLECT,
            &[root, keep.unwrap_or("")],
            "collect unused Mjolnir worker binaries",
        ),
    )?;
    Ok(())
}

/// Collection only retires entries older than a day, so one run per store and
/// interval is enough; running it for every install queued hours of commands.
const COLLECT_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Claim this daemon's collection for `store`; at most one caller per interval
/// wins and runs it inline, so the run has exactly one owner.
///
/// Why this cannot starve this daemon's own installs: one run retires an
/// entry at most once, and a republished entry has a fresh modification time
/// (the publish script touches each upload), so no later run within the day
/// retires it again.
/// A run therefore forces at most one retry of an install, and the install
/// loop allows three. Entries pinned by a link are restored, never removed.
fn claim_collection(locator: &targets::TargetLocator, store: &str) -> bool {
    let mut last = collections().lock().unwrap_or_else(|e| e.into_inner());
    match last.entry(store_key(locator, store)) {
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            if entry.get().elapsed() < COLLECT_INTERVAL {
                return false;
            }
            entry.insert(Instant::now());
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(Instant::now());
        }
    }
    true
}

fn collections() -> &'static Mutex<HashMap<String, Instant>> {
    static LAST: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    LAST.get_or_init(Default::default)
}

#[cfg(test)]
pub(super) fn forget_collections() {
    collections()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// A failed or skipped run is reported and not retried before the next
/// interval; the next daemon starts with no claim and collects again.
fn collect_if_due(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    store: &str,
    keep: Option<&str>,
) {
    if executor.cancellation_requested() || !claim_collection(locator, store) {
        return;
    }
    if let Err(error) = collect(executor, locator, store, keep) {
        tracing::warn!(%store, error = %format!("{error:#}"), "worker binary store collection failed");
    }
}

pub(super) fn install_worker_binary(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker: &WorkerInstall,
    name: &str,
) -> Result<()> {
    let root = targets::worker_root(locator, session_id)?;
    let destination = mj_core::target_path::text(&mj_core::target_path::join(
        Path::new(&root),
        Path::new(name),
    ));
    install_worker_binary_at(
        executor,
        locator,
        session_id,
        worker,
        &destination,
        Some(&root),
    )
}

// Exit statuses of the link script that mean "this entry cannot serve": the
// path is gone (or retired by a collector), or its bytes do not match.
const LINK_MISSING: i32 = 75;
const LINK_NO_HARD_LINKS: i32 = 76;
const LINK_DIGEST_MISMATCH: i32 = 77;

fn install_worker_binary_at(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker: &WorkerInstall,
    destination: &str,
    session_root: Option<&str>,
) -> Result<()> {
    let binary = match worker {
        WorkerInstall::Host(source) => {
            verify_worker_build(source)?;
            Some((
                source,
                mj_core::worker_launch::worker_executable_digest(source)?,
            ))
        }
        WorkerInstall::Image => None,
    };
    let store = mj_core::target_path::text(&store_root(locator, session_id)?);
    // Register before any pin can be created. Registration and sweeping share
    // one session owner, so no sweep can use a snapshot predating a live pin.
    let name = format!(
        "hel.store-{}",
        crate::session_manager::new_command_id("link")?
    );
    let _live = session_root
        .map(|root| {
            super::upgrade::LiveStaging::register_and_sweep(
                executor, locator, session_id, root, &name,
            )
        })
        .transpose()?;
    let temporary = session_root.map_or_else(
        || format!("{destination}.{name}"),
        |root| format!("{root}/{name}"),
    );
    let Some((source, digest)) = binary else {
        super::upgrade::worker_binary_upload_plan(locator, session_id, worker, destination, None)?
            .execute(executor)?;
        // Switching to an image link can release the last session reference
        // to a host-copied build. Image installs must collect that store too.
        collect_if_due(executor, locator, &store, None);
        return Ok(());
    };
    let directory = format!("{store}/{digest}");
    let cached = format!("{directory}/hel");
    // A published build needs no store lock: the link script verifies the
    // digest of the private pin it creates. Only a missing or corrupt entry
    // takes the upload lock below. A collector in another daemon can retire
    // an entry before the link, so that explicitly unaccepted installation
    // retries with fresh bytes.
    let mut publish = false;
    for _ in 0..3 {
        if publish {
            ensure_published(
                executor, locator, session_id, worker, source, &store, &digest,
            )?;
        }
        let command = |mode| {
            script(
                locator,
                r#"set -eu
source=$1; temporary=$2; destination=$3
trap 'rm -f -- "$temporary"' EXIT
if [ "$5" = link ]; then
    if ! ln -- "$source" "$temporary" 2>/dev/null; then
        [ -f "$source" ] || exit 75
        exit 76
    fi
else
    if ! cp -- "$source" "$temporary"; then
        # A collector in another controller can retire an unpinned source
        # after the failed link. Only that missing-source failure is retryable.
        [ -f "$source" ] || exit 75
        exit 1
    fi
fi
# The private link pins the inode even if another daemon collects the entry.
case $(uname -s) in Darwin) actual=$(shasum -a 256 -- "$temporary");; *) actual=$(sha256sum -- "$temporary");; esac
[ "${actual%% *}" = "$4" ] || { echo 'shared worker digest mismatch' >&2; exit 77; }
promote_worker_binary "$temporary" "$destination"
"#,
                &[&cached, &temporary, destination, &digest, mode],
                "link shared Mjolnir worker into session",
            )
        };
        let mut linked = executor.execute(&command("link"))?;
        if linked.status == LINK_NO_HARD_LINKS {
            crate::target_storage::ensure_room_for(
                locator,
                [temporary.as_str()],
                || super::upgrade::file_size(source),
                "copy the replacement Mjolnir worker",
            )?;
            linked = executor.execute(&command("copy"))?;
        }
        match linked.status {
            0 => {
                // After the pin exists, so this install's own entry is linked.
                collect_if_due(executor, locator, &store, Some(&digest));
                return Ok(());
            }
            LINK_MISSING | LINK_DIGEST_MISMATCH => publish = true,
            _ => bail!(
                "install shared Mjolnir worker: {}",
                String::from_utf8_lossy(&linked.stderr)
            ),
        }
    }
    bail!("shared Mjolnir worker repeatedly retired during installation")
}

/// Make `{store}/{digest}/hel` exist with the right bytes. The lock is per
/// build: the first holder uploads, verifies and publishes, and every later
/// holder finds it published after acquiring and returns without a transfer.
/// Builds that are already published never reach this function, and different
/// builds do not wait for each other.
fn ensure_published(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker: &WorkerInstall,
    source: &Path,
    store: &str,
    digest: &str,
) -> Result<()> {
    let directory = format!("{store}/{digest}");
    let gate = store_gate(locator, &directory);
    let _guard = lock_gate(executor, &gate)?;
    if crate::session_manager::installed_digest_matches(
        executor,
        &super::upgrade::installed_file_digest_command(
            locator,
            &format!("{directory}/hel"),
            "verify shared Mjolnir worker",
        ),
        digest,
    ) {
        return Ok(());
    }
    // Reclaim old builds before the capacity check below can refuse the upload.
    collect_if_due(executor, locator, store, Some(digest));
    let upload = format!(
        "{store}/.upload-{}",
        crate::session_manager::new_command_id("worker-store")?
    );
    let temporary = format!("{upload}/hel");
    crate::target_storage::ensure_room_for(
        locator,
        [upload.as_str()],
        || super::upgrade::file_size(source),
        "stage the replacement Mjolnir worker",
    )?;
    let transfer = (|| -> Result<()> {
        execute_checked(
            executor,
            targets::locator_command(locator, vec!["mkdir".into(), "-p".into(), upload.clone()])
                .purpose("create private worker store upload"),
        )?;
        transfer_binary(executor, locator, session_id, worker, &temporary)?;
        execute_checked(
            executor,
            script(
                locator,
                r#"set -eu
case $(uname -s) in Darwin) actual=$(shasum -a 256 -- "$1");; *) actual=$(sha256sum -- "$1");; esac
[ "${actual%% *}" = "$2" ] || { echo 'uploaded Mjolnir worker digest mismatch' >&2; exit 1; }
"#,
                &[&temporary, digest],
                "verify uploaded Mjolnir worker",
            ),
        )?;
        execute_checked(
            executor,
            script(
                locator,
                r#"set -eu
mkdir -p -- "$2"
# Copies such as docker cp keep the source build's age; collection must see a
# fresh entry.
touch -- "$1"
# Atomic creation converges independent uploaders on the winning inode.
if [ ! -e "$2/hel" ]; then
    if ! ln -- "$1" "$2/hel" 2>/dev/null; then
        [ -f "$2/hel" ] || mv -n -- "$1" "$2/hel"
    fi
fi
case $(uname -s) in Darwin) actual=$(shasum -a 256 -- "$2/hel");; *) actual=$(sha256sum -- "$2/hel");; esac
if [ "${actual%% *}" != "$3" ]; then
    # A corrupt entry is replaced, never written through its existing inode.
    mv -f -- "$1" "$2/hel"
fi
"#,
                &[&temporary, &directory, digest],
                "publish shared Mjolnir worker",
            ),
        )?;
        Ok(())
    })();
    let cleanup = executor.execute_cleanup(
        &targets::locator_command(
            locator,
            vec!["rm".into(), "-rf".into(), "--".into(), upload],
        )
        .purpose("discard private worker store upload"),
    );
    match cleanup {
        Ok(output) if output.status == 0 => {}
        other => tracing::warn!(result = ?other, "worker store upload cleanup failed"),
    }
    transfer
}

/// SSH containers reuse the host store as their transport cache, then publish
/// into their own filesystem. The private host link pins bytes during engine cp.
fn transfer_binary(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker: &WorkerInstall,
    destination: &str,
) -> Result<()> {
    let remote = match locator {
        targets::TargetLocator::SshPodman { ssh, .. }
        | targets::TargetLocator::SshDocker { ssh, .. }
        | targets::TargetLocator::SshAppleContainer { ssh, .. } => Some(ssh),
        _ => None,
    };
    if let Some(ssh) = remote {
        let host = targets::TargetLocator::SshBare {
            ssh: ssh.clone(),
            worker_id: None,
            workspace: format!(".local/share/hel/workspaces/{session_id}"),
        };
        let transport = mj_core::target_path::text(&mj_core::target_path::join(
            &store_root(&host, session_id)?,
            Path::new(&format!(
                ".transport-{}",
                crate::session_manager::new_command_id("container-upload")?
            )),
        ));
        let pin = mj_core::target_path::text(&mj_core::target_path::join(
            Path::new(&transport),
            Path::new("hel"),
        ));
        let result = (|| {
            execute_checked(
                executor,
                targets::locator_command(
                    &host,
                    vec!["mkdir".into(), "-p".into(), transport.clone()],
                )
                .purpose("create private host worker transport"),
            )?;
            install_worker_binary_at(executor, &host, session_id, worker, &pin, None)?;
            super::upgrade::worker_binary_upload_plan(
                locator,
                session_id,
                worker,
                destination,
                Some(&pin),
            )?
            .execute(executor)
            .map(|_| ())
        })();
        let cleanup = executor.execute_cleanup(
            &targets::locator_command(
                &host,
                vec!["rm".into(), "-rf".into(), "--".into(), transport],
            )
            .purpose("discard host worker transport pin"),
        );
        match cleanup {
            Ok(output) if output.status == 0 => {}
            other => tracing::warn!(result = ?other, "host worker transport pin cleanup failed"),
        }
        result
    } else {
        super::upgrade::worker_binary_upload_plan(locator, session_id, worker, destination, None)?
            .execute(executor)
            .map(|_| ())
    }
}

#[cfg(all(test, unix))]
mod tests;
