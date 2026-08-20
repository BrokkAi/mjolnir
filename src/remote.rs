//! Compatibility re-exports for remote-control support.

pub use mj_remote::*;

use anyhow::Context;

pub fn ragnarok_record_from_observation(
    observation: crate::session_state::RagnarokObservation,
) -> RagnarokRecord {
    let adoption_hint = ragnarok_adoption_hint(&observation);
    let fighters = observation
        .fighters
        .into_iter()
        .map(|fighter| {
            let (status, vigor) = ragnarok_fighter_status(&fighter.state);
            RagnarokFighterRecord {
                id: fighter.id,
                source: fighter.agent_source_id,
                model: fighter.model_name,
                status,
                vigor,
            }
        })
        .collect();
    let verdict = observation.verdict.map(|verdict| RagnarokVerdictRecord {
        clear_winner: verdict.clear_winner,
        finalists: verdict.finalists,
        ranking: verdict.ranking,
        reasoning: verdict.reasoning,
        thor_fallback: verdict.thor_fallback,
        chosen_finalist: observation.chosen_finalist,
    });
    RagnarokRecord {
        task: observation.task,
        phase: ragnarok_phase_id(observation.phase).to_string(),
        awaiting_approval: observation.awaiting_approval,
        fighters,
        verdict,
        adoption_hint,
        failed: observation.failed,
        done: observation.done,
    }
}

fn ragnarok_phase_id(phase: crate::ragnarok::Phase) -> &'static str {
    match phase {
        crate::ragnarok::Phase::Mustering => "mustering",
        crate::ragnarok::Phase::Routing => "routing",
        crate::ragnarok::Phase::Approval => "approval",
        crate::ragnarok::Phase::Combat => "combat",
        crate::ragnarok::Phase::Review => "review",
        crate::ragnarok::Phase::Judgment => "judgment",
        crate::ragnarok::Phase::Verdict => "verdict",
    }
}

fn ragnarok_fighter_status(state: &crate::ragnarok::FighterState) -> (String, String) {
    match state {
        crate::ragnarok::FighterState::Summoned => ("summoned".into(), "waiting".into()),
        crate::ragnarok::FighterState::Forging => ("forging camp".into(), "waiting".into()),
        crate::ragnarok::FighterState::Connecting => ("approaching".into(), "waiting".into()),
        crate::ragnarok::FighterState::Fighting => ("fighting".into(), "active".into()),
        crate::ragnarok::FighterState::Capturing => ("tallying".into(), "active".into()),
        crate::ragnarok::FighterState::Standing => ("standing".into(), "full".into()),
        crate::ragnarok::FighterState::Slain(reason) => {
            (format!("slain: {reason}"), "empty".into())
        }
    }
}

fn ragnarok_adoption_hint(
    observation: &crate::session_state::RagnarokObservation,
) -> Option<String> {
    use crate::session_state::RagnarokDraftPrStatus;

    if let Some(status) = observation.draft_pr_status.as_ref() {
        return Some(match status {
            RagnarokDraftPrStatus::Publishing { winner } => {
                format!(
                    "Publishing a draft PR for {}.",
                    ragnarok_fighter_name(observation, *winner)
                )
            }
            RagnarokDraftPrStatus::Published { winner, url } => format!(
                "Draft PR for {}: {url}",
                ragnarok_fighter_name(observation, *winner)
            ),
            RagnarokDraftPrStatus::Failed { winner, message } => format!(
                "Draft PR for {} failed: {message}",
                ragnarok_fighter_name(observation, *winner)
            ),
        });
    }

    let recommended = observation.chosen_finalist.or_else(|| {
        observation
            .verdict
            .as_ref()
            .and_then(|verdict| verdict.clear_winner)
    });
    if let Some(winner) = recommended {
        let fighter = observation
            .fighters
            .iter()
            .find(|fighter| fighter.id == winner)?;
        return Some(fighter.worktree_name.as_ref().map_or_else(
            || {
                format!(
                    "{} is selected; its worktree is not ready yet.",
                    fighter.model_name
                )
            },
            |worktree| {
                format!(
                    "Adopt {} with `mj --worktree {worktree}`.",
                    fighter.model_name
                )
            },
        ));
    }
    observation
        .verdict
        .as_ref()
        .and_then(|verdict| verdict.finalists)
        .map(|_| "Choose a finalist in the local TUI before adopting a worktree.".to_string())
}

fn ragnarok_fighter_name(
    observation: &crate::session_state::RagnarokObservation,
    id: crate::ragnarok::FighterId,
) -> String {
    observation
        .fighters
        .iter()
        .find(|fighter| fighter.id == id)
        .map(|fighter| fighter.model_name.clone())
        .unwrap_or_else(|| format!("champion {id}"))
}

#[derive(Debug)]
pub struct ServerOptions {
    pub hostname: Option<String>,
    pub tailscale_detect: bool,
    pub port: u16,
    pub history_days: u32,
    pub session_ttl_days: u32,
    pub logout_all: bool,
    pub cwd: std::path::PathBuf,
    pub additional_directories: Vec<std::path::PathBuf>,
    pub snapshot_exclusions: Vec<std::path::PathBuf>,
    pub fs_max_text_bytes: u64,
    pub termination: tokio_util::sync::CancellationToken,
}

pub async fn run_server(options: ServerOptions) -> anyhow::Result<()> {
    let config_path = crate::config::default_config_path();
    let mut cfg = crate::config::Config::load(&config_path)
        .with_context(|| format!("load {}", config_path.display()))?;
    cfg.apply_default_team();
    let resolved = crate::roster::resolve(&cfg, &options.cwd).await?;
    let session_manager =
        std::sync::Arc::new(crate::remote_host::RootServerSessionManager::new_roster(
            resolved.clone(),
            crate::remote_host::config_file_hash(&config_path),
            options.cwd.clone(),
            options.additional_directories.clone(),
            options.snapshot_exclusions.clone(),
            options.fs_max_text_bytes,
        ));
    run_server_runtime(RuntimeServerOptions {
        config: cfg,
        roster: resolved,
        hostname: options.hostname,
        tailscale_detect: options.tailscale_detect,
        port: options.port,
        history_days: options.history_days,
        session_ttl_days: options.session_ttl_days,
        logout_all: options.logout_all,
        cwd: options.cwd,
        additional_directories: options.additional_directories,
        snapshot_exclusions: options.snapshot_exclusions,
        fs_max_text_bytes: options.fs_max_text_bytes,
        session_manager,
        termination: options.termination,
    })
    .await
}
