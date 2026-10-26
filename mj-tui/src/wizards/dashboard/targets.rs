use super::*;

impl DashboardState {
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

    /// The indexes into `config.targets` of the targets the wizards list:
    /// every one that is offered (`DashboardState::target_offered`).
    /// `wizard.target` keeps indexing the full map; only the rows shown, and
    /// the position selected among them, go through this list.
    pub(in crate::wizards) fn offered_target_indices(&self) -> Vec<usize> {
        self.config
            .targets
            .keys()
            .enumerate()
            .filter(|(_, id)| self.target_offered(id))
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
                .filter(|(id, _)| self.profile_offered(id))
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
        self.target_unavailable(target_id)
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
    /// The step lists every offered target. One counts unless the draft
    /// cannot use it, such as a bundle session on a local bare target. A
    /// target no answer has reached yet is offered, so the step is shown
    /// while it might gain a choice. The one target must also have no size
    /// to set: a container or EC2 target is sized on that step, so its step
    /// is kept. So is the step of a configuration with a machine no runtime
    /// runs on, which the step names.
    pub(in crate::wizards) fn lone_target<W: WizardDraft>(&self, wizard: &W) -> Option<usize> {
        if self.config.machines_without_runtimes().next().is_some() {
            return None;
        }
        let mut offered = self
            .config
            .targets
            .iter()
            .enumerate()
            .filter(|(_, (id, _))| self.target_offered(id) && wizard.target_compatible(self, id));
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
            let action = self.prepare_wizard_target(wizard);
            if mj_core::config::is_container_target(&self.config.targets[&target_id]) {
                self.validate_wizard_resources(wizard);
            }
            action
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
            | TargetTemplate::SshDocker { .. }
            | TargetTemplate::SshAppleContainer { .. } => {
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

    pub(crate) fn host_container_capacity(
        &self,
        target_id: &str,
    ) -> Option<mj_core::state::HostContainerCapacity> {
        self.container_capacities
            .values()
            .find(|capacity| capacity.target_ids.iter().any(|id| id == target_id))
            .map(|capacity| capacity.capacity)
    }

    pub fn set_container_capacities(
        &mut self,
        capacities: BTreeMap<String, mj_client::runtime_feed::RuntimeContainerCapacity>,
    ) {
        let mut affected = std::collections::BTreeSet::new();
        for (target_id, capacity) in &self.container_capacities {
            if capacities.get(target_id) != Some(capacity) {
                affected.extend(capacity.target_ids.iter().cloned());
            }
        }
        for (target_id, capacity) in &capacities {
            if self.container_capacities.get(target_id) != Some(capacity) {
                affected.extend(capacity.target_ids.iter().cloned());
            }
        }
        self.container_capacities = capacities;
        self.refresh_wizard_resource_limits(&affected.into_iter().collect::<Vec<_>>());
    }

    pub(super) fn validate_wizard_resources<W: WizardDraft>(&self, wizard: &mut W) {
        let target_id = nth_key(&self.config.targets, wizard.target());
        let uses_remaining = wizard.remember_container_size().is_some();
        let capacity = self.host_container_capacity(&target_id);
        if uses_remaining && capacity.is_some_and(|capacity| !capacity.complete) {
            let (_, allocation, error) = wizard.sizing_mut();
            *allocation = None;
            *error = Some(
                "A live container has no recorded sizing, so RAM left cannot be calculated safely."
                    .into(),
            );
            return;
        }
        let limits = if uses_remaining {
            capacity.map(|capacity| (capacity.remaining.cpus, capacity.remaining.memory_bytes))
        } else {
            self.host_limits(&target_id)
        };
        let result = wizard.resource_editor().allocation(limits, uses_remaining);
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
