//! Thor advisor mode: a transcript-first orchestrator for normal `mj` turns.
//!
//! V1 keeps the UI simple. Rust owns the workflow phases, while transcript
//! entries show Thor routing, worker output, review, judged findings, fixes,
//! and the final response.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionUpdate, StopReason, TextContent, ToolCallStatus, ToolKind,
};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::{mpsc, watch};

use crate::acp;
use crate::config::Config;
use crate::event::{PromptImage, UiEvent};
use crate::ragnarok::{self, AgentHandle, BattleConfig, Candidate, FighterCard, Launch, TurnEvent};
use crate::scores::ScoreStore;

const THOR_ROUTE_TIMEOUT: Duration = Duration::from_secs(120);
const THOR_JUDGE_TIMEOUT: Duration = Duration::from_secs(180);
const THOR_FINAL_TIMEOUT: Duration = Duration::from_secs(180);
const WORKER_TIMEOUT: Duration = Duration::from_secs(900);
const REVIEW_TIMEOUT: Duration = Duration::from_secs(420);
const FIX_TIMEOUT: Duration = Duration::from_secs(600);
const SNAPSHOT_LIMIT: usize = 20_000;
pub(crate) const ADVISOR_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const SUMMARY_LIMIT: usize = 20_000;

#[derive(Debug, Clone)]
pub(crate) struct AdvisorConfig {
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub config_path: PathBuf,
    pub score_store: ScoreStore,
    pub thor_agent_source_id: String,
    pub thor_launch: Launch,
}

#[derive(Debug, Clone)]
struct SelectedRole {
    card: FighterCard,
    launch: Launch,
    model_value: Option<String>,
}

impl SelectedRole {
    fn from_candidate(candidate: Candidate) -> Self {
        Self {
            model_value: Some(candidate.card.model_value.clone()),
            launch: candidate.launch,
            card: candidate.card,
        }
    }

    fn fallback(agent_source_id: String, launch: Launch, role_name: &str) -> Self {
        Self {
            card: FighterCard {
                id: 0,
                agent_source_id,
                model_value: "current".to_string(),
                model_name: role_name.to_string(),
                elo: 0,
                provisional: false,
            },
            launch,
            model_value: None,
        }
    }

    fn tag(&self) -> String {
        if self.card.elo == 0 {
            format!("{} [{}]", self.card.model_name, self.card.agent_source_id)
        } else {
            self.card.tag()
        }
    }
}

#[derive(Debug, Deserialize)]
struct RouteDecision {
    action: String,
    rationale: Option<String>,
    answer: Option<String>,
    task: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ValidFinding {
    summary: String,
    #[serde(default)]
    evidence: String,
    instruction: String,
}

#[derive(Debug, Deserialize)]
struct ReviewJudgment {
    #[serde(default)]
    valid_findings: Vec<ValidFinding>,
    #[serde(default)]
    invalid_findings: Vec<String>,
    rationale: Option<String>,
}

pub(crate) async fn run_turn(
    cfg: AdvisorConfig,
    user_prompt: String,
    images: Vec<PromptImage>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    abort: watch::Receiver<bool>,
) -> Result<()> {
    emit_info(&ui_tx, "Thor advisor: considering the request");

    let mut thor = connect_role(
        SelectedRole::fallback(
            cfg.thor_agent_source_id.clone(),
            cfg.thor_launch.clone(),
            "Thor",
        ),
        &cfg,
        abort.clone(),
        acp::RuntimeAccessMode::ReadOnly,
    )
    .await
    .context("Thor could not start")?;

    let route_text = thor
        .prompt_with_images(
            route_prompt(&user_prompt, !images.is_empty()),
            images.clone(),
            THOR_ROUTE_TIMEOUT,
            |_| {},
        )
        .await;
    let route_text = match route_text {
        Ok(outcome) => outcome.text,
        Err(e) => {
            thor.dismiss().await;
            return Err(e).context("Thor routing failed");
        }
    };
    let Some(route) = parse_json_object::<RouteDecision>(&route_text) else {
        emit_agent_text(&ui_tx, route_text);
        thor.dismiss().await;
        return Ok(());
    };

    if !route.action.eq_ignore_ascii_case("delegate") {
        let answer = route
            .answer
            .unwrap_or_else(|| route_text.trim().to_string());
        emit_agent_text(&ui_tx, answer);
        thor.dismiss().await;
        return Ok(());
    }

    let task = route.task.unwrap_or_else(|| user_prompt.clone());
    let rationale = route
        .rationale
        .unwrap_or_else(|| "Thor judged this to be an implementation task".to_string());

    let (worker, reviewer) = tokio::select! {
        result = select_worker_and_reviewer(&cfg, &task) => result?,
        _ = wait_abort(abort.clone()) => bail!("advisor turn cancelled"),
    };
    emit_info(
        &ui_tx,
        format!(
            "Thor advisor: delegated to {} ({})",
            worker.tag(),
            rationale
        ),
    );

    let before = capture_workspace_snapshot(&cfg).await;
    let mut worker_handle = connect_role(
        worker.clone(),
        &cfg,
        abort.clone(),
        acp::RuntimeAccessMode::Full,
    )
    .await
    .with_context(|| format!("worker {} could not start", worker.tag()))?;

    emit_info(&ui_tx, format!("worker: {}", worker.tag()));
    let worker_summary = worker_handle
        .prompt_with_images(worker_prompt(&task), images, WORKER_TIMEOUT, |ev| {
            forward_turn_event(&ui_tx, "worker", ev);
        })
        .await;
    worker_handle.dismiss().await;
    let worker_summary = worker_summary.context("worker turn failed")?;
    if !turn_succeeded(worker_summary.stop) {
        bail!(
            "worker stopped before completing the task: {:?}",
            worker_summary.stop
        );
    }

    let after_worker = capture_workspace_snapshot(&cfg).await;
    emit_info(&ui_tx, format!("adversarial reviewer: {}", reviewer.tag()));
    let mut reviewer_handle = connect_role(
        reviewer.clone(),
        &cfg,
        abort.clone(),
        acp::RuntimeAccessMode::ReadOnly,
    )
    .await
    .with_context(|| format!("reviewer {} could not start", reviewer.tag()))?;
    let review_text = reviewer_handle
        .prompt(
            review_prompt(
                &task,
                &worker,
                &reviewer,
                &worker_summary.text,
                before.as_deref(),
                after_worker.as_deref(),
            ),
            REVIEW_TIMEOUT,
            |ev| forward_turn_event(&ui_tx, "reviewer", ev),
        )
        .await;
    reviewer_handle.dismiss().await;
    let review_text = review_text.context("review turn failed")?.text;

    emit_info(&ui_tx, "Thor advisor: judging the review");
    let judgment_text = thor
        .prompt(
            judgment_prompt(&task, &worker_summary.text, &review_text),
            THOR_JUDGE_TIMEOUT,
            |_| {},
        )
        .await;
    let judgment_text = match judgment_text {
        Ok(outcome) => outcome.text,
        Err(e) => {
            thor.dismiss().await;
            return Err(e).context("Thor review judgment failed");
        }
    };
    let judgment =
        parse_json_object::<ReviewJudgment>(&judgment_text).unwrap_or_else(|| ReviewJudgment {
            valid_findings: Vec::new(),
            invalid_findings: vec!["Thor returned an unparseable judgment".to_string()],
            rationale: Some(judgment_text.clone()),
        });
    emit_agent_text(&ui_tx, judgment_summary(&judgment));

    let fix_summary = if judgment.valid_findings.is_empty() {
        emit_info(&ui_tx, "Thor advisor: no valid review findings to fix");
        String::new()
    } else {
        emit_info(&ui_tx, format!("fix pass: {}", worker.tag()));
        let mut fixer = connect_role(
            worker.clone(),
            &cfg,
            abort.clone(),
            acp::RuntimeAccessMode::Full,
        )
        .await
        .with_context(|| format!("fix worker {} could not start", worker.tag()))?;
        let outcome = fixer
            .prompt(fix_prompt(&task, &judgment), FIX_TIMEOUT, |ev| {
                forward_turn_event(&ui_tx, "fix", ev)
            })
            .await;
        fixer.dismiss().await;
        let outcome = outcome.context("fix turn failed")?;
        outcome.text
    };

    let after_fix = capture_workspace_snapshot(&cfg).await;
    emit_info(&ui_tx, "Thor advisor: final response");
    let final_result = thor
        .prompt(
            final_prompt(
                &task,
                &worker_summary.text,
                &review_text,
                &judgment,
                &fix_summary,
                after_fix.as_deref(),
            ),
            THOR_FINAL_TIMEOUT,
            |ev| forward_turn_event(&ui_tx, "thor", ev),
        )
        .await;
    thor.dismiss().await;
    final_result.context("Thor final response failed")?;

    Ok(())
}

async fn connect_role(
    role: SelectedRole,
    cfg: &AdvisorConfig,
    abort: watch::Receiver<bool>,
    access_mode: acp::RuntimeAccessMode,
) -> Result<AgentHandle> {
    let saved_session_config = saved_session_config(&cfg.config_path, &role.card.agent_source_id);
    let mut handle = AgentHandle::connect_with_saved_session_config(
        &role.launch,
        &cfg.cwd,
        &cfg.additional_directories,
        abort,
        access_mode,
        saved_session_config,
    )
    .await?;
    if let Some(model_value) = role.model_value.as_deref() {
        handle.arm_model(model_value).await?;
    }
    Ok(handle)
}

fn saved_session_config(
    config_path: &Path,
    agent_source_id: &str,
) -> std::collections::HashMap<String, String> {
    Config::load(config_path)
        .ok()
        .and_then(|cfg| cfg.session_config.get(agent_source_id).cloned())
        .unwrap_or_default()
}

async fn select_worker_and_reviewer(
    cfg: &AdvisorConfig,
    task: &str,
) -> Result<(SelectedRole, SelectedRole)> {
    let user_cfg = Config::load(&cfg.config_path)
        .with_context(|| format!("load {}", cfg.config_path.display()))?;
    let store = ragnarok::ensure_scores(&cfg.score_store, &user_cfg).await;
    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let battle_cfg = BattleConfig {
        task: task.to_string(),
        cwd: cfg.cwd.clone(),
        config_path: cfg.config_path.clone(),
        score_store: store.clone(),
        thor_host: None,
    };
    let mut pool = ragnarok::muster(&battle_cfg, &user_cfg, &store, &events_tx)
        .await
        .unwrap_or_default();
    for (idx, candidate) in pool.iter_mut().enumerate() {
        candidate.card.id = idx;
    }

    if let Some(worker_candidate) = ragnarok::select_fighters(&pool, 1).into_iter().next() {
        let reviewer_candidate = ragnarok::select_judge_only_reviewer(
            &pool,
            std::slice::from_ref(&worker_candidate),
            worker_candidate.card.id,
        )
        .or_else(|| {
            pool.iter()
                .find(|candidate| candidate.match_key != worker_candidate.match_key)
                .cloned()
        });
        let worker = SelectedRole::from_candidate(worker_candidate);
        let reviewer = reviewer_candidate
            .map(SelectedRole::from_candidate)
            .unwrap_or_else(|| {
                SelectedRole::fallback(
                    cfg.thor_agent_source_id.clone(),
                    cfg.thor_launch.clone(),
                    "Reviewer",
                )
            });
        return Ok((worker, reviewer));
    }

    let fallback_worker = SelectedRole::fallback(
        cfg.thor_agent_source_id.clone(),
        cfg.thor_launch.clone(),
        "Worker",
    );
    let fallback_reviewer = SelectedRole::fallback(
        cfg.thor_agent_source_id.clone(),
        cfg.thor_launch.clone(),
        "Reviewer",
    );
    Ok((fallback_worker, fallback_reviewer))
}

fn route_prompt(user_prompt: &str, has_images: bool) -> String {
    let image_note = if has_images {
        "\nThe user attached one or more images. Consider them part of the request. \
         If you delegate, the worker will receive the same images.\n"
    } else {
        ""
    };
    format!(
        "You are Thor advisor mode for mjolnir. Decide whether the user's request \
         should be answered directly by you or delegated to a worker agent.\n\n\
         Answer directly when the user asks a question, asks for explanation, \
         requests advice, or wants analysis without changing the workspace.\n\
         Delegate when the user asks for implementation, edits, test fixes, \
         investigation that should modify files, or any task that should be \
         completed by another model while you supervise.\n\n\
         You may inspect the repository if needed, but do not modify files. \
         Respond with ONLY one JSON object and no markdown:\n\
         {{\"action\":\"answer\",\"rationale\":\"<short>\",\"answer\":\"<your direct answer>\"}}\n\
         or\n\
         {{\"action\":\"delegate\",\"rationale\":\"<short>\",\"task\":\"<clear worker task>\"}}\n\n\
         {image_note}\
         USER REQUEST:\n{user_prompt}"
    )
}

fn worker_prompt(task: &str) -> String {
    format!(
        "THOR ADVISOR DELEGATION. Thor selected you to implement the user's task. \
         You are the worker, not the final judge.\n\n\
         Rules:\n\
         - Work in the current working directory.\n\
         - Do not create commits and do not push.\n\
         - Preserve unrelated user changes.\n\
         - Verify with focused commands when practical.\n\
         - Finish with a concise summary of changes and validation.\n\n\
         TASK:\n{task}"
    )
}

fn review_prompt(
    task: &str,
    worker: &SelectedRole,
    reviewer: &SelectedRole,
    worker_summary: &str,
    before: Option<&str>,
    after: Option<&str>,
) -> String {
    format!(
        "THOR ADVISOR ADVERSARIAL REVIEW. You are {reviewer}. Review the work \
         produced by {worker}. You are analysis-only: do not modify files. \
         No raw diffs are included; inspect the repository directly when you need details.\n\n\
         Check the implementation against the task, inspect files if needed, \
         and be adversarial but honest. Thor will judge whether your findings \
         are valid before sending fixes back to the worker.\n\n\
         Deliver exactly these sections:\n\
         VERDICT: SHIP IT | FLAWED | FATALLY FLAWED\n\
         VALID FINDINGS: numbered list with file:line or concrete evidence, or 'none found'\n\
         QUESTIONABLE CLAIMS: worker claims not backed by the code, or 'none'\n\
         STRENGTHS: short list\n\n\
         TASK:\n{task}\n\n\
         WORKER SUMMARY:\n{summary}\n\n\
         WORKSPACE SNAPSHOT BEFORE WORKER:\n{before}\n\n\
         WORKSPACE SNAPSHOT AFTER WORKER:\n{after}",
        reviewer = reviewer.tag(),
        worker = worker.tag(),
        summary = truncate_middle(worker_summary, SUMMARY_LIMIT),
        before = before.unwrap_or("(snapshot unavailable)"),
        after = after.unwrap_or("(snapshot unavailable)"),
    )
}

fn judgment_prompt(task: &str, worker_summary: &str, review: &str) -> String {
    format!(
        "You are Thor advisor mode. Judge the adversarial review below. \
         Decide which findings are valid and actionable for the original worker.\n\n\
         You may inspect the repository if needed, but do not modify files. \
         Reject findings that are speculative, unsupported, duplicate, irrelevant, \
         or not worth fixing for the user's task.\n\n\
         Respond with ONLY one JSON object and no markdown:\n\
         {{\"valid_findings\":[{{\"summary\":\"<finding>\",\"evidence\":\"<file/line or concrete evidence>\",\"instruction\":\"<exact fix instruction for worker>\"}}],\
         \"invalid_findings\":[\"<short reason>\"],\
         \"rationale\":\"<short judgment>\"}}\n\n\
         TASK:\n{task}\n\n\
         WORKER SUMMARY:\n{summary}\n\n\
         ADVERSARIAL REVIEW:\n{review}",
        summary = truncate_middle(worker_summary, SUMMARY_LIMIT),
        review = truncate_middle(review, SUMMARY_LIMIT),
    )
}

fn fix_prompt(task: &str, judgment: &ReviewJudgment) -> String {
    let findings = judgment
        .valid_findings
        .iter()
        .enumerate()
        .map(|(idx, finding)| {
            format!(
                "{}. {}\nEvidence: {}\nInstruction: {}",
                idx + 1,
                finding.summary,
                finding.evidence,
                finding.instruction
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "THOR ADVISOR FIX PASS. Thor judged the review and selected the valid \
         findings below. Fix all of them and only them unless you discover a \
         directly related issue while editing.\n\n\
         Rules:\n\
         - Work in the current working directory.\n\
         - Do not create commits and do not push.\n\
         - Preserve unrelated user changes.\n\
         - Run focused validation when practical.\n\
         - Finish with a concise summary of fixes and validation.\n\n\
         ORIGINAL TASK:\n{task}\n\n\
         VALID FINDINGS TO FIX:\n{findings}"
    )
}

fn final_prompt(
    task: &str,
    worker_summary: &str,
    review: &str,
    judgment: &ReviewJudgment,
    fix_summary: &str,
    final_snapshot: Option<&str>,
) -> String {
    format!(
        "You are Thor advisor mode. Give the user the final concise response for \
         this completed workflow. Mention what changed, how it was reviewed, what \
         valid review findings were fixed, and any validation or remaining risk. \
         Do not expose raw JSON.\n\n\
         ORIGINAL TASK:\n{task}\n\n\
         WORKER SUMMARY:\n{worker_summary}\n\n\
         REVIEW:\n{review}\n\n\
         THOR JUDGMENT:\n{judgment}\n\n\
         FIX SUMMARY:\n{fix_summary}\n\n\
         FINAL WORKSPACE SNAPSHOT (status/stat summary only):\n{final_snapshot}",
        worker_summary = truncate_middle(worker_summary, SUMMARY_LIMIT),
        review = truncate_middle(review, SUMMARY_LIMIT),
        judgment = judgment_summary(judgment),
        fix_summary = truncate_middle(fix_summary, SUMMARY_LIMIT),
        final_snapshot = final_snapshot.unwrap_or("(snapshot unavailable)"),
    )
}

fn judgment_summary(judgment: &ReviewJudgment) -> String {
    let mut out = String::from("Thor judgment\n");
    if let Some(rationale) = &judgment.rationale {
        out.push_str(rationale.trim());
        out.push('\n');
    }
    if judgment.valid_findings.is_empty() {
        out.push_str("\nValid findings: none\n");
    } else {
        out.push_str("\nValid findings:\n");
        for (idx, finding) in judgment.valid_findings.iter().enumerate() {
            out.push_str(&format!(
                "{}. {} ({})\n   Fix: {}\n",
                idx + 1,
                finding.summary,
                finding.evidence,
                finding.instruction
            ));
        }
    }
    if !judgment.invalid_findings.is_empty() {
        out.push_str("\nRejected findings:\n");
        for finding in &judgment.invalid_findings {
            out.push_str("- ");
            out.push_str(finding.trim());
            out.push('\n');
        }
    }
    out
}

fn forward_turn_event(ui_tx: &mpsc::UnboundedSender<UiEvent>, role: &str, ev: TurnEvent) {
    match ev {
        TurnEvent::Message(text) => emit_agent_text(ui_tx, text),
        TurnEvent::Thought(text) => {
            let _ = ui_tx.send(UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
                text_chunk(text),
            )));
        }
        TurnEvent::Tool {
            title,
            kind,
            status,
            started,
        } => {
            if started || status == Some(ToolCallStatus::Failed) {
                emit_info(
                    ui_tx,
                    format!("{role} tool: {} ({})", title, tool_label(kind)),
                );
            }
        }
        TurnEvent::Permission { prompt, .. } => {
            let _ = ui_tx.send(UiEvent::PermissionRequest(*prompt));
        }
        TurnEvent::Note(note) => emit_info(ui_tx, format!("{role}: {note}")),
    }
}

fn tool_label(kind: Option<ToolKind>) -> &'static str {
    match kind {
        Some(kind) => crate::labels::tool_kind_label(kind),
        None => "tool",
    }
}

fn emit_agent_text(ui_tx: &mpsc::UnboundedSender<UiEvent>, text: impl Into<String>) {
    let _ = ui_tx.send(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
        text_chunk(text),
    )));
}

fn emit_info(ui_tx: &mpsc::UnboundedSender<UiEvent>, text: impl Into<String>) {
    let _ = ui_tx.send(UiEvent::Info(text.into()));
}

fn text_chunk(text: impl Into<String>) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text.into())))
}

fn parse_json_object<T: for<'de> Deserialize<'de>>(text: &str) -> Option<T> {
    let value = ragnarok::extract_json_object(text)?;
    serde_json::from_value(value).ok()
}

async fn capture_workspace_snapshot(cfg: &AdvisorConfig) -> Option<String> {
    let mut roots = Vec::with_capacity(cfg.additional_directories.len() + 1);
    roots.push(cfg.cwd.clone());
    roots.extend(cfg.additional_directories.iter().cloned());

    let mut sections = Vec::new();
    for root in roots {
        if let Some(section) = capture_root_snapshot(&root).await {
            sections.push(section);
        }
    }
    if sections.is_empty() {
        None
    } else {
        Some(truncate_middle(&sections.join("\n\n"), SNAPSHOT_LIMIT))
    }
}

async fn capture_root_snapshot(cwd: &PathBuf) -> Option<String> {
    let status = git_capture(cwd, &["status", "--short"]).await.ok()?;
    let diffstat = git_capture(cwd, &["diff", "--stat"])
        .await
        .unwrap_or_default();
    Some(format!(
        "workspace root: {}\ngit status --short:\n{}\n\ngit diff --stat:\n{}",
        cwd.display(),
        status.trim_end(),
        diffstat.trim_end()
    ))
}

async fn git_capture(cwd: &PathBuf, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn wait_abort(mut abort: watch::Receiver<bool>) {
    loop {
        if *abort.borrow() {
            return;
        }
        if abort.changed().await.is_err() {
            return;
        }
    }
}

fn turn_succeeded(stop: StopReason) -> bool {
    matches!(
        stop,
        StopReason::EndTurn | StopReason::MaxTokens | StopReason::MaxTurnRequests
    )
}

fn truncate_middle(text: &str, limit: usize) -> String {
    ragnarok::truncate_middle(text, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_route_json_inside_extra_text() {
        let parsed = parse_json_object::<RouteDecision>(
            "```json\n{\"action\":\"delegate\",\"task\":\"fix it\"}\n```",
        )
        .expect("route");
        assert_eq!(parsed.action, "delegate");
        assert_eq!(parsed.task.as_deref(), Some("fix it"));
    }

    #[test]
    fn parses_route_json_with_braces_inside_string() {
        let parsed = parse_json_object::<RouteDecision>(
            "note {\"action\":\"answer\",\"answer\":\"literal } brace\"} trailing {noise}",
        )
        .expect("route");
        assert_eq!(parsed.action, "answer");
        assert_eq!(parsed.answer.as_deref(), Some("literal } brace"));
    }

    #[test]
    fn judgment_summary_lists_valid_and_rejected_findings() {
        let summary = judgment_summary(&ReviewJudgment {
            valid_findings: vec![ValidFinding {
                summary: "bug".into(),
                evidence: "src/lib.rs:1".into(),
                instruction: "fix bug".into(),
            }],
            invalid_findings: vec!["not relevant".into()],
            rationale: Some("valid review".into()),
        });
        assert!(summary.contains("Valid findings"));
        assert!(summary.contains("fix bug"));
        assert!(summary.contains("Rejected findings"));
    }

    #[test]
    fn truncate_middle_respects_char_boundaries() {
        let text = "⚡".repeat(100);
        let out = truncate_middle(&text, 50);

        assert!(out.contains("excised"));
        assert!(out.chars().count() > 0);
    }

    #[test]
    fn saved_session_config_loads_agent_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.session_config.insert(
            "agent-a".to_string(),
            std::collections::HashMap::from([("config:model".to_string(), "opus".to_string())]),
        );
        cfg.save(&path).expect("save");

        let loaded = saved_session_config(&path, "agent-a");

        assert_eq!(loaded.get("config:model").map(String::as_str), Some("opus"));
        assert!(saved_session_config(&path, "agent-b").is_empty());
    }

    #[tokio::test]
    async fn wait_abort_resolves_when_sender_closes() {
        let (tx, rx) = watch::channel(false);
        drop(tx);

        tokio::time::timeout(Duration::from_secs(1), wait_abort(rx))
            .await
            .expect("abort wait should resolve");
    }
}
