//! Cheap, display-scoped invalidation for dashboard timers and animation.
//!
//! The dashboard is fed by several independent background projections.  Their
//! state remains current even while another workspace or a modal is visible,
//! but only the values that can change the next frame are included here.  The
//! snapshot is refreshed after a frame is drawn; checking a clock or animation
//! never mutates the dashboard or requests a redraw by itself.

pub(crate) use hel::clock::epoch_seconds;

use hel::hel_state::SessionState;
use hel::hel_targets::{DeploymentCapacityKind, DeploymentCapacityUsage};
use mj_controller::hel_review_host::RuntimeReviewView;

use crate::ingest::{CapacityDetail, SessionDetail};
use crate::render::{
    CAPACITY_SAMPLE_STALE_AFTER_SECONDS, checkpoint_age, quota_reset_cells, refresh_age,
    session_display_clock,
};
use crate::{DashboardState, Mode, SupportPane};

/// The values that were visible when the last frame was drawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RenderChangeSnapshot {
    pub(crate) clock: RenderClockSignature,
    pub(crate) animation: RenderAnimationSignature,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RenderClockSignature {
    sessions: Vec<DisplayedClock>,
    capacity: Vec<DisplayedClock>,
    quota: Vec<DisplayedClock>,
    resume: Vec<DisplayedClock>,
    import_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DisplayedClock {
    key: String,
    value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RenderAnimationSignature {
    active: bool,
    frame: Option<&'static str>,
    modal_frame: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapacityDisplaySignature {
    host: String,
    target_ids: Vec<String>,
    kind: DeploymentCapacityKind,
    probe_count: usize,
    usage: Option<DeploymentCapacityUsage>,
    on_demand: bool,
    probe_error: Option<String>,
    refreshing: bool,
    stale: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MaterializedDisplaySignature {
    current_turn_started_at: Option<u64>,
    last_agent_message: Option<std::sync::Arc<str>>,
    last_user_message: Option<std::sync::Arc<str>>,
    last_agent_message_follows_last_user: bool,
    latest_agent_activity_after_last_user: Option<std::sync::Arc<str>>,
    unread_agent_messages: usize,
    unread_session_restarts: usize,
    queued_prompt_count: usize,
    pending_elicitation_count: usize,
}

pub(crate) fn materialized_display_signature(
    detail: &SessionDetail,
) -> MaterializedDisplaySignature {
    MaterializedDisplaySignature {
        current_turn_started_at: detail.current_turn_started_at,
        last_agent_message: detail.last_agent_message.clone(),
        last_user_message: detail.last_user_message.clone(),
        last_agent_message_follows_last_user: detail.last_agent_message_follows_last_user,
        latest_agent_activity_after_last_user: detail.latest_agent_activity_after_last_user.clone(),
        unread_agent_messages: detail.unread_agent_messages,
        unread_session_restarts: detail.unread_session_restarts,
        queued_prompt_count: detail.queued_prompts.len(),
        pending_elicitation_count: detail.pending_elicitations.len(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisibleStateSignature {
    id: String,
    workspace_id: String,
    state: SessionState,
    display_title: String,
    profile: String,
    target: String,
    created_at: String,
    updated_at: String,
    last_error: Option<String>,
    last_checkpoint_error: Option<String>,
    checkpoint_created_at: Option<String>,
    project_source: hel::hel_state::ProjectSourceIdentity,
    materialized: Option<MaterializedDisplaySignature>,
}

/// Capture only the fields that the Targets pane renders for one capacity
/// detail.  In particular, a fresh sample timestamp does not invalidate the
/// pane while its displayed value and stale status stay the same.
pub(crate) fn capacity_display_signature(
    detail: &CapacityDetail,
    now_epoch_seconds: u64,
) -> CapacityDisplaySignature {
    CapacityDisplaySignature {
        host: detail.target.host.clone(),
        target_ids: detail.target.target_ids.clone(),
        kind: detail.target.kind,
        probe_count: detail.target.probes.len(),
        usage: detail.usage.clone(),
        on_demand: detail.on_demand,
        probe_error: detail.probe_error.clone(),
        refreshing: detail.refreshing,
        stale: crate::render::capacity_staleness(detail, now_epoch_seconds),
    }
}

impl DashboardState {
    /// Whether a dashboard clock has changed since the last drawn frame.
    ///
    /// Only text that actually contains a moving value is sampled.  Idle
    /// sessions, unrefreshed panes, and static reset strings therefore do not
    /// turn the once-per-second wakeup into a redraw.
    pub fn clock_changed(&self) -> bool {
        self.render_change_snapshot.clock != self.current_clock_signature()
    }

    /// Whether a visible dashboard spinner has advanced to another frame.
    ///
    /// The active flag is part of the signature so starting or settling the
    /// last spinner is noticed even when its previous frame happens to match
    /// the new idle signature.
    pub fn animation_changed(&self) -> bool {
        self.render_change_snapshot.animation != self.current_animation_signature()
    }

    /// Seed the time signatures with the values represented by the frame just
    /// drawn.  The event loop calls this after rendering, including frames
    /// requested by input or background content, so the next timer checks the
    /// displayed baseline rather than the previous timer check.
    pub fn acknowledge_render(&mut self) {
        self.render_change_snapshot.clock = self.current_clock_signature();
        self.render_change_snapshot.animation = self.current_animation_signature();
    }

    fn current_clock_signature(&self) -> RenderClockSignature {
        let now = epoch_seconds();
        RenderClockSignature {
            sessions: self.session_clock_signature(now),
            capacity: self.capacity_clock_signature(now),
            quota: self.quota_clock_signature(now),
            resume: self.resume_clock_signature(),
            import_status: match &self.mode {
                Mode::Importing(progress) => {
                    Some(crate::dialogs::import_progress_status(progress).to_string())
                }
                _ => None,
            },
        }
    }

    fn current_animation_signature(&self) -> RenderAnimationSignature {
        let active = self.visible_animation_active();
        RenderAnimationSignature {
            active,
            frame: active.then(|| {
                mj_chat::spinner::compact_frame(self.config.spinner, mj_chat::spinner::elapsed_ms())
            }),
            modal_frame: match &self.mode {
                Mode::ResumeDialog(dialog) if dialog.is_scanning() => {
                    Some(mj_chat::spinner::compact_frame(
                        self.config.spinner,
                        dialog.opened_at.elapsed().as_millis(),
                    ))
                }
                Mode::Setup(setup) => setup
                    .review_editor
                    .as_ref()
                    .and_then(|dialog| dialog.animation_frame()),
                _ => None,
            },
        }
    }

    /// A session row belongs to the current tab while the record is still
    /// active, or while an explicit operation keeps its row on screen.
    pub(crate) fn session_is_visible(&self, session_id: &str) -> bool {
        self.ordered_sessions()
            .into_iter()
            .enumerate()
            .find(|(_, session)| session.id == session_id)
            .is_some_and(|(index, _)| self.session_row_is_visible_at(index))
    }

    /// `session_row_areas` is populated by the last rendered Sessions table.
    /// Before the first frame there is no viewport to consult, so an active
    /// workspace row is treated as visible and the first projection can still
    /// request that frame. Once geometry exists, only rows in that viewport
    /// participate in feed invalidation and timer signatures.
    pub(crate) fn session_row_is_visible_at(&self, index: usize) -> bool {
        self.session_row_areas.is_empty()
            || self
                .session_row_areas
                .iter()
                .any(|(visible_index, _)| *visible_index == index)
    }

    /// Whether a change to a support projection can be seen on the dashboard.
    /// Minimized panes still show their one-line summary, so all three panes
    /// count as visible whenever the dashboard has a workspace selected. A
    /// target-capacity sample is also used by the New and Resume wizards.
    pub(crate) fn support_projection_visible(&self, pane: SupportPane) -> bool {
        if self.active_workspace_id().is_none() {
            return false;
        }
        if matches!(self.mode, Mode::New(_) | Mode::Resume(_)) {
            return pane == SupportPane::Targets;
        }
        if self.modal_open() {
            return false;
        }
        self.pane_areas.is_none_or(|areas| {
            let index = match pane {
                SupportPane::Sessions => 0,
                SupportPane::Targets => 1,
                SupportPane::Quota => 2,
            };
            let area = areas[index];
            area.width > 0 && area.height > 0
        })
    }

    /// Capture the fields whose state projection can alter a visible Sessions
    /// row. Hidden workspaces never enter this list, so a complete daemon
    /// snapshot can update them without causing an unnecessary frame.
    pub(crate) fn visible_state_signature(&self) -> Vec<VisibleStateSignature> {
        let Some(workspace_id) = self.active_workspace_id() else {
            return Vec::new();
        };
        self.ordered_sessions()
            .into_iter()
            .enumerate()
            .filter(|(index, session)| {
                session.workspace_id == workspace_id && self.session_row_is_visible_at(*index)
            })
            .map(|(_, session)| session)
            .map(|session| VisibleStateSignature {
                id: session.id.clone(),
                workspace_id: session.workspace_id.clone(),
                state: session.state,
                display_title: session.display_title().to_owned(),
                profile: session.last_profile.clone(),
                target: session.target_template_id.clone(),
                created_at: session.created_at.clone(),
                updated_at: session.updated_at.clone(),
                last_error: session.last_error.clone(),
                last_checkpoint_error: session.last_checkpoint_error.clone(),
                checkpoint_created_at: session
                    .checkpoint
                    .as_ref()
                    .map(|checkpoint| checkpoint.created_at.clone()),
                project_source: self.project_source(session),
                materialized: self
                    .session_details
                    .get(&session.id)
                    .map(materialized_display_signature),
            })
            .collect()
    }

    fn session_clock_signature(&self, now: u64) -> Vec<DisplayedClock> {
        let mut clocks = Vec::new();
        for (index, session) in self.ordered_sessions().into_iter().enumerate() {
            if !self.session_row_is_visible_at(index) {
                continue;
            }
            let detail = self.session_details.get(&session.id);
            let operation = self.session_operations.get(&session.id);
            if let Some(value) = session_display_clock(
                self,
                session,
                detail,
                self.session_review(&session.id),
                self.unreachable_sessions.contains(&session.id),
                operation,
                now,
            ) {
                clocks.push(DisplayedClock {
                    key: format!("session:{}:elapsed", session.id),
                    value,
                });
            }
            if let Some(value) = recovery_age_clock(session, self, now) {
                clocks.push(DisplayedClock {
                    key: format!("session:{}:recovery", session.id),
                    value,
                });
            }
        }
        clocks
    }

    fn capacity_clock_signature(&self, now: u64) -> Vec<DisplayedClock> {
        if !self.support_projection_visible(SupportPane::Targets) {
            return Vec::new();
        }
        self.capacity_details
            .iter()
            .filter_map(|(id, detail)| {
                let sampled_at = detail.sampled_at_epoch_seconds?;
                if detail.probe_error.is_some()
                    || now.saturating_sub(sampled_at) <= CAPACITY_SAMPLE_STALE_AFTER_SECONDS
                {
                    return None;
                }
                Some(DisplayedClock {
                    key: format!("capacity:{id}"),
                    value: refresh_age(now, sampled_at),
                })
            })
            .collect()
    }

    fn quota_clock_signature(&self, now: u64) -> Vec<DisplayedClock> {
        let mut clocks = Vec::new();
        if !self.support_projection_visible(SupportPane::Quota) {
            return clocks;
        }
        if self.quota_refreshing.is_empty()
            && let Some(refreshed) = self
                .quotas
                .values()
                .map(|quota| quota.refreshed_at_epoch_seconds)
                .min()
        {
            clocks.push(DisplayedClock {
                key: "quota:refreshed".to_owned(),
                value: refresh_age(now, refreshed),
            });
        }

        for (id, quota) in &self.quotas {
            if quota.error.is_some() {
                continue;
            }
            let (weekly, five_hour) = quota_reset_cells(quota, now);
            if quota
                .weekly_window()
                .is_some_and(|window| window.resets_at_epoch_seconds.is_some())
            {
                clocks.push(DisplayedClock {
                    key: format!("quota:{id}:weekly"),
                    value: weekly,
                });
            }
            let weekly_exhausted = quota
                .weekly_window()
                .and_then(|window| window.remaining_percent)
                .is_some_and(|remaining| remaining < 1);
            if !weekly_exhausted
                && quota
                    .five_hour_window()
                    .is_some_and(|window| window.resets_at_epoch_seconds.is_some())
            {
                clocks.push(DisplayedClock {
                    key: format!("quota:{id}:five-hour"),
                    value: five_hour,
                });
            }
        }
        clocks
    }

    fn resume_clock_signature(&self) -> Vec<DisplayedClock> {
        let Mode::ResumeDialog(dialog) = &self.mode else {
            return Vec::new();
        };
        let now = chrono::Local::now();
        let offset = dialog
            .form
            .borrow()
            .list_offset(crate::resume::ResumeFocus::Sessions);
        let height = self
            .frame_surfaces
            .surface(mj_chat::hel_selection::SurfaceId::ResumeList)
            .map_or(usize::MAX, |surface| {
                usize::from(surface.rect.height.saturating_sub(1))
            });
        self.resume_rows
            .iter()
            .enumerate()
            .skip(offset)
            .take(height)
            .map(|(index, row)| DisplayedClock {
                key: index.to_string(),
                value: crate::resume::format_last_active(&now, row.last_activity_ms),
            })
            .collect()
    }

    fn visible_animation_active(&self) -> bool {
        let sessions = self
            .ordered_sessions()
            .into_iter()
            .enumerate()
            .any(|(index, session)| {
                if !self.session_row_is_visible_at(index) {
                    return false;
                }
                let detail = self.session_details.get(&session.id);
                let review = self.session_review(&session.id);
                let primary = session.state == SessionState::Running
                    && !self.unreachable_sessions.contains(&session.id)
                    && detail.is_some_and(|detail| {
                        detail.activity.is_working(
                            detail.current_turn_started_at,
                            !detail.pending_elicitations.is_empty(),
                        )
                    });
                let transition = self
                    .transition_kind(&session.id)
                    .is_some_and(|_| session.last_error.is_none());
                primary || transition || review.is_some_and(RuntimeReviewView::is_working)
            });
        let opening = self
            .opening_session()
            .is_some_and(|session_id| self.session_is_visible(session_id));
        let modal = match &self.mode {
            Mode::TargetActions(dialog) => dialog.testing.is_some(),
            _ => false,
        };
        sessions || opening || modal
    }
}

fn recovery_age_clock(
    session: &hel::hel_state::SessionRecord,
    dashboard: &DashboardState,
    now: u64,
) -> Option<String> {
    if session.last_checkpoint_error.is_none()
        || dashboard.transition_kind(&session.id).is_some()
        || dashboard.transition_failure_kind(&session.id).is_some()
    {
        return None;
    }
    let checkpoint = session.checkpoint.as_ref()?;
    Some(checkpoint_age(now, &checkpoint.created_at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionOperationKind;
    use crate::test_support::{dashboard_with_session, running_session, stopped_session};

    #[test]
    fn idle_dashboard_has_no_clock_to_advance() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.acknowledge_render();

        assert!(!dashboard.clock_changed());
    }

    #[test]
    fn visible_operation_clock_advances_without_rendering() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_session_operation_at(
            "session-1".into(),
            SessionOperationKind::Launching,
            None,
            epoch_seconds().saturating_sub(1),
        );
        dashboard.acknowledge_render();
        assert!(!dashboard.clock_changed());

        dashboard.render_change_snapshot.clock.sessions[0].value = "0s".into();
        assert!(dashboard.clock_changed());
    }
}
