//! Immutable target binaries. Session installation only creates new directory
//! entries; it never writes executable bytes through an existing inode.
use super::*;
use mj_core::targets::SshTrafficClass;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

/// Warm the host store used by subsequent container launches.
pub fn prestage_host_container_worker(
    executor: &impl CommandExecutor,
    ssh: &targets::SshTarget,
    platform: targets::TargetPlatform,
) -> Result<()> {
    let source = super::binary_select::materialize_worker_source(worker_binary_for_arch(
        platform.architecture,
        WorkerBinaryRequirement::for_os(platform.os),
    )?)?;
    let digest = mj_core::worker_launch::worker_executable_digest(&source)?;
    let host = targets::TargetLocator::SshBare {
        ssh: ssh.clone(),
        worker_id: None,
        workspace: ".local/share/hel/workspaces/prestage".into(),
    };
    let store = mj_core::target_path::text(&store_root(&host, "prestage")?);
    ensure_published(executor, &host, "prestage", &source, &store, &digest)
}

/// Beside the session roots, in their target filesystem (inside containers).
pub fn store_root(locator: &targets::TargetLocator, session_id: &str) -> Result<PathBuf> {
    store_directory(Path::new(&targets::worker_root(locator, session_id)?))
        .context("worker root has no parent")
}

/// The worker store beside a worker root; the only place that interprets where
/// a store is.
fn store_directory(worker_root: &Path) -> Option<PathBuf> {
    Some(mj_core::target_path::join(
        worker_root.parent()?,
        Path::new("worker-binaries"),
    ))
}

/// Key of one target store: this daemon's data directory, the target's command
/// prefix and the store path.
pub fn store_key(locator: &targets::TargetLocator, root: &str) -> String {
    let command = targets::locator_command(SshTrafficClass::Control, locator, vec!["true".into()]);
    format!(
        "{:?}:{}:{:?}:{root}",
        DataLayout::current().root(),
        command.program,
        command.args
    )
}

/// One owner per `root` string in this daemon. Callers choose the string. An
/// upload takes `{store}/{digest}` so one build is transferred once, then the
/// store itself, which publication and collection share so a collector never
/// retires a directory an upload is publishing into. A session's staging
/// registry takes its own root. Cached installs of a published build take none
/// of these. Other
/// daemons targeting the same host are protected by the upload's unique name
/// and its atomic publication. Waiting observes the executor cancellation.
pub fn store_gate(locator: &targets::TargetLocator, root: &str) -> Arc<StoreGate> {
    type Gates = Mutex<HashMap<String, Weak<StoreGate>>>;
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
    let gate = Arc::new(StoreGate {
        label: format!("{} {root}", gate_target(locator)),
        holder: Mutex::new(None),
    });
    gates.insert(key, Arc::downgrade(&gate));
    gate
}

/// Where a gate's store lives, for the log line of a long wait.
fn gate_target(locator: &targets::TargetLocator) -> String {
    use targets::TargetLocator::*;
    match locator {
        SshPodman {
            ssh, container_id, ..
        }
        | SshDocker {
            ssh, container_id, ..
        }
        | SshAppleContainer {
            ssh, container_id, ..
        }
        | SshNitro {
            ssh, container_id, ..
        } => format!("container {container_id} on {}", ssh.destination),
        LocalPodman { container_id, .. }
        | LocalDocker { container_id, .. }
        | AppleContainer { container_id, .. } => format!("local container {container_id}"),
        SshBare { ssh, .. } | AwsEc2 { ssh, .. } => format!("host {}", ssh.destination),
        LocalBare { .. } => "this host".to_owned(),
    }
}

/// One owner for a store root in this daemon. The record of who holds it is
/// the lock itself: taking the gate sets it and releasing the guard on any
/// path clears it, so a waiter reads the owner from the same place that
/// decides ownership.
pub struct StoreGate {
    label: String,
    holder: Mutex<Option<GateHolder>>,
}

struct GateHolder {
    session_id: String,
    purpose: &'static str,
    since: Instant,
}

/// Ownership of a [`StoreGate`]; dropping it releases the gate.
pub struct GateGuard<'a>(&'a StoreGate);

impl StoreGate {
    fn holder(&self) -> MutexGuard<'_, Option<GateHolder>> {
        self.holder.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take the gate if it is free, recording `session_id` and `purpose`.
    pub fn try_lock(&self, session_id: &str, purpose: &'static str) -> Option<GateGuard<'_>> {
        let mut holder = self.holder();
        if holder.is_some() {
            return None;
        }
        *holder = Some(GateHolder {
            session_id: session_id.to_owned(),
            purpose,
            since: Instant::now(),
        });
        Some(GateGuard(self))
    }

    fn describe_holder(&self) -> String {
        self.holder().as_ref().map_or_else(
            || "released just now".to_owned(),
            |holder| {
                format!(
                    "{} ({}) held for {}s",
                    holder.session_id,
                    holder.purpose,
                    holder.since.elapsed().as_secs()
                )
            },
        )
    }
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        *self.0.holder() = None;
    }
}

/// Wait for the gate, logging the holder once the wait passes five seconds.
/// Waiting observes the executor's cancellation. A wait that has to queue is
/// reported as its own stage, so a lifecycle watching for stalls does not
/// count the queue as time with no progress.
pub fn lock_gate<'a>(
    executor: &impl CommandExecutor,
    gate: &'a StoreGate,
    session_id: &str,
    purpose: &'static str,
) -> Result<GateGuard<'a>> {
    // Queuing is not work performed by this operation. Preserve its command
    // budget while continuing to observe explicit supervisor cancellation.
    let untimed = targets::WithoutDeadline::new(executor);
    let executor = &untimed;
    let mut waiting = None::<(mj_core::lock_wait::LockWait, ProvisionStageGuard<'_, _>)>;
    loop {
        if let Some(guard) = gate.try_lock(session_id, purpose) {
            if let Some((wait, _queued)) = waiting {
                wait.acquired();
            }
            return Ok(guard);
        }
        if executor.cancellation_requested() {
            executor.execute(
                &CommandSpec::new("true", Vec::<String>::new())
                    .purpose(format!("wait to {purpose} in {}", gate.label)),
            )?;
            bail!(
                "shared target content wait cancelled: {} ({purpose})",
                gate.label
            );
        }
        let (wait, _queued) = waiting.get_or_insert_with(|| {
            (
                mj_core::lock_wait::LockWait::new("the shared target content store"),
                ProvisionStageGuard::new(executor, ProvisionStage::WaitingForWorkerStore),
            )
        });
        wait.blocked(&gate.label, &format!("{session_id} ({purpose})"), || {
            gate.describe_holder()
        });
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn script(
    class: targets::SshTrafficClass,
    locator: &targets::TargetLocator,
    body: &str,
    args: &[&str],
    purpose: &str,
) -> CommandSpec {
    targets::locator_command(class, locator, script_argv(body, args)).purpose(purpose)
}

fn shell_words(args: &[String]) -> String {
    args.iter()
        .map(|arg| targets::posix_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn inline_collection() -> String {
    format!(
        r#"collect_worker_store() (
store=$1; keep=${{2-}}
set -- "$store"/*/hel
if {{ [ -n "$keep" ] && [ "$#" -le 1 ]; }} || {{ [ -z "$keep" ] && [ ! -e "$1" ]; }}; then
    found=0
    for p in "$store"/.upload-* "$store"/.transport-* "$store"/.gc-*; do
        [ -e "$p" ] && found=1 && break
    done
    [ "$found" = 1 ] || exit 0
fi
if [ -e "$store/.collected" ] && [ -z "$(find "$store/.collected" -mmin +60 -print)" ]; then exit 0; fi
set -- "$store" "$keep"
{COLLECT}
touch -- "$store/.collected"
)
"#
    )
}

fn script_argv(body: &str, args: &[&str]) -> Vec<String> {
    let mut argv = vec![
        "sh".into(),
        "-c".into(),
        format!(
            "{}\n{}\n{body}",
            super::upgrade::PROMOTE_WORKER_BINARY,
            inline_collection()
        ),
        "mj-worker-store".into(),
    ];
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    argv
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
            targets::SshTrafficClass::Control,
            locator,
            COLLECT,
            &[root, keep.unwrap_or("")],
            "collect unused Mjolnir worker binaries",
        ),
    )?;
    Ok(())
}

fn collect_reporting(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    store: &str,
    keep: Option<&str>,
) {
    if let Err(error) = collect(executor, locator, store, keep) {
        tracing::warn!(%store, error = %format!("{error:#}"), "worker binary store collection failed");
    }
}

pub fn install_worker_binary(
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
const LINK_NO_ROOM: i32 = 78;

pub fn install_worker_binary_at(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker: &WorkerInstall,
    destination: &str,
    session_root: Option<&str>,
) -> Result<()> {
    let source = worker.host_binary();
    if let Some(source) = source {
        verify_worker_build(source)?;
    }
    install_binary_at(
        executor,
        locator,
        session_id,
        source,
        destination,
        session_root,
    )
}

/// Install a caller-verified executable through the same immutable stores and
/// transport used by the worker. The store verifies the bytes at each hop.
pub fn install_session_executable(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    source: &Path,
    name: &str,
) -> Result<PathBuf> {
    let root = targets::worker_root(locator, session_id)?;
    let destination = mj_core::target_path::join(Path::new(&root), Path::new(name));
    install_binary_at(
        executor,
        locator,
        session_id,
        Some(source),
        &mj_core::target_path::text(&destination),
        Some(&root),
    )?;
    Ok(destination)
}

fn install_binary_at(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    source: Option<&Path>,
    destination: &str,
    session_root: Option<&str>,
) -> Result<()> {
    let store = mj_core::target_path::text(&store_root(locator, session_id)?);
    // Register before any pin can be created. Registration and sweeping share
    // one session owner, so no sweep can use a snapshot predating a live pin.
    let name = format!("hel.store-{}", mj_core::relay::new_command_id("link")?);
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
    let Some(source) = source else {
        let collect_gate = store_gate(locator, &store);
        let _collect_guard = collect_gate.try_lock(session_id, "collect unused worker binaries");
        let allow_collect = if _collect_guard.is_some() { "1" } else { "0" };
        execute_checked(
            executor,
            script(
                SshTrafficClass::Control,
                locator,
                "set -eu; mkdir -p -- \"${2%/*}\"; ln -sfn -- \"$1\" \"$2.next\"; mv -f -- \"$2.next\" \"$2\"; case \"$2\" in */hel) rm -f -- \"${2%/*}\"/hel-control*;; esac; if [ \"$4\" = 1 ]; then collect_worker_store \"$3\" || echo 'worker binary store collection failed' >&2; fi",
                &[IMAGE_WORKER, destination, &store, allow_collect],
                "link image worker and collect store",
            ),
        )?;
        // Switching to an image link can release the last session reference
        // to a host-copied build. Image installs must collect that store too.

        return Ok(());
    };
    let digest = mj_core::worker_launch::worker_executable_digest(source)?;
    if matches!(
        locator,
        targets::TargetLocator::SshPodman { .. }
            | targets::TargetLocator::SshDocker { .. }
            | targets::TargetLocator::SshAppleContainer { .. }
    ) {
        return install_ssh_container(
            executor,
            locator,
            session_id,
            source,
            destination,
            &temporary,
            &store,
            &digest,
        );
    }
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
            ensure_published(executor, locator, session_id, source, &store, &digest)?;
        }
        let collect_gate = store_gate(locator, &store);
        let _collect_guard = collect_gate.try_lock(session_id, "collect unused worker binaries");
        let allow_collect = if _collect_guard.is_some() { "1" } else { "0" };
        let command = |mode| {
            script(
                if mode == "link" {
                    targets::SshTrafficClass::Control
                } else {
                    targets::SshTrafficClass::Bulk
                },
                locator,
                r#"set -eu
allow_collect=$1; shift
source=$1; temporary=$2; destination=$3
trap 'rm -f -- "$temporary"' EXIT
if [ "$5" = link ]; then
    if ! ln -- "$source" "$temporary" 2>/dev/null; then
        [ -f "$source" ] || exit 75
        # A concurrent publisher can create $source after the first ln fails.
        # Retry once now that $source exists so we only report no hard links
        # when ln fails against a present source.
        if ! ln -- "$source" "$temporary" 2>/dev/null; then
            [ -f "$source" ] || exit 75
            exit 76
        fi
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
# This build serves every control subcommand, so a staged control worker has
# nothing left to do; its link would only pin a store entry.
case "$destination" in */hel) rm -f -- "${destination%/*}"/hel-control* ;; esac
if [ "$allow_collect" = 1 ]; then collect_worker_store "${source%/*/*}" "$4" || echo 'worker binary store collection failed' >&2; fi
"#,
                &[
                    allow_collect,
                    &cached,
                    &temporary,
                    destination,
                    &digest,
                    mode,
                ],
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

#[allow(clippy::too_many_arguments)]
fn install_ssh_container(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    source: &Path,
    destination: &str,
    temporary: &str,
    store: &str,
    digest: &str,
) -> Result<()> {
    let endpoint = locator.container_endpoint().context("container endpoint")?;
    let host = targets::TargetLocator::SshBare {
        ssh: endpoint.ssh.context("SSH container host")?.clone(),
        worker_id: None,
        workspace: format!(".local/share/hel/workspaces/{session_id}"),
    };
    let host_store = mj_core::target_path::text(&store_root(&host, session_id)?);
    let transport = format!(
        "{host_store}/.transport-{}",
        mj_core::relay::new_command_id("container-upload")?
    );
    let upload = format!(
        "{store}/.upload-{}",
        mj_core::relay::new_command_id("container-upload")?
    );
    let gate = store_gate(locator, store);
    let _guard = lock_gate(executor, &gate, session_id, "install container worker")?;
    let room = crate::target_storage::ensure_room_for(
        locator,
        [upload.as_str()],
        || super::upgrade::file_size(source),
        "stage container worker",
    );
    let can_upload = if room.is_ok() { "1" } else { "0" };
    let inner = r#"set -eu
store=$1; digest=$2; temporary=$3; destination=$4; upload=$5; mode=$6
trap 'rm -f -- "$temporary"; rm -rf -- "$upload"' EXIT
cached="$store/$digest/hel"
if [ "$mode" = hit ]; then
    [ -f "$cached" ] || exit 75
    if ! ln -- "$cached" "$temporary" 2>/dev/null; then
        if ! cp -- "$cached" "$temporary"; then
            [ -f "$cached" ] || exit 75
            exit 1
        fi
    fi
else
    temporary_upload="$upload/hel"
    ln -- "$temporary_upload" "$temporary" 2>/dev/null || cp -- "$temporary_upload" "$temporary"
fi
case $(uname -s) in Darwin) actual=$(shasum -a 256 -- "$temporary");; *) actual=$(sha256sum -- "$temporary");; esac
[ "${actual%% *}" = "$digest" ] || { echo 'container worker digest mismatch' >&2; exit 77; }
if [ "$mode" != hit ]; then
    mkdir -p -- "$store/$digest"
    touch -- "$temporary"
    if ! ln -- "$temporary" "$cached" 2>/dev/null; then
        if [ ! -e "$cached" ]; then mv -n -- "$upload/hel" "$cached"; fi
        if [ -e "$upload/hel" ]; then
        case $(uname -s) in Darwin) actual=$(shasum -a 256 -- "$cached");; *) actual=$(sha256sum -- "$cached");; esac
        [ "${actual%% *}" = "$digest" ] || mv -f -- "$upload/hel" "$cached"
        fi
    fi
fi
chmod -- 700 "$temporary"
promote_worker_binary "$temporary" "$destination"
case "$destination" in */hel) rm -f -- "${destination%/*}"/hel-control* ;; esac
collect_worker_store "$store" "$digest" || echo 'worker binary store collection failed' >&2
"#;
    let inner = format!(
        "{}\n{}\n{inner}",
        super::upgrade::PROMOTE_WORKER_BINARY,
        inline_collection()
    );
    let exec = shell_words(&targets::with_engine_argv(vec![
        endpoint.engine.to_owned(),
        "exec".into(),
        endpoint.container_id.to_owned(),
        "sh".into(),
        "-c".into(),
        inner,
        "mj-container-store".into(),
        store.into(),
        digest.into(),
        temporary.into(),
        destination.into(),
        upload.clone(),
    ]));
    let engine_command = |args: Vec<String>| {
        shell_words(&targets::with_engine_argv(
            std::iter::once(endpoint.engine.to_owned())
                .chain(args)
                .collect(),
        ))
    };
    let mkdir = engine_command(vec![
        "exec".into(),
        endpoint.container_id.into(),
        "mkdir".into(),
        "-p".into(),
        "--".into(),
        upload.clone(),
    ]);
    let cleanup = engine_command(vec![
        "exec".into(),
        endpoint.container_id.into(),
        "rm".into(),
        "-rf".into(),
        "--".into(),
        upload.clone(),
    ]);
    let copy = format!(
        "{} \"$transport_path/hel\" {}",
        engine_command(vec!["cp".into()]),
        targets::posix_quote(&format!("{}:{upload}/hel", endpoint.container_id))
    );
    let destination_dir = Path::new(destination)
        .parent()
        .context("worker destination directory")?;
    let ownership = engine_command(super::install::container_upload_ownership_args(
        endpoint.engine,
        endpoint.container_id,
        &mj_core::target_path::text(destination_dir),
        &[&format!("{upload}/hel")],
    ));
    let body = format!(
        r#"set -eu
rc=0
{exec} hit || rc=$?
[ "$rc" -ne 0 ] || exit 0
case $rc in 75|77) ;; *) exit "$rc";; esac
[ "$4" = 1 ] || exit 78
[ -f "$1/$2/hel" ] || exit 75
transport=$3
need_upload_cleanup=0
cleanup_upload() {{
    rc=$?
    trap - EXIT
    rm -rf -- "$transport" || echo 'host worker transport cleanup failed' >&2
    if [ "$need_upload_cleanup" -eq 1 ]; then {cleanup} || echo 'container upload cleanup failed' >&2; fi
    exit "$rc"
}}
trap cleanup_upload EXIT
trap 'exit 1' HUP INT TERM
case "$transport" in /*) transport_path=$transport;; *) transport_path=$HOME/$transport;; esac
mkdir -p -- "$transport"
if ! ln -- "$1/$2/hel" "$transport/hel" 2>/dev/null; then
    if ! cp -- "$1/$2/hel" "$transport/hel"; then
        [ -f "$1/$2/hel" ] || exit 75
        exit 1
    fi
fi
need_upload_cleanup=1
{mkdir}
{copy}
{ownership}
{exec} publish
need_upload_cleanup=0
if [ "$5" = 1 ]; then collect_worker_store "$1" "$2" || echo 'worker binary store collection failed' >&2; fi
"#
    );
    for _ in 0..3 {
        let collect_gate = store_gate(&host, &host_store);
        let collect_guard = collect_gate.try_lock(session_id, "collect unused worker binaries");
        let allow_collect = if collect_guard.is_some() { "1" } else { "0" };
        let command = script(
            SshTrafficClass::Control,
            &host,
            &body,
            &[&host_store, digest, &transport, can_upload, allow_collect],
            "install shared container worker",
        );
        let output = executor.execute(&command)?;
        drop(collect_guard);
        if output.status == LINK_NO_ROOM {
            room?;
            bail!("container worker upload refused despite available space");
        }
        if matches!(output.status, LINK_MISSING | LINK_DIGEST_MISMATCH) {
            ensure_published(executor, &host, session_id, source, &host_store, digest)?;
        } else {
            targets::checked_command_output(&command, output)?;
            return Ok(());
        }
    }
    bail!("shared container worker repeatedly retired during installation")
}

/// Make `{store}/{digest}/hel` exist with the right bytes. The lock is per
/// build: the first holder uploads, verifies and publishes, and every later
/// holder finds it published after acquiring and returns without a transfer.
/// Builds that are already published never reach this function. Publication
/// then owns the whole store, shared with collection, so a collector cannot
/// retire the digest directory between its creation and the publishing link.
/// Uploads are rare, so different builds waiting for each other here is cheap.
fn ensure_published(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    source: &Path,
    store: &str,
    digest: &str,
) -> Result<()> {
    let directory = format!("{store}/{digest}");
    let gate = store_gate(locator, &directory);
    let _guard = lock_gate(executor, &gate, session_id, "publish a worker build")?;
    if mj_targets::command_checks::installed_digest_matches(
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
    let store_owner = store_gate(locator, store);
    let _store = lock_gate(
        executor,
        &store_owner,
        session_id,
        "upload and collect worker builds",
    )?;
    // Reclaim old builds before the capacity check below can refuse the
    // upload, even when this interval's collection already ran.
    collect_reporting(executor, locator, store, Some(digest));
    let upload = format!(
        "{store}/.upload-{}",
        mj_core::relay::new_command_id("worker-store")?
    );
    let temporary = format!("{upload}/hel");
    crate::target_storage::ensure_room_for(
        locator,
        [upload.as_str()],
        || super::upgrade::file_size(source),
        "stage the replacement Mjolnir worker",
    )?;
    let mut batch_completed = false;
    let transfer = (|| -> Result<()> {
        execute_checked(
            executor,
            targets::locator_command(
                SshTrafficClass::Control,
                locator,
                vec!["mkdir".into(), "-p".into(), upload.clone()],
            )
            .purpose("create private worker store upload"),
        )?;
        transfer_binary(executor, locator, session_id, source, &temporary)?;
        let steps = [
            targets::CommandStep::new(
                "verify uploaded Mjolnir worker",
                script_argv(
                    r#"set -eu
case $(uname -s) in Darwin) actual=$(shasum -a 256 -- "$1");; *) actual=$(sha256sum -- "$1");; esac
[ "${actual%% *}" = "$2" ] || { echo 'uploaded Mjolnir worker digest mismatch' >&2; exit 1; }
"#,
                    &[&temporary, digest],
                ),
            ),
            targets::CommandStep::new(
                "publish shared Mjolnir worker",
                script_argv(
                    r#"set -eu
mkdir -p -- "$2"
# Copies such as docker cp keep the source build's age; collection must see a
# fresh entry.
touch -- "$1"
# Atomic creation converges independent uploaders on the winning inode.
if [ ! -e "$2/hel" ]; then
    if ln -- "$1" "$2/hel" 2>/dev/null; then exit 0; fi
    if [ ! -e "$2/hel" ]; then
        mv -n -- "$1" "$2/hel"
        [ -e "$1" ] || exit 0
    fi
fi
case $(uname -s) in Darwin) actual=$(shasum -a 256 -- "$2/hel");; *) actual=$(sha256sum -- "$2/hel");; esac
if [ "${actual%% *}" != "$3" ]; then
    # A corrupt entry is replaced, never written through its existing inode.
    mv -f -- "$1" "$2/hel"
fi
"#,
                    &[&temporary, &directory, digest],
                ),
            ),
            targets::CommandStep::new(
                "discard private worker store upload",
                ["rm", "-rf", "--", &upload],
            )
            .best_effort_cleanup(),
        ];
        let command = targets::locator_command(
            SshTrafficClass::Control,
            locator,
            targets::step_script(steps),
        )
        .purpose("publish shared Mjolnir worker");
        // A failed or disconnected batch may not have reached its EXIT trap.
        // Retry its idempotent discard through the executor's cleanup path.
        let output = executor.execute(&command)?;
        batch_completed = output.status == 0;
        for warning in targets::step_script_warnings(&output) {
            tracing::warn!("{warning}");
        }
        targets::checked_command_output(&command, output)?;
        Ok(())
    })();
    if !batch_completed {
        let cleanup = executor.execute_cleanup(
            &targets::locator_command(
                SshTrafficClass::Control,
                locator,
                vec!["rm".into(), "-rf".into(), "--".into(), upload],
            )
            .purpose("discard private worker store upload"),
        );
        match cleanup {
            Ok(output) if output.status == 0 => {}
            other => tracing::warn!(result = ?other, "worker store upload cleanup failed"),
        }
    }
    transfer
}

/// SSH containers reuse the host store as their transport cache, then publish
/// into their own filesystem. The private host link pins bytes during engine cp.
fn transfer_binary(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    source: &Path,
    destination: &str,
) -> Result<()> {
    if let targets::TargetLocator::AwsEc2 { ssh, .. }
    | targets::TargetLocator::SshBare { ssh, .. } = locator
    {
        // This path already belongs to the private store upload. Stream the
        // worker without the deadline reserved for the surrounding commands.
        let command = targets::ssh_command(
            SshTrafficClass::Bulk,
            ssh,
            [
                "sh",
                "-c",
                "umask 077; cat > \"$1\" && chmod -- 700 \"$1\"",
                "mj-upload",
                destination,
            ],
        )
        .gzip_ssh_stdin()
        .purpose("stage replacement Mjolnir worker");
        let mut input = std::fs::File::open(source)?;
        let output =
            targets::WithoutDeadline::new(executor).execute_with_stdin(&command, &mut input)?;
        targets::checked_command_output(&command, output)?;
        return Ok(());
    }
    let remote = match locator {
        targets::TargetLocator::SshPodman { ssh, .. }
        | targets::TargetLocator::SshDocker { ssh, .. }
        | targets::TargetLocator::SshAppleContainer { ssh, .. }
        | targets::TargetLocator::SshNitro { ssh, .. } => Some(ssh),
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
                mj_core::relay::new_command_id("container-upload")?
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
                    SshTrafficClass::Control,
                    &host,
                    vec!["mkdir".into(), "-p".into(), transport.clone()],
                )
                .purpose("create private host worker transport"),
            )?;
            install_binary_at(executor, &host, session_id, Some(source), &pin, None)?;
            super::upgrade::binary_upload_plan(
                locator,
                session_id,
                Some(source),
                destination,
                Some(&pin),
            )?
            .execute(executor)?;
            Ok(())
        })();
        let cleanup = executor.execute_cleanup(
            &targets::locator_command(
                SshTrafficClass::Control,
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
        super::upgrade::binary_upload_plan(locator, session_id, Some(source), destination, None)?
            .execute(executor)?;
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests;
