//! Machines that no runtime runs on yet.
//!
//! A machine is a session target only through its runtimes. The dashboard
//! probes every SSH or EC2 machine the configuration names without one, at
//! startup and whenever the configuration gains such a machine, and the probe
//! writes the runtimes it finds. What a probe found is the one answer every
//! surface gives for why a machine still has no runtime: the Settings
//! Machines page, the session picker, and the notice when the probe ends.

use std::collections::BTreeMap;

use mj_core::config::{Machine, TargetTemplate};

use crate::{DashboardAction, DashboardState, Mode, RejectedRuntime};

/// What probing one machine found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineRuntimeProbe {
    pub machine_id: String,
    /// The machine as it was probed.
    pub machine: Machine,
    pub finding: MachineFinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineFinding {
    /// The machine answered.
    Answered {
        /// The runtimes it can run, under the ids they would like.
        runtimes: Vec<(String, TargetTemplate)>,
        /// The runtimes it cannot run, with why.
        rejected: Vec<RejectedRuntime>,
    },
    /// The machine did not answer, so what it runs is unknown.
    Unreachable { detail: String, remediation: String },
}

/// What one automatic check did: what each machine was found to run, and
/// the runtimes written to the configuration for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineRuntimesChecked {
    pub probes: Vec<MachineRuntimeProbe>,
    pub added: BTreeMap<String, Vec<String>>,
}

/// The dashboard's record of one machine's check.
#[derive(Debug, Clone)]
pub(crate) struct MachineRuntimeCheck {
    /// The machine as it was when the check started. A machine edited since
    /// is checked again.
    machine: Machine,
    generation: u64,
    state: CheckState,
}

#[derive(Debug, Clone)]
enum CheckState {
    Probing,
    Added(Vec<String>),
    Found(MachineFinding),
    Failed(String),
}

impl DashboardState {
    /// Start a check of every machine the configuration names without a
    /// runtime and that has no check of its current definition yet. A check
    /// in flight is never repeated. Settings holds its own draft of the
    /// machines, so nothing starts while it is open; its save is what
    /// brings a new machine here.
    pub fn take_machine_runtime_check(&mut self) -> Option<DashboardAction> {
        if setup_open(&self.mode) {
            return None;
        }
        // A finished check answers only while its machine still has no
        // runtime. Once the configuration shows the runtimes it added, or the
        // machine is gone, it is forgotten, so a machine that loses its
        // runtimes later is probed again.
        let without: std::collections::BTreeSet<String> = self
            .config
            .machines_without_runtimes()
            .map(|(id, _)| id.to_owned())
            .collect();
        self.machine_runtime_checks
            .retain(|id, check| matches!(check.state, CheckState::Probing) || without.contains(id));
        let machines: Vec<(String, Machine)> = self
            .config
            .machines_without_runtimes()
            .filter(|(id, machine)| {
                self.machine_runtime_checks
                    .get(*id)
                    .is_none_or(|check| check.machine != **machine)
            })
            .map(|(id, machine)| (id.to_owned(), machine.clone()))
            .collect();
        if machines.is_empty() {
            return None;
        }
        self.machine_runtime_generation = self.machine_runtime_generation.wrapping_add(1);
        let generation = self.machine_runtime_generation;
        for (id, machine) in &machines {
            self.machine_runtime_checks.insert(
                id.clone(),
                MachineRuntimeCheck {
                    machine: machine.clone(),
                    generation,
                    state: CheckState::Probing,
                },
            );
        }
        Some(DashboardAction::CheckMachineRuntimes {
            generation,
            machines,
        })
    }

    /// Record what a check found and say so. An answer for a machine that
    /// has been checked again since, or edited, is dropped.
    pub fn apply_machine_runtime_check(
        &mut self,
        generation: u64,
        result: Result<MachineRuntimesChecked, String>,
    ) {
        let checked = match result {
            Ok(checked) => checked,
            Err(error) => {
                let mut failed = Vec::new();
                for (id, check) in &mut self.machine_runtime_checks {
                    if check.generation == generation && matches!(check.state, CheckState::Probing)
                    {
                        check.state = CheckState::Failed(error.clone());
                        failed.push(id.clone());
                    }
                }
                if !failed.is_empty() {
                    self.set_failure_notice(format!(
                        "Could not add runtimes for {}: {error}",
                        failed.join(", ")
                    ));
                }
                return;
            }
        };
        // What was added leads; the machines still without a runtime follow.
        let (mut added_sentences, mut problems) = (Vec::new(), Vec::new());
        for probe in checked.probes {
            let Some(check) = self.machine_runtime_checks.get_mut(&probe.machine_id) else {
                continue;
            };
            if check.generation != generation || check.machine != probe.machine {
                continue;
            }
            match checked.added.get(&probe.machine_id) {
                Some(added) if !added.is_empty() => {
                    added_sentences.push(format!(
                        "Added runtimes for {}: {}.",
                        probe.machine_id,
                        added.join(", ")
                    ));
                    check.state = CheckState::Added(added.clone());
                }
                _ => {
                    // A machine that gained a runtime while it was probed
                    // needs no word; one that still has none says why.
                    if self
                        .config
                        .machines_without_runtimes()
                        .any(|(id, _)| id == probe.machine_id)
                    {
                        problems.push(format!(
                            "Machine {} has no runtime: {}",
                            probe.machine_id,
                            finding_reason(&probe.finding)
                        ));
                    }
                    check.state = CheckState::Found(probe.finding);
                }
            }
        }
        added_sentences.append(&mut problems);
        if !added_sentences.is_empty() {
            self.set_notice(added_sentences.join(" "));
        }
    }

    /// Why machine `id`, which no runtime runs on, still has none, or `None`
    /// when no check of its current definition has started.
    pub(crate) fn machine_runtime_reason(&self, id: &str) -> Option<String> {
        let machine = self.config.machines.get(id)?;
        let check = self
            .machine_runtime_checks
            .get(id)
            .filter(|check| check.machine == *machine)?;
        Some(match &check.state {
            CheckState::Probing => "checking what it can run…".to_owned(),
            CheckState::Added(added) => format!("adding {}…", added.join(", ")),
            CheckState::Found(finding) => finding_reason(finding),
            CheckState::Failed(error) => format!("could not add its runtimes: {error}"),
        })
    }

    /// Forget every finished check, so the next pass probes each machine
    /// without a runtime again. A check in flight keeps its place.
    pub(crate) fn recheck_machine_runtimes(&mut self) {
        self.machine_runtime_checks
            .retain(|_, check| matches!(check.state, CheckState::Probing));
    }
}

fn setup_open(mode: &Mode) -> bool {
    match mode {
        Mode::Setup(_) => true,
        Mode::Help(overlay) => setup_open(&overlay.return_to),
        _ => false,
    }
}

/// Why a machine has no runtime, from what probing it found: the reason it
/// did not answer, or each runtime it cannot run. A finding with usable
/// runtimes only arises when they could not be added.
pub(crate) fn finding_reason(finding: &MachineFinding) -> String {
    match finding {
        MachineFinding::Unreachable {
            detail,
            remediation,
        } => sentence(
            &format!("it did not answer. {}", detail.trim()),
            remediation,
        ),
        MachineFinding::Answered { runtimes, rejected } if runtimes.is_empty() => {
            let mut reason = "it can run none.".to_owned();
            for runtime in rejected {
                reason.push(' ');
                reason.push_str(&sentence(
                    &format!("{}: {}", runtime.label, runtime.detail.trim()),
                    runtime.remediation.as_deref().unwrap_or_default(),
                ));
            }
            reason
        }
        MachineFinding::Answered { runtimes, .. } => format!(
            "it can run {}; choose Detect runtimes in Settings › Runtimes to add them.",
            runtimes
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `text` as a sentence, followed by `then` as another.
fn sentence(text: &str, then: &str) -> String {
    let mut sentence = text.trim().to_owned();
    if !sentence.ends_with('.') {
        sentence.push('.');
    }
    let then = then.trim();
    if !then.is_empty() {
        sentence.push(' ');
        sentence.push_str(then);
        if !sentence.ends_with('.') {
            sentence.push('.');
        }
    }
    sentence
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mj_core::config::{Config, DEFAULT_WORKSPACE_PREFIX, PermissionMode, SshConnection};
    use mj_core::state::State;

    use super::*;

    fn ssh_machine(host: &str) -> (Machine, SshConnection) {
        let ssh = SshConnection {
            host: host.to_owned(),
            user: Some("dev".to_owned()),
            identity_file: None,
            extra_args: vec![],
        };
        let machine = Machine::Ssh {
            ssh: ssh.clone(),
            workspace_prefix: DEFAULT_WORKSPACE_PREFIX.into(),
            build_cache: None,
        };
        (machine, ssh)
    }

    fn unreachable(machine: &Machine) -> MachineRuntimesChecked {
        MachineRuntimesChecked {
            probes: vec![MachineRuntimeProbe {
                machine_id: "builder".into(),
                machine: machine.clone(),
                finding: MachineFinding::Unreachable {
                    detail: "`ssh dev@builder true` failed: Connection timed out".into(),
                    remediation: "Check that builder is up.".into(),
                },
            }],
            added: BTreeMap::new(),
        }
    }

    fn started(dashboard: &mut DashboardState) -> Option<(u64, Vec<String>)> {
        match dashboard.take_machine_runtime_check()? {
            DashboardAction::CheckMachineRuntimes {
                generation,
                machines,
            } => Some((generation, machines.into_iter().map(|(id, _)| id).collect())),
            other => panic!("unexpected action {other:?}"),
        }
    }

    /// One check per machine definition: never repeated while in flight,
    /// an answer about a host the machine no longer names is dropped, and a
    /// machine that loses the runtimes a check gave it is checked again.
    #[test]
    fn a_machine_without_a_runtime_is_checked_once_per_definition() {
        let (first, _) = ssh_machine("builder.example.test");
        let (moved, moved_ssh) = ssh_machine("moved.example.test");
        let mut config = Config::default();
        config.machines.insert("builder".into(), first.clone());
        let mut dashboard = DashboardState::new(config.clone(), State::default(), BTreeMap::new());

        let (stale, machines) = started(&mut dashboard).expect("startup checks the machine");
        assert_eq!(machines, ["builder"]);
        assert_eq!(
            dashboard.machine_runtime_reason("builder").as_deref(),
            Some("checking what it can run…")
        );
        assert_eq!(started(&mut dashboard), None, "a check in flight repeats");

        config.machines.insert("builder".into(), moved.clone());
        dashboard.set_config(config.clone());
        let (current, _) = started(&mut dashboard).expect("an edited machine is checked again");
        dashboard.apply_machine_runtime_check(stale, Ok(unreachable(&first)));
        assert_eq!(
            dashboard.machine_runtime_reason("builder").as_deref(),
            Some("checking what it can run…"),
            "an answer about the old host must not describe the new one"
        );

        dashboard.apply_machine_runtime_check(current, Ok(unreachable(&moved)));
        let reason = dashboard.machine_runtime_reason("builder").unwrap();
        assert!(reason.starts_with("it did not answer."), "{reason}");
        assert!(
            dashboard
                .notice()
                .is_some_and(|notice| notice.contains("Machine builder has no runtime")),
            "{:?}",
            dashboard.notice()
        );
        assert_eq!(started(&mut dashboard), None, "an answered machine repeats");

        config.targets.insert(
            "builder".into(),
            TargetTemplate::SshBare {
                ssh: moved_ssh,
                permissions: PermissionMode::Guardian,
                workspace_prefix: DEFAULT_WORKSPACE_PREFIX.into(),
            },
        );
        dashboard.set_config(config.clone());
        assert_eq!(started(&mut dashboard), None);
        config.targets.remove("builder");
        dashboard.set_config(config);
        assert!(
            started(&mut dashboard).is_some(),
            "a machine that lost its runtimes is checked again"
        );
    }
}
