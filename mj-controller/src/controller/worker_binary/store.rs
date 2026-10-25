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

// One owner serializes preparation and collection on a target in this daemon.
// The upload's unique name and atomic publication also protect different
// daemons targeting the same host. Waiting observes the supervising executor cancellation.
pub(super) fn store_gate(locator: &targets::TargetLocator, root: &str) -> Arc<Mutex<()>> {
    type Gates = Mutex<HashMap<String, Weak<Mutex<()>>>>;
    static GATES: OnceLock<Gates> = OnceLock::new();
    let command = targets::locator_command(locator, vec!["true".into()]);
    let key = format!(
        "{:?}:{}:{:?}:{root}",
        data_dir(),
        command.program,
        command.args
    );
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

/// Called under the same store owner as staging. The one-day grace avoids
/// collecting newly uploaded entries before another session can use them.
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
    let gate = store_gate(locator, &store);
    let _guard = lock_gate(executor, &gate)?;
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
        if let Err(error) = collect(executor, locator, &store, None) {
            tracing::warn!(%store, error = %format!("{error:#}"), "worker binary store collection failed");
        }
        return Ok(());
    };
    if let Err(error) = collect(executor, locator, &store, Some(&digest)) {
        tracing::warn!(%store, error = %format!("{error:#}"), "worker binary store collection failed");
    }
    let directory = format!("{store}/{digest}");
    let cached = format!("{directory}/hel");
    // A collector in another daemon can retire an entry between the probe and
    // the link. Retry that explicitly unaccepted installation with fresh bytes.
    for _ in 0..3 {
        if !crate::session_manager::installed_digest_matches(
            executor,
            &super::upgrade::installed_file_digest_command(
                locator,
                &cached,
                "verify shared Mjolnir worker",
            ),
            &digest,
        ) {
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
                    targets::locator_command(
                        locator,
                        vec!["mkdir".into(), "-p".into(), upload.clone()],
                    )
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
                        &[&temporary, &digest],
                        "verify uploaded Mjolnir worker",
                    ),
                )?;
                execute_checked(
                    executor,
                    script(
                        locator,
                        r#"set -eu
mkdir -p -- "$2"
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
                        &[&temporary, &directory, &digest],
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
            transfer?;
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
[ "${actual%% *}" = "$4" ] || { echo 'shared worker digest mismatch' >&2; exit 1; }
promote_worker_binary "$temporary" "$destination"
"#,
                &[&cached, &temporary, destination, &digest, mode],
                "link shared Mjolnir worker into session",
            )
        };
        let mut linked = executor.execute(&command("link"))?;
        if linked.status == 76 {
            crate::target_storage::ensure_room_for(
                locator,
                [temporary.as_str()],
                || super::upgrade::file_size(source),
                "copy the replacement Mjolnir worker",
            )?;
            linked = executor.execute(&command("copy"))?;
        }
        if linked.status == 75 {
            continue;
        }
        ensure!(
            linked.status == 0,
            "install shared Mjolnir worker: {}",
            String::from_utf8_lossy(&linked.stderr)
        );
        return Ok(());
    }
    bail!("shared Mjolnir worker repeatedly retired during installation")
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
