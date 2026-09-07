//! Best-effort host-side Git object caches for container-backed sessions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use hel::hel_targets::{
    self, AdditionalMount, CommandExecutor, CommandOutput, CommandSpec, ProjectBundleSpec,
    ProvisionStage, ProvisionStageGuard, SshTarget,
};

const CACHE_CONTAINER_ROOT: &str = "/run/hel/git-cache";
const CACHE_RELATIVE_ROOT: &str = ".cache/mjolnir/git";
const CACHE_MAX_KIB: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone)]
enum CacheHost {
    LocalPodman,
    LocalDocker,
    Apple,
    SshPodman(SshTarget),
    SshDocker(SshTarget),
}

impl CacheHost {
    fn for_target(target: &hel_targets::TargetTemplate) -> Option<Self> {
        match target {
            hel_targets::TargetTemplate::LocalPodman(_) => Some(Self::LocalPodman),
            hel_targets::TargetTemplate::LocalDocker(_) => Some(Self::LocalDocker),
            hel_targets::TargetTemplate::AppleContainer(_) => Some(Self::Apple),
            hel_targets::TargetTemplate::SshPodman { ssh, .. } => {
                Some(Self::SshPodman(ssh.clone()))
            }
            hel_targets::TargetTemplate::SshDocker { ssh, .. } => {
                Some(Self::SshDocker(ssh.clone()))
            }
            hel_targets::TargetTemplate::LocalBare
            | hel_targets::TargetTemplate::AwsEc2(_)
            | hel_targets::TargetTemplate::SshBare { .. } => None,
        }
    }

    fn command(&self, remote: Vec<String>, purpose: impl Into<String>) -> CommandSpec {
        let command = match self {
            Self::LocalPodman | Self::LocalDocker | Self::Apple => {
                CommandSpec::new(remote[0].clone(), remote[1..].iter().cloned())
            }
            Self::SshPodman(ssh) | Self::SshDocker(ssh) => {
                let mut args = ssh.ssh_args.clone();
                args.push(ssh.destination.clone());
                args.push(hel_targets::join_remote_command(&remote));
                CommandSpec::new("ssh", args)
            }
        };
        command.purpose(purpose).stage(ProvisionStage::Cloning)
    }

    fn shell_command(
        &self,
        script: &str,
        arguments: impl IntoIterator<Item = String>,
        purpose: impl Into<String>,
    ) -> CommandSpec {
        let mut remote = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            script.to_owned(),
            "hel-git-cache".to_owned(),
        ];
        remote.extend(arguments);
        self.command(remote, purpose)
    }

    fn managed_sessions(&self, executor: &impl CommandExecutor) -> Result<Vec<String>> {
        let remote = match self {
            Self::LocalPodman | Self::SshPodman(_) => vec![
                "podman".to_owned(),
                "ps".to_owned(),
                "--all".to_owned(),
                "--filter".to_owned(),
                format!("label={}=true", hel_targets::MANAGED_LABEL),
                "--format".to_owned(),
                "json".to_owned(),
            ],
            Self::LocalDocker => vec![
                "docker".to_owned(),
                "ps".to_owned(),
                "--all".to_owned(),
                "--filter".to_owned(),
                format!("label={}=true", hel_targets::MANAGED_LABEL),
                "--format".to_owned(),
                "json".to_owned(),
            ],
            Self::SshDocker(_) => vec![
                "docker".to_owned(),
                "ps".to_owned(),
                "--all".to_owned(),
                "--filter".to_owned(),
                format!("label={}=true", hel_targets::MANAGED_LABEL),
                "--format".to_owned(),
                "json".to_owned(),
            ],
            Self::Apple => vec![
                "container".to_owned(),
                "list".to_owned(),
                "--all".to_owned(),
                "--format".to_owned(),
                "json".to_owned(),
            ],
        };
        let command = self.command(remote, "find live container Git cache snapshots");
        let output = checked(executor.execute(&command)?, &command)?;
        super::recovery_scan::managed_sessions_from_container_json(&output.stdout)
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
pub(super) fn prepare(
    target: &hel_targets::TargetTemplate,
    session_id: &str,
    bundle: &mut ProjectBundleSpec,
    mounts: &mut Vec<AdditionalMount>,
    github_token: Option<&str>,
    executor: &(impl CommandExecutor + Sync),
) -> Option<PreparedCloneCache> {
    let host = CacheHost::for_target(target)?;
    let repositories = bundle
        .repositories
        .iter()
        .filter_map(|repository| {
            let source = repository.url.as_deref()?;
            repository_cache_key(source)
                .map(|key| (key, source.to_owned()))
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
            .map(|(key, source)| {
                let snapshot = session_root.join(format!("{key}.git"));
                let key = key.clone();
                let source = source.clone();
                let host = &host;
                let cache_root = &cache_root;
                scope.spawn(move || {
                    prepare_repository(
                        host,
                        cache_root,
                        &key,
                        &source,
                        &snapshot,
                        github_token,
                        executor,
                    )
                    .map(|()| (key, snapshot))
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|panic| {
                    Err(anyhow::anyhow!(
                        "clone-cache preparation thread panicked: {}",
                        hel_targets::command_thread_panic_message(panic.as_ref())
                    ))
                })
            })
            .collect::<Vec<_>>()
    });

    let mut references = BTreeMap::new();
    for result in prepared {
        match result {
            Ok((key, _snapshot)) => {
                references.insert(key.clone(), format!("{CACHE_CONTAINER_ROOT}/{key}.git"));
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
        read_only: true,
    });

    let mut live_sessions = match host.managed_sessions(executor) {
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

fn cache_home(host: &CacheHost, executor: &impl CommandExecutor) -> Result<PathBuf> {
    let command = host.shell_command(
        "command -v git >/dev/null 2>&1 || exit 127; printf '%s' \"$HOME\"",
        [],
        "locate the container host Git cache",
    );
    let output = checked(executor.execute(&command)?, &command)?;
    let home = String::from_utf8(output.stdout).context("decode container host home directory")?;
    let home = PathBuf::from(home);
    ensure!(home.is_absolute(), "container host HOME is not absolute");
    Ok(home)
}

fn repository_cache_key(source: &str) -> Result<String> {
    let repository = crate::hel_setup::github_repository_from_origin(source)
        .context("repository is not a recognized GitHub URL")?;
    let identity = format!(
        "github.com/{}/{}",
        repository.owner.to_ascii_lowercase(),
        repository.repository.to_ascii_lowercase()
    );
    Ok(format!("{:x}", Sha256::digest(identity.as_bytes())))
}

const PREPARE_REPOSITORY_SCRIPT: &str = r#"
set -eu
cache_root=$1
key=$2
source=$3
snapshot=$4
has_token=$5
umask 077
mkdir -p "$cache_root/mirrors" "$cache_root/locks" "$(dirname "$snapshot")"
lock="$cache_root/locks/$key"
attempt=0
while ! mkdir "$lock" 2>/dev/null; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 50 ]; then
        echo "timed out waiting for clone-cache lock $key" >&2
        exit 75
    fi
    sleep 0.1
done
partial="$cache_root/mirrors/$key.partial-$$"
snapshot_partial="$snapshot.partial-$$"
cleanup() {
    rm -rf -- "$partial" "$snapshot_partial" "$lock"
}
trap cleanup EXIT HUP INT TERM
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
entry="$cache_root/mirrors/$key"
mirror="$entry/repo.git"
if [ ! -d "$mirror" ]; then
    rm -rf -- "$entry" "$partial"
    mkdir -p "$partial"
    git_remote clone --mirror -- "$source" "$partial/repo.git"
    git -C "$partial/repo.git" config gc.auto 0
    : > "$partial/last-used"
    mv -- "$partial" "$entry"
else
    git_remote -C "$mirror" remote update --prune
    : > "$entry/last-used"
fi
rm -rf -- "$snapshot" "$snapshot_partial"
git clone --mirror --local -- "$mirror" "$snapshot_partial"
mv -- "$snapshot_partial" "$snapshot"
# A bind mount can expose the host owner as container root, while the worker
# runs as an unprivileged user. The private cache parents still protect these
# files on the host; the mounted session subtree must be readable.
chmod -R a+rX "$snapshot"
chmod a+rx "$(dirname "$snapshot")"
trap - EXIT HUP INT TERM
rm -rf -- "$lock"
"#;

fn prepare_repository(
    host: &CacheHost,
    cache_root: &Path,
    key: &str,
    source: &str,
    snapshot: &Path,
    github_token: Option<&str>,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let token = github_token.is_some();
    let mut command = host.shell_command(
        PREPARE_REPOSITORY_SCRIPT,
        [
            cache_root.to_string_lossy().into_owned(),
            key.to_owned(),
            source.to_owned(),
            snapshot.to_string_lossy().into_owned(),
            u8::from(token).to_string(),
        ],
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
    let command = host.shell_command(
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
    let command = host.shell_command(
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

    use hel::hel_config::ImagePullPolicy;
    use hel::hel_targets::{ContainerTemplate, ProcessExecutor, RepositorySpec, TargetTemplate};

    #[test]
    fn github_cache_keys_ignore_transport_and_case() {
        assert_eq!(
            repository_cache_key("git@github.com:BrokkAi/hel.git").unwrap(),
            repository_cache_key("https://github.com/brokkai/HEL.git").unwrap()
        );
    }

    #[test]
    fn unrelated_urls_are_not_cacheable() {
        assert!(repository_cache_key("https://example.com/org/repo.git").is_err());
    }

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
            image: "ubuntu:24.04".to_owned(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: Vec::new(),
            workspace_storage: Default::default(),
        });
        let mut bundle = ProjectBundleSpec {
            primary: "app".to_owned(),
            repositories: vec![RepositorySpec {
                url: Some("https://github.com/example/app.git".to_owned()),
                destination: "app".to_owned(),
                git_ref: None,
                reference: None,
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
            &CacheHost::LocalPodman,
            Path::new("/home/test/.cache/mjolnir/git"),
            "abc123",
            "https://github.com/example/app.git",
            Path::new("/home/test/.cache/mjolnir/git/sessions/session-12345678/abc123.git"),
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
    fn apple_and_ssh_hosts_use_their_native_command_boundaries() {
        let apple = CacheHost::Apple.shell_command("true", [], "probe");
        assert_eq!(apple.program, "sh");

        let ssh = CacheHost::SshPodman(SshTarget {
            destination: "dev@example.test".to_owned(),
            ssh_args: vec!["-o".to_owned(), "BatchMode=yes".to_owned()],
        })
        .shell_command("true", [], "probe");
        assert_eq!(ssh.program, "ssh");
        assert!(ssh.args.last().unwrap().contains("'sh' '-c' 'true'"));
    }

    #[test]
    fn docker_cache_discovers_managed_sessions_through_docker_json_lines() {
        let session_id = "0123456789abcdef0123456789abcdef";
        let executor = RecordingExecutor {
            stdout: format!(
                "{{\"Labels\":\"{}=true,{}={session_id}\"}}\n",
                hel_targets::MANAGED_LABEL,
                hel_targets::SESSION_LABEL
            )
            .into_bytes(),
            ..Default::default()
        };
        let target = TargetTemplate::LocalDocker(ContainerTemplate {
            image: "ubuntu:24.04".to_owned(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: Vec::new(),
            workspace_storage: Default::default(),
        });

        let host = CacheHost::for_target(&target).expect("Docker has a clone-cache host");
        assert_eq!(
            host.managed_sessions(&executor).unwrap(),
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
            &CacheHost::LocalPodman,
            &cache_root,
            "repository",
            source.to_str().unwrap(),
            &snapshot,
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

    #[test]
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
        let limit = directory_kib(&recent) + 4;

        collect_garbage_with_limit(
            &CacheHost::LocalPodman,
            &cache_root,
            &[],
            limit,
            &ProcessExecutor,
        )
        .unwrap();

        assert!(!old.exists());
        assert!(recent.exists());
    }

    #[test]
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
            &CacheHost::LocalPodman,
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
