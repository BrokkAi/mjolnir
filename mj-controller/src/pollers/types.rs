use super::*;

#[derive(Debug, Clone, Default)]
pub struct QuotaRefreshBatch {
    pub generation: u64,
    pub profiles: Vec<QuotaRefreshRequest>,
}

#[derive(Debug)]
pub enum QuotaUpdate {
    Refreshing { profile_ids: Vec<String> },
    Report(QuotaRefreshOutcome),
    Finished { generation: u64 },
}

pub type WorkerPollTarget = RelaySessionTarget;
pub type WorkerPollUpdate = SessionManagerUpdate;

#[derive(Debug)]
pub(super) struct WorkerDiagnosisEpisode {
    pub(super) id: u64,
    pub(super) error: String,
    pub(super) diagnosed: bool,
}

#[derive(Debug, Default)]
pub struct WorkerDiagnosisTracker {
    pub(super) next_episode: u64,
    pub(super) current: std::collections::BTreeMap<String, WorkerDiagnosisEpisode>,
    pub(super) pending: std::collections::BTreeMap<String, u64>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct WorkerDiagnosisCompletion {
    pub display_error: Option<String>,
    pub restart_episode: Option<u64>,
}

impl WorkerDiagnosisTracker {
    pub fn observe(
        &mut self,
        session_id: &str,
        connected: bool,
        error: Option<String>,
    ) -> Option<u64> {
        if connected || error.is_none() {
            self.current.remove(session_id);
        }
        let error = error?;
        let episode = self
            .current
            .entry(session_id.to_owned())
            .or_insert_with(|| {
                self.next_episode = self.next_episode.wrapping_add(1).max(1);
                WorkerDiagnosisEpisode {
                    id: self.next_episode,
                    error: error.clone(),
                    diagnosed: false,
                }
            });
        episode.error = error;
        if episode.diagnosed || self.pending.contains_key(session_id) {
            return None;
        }
        self.pending.insert(session_id.to_owned(), episode.id);
        Some(episode.id)
    }

    pub fn finish(&mut self, session_id: &str, episode_id: u64) -> WorkerDiagnosisCompletion {
        if self.pending.get(session_id) != Some(&episode_id) {
            return WorkerDiagnosisCompletion::default();
        }
        self.pending.remove(session_id);
        let Some(current) = self.current.get_mut(session_id) else {
            return WorkerDiagnosisCompletion::default();
        };
        if current.id == episode_id {
            current.diagnosed = true;
            return WorkerDiagnosisCompletion {
                display_error: Some(current.error.clone()),
                restart_episode: None,
            };
        }
        if !current.diagnosed {
            self.pending.insert(session_id.to_owned(), current.id);
            return WorkerDiagnosisCompletion {
                display_error: None,
                restart_episode: Some(current.id),
            };
        }
        WorkerDiagnosisCompletion::default()
    }
}

#[derive(Debug, Clone)]
pub struct ResourcePollTarget {
    pub(super) session_id: String,
    pub(super) probe: SessionResourceProbe,
}

#[derive(Debug)]
pub struct ResourcePollUpdate {
    pub session_id: String,
    pub usage: SessionResourceUsage,
}

#[derive(Debug)]
pub struct CapacityPollUpdate {
    pub target_id: String,
    pub result: std::result::Result<Option<DeploymentCapacityUsage>, String>,
    pub sampled_at_epoch_seconds: u64,
}

pub fn projected_queued_prompts(
    controller: &Controller,
) -> Result<std::collections::BTreeMap<String, Vec<mj_core::relay::QueuedPrompt>>> {
    let queues = crate::database::load_materialized_queued_prompts()?;
    Ok(controller
        .state
        .sessions
        .keys()
        .filter_map(|session_id| {
            queues
                .get(session_id)
                .map(|queue| (session_id.clone(), queued_prompt_entries(queue)))
        })
        .collect())
}
