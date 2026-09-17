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
        if let Some(reason) = self.target_readiness_rejection(target_id) {
            return Some(reason);
        }
        let session = self.state.sessions.get(session_id)?;
        mj_client::target::resume_compatibility(session, &self.config, target_id).err()
    }

    pub(super) fn prepare_wizard_target<W: WizardDraft>(&self, wizard: &mut W) -> DashboardAction {
        let previous = wizard.previous_allocation(self);
        let target_index = wizard.target();
        let (aws_options, allocation, sizing_error) = wizard.sizing_mut();
        self.prepare_target(
            target_index,
            aws_options,
            allocation,
            sizing_error,
            previous,
        )
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
                if limits.is_none() {
                    *sizing_error = Some("host totals unavailable; + disabled".into());
                }
                let remembered = container_size_host(target)
                    .and_then(|host| self.state.container_sizes.get(host));
                let (cpus, memory_bytes) = match previous {
                    Some(SessionResourceAllocation::Container { cpus, memory_bytes }) => {
                        clamp_resources(*cpus, *memory_bytes, limits)
                    }
                    _ if remembered.is_some() => {
                        let remembered = remembered.expect("remembered size checked above");
                        clamp_resources(remembered.cpus, remembered.memory_bytes, limits)
                    }
                    _ => clamp_resources(BASELINE_CPUS, BASELINE_MEMORY_BYTES, limits),
                };
                *allocation = Some(SessionResourceAllocation::Container { cpus, memory_bytes });
                DashboardAction::None
            }
            TargetTemplate::AwsEc2 { .. } => {
                if let Some(options) = aws_options.get(&target_id) {
                    *allocation = preferred_aws_option(options, previous).cloned();
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

    fn host_limits(&self, target_id: &str) -> Option<(u64, u64)> {
        self.capacity_details
            .values()
            .find(|detail| detail.target.target_ids.iter().any(|id| id == target_id))
            .and_then(|detail| detail.usage.as_ref())
            .map(|usage| (usage.logical_cores, usage.memory_total_bytes))
    }

    pub(super) fn adjust_wizard_resources<W: WizardDraft>(&self, wizard: &mut W, code: KeyCode) {
        let target_id = nth_key(&self.config.targets, wizard.target());
        let limits = self.host_limits(&target_id);
        let (aws_options, allocation, _) = wizard.sizing_mut();
        adjust_resources(allocation, aws_options.get(&target_id), limits, code);
    }
}
