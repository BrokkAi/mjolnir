//! Local bare sessions that an earlier release started from the profile home
//! itself.
//!
//! Before this release, a local bare session of most harnesses ran straight out
//! of the person's profile home, such as `~/.codex`. Every session now runs from
//! a staged home at `<worker root>/profile`, and the controller derives every
//! path it uses (the launch configuration, a checkpoint's harness home, the
//! project-memory replica) from that one rule. A session an earlier release
//! started is still running from the profile home, with its login, its native
//! transcript and its project-memory replica there.
//!
//! So that the rule still holds for such a session, daemon start puts a
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

use std::path::{Path, PathBuf};

use mj_core::config::HarnessKind;
use mj_core::state::{State, TargetLocator};

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
}
