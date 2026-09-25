//! What earlier releases left in the profile homes on this machine.
//!
//! Before this release, a local bare session of most harnesses ran straight out
//! of the person's profile home, such as `~/.codex`. Every session now runs from
//! a staged home at `<worker root>/profile`, and the controller derives every
//! path it uses (the launch configuration, a checkpoint's harness home, the
//! project-memory replica) from that one rule. Daemon start deals with two
//! kinds of leftovers from the old way.
//!
//! **Sessions still running from a profile home.** A session an earlier
//! release started is still running from the profile home, with its login, its
//! native transcript and its project-memory replica there. So that the rule
//! still holds for such a session, daemon start puts a
//! symbolic link at the staged path that points at the home the installed
//! worker was started with. A restart with a refreshed launch configuration
//! and a checkpoint then find the files the harness is using. Nothing is copied
//! into or out of the profile home.
//!
//! Two writers must never go through the link: installing worker files, which
//! would copy a stage over the person's own configuration, and the skills
//! sync, which would replace their skills tree. Installing replaces the link
//! with a new directory, and the credential sync leaves a session alone while
//! its staged home is a link. The link goes when the worker root does, at
//! close or when the harness is replaced in place; a resume then stages the
//! session like any other.
//!
//! **Replicas of ended sessions.** Such a session kept Mjolnir's replica of its
//! project memory at `<profile home>/projects/hel-<key>-<session>/`, and
//! closing the session left it there. Daemon start removes the ones whose
//! session has ended ([`remove_replicas_of_ended_sessions`]).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use mj_core::config::HarnessKind;
use mj_core::state::{State, TargetLocator};

use crate::targets::{CommandExecutor, CommandSpec};

/// A session whose staged home was linked to the profile home its worker runs
/// from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedProfileHome {
    pub session_id: String,
    /// The staged path, now a symbolic link.
    pub staged_home: PathBuf,
    /// The profile home the installed worker was started with.
    pub profile_home: PathBuf,
}

/// Link the staged home of every local bare session whose installed worker an
/// earlier release started from a profile home. Returns what was linked; a
/// session that already has a staged home, or has no installed worker, is left
/// alone.
pub fn link_profile_homes_of_earlier_sessions(state: &State) -> Vec<LinkedProfileHome> {
    let mut linked = Vec::new();
    for session in state.sessions.values() {
        let Some(TargetLocator::LocalBare { worker_root }) = &session.target else {
            continue;
        };
        // Muse always ran from a per-session root under the data directory.
        if session.harness_kind == HarnessKind::Muse {
            continue;
        }
        match link_profile_home(worker_root) {
            Ok(Some(profile_home)) => {
                tracing::info!(
                    session_id = %session.id,
                    profile_home = %profile_home.display(),
                    "this session's worker was started by an earlier release from the profile \
                     home; its staged home now links there until the session is next staged"
                );
                linked.push(LinkedProfileHome {
                    session_id: session.id.clone(),
                    staged_home: worker_root.join("profile"),
                    profile_home,
                });
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(
                session_id = %session.id,
                "could not link the staged home of a session an earlier release started: {error:#}"
            ),
        }
    }
    linked
}

/// Link `<worker_root>/profile` to the home the installed worker was started
/// with, when that home is not inside the worker root.
fn link_profile_home(worker_root: &Path) -> anyhow::Result<Option<PathBuf>> {
    let staged_home = worker_root.join("profile");
    match std::fs::symlink_metadata(&staged_home) {
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let Some(installed_home) = installed_harness_home(&worker_root.join("launch.json"))? else {
        return Ok(None);
    };
    if installed_home.starts_with(worker_root) || !installed_home.is_dir() {
        return Ok(None);
    }
    link_directory(&installed_home, &staged_home)?;
    Ok(Some(installed_home))
}

/// The harness home an installed launch configuration names, or `None` when
/// there is no installed configuration.
///
/// Read loosely, as JSON, because it was written by an earlier release: only
/// the fields that locate the home matter here. A configuration that states no
/// home was started with the harness's home variable.
fn installed_harness_home(launch_path: &Path) -> anyhow::Result<Option<PathBuf>> {
    let body = match std::fs::read(launch_path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let launch: serde_json::Value = serde_json::from_slice(&body)?;
    let stated = launch
        .get("harness_home")
        .and_then(serde_json::Value::as_str)
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    if stated.is_some() {
        return Ok(stated);
    }
    let Some(harness) = launch
        .get("harness")
        .cloned()
        .and_then(|harness| serde_json::from_value::<HarnessKind>(harness).ok())
    else {
        return Ok(None);
    };
    Ok(launch
        .get("environment")
        .and_then(|environment| environment.get(harness.home_env()))
        .and_then(serde_json::Value::as_str)
        .map(|home| harness.home_from_environment(home)))
}

#[cfg(unix)]
fn link_directory(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn link_directory(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "linking a staged home needs a Unix file system",
    ))
}

/// Whether a session's staged home is a directory of its own, which the
/// credential sync may write into.
///
/// Every target but this machine stages a home for each session, and so does
/// this release on this machine. A local session is left out while its staged
/// home is a link to a profile home, or missing because an earlier release
/// started its worker from a profile home that could not be linked.
pub fn session_has_a_staged_home_of_its_own(session: &mj_core::state::SessionRecord) -> bool {
    let Some(TargetLocator::LocalBare { worker_root }) = &session.target else {
        return true;
    };
    if session.harness_kind == HarnessKind::Muse {
        return true;
    }
    std::fs::symlink_metadata(worker_root.join("profile")).is_ok_and(|metadata| metadata.is_dir())
}

/// A project-memory replica an earlier release left in a profile home, as the
/// daemon found and removed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedReplica {
    pub session_id: String,
    /// The `projects/hel-<key>-<session>` directory.
    pub directory: PathBuf,
    /// Whether the directory itself is gone. A Claude Code project directory
    /// also holds the session's native transcripts, which stay where they are.
    pub removed_directory: bool,
}

/// The entries Mjolnir itself writes into a replica directory.
const REPLICA_ENTRIES: [&str; 2] = ["memory", ".hel-memory-baseline"];

/// Remove the project-memory replicas that earlier releases left in profile
/// homes for sessions that have ended.
///
/// A directory is left alone while its session is in `live_sessions`, this
/// instance's store, or while a running process names the session in its
/// arguments. A local worker's arguments carry its worker root, which ends in
/// the session id, so the second rule spares the running sessions of another
/// Mjolnir instance that shares the profile home and keeps its own store. Only
/// Mjolnir's own entries, `memory/` and `.hel-memory-baseline/`, are removed;
/// the directory goes too when nothing else is left in it.
pub fn remove_replicas_of_ended_sessions(
    homes: impl IntoIterator<Item = PathBuf>,
    live_sessions: &BTreeSet<String>,
    running_process_arguments: &[String],
) -> Vec<RemovedReplica> {
    let mut removed = Vec::new();
    let mut seen = BTreeSet::new();
    for home in homes {
        if !seen.insert(home.clone()) {
            continue;
        }
        let projects = home.join("projects");
        let entries = match std::fs::read_dir(&projects) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                tracing::warn!(
                    directory = %projects.display(),
                    "could not look for leftover project-memory replicas: {error}"
                );
                continue;
            }
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let Some(session_id) = name.to_str().and_then(replica_session_id) else {
                continue;
            };
            if live_sessions.contains(session_id)
                || running_process_arguments
                    .iter()
                    .any(|arguments| arguments.contains(session_id))
            {
                continue;
            }
            let directory = entry.path();
            // Only a real directory is entered; a link is never followed.
            if !std::fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.is_dir()) {
                continue;
            }
            match remove_replica(&directory) {
                // A Claude project directory whose replica went at an earlier
                // start: only the session's native transcripts are left.
                Ok(None) => {}
                Ok(Some(removed_directory)) => {
                    tracing::info!(
                        session_id,
                        directory = %directory.display(),
                        removed_directory,
                        "removed the project-memory replica an earlier release left for an ended session"
                    );
                    removed.push(RemovedReplica {
                        session_id: session_id.to_owned(),
                        directory,
                        removed_directory,
                    });
                }
                Err(error) => tracing::warn!(
                    session_id,
                    directory = %directory.display(),
                    "could not remove a leftover project-memory replica: {error}"
                ),
            }
        }
    }
    removed
}

/// The session id in a replica directory name, `hel-<16 hex>-<32 hex>`, the
/// only shape the project-memory replica slug takes.
fn replica_session_id(name: &str) -> Option<&str> {
    let (key, session_id) = name.strip_prefix("hel-")?.split_once('-')?;
    let lower_hex = |text: &str, length: usize| {
        text.len() == length
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    (lower_hex(key, 16) && lower_hex(session_id, 32)).then_some(session_id)
}

/// Remove Mjolnir's entries from one replica directory, then the directory
/// itself when nothing else is in it. Returns whether the directory is gone,
/// or `None` when nothing was removed.
fn remove_replica(directory: &Path) -> std::io::Result<Option<bool>> {
    let mut removed_entry = false;
    for name in REPLICA_ENTRIES {
        let path = directory.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(&path)?,
            Ok(_) => std::fs::remove_file(&path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        removed_entry = true;
    }
    match std::fs::remove_dir(directory) {
        Ok(()) => Ok(Some(true)),
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
            Ok(removed_entry.then_some(false))
        }
        Err(error) => Err(error),
    }
}

/// The argument lists of every process running on this machine, or `None`
/// when they cannot be listed. The same `ps` options work on Linux and macOS,
/// and `-ww` keeps long arguments whole.
pub fn running_process_arguments(executor: &impl CommandExecutor) -> Option<Vec<String>> {
    let command = CommandSpec::new("ps", ["-A", "-ww", "-o", "args="])
        .purpose("list running processes before removing leftover project-memory replicas");
    match executor.execute(&command) {
        Ok(output) if output.status == 0 => Some(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::to_owned)
                .collect(),
        ),
        Ok(output) => {
            tracing::warn!(
                status = output.status,
                "could not list running processes: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            None
        }
        Err(error) => {
            tracing::warn!("could not list running processes: {error:#}");
            None
        }
    }
}

/// Daemon start's cleanup of replicas that earlier releases left in the homes
/// of the configured profiles. Nothing is removed when the running processes
/// cannot be listed, because a live session of another instance could not be
/// told apart from an ended one.
pub fn remove_replicas_left_in_profile_homes(
    config: &mj_core::config::Config,
    state: &State,
    executor: &impl CommandExecutor,
) -> Vec<RemovedReplica> {
    let Some(running) = running_process_arguments(executor) else {
        tracing::warn!("left earlier releases' project-memory replicas in place");
        return Vec::new();
    };
    let live_sessions = state.sessions.keys().cloned().collect::<BTreeSet<_>>();
    let removed = remove_replicas_of_ended_sessions(
        config.profiles.values().map(|profile| profile.home.clone()),
        &live_sessions,
        &running,
    );
    if !removed.is_empty() {
        tracing::info!(
            count = removed.len(),
            "removed project-memory replicas that earlier releases left in profile homes"
        );
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(
        id: &str,
        harness: HarnessKind,
        worker_root: &Path,
    ) -> mj_core::state::SessionRecord {
        let mut session = crate::controller::test_support::checkpoint_test_session(id);
        session.harness_kind = harness;
        session.target = Some(TargetLocator::LocalBare {
            worker_root: worker_root.to_path_buf(),
        });
        session
    }

    fn install_launch(worker_root: &Path, launch: serde_json::Value) {
        std::fs::create_dir_all(worker_root).unwrap();
        std::fs::write(
            worker_root.join("launch.json"),
            serde_json::to_vec(&launch).unwrap(),
        )
        .unwrap();
    }

    /// A Codex session an earlier release started from `~/.codex` keeps
    /// running from it: its staged home links there, so a relaunch and a
    /// checkpoint find its login and its native transcript.
    #[cfg(unix)]
    #[test]
    fn an_earlier_session_s_staged_home_links_to_the_home_its_worker_uses() {
        let directory = tempfile::tempdir().unwrap();
        let profile_home = directory.path().join(".codex");
        std::fs::create_dir_all(profile_home.join("sessions")).unwrap();
        std::fs::write(profile_home.join("auth.json"), "{}").unwrap();
        let earlier = directory.path().join("workers/earlier");
        install_launch(
            &earlier,
            serde_json::json!({
                "harness": "codex",
                "harness_home": profile_home,
                "environment": {},
            }),
        );
        // An older configuration stated the home only through the variable.
        let older = directory.path().join("workers/older");
        install_launch(
            &older,
            serde_json::json!({
                "harness": "codex",
                "environment": { "CODEX_HOME": profile_home },
            }),
        );
        // A session this release staged already has a home of its own.
        let staged = directory.path().join("workers/staged");
        std::fs::create_dir_all(staged.join("profile")).unwrap();
        install_launch(
            &staged,
            serde_json::json!({
                "harness": "codex",
                "harness_home": staged.join("profile"),
                "environment": {},
            }),
        );
        // A session still provisioning has no installed worker yet.
        let provisioning = directory.path().join("workers/provisioning");
        std::fs::create_dir_all(&provisioning).unwrap();
        let mut state = State::default();
        for (id, root) in [
            ("earlier", &earlier),
            ("older", &older),
            ("staged", &staged),
            ("provisioning", &provisioning),
        ] {
            state
                .sessions
                .insert(id.to_owned(), session(id, HarnessKind::Codex, root));
        }

        let linked = link_profile_homes_of_earlier_sessions(&state);

        assert_eq!(
            linked
                .iter()
                .map(|link| link.session_id.as_str())
                .collect::<Vec<_>>(),
            ["earlier", "older"]
        );
        for id in ["earlier", "older"] {
            let TargetLocator::LocalBare { worker_root } =
                state.sessions[id].target.as_ref().unwrap()
            else {
                unreachable!()
            };
            let link = worker_root.join("profile");
            assert_eq!(std::fs::read_link(&link).unwrap(), profile_home);
            assert!(link.join("auth.json").is_file());
            // The credential sync must not push through the link.
            assert!(!session_has_a_staged_home_of_its_own(&state.sessions[id]));
        }
        assert!(staged.join("profile").is_dir());
        assert!(session_has_a_staged_home_of_its_own(
            &state.sessions["staged"]
        ));
        assert!(!provisioning.join("profile").exists());

        // A second daemon start finds the links in place and changes nothing.
        assert!(link_profile_homes_of_earlier_sessions(&state).is_empty());
    }

    const KEY: &str = "d4ef94e0b5b8a9f4";

    fn replica(home: &Path, session_id: &str) -> PathBuf {
        let directory = home
            .join("projects")
            .join(format!("hel-{KEY}-{session_id}"));
        for entry in REPLICA_ENTRIES {
            std::fs::create_dir_all(directory.join(entry)).unwrap();
            std::fs::write(directory.join(entry).join("MEMORY.md"), "- a fact\n").unwrap();
        }
        directory
    }

    /// Replicas an earlier release left in profile homes are removed once
    /// their session has ended, and only then. Measured on fixture homes: a
    /// Codex home with one replica per case, and a Claude home whose replica
    /// directory also holds the session's native transcript.
    #[test]
    fn replicas_of_ended_sessions_are_removed_and_every_other_directory_is_kept() {
        let directory = tempfile::tempdir().unwrap();
        let codex = directory.path().join(".codex");
        let claude = directory.path().join(".claude");
        let stored = "0146ce088c16780675b6523eeab52218";
        let running_elsewhere = "0858ecafaa06ecd148aa8bcf70bba899";
        let ended = "189221e5da4a3e6ede59b26a668d55d4";
        let ended_claude = "5a74db9c320cfb00d9f27fc092adf16b";
        let stored_replica = replica(&codex, stored);
        let running_replica = replica(&codex, running_elsewhere);
        let ended_replica = replica(&codex, ended);
        let claude_replica = replica(&claude, ended_claude);
        std::fs::write(claude_replica.join("native.jsonl"), "{}\n").unwrap();
        // Directories that are not replicas: the harness's own project
        // directory, and names that only look like a replica.
        for other in [
            codex.join("projects/-home-me-app/native.jsonl"),
            codex.join(format!("projects/hel-{KEY}-not-a-session/memory/MEMORY.md")),
            codex.join(format!("projects/hel-{KEY}/memory/MEMORY.md")),
        ] {
            std::fs::create_dir_all(other.parent().unwrap()).unwrap();
            std::fs::write(&other, "kept").unwrap();
        }
        let live = BTreeSet::from([stored.to_owned()]);
        // Another Mjolnir instance's worker for a session this store does not
        // know, as `ps` shows it.
        let running = vec![format!(
            "/home/me/.local/share/mj-lab/workers/{running_elsewhere}/hel worker run --root \
             /home/me/.local/share/mj-lab/workers/{running_elsewhere}"
        )];

        let removed = remove_replicas_of_ended_sessions(
            [codex.clone(), claude.clone(), codex.clone()],
            &live,
            &running,
        );

        assert_eq!(
            removed,
            vec![
                RemovedReplica {
                    session_id: ended.to_owned(),
                    directory: ended_replica.clone(),
                    removed_directory: true,
                },
                RemovedReplica {
                    session_id: ended_claude.to_owned(),
                    directory: claude_replica.clone(),
                    removed_directory: false,
                },
            ]
        );
        assert!(!ended_replica.exists());
        assert!(stored_replica.join("memory/MEMORY.md").is_file());
        assert!(running_replica.join("memory/MEMORY.md").is_file());
        // The Claude transcript stays; only Mjolnir's entries went.
        assert!(claude_replica.join("native.jsonl").is_file());
        assert!(!claude_replica.join("memory").exists());
        assert!(!claude_replica.join(".hel-memory-baseline").exists());
        assert!(codex.join("projects/-home-me-app/native.jsonl").is_file());
        assert!(
            codex
                .join(format!("projects/hel-{KEY}-not-a-session/memory/MEMORY.md"))
                .is_file()
        );
        assert!(
            codex
                .join(format!("projects/hel-{KEY}/memory/MEMORY.md"))
                .is_file()
        );

        // A second daemon start finds nothing more to remove.
        assert!(remove_replicas_of_ended_sessions([codex, claude], &live, &running).is_empty());
    }

    /// When the running processes cannot be listed, a live session of another
    /// instance cannot be told from an ended one, so nothing is removed.
    #[test]
    fn nothing_is_removed_when_running_processes_cannot_be_listed() {
        struct FailingPs;
        impl CommandExecutor for FailingPs {
            fn execute(
                &self,
                _command: &CommandSpec,
            ) -> anyhow::Result<crate::targets::CommandOutput> {
                anyhow::bail!("ps is not installed")
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join(".codex");
        let ended = replica(&home, "189221e5da4a3e6ede59b26a668d55d4");
        let mut config = mj_core::config::Config::default();
        config.profiles.insert(
            "codex".into(),
            mj_core::config::HarnessProfile {
                enabled: true,
                kind: HarnessKind::Codex,
                home,
                environment: Default::default(),
                context_window_bytes: None,
                guardian_review_model: None,
            },
        );

        assert!(
            remove_replicas_left_in_profile_homes(&config, &State::default(), &FailingPs)
                .is_empty()
        );
        assert!(ended.join("memory/MEMORY.md").is_file());
    }
}
