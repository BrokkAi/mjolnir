//! Host-side Git caches for container-backed sessions.

use mj_core::hex::lower_hex;
use mj_core::refusal::Refusal;
use mj_core::remote_git::{display_url, is_github_https_url, is_host_only_git_source};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::targets::{
    self, AdditionalMount, CommandExecutor, CommandOutput, CommandSpec, HistoryCutoff,
    ProjectBundleSpec, ProvisionStage, ProvisionStageGuard,
};

const CACHE_CONTAINER_ROOT: &str = "/run/hel/git-cache";
const CACHE_RELATIVE_ROOT: &str = ".cache/mjolnir/git";
const CACHE_MAX_KIB: u64 = 20 * 1024 * 1024;

// A successful redirection creates the lock file exclusively. Some mkdir
// implementations report success after the mkdir syscall loses to a peer.
const CACHE_LOCK_FUNCTIONS: &str = r#"
cache_lock_signal() {
    if [ "$changing_cache_lock" = 0 ]; then
        exit "$1"
    fi
    pending_cache_lock_signal=$1
}
track_cache_locks() {
    held_cache_lock=
    gc_lock=
    changing_cache_lock=0
    pending_cache_lock_signal=
    trap 'cache_lock_signal 129' HUP
    trap 'cache_lock_signal 130' INT
    trap 'cache_lock_signal 143' TERM
}
finish_cache_lock_change() {
    changing_cache_lock=0
    [ -z "$pending_cache_lock_signal" ] || exit "$pending_cache_lock_signal"
}
try_cache_lock() {
    # Defer exit until the recorded owner agrees with the filesystem.
    changing_cache_lock=1
    lock_status=1
    set -C
    if command : 2>/dev/null > "$1"; then
        if [ "${2:-}" = gc ]; then
            gc_lock=$1
        else
            held_cache_lock=$1
        fi
        lock_status=0
    fi
    set +C
    finish_cache_lock_change
    return "$lock_status"
}
release_cache_lock() {
    changing_cache_lock=1
    # The child must finish unlinking and reporting success even when the
    # same signal hits both the shell and its children.
    (trap '' HUP INT TERM; exec rm -- "$held_cache_lock")
    held_cache_lock=
    finish_cache_lock_change
}
cleanup_cache_locks() {
    trap '' HUP INT TERM
    set --
    [ -z "$held_cache_lock" ] || set -- "$held_cache_lock"
    [ -z "$gc_lock" ] || set -- "$@" "$gc_lock"
    if [ "$#" -gt 0 ]; then
        rm -f -- "$@"
    fi
}
"#;

use super::cache_host::CacheHost;

impl CacheHost {
    /// `CacheHost::shell_command` with this cache's `$0` label and lifecycle
    /// stage, so every clone-cache command reports as cloning work.
    fn git_shell_command(
        &self,
        script: &str,
        arguments: impl IntoIterator<Item = String>,
        purpose: impl Into<String>,
    ) -> CommandSpec {
        self.shell_command(
            &format!("{CACHE_LOCK_FUNCTIONS}\n{script}"),
            "hel-git-cache",
            arguments,
            purpose,
        )
        .stage(ProvisionStage::Cloning)
    }

    /// The sessions `engine` still has containers for on this host. The
    /// engine is a property of the runtime, not of the machine, so the caller
    /// names it.
    fn managed_sessions(
        &self,
        engine: &str,
        executor: &impl CommandExecutor,
    ) -> Result<Vec<String>> {
        let remote = match engine {
            "container" => vec![
                "container".to_owned(),
                "list".to_owned(),
                "--all".to_owned(),
                "--format".to_owned(),
                "json".to_owned(),
            ],
            engine => vec![
                engine.to_owned(),
                "ps".to_owned(),
                "--all".to_owned(),
                "--filter".to_owned(),
                format!("label={}=true", targets::MANAGED_LABEL),
                "--format".to_owned(),
                "json".to_owned(),
            ],
        };
        let command = self
            .command(remote, "find live container Git cache snapshots")
            .stage(ProvisionStage::Cloning);
        let output = checked(executor.execute(&command)?, &command)?;
        targets::managed_sessions_from_container_json(&output.stdout)
            .map(|sessions| sessions.into_iter().map(|(session, _)| session).collect())
    }
}

#[derive(Debug, Clone)]
pub struct PreparedCloneCache {
    host: CacheHost,
    session_root: PathBuf,
}

impl PreparedCloneCache {
    pub fn cleanup(&self, executor: &impl CommandExecutor) -> Result<()> {
        cleanup_session_root(&self.host, &self.session_root, executor)
    }
}

/// Populate session-scoped Git snapshots and attach them to a container bundle.
/// A history cutoff mounts only the selected commits and their ancestors.
/// Host-only sources require a snapshot. For other sources, preparation
/// failures are visible optimization misses and checkout can reach the remote.
pub fn prepare(
    target: &targets::TargetTemplate,
    session_id: &str,
    bundle: &mut ProjectBundleSpec,
    mounts: &mut Vec<AdditionalMount>,
    github_token: Option<&str>,
    exact_checkout: Option<(&str, &str)>,
    executor: &(impl CommandExecutor + Sync),
) -> Result<Option<PreparedCloneCache>> {
    let Some((host, engine)) = CacheHost::for_target(target).zip(target.container_engine()) else {
        return Ok(None);
    };
    bundle.validate()?;
    let exact_for = |destination: &str, source: &str, cutoff: Option<&HistoryCutoff>| {
        exact_checkout
            .filter(|(selected, _)| {
                *selected == destination && cutoff.is_none() && is_host_only_git_source(source)
            })
            .map(|(_, commit)| commit)
    };
    let repositories = bundle
        .repositories
        .iter()
        .map(|repository| {
            let source = repository
                .url
                .as_deref()
                .expect("validated network repository");
            let cutoff = repository.history_cutoff.as_ref();
            let exact = exact_for(&repository.destination, source, cutoff);
            (
                snapshot_name(source, cutoff, exact),
                (source, cutoff, exact, is_host_only_git_source(source)),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let required_sources = repositories
        .values()
        .filter(|(_, _, _, required)| *required)
        .map(|(source, _, _, _)| display_url(source))
        .collect::<Vec<_>>()
        .join(", ");
    let cache_destination = Path::new(CACHE_CONTAINER_ROOT);
    if mounts.iter().any(|mount| {
        mount.destination.starts_with(cache_destination)
            || cache_destination.starts_with(&mount.destination)
    }) {
        if !required_sources.is_empty() {
            return Err(Refusal::precondition(format!(
                "Cannot mount the host-only Git sources {required_sources}. Move the attached directory that overlaps {CACHE_CONTAINER_ROOT}."
            )).into());
        }
        executor.notify_notice(
            "Clone cache disabled because an attached directory overlaps /run/hel/git-cache.",
        );
        return Ok(None);
    }

    let _stage = ProvisionStageGuard::new(executor, ProvisionStage::Cloning);
    let home = match cache_home(&host, executor) {
        Ok(home) => home,
        Err(error) if !required_sources.is_empty() => {
            return Err(error.context(Refusal::precondition(format!(
                "Cannot prepare host-only Git sources {required_sources} on the container host. Make sure Git and sh are available and the host has an absolute home directory."
            ))));
        }
        Err(error) => {
            executor.notify_notice(&format!(
                "Clone cache unavailable on the container host: {error:#}; using direct clones."
            ));
            return Ok(None);
        }
    };
    let cache_root = home.join(CACHE_RELATIVE_ROOT);
    let session_root = cache_root.join("sessions").join(session_id);

    let prepared = std::thread::scope(|scope| {
        let handles = repositories
            .iter()
            .map(|(name, (source, cutoff, exact, required))| {
                let snapshot = session_root.join(name);
                let host = &host;
                let cache_root = &cache_root;
                let handle = scope.spawn(move || {
                    prepare_repository(
                        host,
                        cache_root,
                        &repository_cache_key(source),
                        source,
                        &snapshot,
                        *cutoff,
                        *exact,
                        github_token,
                        executor,
                    )
                    .map(|()| format!("{CACHE_CONTAINER_ROOT}/{name}"))
                });
                (name, source, required, handle)
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|(name, source, required, handle)| {
                let result = handle.join().unwrap_or_else(|panic| {
                    Err(anyhow::anyhow!(
                        "clone-cache preparation thread panicked: {}",
                        targets::command_thread_panic_message(panic.as_ref())
                    ))
                });
                (name.clone(), source, *required, result)
            })
            .collect::<Vec<_>>()
    });

    let mut references = BTreeMap::new();
    let mut failures = Vec::new();
    for (name, source, required, result) in prepared {
        match result {
            Ok(reference) => {
                references.insert(name, reference);
            }
            Err(error) if required => failures.push(error.context(Refusal::precondition(format!(
                "Cannot prepare {} on the container host. Verify that the host can fetch the source with Git and write its cache. For a pinned checkout, verify that the requested commits are available from the source.",
                display_url(source)
            )))),
            Err(error) => executor.notify_notice(&format!(
                "Clone cache preparation failed: {error:#}; using a direct checkout for that repository."
            )),
        }
    }
    if !failures.is_empty() || references.is_empty() {
        if let Err(cleanup) = cleanup_session_root(&host, &session_root, executor) {
            executor.notify_notice(&format!(
                "Could not remove the failed Git checkout snapshot: {cleanup:#}"
            ));
        }
        let mut failures = failures.into_iter();
        if let Some(error) = failures.next() {
            for additional in failures {
                executor.notify_notice(&format!("{additional:#}"));
            }
            return Err(error);
        }
        return Ok(None);
    }
    for repository in &mut bundle.repositories {
        let source = repository
            .url
            .as_deref()
            .expect("validated network repository");
        let cutoff = repository.history_cutoff.as_ref();
        let name = snapshot_name(
            source,
            cutoff,
            exact_for(&repository.destination, source, cutoff),
        );
        repository.reference = references.get(&name).cloned();
    }
    mounts.push(AdditionalMount {
        source: session_root.clone(),
        destination: PathBuf::from(CACHE_CONTAINER_ROOT),
        access: crate::targets::MountAccess::Ro,
    });

    let mut live_sessions = match host.managed_sessions(engine, executor) {
        Ok(sessions) => sessions,
        Err(error) => {
            executor.notify_notice(&format!(
                "Clone-cache orphan cleanup was skipped: {error:#}"
            ));
            vec![session_id.to_owned()]
        }
    };
    if !live_sessions.iter().any(|live| live == session_id) {
        live_sessions.push(session_id.to_owned());
    }
    if let Err(error) = collect_garbage(&host, &cache_root, &live_sessions, executor) {
        executor.notify_notice(&format!("Clone-cache cleanup was skipped: {error:#}"));
    }
    Ok(Some(PreparedCloneCache { host, session_root }))
}

/// Snapshots share the mirror, but each source and selection owns its files.
fn snapshot_name(source: &str, cutoff: Option<&HistoryCutoff>, exact: Option<&str>) -> String {
    let mut digest = Sha256::new_with_prefix(source);
    if let Some(cutoff) = cutoff {
        for commit in cutoff.commits() {
            digest.update([0]);
            digest.update(commit);
        }
        format!("{}.bundle", lower_hex(digest.finalize()))
    } else {
        if let Some(commit) = exact {
            digest.update([0]);
            digest.update(commit);
        }
        format!("{}.git", lower_hex(digest.finalize()))
    }
}

fn cache_home(host: &CacheHost, executor: &impl CommandExecutor) -> Result<PathBuf> {
    let command = host.git_shell_command(
        "command -v git >/dev/null 2>&1 || exit 127; printf '%s' \"$HOME\"",
        [],
        "locate the container host Git cache",
    );
    let output = checked(executor.execute(&command)?, &command)?;
    let home = String::from_utf8(output.stdout).context("decode container host home directory")?;
    let home = PathBuf::from(home);
    ensure!(
        mj_core::target_path::is_absolute_on_host_or_target(&home),
        "container host HOME is not absolute"
    );
    Ok(home)
}

fn repository_cache_key(source: &str) -> String {
    let identity = match crate::import::github_repository_from_origin(source) {
        Some(repository) => format!(
            "github.com/{}/{}",
            repository.owner.to_ascii_lowercase(),
            repository.repository.to_ascii_lowercase()
        ),
        // Resolution has already expanded shorthands and Git's configured URL
        // rewrites. Preserve user, port, and the entire transport path here:
        // two accounts or paths on one server can serve different objects.
        // RepositoryIdentity::Network drops userinfo and the .git suffix.
        None => format!("git:{source}"),
    };
    lower_hex(Sha256::digest(identity.as_bytes()))
}

const PREPARE_REPOSITORY_SCRIPT: &str = r#"
set -eu
cache_root=$1
key=$2
source=$3
snapshot=$4
has_token=$5
# How Git reaches an SSH remote, as the controller decided it. Empty leaves
# this host's GIT_SSH_COMMAND as it is: on the controller that is the
# operator's own command, inherited exactly.
ssh_command=$6
source_id=$7
host_only=$8
exact_ref=$9
# Any further arguments are a history cutoff: its head, then its base.
shift 9
umask 077
mkdir -p "$cache_root/mirrors" "$cache_root/locks" "$(dirname "$snapshot")"
# The session root's own time is its preparation stamp: until the container
# exists, it is what keeps the cache GC from treating the root as an orphan.
touch -- "$(dirname "$snapshot")"
entry="$cache_root/mirrors/$key"
mirror="$entry/repo.git"
# An overlapping refresh of the same requested URL satisfies this request too.
# GitHub URLs with different transports still share one mirror. The locked read
# is authoritative; this optimistic read may race eviction or publication.
observed_generation=$(cat "$entry/generation" 2>/dev/null || :)
lock="$cache_root/locks/$key"
preparation=$(mktemp -d "$cache_root/mirrors/$key.partial-$$-XXXXXX")
partial="$preparation/mirror"
snapshot_partial="$snapshot.partial-$$"
# Full history must never enter a session's mounted subtree, even temporarily.
pinned="$preparation/pinned.git"
cutoff_repository="$snapshot.cutoff-$$"
cleanup() {
    cleanup_cache_locks
    rm -rf -- "$preparation" "$snapshot_partial" "$cutoff_repository"
}
track_cache_locks
trap cleanup EXIT
attempt=0
while ! try_cache_lock "$lock"; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 1500 ]; then
        echo "timed out waiting for clone-cache lock $key" >&2
        exit 75
    fi
    sleep 0.2
done
if [ "$has_token" = 1 ]; then
    IFS= read -r GH_TOKEN || exit 1
    export GH_TOKEN
fi
export GIT_TERMINAL_PROMPT=0
if [ -n "$ssh_command" ]; then
    export GIT_SSH_COMMAND="$ssh_command"
fi
git_remote() {
    if [ "$has_token" = 1 ]; then
        git -c credential.https://github.com.helper= -c 'credential.https://github.com.helper=!f() { if [ "$1" = get ]; then printf "%s\n" username=x-access-token "password=$GH_TOKEN"; fi; }; f' "$@"
    else
        git "$@"
    fi
}
generation=$(cat "$entry/generation" 2>/dev/null || :)
next_generation="$source_id:${preparation##*/}"
if [ ! -d "$mirror" ]; then
    rm -rf -- "$entry"
    mkdir "$partial"
    git_remote clone --mirror -- "$source" "$partial/repo.git"
    git -C "$partial/repo.git" config gc.auto 0
    printf '%s\n' "$next_generation" > "$partial/generation"
    mv -- "$partial" "$entry"
elif [ "${generation%%:*}" != "$source_id" ] || [ "$generation" = "$observed_generation" ]; then
    git_remote -C "$mirror" fetch --prune -- "$source" '+refs/*:refs/*'
    # Only a host-only checkout reads the mirror's default branch.
    if [ "$host_only" = 1 ]; then
        advertised_head=$(git_remote -C "$mirror" ls-remote --symref -- "$source" HEAD)
        default_ref=$(printf '%s\n' "$advertised_head" | awk '$1 == "ref:" && $3 == "HEAD" { print $2 }')
        if [ -n "$default_ref" ]; then
            git -C "$mirror" symbolic-ref HEAD "$default_ref"
        elif [ -n "$advertised_head" ]; then
            detached_head=$(printf '%s\n' "$advertised_head" | awk '$2 == "HEAD" { print $1 }')
            if ! git -C "$mirror" cat-file -e "$detached_head^{commit}" 2>/dev/null; then
                # A detached HEAD may be outside every mirrored ref.
                git_remote -C "$mirror" fetch --no-tags -- "$source" HEAD
            fi
            git -C "$mirror" update-ref --no-deref HEAD "$detached_head"
        fi
    fi
    printf '%s\n' "$next_generation" > "$entry/generation"
fi
: > "$entry/last-used"
# Own every object and ref before releasing the mirror to refresh or eviction.
# Git hardlinks immutable objects; this private repository needs no shared lock
# while packing a cutoff bundle or making the mounted snapshot readable.
git clone --mirror --local -- "$mirror" "$pinned"
release_cache_lock
rm -rf -- "$snapshot" "$snapshot_partial"
if [ "$#" -eq 0 ]; then
    if [ -n "$exact_ref" ]; then
        commit=${exact_ref##*/}
        if ! git -C "$pinned" cat-file -e "$commit^{commit}" 2>/dev/null; then
            git_remote -C "$pinned" fetch --quiet --no-tags -- "$source" "$commit"
        fi
        git -C "$pinned" update-ref "$exact_ref" "$commit"
    fi
    mv -- "$pinned" "$snapshot_partial"
else
    # Copy only the cutoff commits and their history out of the private pin,
    # which holds every later commit too. A fetch sends only what the wanted
    # commits reach.
    git init --quiet --bare -- "$cutoff_repository"
    # The names HistoryCutoff::BUNDLE_HEAD_REF and BASE_REF give them, which
    # the checkout fetches by.
    refspecs=
    refs=
    name=head
    for commit do
        case $commit in
            ''|*[!0-9a-f]*) echo "invalid history cutoff commit" >&2; exit 2 ;;
        esac
        if ! git -C "$pinned" cat-file -e "$commit^{commit}" 2>/dev/null; then
            git_remote -C "$pinned" fetch --quiet --no-tags -- "$source" "$commit"
        fi
        refspecs="$refspecs $commit:refs/mj/$name"
        refs="$refs refs/mj/$name"
        name=base
    done
    # Object IDs and these ref names contain no spaces, so splitting is safe.
    # shellcheck disable=SC2086
    git -C "$cutoff_repository" fetch --quiet --no-tags \
        --upload-pack='git -c uploadpack.allowAnySHA1InWant=true upload-pack' \
        -- "$pinned" $refspecs
    # shellcheck disable=SC2086
    git -C "$cutoff_repository" bundle create "$snapshot_partial" $refs
    rm -rf -- "$cutoff_repository" "$pinned"
fi
mv -- "$snapshot_partial" "$snapshot"
# A bind mount can expose the host owner as container root, while the worker
# runs as an unprivileged user. The private cache parents still protect these
# files on the host; the mounted session subtree must be readable.
chmod -R a+rX "$snapshot"
chmod a+rx "$(dirname "$snapshot")"
"#;

#[allow(clippy::too_many_arguments)]
fn prepare_repository(
    host: &CacheHost,
    cache_root: &Path,
    key: &str,
    source: &str,
    snapshot: &Path,
    cutoff: Option<&HistoryCutoff>,
    exact: Option<&str>,
    github_token: Option<&str>,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let exact_ref = if let Some(commit) = exact {
        ensure!(
            mj_core::remote_git::is_object_id(commit),
            "invalid exact checkout commit"
        );
        format!("{}/{commit}", mj_core::targets::HOST_SNAPSHOT_EXACT_REFS)
    } else {
        String::new()
    };
    let github_token = github_token.filter(|_| is_github_https_url(source));
    let token = github_token.is_some();
    let mut arguments = vec![
        cache_root.to_string_lossy().into_owned(),
        key.to_owned(),
        source.to_owned(),
        snapshot.to_string_lossy().into_owned(),
        u8::from(token).to_string(),
        match host {
            CacheHost::Local => mj_core::local_git::local_git_ssh_command()
                .unwrap_or_default()
                .to_owned(),
            _ => mj_core::local_git::remote_git_ssh_command()?,
        },
        lower_hex(Sha256::digest(source.as_bytes())),
        u8::from(is_host_only_git_source(source)).to_string(),
        exact_ref,
    ];
    if let Some(cutoff) = cutoff {
        cutoff.validate()?;
        arguments.extend(cutoff.commits().map(str::to_owned));
    }
    let mut command = host.git_shell_command(
        PREPARE_REPOSITORY_SCRIPT,
        arguments,
        format!("prepare cached Git objects for {}", display_url(source)),
    );
    if let Some(token) = github_token {
        let mut input = token.as_bytes().to_vec();
        input.push(b'\n');
        command = command.with_sensitive_stdin(input);
    }
    let output = executor.execute(&command)?;
    checked(output, &command)
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!(error.to_string().replace(source, &display_url(source))))
}

const CACHE_GC_SCRIPT: &str = r#"
set -eu
root=$1
limit_kib=$2
shift 2
umask 077
mkdir -p "$root/mirrors" "$root/locks" "$root/sessions"
marker="$root/last-gc"
if [ -f "$marker" ] && find "$marker" -mmin -1440 -print | grep -q .; then
    exit 0
fi
# Reap old lock files and stale lock directories from earlier builds.
find "$root/locks" -mindepth 1 -maxdepth 1 \( -type f -o -type d \) -mmin +1440 -exec rm -rf {} \;
candidates="$root/gc-candidates-$$"
track_cache_locks
trap 'cleanup_cache_locks; rm -f -- "$candidates"' EXIT
try_cache_lock "$root/locks/gc" gc || exit 0
for snapshot in "$root"/sessions/*; do
    [ -d "$snapshot" ] || continue
    session=${snapshot##*/}
    live=0
    for active in "$@"; do
        if [ "$session" = "$active" ]; then
            live=1
            break
        fi
    done
    # A session without a container may still be between preparation and
    # container start, so only the root's own preparation stamp ages it. Its
    # snapshots hardlink mirror objects and inherit their older times.
    if [ "$live" -eq 0 ] && find "$snapshot" -maxdepth 0 -mmin +1440 -print | grep -q .; then
        rm -rf -- "$snapshot"
    fi
done
for entry in "$root"/mirrors/*; do
    [ -d "$entry" ] || continue
    key=${entry##*/}
    [ -f "$entry/last-used" ] || continue
    if find "$entry/last-used" -mmin +43200 -print | grep -q . && try_cache_lock "$root/locks/$key"; then
        rm -rf -- "$entry"
        release_cache_lock
    fi
done
size=$(du -sk "$root/mirrors" | awk '{print $1}')
if [ "$size" -gt "$limit_kib" ]; then
    ls -1tr "$root"/mirrors/*/last-used > "$candidates" 2>/dev/null || true
    while IFS= read -r used; do
        [ -n "$used" ] || continue
        entry=${used%/last-used}
        key=${entry##*/}
        if try_cache_lock "$root/locks/$key"; then
            rm -rf -- "$entry"
            release_cache_lock
            size=$(du -sk "$root/mirrors" | awk '{print $1}')
            [ "$size" -le "$limit_kib" ] && break
        fi
    done < "$candidates"
fi
: > "$marker"
"#;

fn collect_garbage(
    host: &CacheHost,
    cache_root: &Path,
    live_sessions: &[String],
    executor: &impl CommandExecutor,
) -> Result<()> {
    collect_garbage_with_limit(host, cache_root, live_sessions, CACHE_MAX_KIB, executor)
}

fn collect_garbage_with_limit(
    host: &CacheHost,
    cache_root: &Path,
    live_sessions: &[String],
    limit_kib: u64,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let mut arguments = vec![
        cache_root.to_string_lossy().into_owned(),
        limit_kib.to_string(),
    ];
    arguments.extend(live_sessions.iter().cloned());
    let command = host.git_shell_command(
        CACHE_GC_SCRIPT,
        arguments,
        "prune the container host Git cache",
    );
    let output = executor.execute(&command)?;
    checked(output, &command).map(|_| ())
}

fn cleanup_session_root(
    host: &CacheHost,
    session_root: &Path,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let command = host.git_shell_command(
        "set -eu; root=$1; case $root in */.cache/mjolnir/git/sessions/*) rm -rf -- \"$root\" ;; *) echo 'refusing unsafe clone-cache cleanup path' >&2; exit 2 ;; esac",
        [session_root.to_string_lossy().into_owned()],
        "remove failed session Git cache snapshot",
    );
    let output = executor.execute(&command)?;
    checked(output, &command).map(|_| ())
}

fn checked(output: CommandOutput, command: &CommandSpec) -> Result<CommandOutput> {
    if output.status != 0 {
        bail!(
            "{} failed with status {}: {}",
            command.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::targets::{ContainerTemplate, ProcessExecutor, RepositorySpec, TargetTemplate};
    use mj_core::config::ImagePullPolicy;

    #[derive(Default)]
    struct RecordingExecutor {
        commands: Mutex<Vec<CommandSpec>>,
        notices: Mutex<Vec<String>>,
        status: i32,
        stdout: Vec<u8>,
    }

    impl CommandExecutor for RecordingExecutor {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.lock().unwrap().push(command.clone());
            Ok(CommandOutput {
                status: self.status,
                stdout: self.stdout.clone(),
                stderr: b"unavailable".to_vec(),
            })
        }

        fn notify_notice(&self, notice: &str) {
            self.notices.lock().unwrap().push(notice.to_owned());
        }
    }

    #[cfg(unix)]
    struct GitExecutor(BTreeMap<String, String>);

    #[cfg(unix)]
    impl CommandExecutor for GitExecutor {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            if command.program == "podman" {
                return Ok(CommandOutput {
                    status: 0,
                    stdout: b"[]".to_vec(),
                    stderr: Vec::new(),
                });
            }
            let mut command = command.clone();
            command.env.extend(self.0.clone());
            ProcessExecutor.execute(&command)
        }
    }

    fn podman_fixture_bundle(source: &str) -> (TargetTemplate, ProjectBundleSpec) {
        let target = TargetTemplate::LocalPodman(ContainerTemplate {
            build_cache: None,
            image: "ubuntu:24.04".to_owned(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: Vec::new(),
            workspace_storage: Default::default(),
        });
        let bundle = ProjectBundleSpec {
            primary: "app".to_owned(),
            repositories: vec![RepositorySpec {
                url: Some(source.to_owned()),
                push_urls: Vec::new(),
                destination: "app".to_owned(),
                git_ref: None,
                reference: None,
                history_cutoff: None,
            }],
        };
        (target, bundle)
    }

    #[test]
    fn unavailable_host_git_reports_the_source_without_mutating_the_bundle() {
        let executor = RecordingExecutor {
            status: 127,
            ..Default::default()
        };
        let source = "git://127.0.0.2/app.git";
        let (target, mut bundle) = podman_fixture_bundle(source);
        let mut mounts = Vec::new();

        let error = prepare(
            &target,
            "session-12345678",
            &mut bundle,
            &mut mounts,
            None,
            None,
            &executor,
        )
        .unwrap_err();
        assert!(Refusal::of(&error).unwrap().message().contains(source));
        assert_eq!(bundle.repositories[0].reference, None);
        assert!(mounts.is_empty());
    }

    #[test]
    fn unavailable_host_git_leaves_a_github_bundle_to_clone_directly() {
        let executor = RecordingExecutor {
            status: 127,
            ..Default::default()
        };
        let source = "https://github.com/example/app.git";
        let (target, mut bundle) = podman_fixture_bundle(source);
        let session_id = "session-12345678";
        let mut mounts = Vec::new();
        assert!(
            prepare(
                &target,
                session_id,
                &mut bundle,
                &mut mounts,
                None,
                None,
                &executor
            )
            .unwrap()
            .is_none()
        );
        assert!(mounts.is_empty());
        assert_eq!(bundle.repositories[0].reference, None);
        assert!(executor.notices.lock().unwrap()[0].contains("using direct clones"));
        let plan = targets::provision_plan(&target, session_id, &bundle, &mounts, None).unwrap();
        let clone = plan
            .commands
            .iter()
            .find(|command| command.purpose == "clone app")
            .unwrap();
        assert!(clone.args.iter().any(|arg| arg == source));
        assert!(!clone.args.iter().any(|arg| arg.starts_with("--reference")));
    }

    #[test]
    fn cache_token_never_enters_arguments_or_debug_output() {
        let executor = RecordingExecutor {
            status: 1,
            ..Default::default()
        };
        let token = "github-token-that-must-stay-private";
        let _ = prepare_repository(
            &CacheHost::Local,
            Path::new("/home/test/.cache/mjolnir/git"),
            "abc123",
            "https://github.com/example/app.git",
            Path::new("/home/test/.cache/mjolnir/git/sessions/session-12345678/abc123.git"),
            None,
            None,
            Some(token),
            &executor,
        );

        let commands = executor.commands.lock().unwrap();
        let command = commands.last().unwrap();
        assert!(!command.args.iter().any(|argument| argument.contains(token)));
        assert!(!format!("{command:?}").contains(token));
        assert!(format!("{command:?}").contains("<redacted>"));
        assert!(!serde_json::to_string(command).unwrap().contains(token));
    }

    #[test]
    fn docker_cache_discovers_managed_sessions_through_docker_json_lines() {
        let session_id = "0123456789abcdef0123456789abcdef";
        let executor = RecordingExecutor {
            stdout: format!(
                "{{\"Labels\":\"{}=true,{}={session_id}\"}}\n",
                targets::MANAGED_LABEL,
                targets::SESSION_LABEL
            )
            .into_bytes(),
            ..Default::default()
        };
        let target = TargetTemplate::LocalDocker(ContainerTemplate {
            build_cache: None,
            image: "ubuntu:24.04".to_owned(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: Vec::new(),
            workspace_storage: Default::default(),
        });

        let host = CacheHost::for_target(&target).expect("Docker has a clone-cache host");
        assert_eq!(
            host.managed_sessions("docker", &executor).unwrap(),
            [session_id.to_owned()]
        );

        let commands = executor.commands.lock().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].program, "docker");
        assert_eq!(
            commands[0].args,
            [
                "ps",
                "--all",
                "--filter",
                "label=dev.mj.managed=true",
                "--format",
                "json"
            ]
        );
    }

    // Hard-won: 020257d9f: cached GitHub mirrors reused the original fetch transport.
    #[cfg(unix)]
    #[test]
    fn local_snapshot_hardlinks_objects_but_owns_its_refs() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let cache_root = temporary.path().join("cache");
        let snapshot = cache_root.join("sessions/session-12345678/repository.git");
        for (args, purpose) in [
            (
                vec!["init", "--initial-branch=main", source.to_str().unwrap()],
                "initialize source repository",
            ),
            (
                vec![
                    "-C",
                    source.to_str().unwrap(),
                    "config",
                    "user.name",
                    "Hel Test",
                ],
                "configure test user",
            ),
            (
                vec![
                    "-C",
                    source.to_str().unwrap(),
                    "config",
                    "user.email",
                    "hel@example.test",
                ],
                "configure test email",
            ),
        ] {
            let command = CommandSpec::new("git", args).purpose(purpose);
            checked(ProcessExecutor.execute(&command).unwrap(), &command).unwrap();
        }
        std::fs::write(source.join("README.md"), "cached objects\n").unwrap();
        for (args, purpose) in [
            (
                vec!["-C", source.to_str().unwrap(), "add", "README.md"],
                "stage test content",
            ),
            (
                vec!["-C", source.to_str().unwrap(), "commit", "-m", "initial"],
                "commit test content",
            ),
        ] {
            let command = CommandSpec::new("git", args).purpose(purpose);
            checked(ProcessExecutor.execute(&command).unwrap(), &command).unwrap();
        }

        let git = |directory: &Path, args: &[&str]| {
            let command = CommandSpec::new(
                "git",
                ["-C", directory.to_str().unwrap()]
                    .into_iter()
                    .chain(args.iter().copied()),
            );
            let output = checked(ProcessExecutor.execute(&command).unwrap(), &command).unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        let ssh_url = "git@github.com:example/cache.git";
        let https_url = "https://github.com/example/cache.git";
        let config = temporary.path().join("gitconfig");
        let config_path = config.to_str().unwrap();
        let rewrite = format!("url.{}.insteadOf", source.display());
        git(
            &source,
            &["config", "--file", config_path, &rewrite, ssh_url],
        );
        let executor = GitExecutor(BTreeMap::from([
            ("GIT_CONFIG_GLOBAL".into(), config.display().to_string()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ]));
        let key = repository_cache_key(ssh_url);
        let prepare = |url: &str, snapshot: &Path| {
            prepare_repository(
                &CacheHost::Local,
                &cache_root,
                &key,
                url,
                snapshot,
                None,
                None,
                None,
                &executor,
            )
        };
        prepare(ssh_url, &snapshot).unwrap();

        let mirror = cache_root.join("mirrors").join(&key).join("repo.git");
        let object = first_object_file(&mirror.join("objects")).unwrap();
        let relative = object.strip_prefix(&mirror).unwrap();
        let snapshot_object = snapshot.join(relative);
        let mirror_metadata = object.metadata().unwrap();
        let snapshot_metadata = snapshot_object.metadata().unwrap();
        assert_eq!(mirror_metadata.dev(), snapshot_metadata.dev());
        assert_eq!(mirror_metadata.ino(), snapshot_metadata.ino());
        assert_ne!(
            mirror.join("HEAD").metadata().unwrap().ino(),
            snapshot.join("HEAD").metadata().unwrap().ino()
        );
        assert_eq!(
            snapshot
                .parent()
                .unwrap()
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o005,
            0o005
        );
        assert_eq!(snapshot_metadata.permissions().mode() & 0o004, 0o004);

        let initial = git(&snapshot, &["rev-parse", "--verify", "HEAD"]);
        // Retire the original transport after populating the shared mirror.
        git(
            &source,
            &["config", "--file", config_path, &rewrite, https_url],
        );
        let missing = temporary.path().join("missing");
        git(
            &source,
            &[
                "config",
                "--file",
                config_path,
                &format!("url.{}.insteadOf", missing.display()),
                ssh_url,
            ],
        );
        git(&source, &["switch", "--quiet", "-c", "new-default"]);
        git(
            &source,
            &[
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "new upstream state",
            ],
        );
        assert_eq!(repository_cache_key(https_url), key);
        let refreshed = snapshot.with_file_name("refreshed.git");
        prepare(https_url, &refreshed).unwrap();
        assert_eq!(
            git(&refreshed, &["rev-parse", "refs/heads/new-default"]),
            git(&source, &["rev-parse", "HEAD"])
        );
        assert_eq!(git(&snapshot, &["rev-parse", "HEAD"]), initial);
    }

    // Hard-won: 020257d9f: destinations sharing a remote overwrote each other's cutoff snapshots.
    #[cfg(unix)]
    #[test]
    fn a_cutoff_snapshot_is_a_bundle_of_the_cutoff_history_alone() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let cache_root = temporary.path().join(CACHE_RELATIVE_ROOT);
        let git = |directory: &Path, args: &[&str]| -> String {
            let mut arguments = vec!["-C", directory.to_str().unwrap()];
            arguments.extend(args);
            let command = CommandSpec::new("git", arguments).purpose("cutoff fixture Git");
            let output = checked(ProcessExecutor.execute(&command).unwrap(), &command).unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        std::fs::create_dir(&source).unwrap();
        git(&source, &["init", "--quiet", "--initial-branch=main"]);
        git(&source, &["config", "user.name", "Hel Test"]);
        git(&source, &["config", "user.email", "hel@example.test"]);
        let commit = |message: &str| {
            std::fs::write(source.join("README.md"), format!("{message}\n")).unwrap();
            git(&source, &["add", "README.md"]);
            git(&source, &["commit", "--quiet", "-m", message]);
            git(&source, &["rev-parse", "HEAD"])
        };
        commit("initial");
        git(&source, &["switch", "--quiet", "-c", "trunk"]);
        let base = commit("base");
        git(&source, &["switch", "--quiet", "main"]);
        let head = commit("head");
        let later = commit("later");
        git(&source, &["tag", "-a", "v-later", "-m", "later"]);
        let cutoff = HistoryCutoff {
            head: head.clone(),
            base: Some(base.clone()),
        };

        let remote = "git://127.0.0.1:9418/repository";
        let config = temporary.path().join("gitconfig");
        git(
            &source,
            &[
                "config",
                "--file",
                config.to_str().unwrap(),
                &format!("url.{}.insteadOf", source.display()),
                remote,
            ],
        );
        let executor = GitExecutor(BTreeMap::from([
            ("HOME".into(), temporary.path().display().to_string()),
            ("GIT_CONFIG_GLOBAL".into(), config.display().to_string()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ]));
        let target = TargetTemplate::LocalPodman(ContainerTemplate {
            build_cache: None,
            image: "ubuntu:24.04".into(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: Vec::new(),
            workspace_storage: Default::default(),
        });
        let repository = |destination: &str, history_cutoff| RepositorySpec {
            url: Some(remote.into()),
            push_urls: Vec::new(),
            destination: destination.into(),
            git_ref: None,
            reference: None,
            history_cutoff,
        };
        let mut bundle = ProjectBundleSpec {
            primary: "pinned".into(),
            repositories: vec![
                repository("pinned", Some(cutoff)),
                repository(
                    "other-pin",
                    Some(HistoryCutoff {
                        head: base.clone(),
                        base: None,
                    }),
                ),
                repository("full", None),
            ],
        };
        let mut mounts = Vec::new();
        prepare(
            &target,
            "session-12345678",
            &mut bundle,
            &mut mounts,
            None,
            None,
            &executor,
        )
        .unwrap()
        .unwrap();
        assert_eq!(mounts.len(), 1);
        let snapshots: Vec<_> = bundle
            .repositories
            .iter()
            .map(|repository| {
                let reference = Path::new(repository.reference.as_ref().unwrap());
                mounts[0]
                    .source
                    .join(reference.strip_prefix(&mounts[0].destination).unwrap())
            })
            .collect();
        let [snapshot, other, full] = snapshots.as_slice() else {
            unreachable!()
        };
        assert_ne!(snapshot, other);
        assert_eq!(std::fs::read_dir(&mounts[0].source).unwrap().count(), 3);
        for (path, refs) in [
            (
                snapshot,
                format!("{head} refs/mj/head\n{base} refs/mj/base"),
            ),
            (other, format!("{base} refs/mj/head")),
        ] {
            assert_eq!(
                git(
                    temporary.path(),
                    &["bundle", "list-heads", path.to_str().unwrap()]
                ),
                refs
            );
        }
        assert_eq!(git(full, &["rev-parse", "HEAD"]), later);
        let key = repository_cache_key(remote);
        let mirror = cache_root.join("mirrors").join(&key).join("repo.git");
        git(&mirror, &["cat-file", "-e", &later]);

        let checkout = temporary.path().join("checkout");
        git(
            temporary.path(),
            &["init", "--quiet", checkout.to_str().unwrap()],
        );
        git(
            &checkout,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--",
                snapshot.to_str().unwrap(),
                &head,
            ],
        );
        let mut present = git(
            &checkout,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname)",
            ],
        )
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        present.sort();
        let mut reachable = git(&checkout, &["rev-list", "--objects", &head, &base])
            .lines()
            .map(|line| line.split(' ').next().unwrap().to_owned())
            .collect::<Vec<_>>();
        reachable.sort();
        reachable.dedup();
        // Fetching the head alone stored the whole bundle, base included.
        assert_eq!(present, reachable);
        assert!(!present.contains(&later));

        // A warm host-only mirror also serves a detached HEAD outside refs/*.
        git(&source, &["switch", "--quiet", "--detach"]);
        let detached = commit("unreferenced detached HEAD");
        let refreshed = full.with_file_name("detached.git");
        prepare_repository(
            &CacheHost::Local,
            &cache_root,
            &key,
            remote,
            &refreshed,
            None,
            None,
            None,
            &executor,
        )
        .unwrap();
        assert_eq!(git(&refreshed, &["rev-parse", "HEAD"]), detached);

        // A cutoff the host cannot fetch fails before anything is mounted.
        let missing = HistoryCutoff {
            head: "1".repeat(40),
            base: None,
        };
        assert!(
            prepare_repository(
                &CacheHost::Local,
                &cache_root,
                &key,
                remote,
                snapshot,
                Some(&missing),
                None,
                None,
                &executor,
            )
            .is_err()
        );
        assert!(!snapshot.exists());
    }

    /// The clone cache runs Git on the controller or on a container target's
    /// SSH host with the SSH command the controller decided, so the operator's
    /// `GIT_SSH_COMMAND` governs it as it does every other Git command.
    // Hard-won: #1466: the clone cache forced accept-new over the operator's GIT_SSH_COMMAND
    #[cfg(unix)]
    #[test]
    fn the_clone_cache_reaches_ssh_remotes_with_the_controllers_ssh_command() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = root.join("ssh-arguments");
        let fake_ssh = bin.join("ssh");
        std::fs::write(
            &fake_ssh,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$SSH_TEST_LOG\"\nexit 255\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        for (ssh_command, chosen) in [
            (
                mj_core::local_git::NON_INTERACTIVE_GIT_SSH_COMMAND,
                "StrictHostKeyChecking=accept-new",
            ),
            (
                "ssh -o StrictHostKeyChecking=yes",
                "StrictHostKeyChecking=yes",
            ),
        ] {
            let _ = std::fs::remove_file(&log);
            let mut command = CacheHost::Local.git_shell_command(
                PREPARE_REPOSITORY_SCRIPT,
                [
                    root.join("cache").display().to_string(),
                    "repository".into(),
                    "ssh://git.example.invalid/repository.git".into(),
                    root.join("cache/sessions/one/repository.bundle")
                        .display()
                        .to_string(),
                    "0".into(),
                    ssh_command.into(),
                    "ssh-fixture-source".into(),
                    "0".into(),
                    String::new(),
                ],
                "prepare cached Git objects",
            );
            command.env.insert(
                "PATH".into(),
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            );
            command
                .env
                .insert("SSH_TEST_LOG".into(), log.display().to_string());
            let output = ProcessExecutor.execute(&command).unwrap();
            assert_ne!(output.status, 0, "the fake SSH refuses every connection");
            let arguments = std::fs::read_to_string(&log)
                .unwrap_or_else(|error| panic!("{ssh_command}: SSH never ran: {error}"));
            let options: Vec<_> = arguments.lines().collect();
            assert!(options.contains(&chosen), "{ssh_command}: {arguments}");
            assert_eq!(
                options
                    .iter()
                    .filter(|option| option.starts_with("StrictHostKeyChecking="))
                    .count(),
                1,
                "{ssh_command}: {arguments}"
            );
        }
    }

    // Hard-won: #1388: packing one session's history blocked every clone of the same repository.
    // Hard-won: 1bd051d40: a mkdir lock was acquired twice under uutils.
    #[cfg(unix)]
    #[test]
    fn concurrent_snapshots_share_a_refresh_and_survive_mirror_eviction() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::mpsc;
        use std::time::Duration;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let source = root.join("source with spaces");
        let cache = root.join("cache");
        let wrappers = root.join("bin");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&wrappers).unwrap();
        let git = |directory: &Path, args: &[&str]| {
            let command = CommandSpec::new(
                "git",
                ["-C", directory.to_str().unwrap()]
                    .into_iter()
                    .chain(args.iter().copied()),
            );
            let output = checked(ProcessExecutor.execute(&command).unwrap(), &command).unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&source, &["init", "--quiet", "--initial-branch=main"]);
        git(&source, &["config", "user.name", "Cache Test"]);
        git(&source, &["config", "user.email", "cache@example.test"]);
        std::fs::write(source.join("data"), vec![b'a'; 128 * 1024]).unwrap();
        git(&source, &["add", "data"]);
        git(&source, &["commit", "--quiet", "-m", "cutoff"]);
        let head = git(&source, &["rev-parse", "HEAD"]);
        std::fs::write(source.join("data"), "later history").unwrap();
        git(&source, &["commit", "--quiet", "-am", "later"]);
        let later = git(&source, &["rev-parse", "HEAD"]);
        let cutoff = HistoryCutoff {
            head: head.clone(),
            base: None,
        };

        let executable = |name: &str| {
            let command = CommandSpec::new("sh", ["-c", &format!("command -v {name}")]);
            let output = checked(ProcessExecutor.execute(&command).unwrap(), &command).unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        let real_git = executable("git");
        let real_mkdir = executable("mkdir");
        let real_sleep = executable("sleep");
        let git_wrapper = r#"#!/bin/sh
set -eu
wait_for() {
    attempts=0
    while [ ! -f "$1" ]; do
        attempts=$((attempts + 1))
        if [ "$attempts" -ge 1200 ]; then
            echo "cache regression gate timed out: $1" >&2
            exit 99
        fi
        sleep 0.05
    done
}
if { [ "$1" = clone ] && [ "$2" = --mirror ] && [ "$3" = -- ]; } ||
   { [ "$1" = -C ] && [ "$3" = fetch ] && [ "$4" = --prune ]; }; then
    echo refresh >> "$CACHE_TEST_ROOT/refreshes"
    : > "$CACHE_TEST_ROOT/refresh-started"
    wait_for "$CACHE_TEST_ROOT/release-refresh"
fi
if [ "$1" = -C ] && [ "$3" = fetch ] && [ "$4" != --prune ]; then
    if [ "${CACHE_TEST_PAUSE_FETCH:-0}" = 1 ]; then
        : > "$CACHE_TEST_ROOT/snapshot-pinned"
        wait_for "$CACHE_TEST_ROOT/release-snapshot"
    fi
    if [ "${CACHE_TEST_FAIL_FETCH:-0}" = 1 ]; then
        echo 'injected cutoff failure' >&2
        exit 42
    fi
fi
exec "$CACHE_TEST_GIT" "$@"
"#;
        let mkdir_wrapper = r#"#!/bin/sh
set -eu
if [ "$#" -eq 1 ] && [ "$1" = "$CACHE_TEST_ROOT/cache/locks/repository" ] &&
   [ "${CACHE_TEST_WAITER:-0}" = 1 ] && [ -d "$1" ]; then
    # Simulate uutils treating a competing mkdir's EEXIST as success.
    : > "$CACHE_TEST_ROOT/waiter-started"
    exit 0
fi
exec "$CACHE_TEST_MKDIR" "$@"
"#;
        let sleep_wrapper = r#"#!/bin/sh
set -eu
if [ "${CACHE_TEST_WAITER:-0}" = 1 ] && [ "$1" = "0.2" ]; then
    : > "$CACHE_TEST_ROOT/waiter-started"
fi
exec "$CACHE_TEST_SLEEP" "$@"
"#;
        for (name, script) in [
            ("git", git_wrapper),
            ("mkdir", mkdir_wrapper),
            ("sleep", sleep_wrapper),
        ] {
            let path = wrappers.join(name);
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let environment = BTreeMap::from([
            (
                "PATH".into(),
                format!("{}:{}", wrappers.display(), std::env::var("PATH").unwrap()),
            ),
            ("CACHE_TEST_ROOT".into(), root.display().to_string()),
            ("CACHE_TEST_GIT".into(), real_git),
            ("CACHE_TEST_MKDIR".into(), real_mkdir),
            ("CACHE_TEST_SLEEP".into(), real_sleep),
        ]);
        let mut first = GitExecutor(environment.clone());
        first.0.insert("CACHE_TEST_PAUSE_FETCH".into(), "1".into());
        let mut second = GitExecutor(environment.clone());
        second.0.insert("CACHE_TEST_WAITER".into(), "1".into());
        let snapshot_a = cache.join("sessions/first/repository.bundle");
        let snapshot_b = cache.join("sessions/second/repository.bundle");
        let prepare = |snapshot: &Path, executor: &GitExecutor| {
            prepare_repository(
                &CacheHost::Local,
                &cache,
                "repository",
                source.to_str().unwrap(),
                snapshot,
                Some(&cutoff),
                None,
                None,
                executor,
            )
        };
        let wait_for = |name: &str| {
            let marker = root.join(name);
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            while !marker.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(marker.exists(), "missing regression marker: {name}");
        };
        std::thread::scope(|scope| {
            let a = scope.spawn(|| prepare(&snapshot_a, &first));
            wait_for("refresh-started");
            let (done_tx, done_rx) = mpsc::channel();
            let b = scope.spawn({
                let snapshot = &snapshot_b;
                let executor = &second;
                let prepare = &prepare;
                move || {
                    let result = prepare(snapshot, executor);
                    done_tx.send(result).unwrap();
                }
            });
            wait_for("waiter-started");
            // The old implementation abandons this accepted wait after five seconds.
            let early = done_rx.recv_timeout(Duration::from_secs(6));
            std::fs::write(root.join("release-refresh"), []).unwrap();
            wait_for("snapshot-pinned");
            let mounted_entries = std::fs::read_dir(snapshot_a.parent().unwrap())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>();
            assert!(
                mounted_entries
                    .iter()
                    .all(|entry| entry.starts_with("repository.bundle.cutoff-")),
                "a full-history pin entered the mounted session subtree: {mounted_entries:?}"
            );
            let completed = match early {
                Ok(result) => result,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    done_rx.recv_timeout(Duration::from_secs(20)).unwrap()
                }
                Err(error) => panic!("waiter disconnected: {error}"),
            };
            // A private snapshot must own its files even if the shared mirror is evicted.
            collect_garbage_with_limit(
                &CacheHost::Local,
                &cache,
                &["first".into(), "second".into()],
                0,
                &ProcessExecutor,
            )
            .unwrap();
            let evicted = !cache.join("mirrors/repository").exists();
            std::fs::write(root.join("release-snapshot"), []).unwrap();
            b.join().unwrap();
            let first_result = a.join().unwrap();
            completed.unwrap();
            first_result.unwrap();
            assert!(
                evicted,
                "the shared mirror was not evicted while the reader was pinned"
            );
        });
        assert_eq!(
            std::fs::read_to_string(root.join("refreshes"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        assert_eq!(std::fs::read_dir(cache.join("mirrors")).unwrap().count(), 0);
        for snapshot in [&snapshot_a, &snapshot_b] {
            assert_eq!(
                git(root, &["bundle", "list-heads", snapshot.to_str().unwrap()]),
                format!("{head} refs/mj/head")
            );
            let checkout = snapshot.with_extension("checkout");
            git(
                root,
                &["init", "--quiet", "--bare", checkout.to_str().unwrap()],
            );
            git(
                &checkout,
                &["fetch", "--quiet", "--", snapshot.to_str().unwrap(), &head],
            );
            let command = CommandSpec::new(
                "git",
                ["-C", checkout.to_str().unwrap(), "cat-file", "-e", &later],
            );
            assert_ne!(ProcessExecutor.execute(&command).unwrap().status, 0);
            assert_eq!(
                std::fs::read_dir(snapshot.parent().unwrap())
                    .unwrap()
                    .filter_map(|entry| {
                        let name = entry.unwrap().file_name();
                        name.to_str()
                            .filter(|name| {
                                name.contains(".pinned-")
                                    || name.contains(".cutoff-")
                                    || name.contains(".partial-")
                            })
                            .map(str::to_owned)
                    })
                    .collect::<Vec<_>>(),
                Vec::<String>::new()
            );
        }

        let mut failing = GitExecutor(environment);
        failing.0.insert("CACHE_TEST_FAIL_FETCH".into(), "1".into());
        let failed = cache.join("sessions/failed/repository.bundle");
        assert!(
            prepare(&failed, &failing)
                .unwrap_err()
                .to_string()
                .contains("injected cutoff failure")
        );
        assert_eq!(
            std::fs::read_dir(failed.parent().unwrap()).unwrap().count(),
            0
        );
        assert!(!cache.join("locks/repository").exists());
        assert!(
            std::fs::read_dir(cache.join("mirrors"))
                .unwrap()
                .all(|entry| { entry.unwrap().file_name() == "repository" })
        );
    }

    // Hard-won: 932a8969f: interrupted GC left its eviction lock.
    #[test]
    #[cfg_attr(windows, ignore = "#1297: runs sh, du or touch on the controller")]
    fn cache_gc_evicts_the_oldest_mirror_to_meet_its_soft_cap() {
        let temporary = tempfile::tempdir().unwrap();
        let cache_root = temporary.path().join("cache");
        let mirrors = cache_root.join("mirrors");
        let old = mirrors.join("old");
        let recent = mirrors.join("recent");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("objects"), vec![b'o'; 16 * 1024]).unwrap();
        std::fs::write(old.join("last-used"), []).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        std::fs::create_dir_all(&recent).unwrap();
        std::fs::write(recent.join("objects"), vec![b'n'; 16 * 1024]).unwrap();
        std::fs::write(recent.join("last-used"), []).unwrap();
        // `du` counts allocated blocks, which FUSE or compressed development
        // mounts can report far below the written bytes, so a fixed offset
        // from one entry may never exceed the soft cap. Place the cap between
        // "both mirrors" and "only the newest mirror", measured with the same
        // du the garbage collector uses.
        let total = directory_kib(&mirrors);
        let oldest = directory_kib(&old);
        assert!(oldest > 0, "du must see the older mirror's blocks");
        let limit = total - oldest.div_ceil(2);

        collect_garbage_with_limit(&CacheHost::Local, &cache_root, &[], limit, &ProcessExecutor)
            .unwrap();

        assert!(!old.exists());
        assert!(recent.exists());

        #[cfg(unix)]
        {
            // Run the size-based eviction again with a TERM arriving after
            // GC owns the recent mirror's lock and before removal finishes.
            std::fs::remove_file(cache_root.join("last-gc")).unwrap();
            let wrappers = temporary.path().join("bin");
            std::fs::create_dir(&wrappers).unwrap();
            let eviction_lock = cache_root.join("locks/recent");
            mj_core::test_hooks::install_fake_command(
                &wrappers,
                "rm",
                r#"#!/bin/sh
set -eu
if [ "$#" -ge 3 ] && [ "$3" = "${CACHE_TEST_EVICTION:-}" ]; then
    test -f "$CACHE_TEST_EVICTION_LOCK"
    kill -TERM "$PPID"
    exit 0
fi
exec /bin/rm "$@"
"#,
            );
            let mut executor = GitExecutor(BTreeMap::from([
                (
                    "PATH".into(),
                    format!("{}:{}", wrappers.display(), std::env::var("PATH").unwrap()),
                ),
                ("CACHE_TEST_EVICTION".into(), recent.display().to_string()),
                (
                    "CACHE_TEST_EVICTION_LOCK".into(),
                    eviction_lock.display().to_string(),
                ),
            ]));
            let error =
                collect_garbage_with_limit(&CacheHost::Local, &cache_root, &[], 0, &executor)
                    .unwrap_err();
            assert!(error.to_string().contains("status 143"), "{error:#}");
            assert!(recent.exists());
            assert!(!eviction_lock.exists());
            assert!(!cache_root.join("locks/gc").exists());
            assert!(std::fs::read_dir(&cache_root).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("gc-candidates-")
            }));

            executor.0.remove("CACHE_TEST_EVICTION");
            let source = temporary.path().join("source");
            let git = CommandSpec::new("git", ["init", "--bare", source.to_str().unwrap()]);
            checked(ProcessExecutor.execute(&git).unwrap(), &git).unwrap();
            mj_core::test_hooks::install_fake_command(
                &wrappers,
                "sleep",
                "#!/bin/sh\necho 'preparation unexpectedly waited for the eviction lock' >&2\nexit 99\n",
            );
            let snapshot = cache_root.join("sessions/new/recent.git");
            prepare_repository(
                &CacheHost::Local,
                &cache_root,
                "recent",
                source.to_str().unwrap(),
                &snapshot,
                None,
                None,
                None,
                &executor,
            )
            .unwrap();
            assert!(snapshot.join("HEAD").exists());
            assert!(!eviction_lock.exists());
        }
    }

    #[test]
    #[cfg_attr(windows, ignore = "#1297: runs sh, du or touch on the controller")]
    fn cache_gc_removes_only_old_orphan_session_snapshots() {
        let temporary = tempfile::tempdir().unwrap();
        let cache_root = temporary.path().join("cache");
        let orphan = cache_root.join("sessions/orphan-session");
        let live = cache_root.join("sessions/live-session");
        let starting = cache_root.join("sessions/starting-session");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::create_dir_all(&live).unwrap();
        // A just-prepared snapshot whose container does not exist yet.
        let inherited = starting.join("repository.git/objects/pack/old.pack");
        std::fs::create_dir_all(inherited.parent().unwrap()).unwrap();
        std::fs::write(&inherited, []).unwrap();
        for path in [&orphan, &live, &inherited] {
            let command = CommandSpec::new("touch", ["-t", "200001010000", path.to_str().unwrap()])
                .purpose("age test clone-cache snapshot");
            checked(ProcessExecutor.execute(&command).unwrap(), &command).unwrap();
        }

        collect_garbage_with_limit(
            &CacheHost::Local,
            &cache_root,
            &["live-session".to_owned()],
            CACHE_MAX_KIB,
            &ProcessExecutor,
        )
        .unwrap();

        assert!(!orphan.exists());
        assert!(live.exists());
        // Hard-won: #1567: a just-prepared snapshot with no container yet was
        // collected because its hardlinked mirror objects carry old times.
        assert!(inherited.exists());
    }

    fn directory_kib(path: &Path) -> u64 {
        let command = CommandSpec::new("du", ["-sk", path.to_str().unwrap()])
            .purpose("measure test clone-cache entry");
        let output = checked(ProcessExecutor.execute(&command).unwrap(), &command).unwrap();
        String::from_utf8(output.stdout)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    #[cfg(unix)]
    fn first_object_file(root: &Path) -> Option<PathBuf> {
        for entry in std::fs::read_dir(root).ok()? {
            let entry = entry.ok()?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir()
                && name.len() == 2
                && name.bytes().all(|byte| byte.is_ascii_hexdigit())
                && let Some(object) = std::fs::read_dir(path)
                    .ok()?
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .find(|path| path.is_file())
            {
                return Some(object);
            }
        }
        std::fs::read_dir(root.join("pack"))
            .ok()?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "pack")
            })
    }
}
