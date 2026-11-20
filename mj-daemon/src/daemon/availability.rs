//! Demand-driven launch availability, shared by all surfaces.
use super::*;
use mj_client::availability::{AvailabilityNote, AvailabilityVerdict, RuntimeAvailability};
use mj_core::config::{TargetTemplate, runtime_kind_name, target_machine_id};
use mj_setup::setup::{AvailabilityReport, MachineProbe, MachineProbeOutcome};
use std::time::Instant;

#[derive(Default)]
pub(super) struct Availability {
    run: CheckRun,
    pub(super) report: Option<Arc<AvailabilityReport>>,
    failure: Option<String>,
}

#[derive(Default, PartialEq, Eq)]
enum CheckRun {
    #[default]
    Idle,
    Queued,
    Checking {
        rerun: bool,
    },
}

impl Availability {
    // Returns whether to wake the sole runner.
    fn request(&mut self) -> bool {
        match &mut self.run {
            CheckRun::Idle => {
                self.run = CheckRun::Queued;
                true
            }
            CheckRun::Queued => false,
            CheckRun::Checking { rerun } => {
                *rerun = true;
                false
            }
        }
    }

    fn start(&mut self) -> bool {
        if self.run != CheckRun::Queued {
            return false;
        }
        self.run = CheckRun::Checking { rerun: false };
        true
    }

    fn complete(&mut self) {
        self.run = match self.run {
            CheckRun::Checking { rerun: true } => CheckRun::Queued,
            CheckRun::Checking { rerun: false } => CheckRun::Idle,
            _ => unreachable!("the runner completes its own check"),
        };
    }
}

impl RuntimeState {
    pub(crate) fn request_availability_check(&self) -> Result<()> {
        let _work = mj_controller::upgrade::activity_unless_draining("availability check request")?;
        if self.availability().request() {
            self.availability_requested.notify_one();
        }
        Ok(())
    }

    pub(super) fn availability(&self) -> std::sync::MutexGuard<'_, Availability> {
        self.availability
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

pub(super) async fn run(state: Arc<RuntimeState>, cancellation: CancellationToken) -> Result<()> {
    loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            () = state.availability_requested.notified() => {}
        }
        while !cancellation.is_cancelled() && state.availability().start() {
            let cancelled = Arc::new(AtomicBool::new(false));
            let mut check = mj_core::runtime::spawn_blocking({
                let cancelled = cancelled.clone();
                move || {
                    let executor = CancellableProcessExecutor::new(cancelled.clone())
                        .with_deadline(Duration::from_secs(60));
                    mj_setup::setup::check_availability(
                        &mj_core::config::config_path(),
                        &executor,
                        &|| cancelled.load(Ordering::Acquire),
                    )
                }
            });
            let result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    cancelled.store(true, Ordering::Release);
                    if let Err(error) = check.await {
                        tracing::warn!(%error, "availability check task failed during shutdown");
                    }
                    return Ok(());
                }
                result = &mut check => result.context("availability check task failed").and_then(std::convert::identity),
            };
            let result = match result {
                Ok(report)
                    if !report.added_profiles.is_empty() || !report.added_runtimes.is_empty() =>
                {
                    // Publish the newly written configuration without waiting for
                    // the periodic config watcher to notice it.
                    super::process::refresh_runtime_controller(&state).await;
                    Ok(report)
                }
                other => other,
            };
            let notice = {
                let mut availability = state.availability();
                let notice = match result {
                    Ok(report) => {
                        let notice = discoveries(&report);
                        availability.report = Some(Arc::new(report));
                        availability.failure = None;
                        notice
                    }
                    Err(error) => {
                        // A parse error chain can quote lines from the config.
                        let summary = error.to_string();
                        let error = format!("{error:#}");
                        if availability.failure.as_ref() == Some(&error) {
                            None
                        } else {
                            let notice = format!("Could not check what is available: {summary}");
                            tracing::warn!(%error, "availability check failed");
                            availability.failure = Some(error);
                            Some(notice)
                        }
                    }
                };
                availability.complete();
                notice
            };
            if let Some(notice) = notice {
                state.push_public_notice("", notice);
            } else {
                state.publish_revision();
            }
        }
    }
}

fn discoveries(report: &AvailabilityReport) -> Option<String> {
    let mut sentences = Vec::new();
    if !report.added_profiles.is_empty() {
        sentences.push(format!(
            "Added agent profiles: {}.",
            report.added_profiles.join(", ")
        ));
    }
    for (machine, added) in &report.added_runtimes {
        sentences.push(format!(
            "Added runtimes for {machine}: {}.",
            added.join(", ")
        ));
    }
    (!sentences.is_empty()).then(|| sentences.join(" "))
}

/// Whether a local engine command is on PATH. Keep the existing PATH cache
/// cadence, independent of client-triggered launch probes.
#[derive(Default)]
pub(super) struct InstalledEngines {
    checked_at: Option<Instant>,
    path: Option<std::ffi::OsString>,
    missing: BTreeMap<&'static str, bool>,
}

impl InstalledEngines {
    fn runtime_missing(&mut self, template: &TargetTemplate) -> bool {
        let Some(engine) = mj_controller::targets::local_engine_command(template) else {
            return false;
        };
        let path = std::env::var_os("PATH");
        if self.path != path
            || self
                .checked_at
                .is_none_or(|at| at.elapsed() >= crate::pollers::CAPACITY_POLL_INTERVAL)
        {
            self.missing.clear();
            self.path = path;
            self.checked_at = Some(Instant::now());
        }
        *self.missing.entry(engine).or_insert_with(|| {
            mj_controller::targets::runtime_missing_on_host(template, self.path.as_deref())
        })
    }
}

enum TargetFinding {
    Verdict(AvailabilityVerdict),
    Unreachable {
        machine: String,
        detail: String,
        remediation: String,
        answered_nothing: bool,
    },
}

impl TargetFinding {
    fn reason(reason: String) -> Self {
        Self::Verdict(AvailabilityVerdict::Missing { reason })
    }

    fn verdict(&self) -> AvailabilityVerdict {
        match self {
            Self::Verdict(verdict) => verdict.clone(),
            Self::Unreachable {
                machine,
                detail,
                remediation,
                answered_nothing,
            } => AvailabilityVerdict::Missing {
                reason: sentence(
                    &format!(
                        "{}. {}",
                        ssh_failure(Some(machine), *answered_nothing),
                        detail.trim()
                    ),
                    remediation,
                ),
            },
        }
    }
}

pub(super) fn project(
    report: Option<&AvailabilityReport>,
    config: &Config,
    engines: &mut InstalledEngines,
) -> RuntimeAvailability {
    let mut projection = RuntimeAvailability::default();
    let machines: BTreeMap<_, _> = report
        .into_iter()
        .flat_map(|report| &report.machines)
        .filter(|probe| config.machines.get(&probe.machine_id) == Some(&probe.machine))
        .map(|probe| (probe.machine_id.as_str(), probe))
        .collect();
    for (id, current) in config.enabled_profiles() {
        let verdict = report
            .and_then(|report| report.profiles.get(id))
            .filter(|(checked, _)| checked == current)
            .map_or(AvailabilityVerdict::Unknown, |(_, missing)| match missing {
                Some(reason) => AvailabilityVerdict::Missing {
                    reason: reason.clone(),
                },
                None => AvailabilityVerdict::Available,
            });
        projection.profiles.insert(id.to_owned(), verdict);
    }
    let mut unreachable = BTreeMap::<String, (Vec<&str>, String, String, bool)>::new();
    for (id, target) in &config.targets {
        let installed_missing = engines.runtime_missing(target);
        if installed_missing {
            projection.uninstalled_engines.insert(id.clone());
        }
        let finding = if let Some(refusal) = target.launch_refusal() {
            TargetFinding::reason(refusal.into())
        } else if config.disabled_targets.contains(id) {
            TargetFinding::reason(format!(
                "runtime {id:?} is turned off; turn it on in Settings › Runtimes or choose another"
            ))
        } else if installed_missing {
            let label = match target {
                TargetTemplate::LocalDocker { .. } => "Docker",
                TargetTemplate::LocalPodman { .. } => "Podman",
                TargetTemplate::AppleContainer { .. } => "Apple container",
                _ => unreachable!("only local engine commands are checked"),
            };
            TargetFinding::reason(mj_controller::targets::engine_not_installed(label))
        } else {
            target_finding(config, id, target, report, &machines)
        };
        let verdict = finding.verdict();
        if config.enabled_target(id).is_some()
            && !(installed_missing && config.is_default_target(id))
        {
            match finding {
                TargetFinding::Unreachable {
                    machine,
                    detail,
                    remediation,
                    answered_nothing,
                } => {
                    unreachable
                        .entry(machine)
                        .or_insert_with(|| (Vec::new(), detail, remediation, answered_nothing))
                        .0
                        .push(id);
                }
                TargetFinding::Verdict(_) => {
                    if let Some(reason) = verdict.missing_reason() {
                        let text = format!("{id}: {reason}");
                        projection.target_notes.push(AvailabilityNote {
                            full: text.clone(),
                            text,
                            routine: false,
                        });
                    }
                }
            }
        }
        projection.targets.insert(id.clone(), verdict);
    }
    for (machine, (ids, detail, remediation, answered_nothing)) in unreachable {
        let brief = format!(
            "{}; not listing {}.",
            ssh_failure(Some(&machine), answered_nothing),
            ids.join(", ")
        );
        let full = sentence(&format!("{brief} {}", detail.trim()), &remediation);
        projection.target_notes.push(AvailabilityNote {
            text: if answered_nothing {
                brief
            } else {
                full.clone()
            },
            full,
            routine: answered_nothing,
        });
    }
    for (id, _) in config.machines_without_runtimes() {
        let Some(probe) = machines.get(id) else {
            continue;
        };
        let reason = finding_reason(probe);
        let note = if matches!(
            &probe.outcome,
            MachineProbeOutcome::Unreachable {
                answered_nothing: true,
                ..
            }
        ) {
            AvailabilityNote {
                text: format!("{id} did not answer."),
                full: format!("{id}: {reason}"),
                routine: true,
            }
        } else {
            let text = format!("Machine {id} has no runtime: {reason}");
            AvailabilityNote {
                full: text.clone(),
                text,
                routine: false,
            }
        };
        projection
            .machines_without_runtime
            .insert(id.to_owned(), reason);
        projection.target_notes.push(note);
    }
    projection
}

fn target_finding(
    config: &Config,
    id: &str,
    target: &TargetTemplate,
    report: Option<&AvailabilityReport>,
    machines: &BTreeMap<&str, &MachineProbe>,
) -> TargetFinding {
    let unknown = TargetFinding::Verdict(AvailabilityVerdict::Unknown);
    let available = TargetFinding::Verdict(AvailabilityVerdict::Available);
    let Some(report) = report.filter(|report| report.targets.get(id) == Some(target)) else {
        return unknown;
    };
    let kind = runtime_kind_name(target);
    match target {
        TargetTemplate::LocalBare => available,
        TargetTemplate::LocalPodman { .. }
        | TargetTemplate::LocalDocker { .. }
        | TargetTemplate::AppleContainer { .. } => {
            let Some(runtime) = report
                .local
                .iter()
                .find(|runtime| runtime.kind.name() == kind)
            else {
                return unknown;
            };
            if runtime.usable() {
                available
            } else {
                TargetFinding::reason(match runtime.remediation.as_deref() {
                    Some(fix) => format!("{} {fix}", runtime.detail.trim()),
                    None => runtime.detail.clone(),
                })
            }
        }
        _ => {
            let Some(probe) =
                target_machine_id(target, &config.machines).and_then(|id| machines.get(id))
            else {
                return unknown;
            };
            match &probe.outcome {
                MachineProbeOutcome::Unreachable {
                    detail,
                    remediation,
                    answered_nothing,
                } => TargetFinding::Unreachable {
                    machine: probe.machine_id.clone(),
                    detail: detail.clone(),
                    remediation: remediation.clone(),
                    answered_nothing: *answered_nothing,
                },
                MachineProbeOutcome::Answered(found) => {
                    match found.iter().find(|runtime| runtime.kind.name() == kind) {
                        Some(runtime) if runtime.usable() => available,
                        Some(runtime) => TargetFinding::reason(sentence(
                            &runtime.detail,
                            runtime.remediation.as_deref().unwrap_or_default(),
                        )),
                        None => unknown,
                    }
                }
            }
        }
    }
}

fn ssh_failure(machine: Option<&str>, answered_nothing: bool) -> String {
    match (machine, answered_nothing) {
        (Some(machine), true) => format!("{machine} did not answer"),
        (Some(machine), false) => format!("SSH to {machine} failed"),
        (None, true) => "it did not answer".into(),
        (None, false) => "its SSH check failed".into(),
    }
}

/// Why a machine has no runtime, from what probing it found: the reason it
/// did not answer, or each runtime it cannot run. A finding with usable
/// runtimes only arises when they could not be added.
fn finding_reason(probe: &MachineProbe) -> String {
    match &probe.outcome {
        MachineProbeOutcome::Unreachable {
            detail,
            remediation,
            answered_nothing,
        } => sentence(
            &format!(
                "{}. {}",
                ssh_failure(None, *answered_nothing),
                detail.trim()
            ),
            remediation,
        ),
        MachineProbeOutcome::Answered(_) => {
            let runtimes = probe.runtimes();
            if !runtimes.is_empty() {
                return format!(
                    "it can run {}; choose Detect runtimes in Settings › Runtimes to add them.",
                    runtimes
                        .iter()
                        .map(|(id, _)| id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            let mut reason = "it can run none.".to_owned();
            for runtime in probe.rejected() {
                reason.push(' ');
                reason.push_str(&sentence(
                    &format!("{}: {}", runtime.kind.label(), runtime.detail.trim()),
                    runtime.remediation.as_deref().unwrap_or_default(),
                ));
            }
            reason
        }
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
    use super::*;
    use mj_core::config::{HarnessKind, HarnessProfile};

    #[test]
    fn availability_requests_coalesce_into_one_rerun() {
        let mut state = Availability::default();
        assert!(!state.start(), "a new daemon leaves the check idle");
        assert!(state.request());
        assert!(
            !state.request(),
            "requests before the runner wakes share one check"
        );
        assert!(state.start());
        assert!(!state.request());
        assert!(!state.request());
        assert!(!state.start(), "no second check may run in parallel");
        state.complete();
        assert!(state.start(), "requests during a check leave one rerun");
        assert!(!state.start());
        state.complete();
        assert!(!state.start(), "the rerun leaves the runner idle");
        assert!(state.request(), "a new request can wake the runner again");
    }

    // Hard-won: 471c11891: transmitting the checked profile lost readiness
    // when the client had resolved environments.
    #[tokio::test]
    async fn availability_capture_keeps_missing_until_the_configured_entry_is_edited() {
        let mut config = Config::default();
        let mut profile =
            HarnessProfile::new(HarnessKind::Codex, PathBuf::from("/check-before-edit"));
        profile.environment = [("CONFIGURED_KEY".to_owned(), "value".to_owned())]
            .into_iter()
            .collect();
        config.profiles.insert("agent".into(), profile.clone());
        config
            .targets
            .insert("raw".into(), TargetTemplate::LocalBare);
        let state = super::super::tests::test_runtime_state_with(Controller {
            config: config.clone(),
            state: Default::default(),
        });
        let capture = || {
            let snapshot = state.runtime_publication().unwrap();
            // The verdict survives the same source-only feed transport a terminal uses.
            let wire = serde_json::to_value(snapshot.metadata).unwrap();
            let metadata: mj_client::runtime_feed::RuntimeMetadata =
                serde_json::from_value(wire).unwrap();
            metadata.availability
        };
        let before = capture();
        assert_eq!(before.profiles["agent"], AvailabilityVerdict::Unknown);
        assert_eq!(before.targets["raw"], AvailabilityVerdict::Unknown);

        state.availability().report = Some(Arc::new(AvailabilityReport {
            profiles: [(
                "agent".into(),
                (profile, Some("daemon credential missing".into())),
            )]
            .into(),
            targets: config.targets.clone(),
            local: Vec::new(),
            machines: Vec::new(),
            added_profiles: Vec::new(),
            added_runtimes: BTreeMap::new(),
        }));
        let checked = capture();
        assert_eq!(
            checked.profiles["agent"].missing_reason(),
            Some("daemon credential missing")
        );
        assert_eq!(checked.targets["raw"], AvailabilityVerdict::Available);

        config.profiles.get_mut("agent").unwrap().environment =
            [("CONFIGURED_KEY".to_owned(), "changed".to_owned())]
                .into_iter()
                .collect();
        state.owner().install_config(config);
        let edited = capture();
        assert_eq!(edited.profiles["agent"], AvailabilityVerdict::Unknown);
        assert_eq!(edited.targets["raw"], AvailabilityVerdict::Available);
    }
}
