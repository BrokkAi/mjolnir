use super::*;

impl DashboardState {
    pub(in crate::wizards) fn target_readiness_rejection(&self, target_id: &str) -> Option<String> {
        let template = self.config.targets.get(target_id)?;
        // A raw local target runs in this controller process's host; there is
        // no external service or connection whose readiness needs a probe.
        if matches!(template, TargetTemplate::LocalBare) {
            return None;
        }
        match self
            .target_readiness
            .get(target_id)
            .filter(|check| &check.template == template && !check.is_stale(Instant::now()))
            .and_then(|check| check.result.as_ref())
        {
            Some(Ok(())) => None,
            Some(Err(error)) => Some(format!("unavailable: {error}")),
            None => Some("checking availability…".into()),
        }
    }

    /// Start readiness checks for those of `target_ids` with no current
    /// result, sharing one generation. A check in flight is never repeated.
    pub(in crate::wizards) fn begin_target_readiness_checks(
        &mut self,
        target_ids: Vec<String>,
    ) -> Option<DashboardAction> {
        let now = Instant::now();
        let target_ids: Vec<_> = target_ids
            .into_iter()
            .filter(|id| {
                self.config.targets.get(id).is_some_and(|template| {
                    self.target_readiness
                        .get(id)
                        .is_none_or(|check| &check.template != template || check.is_stale(now))
                })
            })
            .collect();
        if target_ids.is_empty() {
            return None;
        }
        self.target_readiness_generation = self.target_readiness_generation.wrapping_add(1);
        let generation = self.target_readiness_generation;
        for id in &target_ids {
            let runtime_missing = self.target_runtime_missing(id);
            self.target_readiness.insert(
                id.clone(),
                TargetReadiness {
                    template: self.config.targets[id].clone(),
                    generation,
                    result: None,
                    runtime_missing,
                    recorded_at: now,
                },
            );
        }
        Some(DashboardAction::CheckTargetReadiness {
            generation,
            target_ids,
        })
    }

    /// Check the local container targets the Targets pane lists. The standard
    /// ones exist whether or not their engine is installed
    /// (`Config::with_local_targets` offers them as candidates that callers
    /// must check), so the pane has to find out which of them can run. Only
    /// local container engines are probed here: each check is a quick local
    /// command, while SSH and AWS targets would open connections.
    pub fn take_target_availability_check(&mut self) -> Option<DashboardAction> {
        let target_ids: Vec<_> = self
            .capacity_details
            .values()
            .filter(|detail| detail.target.local)
            .flat_map(|detail| detail.target.target_ids.iter())
            .filter(|id| {
                matches!(
                    self.config.targets.get(*id),
                    Some(
                        TargetTemplate::LocalPodman { .. }
                            | TargetTemplate::LocalDocker { .. }
                            | TargetTemplate::AppleContainer { .. }
                    )
                )
            })
            .cloned()
            .collect();
        self.begin_target_readiness_checks(target_ids)
    }

    /// Start checks for the local directories of configured projects that
    /// have no current answer. A check in flight is never repeated.
    pub(in crate::wizards) fn begin_project_directory_checks(&mut self) -> Option<DashboardAction> {
        let now = Instant::now();
        let paths: Vec<std::path::PathBuf> = self
            .config
            .bundles
            .values()
            .flat_map(|bundle| bundle.repositories.iter())
            .filter_map(|repository| repository.local.clone())
            .filter(|path| {
                self.project_directory_checks.get(path).is_none_or(|check| {
                    check.state != ProjectDirectoryState::Checking
                        && now.saturating_duration_since(check.recorded_at)
                            >= PROJECT_DIRECTORY_CHECK_TTL
                })
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if paths.is_empty() {
            return None;
        }
        for path in &paths {
            self.project_directory_checks.insert(
                path.clone(),
                ProjectDirectoryCheck {
                    state: ProjectDirectoryState::Checking,
                    recorded_at: now,
                },
            );
        }
        Some(DashboardAction::CheckProjectDirectories { paths })
    }

    /// Record the answer for `path`: whether it is a directory, or `None`
    /// when the check could not tell.
    pub fn apply_project_directory_check(
        &mut self,
        path: std::path::PathBuf,
        exists: Option<bool>,
    ) {
        let state = match exists {
            Some(true) => ProjectDirectoryState::Present,
            Some(false) => ProjectDirectoryState::Missing,
            None => ProjectDirectoryState::Unknown,
        };
        self.project_directory_checks.insert(
            path,
            ProjectDirectoryCheck {
                state,
                recorded_at: Instant::now(),
            },
        );
    }

    /// The configured local directories of `bundle` that the last check
    /// found missing. A directory not yet checked is not called missing.
    pub(crate) fn missing_project_directories<'a>(
        &self,
        bundle: &'a mj_core::config::ProjectBundle,
    ) -> Vec<&'a std::path::Path> {
        bundle
            .repositories
            .iter()
            .filter_map(|repository| repository.local.as_deref())
            .filter(|path| {
                self.project_directory_checks
                    .get(*path)
                    .is_some_and(|check| check.state == ProjectDirectoryState::Missing)
            })
            .collect()
    }

    /// Whether the last readiness check of `target_id` failed. A target not
    /// yet checked is not called unavailable.
    pub(crate) fn target_known_unavailable(&self, target_id: &str) -> bool {
        let Some(template) = self.config.targets.get(target_id) else {
            return false;
        };
        self.target_readiness.get(target_id).is_some_and(|check| {
            &check.template == template && matches!(check.result, Some(Err(_)))
        })
    }

    /// Whether the last check found the runtime `target_id` needs not
    /// installed on this host. Such a target is permanently unavailable here,
    /// so the wizards do not offer it; a host that merely did not answer is
    /// transient and stays listed.
    pub(crate) fn target_runtime_missing(&self, target_id: &str) -> bool {
        let Some(template) = self.config.targets.get(target_id) else {
            return false;
        };
        self.target_readiness
            .get(target_id)
            .is_some_and(|check| &check.template == template && check.runtime_missing)
    }

    /// The indexes into `config.targets` of the targets the wizards list:
    /// every one whose runtime is not known to be missing on this host.
    /// `wizard.target` keeps indexing the full map; only the rows shown, and
    /// the position selected among them, go through this list.
    pub(in crate::wizards) fn offered_target_indices(&self) -> Vec<usize> {
        self.config
            .targets
            .keys()
            .enumerate()
            .filter(|(_, id)| !self.target_runtime_missing(id))
            .map(|(index, _)| index)
            .collect()
    }

    /// The row `target` (an index into `config.targets`) has among the
    /// offered targets. A target that is not offered has no row, so the first
    /// stands in and Next stays disabled by the target's own rejection.
    pub(in crate::wizards) fn target_row(&self, target: usize) -> usize {
        self.offered_target_indices()
            .iter()
            .position(|index| *index == target)
            .unwrap_or(0)
    }

    /// Record that the check of `target_id` found its runtime not installed.
    pub fn apply_target_runtime_missing(
        &mut self,
        generation: u64,
        target_id: String,
        reason: String,
    ) {
        self.apply_target_readiness(generation, target_id.clone(), Err(reason));
        let Some(check) = self.target_readiness.get_mut(&target_id) else {
            return;
        };
        if check.generation != generation {
            return;
        }
        check.runtime_missing = true;
        self.move_wizard_off_missing_target(&target_id);
    }

    /// A wizard left on a target that has just disappeared from its list
    /// moves to the first target still offered.
    fn move_wizard_off_missing_target(&mut self, target_id: &str) {
        let Some(missing) = self.config.targets.keys().position(|id| id == target_id) else {
            return;
        };
        let Some(first) = self.offered_target_indices().first().copied() else {
            return;
        };
        match &mut self.mode {
            Mode::New(wizard) if wizard.target == missing => wizard.target = first,
            Mode::Resume(wizard) if wizard.target == missing => wizard.target = first,
            _ => {}
        }
    }

    pub fn apply_target_readiness(
        &mut self,
        generation: u64,
        target_id: String,
        result: Result<(), String>,
    ) {
        let Some(check) = self.target_readiness.get_mut(&target_id) else {
            return;
        };
        if check.generation != generation
            || self.config.targets.get(&target_id) != Some(&check.template)
        {
            return;
        }
        check.result = Some(result);
        check.runtime_missing = false;
        check.recorded_at = Instant::now();
    }

    /// The profiles the resume wizard offers. A record's resume is limited to
    /// the profiles compatible with it; an archived session has no record, so
    /// every enabled profile can carry its summary.
    pub(crate) fn resume_wizard_profiles(
        &self,
        wizard: &ResumeWizard,
    ) -> Vec<(&String, HarnessKind)> {
        match wizard.source {
            ResumeSource::Session => self.compatible_profiles(&wizard.session_id),
            ResumeSource::Archive => self
                .config
                .profiles
                .iter()
                .filter(|(_, profile)| profile.enabled)
                .map(|(id, profile)| (id, profile.kind))
                .collect(),
        }
    }

    /// Why this session cannot resume on `target_id`, or `None` when it can.
    pub(in crate::wizards) fn resume_target_rejection(
        &self,
        session_id: &str,
        target_id: &str,
    ) -> Option<String> {
        self.target_readiness_rejection(target_id)
            .or_else(|| self.resume_target_incompatibility(session_id, target_id))
    }

    /// Why this session could not resume on `target_id` even once the target
    /// is ready. An archived session has no record here, so nothing rules a
    /// target out for it.
    pub(in crate::wizards) fn resume_target_incompatibility(
        &self,
        session_id: &str,
        target_id: &str,
    ) -> Option<String> {
        let session = self.session_record(session_id)?;
        let checkout = self
            .state
            .checkout(session_id)
            .unwrap_or_else(|_| session.checkout());
        mj_client::target::resume_compatibility_with_checkout(
            session,
            &checkout,
            &self.config,
            target_id,
        )
        .err()
    }

    /// The index of the only target the target step would offer `wizard`,
    /// when that step has nothing else to decide.
    ///
    /// The step lists every configured and built-in target. A target counts
    /// as offered unless its last availability check failed, such as the
    /// built-in docker on a host without Docker, or the draft cannot use it,
    /// such as a bundle session on a local bare target. A target whose check
    /// has not answered yet still counts, so the step is shown while it
    /// might gain a choice. The one target must also be usable now and have
    /// no size to set: a container or EC2 target is sized on that step, so
    /// its step is kept.
    pub(in crate::wizards) fn lone_target<W: WizardDraft>(&self, wizard: &W) -> Option<usize> {
        let mut offered = self
            .config
            .targets
            .iter()
            .enumerate()
            .filter(|(_, (id, _))| {
                !self.target_known_unavailable(id) && wizard.target_compatible(self, id)
            });
        let (index, (id, template)) = offered.next()?;
        if offered.next().is_some() {
            return None;
        }
        let no_size = matches!(
            template,
            TargetTemplate::LocalBare | TargetTemplate::SshBare { .. }
        );
        (no_size && wizard.target_rejection(self, id).is_none()).then_some(index)
    }

    /// Selects the target step's only target when [`Self::lone_target`]
    /// finds one, and records whether it did. The caller then advances from
    /// the target step, as Next there would; on false it shows the step.
    pub(in crate::wizards) fn skip_target_step<W: WizardDraft>(&mut self, wizard: &mut W) -> bool {
        let Some(index) = self.lone_target(wizard) else {
            wizard.set_target_step_skipped(false);
            return false;
        };
        if wizard.target() != index {
            wizard.note_draft_change(self, DraftChange::TargetSelected);
            wizard.set_target(index);
        }
        // A bare target has no size, so this only clears a size left from the
        // target selected before, and never asks for EC2 sizes.
        let _ = self.prepare_wizard_target(wizard);
        wizard.set_target_step_skipped(true);
        true
    }

    /// Capacity samples validate the same draft as keystrokes; they never
    /// replace typed values with a silently clamped allocation.
    pub(crate) fn refresh_wizard_resource_limits(&mut self, affected: &[String]) {
        let (index, initialized) = match &self.mode {
            Mode::New(wizard) => (wizard.target, wizard.resource_editor.target_id.is_some()),
            Mode::Resume(wizard) => (wizard.target, wizard.resource_editor.target_id.is_some()),
            _ => return,
        };
        let id = nth_key(&self.config.targets, index);
        if !initialized
            || !affected.contains(&id)
            || !mj_core::config::is_container_target(&self.config.targets[&id])
        {
            return;
        }
        match std::mem::replace(&mut self.mode, Mode::Dashboard) {
            Mode::New(mut wizard) => {
                self.refresh_resource_draft(&mut wizard);
                self.mode = Mode::New(wizard);
            }
            Mode::Resume(mut wizard) => {
                self.refresh_resource_draft(&mut wizard);
                self.mode = Mode::Resume(wizard);
            }
            _ => unreachable!("checked wizard mode"),
        }
    }

    fn refresh_resource_draft<W: WizardDraft>(&mut self, wizard: &mut W) {
        let before = wizard.resource_allocation().cloned();
        self.validate_wizard_resources(wizard);
        if wizard.resource_allocation() != before.as_ref() {
            wizard.note_draft_change(self, DraftChange::ResourcesAdjusted);
            if wizard.resource_allocation().is_none() && wizard.step() != WizardStep::Profile {
                wizard.set_step(WizardStep::Target);
                wizard.form_mut().focus(WizardControl::ResourceCpu);
            }
        }
    }

    pub(super) fn initialize_wizard_resources<W: WizardDraft>(
        &self,
        wizard: &mut W,
    ) -> DashboardAction {
        let target_id = nth_key(&self.config.targets, wizard.target());
        if wizard.resource_editor().target_id.as_deref() == Some(&target_id) {
            return DashboardAction::None;
        }
        if matches!(
            self.config.targets[&target_id],
            TargetTemplate::AwsEc2 { .. }
        ) && matches!(
            wizard.resource_allocation(),
            Some(SessionResourceAllocation::Container { .. })
        ) {
            *wizard.sizing_mut().1 = None;
        }
        if let Some(allocation) = wizard.resource_allocation().cloned() {
            let editor = wizard.resource_editor_mut();
            editor.reset(Some(&allocation));
            editor.target_id = Some(target_id.clone());
            if mj_core::config::is_container_target(&self.config.targets[&target_id]) {
                self.validate_wizard_resources(wizard);
            }
            DashboardAction::None
        } else {
            self.prepare_wizard_target(wizard)
        }
    }

    pub(super) fn prepare_wizard_target<W: WizardDraft>(&self, wizard: &mut W) -> DashboardAction {
        let previous = wizard.previous_allocation(self);
        let target_index = wizard.target();
        let (aws_options, allocation, sizing_error) = wizard.sizing_mut();
        let action = self.prepare_target(
            target_index,
            aws_options,
            allocation,
            sizing_error,
            previous,
        );
        let allocation = wizard.resource_allocation().cloned();
        let target_id = nth_key(&self.config.targets, target_index);
        let editor = wizard.resource_editor_mut();
        editor.reset(allocation.as_ref());
        editor.target_id = Some(target_id);
        action
    }

    fn prepare_target(
        &self,
        target_index: usize,
        aws_options: &BTreeMap<String, Vec<SessionResourceAllocation>>,
        allocation: &mut Option<SessionResourceAllocation>,
        sizing_error: &mut Option<String>,
        previous: Option<&SessionResourceAllocation>,
    ) -> DashboardAction {
        let target_id = nth_key(&self.config.targets, target_index);
        let target = &self.config.targets[&target_id];
        *sizing_error = None;
        match target {
            TargetTemplate::LocalBare => {
                *allocation = None;
                DashboardAction::None
            }
            TargetTemplate::LocalPodman { .. }
            | TargetTemplate::LocalDocker { .. }
            | TargetTemplate::AppleContainer { .. }
            | TargetTemplate::SshPodman { .. }
            | TargetTemplate::SshDocker { .. } => {
                let limits = self.host_limits(&target_id);
                let remembered = container_size_host(target)
                    .and_then(|host| self.state.container_sizes.get(host))
                    .copied();
                let previous = match previous {
                    Some(SessionResourceAllocation::Container { cpus, memory_bytes }) => {
                        Some(HostContainerSize {
                            cpus: *cpus,
                            memory_bytes: *memory_bytes,
                        })
                    }
                    _ => remembered,
                };
                let limits =
                    limits.map(|(cpus, memory_bytes)| HostContainerSize { cpus, memory_bytes });
                let size = default_container_size(previous, limits);
                *allocation = Some(SessionResourceAllocation::Container {
                    cpus: size.cpus,
                    memory_bytes: size.memory_bytes,
                });
                DashboardAction::None
            }
            TargetTemplate::AwsEc2 { .. } => {
                if let Some(options) = aws_options.get(&target_id) {
                    *allocation = preferred_aws_allocation(options, previous).cloned();
                    DashboardAction::None
                } else {
                    *allocation = None;
                    DashboardAction::ResolveAwsResourceOptions {
                        target_template_ids: vec![target_id],
                    }
                }
            }
            TargetTemplate::SshBare { .. } => {
                *allocation = None;
                DashboardAction::None
            }
        }
    }

    pub(in crate::wizards) fn host_limits(&self, target_id: &str) -> Option<(u64, u64)> {
        self.capacity_details
            .values()
            .find(|detail| detail.target.target_ids.iter().any(|id| id == target_id))
            .and_then(|detail| detail.usage.as_ref())
            .map(|usage| (usage.logical_cores, usage.memory_total_bytes))
    }

    pub(super) fn validate_wizard_resources<W: WizardDraft>(&self, wizard: &mut W) {
        let target_id = nth_key(&self.config.targets, wizard.target());
        let result = wizard
            .resource_editor()
            .allocation(self.host_limits(&target_id));
        let (_, allocation, error) = wizard.sizing_mut();
        match result {
            Ok(value) => {
                *allocation = Some(value);
                *error = None;
            }
            Err(reason) => {
                *allocation = None;
                *error = Some(reason);
            }
        }
    }
}
