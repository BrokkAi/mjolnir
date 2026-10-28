//! Best-effort host-side Git object caches for container-backed sessions.

use mj_core::hex::lower_hex;
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
        self.shell_command(script, "hel-git-cache", arguments, purpose)
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
        super::recovery_scan::managed_sessions_from_container_json(&output.stdout)
            .map(|sessions| sessions.into_iter().map(|(session, _)| session).collect())
    }
}

#[derive(Debug, Clone)]
pub(super) struct PreparedCloneCache {
    host: CacheHost,
    session_root: PathBuf,
}

impl PreparedCloneCache {
    pub(super) fn cleanup(&self, executor: &impl CommandExecutor) -> Result<()> {
        cleanup_session_root(&self.host, &self.session_root, executor)
    }
}

/// Populate immutable, session-scoped reference repositories and attach them
/// to a container bundle. Every failure is an optimization miss: the original
/// network clone remains authoritative and receives a user-visible notice.
///
/// A repository with a history cutoff gets a Git bundle of the cutoff commits
/// and their ancestors instead of a copy of the mirror, so nothing later is
/// mounted into its container.
pub(super) fn prepare(
    target: &targets::TargetTemplate,
    session_id: &str,
    bundle: &mut ProjectBundleSpec,
    mounts: &mut Vec<AdditionalMount>,
    github_token: Option<&str>,
    executor: &(impl CommandExecutor + Sync),
) -> Option<PreparedCloneCache> {
    let host = CacheHost::for_target(target)?;
    let engine = target.container_engine()?;
    let repositories = bundle
        .repositories
        .iter()
        .filter_map(|repository| {
            let source = repository.url.as_deref()?;
            repository_cache_key(source)
                .map(|key| (key, (source.to_owned(), repository.history_cutoff.clone())))
                .map_err(|error| {
                    executor.notify_notice(&format!(
                        "Clone cache skipped for {}: {error:#}",
                        repository.destination
                    ));
                })
                .ok()
        })
        .collect::<BTreeMap<_, _>>();
    if repositories.is_empty() {
        return None;
    }
    let cache_destination = Path::new(CACHE_CONTAINER_ROOT);
    if mounts.iter().any(|mount| {
        mount.destination.starts_with(cache_destination)
            || cache_destination.starts_with(&mount.destination)
    }) {
        executor.notify_notice(
            "Clone cache disabled because an attached directory overlaps /run/hel/git-cache.",
        );
        return None;
    }

    let _stage = ProvisionStageGuard::new(executor, ProvisionStage::Cloning);
    let home = match cache_home(&host, executor) {
        Ok(home) => home,
        Err(error) => {
            executor.notify_notice(&format!(
                "Clone cache unavailable on the container host: {error:#}; using direct clones."
            ));
            return None;
        }
    };
    let cache_root = home.join(CACHE_RELATIVE_ROOT);
    let session_root = cache_root.join("sessions").join(session_id);

    let prepared = std::thread::scope(|scope| {
        let handles = repositories
            .iter()
            .map(|(key, (source, cutoff))| {
                let snapshot = session_root.join(snapshot_name(key, cutoff.as_ref()));
                let key = key.clone();
                let host = &host;
                let cache_root = &cache_root;
                scope.spawn(move || {
                    prepare_repository(
                        host,
                        cache_root,
                        &key,
                        source,
                        &snapshot,
                        cutoff.as_ref(),
                        github_token,
                        executor,
                    )
                    .map(|()| key)
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|panic| {
                    Err(anyhow::anyhow!(
                        "clone-cache preparation thread panicked: {}",
                        targets::command_thread_panic_message(panic.as_ref())
                    ))
                })
            })
            .collect::<Vec<_>>()
    });

    let mut references = BTreeMap::new();
    for result in prepared {
        match result {
            Ok(key) => {
                let name = snapshot_name(&key, repositories[&key].1.as_ref());
                references.insert(key, format!("{CACHE_CONTAINER_ROOT}/{name}"));
            }
            Err(error) => executor.notify_notice(&format!(
                "Clone cache preparation failed: {error:#}; using a direct clone for that repository."
            )),
        }
    }
    if references.is_empty() {
        let _ = cleanup_session_root(&host, &session_root, executor);
        return None;
    }
    for repository in &mut bundle.repositories {
        let Some(source) = repository.url.as_deref() else {
            continue;
        };
        let Ok(key) = repository_cache_key(source) else {
            continue;
        };
        repository.reference = references.get(&key).cloned();
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
    Some(PreparedCloneCache { host, session_root })
}

/// A session snapshot's file name: a bare repository, or for a cutoff the
/// bundle a checkout without later history fetches from.
fn snapshot_name(key: &str, cutoff: Option<&HistoryCutoff>) -> String {
    match cutoff {
        Some(_) => format!("{key}.bundle"),
        None => format!("{key}.git"),
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

fn repository_cache_key(source: &str) -> Result<String> {
    let repository = crate::setup::github_repository_from_origin(source)
        .context("repository is not a recognized GitHub URL")?;
    let identity = format!(
        "github.com/{}/{}",
        repository.owner.to_ascii_lowercase(),
        repository.repository.to_ascii_lowercase()
    );
    Ok(lower_hex(Sha256::digest(identity.as_bytes())))
}

const PREPARE_REPOSITORY_SCRIPT: &str = r#"
set -eu
cache_root=$1
key=$2
source=$3
snapshot=$4
has_token=$5
# Any further arguments are a history cutoff: its head, then its base.
shift 5
umask 077
mkdir -p "$cache_root/mirrors" "$cache_root/locks" "$(dirname "$snapshot")"
entry="$cache_root/mirrors/$key"
mirror="$entry/repo.git"
# An overlapping refresher satisfies this request too. The locked read below
# is authoritative; this optimistic read may race eviction or publication.
observed_generation=$(cat "$entry/generation" 2>/dev/null || :)
lock="$cache_root/locks/$key"
partial=$(mktemp -d "$cache_root/mirrors/$key.partial-XXXXXX")
snapshot_partial="$snapshot.partial-$$"
pinned="$snapshot.pinned-$$"
cutoff_repository="$snapshot.cutoff-$$"
locked=0
cleanup() {
    if [ "$locked" -eq 1 ]; then
        rmdir "$lock"
    fi
    rm -rf -- "$partial" "$snapshot_partial" "$pinned" "$cutoff_repository"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
attempt=0
while ! mkdir "$lock" 2>/dev/null; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 1500 ]; then
        echo "timed out waiting for clone-cache lock $key" >&2
        exit 75
    fi
    sleep 0.2
done
locked=1
if [ "$has_token" = 1 ]; then
    IFS= read -r GH_TOKEN || exit 1
    export GH_TOKEN
fi
export GIT_TERMINAL_PROMPT=0
export GIT_SSH_COMMAND='ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=15'
git_remote() {
    if [ "$has_token" = 1 ]; then
        git -c credential.helper= -c 'credential.helper=!f() { if [ "$1" = get ]; then printf "%s\n" username=x-access-token "password=$GH_TOKEN"; fi; }; f' "$@"
    else
        git "$@"
    fi
}
generation=$(cat "$entry/generation" 2>/dev/null || :)
if [ ! -d "$mirror" ]; then
    rm -rf -- "$entry"
    git_remote clone --mirror -- "$source" "$partial/repo.git"
    git -C "$partial/repo.git" config gc.auto 0
    printf '%s\n' "${partial##*/}" > "$partial/generation"
    mv -- "$partial" "$entry"
elif [ -z "$generation" ] || [ "$generation" = "$observed_generation" ]; then
    git_remote -C "$mirror" remote update --prune
    printf '%s\n' "${partial##*/}" > "$entry/generation"
fi
: > "$entry/last-used"
# Own every object and ref before releasing the mirror to refresh or eviction.
# Git hardlinks immutable objects; this private repository needs no shared lock
# while packing a cutoff bundle or making the mounted snapshot readable.
git clone --mirror --local -- "$mirror" "$pinned"
locked=0
rmdir "$lock"
rm -rf -- "$snapshot" "$snapshot_partial"
if [ "$#" -eq 0 ]; then
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
    github_token: Option<&str>,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let token = github_token.is_some();
    let mut arguments = vec![
        cache_root.to_string_lossy().into_owned(),
        key.to_owned(),
        source.to_owned(),
        snapshot.to_string_lossy().into_owned(),
        u8::from(token).to_string(),
    ];
    if let Some(cutoff) = cutoff {
        cutoff.validate()?;
        arguments.extend(cutoff.commits().map(str::to_owned));
    }
    let mut command = host.git_shell_command(
        PREPARE_REPOSITORY_SCRIPT,
        arguments,
        format!("prepare cached Git objects for {source}"),
    );
    if let Some(token) = github_token {
        let mut input = token.as_bytes().to_vec();
        input.push(b'\n');
        command = command.with_sensitive_stdin(input);
    }
    let output = executor.execute(&command)?;
    checked(output, &command).map(|_| ())
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
find "$root/locks" -mindepth 1 -maxdepth 1 -type d -mmin +1440 -exec rm -rf {} \;
gc_lock="$root/locks/gc"
mkdir "$gc_lock" 2>/dev/null || exit 0
candidates="$root/gc-candidates-$$"
trap 'rm -f -- "$candidates"; rm -rf -- "$gc_lock"' EXIT HUP INT TERM
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
    if [ "$live" -eq 0 ] && find "$snapshot" -mmin +1440 -print | grep -q .; then
        rm -rf -- "$snapshot"
    fi
done
for entry in "$root"/mirrors/*; do
    [ -d "$entry" ] || continue
    key=${entry##*/}
    [ -f "$entry/last-used" ] || continue
    if find "$entry/last-used" -mmin +43200 -print | grep -q . && mkdir "$root/locks/$key" 2>/dev/null; then
        rm -rf -- "$entry" "$root/locks/$key"
    fi
done
size=$(du -sk "$root/mirrors" | awk '{print $1}')
if [ "$size" -gt "$limit_kib" ]; then
    ls -1tr "$root"/mirrors/*/last-used > "$candidates" 2>/dev/null || true
    while IFS= read -r used; do
        [ -n "$used" ] || continue
        entry=${used%/last-used}
        key=${entry##*/}
        if mkdir "$root/locks/$key" 2>/dev/null; then
            rm -rf -- "$entry" "$root/locks/$key"
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

    #[test]
    fn unavailable_host_git_falls_back_without_mutating_the_bundle() {
        let executor = RecordingExecutor {
            status: 127,
            ..Default::default()
        };
        let target = TargetTemplate::LocalPodman(ContainerTemplate {
            build_cache: None,
            image: "ubuntu:24.04".to_owned(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: Vec::new(),
            workspace_storage: Default::default(),
        });
        let mut bundle = ProjectBundleSpec {
            primary: "app".to_owned(),
            repositories: vec![RepositorySpec {
                url: Some("https://github.com/example/app.git".to_owned()),
                push_urls: Vec::new(),
                destination: "app".to_owned(),
                git_ref: None,
                reference: None,
                history_cutoff: None,
            }],
        };
        let mut mounts = Vec::new();

        assert!(
            prepare(
                &target,
                "session-12345678",
                &mut bundle,
                &mut mounts,
                None,
                &executor
            )
            .is_none()
        );
        assert_eq!(bundle.repositories[0].reference, None);
        assert!(mounts.is_empty());
        assert!(executor.notices.lock().unwrap()[0].contains("using direct clones"));
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

        prepare_repository(
            &CacheHost::Local,
            &cache_root,
            "repository",
            source.to_str().unwrap(),
            &snapshot,
            None,
            None,
            &ProcessExecutor,
        )
        .unwrap();

        let mirror = cache_root.join("mirrors/repository/repo.git");
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

        let command = CommandSpec::new(
            "git",
            [
                "-C",
                snapshot.to_str().unwrap(),
                "rev-parse",
                "--verify",
                "HEAD",
            ],
        )
        .purpose("verify cached snapshot");
        checked(ProcessExecutor.execute(&command).unwrap(), &command).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_cutoff_snapshot_is_a_bundle_of_the_cutoff_history_alone() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let cache_root = temporary.path().join("cache");
        let snapshot = cache_root.join("sessions/session-12345678/repository.bundle");
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

        prepare_repository(
            &CacheHost::Local,
            &cache_root,
            "repository",
            source.to_str().unwrap(),
            &snapshot,
            Some(&cutoff),
            None,
            &ProcessExecutor,
        )
        .unwrap();

        assert!(snapshot.is_file());
        assert_eq!(
            std::fs::read_dir(snapshot.parent().unwrap())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            ["repository.bundle"]
        );
        assert_eq!(
            git(
                temporary.path(),
                &["bundle", "list-heads", snapshot.to_str().unwrap()]
            ),
            format!("{head} refs/mj/head\n{base} refs/mj/base")
        );
        // The mirror keeps the full history on the host; only the bundle is
        // mounted.
        let mirror = cache_root.join("mirrors/repository/repo.git");
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

        // A cutoff the mirror cannot serve fails, so the session falls back
        // to the network instead of mounting anything.
        let missing = HistoryCutoff {
            head: "1".repeat(40),
            base: None,
        };
        assert!(
            prepare_repository(
                &CacheHost::Local,
                &cache_root,
                "repository",
                source.to_str().unwrap(),
                &snapshot,
                Some(&missing),
                None,
                &ProcessExecutor,
            )
            .is_err()
        );
        assert!(!snapshot.exists());
    }

    // Hard-won: #1388: packing one session's history blocked every clone of the same repository.
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
   { [ "$1" = -C ] && [ "$3" = remote ] && [ "$4" = update ]; }; then
    echo refresh >> "$CACHE_TEST_ROOT/refreshes"
    : > "$CACHE_TEST_ROOT/refresh-started"
    wait_for "$CACHE_TEST_ROOT/release-refresh"
fi
if [ "$1" = -C ] && [ "$3" = fetch ]; then
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
   [ "${CACHE_TEST_WAITER:-0}" = 1 ]; then
    : > "$CACHE_TEST_ROOT/waiter-started"
fi
exec "$CACHE_TEST_MKDIR" "$@"
"#;
        for (name, script) in [("git", git_wrapper), ("mkdir", mkdir_wrapper)] {
            let path = wrappers.join(name);
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        struct GitExecutor(BTreeMap<String, String>);
        impl CommandExecutor for GitExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                let mut command = command.clone();
                command.env.extend(self.0.clone());
                ProcessExecutor.execute(&command)
            }
        }
        let environment = BTreeMap::from([
            (
                "PATH".into(),
                format!("{}:{}", wrappers.display(), std::env::var("PATH").unwrap()),
            ),
            ("CACHE_TEST_ROOT".into(), root.display().to_string()),
            ("CACHE_TEST_GIT".into(), real_git),
            ("CACHE_TEST_MKDIR".into(), real_mkdir),
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
    }

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
    }

    #[test]
    #[cfg_attr(windows, ignore = "#1297: runs sh, du or touch on the controller")]
    fn cache_gc_removes_only_old_orphan_session_snapshots() {
        let temporary = tempfile::tempdir().unwrap();
        let cache_root = temporary.path().join("cache");
        let orphan = cache_root.join("sessions/orphan-session");
        let live = cache_root.join("sessions/live-session");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::create_dir_all(&live).unwrap();
        for path in [&orphan, &live] {
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
