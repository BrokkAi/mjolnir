//! ⚡ Ragnarok: model-vs-model combat for one implementation task.
//!
//! `/ragnarok <task>` summons THOR, a router agent running on the
//! strongest DeepSWE-ranked model available. Thor sizes up the task and decrees
//! how many champions battle (2–10). Each champion is a distinct model —
//! ideally from distinct providers — chosen by Pass@1 from the shared Council
//! catalog. Unranked models are not eligible. Every champion implements the task in
//! parallel inside its own git worktree with permissions bypassed, then each
//! is assigned a rival's implementation to adversarially review (never their
//! own). Thor judges the reviews for honesty and validity, ranks the
//! implementations against the original task, and crowns a clear winner — or
//! presents two finalists for the user to choose between.
//!
//! Architecture: [`run_battle`] is a background tokio task owning one ACP
//! connection per champion/reviewer plus one for Thor (the same in-process
//! `acp::run` runtime the TUI uses). It streams [`RagnarokEvent`]s
//! to the UI over an unbounded channel; the arena view in `ui.rs` renders the
//! battle. Dropping the UI receiver or firing the abort watch ends the battle
//! and tears down every agent subprocess.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_client_protocol::schema::v1::{
    SessionConfigOption, SessionUpdate, StopReason, ToolCallStatus, ToolKind,
};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{mpsc, watch};

use crate::acp;
use crate::council;
use crate::event::{
    ElicitationOutcome, PermissionDecision, SessionConfigTarget, UiCommand, UiEvent,
    content_block_text,
};
use crate::headless::choose_allow_option;
use crate::labels::stop_reason_label;
use crate::worktree;

/// Thor may field at most this many champions.
pub const MAX_FIGHTERS: usize = 10;
/// ... and no fewer than this many.
pub const MIN_FIGHTERS: usize = 2;

/// Diversity bonus for a model vendor not already represented in the roster.
const NEW_VENDOR_SELECTION_BONUS: i32 = 50;
/// Judge-only replacements should be especially unlike the sole survivor.
const JUDGE_ONLY_VENDOR_SELECTION_BONUS: i32 = NEW_VENDOR_SELECTION_BONUS * 2;
/// After scoring with penalties/bonuses, pick randomly from this many top rows.
const SELECTION_RANDOM_TOP_N: usize = 4;
/// Budget for an agent to reach `SessionStarted` (covers cold npx/uvx runs).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(180);
/// Budget for one champion to implement the task.
const FIGHT_TIMEOUT: Duration = Duration::from_secs(45 * 60);
/// A no-diff champion gets one explicit second chance before the tournament
/// accepts that it still produced no artifact.
const EMPTY_DIFF_CONTINUATION_LIMIT: usize = 1;
/// Budget for one adversarial review.
const REVIEW_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Budget for each of Thor's pronouncements (route / assign / judge).
const THOR_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long to wait for the session config options needed to arm a model.
const CONFIG_OPTIONS_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to wait for the runtime to confirm a model selection. The
/// runtime rejects prompts while a config update is in flight, so arming
/// must not return before the confirmation lands.
const CONFIG_UPDATE_TIMEOUT: Duration = Duration::from_secs(30);
/// Defense in depth: how many times one turn re-sends a prompt the runtime
/// rejected with "config update already in flight" (250ms apart).
const PROMPT_RESEND_LIMIT: usize = 20;

/// Straggler judgment never begins before this much combat time has passed…
const STRAGGLER_MIN_ELAPSED: Duration = Duration::from_secs(4 * 60);
/// …or before a fighter has burned this multiple of the median finisher's
/// time (only fighters who finished cleanly count toward the median).
const STRAGGLER_MULT: u32 = 3;
/// How often the combat watchdog looks for stragglers.
const STRAGGLER_CHECK_EVERY: Duration = Duration::from_secs(20);
/// Budget for Thor's mid-combat mercy ruling.
const MERCY_TURN_TIMEOUT: Duration = Duration::from_secs(180);
/// Recent tool actions remembered per fighter for loop detection.
const RECENT_ACTIONS_CAP: usize = 30;

/// Cap on a captured `git diff` artifact.
const DIFF_CAPTURE_LIMIT: usize = 256 * 1024;
/// Cap on captured diffstat/error streams before they reach the UI.
const DIFFSTAT_CAPTURE_LIMIT: usize = 64 * 1024;
const GIT_STDERR_CAPTURE_LIMIT: usize = 64 * 1024;
/// Diff budget inside a reviewer's prompt.
const DIFF_FOR_REVIEW_LIMIT: usize = 24 * 1024;
/// Per-champion diff budget inside Thor's judgment dossier.
const DIFF_FOR_JUDGMENT_LIMIT: usize = 10 * 1024;
/// Per-review budget inside Thor's judgment dossier.
const REVIEW_FOR_JUDGMENT_LIMIT: usize = 8 * 1024;
/// Per-champion closing-summary budget inside prompts.
const SUMMARY_LIMIT: usize = 4 * 1024;
/// Cap on accumulated agent text per turn.
const FINAL_TEXT_LIMIT: usize = 1024 * 1024;
/// Keep synthetic outbound prompt markers readable in fighter transcripts.
const PROMPT_MARKER_TASK_LIMIT: usize = 800;

// ---------------------------------------------------------------------------
// Events consumed by the arena UI
// ---------------------------------------------------------------------------

pub type FighterId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Mustering,
    Routing,
    /// The roster is chosen; combat waits for the user to unleash it (the
    /// one moment to balk at the bill before champions start burning tokens).
    Approval,
    Combat,
    Review,
    Judgment,
    Verdict,
}

impl Phase {
    pub fn banner(self) -> &'static str {
        match self {
            Phase::Mustering => "MUSTERING THE CHAMPIONS",
            Phase::Routing => "THOR WEIGHS THE TASK",
            Phase::Approval => "THE MUSTER AWAITS YOUR COMMAND",
            Phase::Combat => "COMBAT",
            Phase::Review => "ADVERSARIAL REVIEW",
            Phase::Judgment => "THOR SITS IN JUDGMENT",
            Phase::Verdict => "VERDICT",
        }
    }
}

/// One champion on the roster card shown in the arena.
#[derive(Debug, Clone)]
pub struct FighterCard {
    pub id: FighterId,
    pub agent_source_id: String,
    pub model_value: String,
    pub model_name: String,
    pub pass_at_1_bps: u32,
    pub mean_cost_usd: f64,
}

impl FighterCard {
    /// `Opus [claude-acp] ⚡51.8% · $4.28`
    pub fn tag(&self) -> String {
        format!(
            "{} [{}] ⚡{:.1}% · ${:.2}",
            self.model_name,
            self.agent_source_id,
            self.pass_at_1_bps as f64 / 100.0,
            self.mean_cost_usd
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FighterState {
    Summoned,
    Forging,
    Connecting,
    Fighting,
    Capturing,
    Standing,
    Slain(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Forge,  // edit / delete / move
    Strike, // execute
    Scry,   // read / search / fetch
    Chant,  // agent message
    Ponder, // agent thought
    Wound,  // failed tool call
    Guard,  // permission auto-answered
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextLane {
    Message,
    Thought,
    Tool,
    Review,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewProgress {
    Connecting,
    Reviewing,
    Done,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThorAction {
    Descending,
    Deciding,
    Assigning,
    Judging,
    Mercy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assignment {
    pub reviewer: FighterId,
    pub defender: FighterId,
}

#[derive(Debug, Clone)]
pub struct ReviewVerdict {
    pub reviewer: FighterId,
    pub defender: FighterId,
    pub honesty: u8,
    pub validity: u8,
    pub notes: String,
}

#[derive(Debug, Clone)]
pub struct Verdict {
    pub clear_winner: Option<FighterId>,
    pub finalists: Option<(FighterId, FighterId)>,
    pub ranking: Vec<FighterId>,
    pub review_verdicts: Vec<ReviewVerdict>,
    pub reasoning: String,
    /// True when Thor's judgment was unusable and Pass@1 order decided instead.
    pub thor_fallback: bool,
}

/// Events streamed from the battle task into the arena UI.
#[derive(Debug)]
pub enum RagnarokEvent {
    Phase(Phase),
    /// A themed line for the combat feed.
    Log {
        fighter: Option<FighterId>,
        text: String,
    },
    /// Thor's streamed words (rationale, judgment prose).
    ThorSpeaks(String),
    /// Thor's current visible action for the arena animation.
    ThorAction(ThorAction),
    /// The chosen roster, in fighter-id order.
    Roster(Vec<FighterCard>),
    /// A late entrant that did not fight, but may adversarially review.
    FighterJoined(FighterCard),
    FighterState {
        id: FighterId,
        state: FighterState,
    },
    /// The champion's private worktree was forged.
    FighterWorktree {
        id: FighterId,
        name: String,
        path: PathBuf,
        base_sha: String,
    },
    FighterAction {
        id: FighterId,
        action: ActionKind,
        detail: String,
    },
    /// Raw transcript chunk for the per-fighter transcript pane.
    FighterText {
        id: FighterId,
        lane: TextLane,
        chunk: String,
    },
    FighterDiffStat {
        id: FighterId,
        stat: String,
    },
    Assignments(Vec<Assignment>),
    ReviewState {
        reviewer: FighterId,
        progress: ReviewProgress,
    },
    Verdict(Box<Verdict>),
    DraftPrPublishing {
        winner: FighterId,
    },
    DraftPrPublished {
        winner: FighterId,
        url: String,
    },
    DraftPrFailed {
        winner: FighterId,
        message: String,
    },
    Failed(String),
    Done,
}

// ---------------------------------------------------------------------------
// Battle configuration and entry point
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BattleConfig {
    /// The task prompt the user wants implemented.
    pub task: String,
    /// The current session cwd; worktrees are forged off its git project.
    pub cwd: PathBuf,
    /// Ranked launchable models from the session's unified Council catalog.
    pub available_models: Vec<council::ResolvedRole>,
    /// Active session agent/model used for Thor. Competitors are selected from
    /// the scored pool; Thor follows the user's current session config.
    pub thor_host: Option<ThorHost>,
}

/// Camps (Thor's own, and each champion's) forged over the course of a
/// battle. On a successful battle these are intentionally left behind so the
/// user can inspect or adopt them (`mj --worktree <name>`); on abort/failure
/// there is no winner to adopt, so every camp forged so far is swept away
/// here instead of leaking under `.mjolnir/worktrees/` forever.
type WorktreeRegistry = std::sync::Arc<std::sync::Mutex<Vec<worktree::CreatedWorktree>>>;

fn register_camp(registry: &WorktreeRegistry, camp: &worktree::CreatedWorktree) {
    if let Ok(mut camps) = registry.lock() {
        camps.push(camp.clone());
    }
}

async fn sweep_camps(registry: WorktreeRegistry) {
    let camps = registry
        .lock()
        .map(|mut camps| std::mem::take(&mut *camps))
        .unwrap_or_default();
    if camps.is_empty() {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || {
        for camp in camps {
            if let Err(e) =
                worktree::remove_automation_worktree(&camp.project_root, &camp.worktree_root)
            {
                tracing::warn!("sweep camp {:?}: {e:#}", camp.worktree_root);
            }
        }
    })
    .await;
}

/// Run one full battle. Never panics the UI: any error surfaces as
/// [`RagnarokEvent::Failed`], and [`RagnarokEvent::Done`] is always the final
/// event. `proceed` is the user's pre-combat approval (see [`Phase::Approval`]);
/// `abort` flees the battle at any point.
pub async fn run_battle(
    cfg: BattleConfig,
    tx: mpsc::UnboundedSender<RagnarokEvent>,
    abort: watch::Receiver<bool>,
    proceed: watch::Receiver<bool>,
) {
    let camps: WorktreeRegistry = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    if let Err(e) = battle(&cfg, &tx, abort, proceed, camps.clone()).await {
        sweep_camps(camps).await;
        let _ = tx.send(RagnarokEvent::Failed(format!("{e:#}")));
    }
    let _ = tx.send(RagnarokEvent::Done);
}

#[derive(Debug, Clone)]
pub struct DraftPrRequest {
    pub winner: FighterId,
    pub winner_tag: String,
    pub task: String,
    pub worktree_path: PathBuf,
    pub base_sha: String,
}

fn emit(tx: &mpsc::UnboundedSender<RagnarokEvent>, ev: RagnarokEvent) -> Result<()> {
    tx.send(ev)
        .map_err(|_| anyhow!("the arena was abandoned (ui closed)"))
}

fn feed(
    tx: &mpsc::UnboundedSender<RagnarokEvent>,
    fighter: Option<FighterId>,
    text: impl Into<String>,
) -> Result<()> {
    emit(
        tx,
        RagnarokEvent::Log {
            fighter,
            text: text.into(),
        },
    )
}

/// Resolves when the battle should stop: the abort flag flipped, or the abort
/// sender vanished (the UI is gone).
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

/// Resolves only when the user unleashes combat. A vanished sender pends
/// forever — the racing [`wait_abort`] handles a closed UI.
async fn wait_proceed(mut proceed: watch::Receiver<bool>) {
    loop {
        if *proceed.borrow() {
            return;
        }
        if proceed.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// How many champions actually take the field: Thor's routed count, bounded
/// to 2-10 and further capped by the user's `[ragnarok] max_competitors`.
pub fn field_size(routed: usize, cap: usize) -> usize {
    let cap = cap.clamp(MIN_FIGHTERS, MAX_FIGHTERS);
    routed.clamp(MIN_FIGHTERS, MAX_FIGHTERS).min(cap)
}

/// Same user cap as [`field_size`], plus a sanity ceiling from Thor's stated
/// complexity so easy tasks do not accidentally summon a costly army.
pub fn field_size_for_route(route: &RouteDecision, cap: usize) -> usize {
    field_size(route.competitors, cap).min(complexity_field_ceiling(&route.complexity))
}

fn complexity_field_ceiling(complexity: &str) -> usize {
    match complexity.trim().to_ascii_lowercase().as_str() {
        "trivial" | "simple" => 2,
        "moderate" => 3,
        "complex" => 5,
        "epic" => MAX_FIGHTERS,
        _ => MAX_FIGHTERS,
    }
}

async fn battle(
    cfg: &BattleConfig,
    tx: &mpsc::UnboundedSender<RagnarokEvent>,
    abort: watch::Receiver<bool>,
    proceed: watch::Receiver<bool>,
    camps: WorktreeRegistry,
) -> Result<()> {
    // ---- Phase: muster -----------------------------------------------------
    emit(tx, RagnarokEvent::Phase(Phase::Mustering))?;
    feed(tx, None, "⚡ The Gjallarhorn sounds. Ragnarok begins.")?;

    let clean_cwd = cfg.cwd.clone();
    tokio::task::spawn_blocking(move || worktree::ensure_clean_for_automation(&clean_cwd))
        .await
        .context("source tree cleanliness check failed")??;
    let pool = tokio::select! {
        pool = muster(cfg, tx) => pool?,
        _ = wait_abort(abort.clone()) => bail!("the battle was called off"),
    };
    if pool.len() < MIN_FIGHTERS {
        bail!(
            "only {} eligible champion(s) mustered — Ragnarok needs at least {MIN_FIGHTERS} \
             distinct ranked models on ready-to-use ACP adapters",
            pool.len()
        );
    }
    feed(
        tx,
        None,
        format!(
            "🛡 {} DeepSWE-ranked models stand in the candidate pool (across the ready adapters; \
             only the chosen few will fight).",
            pool.len()
        ),
    )?;

    // ---- Phase: Thor routes ------------------------------------------------
    emit(tx, RagnarokEvent::Phase(Phase::Routing))?;
    emit(tx, RagnarokEvent::ThorAction(ThorAction::Descending))?;
    let thor_host = cfg
        .thor_host
        .clone()
        .unwrap_or_else(|| ThorHost::from_candidate(&pool[0]));
    feed(
        tx,
        None,
        format!(
            "⚡ THOR descends, wearing the guise of {}.",
            thor_host.tag()
        ),
    )?;
    let thor_camp = {
        let cwd = cfg.cwd.clone();
        tokio::task::spawn_blocking(move || worktree::create_for_automation(&cwd, "ragnarok-thor"))
            .await
            .context("thor camp task failed")?
            .context("could not forge Thor's camp")?
    };
    register_camp(&camps, &thor_camp);
    let mut thor = Thor::summon(thor_host, thor_camp, abort.clone()).await?;
    let battle_result: Result<()> = async {
    emit(tx, RagnarokEvent::ThorAction(ThorAction::Deciding))?;
    let route = thor.route(&cfg.task, tx).await?;
    let cap = crate::config::Config::load(&crate::config::default_config_path())
        .map(|config| config.ragnarok.max_competitors)
        .unwrap_or_else(|_| crate::config::RagnarokConfig::default().max_competitors);
    let bounded_route = route.competitors.clamp(MIN_FIGHTERS, MAX_FIGHTERS);
    let complexity_cap = complexity_field_ceiling(&route.complexity);
    let want = field_size_for_route(&route, cap);
    feed(
        tx,
        None,
        format!(
            "⚡ THOR decrees: this task is {} — {} champions shall battle. ({})",
            route.complexity, route.competitors, route.rationale
        ),
    )?;
    if want < bounded_route && want == complexity_cap {
        feed(
            tx,
            None,
            format!(
                "🧭 The runes temper the field to {want} for a {} task.",
                route.complexity
            ),
        )?;
    }
    if want < bounded_route && want < complexity_cap {
        feed(
            tx,
            None,
            format!(
                "💰 The coffers cap the field at {want} (config `[ragnarok] max_competitors`)."
            ),
        )?;
    }

    let mut chosen = select_fighters(&pool, want);
    if chosen.len() < want {
        feed(
            tx,
            None,
            format!(
                "🌫 Thor decreed {want}, but only {} distinct champions exist. So be it.",
                chosen.len()
            ),
        )?;
    }
    if chosen.len() < MIN_FIGHTERS {
        bail!("fewer than {MIN_FIGHTERS} distinct champions available");
    }
    for (id, fighter) in chosen.iter_mut().enumerate() {
        fighter.card.id = id;
    }
    let mut cards: Vec<FighterCard> = chosen.iter().map(|c| c.card.clone()).collect();
    emit(tx, RagnarokEvent::Roster(cards.clone()))?;
    for card in &cards {
        feed(
            tx,
            Some(card.id),
            format!("⚔ {} enters the arena!", card.tag()),
        )?;
    }

    // ---- Phase: approval (the one chance to balk at the bill) ---------------
    emit(tx, RagnarokEvent::Phase(Phase::Approval))?;
    feed(
        tx,
        None,
        format!(
            "⚖ {} champions stand ready and nothing has been spent on combat yet. \
             Unleash Ragnarok? [Enter to begin · Esc Esc to flee]",
            cards.len()
        ),
    )?;
    tokio::select! {
        _ = wait_proceed(proceed.clone()) => {}
        _ = wait_abort(abort.clone()) => bail!("the battle was called off at the gates"),
    }
    feed(
        tx,
        None,
        "⚡ UNLEASHED! The horns of war thunder across the nine realms!",
    )?;

    // ---- Phase: combat (parallel implementations, watched by Thor) ----------
    emit(tx, RagnarokEvent::Phase(Phase::Combat))?;
    let forge_lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
    let (ping_tx, mut ping_rx) = mpsc::unbounded_channel::<WatchdogPing>();
    let mut kills: HashMap<FighterId, KillSwitch> = HashMap::new();
    let mut joinset = tokio::task::JoinSet::new();
    for fighter in chosen.clone() {
        let (kill_tx, kill_rx) = watch::channel(false);
        let kill_reason = std::sync::Arc::new(std::sync::Mutex::new(None));
        kills.insert(
            fighter.card.id,
            KillSwitch {
                trigger: kill_tx,
                reason: kill_reason.clone(),
            },
        );
        let orders = FightOrders {
            task: cfg.task.clone(),
            cwd: cfg.cwd.clone(),
            forge_lock: forge_lock.clone(),
            kill: kill_rx,
            kill_reason,
            ping: ping_tx.clone(),
            tx: tx.clone(),
            abort: abort.clone(),
            camps: camps.clone(),
        };
        joinset.spawn(async move { fight(fighter, orders).await });
    }

    // Supervise the melee: collect finishers, feed the watchdog with action
    // pings, and let Thor pass mid-battle judgment on stragglers so one
    // champion stuck in a loop cannot hold the whole tournament hostage.
    let spawned = chosen.len();
    let mut seen = 0usize;
    let mut reports: Vec<FighterReport> = Vec::new();
    let mut watchdog = StragglerWatch::new(spawned);
    let mut pending: HashSet<FighterId> = cards.iter().map(|c| c.id).collect();
    let combat_started = tokio::time::Instant::now();
    let mut ticker = tokio::time::interval(STRAGGLER_CHECK_EVERY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut abort_fanned = false;
    while seen < spawned {
        tokio::select! {
            joined = joinset.join_next() => {
                match joined {
                    Some(Ok(report)) => {
                        seen += 1;
                        pending.remove(&report.id);
                        if report.slain_reason.is_none() {
                            watchdog.note_finished(combat_started.elapsed());
                        }
                        reports.push(report);
                    }
                    Some(Err(e)) => {
                        seen += 1;
                        feed(tx, None, format!("☠ a champion's thread was severed: {e}"))?;
                    }
                    None => break,
                }
            }
            maybe_ping = ping_rx.recv() => {
                if let Some(ping) = maybe_ping {
                    watchdog.note_action(ping.id, ping.title);
                }
            }
            _ = ticker.tick() => {
                judge_stragglers(
                    &mut watchdog,
                    &mut thor,
                    &cards,
                    &pending,
                    &kills,
                    combat_started,
                    tx,
                )
                .await?;
            }
            _ = wait_abort(abort.clone()), if !abort_fanned => {
                abort_fanned = true;
                for switch in kills.values() {
                    switch.fire("the battle was called off");
                }
            }
        }
    }
    if *abort.borrow() {
        bail!("the battle was called off");
    }
    reports.sort_by_key(|r| r.id);
    let survivors: Vec<&FighterReport> = reports
        .iter()
        .filter(|r| r.slain_reason.is_none())
        .collect();
    if survivors.is_empty() {
        bail!("no champions survived combat — not enough for adversarial review");
    }
    feed(
        tx,
        None,
        format!("🏰 Combat ends. {} champions still stand.", survivors.len()),
    )?;

    // ---- Phase: Thor assigns adversarial reviews ----------------------------
    let survivor_ids: Vec<FighterId> = survivors.iter().map(|r| r.id).collect();
    let mut review_assignments = None;
    if survivor_ids.len() == 1 {
        let survivor = survivor_ids[0];
        let mut judge = select_judge_only_reviewer(&pool, &chosen, survivor).ok_or_else(|| {
            anyhow!(
                "only one champion survived combat and no distinct judge-only competitor was available"
            )
        })?;
        let judge_id = cards.len();
        judge.card.id = judge_id;
        feed(
            tx,
            Some(survivor),
            format!(
                "⚖ Only {} still stands; THOR summons {} only to judge the surviving work.",
                cards[survivor].model_name,
                judge.card.tag()
            ),
        )?;
        emit(tx, RagnarokEvent::FighterJoined(judge.card.clone()))?;
        cards.push(judge.card.clone());
        chosen.push(judge);
        review_assignments = Some(vec![Assignment {
            reviewer: judge_id,
            defender: survivor,
        }]);
    }
    emit(tx, RagnarokEvent::ThorAction(ThorAction::Assigning))?;
    let assignments = if let Some(assignments) = review_assignments {
        assignments
    } else {
        thor.assign(&survivor_ids, &cards, tx)
            .await
            .unwrap_or_else(|_| assignments_rotation(&survivor_ids))
    };
    let review_judges: Vec<FighterId> = assignments.iter().map(|a| a.reviewer).collect();
    emit(tx, RagnarokEvent::Phase(Phase::Review))?;
    emit(tx, RagnarokEvent::Assignments(assignments.clone()))?;
    for a in &assignments {
        feed(
            tx,
            Some(a.reviewer),
            format!(
                "🗡 THOR commands: {} shall tear apart the work of {}!",
                cards[a.reviewer].model_name, cards[a.defender].model_name
            ),
        )?;
    }

    // ---- Phase: adversarial reviews (parallel) -------------------------------
    let by_id: HashMap<FighterId, &FighterReport> = reports.iter().map(|r| (r.id, r)).collect();
    let mut review_set = tokio::task::JoinSet::new();
    for a in assignments.clone() {
        let reviewer = chosen.iter().find(|c| c.card.id == a.reviewer).cloned();
        let Some(reviewer) = reviewer else { continue };
        let Some(defender) = by_id.get(&a.defender) else {
            continue;
        };
        let prompt = review_prompt(
            &cfg.task,
            &cards[a.reviewer],
            &cards[a.defender],
            defender.artifact.as_ref(),
            &defender.final_text,
        );
        let defender_cwd = defender
            .worktree
            .as_ref()
            .map(|w| w.session_cwd.clone())
            .unwrap_or_else(|| cfg.cwd.clone());
        let tx = tx.clone();
        let abort = abort.clone();
        review_set.spawn(async move { review(reviewer, a, prompt, defender_cwd, tx, abort).await });
    }
    let mut reviews: Vec<ReviewReport> = Vec::new();
    while let Some(joined) = review_set.join_next().await {
        match joined {
            Ok(report) => reviews.push(report),
            Err(e) => feed(tx, None, format!("☠ a reviewer's thread was severed: {e}"))?,
        }
    }
    if *abort.borrow() {
        bail!("the battle was called off");
    }
    reviews.sort_by_key(|r| r.assignment.defender);
    feed(tx, None, "📜 All reviews are carved in stone.")?;

    // ---- Phase: judgment -----------------------------------------------------
    emit(tx, RagnarokEvent::Phase(Phase::Judgment))?;
    emit(tx, RagnarokEvent::ThorAction(ThorAction::Judging))?;
    let dossier = judgment_dossier(&cfg.task, &cards, &survivor_ids, &by_id, &reviews);
    let verdict = thor
        .judge(&dossier, &survivor_ids, &review_judges, tx)
        .await;
    let verdict = match verdict {
        Ok(v) => v,
        Err(e) => {
            feed(
                tx,
                None,
                format!("🌩 Thor's judgment was garbled ({e:#}); the runes fall back to Pass@1 order."),
            )?;
            strength_fallback_verdict(&survivor_ids, &cards)
        }
    };

    emit(tx, RagnarokEvent::Phase(Phase::Verdict))?;
    match (&verdict.clear_winner, &verdict.finalists) {
        (Some(id), _) => feed(
            tx,
            Some(*id),
            format!(
                "👑 THOR crowns {} the victor of Ragnarok!",
                cards[*id].tag()
            ),
        )?,
        (None, Some((a, b))) => feed(
            tx,
            None,
            format!(
                "⚖ No clear victor. THOR presents two finalists: {} and {}. The choice is yours.",
                cards[*a].model_name, cards[*b].model_name
            ),
        )?,
        (None, None) => {}
    }
    emit(tx, RagnarokEvent::Verdict(Box::new(verdict)))?;
    Ok(())
    }
    .await;
    thor.dismiss().await;
    battle_result
}

/// Deterministic fallback when Thor cannot deliver a parseable judgment:
/// no honest quality signal exists, so present the two strongest champions
/// (by Pass@1) as finalists and let the user decide.
fn strength_fallback_verdict(survivors: &[FighterId], cards: &[FighterCard]) -> Verdict {
    let mut ranking: Vec<FighterId> = survivors.to_vec();
    ranking.sort_by_key(|id| std::cmp::Reverse(cards[*id].pass_at_1_bps));
    let clear_winner = (ranking.len() == 1).then(|| ranking[0]);
    let finalists = (ranking.len() >= 2).then(|| (ranking[0], ranking[1]));
    Verdict {
        clear_winner,
        finalists,
        ranking,
        review_verdicts: Vec::new(),
        reasoning: "Thor's judgment could not be parsed; finalists are presented in Pass@1 order. \
                    Read the adversarial reviews in the transcripts and choose."
            .to_string(),
        thor_fallback: true,
    }
}

// ---------------------------------------------------------------------------
// Muster: which (agent, model) pairs may fight?
// ---------------------------------------------------------------------------

/// A battle-ready adapter/model pair with a DeepSWE rating.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub card: FighterCard,
    pub launch: Launch,
    pub match_key: String,
    pub vendor: Option<String>,
}

/// The active session's agent/model used for Thor's router/judge role.
#[derive(Debug, Clone)]
pub struct ThorHost {
    pub agent_source_id: String,
    pub launch: Launch,
    pub model_value: Option<String>,
    pub model_name: Option<String>,
}

impl ThorHost {
    fn from_candidate(candidate: &Candidate) -> Self {
        Self {
            agent_source_id: candidate.card.agent_source_id.clone(),
            launch: candidate.launch.clone(),
            model_value: Some(candidate.card.model_value.clone()),
            model_name: Some(candidate.card.model_name.clone()),
        }
    }

    fn tag(&self) -> String {
        match self.model_name.as_deref() {
            Some(model_name) if !model_name.trim().is_empty() => {
                format!("{model_name} [{}]", self.agent_source_id)
            }
            _ => format!("{} [current model]", self.agent_source_id),
        }
    }
}

/// Clonable launch command (mirror of `picker::LaunchCommand`).
#[derive(Debug, Clone)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// Convert the shared launchable DeepSWE catalog into battle candidates.
pub(crate) async fn muster(
    cfg: &BattleConfig,
    tx: &mpsc::UnboundedSender<RagnarokEvent>,
) -> Result<Vec<Candidate>> {
    let mut pool = cfg
        .available_models
        .iter()
        .filter(|role| role.ranked)
        .map(|role| Candidate {
            card: FighterCard {
                id: 0,
                agent_source_id: role.launch.source_id.clone(),
                model_value: role.model_value.clone(),
                model_name: role.model.model.clone(),
                pass_at_1_bps: (role.model.pass_at_1 * 10_000.0).round() as u32,
                mean_cost_usd: role.model.mean_cost_usd,
            },
            launch: Launch {
                program: role.launch.command.clone(),
                args: role.launch.args.clone(),
                env: role.launch.env.clone(),
            },
            vendor: Some(council_provider(&role.model.model)),
            match_key: role.model.model.clone(),
        })
        .collect::<Vec<_>>();

    pool.sort_by(|a, b| {
        b.card
            .pass_at_1_bps
            .cmp(&a.card.pass_at_1_bps)
            .then_with(|| a.card.mean_cost_usd.total_cmp(&b.card.mean_cost_usd))
            .then_with(|| a.card.model_name.cmp(&b.card.model_name))
    });
    feed(
        tx,
        None,
        format!("🏹 {} ranked models answer the call.", pool.len()),
    )?;
    Ok(pool)
}

fn council_provider(model: &str) -> String {
    let provider = crate::deepswe::model_provider(model);
    if provider.is_empty() {
        model
            .split_once('-')
            .map_or(model, |(head, _)| head)
            .to_string()
    } else {
        provider.to_string()
    }
}

fn adjusted_selection_score(candidate: &Candidate, used_vendors: &HashSet<String>) -> i32 {
    adjusted_selection_score_with_bonuses(candidate, used_vendors, NEW_VENDOR_SELECTION_BONUS)
}

fn adjusted_selection_score_with_bonuses(
    candidate: &Candidate,
    used_vendors: &HashSet<String>,
    vendor_bonus: i32,
) -> i32 {
    let mut score = candidate.card.pass_at_1_bps as i32;
    let diversity_applies = !used_vendors.is_empty();
    if diversity_applies
        && candidate
            .vendor
            .as_deref()
            .is_some_and(|vendor| !used_vendors.contains(vendor))
    {
        score += vendor_bonus;
    }
    score
}

fn select_judge_only_reviewer_with_picker<F>(
    pool: &[Candidate],
    current_roster: &[Candidate],
    survivor: FighterId,
    mut pick_index: F,
) -> Option<Candidate>
where
    F: FnMut(usize) -> usize,
{
    let survivor = current_roster.iter().find(|c| c.card.id == survivor)?;
    let roster_keys: HashSet<&str> = current_roster
        .iter()
        .map(|c| c.match_key.as_str())
        .collect();
    let mut used_vendors = HashSet::new();
    if let Some(vendor) = &survivor.vendor {
        used_vendors.insert(vendor.clone());
    }

    let mut ranked: Vec<(usize, i32)> = pool
        .iter()
        .enumerate()
        .filter(|(_, candidate)| !roster_keys.contains(candidate.match_key.as_str()))
        .map(|(idx, candidate)| {
            (
                idx,
                adjusted_selection_score_with_bonuses(
                    candidate,
                    &used_vendors,
                    JUDGE_ONLY_VENDOR_SELECTION_BONUS,
                ),
            )
        })
        .collect();
    if ranked.is_empty() {
        return None;
    }
    ranked.sort_by(|(a_idx, a_score), (b_idx, b_score)| {
        let a = &pool[*a_idx];
        let b = &pool[*b_idx];
        b_score
            .cmp(a_score)
            .then_with(|| b.card.pass_at_1_bps.cmp(&a.card.pass_at_1_bps))
            .then_with(|| a.card.model_name.cmp(&b.card.model_name))
            .then_with(|| a.card.agent_source_id.cmp(&b.card.agent_source_id))
    });
    let base_window = ranked.len().min(SELECTION_RANDOM_TOP_N.max(1));
    let cutoff = ranked[base_window - 1].1;
    let top_len = ranked
        .iter()
        .take_while(|(_, score)| *score >= cutoff)
        .count();
    let choice = pick_index(top_len).min(top_len.saturating_sub(1));
    Some(pool[ranked[choice].0].clone())
}

pub(crate) fn select_judge_only_reviewer(
    pool: &[Candidate],
    current_roster: &[Candidate],
    survivor: FighterId,
) -> Option<Candidate> {
    select_judge_only_reviewer_with_picker(pool, current_roster, survivor, selection_random_index)
}

fn selection_random_index(upper: usize) -> usize {
    if upper <= 1 {
        return 0;
    }
    let upper = upper as u64;
    let zone = u64::MAX - (u64::MAX % upper);
    for _ in 0..8 {
        let mut bytes = [0u8; 8];
        if getrandom::fill(&mut bytes).is_err() {
            break;
        }
        let value = u64::from_ne_bytes(bytes);
        if value < zone {
            return (value % upper) as usize;
        }
    }
    selection_fallback_index(upper as usize)
}

fn selection_fallback_index(upper: usize) -> usize {
    if upper <= 1 {
        return 0;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mixed = (nanos as usize)
        .wrapping_add((nanos >> 64) as usize)
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add(std::process::id() as usize);
    mixed % upper
}

fn select_fighters_with_picker<F>(
    pool: &[Candidate],
    want: usize,
    mut pick_index: F,
) -> Vec<Candidate>
where
    F: FnMut(usize) -> usize,
{
    let mut picked: Vec<Candidate> = Vec::new();
    let mut used_keys: HashSet<String> = HashSet::new();
    let mut used_vendors: HashSet<String> = HashSet::new();

    while picked.len() < want {
        let mut ranked: Vec<(usize, i32)> = pool
            .iter()
            .enumerate()
            .filter(|(_, candidate)| !used_keys.contains(&candidate.match_key))
            .map(|(idx, candidate)| (idx, adjusted_selection_score(candidate, &used_vendors)))
            .collect();
        if ranked.is_empty() {
            break;
        }
        ranked.sort_by(|(a_idx, a_score), (b_idx, b_score)| {
            let a = &pool[*a_idx];
            let b = &pool[*b_idx];
            b_score
                .cmp(a_score)
                .then_with(|| b.card.pass_at_1_bps.cmp(&a.card.pass_at_1_bps))
                .then_with(|| a.card.model_name.cmp(&b.card.model_name))
                .then_with(|| a.card.agent_source_id.cmp(&b.card.agent_source_id))
        });
        let base_window = ranked.len().min(SELECTION_RANDOM_TOP_N.max(1));
        let cutoff = ranked[base_window - 1].1;
        let top_len = ranked
            .iter()
            .take_while(|(_, score)| *score >= cutoff)
            .count();
        let choice = pick_index(top_len).min(top_len.saturating_sub(1));
        let selected = pool[ranked[choice].0].clone();
        used_keys.insert(selected.match_key.clone());
        if let Some(vendor) = &selected.vendor {
            used_vendors.insert(vendor.clone());
        }
        picked.push(selected);
    }

    picked
}

/// Pick `want` champions from the pool. Models must be genuinely distinct
/// (dedup by leaderboard match key). Selection is greedy on adjusted Pass@1:
/// New providers receive the configured diversity bonus, then the final choice
/// is randomized within the top ranked window. Adapter identity is irrelevant.
pub fn select_fighters(pool: &[Candidate], want: usize) -> Vec<Candidate> {
    select_fighters_with_picker(pool, want, selection_random_index)
}

// ---------------------------------------------------------------------------
// One ACP connection driven programmatically
// ---------------------------------------------------------------------------

// Every battle connection — Thor, champions, reviewers — runs inside its own
// disposable git worktree, so permissions are auto-granted rather than
// prompted: there is no user at the modal, and answering a permission with
// `Cancelled` makes agents cancel the whole turn (Thor learned this the hard
// way). When an agent offers no allow option, the first reject option is
// chosen — an explicit denial keeps the turn alive where a cancel kills it.

/// What a turn streamed, reduced to what the arena cares about.
pub(crate) enum TurnEvent {
    Message(String),
    Thought(String),
    Tool {
        title: String,
        kind: Option<ToolKind>,
        status: Option<ToolCallStatus>,
        started: bool,
    },
    Permission {
        prompt: Box<crate::event::PermissionPrompt>,
        access_mode: acp::RuntimeAccessMode,
    },
    Note(String),
}

#[derive(Debug)]
pub(crate) struct TurnOutcome {
    pub text: String,
    pub stop: StopReason,
}

/// A live agent subprocess + session, driven over the same channel pair the
/// TUI uses.
pub(crate) struct AgentHandle {
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    events: mpsc::UnboundedReceiver<UiEvent>,
    runtime: tokio::task::JoinHandle<Result<()>>,
    config_options: Vec<SessionConfigOption>,
    config_targets: Vec<SessionConfigTarget>,
    abort: watch::Receiver<bool>,
    access_mode: acp::RuntimeAccessMode,
}

impl AgentHandle {
    pub(crate) async fn connect(
        launch: &Launch,
        cwd: &Path,
        additional_directories: &[PathBuf],
        abort: watch::Receiver<bool>,
        access_mode: acp::RuntimeAccessMode,
    ) -> Result<Self> {
        Self::connect_with_saved_session_config(
            launch,
            cwd,
            additional_directories,
            abort,
            access_mode,
            HashMap::new(),
        )
        .await
    }

    pub(crate) async fn connect_with_saved_session_config(
        launch: &Launch,
        cwd: &Path,
        additional_directories: &[PathBuf],
        abort: watch::Receiver<bool>,
        access_mode: acp::RuntimeAccessMode,
        saved_session_config: HashMap<String, String>,
    ) -> Result<Self> {
        Self::connect_with_role_config(
            launch,
            cwd,
            additional_directories,
            abort,
            access_mode,
            saved_session_config,
            None,
        )
        .await
    }

    pub(crate) async fn connect_with_role_config(
        launch: &Launch,
        cwd: &Path,
        additional_directories: &[PathBuf],
        abort: watch::Receiver<bool>,
        access_mode: acp::RuntimeAccessMode,
        saved_session_config: HashMap<String, String>,
        role_config: Option<acp::RuntimeRoleConfig>,
    ) -> Result<Self> {
        Self::connect_with_role_config_and_mcp(
            launch,
            cwd,
            additional_directories,
            abort,
            access_mode,
            saved_session_config,
            role_config,
            Vec::new(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn connect_with_role_config_and_mcp(
        launch: &Launch,
        cwd: &Path,
        additional_directories: &[PathBuf],
        abort: watch::Receiver<bool>,
        access_mode: acp::RuntimeAccessMode,
        saved_session_config: HashMap<String, String>,
        role_config: Option<acp::RuntimeRoleConfig>,
        mcp_servers: Vec<agent_client_protocol::schema::v1::McpServer>,
    ) -> Result<Self> {
        let (event_tx, events) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let runtime_cfg = acp::AcpRuntimeConfig {
            command: launch.program.clone(),
            args: launch.args.clone(),
            cwd: cwd.to_path_buf(),
            additional_directories: additional_directories.to_vec(),
            mcp_servers,
            resume_session: None,
            env: launch.env.clone(),
            agent_stderr: None,
            fs_max_text_bytes: acp::DEFAULT_FS_TEXT_BYTES,
            access_mode,
            agent_source_id: None,
            config_path: None,
            saved_session_config,
            role_config,
            code_agent: None,
        };
        let runtime = tokio::spawn(acp::run(runtime_cfg, event_tx, cmd_rx));
        let mut handle = Self {
            cmd_tx,
            events,
            runtime,
            config_options: Vec::new(),
            config_targets: Vec::new(),
            abort,
            access_mode,
        };
        handle.wait_session_started().await?;
        Ok(handle)
    }

    async fn wait_session_started(&mut self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
        loop {
            let ev = tokio::select! {
                ev = self.events.recv() => ev,
                _ = wait_abort(self.abort.clone()) => bail!("battle aborted"),
                _ = tokio::time::sleep_until(deadline) => bail!("timed out waiting for session"),
            };
            let Some(ev) = ev else {
                bail!("agent runtime closed before a session started");
            };
            match ev {
                UiEvent::SessionStarted { .. } => return Ok(()),
                UiEvent::SessionConfigOptions { options, targets } => {
                    self.store_config(options, targets);
                }
                UiEvent::PermissionRequest(p) => self.answer_permission(p),
                UiEvent::ElicitationRequest(e) => {
                    let _ = e.responder.send(ElicitationOutcome::Decline);
                }
                UiEvent::Fatal(m) => bail!("agent failed: {m}"),
                UiEvent::PromptFailed { message } => bail!("agent failed: {message}"),
                _ => {}
            }
        }
    }

    fn store_config(
        &mut self,
        options: Vec<SessionConfigOption>,
        targets: Vec<SessionConfigTarget>,
    ) {
        if options.len() == targets.len() {
            self.config_options = options;
            self.config_targets = targets;
        }
    }

    fn answer_permission(&self, prompt: crate::event::PermissionPrompt) {
        let decision = permission_decision_for_access(self.access_mode, &prompt);
        let _ = prompt.responder.send(decision);
    }

    /// Select `model_value` through the session's Model config option and
    /// wait for the runtime to confirm the update. The runtime rejects
    /// prompts while a config update is in flight ("config update already in
    /// flight"), so returning before the confirmation would poison the very
    /// next turn. Confirmation is a refreshed `SessionConfigOptions` whose
    /// model option carries the requested value; failure surfaces as a
    /// "session config update failed" warning. Already-current models skip
    /// the round trip entirely.
    pub(crate) async fn arm_model(&mut self, model_value: &str) -> Result<()> {
        if self.model_is_current(model_value) {
            return Ok(());
        }

        // Wait for the option table if it hasn't arrived yet, then send.
        let deadline = tokio::time::Instant::now() + CONFIG_OPTIONS_TIMEOUT;
        let (target, value) = loop {
            if let Some(found) = self.find_model_choice(model_value) {
                break found;
            }
            let ev = tokio::select! {
                ev = self.events.recv() => ev,
                _ = wait_abort(self.abort.clone()) => bail!("battle aborted"),
                _ = tokio::time::sleep_until(deadline) => {
                    bail!("agent never offered model '{model_value}' to select")
                }
            };
            match ev {
                Some(UiEvent::SessionConfigOptions { options, targets }) => {
                    self.store_config(options, targets)
                }
                Some(UiEvent::PermissionRequest(p)) => self.answer_permission(p),
                Some(UiEvent::ElicitationRequest(e)) => {
                    let _ = e.responder.send(ElicitationOutcome::Decline);
                }
                Some(UiEvent::Fatal(m)) => bail!("agent failed: {m}"),
                Some(_) => {}
                None => bail!("agent runtime closed"),
            }
        };
        let _ = self
            .cmd_tx
            .send(UiCommand::SetSessionConfigOption { target, value });

        // Await the confirmation before releasing the caller to prompt.
        let deadline = tokio::time::Instant::now() + CONFIG_UPDATE_TIMEOUT;
        loop {
            let ev = tokio::select! {
                ev = self.events.recv() => ev,
                _ = wait_abort(self.abort.clone()) => bail!("battle aborted"),
                _ = tokio::time::sleep_until(deadline) => {
                    bail!("model select for '{model_value}' was not confirmed in time")
                }
            };
            match ev {
                Some(UiEvent::SessionConfigOptions { options, targets }) => {
                    self.store_config(options, targets);
                    if self.model_is_current(model_value) {
                        return Ok(());
                    }
                }
                Some(UiEvent::Warning(w)) if w.contains("session config update failed") => {
                    bail!("agent refused model '{model_value}': {w}")
                }
                Some(UiEvent::PermissionRequest(p)) => self.answer_permission(p),
                Some(UiEvent::ElicitationRequest(e)) => {
                    let _ = e.responder.send(ElicitationOutcome::Decline);
                }
                Some(UiEvent::Fatal(m)) => bail!("agent failed: {m}"),
                Some(_) => {}
                None => bail!("agent runtime closed"),
            }
        }
    }

    /// True when a Model-category option's current value already equals
    /// `model_value`.
    fn model_is_current(&self, model_value: &str) -> bool {
        self.config_options.iter().any(|option| {
            crate::app::is_model_config_option(option)
                && crate::app::config_option_current_value_id(option)
                    .is_some_and(|current| current.to_string() == model_value)
        })
    }

    fn find_model_choice(
        &self,
        model_value: &str,
    ) -> Option<(
        SessionConfigTarget,
        agent_client_protocol::schema::v1::SessionConfigValueId,
    )> {
        for (option, target) in self.config_options.iter().zip(&self.config_targets) {
            if !crate::app::is_model_config_option(option) {
                continue;
            }
            let Some(choices) = crate::app::config_option_choices(option) else {
                continue;
            };
            // Setting the current value again is harmless, so no special case
            // for an already-armed model.
            for choice in choices {
                if choice.value.to_string() == model_value {
                    return Some((target.clone(), choice.value));
                }
            }
        }
        None
    }

    /// Send one prompt and drive it to completion, streaming digested events
    /// through `on_event`. A rejection caused by a still-in-flight config
    /// update ([`Self::arm_model`] should prevent it, but belt and braces)
    /// re-sends the prompt a bounded number of times instead of failing.
    pub(crate) async fn prompt(
        &mut self,
        text: String,
        budget: Duration,
        on_event: impl FnMut(TurnEvent),
    ) -> Result<TurnOutcome> {
        self.prompt_with_images(text, Vec::new(), budget, on_event)
            .await
    }

    /// Send one prompt with optional image blocks and drive it to completion.
    /// Text-only callers should use [`Self::prompt`].
    pub(crate) async fn prompt_with_images(
        &mut self,
        text: String,
        images: Vec<crate::event::PromptImage>,
        budget: Duration,
        mut on_event: impl FnMut(TurnEvent),
    ) -> Result<TurnOutcome> {
        let resend_text = text.clone();
        let resend_images = images.clone();
        let mut resends = 0usize;
        let _ = self.cmd_tx.send(UiCommand::SendPrompt { text, images });
        let deadline = tokio::time::Instant::now() + budget;
        let mut acc = String::new();
        let mut truncated = false;
        let mut known_tools: HashMap<String, (String, Option<ToolKind>)> = HashMap::new();
        loop {
            let ev = tokio::select! {
                ev = self.events.recv() => ev,
                _ = wait_abort(self.abort.clone()) => {
                    let _ = self.cmd_tx.send(UiCommand::CancelPrompt);
                    bail!("battle aborted");
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let _ = self.cmd_tx.send(UiCommand::CancelPrompt);
                    bail!("ran out of time");
                }
            };
            let Some(ev) = ev else {
                bail!("agent runtime closed mid-turn");
            };
            match ev {
                UiEvent::SessionUpdate(update) => match update {
                    SessionUpdate::AgentMessageChunk(chunk) => {
                        let piece = content_block_text(&chunk.content);
                        if acc.len() + piece.len() <= FINAL_TEXT_LIMIT {
                            acc.push_str(&piece);
                        } else {
                            truncated = true;
                        }
                        on_event(TurnEvent::Message(piece));
                    }
                    SessionUpdate::AgentThoughtChunk(chunk) => {
                        on_event(TurnEvent::Thought(content_block_text(&chunk.content)));
                    }
                    SessionUpdate::ToolCall(call) => {
                        let id = call.tool_call_id.to_string();
                        known_tools.insert(id, (call.title.clone(), Some(call.kind)));
                        on_event(TurnEvent::Tool {
                            title: call.title,
                            kind: Some(call.kind),
                            status: Some(call.status),
                            started: true,
                        });
                    }
                    SessionUpdate::ToolCallUpdate(update) => {
                        let id = update.tool_call_id.to_string();
                        let entry = known_tools.entry(id).or_default();
                        if let Some(title) = &update.fields.title {
                            entry.0 = title.clone();
                        }
                        if let Some(kind) = update.fields.kind {
                            entry.1 = Some(kind);
                        }
                        on_event(TurnEvent::Tool {
                            title: entry.0.clone(),
                            kind: entry.1,
                            status: update.fields.status,
                            started: false,
                        });
                    }
                    _ => {}
                },
                UiEvent::SessionConfigOptions { options, targets } => {
                    self.store_config(options, targets)
                }
                UiEvent::PermissionRequest(p) => {
                    on_event(TurnEvent::Permission {
                        prompt: Box::new(p),
                        access_mode: self.access_mode,
                    });
                }
                UiEvent::ElicitationRequest(e) => {
                    let _ = e.responder.send(ElicitationOutcome::Decline);
                }
                UiEvent::PromptDone { stop_reason, .. } => {
                    if truncated {
                        acc.push_str("\n…[output truncated]");
                    }
                    return Ok(TurnOutcome {
                        text: acc,
                        stop: stop_reason,
                    });
                }
                UiEvent::PromptFailed { message } => {
                    if prompt_rejected_transiently(&message) && resends < PROMPT_RESEND_LIMIT {
                        resends += 1;
                        on_event(TurnEvent::Note(format!(
                            "runtime rejected prompt ({message}); retrying {resends}/{PROMPT_RESEND_LIMIT}"
                        )));
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        let _ = self.cmd_tx.send(UiCommand::SendPrompt {
                            text: resend_text.clone(),
                            images: resend_images.clone(),
                        });
                        continue;
                    }
                    bail!("prompt failed: {message}")
                }
                UiEvent::Fatal(m) => bail!("agent failed: {m}"),
                UiEvent::Warning(w) => {
                    if prompt_rejected_transiently(&w) {
                        if resends < PROMPT_RESEND_LIMIT {
                            resends += 1;
                            on_event(TurnEvent::Note(format!(
                                "runtime warning ({w}); retrying prompt {resends}/{PROMPT_RESEND_LIMIT}"
                            )));
                            tokio::time::sleep(Duration::from_millis(250)).await;
                            let _ = self.cmd_tx.send(UiCommand::SendPrompt {
                                text: resend_text.clone(),
                                images: resend_images.clone(),
                            });
                            continue;
                        }
                        bail!("prompt failed: {w}");
                    }
                    on_event(TurnEvent::Note(w));
                }
                _ => {}
            }
        }
    }

    /// Graceful teardown: ask the runtime to shut down and give it a moment;
    /// dropping the handle afterwards closes the command channel, which ends
    /// the runtime loop and kills the agent process tree in any case.
    pub(crate) async fn dismiss(self) {
        let _ = self.cmd_tx.send(UiCommand::Shutdown);
        let _ = tokio::time::timeout(Duration::from_secs(3), self.runtime).await;
    }
}

fn turn_succeeded(stop: StopReason) -> bool {
    matches!(
        stop,
        StopReason::EndTurn | StopReason::MaxTokens | StopReason::MaxTurnRequests
    )
}

fn prompt_rejected_transiently(message: &str) -> bool {
    message.contains("config update already in flight")
        || message.contains("prompt already in flight")
}

pub(crate) fn permission_decision_for_access(
    access_mode: acp::RuntimeAccessMode,
    prompt: &crate::event::PermissionPrompt,
) -> PermissionDecision {
    let allow = access_mode == acp::RuntimeAccessMode::Full
        || matches!(
            prompt.tool_call.fields.kind,
            Some(ToolKind::Read | ToolKind::Search | ToolKind::Think)
        );
    let selected = if allow {
        choose_allow_option(&prompt.options).or_else(|| choose_reject_option(&prompt.options))
    } else {
        choose_reject_option(&prompt.options)
    };
    selected
        .map(PermissionDecision::Selected)
        .unwrap_or(PermissionDecision::Cancelled)
}

/// First `RejectOnce` option, else first `RejectAlways`. Used only when an
/// agent offers no allow option at all: an explicit rejection lets the turn
/// continue, whereas cancelling the request cancels the whole turn.
fn choose_reject_option(
    options: &[agent_client_protocol::schema::v1::PermissionOption],
) -> Option<String> {
    use agent_client_protocol::schema::v1::PermissionOptionKind;
    options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::RejectOnce)
        .or_else(|| {
            options
                .iter()
                .find(|option| option.kind == PermissionOptionKind::RejectAlways)
        })
        .map(|option| option.option_id.to_string())
}

// ---------------------------------------------------------------------------
// The straggler watchdog: Thor's mid-battle mercy
// ---------------------------------------------------------------------------

/// A lightweight action report from a fighter to the combat watchdog.
struct WatchdogPing {
    id: FighterId,
    title: String,
}

/// One fighter's mid-battle kill switch: the reason is set before the
/// trigger fires so the fighter can die with an honest epitaph.
struct KillSwitch {
    trigger: watch::Sender<bool>,
    reason: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl KillSwitch {
    fn fire(&self, reason: &str) {
        if let Ok(mut slot) = self.reason.lock() {
            slot.get_or_insert_with(|| reason.to_string());
        }
        let _ = self.trigger.send(true);
    }
}

/// Combat bookkeeping for straggler judgment.
struct StragglerWatch {
    total: usize,
    finished: Vec<Duration>,
    recent: HashMap<FighterId, std::collections::VecDeque<String>>,
    next_review: HashMap<FighterId, tokio::time::Instant>,
    condemned: HashSet<FighterId>,
}

impl StragglerWatch {
    fn new(total: usize) -> Self {
        Self {
            total,
            finished: Vec::new(),
            recent: HashMap::new(),
            next_review: HashMap::new(),
            condemned: HashSet::new(),
        }
    }

    fn note_action(&mut self, id: FighterId, title: String) {
        let recent = self.recent.entry(id).or_default();
        recent.push_back(title);
        while recent.len() > RECENT_ACTIONS_CAP {
            recent.pop_front();
        }
    }

    fn note_finished(&mut self, took: Duration) {
        self.finished.push(took);
    }

    fn quorum_reached(&self) -> bool {
        self.finished.len() * 2 >= self.total
    }

    fn recent_actions(&self, id: FighterId) -> Vec<String> {
        self.recent
            .get(&id)
            .map(|d| d.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// Once a quorum has finished, examine every fighter that is dramatically
/// over the median finishing time; Thor rules mercy or death for each.
async fn judge_stragglers(
    watchdog: &mut StragglerWatch,
    thor: &mut Thor,
    cards: &[FighterCard],
    pending: &HashSet<FighterId>,
    kills: &HashMap<FighterId, KillSwitch>,
    combat_started: tokio::time::Instant,
    tx: &mpsc::UnboundedSender<RagnarokEvent>,
) -> Result<()> {
    if !watchdog.quorum_reached() {
        return Ok(());
    }
    let Some(median) = median_duration(&watchdog.finished) else {
        return Ok(());
    };
    let elapsed = combat_started.elapsed();
    if !should_judge_straggler(elapsed, median) {
        return Ok(());
    }
    let now = tokio::time::Instant::now();
    let mut ids: Vec<FighterId> = pending.iter().copied().collect();
    ids.sort_unstable();
    for id in ids {
        if watchdog.condemned.contains(&id) {
            continue;
        }
        if watchdog
            .next_review
            .get(&id)
            .is_some_and(|&review_at| now < review_at)
        {
            continue;
        }
        let Some(card) = cards.get(id) else { continue };
        let ratio = elapsed.as_secs_f64() / median.as_secs_f64().max(1.0);
        let recent = watchdog.recent_actions(id);
        let tally = action_tally(&recent);
        feed(
            tx,
            Some(id),
            format!(
                "👁 THOR turns his gaze upon {} ({ratio:.1}x the median finisher)…",
                card.model_name
            ),
        )?;
        emit(tx, RagnarokEvent::ThorAction(ThorAction::Mercy))?;
        let (cut, reason) = thor
            .mercy(&card.tag(), elapsed, median, &tally, &recent, tx)
            .await;
        if cut {
            watchdog.condemned.insert(id);
            if let Some(switch) = kills.get(&id) {
                switch.fire(&format!("cut down by Thor — {reason}"));
            }
            feed(
                tx,
                Some(id),
                format!(
                    "⚡ THOR'S PATIENCE ENDS: {} is cut down! ({reason})",
                    card.model_name
                ),
            )?;
        } else {
            let reprieve = median.max(Duration::from_secs(180));
            watchdog.next_review.insert(id, now + reprieve);
            feed(
                tx,
                Some(id),
                format!("⚖ THOR grants {} a reprieve — {reason}", card.model_name),
            )?;
        }
    }
    Ok(())
}

/// True when a still-running fighter has earned Thor's scrutiny.
pub fn should_judge_straggler(elapsed: Duration, median: Duration) -> bool {
    elapsed >= STRAGGLER_MIN_ELAPSED && elapsed >= median.saturating_mul(STRAGGLER_MULT)
}

/// Median of a set of durations. `None` when empty.
pub fn median_duration(samples: &[Duration]) -> Option<Duration> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    Some(if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2
    })
}

/// The deterministic loop detector: a fighter is stuck when one action
/// dominates ≥60% of their recent actions (with enough actions to matter).
pub fn looks_stuck(recent: &[String]) -> bool {
    if recent.len() < 10 {
        return false;
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for action in recent {
        *counts.entry(action.as_str()).or_default() += 1;
    }
    let top = counts.values().copied().max().unwrap_or(0);
    top * 10 >= recent.len() * 6
}

/// Summarize recent actions for Thor: `edit src/foo.rs ×17 · cargo test ×2`.
pub fn action_tally(recent: &[String]) -> String {
    if recent.is_empty() {
        return "(no recorded actions)".to_string();
    }
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for action in recent {
        match counts.iter_mut().find(|(title, _)| *title == action) {
            Some((_, n)) => *n += 1,
            None => counts.push((action.as_str(), 1)),
        }
    }
    counts.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
    let shown: Vec<String> = counts
        .iter()
        .take(4)
        .map(|(title, n)| format!("{title} ×{n}"))
        .collect();
    format!("{} (of the last {})", shown.join(" · "), recent.len())
}

#[derive(Debug, Deserialize)]
struct RawMercy {
    verdict: Option<String>,
    reason: Option<String>,
}

/// Parse Thor's mercy ruling: `(cut_down, reason)`.
pub fn parse_mercy(text: &str) -> Option<(bool, String)> {
    let value = extract_json_object(text)?;
    let raw: RawMercy = serde_json::from_value(value).ok()?;
    let cut = match raw.verdict?.as_str() {
        "cut_down" => true,
        "mercy" => false,
        _ => return None,
    };
    Some((
        cut,
        raw.reason
            .unwrap_or_else(|| "Thor keeps his counsel.".to_string()),
    ))
}

fn mercy_prompt(tag: &str, elapsed: Duration, median: Duration, tally: &str) -> String {
    let ratio = elapsed.as_secs_f64() / median.as_secs_f64().max(1.0);
    format!(
        "MID-BATTLE JUDGMENT. Champion {tag} has fought for {:.1} minutes; the median \
         finisher needed {:.1} minutes ({ratio:.1}x). Their most recent actions:\n{tally}\n\n\
         A champion stuck repeating the same futile blow (for example, editing one file \
         over and over without progress) must be CUT DOWN so the tournament can proceed. \
         A champion visibly making steady progress on genuinely larger work deserves \
         MERCY.\n\n\
         Do not use any tools. Respond with ONLY this JSON object — no prose, no code \
         fences:\n\
         {{\"verdict\":\"cut_down\"|\"mercy\",\"reason\":\"<one short sentence>\"}}",
        elapsed.as_secs_f64() / 60.0,
        median.as_secs_f64() / 60.0,
    )
}

// ---------------------------------------------------------------------------
// Champions: fight + artifact capture
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct CapturedArtifact {
    diffstat: String,
    diff: String,
    truncated: bool,
}

impl CapturedArtifact {
    fn is_empty(&self) -> bool {
        self.diff.trim().is_empty()
    }
}

#[derive(Debug)]
struct FighterReport {
    id: FighterId,
    worktree: Option<worktree::CreatedWorktree>,
    artifact: Option<CapturedArtifact>,
    final_text: String,
    slain_reason: Option<String>,
}

/// Everything one champion needs to fight: the task, the battleground, the
/// shared forge lock, their personal kill switch, the watchdog ping line,
/// and the arena event channel.
struct FightOrders {
    task: String,
    cwd: PathBuf,
    forge_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    kill: watch::Receiver<bool>,
    kill_reason: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    ping: mpsc::UnboundedSender<WatchdogPing>,
    tx: mpsc::UnboundedSender<RagnarokEvent>,
    abort: watch::Receiver<bool>,
    /// Camps forged so far in this battle, swept away if the whole battle
    /// aborts/fails (see [`WorktreeRegistry`]).
    camps: WorktreeRegistry,
}

async fn fight(fighter: Candidate, orders: FightOrders) -> FighterReport {
    let FightOrders {
        task,
        cwd,
        forge_lock,
        kill,
        kill_reason,
        ping,
        tx,
        abort,
        camps,
    } = orders;
    let id = fighter.card.id;
    let mut report = FighterReport {
        id,
        worktree: None,
        artifact: None,
        final_text: String::new(),
        slain_reason: None,
    };
    let set_state = |state: FighterState| {
        let _ = tx.send(RagnarokEvent::FighterState { id, state });
    };
    // Merge the global abort and this fighter's personal kill switch into
    // the one stop channel the agent driver understands.
    let (merged_tx, merged) = watch::channel(false);
    tokio::spawn(async move {
        tokio::select! {
            _ = wait_abort(abort) => {}
            _ = wait_abort(kill) => {}
        }
        let _ = merged_tx.send(true);
    });
    // When the kill switch fired, its reason outranks the generic error.
    let kill_note = |fallback: String| -> String {
        kill_reason
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .unwrap_or(fallback)
    };

    set_state(FighterState::Forging);
    let hint = format!(
        "ragnarok-{}-{}",
        fighter.card.model_name, fighter.card.agent_source_id
    );
    let forge_cwd = cwd.clone();
    // `git worktree add` mutates shared repo metadata; forging one camp at a
    // time keeps ten simultaneous champions from tripping over git's locks.
    let created = {
        let _guard = forge_lock.lock().await;
        tokio::task::spawn_blocking(move || worktree::create_for_automation(&forge_cwd, &hint))
            .await
    };
    let created = match created {
        Ok(Ok(w)) => w,
        Ok(Err(e)) => {
            return slain(
                report,
                set_state,
                format!("could not forge worktree: {e:#}"),
            );
        }
        Err(e) => return slain(report, set_state, format!("worktree task failed: {e}")),
    };
    register_camp(&camps, &created);
    let worktree_name = created
        .worktree_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| created.worktree_root.display().to_string());
    let base_sha = match git_capture(&created.worktree_root, &["rev-parse", "HEAD"]).await {
        Ok(sha) => sha.trim().to_string(),
        Err(e) => return slain(report, set_state, format!("could not read base sha: {e:#}")),
    };
    let _ = tx.send(RagnarokEvent::FighterWorktree {
        id,
        name: worktree_name,
        path: created.worktree_root.clone(),
        base_sha: base_sha.clone(),
    });
    report.worktree = Some(created.clone());

    set_state(FighterState::Connecting);
    let mut handle = match AgentHandle::connect(
        &fighter.launch,
        &created.session_cwd,
        &[],
        merged.clone(),
        acp::RuntimeAccessMode::Full,
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            return slain(
                report,
                set_state,
                kill_note(format!("never reached the arena: {e:#}")),
            );
        }
    };
    if let Err(e) = handle.arm_model(&fighter.card.model_value).await {
        handle.dismiss().await;
        return slain(
            report,
            set_state,
            kill_note(format!("could not arm their model: {e:#}")),
        );
    }

    let mut cry_roll = id.wrapping_mul(7);
    let mut chunk_count = 0usize;
    let fighter_name = fighter.card.model_name.clone();
    let tx_events = tx.clone();
    let mut prompt = fight_prompt(&task);
    let mut continuation_count = 0usize;
    loop {
        set_state(FighterState::Fighting);
        let prompt_kind = if continuation_count > 0 {
            "continuation"
        } else {
            "combat"
        };
        if continuation_count > 0 {
            let _ = tx.send(RagnarokEvent::Log {
                fighter: Some(id),
                text: format!(
                    "📯 THOR's continuation prompt is being sent to {} ({continuation_count}/{EMPTY_DIFF_CONTINUATION_LIMIT}).",
                    fighter.card.model_name
                ),
            });
        }
        let _ = tx.send(RagnarokEvent::FighterText {
            id,
            lane: TextLane::Tool,
            chunk: prompt_marker(prompt_kind, &prompt),
        });
        let outcome = handle
            .prompt(prompt, FIGHT_TIMEOUT, |ev| {
                if let TurnEvent::Tool {
                    title,
                    started: true,
                    ..
                } = &ev
                {
                    let _ = ping.send(WatchdogPing {
                        id,
                        title: title.clone(),
                    });
                }
                forward_turn_event(
                    &tx_events,
                    id,
                    &fighter_name,
                    ev,
                    TextLane::Message,
                    &mut cry_roll,
                    &mut chunk_count,
                );
            })
            .await;
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                handle.dismiss().await;
                return slain(
                    report,
                    set_state,
                    kill_note(format!("fell in battle: {e:#}")),
                );
            }
        };
        if !turn_succeeded(outcome.stop) {
            handle.dismiss().await;
            return slain(
                report,
                set_state,
                kill_note(format!("yielded ({})", stop_reason_label(outcome.stop))),
            );
        }
        append_turn_text(&mut report.final_text, &outcome.text);
        if continuation_count > 0 {
            let _ = tx.send(RagnarokEvent::Log {
                fighter: Some(id),
                text: format!(
                    "🔎 {}'s continuation turn ended; recapturing the diff.",
                    fighter.card.model_name
                ),
            });
        }

        set_state(FighterState::Capturing);
        let root = created.worktree_root.clone();
        let artifact = capture_artifact(&root, &base_sha).await;
        match artifact {
            Ok(a) => {
                let empty = a.is_empty();
                let _ = tx.send(RagnarokEvent::FighterDiffStat {
                    id,
                    stat: a.diffstat.clone(),
                });
                if empty && continuation_count < EMPTY_DIFF_CONTINUATION_LIMIT {
                    continuation_count += 1;
                    let _ = tx.send(RagnarokEvent::Log {
                        fighter: Some(id),
                        text: format!(
                            "⚡ THOR finds no changed files from {}; queueing continuation {continuation_count}/{EMPTY_DIFF_CONTINUATION_LIMIT}.",
                            fighter.card.model_name,
                        ),
                    });
                    prompt = empty_diff_continue_prompt(&task);
                    continue;
                }
                if empty {
                    let _ = tx.send(RagnarokEvent::Log {
                        fighter: Some(id),
                        text: format!(
                            "⚠ THOR still sees no changed files from {} after continuation; carrying the empty artifact forward.",
                            fighter.card.model_name
                        ),
                    });
                } else if continuation_count > 0 {
                    let _ = tx.send(RagnarokEvent::Log {
                        fighter: Some(id),
                        text: format!(
                            "✅ THOR sees a diff from {} after continuation.",
                            fighter.card.model_name
                        ),
                    });
                }
                report.artifact = Some(a);
            }
            Err(e) => {
                let _ = tx.send(RagnarokEvent::Log {
                    fighter: Some(id),
                    text: format!("⚠ artifact capture faltered: {e:#}"),
                });
            }
        }
        break;
    }
    handle.dismiss().await;
    set_state(FighterState::Standing);
    let _ = tx.send(RagnarokEvent::Log {
        fighter: Some(id),
        text: format!(
            "🏁 {} plants their banner: the work is done!",
            fighter.card.model_name
        ),
    });
    report
}

fn append_turn_text(acc: &mut String, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    if !acc.is_empty() {
        acc.push_str("\n\n--- continuation turn ---\n");
    }
    acc.push_str(text);
}

fn prompt_marker(kind: &str, prompt: &str) -> String {
    let task = prompt
        .split("THE TASK:\n")
        .nth(1)
        .map(str::trim)
        .filter(|task| !task.is_empty())
        .map(|task| truncate_middle(task, PROMPT_MARKER_TASK_LIMIT))
        .unwrap_or_else(|| first_line(prompt, PROMPT_MARKER_TASK_LIMIT));
    format!("\n▶ mj sent {kind} prompt to this fighter\nTASK:\n{task}\n\n")
}

fn slain(
    mut report: FighterReport,
    set_state: impl Fn(FighterState),
    reason: String,
) -> FighterReport {
    set_state(FighterState::Slain(reason.clone()));
    report.slain_reason = Some(reason);
    report
}

/// Fold a digested turn event into arena events: transcript text, an action
/// for the animation, and (sometimes) a silly battle cry for the feed.
#[allow(clippy::too_many_arguments)]
fn forward_turn_event(
    tx: &mpsc::UnboundedSender<RagnarokEvent>,
    id: FighterId,
    fighter_name: &str,
    ev: TurnEvent,
    message_lane: TextLane,
    cry_roll: &mut usize,
    chunk_count: &mut usize,
) {
    match ev {
        TurnEvent::Message(chunk) => {
            *chunk_count += 1;
            if *chunk_count == 1 || chunk_count.is_multiple_of(40) {
                *cry_roll += 1;
                let _ = tx.send(RagnarokEvent::FighterAction {
                    id,
                    action: ActionKind::Chant,
                    detail: first_line(&chunk, 60),
                });
            }
            let _ = tx.send(RagnarokEvent::FighterText {
                id,
                lane: message_lane,
                chunk,
            });
        }
        TurnEvent::Thought(chunk) => {
            *cry_roll += 1;
            if cry_roll.is_multiple_of(3) {
                let _ = tx.send(RagnarokEvent::FighterAction {
                    id,
                    action: ActionKind::Ponder,
                    detail: first_line(&chunk, 60),
                });
            }
            let _ = tx.send(RagnarokEvent::FighterText {
                id,
                lane: TextLane::Thought,
                chunk,
            });
        }
        TurnEvent::Tool {
            title,
            kind,
            status,
            started,
        } => {
            let failed = status == Some(ToolCallStatus::Failed);
            let action = if failed {
                ActionKind::Wound
            } else {
                match kind {
                    Some(ToolKind::Edit | ToolKind::Delete | ToolKind::Move) => ActionKind::Forge,
                    Some(ToolKind::Execute) => ActionKind::Strike,
                    Some(ToolKind::Read | ToolKind::Search | ToolKind::Fetch) => ActionKind::Scry,
                    Some(ToolKind::Think) => ActionKind::Ponder,
                    _ => ActionKind::Strike,
                }
            };
            if started || failed {
                *cry_roll += 1;
                let _ = tx.send(RagnarokEvent::FighterAction {
                    id,
                    action,
                    detail: first_line(&title, 60),
                });
                let _ = tx.send(RagnarokEvent::Log {
                    fighter: Some(id),
                    text: battle_cry(fighter_name, action, &first_line(&title, 48), *cry_roll),
                });
                let _ = tx.send(RagnarokEvent::FighterText {
                    id,
                    lane: TextLane::Tool,
                    chunk: format!(
                        "\n⚙ [{}] {}\n",
                        tool_kind_word(kind),
                        first_line(&title, 100)
                    ),
                });
            }
        }
        TurnEvent::Permission {
            prompt,
            access_mode,
        } => {
            let decision = permission_decision_for_access(access_mode, &prompt);
            let _ = prompt.responder.send(decision);
            let _ = tx.send(RagnarokEvent::Log {
                fighter: Some(id),
                text: format!("🛡 {fighter_name} permission auto-answered"),
            });
        }
        TurnEvent::Note(note) => {
            let shown = first_line(&note, 90);
            let _ = tx.send(RagnarokEvent::FighterAction {
                id,
                action: ActionKind::Guard,
                detail: first_line(&note, 60),
            });
            let _ = tx.send(RagnarokEvent::Log {
                fighter: Some(id),
                text: format!("🛡 {fighter_name} runtime note: {shown}"),
            });
        }
    }
}

fn tool_kind_word(kind: Option<ToolKind>) -> &'static str {
    match kind {
        Some(k) => crate::labels::tool_kind_label(k),
        None => "tool",
    }
}

fn fight_prompt(task: &str) -> String {
    format!(
        "⚔ RAGNAROK. You are one of several rival AI coding agents. Each rival is \
         implementing the SAME task in parallel, each in an isolated git worktree. When \
         combat ends, a rival will adversarially review your work, and Thor will judge \
         whose implementation most faithfully and completely satisfies the task. Only \
         one can win.\n\n\
         Rules of combat:\n\
         - Implement the task below in the current working directory (your private worktree).\n\
         - Do NOT create git commits. Leave every change in the working tree.\n\
         - Do NOT push, and do NOT touch anything outside this worktree.\n\
         - Verify your work (build/tests) when the project allows it.\n\
         - Finish with a concise summary of what you built and how you verified it. \
           Overclaiming will be found out in review.\n\n\
         THE TASK:\n{task}"
    )
}

fn empty_diff_continue_prompt(task: &str) -> String {
    format!(
        "⚡ THOR INSPECTION. Your previous turn ended, but Thor checked `git diff` \
         against the starting commit and found no changes. You have not produced an \
         implementation artifact for Ragnarok.\n\n\
         Continue in this same worktree now.\n\n\
         Rules:\n\
         - Implement the task below in the current working directory.\n\
         - Do NOT create git commits. Leave every change in the working tree.\n\
         - Do NOT push, and do NOT touch anything outside this worktree.\n\
         - Verify your work when the project allows it.\n\
         - Finish with a concise summary of the changes and verification.\n\n\
         THE TASK:\n{task}"
    )
}

/// `git add -N` makes untracked files visible to `git diff`, then diff against
/// the sha captured at worktree creation (immune to agents committing despite
/// the rules).
async fn capture_artifact(worktree_root: &Path, base_sha: &str) -> Result<CapturedArtifact> {
    let _ = git_capture(worktree_root, &["add", "-A", "-N"]).await;
    let diffstat = git_capture_capped(
        worktree_root,
        &["diff", "--stat", base_sha],
        DIFFSTAT_CAPTURE_LIMIT,
    )
    .await?;
    let diff = git_capture_capped(worktree_root, &["diff", base_sha], DIFF_CAPTURE_LIMIT).await?;
    let truncated = diff.truncated || diffstat.truncated;
    Ok(CapturedArtifact {
        diffstat: diffstat.text.trim_end().to_string(),
        diff: diff.text,
        truncated,
    })
}

async fn git_capture(dir: &Path, args: &[&str]) -> Result<String> {
    let dir = dir.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let joined = args.join(" ");
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(&args)
        .output()
        .await
        .with_context(|| format!("run git {joined} in {}", dir.display()))?;
    if !output.status.success() {
        bail!(
            "git {joined} failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn gh_capture(dir: &Path, args: &[&str]) -> Result<String> {
    let dir = dir.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let joined = args.join(" ");
    let output = tokio::process::Command::new("gh")
        .current_dir(&dir)
        .args(&args)
        .output()
        .await
        .with_context(|| format!("run gh {joined} in {}", dir.display()))?;
    if !output.status.success() {
        bail!(
            "gh {joined} failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub async fn publish_draft_pr(req: DraftPrRequest) -> Result<String> {
    let status = git_capture(
        &req.worktree_path,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )
    .await?;
    let has_worktree_changes = !status.trim().is_empty();
    let head = git_capture(&req.worktree_path, &["rev-parse", "HEAD"]).await?;
    let has_committed_changes = head.trim() != req.base_sha.trim();
    if !has_worktree_changes && !has_committed_changes {
        bail!("winner worktree has no changes to publish");
    }

    let base = origin_default_branch(&req.worktree_path).await?;
    let branch = draft_pr_branch_name(req.winner, &req.task);
    let title = format!("Ragnarok: {}", first_line(&req.task, 72));
    let commit_subject = format!("ragnarok winner: {}", first_line(&req.task, 60));
    let body = format!(
        "Draft PR generated from Ragnarok's winning pick.\n\n\
         Winner: {}\n\n\
         Task:\n{}\n\n\
         Source worktree: `{}`\n",
        req.winner_tag,
        req.task,
        req.worktree_path.display(),
    );

    git_capture(&req.worktree_path, &["switch", "-c", &branch]).await?;
    if has_worktree_changes {
        git_capture(&req.worktree_path, &["add", "-A"]).await?;
        git_capture(&req.worktree_path, &["commit", "-m", &commit_subject]).await?;
    }
    git_capture(&req.worktree_path, &["push", "-u", "origin", &branch]).await?;
    let out = gh_capture(
        &req.worktree_path,
        &[
            "pr", "create", "--draft", "--base", &base, "--head", &branch, "--title", &title,
            "--body", &body,
        ],
    )
    .await?;
    let url = out
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("http://") || line.starts_with("https://"))
        .unwrap_or(out.trim());
    if url.is_empty() {
        bail!("gh created a draft PR but did not return its URL");
    }
    Ok(url.to_string())
}

async fn origin_default_branch(dir: &Path) -> Result<String> {
    match git_capture(
        dir,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .await
    {
        Ok(head) => {
            let head = head.trim();
            if let Some(branch) = head.strip_prefix("origin/")
                && !branch.is_empty()
            {
                return Ok(branch.to_string());
            }
        }
        Err(e) => tracing::debug!("origin/HEAD lookup failed: {e:#}"),
    }

    let remote = git_capture(dir, &["remote", "show", "-n", "origin"]).await?;
    remote
        .lines()
        .find_map(|line| line.trim().strip_prefix("HEAD branch: "))
        .map(str::trim)
        .filter(|branch| !branch.is_empty() && *branch != "(unknown)")
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow!("could not determine origin default branch; set origin/HEAD before publishing")
        })
}

fn draft_pr_branch_name(winner: FighterId, task: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true;
    for ch in task.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if mapped == '-' {
            if last_dash {
                continue;
            }
            last_dash = true;
        } else {
            last_dash = false;
            slug.push(mapped);
        }
        if slug.len() >= 36 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "winner" } else { slug };
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("ragnarok/{slug}-{winner}-{millis}")
}

struct CappedOutput {
    text: String,
    truncated: bool,
}

async fn git_capture_capped(dir: &Path, args: &[&str], limit: usize) -> Result<CappedOutput> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let joined = args.join(" ");
    let mut child = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn git {joined} in {}", dir.display()))?;
    let stdout = child
        .stdout
        .take()
        .context("git child stdout was not piped")?;
    let stderr = child
        .stderr
        .take()
        .context("git child stderr was not piped")?;
    let stdout_task = tokio::spawn(read_capped_output(stdout, limit));
    let stderr_task = tokio::spawn(read_capped_output(stderr, GIT_STDERR_CAPTURE_LIMIT));
    let status = child
        .wait()
        .await
        .with_context(|| format!("wait for git {joined} in {}", dir.display()))?;
    let stdout = stdout_task
        .await
        .context("git stdout reader task failed")?
        .context("read git stdout")?;
    let stderr = stderr_task
        .await
        .context("git stderr reader task failed")?
        .context("read git stderr")?;
    if !status.success() {
        bail!(
            "git {joined} failed in {}: {}",
            dir.display(),
            stderr.text.trim()
        );
    }
    Ok(stdout)
}

async fn read_capped_output<R>(mut reader: R, limit: usize) -> std::io::Result<CappedOutput>
where
    R: AsyncRead + Unpin,
{
    let head_budget = limit * 7 / 10;
    let tail_budget = limit.saturating_sub(head_budget);
    let mut head = Vec::with_capacity(head_budget);
    let mut tail = VecDeque::with_capacity(tail_budget);
    let mut total = 0usize;
    let mut buf = [0u8; 8192];

    loop {
        let read = reader.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read);
        let mut chunk = &buf[..read];
        if head.len() < head_budget {
            let take = (head_budget - head.len()).min(chunk.len());
            head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }
        if tail_budget > 0 {
            for &byte in chunk {
                if tail.len() == tail_budget {
                    tail.pop_front();
                }
                tail.push_back(byte);
            }
        }
    }

    let tail: Vec<u8> = tail.into_iter().collect();
    let stored = head.len() + tail.len();
    let truncated = total > stored;
    let mut out = head;
    if truncated {
        let excised = total.saturating_sub(stored);
        out.extend_from_slice(format!("\n...[{excised} bytes excised]...\n").as_bytes());
    }
    out.extend_from_slice(&tail);
    Ok(CappedOutput {
        text: String::from_utf8_lossy(&out).into_owned(),
        truncated,
    })
}

// ---------------------------------------------------------------------------
// Adversarial reviews
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ReviewReport {
    assignment: Assignment,
    text: String,
    delivered: bool,
}

async fn review(
    reviewer: Candidate,
    assignment: Assignment,
    prompt: String,
    defender_cwd: PathBuf,
    tx: mpsc::UnboundedSender<RagnarokEvent>,
    abort: watch::Receiver<bool>,
) -> ReviewReport {
    let set_progress = |progress: ReviewProgress| {
        let _ = tx.send(RagnarokEvent::ReviewState {
            reviewer: assignment.reviewer,
            progress,
        });
    };
    set_progress(ReviewProgress::Connecting);
    let mut handle = match AgentHandle::connect(
        &reviewer.launch,
        &defender_cwd,
        &[],
        abort.clone(),
        acp::RuntimeAccessMode::ReadOnly,
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            set_progress(ReviewProgress::Failed);
            return ReviewReport {
                assignment,
                text: format!("(review not delivered: {e:#})"),
                delivered: false,
            };
        }
    };
    if let Err(e) = handle.arm_model(&reviewer.card.model_value).await {
        handle.dismiss().await;
        set_progress(ReviewProgress::Failed);
        return ReviewReport {
            assignment,
            text: format!("(review not delivered: {e:#})"),
            delivered: false,
        };
    }
    set_progress(ReviewProgress::Reviewing);
    let id = assignment.reviewer;
    let name = reviewer.card.model_name.clone();
    let mut cry_roll = id.wrapping_mul(13);
    let mut chunk_count = 0usize;
    let tx_events = tx.clone();
    let outcome = handle
        .prompt(prompt, REVIEW_TIMEOUT, |ev| {
            forward_turn_event(
                &tx_events,
                id,
                &name,
                ev,
                TextLane::Review,
                &mut cry_roll,
                &mut chunk_count,
            );
        })
        .await;
    handle.dismiss().await;
    match outcome {
        Ok(o) if turn_succeeded(o.stop) => {
            set_progress(ReviewProgress::Done);
            let _ = tx.send(RagnarokEvent::Log {
                fighter: Some(id),
                text: format!("🔍 {name} delivers a merciless review scroll."),
            });
            ReviewReport {
                assignment,
                text: o.text,
                delivered: true,
            }
        }
        Ok(o) => {
            set_progress(ReviewProgress::Failed);
            ReviewReport {
                assignment,
                text: format!("(review not delivered: {})", stop_reason_label(o.stop)),
                delivered: false,
            }
        }
        Err(e) => {
            set_progress(ReviewProgress::Failed);
            ReviewReport {
                assignment,
                text: format!("(review not delivered: {e:#})"),
                delivered: false,
            }
        }
    }
}

fn review_prompt(
    task: &str,
    reviewer: &FighterCard,
    defender: &FighterCard,
    artifact: Option<&CapturedArtifact>,
    defender_summary: &str,
) -> String {
    let diff = artifact
        .map(|a| truncate_middle(&a.diff, DIFF_FOR_REVIEW_LIMIT))
        .unwrap_or_else(|| "(no diff was captured)".to_string());
    let diffstat = artifact.map(|a| a.diffstat.clone()).unwrap_or_default();
    format!(
        "🛡 RAGNAROK ADVERSARIAL REVIEW. You are {reviewer_name}. Your rival \
         {defender_name} implemented the task below; you are standing inside THEIR git \
         worktree. Tear the implementation apart — but honestly. Thor will judge YOUR \
         review for honesty and validity against the actual code, so fabricated flaws \
         or lazy praise will cost you.\n\n\
         Rules:\n\
         - Inspect their real changes (diff included below; verify key claims against files).\n\
         - Do NOT modify anything. Analysis only.\n\
         - Judge fidelity to the task as written, correctness, completeness, and quality.\n\n\
         Deliver exactly these sections:\n\
         VERDICT: one line — SHIP IT | FLAWED | FATALLY FLAWED\n\
         REQUIREMENT COVERAGE: score 0-10 and one sentence why\n\
         FLAWS: numbered list with file:line evidence (or 'none found')\n\
         STRENGTHS: short list\n\
         LIES OR OVERCLAIMS: claims in their summary the code does not deliver (or 'none')\n\n\
         THE TASK THEY WERE GIVEN:\n{task}\n\n\
         THEIR DIFFSTAT:\n{diffstat}\n\n\
         THEIR DIFF (may be truncated):\n{diff}\n\n\
         THEIR CLOSING SUMMARY:\n{summary}",
        reviewer_name = reviewer.tag(),
        defender_name = defender.tag(),
        task = task,
        diffstat = diffstat,
        diff = diff,
        summary = truncate_middle(defender_summary, SUMMARY_LIMIT),
    )
}

// ---------------------------------------------------------------------------
// Thor: route → assign → judge (one persistent session)
// ---------------------------------------------------------------------------

struct Thor {
    handle: AgentHandle,
    /// Thor's own disposable worktree. Like the champions he judges, Thor
    /// runs with auto-granted permissions inside a throwaway copy — a
    /// rejected/cancelled permission makes agents cancel the whole turn,
    /// which is how the first live Thor died. Removed at dismissal; battles
    /// that error out mid-flight leave it behind like any fighter camp.
    camp: worktree::CreatedWorktree,
}

#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub complexity: String,
    pub competitors: usize,
    pub rationale: String,
}

impl Thor {
    async fn summon(
        host: ThorHost,
        camp: worktree::CreatedWorktree,
        abort: watch::Receiver<bool>,
    ) -> Result<Self> {
        let mut handle = AgentHandle::connect(
            &host.launch,
            &camp.session_cwd,
            &[],
            abort,
            acp::RuntimeAccessMode::Full,
        )
        .await
        .context("Thor could not descend (agent connect failed)")?;
        if let Some(model_value) = host.model_value.as_deref() {
            handle
                .arm_model(model_value)
                .await
                .context("Thor could not take form (model select failed)")?;
        }
        Ok(Self { handle, camp })
    }

    /// One Thor turn: prompt, stream to the Thor panel, return the final text.
    async fn speak(
        &mut self,
        prompt: String,
        tx: &mpsc::UnboundedSender<RagnarokEvent>,
    ) -> Result<String> {
        self.speak_budget(prompt, THOR_TIMEOUT, tx).await
    }

    async fn speak_budget(
        &mut self,
        prompt: String,
        budget: Duration,
        tx: &mpsc::UnboundedSender<RagnarokEvent>,
    ) -> Result<String> {
        let tx = tx.clone();
        let outcome = self
            .handle
            .prompt(prompt, budget, |ev| {
                if let TurnEvent::Message(chunk) = ev {
                    let _ = tx.send(RagnarokEvent::ThorSpeaks(chunk));
                }
            })
            .await?;
        if !turn_succeeded(outcome.stop) {
            bail!("Thor fell silent ({})", stop_reason_label(outcome.stop));
        }
        Ok(outcome.text)
    }

    /// Mid-combat straggler ruling: `(cut_down, reason)`. Never fails the
    /// battle — when Thor's ruling is garbled or he cannot be reached, the
    /// deterministic loop detector rules in his stead, and says so.
    async fn mercy(
        &mut self,
        fighter_tag: &str,
        elapsed: Duration,
        median: Duration,
        tally: &str,
        recent: &[String],
        tx: &mpsc::UnboundedSender<RagnarokEvent>,
    ) -> (bool, String) {
        let prompt = mercy_prompt(fighter_tag, elapsed, median, tally);
        for attempt in 0..2 {
            let text = if attempt == 0 {
                self.speak_budget(prompt.clone(), MERCY_TURN_TIMEOUT, tx)
                    .await
            } else {
                self.speak_budget(
                    "Your previous reply could not be parsed. Respond again with ONLY the \
                     JSON object described before — no prose, no code fences."
                        .to_string(),
                    MERCY_TURN_TIMEOUT,
                    tx,
                )
                .await
            };
            match text {
                Ok(text) => {
                    if let Some(ruling) = parse_mercy(&text) {
                        return ruling;
                    }
                }
                Err(_) => break,
            }
        }
        if looks_stuck(recent) {
            (
                true,
                "Thor's ruling was garbled; the loop-detector decreed death (the same blow \
                 repeated over and over)"
                    .to_string(),
            )
        } else {
            (
                false,
                "Thor's ruling was garbled; mercy by default".to_string(),
            )
        }
    }

    async fn route(
        &mut self,
        task: &str,
        tx: &mpsc::UnboundedSender<RagnarokEvent>,
    ) -> Result<RouteDecision> {
        let prompt = route_prompt(task);
        let first = self.speak(prompt, tx).await?;
        if let Some(route) = parse_route(&first) {
            return Ok(route);
        }
        feed(
            tx,
            None,
            "🌩 Thor's first pronouncement was garbled; asking again.",
        )?;
        let retry = self
            .speak(
                "Your previous reply could not be parsed. Respond again with ONLY the JSON \
                 object described before — no prose, no code fences."
                    .to_string(),
                tx,
            )
            .await?;
        if let Some(route) = parse_route(&retry) {
            return Ok(route);
        }
        let fallback = route_by_runes(task);
        feed(
            tx,
            None,
            format!(
                "🌩 Thor's ravens garbled the message twice; the rune-count heuristic decrees \
                 {} champions.",
                fallback.competitors
            ),
        )?;
        Ok(fallback)
    }

    async fn assign(
        &mut self,
        survivors: &[FighterId],
        cards: &[FighterCard],
        tx: &mpsc::UnboundedSender<RagnarokEvent>,
    ) -> Result<Vec<Assignment>> {
        let prompt = assign_prompt(survivors, cards);
        for attempt in 0..2 {
            let text = if attempt == 0 {
                self.speak(prompt.clone(), tx).await?
            } else {
                self.speak(
                    "Your previous reply could not be used. Respond with ONLY the JSON object \
                     (every champion reviews exactly one rival, nobody reviews themselves, \
                     every implementation reviewed exactly once)."
                        .to_string(),
                    tx,
                )
                .await?
            };
            if let Some(assignments) = parse_assignments(&text, survivors) {
                return Ok(assignments);
            }
        }
        feed(
            tx,
            None,
            "🌩 Thor's pairings were invalid; the wheel of fate rotates the assignments instead.",
        )?;
        Ok(assignments_rotation(survivors))
    }

    async fn judge(
        &mut self,
        dossier: &str,
        survivors: &[FighterId],
        allowed_reviewers: &[FighterId],
        tx: &mpsc::UnboundedSender<RagnarokEvent>,
    ) -> Result<Verdict> {
        let prompt = judge_prompt(dossier);
        for attempt in 0..2 {
            let text = if attempt == 0 {
                self.speak(prompt.clone(), tx).await?
            } else {
                self.speak(
                    "Your previous reply could not be parsed. Respond with ONLY the JSON object \
                     described before — no prose, no code fences."
                        .to_string(),
                    tx,
                )
                .await?
            };
            if let Some(verdict) =
                parse_judgment_with_reviewers(&text, survivors, allowed_reviewers)
            {
                return Ok(verdict);
            }
        }
        bail!("judgment unparseable after retry")
    }

    async fn dismiss(self) {
        self.handle.dismiss().await;
        let camp = self.camp;
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) =
                worktree::remove_automation_worktree(&camp.project_root, &camp.worktree_root)
            {
                tracing::warn!("remove thor camp {:?}: {e:#}", camp.worktree_root);
            }
        })
        .await;
    }
}

fn route_prompt(task: &str) -> String {
    format!(
        "You are THOR, arbiter of RAGNAROK: a tournament where several rival AI coding \
         agents each implement the same task in parallel and the best implementation \
         wins. Assess the complexity of the task below and decree how many champions \
         shall battle — an integer from {MIN_FIGHTERS} to {MAX_FIGHTERS}. Keep the field \
         small by default: trivial/simple tasks should normally use 2 champions, ordinary \
         moderate tasks should use 3, complex tasks should use 4-5, and 6+ champions should \
         be reserved for broad, risky, ambiguous, cross-cutting architecture/security/migration \
         work. UI polish, spacing, copy, and small animation tasks are usually simple unless \
         they require a major state-machine or rendering rewrite.\n\n\
         Do not use any tools. Respond with ONLY this JSON object — no prose, no code \
         fences:\n\
         {{\"complexity\":\"trivial|simple|moderate|complex|epic\",\
         \"competitors\":<{MIN_FIGHTERS}-{MAX_FIGHTERS}>,\
         \"rationale\":\"<one short sentence>\"}}\n\n\
         THE TASK:\n{task}"
    )
}

fn assign_prompt(survivors: &[FighterId], cards: &[FighterCard]) -> String {
    let roster: Vec<String> = survivors
        .iter()
        .map(|id| format!("{{\"id\":{},\"name\":\"{}\"}}", id, cards[*id].model_name))
        .collect();
    format!(
        "The champions below each produced an implementation. Assign each champion \
         exactly one RIVAL's implementation to adversarially review. Every implementation \
         must be reviewed exactly once, and no champion may review their own work.\n\n\
         Champions: [{}]\n\n\
         Do not use any tools. Respond with ONLY this JSON object:\n\
         {{\"assignments\":[{{\"reviewer\":<id>,\"defender\":<id>}},…]}}",
        roster.join(",")
    )
}

fn judge_prompt(dossier: &str) -> String {
    format!(
        "All implementations and adversarial reviews are in. Judge RAGNAROK.\n\n\
         Step 1 — judge the reviews: for each review, score its HONESTY (did the \
         reviewer argue in good faith, neither inventing flaws nor flattering?) and \
         VALIDITY (do its claims hold against the actual diff?) from 0-10.\n\
         Step 2 — judge the implementations: rank every champion by how faithfully, \
         completely, and correctly their implementation satisfies THE TASK as written. \
         Weigh verified review findings; discount review claims you judged dishonest \
         or invalid.\n\
         Step 3 — verdict: if one champion is clearly best, declare them the winner. \
         If the top two are genuinely close, name two finalists instead. Exactly one of \
         \"clear_winner\" / \"finalists\" must be non-null.\n\n\
         Do not use any tools. Respond with ONLY this JSON object — no prose, no code \
         fences:\n\
         {{\"review_verdicts\":[{{\"reviewer\":<id>,\"defender\":<id>,\"honesty\":<0-10>,\
         \"validity\":<0-10>,\"notes\":\"<short>\"}}],\
         \"ranking\":[<ids best to worst>],\
         \"clear_winner\":<id or null>,\
         \"finalists\":[<id>,<id>] or null,\
         \"reasoning\":\"<a few sentences>\"}}\n\n\
         {dossier}"
    )
}

/// The complete, size-budgeted judgment dossier.
fn judgment_dossier(
    task: &str,
    cards: &[FighterCard],
    survivors: &[FighterId],
    reports: &HashMap<FighterId, &FighterReport>,
    reviews: &[ReviewReport],
) -> String {
    let mut out = format!("THE TASK:\n{task}\n");
    for id in survivors {
        let card = &cards[*id];
        out.push_str(&format!("\n===== CHAMPION {id}: {} =====\n", card.tag()));
        if let Some(report) = reports.get(id) {
            match &report.artifact {
                Some(a) => {
                    out.push_str(&format!("DIFFSTAT:\n{}\n", a.diffstat));
                    out.push_str(&format!(
                        "DIFF{}:\n{}\n",
                        if a.truncated { " (truncated)" } else { "" },
                        truncate_middle(&a.diff, DIFF_FOR_JUDGMENT_LIMIT)
                    ));
                }
                None => out.push_str("DIFF: (none captured)\n"),
            }
            out.push_str(&format!(
                "THEIR CLOSING SUMMARY:\n{}\n",
                truncate_middle(&report.final_text, SUMMARY_LIMIT)
            ));
        }
        for review in reviews.iter().filter(|r| r.assignment.defender == *id) {
            out.push_str(&format!(
                "REVIEW OF CHAMPION {id} BY CHAMPION {} ({}){}:\n{}\n",
                review.assignment.reviewer,
                cards[review.assignment.reviewer].model_name,
                if review.delivered {
                    ""
                } else {
                    " — NOT DELIVERED"
                },
                truncate_middle(&review.text, REVIEW_FOR_JUDGMENT_LIMIT)
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Thor output parsing (lenient, validated)
// ---------------------------------------------------------------------------

/// Find the first balanced `{…}` region that parses as JSON. Tolerates prose
/// and code fences around the object.
pub fn extract_json_object(text: &str) -> Option<serde_json::Value> {
    let bytes = text.as_bytes();
    let mut start = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' if start.is_some() => in_string = true,
            b'{' => {
                if start.is_none() {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' if start.is_some() => {
                depth -= 1;
                if depth == 0 {
                    let candidate = &text[start.unwrap()..=i];
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) {
                        return Some(value);
                    }
                    start = None;
                }
            }
            _ => {}
        }
    }
    None
}

#[derive(Debug, Deserialize)]
struct RawRoute {
    complexity: Option<String>,
    competitors: Option<i64>,
    rationale: Option<String>,
}

pub fn parse_route(text: &str) -> Option<RouteDecision> {
    let value = extract_json_object(text)?;
    let raw: RawRoute = serde_json::from_value(value).ok()?;
    let competitors = raw.competitors?;
    if !(1..=100).contains(&competitors) {
        return None;
    }
    Some(RouteDecision {
        complexity: raw.complexity.unwrap_or_else(|| "unknowable".to_string()),
        competitors: (competitors as usize).clamp(MIN_FIGHTERS, MAX_FIGHTERS),
        rationale: raw
            .rationale
            .unwrap_or_else(|| "Thor keeps his counsel.".to_string()),
    })
}

/// Deterministic complexity heuristic when Thor's routing is unusable twice.
pub fn route_by_runes(task: &str) -> RouteDecision {
    let runes = task.chars().count();
    let mut competitors = match runes {
        0..=160 => 2,
        161..=600 => 3,
        601..=2000 => 4,
        _ => 5,
    };
    let heavy_words = [
        "refactor",
        "migrate",
        "rewrite",
        "architecture",
        "concurrent",
        "parallel",
        "protocol",
        "database",
        "test",
        "security",
    ];
    let lowered = task.to_lowercase();
    if heavy_words.iter().filter(|w| lowered.contains(**w)).count() >= 2 {
        competitors += 1;
    }
    let complexity = match competitors {
        2 => "simple",
        3 => "moderate",
        4 => "complex",
        _ => "epic",
    };
    RouteDecision {
        complexity: complexity.to_string(),
        competitors: competitors.clamp(MIN_FIGHTERS, MAX_FIGHTERS),
        rationale: "rune-count heuristic (Thor's routing was unusable)".to_string(),
    }
}

#[derive(Debug, Deserialize)]
struct RawAssignments {
    assignments: Vec<RawAssignment>,
}

#[derive(Debug, Deserialize)]
struct RawAssignment {
    reviewer: i64,
    defender: i64,
}

/// Accept Thor's pairings only when they form a perfect derangement over the
/// survivors: everyone reviews exactly once, everyone is reviewed exactly
/// once, nobody reviews themselves.
pub fn parse_assignments(text: &str, survivors: &[FighterId]) -> Option<Vec<Assignment>> {
    let value = extract_json_object(text)?;
    let raw: RawAssignments = serde_json::from_value(value).ok()?;
    let valid: HashSet<FighterId> = survivors.iter().copied().collect();
    let mut reviewers = HashSet::new();
    let mut defenders = HashSet::new();
    let mut out = Vec::new();
    for a in raw.assignments {
        let reviewer = usize::try_from(a.reviewer).ok()?;
        let defender = usize::try_from(a.defender).ok()?;
        if reviewer == defender || !valid.contains(&reviewer) || !valid.contains(&defender) {
            return None;
        }
        if !reviewers.insert(reviewer) || !defenders.insert(defender) {
            return None;
        }
        out.push(Assignment { reviewer, defender });
    }
    (reviewers.len() == survivors.len() && defenders.len() == survivors.len()).then_some(out)
}

/// Everyone reviews the next survivor (wrapping): a valid derangement for any
/// n ≥ 2.
pub fn assignments_rotation(survivors: &[FighterId]) -> Vec<Assignment> {
    let n = survivors.len();
    (0..n)
        .map(|i| Assignment {
            reviewer: survivors[i],
            defender: survivors[(i + 1) % n],
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct RawJudgment {
    review_verdicts: Option<Vec<RawReviewVerdict>>,
    ranking: Option<Vec<i64>>,
    clear_winner: Option<i64>,
    finalists: Option<Vec<i64>>,
    reasoning: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawReviewVerdict {
    reviewer: i64,
    defender: i64,
    honesty: Option<i64>,
    validity: Option<i64>,
    notes: Option<String>,
}

fn parse_judgment_with_reviewers(
    text: &str,
    survivors: &[FighterId],
    allowed_reviewers: &[FighterId],
) -> Option<Verdict> {
    let value = extract_json_object(text)?;
    let raw: RawJudgment = serde_json::from_value(value).ok()?;
    let valid_survivors: HashSet<FighterId> = survivors.iter().copied().collect();
    let valid_reviewers: HashSet<FighterId> = allowed_reviewers.iter().copied().collect();
    let to_survivor_id = |v: i64| -> Option<FighterId> {
        let id = usize::try_from(v).ok()?;
        valid_survivors.contains(&id).then_some(id)
    };
    let to_reviewer_id = |v: i64| -> Option<FighterId> {
        let id = usize::try_from(v).ok()?;
        valid_reviewers.contains(&id).then_some(id)
    };

    let mut ranking: Vec<FighterId> = Vec::new();
    for v in raw.ranking.unwrap_or_default() {
        let id = to_survivor_id(v)?;
        if !ranking.contains(&id) {
            ranking.push(id);
        }
    }
    for id in survivors {
        if !ranking.contains(id) {
            ranking.push(*id);
        }
    }

    let clear_winner = match raw.clear_winner {
        Some(v) => Some(to_survivor_id(v)?),
        None => None,
    };
    let finalists = match raw.finalists {
        Some(pair) if pair.len() == 2 => {
            let a = to_survivor_id(pair[0])?;
            let b = to_survivor_id(pair[1])?;
            if a == b {
                return None;
            }
            Some((a, b))
        }
        Some(_) => return None,
        None => None,
    };
    // Exactly one of winner/finalists; fall back to the ranking when Thor
    // supplied neither, and prefer the winner when he supplied both.
    let (clear_winner, finalists) = match (clear_winner, finalists) {
        (Some(w), _) => (Some(w), None),
        (None, Some(pair)) => (None, Some(pair)),
        (None, None) => match ranking.len() {
            0 => return None,
            1 => (Some(ranking[0]), None),
            _ => (None, Some((ranking[0], ranking[1]))),
        },
    };

    let review_verdicts = raw
        .review_verdicts
        .unwrap_or_default()
        .into_iter()
        .filter_map(|rv| {
            Some(ReviewVerdict {
                reviewer: to_reviewer_id(rv.reviewer)?,
                defender: to_survivor_id(rv.defender)?,
                honesty: rv.honesty.unwrap_or(0).clamp(0, 10) as u8,
                validity: rv.validity.unwrap_or(0).clamp(0, 10) as u8,
                notes: rv.notes.unwrap_or_default(),
            })
        })
        .collect();

    Some(Verdict {
        clear_winner,
        finalists,
        ranking,
        review_verdicts,
        reasoning: raw
            .reasoning
            .unwrap_or_else(|| "Thor offers no reasoning.".to_string()),
        thor_fallback: false,
    })
}

// ---------------------------------------------------------------------------
// Text utilities + battle cries
// ---------------------------------------------------------------------------

/// Keep the head and tail of oversized text, excising the middle.
pub fn truncate_middle(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let head_budget = max * 7 / 10;
    let tail_budget = max.saturating_sub(head_budget);
    let head_end = floor_char_boundary(text, head_budget);
    let tail_start = ceil_char_boundary(text, text.len().saturating_sub(tail_budget));
    let excised = tail_start.saturating_sub(head_end);
    format!(
        "{}\n…[{excised} bytes excised]…\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// First line of `text`, hard-capped at `max` chars with an ellipsis.
pub fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let cut: String = line.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// An extremely silly, deterministic battle cry for the combat feed.
pub fn battle_cry(fighter: &str, action: ActionKind, detail: &str, roll: usize) -> String {
    let pool: &[&str] = match action {
        ActionKind::Forge => &[
            "🔨 {f} hammers white-hot code upon the anvil: {d}",
            "🔥 {f} quenches a fresh blade in coolant: {d}",
            "⚒ {f} reforges the very bones of the repo: {d}",
            "🧲 {f} bends molten syntax to their will: {d}",
        ],
        ActionKind::Strike => &[
            "⚡ {f} hurls a thunderbolt of shell: {d}",
            "🌪 {f} unleashes a whirlwind subprocess: {d}",
            "💥 {f} smites the terminal with {d}",
            "🐍 {f} releases a screaming daemon: {d}",
        ],
        ActionKind::Scry => &[
            "🔮 {f} peers into the swirling runes: {d}",
            "🦉 {f} dispatches ravens to spy upon {d}",
            "📜 {f} unrolls a dusty scroll: {d}",
            "👁 {f} gazes unblinking at {d}",
        ],
        ActionKind::Chant => &[
            "🎵 {f} chants an epic saga: “{d}”",
            "📯 {f} bellows across the arena: “{d}”",
            "🗣 {f} monologues heroically: “{d}”",
        ],
        ActionKind::Ponder => &[
            "🤔 {f} strokes a magnificent imaginary beard: {d}",
            "🧠 {f} enters the mind palace: {d}",
            "💭 {f} consults their inner völva: {d}",
        ],
        ActionKind::Wound => &[
            "🩸 {f} takes a nasty error to the knee: {d}",
            "☄ {f} is scorched by a failing rune: {d}",
            "😱 {f} staggers — the tool has betrayed them! {d}",
        ],
        ActionKind::Guard => &[
            "🛡 {f} flashes the seal of permission: {d}",
            "🗝 {f} is waved through the gates: {d}",
        ],
    };
    let template = pool[roll % pool.len()];
    template.replace("{f}", fighter).replace("{d}", detail)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(agent: &str, model: &str, pass_at_1_bps: u32, key: &str) -> Candidate {
        Candidate {
            card: FighterCard {
                id: 0,
                agent_source_id: agent.to_string(),
                model_value: model.to_string(),
                model_name: model.to_string(),
                pass_at_1_bps,
                mean_cost_usd: 0.0,
            },
            launch: Launch {
                program: PathBuf::from("true"),
                args: vec![],
                env: HashMap::new(),
            },
            vendor: Some(council_provider(model)),
            match_key: key.to_string(),
        }
    }

    fn sorted(mut pool: Vec<Candidate>) -> Vec<Candidate> {
        pool.sort_by_key(|c| std::cmp::Reverse(c.card.pass_at_1_bps));
        pool
    }

    fn init_git_repo_with_commit(path: &Path) {
        let status = std::process::Command::new("git")
            .arg("init")
            .arg(path)
            .status()
            .expect("git init should run");
        assert!(status.success(), "git init failed");
        std::fs::write(path.join("file.txt"), "hello").expect("write file");
        let status = std::process::Command::new("git")
            .current_dir(path)
            .args(["add", "."])
            .status()
            .expect("git add should run");
        assert!(status.success(), "git add failed");
        let status = std::process::Command::new("git")
            .current_dir(path)
            .args([
                "-c",
                "user.name=Mjolnir Test",
                "-c",
                "user.email=mjolnir@example.invalid",
                "commit",
                "-am",
                "initial",
            ])
            .status()
            .expect("git commit should run");
        assert!(status.success(), "git commit failed");
    }

    #[tokio::test]
    async fn sweep_camps_removes_every_registered_worktree_but_not_unregistered_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo_with_commit(dir.path());

        let thor_camp =
            worktree::create_for_automation(dir.path(), "ragnarok-thor").expect("thor camp");
        let fighter_camp =
            worktree::create_for_automation(dir.path(), "ragnarok-fighter").expect("fighter camp");
        let kept_camp =
            worktree::create_for_automation(dir.path(), "ragnarok-kept").expect("kept camp");
        assert!(thor_camp.worktree_root.is_dir());
        assert!(fighter_camp.worktree_root.is_dir());
        assert!(kept_camp.worktree_root.is_dir());

        let registry: WorktreeRegistry = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        register_camp(&registry, &thor_camp);
        register_camp(&registry, &fighter_camp);
        // `kept_camp` is never registered, standing in for a worktree left
        // behind by a successful battle that `sweep_camps` must not touch.

        sweep_camps(registry).await;

        assert!(
            !thor_camp.worktree_root.exists(),
            "Thor's camp should be swept away on battle failure"
        );
        assert!(
            !fighter_camp.worktree_root.exists(),
            "a fighter's camp should be swept away on battle failure"
        );
        assert!(
            kept_camp.worktree_root.is_dir(),
            "camps outside the failed battle's registry must survive"
        );
    }

    #[test]
    fn select_does_not_reward_adapter_diversity() {
        let pool = sorted(vec![
            candidate("agent-a", "alpha", 1500, "openai/alpha"),
            candidate("agent-a", "beta", 1490, "anthropic/beta"),
            candidate("agent-b", "gamma", 1484, "openai/gamma"),
        ]);
        let picked = select_fighters_with_picker(&pool, 2, |_| 0);
        let agents: Vec<&str> = picked
            .iter()
            .map(|c| c.card.agent_source_id.as_str())
            .collect();
        assert_eq!(picked.len(), 2);
        // Adapter identity is not part of the score, so beta's raw Pass@1 edge
        // beats gamma.
        assert_eq!(agents, vec!["agent-a", "agent-a"]);
        assert_eq!(picked[1].card.model_name, "beta");
    }

    #[test]
    fn select_fills_from_same_agent_when_needed() {
        let pool = sorted(vec![
            candidate("claude-acp", "opus", 1456, "anthropic/opus48"),
            candidate("claude-acp", "sonnet", 1457, "anthropic/sonnet46"),
            candidate("codex-acp", "gpt-5.5", 1463, "openai/gpt55"),
        ]);
        let picked = select_fighters_with_picker(&pool, 3, |_| 0);
        assert_eq!(picked.len(), 3);
        // Distinct models even when agents repeat.
        let keys: HashSet<&str> = picked.iter().map(|c| c.match_key.as_str()).collect();
        assert_eq!(keys.len(), 3);
    }

    #[test]
    fn select_rewards_new_provider_only() {
        let pool = sorted(vec![
            candidate("agent-a", "alpha", 1500, "openai/alpha"),
            candidate("agent-a", "beta", 1490, "anthropic/beta"),
            candidate("agent-b", "gamma", 1490, "openai/gamma"),
            candidate("agent-c", "delta", 1445, "google/delta"),
        ]);
        let picked = select_fighters_with_picker(&pool, 2, |_| 0);

        assert_eq!(picked[0].card.model_name, "alpha");
        // beta is the strongest model from a provider not represented by alpha.
        // Adapter identity no longer affects selection.
        assert_eq!(picked[1].card.model_name, "beta");
    }

    #[test]
    fn select_does_not_apply_adapter_specific_penalties() {
        let pool = sorted(vec![
            candidate(
                "anvil",
                "bedrock::us.anthropic.claude-opus-4-8",
                1500,
                "anthropic/opus48",
            ),
            candidate("claude-acp", "opus", 1425, "anthropic/opus47"),
        ]);
        let picked = select_fighters_with_picker(&pool, 1, |_| 0);

        assert_eq!(picked[0].card.agent_source_id, "anvil");
        assert_eq!(
            picked[0].card.model_name,
            "bedrock::us.anthropic.claude-opus-4-8"
        );
    }

    #[test]
    fn select_does_not_award_diversity_before_first_pick() {
        let pool = sorted(vec![
            candidate("agent-a", "alpha", 1500, "openai/alpha"),
            candidate("agent-b", "beta", 1499, "anthropic/beta"),
            candidate("agent-c", "gamma", 1498, "google/gamma"),
        ]);
        let picked = select_fighters_with_picker(&pool, 1, |upper| {
            assert_eq!(upper, pool.len());
            0
        });

        assert_eq!(picked[0].card.model_name, "alpha");
        assert_eq!(adjusted_selection_score(&pool[0], &HashSet::new()), 1500);
    }

    #[test]
    fn select_can_pick_lower_ranked_candidate_inside_top_window() {
        let pool = sorted(vec![
            candidate("agent-a", "alpha", 1500, "openai/alpha"),
            candidate("agent-b", "beta", 1499, "anthropic/beta"),
            candidate("agent-c", "gamma", 1498, "google/gamma"),
            candidate("agent-d", "delta", 1497, "deepseek/delta"),
            candidate("agent-e", "epsilon", 1200, "xai/epsilon"),
        ]);
        let picked = select_fighters_with_picker(&pool, 1, |upper| {
            assert_eq!(upper, SELECTION_RANDOM_TOP_N);
            upper - 1
        });

        assert_eq!(picked[0].card.model_name, "delta");
    }

    #[test]
    fn select_never_duplicates_the_same_underlying_model() {
        // opus via two different agents shares a match key: only one may fight.
        let pool = sorted(vec![
            candidate("claude-acp", "opus", 1456, "anthropic/opus48"),
            candidate(
                "anvil",
                "bedrock::us.anthropic.claude-opus-4-8",
                1456,
                "anthropic/opus48",
            ),
            candidate("codex-acp", "gpt-5.5", 1463, "openai/gpt55"),
        ]);
        let picked = select_fighters_with_picker(&pool, 3, |_| 0);
        assert_eq!(picked.len(), 2, "duplicate model must be excluded");
    }

    #[test]
    fn judge_only_reviewer_uses_provider_bonus_and_skips_roster_models() {
        let mut survivor = candidate("agent-a", "gpt-alpha", 1500, "openai/alpha");
        survivor.card.id = 0;
        let mut original_rival = candidate("agent-c", "gemini-delta", 1700, "google/delta");
        original_rival.card.id = 1;
        let pool = sorted(vec![
            survivor.clone(),
            original_rival.clone(),
            candidate("agent-a", "gpt-beta", 1490, "openai/beta"),
            candidate("agent-b", "claude-gamma", 1400, "anthropic/gamma"),
        ]);

        let picked =
            select_judge_only_reviewer_with_picker(&pool, &[survivor, original_rival], 0, |_| 0)
                .expect("judge-only reviewer");

        assert_eq!(picked.card.model_name, "claude-gamma");
        assert_eq!(picked.card.agent_source_id, "agent-b");
    }

    #[test]
    fn rotation_is_a_derangement_for_all_sizes() {
        for n in 2..=10 {
            let ids: Vec<FighterId> = (0..n).collect();
            let assignments = assignments_rotation(&ids);
            assert_eq!(assignments.len(), n);
            let reviewers: HashSet<_> = assignments.iter().map(|a| a.reviewer).collect();
            let defenders: HashSet<_> = assignments.iter().map(|a| a.defender).collect();
            assert_eq!(reviewers.len(), n);
            assert_eq!(defenders.len(), n);
            assert!(assignments.iter().all(|a| a.reviewer != a.defender));
        }
    }

    #[test]
    fn extract_json_handles_fences_prose_and_nested_braces() {
        let text = "Very well!\n```json\n{\"a\":{\"b\":\"}{\"},\"n\":2}\n```\ndone";
        let v = extract_json_object(text).expect("json");
        assert_eq!(v["n"], 2);
        assert_eq!(v["a"]["b"], "}{");
    }

    #[test]
    fn extract_json_skips_unparseable_candidates() {
        let text = "{not json} but later {\"ok\":true}";
        let v = extract_json_object(text).expect("json");
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn parse_route_clamps_and_validates() {
        let route =
            parse_route("{\"complexity\":\"epic\",\"competitors\":25,\"rationale\":\"big\"}")
                .expect("route");
        assert_eq!(route.competitors, MAX_FIGHTERS);
        let route = parse_route("{\"competitors\":2}").expect("route");
        assert_eq!(route.competitors, 2);
        assert!(parse_route("{\"competitors\":0}").is_none());
        assert!(parse_route("no json here").is_none());
    }

    #[test]
    fn field_size_honors_thor_bounds_and_user_cap() {
        // Thor's decree passes through when the cap allows it.
        assert_eq!(field_size(4, 10), 4);
        // Thor is bounded to 2-10 regardless.
        assert_eq!(field_size(25, 10), MAX_FIGHTERS);
        assert_eq!(field_size(0, 10), MIN_FIGHTERS);
        // The user's cap bites.
        assert_eq!(field_size(8, 3), 3);
        // Degenerate caps are clamped into the legal range.
        assert_eq!(field_size(8, 0), MIN_FIGHTERS);
        assert_eq!(field_size(8, 99), 8);
    }

    #[test]
    fn field_size_for_route_tempers_easy_tasks() {
        let simple = RouteDecision {
            complexity: "simple".to_string(),
            competitors: 8,
            rationale: String::new(),
        };
        assert_eq!(field_size_for_route(&simple, 10), 2);

        let moderate = RouteDecision {
            complexity: "moderate".to_string(),
            competitors: 8,
            rationale: String::new(),
        };
        assert_eq!(field_size_for_route(&moderate, 10), 3);

        let complex = RouteDecision {
            complexity: "complex".to_string(),
            competitors: 8,
            rationale: String::new(),
        };
        assert_eq!(field_size_for_route(&complex, 10), 5);

        let epic = RouteDecision {
            complexity: "epic".to_string(),
            competitors: 8,
            rationale: String::new(),
        };
        assert_eq!(field_size_for_route(&epic, 3), 3);
    }

    #[test]
    fn route_by_runes_stays_in_bounds() {
        for task in [
            "fix typo",
            &"x".repeat(5000),
            "refactor the ui and migrate tests",
        ] {
            let route = route_by_runes(task);
            assert!((MIN_FIGHTERS..=MAX_FIGHTERS).contains(&route.competitors));
        }
    }

    #[test]
    fn route_by_runes_keeps_small_ui_polish_small() {
        let route = route_by_runes("adjust the ui spacing and add a small thor animation");
        assert_eq!(route.competitors, 2);
        assert_eq!(route.complexity, "simple");
    }

    #[test]
    fn parse_assignments_accepts_only_perfect_derangements() {
        let survivors = vec![0, 1, 2];
        let good = "{\"assignments\":[{\"reviewer\":0,\"defender\":2},{\"reviewer\":2,\"defender\":1},{\"reviewer\":1,\"defender\":0}]}";
        let parsed = parse_assignments(good, &survivors).expect("valid");
        assert_eq!(parsed.len(), 3);

        // Self-review rejected.
        let selfish = "{\"assignments\":[{\"reviewer\":0,\"defender\":0},{\"reviewer\":1,\"defender\":2},{\"reviewer\":2,\"defender\":1}]}";
        assert!(parse_assignments(selfish, &survivors).is_none());
        // Missing coverage rejected.
        let partial = "{\"assignments\":[{\"reviewer\":0,\"defender\":1}]}";
        assert!(parse_assignments(partial, &survivors).is_none());
        // Unknown ids rejected.
        let unknown = "{\"assignments\":[{\"reviewer\":0,\"defender\":7},{\"reviewer\":7,\"defender\":1},{\"reviewer\":1,\"defender\":0}]}";
        assert!(parse_assignments(unknown, &survivors).is_none());
    }

    #[test]
    fn parse_judgment_normalizes_winner_vs_finalists() {
        let survivors = vec![0, 1, 2];
        let win =
            "{\"ranking\":[1,0,2],\"clear_winner\":1,\"finalists\":null,\"reasoning\":\"strong\"}";
        let v = parse_judgment_with_reviewers(win, &survivors, &survivors).expect("verdict");
        assert_eq!(v.clear_winner, Some(1));
        assert!(v.finalists.is_none());
        assert_eq!(v.ranking, vec![1, 0, 2]);

        let split =
            "{\"ranking\":[2,0],\"clear_winner\":null,\"finalists\":[2,0],\"reasoning\":\"close\"}";
        let v = parse_judgment_with_reviewers(split, &survivors, &survivors).expect("verdict");
        assert_eq!(v.finalists, Some((2, 0)));
        // Missing survivors are appended to the ranking.
        assert_eq!(v.ranking, vec![2, 0, 1]);

        // Neither winner nor finalists: derive finalists from ranking.
        let neither = "{\"ranking\":[0,2,1],\"reasoning\":\"meh\"}";
        let v = parse_judgment_with_reviewers(neither, &survivors, &survivors).expect("verdict");
        assert_eq!(v.finalists, Some((0, 2)));
        assert!(v.clear_winner.is_none());

        // Bogus ids fail the parse entirely.
        assert!(
            parse_judgment_with_reviewers("{\"clear_winner\":9}", &survivors, &survivors).is_none()
        );
    }

    #[test]
    fn parse_judgment_collects_review_verdicts() {
        let survivors = vec![0, 1];
        let text = "{\"review_verdicts\":[{\"reviewer\":0,\"defender\":1,\"honesty\":9,\"validity\":22,\"notes\":\"fair\"}],\"clear_winner\":0,\"reasoning\":\"ok\"}";
        let v = parse_judgment_with_reviewers(text, &survivors, &survivors).expect("verdict");
        assert_eq!(v.review_verdicts.len(), 1);
        assert_eq!(v.review_verdicts[0].honesty, 9);
        assert_eq!(v.review_verdicts[0].validity, 10, "clamped to 10");
    }

    #[test]
    fn parse_judgment_allows_judge_only_reviewer_without_making_them_a_finalist() {
        let text = "{\"review_verdicts\":[{\"reviewer\":2,\"defender\":0,\"honesty\":8,\"validity\":7,\"notes\":\"sound\"}],\"ranking\":[0],\"clear_winner\":0,\"reasoning\":\"only implementation left\"}";
        let v = parse_judgment_with_reviewers(text, &[0], &[2]).expect("verdict");

        assert_eq!(v.clear_winner, Some(0));
        assert_eq!(v.ranking, vec![0]);
        assert_eq!(v.review_verdicts.len(), 1);
        assert_eq!(v.review_verdicts[0].reviewer, 2);
        assert_eq!(v.review_verdicts[0].defender, 0);
    }

    #[test]
    fn pass_at_1_fallback_presents_two_strongest_finalists() {
        let cards = vec![
            FighterCard {
                id: 0,
                agent_source_id: "a".into(),
                model_value: "m0".into(),
                model_name: "m0".into(),
                pass_at_1_bps: 1400,
                mean_cost_usd: 0.0,
            },
            FighterCard {
                id: 1,
                agent_source_id: "b".into(),
                model_value: "m1".into(),
                model_name: "m1".into(),
                pass_at_1_bps: 1460,
                mean_cost_usd: 0.0,
            },
            FighterCard {
                id: 2,
                agent_source_id: "c".into(),
                model_value: "m2".into(),
                model_name: "m2".into(),
                pass_at_1_bps: 1430,
                mean_cost_usd: 0.0,
            },
        ];
        let v = strength_fallback_verdict(&[0, 1, 2], &cards);
        assert!(v.thor_fallback);
        assert_eq!(v.finalists, Some((1, 2)));
        assert_eq!(v.ranking, vec![1, 2, 0]);
    }

    #[test]
    fn pass_at_1_fallback_crowns_the_only_survivor() {
        let cards = vec![FighterCard {
            id: 0,
            agent_source_id: "a".into(),
            model_value: "m".into(),
            model_name: "Solo".into(),
            pass_at_1_bps: 1400,
            mean_cost_usd: 0.0,
        }];
        let v = strength_fallback_verdict(&[0], &cards);

        assert!(v.thor_fallback);
        assert_eq!(v.clear_winner, Some(0));
        assert!(v.finalists.is_none());
        assert_eq!(v.ranking, vec![0]);
    }

    // ---- Straggler watchdog -----------------------------------------------

    #[test]
    fn straggler_judgment_needs_both_time_gates() {
        let median = Duration::from_secs(120);
        // Over the multiplier but under the absolute floor: no judgment.
        assert!(!should_judge_straggler(Duration::from_secs(200), median));
        // Past the floor but under the multiplier: no judgment.
        assert!(!should_judge_straggler(
            Duration::from_secs(300),
            Duration::from_secs(150)
        ));
        // Past both: judged.
        assert!(should_judge_straggler(Duration::from_secs(400), median));
    }

    #[test]
    fn median_duration_handles_odd_even_and_empty() {
        assert_eq!(median_duration(&[]), None);
        assert_eq!(
            median_duration(&[Duration::from_secs(5)]),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            median_duration(&[
                Duration::from_secs(9),
                Duration::from_secs(1),
                Duration::from_secs(4)
            ]),
            Some(Duration::from_secs(4))
        );
        assert_eq!(
            median_duration(&[Duration::from_secs(2), Duration::from_secs(4)]),
            Some(Duration::from_secs(3))
        );
    }

    #[test]
    fn loop_detector_fires_on_dominant_repeats_only() {
        let stuck: Vec<String> = (0..12)
            .map(|i| {
                if i % 5 == 4 {
                    "run tests".to_string()
                } else {
                    "edit src/foo.rs".to_string()
                }
            })
            .collect();
        assert!(looks_stuck(&stuck));

        let varied: Vec<String> = (0..12).map(|i| format!("edit file-{i}.rs")).collect();
        assert!(!looks_stuck(&varied));

        let short = vec!["edit src/foo.rs".to_string(); 5];
        assert!(!looks_stuck(&short), "too few actions to condemn");
    }

    #[test]
    fn action_tally_ranks_repeats() {
        let recent = vec![
            "edit src/foo.rs".to_string(),
            "edit src/foo.rs".to_string(),
            "run tests".to_string(),
            "edit src/foo.rs".to_string(),
        ];
        let tally = action_tally(&recent);
        assert!(tally.starts_with("edit src/foo.rs ×3"), "tally: {tally}");
        assert!(tally.contains("run tests ×1"));
        assert!(tally.contains("of the last 4"));
        assert_eq!(action_tally(&[]), "(no recorded actions)");
    }

    #[test]
    fn parse_mercy_accepts_only_known_verdicts() {
        assert_eq!(
            parse_mercy("{\"verdict\":\"cut_down\",\"reason\":\"stuck\"}"),
            Some((true, "stuck".to_string()))
        );
        assert_eq!(
            parse_mercy("verily {\"verdict\":\"mercy\"} so be it"),
            Some((false, "Thor keeps his counsel.".to_string()))
        );
        assert!(parse_mercy("{\"verdict\":\"maybe\"}").is_none());
        assert!(parse_mercy("no json").is_none());
    }

    #[test]
    fn kill_switch_sets_reason_once_and_fires() {
        let (trigger, rx) = watch::channel(false);
        let switch = KillSwitch {
            trigger,
            reason: std::sync::Arc::new(std::sync::Mutex::new(None)),
        };
        switch.fire("cut down by Thor — looping");
        switch.fire("second reason must not overwrite");
        assert!(*rx.borrow());
        assert_eq!(
            switch.reason.lock().unwrap().as_deref(),
            Some("cut down by Thor — looping")
        );
    }

    #[test]
    fn straggler_watch_tracks_quorum_and_recent_actions() {
        let mut watch = StragglerWatch::new(4);
        assert!(!watch.quorum_reached());
        watch.note_finished(Duration::from_secs(60));
        assert!(!watch.quorum_reached());
        watch.note_finished(Duration::from_secs(90));
        assert!(watch.quorum_reached(), "2 of 4 is a quorum");

        for i in 0..(RECENT_ACTIONS_CAP + 10) {
            watch.note_action(1, format!("edit {i}"));
        }
        assert_eq!(watch.recent_actions(1).len(), RECENT_ACTIONS_CAP);
        assert!(watch.recent_actions(2).is_empty());
    }

    #[test]
    fn truncate_middle_keeps_head_and_tail() {
        let text = format!("{}MIDDLE{}", "H".repeat(100), "T".repeat(100));
        let out = truncate_middle(&text, 60);
        assert!(out.starts_with("HHHH"));
        assert!(out.ends_with("TTTT"));
        assert!(out.contains("excised"));
        assert!(truncate_middle("short", 60) == "short");
    }

    #[test]
    fn truncate_middle_respects_char_boundaries() {
        let text = "⚔".repeat(100);
        let out = truncate_middle(&text, 50);
        assert!(out.contains("excised"));
        // Must not panic and must remain valid UTF-8 (guaranteed by String).
        assert!(out.chars().count() > 0);
    }

    #[test]
    fn captured_artifact_empty_tracks_diff_body() {
        let empty = CapturedArtifact {
            diffstat: "1 file changed".to_string(),
            diff: "\n".to_string(),
            truncated: false,
        };
        assert!(empty.is_empty());

        let changed = CapturedArtifact {
            diffstat: String::new(),
            diff: "diff --git a/src/lib.rs b/src/lib.rs\n".to_string(),
            truncated: false,
        };
        assert!(!changed.is_empty());
    }

    #[test]
    fn empty_diff_continue_prompt_orders_same_worktree_changes() {
        let prompt = empty_diff_continue_prompt("fix empty output");
        assert!(prompt.contains("THOR INSPECTION"));
        assert!(prompt.contains("git diff"));
        assert!(prompt.contains("Continue in this same worktree"));
        assert!(prompt.contains("Do NOT create git commits"));
        assert!(prompt.contains("fix empty output"));
        assert!(!prompt.contains("plan"));
    }

    #[test]
    fn prompt_marker_shows_outbound_prompt_without_full_wrapper() {
        let prompt = fight_prompt("change the thing");
        let marker = prompt_marker("combat", &prompt);
        assert!(marker.contains("mj sent combat prompt"));
        assert!(marker.contains("TASK:\nchange the thing"));
        assert!(
            !marker.contains("RAGNAROK. You are one of several"),
            "marker should not dump the full wrapper: {marker}"
        );
    }

    #[test]
    fn append_turn_text_preserves_continuation_summary() {
        let mut text = String::new();
        append_turn_text(&mut text, "first summary");
        append_turn_text(&mut text, "second summary");
        append_turn_text(&mut text, "  ");

        assert!(text.contains("first summary"));
        assert!(text.contains("--- continuation turn ---"));
        assert!(text.ends_with("second summary"));
    }

    #[tokio::test]
    async fn capped_output_keeps_head_and_tail() {
        use tokio::io::AsyncWriteExt;

        let input = format!("{}MIDDLE{}", "H".repeat(100), "T".repeat(100));
        let (mut writer, reader) = tokio::io::duplex(256);
        tokio::spawn(async move {
            writer.write_all(input.as_bytes()).await.expect("write");
        });

        let out = read_capped_output(reader, 60).await.expect("read");
        assert!(out.truncated, "output should be marked truncated");
        assert!(out.text.starts_with("HHHH"), "out: {}", out.text);
        assert!(out.text.ends_with("TTTT"), "out: {}", out.text);
        assert!(out.text.contains("bytes excised"), "out: {}", out.text);
    }

    #[test]
    fn first_line_truncates_politely() {
        assert_eq!(first_line("hello\nworld", 60), "hello");
        assert_eq!(first_line("", 60), "");
        let long = "x".repeat(100);
        assert_eq!(first_line(&long, 10).chars().count(), 10);
    }

    #[test]
    fn battle_cries_are_deterministic_and_themed() {
        let a = battle_cry("Opus", ActionKind::Forge, "src/main.rs", 3);
        let b = battle_cry("Opus", ActionKind::Forge, "src/main.rs", 3);
        assert_eq!(a, b);
        assert!(a.contains("Opus"));
        assert!(a.contains("src/main.rs"));
        // All action kinds produce something for many rolls.
        for kind in [
            ActionKind::Forge,
            ActionKind::Strike,
            ActionKind::Scry,
            ActionKind::Chant,
            ActionKind::Ponder,
            ActionKind::Wound,
            ActionKind::Guard,
        ] {
            for roll in 0..8 {
                assert!(!battle_cry("X", kind, "y", roll).is_empty());
            }
        }
    }

    // ---- AgentHandle: model arming + prompt turn contract -----------------

    use agent_client_protocol::schema::v1::{
        SessionConfigOptionCategory, SessionConfigSelectOption,
    };

    /// A handle wired to test-owned channels, plus the guards that keep it
    /// alive (dropping the abort sender reads as "UI gone" = abort).
    struct TestRig {
        handle: AgentHandle,
        event_tx: mpsc::UnboundedSender<UiEvent>,
        cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
        _abort_tx: watch::Sender<bool>,
    }

    fn test_rig() -> TestRig {
        test_rig_with_access(acp::RuntimeAccessMode::Full)
    }

    fn test_rig_with_access(access_mode: acp::RuntimeAccessMode) -> TestRig {
        let (event_tx, events) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (abort_tx, abort) = watch::channel(false);
        let handle = AgentHandle {
            cmd_tx,
            events,
            runtime: tokio::spawn(async { Ok(()) }),
            config_options: Vec::new(),
            config_targets: Vec::new(),
            abort,
            access_mode,
        };
        TestRig {
            handle,
            event_tx,
            cmd_rx,
            _abort_tx: abort_tx,
        }
    }

    fn model_options(current: &str) -> (Vec<SessionConfigOption>, Vec<SessionConfigTarget>) {
        let option = SessionConfigOption::select(
            "model",
            "Model",
            current.to_string(),
            vec![
                SessionConfigSelectOption::new("opus", "Opus"),
                SessionConfigSelectOption::new("sonnet", "Sonnet"),
            ],
        )
        .category(Some(SessionConfigOptionCategory::Model));
        let target = SessionConfigTarget::ConfigOption {
            config_id: option.id.clone(),
        };
        (vec![option], vec![target])
    }

    #[tokio::test]
    async fn arm_model_skips_round_trip_when_already_current() {
        let mut rig = test_rig();
        let (options, targets) = model_options("opus");
        rig.handle.store_config(options, targets);
        rig.handle.arm_model("opus").await.expect("already armed");
        assert!(
            rig.cmd_rx.try_recv().is_err(),
            "no command should be sent for an already-current model"
        );
    }

    #[tokio::test]
    async fn arm_model_waits_for_the_runtime_confirmation() {
        let mut rig = test_rig();
        let (options, targets) = model_options("sonnet");
        rig.handle.store_config(options, targets);

        // Buffer an interim (unchanged) table and then the real confirmation.
        let (stale_options, stale_targets) = model_options("sonnet");
        rig.event_tx
            .send(UiEvent::SessionConfigOptions {
                options: stale_options,
                targets: stale_targets,
            })
            .unwrap();
        let (confirmed_options, confirmed_targets) = model_options("opus");
        rig.event_tx
            .send(UiEvent::SessionConfigOptions {
                options: confirmed_options,
                targets: confirmed_targets,
            })
            .unwrap();

        rig.handle.arm_model("opus").await.expect("armed");
        assert!(
            matches!(
                rig.cmd_rx.try_recv(),
                Ok(UiCommand::SetSessionConfigOption { value, .. }) if value.to_string() == "opus"
            ),
            "the set command must have been sent"
        );
        assert!(rig.handle.model_is_current("opus"));
    }

    #[tokio::test]
    async fn arm_model_surfaces_config_update_failure() {
        let mut rig = test_rig();
        let (options, targets) = model_options("sonnet");
        rig.handle.store_config(options, targets);
        rig.event_tx
            .send(UiEvent::Warning(
                "session config update failed: no such model".to_string(),
            ))
            .unwrap();
        let err = rig
            .handle
            .arm_model("opus")
            .await
            .expect_err("failure must surface");
        assert!(format!("{err:#}").contains("refused"), "err: {err:#}");
    }

    #[tokio::test]
    async fn prompt_resends_after_config_in_flight_rejection() {
        // The exact live failure: the first prompt lands while the runtime
        // still has a config update in flight and gets rejected.
        let mut rig = test_rig();
        rig.event_tx
            .send(UiEvent::PromptFailed {
                message: "prompt failed: config update already in flight".to_string(),
            })
            .unwrap();
        rig.event_tx
            .send(UiEvent::PromptDone {
                stop_reason: StopReason::EndTurn,
                usage: None,
            })
            .unwrap();

        let outcome = rig
            .handle
            .prompt("go".to_string(), Duration::from_secs(5), |_| {})
            .await
            .expect("prompt survives the rejection");
        assert_eq!(outcome.stop, StopReason::EndTurn);

        let mut sends = 0;
        while let Ok(cmd) = rig.cmd_rx.try_recv() {
            if matches!(cmd, UiCommand::SendPrompt { .. }) {
                sends += 1;
            }
        }
        assert_eq!(sends, 2, "the rejected prompt must be re-sent once");
    }

    #[tokio::test]
    async fn prompt_resends_after_prompt_in_flight_warning() {
        let mut rig = test_rig();
        rig.event_tx
            .send(UiEvent::Warning("prompt already in flight".to_string()))
            .unwrap();
        rig.event_tx
            .send(UiEvent::PromptDone {
                stop_reason: StopReason::EndTurn,
                usage: None,
            })
            .unwrap();

        let mut notes = Vec::new();
        let outcome = rig
            .handle
            .prompt("go".to_string(), Duration::from_secs(5), |ev| {
                if let TurnEvent::Note(note) = ev {
                    notes.push(note);
                }
            })
            .await
            .expect("prompt survives transient in-flight warning");
        assert_eq!(outcome.stop, StopReason::EndTurn);
        assert!(
            notes.iter().any(|note| note.contains("retrying prompt")),
            "notes: {notes:?}"
        );

        let mut sends = 0;
        while let Ok(cmd) = rig.cmd_rx.try_recv() {
            if matches!(cmd, UiCommand::SendPrompt { .. }) {
                sends += 1;
            }
        }
        assert_eq!(sends, 2, "the bounced prompt must be re-sent once");
    }

    #[tokio::test]
    async fn permissions_prefer_allow_then_reject_never_cancel_when_avoidable() {
        use agent_client_protocol::schema::v1::{
            PermissionOption, PermissionOptionKind, ToolCallUpdate, ToolCallUpdateFields,
        };
        let rig = test_rig();

        // Allow option present: allow wins.
        let (ptx, prx) = tokio::sync::oneshot::channel();
        rig.handle
            .answer_permission(crate::event::PermissionPrompt {
                tool_call: ToolCallUpdate::new("t1", ToolCallUpdateFields::default()),
                options: vec![
                    PermissionOption::new("deny", "Deny", PermissionOptionKind::RejectOnce),
                    PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
                ],
                responder: ptx,
            });
        assert!(matches!(prx.await, Ok(PermissionDecision::Selected(id)) if id == "allow"));

        // No allow options: an explicit rejection keeps the turn alive where
        // a cancel would cancel the whole turn (the live Thor failure).
        let (ptx, prx) = tokio::sync::oneshot::channel();
        rig.handle
            .answer_permission(crate::event::PermissionPrompt {
                tool_call: ToolCallUpdate::new("t2", ToolCallUpdateFields::default()),
                options: vec![PermissionOption::new(
                    "deny",
                    "Deny",
                    PermissionOptionKind::RejectOnce,
                )],
                responder: ptx,
            });
        assert!(matches!(prx.await, Ok(PermissionDecision::Selected(id)) if id == "deny"));

        // No options at all: cancel is the only move left.
        let (ptx, prx) = tokio::sync::oneshot::channel();
        rig.handle
            .answer_permission(crate::event::PermissionPrompt {
                tool_call: ToolCallUpdate::new("t3", ToolCallUpdateFields::default()),
                options: Vec::new(),
                responder: ptx,
            });
        assert!(matches!(prx.await, Ok(PermissionDecision::Cancelled)));
    }

    #[tokio::test]
    async fn read_only_review_permissions_reject_mutating_tools() {
        use agent_client_protocol::schema::v1::{
            PermissionOption, PermissionOptionKind, ToolCallUpdate, ToolCallUpdateFields,
        };
        let rig = test_rig_with_access(acp::RuntimeAccessMode::ReadOnly);

        let mut fields = ToolCallUpdateFields::default();
        fields.kind = Some(ToolKind::Edit);
        let (ptx, prx) = tokio::sync::oneshot::channel();
        rig.handle
            .answer_permission(crate::event::PermissionPrompt {
                tool_call: ToolCallUpdate::new("edit", fields),
                options: vec![
                    PermissionOption::new("deny", "Deny", PermissionOptionKind::RejectOnce),
                    PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
                ],
                responder: ptx,
            });
        assert!(matches!(prx.await, Ok(PermissionDecision::Selected(id)) if id == "deny"));

        let mut fields = ToolCallUpdateFields::default();
        fields.kind = Some(ToolKind::Read);
        let (ptx, prx) = tokio::sync::oneshot::channel();
        rig.handle
            .answer_permission(crate::event::PermissionPrompt {
                tool_call: ToolCallUpdate::new("read", fields),
                options: vec![
                    PermissionOption::new("deny", "Deny", PermissionOptionKind::RejectOnce),
                    PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
                ],
                responder: ptx,
            });
        assert!(matches!(prx.await, Ok(PermissionDecision::Selected(id)) if id == "allow"));
    }

    #[tokio::test]
    async fn forward_turn_event_respects_read_only_permission_policy() {
        use agent_client_protocol::schema::v1::{
            PermissionOption, PermissionOptionKind, ToolCallUpdate, ToolCallUpdateFields,
        };

        let (tx, _rx) = mpsc::unbounded_channel();
        let mut fields = ToolCallUpdateFields::default();
        fields.kind = Some(ToolKind::Edit);
        let (ptx, prx) = tokio::sync::oneshot::channel();
        let prompt = crate::event::PermissionPrompt {
            tool_call: ToolCallUpdate::new("edit", fields),
            options: vec![
                PermissionOption::new("deny", "Deny", PermissionOptionKind::RejectOnce),
                PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
            ],
            responder: ptx,
        };

        let mut cry_roll = 0;
        let mut chunk_count = 0;
        forward_turn_event(
            &tx,
            1,
            "reviewer",
            TurnEvent::Permission {
                prompt: Box::new(prompt),
                access_mode: acp::RuntimeAccessMode::ReadOnly,
            },
            TextLane::Tool,
            &mut cry_roll,
            &mut chunk_count,
        );

        assert!(matches!(prx.await, Ok(PermissionDecision::Selected(id)) if id == "deny"));
    }

    #[tokio::test]
    async fn prompt_still_fails_on_other_rejections() {
        let mut rig = test_rig();
        rig.event_tx
            .send(UiEvent::PromptFailed {
                message: "agent exploded".to_string(),
            })
            .unwrap();
        let err = rig
            .handle
            .prompt("go".to_string(), Duration::from_secs(5), |_| {})
            .await
            .expect_err("unrelated failures still fail");
        assert!(format!("{err:#}").contains("agent exploded"));
    }

    #[test]
    fn fighter_tag_includes_pass_at_1_cost_and_agent() {
        let card = FighterCard {
            id: 0,
            agent_source_id: "claude-acp".into(),
            model_value: "opus".into(),
            model_name: "Opus".into(),
            pass_at_1_bps: 1456,
            mean_cost_usd: 4.28,
        };
        assert_eq!(card.tag(), "Opus [claude-acp] ⚡14.6% · $4.28");
    }
}
