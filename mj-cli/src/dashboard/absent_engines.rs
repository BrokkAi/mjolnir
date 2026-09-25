//! Local container targets whose engine is not installed on this host.
//!
//! The Targets pane checks a failed target again every minute, and each check
//! of a target whose engine is not installed logged a warning: 13 in 12
//! minutes for the built-in `docker` target on a host without Docker (launch
//! finding R8-6). An engine that is not installed is a fact about the host,
//! not a failure. It is logged once, at info, and the target is not checked
//! again until its configuration changes or the engine's command appears on
//! PATH.

use std::collections::BTreeMap;
use std::ffi::OsStr;

use mj_controller::targets::{local_engine_command, program_on_path};
use mj_core::config::TargetTemplate;

#[derive(Debug, Default)]
pub(crate) struct AbsentEngines {
    targets: BTreeMap<String, AbsentEngine>,
}

#[derive(Debug)]
struct AbsentEngine {
    template: TargetTemplate,
    engine: &'static str,
    message: String,
}

impl AbsentEngines {
    /// The recorded answer for `target_id`, while it is still configured as
    /// `template` and its engine's command is still not on `path`. Otherwise
    /// the record is forgotten and `None` says to run the check.
    pub(crate) fn answer(
        &mut self,
        target_id: &str,
        template: Option<&TargetTemplate>,
        path: Option<&OsStr>,
    ) -> Option<String> {
        let absent = self.targets.get(target_id)?;
        if template == Some(&absent.template) && !program_on_path(absent.engine, path) {
            return Some(absent.message.clone());
        }
        self.targets.remove(target_id);
        None
    }

    /// Record a check of `template` that found its engine not installed.
    /// Returns whether this is news, so the caller logs it once.
    pub(crate) fn record(
        &mut self,
        target_id: &str,
        template: &TargetTemplate,
        message: String,
    ) -> bool {
        let Some(engine) = local_engine_command(template) else {
            return false;
        };
        let previous = self.targets.insert(
            target_id.to_owned(),
            AbsentEngine {
                template: template.clone(),
                engine,
                message,
            },
        );
        previous.is_none_or(|previous| previous.template != *template)
    }
}

/// Whether a failed check of `template` failed because its local engine is
/// not installed: the engine's command is not on `path`. The check's own
/// error cannot say so, because the target check rewrites the cause into
/// the sentence the session wizard shows.
pub(crate) fn engine_absent(template: &TargetTemplate, path: Option<&OsStr>) -> bool {
    local_engine_command(template).is_some_and(|engine| !program_on_path(engine, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docker(image: &str) -> TargetTemplate {
        serde_json::from_value(serde_json::json!({"kind": "local-docker", "image": image})).unwrap()
    }

    const NOT_INSTALLED: &str =
        "Docker is not installed on this host. Install Docker or choose another target.";

    /// R8-6: a target whose engine is not installed is checked once, logged
    /// once, and answered from the record until its configuration or the
    /// engine changes.
    #[test]
    fn an_absent_engine_is_not_checked_again_until_its_target_or_the_engine_changes() {
        let empty_path = tempfile::tempdir().unwrap();
        let path = empty_path.path().as_os_str();
        let mut absent = AbsentEngines::default();
        let template = docker("mjolnir:latest");

        // Nothing recorded yet: the check runs.
        assert_eq!(absent.answer("docker", Some(&template), Some(path)), None);
        assert!(absent.record("docker", &template, NOT_INSTALLED.into()));
        assert!(
            !absent.record("docker", &template, NOT_INSTALLED.into()),
            "the same finding is logged once"
        );
        for _ in 0..3 {
            assert_eq!(
                absent
                    .answer("docker", Some(&template), Some(path))
                    .as_deref(),
                Some(NOT_INSTALLED)
            );
        }

        // A changed configuration is checked again, and so is a removed one.
        let changed = docker("mjolnir:next");
        assert_eq!(absent.answer("docker", Some(&changed), Some(path)), None);
        assert_eq!(absent.answer("docker", Some(&template), Some(path)), None);
        assert!(absent.record("docker", &template, NOT_INSTALLED.into()));
        assert_eq!(absent.answer("docker", None, Some(path)), None);

        // So is a target whose engine has been installed since.
        assert!(absent.record("docker", &template, NOT_INSTALLED.into()));
        let installed = tempfile::tempdir().unwrap();
        std::fs::write(installed.path().join("docker"), "").unwrap();
        assert_eq!(
            absent.answer(
                "docker",
                Some(&template),
                Some(installed.path().as_os_str())
            ),
            None
        );
    }

    /// Only a local engine can be absent this way: a target on another host
    /// is recorded nowhere, so its failures keep being checked and reported.
    #[test]
    fn only_local_container_targets_are_recorded() {
        let mut absent = AbsentEngines::default();
        assert!(!absent.record("local", &TargetTemplate::LocalBare, "gone".into()));
        assert_eq!(
            absent.answer("local", Some(&TargetTemplate::LocalBare), None),
            None
        );
        assert!(!engine_absent(&TargetTemplate::LocalBare, None));
    }

    /// A failed check is an absent engine exactly when the engine's command
    /// is not on PATH; a Docker that is installed but stopped is a failure
    /// that keeps being checked and reported.
    #[test]
    fn a_failed_check_is_an_absent_engine_only_when_its_command_is_not_on_path() {
        let template = docker("mjolnir:latest");
        let empty = tempfile::tempdir().unwrap();
        assert!(engine_absent(&template, Some(empty.path().as_os_str())));
        assert!(engine_absent(&template, None));
        let installed = tempfile::tempdir().unwrap();
        std::fs::write(installed.path().join("docker"), "").unwrap();
        assert!(!engine_absent(
            &template,
            Some(installed.path().as_os_str())
        ));
    }
}
