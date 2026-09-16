use super::*;

impl DashboardState {
    pub(crate) fn prepare_new_target(&self, wizard: &mut NewWizard) -> DashboardAction {
        self.prepare_target(
            wizard.target,
            &wizard.aws_options,
            &mut wizard.resource_allocation,
            &mut wizard.sizing_error,
            None,
        )
    }

    pub(crate) fn target_readiness_rejection(&self, target_id: &str) -> Option<String> {
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
        self.mark_render_changed();
    }

    /// Why this session cannot resume on `target_id`, or `None` when it can.
    pub(crate) fn resume_target_rejection(
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

    pub(crate) fn prepare_resume_target(&self, wizard: &mut ResumeWizard) -> DashboardAction {
        let previous = self
            .state
            .sessions
            .get(&wizard.session_id)
            .and_then(|session| session.resource_allocation.as_ref());
        self.prepare_target(
            wizard.target,
            &wizard.aws_options,
            &mut wizard.resource_allocation,
            &mut wizard.sizing_error,
            previous,
        )
    }

    pub(crate) fn prepare_target(
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

    pub(crate) fn host_limits(&self, target_id: &str) -> Option<(u64, u64)> {
        self.capacity_details
            .values()
            .find(|detail| detail.target.target_ids.iter().any(|id| id == target_id))
            .and_then(|detail| detail.usage.as_ref())
            .map(|usage| (usage.logical_cores, usage.memory_total_bytes))
    }

    pub(crate) fn adjust_new_resources(&self, wizard: &mut NewWizard, code: KeyCode) {
        let target_id = nth_key(&self.config.targets, wizard.target);
        adjust_resources(
            &mut wizard.resource_allocation,
            wizard.aws_options.get(&target_id),
            self.host_limits(&target_id),
            code,
        );
    }

    pub(crate) fn adjust_resume_resources(&self, wizard: &mut ResumeWizard, code: KeyCode) {
        let target_id = nth_key(&self.config.targets, wizard.target);
        adjust_resources(
            &mut wizard.resource_allocation,
            wizard.aws_options.get(&target_id),
            self.host_limits(&target_id),
            code,
        );
    }
}
