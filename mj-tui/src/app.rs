//! UI state machine.
//!
//! Holds the transcript, current tool-call table, input buffer, and FIFO
//! queues for pending user prompts and permission prompts. Every incoming
//! ACP event is folded in through `apply_event`; ratatui then renders from
//! this state.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::claude_usage::ClaudeUsageStatus;
use crate::clipboard::ClipboardLease;
use crate::codex_usage::CodexUsageStatus;
use agent_client_protocol::schema::v1::{
    AvailableCommand, Diff, ElicitationContentValue, ElicitationMode, ElicitationPropertySchema,
    EnumOption, MultiSelectItems, Plan, PlanEntry, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelect, SessionConfigSelectOptions,
    SessionConfigValueId, SessionUpdate, StopReason, TerminalExitStatus, ToolCall, ToolCallContent,
    ToolCallStatus, ToolCallUpdate, ToolKind, Usage, UsageUpdate,
};

use crate::event::{
    ElicitationOutcome, ElicitationPrompt, InternalMessage, PermissionDecision, PermissionPrompt,
    PromptImage, PromptResource, ReviewTarget, SessionConfigTarget, SubagentEvent, SubagentOutcome,
    SubagentStatusKind, TerminalOutputSnapshot, UiEvent, content_block_text,
};
use crate::palette::TerminalTheme;
use crate::palette::TerminalThemeKindExt;
use crate::ragnarok;
use crate::session_state::SessionState;
use crate::settings::{SettingsAction, SettingsEditor};
use crate::spinner::SpinnerStyle;
use crate::theme::TerminalThemeKind;

/// Maximum width of the queued-prompt preview shown above the input.
/// Beyond this we truncate with an ellipsis.
pub const QUEUED_PROMPT_PREVIEW_WIDTH: usize = 40;

/// Maximum width of the provisional session title seeded from the first
/// user prompt while waiting for the agent's `SessionInfoUpdate`.
const PROVISIONAL_TITLE_WIDTH: u16 = 48;

/// Longest excerpt of an objective or failure message kept in a subagent's
/// permanent transcript record.
const SUBAGENT_RECORD_LINE_CHARS: usize = 160;
const NESTED_AGENT_VIEWER_LIMIT: usize = 10;
/// Completed nested histories larger than this are offloaded even when the
/// session as a whole is still below its budget.
const NESTED_ACTOR_RESIDENT_BUDGET: usize = 2 * 1024 * 1024;
/// Soft cap for completed nested transcript/tool/terminal state kept in RAM.
/// Protected actors may temporarily take the resident total above this cap.
const NESTED_SESSION_RESIDENT_BUDGET: usize = 8 * 1024 * 1024;
static NESTED_HISTORY_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct NestedHistoryDir(Option<PathBuf>);

impl NestedHistoryDir {
    fn path(&self) -> &PathBuf {
        self.0.as_ref().expect("nested history directory is live")
    }

    fn remove(&mut self) -> std::io::Result<()> {
        let Some(path) = self.0.take() else {
            return Ok(());
        };
        std::fs::remove_dir_all(path)
    }
}

impl Drop for NestedHistoryDir {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            tracing::warn!(%error, "failed to remove nested-agent history directory");
        }
    }
}

/// Durable UI state for one nested ACP actor. The on-demand viewer reads its
/// lifecycle and `transcript`; completed actors stay here for the whole
/// primary session.
#[derive(Debug, Clone)]
pub struct SubagentStatus {
    pub label: String,
    label_is_placeholder: bool,
    pub model: Option<String>,
    pub adapter: String,
    pub objective: String,
    pub role: Option<crate::workflow::WorkflowActorRole>,
    pub lifecycle: Option<crate::workflow::WorkflowActorLifecycle>,
    pub session_id: Option<String>,
    pub transcript: Vec<Entry>,
    /// Full Markdown history after a completed actor is evicted from RAM.
    /// The path belongs to this primary session and is removed on session
    /// replacement or when the owning AppState is dropped.
    archived_history: Vec<PathBuf>,
    open_message_index: Option<usize>,
    plan_index: Option<usize>,
    /// Latest distilled one-liner: the objective at spawn, then whatever the
    /// subagent is doing now.
    pub activity: String,
    /// Stamped UI-side on `Started`, so elapsed time is measured against the
    /// same clock that renders it.
    pub started_at: Instant,
    /// Outcome and the moment it landed, once the subagent is done.
    pub finished: Option<(SubagentOutcome, Instant)>,
}

impl SubagentStatus {
    fn placeholder(role: Option<crate::workflow::WorkflowActorRole>, now: Instant) -> Self {
        let label = role
            .as_ref()
            .map(nested_role_label)
            .unwrap_or_else(|| "subagent".to_string());
        Self {
            label: label.clone(),
            label_is_placeholder: true,
            model: None,
            adapter: String::new(),
            objective: String::new(),
            role,
            lifecycle: Some(crate::workflow::WorkflowActorLifecycle::Running),
            session_id: None,
            transcript: Vec::new(),
            archived_history: Vec::new(),
            open_message_index: None,
            plan_index: None,
            activity: "starting".to_string(),
            started_at: now,
            finished: None,
        }
    }

    /// Wall-clock the row displays: still counting while running, frozen at the
    /// finish for a done row.
    pub fn elapsed_at(&self, now: Instant) -> Duration {
        let end = match self.finished.as_ref() {
            Some((_, finished_at)) => *finished_at,
            None => now,
        };
        end.saturating_duration_since(self.started_at)
    }

    pub fn outcome(&self) -> Option<&SubagentOutcome> {
        self.finished.as_ref().map(|(outcome, _)| outcome)
    }

    fn counts_as_subagent(&self) -> bool {
        self.role
            .as_ref()
            .is_none_or(|role| !role.is_internal_review_session())
    }

    pub fn archived_history_markdown(&self) -> Option<String> {
        (!self.archived_history.is_empty()).then(|| {
            let mut history = String::new();
            for path in &self.archived_history {
                history.push_str(&std::fs::read_to_string(path).unwrap_or_else(|error| {
                    format!(
                        "_Offloaded nested-agent history could not be read from `{}`: {error}_\n",
                        path.display()
                    )
                }));
                if !history.ends_with('\n') {
                    history.push('\n');
                }
            }
            history
        })
    }

    #[cfg(test)]
    pub(crate) fn archived_history_segments(&self) -> usize {
        self.archived_history.len()
    }
}

pub fn nested_role_label(role: &crate::workflow::WorkflowActorRole) -> String {
    match role {
        crate::workflow::WorkflowActorRole::Implementation => "implementation".to_string(),
        crate::workflow::WorkflowActorRole::IntentAnalyst => "review intent".to_string(),
        crate::workflow::WorkflowActorRole::ReviewSupervisor => "review supervisor".to_string(),
        crate::workflow::WorkflowActorRole::SpecialistReviewer { lane } => {
            format!("reviewer {lane}")
        }
        crate::workflow::WorkflowActorRole::PrimaryCorrection => "primary correction".to_string(),
        crate::workflow::WorkflowActorRole::FallbackReviewer => "fallback reviewer".to_string(),
    }
}

fn nested_actor_reference(role: Option<&crate::workflow::WorkflowActorRole>, id: u64) -> String {
    let label = role.map_or(
        "subagent",
        crate::workflow::WorkflowActorRole::display_label,
    );
    match role {
        Some(crate::workflow::WorkflowActorRole::SpecialistReviewer { lane }) => {
            format!("{label} {lane} #{id}")
        }
        _ => format!("{label} #{id}"),
    }
}

#[derive(Debug, Clone, Copy)]
struct WorkflowClock {
    started_at: Instant,
    finished_at: Option<Instant>,
}

const BUILTIN_NEW_COMMAND: &str = "new";
const BUILTIN_CLEAR_COMMAND: &str = "clear";
const BUILTIN_COMPACT_COMMAND: &str = "compact";
const BUILTIN_LOAD_COMMAND: &str = "load";
const BUILTIN_FORK_COMMAND: &str = "fork";
const BUILTIN_SIDE_COMMAND: &str = "side";
const BUILTIN_EXPORT_COMMAND: &str = "export";
const BUILTIN_DIFF_COMMAND: &str = "diff";
const BUILTIN_MJCONFIG_COMMAND: &str = "mjconfig";
const BUILTIN_AGENTS_COMMAND: &str = "agents";
const BUILTIN_SUBAGENTS_COMMAND: &str = "subagents";
const BUILTIN_REVIEW_COMMAND: &str = "review";
const BUILTIN_RAGNAROK_COMMAND: &str = "ragnarok";
const BUILTIN_TERMINALS_COMMAND: &str = "terminals";
const BUILTIN_MEMORY_COMMAND: &str = "memory";
const CLAUDE_RATE_LIMIT_META_KEY: &str = "_claude/rateLimit";

fn builtin_new_command() -> AvailableCommand {
    AvailableCommand::new(BUILTIN_NEW_COMMAND, "start a new session")
}

fn builtin_clear_command() -> AvailableCommand {
    AvailableCommand::new(
        BUILTIN_CLEAR_COMMAND,
        "start a fresh session with the current agent",
    )
}

fn builtin_compact_command() -> AvailableCommand {
    AvailableCommand::new(
        BUILTIN_COMPACT_COMMAND,
        "compact the primary agent's session where supported",
    )
}

fn builtin_load_command() -> AvailableCommand {
    AvailableCommand::new(BUILTIN_LOAD_COMMAND, "load a previous session")
}

fn builtin_fork_command() -> AvailableCommand {
    AvailableCommand::new(
        BUILTIN_FORK_COMMAND,
        "fork the current session (unstable ACP extension)",
    )
}

fn builtin_side_command() -> AvailableCommand {
    AvailableCommand::new(
        BUILTIN_SIDE_COMMAND,
        "open an isolated ephemeral conversation",
    )
}

fn builtin_export_command() -> AvailableCommand {
    AvailableCommand::new(
        BUILTIN_EXPORT_COMMAND,
        "export primary transcript; add full for nested agents",
    )
}

fn builtin_mjconfig_command() -> AvailableCommand {
    AvailableCommand::new(
        BUILTIN_MJCONFIG_COMMAND,
        "configure the team, agents, ACP servers, and appearance",
    )
}

fn builtin_diff_command() -> AvailableCommand {
    AvailableCommand::new(BUILTIN_DIFF_COMMAND, "show workspace changes against HEAD")
}

fn builtin_agents_command() -> AvailableCommand {
    AvailableCommand::new(
        BUILTIN_AGENTS_COMMAND,
        "show active model selections and usage",
    )
}

fn builtin_subagents_command() -> AvailableCommand {
    AvailableCommand::new(
        BUILTIN_SUBAGENTS_COMMAND,
        "inspect implementation and review agent transcripts",
    )
}

fn builtin_review_command() -> AvailableCommand {
    AvailableCommand::new(
        BUILTIN_REVIEW_COMMAND,
        "review recent, uncommitted, or HEAD changes",
    )
}

fn builtin_ragnarok_command() -> AvailableCommand {
    AvailableCommand::new(
        BUILTIN_RAGNAROK_COMMAND,
        "⚡ rival models battle over a task; Thor judges (usage: /ragnarok <task>)",
    )
}

fn builtin_terminals_command() -> AvailableCommand {
    AvailableCommand::new(
        BUILTIN_TERMINALS_COMMAND,
        "view terminals the agent started, including ones still running",
    )
}

fn builtin_memory_command() -> AvailableCommand {
    AvailableCommand::new(
        BUILTIN_MEMORY_COMMAND,
        "list and manage persistent memories (usage: /memory [add|forget|on|off|use|generate|clear])",
    )
}

fn install_builtin_commands(
    commands: &mut Vec<AvailableCommand>,
    include_fork: bool,
    include_side: bool,
) {
    commands.retain(|command| {
        command.name != BUILTIN_NEW_COMMAND
            && command.name != BUILTIN_CLEAR_COMMAND
            && command.name != BUILTIN_COMPACT_COMMAND
            && command.name != BUILTIN_LOAD_COMMAND
            && command.name != BUILTIN_FORK_COMMAND
            && command.name != BUILTIN_SIDE_COMMAND
            && command.name != BUILTIN_EXPORT_COMMAND
            && command.name != BUILTIN_DIFF_COMMAND
            && command.name != BUILTIN_MJCONFIG_COMMAND
            && command.name != BUILTIN_AGENTS_COMMAND
            && command.name != BUILTIN_SUBAGENTS_COMMAND
            && command.name != BUILTIN_REVIEW_COMMAND
            && command.name != BUILTIN_RAGNAROK_COMMAND
            && command.name != BUILTIN_TERMINALS_COMMAND
            && command.name != BUILTIN_MEMORY_COMMAND
    });
    if include_fork {
        commands.insert(0, builtin_fork_command());
    }
    if include_side {
        commands.insert(0, builtin_side_command());
    }
    commands.insert(0, builtin_ragnarok_command());
    commands.insert(0, builtin_memory_command());
    commands.insert(0, builtin_mjconfig_command());
    commands.insert(0, builtin_diff_command());
    commands.insert(0, builtin_review_command());
    commands.insert(0, builtin_terminals_command());
    commands.insert(0, builtin_subagents_command());
    commands.insert(0, builtin_agents_command());
    commands.insert(0, builtin_export_command());
    commands.insert(0, builtin_load_command());
    commands.insert(0, builtin_compact_command());
    commands.insert(0, builtin_clear_command());
    commands.insert(0, builtin_new_command());
}

fn install_side_builtin_commands(commands: &mut Vec<AvailableCommand>) {
    commands.retain(|command| {
        ![
            BUILTIN_NEW_COMMAND,
            BUILTIN_CLEAR_COMMAND,
            BUILTIN_COMPACT_COMMAND,
            BUILTIN_LOAD_COMMAND,
            BUILTIN_FORK_COMMAND,
            BUILTIN_SIDE_COMMAND,
            BUILTIN_DIFF_COMMAND,
            BUILTIN_MJCONFIG_COMMAND,
            BUILTIN_AGENTS_COMMAND,
            BUILTIN_SUBAGENTS_COMMAND,
            BUILTIN_REVIEW_COMMAND,
            BUILTIN_RAGNAROK_COMMAND,
            BUILTIN_TERMINALS_COMMAND,
            BUILTIN_MEMORY_COMMAND,
        ]
        .contains(&command.name.as_str())
    });
    commands.insert(0, builtin_side_command());
    commands.insert(0, builtin_export_command());
}

/// How the UI loop ends, so `main` can decide whether to quit entirely
/// or start a fresh session from the saved model preferences.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UiExitReason {
    Quit,
    NewSession,
    ClearSession,
    LoadSession,
    SwitchSession,
}

/// One entry in the scrolling transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThoughtEntry {
    pub text: String,
    pub completed: bool,
}

#[derive(Debug, Clone)]
pub enum Entry {
    /// Plain user prompt (echoed locally as soon as it is sent).
    UserPrompt(String),
    /// Streaming agent reply. Mutated in place as chunks arrive.
    AgentMessage(String),
    /// Streaming agent reasoning ("thoughts").
    AgentThought(ThoughtEntry),
    /// Nested agent response and reasoning, kept visually distinct from the primary.
    SubagentMessage(String),
    SubagentThought(ThoughtEntry),
    /// A tool call slot identified by id. The body is rendered from
    /// `tool_calls[id]`; we keep an entry pointer so it shows up in order.
    ToolCall(String),
    SubagentToolCall(String),
    /// Latest plan posted by the agent.
    Plan(Vec<PlanEntry>),
    SubagentPlan(Vec<PlanEntry>),
    /// Orchestration prompt retained in full but normally rendered compactly.
    InternalMessage(InternalMessage),
    /// System-level note (errors, warnings, mode changes).
    System(String),
    /// Local Mjolnir feature-discovery hint. Never sent to the agent.
    FeatureHint(String),
    /// Settled review-issue record: validated findings, pass verdicts, and
    /// the final tally banner. Unlike `System`, each span carries a tone so
    /// fixed/invalidated stay color-coded in scrollback.
    ReviewLedger(Vec<ReviewLedgerLine>),
    /// Visual separator inserted at local session boundaries so a freshly
    /// started session is not confused with the previous transcript.
    SessionBoundary(String),
}

/// One transcript line of a [`Entry::ReviewLedger`] record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewLedgerLine {
    pub spans: Vec<(String, ReviewTone)>,
}

impl ReviewLedgerLine {
    pub fn new(spans: Vec<(String, ReviewTone)>) -> Self {
        Self { spans }
    }

    pub fn plain_text(&self) -> String {
        self.spans
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<String>()
    }
}

/// Semantic tone of a review ledger span; the renderer maps it to the theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewTone {
    /// Record headline ("⚖ review pass 1 · 3 validated issues").
    Header,
    /// A finding still awaiting its fix.
    Open,
    /// A finding the correction turn fixed.
    Fixed,
    /// A finding that did not survive: full error weight, never muted.
    Invalidated,
    /// The summary text of an invalidated finding, struck through.
    Struck,
    /// Supporting evidence such as the invalidation reason.
    Detail,
}

pub const REVIEW_GLYPH: &str = "⚖";

pub(crate) fn review_issue_row(issue: &crate::workflow::ReviewIssue) -> ReviewLedgerLine {
    use crate::workflow::ReviewIssueStatus;

    let label = format!(
        "#{} {}",
        issue.id,
        crate::ragnarok::first_line(&issue.summary, 200)
    );
    match issue.status {
        ReviewIssueStatus::Validated => {
            ReviewLedgerLine::new(vec![(format!("   ● {label}"), ReviewTone::Open)])
        }
        ReviewIssueStatus::Fixed => {
            ReviewLedgerLine::new(vec![(format!("   ✔ {label}"), ReviewTone::Fixed)])
        }
        ReviewIssueStatus::Invalidated => {
            let mut spans = vec![
                ("   ✘ ".to_string(), ReviewTone::Invalidated),
                (label, ReviewTone::Struck),
            ];
            if let Some(reason) = issue.resolution_reason.as_deref() {
                spans.push((format!(" — {reason}"), ReviewTone::Detail));
            }
            ReviewLedgerLine::new(spans)
        }
    }
}

/// Transcript record for a pass's freshly validated findings.
fn review_validated_record(
    pass: u32,
    issues: &[crate::workflow::ReviewIssue],
) -> Vec<ReviewLedgerLine> {
    let of_pass: Vec<_> = issues.iter().filter(|issue| issue.pass == pass).collect();
    if of_pass.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![ReviewLedgerLine::new(vec![(
        format!(
            "{REVIEW_GLYPH} review pass {} · {} validated issue{}",
            pass + 1,
            of_pass.len(),
            if of_pass.len() == 1 { "" } else { "s" }
        ),
        ReviewTone::Header,
    )])];
    lines.extend(of_pass.into_iter().map(review_issue_row));
    lines
}

/// Transcript record for a pass verdict: every listed issue just moved from
/// `Validated` to `status`, with the mechanical evidence spelled out.
fn review_resolved_record(
    pass: u32,
    status: crate::workflow::ReviewIssueStatus,
    reason: Option<&str>,
    issues: &[crate::workflow::ReviewIssue],
) -> Vec<ReviewLedgerLine> {
    use crate::workflow::ReviewIssueStatus;

    let resolved: Vec<_> = issues
        .iter()
        .filter(|issue| issue.pass == pass && issue.status == status)
        .collect();
    if resolved.is_empty() {
        return Vec::new();
    }
    let verdict_tone = match status {
        ReviewIssueStatus::Fixed => ReviewTone::Fixed,
        ReviewIssueStatus::Invalidated => ReviewTone::Invalidated,
        ReviewIssueStatus::Validated => ReviewTone::Open,
    };
    let mut head = vec![
        (
            format!("{REVIEW_GLYPH} review pass {} · ", pass + 1),
            ReviewTone::Header,
        ),
        (
            format!(
                "{} issue{} {}",
                resolved.len(),
                if resolved.len() == 1 { "" } else { "s" },
                status.as_str()
            ),
            verdict_tone,
        ),
    ];
    if let Some(reason) = reason {
        head.push((format!(" — {reason}"), ReviewTone::Detail));
    }
    let mut lines = vec![ReviewLedgerLine::new(head)];
    lines.extend(resolved.into_iter().map(review_issue_row));
    lines
}

/// The banner a finished review fossilizes into: final counts up front, then
/// one row per issue that did not end up plainly fixed.
fn review_verdict_record(
    state: &crate::workflow::WorkflowState,
    outcome: crate::workflow::WorkflowOutcome,
) -> Vec<ReviewLedgerLine> {
    use crate::workflow::{ReviewIssueStatus, WorkflowOutcome};

    let tally = state.issue_tally();
    let head = match outcome {
        WorkflowOutcome::Failed => "review failed",
        WorkflowOutcome::Cancelled => "review cancelled",
        _ => "review complete",
    };
    let mut counts = vec![
        (format!("{REVIEW_GLYPH} {head} · "), ReviewTone::Header),
        (
            format!(
                "{} issue{}",
                tally.found,
                if tally.found == 1 { "" } else { "s" }
            ),
            ReviewTone::Header,
        ),
    ];
    for (count, label, tone) in [
        (tally.fixed, "fixed", ReviewTone::Fixed),
        (tally.invalidated, "invalidated", ReviewTone::Invalidated),
        (tally.open, "unresolved", ReviewTone::Open),
    ] {
        if count > 0 {
            counts.push((" · ".to_string(), ReviewTone::Detail));
            counts.push((format!("{count} {label}"), tone));
        }
    }
    let mut lines = vec![
        ReviewLedgerLine::new(vec![(
            format!("═══ {REVIEW_GLYPH} review verdict {}", "═".repeat(28)),
            ReviewTone::Header,
        )]),
        ReviewLedgerLine::new(counts),
    ];
    // Fixed issues are the happy path and already recorded pass by pass;
    // the banner re-lists only what still needs the user's judgement.
    lines.extend(
        state
            .issues
            .iter()
            .filter(|issue| issue.status != ReviewIssueStatus::Fixed)
            .map(review_issue_row),
    );
    lines.push(ReviewLedgerLine::new(vec![(
        "═".repeat(46),
        ReviewTone::Header,
    )]));
    lines
}

/// Ephemeral search state shared by the fullscreen transcript and the inline
/// full-history reader. Matches are derived from the canonical transcript so
/// streaming updates cannot leave a stale list behind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranscriptSearch {
    pub query: String,
    pub editing: bool,
    pub selected: usize,
    pub jump_pending: bool,
    pub(crate) matches: Vec<usize>,
    pub(crate) matches_revision: Option<u64>,
}

const FEATURE_HINT_INTERVAL_TURNS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureHintCapabilities {
    pub subagents: bool,
    pub ragnarok: bool,
    pub voice: bool,
    pub fork: bool,
    pub side: bool,
    pub images: bool,
    pub fullscreen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeatureHintRequirement {
    Always,
    Subagents,
    Ragnarok,
    Voice,
    Fork,
    Side,
    Images,
    Fullscreen,
    Inline,
}

#[derive(Debug, Clone, Copy)]
struct FeatureHint {
    text: &'static str,
    requirement: FeatureHintRequirement,
}

const FEATURE_HINTS: &[FeatureHint] = &[
    FeatureHint {
        text: "Press Ctrl+Tab to switch coding teams, or open /mjconfig for models and session options.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "Use /new for another workspace, /clear for a fresh thread, /load to resume, and /export to save this transcript.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "Press Ctrl+J for a new line, Up/Down for prompt history, F10 for help, and Ctrl+Y to copy the latest reply.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "Scroll the transcript normally; Ctrl+T expands details and Ctrl+G shows uncommitted workspace changes.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "Choose Default or Full thought output under Appearance in /mjconfig; Full shows every available thought line.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "Choose an activity spinner under Appearance in /mjconfig to change the prompt-working animation.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "If a terminal or multiplexer reports colors unreliably, choose the strict 16-color ANSI theme under Appearance in /mjconfig.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "Mjolnir can queue another prompt while an agent is working; Ctrl+C cancels the active turn first.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "Review permission requests before allowing tools, or change session permission behavior in /mjconfig.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "Launch and monitor specialist subagents from the agent; F8 opens the nested-agent viewer.",
        requirement: FeatureHintRequirement::Subagents,
    },
    FeatureHint {
        text: "Use /ragnarok <task> to compare independent implementations and adopt the strongest result.",
        requirement: FeatureHintRequirement::Ragnarok,
    },
    FeatureHint {
        text: "Press Ctrl+R to dictate a prompt when voice input is available.",
        requirement: FeatureHintRequirement::Voice,
    },
    FeatureHint {
        text: "Run mj --remote to monitor sessions from the web viewer on another device.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "For adapter diagnostics, use --debug-file and --agent-stderr when starting mj.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "Run /review to check recent, uncommitted, or HEAD changes; F9 opens the review issue ledger.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "Use /terminals to view terminals the agent started, including ones still running.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "Use /compact to shrink the primary agent's session context where the agent supports it.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "Run /agents to see active model selections and usage for each seat.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "Use /side to open an isolated ephemeral conversation without disturbing this thread.",
        requirement: FeatureHintRequirement::Side,
    },
    FeatureHint {
        text: "Use /fork to branch the current session and explore an alternative direction.",
        requirement: FeatureHintRequirement::Fork,
    },
    FeatureHint {
        text: "With an empty prompt, Ctrl+F searches the transcript; n and N jump between matches.",
        requirement: FeatureHintRequirement::Fullscreen,
    },
    FeatureHint {
        text: "Press Shift+Tab to choose the primary model and reasoning effort.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "Paste an image from the clipboard with Ctrl+V; it attaches as a chip on your next prompt.",
        requirement: FeatureHintRequirement::Images,
    },
    FeatureHint {
        text: "Ctrl+N starts a new session and Ctrl+O opens the session picker.",
        requirement: FeatureHintRequirement::Always,
    },
    FeatureHint {
        text: "Press Alt+T to expand or collapse the latest visible tool output.",
        requirement: FeatureHintRequirement::Fullscreen,
    },
    FeatureHint {
        text: "Open the transcript reader with Ctrl+T; / or Ctrl+F searches, n and N jump between matches, and Alt+T toggles the latest tool output.",
        requirement: FeatureHintRequirement::Inline,
    },
    FeatureHint {
        text: "The prompt honors readline keys: Ctrl+A/E jump to line start/end and Ctrl+K/U/W delete.",
        requirement: FeatureHintRequirement::Always,
    },
];

/// One displayed value for a select-style session config option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigValueChoice {
    pub value: SessionConfigValueId,
    pub name: String,
    pub description: Option<String>,
    pub group: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolCallView {
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolCallStatus,
    pub body: Vec<ToolCallOutput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentBranchPullRequest {
    pub number: u64,
    pub url: String,
}

/// Durable facts about one locally submitted prompt turn.  Entries remain the
/// source-of-truth transcript; this only records lifecycle data which would
/// otherwise be lost once a later turn starts.
#[derive(Debug, Clone, Copy)]
struct PromptTurn {
    prompt_index: usize,
    elapsed: Option<Duration>,
    completed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallOutput {
    Text(String),
    Diff {
        path: String,
        old_text: Option<String>,
        new_text: String,
    },
    Terminal {
        terminal_id: String,
        output: String,
        truncated: bool,
        exit_status: Option<TerminalExitStatus>,
    },
    Note(String),
}

/// A terminal the agent started, tracked independently of the transcript.
///
/// Terminal output is inherently open-ended: a dev server or watcher never
/// reports an exit status, so it can never become an immutable transcript
/// record. Registering terminals here gives `/terminals` a stable, ordered
/// place to read them from without the transcript having to carry live state.
#[derive(Debug, Clone)]
pub(crate) struct TerminalRegistration {
    terminal_id: String,
    /// Tool call that started it, used to resolve current output and status.
    tool_call_id: String,
    /// Human label for the viewer, taken from the tool call title.
    label: String,
}

/// A terminal as `/terminals` presents it, resolved from whichever source
/// still holds its state.
#[derive(Debug, Clone)]
pub struct TerminalSummary {
    pub label: String,
    pub truncated: bool,
    pub exit_status: Option<TerminalExitStatus>,
}

impl TerminalSummary {
    /// A terminal with no exit status is still running. That is exactly the
    /// state that cannot be represented as a finished transcript entry.
    pub fn is_running(&self) -> bool {
        self.exit_status.is_none()
    }
}

impl ToolCallOutput {
    fn from_diff(diff: &Diff) -> Self {
        Self::Diff {
            path: diff.path.display().to_string(),
            old_text: diff.old_text.clone(),
            new_text: diff.new_text.clone(),
        }
    }
}

impl ToolCallView {
    fn from_tool_call(tc: &ToolCall) -> Self {
        let mut v = Self {
            title: tc.title.clone(),
            kind: tc.kind,
            status: tc.status,
            body: Vec::new(),
        };
        v.set_content(&tc.content);
        v
    }

    fn apply_update(&mut self, u: &ToolCallUpdate) {
        if let Some(t) = &u.fields.title {
            self.title = t.clone();
        }
        if let Some(k) = u.fields.kind {
            self.kind = k;
        }
        if let Some(s) = u.fields.status {
            self.status = s;
        }
        if let Some(c) = &u.fields.content {
            self.set_content(c);
        }
    }

    fn set_content(&mut self, content: &[ToolCallContent]) {
        self.body.clear();
        for c in content {
            match c {
                ToolCallContent::Content(block) => {
                    self.body
                        .push(ToolCallOutput::Text(content_block_text(&block.content)));
                }
                ToolCallContent::Diff(d) => {
                    self.body.push(ToolCallOutput::from_diff(d));
                }
                ToolCallContent::Terminal(t) => {
                    self.body.push(ToolCallOutput::Terminal {
                        terminal_id: t.terminal_id.to_string(),
                        output: String::new(),
                        truncated: false,
                        exit_status: None,
                    });
                }
                _ => self
                    .body
                    .push(ToolCallOutput::Note("unsupported tool content".to_string())),
            }
        }
    }

    fn apply_terminal_output(&mut self, snapshot: &TerminalOutputSnapshot) -> bool {
        let mut changed = false;
        for output in &mut self.body {
            if let ToolCallOutput::Terminal {
                terminal_id,
                output,
                truncated,
                exit_status,
            } = output
                && terminal_id == &snapshot.terminal_id
                && (output != &snapshot.output
                    || *truncated != snapshot.truncated
                    || *exit_status != snapshot.exit_status)
            {
                *output = snapshot.output.clone();
                *truncated = snapshot.truncated;
                *exit_status = snapshot.exit_status.clone();
                changed = true;
            }
        }
        changed
    }

    fn namespace_terminal_ids(&mut self, prefix: &str) {
        for output in &mut self.body {
            if let ToolCallOutput::Terminal { terminal_id, .. } = output
                && !terminal_id.starts_with(prefix)
            {
                *terminal_id = format!("{prefix}{terminal_id}");
            }
        }
    }
}

/// Prefix stamped onto every id that came from a subagent's ACP session.
const SUBAGENT_ID_PREFIX: &str = "subagent-";

pub(crate) fn is_subagent_transport_call(tool_call: &ToolCall) -> bool {
    subagent_identity_from_raw_input(tool_call.raw_input.as_ref())
        || subagent_identity_from_name(&tool_call.title)
        || subagent_identity_from_meta(tool_call.meta.as_ref())
}

pub(crate) fn is_subagent_transport_update(update: &ToolCallUpdate) -> bool {
    subagent_identity_from_raw_input(update.fields.raw_input.as_ref())
        || update
            .fields
            .title
            .as_deref()
            .is_some_and(subagent_identity_from_name)
        || subagent_identity_from_meta(update.meta.as_ref())
}

fn subagent_identity_from_raw_input(raw_input: Option<&serde_json::Value>) -> bool {
    let Some(object) = raw_input.and_then(serde_json::Value::as_object) else {
        return false;
    };
    object
        .get("server")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|server| server == "mj-subagents")
        && object
            .get("tool")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|tool| matches!(tool, "create_subagent" | "subagent_cancel"))
}

fn subagent_identity_from_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("mj-subagents")
        && ["create_subagent", "subagent_cancel"]
            .into_iter()
            .any(|tool| contains_tool_identifier(&name, tool))
}

/// Tool titles arrive in a few transport-specific forms (for example
/// `mcp.mj-subagents.create_subagent` and
/// `mcp__mj-subagents__create_subagent`).  Match complete identifiers so a
/// similarly named third-party tool is not hidden from the transcript.
fn contains_tool_identifier(name: &str, tool: &str) -> bool {
    name.match_indices(tool).any(|(start, _)| {
        let before = name[..start].chars().next_back();
        let suffix = &name[start + tool.len()..];
        let after = suffix.chars().next();
        (!before.is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
            || name[..start].ends_with("__"))
            && (!after
                .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
                || suffix.starts_with("__"))
    })
}

fn subagent_identity_from_meta(meta: Option<&serde_json::Map<String, serde_json::Value>>) -> bool {
    let Some(meta) = meta else {
        return false;
    };
    meta.get("toolName")
        .and_then(serde_json::Value::as_str)
        .is_some_and(subagent_identity_from_name)
        || meta
            .get("claudeCode")
            .and_then(serde_json::Value::as_object)
            .and_then(|claude| claude.get("toolName"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(subagent_identity_from_name)
}

/// Lifecycle of the ACP connection from launch through shutdown.
///
/// Driven by `UiEvent`s from the ACP runtime plus a couple of UI-initiated
/// transitions (`record_user_prompt`, `mark_cancelling`). The header label
/// is derived from this state, so it doubles as the externally visible
/// connection indicator described in PLANS.md M1.
///
/// Prompt turn state is derived from this enum via `AppState::is_streaming`.
/// Submission gating uses `AppState::is_busy`, which also includes lifecycle
/// operations like session fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Agent process is being spawned and `initialize` is in flight.
    Launching,
    /// `initialize` succeeded; `session/new` is in flight.
    Initializing,
    /// Session is open and accepting prompts.
    Ready,
    /// A prompt turn is streaming responses.
    Streaming,
    /// Cancellation was requested; awaiting the final `PromptDone`.
    Cancelling,
    /// A `session/fork` request is in flight.
    Forking,
    /// Runtime shut down cleanly (UI quit or agent EOF).
    Closed,
    /// Runtime ended with a fatal error.
    Fatal,
}

/// Severity attached to transient status text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Warning,
    Fatal,
}

/// Transient status text kept for input handling and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusMessage {
    pub kind: StatusKind,
    pub text: String,
}

impl StatusMessage {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            kind: StatusKind::Info,
            text: text.into(),
        }
    }

    pub fn warning(text: impl Into<String>) -> Self {
        Self {
            kind: StatusKind::Warning,
            text: text.into(),
        }
    }

    pub fn fatal(text: impl Into<String>) -> Self {
        Self {
            kind: StatusKind::Fatal,
            text: text.into(),
        }
    }
}

/// Token and context usage surfaced by the agent, when available.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub total_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub thought_tokens: Option<u64>,
    pub context_used: Option<u64>,
    pub context_size: Option<u64>,
    /// The most recently surfaced rate-limit line, kept so the header can
    /// deliberately omit it (see `ui`) while the transcript shows it.
    pub rate_limit: Option<String>,
    /// Last surfaced line per window (`Current session`, `Current week …`).
    /// Claude emits one window per `_claude/rateLimit` event, so we dedup per
    /// window rather than against a single value — otherwise an unchanged
    /// window re-appears whenever a *different* window updates in between.
    rate_limit_windows: HashMap<String, String>,
}

impl TokenUsage {
    fn apply_prompt_usage(&mut self, usage: Usage) {
        self.total_tokens = Some(usage.total_tokens);
        self.input_tokens = Some(usage.input_tokens);
        self.output_tokens = Some(usage.output_tokens);
        self.thought_tokens = usage.thought_tokens;
    }

    /// Apply a usage update and return a Claude rate-limit line when one is
    /// newly observed for its window. The caller surfaces that line in the
    /// transcript; the header intentionally omits it. Deduplicating per window
    /// keeps the frequent usage updates from spamming the transcript with a
    /// status that hasn't actually changed.
    fn apply_usage_update(&mut self, update: UsageUpdate) -> Option<String> {
        self.context_used = Some(update.used);
        self.context_size = Some(update.size);

        let line = claude_rate_limit_label(update.meta.as_ref())?;
        // The window label is the stable prefix before the first `:`
        // (e.g. "Current session"); dedup against the last line for it.
        let window = line.split(':').next().unwrap_or(line.as_str()).to_string();
        if self.rate_limit_windows.get(&window).map(String::as_str) == Some(line.as_str()) {
            return None;
        }
        self.rate_limit_windows.insert(window, line.clone());
        self.rate_limit = Some(line.clone());
        Some(line)
    }
}

fn claude_rate_limit_label(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    let value = meta?.get(CLAUDE_RATE_LIMIT_META_KEY)?;
    format_claude_rate_limit(value)
}

/// Render a Claude `_claude/rateLimit` payload (the SDK's `SDKRateLimitInfo`)
/// as one human line, e.g.
/// `Current session: 8% used · resets Jun 17 at 4:49pm (Europe/Paris)`.
///
/// The Claude Agent SDK emits one window per event (`rate_limit_event`), so
/// each event maps to exactly one line. `utilization` is a 0..100 percentage
/// and `resetsAt` is a unix timestamp.
fn format_claude_rate_limit(value: &serde_json::Value) -> Option<String> {
    let object = value.as_object()?;
    let label = rate_limit_window_label(string_field(object, "rateLimitType", "rate_limit_type"));

    let utilization = number_field(object, "utilization", "utilization")
        .map(|util| format!("{}% used", util.round().clamp(0.0, 100.0) as u64));
    let reset = number_field(object, "resetsAt", "resets_at")
        .or_else(|| number_field(object, "overageResetsAt", "overage_resets_at"))
        .and_then(crate::usage_format::format_reset_local)
        .map(|reset| format!("resets {reset}"));

    let detail = [utilization, reset]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");

    Some(if detail.is_empty() {
        label.to_string()
    } else {
        format!("{label}: {detail}")
    })
}

/// Map the SDK's `rateLimitType` discriminant to the wording Claude Code shows
/// in `/usage`. Unknown or missing types fall back to a generic label.
fn rate_limit_window_label(kind: Option<&str>) -> &'static str {
    match kind {
        Some("five_hour") => "Current session",
        Some("seven_day") => "Current week (all models)",
        Some("seven_day_opus") => "Current week (Opus)",
        Some("seven_day_sonnet") => "Current week (Sonnet)",
        Some("overage") => "Extra usage",
        _ => "Usage limit",
    }
}

fn string_field<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    camel: &str,
    snake: &str,
) -> Option<&'a str> {
    object
        .get(camel)
        .or_else(|| object.get(snake))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
}

fn number_field(
    object: &serde_json::Map<String, serde_json::Value>,
    camel: &str,
    snake: &str,
) -> Option<f64> {
    object
        .get(camel)
        .or_else(|| object.get(snake))
        .and_then(serde_json::Value::as_f64)
}

/// In-session host for the same settings editor used on first startup.
#[derive(Debug, Clone)]
pub struct MjConfigMenu {
    pub editor: SettingsEditor,
    orig_theme: TerminalThemeKind,
    orig_spinner: SpinnerStyle,
    orig_thought_output: crate::config::ThoughtOutput,
}

/// Mouse drag selection over the fullscreen transcript panel, in terminal
/// screen coordinates. Exists only between left-button press and release;
/// the release copies the covered text to the clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptSelection {
    /// Cell where the drag started.
    pub anchor: (u16, u16),
    /// Cell the pointer last reached.
    pub head: (u16, u16),
}

#[derive(Debug)]
pub struct AppState {
    pub theme_kind: TerminalThemeKind,
    pub theme: TerminalTheme,
    /// Client-side prompt-activity spinner style. Purely cosmetic; persisted
    /// in config like [`theme_kind`](Self::theme_kind).
    pub spinner_style: SpinnerStyle,
    /// Amount of agent thought text shown in the normal transcript.
    pub thought_output: crate::config::ThoughtOutput,
    /// Open `/mjconfig` overlay, if any.
    pub mjconfig_menu: Option<MjConfigMenu>,
    pub acp_inventory: crate::roster::AcpInventory,
    /// Active `/ragnarok` battle (arena overlay), if any.
    pub ragnarok: Option<RagnarokUi>,
    /// One-shot launch request set by `/ragnarok <task>`. The UI loop takes
    /// it and spawns the battle task, because the loop owns the event channel.
    ragnarok_launch: Option<String>,
    /// The ACP session cwd; `/ragnarok` forges its worktrees off this
    /// directory's git project.
    pub session_cwd: PathBuf,
    /// Persistent memory store `/memory` operates on. Tests point this at a
    /// temp file; production uses the default path.
    pub memory_store_path: PathBuf,
    pub agent_label: String,
    /// Human-readable ACP adapter backing the primary model, such as Codex or
    /// Claude Code. Kept separate from the role/model label in the header.
    primary_acp_name: String,
    /// Registry `source_id` of the launched agent (`claude-acp` or
    /// `codex-acp`). Distinct from `agent_label`,
    /// which is a *display* string; this is the stable id the model-score
    /// resolver keys on. Empty until the launch site fills it in.
    pub agent_source_id: String,
    /// Reasoning effort resolved for the active primary session. `None`
    /// means the ACP adapter selected its own default.
    pub primary_reasoning_effort: Option<String>,
    /// Launch command for the active session agent. Ragnarok uses this for
    /// the primary agent so the router follows the user's current agent instead of the
    /// competitor pool.
    pub active_agent_launch: Option<crate::ragnarok::Launch>,
    /// Score catalog for this UI run. It may be populated asynchronously after
    /// startup; render code reads through this explicit state rather than a
    /// process-global catalog.
    pub session: SessionState,
    /// Whether periodic local feature-discovery hints are enabled.
    pub feature_hints_enabled: bool,
    /// Holds an OS sleep assertion while a turn is in flight (and the config
    /// switch is on). Driven from `set_connection_state` so it cannot drift
    /// from the lifecycle enum.
    pub keep_awake: crate::keep_awake::KeepAwake,
    completed_turns_since_hint: usize,
    feature_hint_cursor: usize,
    /// Latest on-demand worktree-versus-`HEAD` diff backing the Ctrl-G reader.
    /// One `Option` rather than a history: the workspace has a single current
    /// state, and every refresh supersedes the last.
    pub workspace_head_diff: Option<crate::event::WorkspaceHeadDiffEvent>,
    /// A refresh is in flight. Distinguishes "still reading the worktree" from
    /// "read the worktree and found nothing", which must not look alike.
    pub workspace_diff_loading: bool,
    /// Accurate changed-file count reported for the current prompt turn.
    /// Consumed when that turn completes so an older diff cannot affect a
    /// later no-diff turn's status.
    pending_workspace_diff_total: Option<usize>,
    /// UI-owned reveal bounds for currently streaming prose. The canonical
    /// transcript always retains the complete source; this only lets the
    /// terminal renderer hold back incomplete or not-yet-paced source.
    stream_visible_bytes: HashMap<usize, usize>,
    /// Entries below this index have been irrevocably written to terminal
    /// scrollback even though their backing state may still change (#615's
    /// overflow valve). Monotonic: scrollback cannot be retracted, so the
    /// stability predicate must keep reporting them stable forever.
    committed_transcript_entries: usize,
    pub input: String,
    /// Cursor position in `input`, counted in Unicode scalar values from
    /// the start of the buffer.
    pub input_cursor: usize,
    /// Scroll offset measured in rendered lines from the bottom of the
    /// prompt box. `0` keeps the view pinned to the newest line.
    pub input_scroll_offset: usize,
    /// Previously submitted prompts, oldest first. Used for Up/Down
    /// navigation in the input buffer.
    prompt_history: Vec<String>,
    /// Structured file resources associated with each prompt-history entry.
    /// Entries loaded from the legacy text-only history file use an empty vec.
    prompt_history_resources: Vec<Vec<PromptResource>>,
    /// Index into `prompt_history` when navigating history. `None` means
    /// the user is not currently browsing history (they're editing a fresh
    /// input or the navigation was reset).
    history_cursor: Option<usize>,
    /// Saved input when history navigation starts. Restored when the user
    /// presses Down past the most recent history entry.
    history_saved_input: String,
    /// File chips belonging to `history_saved_input`.
    history_saved_file_attachments: Vec<FileAttachment>,
    /// Text attachments shown as compact badges in the input box; their
    /// contents are concatenated with `input` when the prompt is submitted.
    pub attachments: Vec<PastedAttachment>,
    /// Pasted image attachments shown as compact badges and submitted as
    /// ACP image content blocks.
    pub image_attachments: Vec<PastedImageAttachment>,
    /// Workspace files selected through `@` completion and submitted as ACP
    /// resource links.
    pub file_attachments: Vec<FileAttachment>,
    /// Fast plain-character stream candidate. Terminals can deliver
    /// drag/drop and paste data as key events instead of bracketed paste.
    pub input_paste_burst: InputPasteBurst,
    pub next_attachment_id: usize,
    /// FIFO queue of permission prompts. The front element is the one
    /// currently shown in the modal; new requests are pushed to the back
    /// so they aren't silently dropped when one is already on screen.
    ///
    /// Private so callers can't accidentally bypass the queue invariants
    /// (e.g. push without going through `apply_event`, or take without
    /// answering the responder). External code goes through
    /// `has_pending_permission` / `pending_permission` /
    /// `take_pending_permission` / `cancel_all_pending_permissions`.
    permission_queue: VecDeque<PendingPermission>,
    /// FIFO queue of elicitation prompts (`/setup` menus / URL steps), with
    /// the same anti-drop invariant as `permission_queue`: a second request
    /// queues behind the first rather than overwriting (and silently
    /// cancelling) its responder. Private; accessed via the
    /// `*_pending_elicitation*` helpers.
    elicitation_queue: VecDeque<PendingElicitation>,
    pub agent_picker: Option<AgentPicker>,
    pub team_picker: Option<TeamPicker>,
    pub config_picker: Option<ConfigPicker>,
    pub review_picker: Option<ReviewPicker>,
    /// Scroll offset measured in rendered lines from the bottom of the
    /// transcript. `0` keeps the view pinned to the newest line.
    pub scroll_offset: usize,
    /// When false, stable long messages and tool-call outputs are compacted in
    /// the transcript. In the fullscreen TUI, Ctrl-T flips this globally.
    pub expand_transcript_details: bool,
    /// When true (inline mode only), the compact chat view is replaced by a
    /// full-height, scrollable reader showing the entire transcript with
    /// messages and tool outputs fully expanded. Inline scrollback is immutable once
    /// flushed, so this reader is how users re-read earlier output in full.
    pub transcript_viewer: bool,
    pub transcript_search: Option<TranscriptSearch>,
    /// On-demand roster and transcript reader for nested implementation and
    /// review actors. It is available in inline and fullscreen modes.
    pub nested_agent_viewer: bool,
    pub nested_agent_selected: Option<u64>,
    pub nested_agent_scroll_offset: usize,
    /// On-demand reader for agent-started terminals, opened with `/terminals`.
    pub terminals_viewer: bool,
    pub terminals_selected: usize,
    pub terminals_scroll_offset: usize,
    /// Session-wide review finding ledger, opened with F9.
    pub review_issue_viewer: bool,
    pub review_issue_scroll_offset: usize,
    /// Dedicated reader for the most recent native workspace-diff event.
    /// Its selection and scroll state intentionally do not share transcript
    /// state: workspace changes are not transcript entries.
    pub workspace_diff_viewer: bool,
    pub workspace_diff_selected_file: usize,
    pub workspace_diff_scroll_offset: usize,
    pub exit_reason: Option<UiExitReason>,
    /// True once the runtime has stopped accepting commands.
    pub runtime_closed: bool,
    /// At least one subagent is currently running in the background.
    pub subagent_active: bool,
    /// Display label of the most recently started subagent.
    pub subagent_label: Option<String>,
    /// Number of subagents currently running in the background.
    pub active_subagents: usize,
    /// Durable nested-agent state keyed by stable subagent id.
    subagents: BTreeMap<u64, SubagentStatus>,
    nested_history_dir: Option<NestedHistoryDir>,
    /// Canonical runtime-owned workflow state. Transcript prose and display
    /// labels never mutate this store.
    pub workflows: crate::workflow::WorkflowStore,
    /// UI-side clocks for visible workflow progress rows. Lifecycle and counts
    /// remain reducer-owned; terminal rows keep their frozen clock until the
    /// next user turn starts.
    workflow_clocks: BTreeMap<crate::workflow::WorkflowId, WorkflowClock>,
    pub agent_usage: crate::agent_usage::Snapshot,
    /// Transient status line with severity.
    pub status_line: Option<StatusMessage>,
    /// Open pull request resolved by `gh pr view` for the checked-out branch.
    pub current_branch_pull_request: Option<CurrentBranchPullRequest>,
    /// Branch used for the latest pull-request probe. Kept separately so a
    /// branch switch immediately retires the previous branch's result.
    pub(crate) current_branch_pull_request_branch: Option<String>,
    /// True while the local microphone dictation helper is running.
    pub voice_input_active: bool,
    /// Prompt buffer range currently owned by live voice dictation.
    pub voice_input_range: Option<(usize, usize)>,
    /// Last microphone input level reported by voice dictation, 0.0..=1.0.
    pub voice_input_level: Option<f32>,
    /// Timing for the active or most recently completed prompt turn.
    turn_started_at: Option<Instant>,
    last_turn_elapsed: Option<Duration>,
    prompt_turns: Vec<PromptTurn>,
    active_prompt_turn: Option<usize>,
    /// Last token/context usage reported by the agent.
    pub token_usage: TokenUsage,
    /// Usage for the most recently started nested subagent session. Kept
    /// separate so the header never presents the primary's context as a
    /// subagent's.
    subagent_token_usage: TokenUsage,
    /// Last Claude Code `/usage` quota scrape, when the active agent is Claude.
    pub claude_usage: Option<ClaudeUsageStatus>,
    /// Last Codex app-server quota query, including explicit unavailable states.
    pub codex_usage: Option<CodexUsageStatus>,
    /// Slash-command and workspace-file autocomplete state.
    pub autocomplete: Autocomplete,
    /// Additional directories registered as ACP workspace roots.
    pub additional_workspace_roots: Vec<PathBuf>,
    file_autocomplete_indexed_roots: Option<Vec<PathBuf>>,
    file_autocomplete_indexed_at: Option<Instant>,
    file_autocomplete_loading_roots: Option<Vec<PathBuf>>,
    file_autocomplete_scan_request: Option<Vec<PathBuf>>,
    file_autocomplete_candidates: Vec<WorkspaceFile>,
    /// True while the keyboard help overlay is visible.
    pub help_overlay: bool,
    /// Wrapped row offset shown by the keyboard help overlay.
    pub help_scroll: u16,
    /// True while mouse capture is disabled so the terminal can select text.
    pub text_selection_mode: bool,
    /// In-progress mouse drag selection over the fullscreen transcript panel.
    /// Cleared (and copied to the clipboard) on mouse-up.
    pub transcript_selection: Option<TranscriptSelection>,
    /// Screen area `(x, y, width, height)` of the transcript panel, captured
    /// each frame so mouse events can be mapped onto the visible text.
    pub transcript_panel_area: Option<(u16, u16, u16, u16)>,
    /// Per-cell symbols of the visible transcript rows, captured at draw time
    /// while a selection is active. Continuation cells of wide graphemes hold
    /// empty strings so cell columns stay aligned with screen columns.
    pub transcript_panel_grid: Vec<Vec<String>>,
    /// Project shown in the bottom status line so users can tell which
    /// checkout this session belongs to without leaking nested worktree paths.
    pub project_label: String,
    /// Short linked-worktree name shown separately from the project when
    /// the session runs under `.mjolnir/worktrees/`.
    pub worktree_label: Option<String>,
    /// Number of extra ACP workspace roots active for this session.
    pub additional_roots: usize,
    /// Directory where `/export` writes Markdown transcript files.
    pub transcript_export_dir: Option<PathBuf>,
    /// Config file used by local UI-only settings such as `/mjconfig`.
    pub config_path: Option<PathBuf>,
    /// DeepSWE model catalog and current model-first selectors.
    pub model_choices: Vec<crate::roster::ModelChoice>,
    /// Preferences saved for the next `/new` or `/clear` boundary and immutable
    /// resolutions used by the current session are deliberately shown separately.
    pub configured_models: crate::config::ModelsConfig,
    pub active_models: crate::config::ModelsConfig,
    pub review_enabled: bool,
    pub review_tier: crate::config::ReviewTier,
    pub ragnarok_models: Vec<crate::roster::ResolvedAgent>,
    /// Holds the platform clipboard lease so copied text remains available
    /// on Linux/X11 where the owning process must stay alive.
    #[allow(dead_code)]
    pub clipboard_lease: Option<ClipboardLease>,
    /// Prompts the user submitted while a previous turn was still in
    /// flight. They stay out of the transcript until the runtime actually
    /// sends them, then drain oldest-first.
    queued_prompts: VecDeque<QueuedPrompt>,
    /// The one prompt accepted while the primary ACP session is still
    /// starting. Its command is already queued for the runtime, but the
    /// editor stays intact until session readiness (or startup failure).
    startup_prompt: Option<QueuedPrompt>,
}

/// A prompt staged behind the currently streaming turn. The runtime takes
/// it from the UI loop once `is_streaming` flips back to false and
/// `session_id` is still bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedPrompt {
    /// Raw text sent to the agent (attachments already concatenated).
    pub text: String,
    /// Image content blocks captured at queue time.
    pub images: Vec<PromptImage>,
    /// ACP resource links captured at queue time.
    pub resources: Vec<PromptResource>,
    /// Transcript-ready display text (matches what `submit_prompt` would
    /// have produced if the prompt had fired immediately).
    pub display_text: String,
}

#[derive(Debug)]
pub struct PendingPermission {
    pub prompt: PermissionPrompt,
    pub selected: usize,
    pub scroll_offset: Option<usize>,
    pub subagent_id: Option<u64>,
}

#[derive(Debug)]
pub struct PendingElicitation {
    pub prompt: ElicitationPrompt,
    /// Cursor into the active single- or multi-select option list. Ignored by
    /// URL, text, and unsupported views.
    pub selected: usize,
    /// Manual scroll position for content taller than the modal (e.g. a URL
    /// QR code). `None` auto-scrolls to keep the selected option visible.
    pub scroll_offset: Option<usize>,
    /// Typed buffer for a free-text field, including the active text field in
    /// a multi-property form. Editing is append/backspace at the end.
    pub input: String,
    /// Current field and accumulated answers for a multi-property form.
    pub form_field: usize,
    pub form_content: BTreeMap<String, ElicitationContentValue>,
    /// Selected option indices for the active multi-select field.
    pub multi_selected: HashSet<usize>,
    pub subagent_id: Option<u64>,
}

impl PendingElicitation {
    pub fn new(prompt: ElicitationPrompt, subagent_id: Option<u64>) -> Self {
        Self {
            prompt,
            selected: 0,
            scroll_offset: None,
            input: String::new(),
            form_field: 0,
            form_content: BTreeMap::new(),
            multi_selected: HashSet::new(),
            subagent_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ElicitationFormField {
    pub property_name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub required: bool,
    pub kind: ElicitationFormFieldKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ElicitationFormFieldKind {
    SingleSelect {
        options: Vec<EnumOption>,
    },
    MultiSelect {
        options: Vec<EnumOption>,
        min_items: Option<u64>,
        max_items: Option<u64>,
    },
    Text,
    Number {
        minimum: Option<f64>,
        maximum: Option<f64>,
    },
    Integer {
        minimum: Option<i64>,
        maximum: Option<i64>,
    },
    Boolean,
}

/// How a pending elicitation should be rendered and resolved, derived once
/// from its mode + schema so the renderer and the key handler agree on the
/// interpretation. Owned data keeps both call sites borrow-free.
#[derive(Debug, Clone, PartialEq)]
pub enum ElicitationView {
    /// Single-select form: exactly one property, a `StringPropertySchema`
    /// with a non-empty `oneOf` or `enum`. Accept maps `{ property => String(value) }`.
    SingleSelect {
        property_name: String,
        title: Option<String>,
        options: Vec<EnumOption>,
    },
    /// URL/QR step (e.g. OAuth login). Accept carries no content.
    Url { url: String },
    /// Free-text form: exactly one property, a `StringPropertySchema` with no
    /// `oneOf`/`enum` (e.g. an API-key entry). Accept maps
    /// `{ property => String(typed_value) }`.
    Text {
        property_name: String,
        title: Option<String>,
        description: Option<String>,
    },
    /// A form with multiple properties, or a single multi-select property.
    /// Fields are presented in schema order and accumulated into one Accept.
    Form {
        title: Option<String>,
        fields: Vec<ElicitationFormField>,
    },
    /// Any shape the UI cannot render (an enum with no options or a future
    /// schema variant). The modal shows an informational message and resolves
    /// to `decline` on dismiss.
    Unsupported,
}

/// Classify an elicitation prompt into the renderable/resolvable view. Never
/// panics on an unexpected schema: unsupported primitive or future variants
/// become [`ElicitationView::Unsupported`].
pub fn classify_elicitation(prompt: &ElicitationPrompt) -> ElicitationView {
    match &prompt.mode {
        ElicitationMode::Url(url_mode) => ElicitationView::Url {
            url: url_mode.url.clone(),
        },
        ElicitationMode::Form(form) => {
            let schema = &form.requested_schema;
            if schema.properties.is_empty() {
                return ElicitationView::Unsupported;
            }
            if schema.properties.len() > 1
                || matches!(
                    schema.properties.values().next(),
                    Some(
                        ElicitationPropertySchema::Array(_)
                            | ElicitationPropertySchema::Number(_)
                            | ElicitationPropertySchema::Integer(_)
                            | ElicitationPropertySchema::Boolean(_)
                    )
                )
            {
                let required = schema.required.as_deref().unwrap_or_default();
                let mut fields = Vec::with_capacity(schema.properties.len());
                for (property_name, property) in &schema.properties {
                    let field = match property {
                        ElicitationPropertySchema::String(string_schema) => {
                            let options = string_schema
                                .one_of
                                .clone()
                                .filter(|options| !options.is_empty())
                                .or_else(|| {
                                    string_schema.enum_values.as_ref().and_then(|values| {
                                        (!values.is_empty()).then(|| {
                                            values
                                                .iter()
                                                .map(|value| {
                                                    EnumOption::new(value.clone(), value.clone())
                                                })
                                                .collect()
                                        })
                                    })
                                });
                            ElicitationFormField {
                                property_name: property_name.clone(),
                                title: string_schema.title.clone(),
                                description: string_schema.description.clone(),
                                required: required.contains(property_name),
                                kind: options.map_or(ElicitationFormFieldKind::Text, |options| {
                                    ElicitationFormFieldKind::SingleSelect { options }
                                }),
                            }
                        }
                        ElicitationPropertySchema::Array(array_schema) => {
                            let options = match &array_schema.items {
                                MultiSelectItems::Titled(items) => items.options.clone(),
                                MultiSelectItems::String(items) => items
                                    .values
                                    .iter()
                                    .map(|value| EnumOption::new(value.clone(), value.clone()))
                                    .collect(),
                                _ => return ElicitationView::Unsupported,
                            };
                            if options.is_empty() {
                                return ElicitationView::Unsupported;
                            }
                            ElicitationFormField {
                                property_name: property_name.clone(),
                                title: array_schema.title.clone(),
                                description: array_schema.description.clone(),
                                required: required.contains(property_name),
                                kind: ElicitationFormFieldKind::MultiSelect {
                                    options,
                                    min_items: array_schema.min_items,
                                    max_items: array_schema.max_items,
                                },
                            }
                        }
                        ElicitationPropertySchema::Number(number_schema) => ElicitationFormField {
                            property_name: property_name.clone(),
                            title: number_schema.title.clone(),
                            description: number_schema.description.clone(),
                            required: required.contains(property_name),
                            kind: ElicitationFormFieldKind::Number {
                                minimum: number_schema.minimum,
                                maximum: number_schema.maximum,
                            },
                        },
                        ElicitationPropertySchema::Integer(integer_schema) => {
                            ElicitationFormField {
                                property_name: property_name.clone(),
                                title: integer_schema.title.clone(),
                                description: integer_schema.description.clone(),
                                required: required.contains(property_name),
                                kind: ElicitationFormFieldKind::Integer {
                                    minimum: integer_schema.minimum,
                                    maximum: integer_schema.maximum,
                                },
                            }
                        }
                        ElicitationPropertySchema::Boolean(boolean_schema) => {
                            ElicitationFormField {
                                property_name: property_name.clone(),
                                title: boolean_schema.title.clone(),
                                description: boolean_schema.description.clone(),
                                required: required.contains(property_name),
                                kind: ElicitationFormFieldKind::Boolean,
                            }
                        }
                        _ => return ElicitationView::Unsupported,
                    };
                    fields.push(field);
                }
                return ElicitationView::Form {
                    title: schema.title.clone(),
                    fields,
                };
            }
            let Some((property_name, property)) = schema.properties.iter().next() else {
                return ElicitationView::Unsupported;
            };
            match property {
                ElicitationPropertySchema::String(string_schema) => {
                    let one_of_options = string_schema
                        .one_of
                        .as_ref()
                        .filter(|opts| !opts.is_empty());
                    let enum_options = string_schema
                        .enum_values
                        .as_ref()
                        .filter(|opts| !opts.is_empty());
                    match (one_of_options, enum_options) {
                        (Some(options), _) => ElicitationView::SingleSelect {
                            property_name: property_name.clone(),
                            // Prefer the per-property title, falling back to the
                            // schema-level title for the modal heading.
                            title: string_schema.title.clone().or_else(|| schema.title.clone()),
                            options: options.clone(),
                        },
                        (None, Some(values)) => ElicitationView::SingleSelect {
                            property_name: property_name.clone(),
                            title: string_schema.title.clone().or_else(|| schema.title.clone()),
                            options: values
                                .iter()
                                .map(|value| EnumOption::new(value.clone(), value.clone()))
                                .collect(),
                        },
                        // A string field without `oneOf` or `enum` is free
                        // text: render an input field (e.g. API-key entry).
                        _ => ElicitationView::Text {
                            property_name: property_name.clone(),
                            title: string_schema.title.clone().or_else(|| schema.title.clone()),
                            description: string_schema.description.clone(),
                        },
                    }
                }
                _ => ElicitationView::Unsupported,
            }
        }
        // `ElicitationMode` is `#[non_exhaustive]`; future modes degrade safely.
        _ => ElicitationView::Unsupported,
    }
}

/// A text attachment shown as a compact badge in the input box.
#[derive(Debug, Clone)]
pub struct PastedAttachment {
    #[allow(dead_code)]
    pub id: usize,
    pub position: usize,
    pub content: String,
}

/// Image content captured from the clipboard and held until submission.
#[derive(Debug, Clone)]
pub struct PastedImageAttachment {
    #[allow(dead_code)]
    pub id: usize,
    pub position: usize,
    pub data_base64: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub byte_len: usize,
}

/// Workspace file shown as an anchored chip in the prompt editor.
#[derive(Debug, Clone)]
pub struct FileAttachment {
    #[allow(dead_code)]
    pub id: usize,
    pub position: usize,
    pub display_path: String,
    pub resource: PromptResource,
}

/// Candidate text inserted by a rapid stream of plain character events.
#[derive(Debug, Clone, Default)]
pub struct InputPasteBurst {
    pub start_cursor: usize,
    pub text: String,
    pub last_char_at: Option<Instant>,
}

impl InputPasteBurst {
    pub fn clear(&mut self) {
        self.start_cursor = 0;
        self.text.clear();
        self.last_char_at = None;
    }
}

/// Steps in the deferred primary model and effort picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPickerStep {
    Model,
    Effort,
    ConfirmSave,
    StartNewSession,
}

/// Persisted primary-agent effort choices. `None` retains the ACP adapter's
/// own default rather than forcing a reasoning level.
pub const PRIMARY_EFFORT_OPTIONS: &[Option<&str>] = &[
    None,
    Some("off"),
    Some("minimal"),
    Some("low"),
    Some("medium"),
    Some("high"),
    Some("xhigh"),
    Some("max"),
];

pub fn primary_effort_value(index: usize) -> Option<&'static str> {
    PRIMARY_EFFORT_OPTIONS.get(index).copied().flatten()
}

/// Deferred primary-agent model picker overlay state.
#[derive(Debug, Clone)]
pub struct AgentPicker {
    pub selected: usize,
    /// Indices into `ragnarok_models`, deduplicated by configured model ID.
    pub role_indices: Vec<usize>,
    pub effort_selected: usize,
    pub step: AgentPickerStep,
    /// Whether the post-save new-session offer selects its affirmative action.
    pub start_new_session: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamPickerStep {
    Choose,
    StartNewSession,
}

/// Deferred four-configuration team picker opened by Ctrl+Tab.
#[derive(Debug, Clone)]
pub struct TeamPicker {
    pub selected: usize,
    pub step: TeamPickerStep,
    pub start_new_session: bool,
}

/// Config option picker overlay state.
#[derive(Debug, Clone)]
pub struct ConfigPicker {
    pub selected_option: usize,
    pub selected_value: usize,
    /// Search query to filter choices. Empty means show all.
    pub search_query: String,
    /// Indices into the full `choices` vec that match `search_query`.
    /// Always non-empty when `search_query` is non-empty (falls back to
    /// full list if no match).
    pub filtered_indices: Vec<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct ReviewPicker {
    pub selected: usize,
}

/// The candidate collection currently shown by the prompt autocomplete.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AutocompleteKind {
    #[default]
    Commands,
    Files {
        trigger_start: usize,
        trigger_end: usize,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceFile {
    display_path: String,
    absolute_path: PathBuf,
    root: PathBuf,
    size: Option<i64>,
}

/// Autocomplete popover for slash commands and workspace files.
///
/// `matches` holds indices into either `AppState.available_commands` or the
/// cached workspace-file index, as identified by `kind`.
#[derive(Debug, Default)]
pub struct Autocomplete {
    pub visible: bool,
    pub selected: usize,
    pub matches: Vec<usize>,
    pub kind: AutocompleteKind,
}

const MAX_FILE_AUTOCOMPLETE_CANDIDATES: usize = 50_000;
const MAX_FILE_AUTOCOMPLETE_MATCHES: usize = 200;
const FILE_AUTOCOMPLETE_CACHE_TTL: Duration = Duration::from_secs(5);

fn input_byte_index_at_char(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or(text.len())
}

fn active_file_autocomplete(input: &str, cursor: usize) -> Option<(usize, usize, String)> {
    let chars: Vec<char> = input.chars().collect();
    let cursor = cursor.min(chars.len());
    for index in (0..cursor).rev() {
        let ch = chars[index];
        if ch.is_whitespace() {
            break;
        }
        if ch != '@' {
            continue;
        }
        let at_boundary = index == 0
            || chars[index - 1].is_whitespace()
            || matches!(chars[index - 1], '(' | '[' | '{' | ',' | ';' | ':');
        if !at_boundary {
            return None;
        }
        let query: String = chars[index + 1..cursor].iter().collect();
        return Some((index, cursor, query));
    }
    None
}

pub(crate) fn file_mention_text(path: &str) -> String {
    if path.chars().any(char::is_whitespace) {
        format!("@\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("@{path}")
    }
}

fn git_workspace_files(root: &Path, limit: usize) -> Option<Vec<PathBuf>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let mut reader = BufReader::new(stdout);
    let mut relative_paths = Vec::new();
    let mut path = Vec::new();
    while relative_paths.len() < limit {
        path.clear();
        let read = match reader.read_until(0, &mut path) {
            Ok(read) => read,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        };
        if read == 0 {
            break;
        }
        if path.last() == Some(&0) {
            path.pop();
        }
        if !path.is_empty() {
            relative_paths.push(PathBuf::from(String::from_utf8_lossy(&path).into_owned()));
        }
    }

    if relative_paths.len() == limit {
        let _ = child.kill();
        let _ = child.wait();
        return Some(relative_paths);
    }
    child
        .wait()
        .ok()
        .filter(|status| status.success())
        .map(|_| relative_paths)
}

fn workspace_root_label(root: &Path, index: usize) -> Option<String> {
    (index > 0).then(|| mj_core::paths::folder_label(root))
}

pub(crate) fn workspace_file_candidates(roots: &[PathBuf]) -> Vec<WorkspaceFile> {
    let mut files = Vec::new();
    let mut canonical_roots = Vec::new();
    for root in roots {
        let root = root.canonicalize().unwrap_or_else(|_| root.clone());
        if !canonical_roots.iter().any(|candidate| candidate == &root) {
            canonical_roots.push(root);
        }
    }

    for (root_index, root) in canonical_roots.iter().enumerate() {
        let roots_left = canonical_roots.len() - root_index;
        let remaining = MAX_FILE_AUTOCOMPLETE_CANDIDATES.saturating_sub(files.len());
        if remaining == 0 {
            break;
        }
        // Reserve a fair share for every remaining root. Unused capacity from
        // a small root rolls forward to later roots.
        let root_limit = remaining / roots_left;
        let relative_paths = git_workspace_files(root, root_limit)
            .unwrap_or_else(|| fallback_workspace_files(root, root_limit));
        let root_label = workspace_root_label(root, root_index);
        for relative_path in relative_paths {
            if relative_path.is_absolute()
                || relative_path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                continue;
            }
            let absolute_path = root.join(&relative_path);
            let Ok(metadata) = std::fs::symlink_metadata(&absolute_path) else {
                continue;
            };
            if !metadata.file_type().is_file() {
                continue;
            }
            let relative_display = relative_path.to_string_lossy().replace('\\', "/");
            let display_path = root_label
                .as_ref()
                .map(|label| format!("{label}/{relative_display}"))
                .unwrap_or(relative_display);
            files.push(WorkspaceFile {
                display_path,
                absolute_path,
                root: root.clone(),
                size: i64::try_from(metadata.len()).ok(),
            });
        }
    }
    files.sort_by_cached_key(|file| file.display_path.to_lowercase());
    files
}

fn fallback_workspace_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if files.len() >= limit {
                return files;
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !matches!(
                    name.as_ref(),
                    ".git" | ".hg" | ".mjolnir" | ".svn" | "node_modules" | "target"
                ) {
                    pending.push(path);
                }
            } else if file_type.is_file()
                && let Ok(relative) = path.strip_prefix(root)
            {
                files.push(relative.to_path_buf());
            }
        }
    }
    files
}

impl std::ops::Deref for AppState {
    type Target = SessionState;

    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

impl std::ops::DerefMut for AppState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.session
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        let now = Instant::now();
        let theme_kind = TerminalThemeKind::default();
        Self {
            theme_kind,
            theme: theme_kind.palette(),
            spinner_style: SpinnerStyle::default(),
            thought_output: crate::config::ThoughtOutput::default(),
            mjconfig_menu: None,
            acp_inventory: crate::roster::AcpInventory::default(),
            ragnarok: None,
            ragnarok_launch: None,
            session_cwd: PathBuf::from("."),
            memory_store_path: crate::memory::default_path(),
            agent_label: String::new(),
            primary_acp_name: "ACP server".to_string(),
            agent_source_id: String::new(),
            primary_reasoning_effort: None,
            active_agent_launch: None,
            session: SessionState::new(now, {
                let mut commands = Vec::new();
                install_builtin_commands(&mut commands, false, false);
                commands
            }),
            feature_hints_enabled: true,
            keep_awake: crate::keep_awake::KeepAwake::new(),
            completed_turns_since_hint: 0,
            feature_hint_cursor: 0,
            workspace_head_diff: None,
            workspace_diff_loading: false,
            stream_visible_bytes: HashMap::new(),
            committed_transcript_entries: 0,
            input: String::new(),
            input_cursor: 0,
            input_scroll_offset: 0,
            attachments: Vec::new(),
            image_attachments: Vec::new(),
            file_attachments: Vec::new(),
            input_paste_burst: InputPasteBurst::default(),
            next_attachment_id: 0,
            prompt_history: Vec::new(),
            prompt_history_resources: Vec::new(),
            history_cursor: None,
            history_saved_input: String::new(),
            history_saved_file_attachments: Vec::new(),
            permission_queue: VecDeque::new(),
            elicitation_queue: VecDeque::new(),
            agent_picker: None,
            team_picker: None,
            config_picker: None,
            review_picker: None,
            scroll_offset: 0,
            expand_transcript_details: false,
            transcript_viewer: false,
            transcript_search: None,
            nested_agent_viewer: false,
            nested_agent_selected: None,
            nested_agent_scroll_offset: 0,
            terminals_viewer: false,
            terminals_selected: 0,
            terminals_scroll_offset: 0,
            review_issue_viewer: false,
            review_issue_scroll_offset: 0,
            workspace_diff_viewer: false,
            workspace_diff_selected_file: 0,
            workspace_diff_scroll_offset: 0,
            exit_reason: None,
            runtime_closed: false,
            subagent_active: false,
            subagent_label: None,
            active_subagents: 0,
            subagents: BTreeMap::new(),
            nested_history_dir: None,
            workflows: crate::workflow::WorkflowStore::default(),
            workflow_clocks: BTreeMap::new(),
            agent_usage: crate::agent_usage::Snapshot::default(),
            status_line: None,
            current_branch_pull_request: None,
            current_branch_pull_request_branch: None,
            voice_input_active: false,
            voice_input_range: None,
            voice_input_level: None,
            turn_started_at: None,
            last_turn_elapsed: None,
            prompt_turns: Vec::new(),
            active_prompt_turn: None,
            token_usage: TokenUsage::default(),
            subagent_token_usage: TokenUsage::default(),
            claude_usage: None,
            codex_usage: None,
            autocomplete: Autocomplete::default(),
            additional_workspace_roots: Vec::new(),
            file_autocomplete_indexed_roots: None,
            file_autocomplete_indexed_at: None,
            file_autocomplete_loading_roots: None,
            file_autocomplete_scan_request: None,
            file_autocomplete_candidates: Vec::new(),
            help_overlay: false,
            help_scroll: 0,
            text_selection_mode: false,
            transcript_selection: None,
            transcript_panel_area: None,
            transcript_panel_grid: Vec::new(),
            project_label: String::new(),
            worktree_label: None,
            additional_roots: 0,
            transcript_export_dir: None,
            config_path: None,
            model_choices: Vec::new(),
            configured_models: crate::config::ModelsConfig::default(),
            active_models: crate::config::ModelsConfig::default(),
            review_enabled: true,
            review_tier: crate::config::ReviewTier::default(),
            ragnarok_models: Vec::new(),
            clipboard_lease: None,
            queued_prompts: VecDeque::new(),
            startup_prompt: None,
            pending_workspace_diff_total: None,
        }
    }

    pub fn side_conversation(&self, question: Option<String>) -> Self {
        let mut side = Self::new();
        side.is_side = true;
        side.side_initial_question = question;
        side.theme_kind = self.theme_kind;
        side.theme = self.theme;
        side.spinner_style = self.spinner_style;
        side.thought_output = self.thought_output;
        side.feature_hints_enabled = self.feature_hints_enabled;
        side.keep_awake.set_enabled(self.keep_awake.enabled());
        side.project_label = self.project_label.clone();
        side.worktree_label = self.worktree_label.clone();
        side.additional_roots = self.additional_roots;
        side.session_cwd = self.session_cwd.clone();
        side.additional_workspace_roots = self.additional_workspace_roots.clone();
        side.agent_label = format!("Side · {}", self.agent_label);
        side.primary_acp_name = self.primary_acp_name.clone();
        side.agent_source_id = self.agent_source_id.clone();
        side.primary_reasoning_effort = self.primary_reasoning_effort.clone();
        side.active_agent_launch = self.active_agent_launch.clone();
        side.current_branch_pull_request = self.current_branch_pull_request.clone();
        side.current_branch_pull_request_branch = self.current_branch_pull_request_branch.clone();
        side.transcript_export_dir = self.transcript_export_dir.clone();
        side.prompt_images_supported = self.prompt_images_supported;
        side.side_main_notice = Some(
            if self.has_pending_permission() || self.has_pending_elicitation() {
                "Main needs input".to_string()
            } else if self.is_busy() {
                "Main running".to_string()
            } else {
                "Main idle".to_string()
            },
        );
        install_side_builtin_commands(&mut side.available_commands);
        side
    }

    pub(crate) fn agent_open_message_index(&self) -> Option<usize> {
        self.agent_open_message_index
    }

    pub(crate) fn set_codex_usage(&mut self, status: CodexUsageStatus) {
        self.codex_usage = Some(status);
    }

    pub(crate) fn set_claude_usage(&mut self, status: ClaudeUsageStatus) {
        self.claude_usage = Some(status);
    }

    pub fn set_theme(&mut self, theme_kind: TerminalThemeKind) {
        if self.theme_kind != theme_kind {
            self.bump_transcript_revision();
        }
        self.theme_kind = theme_kind;
        self.theme = theme_kind.palette();
    }

    pub fn set_spinner_style(&mut self, spinner_style: SpinnerStyle) {
        self.spinner_style = spinner_style;
    }

    pub fn set_thought_output(&mut self, thought_output: crate::config::ThoughtOutput) {
        if self.thought_output != thought_output {
            self.bump_transcript_revision();
        }
        self.thought_output = thought_output;
    }

    pub fn open_agent_picker(&mut self) -> bool {
        let mut seen = HashSet::new();
        let role_indices = self
            .ragnarok_models
            .iter()
            .enumerate()
            .filter_map(|(index, role)| seen.insert(role.model.model.as_str()).then_some(index))
            .collect::<Vec<_>>();
        if role_indices.is_empty() {
            return false;
        }
        let selected = role_indices
            .iter()
            .position(|&index| {
                self.ragnarok_models[index].model.model == self.active_models.primary
            })
            .unwrap_or(0);
        self.agent_picker = Some(AgentPicker {
            selected,
            role_indices,
            // Effort is chosen for the model being selected. Do not carry a
            // previous model's override into a new selection implicitly.
            effort_selected: 0,
            step: AgentPickerStep::Model,
            start_new_session: true,
        });
        true
    }

    pub fn open_team_picker(&mut self) {
        if crate::roster::external_adapter().is_some() {
            return;
        }
        let active = self
            .config_path
            .as_deref()
            .and_then(|path| crate::config::Config::load(path).ok())
            .as_ref()
            .and_then(crate::config::TeamPreset::from_config);
        let selected = active
            .and_then(|active| {
                crate::config::TeamPreset::ALL
                    .iter()
                    .position(|preset| *preset == active)
            })
            .unwrap_or(0);
        self.team_picker = Some(TeamPicker {
            selected,
            step: TeamPickerStep::Choose,
            start_new_session: true,
        });
    }

    pub fn team_picker_move(&mut self, delta: i32) {
        let Some(picker) = self.team_picker.as_mut() else {
            return;
        };
        let len = crate::config::TeamPreset::ALL.len();
        picker.selected = (picker.selected as i32 + delta).rem_euclid(len as i32) as usize;
    }

    pub fn team_picker_selection(&self) -> Option<crate::config::TeamPreset> {
        let picker = self.team_picker.as_ref()?;
        crate::config::TeamPreset::ALL.get(picker.selected).copied()
    }

    pub fn team_picker_toggle_start_new_session(&mut self) {
        if let Some(picker) = self.team_picker.as_mut() {
            picker.start_new_session = !picker.start_new_session;
        }
    }

    pub fn agent_picker_move(&mut self, delta: i32) {
        let Some(picker) = self.agent_picker.as_mut() else {
            return;
        };
        move_wrapped(&mut picker.selected, delta, picker.role_indices.len());
    }

    pub fn agent_picker_move_effort(&mut self, delta: i32) {
        let Some(picker) = self.agent_picker.as_mut() else {
            return;
        };
        move_wrapped(
            &mut picker.effort_selected,
            delta,
            PRIMARY_EFFORT_OPTIONS.len(),
        );
    }

    pub fn agent_picker_toggle_start_new_session(&mut self) {
        if let Some(picker) = self.agent_picker.as_mut() {
            picker.start_new_session = !picker.start_new_session;
        }
    }

    pub fn agent_picker_advance(&mut self) -> bool {
        let Some(picker) = self.agent_picker.as_mut() else {
            return false;
        };
        picker.step = match picker.step {
            AgentPickerStep::Model => AgentPickerStep::Effort,
            AgentPickerStep::Effort => AgentPickerStep::ConfirmSave,
            AgentPickerStep::ConfirmSave | AgentPickerStep::StartNewSession => return false,
        };
        true
    }

    pub fn agent_picker_selection(&self) -> Option<(crate::roster::ResolvedAgent, Option<String>)> {
        let picker = self.agent_picker.as_ref()?;
        let role_index = picker.role_indices.get(picker.selected).copied()?;
        Some((
            self.ragnarok_models.get(role_index).cloned()?,
            primary_effort_value(picker.effort_selected).map(str::to_string),
        ))
    }

    pub fn agent_picker_back(&mut self) -> bool {
        let Some(picker) = self.agent_picker.as_mut() else {
            return false;
        };
        picker.step = match picker.step {
            AgentPickerStep::Model => return false,
            AgentPickerStep::Effort => AgentPickerStep::Model,
            AgentPickerStep::ConfirmSave => AgentPickerStep::Effort,
            AgentPickerStep::StartNewSession => return false,
        };
        true
    }

    /// Open `/mjconfig`, seeded from the same persisted config startup edits.
    pub fn open_mjconfig_menu(&mut self) {
        let mut config = self
            .config_path
            .as_deref()
            .and_then(|path| crate::config::Config::load(path).ok())
            .unwrap_or_default();
        config.theme = self.theme_kind;
        config.spinner = self.spinner_style;
        config.thought_output = self.thought_output;
        let notice = config.newer_build_notice();
        self.mjconfig_menu = Some(MjConfigMenu {
            editor: SettingsEditor::new(config, self.model_choices.clone(), notice)
                .with_active_models(self.active_models.clone())
                .with_active_session_config(self.session_config_options.clone())
                .with_inventory(self.acp_inventory.clone()),
            orig_theme: self.theme_kind,
            orig_spinner: self.spinner_style,
            orig_thought_output: self.thought_output,
        });
    }

    /// Apply a shared editor key and synchronize appearance preview.
    pub fn mjconfig_menu_key(&mut self, code: crossterm::event::KeyCode) -> SettingsAction {
        let Some(menu) = self.mjconfig_menu.as_mut() else {
            return SettingsAction::None;
        };
        let action = menu.editor.handle_key(code);
        let theme = menu.editor.config.theme;
        let spinner = menu.editor.config.spinner;
        let thought_output = menu.editor.config.thought_output;
        self.set_theme(theme);
        self.set_spinner_style(spinner);
        self.set_thought_output(thought_output);
        action
    }

    pub fn open_review_picker(&mut self) {
        self.review_picker = Some(ReviewPicker::default());
    }

    pub fn review_picker_move(&mut self, delta: i32) {
        let Some(picker) = self.review_picker.as_mut() else {
            return;
        };
        picker.selected = (picker.selected as i32 + delta).rem_euclid(3) as usize;
    }

    pub fn review_picker_accept(&mut self) -> Option<ReviewTarget> {
        let selected = self.review_picker.take()?.selected;
        Some(match selected {
            0 => ReviewTarget::Recent,
            1 => ReviewTarget::Uncommitted,
            _ => ReviewTarget::Head,
        })
    }

    /// Close the menu, keeping its live appearance preview.
    pub fn mjconfig_menu_accept(&mut self) -> Option<crate::config::Config> {
        self.mjconfig_menu.take().map(|menu| menu.editor.config)
    }

    /// Close the menu and restore the theme and spinner that were active when
    /// it opened, discarding the live preview.
    pub fn mjconfig_menu_cancel(&mut self) {
        if let Some(menu) = self.mjconfig_menu.take() {
            self.set_theme(menu.orig_theme);
            self.set_spinner_style(menu.orig_spinner);
            self.set_thought_output(menu.orig_thought_output);
        }
    }

    /// Stage a prompt to fire when the current turn completes.
    pub fn push_queued_prompt(&mut self, prompt: QueuedPrompt) {
        self.queued_prompts.push_back(prompt);
    }

    /// Drop all queued prompts, if any. Used when the runtime closes or a
    /// prompt failure makes automatic resubmission unsafe.
    pub fn clear_queued_prompts(&mut self) {
        self.queued_prompts.clear();
    }

    /// Iterate over queued prompts in send order.
    pub fn queued_prompts(&self) -> impl Iterator<Item = &QueuedPrompt> {
        self.queued_prompts.iter()
    }

    /// Number of queued user prompts waiting behind the active turn.
    pub fn queued_prompt_count(&self) -> usize {
        self.queued_prompts.len()
    }

    /// True when at least one user prompt is queued behind the active turn.
    pub fn has_queued_prompts(&self) -> bool {
        !self.queued_prompts.is_empty()
    }

    /// Pull the oldest queued prompt out for submission. Returns `None` if
    /// none is staged.
    pub fn take_queued_prompt(&mut self) -> Option<QueuedPrompt> {
        self.queued_prompts.pop_front()
    }

    /// Pull the newest queued prompt out for editing. Older prompts retain
    /// their FIFO order and continue waiting behind the active turn.
    pub fn take_latest_queued_prompt(&mut self) -> Option<QueuedPrompt> {
        self.queued_prompts.pop_back()
    }

    /// Remember the single command queued while the primary session starts.
    /// Returns `false` when an earlier Enter already staged the same startup
    /// slot, preventing duplicate runtime commands.
    pub fn stage_startup_prompt(&mut self, prompt: QueuedPrompt) -> bool {
        if self.startup_prompt.is_some() {
            return false;
        }
        self.startup_prompt = Some(prompt);
        true
    }

    pub fn has_startup_prompt(&self) -> bool {
        self.startup_prompt.is_some()
    }

    pub fn take_startup_prompt(&mut self) -> Option<QueuedPrompt> {
        self.startup_prompt.take()
    }

    /// Return a copy of the prompt history for persistence.
    pub fn prompt_history(&self) -> Vec<String> {
        self.prompt_history.clone()
    }

    /// Replace the in-memory prompt history (e.g. with entries loaded
    /// from disk at startup).
    pub fn set_prompt_history(&mut self, entries: Vec<String>) {
        self.prompt_history_resources = vec![Vec::new(); entries.len()];
        self.prompt_history = entries;
    }

    pub fn record_prompt_history(&mut self, text: String) {
        self.record_prompt_history_with_resources(text, Vec::new());
    }

    fn record_prompt_history_with_resources(
        &mut self,
        text: String,
        resources: Vec<PromptResource>,
    ) {
        // Deduplicate consecutive identical prompts, matching the normal
        // agent prompt path and shell-style history behavior.
        let duplicate = self.prompt_history.last().map(String::as_str) == Some(&text)
            && self.prompt_history_resources.last() == Some(&resources);
        if !duplicate {
            self.prompt_history.push(text);
            self.prompt_history_resources.push(resources);
        }
        self.reset_history_navigation();
    }

    fn restore_prompt_history_entry(&mut self, index: usize) {
        let mut input = self.prompt_history[index].clone();
        let resources = self
            .prompt_history_resources
            .get(index)
            .cloned()
            .unwrap_or_default();
        let mut attachments = Vec::with_capacity(resources.len());
        let mut search_start = 0usize;
        for resource in resources {
            let mention = file_mention_text(&resource.name);
            let mention_start = input
                .get(search_start..)
                .and_then(|suffix| suffix.find(&mention).map(|offset| search_start + offset));
            let position = if let Some(byte_start) = mention_start {
                let position = input[..byte_start].chars().count();
                input.replace_range(byte_start..byte_start + mention.len(), "");
                search_start = byte_start;
                position
            } else {
                if !input.is_empty() && !input.ends_with(char::is_whitespace) {
                    input.push(' ');
                }
                input.chars().count()
            };
            let id = self.next_attachment_id;
            self.next_attachment_id += 1;
            attachments.push(FileAttachment {
                id,
                position,
                display_path: resource.name.clone(),
                resource,
            });
        }
        self.input = input;
        self.file_attachments = attachments;
        self.input_cursor = self.input.chars().count();
    }

    /// Navigate to the previous (older) prompt in history. Returns `true`
    /// if the navigation moved (i.e. there is an older entry available).
    /// Saves the current input the first time in a navigation sequence.
    pub fn prompt_history_previous(&mut self) -> bool {
        if self.prompt_history.is_empty() {
            return false;
        }
        let new_cursor = match self.history_cursor {
            Some(i) => {
                if i == 0 {
                    return false; // already at the oldest
                }
                i - 1
            }
            None => self.prompt_history.len() - 1,
        };
        if self.history_cursor.is_none() {
            self.history_saved_input = self.input.clone();
            self.history_saved_file_attachments = self.file_attachments.clone();
        }
        self.history_cursor = Some(new_cursor);
        self.restore_prompt_history_entry(new_cursor);
        self.scroll_input_to_bottom();
        self.update_autocomplete();
        true
    }

    /// Navigate to the next (newer) prompt in history. Returns `true`
    /// if the navigation moved. When past the most recent entry, the
    /// saved input is restored and `history_cursor` is set to `None`.
    pub fn prompt_history_next(&mut self) -> bool {
        if self.prompt_history.is_empty() {
            return false;
        }
        match self.history_cursor {
            Some(i) => {
                if i + 1 >= self.prompt_history.len() {
                    // Past the end: restore saved input.
                    let saved = std::mem::take(&mut self.history_saved_input);
                    let saved_files = std::mem::take(&mut self.history_saved_file_attachments);
                    self.history_cursor = None;
                    self.input = saved;
                    self.file_attachments = saved_files;
                    self.input_cursor = self.input.chars().count();
                    self.scroll_input_to_bottom();
                    self.update_autocomplete();
                    true
                } else {
                    let new_cursor = i + 1;
                    self.history_cursor = Some(new_cursor);
                    self.restore_prompt_history_entry(new_cursor);
                    self.scroll_input_to_bottom();
                    self.update_autocomplete();
                    true
                }
            }
            None => false, // not currently navigating
        }
    }

    /// Reset any ongoing history navigation so the current text is
    /// treated as a new input. Called whenever the user edits the buffer.
    pub fn reset_history_navigation(&mut self) {
        self.history_cursor = None;
        self.history_saved_input.clear();
        self.history_saved_file_attachments.clear();
    }

    /// Monotonic counter that the UI uses as a cache key for the rendered
    /// transcript. Increases each time `transcript` or `tool_calls` mutate
    /// in a way that the renderer cares about.
    pub fn transcript_revision(&self) -> u64 {
        self.transcript_revision
    }

    /// Limit an active transcript entry to a source prefix for live rendering.
    /// This is deliberately transient: exports, history, and replay continue
    /// to read the complete entry.
    pub(crate) fn set_stream_visible_bytes(&mut self, entry_index: usize, bytes: usize) -> bool {
        let Some(text) = self.transcript_entry_text(entry_index) else {
            return false;
        };
        let bytes = bytes.min(text.len());
        if !text.is_char_boundary(bytes) {
            return false;
        }
        if self.stream_visible_bytes.get(&entry_index) == Some(&bytes) {
            return false;
        }
        self.stream_visible_bytes.insert(entry_index, bytes);
        self.bump_transcript_revision();
        true
    }

    /// Stop applying a live-render prefix once an entry is complete.
    pub(crate) fn clear_stream_visible_bytes(&mut self, entry_index: usize) -> bool {
        if self.stream_visible_bytes.remove(&entry_index).is_some() {
            self.bump_transcript_revision();
            true
        } else {
            false
        }
    }

    /// Release every transient live-render prefix before the UI state is
    /// detached from its reveal controller.
    pub(crate) fn clear_stream_visibility(&mut self) -> bool {
        if self.stream_visible_bytes.is_empty() {
            return false;
        }
        self.stream_visible_bytes.clear();
        self.bump_transcript_revision();
        true
    }

    pub(crate) fn stream_visible_text<'a>(&self, entry_index: usize, text: &'a str) -> &'a str {
        // A committed entry's scrollback rows are final: render the complete
        // text, never a pacing prefix that would be truncated forever (#615).
        if entry_index < self.committed_transcript_entries {
            return text;
        }
        self.stream_visible_bytes
            .get(&entry_index)
            .and_then(|bytes| text.get(..*bytes))
            .unwrap_or(text)
    }

    /// Entries below this boundary were force-committed to scrollback while
    /// their backing state could still change. Clamped so a stale value can
    /// never report more entries than the transcript holds.
    pub(crate) fn committed_transcript_entries(&self) -> usize {
        self.committed_transcript_entries.min(self.transcript.len())
    }

    /// Raise the committed boundary (monotonic; clamped to the transcript
    /// length). Drops live-render prefixes for the newly committed entries so
    /// they flush with their complete text in the same frame.
    pub(crate) fn force_commit_transcript_entries(&mut self, count: usize) -> bool {
        let count = count.min(self.transcript.len());
        if count <= self.committed_transcript_entries {
            return false;
        }
        self.committed_transcript_entries = count;
        self.stream_visible_bytes.retain(|&idx, _| idx >= count);
        self.bump_transcript_revision();
        true
    }

    fn transcript_entry_text(&self, entry_index: usize) -> Option<&str> {
        match self.transcript.get(entry_index)? {
            Entry::AgentMessage(text) | Entry::SubagentMessage(text) => Some(text),
            Entry::AgentThought(thought) | Entry::SubagentThought(thought) => Some(&thought.text),
            _ => None,
        }
    }

    fn bump_transcript_revision(&mut self) {
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
    }

    fn apply_known_terminal_outputs(&mut self) {
        let snapshots: Vec<TerminalOutputSnapshot> =
            self.terminal_outputs.values().cloned().collect();
        let mut changed = false;
        for snapshot in &snapshots {
            for view in self.tool_calls.values_mut() {
                changed |= view.apply_terminal_output(snapshot);
            }
        }
        if changed {
            self.bump_transcript_revision();
        }
    }

    /// Flip the global transcript-detail collapse setting. Bumps the transcript
    /// revision so the renderer rebuilds its cached `Vec<Line>` with the
    /// new line budget.
    pub fn toggle_expand_transcript_details(&mut self) {
        self.expand_transcript_details = !self.expand_transcript_details;
        self.tool_detail_overrides.clear();
        self.bump_transcript_revision();
    }

    /// Toggle one tool's details relative to the current renderer default.
    /// Returns `false` when the tool is unknown or has no output to display.
    pub fn toggle_tool_detail(&mut self, id: &str, default_expanded: bool) -> bool {
        if self
            .tool_calls
            .get(id)
            .is_none_or(|view| view.body.is_empty())
        {
            return false;
        }
        let expanded = !self
            .tool_detail_overrides
            .get(id)
            .copied()
            .unwrap_or(default_expanded);
        if expanded == default_expanded {
            self.tool_detail_overrides.remove(id);
        } else {
            self.tool_detail_overrides.insert(id.to_string(), expanded);
        }
        self.bump_transcript_revision();
        true
    }

    pub fn tool_detail_expanded(&self, id: &str) -> Option<bool> {
        self.tool_detail_overrides.get(id).copied()
    }

    /// Open the inline full-transcript reader. The reader starts pinned to
    /// the newest line (`scroll_offset` is reused as the top-visible line
    /// index and clamped to the last screen during draw).
    pub fn open_transcript_viewer(&mut self) {
        self.close_nested_agent_viewer();
        self.close_workspace_diff_viewer();
        self.close_terminals_viewer();
        self.transcript_viewer = true;
        self.scroll_offset = usize::MAX;
    }

    /// Close the inline full-transcript reader and reset its scroll position.
    pub fn close_transcript_viewer(&mut self) {
        self.transcript_viewer = false;
        self.transcript_search = None;
        self.scroll_offset = 0;
    }

    pub fn open_nested_agent_viewer(&mut self) -> bool {
        // Opening the viewer should land on work the user can act on, not the
        // oldest completed actor retained for session history.
        let selected = self.nested_agent_viewer_ids().into_iter().next();
        let Some(selected) = selected else {
            return false;
        };
        self.close_transcript_viewer();
        self.close_workspace_diff_viewer();
        self.close_review_issue_viewer();
        self.close_terminals_viewer();
        self.nested_agent_viewer = true;
        self.nested_agent_selected = Some(selected);
        self.nested_agent_scroll_offset = usize::MAX;
        true
    }

    pub fn close_nested_agent_viewer(&mut self) {
        self.nested_agent_viewer = false;
        self.nested_agent_scroll_offset = 0;
    }

    /// Record any terminals a tool call owns. Called whenever a tool call is
    /// created or updated, so a terminal is registered the moment the agent
    /// starts it rather than when (or if) it ever exits.
    fn register_terminals_for_tool_call(&mut self, tool_call_id: &str) {
        let Some(view) = self.tool_calls.get(tool_call_id) else {
            return;
        };
        let label = view.title.clone();
        let ids = view
            .body
            .iter()
            .filter_map(|output| match output {
                ToolCallOutput::Terminal { terminal_id, .. } => Some(terminal_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        for terminal_id in ids {
            match self
                .terminal_registry
                .iter_mut()
                .find(|record| record.terminal_id == terminal_id)
            {
                // A tool call's title can sharpen after the first update
                // (generic "tool" becoming the real command), so keep it fresh.
                Some(record) => record.label = label.clone(),
                None => self.terminal_registry.push(TerminalRegistration {
                    terminal_id,
                    tool_call_id: tool_call_id.to_string(),
                    label: label.clone(),
                }),
            }
        }
    }

    /// Every registered terminal, running ones first, each in the order the
    /// agent started it. Terminals whose state has been dropped (an offloaded
    /// nested actor) are omitted rather than shown empty.
    pub fn terminal_summaries(&self) -> Vec<TerminalSummary> {
        self.ordered_terminal_records()
            .into_iter()
            .filter_map(|record| {
                let (_, truncated, exit_status) = self.terminal_state(record)?;
                Some(TerminalSummary {
                    label: record.label.clone(),
                    truncated,
                    exit_status: exit_status.cloned(),
                })
            })
            .collect()
    }

    /// Registered terminals in display order — running first, each otherwise in
    /// the order the agent started it. Borrows throughout: terminal output can
    /// reach a megabyte apiece, and this ordering is recomputed on every frame
    /// the affordance row is measured.
    fn ordered_terminal_records(&self) -> Vec<&TerminalRegistration> {
        let mut records = self
            .terminal_registry
            .iter()
            .filter(|record| self.terminal_state(record).is_some())
            .collect::<Vec<_>>();
        records.sort_by_key(|record| {
            self.terminal_state(record)
                .is_none_or(|(_, _, exit_status)| exit_status.is_some())
        });
        records
    }

    /// Output of the terminal at `index` in display order, borrowed rather than
    /// cloned so the viewer can render a large buffer without copying it.
    pub fn terminal_output_at(&self, index: usize) -> Option<&str> {
        let record = *self.ordered_terminal_records().get(index)?;
        self.terminal_state(record).map(|(output, _, _)| output)
    }

    /// Resolve a terminal from the tool call that owns it, falling back to the
    /// raw snapshot. The tool call view is preferred because it is kept in
    /// sync by `apply_terminal_output` and carries the same fields.
    fn terminal_state(
        &self,
        record: &TerminalRegistration,
    ) -> Option<(&str, bool, Option<&TerminalExitStatus>)> {
        let from_view = self.tool_calls.get(&record.tool_call_id).and_then(|view| {
            view.body.iter().find_map(|output| match output {
                ToolCallOutput::Terminal {
                    terminal_id,
                    output,
                    truncated,
                    exit_status,
                } if terminal_id == &record.terminal_id => {
                    Some((output.as_str(), *truncated, exit_status.as_ref()))
                }
                _ => None,
            })
        });
        from_view.or_else(|| {
            let snapshot = self.terminal_outputs.get(&record.terminal_id)?;
            Some((
                snapshot.output.as_str(),
                snapshot.truncated,
                snapshot.exit_status.as_ref(),
            ))
        })
    }

    /// Terminals still running. This is what the status affordance counts, so
    /// it must not include ones that have already exited. Counts without
    /// materialising any output, because it runs on every frame.
    pub fn running_terminal_count(&self) -> usize {
        self.terminal_registry
            .iter()
            .filter(|record| {
                self.terminal_state(record)
                    .is_some_and(|(_, _, exit_status)| exit_status.is_none())
            })
            .count()
    }

    /// Label of the first running terminal, for the affordance row.
    pub fn first_running_terminal_label(&self) -> Option<&str> {
        self.terminal_registry
            .iter()
            .find(|record| {
                self.terminal_state(record)
                    .is_some_and(|(_, _, exit_status)| exit_status.is_none())
            })
            .map(|record| record.label.as_str())
    }

    pub fn terminal_count(&self) -> usize {
        self.ordered_terminal_records().len()
    }

    /// Open the terminal reader. Returns `false` when there is nothing to show
    /// so the caller can explain that instead of opening an empty pane.
    pub fn open_terminals_viewer(&mut self) -> bool {
        if self.terminal_count() == 0 {
            return false;
        }
        self.close_transcript_viewer();
        self.close_nested_agent_viewer();
        self.close_workspace_diff_viewer();
        self.close_review_issue_viewer();
        self.terminals_viewer = true;
        self.terminals_selected = 0;
        // Running terminals are tailed, so start pinned to the newest output.
        self.terminals_scroll_offset = usize::MAX;
        true
    }

    pub fn close_terminals_viewer(&mut self) {
        self.terminals_viewer = false;
        self.terminals_selected = 0;
        self.terminals_scroll_offset = 0;
    }

    pub fn select_terminal(&mut self, next: bool) {
        let count = self.terminal_count();
        if count == 0 {
            self.terminals_selected = 0;
            self.terminals_scroll_offset = 0;
            return;
        }
        let current = self.terminals_selected.min(count - 1);
        self.terminals_selected = if next {
            (current + 1) % count
        } else if current == 0 {
            count - 1
        } else {
            current - 1
        };
        self.terminals_scroll_offset = usize::MAX;
    }

    pub fn open_review_issue_viewer(&mut self) {
        self.close_nested_agent_viewer();
        self.close_transcript_viewer();
        self.close_workspace_diff_viewer();
        self.close_terminals_viewer();
        self.review_issue_viewer = true;
        self.review_issue_scroll_offset = 0;
    }

    pub fn close_review_issue_viewer(&mut self) {
        self.review_issue_viewer = false;
        self.review_issue_scroll_offset = 0;
    }

    pub fn select_nested_agent(&mut self, next: bool) {
        let ids = self.nested_agent_viewer_ids();
        if ids.is_empty() {
            self.nested_agent_selected = None;
            self.nested_agent_scroll_offset = 0;
            return;
        }
        let current = self
            .nested_agent_selected
            .and_then(|selected| ids.iter().position(|id| *id == selected))
            .unwrap_or(0);
        let selected = if next {
            (current + 1) % ids.len()
        } else if current == 0 {
            ids.len() - 1
        } else {
            current - 1
        };
        self.nested_agent_selected = Some(ids[selected]);
        self.nested_agent_scroll_offset = usize::MAX;
    }

    pub fn nested_agents(&self) -> impl Iterator<Item = (u64, &SubagentStatus)> {
        self.subagents.iter().map(|(id, state)| (*id, state))
    }

    pub fn nested_agent_viewer_ids(&self) -> Vec<u64> {
        let mut ids = self.subagents.keys().copied().collect::<Vec<_>>();
        ids.sort_by(|left_id, right_id| {
            let left = &self.subagents[left_id];
            let right = &self.subagents[right_id];
            left.finished
                .is_some()
                .cmp(&right.finished.is_some())
                .then_with(|| right_id.cmp(left_id))
        });
        ids.truncate(NESTED_AGENT_VIEWER_LIMIT);
        ids
    }

    pub fn nested_agent(&self, id: u64) -> Option<&SubagentStatus> {
        self.subagents.get(&id)
    }

    pub fn selected_nested_agent(&self) -> Option<(u64, &SubagentStatus)> {
        let id = self.nested_agent_selected?;
        self.nested_agent(id).map(|state| (id, state))
    }

    /// Open the workspace-diff reader. It always starts at the first retained
    /// file and top of that file, including when there are no diffs. The
    /// caller is responsible for requesting the refresh this marks pending.
    pub fn open_workspace_diff_viewer(&mut self) {
        self.close_nested_agent_viewer();
        self.close_transcript_viewer();
        self.close_review_issue_viewer();
        self.close_terminals_viewer();
        self.workspace_diff_viewer = true;
        self.workspace_diff_selected_file = 0;
        self.workspace_diff_scroll_offset = 0;
        self.workspace_diff_loading = true;
    }

    /// Mark an explicit refresh pending. Selection survives so re-reading after
    /// an edit keeps the file the user was looking at; the renderer clamps it
    /// if that file is no longer part of the diff.
    pub fn begin_workspace_diff_refresh(&mut self) {
        self.workspace_diff_loading = true;
    }

    /// Close the workspace-diff reader and discard its ephemeral navigation.
    /// The last result is kept: reopening shows it immediately while the
    /// refresh runs, which beats flashing an empty reader.
    pub fn close_workspace_diff_viewer(&mut self) {
        self.workspace_diff_viewer = false;
        self.workspace_diff_selected_file = 0;
        self.workspace_diff_scroll_offset = 0;
        self.workspace_diff_loading = false;
    }

    pub fn workspace_diff_file_count(&self) -> usize {
        self.workspace_head_diff
            .as_ref()
            .map_or(0, |event| event.diffs.len())
    }

    /// Move among retained files, clamping at either end. A file change resets
    /// line scrolling so selection can never retain another file's offset.
    pub fn select_workspace_diff_file(&mut self, next: bool) {
        let count = self.workspace_diff_file_count();
        if count == 0 {
            self.workspace_diff_selected_file = 0;
        } else if next {
            self.workspace_diff_selected_file = self
                .workspace_diff_selected_file
                .saturating_add(1)
                .min(count - 1);
        } else {
            self.workspace_diff_selected_file = self.workspace_diff_selected_file.saturating_sub(1);
        }
        self.workspace_diff_scroll_offset = 0;
    }

    /// Extract the text of the most recent agent message from the transcript.
    /// Returns None if no agent message exists yet.
    pub fn last_agent_message(&self) -> Option<String> {
        self.transcript.iter().rev().find_map(|entry| match entry {
            Entry::AgentMessage(text) => Some(text.clone()),
            Entry::UserPrompt(_)
            | Entry::AgentThought(_)
            | Entry::SubagentMessage(_)
            | Entry::SubagentThought(_)
            | Entry::ToolCall(_)
            | Entry::SubagentToolCall(_)
            | Entry::Plan(_)
            | Entry::SubagentPlan(_)
            | Entry::InternalMessage(_)
            | Entry::System(_)
            | Entry::FeatureHint(_)
            | Entry::ReviewLedger(_)
            | Entry::SessionBoundary(_) => None,
        })
    }

    /// Reset the prompt box to follow the newest line.
    pub fn scroll_input_to_bottom(&mut self) {
        self.input_scroll_offset = 0;
    }

    /// True while a prompt turn is in flight (i.e. we are waiting for or
    /// finishing the agent's response). Single source of truth for input
    /// gating, Ctrl-C handling, autocomplete visibility, and cursor
    /// placement — derived from `connection_state` so the turn-in-flight
    /// signal cannot drift from the lifecycle enum.
    pub fn is_streaming(&self) -> bool {
        matches!(
            self.connection_state,
            ConnectionState::Streaming | ConnectionState::Cancelling
        )
    }

    pub fn is_busy(&self) -> bool {
        matches!(
            self.connection_state,
            ConnectionState::Streaming | ConnectionState::Cancelling | ConnectionState::Forking
        )
    }

    pub fn active_turn_elapsed(&self) -> Option<Duration> {
        if self.is_busy() {
            self.turn_started_at.map(|started| started.elapsed())
        } else {
            None
        }
    }

    pub fn last_turn_elapsed(&self) -> Option<Duration> {
        self.last_turn_elapsed
    }

    /// Whether the locally submitted prompt at `prompt_index` has received a
    /// terminal prompt result. Replayed transcript prompts intentionally have
    /// no such lifecycle fact and are therefore not considered complete.
    pub fn prompt_turn_completed(&self, prompt_index: usize) -> bool {
        self.prompt_turns
            .iter()
            .find(|turn| turn.prompt_index == prompt_index)
            .is_some_and(|turn| turn.completed)
    }

    /// Whether `prompt_index` belongs to a locally submitted prompt whose
    /// lifecycle is tracked by this UI instance. Replayed prompts deliberately
    /// have no record here.
    pub fn has_prompt_turn(&self, prompt_index: usize) -> bool {
        self.prompt_turns
            .iter()
            .any(|turn| turn.prompt_index == prompt_index)
    }

    /// Recorded elapsed time for a completed locally submitted prompt turn.
    pub fn prompt_turn_elapsed(&self, prompt_index: usize) -> Option<Duration> {
        self.prompt_turns
            .iter()
            .find(|turn| turn.prompt_index == prompt_index)
            .and_then(|turn| turn.elapsed)
    }

    pub fn connection_state_elapsed(&self) -> Duration {
        self.connection_state_started_at.elapsed()
    }

    pub fn connection_state(&self) -> ConnectionState {
        self.connection_state
    }

    /// Sanitize an agent-supplied session title (stripping control characters
    /// and collapsing whitespace) and store it. Returns `true` when a
    /// non-empty title was set; empty/whitespace-only titles are ignored so
    /// "no title" stays representable and a single guard lives here rather
    /// than at every call site.
    pub fn set_session_title(&mut self, raw: &str) -> bool {
        let sanitized = crate::notifications::sanitize_message(raw);
        if sanitized.is_empty() {
            return false;
        }
        self.session_title = Some(sanitized);
        true
    }

    pub(crate) fn set_connection_state(&mut self, state: ConnectionState) {
        if self.connection_state != state {
            self.connection_state = state;
            self.connection_state_started_at = Instant::now();
            self.keep_awake.set_active(self.is_busy());
        }
    }

    pub fn set_primary_acp_name(&mut self, name: impl Into<String>) {
        self.primary_acp_name = name.into();
    }

    pub fn announce_waiting_for_primary(&mut self) {
        self.set_status_line(StatusKind::Info, "session is still starting");
    }

    fn set_status_line(&mut self, kind: StatusKind, text: impl Into<String>) {
        let text = text.into();
        self.status_line = Some(match kind {
            StatusKind::Info => StatusMessage::info(text),
            StatusKind::Warning => StatusMessage::warning(text),
            StatusKind::Fatal => StatusMessage::fatal(text),
        });
    }

    pub fn push_system_message(&mut self, text: impl Into<String>) {
        self.transcript.push(Entry::System(text.into()));
        self.bump_transcript_revision();
    }

    pub fn push_review_ledger(&mut self, lines: Vec<ReviewLedgerLine>) {
        if lines.is_empty() {
            return;
        }
        self.transcript.push(Entry::ReviewLedger(lines));
        self.bump_transcript_revision();
    }

    pub fn push_session_boundary(&mut self, text: impl Into<String>) {
        self.finalize_message(EntryKind::Agent);
        self.transcript.push(Entry::SessionBoundary(text.into()));
        self.bump_transcript_revision();
    }

    pub fn record_status_message(&mut self, kind: StatusKind, text: impl Into<String>) {
        let text = text.into();
        let transcript_text = status_transcript_text(kind, &text);
        self.set_status_line(kind, text.clone());
        if matches!(self.transcript.last(), Some(Entry::System(existing)) if existing == &transcript_text)
        {
            return;
        }
        self.push_system_message(transcript_text);
    }

    /// Record the next eligible local feature hint after a quiet run of turns.
    /// The hint is transcript-only UI state and never becomes ACP history.
    pub fn maybe_record_feature_hint(&mut self, capabilities: FeatureHintCapabilities) -> bool {
        if !self.feature_hints_enabled || self.is_side {
            return false;
        }
        self.completed_turns_since_hint += 1;
        if self.completed_turns_since_hint < FEATURE_HINT_INTERVAL_TURNS {
            return false;
        }

        for offset in 0..FEATURE_HINTS.len() {
            let index = (self.feature_hint_cursor + offset) % FEATURE_HINTS.len();
            let hint = FEATURE_HINTS[index];
            let eligible = match hint.requirement {
                FeatureHintRequirement::Always => true,
                FeatureHintRequirement::Subagents => capabilities.subagents,
                FeatureHintRequirement::Ragnarok => capabilities.ragnarok,
                FeatureHintRequirement::Voice => capabilities.voice,
                FeatureHintRequirement::Fork => capabilities.fork,
                FeatureHintRequirement::Side => capabilities.side,
                FeatureHintRequirement::Images => capabilities.images,
                FeatureHintRequirement::Fullscreen => capabilities.fullscreen,
                FeatureHintRequirement::Inline => !capabilities.fullscreen,
            };
            if eligible {
                self.transcript
                    .push(Entry::FeatureHint(hint.text.to_string()));
                self.feature_hint_cursor = (index + 1) % FEATURE_HINTS.len();
                self.completed_turns_since_hint = 0;
                self.bump_transcript_revision();
                return true;
            }
        }
        false
    }

    /// Mark the runtime as closed and switch the UI into read-only mode.
    pub fn mark_runtime_closed(&mut self) {
        self.runtime_closed = true;
        self.finish_turn_timer();
        self.cancel_all_pending_permissions();
        self.cancel_all_pending_elicitations();
        self.agent_picker = None;
        self.team_picker = None;
        self.config_picker = None;
        self.autocomplete = Autocomplete::default();
        self.clear_queued_prompts();
        // Preserve Fatal: a fatal event always supersedes a clean close,
        // since the channel-drop that triggers this method follows the
        // Fatal event by design.
        if self.connection_state != ConnectionState::Fatal {
            self.set_connection_state(ConnectionState::Closed);
        }

        let is_fatal = matches!(
            self.status_line,
            Some(StatusMessage {
                kind: StatusKind::Fatal,
                ..
            })
        );
        if !is_fatal {
            self.record_status_message(
                StatusKind::Info,
                "acp runtime closed; press Ctrl-C to quit",
            );
        }
    }

    /// Note that the user has requested cancellation of the in-flight
    /// prompt. Idempotent and only meaningful while `Streaming`.
    pub fn mark_cancelling(&mut self) {
        if self.connection_state == ConnectionState::Streaming {
            self.set_connection_state(ConnectionState::Cancelling);
        }
    }

    pub fn mark_forking(&mut self) {
        self.set_connection_state(ConnectionState::Forking);
        self.turn_started_at = Some(Instant::now());
        self.last_turn_elapsed = None;
        self.active_prompt_turn = None;
        self.autocomplete = Autocomplete::default();
    }

    /// The permission prompt the UI should currently render, if any.
    pub fn pending_permission(&self) -> Option<&PendingPermission> {
        self.permission_queue.front()
    }

    /// Mutable accessor for the front prompt (used to move the option
    /// cursor without removing it from the queue).
    pub fn pending_permission_mut(&mut self) -> Option<&mut PendingPermission> {
        self.permission_queue.front_mut()
    }

    /// True when there is at least one queued permission prompt.
    pub fn has_pending_permission(&self) -> bool {
        !self.permission_queue.is_empty()
    }

    /// Number of prompts queued, including the one currently displayed.
    pub fn pending_permission_count(&self) -> usize {
        self.permission_queue.len()
    }

    /// Pop the currently-displayed prompt off the front of the queue.
    /// The caller is responsible for sending a decision through the
    /// `prompt.responder` before dropping it.
    pub fn take_pending_permission(&mut self) -> Option<PendingPermission> {
        self.permission_queue.pop_front()
    }

    /// Drain every queued permission prompt and send `Cancelled` through
    /// each responder. Used during fatal shutdown / runtime close.
    ///
    /// Note: the agent doesn't observe a difference between this and
    /// dropping the senders -- by the time we reach this method the ACP
    /// transport has typically already closed, and the receiver side maps
    /// both `Ok(Cancelled)` and `Err(RecvError)` to the same outcome. The
    /// explicit send is for code-clarity at the call site (intentional
    /// cancel vs. accidental drop), not for any wire-level guarantee.
    pub fn cancel_all_pending_permissions(&mut self) {
        while let Some(pending) = self.permission_queue.pop_front() {
            let _ = pending.prompt.responder.send(PermissionDecision::Cancelled);
        }
    }

    /// Resolve a queued permission prompt with a decision made in the
    /// remote-control viewer. Matches by tool-call id and only consumes
    /// the prompt when the option exists on it, so a stale decision for an
    /// already-answered request is dropped instead of cancelling an
    /// unrelated prompt. Returns true when a prompt was resolved.
    pub fn resolve_permission_remotely(&mut self, request_id: &str, option_id: &str) -> bool {
        let Some(index) = self.permission_queue.iter().position(|pending| {
            pending.prompt.tool_call.tool_call_id.to_string() == request_id
                && pending
                    .prompt
                    .options
                    .iter()
                    .any(|option| option.option_id.to_string() == option_id)
        }) else {
            return false;
        };
        let pending = self
            .permission_queue
            .remove(index)
            .expect("position returned a valid index");
        let _ = pending
            .prompt
            .responder
            .send(PermissionDecision::Selected(option_id.to_string()));
        self.record_status_message(
            StatusKind::Info,
            "permission request answered from the remote viewer".to_string(),
        );
        self.update_autocomplete();
        true
    }

    /// Resolve a queued elicitation (a question menu, `/setup` form, or sign-in
    /// URL) with a decision made in the remote-control viewer. Matches on the
    /// id the remote tracker stamped when it published the prompt, because an
    /// elicitation carries no intrinsic id of its own. The decision must also
    /// validate against this prompt's schema, so a stale decision for an
    /// already-answered request is dropped instead of resolving whatever
    /// happens to be queued now. Returns true when a prompt was resolved.
    pub fn resolve_elicitation_remotely(&mut self, request_id: &str, option_id: &str) -> bool {
        let Some(index) = self.elicitation_queue.iter().position(|pending| {
            pending.prompt.remote_id.as_deref() == Some(request_id)
                && crate::session_state::remote_elicitation_outcome(&pending.prompt, option_id)
                    .is_some()
        }) else {
            return false;
        };
        let pending = self
            .elicitation_queue
            .remove(index)
            .expect("position returned a valid index");
        let Some(outcome) =
            crate::session_state::remote_elicitation_outcome(&pending.prompt, option_id)
        else {
            return false;
        };
        let _ = pending.prompt.responder.send(outcome);
        self.record_status_message(
            StatusKind::Info,
            "question answered from the remote viewer".to_string(),
        );
        self.update_autocomplete();
        true
    }

    /// The elicitation prompt the UI should currently render, if any.
    pub fn pending_elicitation(&self) -> Option<&PendingElicitation> {
        self.elicitation_queue.front()
    }

    /// Mutable accessor for the front elicitation while editing its field.
    pub fn pending_elicitation_mut(&mut self) -> Option<&mut PendingElicitation> {
        self.elicitation_queue.front_mut()
    }

    /// True when there is at least one queued elicitation prompt.
    pub fn has_pending_elicitation(&self) -> bool {
        !self.elicitation_queue.is_empty()
    }

    /// Number of elicitation prompts queued, including the displayed one.
    pub fn pending_elicitation_count(&self) -> usize {
        self.elicitation_queue.len()
    }

    /// The renderable/resolvable view for the front elicitation, if any.
    pub fn elicitation_view(&self) -> Option<ElicitationView> {
        self.pending_elicitation()
            .map(|pending| classify_elicitation(&pending.prompt))
    }

    pub fn elicitation_accepts_text_input(&self) -> bool {
        match self.elicitation_view() {
            Some(ElicitationView::Text { .. }) => true,
            Some(ElicitationView::Form { fields, .. }) => self
                .pending_elicitation()
                .and_then(|pending| fields.get(pending.form_field))
                .is_some_and(|field| {
                    matches!(
                        field.kind,
                        ElicitationFormFieldKind::Text
                            | ElicitationFormFieldKind::Number { .. }
                            | ElicitationFormFieldKind::Integer { .. }
                    )
                }),
            _ => false,
        }
    }

    /// Pop the front elicitation off the queue. The caller must answer the
    /// responder before dropping it (a drop maps to Cancel on the runtime side).
    fn take_pending_elicitation(&mut self) -> Option<PendingElicitation> {
        self.elicitation_queue.pop_front()
    }

    /// Drain every queued elicitation and send `Cancel` through each
    /// responder. Used during fatal shutdown / runtime close, mirroring
    /// `cancel_all_pending_permissions`.
    pub fn cancel_all_pending_elicitations(&mut self) {
        while let Some(pending) = self.elicitation_queue.pop_front() {
            let _ = pending.prompt.responder.send(ElicitationOutcome::Cancel);
        }
    }

    /// Move the active select cursor by `delta`, wrapping within its options.
    pub fn elicitation_select_move(&mut self, delta: i32) {
        let len = match self.elicitation_view() {
            Some(ElicitationView::SingleSelect { options, .. }) => options.len(),
            Some(ElicitationView::Form { fields, .. }) => {
                let field_index = self
                    .pending_elicitation()
                    .map_or(0, |pending| pending.form_field);
                match fields.get(field_index).map(|field| &field.kind) {
                    Some(ElicitationFormFieldKind::SingleSelect { options })
                    | Some(ElicitationFormFieldKind::MultiSelect { options, .. }) => options.len(),
                    Some(ElicitationFormFieldKind::Boolean) => 2,
                    _ => return,
                }
            }
            _ => return,
        };
        if len == 0 {
            return;
        }
        if let Some(pending) = self.elicitation_queue.front_mut() {
            pending.selected = pending.selected.min(len - 1);
            move_wrapped(&mut pending.selected, delta, len);
            // Resume auto-scroll so the newly selected option stays visible.
            pending.scroll_offset = None;
        }
    }

    /// Toggle the option under the cursor for the active multi-select field.
    pub fn elicitation_multi_toggle(&mut self) {
        let Some(ElicitationView::Form { fields, .. }) = self.elicitation_view() else {
            return;
        };
        let Some(pending) = self.elicitation_queue.front_mut() else {
            return;
        };
        let Some(ElicitationFormFieldKind::MultiSelect {
            options, max_items, ..
        }) = fields.get(pending.form_field).map(|field| &field.kind)
        else {
            return;
        };
        if options.is_empty() {
            return;
        }
        let selected = pending.selected.min(options.len() - 1);
        if !pending.multi_selected.remove(&selected)
            && max_items.is_none_or(|max| pending.multi_selected.len() < max as usize)
        {
            pending.multi_selected.insert(selected);
        }
        pending.scroll_offset = None;
    }

    /// Resolve the front elicitation as an Accept (Enter). The content map is
    /// built from the view. Multi-property forms advance one field at a time
    /// and send their accumulated content after the final field.
    pub fn resolve_elicitation_accept(&mut self) {
        if let Some(ElicitationView::Form { fields, .. }) = self.elicitation_view() {
            let Some(pending) = self.elicitation_queue.front_mut() else {
                return;
            };
            let Some(field) = fields.get(pending.form_field) else {
                return;
            };
            let value = match &field.kind {
                ElicitationFormFieldKind::SingleSelect { options } => {
                    let Some(option) = options.get(pending.selected.min(options.len() - 1)) else {
                        return;
                    };
                    Some(ElicitationContentValue::String(option.value.clone()))
                }
                ElicitationFormFieldKind::MultiSelect {
                    options,
                    min_items,
                    max_items,
                } => {
                    let count = pending.multi_selected.len() as u64;
                    if min_items.is_some_and(|min| count < min)
                        || max_items.is_some_and(|max| count > max)
                        || (field.required && count == 0)
                    {
                        return;
                    }
                    let values = options
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| pending.multi_selected.contains(index))
                        .map(|(_, option)| option.value.clone())
                        .collect::<Vec<_>>();
                    (!values.is_empty()).then_some(ElicitationContentValue::StringArray(values))
                }
                ElicitationFormFieldKind::Text => {
                    let value = pending.input.trim();
                    if value.is_empty() {
                        if field.required {
                            return;
                        }
                        None
                    } else {
                        Some(ElicitationContentValue::String(value.to_string()))
                    }
                }
                ElicitationFormFieldKind::Number { minimum, maximum } => {
                    let value = pending.input.trim();
                    if value.is_empty() && !field.required {
                        None
                    } else {
                        let Ok(value) = value.parse::<f64>() else {
                            return;
                        };
                        if !value.is_finite()
                            || minimum.is_some_and(|minimum| value < minimum)
                            || maximum.is_some_and(|maximum| value > maximum)
                        {
                            return;
                        }
                        Some(ElicitationContentValue::Number(value))
                    }
                }
                ElicitationFormFieldKind::Integer { minimum, maximum } => {
                    let value = pending.input.trim();
                    if value.is_empty() && !field.required {
                        None
                    } else {
                        let Ok(value) = value.parse::<i64>() else {
                            return;
                        };
                        if minimum.is_some_and(|minimum| value < minimum)
                            || maximum.is_some_and(|maximum| value > maximum)
                        {
                            return;
                        }
                        Some(ElicitationContentValue::Integer(value))
                    }
                }
                ElicitationFormFieldKind::Boolean => {
                    Some(ElicitationContentValue::Boolean(pending.selected == 1))
                }
            };
            if let Some(value) = value {
                pending
                    .form_content
                    .insert(field.property_name.clone(), value);
            }
            if pending.form_field + 1 < fields.len() {
                pending.form_field += 1;
                pending.selected = 0;
                pending.input.clear();
                pending.multi_selected.clear();
                pending.scroll_offset = None;
                return;
            }
            let pending = self
                .take_pending_elicitation()
                .expect("the form remained queued while resolving its final field");
            let _ = pending
                .prompt
                .responder
                .send(ElicitationOutcome::Accept(pending.form_content));
            self.update_autocomplete();
            return;
        }
        let Some(pending) = self.take_pending_elicitation() else {
            return;
        };
        let outcome = match classify_elicitation(&pending.prompt) {
            ElicitationView::SingleSelect {
                property_name,
                options,
                ..
            } => {
                // `options` is non-empty (classify guarantees it); clamp the
                // cursor defensively before indexing.
                let index = pending.selected.min(options.len().saturating_sub(1));
                match options.get(index) {
                    Some(option) => {
                        let mut content = BTreeMap::new();
                        content.insert(
                            property_name,
                            ElicitationContentValue::String(option.value.clone()),
                        );
                        ElicitationOutcome::Accept(content)
                    }
                    None => ElicitationOutcome::Cancel,
                }
            }
            ElicitationView::Url { .. } => ElicitationOutcome::Accept(BTreeMap::new()),
            ElicitationView::Text { property_name, .. } => {
                let value = pending.input.trim();
                // An empty submission is a no-op skip rather than writing a
                // blank value the agent would reject.
                if value.is_empty() {
                    ElicitationOutcome::Cancel
                } else {
                    let mut content = BTreeMap::new();
                    content.insert(
                        property_name,
                        ElicitationContentValue::String(value.to_string()),
                    );
                    ElicitationOutcome::Accept(content)
                }
            }
            ElicitationView::Form { .. } => unreachable!("handled before removing the form"),
            ElicitationView::Unsupported => ElicitationOutcome::Decline,
        };
        let _ = pending.prompt.responder.send(outcome);
        self.update_autocomplete();
    }

    /// Resolve the front elicitation as a dismiss (Esc). Supported views send
    /// Cancel; the unsupported-shape info modal sends Decline.
    pub fn resolve_elicitation_dismiss(&mut self) {
        let Some(pending) = self.take_pending_elicitation() else {
            return;
        };
        let outcome = match classify_elicitation(&pending.prompt) {
            ElicitationView::Unsupported => ElicitationOutcome::Decline,
            _ => ElicitationOutcome::Cancel,
        };
        let _ = pending.prompt.responder.send(outcome);
        self.update_autocomplete();
    }

    /// Push a user prompt into the transcript immediately, before the
    /// command reaches the runtime. Keeps the UI responsive.
    pub fn record_user_prompt(&mut self, text: String) {
        self.record_user_prompt_with_resources(text, Vec::new());
    }

    pub fn record_user_prompt_with_resources(
        &mut self,
        text: String,
        resources: Vec<PromptResource>,
    ) {
        self.workflow_clocks.retain(|workflow_id, _| {
            self.workflows
                .get(*workflow_id)
                .is_some_and(|workflow| workflow.outcome.is_none())
        });
        self.pending_workspace_diff_total = None;
        self.agent_open_message_index = None;
        // The agent names sessions with an asynchronous summarization call
        // that can land well after the first exchange; until then the header
        // shows no title and the pickers fall back to the bare session id.
        // Seed a provisional title from the first prompt — the agent's
        // `SessionInfoUpdate` overwrites it whenever it arrives.
        if self.session_title.is_none() {
            let provisional = crate::text::truncate_text_to_width(
                crate::notifications::sanitize_message(&text),
                PROVISIONAL_TITLE_WIDTH,
            );
            self.set_session_title(&provisional);
        }
        let prompt_index = self.transcript.len();
        self.transcript.push(Entry::UserPrompt(text.clone()));
        self.prompt_turns.push(PromptTurn {
            prompt_index,
            elapsed: None,
            completed: false,
        });
        self.active_prompt_turn = Some(prompt_index);
        self.record_prompt_history_with_resources(text, resources);
        self.bump_transcript_revision();
        self.set_connection_state(ConnectionState::Streaming);
        self.turn_started_at = Some(Instant::now());
        self.last_turn_elapsed = None;
        self.input_cursor = 0;
        self.scroll_offset = 0;
        // Sending the prompt clears the input; tear down any open
        // autocomplete popover so it doesn't linger over an empty buffer.
        self.autocomplete = Autocomplete::default();
    }

    /// Open the value picker for one config option. Returns `true` if it
    /// became visible.
    #[cfg(test)]
    pub fn open_config_value_picker(&mut self, option_index: usize) -> bool {
        if self.runtime_closed {
            return false;
        }
        let Some(option) = self.session_config_options.get(option_index) else {
            return false;
        };
        let Some(choices) = config_option_choices(option) else {
            return false;
        };
        if choices.is_empty() {
            self.record_status_message(
                StatusKind::Warning,
                format!("config option '{}' has no values", option.name),
            );
            return false;
        }
        let current = config_option_current_value_id(option)
            .and_then(|value| choices.iter().position(|choice| &choice.value == value))
            .unwrap_or(0);
        let all_indices: Vec<usize> = (0..choices.len()).collect();
        self.config_picker = Some(ConfigPicker {
            selected_option: option_index,
            selected_value: current,
            search_query: String::new(),
            filtered_indices: all_indices,
        });
        self.autocomplete = Autocomplete::default();
        true
    }

    /// Close the config picker overlay and restore autocomplete if needed.
    pub fn dismiss_config_picker(&mut self) {
        self.config_picker = None;
        if self.runtime_closed {
            self.autocomplete = Autocomplete::default();
        } else {
            self.update_autocomplete();
        }
    }

    /// Move the config picker cursor by `delta`, wrapping within the
    /// current option's filtered value list.
    pub fn config_picker_move(&mut self, delta: i32) {
        let Some(picker) = self.config_picker.as_mut() else {
            return;
        };
        let len = picker.filtered_indices.len();
        if len == 0 {
            return;
        }
        move_wrapped(&mut picker.selected_value, delta, len);
    }

    /// Update the config picker search query, recompute the filtered
    /// indices, and reset the cursor to the first match (or to whichever
    /// previously-selected item is still visible). The filter is a
    /// case-insensitive substring match over each choice's `name` and
    /// (if present) `description`.
    pub fn config_picker_set_search(&mut self, query: impl Into<String>) {
        let selected_option = match self.config_picker.as_ref() {
            Some(picker) => picker.selected_option,
            None => return,
        };
        let query = query.into();
        let option = self.session_config_options.get(selected_option).cloned();
        let Some(picker) = self.config_picker.as_mut() else {
            return;
        };
        let Some(option) = option.as_ref() else {
            picker.search_query = query;
            picker.filtered_indices = Vec::new();
            picker.selected_value = 0;
            return;
        };
        let Some(choices) = config_option_choices(option) else {
            picker.search_query = query;
            picker.filtered_indices = Vec::new();
            picker.selected_value = 0;
            return;
        };

        // Remember the full-choice index that was selected before the
        // filter changed so we can keep pointing at it if it survives.
        let previously_selected_full = picker.filtered_indices.get(picker.selected_value).copied();

        let haystack = query.to_lowercase();
        let filtered: Vec<usize> = if haystack.is_empty() {
            (0..choices.len()).collect()
        } else {
            choices
                .iter()
                .enumerate()
                .filter(|(_, choice)| {
                    choice.name.to_lowercase().contains(&haystack)
                        || choice
                            .description
                            .as_deref()
                            .map(|d| d.to_lowercase().contains(&haystack))
                            .unwrap_or(false)
                })
                .map(|(i, _)| i)
                .collect()
        };

        let new_selected = previously_selected_full
            .and_then(|full_idx| filtered.iter().position(|&i| i == full_idx))
            .unwrap_or(0);

        picker.search_query = query;
        picker.filtered_indices = filtered;
        picker.selected_value = new_selected;
    }

    /// Submit the current config value selection.
    pub fn config_picker_accept(&mut self) -> Option<(SessionConfigTarget, SessionConfigValueId)> {
        let (selected_option, selected_value) = {
            let picker = self.config_picker.as_ref()?;
            (picker.selected_option, picker.selected_value)
        };

        let (target, value) = {
            let option = self.session_config_options.get(selected_option)?;
            let choices = config_option_choices(option)?;
            let picker = self.config_picker.as_ref()?;
            let full_index = *picker.filtered_indices.get(selected_value)?;
            let choice = choices.get(full_index)?;
            let target = self
                .session_config_targets
                .get(selected_option)
                .cloned()
                .unwrap_or_else(|| SessionConfigTarget::ConfigOption {
                    config_id: option.id.clone(),
                });
            (target, choice.value.clone())
        };
        self.dismiss_config_picker();
        Some((target, value))
    }

    /// Recompute slash-command or inline workspace-file completion from the
    /// current prompt and cursor.
    pub fn update_autocomplete(&mut self) {
        if self.has_pending_permission() || self.config_picker.is_some() || self.runtime_closed {
            self.autocomplete = Autocomplete::default();
            return;
        }

        if let Some((trigger_start, trigger_end, query)) =
            active_file_autocomplete(&self.input, self.input_cursor)
        {
            self.update_file_autocomplete(trigger_start, trigger_end, &query);
            return;
        }

        self.update_command_autocomplete();
    }

    fn update_command_autocomplete(&mut self) {
        if !self.input.starts_with('/') {
            self.autocomplete = Autocomplete::default();
            return;
        }

        // Slug = chars between the leading `/` and the first whitespace
        // or end-of-input. Once the user has typed an argument we stop
        // suggesting (they've committed to a command).
        let after_slash = &self.input[1..];
        if after_slash.contains(char::is_whitespace) {
            self.autocomplete = Autocomplete::default();
            return;
        }
        let query = after_slash.to_lowercase();

        let prev_selected_name = (self.autocomplete.kind == AutocompleteKind::Commands)
            .then(|| {
                self.autocomplete
                    .matches
                    .get(self.autocomplete.selected)
                    .and_then(|&i| self.available_commands.get(i))
                    .map(|c| c.name.clone())
            })
            .flatten();

        let prefix: Vec<usize> = self
            .available_commands
            .iter()
            .enumerate()
            .filter(|(_, c)| c.name.to_lowercase().starts_with(&query))
            .map(|(i, _)| i)
            .collect();
        let matches = if prefix.is_empty() {
            self.available_commands
                .iter()
                .enumerate()
                .filter(|(_, c)| c.name.to_lowercase().contains(&query))
                .map(|(i, _)| i)
                .collect()
        } else {
            prefix
        };

        // Keep the user's selection on the same command if it survived
        // the new filter; otherwise reset to the top.
        let selected = prev_selected_name
            .and_then(|name| {
                matches
                    .iter()
                    .position(|&i| self.available_commands[i].name == name)
            })
            .unwrap_or(0);

        self.autocomplete = Autocomplete {
            visible: !matches.is_empty(),
            selected,
            matches,
            kind: AutocompleteKind::Commands,
        };
    }

    fn update_file_autocomplete(&mut self, trigger_start: usize, trigger_end: usize, query: &str) {
        let mut roots = Vec::with_capacity(1 + self.additional_workspace_roots.len());
        for root in std::iter::once(&self.session_cwd).chain(&self.additional_workspace_roots) {
            let root = root.canonicalize().unwrap_or_else(|_| root.clone());
            if !roots.iter().any(|candidate| candidate == &root) {
                roots.push(root);
            }
        }
        let continuing_file_completion =
            matches!(self.autocomplete.kind, AutocompleteKind::Files { .. });
        let cache_matches = self.file_autocomplete_indexed_roots.as_ref() == Some(&roots);
        let cache_stale = self
            .file_autocomplete_indexed_at
            .is_none_or(|indexed_at| indexed_at.elapsed() >= FILE_AUTOCOMPLETE_CACHE_TTL);
        if (!cache_matches || (!continuing_file_completion && cache_stale))
            && self.file_autocomplete_loading_roots.as_ref() != Some(&roots)
        {
            if !cache_matches {
                self.file_autocomplete_candidates.clear();
                self.file_autocomplete_indexed_roots = None;
                self.file_autocomplete_indexed_at = None;
            }
            self.file_autocomplete_loading_roots = Some(roots.clone());
            self.file_autocomplete_scan_request = Some(roots);
        }

        let previous_path = matches!(self.autocomplete.kind, AutocompleteKind::Files { .. })
            .then(|| {
                self.autocomplete
                    .matches
                    .get(self.autocomplete.selected)
                    .and_then(|index| self.file_autocomplete_candidates.get(*index))
                    .map(|file| file.display_path.clone())
            })
            .flatten();
        let query = query
            .trim_start_matches("./")
            .replace('\\', "/")
            .to_lowercase();
        let mut matches: Vec<usize> = self
            .file_autocomplete_candidates
            .iter()
            .enumerate()
            .filter(|(_, file)| file.display_path.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect();
        matches.sort_by_key(|index| {
            let path = self.file_autocomplete_candidates[*index]
                .display_path
                .to_lowercase();
            let file_name = path.rsplit('/').next().unwrap_or(&path);
            let rank = if file_name == query {
                0
            } else if file_name.starts_with(&query) {
                1
            } else if path.starts_with(&query) {
                2
            } else {
                3
            };
            (rank, path.find(&query).unwrap_or(usize::MAX), path.len())
        });
        matches.truncate(MAX_FILE_AUTOCOMPLETE_MATCHES);

        let selected = previous_path
            .and_then(|path| {
                matches.iter().position(|index| {
                    self.file_autocomplete_candidates[*index].display_path == path
                })
            })
            .unwrap_or(0);
        self.autocomplete = Autocomplete {
            visible: !matches.is_empty(),
            selected,
            matches,
            kind: AutocompleteKind::Files {
                trigger_start,
                trigger_end,
            },
        };
    }

    pub(crate) fn take_file_autocomplete_scan_request(&mut self) -> Option<Vec<PathBuf>> {
        self.file_autocomplete_scan_request.take()
    }

    pub(crate) fn awaits_file_autocomplete_scan(&self, roots: &[PathBuf]) -> bool {
        self.file_autocomplete_loading_roots.as_deref() == Some(roots)
    }

    pub(crate) fn apply_file_autocomplete_scan(
        &mut self,
        roots: Vec<PathBuf>,
        candidates: Vec<WorkspaceFile>,
    ) -> bool {
        if self.file_autocomplete_loading_roots.as_ref() != Some(&roots) {
            return false;
        }
        self.file_autocomplete_loading_roots = None;
        self.file_autocomplete_indexed_roots = Some(roots);
        self.file_autocomplete_indexed_at = Some(Instant::now());
        self.file_autocomplete_candidates = candidates;
        self.update_autocomplete();
        true
    }

    pub fn autocomplete_file_path(&self, index: usize) -> Option<&str> {
        self.file_autocomplete_candidates
            .get(index)
            .map(|file| file.display_path.as_str())
    }

    /// Move the autocomplete cursor by `delta`, wrapping at both ends.
    /// No-op when the popover is hidden or empty.
    pub fn autocomplete_move(&mut self, delta: i32) {
        let len = self.autocomplete.matches.len();
        if !self.autocomplete.visible || len == 0 {
            return;
        }
        move_wrapped(&mut self.autocomplete.selected, delta, len);
    }

    /// Accept the selected command or file. File completion replaces only the
    /// active `@query` range and anchors a resource-link chip at that point.
    pub fn autocomplete_accept(&mut self) -> bool {
        if !self.autocomplete.visible {
            return false;
        }
        let Some(&index) = self.autocomplete.matches.get(self.autocomplete.selected) else {
            return false;
        };
        match self.autocomplete.kind {
            AutocompleteKind::Commands => {
                let Some(cmd) = self.available_commands.get(index) else {
                    return false;
                };
                self.input = format!("/{} ", cmd.name);
                self.input_cursor = self.input.chars().count();
            }
            AutocompleteKind::Files {
                trigger_start,
                trigger_end,
            } => {
                let Some(file) = self.file_autocomplete_candidates.get(index).cloned() else {
                    return false;
                };
                let Ok(path) = file.absolute_path.canonicalize() else {
                    return false;
                };
                if !path.starts_with(&file.root) {
                    return false;
                }
                let Ok(uri) = url::Url::from_file_path(&path) else {
                    return false;
                };
                self.replace_input_range_for_completion(trigger_start, trigger_end, " ");
                let id = self.next_attachment_id;
                self.next_attachment_id += 1;
                self.file_attachments.push(FileAttachment {
                    id,
                    position: trigger_start,
                    display_path: file.display_path.clone(),
                    resource: PromptResource {
                        name: file.display_path,
                        uri: uri.to_string(),
                        size: file.size,
                    },
                });
            }
        }
        self.scroll_input_to_bottom();
        self.autocomplete = Autocomplete::default();
        true
    }

    fn replace_input_range_for_completion(&mut self, start: usize, end: usize, replacement: &str) {
        self.reset_history_navigation();
        let len = self.input.chars().count();
        let start = start.min(len);
        let end = end.min(len).max(start);
        let byte_start = input_byte_index_at_char(&self.input, start);
        let byte_end = input_byte_index_at_char(&self.input, end);
        self.input.replace_range(byte_start..byte_end, replacement);
        let removed = end - start;
        let inserted = replacement.chars().count();
        let adjust = |position: &mut usize| {
            if *position > end {
                *position = position.saturating_sub(removed).saturating_add(inserted);
            } else if *position > start {
                *position = start + inserted;
            }
        };
        for attachment in &mut self.attachments {
            adjust(&mut attachment.position);
        }
        for attachment in &mut self.image_attachments {
            adjust(&mut attachment.position);
        }
        for attachment in &mut self.file_attachments {
            adjust(&mut attachment.position);
        }
        self.input_cursor = start + inserted;
    }

    /// Hide the popover without modifying the input buffer.
    pub fn autocomplete_dismiss(&mut self) {
        self.autocomplete = Autocomplete::default();
    }

    pub fn apply_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Connected {
                prompt_images_supported,
                session_fork_supported,
                side_session_supported,
                side_session_unsupported_reason,
                ..
            } => {
                // Keep the pre-filled agent_label (the configured
                // executable name). The agent may report a different
                // name over ACP, but the user wants to see which
                // binary they wired up in config.
                self.prompt_images_supported = prompt_images_supported;
                self.session_fork_supported = session_fork_supported;
                self.side_session_supported = side_session_supported;
                self.side_session_unsupported_reason = side_session_unsupported_reason;
                if self.is_side {
                    install_side_builtin_commands(&mut self.available_commands);
                } else {
                    install_builtin_commands(
                        &mut self.available_commands,
                        session_fork_supported,
                        side_session_supported,
                    );
                }
                if !self.is_streaming() {
                    self.set_connection_state(ConnectionState::Initializing);
                }
            }
            UiEvent::SessionStarted { session_id, .. } => {
                if self.connection_state == ConnectionState::Forking {
                    self.finish_turn_timer();
                }
                if self.session_id.as_deref() != Some(&session_id) {
                    self.cleanup_nested_history();
                    self.workspace_head_diff = None;
                    self.pending_workspace_diff_total = None;
                    self.close_workspace_diff_viewer();
                    self.close_nested_agent_viewer();
                    self.nested_agent_selected = None;
                    self.subagents.clear();
                    self.tool_calls
                        .retain(|id, _| !id.starts_with(SUBAGENT_ID_PREFIX));
                    self.terminal_outputs
                        .retain(|id, _| !id.starts_with(SUBAGENT_ID_PREFIX));
                    self.subagent_active = false;
                    self.subagent_label = None;
                    self.active_subagents = 0;
                    self.workflow_clocks.clear();
                    if !self.tool_detail_overrides.is_empty() {
                        self.tool_detail_overrides.clear();
                        self.bump_transcript_revision();
                    }
                }
                self.session_id = Some(session_id);
                if !self.is_streaming() {
                    self.set_connection_state(ConnectionState::Ready);
                }
            }
            UiEvent::SessionUpdate(u) => {
                self.apply_session_update(u);
                self.apply_known_terminal_outputs();
            }
            UiEvent::ContextCompacted => {}
            UiEvent::TerminalOutput(snapshot) => {
                self.finalize_thinking(EntryKind::Thought);
                self.terminal_outputs
                    .insert(snapshot.terminal_id.clone(), snapshot);
                self.apply_known_terminal_outputs();
            }
            UiEvent::SessionConfigOptions {
                options,
                targets,
                hidden_config_ids,
            } => {
                self.hidden_session_config_ids.extend(hidden_config_ids);
                self.apply_connected_session_config_options(options, targets);
            }
            UiEvent::InternalMessage(message) => {
                // An internal message starts a fresh orchestrator-initiated
                // exchange. The previous turn's completion may have been
                // deliberately withheld (a Findings verdict drops it and lets
                // the corrective turn produce the real one), so no PromptDone
                // finalized the open message; close it here or the next
                // turn's chunks silently append onto the stale entry.
                self.finalize_thinking(EntryKind::Thought);
                self.finalize_message(EntryKind::Agent);
                // A discrete review starts a fresh orchestrator-initiated
                // turn after the user's turn already completed. Re-enter the
                // streaming state so submissions queue behind it instead of
                // racing the in-flight prompt.
                if matches!(
                    message.kind,
                    crate::event::InternalMessageKind::DiscreteReview
                ) && self.connection_state == ConnectionState::Ready
                {
                    self.set_connection_state(ConnectionState::Streaming);
                    self.turn_started_at = Some(Instant::now());
                    self.last_turn_elapsed = None;
                }
                match message.owner_subagent_id {
                    Some(subagent_id) => {
                        self.append_nested_internal_message(subagent_id, message);
                    }
                    None => {
                        // Primary-owned orchestration packets are model input,
                        // not user transcript. Their visible state is carried
                        // by typed workflow transitions and adjacent
                        // info/warning events, while full nested packets live
                        // only under an explicitly identified actor.
                    }
                }
            }
            UiEvent::AgentUsage(record) => self.agent_usage.observe(record),
            UiEvent::SubagentPoolModelChanged { model, source_id } => {
                self.active_models.subagent = model;
                self.active_models.subagent_source = Some(source_id);
            }
            UiEvent::WorkspaceDiff(diff) => {
                // Per-turn attribution only: it reports how much this turn
                // touched, for the status line and the remote mirror. The
                // Ctrl-G reader deliberately does not read it, because
                // "what this turn changed" and "what is uncommitted" are
                // different questions with different answers.
                self.pending_workspace_diff_total = Some(diff.total_files);
            }
            UiEvent::WorkspaceHeadDiff(diff) => {
                self.workspace_diff_loading = false;
                self.workspace_head_diff = Some(diff);
                self.workspace_diff_scroll_offset = 0;
            }
            UiEvent::PermissionRequest(prompt) => {
                self.finalize_thinking(EntryKind::Thought);
                // Append to the queue rather than replacing the current
                // pending prompt: overwriting would drop the prior
                // oneshot responder, which the agent reads as a silent
                // cancel even though the user never saw it.
                self.help_overlay = false;
                self.permission_queue.push_back(PendingPermission {
                    prompt,
                    selected: 0,
                    scroll_offset: None,
                    subagent_id: None,
                });
                self.update_autocomplete();
            }
            UiEvent::CancelPendingPermissions => {
                self.finalize_thinking(EntryKind::Thought);
                self.cancel_all_pending_permissions();
                self.mark_unfinished_tool_calls_failed("tool call cancelled");
                self.update_autocomplete();
            }
            UiEvent::ElicitationRequest(prompt) => {
                self.finalize_thinking(EntryKind::Thought);
                // Append to the queue rather than replacing the front prompt:
                // overwriting would drop the prior oneshot responder, which the
                // agent reads as a silent cancel. Render unconditionally (no
                // session gating) -- `/setup` elicitations are request-scoped.
                self.help_overlay = false;
                self.elicitation_queue
                    .push_back(PendingElicitation::new(prompt, None));
                self.update_autocomplete();
            }
            UiEvent::Subagent(event) => self.apply_subagent_event(event),
            UiEvent::Workflow(event) => match self.workflows.apply(&event) {
                Ok(crate::workflow::ApplyOutcome::Changed) => {
                    self.apply_workflow_transition(&event);
                }
                Ok(crate::workflow::ApplyOutcome::Duplicate) => {}
                Err(error) => {
                    tracing::warn!(
                        event = "workflow_transition_rejected_by_ui",
                        error = %error,
                        "ignoring an invalid workflow transition"
                    );
                }
            },
            UiEvent::RemotePermissionDecision {
                request_id,
                option_id,
            } => {
                // One remote decision channel carries both prompt kinds. The
                // id namespaces never collide (tool-call id vs. `elicitation:N`),
                // so an unmatched permission lookup simply falls through.
                if !self.resolve_permission_remotely(&request_id, &option_id) {
                    self.resolve_elicitation_remotely(&request_id, &option_id);
                }
            }
            UiEvent::PromptDone { stop_reason, usage } => {
                self.finalize_thinking(EntryKind::Thought);
                self.finalize_message(EntryKind::Agent);
                self.finish_prompt_turn(matches!(stop_reason, StopReason::Cancelled));
                if let Some(usage) = usage {
                    self.token_usage.apply_prompt_usage(usage);
                }
                // A queued prompt is about to fire as the next turn and
                // will own the status line, so any "turn done: <reason>"
                // would only flash and then hang around stale through the
                // new turn.
                if !self.has_queued_prompts() {
                    if let Some(total_files) = self.pending_workspace_diff_total.take()
                        && !matches!(stop_reason, StopReason::Cancelled)
                    {
                        let noun = if total_files == 1 { "file" } else { "files" };
                        self.set_status_line(
                            StatusKind::Info,
                            format!(
                                "this turn changed {total_files} {noun} · Ctrl-G workspace diff"
                            ),
                        );
                    } else {
                        self.set_status_line(
                            StatusKind::Info,
                            format!("turn done: {stop_reason:?}"),
                        );
                    }
                }
                self.pending_workspace_diff_total = None;
                self.update_autocomplete();
            }
            UiEvent::ClaudeUsage(report) => {
                self.set_claude_usage(report);
            }
            UiEvent::CodexUsage(status) => {
                self.set_codex_usage(status);
            }
            UiEvent::PromptFailed { message } => {
                self.finalize_thinking(EntryKind::Thought);
                self.finalize_message(EntryKind::Agent);
                self.finish_prompt_turn(true);
                self.pending_workspace_diff_total = None;
                // Drop queued prompts: finish_prompt_turn flips back to
                // Ready, which the next drain pass would otherwise read as
                // "fire the stash" and auto-resubmit into a
                // possibly-degraded runtime before the user has seen the
                // failure. Mirrors mark_runtime_closed.
                let queued_count = self.queued_prompt_count();
                self.clear_queued_prompts();
                let surfaced = if queued_count > 0 {
                    format!("{message} ({queued_count} queued prompt(s) dropped)")
                } else {
                    message
                };
                self.record_status_message(StatusKind::Warning, surfaced);
                self.update_autocomplete();
            }
            UiEvent::SessionForkFailed { message } => {
                if self.connection_state == ConnectionState::Forking {
                    self.finish_turn_timer();
                    self.set_connection_state(ConnectionState::Ready);
                }
                self.record_status_message(StatusKind::Warning, message);
                self.update_autocomplete();
            }
            UiEvent::Side(event) => self.apply_event(*event),
            UiEvent::SideStartFailed { message } => {
                self.record_status_message(StatusKind::Warning, message);
            }
            UiEvent::RemoteSideStartRequested { .. } | UiEvent::RemoteSideExitRequested => {}
            UiEvent::Warning(msg) => {
                self.record_status_message(StatusKind::Warning, msg);
            }
            UiEvent::Info(msg) => {
                // A session/load replay ends with an ordinary open agent
                // message that no PromptDone will ever close; the load notice
                // is its terminator, so the replayed turn can reach scrollback.
                if msg == crate::event::SESSION_LOADED_NOTICE {
                    self.finalize_thinking(EntryKind::Thought);
                    self.finalize_message(EntryKind::Agent);
                }
                self.record_status_message(StatusKind::Info, msg);
            }
            UiEvent::Fatal(msg) => {
                self.finalize_thinking(EntryKind::Thought);
                self.finalize_message(EntryKind::Agent);
                self.set_connection_state(ConnectionState::Fatal);
                self.record_status_message(StatusKind::Fatal, msg);
                self.mark_runtime_closed();
            }
        }
    }

    fn apply_workflow_transition(&mut self, event: &crate::workflow::WorkflowEvent) {
        use crate::workflow::{
            WorkflowActorId, WorkflowActorLifecycle, WorkflowState, WorkflowTransition,
        };

        match &event.transition {
            WorkflowTransition::Started { .. } => {
                self.workflow_clocks
                    .entry(event.workflow_id)
                    .or_insert_with(|| WorkflowClock {
                        started_at: Instant::now(),
                        finished_at: None,
                    });
                if let Some(notice) = self
                    .workflows
                    .get(event.workflow_id)
                    .and_then(WorkflowState::started_notice)
                {
                    self.push_system_message(notice);
                }
            }
            WorkflowTransition::ActorStarted { actor_id, role } => {
                if let WorkflowActorId::Subagent(subagent_id) = actor_id {
                    let now = Instant::now();
                    let state = self
                        .subagents
                        .entry(*subagent_id)
                        .or_insert_with(|| SubagentStatus::placeholder(Some(role.clone()), now));
                    state.role = Some(role.clone());
                    state.lifecycle = Some(WorkflowActorLifecycle::Running);
                    if state.label_is_placeholder {
                        state.label = nested_role_label(role);
                    }
                }
            }
            WorkflowTransition::ActorSessionBound {
                actor_id,
                retained_session_id,
            } => {
                if let WorkflowActorId::Subagent(subagent_id) = actor_id {
                    self.ensure_subagent_state(*subagent_id).session_id =
                        Some(retained_session_id.clone());
                }
            }
            WorkflowTransition::ActorWaiting {
                actor_id,
                dependency,
                remaining,
                requires_user_action,
            } => {
                if let WorkflowActorId::Subagent(subagent_id) = actor_id {
                    self.ensure_subagent_state(*subagent_id).lifecycle =
                        Some(WorkflowActorLifecycle::Waiting {
                            dependency: dependency.clone(),
                            remaining: *remaining,
                            requires_user_action: *requires_user_action,
                        });
                }
            }
            WorkflowTransition::ActorResumed { actor_id } => {
                if let WorkflowActorId::Subagent(subagent_id) = actor_id {
                    self.ensure_subagent_state(*subagent_id).lifecycle =
                        Some(WorkflowActorLifecycle::Running);
                }
            }
            WorkflowTransition::ActorFinished { actor_id, outcome } => {
                if let WorkflowActorId::Subagent(subagent_id) = actor_id {
                    self.ensure_subagent_state(*subagent_id).lifecycle = Some(match outcome {
                        SubagentOutcome::Completed => WorkflowActorLifecycle::Completed,
                        SubagentOutcome::Cancelled => WorkflowActorLifecycle::Cancelled,
                        SubagentOutcome::Failed(message) => {
                            WorkflowActorLifecycle::Failed(message.clone())
                        }
                    });
                }
            }
            WorkflowTransition::Waiting {
                remaining,
                requires_user_action,
                ..
            } => {
                let summary = self
                    .workflows
                    .get(event.workflow_id)
                    .and_then(|state| state.waiting_notice(*remaining, *requires_user_action));
                if let Some(summary) = summary {
                    self.push_system_message(summary);
                }
            }
            WorkflowTransition::Terminal { outcome, .. } => {
                if let Some(clock) = self.workflow_clocks.get_mut(&event.workflow_id) {
                    clock.finished_at = Some(Instant::now());
                }
                let Some(state) = self.workflows.get(event.workflow_id) else {
                    return;
                };
                // A review that surfaced issues fossilizes into the verdict
                // banner; a plain "review complete" would bury the tally.
                if state.kind == crate::workflow::WorkflowKind::Review && !state.issues.is_empty() {
                    let banner = review_verdict_record(state, *outcome);
                    self.push_review_ledger(banner);
                } else {
                    let summary = state.terminal_notice(*outcome);
                    self.push_system_message(summary);
                }
            }
            WorkflowTransition::IssuesValidated { pass, summaries } => {
                let record = self
                    .workflows
                    .get(event.workflow_id)
                    .map(|state| review_validated_record(*pass, &state.issues))
                    .unwrap_or_default();
                self.push_review_ledger(record);
                self.set_status_line(
                    StatusKind::Warning,
                    format!(
                        "review found {} validated issue{} · F9 details",
                        summaries.len(),
                        if summaries.len() == 1 { "" } else { "s" }
                    ),
                );
            }
            WorkflowTransition::IssuesResolved {
                pass,
                status,
                reason,
            } => {
                let record = self
                    .workflows
                    .get(event.workflow_id)
                    .map(|state| {
                        review_resolved_record(*pass, *status, reason.as_deref(), &state.issues)
                    })
                    .unwrap_or_default();
                self.push_review_ledger(record);
                let count = self
                    .workflows
                    .get(event.workflow_id)
                    .map(|workflow| {
                        workflow
                            .issues
                            .iter()
                            .filter(|issue| issue.status == *status)
                            .count()
                    })
                    .unwrap_or(0);
                self.set_status_line(
                    StatusKind::Info,
                    format!("review issues {}: {count} · F9 details", status.as_str()),
                );
            }
            WorkflowTransition::PhaseChanged { .. }
            | WorkflowTransition::CoverageChanged { .. } => {}
        }
    }

    fn apply_subagent_event(&mut self, event: SubagentEvent) {
        match event {
            SubagentEvent::Started {
                subagent_id,
                resumed,
                label,
                model,
                agent,
                objective,
            } => {
                self.subagent_token_usage = TokenUsage::default();
                let objective = objective.trim().to_string();
                let now = Instant::now();
                let row = self
                    .subagents
                    .entry(subagent_id)
                    .and_modify(|status| {
                        status.label = label.clone();
                        status.label_is_placeholder = false;
                        status.model = model.clone();
                        status.adapter = agent.clone();
                        if status.objective.is_empty() {
                            status.objective = objective.clone();
                        }
                        status.activity = if objective.is_empty() {
                            ragnarok::first_line(&status.objective, SUBAGENT_RECORD_LINE_CHARS)
                        } else {
                            ragnarok::first_line(&objective, SUBAGENT_RECORD_LINE_CHARS)
                        };
                        status.lifecycle = Some(crate::workflow::WorkflowActorLifecycle::Running);
                        status.started_at = now;
                        status.finished = None;
                    })
                    .or_insert_with(|| SubagentStatus {
                        label: label.clone(),
                        label_is_placeholder: false,
                        model: model.clone(),
                        adapter: agent.clone(),
                        objective: objective.clone(),
                        role: None,
                        lifecycle: Some(crate::workflow::WorkflowActorLifecycle::Running),
                        session_id: None,
                        transcript: Vec::new(),
                        archived_history: Vec::new(),
                        open_message_index: None,
                        plan_index: None,
                        activity: objective.clone(),
                        started_at: now,
                        finished: None,
                    });
                let role = row.role.clone();
                let objective = row.objective.clone();
                self.active_subagents = self.running_subagent_count();
                self.subagent_active = self.active_subagents > 0;
                self.subagent_label = self.subagent_active.then(|| label.clone());
                let backend = match model.as_deref() {
                    Some(model) => format!("{agent}/{model}"),
                    None => agent.clone(),
                };
                if !resumed {
                    // A retained actor has one identity across ACP turns. Its
                    // later resumes update the durable detail without
                    // manufacturing another permanent "started" record.
                    let headline = ragnarok::first_line(&objective, SUBAGENT_RECORD_LINE_CHARS);
                    let actor = role
                        .as_ref()
                        .filter(|role| {
                            !matches!(role, crate::workflow::WorkflowActorRole::Implementation)
                        })
                        .map(nested_role_label);
                    let started = if let Some(actor) = actor {
                        if headline.is_empty() {
                            format!("{actor} #{subagent_id} · started")
                        } else {
                            format!("{actor} #{subagent_id} · started · {headline}")
                        }
                    } else if headline.is_empty() {
                        format!("subagent #{subagent_id} · {label} · started")
                    } else {
                        format!("subagent #{subagent_id} · {label} · started · {headline}")
                    };
                    self.push_system_message(started);
                }
                let actor = nested_actor_reference(role.as_ref(), subagent_id);
                let hint = if role
                    .as_ref()
                    .is_none_or(|role| !role.is_internal_review_session())
                {
                    " · /subagents"
                } else {
                    ""
                };
                self.set_status_line(
                    StatusKind::Info,
                    if resumed {
                        format!("{actor} · {label} resumed ({backend}){hint}")
                    } else {
                        format!("{actor} · {label} ({backend}){hint}")
                    },
                );
            }
            SubagentEvent::Activity {
                subagent_id,
                activity,
            } => {
                // Status-row only: no transcript entry, no revision bump. An
                // in-place transcript rewrite here is what used to corrupt
                // already-flushed inline scrollback.
                if let Some(state) = self.subagents.get_mut(&subagent_id) {
                    state.activity = activity;
                }
            }
            SubagentEvent::SessionStarted {
                subagent_id,
                session_id,
            } => {
                self.ensure_subagent_state(subagent_id).session_id = Some(session_id);
            }
            SubagentEvent::SessionUpdate {
                subagent_id,
                update,
            } => self.apply_subagent_update(subagent_id, update),
            SubagentEvent::TerminalOutput {
                subagent_id,
                mut snapshot,
            } => {
                self.finalize_subagent_thinking(subagent_id);
                snapshot.terminal_id = format!(
                    "{}{}",
                    Self::subagent_id_prefix(subagent_id),
                    snapshot.terminal_id
                );
                self.terminal_outputs
                    .insert(snapshot.terminal_id.clone(), snapshot);
                self.apply_known_terminal_outputs();
            }
            SubagentEvent::PermissionRequest {
                subagent_id,
                mut prompt,
            } => {
                self.finalize_subagent_thinking(subagent_id);
                self.help_overlay = false;
                let local_id = prompt.tool_call.tool_call_id.to_string();
                prompt.tool_call.tool_call_id = format!("subagent-{subagent_id}:{local_id}").into();
                self.permission_queue.push_back(PendingPermission {
                    prompt,
                    selected: 0,
                    scroll_offset: None,
                    subagent_id: Some(subagent_id),
                });
                self.update_autocomplete();
            }
            SubagentEvent::ElicitationRequest {
                subagent_id,
                prompt,
            } => {
                self.finalize_subagent_thinking(subagent_id);
                self.help_overlay = false;
                self.elicitation_queue
                    .push_back(PendingElicitation::new(prompt, Some(subagent_id)));
                self.update_autocomplete();
            }
            SubagentEvent::CancelPendingPermissions { subagent_id } => {
                self.finalize_subagent_thinking(subagent_id);
                self.cancel_subagent_prompts(subagent_id);
                self.mark_subagent_tools_failed(subagent_id, "tool call cancelled");
            }
            SubagentEvent::Status {
                subagent_id,
                kind,
                message,
            } => {
                self.finalize_subagent_thinking(subagent_id);
                self.finalize_subagent_message(subagent_id);
                let state = self.ensure_subagent_state(subagent_id);
                state.activity = message.clone();
                state.transcript.push(Entry::System(message.clone()));
                if kind == SubagentStatusKind::Warning {
                    let role = state.role.clone();
                    self.record_status_message(
                        StatusKind::Warning,
                        format!(
                            "{} · {message}",
                            nested_actor_reference(role.as_ref(), subagent_id)
                        ),
                    );
                }
            }
            SubagentEvent::Finished {
                subagent_id,
                outcome,
            } => {
                self.finalize_subagent_thinking(subagent_id);
                self.finalize_subagent_message(subagent_id);
                self.cancel_subagent_prompts(subagent_id);
                self.finish_subagent_row(subagent_id, &outcome);
                self.active_subagents = self.running_subagent_count();
                if self.active_subagents == 0 {
                    self.subagent_active = false;
                    self.subagent_label = None;
                }
                let role = self
                    .subagents
                    .get(&subagent_id)
                    .and_then(|state| state.role.as_ref());
                let actor = nested_actor_reference(role, subagent_id);
                let hint = if role.is_none_or(|role| !role.is_internal_review_session()) {
                    " · /subagents"
                } else {
                    ""
                };
                match outcome {
                    SubagentOutcome::Completed => {
                        self.set_status_line(StatusKind::Info, format!("{actor} complete{hint}"))
                    }
                    SubagentOutcome::Cancelled => {
                        self.mark_subagent_tools_failed(subagent_id, "tool call cancelled");
                        self.set_status_line(StatusKind::Info, format!("{actor} cancelled{hint}"));
                    }
                    SubagentOutcome::Failed(message) => {
                        self.mark_subagent_tools_failed(subagent_id, "tool call failed");
                        // Status line only: the finish record above is already
                        // this subagent's permanent transcript entry.
                        self.set_status_line(
                            StatusKind::Warning,
                            format!("{actor} failed · {message}{hint}"),
                        );
                    }
                }
                self.enforce_nested_history_budget();
            }
        }
    }

    /// Closes out a subagent's durable state, retains its private transcript
    /// for the session, and appends only a compact lifecycle summary to the
    /// primary transcript.
    fn finish_subagent_row(&mut self, subagent_id: u64, outcome: &SubagentOutcome) {
        let now = Instant::now();
        let status = match outcome {
            SubagentOutcome::Completed => "completed".to_string(),
            SubagentOutcome::Cancelled => "cancelled".to_string(),
            SubagentOutcome::Failed(message) => format!(
                "failed: {}",
                ragnarok::first_line(message, SUBAGENT_RECORD_LINE_CHARS)
            ),
        };
        let record = match self.subagents.get_mut(&subagent_id) {
            Some(row) => {
                row.finished = Some((outcome.clone(), now));
                row.lifecycle = Some(match outcome {
                    SubagentOutcome::Completed => {
                        crate::workflow::WorkflowActorLifecycle::Completed
                    }
                    SubagentOutcome::Cancelled => {
                        crate::workflow::WorkflowActorLifecycle::Cancelled
                    }
                    SubagentOutcome::Failed(message) => {
                        crate::workflow::WorkflowActorLifecycle::Failed(message.clone())
                    }
                });
                row.open_message_index = None;
                row.plan_index = None;
                row.transcript.push(Entry::System(status.clone()));
                let elapsed = crate::ui::format_duration(row.elapsed_at(now));
                match (&row.role, outcome) {
                    (
                        Some(crate::workflow::WorkflowActorRole::SpecialistReviewer { lane }),
                        SubagentOutcome::Completed,
                    ) => {
                        format!("reviewer {lane} #{subagent_id} · report delivered · {elapsed}")
                    }
                    (Some(role), SubagentOutcome::Completed)
                        if matches!(
                            role,
                            crate::workflow::WorkflowActorRole::IntentAnalyst
                                | crate::workflow::WorkflowActorRole::ReviewSupervisor
                        ) =>
                    {
                        format!(
                            "{} #{subagent_id} · completed · {elapsed}",
                            nested_role_label(role)
                        )
                    }
                    _ => format!(
                        "subagent #{subagent_id} · {} · {status} · {elapsed}",
                        row.label
                    ),
                }
            }
            // A `Finished` with no row (a start this UI never saw) still gets
            // its record; there is simply no elapsed time to report.
            None => format!("subagent #{subagent_id} · {status}"),
        };
        self.push_system_message(record);
    }

    fn nested_actor_resident_bytes(&self, subagent_id: u64) -> usize {
        let Some(actor) = self.subagents.get(&subagent_id) else {
            return 0;
        };
        let prefix = Self::subagent_id_prefix(subagent_id);
        let transcript_and_tools = crate::ui::nested_actor_history_markdown(self, actor).len();
        let terminal_snapshots = self
            .terminal_outputs
            .iter()
            .filter(|(id, _)| id.starts_with(&prefix))
            .map(|(id, snapshot)| id.len() + format!("{snapshot:?}").len())
            .sum::<usize>();
        let overrides = self
            .tool_detail_overrides
            .keys()
            .filter(|id| id.starts_with(&prefix))
            .map(String::len)
            .sum::<usize>();
        transcript_and_tools + terminal_snapshots + overrides
    }

    fn nested_actor_is_protected(&self, subagent_id: u64) -> bool {
        self.nested_agent_selected == Some(subagent_id)
            || self
                .subagents
                .get(&subagent_id)
                .is_some_and(|actor| actor.finished.is_none())
            || self
                .permission_queue
                .iter()
                .any(|pending| pending.subagent_id == Some(subagent_id))
            || self
                .elicitation_queue
                .iter()
                .any(|pending| pending.subagent_id == Some(subagent_id))
    }

    fn nested_history_dir(&mut self) -> std::io::Result<PathBuf> {
        if let Some(path) = &self.nested_history_dir {
            return Ok(path.path().clone());
        }
        let sequence = NESTED_HISTORY_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mjolnir-nested-history-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        self.nested_history_dir = Some(NestedHistoryDir(Some(path.clone())));
        Ok(path)
    }

    fn offload_nested_actor(&mut self, subagent_id: u64) -> std::io::Result<()> {
        let Some(actor) = self.subagents.get(&subagent_id) else {
            return Ok(());
        };
        if actor.transcript.is_empty() || self.nested_actor_is_protected(subagent_id) {
            return Ok(());
        }
        let markdown = crate::ui::nested_actor_history_markdown(self, actor);
        let segment = actor.archived_history.len();
        let dir = self.nested_history_dir()?;
        let path = dir.join(format!("actor-{subagent_id}-{segment}.md"));
        let partial_path = dir.join(format!("actor-{subagent_id}-{segment}.partial"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        let write_result = (|| {
            let mut file = options.open(&partial_path)?;
            file.write_all(markdown.as_bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&partial_path, &path)
        })();
        if let Err(error) = write_result {
            let _ = std::fs::remove_file(&partial_path);
            return Err(error);
        }

        let prefix = Self::subagent_id_prefix(subagent_id);
        if let Some(actor) = self.subagents.get_mut(&subagent_id) {
            actor.transcript.clear();
            actor.transcript.shrink_to_fit();
            actor.archived_history.push(path);
            actor.open_message_index = None;
            actor.plan_index = None;
        }
        self.tool_calls.retain(|id, _| !id.starts_with(&prefix));
        self.terminal_outputs
            .retain(|id, _| !id.starts_with(&prefix));
        self.tool_detail_overrides
            .retain(|id, _| !id.starts_with(&prefix));
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn force_offload_nested_actor_for_test(&mut self, subagent_id: u64) {
        self.offload_nested_actor(subagent_id)
            .expect("offload nested actor in test");
    }

    fn enforce_nested_history_budget(&mut self) {
        let mut candidates = self
            .subagents
            .iter()
            .filter_map(|(id, actor)| {
                (actor.finished.is_some() && !actor.transcript.is_empty())
                    .then_some((*id, actor.finished.as_ref().map(|(_, at)| *at)))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(_, finished)| *finished);
        let sizes = candidates
            .iter()
            .map(|(id, _)| (*id, self.nested_actor_resident_bytes(*id)))
            .collect::<HashMap<_, _>>();
        let mut total = sizes.values().sum::<usize>();
        for (id, _) in candidates {
            let size = sizes.get(&id).copied().unwrap_or(0);
            if self.nested_actor_is_protected(id)
                || (size <= NESTED_ACTOR_RESIDENT_BUDGET && total <= NESTED_SESSION_RESIDENT_BUDGET)
            {
                continue;
            }
            match self.offload_nested_actor(id) {
                Ok(()) => total = total.saturating_sub(size),
                Err(error) => self.record_status_message(
                    StatusKind::Warning,
                    format!("could not offload nested-agent history: {error}"),
                ),
            }
        }
    }

    fn cleanup_nested_history(&mut self) {
        self.nested_history_dir = None;
    }

    pub fn visible_workflows(&self) -> impl Iterator<Item = &crate::workflow::WorkflowState> {
        self.workflows
            .iter()
            .filter(|workflow| self.workflow_clocks.contains_key(&workflow.id))
    }

    pub fn has_active_workflows(&self) -> bool {
        self.visible_workflows()
            .any(|workflow| workflow.outcome.is_none())
    }

    pub fn workflow_elapsed_at(
        &self,
        workflow_id: crate::workflow::WorkflowId,
        now: Instant,
    ) -> Duration {
        self.workflow_clocks
            .get(&workflow_id)
            .map(|clock| {
                clock
                    .finished_at
                    .unwrap_or(now)
                    .saturating_duration_since(clock.started_at)
            })
            .unwrap_or_default()
    }

    pub fn running_subagent_count(&self) -> usize {
        self.subagents
            .values()
            .filter(|row| row.finished.is_none() && row.counts_as_subagent())
            .count()
    }

    fn finalize_thinking(&mut self, kind: EntryKind) {
        if finalize_active_thinking(&mut self.transcript, kind) {
            self.bump_transcript_revision();
        }
    }

    fn finalize_message(&mut self, kind: EntryKind) {
        match kind {
            EntryKind::Agent => self.agent_open_message_index = None,
            _ => unreachable!("finalize_message requires a message entry kind"),
        }
    }

    fn append_message_chunk(&mut self, kind: EntryKind, text: String) {
        let open_entry = match kind {
            EntryKind::Agent => self.agent_open_message_index,
            _ => unreachable!("append_message_chunk requires a message entry kind"),
        };
        self.agent_open_message_index = Some(append_or_start_owned(
            &mut self.session.transcript,
            kind,
            text,
            open_entry,
        ));
    }

    /// Namespaces one subagent's ACP ids so concurrent subagents (and the
    /// primary) cannot collide on tool-call or terminal ids.
    fn subagent_id_prefix(subagent_id: u64) -> String {
        format!("{SUBAGENT_ID_PREFIX}{subagent_id}:")
    }

    fn ensure_subagent_state(&mut self, subagent_id: u64) -> &mut SubagentStatus {
        self.subagents
            .entry(subagent_id)
            .or_insert_with(|| SubagentStatus::placeholder(None, Instant::now()))
    }

    fn finalize_subagent_thinking(&mut self, subagent_id: u64) {
        if let Some(state) = self.subagents.get_mut(&subagent_id) {
            finalize_active_thinking(&mut state.transcript, EntryKind::SubagentThought);
        }
    }

    fn finalize_subagent_message(&mut self, subagent_id: u64) {
        if let Some(state) = self.subagents.get_mut(&subagent_id) {
            state.open_message_index = None;
        }
    }

    fn append_subagent_message_chunk(&mut self, subagent_id: u64, text: String) {
        let state = self.ensure_subagent_state(subagent_id);
        state.open_message_index = Some(append_or_start_owned(
            &mut state.transcript,
            EntryKind::Subagent,
            text,
            state.open_message_index,
        ));
    }

    fn append_subagent_thinking_chunk(&mut self, subagent_id: u64, text: String) {
        let state = self.ensure_subagent_state(subagent_id);
        append_thinking_chunk(&mut state.transcript, EntryKind::SubagentThought, text);
    }

    fn append_nested_internal_message(&mut self, subagent_id: u64, message: InternalMessage) {
        self.finalize_subagent_thinking(subagent_id);
        self.finalize_subagent_message(subagent_id);
        let state = self.ensure_subagent_state(subagent_id);
        if state.objective.is_empty()
            && matches!(message.kind, crate::event::InternalMessageKind::Delegation)
        {
            state.objective = message.text.clone();
            state.activity = ragnarok::first_line(&message.text, SUBAGENT_RECORD_LINE_CHARS);
        }
        state.transcript.push(Entry::InternalMessage(message));
    }

    fn apply_subagent_update(&mut self, subagent_id: u64, update: SessionUpdate) {
        let prefix = Self::subagent_id_prefix(subagent_id);
        let prefix = prefix.as_str();
        match update {
            SessionUpdate::UserMessageChunk(_) => {}
            SessionUpdate::AgentMessageChunk(chunk) => {
                self.finalize_subagent_thinking(subagent_id);
                self.append_subagent_message_chunk(subagent_id, content_block_text(&chunk.content));
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                self.finalize_subagent_message(subagent_id);
                self.append_subagent_thinking_chunk(
                    subagent_id,
                    content_block_text(&chunk.content),
                );
            }
            SessionUpdate::ToolCall(tool_call) => {
                self.finalize_subagent_thinking(subagent_id);
                self.finalize_subagent_message(subagent_id);
                let key = format!("{prefix}{}", tool_call.tool_call_id);
                let mut view = ToolCallView::from_tool_call(&tool_call);
                view.namespace_terminal_ids(prefix);
                self.tool_calls.insert(key.clone(), view);
                self.ensure_subagent_state(subagent_id)
                    .transcript
                    .push(Entry::SubagentToolCall(key));
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.finalize_subagent_thinking(subagent_id);
                self.finalize_subagent_message(subagent_id);
                let key = format!("{prefix}{}", update.tool_call_id);
                if let Some(view) = self.tool_calls.get_mut(&key) {
                    view.apply_update(&update);
                    view.namespace_terminal_ids(prefix);
                } else {
                    let mut view = ToolCallView {
                        title: update
                            .fields
                            .title
                            .clone()
                            .unwrap_or_else(|| "tool".to_string()),
                        kind: update.fields.kind.unwrap_or(ToolKind::Other),
                        status: update.fields.status.unwrap_or(ToolCallStatus::Pending),
                        body: Vec::new(),
                    };
                    if let Some(content) = &update.fields.content {
                        view.set_content(content);
                        view.namespace_terminal_ids(prefix);
                    }
                    self.tool_calls.insert(key.clone(), view);
                    self.ensure_subagent_state(subagent_id)
                        .transcript
                        .push(Entry::SubagentToolCall(key));
                }
            }
            SessionUpdate::Plan(Plan { entries, .. }) => {
                self.finalize_subagent_thinking(subagent_id);
                self.finalize_subagent_message(subagent_id);
                let state = self.ensure_subagent_state(subagent_id);
                if let Some(index) = state.plan_index
                    && let Some(Entry::SubagentPlan(existing)) = state.transcript.get_mut(index)
                {
                    *existing = entries;
                } else {
                    let index = state.transcript.len();
                    state.transcript.push(Entry::SubagentPlan(entries));
                    state.plan_index = Some(index);
                }
            }
            SessionUpdate::UsageUpdate(update) => {
                let _ = self.subagent_token_usage.apply_usage_update(update);
            }
            _ => {}
        }
        self.apply_known_terminal_outputs();
    }

    fn cancel_subagent_prompts(&mut self, subagent_id: u64) {
        let permission_prefix = format!("subagent-{subagent_id}:");
        let mut primary_permissions = VecDeque::new();
        while let Some(pending) = self.permission_queue.pop_front() {
            if pending.subagent_id == Some(subagent_id)
                && pending
                    .prompt
                    .tool_call
                    .tool_call_id
                    .to_string()
                    .starts_with(&permission_prefix)
            {
                let _ = pending.prompt.responder.send(PermissionDecision::Cancelled);
            } else {
                primary_permissions.push_back(pending);
            }
        }
        self.permission_queue = primary_permissions;

        let mut primary_elicitations = VecDeque::new();
        while let Some(pending) = self.elicitation_queue.pop_front() {
            if pending.subagent_id == Some(subagent_id) {
                let _ = pending.prompt.responder.send(ElicitationOutcome::Cancel);
            } else {
                primary_elicitations.push_back(pending);
            }
        }
        self.elicitation_queue = primary_elicitations;
        self.update_autocomplete();
    }

    fn mark_subagent_tools_failed(&mut self, subagent_id: u64, note: &str) {
        let prefix = Self::subagent_id_prefix(subagent_id);
        let mut changed = false;
        for (id, view) in &mut self.tool_calls {
            if id.starts_with(&prefix)
                && matches!(
                    view.status,
                    ToolCallStatus::Pending | ToolCallStatus::InProgress
                )
            {
                view.status = ToolCallStatus::Failed;
                view.body.push(ToolCallOutput::Note(note.to_string()));
                changed = true;
            }
        }
        if changed {
            self.bump_transcript_revision();
        }
    }

    fn finish_prompt_turn(&mut self, fail_unfinished_tools: bool) {
        self.finish_turn_timer();
        if fail_unfinished_tools {
            self.fail_unfinished_tool_calls();
        }
        // Drop out of Streaming/Cancelling and back to Ready when the turn
        // lands. Leave non-prompt states (Fatal, Closed, unexpected Ready)
        // untouched.
        if matches!(
            self.connection_state,
            ConnectionState::Streaming | ConnectionState::Cancelling
        ) {
            self.set_connection_state(ConnectionState::Ready);
        }
        // Completion changes the derived turn projection even when no entry
        // or tool body changed, so invalidate the transcript render cache.
        self.bump_transcript_revision();
    }

    fn fail_unfinished_tool_calls(&mut self) {
        self.mark_unfinished_tool_calls_failed("tool call ended before completion");
    }

    fn mark_unfinished_tool_calls_failed(&mut self, note: &str) {
        let mut changed = false;
        for view in self.tool_calls.values_mut() {
            if matches!(
                view.status,
                ToolCallStatus::Pending | ToolCallStatus::InProgress
            ) {
                view.status = ToolCallStatus::Failed;
                if !matches!(view.body.last(), Some(ToolCallOutput::Note(existing)) if existing == note)
                {
                    view.body.push(ToolCallOutput::Note(note.to_string()));
                }
                changed = true;
            }
        }
        if changed {
            self.bump_transcript_revision();
        }
    }

    fn finish_turn_timer(&mut self) {
        if let Some(started_at) = self.turn_started_at.take() {
            let elapsed = started_at.elapsed();
            self.last_turn_elapsed = Some(elapsed);
            if let Some(prompt_index) = self.active_prompt_turn.take()
                && let Some(turn) = self
                    .prompt_turns
                    .iter_mut()
                    .find(|turn| turn.prompt_index == prompt_index)
            {
                turn.elapsed = Some(elapsed);
                turn.completed = true;
            }
        }
    }

    fn apply_session_update(&mut self, update: SessionUpdate) {
        match update {
            SessionUpdate::UserMessageChunk(c) => {
                // During an active prompt turn (`Streaming`), the user's
                // message was already echoed locally via
                // `record_user_prompt` for immediate feedback. The agent
                // may replay the same text as a `UserMessageChunk`;
                // suppressing it here keeps the transcript from showing
                // the prompt twice. When the session is `Idle`, this
                // chunk is part of a session replay (e.g. from
                // `session/load`) and the only source of that user
                // message, so we render it.
                if self.is_streaming() {
                    return;
                }
                self.finalize_message(EntryKind::Agent);
                let text = content_block_text(&c.content);
                append_or_start(&mut self.transcript, EntryKind::User, text);
                self.bump_transcript_revision();
            }
            SessionUpdate::AgentMessageChunk(c) => {
                self.finalize_thinking(EntryKind::Thought);
                let text = content_block_text(&c.content);
                self.append_message_chunk(EntryKind::Agent, text);
                self.bump_transcript_revision();
            }
            SessionUpdate::AgentThoughtChunk(c) => {
                self.finalize_message(EntryKind::Agent);
                let text = content_block_text(&c.content);
                append_thinking_chunk(&mut self.transcript, EntryKind::Thought, text);
                self.bump_transcript_revision();
            }
            SessionUpdate::ToolCall(tc) => {
                self.finalize_thinking(EntryKind::Thought);
                self.finalize_message(EntryKind::Agent);
                let id = tc.tool_call_id.to_string();
                let suppressed = is_subagent_transport_call(&tc);
                self.tool_calls
                    .insert(id.clone(), ToolCallView::from_tool_call(&tc));
                self.register_terminals_for_tool_call(&id);
                if suppressed {
                    self.suppressed_tool_calls.insert(id);
                } else {
                    self.transcript.push(Entry::ToolCall(id));
                }
                self.bump_transcript_revision();
            }
            SessionUpdate::ToolCallUpdate(u) => {
                self.finalize_thinking(EntryKind::Thought);
                self.finalize_message(EntryKind::Agent);
                let id = u.tool_call_id.to_string();
                let suppressed =
                    self.suppressed_tool_calls.contains(&id) || is_subagent_transport_update(&u);
                if suppressed {
                    self.suppressed_tool_calls.insert(id.clone());
                    if matches!(self.transcript.last(), Some(Entry::ToolCall(entry_id)) if entry_id == &id)
                    {
                        self.transcript.pop();
                    }
                }
                if let Some(view) = self.tool_calls.get_mut(&id) {
                    view.apply_update(&u);
                } else {
                    // Update before create; synthesize a placeholder.
                    let mut view = ToolCallView {
                        title: u.fields.title.clone().unwrap_or_else(|| "tool".to_string()),
                        kind: u.fields.kind.unwrap_or(ToolKind::Other),
                        status: u.fields.status.unwrap_or(ToolCallStatus::Pending),
                        body: Vec::new(),
                    };
                    if let Some(content) = &u.fields.content {
                        view.set_content(content);
                    }
                    self.tool_calls.insert(id.clone(), view);
                    if !suppressed {
                        self.transcript.push(Entry::ToolCall(id.clone()));
                    }
                }
                self.register_terminals_for_tool_call(&id);
                self.bump_transcript_revision();
            }
            SessionUpdate::Plan(Plan { entries, .. }) => {
                self.finalize_thinking(EntryKind::Thought);
                self.finalize_message(EntryKind::Agent);
                // Replace the most recent Plan entry if present, else push.
                if let Some(Entry::Plan(existing)) = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|e| matches!(e, Entry::Plan(_)))
                {
                    *existing = entries;
                } else {
                    self.transcript.push(Entry::Plan(entries));
                }
                self.bump_transcript_revision();
            }
            SessionUpdate::AvailableCommandsUpdate(u) => {
                self.available_commands = u.available_commands;
                if self.is_side {
                    install_side_builtin_commands(&mut self.available_commands);
                } else {
                    let session_fork_supported = self.session_fork_supported;
                    let side_session_supported = self.side_session_supported;
                    install_builtin_commands(
                        &mut self.available_commands,
                        session_fork_supported,
                        side_session_supported,
                    );
                }
                // The catalog changed mid-typing; rebuild the popover so
                // a `/` already in the buffer reflects the new commands
                // (and so a previously-empty filter can become non-empty).
                self.update_autocomplete();
            }
            SessionUpdate::CurrentModeUpdate(u) => {
                let mode = u.current_mode_id.to_string();
                self.current_mode = Some(mode.clone());
                self.transcript.push(Entry::System(format!("mode: {mode}")));
                self.bump_transcript_revision();
            }
            SessionUpdate::ConfigOptionUpdate(u) => {
                let targets = config_option_targets(&u.config_options);
                self.apply_connected_session_config_options(u.config_options, targets);
            }
            SessionUpdate::SessionInfoUpdate(info) => {
                if let Some(title) = info.title.value()
                    && self.set_session_title(title)
                {
                    let shown = self.session_title.clone().unwrap_or_default();
                    self.transcript
                        .push(Entry::System(format!("session title: {shown}")));
                    self.bump_transcript_revision();
                }
            }
            SessionUpdate::UsageUpdate(u) => {
                if let Some(rate_limit) = self.token_usage.apply_usage_update(u) {
                    // The line is self-describing ("Current session: …"), so
                    // surface it verbatim rather than wrapping it.
                    self.push_system_message(rate_limit);
                }
            }
            _ => {
                self.transcript
                    .push(Entry::System("unsupported session update".to_string()));
                self.bump_transcript_revision();
            }
        }
    }

    fn refresh_config_picker(&mut self) {
        if self.session_config_options.is_empty() {
            self.config_picker = None;
            return;
        };
        let Some((selected_option, selected_value)) = self
            .config_picker
            .as_ref()
            .map(|picker| (picker.selected_option, picker.selected_value))
        else {
            return;
        };

        let Some(option) = self.session_config_options.get(selected_option) else {
            self.config_picker = None;
            return;
        };
        let Some(choices) = config_option_choices(option) else {
            self.config_picker = None;
            return;
        };
        if choices.is_empty() {
            self.config_picker = None;
            return;
        }
        if let Some(picker) = self.config_picker.as_mut() {
            let query = picker.search_query.clone();
            // Recompute filtered indices against the new choices list.
            let haystack = query.to_lowercase();
            let filtered: Vec<usize> = if haystack.is_empty() {
                (0..choices.len()).collect()
            } else {
                choices
                    .iter()
                    .enumerate()
                    .filter(|(_, choice)| {
                        choice.name.to_lowercase().contains(&haystack)
                            || choice
                                .description
                                .as_deref()
                                .map(|d| d.to_lowercase().contains(&haystack))
                                .unwrap_or(false)
                    })
                    .map(|(i, _)| i)
                    .collect()
            };
            picker.filtered_indices = filtered;
            picker.selected_value =
                selected_value.min(picker.filtered_indices.len().saturating_sub(1));
        }
    }

    fn apply_session_config_options(
        &mut self,
        options: Vec<SessionConfigOption>,
        targets: Vec<SessionConfigTarget>,
    ) {
        let targets = if targets.len() == options.len() {
            targets
        } else {
            config_option_targets(&options)
        };
        let (options, targets): (Vec<_>, Vec<_>) = options
            .into_iter()
            .zip(targets)
            .filter(|(option, _)| {
                !self
                    .hidden_session_config_ids
                    .contains(&option.id.to_string())
                    && !matches!(
                        option.category,
                        Some(
                            SessionConfigOptionCategory::Model
                                | SessionConfigOptionCategory::ThoughtLevel
                        )
                    )
            })
            .unzip();
        self.session_config_targets = targets;
        self.session_config_options = options;
        self.refresh_config_picker();

        if let Some(mode_option) = self.session_config_options.iter().find(|option| {
            matches!(
                option.category,
                Some(SessionConfigOptionCategory::Mode | SessionConfigOptionCategory::ThoughtLevel)
            )
        }) && let Some(value) = config_option_current_value_id(mode_option)
        {
            self.current_mode = Some(value.to_string());
        }
    }

    fn apply_connected_session_config_options(
        &mut self,
        options: Vec<SessionConfigOption>,
        targets: Vec<SessionConfigTarget>,
    ) {
        if !self.agent_source_id.is_empty() {
            let visible_options: Vec<SessionConfigOption> = options
                .iter()
                .filter(|option| {
                    !self
                        .hidden_session_config_ids
                        .contains(&option.id.to_string())
                })
                .cloned()
                .collect();
            Self::overlay_session_config(
                &mut self.acp_inventory,
                &self.agent_source_id,
                &visible_options,
            );
            if let Some(menu) = self.mjconfig_menu.as_mut() {
                menu.editor
                    .update_catalog(self.model_choices.clone(), self.acp_inventory.clone());
            }
        }
        self.apply_session_config_options(options, targets);
    }

    fn overlay_session_config(
        inventory: &mut crate::roster::AcpInventory,
        source_id: &str,
        options: &[SessionConfigOption],
    ) {
        if let Some(server) = inventory
            .servers
            .iter_mut()
            .find(|server| server.id == source_id)
        {
            server.session_config = options.to_vec();
        }
    }
}

fn move_wrapped(selected: &mut usize, delta: i32, len: usize) {
    if len > 0 {
        *selected = (*selected as i32 + delta).rem_euclid(len as i32) as usize;
    }
}

fn config_option_targets(options: &[SessionConfigOption]) -> Vec<SessionConfigTarget> {
    options
        .iter()
        .map(|option| SessionConfigTarget::ConfigOption {
            config_id: option.id.clone(),
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    User,
    Agent,
    Thought,
    Subagent,
    SubagentThought,
}

/// Append `text` to the trailing entry of the same kind, or start a new
/// entry. Streaming chunks for the same logical message land in one entry.
fn append_or_start(transcript: &mut Vec<Entry>, kind: EntryKind, text: String) {
    if let Some(last) = transcript.last_mut() {
        match (&kind, last) {
            (EntryKind::User, Entry::UserPrompt(s))
            | (EntryKind::Agent, Entry::AgentMessage(s))
            | (EntryKind::Subagent, Entry::SubagentMessage(s)) => {
                s.push_str(&text);
                return;
            }
            _ => {}
        }
    }
    transcript.push(match kind {
        EntryKind::User => Entry::UserPrompt(text),
        EntryKind::Agent => Entry::AgentMessage(text),
        EntryKind::Thought => Entry::AgentThought(ThoughtEntry {
            text,
            completed: false,
        }),
        EntryKind::Subagent => Entry::SubagentMessage(text),
        EntryKind::SubagentThought => Entry::SubagentThought(ThoughtEntry {
            text,
            completed: false,
        }),
    });
}

/// Append a chunk to its actor-owned open message, even when coordination
/// activity from another actor was appended after it.  Once the owner reaches
/// a content boundary, callers close the stream and the next chunk starts a
/// distinct transcript entry.
fn append_or_start_owned(
    transcript: &mut Vec<Entry>,
    kind: EntryKind,
    text: String,
    open_entry: Option<usize>,
) -> usize {
    if let Some(index) = open_entry {
        let existing = match (kind, transcript.get_mut(index)) {
            (EntryKind::Agent, Some(Entry::AgentMessage(message))) => Some(message),
            (EntryKind::Subagent, Some(Entry::SubagentMessage(message))) => Some(message),
            _ => None,
        };
        if let Some(message) = existing {
            message.push_str(&text);
            return index;
        }
    }
    let index = transcript.len();
    transcript.push(match kind {
        EntryKind::Agent => Entry::AgentMessage(text),
        EntryKind::Subagent => Entry::SubagentMessage(text),
        _ => unreachable!("append_or_start_owned requires a message entry kind"),
    });
    index
}

fn append_thinking_chunk(transcript: &mut Vec<Entry>, kind: EntryKind, text: String) {
    let existing = match kind {
        EntryKind::Thought => transcript.iter_mut().rev().find_map(|entry| match entry {
            Entry::AgentThought(thought) if !thought.completed => Some(thought),
            _ => None,
        }),
        EntryKind::SubagentThought => transcript.iter_mut().rev().find_map(|entry| match entry {
            Entry::SubagentThought(thought) if !thought.completed => Some(thought),
            _ => None,
        }),
        _ => unreachable!("append_thinking_chunk requires a thought entry kind"),
    };
    if let Some(thought) = existing {
        thought.text.push_str(&text);
        return;
    }
    match kind {
        EntryKind::Thought => transcript.push(Entry::AgentThought(ThoughtEntry {
            text,
            completed: false,
        })),
        EntryKind::SubagentThought => transcript.push(Entry::SubagentThought(ThoughtEntry {
            text,
            completed: false,
        })),
        _ => unreachable!("append_thinking_chunk requires a thought entry kind"),
    }
}

fn finalize_active_thinking(transcript: &mut [Entry], kind: EntryKind) -> bool {
    let thought = match kind {
        EntryKind::Thought => transcript.iter_mut().rev().find_map(|entry| match entry {
            Entry::AgentThought(thought) if !thought.completed => Some(thought),
            _ => None,
        }),
        EntryKind::SubagentThought => transcript.iter_mut().rev().find_map(|entry| match entry {
            Entry::SubagentThought(thought) if !thought.completed => Some(thought),
            _ => None,
        }),
        _ => unreachable!("finalize_active_thinking requires a thought entry kind"),
    };
    if let Some(thought) = thought {
        thought.completed = true;
        true
    } else {
        false
    }
}

/// Return the current value identifier for a select-style session config option.
pub fn config_option_current_value_id(
    option: &SessionConfigOption,
) -> Option<&SessionConfigValueId> {
    match &option.kind {
        SessionConfigKind::Select(select) => Some(&select.current_value),
        _ => None,
    }
}

/// Return the current value label for a session config option.
pub fn config_option_current_value_label(option: &SessionConfigOption) -> String {
    match &option.kind {
        SessionConfigKind::Select(select) => config_select_current_value_label(select),
        _ => "unsupported".to_string(),
    }
}

/// Return the value choices for a select-style config option.
pub fn config_option_choices(option: &SessionConfigOption) -> Option<Vec<ConfigValueChoice>> {
    match &option.kind {
        SessionConfigKind::Select(select) => Some(config_select_choices(select)),
        _ => None,
    }
}

/// Whether a session config option selects a model (vs. a mode or thought
/// level). Used to decide which picker rows get a strength score.
pub fn is_model_config_option(option: &SessionConfigOption) -> bool {
    matches!(option.category, Some(SessionConfigOptionCategory::Model))
}

fn config_select_current_value_label(select: &SessionConfigSelect) -> String {
    let choices = config_select_choices(select);
    choices
        .iter()
        .find(|choice| choice.value == select.current_value)
        .map(|choice| choice.name.clone())
        .unwrap_or_else(|| select.current_value.to_string())
}

fn config_select_choices(select: &SessionConfigSelect) -> Vec<ConfigValueChoice> {
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .map(|opt| ConfigValueChoice {
                value: opt.value.clone(),
                name: opt.name.clone(),
                description: opt.description.clone(),
                group: None,
            })
            .collect(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| {
                group.options.iter().map(move |opt| ConfigValueChoice {
                    value: opt.value.clone(),
                    name: opt.name.clone(),
                    description: opt.description.clone(),
                    group: Some(group.name.clone()),
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Render one status message as transcript text. The TUI and the remote
/// mirror both fold the status channel into a transcript, so they share this
/// function rather than each spelling out the severity prefix — two copies of
/// one rule is how the folds drift apart in the first place.
pub(crate) fn status_transcript_text(kind: StatusKind, text: &str) -> String {
    match kind {
        StatusKind::Info => text.to_string(),
        StatusKind::Warning => format!("warning: {text}"),
        StatusKind::Fatal => format!("fatal: {text}"),
    }
}

// ---------------------------------------------------------------------------
// Ragnarok arena state
// ---------------------------------------------------------------------------

/// Battle-feed lines retained for rendering.
const RAGNAROK_FEED_CAP: usize = 250;
/// Per-fighter transcript buffer cap (bytes); trimmed from the front.
const RAGNAROK_TRANSCRIPT_CAP: usize = 96 * 1024;
/// Cap on Thor's streamed text.
const RAGNAROK_THOR_CAP: usize = 32 * 1024;

/// Which pane the arena shows. `Enter` toggles during active combat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArenaPane {
    Arena,
    Transcript,
}

/// Per-fighter render state, folded from [`ragnarok::RagnarokEvent`]s.
#[derive(Debug)]
pub struct RagnarokFighterUi {
    pub card: ragnarok::FighterCard,
    pub state: ragnarok::FighterState,
    /// Most recent action flourish: (kind, caption, when). Drives the pose
    /// and caption in the arena; treated as faded after a few seconds.
    pub action: Option<(ragnarok::ActionKind, String, Instant)>,
    pub actions_seen: u64,
    /// Combat-phase transcript (messages, thoughts, tool lines).
    pub transcript: String,
    /// What this fighter wrote while reviewing a rival.
    pub review_transcript: String,
    /// Last lane appended to `transcript` / `review_transcript`. Streaming
    /// chunks arrive as tiny deltas, so a separator belongs at lane changes,
    /// not between every chunk.
    last_transcript_lane: Option<ragnarok::TextLane>,
    last_review_lane: Option<ragnarok::TextLane>,
    pub diffstat: Option<String>,
    pub worktree_name: Option<String>,
    pub worktree_path: Option<PathBuf>,
    pub worktree_base_sha: Option<String>,
    pub review_progress: Option<ragnarok::ReviewProgress>,
}

impl RagnarokFighterUi {
    fn new(card: ragnarok::FighterCard) -> Self {
        Self {
            card,
            state: ragnarok::FighterState::Summoned,
            action: None,
            actions_seen: 0,
            transcript: String::new(),
            review_transcript: String::new(),
            last_transcript_lane: None,
            last_review_lane: None,
            diffstat: None,
            worktree_name: None,
            worktree_path: None,
            worktree_base_sha: None,
            review_progress: None,
        }
    }

    /// Append a streaming transcript delta, inserting a blank-line break and
    /// a small header when a new lane starts.
    fn push_transcript_chunk(&mut self, lane: ragnarok::TextLane, chunk: &str) {
        if chunk.is_empty() {
            return;
        }

        let review = lane == ragnarok::TextLane::Review;
        let (buf, last): (&mut String, &mut Option<ragnarok::TextLane>) = if review {
            (&mut self.review_transcript, &mut self.last_review_lane)
        } else {
            (&mut self.transcript, &mut self.last_transcript_lane)
        };

        let chunk = if *last != Some(lane) {
            if !buf.is_empty() {
                let trimmed = buf.trim_end_matches(['\n', ' ']).len();
                buf.truncate(trimmed);
                buf.push_str("\n\n");
                match lane {
                    ragnarok::TextLane::Message | ragnarok::TextLane::Review => {
                        buf.push_str("💬 message\n");
                    }
                    ragnarok::TextLane::Thought => buf.push_str("🧠 thinking\n"),
                    ragnarok::TextLane::Tool => {}
                }
            }
            chunk.trim_start_matches('\n')
        } else {
            chunk
        };

        *last = Some(lane);
        push_capped(buf, chunk, RAGNAROK_TRANSCRIPT_CAP);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RagnarokDraftPrStatus {
    Publishing {
        winner: ragnarok::FighterId,
    },
    Published {
        winner: ragnarok::FighterId,
        url: String,
    },
    Failed {
        winner: ragnarok::FighterId,
        message: String,
    },
}

/// Compact arena state shared with read-only observers such as the remote
/// viewer. This is derived from the TUI reducer after every battle update, so
/// observers cannot drift from the state the operator sees locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RagnarokObservation {
    pub task: String,
    pub phase: ragnarok::Phase,
    pub awaiting_approval: bool,
    pub fighters: Vec<RagnarokFighterObservation>,
    pub verdict: Option<RagnarokVerdictObservation>,
    pub chosen_finalist: Option<ragnarok::FighterId>,
    pub draft_pr_status: Option<RagnarokDraftPrStatus>,
    pub failed: Option<String>,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RagnarokFighterObservation {
    pub id: ragnarok::FighterId,
    pub agent_source_id: String,
    pub model_name: String,
    pub state: ragnarok::FighterState,
    pub worktree_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RagnarokVerdictObservation {
    pub clear_winner: Option<ragnarok::FighterId>,
    pub finalists: Option<(ragnarok::FighterId, ragnarok::FighterId)>,
    pub ranking: Vec<ragnarok::FighterId>,
    pub reasoning: String,
    pub thor_fallback: bool,
}

/// All render state for one `/ragnarok` battle.
#[derive(Debug)]
pub struct RagnarokUi {
    pub task: String,
    pub phase: ragnarok::Phase,
    pub fighters: Vec<RagnarokFighterUi>,
    /// The scrolling battle feed: (fighter, themed line).
    pub feed: VecDeque<(Option<ragnarok::FighterId>, String)>,
    /// Number of newest feed lines hidden below the viewport. Zero follows
    /// live output at the bottom.
    pub feed_scroll: usize,
    pub thor_text: String,
    pub thor_action: Option<ragnarok::ThorAction>,
    pub thor_action_at: Instant,
    pub assignments: Vec<ragnarok::Assignment>,
    pub verdict: Option<ragnarok::Verdict>,
    pub failed: Option<String>,
    pub done: bool,
    pub pane: ArenaPane,
    pub selected_fighter: usize,
    /// In the transcript pane, show the fighter's review instead of their
    /// combat transcript.
    pub show_review_lane: bool,
    /// The finalist chosen by the user at a split decision.
    pub chosen_finalist: Option<ragnarok::FighterId>,
    pub draft_pr_status: Option<RagnarokDraftPrStatus>,
    draft_pr_requested_for: Option<ragnarok::FighterId>,
    draft_pr_request: Option<ragnarok::DraftPrRequest>,
    /// First `q` arms quitting; the second `q` aborts the battle.
    pub quit_armed: bool,
    pub started_at: Instant,
    abort_tx: tokio::sync::watch::Sender<bool>,
    proceed_tx: tokio::sync::watch::Sender<bool>,
}

impl RagnarokUi {
    pub fn new(
        task: String,
        abort_tx: tokio::sync::watch::Sender<bool>,
        proceed_tx: tokio::sync::watch::Sender<bool>,
    ) -> Self {
        Self {
            task,
            phase: ragnarok::Phase::Mustering,
            fighters: Vec::new(),
            feed: VecDeque::new(),
            feed_scroll: 0,
            thor_text: String::new(),
            thor_action: None,
            thor_action_at: Instant::now(),
            assignments: Vec::new(),
            verdict: None,
            failed: None,
            done: false,
            pane: ArenaPane::Arena,
            selected_fighter: 0,
            show_review_lane: false,
            chosen_finalist: None,
            draft_pr_status: None,
            draft_pr_requested_for: None,
            draft_pr_request: None,
            quit_armed: false,
            started_at: Instant::now(),
            abort_tx,
            proceed_tx,
        }
    }

    /// Signal the battle task to stop. Idempotent.
    pub fn abort(&self) {
        let _ = self.abort_tx.send(true);
    }

    /// Unleash combat from the pre-combat approval gate. Idempotent.
    pub fn unleash(&self) {
        let _ = self.proceed_tx.send(true);
    }

    /// True while the battle waits at the pre-combat approval gate.
    pub fn awaiting_approval(&self) -> bool {
        self.phase == ragnarok::Phase::Approval && !self.battle_over()
    }

    /// The battle reached a terminal state (verdict, failure, or done).
    pub fn battle_over(&self) -> bool {
        self.done || self.failed.is_some() || self.verdict.is_some()
    }

    pub fn queue_draft_pr_publish(&mut self, winner: ragnarok::FighterId) {
        if self.draft_pr_requested_for.is_some() {
            return;
        }
        if matches!(
            self.draft_pr_status,
            Some(RagnarokDraftPrStatus::Published { winner: published, .. }) if published == winner
        ) {
            return;
        }
        let Some(fighter) = self.fighter(winner) else {
            return;
        };
        let Some(worktree_path) = fighter.worktree_path.clone() else {
            self.draft_pr_status = Some(RagnarokDraftPrStatus::Failed {
                winner,
                message: "winner worktree path is unavailable".to_string(),
            });
            return;
        };
        let Some(base_sha) = fighter.worktree_base_sha.clone() else {
            self.draft_pr_status = Some(RagnarokDraftPrStatus::Failed {
                winner,
                message: "winner worktree base SHA is unavailable".to_string(),
            });
            return;
        };
        let winner_tag = fighter.card.tag();
        self.draft_pr_requested_for = Some(winner);
        self.draft_pr_request = Some(ragnarok::DraftPrRequest {
            winner,
            winner_tag,
            task: self.task.clone(),
            worktree_path,
            base_sha,
        });
    }

    pub fn take_draft_pr_publish_request(&mut self) -> Option<ragnarok::DraftPrRequest> {
        self.draft_pr_request.take()
    }

    pub fn fighter(&self, id: ragnarok::FighterId) -> Option<&RagnarokFighterUi> {
        self.fighters.iter().find(|f| f.card.id == id)
    }

    pub fn fighter_mut(&mut self, id: ragnarok::FighterId) -> Option<&mut RagnarokFighterUi> {
        self.fighters.iter_mut().find(|f| f.card.id == id)
    }

    pub fn fighter_name(&self, id: ragnarok::FighterId) -> String {
        self.fighter(id)
            .map(|f| f.card.model_name.clone())
            .unwrap_or_else(|| format!("champion {id}"))
    }

    pub fn observation(&self) -> RagnarokObservation {
        RagnarokObservation {
            task: self.task.clone(),
            phase: self.phase,
            awaiting_approval: self.awaiting_approval(),
            fighters: self
                .fighters
                .iter()
                .map(|fighter| RagnarokFighterObservation {
                    id: fighter.card.id,
                    agent_source_id: fighter.card.agent_source_id.clone(),
                    model_name: fighter.card.model_name.clone(),
                    state: fighter.state.clone(),
                    worktree_name: fighter.worktree_name.clone(),
                })
                .collect(),
            verdict: self
                .verdict
                .as_ref()
                .map(|verdict| RagnarokVerdictObservation {
                    clear_winner: verdict.clear_winner,
                    finalists: verdict.finalists,
                    ranking: verdict.ranking.clone(),
                    reasoning: verdict.reasoning.clone(),
                    thor_fallback: verdict.thor_fallback,
                }),
            chosen_finalist: self.chosen_finalist,
            draft_pr_status: self.draft_pr_status.clone(),
            failed: self.failed.clone(),
            done: self.done,
        }
    }

    fn push_feed(&mut self, fighter: Option<ragnarok::FighterId>, text: String) {
        let preserve_scrolled_view = self.feed_scroll > 0;
        self.feed.push_back((fighter, text));
        if preserve_scrolled_view {
            self.feed_scroll = self.feed_scroll.saturating_add(1);
        }
        while self.feed.len() > RAGNAROK_FEED_CAP {
            self.feed.pop_front();
        }
        self.feed_scroll = self.feed_scroll.min(self.feed.len().saturating_sub(1));
    }

    pub fn scroll_feed(&mut self, delta: isize) {
        if delta > 0 {
            self.feed_scroll = self
                .feed_scroll
                .saturating_add(delta as usize)
                .min(self.feed.len().saturating_sub(1));
        } else if delta < 0 {
            self.feed_scroll = self.feed_scroll.saturating_sub((-delta) as usize);
        }
    }

    pub fn feed_max_scroll_for_rows(&self, visible_rows: usize) -> usize {
        if visible_rows == 0 {
            0
        } else {
            self.feed.len().saturating_sub(visible_rows)
        }
    }

    pub fn feed_scroll_for_rows(&self, visible_rows: usize) -> usize {
        self.feed_scroll
            .min(self.feed_max_scroll_for_rows(visible_rows))
    }

    pub fn cycle_fighter(&mut self, delta: isize) {
        if self.fighters.is_empty() {
            return;
        }
        let len = self.fighters.len() as isize;
        let next = (self.selected_fighter as isize + delta).rem_euclid(len);
        self.selected_fighter = next as usize;
    }
}

/// Append while trimming the front (at a char boundary) once over `cap`.
fn push_capped(buf: &mut String, chunk: &str, cap: usize) {
    buf.push_str(chunk);
    if buf.len() > cap {
        let mut cut = buf.len() - cap * 3 / 4;
        while cut < buf.len() && !buf.is_char_boundary(cut) {
            cut += 1;
        }
        buf.replace_range(..cut, "…");
    }
}

impl AppState {
    /// `/ragnarok <task>`: stash a launch request for the UI loop (which owns
    /// the battle event channel and the tokio spawn).
    pub fn request_ragnarok(&mut self, task: String) {
        self.ragnarok_launch = Some(task);
    }

    pub fn take_ragnarok_launch(&mut self) -> Option<String> {
        self.ragnarok_launch.take()
    }

    /// Fold one battle event into the arena state.
    pub fn apply_ragnarok_event(&mut self, ev: ragnarok::RagnarokEvent) {
        use ragnarok::RagnarokEvent as E;
        let Some(arena) = self.ragnarok.as_mut() else {
            return;
        };
        match ev {
            E::Phase(phase) => {
                arena.phase = phase;
                arena.quit_armed = false;
                arena.push_feed(None, format!("━━ {} ━━", phase.banner()));
            }
            E::Log { fighter, text } => arena.push_feed(fighter, text),
            E::ThorSpeaks(chunk) => push_capped(&mut arena.thor_text, &chunk, RAGNAROK_THOR_CAP),
            E::ThorAction(action) => {
                arena.thor_action = Some(action);
                arena.thor_action_at = Instant::now();
            }
            E::Roster(cards) => {
                arena.fighters = cards.into_iter().map(RagnarokFighterUi::new).collect();
                arena.selected_fighter = 0;
            }
            E::FighterJoined(card) => {
                let id = card.id;
                let tag = card.tag();
                arena.fighters.push(RagnarokFighterUi::new(card));
                arena.push_feed(
                    Some(id),
                    format!("⚖ {tag} enters only to judge the survivor."),
                );
            }
            E::FighterState { id, state } => {
                let slain_line = if let ragnarok::FighterState::Slain(reason) = &state {
                    Some(format!("☠ {} is slain: {reason}", arena.fighter_name(id)))
                } else {
                    None
                };
                if let Some(f) = arena.fighter_mut(id) {
                    f.state = state;
                }
                if let Some(line) = slain_line {
                    arena.push_feed(Some(id), line);
                }
            }
            E::FighterWorktree {
                id,
                name,
                path,
                base_sha,
            } => {
                let line = format!("🏕 {} pitches camp in {name}", arena.fighter_name(id));
                if let Some(f) = arena.fighter_mut(id) {
                    f.worktree_name = Some(name);
                    f.worktree_path = Some(path);
                    f.worktree_base_sha = Some(base_sha);
                }
                arena.push_feed(Some(id), line);
            }
            E::FighterAction { id, action, detail } => {
                if let Some(f) = arena.fighter_mut(id) {
                    f.action = Some((action, detail, Instant::now()));
                    f.actions_seen += 1;
                }
            }
            E::FighterText { id, lane, chunk } => {
                if let Some(f) = arena.fighter_mut(id) {
                    f.push_transcript_chunk(lane, &chunk);
                }
            }
            E::FighterDiffStat { id, stat } => {
                let tally = stat.lines().last().unwrap_or("").trim().to_string();
                let line = format!("📜 the saga of {}: {tally}", arena.fighter_name(id));
                if let Some(f) = arena.fighter_mut(id) {
                    f.diffstat = Some(stat);
                }
                if !tally.is_empty() {
                    arena.push_feed(Some(id), line);
                }
            }
            E::Assignments(assignments) => arena.assignments = assignments,
            E::ReviewState { reviewer, progress } => {
                if let Some(f) = arena.fighter_mut(reviewer) {
                    f.review_progress = Some(progress);
                }
            }
            E::Verdict(verdict) => {
                arena.verdict = Some(*verdict);
                arena.phase = ragnarok::Phase::Verdict;
                if let Some(winner) = arena.verdict.as_ref().and_then(|v| v.clear_winner) {
                    arena.queue_draft_pr_publish(winner);
                }
            }
            E::DraftPrPublishing { winner } => {
                arena.draft_pr_status = Some(RagnarokDraftPrStatus::Publishing { winner });
                arena.push_feed(
                    Some(winner),
                    format!(
                        "🚀 publishing {} as a draft PR...",
                        arena.fighter_name(winner)
                    ),
                );
            }
            E::DraftPrPublished { winner, url } => {
                arena.draft_pr_status = Some(RagnarokDraftPrStatus::Published {
                    winner,
                    url: url.clone(),
                });
                arena.push_feed(
                    Some(winner),
                    format!("🔗 draft PR for {}: {url}", arena.fighter_name(winner)),
                );
            }
            E::DraftPrFailed { winner, message } => {
                arena.draft_pr_status = Some(RagnarokDraftPrStatus::Failed {
                    winner,
                    message: message.clone(),
                });
                arena.push_feed(
                    Some(winner),
                    format!(
                        "⚠ draft PR for {} failed: {message}",
                        arena.fighter_name(winner)
                    ),
                );
            }
            E::Failed(message) => {
                arena.push_feed(None, format!("💀 RAGNAROK HAS FALLEN: {message}"));
                arena.failed = Some(message);
            }
            E::Done => arena.done = true,
        }
    }

    /// Close the arena: abort the battle if still raging and drop a summary
    /// into the main transcript so the outcome survives in scrollback.
    pub fn close_ragnarok(&mut self) {
        let Some(arena) = self.ragnarok.take() else {
            return;
        };
        arena.abort();
        let summary = ragnarok_summary(&arena);
        self.push_system_message(summary);
    }

    pub fn take_ragnarok_draft_pr_publish_request(&mut self) -> Option<ragnarok::DraftPrRequest> {
        self.ragnarok
            .as_mut()
            .and_then(RagnarokUi::take_draft_pr_publish_request)
    }
}

/// Human-readable battle summary for the main transcript.
fn ragnarok_summary(arena: &RagnarokUi) -> String {
    let mut out = format!("⚡ RAGNAROK — task: {}\n", arena.task);
    if let Some(failed) = &arena.failed {
        out.push_str(&format!("outcome: failed — {failed}\n"));
    }
    if let Some(verdict) = &arena.verdict {
        match (verdict.clear_winner, verdict.finalists) {
            (Some(id), _) => {
                out.push_str(&format!(
                    "👑 winner (Thor's recommendation): {}\n",
                    arena
                        .fighter(id)
                        .map(|f| f.card.tag())
                        .unwrap_or_else(|| format!("champion {id}"))
                ));
            }
            (None, Some((a, b))) => {
                out.push_str("⚖ split decision — finalists:\n");
                for (n, id) in [a, b].into_iter().enumerate() {
                    let marker = if arena.chosen_finalist == Some(id) {
                        " ← your pick"
                    } else {
                        ""
                    };
                    out.push_str(&format!(
                        "  {}. {}{marker}\n",
                        n + 1,
                        arena
                            .fighter(id)
                            .map(|f| f.card.tag())
                            .unwrap_or_else(|| format!("champion {id}"))
                    ));
                }
            }
            (None, None) => {}
        }
        if !verdict.review_verdicts.is_empty() {
            out.push_str("review honesty/validity (Thor's scores):\n");
            for rv in &verdict.review_verdicts {
                out.push_str(&format!(
                    "  {} reviewing {}: honesty {}/10, validity {}/10 — {}\n",
                    arena.fighter_name(rv.reviewer),
                    arena.fighter_name(rv.defender),
                    rv.honesty,
                    rv.validity,
                    rv.notes
                ));
            }
        }
        out.push_str(&format!("Thor's reasoning: {}\n", verdict.reasoning));
    }
    if let Some(status) = &arena.draft_pr_status {
        match status {
            RagnarokDraftPrStatus::Publishing { winner } => {
                out.push_str(&format!(
                    "draft PR: publishing for {}\n",
                    arena.fighter_name(*winner)
                ));
            }
            RagnarokDraftPrStatus::Published { winner, url } => {
                out.push_str(&format!(
                    "draft PR for {}: {url}\n",
                    arena.fighter_name(*winner)
                ));
            }
            RagnarokDraftPrStatus::Failed { winner, message } => {
                out.push_str(&format!(
                    "draft PR for {} failed: {message}\n",
                    arena.fighter_name(*winner)
                ));
            }
        }
    }
    let with_worktrees: Vec<&RagnarokFighterUi> = arena
        .fighters
        .iter()
        .filter(|f| f.worktree_name.is_some())
        .collect();
    if !with_worktrees.is_empty() {
        out.push_str("worktrees (inspect or adopt with `mj --worktree <name>`):\n");
        for f in with_worktrees {
            out.push_str(&format!(
                "  {} → {}\n",
                f.card.tag(),
                f.worktree_name.as_deref().unwrap_or("?")
            ));
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subagent_session_update(update: SessionUpdate) -> UiEvent {
        subagent_session_update_for(1, update)
    }

    fn subagent_session_update_for(subagent_id: u64, update: SessionUpdate) -> UiEvent {
        UiEvent::Subagent(SubagentEvent::SessionUpdate {
            subagent_id,
            update,
        })
    }

    fn subagent_finished(outcome: SubagentOutcome) -> UiEvent {
        UiEvent::Subagent(SubagentEvent::Finished {
            subagent_id: 1,
            outcome,
        })
    }

    fn apply_workflow(
        state: &mut AppState,
        workflow_id: crate::workflow::WorkflowId,
        transition: crate::workflow::WorkflowTransition,
    ) {
        state.apply_event(UiEvent::Workflow(crate::workflow::WorkflowEvent::new(
            workflow_id,
            transition,
        )));
    }

    fn subagent_started(subagent_id: u64, label: &str, objective: &str) -> UiEvent {
        UiEvent::Subagent(SubagentEvent::Started {
            subagent_id,
            resumed: false,
            label: label.to_string(),
            model: Some("gpt-y".to_string()),
            agent: "codex-acp".to_string(),
            objective: objective.to_string(),
        })
    }

    fn subagent_activity(subagent_id: u64, activity: &str) -> UiEvent {
        UiEvent::Subagent(SubagentEvent::Activity {
            subagent_id,
            activity: activity.to_string(),
        })
    }

    fn finished_subagent(subagent_id: u64, outcome: SubagentOutcome) -> UiEvent {
        UiEvent::Subagent(SubagentEvent::Finished {
            subagent_id,
            outcome,
        })
    }
    use agent_client_protocol::schema::v1::{
        AudioContent, AvailableCommand, AvailableCommandsUpdate, ConfigOptionUpdate, Content,
        ContentBlock, ContentChunk, CreateElicitationRequest, CreateElicitationResponse, Diff,
        ElicitationAcceptAction, ElicitationAction, ElicitationFormMode, ElicitationId,
        ElicitationSchema, ElicitationSessionScope, ElicitationUrlMode, EmbeddedResource,
        EmbeddedResourceResource, ImageContent, PermissionOption, PermissionOptionKind, Plan,
        PlanEntry, PlanEntryPriority, PlanEntryStatus, ResourceLink, SessionConfigOption,
        SessionConfigOptionCategory, SessionConfigSelectOption, StopReason, StringPropertySchema,
        Terminal, TextContent, TextResourceContents, Usage, UsageUpdate,
    };

    fn text_chunk(s: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(s)))
    }

    #[test]
    fn side_conversation_keeps_main_transcript_and_usage_independent() {
        let mut main = AppState::new();
        main.agent_label = "model".to_string();
        main.session_id = Some("main-session".to_string());
        main.set_connection_state(ConnectionState::Streaming);
        let side = main.side_conversation(None);

        main.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("main answer"),
        )));
        main.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: Some(Usage::new(42, 30, 12)),
        });

        assert!(
            main.transcript
                .iter()
                .any(|entry| matches!(entry, Entry::AgentMessage(text) if text == "main answer"))
        );
        assert_eq!(main.token_usage.total_tokens, Some(42));
        assert!(side.transcript.is_empty());
        assert_eq!(side.token_usage.total_tokens, None);
        assert!(side.is_side);
        assert_eq!(side.side_main_notice.as_deref(), Some("Main running"));
    }

    #[test]
    fn tool_detail_overrides_are_per_id_reset_by_global_toggle_and_session_change() {
        let mut state = AppState::new();
        for id in ["first", "second"] {
            state.tool_calls.insert(
                id.to_string(),
                ToolCallView {
                    title: id.to_string(),
                    kind: ToolKind::Execute,
                    status: ToolCallStatus::Completed,
                    body: vec![ToolCallOutput::Text("output".to_string())],
                },
            );
        }
        let revision = state.transcript_revision();
        assert!(state.toggle_tool_detail("first", false));
        assert_eq!(state.tool_detail_expanded("first"), Some(true));
        assert_eq!(state.tool_detail_expanded("second"), None);
        assert_ne!(state.transcript_revision(), revision);

        state.toggle_expand_transcript_details();
        assert!(state.expand_transcript_details);
        assert_eq!(state.tool_detail_expanded("first"), None);
        assert!(state.toggle_tool_detail("second", true));
        assert_eq!(state.tool_detail_expanded("second"), Some(false));

        state.apply_event(UiEvent::SessionStarted {
            session_id: "one".to_string(),
            resumed: false,
        });
        assert_eq!(state.tool_detail_expanded("second"), None);
        assert!(state.toggle_tool_detail("first", true));
        state.apply_event(UiEvent::SessionStarted {
            session_id: "one".to_string(),
            resumed: true,
        });
        assert_eq!(state.tool_detail_expanded("first"), Some(false));
        state.apply_event(UiEvent::SessionStarted {
            session_id: "two".to_string(),
            resumed: false,
        });
        assert_eq!(state.tool_detail_expanded("first"), None);
    }

    #[test]
    fn streaming_agent_chunks_coalesce() {
        let mut s = AppState::new();
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("hello "),
        )));
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("world"),
        )));
        assert_eq!(s.session.transcript.len(), 1);
        match &s.session.transcript[0] {
            Entry::AgentMessage(s) => assert_eq!(s, "hello world"),
            other => panic!("unexpected entry: {other:?}"),
        }
    }

    #[test]
    fn plan_updates_replace_entries_without_changing_priority_or_status() {
        let mut state = AppState::new();
        let initial = vec![PlanEntry::new(
            "inspect renderer",
            PlanEntryPriority::High,
            PlanEntryStatus::InProgress,
        )];
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::Plan(Plan::new(
            initial.clone(),
        ))));
        assert!(
            matches!(state.transcript.last(), Some(Entry::Plan(entries)) if entries == &initial)
        );

        let replacement = vec![
            PlanEntry::new(
                "inspect renderer",
                PlanEntryPriority::High,
                PlanEntryStatus::Completed,
            ),
            PlanEntry::new(
                "verify narrow layout",
                PlanEntryPriority::Low,
                PlanEntryStatus::Pending,
            ),
        ];
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::Plan(Plan::new(
            replacement.clone(),
        ))));

        assert_eq!(state.session.transcript.len(), 1);
        assert!(
            matches!(state.transcript.last(), Some(Entry::Plan(entries)) if entries == &replacement)
        );
    }

    fn head_diff_event(paths: &[&str]) -> crate::event::WorkspaceHeadDiffEvent {
        crate::event::WorkspaceHeadDiffEvent {
            diffs: paths
                .iter()
                .map(|path| crate::event::WorkspaceDiff {
                    path: PathBuf::from(path),
                    old_text: Some("old\n".to_string()),
                    new_text: "new\n".to_string(),
                })
                .collect(),
            total_files: paths.len(),
            max_files: 100,
            truncated: false,
            unavailable: None,
        }
    }

    #[test]
    fn turn_workspace_diffs_arm_the_status_total_without_feeding_the_reader() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::SessionStarted {
            session_id: "first-session".to_string(),
            resumed: false,
        });
        let revision = state.transcript_revision;

        state.apply_event(UiEvent::WorkspaceDiff(crate::event::WorkspaceDiffEvent {
            turn_id: 7,
            diffs: vec![crate::event::WorkspaceDiff {
                path: PathBuf::from("src/lib.rs"),
                old_text: Some("before\n".to_string()),
                new_text: "after\n".to_string(),
            }],
            total_files: 1,
            max_files: 20,
            truncated: false,
        }));

        // The per-turn event feeds the status line only. Routing it into the
        // reader is what used to make Ctrl-G show one turn while claiming to
        // show the session.
        assert_eq!(state.pending_workspace_diff_total, Some(1));
        assert!(state.workspace_head_diff.is_none());
        assert_eq!(state.workspace_diff_file_count(), 0);
        assert!(state.transcript.is_empty());
        assert!(state.tool_calls.is_empty());
        assert_eq!(state.transcript_revision, revision);
    }

    #[test]
    fn workspace_head_diff_replaces_rather_than_accumulates() {
        let mut state = AppState::new();
        state
            .transcript
            .push(Entry::UserPrompt("keep me".to_string()));
        let transcript_len = state.transcript.len();

        state.open_workspace_diff_viewer();
        assert!(state.workspace_diff_loading, "opening requests a refresh");

        state.apply_event(UiEvent::WorkspaceHeadDiff(head_diff_event(&[
            "one.rs", "two.rs",
        ])));
        assert!(!state.workspace_diff_loading);
        assert_eq!(state.workspace_diff_file_count(), 2);

        // A second result supersedes the first outright: the workspace has one
        // current state, so there is no history to page back through.
        state.apply_event(UiEvent::WorkspaceHeadDiff(head_diff_event(&["three.rs"])));
        assert_eq!(state.workspace_diff_file_count(), 1);
        assert_eq!(
            state.workspace_head_diff.as_ref().unwrap().diffs[0].path,
            PathBuf::from("three.rs")
        );
        assert_eq!(state.transcript.len(), transcript_len);
    }

    #[test]
    fn workspace_diff_refresh_keeps_the_selected_file() {
        let mut state = AppState::new();
        state.open_workspace_diff_viewer();
        state.apply_event(UiEvent::WorkspaceHeadDiff(head_diff_event(&[
            "one.rs", "two.rs",
        ])));
        state.select_workspace_diff_file(true);
        assert_eq!(state.workspace_diff_selected_file, 1);

        state.begin_workspace_diff_refresh();
        assert!(state.workspace_diff_loading);
        assert_eq!(
            state.workspace_diff_selected_file, 1,
            "an explicit refresh must not yank the reader back to the first file"
        );
    }

    #[test]
    fn workspace_head_diff_clears_for_a_new_session() {
        let mut state = AppState::new();
        state.open_workspace_diff_viewer();
        state.apply_event(UiEvent::WorkspaceHeadDiff(head_diff_event(&["one.rs"])));
        state.workspace_diff_selected_file = 9;
        state.workspace_diff_scroll_offset = 9;

        state.apply_event(UiEvent::SessionStarted {
            session_id: "replacement".to_string(),
            resumed: false,
        });
        assert!(state.workspace_head_diff.is_none());
        assert!(!state.workspace_diff_viewer);
        assert_eq!(state.workspace_diff_selected_file, 0);
        assert_eq!(state.workspace_diff_scroll_offset, 0);
    }

    #[test]
    fn workspace_diff_viewer_open_close_resets_and_excludes_transcript_reader() {
        let mut state = AppState::new();
        state.transcript_viewer = true;
        state.scroll_offset = 17;

        state.open_workspace_diff_viewer();
        assert!(state.workspace_diff_viewer);
        assert!(!state.transcript_viewer);
        assert_eq!(state.scroll_offset, 0);
        state.workspace_diff_selected_file = 3;
        state.workspace_diff_scroll_offset = 12;

        state.close_workspace_diff_viewer();
        assert!(!state.workspace_diff_viewer);
        assert_eq!(state.workspace_diff_selected_file, 0);
        assert_eq!(state.workspace_diff_scroll_offset, 0);

        state.open_workspace_diff_viewer();
        state.open_transcript_viewer();
        assert!(state.transcript_viewer);
        assert!(!state.workspace_diff_viewer);
        assert_eq!(state.workspace_diff_selected_file, 0);
        assert_eq!(state.workspace_diff_scroll_offset, 0);
    }

    #[test]
    fn first_prompt_seeds_provisional_session_title() {
        let mut state = AppState::new();
        assert_eq!(state.session_title, None);

        state.record_user_prompt("fix the flaky\nresume test".to_string());

        assert_eq!(
            state.session_title.as_deref(),
            Some("fix the flaky resume test")
        );
    }

    #[test]
    fn provisional_session_title_is_truncated_to_width() {
        let mut state = AppState::new();

        state.record_user_prompt("x".repeat(100));

        let title = state.session.session_title.expect("provisional title");
        assert_eq!(title, format!("{}...", "x".repeat(45)));
    }

    #[test]
    fn later_prompts_do_not_replace_an_existing_session_title() {
        let mut state = AppState::new();
        state.record_user_prompt("first prompt".to_string());

        state.record_user_prompt("second prompt".to_string());

        assert_eq!(state.session_title.as_deref(), Some("first prompt"));
    }

    #[test]
    fn agent_session_info_update_overwrites_provisional_title() {
        let mut state = AppState::new();
        state.record_user_prompt("first prompt".to_string());

        state.apply_session_update(SessionUpdate::SessionInfoUpdate(
            agent_client_protocol::schema::v1::SessionInfoUpdate::new().title("Real agent title"),
        ));

        assert_eq!(state.session_title.as_deref(), Some("Real agent title"));
    }

    #[test]
    fn resumed_session_title_survives_the_next_prompt() {
        let mut state = AppState::new();
        assert!(state.set_session_title("Carried over from resume"));

        state.record_user_prompt("follow-up work".to_string());

        assert_eq!(
            state.session_title.as_deref(),
            Some("Carried over from resume")
        );
    }

    #[test]
    fn completed_turn_surfaces_workspace_diff_hint_with_singular_and_plural_counts() {
        for (total_files, expected) in [
            (1, "this turn changed 1 file · Ctrl-G workspace diff"),
            (3, "this turn changed 3 files · Ctrl-G workspace diff"),
        ] {
            let mut state = AppState::new();
            state.record_user_prompt("make a change".to_string());
            let transcript_len = state.transcript.len();

            state.apply_event(UiEvent::WorkspaceDiff(crate::event::WorkspaceDiffEvent {
                turn_id: 1,
                diffs: vec![crate::event::WorkspaceDiff {
                    path: PathBuf::from("src/lib.rs"),
                    old_text: Some("before\n".to_string()),
                    new_text: "after\n".to_string(),
                }],
                total_files,
                max_files: 20,
                truncated: false,
            }));
            state.apply_event(UiEvent::PromptDone {
                stop_reason: StopReason::EndTurn,
                usage: None,
            });

            assert_eq!(
                state
                    .status_line
                    .as_ref()
                    .map(|status| status.text.as_str()),
                Some(expected)
            );
            assert_eq!(state.transcript.len(), transcript_len);
            assert!(state.tool_calls.is_empty());
        }
    }

    #[test]
    fn completed_no_diff_turn_keeps_normal_done_status_and_clears_prior_diff_hint() {
        let mut state = AppState::new();
        state.record_user_prompt("first".to_string());
        state.apply_event(UiEvent::WorkspaceDiff(crate::event::WorkspaceDiffEvent {
            turn_id: 1,
            diffs: vec![crate::event::WorkspaceDiff {
                path: PathBuf::from("src/lib.rs"),
                old_text: None,
                new_text: "after\n".to_string(),
            }],
            total_files: 1,
            max_files: 20,
            truncated: false,
        }));
        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        state.record_user_prompt("second".to_string());
        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });

        assert_eq!(
            state
                .status_line
                .as_ref()
                .map(|status| status.text.as_str()),
            Some("turn done: EndTurn")
        );
    }

    #[test]
    fn completed_turn_uses_uncapped_workspace_diff_total_in_hint() {
        let mut state = AppState::new();
        state.record_user_prompt("make many changes".to_string());
        state.apply_event(UiEvent::WorkspaceDiff(crate::event::WorkspaceDiffEvent {
            turn_id: 1,
            diffs: vec![crate::event::WorkspaceDiff {
                path: PathBuf::from("src/lib.rs"),
                old_text: None,
                new_text: "after\n".to_string(),
            }],
            total_files: 21,
            max_files: 20,
            truncated: true,
        }));
        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });

        assert_eq!(
            state
                .status_line
                .as_ref()
                .map(|status| status.text.as_str()),
            Some("this turn changed 21 files · Ctrl-G workspace diff")
        );
    }

    #[test]
    fn thinking_chunks_accumulate_and_same_actor_activity_finalizes_them() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
            text_chunk("first"),
        )));
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
            text_chunk(" thought"),
        )));

        assert!(matches!(
            state.transcript.as_slice(),
            [Entry::AgentThought(text)] if text.text == "first thought" && !text.completed
        ));

        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::ToolCall(
            ToolCall::new("call-1", "work"),
        )));
        assert!(matches!(
            state.transcript.as_slice(),
            [Entry::AgentThought(thought), Entry::ToolCall(id)]
                if thought.text == "first thought" && thought.completed && id == "call-1"
        ));
    }

    #[test]
    fn subagent_thinking_chunks_accumulate() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id: 1,
            resumed: false,
            model: None,
            agent: "codex-acp".to_string(),
            objective: String::new(),
            label: "subagent".to_string(),
        }));
        state.apply_event(subagent_session_update(SessionUpdate::AgentThoughtChunk(
            text_chunk("forging"),
        )));
        state.apply_event(subagent_session_update(SessionUpdate::AgentThoughtChunk(
            text_chunk(" now"),
        )));

        assert!(matches!(
            state.transcript.as_slice(),
            [Entry::System(started)] if started.contains("subagent #1")
        ));
        assert!(matches!(
            state.subagents.get(&1).expect("actor").transcript.as_slice(),
            [Entry::SubagentThought(text)]
                if text.text == "forging now" && !text.completed
        ));
    }

    #[test]
    fn nested_viewer_orders_every_actor_with_newest_running_first() {
        let mut state = AppState::new();
        state.apply_event(subagent_started(1, "old-complete", "done"));
        state.apply_event(subagent_started(2, "older-running", "work"));
        state.apply_event(finished_subagent(1, SubagentOutcome::Completed));
        state.apply_event(subagent_started(3, "newest-running", "work"));

        assert_eq!(state.nested_agent_viewer_ids(), vec![3, 2, 1]);
        assert!(state.open_nested_agent_viewer());
        assert_eq!(state.nested_agent_selected, Some(3));
        state.select_nested_agent(true);
        assert_eq!(state.nested_agent_selected, Some(2));
        assert_eq!(state.nested_agent_scroll_offset, usize::MAX);
        state.select_nested_agent(true);
        assert_eq!(state.nested_agent_selected, Some(1));
    }

    #[test]
    fn oversized_completed_nested_history_is_offloaded_and_cleaned_with_session() {
        let mut state = AppState::new();
        state.apply_event(subagent_started(1, "large", "retain everything"));
        state.apply_event(subagent_session_update_for(
            1,
            SessionUpdate::AgentMessageChunk(text_chunk("actor prose")),
        ));
        let mut fields = agent_client_protocol::schema::v1::ToolCallUpdateFields::default();
        fields.title = Some("large tool".to_string());
        fields.content = Some(vec![
            ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(
                "tool payload ".repeat(100_000),
            )))),
            ToolCallContent::Terminal(Terminal::new(
                agent_client_protocol::schema::v1::TerminalId::new("large-terminal"),
            )),
        ]);
        state.apply_event(subagent_session_update_for(
            1,
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new("large-tool", fields)),
        ));
        state.apply_event(UiEvent::Subagent(SubagentEvent::TerminalOutput {
            subagent_id: 1,
            snapshot: TerminalOutputSnapshot {
                terminal_id: "large-terminal".to_string(),
                output: "terminal payload ".repeat(100_000),
                truncated: false,
                exit_status: Some(TerminalExitStatus::new().exit_code(0)),
            },
        }));
        assert!(state.toggle_tool_detail("subagent-1:large-tool", false));
        state.apply_event(finished_subagent(1, SubagentOutcome::Completed));

        let actor = state.nested_agent(1).expect("retained actor metadata");
        let archive = actor.archived_history[0].clone();
        assert!(actor.transcript.is_empty());
        assert!(!state.tool_calls.contains_key("subagent-1:large-tool"));
        assert!(
            !state
                .terminal_outputs
                .contains_key("subagent-1:large-terminal")
        );
        assert!(
            !state
                .tool_detail_overrides
                .contains_key("subagent-1:large-tool")
        );
        let history = actor.archived_history_markdown().expect("history");
        assert!(history.contains("tool payload"));
        assert!(history.contains("terminal payload"));
        assert!(history.contains("large tool"));
        assert!(archive.exists());

        state.apply_event(UiEvent::SessionStarted {
            session_id: "replacement".to_string(),
            resumed: false,
        });
        assert!(!archive.exists());
        assert!(state.subagents.is_empty());
    }

    #[test]
    fn selected_completed_nested_actor_is_never_offloaded() {
        let mut state = AppState::new();
        state.apply_event(subagent_started(1, "selected", "inspect me"));
        state.nested_agent_selected = Some(1);
        state.apply_event(subagent_session_update_for(
            1,
            SessionUpdate::AgentMessageChunk(text_chunk(&"selected ".repeat(300_000))),
        ));
        state.apply_event(finished_subagent(1, SubagentOutcome::Completed));

        let actor = state.nested_agent(1).expect("selected actor");
        assert!(actor.archived_history.is_empty());
        assert!(!actor.transcript.is_empty());
    }

    #[test]
    fn oversized_running_nested_actor_is_never_offloaded() {
        let mut state = AppState::new();
        state.apply_event(subagent_started(1, "running", "keep active"));
        state.apply_event(subagent_session_update_for(
            1,
            SessionUpdate::AgentMessageChunk(text_chunk(&"running ".repeat(300_000))),
        ));

        state.enforce_nested_history_budget();

        let actor = state.nested_agent(1).expect("running actor");
        assert!(actor.archived_history.is_empty());
        assert!(!actor.transcript.is_empty());
    }

    #[test]
    fn primary_session_change_reclaims_resident_nested_tool_and_terminal_state() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::SessionStarted {
            session_id: "first".to_string(),
            resumed: false,
        });
        state.tool_calls.insert(
            "primary-tool".to_string(),
            ToolCallView {
                title: "primary".to_string(),
                kind: ToolKind::Other,
                status: ToolCallStatus::Completed,
                body: Vec::new(),
            },
        );
        state.apply_event(subagent_session_update_for(
            1,
            SessionUpdate::ToolCall(ToolCall::new("nested-tool", "nested")),
        ));
        state.apply_event(UiEvent::Subagent(SubagentEvent::TerminalOutput {
            subagent_id: 1,
            snapshot: TerminalOutputSnapshot {
                terminal_id: "nested-terminal".to_string(),
                output: "large output".to_string(),
                truncated: false,
                exit_status: None,
            },
        }));

        state.apply_event(UiEvent::SessionStarted {
            session_id: "second".to_string(),
            resumed: false,
        });

        assert!(state.tool_calls.contains_key("primary-tool"));
        assert!(!state.tool_calls.contains_key("subagent-1:nested-tool"));
        assert!(
            !state
                .terminal_outputs
                .contains_key("subagent-1:nested-terminal")
        );
    }

    #[test]
    fn failed_archive_attempt_removes_partial_file_and_can_retry() {
        let mut state = AppState::new();
        state.apply_event(subagent_started(1, "retry", "archive"));
        state.apply_event(subagent_session_update_for(
            1,
            SessionUpdate::AgentMessageChunk(text_chunk("history")),
        ));
        state.apply_event(finished_subagent(1, SubagentOutcome::Completed));
        let dir = state.nested_history_dir().expect("history directory");
        let partial = dir.join("actor-1-0.partial");
        std::fs::write(&partial, "stale partial").expect("seed stale partial");

        assert!(state.offload_nested_actor(1).is_err());
        assert!(!partial.exists());
        state.offload_nested_actor(1).expect("retry succeeds");
        assert!(state.nested_agent(1).expect("actor").archived_history.len() == 1);
    }

    #[test]
    fn session_budget_offloads_oldest_completed_review_cycles() {
        let mut state = AppState::new();
        for id in 1..=5 {
            state.apply_event(subagent_started(id, "review", "review cycle"));
            state.apply_event(subagent_session_update_for(
                id,
                SessionUpdate::AgentMessageChunk(text_chunk(&"cycle ".repeat(280_000))),
            ));
            state.apply_event(finished_subagent(id, SubagentOutcome::Completed));
        }

        assert!(
            state
                .nested_agent(1)
                .expect("oldest actor")
                .archived_history
                .len()
                == 1
        );
        assert!(
            state
                .nested_agent(5)
                .expect("newest actor")
                .archived_history
                .is_empty()
        );
        let resident = (1..=5)
            .map(|id| state.nested_actor_resident_bytes(id))
            .sum::<usize>();
        assert!(resident <= NESTED_SESSION_RESIDENT_BUDGET);
    }

    #[test]
    fn concurrent_subagent_message_and_thought_streams_never_merge() {
        let mut state = AppState::new();
        state.apply_event(subagent_started(1, "one", ""));
        state.apply_event(subagent_started(2, "two", ""));

        state.apply_event(subagent_session_update_for(
            1,
            SessionUpdate::AgentMessageChunk(text_chunk("one-a")),
        ));
        state.apply_event(subagent_session_update_for(
            2,
            SessionUpdate::AgentMessageChunk(text_chunk("two")),
        ));
        state.apply_event(subagent_session_update_for(
            1,
            SessionUpdate::AgentMessageChunk(text_chunk("one-b")),
        ));

        let actor_one_messages: Vec<&str> = state
            .subagents
            .get(&1)
            .expect("actor one")
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                Entry::SubagentMessage(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let actor_two_messages: Vec<&str> = state
            .subagents
            .get(&2)
            .expect("actor two")
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                Entry::SubagentMessage(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(actor_one_messages, ["one-aone-b"]);
        assert_eq!(actor_two_messages, ["two"]);
        assert!(
            !state.transcript.iter().any(|entry| matches!(
                entry,
                Entry::SubagentMessage(_) | Entry::SubagentThought(_)
            ))
        );

        state.apply_event(subagent_session_update_for(
            1,
            SessionUpdate::AgentThoughtChunk(text_chunk("thought-one")),
        ));
        state.apply_event(subagent_session_update_for(
            2,
            SessionUpdate::AgentThoughtChunk(text_chunk("thought-two")),
        ));
        state.apply_event(subagent_session_update_for(
            1,
            SessionUpdate::AgentThoughtChunk(text_chunk("thought-one-again")),
        ));

        let actor_one_thoughts: Vec<(&str, bool)> = state
            .subagents
            .get(&1)
            .expect("actor one")
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                Entry::SubagentThought(thought) => Some((thought.text.as_str(), thought.completed)),
                _ => None,
            })
            .collect();
        let actor_two_thoughts: Vec<(&str, bool)> = state
            .subagents
            .get(&2)
            .expect("actor two")
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                Entry::SubagentThought(thought) => Some((thought.text.as_str(), thought.completed)),
                _ => None,
            })
            .collect();
        assert_eq!(
            actor_one_thoughts,
            [("thought-onethought-one-again", false)]
        );
        assert_eq!(actor_two_thoughts, [("thought-two", false)]);
    }

    #[test]
    fn concurrent_subagent_plans_update_only_their_own_entry() {
        let mut state = AppState::new();
        let plan = |text: &str| {
            SessionUpdate::Plan(Plan::new(vec![PlanEntry::new(
                text,
                PlanEntryPriority::Medium,
                PlanEntryStatus::Pending,
            )]))
        };

        state.apply_event(subagent_session_update_for(1, plan("one-old")));
        state.apply_event(subagent_session_update_for(2, plan("two")));
        state.apply_event(subagent_session_update_for(1, plan("one-new")));

        let plan_for = |id| {
            state
                .subagents
                .get(&id)
                .expect("actor")
                .transcript
                .iter()
                .find_map(|entry| match entry {
                    Entry::SubagentPlan(entries) => {
                        entries.first().map(|entry| entry.content.as_str())
                    }
                    _ => None,
                })
                .expect("plan")
        };
        assert_eq!(plan_for(1), "one-new");
        assert_eq!(plan_for(2), "two");
    }

    #[test]
    fn turn_completion_and_failure_finalize_primary_thoughts() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
            text_chunk("first turn"),
        )));
        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        assert!(matches!(
            &state.transcript[0],
            Entry::AgentThought(thought) if thought.text == "first turn" && thought.completed
        ));

        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
            text_chunk("failed turn"),
        )));
        state.apply_event(UiEvent::PromptFailed {
            message: "boom".to_string(),
        });
        assert!(state.transcript.iter().any(|entry| matches!(
            entry,
            Entry::AgentThought(thought) if thought.text == "failed turn" && thought.completed
        )));
    }

    #[test]
    fn subagent_finish_finalizes_thought_without_reply() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id: 1,
            resumed: false,
            model: None,
            agent: "codex-acp".to_string(),
            objective: String::new(),
            label: "subagent".to_string(),
        }));
        state.apply_event(subagent_session_update(SessionUpdate::AgentThoughtChunk(
            text_chunk("forging"),
        )));
        state.apply_event(subagent_finished(SubagentOutcome::Completed));

        assert!(matches!(
            state.transcript.as_slice(),
            [Entry::System(started), Entry::System(finished)]
                if started.contains("started")
                    && finished.contains("completed")
        ));
        assert!(matches!(
            state.subagents.get(&1).expect("actor").transcript.as_slice(),
            [Entry::SubagentThought(thought), Entry::System(finished)]
                if thought.text == "forging" && thought.completed && finished == "completed"
        ));
    }

    #[test]
    fn primary_and_subagent_thoughts_finalize_without_reordering_each_other() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
            text_chunk("planning"),
        )));
        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id: 1,
            resumed: false,
            model: None,
            agent: "codex-acp".to_string(),
            objective: String::new(),
            label: "subagent".to_string(),
        }));
        state.apply_event(subagent_session_update(SessionUpdate::AgentThoughtChunk(
            text_chunk("forging"),
        )));
        state.apply_event(subagent_session_update(SessionUpdate::AgentMessageChunk(
            text_chunk("built"),
        )));

        assert!(matches!(
            state.transcript.as_slice(),
            [Entry::AgentThought(primary), Entry::System(started)]
                if primary.text == "planning" && !primary.completed
                    && started.contains("subagent #1")
        ));
        assert!(matches!(
            state.subagents.get(&1).expect("actor").transcript.as_slice(),
            [Entry::SubagentThought(sub), Entry::SubagentMessage(message)]
                if sub.text == "forging" && sub.completed && message == "built"
        ));

        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
            text_chunk(" more"),
        )));
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("answer"),
        )));
        assert!(matches!(
            state.transcript.as_slice(),
            [
                Entry::AgentThought(primary),
                Entry::System(started),
                Entry::AgentMessage(primary_message)
            ]
                if primary.text == "planning more" && primary.completed
                    && started.contains("subagent #1")
                    && primary_message == "answer"
        ));
    }

    #[test]
    fn subagent_handoff_has_no_transcript_boundary_or_prompt_block() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id: 1,
            resumed: false,
            model: None,
            agent: "codex-acp".to_string(),
            objective: String::new(),
            label: "subagent".to_string(),
        }));
        // One permanent record, no session boundary and no prompt block.
        assert!(matches!(
            state.transcript.as_slice(),
            [Entry::System(started)] if started == "subagent #1 · subagent · started"
        ));

        state.apply_event(subagent_session_update(SessionUpdate::AgentThoughtChunk(
            text_chunk("forging"),
        )));
        state.apply_event(subagent_session_update(SessionUpdate::AgentMessageChunk(
            text_chunk("done"),
        )));
        state.apply_event(subagent_finished(SubagentOutcome::Completed));

        assert!(matches!(
            state.transcript.as_slice(),
            [Entry::System(_), Entry::System(finished)]
                if finished.starts_with("subagent #1 · subagent · completed · ")
        ));
        assert!(matches!(
            state.subagents.get(&1).expect("actor").transcript.as_slice(),
            [
                Entry::SubagentThought(thought),
                Entry::SubagentMessage(text),
                Entry::System(finished)
            ] if thought.text == "forging" && thought.completed && text == "done"
                && finished == "completed"
        ));
    }

    #[test]
    fn a_started_subagent_populates_durable_state_and_one_transcript_record() {
        let mut state = AppState::new();
        state.apply_event(subagent_started(
            3,
            "fix-tests",
            "Fix the failing parser tests\ndetail",
        ));

        let actor = state.nested_agent(3).expect("durable actor");
        assert_eq!(actor.label, "fix-tests");
        assert_eq!(actor.model.as_deref(), Some("gpt-y"));
        assert_eq!(
            actor.activity, "Fix the failing parser tests\ndetail",
            "the objective seeds the actor's activity until the first update"
        );
        assert!(actor.finished.is_none());
        assert_eq!(state.running_subagent_count(), 1);
        assert!(
            !state.has_active_workflows(),
            "actor prose alone does not mint workflow progress"
        );

        assert!(matches!(
            state.transcript.as_slice(),
            [Entry::System(record)]
                if record == "subagent #3 · fix-tests · started · Fix the failing parser tests"
        ));
    }

    #[test]
    fn nested_agent_viewer_keeps_the_ten_most_recent_actors() {
        let mut state = AppState::new();
        for id in 1..=15 {
            state.apply_event(subagent_started(id, &format!("actor-{id}"), "work"));
        }

        assert_eq!(
            state.nested_agent_viewer_ids(),
            (6..=15).rev().collect::<Vec<_>>()
        );
        assert_eq!(state.nested_agents().count(), 15);
    }

    #[test]
    fn retained_subagent_resume_reuses_its_state_and_start_record() {
        let mut state = AppState::new();
        state.apply_event(subagent_started(3, "review · supervisor", "review"));
        let transcript_len = state.transcript.len();

        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id: 3,
            resumed: true,
            label: "review · supervisor".to_string(),
            model: Some("gpt-y".to_string()),
            agent: "codex-acp".to_string(),
            objective: "vet two automatic reviewer reports".to_string(),
        }));

        assert_eq!(state.transcript.len(), transcript_len);
        let actor = state.nested_agent(3).expect("retained actor");
        assert_eq!(actor.activity, "vet two automatic reviewer reports");
        assert!(actor.finished.is_none());
        assert!(
            state
                .status_line
                .as_ref()
                .is_some_and(|status| status.text.contains("resumed"))
        );
    }

    #[test]
    fn subagent_activity_updates_durable_state_without_touching_the_transcript() {
        let mut state = AppState::new();
        state.apply_event(subagent_started(1, "explore", "look around"));
        let entries = state.transcript.len();
        let revision = state.transcript_revision();

        state.apply_event(subagent_activity(1, "reading src/main.rs"));
        state.apply_event(subagent_activity(1, "running cargo test"));

        assert_eq!(
            state.nested_agent(1).expect("actor").activity,
            "running cargo test"
        );
        assert_eq!(
            state.transcript.len(),
            entries,
            "activity must never append a transcript entry"
        );
        assert_eq!(
            state.transcript_revision(),
            revision,
            "activity must not invalidate already-flushed scrollback"
        );

        // An activity for an unknown id is dropped rather than resurrecting a row.
        state.apply_event(subagent_activity(99, "ghost"));
        assert!(state.nested_agent(99).is_none());
    }

    #[test]
    fn a_finished_subagent_records_and_retains_its_outcome() {
        let mut state = AppState::new();
        state.apply_event(subagent_started(1, "fix-tests", "fix them"));
        state.apply_event(finished_subagent(1, SubagentOutcome::Completed));

        let record = match state.transcript.last() {
            Some(Entry::System(text)) => text.clone(),
            other => panic!("expected a finish record, got {other:?}"),
        };
        assert!(
            record.starts_with("subagent #1 · fix-tests · completed · "),
            "{record}"
        );

        assert_eq!(state.running_subagent_count(), 0);
        assert!(matches!(
            state.nested_agent(1).and_then(SubagentStatus::outcome),
            Some(SubagentOutcome::Completed)
        ));
    }

    #[test]
    fn a_failed_subagent_records_the_first_line_of_its_message() {
        let mut state = AppState::new();
        state.apply_event(subagent_started(2, "build", "build it"));
        state.apply_event(finished_subagent(
            2,
            SubagentOutcome::Failed("adapter exited\nstack trace".to_string()),
        ));

        let record = match state.transcript.last() {
            Some(Entry::System(text)) => text.clone(),
            other => panic!("expected a finish record, got {other:?}"),
        };
        assert!(record.contains("failed: adapter exited"), "{record}");
        assert!(!record.contains("stack trace"), "{record}");
        assert!(matches!(
            state.nested_agent(2).expect("actor").outcome(),
            Some(SubagentOutcome::Failed(_))
        ));
    }

    #[test]
    fn running_subagent_count_excludes_finished_actors() {
        let mut state = AppState::new();
        for id in [1, 2, 3] {
            state.apply_event(subagent_started(id, &format!("lane-{id}"), "work"));
        }
        state.apply_event(finished_subagent(1, SubagentOutcome::Completed));

        assert_eq!(state.running_subagent_count(), 2);
        assert!(matches!(
            state.nested_agent(1).and_then(SubagentStatus::outcome),
            Some(SubagentOutcome::Completed)
        ));
    }

    #[test]
    fn discrete_review_reenters_streaming_so_submissions_queue() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);

        state.apply_event(UiEvent::InternalMessage(InternalMessage {
            source: "primary".to_string(),
            target: "primary".to_string(),
            kind: crate::event::InternalMessageKind::Delegation,
            text: "boundary note".to_string(),
            owner_subagent_id: None,
        }));
        assert_eq!(
            state.connection_state,
            ConnectionState::Ready,
            "delegation notes ride held completions and must not change state"
        );
        assert!(
            state.transcript.is_empty(),
            "primary-owned orchestration packets are intentionally not transcript entries"
        );

        state.apply_event(UiEvent::InternalMessage(InternalMessage {
            source: "primary".to_string(),
            target: "primary".to_string(),
            kind: crate::event::InternalMessageKind::DiscreteReview,
            text: "review the completed work".to_string(),
            owner_subagent_id: None,
        }));
        assert_eq!(state.connection_state, ConnectionState::Streaming);
        assert!(state.is_busy());
        assert!(
            state.transcript.is_empty(),
            "review envelopes stay hidden behind typed workflow summaries"
        );

        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        assert_eq!(state.connection_state, ConnectionState::Ready);
    }

    #[test]
    fn internal_coordination_is_retained_only_by_its_nested_actor() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::InternalMessage(InternalMessage {
            source: "primary".to_string(),
            target: "subagent".to_string(),
            kind: crate::event::InternalMessageKind::Delegation,
            text: "implementation brief".to_string(),
            owner_subagent_id: Some(1),
        }));
        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id: 1,
            resumed: false,
            model: None,
            agent: "codex-acp".to_string(),
            objective: String::new(),
            label: "subagent".to_string(),
        }));
        state.apply_event(subagent_session_update(SessionUpdate::AgentMessageChunk(
            text_chunk("working"),
        )));

        assert!(matches!(
            state.transcript.as_slice(),
            [Entry::System(started)] if started.contains("subagent #1")
        ));
        assert!(matches!(
            state.subagents.get(&1).expect("actor").transcript.as_slice(),
            [Entry::InternalMessage(message), Entry::SubagentMessage(text)]
                if message.source == "primary"
                    && message.target == "subagent"
                    && message.text == "implementation brief"
                    && text == "working"
        ));
        assert!(
            !state
                .transcript
                .iter()
                .any(|entry| matches!(entry, Entry::SessionBoundary(_)))
        );
    }

    #[test]
    fn nested_warnings_are_private_detail_and_primary_visible_alerts() {
        let mut state = AppState::new();
        state.apply_event(subagent_started(7, "build", "compile the workspace"));
        state.apply_event(UiEvent::Subagent(SubagentEvent::Status {
            subagent_id: 7,
            kind: SubagentStatusKind::Info,
            message: "checking dependencies".to_string(),
        }));
        state.apply_event(UiEvent::Subagent(SubagentEvent::Status {
            subagent_id: 7,
            kind: SubagentStatusKind::Warning,
            message: "adapter authentication expires soon".to_string(),
        }));

        let actor = state.nested_agent(7).expect("nested actor");
        assert!(matches!(
            actor.transcript.as_slice(),
            [Entry::System(info), Entry::System(warning)]
                if info == "checking dependencies"
                    && warning == "adapter authentication expires soon"
        ));
        assert!(
            !state.transcript.iter().any(
                |entry| matches!(entry, Entry::System(text) if text == "checking dependencies")
            ),
            "informational nested status remains private"
        );
        assert!(state.transcript.iter().any(|entry| matches!(
            entry,
            Entry::System(text)
                if text
                    == "warning: subagent #7 · adapter authentication expires soon"
        )));
        assert!(matches!(
            state.status_line,
            Some(StatusMessage {
                kind: StatusKind::Warning,
                ref text,
            }) if text == "subagent #7 · adapter authentication expires soon"
        ));
    }

    #[test]
    fn delegation_terminal_summary_matches_the_authoritative_outcome() {
        use crate::workflow::{
            WorkflowCoverage, WorkflowId, WorkflowKind, WorkflowOutcome, WorkflowPhase,
            WorkflowStage, WorkflowTransition,
        };

        for (turn_id, outcome, coverage, expected) in [
            (
                1,
                WorkflowOutcome::Completed,
                WorkflowCoverage::Complete,
                "subagents complete",
            ),
            (
                2,
                WorkflowOutcome::Failed,
                WorkflowCoverage::Degraded,
                "subagents failed · degraded coverage",
            ),
            (
                3,
                WorkflowOutcome::Cancelled,
                WorkflowCoverage::Degraded,
                "subagents cancelled · degraded coverage",
            ),
        ] {
            let mut state = AppState::new();
            let workflow_id = WorkflowId::delegation(turn_id);
            apply_workflow(
                &mut state,
                workflow_id,
                WorkflowTransition::Started {
                    kind: WorkflowKind::Delegation,
                    stage: WorkflowStage::new(0, WorkflowPhase::Delegating),
                },
            );
            apply_workflow(
                &mut state,
                workflow_id,
                WorkflowTransition::Terminal { outcome, coverage },
            );

            assert!(matches!(
                state.transcript.last(),
                Some(Entry::System(summary)) if summary == expected
            ));
            assert!(
                !state.transcript.iter().any(
                    |entry| matches!(entry, Entry::System(summary) if summary.contains("F11"))
                ),
                "permanent scrollback must not bake in a keybinding hint"
            );
        }
    }

    #[test]
    fn next_prompt_retires_only_terminal_workflow_rows_and_clock_stays_frozen() {
        use crate::workflow::{
            WorkflowCoverage, WorkflowId, WorkflowKind, WorkflowOutcome, WorkflowPhase,
            WorkflowStage, WorkflowTransition,
        };

        let mut state = AppState::new();
        let unfinished = WorkflowId::delegation(10);
        let terminal = WorkflowId::review(10);
        for (workflow_id, kind, phase) in [
            (
                unfinished,
                WorkflowKind::Delegation,
                WorkflowPhase::Delegating,
            ),
            (terminal, WorkflowKind::Review, WorkflowPhase::Supervision),
        ] {
            apply_workflow(
                &mut state,
                workflow_id,
                WorkflowTransition::Started {
                    kind,
                    stage: WorkflowStage::new(0, phase),
                },
            );
        }
        apply_workflow(
            &mut state,
            terminal,
            WorkflowTransition::Terminal {
                outcome: WorkflowOutcome::Clean,
                coverage: WorkflowCoverage::Complete,
            },
        );

        let now = Instant::now();
        assert_eq!(
            state.workflow_elapsed_at(terminal, now),
            state.workflow_elapsed_at(terminal, now + Duration::from_secs(60)),
            "terminal elapsed time must stay frozen"
        );

        state.record_user_prompt("next turn".to_string());
        let visible = state
            .visible_workflows()
            .map(|workflow| workflow.id)
            .collect::<Vec<_>>();
        assert_eq!(visible, vec![unfinished]);
        assert!(state.workflows.get(terminal).is_some());
    }

    #[test]
    fn session_switch_hides_progress_without_clearing_in_flight_reducer_state() {
        use crate::workflow::{
            WorkflowActorId, WorkflowActorRole, WorkflowCoverage, WorkflowId, WorkflowKind,
            WorkflowOutcome, WorkflowPhase, WorkflowStage, WorkflowTransition,
        };

        let mut state = AppState::new();
        state.apply_event(UiEvent::SessionStarted {
            session_id: "one".to_string(),
            resumed: false,
        });
        let workflow_id = WorkflowId::delegation(20);
        apply_workflow(
            &mut state,
            workflow_id,
            WorkflowTransition::Started {
                kind: WorkflowKind::Delegation,
                stage: WorkflowStage::new(0, WorkflowPhase::Delegating),
            },
        );

        state.apply_event(UiEvent::SessionStarted {
            session_id: "two".to_string(),
            resumed: false,
        });
        assert!(state.visible_workflows().next().is_none());
        assert!(state.workflows.get(workflow_id).is_some());

        let actor_id = WorkflowActorId::Subagent(77);
        apply_workflow(
            &mut state,
            workflow_id,
            WorkflowTransition::ActorStarted {
                actor_id: actor_id.clone(),
                role: WorkflowActorRole::Implementation,
            },
        );
        assert_eq!(
            state.nested_agent(77).and_then(|actor| actor.role.clone()),
            Some(WorkflowActorRole::Implementation)
        );
        apply_workflow(
            &mut state,
            workflow_id,
            WorkflowTransition::ActorFinished {
                actor_id,
                outcome: SubagentOutcome::Completed,
            },
        );
        apply_workflow(
            &mut state,
            workflow_id,
            WorkflowTransition::Terminal {
                outcome: WorkflowOutcome::Completed,
                coverage: WorkflowCoverage::Complete,
            },
        );

        assert_eq!(
            state
                .workflows
                .get(workflow_id)
                .and_then(|workflow| workflow.outcome),
            Some(WorkflowOutcome::Completed)
        );
        assert!(matches!(
            state.transcript.last(),
            Some(Entry::System(summary)) if summary == "subagents complete · 1 completed"
        ));
    }

    #[test]
    fn workflow_roles_and_supervisor_packets_attach_to_stable_nested_actors() {
        use crate::workflow::{
            WorkflowActorId, WorkflowActorRole, WorkflowCoverage, WorkflowEvent, WorkflowId,
            WorkflowKind, WorkflowOutcome, WorkflowPhase, WorkflowStage, WorkflowTransition,
        };

        let mut state = AppState::new();
        let workflow_id = WorkflowId::review(9);
        state.apply_event(UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::Started {
                kind: WorkflowKind::Review,
                stage: WorkflowStage::new(0, WorkflowPhase::Supervision),
            },
        )));
        state.apply_event(UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::ActorStarted {
                actor_id: WorkflowActorId::Subagent(42),
                role: WorkflowActorRole::ReviewSupervisor,
            },
        )));
        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id: 42,
            resumed: false,
            model: Some("gpt-review".to_string()),
            agent: "codex-acp".to_string(),
            objective: "review the patch".to_string(),
            label: "review · supervisor".to_string(),
        }));
        assert_eq!(
            state.running_subagent_count(),
            0,
            "an internal supervisor must not count as a subagent"
        );
        assert!(!state.subagent_active);
        for (kind, text) in [
            (
                crate::event::InternalMessageKind::ReviewLane,
                "intent brief",
            ),
            (
                crate::event::InternalMessageKind::ReviewProgress,
                "two specialists remain",
            ),
            (
                crate::event::InternalMessageKind::ReviewSynthesis,
                "No material findings.",
            ),
        ] {
            state.apply_event(UiEvent::InternalMessage(InternalMessage {
                source: "review supervisor".to_string(),
                target: "primary".to_string(),
                kind,
                text: text.to_string(),
                owner_subagent_id: Some(42),
            }));
        }

        let actor = state.nested_agent(42).expect("supervisor actor");
        assert_eq!(actor.role, Some(WorkflowActorRole::ReviewSupervisor));
        assert_eq!(actor.adapter, "codex-acp");
        assert_eq!(actor.model.as_deref(), Some("gpt-review"));
        assert!(matches!(
            actor.transcript.as_slice(),
            [Entry::InternalMessage(intent), Entry::InternalMessage(progress), Entry::InternalMessage(synthesis)]
                if intent.kind == crate::event::InternalMessageKind::ReviewLane
                    && intent.text == "intent brief"
                    && progress.kind == crate::event::InternalMessageKind::ReviewProgress
                    && synthesis.kind == crate::event::InternalMessageKind::ReviewSynthesis
        ));
        assert!(
            !state.transcript.iter().any(|entry| matches!(
                entry,
                Entry::InternalMessage(message)
                    if message.text == "intent brief"
                        || message.text == "two specialists remain"
                        || message.text == "No material findings."
            )),
            "review envelopes stay out of the primary transcript"
        );

        state.apply_event(UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::ActorFinished {
                actor_id: WorkflowActorId::Subagent(42),
                outcome: SubagentOutcome::Completed,
            },
        )));
        state.apply_event(UiEvent::Subagent(SubagentEvent::Finished {
            subagent_id: 42,
            outcome: SubagentOutcome::Completed,
        }));
        state.apply_event(UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::Terminal {
                outcome: WorkflowOutcome::Clean,
                coverage: WorkflowCoverage::Complete,
            },
        )));
        let summaries = state
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                Entry::System(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            summaries
                .iter()
                .any(|text| text.starts_with("review supervisor #42 · started"))
        );
        assert!(
            summaries
                .iter()
                .any(|text| text.starts_with("review supervisor #42 · completed"))
        );
        assert!(summaries.contains(&"review complete · no material findings"));
    }

    #[test]
    fn review_issue_lifecycle_fossilizes_ledger_records_and_verdict_banner() {
        use crate::workflow::{
            ReviewIssueStatus, WorkflowCoverage, WorkflowEvent, WorkflowId, WorkflowKind,
            WorkflowOutcome, WorkflowPhase, WorkflowStage, WorkflowTransition,
        };

        fn ledger_texts(state: &AppState) -> Vec<String> {
            state
                .transcript
                .iter()
                .filter_map(|entry| match entry {
                    Entry::ReviewLedger(lines) => Some(
                        lines
                            .iter()
                            .map(ReviewLedgerLine::plain_text)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    _ => None,
                })
                .collect()
        }

        let mut state = AppState::new();
        let workflow_id = WorkflowId::review(4);
        state.apply_event(UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::Started {
                kind: WorkflowKind::Review,
                stage: WorkflowStage::new(0, WorkflowPhase::Supervision),
            },
        )));
        state.apply_event(UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::IssuesValidated {
                pass: 0,
                summaries: vec![
                    "cache write races the eviction sweep".to_string(),
                    "retry budget off by one".to_string(),
                ],
            },
        )));

        let records = ledger_texts(&state);
        assert_eq!(records.len(), 1, "validated findings become a record");
        assert!(
            records[0].contains("review pass 1 · 2 validated issues"),
            "{records:?}"
        );
        assert!(
            records[0].contains("#1 cache write races the eviction sweep"),
            "{records:?}"
        );
        assert!(
            records[0].contains("#2 retry budget off by one"),
            "{records:?}"
        );

        state.apply_event(UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::IssuesResolved {
                pass: 0,
                status: ReviewIssueStatus::Invalidated,
                reason: Some("correction turn changed nothing in the workspace".to_string()),
            },
        )));
        let records = ledger_texts(&state);
        assert_eq!(records.len(), 2, "the pass verdict becomes a record");
        assert!(
            records[1].contains(
                "2 issues invalidated — correction turn changed nothing in the workspace"
            ),
            "{records:?}"
        );
        let struck = state
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                Entry::ReviewLedger(lines) => Some(lines.iter()),
                _ => None,
            })
            .flatten()
            .flat_map(|line| line.spans.iter())
            .any(|(_, tone)| *tone == ReviewTone::Struck);
        assert!(struck, "invalidated summaries render struck through");

        state.apply_event(UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::Terminal {
                outcome: WorkflowOutcome::Completed,
                coverage: WorkflowCoverage::Complete,
            },
        )));
        let records = ledger_texts(&state);
        assert_eq!(
            records.len(),
            3,
            "a review with findings ends in the banner"
        );
        assert!(records[2].contains("review verdict"), "{records:?}");
        assert!(
            records[2].contains("review complete · 2 issues · 2 invalidated"),
            "{records:?}"
        );
        assert!(
            !state.transcript.iter().any(|entry| matches!(
                entry,
                Entry::System(text) if text.starts_with("review complete")
            )),
            "the banner replaces the bare system notice"
        );
    }

    #[test]
    fn implementation_role_after_started_preserves_the_real_actor_label() {
        use crate::workflow::{
            WorkflowActorId, WorkflowActorRole, WorkflowEvent, WorkflowId, WorkflowKind,
            WorkflowPhase, WorkflowStage, WorkflowTransition,
        };

        let mut state = AppState::new();
        state.apply_event(subagent_started(11, "subagent", "implement the parser"));
        let workflow_id = WorkflowId::delegation(4);
        state.apply_event(UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::Started {
                kind: WorkflowKind::Delegation,
                stage: WorkflowStage::new(0, WorkflowPhase::Delegating),
            },
        )));
        state.apply_event(UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::ActorStarted {
                actor_id: WorkflowActorId::Subagent(11),
                role: WorkflowActorRole::Implementation,
            },
        )));

        let actor = state.nested_agent(11).expect("implementation actor");
        assert_eq!(actor.label, "subagent");
        assert!(!actor.label_is_placeholder);
        assert_eq!(actor.role, Some(WorkflowActorRole::Implementation));
    }

    #[test]
    fn specialist_report_summary_keeps_the_actor_id() {
        use crate::workflow::{
            WorkflowActorId, WorkflowActorRole, WorkflowEvent, WorkflowId, WorkflowKind,
            WorkflowPhase, WorkflowStage, WorkflowTransition,
        };

        let mut state = AppState::new();
        let workflow_id = WorkflowId::review(5);
        state.apply_event(UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::Started {
                kind: WorkflowKind::Review,
                stage: WorkflowStage::new(0, WorkflowPhase::SpecialistReview),
            },
        )));
        state.apply_event(UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::ActorStarted {
                actor_id: WorkflowActorId::Subagent(12),
                role: WorkflowActorRole::SpecialistReviewer {
                    lane: "Týr".to_string(),
                },
            },
        )));
        state.apply_event(subagent_started(12, "review · Týr", "inspect correctness"));
        state.apply_event(UiEvent::Subagent(SubagentEvent::Finished {
            subagent_id: 12,
            outcome: SubagentOutcome::Completed,
        }));

        assert!(state.transcript.iter().any(|entry| matches!(
            entry,
            Entry::System(text) if text.starts_with("reviewer Týr #12 · report delivered")
        )));
    }

    #[test]
    fn tool_call_update_merges() {
        let mut s = AppState::new();
        let tc = ToolCall::new("call-1", "running ls");
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ToolCall(tc)));
        let mut fields = agent_client_protocol::schema::v1::ToolCallUpdateFields::default();
        fields.status = Some(ToolCallStatus::Completed);
        let update = ToolCallUpdate::new("call-1", fields);
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(
            update,
        )));
        let view = s.tool_calls.get("call-1").expect("view");
        assert_eq!(view.status, ToolCallStatus::Completed);
        assert_eq!(view.title, "running ls");
    }

    #[test]
    fn primary_create_subagent_transport_call_is_tracked_but_not_transcribed() {
        let mut state = AppState::new();
        let call = ToolCall::new("bridge-call", "mcp.mj-subagents.create_subagent")
            .status(ToolCallStatus::InProgress)
            .raw_input(serde_json::json!({
                "server": "mj-subagents",
                "tool": "create_subagent",
                "arguments": { "prompt": "build it" }
            }));

        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::ToolCall(call)));

        assert!(state.tool_calls.contains_key("bridge-call"));
        assert!(state.transcript.is_empty());

        let fields = agent_client_protocol::schema::v1::ToolCallUpdateFields::default()
            .status(ToolCallStatus::Completed);
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(
            ToolCallUpdate::new("bridge-call", fields),
        )));

        assert_eq!(
            state.tool_calls.get("bridge-call").expect("bridge").status,
            ToolCallStatus::Completed
        );
        assert!(state.transcript.is_empty());
    }

    #[test]
    fn primary_subagent_cancel_transport_call_is_tracked_but_not_transcribed() {
        let mut state = AppState::new();
        let call = ToolCall::new("cancel-bridge", "mcp__mj-subagents__subagent_cancel")
            .status(ToolCallStatus::InProgress)
            .raw_input(serde_json::json!({
                "server": "mj-subagents",
                "tool": "subagent_cancel",
                "arguments": { "subagent_id": 42 }
            }));

        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::ToolCall(call)));

        assert!(state.tool_calls.contains_key("cancel-bridge"));
        assert!(state.transcript.is_empty());
    }

    #[test]
    fn similarly_named_mcp_tools_are_not_filtered_as_subagent_transport() {
        let call = ToolCall::new("other-tool", "mcp.mj-subagents.create_subagent_extra");
        let cancel = ToolCall::new("other-cancel", "mcp.mj-subagents.subagent_cancel_extra");

        assert!(!is_subagent_transport_call(&call));
        assert!(!is_subagent_transport_call(&cancel));
    }

    #[test]
    fn claude_subagent_transport_update_before_create_is_not_transcribed() {
        let mut state = AppState::new();
        let fields = agent_client_protocol::schema::v1::ToolCallUpdateFields::default()
            .title("Running MCP tool")
            .status(ToolCallStatus::InProgress);
        let mut meta = serde_json::Map::new();
        meta.insert(
            "claudeCode".to_string(),
            serde_json::json!({ "toolName": "mcp__mj-subagents__create_subagent" }),
        );
        let mut update = ToolCallUpdate::new("claude-bridge", fields);
        update.meta = Some(meta);
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(
            update,
        )));

        assert!(state.tool_calls.contains_key("claude-bridge"));
        assert!(state.transcript.is_empty());
    }

    #[test]
    fn subagent_tool_ids_are_isolated_from_primary_tools() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::ToolCall(
            ToolCall::new("shared-id", "primary tool"),
        )));
        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id: 1,
            resumed: false,
            model: None,
            agent: "codex-acp".to_string(),
            objective: String::new(),
            label: "codex".to_string(),
        }));
        state.apply_event(subagent_session_update(SessionUpdate::ToolCall(
            ToolCall::new("shared-id", "nested tool"),
        )));

        assert_eq!(
            state.tool_calls.get("shared-id").expect("primary").title,
            "primary tool"
        );
        assert_eq!(
            state
                .tool_calls
                .get("subagent-1:shared-id")
                .expect("nested")
                .title,
            "nested tool"
        );
    }

    #[test]
    fn whole_turn_enters_cancelling_while_a_subagent_is_active() {
        let mut state = AppState::new();
        state.record_user_prompt("delegate".to_string());
        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id: 1,
            resumed: false,
            model: None,
            agent: "codex-acp".to_string(),
            objective: String::new(),
            label: "codex".to_string(),
        }));
        state.mark_cancelling();
        assert_eq!(state.connection_state, ConnectionState::Cancelling);
    }

    #[test]
    fn prompt_done_returns_to_idle() {
        let mut s = AppState::new();
        s.record_user_prompt("test".to_string());
        assert!(s.is_streaming());
        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        assert!(!s.is_streaming());
    }

    #[test]
    fn cancelled_prompt_marks_unfinished_tool_calls_failed() {
        let mut s = AppState::new();
        s.record_user_prompt("run command".to_string());
        s.tool_calls.insert(
            "call-1".to_string(),
            ToolCallView {
                title: "cargo test".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::InProgress,
                body: vec![ToolCallOutput::Text("running".to_string())],
            },
        );
        s.transcript.push(Entry::ToolCall("call-1".to_string()));

        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::Cancelled,
            usage: None,
        });

        let view = s.tool_calls.get("call-1").expect("tool call");
        assert_eq!(view.status, ToolCallStatus::Failed);
        assert!(
            view.body
                .iter()
                .any(|output| matches!(output, ToolCallOutput::Note(note) if note == "tool call ended before completion"))
        );
    }

    #[test]
    fn streaming_updates_preserve_manual_scroll_offset() {
        let mut s = AppState::new();
        s.scroll_offset = 12;

        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("hello"),
        )));

        assert_eq!(s.scroll_offset, 12);
    }

    #[test]
    fn content_block_variants_render_with_visible_placeholders() {
        // PLANS.md M2 calls for ContentBlock variants beyond Text to
        // degrade visibly instead of silently panicking. This pumps each
        // known variant through `AgentMessageChunk` and asserts the
        // transcript shows a labelled placeholder so the user knows
        // something was sent even if we can't render it inline yet.
        let blocks: Vec<(ContentBlock, &str)> = vec![
            (ContentBlock::Text(TextContent::new("hi")), "hi"),
            (
                ContentBlock::Image(ImageContent::new("data", "image/png")),
                "[image]",
            ),
            (
                ContentBlock::Audio(AudioContent::new("data", "audio/wav")),
                "[audio]",
            ),
            (
                ContentBlock::ResourceLink(ResourceLink::new("readme", "file:///readme.md")),
                "[link file:///readme.md]",
            ),
            (
                ContentBlock::Resource(EmbeddedResource::new(
                    EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                        "snippet",
                        "file:///snippet.txt",
                    )),
                )),
                "[resource]",
            ),
        ];

        for (block, expected_substring) in blocks {
            let mut s = AppState::new();
            s.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(block.clone()),
            )));
            assert_eq!(
                s.transcript.len(),
                1,
                "block {block:?} produced an empty transcript"
            );
            match &s.session.transcript[0] {
                Entry::AgentMessage(text) => assert!(
                    text.contains(expected_substring),
                    "block {block:?} rendered as {text:?}, expected substring {expected_substring:?}"
                ),
                other => panic!("block {block:?} produced unexpected entry: {other:?}"),
            }
        }
    }

    #[test]
    fn agent_chunks_keep_folding_while_permission_modal_is_open() {
        // The permission modal owns the keyboard but must NOT block the
        // ACP event pipeline -- chunks streamed concurrently with the
        // prompt that triggered the modal still belong in the transcript.
        // Otherwise scrolling back to read what led to the prompt would
        // show a gap.
        let mut s = AppState::new();
        let (prompt, _rx) = permission_prompt_with_id("call-1");
        s.apply_event(UiEvent::PermissionRequest(prompt));
        assert!(s.has_pending_permission());

        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("thinking..."),
        )));

        assert!(s.has_pending_permission(), "modal must remain queued");
        assert_eq!(s.session.transcript.len(), 1);
        match &s.session.transcript[0] {
            Entry::AgentMessage(text) => assert_eq!(text, "thinking..."),
            other => panic!("unexpected entry: {other:?}"),
        }
    }

    /// Build a tool call that owns one terminal, the shape `/terminals` reads.
    fn terminal_tool_call(call_id: &'static str, title: &str, terminal_id: &str) -> UiEvent {
        let mut fields = agent_client_protocol::schema::v1::ToolCallUpdateFields::default();
        fields.title = Some(title.to_string());
        fields.content = Some(vec![ToolCallContent::Terminal(Terminal::new(
            agent_client_protocol::schema::v1::TerminalId::new(terminal_id.to_string()),
        ))]);
        UiEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            call_id, fields,
        )))
    }

    /// A terminal is registered the moment the agent starts it, not when it
    /// exits — which is the whole point, since a background one never exits.
    #[test]
    fn starting_a_terminal_registers_it_as_running() {
        let mut state = AppState::new();
        state.apply_event(terminal_tool_call("call-1", "npm run dev", "term-1"));

        let summaries = state.terminal_summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].label, "npm run dev");
        assert!(summaries[0].is_running(), "no exit status means running");
        assert_eq!(state.running_terminal_count(), 1);
    }

    #[test]
    fn terminal_output_and_exit_status_reach_the_viewer() {
        let mut state = AppState::new();
        state.apply_event(terminal_tool_call("call-1", "cargo test", "term-1"));
        state.apply_event(UiEvent::TerminalOutput(TerminalOutputSnapshot {
            terminal_id: "term-1".to_string(),
            output: "running 3 tests".to_string(),
            truncated: false,
            exit_status: None,
        }));

        let summaries = state.terminal_summaries();
        assert_eq!(state.terminal_output_at(0), Some("running 3 tests"));
        assert!(summaries[0].is_running());

        state.apply_event(UiEvent::TerminalOutput(TerminalOutputSnapshot {
            terminal_id: "term-1".to_string(),
            output: "test result: ok".to_string(),
            truncated: false,
            exit_status: Some(TerminalExitStatus::new().exit_code(0)),
        }));
        let summaries = state.terminal_summaries();
        assert!(!summaries[0].is_running(), "an exit status ends the run");
        assert_eq!(state.running_terminal_count(), 0);
    }

    /// Running terminals are the ones the user needs to reach, so they lead.
    #[test]
    fn running_terminals_sort_ahead_of_finished_ones() {
        let mut state = AppState::new();
        state.apply_event(terminal_tool_call("call-1", "finished", "term-1"));
        state.apply_event(terminal_tool_call("call-2", "still going", "term-2"));
        state.apply_event(UiEvent::TerminalOutput(TerminalOutputSnapshot {
            terminal_id: "term-1".to_string(),
            output: String::new(),
            truncated: false,
            exit_status: Some(TerminalExitStatus::new().exit_code(0)),
        }));

        let summaries = state.terminal_summaries();
        let labels: Vec<&str> = summaries
            .iter()
            .map(|summary| summary.label.as_str())
            .collect();
        assert_eq!(labels, vec!["still going", "finished"]);
    }

    #[test]
    fn terminals_viewer_opens_only_when_there_is_something_to_show() {
        let mut state = AppState::new();
        assert!(
            !state.open_terminals_viewer(),
            "opening must fail with no terminals so the caller can explain"
        );
        assert!(!state.terminals_viewer);

        state.apply_event(terminal_tool_call("call-1", "npm run dev", "term-1"));
        assert!(state.open_terminals_viewer());
        assert!(state.terminals_viewer);

        state.close_terminals_viewer();
        assert!(!state.terminals_viewer);
        assert_eq!(state.terminals_scroll_offset, 0);
    }

    #[test]
    fn selecting_terminals_wraps_in_both_directions() {
        let mut state = AppState::new();
        state.apply_event(terminal_tool_call("call-1", "first", "term-1"));
        state.apply_event(terminal_tool_call("call-2", "second", "term-2"));
        assert!(state.open_terminals_viewer());
        assert_eq!(state.terminals_selected, 0);

        state.select_terminal(true);
        assert_eq!(state.terminals_selected, 1);
        state.select_terminal(true);
        assert_eq!(state.terminals_selected, 0, "forward wraps to the start");
        state.select_terminal(false);
        assert_eq!(state.terminals_selected, 1, "backward wraps to the end");
    }

    /// Opening another reader must not leave two viewers believing they own
    /// the screen.
    #[test]
    fn opening_another_viewer_closes_the_terminals_reader() {
        let mut state = AppState::new();
        state.apply_event(terminal_tool_call("call-1", "npm run dev", "term-1"));
        assert!(state.open_terminals_viewer());

        state.open_workspace_diff_viewer();
        assert!(!state.terminals_viewer);
        assert!(state.workspace_diff_viewer);
    }

    #[test]
    fn tool_call_content_diff_and_terminal_are_kept_structured() {
        let mut s = AppState::new();
        let mut fields = agent_client_protocol::schema::v1::ToolCallUpdateFields::default();
        fields.content = Some(vec![
            ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(
                "stdout: ok",
            )))),
            ToolCallContent::Diff(
                Diff::new("/tmp/file.rs", "new contents")
                    .old_text(Some("old contents".to_string())),
            ),
            ToolCallContent::Terminal(Terminal::new(
                agent_client_protocol::schema::v1::TerminalId::new("term-1"),
            )),
        ]);
        let update = ToolCallUpdate::new("call-1", fields);
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(
            update,
        )));

        let view = s.tool_calls.get("call-1").expect("view");
        assert_eq!(view.body.len(), 3);
        assert_eq!(view.body[0], ToolCallOutput::Text("stdout: ok".to_string()));
        assert_eq!(
            view.body[1],
            ToolCallOutput::Diff {
                path: "/tmp/file.rs".to_string(),
                old_text: Some("old contents".to_string()),
                new_text: "new contents".to_string(),
            }
        );
        assert_eq!(
            view.body[2],
            ToolCallOutput::Terminal {
                terminal_id: "term-1".to_string(),
                output: String::new(),
                truncated: false,
                exit_status: None,
            }
        );
    }

    #[test]
    fn terminal_output_snapshot_updates_matching_tool_call() {
        let mut s = AppState::new();
        let mut fields = agent_client_protocol::schema::v1::ToolCallUpdateFields::default();
        fields.content = Some(vec![
            ToolCallContent::Terminal(Terminal::new(
                agent_client_protocol::schema::v1::TerminalId::new("term-1"),
            )),
            ToolCallContent::Terminal(Terminal::new(
                agent_client_protocol::schema::v1::TerminalId::new("other"),
            )),
        ]);
        let update = ToolCallUpdate::new("call-1", fields);
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(
            update,
        )));
        let before = s.transcript_revision();

        s.apply_event(UiEvent::TerminalOutput(TerminalOutputSnapshot {
            terminal_id: "term-1".to_string(),
            output: "hello\n".to_string(),
            truncated: true,
            exit_status: Some(TerminalExitStatus::new().exit_code(0)),
        }));

        assert_ne!(s.transcript_revision(), before);
        let view = s.tool_calls.get("call-1").expect("view");
        assert_eq!(
            view.body[0],
            ToolCallOutput::Terminal {
                terminal_id: "term-1".to_string(),
                output: "hello\n".to_string(),
                truncated: true,
                exit_status: Some(TerminalExitStatus::new().exit_code(0)),
            }
        );
        assert_eq!(
            view.body[1],
            ToolCallOutput::Terminal {
                terminal_id: "other".to_string(),
                output: String::new(),
                truncated: false,
                exit_status: None,
            }
        );
    }

    #[test]
    fn terminal_output_snapshot_is_applied_to_later_tool_call() {
        let mut s = AppState::new();
        s.apply_event(UiEvent::TerminalOutput(TerminalOutputSnapshot {
            terminal_id: "term-1".to_string(),
            output: "already done".to_string(),
            truncated: false,
            exit_status: Some(TerminalExitStatus::new().exit_code(0)),
        }));

        let mut fields = agent_client_protocol::schema::v1::ToolCallUpdateFields::default();
        fields.content = Some(vec![ToolCallContent::Terminal(Terminal::new(
            agent_client_protocol::schema::v1::TerminalId::new("term-1"),
        ))]);
        let update = ToolCallUpdate::new("call-1", fields);
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(
            update,
        )));

        let view = s.tool_calls.get("call-1").expect("view");
        assert_eq!(
            view.body[0],
            ToolCallOutput::Terminal {
                terminal_id: "term-1".to_string(),
                output: "already done".to_string(),
                truncated: false,
                exit_status: Some(TerminalExitStatus::new().exit_code(0)),
            }
        );
    }

    #[test]
    fn fatal_event_sets_fatal_status_and_closes_runtime() {
        let mut s = AppState::new();
        s.autocomplete.visible = true;
        // Queue a real permission prompt via the production event path
        // rather than poking the field directly; same shape as what the
        // runtime would send.
        s.apply_event(UiEvent::PermissionRequest(permission_prompt()));
        assert!(s.has_pending_permission());

        s.apply_event(UiEvent::Fatal("boom".to_string()));

        assert!(s.runtime_closed);
        assert!(!s.is_streaming());
        assert_eq!(s.connection_state, ConnectionState::Fatal);
        assert!(!s.has_pending_permission());
        assert!(!s.autocomplete.visible);
        assert_eq!(s.session.transcript.len(), 1);
        match &s.session.transcript[0] {
            Entry::System(text) => assert_eq!(text, "fatal: boom"),
            other => panic!("unexpected entry: {other:?}"),
        }
        let status = s.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Fatal);
        assert_eq!(status.text, "boom");
    }

    #[test]
    fn config_option_update_refreshes_session_state() {
        let mut s = AppState::new();
        let options = vec![
            SessionConfigOption::select(
                "mode",
                "Session Mode",
                "ask",
                vec![
                    SessionConfigSelectOption::new("ask", "Ask"),
                    SessionConfigSelectOption::new("code", "Code"),
                ],
            )
            .category(Some(SessionConfigOptionCategory::Mode)),
            SessionConfigOption::select(
                "model",
                "Model",
                "model-1",
                vec![
                    SessionConfigSelectOption::new("model-1", "Model 1"),
                    SessionConfigSelectOption::new("model-2", "Model 2"),
                ],
            )
            .category(Some(SessionConfigOptionCategory::Model)),
        ];

        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ConfigOptionUpdate(
            ConfigOptionUpdate::new(options),
        )));

        assert_eq!(s.session_config_options.len(), 1);
        assert_eq!(s.current_mode.as_deref(), Some("ask"));
        assert!(s.status_line.is_none());
    }

    #[test]
    fn harness_owned_permission_option_stays_out_of_session_picker() {
        let mut s = AppState::new();
        let option = SessionConfigOption::select(
            "mode",
            "Mode",
            "agent",
            vec![SessionConfigSelectOption::new("agent", "Agent")],
        )
        .category(Some(SessionConfigOptionCategory::Mode));
        s.apply_event(UiEvent::SessionConfigOptions {
            options: vec![option.clone()],
            targets: vec![SessionConfigTarget::ConfigOption {
                config_id: "mode".into(),
            }],
            hidden_config_ids: vec!["mode".to_string()],
        });
        assert!(s.session_config_options.is_empty());

        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ConfigOptionUpdate(
            ConfigOptionUpdate::new(vec![option]),
        )));
        assert!(s.session_config_options.is_empty());
    }

    #[test]
    fn connected_session_config_replaces_probe_inventory_and_updates_open_menu() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut config = crate::roster::config_with_a_visible_builtin();
        let mut inventory = crate::roster::discover_inventory(&config);
        let server = inventory.servers.first_mut().expect("visible ACP server");
        let source_id = server.id.clone();
        config.agent.acp_source = Some(source_id.clone());
        config.save(&path).expect("save config");

        let mut state = AppState::new();
        state.config_path = Some(path);
        state.agent_source_id = source_id;
        state.acp_inventory = inventory;
        state.open_mjconfig_menu();

        let option = SessionConfigOption::select(
            "service_tier",
            "Service tier",
            "priority",
            vec![SessionConfigSelectOption::new("priority", "Priority")],
        );
        state.apply_event(UiEvent::SessionConfigOptions {
            options: vec![option],
            targets: vec![SessionConfigTarget::ConfigOption {
                config_id: "service_tier".into(),
            }],
            hidden_config_ids: Vec::new(),
        });

        let server = state
            .acp_inventory
            .servers
            .iter()
            .find(|server| server.id == state.agent_source_id)
            .expect("connected server");
        assert_eq!(server.session_config[0].id.to_string(), "service_tier");
        assert_eq!(
            state
                .mjconfig_menu
                .as_ref()
                .expect("open menu")
                .editor
                .session_option_rows(crate::settings::SessionDefaultsSeat::Primary)
                .len(),
            1
        );

        state.apply_event(UiEvent::SessionConfigOptions {
            options: Vec::new(),
            targets: Vec::new(),
            hidden_config_ids: Vec::new(),
        });
        let server = state
            .acp_inventory
            .servers
            .iter()
            .find(|server| server.id == state.agent_source_id)
            .expect("connected server");
        assert!(server.session_config.is_empty());
    }

    #[test]
    fn config_option_update_hides_thought_level_from_primary_shortcuts() {
        let mut s = AppState::new();
        let options = vec![
            SessionConfigOption::select(
                "thinking",
                "Thinking",
                "medium",
                vec![
                    SessionConfigSelectOption::new("low", "Thinking: low"),
                    SessionConfigSelectOption::new("medium", "Thinking: medium"),
                ],
            )
            .category(Some(SessionConfigOptionCategory::ThoughtLevel)),
        ];

        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ConfigOptionUpdate(
            ConfigOptionUpdate::new(options),
        )));

        assert!(s.session_config_options.is_empty());
        assert!(s.current_mode.is_none());
    }

    #[test]
    fn open_config_value_picker_preselects_current_value_and_submits() {
        let mut s = AppState::new();
        s.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "model-1",
            vec![
                SessionConfigSelectOption::new("model-1", "Model 1"),
                SessionConfigSelectOption::new("model-2", "Model 2"),
            ],
        )];

        assert!(s.open_config_value_picker(0));
        assert_eq!(s.config_picker.as_ref().expect("picker").selected_value, 0);

        s.config_picker_move(1);
        let submitted = s.config_picker_accept().expect("submitted");
        assert!(s.config_picker.is_none());
        assert_eq!(
            submitted.0,
            SessionConfigTarget::ConfigOption {
                config_id: "model".into()
            }
        );
        assert_eq!(submitted.1.to_string(), "model-2");
    }

    #[test]
    fn config_option_update_clamps_picker_selection() {
        let mut s = AppState::new();
        let initial = vec![
            SessionConfigOption::select(
                "model",
                "Model",
                "model-1",
                vec![
                    SessionConfigSelectOption::new("model-1", "Model 1"),
                    SessionConfigSelectOption::new("model-2", "Model 2"),
                ],
            ),
            SessionConfigOption::select(
                "mode",
                "Mode",
                "ask",
                vec![
                    SessionConfigSelectOption::new("ask", "Ask"),
                    SessionConfigSelectOption::new("code", "Code"),
                ],
            ),
        ];
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ConfigOptionUpdate(
            ConfigOptionUpdate::new(initial),
        )));

        assert!(s.open_config_value_picker(0));
        s.config_picker_move(1);
        assert_eq!(s.config_picker.as_ref().expect("picker").selected_value, 1);

        let updated = vec![SessionConfigOption::select(
            "model",
            "Model",
            "model-1",
            vec![SessionConfigSelectOption::new("model-1", "Model 1")],
        )];
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ConfigOptionUpdate(
            ConfigOptionUpdate::new(updated),
        )));

        assert_eq!(s.config_picker.as_ref().expect("picker").selected_value, 0);
    }

    #[test]
    fn config_picker_search_filters_choices_case_insensitively() {
        let mut s = AppState::new();
        s.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "claude-3-5",
            vec![
                SessionConfigSelectOption::new("gpt-4o", "GPT-4o"),
                SessionConfigSelectOption::new("gpt-4", "GPT-4"),
                SessionConfigSelectOption::new("claude-3-5", "Claude 3.5 Sonnet"),
                SessionConfigSelectOption::new("claude-3", "Claude 3"),
            ],
        )];

        assert!(s.open_config_value_picker(0));
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.filtered_indices.len(), 4);

        // Search for "Claude" (case-insensitive)
        s.config_picker_set_search("claude");
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.filtered_indices, vec![2, 3]);
        assert_eq!(picker.selected_value, 0);

        // Refine to "sonnet"
        s.config_picker_set_search("sonnet");
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.filtered_indices, vec![2]);

        // Clear filter shows all again
        s.config_picker_set_search("");
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.filtered_indices.len(), 4);
    }

    #[test]
    fn config_picker_search_moves_navigates_filtered_list() {
        let mut s = AppState::new();
        s.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "gpt-4",
            vec![
                SessionConfigSelectOption::new("gpt-4o", "GPT-4o"),
                SessionConfigSelectOption::new("gpt-4", "GPT-4"),
                SessionConfigSelectOption::new("claude-3", "Claude 3"),
            ],
        )];

        assert!(s.open_config_value_picker(0));
        // Current value "gpt-4" is at index 1 → selected_value = 1
        s.config_picker_set_search("gpt");

        // Filtered to [0, 1]. Previously selected full index 1 still present
        // at position 1 in the filtered list.
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.filtered_indices, vec![0, 1]);
        assert_eq!(picker.selected_value, 1);

        // Move up to first match
        s.config_picker_move(-1);
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.selected_value, 0);

        // Accept should submit gpt-4o (filtered_indices[0] = 0)
        let submitted = s.config_picker_accept().expect("submitted");
        assert_eq!(submitted.1.to_string(), "gpt-4o");
    }

    #[test]
    fn config_picker_preserves_selection_when_filter_narrows() {
        let mut s = AppState::new();
        s.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "gpt-4",
            vec![
                SessionConfigSelectOption::new("gpt-4", "GPT-4"),
                SessionConfigSelectOption::new("claude-3", "Claude 3"),
                SessionConfigSelectOption::new("claude-3-5", "Claude 3.5"),
            ],
        )];

        assert!(s.open_config_value_picker(0));
        // Current value "gpt-4" is at index 0 → selected_value = 0
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.selected_value, 0);

        // Move to Claude 3 (index 1)
        s.config_picker_move(1);
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.selected_value, 1);

        // Filter to "claude" - should still point at Claude 3 (full index 1)
        s.config_picker_set_search("claude");
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.filtered_indices, vec![1, 2]);
        assert_eq!(picker.selected_value, 0); // Claude 3 at position 0 in filtered list
    }

    #[test]
    fn runtime_close_notice_preserves_fatal_status() {
        let mut s = AppState::new();
        s.status_line = Some(StatusMessage::fatal("boom"));

        s.mark_runtime_closed();

        assert!(s.runtime_closed);
        // A pre-existing Fatal status must outlast the clean-close path:
        // otherwise the user gets a generic "disconnected" instead of the
        // real error.
        assert_eq!(s.connection_state, ConnectionState::Closed);
        let status = s.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Fatal);
        assert_eq!(status.text, "boom");
    }

    #[test]
    fn runtime_close_notice_replaces_nonfatal_status() {
        let mut s = AppState::new();
        s.status_line = Some(StatusMessage::warning("prompt failed"));

        s.mark_runtime_closed();

        assert!(s.runtime_closed);
        let status = s.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Info);
        assert_eq!(status.text, "acp runtime closed; press Ctrl-C to quit");
        assert_eq!(s.session.transcript.len(), 1);
        match &s.session.transcript[0] {
            Entry::System(text) => assert_eq!(text, "acp runtime closed; press Ctrl-C to quit"),
            other => panic!("unexpected entry: {other:?}"),
        }
    }

    #[test]
    fn connection_state_progresses_through_launch_to_streaming_to_ready() {
        let mut s = AppState::new();
        assert_eq!(s.connection_state, ConnectionState::Launching);

        s.apply_event(UiEvent::Connected {
            agent_name: Some("opencode".into()),
            agent_version: Some("0.1".into()),
            prompt_images_supported: false,
            session_fork_supported: false,
            session_load_supported: false,
            side_session_supported: false,
            side_session_unsupported_reason: None,
        });
        assert_eq!(s.connection_state, ConnectionState::Initializing);

        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
            resumed: false,
        });
        assert_eq!(s.connection_state, ConnectionState::Ready);

        s.record_user_prompt("hi".to_string());
        assert_eq!(s.connection_state, ConnectionState::Streaming);

        s.mark_cancelling();
        assert_eq!(s.connection_state, ConnectionState::Cancelling);

        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::Cancelled,
            usage: None,
        });
        assert_eq!(s.connection_state, ConnectionState::Ready);
        assert!(!s.is_streaming());
    }

    #[test]
    fn prompt_submitted_during_startup_stays_busy_through_the_handshake() {
        let mut state = AppState::new();
        state.record_user_prompt("queued while starting".to_string());
        assert_eq!(state.connection_state, ConnectionState::Streaming);

        state.apply_event(UiEvent::Connected {
            agent_name: Some("slow adapter".into()),
            agent_version: None,
            prompt_images_supported: false,
            session_fork_supported: false,
            session_load_supported: false,
            side_session_supported: false,
            side_session_unsupported_reason: None,
        });
        assert_eq!(state.connection_state, ConnectionState::Streaming);
        state.apply_event(UiEvent::SessionStarted {
            session_id: "slow-session".into(),
            resumed: false,
        });
        assert_eq!(state.connection_state, ConnectionState::Streaming);
        assert!(state.is_busy());

        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        assert_eq!(state.connection_state, ConnectionState::Ready);
    }

    #[test]
    fn primary_acp_connection_lifecycle_uses_generic_starting_status() {
        let mut state = AppState::new();
        state.set_primary_acp_name("Claude Code");

        state.announce_waiting_for_primary();
        state.announce_waiting_for_primary();
        assert!(state.transcript.is_empty());
        let status = state.status_line.as_ref().expect("waiting status");
        assert_eq!(status.kind, StatusKind::Info);
        assert_eq!(status.text, "session is still starting");

        state.apply_event(UiEvent::Connected {
            agent_name: Some("claude-agent-acp".into()),
            agent_version: Some("1.0".into()),
            prompt_images_supported: false,
            session_fork_supported: false,
            session_load_supported: false,
            side_session_supported: false,
            side_session_unsupported_reason: None,
        });
        state.announce_waiting_for_primary();

        assert!(state.transcript.is_empty());
        let status = state.status_line.as_ref().expect("starting status");
        assert_eq!(status.kind, StatusKind::Info);
        assert_eq!(status.text, "session is still starting");
    }

    #[test]
    fn prompt_failed_returns_to_ready_with_warning_status() {
        let mut s = AppState::new();
        s.apply_event(UiEvent::Connected {
            agent_name: Some("opencode".into()),
            agent_version: None,
            prompt_images_supported: false,
            session_fork_supported: false,
            session_load_supported: false,
            side_session_supported: false,
            side_session_unsupported_reason: None,
        });
        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
            resumed: false,
        });
        s.record_user_prompt("hi".to_string());

        s.apply_event(UiEvent::PromptFailed {
            message: "prompt failed: boom".to_string(),
        });

        assert_eq!(s.connection_state, ConnectionState::Ready);
        assert!(!s.is_streaming());
        let status = s.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Warning);
        assert_eq!(status.text, "prompt failed: boom");
        assert_eq!(s.session.transcript.len(), 2);
        match &s.session.transcript[1] {
            Entry::System(text) => assert_eq!(text, "warning: prompt failed: boom"),
            other => panic!("unexpected entry: {other:?}"),
        }
    }

    #[test]
    fn prompt_failed_drops_queued_prompts_and_surfaces_drop() {
        // Regression for #156: a PromptFailed mid-queue used to flip
        // back to Ready with the queued prompt intact, which the UI
        // drain pass then auto-fired into a possibly-degraded runtime
        // before the user saw the failure.
        let mut s = AppState::new();
        s.apply_event(UiEvent::Connected {
            agent_name: Some("opencode".into()),
            agent_version: None,
            prompt_images_supported: false,
            session_fork_supported: false,
            session_load_supported: false,
            side_session_supported: false,
            side_session_unsupported_reason: None,
        });
        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
            resumed: false,
        });
        s.record_user_prompt("first".to_string());
        s.push_queued_prompt(QueuedPrompt {
            text: "queued body".to_string(),
            images: Vec::new(),
            resources: Vec::new(),
            display_text: "queued body".to_string(),
        });

        s.apply_event(UiEvent::PromptFailed {
            message: "prompt failed: transport blip".to_string(),
        });

        assert_eq!(s.connection_state, ConnectionState::Ready);
        assert!(
            s.queued_prompts().next().is_none(),
            "PromptFailed must drop queued prompts so the next drain does not auto-fire them"
        );
        let status = s.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Warning);
        assert_eq!(
            status.text,
            "prompt failed: transport blip (1 queued prompt(s) dropped)"
        );
    }

    #[test]
    fn prompt_failed_without_queue_keeps_message_unchanged() {
        // When there is no queued prompt, the warning text must match
        // what callers send verbatim with no spurious drop suffix.
        let mut s = AppState::new();
        s.apply_event(UiEvent::Connected {
            agent_name: Some("opencode".into()),
            agent_version: None,
            prompt_images_supported: false,
            session_fork_supported: false,
            session_load_supported: false,
            side_session_supported: false,
            side_session_unsupported_reason: None,
        });
        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
            resumed: false,
        });
        s.record_user_prompt("hi".to_string());

        s.apply_event(UiEvent::PromptFailed {
            message: "prompt failed: boom".to_string(),
        });

        let status = s.status_line.expect("status");
        assert_eq!(status.text, "prompt failed: boom");
    }

    #[test]
    fn prompt_done_records_elapsed_and_token_usage() {
        let mut s = AppState::new();
        s.apply_event(UiEvent::Connected {
            agent_name: Some("opencode".into()),
            agent_version: None,
            prompt_images_supported: false,
            session_fork_supported: false,
            session_load_supported: false,
            side_session_supported: false,
            side_session_unsupported_reason: None,
        });
        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
            resumed: false,
        });
        s.record_user_prompt("hi".to_string());

        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: Some(Usage::new(42, 30, 12).thought_tokens(Some(4))),
        });

        assert!(!s.is_streaming());
        assert!(s.last_turn_elapsed().is_some());
        assert_eq!(s.token_usage.total_tokens, Some(42));
        assert_eq!(s.token_usage.input_tokens, Some(30));
        assert_eq!(s.token_usage.output_tokens, Some(12));
        assert_eq!(s.token_usage.thought_tokens, Some(4));
    }

    #[test]
    fn prompt_done_keeps_queue_status_for_any_stop_reason_when_queued() {
        // Regression: a queued prompt is about to fire as the
        // next turn and owns the status line. PromptDone must not clobber
        // the "queued ..." indicator with "turn done: ...", regardless
        // of the stop reason.
        for reason in [
            StopReason::EndTurn,
            StopReason::Cancelled,
            StopReason::MaxTokens,
        ] {
            let mut s = AppState::new();
            s.apply_event(UiEvent::SessionStarted {
                session_id: "sess-1".into(),
                resumed: false,
            });
            s.record_user_prompt("first".to_string());
            s.push_queued_prompt(QueuedPrompt {
                text: "queued".to_string(),
                images: Vec::new(),
                resources: Vec::new(),
                display_text: "queued".to_string(),
            });
            s.status_line = Some(StatusMessage::info("queued 1: queued"));
            s.apply_event(UiEvent::WorkspaceDiff(crate::event::WorkspaceDiffEvent {
                turn_id: 1,
                diffs: vec![crate::event::WorkspaceDiff {
                    path: PathBuf::from("src/lib.rs"),
                    old_text: None,
                    new_text: "after\n".to_string(),
                }],
                total_files: 1,
                max_files: 20,
                truncated: false,
            }));

            s.apply_event(UiEvent::PromptDone {
                stop_reason: reason,
                usage: None,
            });

            let status = s.status_line.clone().expect("status line preserved");
            assert_eq!(
                status.text, "queued 1: queued",
                "queued prompt must keep its status across PromptDone({reason:?})"
            );
        }
    }

    #[test]
    fn prompt_done_sets_turn_done_status_without_a_queued_prompt() {
        // Without a prompt queued, PromptDone still surfaces the usual
        // "turn done: ..." status.
        let mut s = AppState::new();
        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
            resumed: false,
        });
        s.record_user_prompt("first".to_string());

        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });

        let status = s.status_line.clone().expect("status line set");
        assert_eq!(status.text, "turn done: EndTurn");
    }

    #[test]
    fn usage_update_records_context_tokens() {
        let mut s = AppState::new();

        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::UsageUpdate(
            UsageUpdate::new(12_000, 128_000),
        )));

        assert_eq!(s.token_usage.context_used, Some(12_000));
        assert_eq!(s.token_usage.context_size, Some(128_000));
    }

    #[test]
    fn usage_update_records_claude_rate_limit_meta() {
        let mut s = AppState::new();
        let mut meta = serde_json::Map::new();
        meta.insert(
            CLAUDE_RATE_LIMIT_META_KEY.to_string(),
            serde_json::json!({
                "status": "allowed_warning",
                "rateLimitType": "five_hour",
                "utilization": 8,
                "resetsAt": 1_781_706_600_i64,
            }),
        );

        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::UsageUpdate(
            UsageUpdate::new(12_000, 128_000).meta(meta),
        )));

        // The reset suffix renders in the machine's local zone, so assert the
        // stable prefix rather than a timezone-dependent wall-clock string.
        let line = s.token_usage.rate_limit.clone().expect("rate limit line");
        assert!(
            line.starts_with("Current session: 8% used · resets "),
            "unexpected line: {line}"
        );
    }

    #[test]
    fn usage_update_accepts_snake_case_claude_rate_limit_meta() {
        let mut s = AppState::new();
        let mut meta = serde_json::Map::new();
        meta.insert(
            CLAUDE_RATE_LIMIT_META_KEY.to_string(),
            serde_json::json!({
                "status": "allowed",
                "rate_limit_type": "seven_day",
                "utilization": 34,
                "resets_at": 1_781_706_600_i64,
            }),
        );

        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::UsageUpdate(
            UsageUpdate::new(12_000, 128_000).meta(meta),
        )));

        let line = s.token_usage.rate_limit.clone().expect("rate limit line");
        assert!(
            line.starts_with("Current week (all models): 34% used · resets "),
            "unexpected line: {line}"
        );
    }

    #[test]
    fn usage_update_surfaces_claude_rate_limit_in_transcript_once() {
        let mut s = AppState::new();
        let make_update = || {
            let mut meta = serde_json::Map::new();
            // No `resetsAt` keeps the line deterministic regardless of zone.
            meta.insert(
                CLAUDE_RATE_LIMIT_META_KEY.to_string(),
                serde_json::json!({
                    "status": "allowed_warning",
                    "rateLimitType": "five_hour",
                    "utilization": 8,
                }),
            );
            UiEvent::SessionUpdate(SessionUpdate::UsageUpdate(
                UsageUpdate::new(12_000, 128_000).meta(meta),
            ))
        };

        // First observation surfaces the line in the transcript.
        s.apply_event(make_update());
        // An identical follow-up update must not duplicate the message.
        s.apply_event(make_update());

        let entries = s
            .transcript
            .iter()
            .filter(
                |entry| matches!(entry, Entry::System(text) if text == "Current session: 8% used"),
            )
            .count();
        assert_eq!(entries, 1);
    }

    #[test]
    fn claude_usage_event_replaces_available_with_unavailable() {
        let mut s = AppState::new();

        s.apply_event(UiEvent::ClaudeUsage(ClaudeUsageStatus::Available(
            crate::claude_usage::ClaudeUsageReport {
                five_hour: Some(crate::claude_usage::ClaudeUsageWindow {
                    remaining_percent: 88,
                    reset_context: None,
                }),
                week: Some(crate::claude_usage::ClaudeUsageWindow {
                    remaining_percent: 63,
                    reset_context: None,
                }),
            },
        )));

        assert_eq!(
            s.claude_usage
                .as_ref()
                .map(ClaudeUsageStatus::compact_label),
            Some("Claude usage: 5H 88% left · week 63% left".to_string())
        );

        s.apply_event(UiEvent::ClaudeUsage(ClaudeUsageStatus::Unavailable(
            "not signed in".to_string(),
        )));
        assert_eq!(
            s.claude_usage,
            Some(ClaudeUsageStatus::Unavailable("not signed in".to_string()))
        );
    }

    #[test]
    fn codex_usage_event_replaces_available_with_unavailable() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::CodexUsage(CodexUsageStatus::Available(
            crate::codex_usage::CodexUsageReport {
                primary: Some(crate::codex_usage::CodexUsageWindow {
                    label: "5H".to_string(),
                    remaining_percent: 75,
                    resets_at: None,
                }),
                secondary: None,
            },
        )));
        assert!(matches!(
            state.codex_usage,
            Some(CodexUsageStatus::Available(_))
        ));

        state.apply_event(UiEvent::CodexUsage(CodexUsageStatus::Unavailable(
            "not signed in".to_string(),
        )));
        assert_eq!(
            state.codex_usage,
            Some(CodexUsageStatus::Unavailable("not signed in".to_string()))
        );
    }

    #[test]
    fn usage_update_dedups_each_rate_limit_window_independently() {
        let mut s = AppState::new();
        let event = |kind: &str, utilization: u64| {
            let mut meta = serde_json::Map::new();
            meta.insert(
                CLAUDE_RATE_LIMIT_META_KEY.to_string(),
                serde_json::json!({
                    "status": "allowed",
                    "rateLimitType": kind,
                    "utilization": utilization,
                }),
            );
            UiEvent::SessionUpdate(SessionUpdate::UsageUpdate(
                UsageUpdate::new(12_000, 128_000).meta(meta),
            ))
        };

        s.apply_event(event("five_hour", 8));
        s.apply_event(event("seven_day", 34));
        // The session window is unchanged since its last update — it must not
        // re-surface just because the week window updated in between.
        s.apply_event(event("five_hour", 8));

        let lines: Vec<&str> = s
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                Entry::System(text) if text.starts_with("Current ") => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            lines,
            vec![
                "Current session: 8% used",
                "Current week (all models): 34% used",
            ]
        );
    }

    #[test]
    fn mark_cancelling_is_noop_outside_streaming() {
        // Cancelling is only meaningful while a prompt is in flight; from
        // Ready, a stray Ctrl-C must not lie about the connection state.
        let mut s = AppState::new();
        s.apply_event(UiEvent::Connected {
            agent_name: Some("opencode".into()),
            agent_version: None,
            prompt_images_supported: false,
            session_fork_supported: false,
            session_load_supported: false,
            side_session_supported: false,
            side_session_unsupported_reason: None,
        });
        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
            resumed: false,
        });
        assert_eq!(s.connection_state, ConnectionState::Ready);

        s.mark_cancelling();
        assert_eq!(s.connection_state, ConnectionState::Ready);
    }

    #[test]
    fn fatal_state_outlasts_runtime_close() {
        // Fatal arrives via UiEvent::Fatal, which internally calls
        // mark_runtime_closed. A subsequent mark_runtime_closed (the
        // channel-drop path in ui_loop) must not downgrade Fatal to Closed.
        let mut s = AppState::new();
        s.apply_event(UiEvent::Fatal("kaboom".to_string()));
        assert_eq!(s.connection_state, ConnectionState::Fatal);

        s.mark_runtime_closed();
        assert_eq!(s.connection_state, ConnectionState::Fatal);
    }

    #[test]
    fn permission_request_queues_behind_existing_modal() {
        // Two consecutive PermissionRequest events must enqueue rather
        // than replace. Overwriting would drop the prior oneshot, which
        // the agent reads as a silent cancel even though the user never
        // saw that prompt.
        let mut s = AppState::new();
        let (prompt_a, _rx_a) = permission_prompt_with_id("call-a");
        let (prompt_b, _rx_b) = permission_prompt_with_id("call-b");

        s.apply_event(UiEvent::PermissionRequest(prompt_a));
        s.apply_event(UiEvent::PermissionRequest(prompt_b));

        assert!(s.has_pending_permission());
        assert_eq!(s.pending_permission_count(), 2);
        assert_eq!(
            s.pending_permission()
                .expect("front")
                .prompt
                .tool_call
                .tool_call_id
                .to_string(),
            "call-a",
            "the first-enqueued prompt must remain at the front",
        );
    }

    #[test]
    fn remote_decision_resolves_matching_queued_prompt() {
        let mut s = AppState::new();
        let (prompt_a, rx_a) = permission_prompt_with_id("call-a");
        let (prompt_b, _rx_b) = permission_prompt_with_id("call-b");
        s.apply_event(UiEvent::PermissionRequest(prompt_a));
        s.apply_event(UiEvent::PermissionRequest(prompt_b));

        s.apply_event(UiEvent::RemotePermissionDecision {
            request_id: "call-a".to_string(),
            option_id: "allow".to_string(),
        });

        // The matching prompt was consumed and answered; the other stays.
        assert_eq!(s.pending_permission_count(), 1);
        assert_eq!(
            s.pending_permission()
                .expect("remaining prompt")
                .prompt
                .tool_call
                .tool_call_id
                .to_string(),
            "call-b"
        );
        match rx_a.blocking_recv() {
            Ok(PermissionDecision::Selected(id)) => assert_eq!(id, "allow"),
            other => panic!("expected Selected decision, got {other:?}"),
        }
    }

    #[test]
    fn remote_decision_for_unknown_request_or_option_is_dropped() {
        let mut s = AppState::new();
        let (prompt, _rx) = permission_prompt_with_id("call-a");
        s.apply_event(UiEvent::PermissionRequest(prompt));

        // Unknown request id: nothing is consumed.
        s.apply_event(UiEvent::RemotePermissionDecision {
            request_id: "call-z".to_string(),
            option_id: "allow".to_string(),
        });
        assert_eq!(s.pending_permission_count(), 1);

        // Known request id but an option the prompt never offered: a stale
        // or corrupted decision must not cancel the prompt either.
        s.apply_event(UiEvent::RemotePermissionDecision {
            request_id: "call-a".to_string(),
            option_id: "no-such-option".to_string(),
        });
        assert_eq!(s.pending_permission_count(), 1);
    }

    #[test]
    fn permission_request_closes_help_overlay() {
        let mut s = AppState::new();
        let (prompt, _rx) = permission_prompt_with_id("call-a");
        s.help_overlay = true;

        s.apply_event(UiEvent::PermissionRequest(prompt));

        assert!(s.has_pending_permission());
        assert!(
            !s.help_overlay,
            "permission prompt should dismiss stale help"
        );
    }

    #[test]
    fn permission_queue_is_fifo_and_routes_decisions_to_the_right_prompt() {
        // Verify both FIFO order (A is at the front before B) and that
        // the responder we send a decision through belongs to the prompt
        // the user just saw, not a later one in the queue.
        let mut s = AppState::new();
        let (prompt_a, rx_a) = permission_prompt_with_id("call-a");
        let (prompt_b, rx_b) = permission_prompt_with_id("call-b");

        s.apply_event(UiEvent::PermissionRequest(prompt_a));
        s.apply_event(UiEvent::PermissionRequest(prompt_b));

        let front_a = s.take_pending_permission().expect("front a");
        assert_eq!(front_a.prompt.tool_call.tool_call_id.to_string(), "call-a");
        let _ = front_a
            .prompt
            .responder
            .send(PermissionDecision::Selected("allow".into()));
        match rx_a.blocking_recv() {
            Ok(PermissionDecision::Selected(id)) => assert_eq!(id, "allow"),
            other => panic!("rx_a expected Selected, got {other:?}"),
        }

        let front_b = s.take_pending_permission().expect("front b");
        assert_eq!(front_b.prompt.tool_call.tool_call_id.to_string(), "call-b");
        let _ = front_b.prompt.responder.send(PermissionDecision::Cancelled);
        match rx_b.blocking_recv() {
            Ok(PermissionDecision::Cancelled) => {}
            other => panic!("rx_b expected Cancelled, got {other:?}"),
        }

        assert!(!s.has_pending_permission());
    }

    #[test]
    fn runtime_close_cancels_all_queued_permissions() {
        // Closing the runtime while prompts are queued must cancel every
        // one of them explicitly so the agent sees a deterministic
        // outcome instead of inferring "cancelled" from a dropped sender.
        let mut s = AppState::new();
        let (prompt_a, rx_a) = permission_prompt_with_id("call-a");
        let (prompt_b, rx_b) = permission_prompt_with_id("call-b");

        s.apply_event(UiEvent::PermissionRequest(prompt_a));
        s.apply_event(UiEvent::PermissionRequest(prompt_b));

        s.mark_runtime_closed();

        assert!(!s.has_pending_permission());
        assert!(matches!(
            rx_a.blocking_recv(),
            Ok(PermissionDecision::Cancelled)
        ));
        assert!(matches!(
            rx_b.blocking_recv(),
            Ok(PermissionDecision::Cancelled)
        ));
    }

    #[test]
    fn cancel_pending_permissions_event_cancels_all_queued_permissions() {
        let mut s = AppState::new();
        let (prompt_a, rx_a) = permission_prompt_with_id("call-a");
        let (prompt_b, rx_b) = permission_prompt_with_id("call-b");

        s.apply_event(UiEvent::PermissionRequest(prompt_a));
        s.apply_event(UiEvent::PermissionRequest(prompt_b));
        s.apply_event(UiEvent::CancelPendingPermissions);

        assert!(!s.has_pending_permission());
        assert!(matches!(
            rx_a.blocking_recv(),
            Ok(PermissionDecision::Cancelled)
        ));
        assert!(matches!(
            rx_b.blocking_recv(),
            Ok(PermissionDecision::Cancelled)
        ));
    }

    #[test]
    fn cancel_pending_permissions_event_marks_unfinished_tool_calls_failed() {
        let mut s = AppState::new();
        s.record_user_prompt("run command".to_string());
        s.tool_calls.insert(
            "call-1".to_string(),
            ToolCallView {
                title: "cargo test".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::InProgress,
                body: vec![ToolCallOutput::Text("running".to_string())],
            },
        );
        s.transcript.push(Entry::ToolCall("call-1".to_string()));

        s.apply_event(UiEvent::CancelPendingPermissions);

        let view = s.tool_calls.get("call-1").expect("tool call");
        assert_eq!(view.status, ToolCallStatus::Failed);
        assert!(view.body.iter().any(
            |output| matches!(output, ToolCallOutput::Note(note) if note == "tool call cancelled")
        ));
    }

    #[test]
    fn subagent_permission_cancellation_is_scoped_to_its_owner() {
        let mut state = AppState::new();
        let (first, first_rx) = permission_prompt_with_id("same-local-id");
        let (second, _second_rx) = permission_prompt_with_id("same-local-id");
        state.apply_event(UiEvent::Subagent(SubagentEvent::PermissionRequest {
            subagent_id: 1,
            prompt: first,
        }));
        state.apply_event(UiEvent::Subagent(SubagentEvent::PermissionRequest {
            subagent_id: 2,
            prompt: second,
        }));

        state.apply_event(UiEvent::Subagent(SubagentEvent::CancelPendingPermissions {
            subagent_id: 1,
        }));

        assert!(matches!(
            first_rx.blocking_recv(),
            Ok(PermissionDecision::Cancelled)
        ));
        assert_eq!(state.pending_permission_count(), 1);
        assert_eq!(
            state
                .pending_permission()
                .expect("second subagent prompt remains")
                .prompt
                .tool_call
                .tool_call_id
                .to_string(),
            "subagent-2:same-local-id"
        );
    }

    #[test]
    fn subagent_elicitation_cancellation_is_scoped_to_its_owner() {
        let mut state = AppState::new();
        let (first, first_rx) = elicitation_prompt();
        let (second, _second_rx) = elicitation_prompt();
        state.apply_event(UiEvent::Subagent(SubagentEvent::ElicitationRequest {
            subagent_id: 1,
            prompt: first,
        }));
        state.apply_event(UiEvent::Subagent(SubagentEvent::ElicitationRequest {
            subagent_id: 2,
            prompt: second,
        }));

        state.apply_event(UiEvent::Subagent(SubagentEvent::CancelPendingPermissions {
            subagent_id: 1,
        }));

        assert!(matches!(
            first_rx.blocking_recv(),
            Ok(ElicitationOutcome::Cancel)
        ));
        assert_eq!(state.pending_elicitation_count(), 1);
        assert_eq!(
            state
                .pending_elicitation()
                .expect("second subagent elicitation remains")
                .subagent_id,
            Some(2)
        );
    }

    #[test]
    fn subagent_failure_marks_only_its_own_tools_failed() {
        let mut state = AppState::new();
        for subagent_id in [1, 2] {
            state.apply_event(subagent_started(subagent_id, "worker", ""));
            state.apply_event(subagent_session_update_for(
                subagent_id,
                SessionUpdate::ToolCall(
                    ToolCall::new("same-local-id", "work").status(ToolCallStatus::InProgress),
                ),
            ));
        }

        state.apply_event(finished_subagent(
            1,
            SubagentOutcome::Failed("boom".to_string()),
        ));

        assert_eq!(
            state
                .tool_calls
                .get("subagent-1:same-local-id")
                .expect("first tool")
                .status,
            ToolCallStatus::Failed
        );
        assert_eq!(
            state
                .tool_calls
                .get("subagent-2:same-local-id")
                .expect("sibling tool")
                .status,
            ToolCallStatus::InProgress
        );
    }

    #[test]
    fn prompt_done_after_fatal_does_not_resurrect_ready() {
        // A stray PromptDone arriving after Fatal (e.g. queued before the
        // fatal error propagated) must not flip the lifecycle back to
        // Ready; Fatal sticks until the user quits.
        let mut s = AppState::new();
        s.apply_event(UiEvent::Fatal("kaboom".to_string()));

        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        assert_eq!(s.connection_state, ConnectionState::Fatal);
    }

    #[test]
    fn user_chunk_suppressed_during_streaming_but_kept_on_replay() {
        // While a prompt is in flight, the local echo from
        // `record_user_prompt` is the source of truth -- any
        // `UserMessageChunk` the agent sends back is a duplicate and
        // must be dropped.
        let mut s = AppState::new();
        s.record_user_prompt("hello".to_string());
        assert_eq!(s.session.transcript.len(), 1);
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::UserMessageChunk(
            text_chunk("hello"),
        )));
        assert_eq!(
            s.transcript.len(),
            1,
            "agent echo must not double the user prompt while streaming"
        );

        // When the session is idle (e.g. mid-`session/load` replay), the
        // same chunk is the only source of truth for the user message
        // and must be rendered.
        let mut s = AppState::new();
        assert!(!s.is_streaming());
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::UserMessageChunk(
            text_chunk("replayed"),
        )));
        assert_eq!(s.session.transcript.len(), 1);
        match &s.session.transcript[0] {
            Entry::UserPrompt(t) => assert_eq!(t, "replayed"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn replayed_user_chunk_closes_the_previous_agent_message() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("first response"),
        )));
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::UserMessageChunk(
            text_chunk("second prompt"),
        )));
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("second response"),
        )));

        assert!(matches!(
            state.transcript.as_slice(),
            [Entry::AgentMessage(first), Entry::UserPrompt(prompt), Entry::AgentMessage(second)]
                if first == "first response"
                    && prompt == "second prompt"
                    && second == "second response"
        ));
    }

    fn cmd(name: &str) -> AvailableCommand {
        AvailableCommand::new(name, format!("does {name}"))
    }

    fn seed_commands(s: &mut AppState) {
        s.available_commands = vec![
            cmd("create_plan"),
            cmd("review_pr"),
            cmd("research_codebase"),
            cmd("clear"),
        ];
    }

    fn finish_file_autocomplete_scan(state: &mut AppState) {
        let roots = state
            .take_file_autocomplete_scan_request()
            .expect("file autocomplete scan requested");
        let candidates = workspace_file_candidates(&roots);
        assert!(state.apply_file_autocomplete_scan(roots, candidates));
    }

    fn permission_prompt() -> PermissionPrompt {
        let (prompt, _rx) = permission_prompt_with_id("call-1");
        prompt
    }

    /// Build a `PermissionPrompt` and keep its responder receiver so the
    /// test can assert what decision (if any) was sent back to it.
    fn permission_prompt_with_id(
        call_id: &str,
    ) -> (
        PermissionPrompt,
        tokio::sync::oneshot::Receiver<PermissionDecision>,
    ) {
        let (responder, rx) = tokio::sync::oneshot::channel();
        let prompt = PermissionPrompt {
            // Convert to owned: `ToolCallId: From<&'static str>` rejects
            // a borrowed `&str` because it would have to inline the
            // lifetime, so go through `String`.
            tool_call: ToolCallUpdate::new(
                call_id.to_string(),
                agent_client_protocol::schema::v1::ToolCallUpdateFields::default(),
            ),
            options: vec![PermissionOption::new(
                "allow",
                "Allow",
                PermissionOptionKind::AllowOnce,
            )],
            responder,
        };
        (prompt, rx)
    }

    fn elicitation_select_schema() -> ElicitationSchema {
        ElicitationSchema::new().title("Choose a model").property(
            "model",
            StringPropertySchema::new().title("Model").one_of(vec![
                EnumOption::new("fast", "Fast"),
                EnumOption::new("smart", "Smart"),
            ]),
            true,
        )
    }

    /// Build a single-select elicitation prompt and keep its responder
    /// receiver so the test can assert what outcome was sent back.
    fn elicitation_prompt() -> (
        ElicitationPrompt,
        tokio::sync::oneshot::Receiver<ElicitationOutcome>,
    ) {
        let (responder, rx) = tokio::sync::oneshot::channel();
        let mode = ElicitationMode::from(ElicitationFormMode::new(
            ElicitationSessionScope::new("setup-session".to_string()),
            elicitation_select_schema(),
        ));
        let prompt = ElicitationPrompt {
            message: "Pick a model".to_string(),
            mode,
            remote_id: None,
            responder,
        };
        (prompt, rx)
    }

    fn enum_values_elicitation_prompt() -> (
        ElicitationPrompt,
        tokio::sync::oneshot::Receiver<ElicitationOutcome>,
    ) {
        let (responder, rx) = tokio::sync::oneshot::channel();
        let schema = ElicitationSchema::new().title("Choose a model").property(
            "model",
            StringPropertySchema::new()
                .title("Model")
                .enum_values(vec!["fast".to_string(), "smart".to_string()]),
            true,
        );
        let mode = ElicitationMode::from(ElicitationFormMode::new(
            ElicitationSessionScope::new("setup-session".to_string()),
            schema,
        ));
        let prompt = ElicitationPrompt {
            message: "Pick a model".to_string(),
            mode,
            remote_id: None,
            responder,
        };
        (prompt, rx)
    }

    fn url_elicitation_prompt() -> (
        ElicitationPrompt,
        tokio::sync::oneshot::Receiver<ElicitationOutcome>,
    ) {
        let (responder, rx) = tokio::sync::oneshot::channel();
        let mode = ElicitationMode::from(ElicitationUrlMode::new(
            ElicitationSessionScope::new("setup-session".to_string()),
            ElicitationId::new("login-1"),
            "https://example.com/oauth/authorize?client_id=abc&scope=all",
        ));
        let prompt = ElicitationPrompt {
            message: "Open this URL to sign in".to_string(),
            mode,
            remote_id: None,
            responder,
        };
        (prompt, rx)
    }

    fn two_property_elicitation_prompt() -> (
        ElicitationPrompt,
        tokio::sync::oneshot::Receiver<ElicitationOutcome>,
    ) {
        let (responder, rx) = tokio::sync::oneshot::channel();
        let schema = ElicitationSchema::new()
            .property(
                "question_0",
                StringPropertySchema::new()
                    .title("Choose a model")
                    .one_of(vec![
                        EnumOption::new("fast", "Fast"),
                        EnumOption::new("smart", "Smart"),
                    ]),
                false,
            )
            .property(
                "question_0_custom",
                StringPropertySchema::new()
                    .title("Other")
                    .description("Type your own answer instead (optional)."),
                false,
            );
        let mode = ElicitationMode::from(ElicitationFormMode::new(
            ElicitationSessionScope::new("setup-session".to_string()),
            schema,
        ));
        let prompt = ElicitationPrompt {
            message: "Configure".to_string(),
            mode,
            remote_id: None,
            responder,
        };
        (prompt, rx)
    }

    #[test]
    fn elicitation_request_enqueues_pending() {
        let mut s = AppState::new();
        let (prompt, _rx) = elicitation_prompt();
        s.apply_event(UiEvent::ElicitationRequest(prompt));
        assert!(s.has_pending_elicitation());
        assert_eq!(s.pending_elicitation_count(), 1);
        assert!(matches!(
            s.elicitation_view(),
            Some(ElicitationView::SingleSelect { .. })
        ));
    }

    /// A question menu raised by a TUI session and answered in the remote
    /// viewer resolves the queued prompt and closes the local modal.
    #[test]
    fn remote_decision_resolves_a_queued_elicitation() {
        let mut s = AppState::new();
        let (mut prompt, rx) = elicitation_prompt();
        prompt.remote_id = Some("elicitation:1".to_string());
        s.apply_event(UiEvent::ElicitationRequest(prompt));
        assert!(s.has_pending_elicitation());

        s.apply_event(UiEvent::RemotePermissionDecision {
            request_id: "elicitation:1".to_string(),
            option_id: "elicitation:accept:{\"model\":\"smart\"}".to_string(),
        });

        assert!(
            !s.has_pending_elicitation(),
            "the local modal must close once the viewer answers"
        );
        match rx.blocking_recv() {
            Ok(ElicitationOutcome::Accept(content)) => assert_eq!(
                content.get("model"),
                Some(&ElicitationContentValue::String("smart".to_string()))
            ),
            other => panic!("expected the remote answer, got {other:?}"),
        }
    }

    /// A decision that does not validate against the queued prompt is dropped
    /// rather than resolving it with something the agent never offered.
    #[test]
    fn remote_decision_ignores_mismatched_elicitations() {
        let mut s = AppState::new();
        let (mut prompt, _rx) = elicitation_prompt();
        prompt.remote_id = Some("elicitation:1".to_string());
        s.apply_event(UiEvent::ElicitationRequest(prompt));

        // Right id, an option this prompt never offered.
        s.apply_event(UiEvent::RemotePermissionDecision {
            request_id: "elicitation:1".to_string(),
            option_id: "elicitation:accept:{\"model\":\"nonexistent\"}".to_string(),
        });
        assert!(s.has_pending_elicitation());

        // Valid payload, but for a different (already-answered) request.
        s.apply_event(UiEvent::RemotePermissionDecision {
            request_id: "elicitation:99".to_string(),
            option_id: "elicitation:accept:{\"model\":\"smart\"}".to_string(),
        });
        assert!(s.has_pending_elicitation());
    }

    /// An elicitation that was never published has no remote id, so a decision
    /// quoting any id must not resolve it.
    #[test]
    fn remote_decision_skips_unpublished_elicitations() {
        let mut s = AppState::new();
        let (prompt, _rx) = elicitation_prompt();
        assert!(prompt.remote_id.is_none());
        s.apply_event(UiEvent::ElicitationRequest(prompt));

        assert!(!s.resolve_elicitation_remotely(
            "elicitation:1",
            "elicitation:accept:{\"model\":\"smart\"}"
        ));
        assert!(s.has_pending_elicitation());
    }

    #[test]
    fn elicitation_form_accept_sends_selected_value() {
        let mut s = AppState::new();
        let (prompt, rx) = elicitation_prompt();
        s.apply_event(UiEvent::ElicitationRequest(prompt));
        // Move from "fast" to "smart" and accept.
        s.elicitation_select_move(1);
        s.resolve_elicitation_accept();
        assert!(!s.has_pending_elicitation());
        match rx.blocking_recv() {
            Ok(ElicitationOutcome::Accept(content)) => {
                assert_eq!(content.len(), 1);
                assert_eq!(
                    content.get("model"),
                    Some(&ElicitationContentValue::String("smart".to_string()))
                );
            }
            other => panic!("expected Accept, got {other:?}"),
        }
    }

    #[test]
    fn elicitation_enum_values_accept_sends_selected_value() {
        let mut s = AppState::new();
        let (prompt, rx) = enum_values_elicitation_prompt();
        s.apply_event(UiEvent::ElicitationRequest(prompt));
        assert!(matches!(
            s.elicitation_view(),
            Some(ElicitationView::SingleSelect { .. })
        ));
        s.elicitation_select_move(1);
        s.resolve_elicitation_accept();

        match rx.blocking_recv() {
            Ok(ElicitationOutcome::Accept(content)) => {
                assert_eq!(content.len(), 1);
                assert_eq!(
                    content.get("model"),
                    Some(&ElicitationContentValue::String("smart".to_string()))
                );
            }
            other => panic!("expected Accept, got {other:?}"),
        }
    }

    #[test]
    fn elicitation_select_move_wraps() {
        let mut s = AppState::new();
        let (prompt, _rx) = elicitation_prompt();
        s.apply_event(UiEvent::ElicitationRequest(prompt));
        // Two options: Up from index 0 wraps to the last option.
        s.elicitation_select_move(-1);
        assert_eq!(s.pending_elicitation().expect("pending").selected, 1);
        s.elicitation_select_move(1);
        assert_eq!(s.pending_elicitation().expect("pending").selected, 0);
    }

    #[test]
    fn elicitation_esc_cancels() {
        let mut s = AppState::new();
        let (prompt, rx) = elicitation_prompt();
        s.apply_event(UiEvent::ElicitationRequest(prompt));
        s.resolve_elicitation_dismiss();
        assert!(!s.has_pending_elicitation());
        assert!(matches!(rx.blocking_recv(), Ok(ElicitationOutcome::Cancel)));
    }

    #[test]
    fn fatal_cancels_pending_elicitation() {
        let mut s = AppState::new();
        let (prompt, rx) = elicitation_prompt();
        s.apply_event(UiEvent::ElicitationRequest(prompt));
        assert!(s.has_pending_elicitation());
        s.apply_event(UiEvent::Fatal("boom".to_string()));
        assert!(!s.has_pending_elicitation());
        assert!(matches!(rx.blocking_recv(), Ok(ElicitationOutcome::Cancel)));
    }

    #[test]
    fn elicitation_url_mode_accepts_with_empty_content() {
        let mut s = AppState::new();
        let (prompt, rx) = url_elicitation_prompt();
        s.apply_event(UiEvent::ElicitationRequest(prompt));
        match s.elicitation_view() {
            Some(ElicitationView::Url { url }) => {
                assert!(url.starts_with("https://example.com/oauth/authorize"));
            }
            other => panic!("expected URL view, got {other:?}"),
        }
        s.resolve_elicitation_accept();
        match rx.blocking_recv() {
            Ok(ElicitationOutcome::Accept(content)) => assert!(content.is_empty()),
            other => panic!("expected empty Accept, got {other:?}"),
        }
    }

    #[test]
    fn claude_two_property_form_accepts_choice_and_skips_empty_other() {
        let mut s = AppState::new();
        let (prompt, rx) = two_property_elicitation_prompt();
        s.apply_event(UiEvent::ElicitationRequest(prompt));
        assert!(matches!(
            s.elicitation_view(),
            Some(ElicitationView::Form { .. })
        ));
        s.elicitation_select_move(1);
        s.resolve_elicitation_accept();
        assert_eq!(s.pending_elicitation().expect("form").form_field, 1);
        s.resolve_elicitation_accept();

        match rx.blocking_recv() {
            Ok(ElicitationOutcome::Accept(content)) => {
                assert_eq!(
                    content.get("question_0"),
                    Some(&ElicitationContentValue::String("smart".to_string()))
                );
                assert!(!content.contains_key("question_0_custom"));
            }
            other => panic!("expected Accept, got {other:?}"),
        }
    }

    #[test]
    fn claude_array_form_toggles_and_accepts_multiple_values() {
        let mut s = AppState::new();
        let (responder, rx) = tokio::sync::oneshot::channel();
        let schema = ElicitationSchema::new().property(
            "question_0",
            agent_client_protocol::schema::v1::MultiSelectPropertySchema::titled(vec![
                EnumOption::new("tests", "Tests"),
                EnumOption::new("docs", "Docs"),
                EnumOption::new("release", "Release"),
            ]),
            false,
        );
        s.apply_event(UiEvent::ElicitationRequest(ElicitationPrompt {
            message: "Choose workstreams".to_string(),
            mode: ElicitationMode::from(ElicitationFormMode::new(
                ElicitationSessionScope::new("setup-session".to_string()),
                schema,
            )),
            remote_id: None,
            responder,
        }));
        assert!(matches!(
            s.elicitation_view(),
            Some(ElicitationView::Form { .. })
        ));
        s.elicitation_multi_toggle();
        s.elicitation_select_move(1);
        s.elicitation_multi_toggle();
        s.resolve_elicitation_accept();

        match rx.blocking_recv() {
            Ok(ElicitationOutcome::Accept(content)) => assert_eq!(
                content.get("question_0"),
                Some(&ElicitationContentValue::StringArray(vec![
                    "tests".to_string(),
                    "docs".to_string(),
                ]))
            ),
            other => panic!("expected Accept, got {other:?}"),
        }
    }

    #[test]
    fn primitive_form_fields_return_typed_content() {
        let mut s = AppState::new();
        let (responder, rx) = tokio::sync::oneshot::channel();
        let schema = ElicitationSchema::new()
            .property(
                "count",
                agent_client_protocol::schema::v1::IntegerPropertySchema::new()
                    .minimum(1)
                    .maximum(5),
                true,
            )
            .property(
                "enabled",
                agent_client_protocol::schema::v1::BooleanPropertySchema::new(),
                true,
            )
            .property(
                "ratio",
                agent_client_protocol::schema::v1::NumberPropertySchema::new()
                    .minimum(0.0)
                    .maximum(1.0),
                true,
            );
        s.apply_event(UiEvent::ElicitationRequest(ElicitationPrompt {
            message: "Configure primitives".to_string(),
            mode: ElicitationMode::from(ElicitationFormMode::new(
                ElicitationSessionScope::new("setup-session".to_string()),
                schema,
            )),
            remote_id: None,
            responder,
        }));

        s.pending_elicitation_mut().expect("count").input = "3".to_string();
        s.resolve_elicitation_accept();
        s.elicitation_select_move(1);
        s.resolve_elicitation_accept();
        s.pending_elicitation_mut().expect("ratio").input = "0.5".to_string();
        s.resolve_elicitation_accept();

        match rx.blocking_recv() {
            Ok(ElicitationOutcome::Accept(content)) => {
                assert_eq!(
                    content.get("count"),
                    Some(&ElicitationContentValue::Integer(3))
                );
                assert_eq!(
                    content.get("enabled"),
                    Some(&ElicitationContentValue::Boolean(true))
                );
                assert_eq!(
                    content.get("ratio"),
                    Some(&ElicitationContentValue::Number(0.5))
                );
            }
            other => panic!("expected Accept, got {other:?}"),
        }
    }

    #[test]
    fn free_text_string_form_is_text_input() {
        // A string property without `oneOf`/`enum` is free text: render an
        // input field (e.g. an API-key entry) carrying the property title and
        // description.
        let (responder, _rx) = tokio::sync::oneshot::channel();
        let schema = ElicitationSchema::new().property(
            "key",
            StringPropertySchema::new()
                .title("OpenRouter API key")
                .description("Paste your key."),
            true,
        );
        let prompt = ElicitationPrompt {
            message: "Enter your OpenRouter API key".to_string(),
            mode: ElicitationMode::from(ElicitationFormMode::new(
                ElicitationSessionScope::new("setup-session".to_string()),
                schema,
            )),
            remote_id: None,
            responder,
        };
        assert_eq!(
            classify_elicitation(&prompt),
            ElicitationView::Text {
                property_name: "key".to_string(),
                title: Some("OpenRouter API key".to_string()),
                description: Some("Paste your key.".to_string()),
            }
        );
    }

    #[test]
    fn text_elicitation_accept_sends_typed_value() {
        // Typing into a free-text field and pressing Enter returns the trimmed
        // value keyed by the property name.
        let mut s = AppState::new();
        let (responder, rx) = tokio::sync::oneshot::channel();
        let schema = ElicitationSchema::new().property(
            "key",
            StringPropertySchema::new().title("OpenRouter API key"),
            true,
        );
        s.apply_event(UiEvent::ElicitationRequest(ElicitationPrompt {
            message: "Enter your OpenRouter API key".to_string(),
            mode: ElicitationMode::from(ElicitationFormMode::new(
                ElicitationSessionScope::new("setup-session".to_string()),
                schema,
            )),
            remote_id: None,
            responder,
        }));
        if let Some(pending) = s.pending_elicitation_mut() {
            pending.input = "  sk-or-123  ".to_string();
        }
        s.resolve_elicitation_accept();
        let outcome = rx.blocking_recv().expect("outcome");
        match outcome {
            ElicitationOutcome::Accept(content) => {
                assert_eq!(
                    content.get("key"),
                    Some(&ElicitationContentValue::String("sk-or-123".to_string()))
                );
            }
            other => panic!("expected Accept, got {other:?}"),
        }
    }

    #[test]
    fn text_elicitation_empty_accept_is_skip() {
        // Pressing Enter on an empty field skips (Cancel) rather than writing a
        // blank value the agent would reject.
        let mut s = AppState::new();
        let (responder, rx) = tokio::sync::oneshot::channel();
        let schema = ElicitationSchema::new().property(
            "key",
            StringPropertySchema::new().title("OpenRouter API key"),
            true,
        );
        s.apply_event(UiEvent::ElicitationRequest(ElicitationPrompt {
            message: "Enter your OpenRouter API key".to_string(),
            mode: ElicitationMode::from(ElicitationFormMode::new(
                ElicitationSessionScope::new("setup-session".to_string()),
                schema,
            )),
            remote_id: None,
            responder,
        }));
        s.resolve_elicitation_accept();
        assert!(matches!(
            rx.blocking_recv().expect("outcome"),
            ElicitationOutcome::Cancel
        ));
    }

    #[test]
    fn second_elicitation_queues_without_dropping_first() {
        let mut s = AppState::new();
        let (prompt_a, mut rx_a) = elicitation_prompt();
        let (prompt_b, _rx_b) = url_elicitation_prompt();
        s.apply_event(UiEvent::ElicitationRequest(prompt_a));
        s.apply_event(UiEvent::ElicitationRequest(prompt_b));
        assert_eq!(s.pending_elicitation_count(), 2);
        // The first responder must still be alive (not silently dropped).
        assert!(matches!(
            rx_a.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        // The front view is the first (single-select) prompt.
        assert!(matches!(
            s.elicitation_view(),
            Some(ElicitationView::SingleSelect { .. })
        ));
    }

    #[test]
    fn elicitation_request_and_response_round_trip() {
        // Pins the `#[serde(flatten)]` + `tag = "mode"` / `tag = "action"`
        // wire framing that mjolnir and its ACP agents must agree on.
        let form_req = CreateElicitationRequest::new(
            ElicitationMode::from(ElicitationFormMode::new(
                ElicitationSessionScope::new("s".to_string()),
                elicitation_select_schema(),
            )),
            "pick",
        );
        let value = serde_json::to_value(&form_req).expect("serialize form req");
        let back: CreateElicitationRequest =
            serde_json::from_value(value).expect("deserialize form req");
        assert_eq!(form_req, back);

        let url_req = CreateElicitationRequest::new(
            ElicitationMode::from(ElicitationUrlMode::new(
                ElicitationSessionScope::new("s".to_string()),
                ElicitationId::new("id-1"),
                "https://example.com",
            )),
            "open",
        );
        let value = serde_json::to_value(&url_req).expect("serialize url req");
        let back: CreateElicitationRequest =
            serde_json::from_value(value).expect("deserialize url req");
        assert_eq!(url_req, back);

        let mut content = BTreeMap::new();
        content.insert(
            "model".to_string(),
            ElicitationContentValue::String("smart".to_string()),
        );
        let actions = [
            ElicitationAction::Accept(ElicitationAcceptAction::new().content(content)),
            ElicitationAction::Decline,
            ElicitationAction::Cancel,
        ];
        for action in actions {
            let resp = CreateElicitationResponse::new(action);
            let value = serde_json::to_value(&resp).expect("serialize resp");
            let back: CreateElicitationResponse =
                serde_json::from_value(value).expect("deserialize resp");
            assert_eq!(resp, back);
        }
    }

    #[test]
    fn autocomplete_hidden_when_input_does_not_start_with_slash() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "hello".to_string();
        s.update_autocomplete();
        assert!(!s.autocomplete.visible);
        assert!(s.autocomplete.matches.is_empty());
    }

    #[test]
    fn autocomplete_advertises_supported_builtin_commands_by_default() {
        let mut s = AppState::new();
        s.input = "/".to_string();
        s.update_autocomplete();

        assert!(s.autocomplete.visible);
        let names: Vec<&str> = s
            .autocomplete
            .matches
            .iter()
            .map(|&i| s.available_commands[i].name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "new",
                "clear",
                "compact",
                "load",
                "export",
                "agents",
                "subagents",
                "terminals",
                "review",
                "diff",
                "mjconfig",
                "memory",
                "ragnarok"
            ]
        );
    }

    #[test]
    fn autocomplete_advertises_fork_after_agent_capability() {
        let mut s = AppState::new();
        s.apply_event(UiEvent::Connected {
            agent_name: Some("opencode".into()),
            agent_version: None,
            prompt_images_supported: false,
            session_fork_supported: true,
            session_load_supported: false,
            side_session_supported: false,
            side_session_unsupported_reason: None,
        });
        s.input = "/".to_string();
        s.update_autocomplete();

        assert!(s.autocomplete.visible);
        let names: Vec<&str> = s
            .autocomplete
            .matches
            .iter()
            .map(|&i| s.available_commands[i].name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "new",
                "clear",
                "compact",
                "load",
                "export",
                "agents",
                "subagents",
                "terminals",
                "review",
                "diff",
                "mjconfig",
                "memory",
                "ragnarok",
                "fork"
            ]
        );
    }

    #[test]
    fn available_command_updates_keep_builtin_commands_first() {
        let mut s = AppState::new();
        s.apply_event(UiEvent::Connected {
            agent_name: Some("opencode".into()),
            agent_version: None,
            prompt_images_supported: false,
            session_fork_supported: true,
            session_load_supported: false,
            side_session_supported: false,
            side_session_unsupported_reason: None,
        });
        s.apply_event(UiEvent::SessionUpdate(
            SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
                cmd("review_pr"),
                AvailableCommand::new("new", "agent-provided command"),
                AvailableCommand::new("clear", "agent-provided command"),
                AvailableCommand::new("load", "agent-provided command"),
                AvailableCommand::new("fork", "agent-provided command"),
                AvailableCommand::new("agents", "agent-provided command"),
            ])),
        ));

        let names: Vec<&str> = s
            .available_commands
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "new",
                "clear",
                "compact",
                "load",
                "export",
                "agents",
                "subagents",
                "terminals",
                "review",
                "diff",
                "mjconfig",
                "memory",
                "ragnarok",
                "fork",
                "review_pr"
            ]
        );
        assert_eq!(s.available_commands[0].description, "start a new session");
        assert_eq!(
            s.available_commands[1].description,
            "start a fresh session with the current agent"
        );
        assert_eq!(
            s.available_commands[2].description,
            "compact the primary agent's session where supported"
        );
        assert_eq!(
            s.available_commands[3].description,
            "load a previous session"
        );
        assert_eq!(
            s.available_commands[4].description,
            "export primary transcript; add full for nested agents"
        );
        assert_eq!(
            s.available_commands[5].description,
            "show active model selections and usage"
        );
        assert_eq!(
            s.available_commands
                .iter()
                .find(|command| command.name == "fork")
                .expect("fork command should be present")
                .description,
            "fork the current session (unstable ACP extension)"
        );
    }

    #[test]
    fn available_command_updates_do_not_add_fork_without_capability() {
        let mut s = AppState::new();
        s.apply_event(UiEvent::SessionUpdate(
            SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
                cmd("review_pr"),
                AvailableCommand::new("fork", "agent-provided command"),
            ])),
        ));

        let names: Vec<&str> = s
            .available_commands
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "new",
                "clear",
                "compact",
                "load",
                "export",
                "agents",
                "subagents",
                "terminals",
                "review",
                "diff",
                "mjconfig",
                "memory",
                "ragnarok",
                "review_pr"
            ]
        );
    }

    #[test]
    fn autocomplete_filters_by_prefix() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "/cre".to_string();
        s.update_autocomplete();
        assert!(s.autocomplete.visible);
        let names: Vec<&str> = s
            .autocomplete
            .matches
            .iter()
            .map(|&i| s.available_commands[i].name.as_str())
            .collect();
        assert_eq!(names, vec!["create_plan"]);
    }

    #[test]
    fn autocomplete_falls_back_to_substring_when_no_prefix_matches() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        // Nothing starts with "plan" but "create_plan" contains it.
        s.input = "/plan".to_string();
        s.update_autocomplete();
        assert!(s.autocomplete.visible);
        let names: Vec<&str> = s
            .autocomplete
            .matches
            .iter()
            .map(|&i| s.available_commands[i].name.as_str())
            .collect();
        assert_eq!(names, vec!["create_plan"]);
    }

    #[test]
    fn autocomplete_hides_once_user_types_an_argument() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "/create_plan ".to_string();
        s.update_autocomplete();
        assert!(
            !s.autocomplete.visible,
            "popover must close once the user commits to a command + arg"
        );
    }

    #[test]
    fn autocomplete_movement_wraps_at_both_ends() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "/".to_string();
        s.update_autocomplete();
        let total = s.autocomplete.matches.len();
        assert!(total >= 2);
        assert_eq!(s.autocomplete.selected, 0);
        s.autocomplete_move(-1);
        assert_eq!(s.autocomplete.selected, total - 1, "wraps to end on Up");
        s.autocomplete_move(1);
        assert_eq!(s.autocomplete.selected, 0, "wraps back to start on Down");
    }

    #[test]
    fn autocomplete_accept_replaces_input_with_command_and_trailing_space() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "/cre".to_string();
        s.update_autocomplete();
        assert!(s.autocomplete.visible);
        assert!(s.autocomplete_accept());
        assert_eq!(s.input, "/create_plan ");
        assert!(!s.autocomplete.visible, "popover closes after acceptance");
    }

    #[test]
    fn file_autocomplete_accepts_inline_query_as_resource_link() {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(directory.path().join("src")).expect("create src");
        let file = directory.path().join("src/acp.rs");
        std::fs::write(&file, "acp").expect("write source");

        let mut state = AppState::new();
        state.session_cwd = directory.path().to_path_buf();
        state.input = "Review @acp".to_string();
        state.input_cursor = state.input.chars().count();
        state.update_autocomplete();

        assert!(matches!(
            state.autocomplete.kind,
            AutocompleteKind::Files { .. }
        ));
        assert!(!state.autocomplete.visible);
        finish_file_autocomplete_scan(&mut state);
        assert!(state.autocomplete.visible);
        let paths: Vec<&str> = state
            .autocomplete
            .matches
            .iter()
            .filter_map(|index| state.autocomplete_file_path(*index))
            .collect();
        assert_eq!(paths, vec!["src/acp.rs"]);

        assert!(state.autocomplete_accept());
        assert_eq!(state.input, "Review  ");
        assert_eq!(state.input_cursor, 8);
        assert_eq!(state.file_attachments.len(), 1);
        let attachment = &state.file_attachments[0];
        assert_eq!(attachment.position, 7);
        assert_eq!(attachment.display_path, "src/acp.rs");
        assert_eq!(attachment.resource.name, "src/acp.rs");
        assert_eq!(attachment.resource.size, Some(3));
        assert_eq!(
            attachment.resource.uri,
            url::Url::from_file_path(file.canonicalize().expect("canonical file"))
                .expect("file URL")
                .to_string()
        );
    }

    #[test]
    fn file_autocomplete_uses_the_query_at_the_cursor() {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(directory.path().join("src")).expect("create src");
        std::fs::write(directory.path().join("src/app.rs"), "app").expect("write source");

        let mut state = AppState::new();
        state.session_cwd = directory.path().to_path_buf();
        state.input = "Review @app, then continue".to_string();
        state.input_cursor = "Review @app".chars().count();
        state.update_autocomplete();
        finish_file_autocomplete_scan(&mut state);
        assert!(state.autocomplete.visible);

        assert!(state.autocomplete_accept());
        assert_eq!(state.input, "Review  , then continue");
        assert_eq!(state.file_attachments[0].display_path, "src/app.rs");
    }

    #[test]
    fn file_autocomplete_does_not_trigger_inside_email_address() {
        let mut state = AppState::new();
        state.input = "ask dev@example.com".to_string();
        state.input_cursor = state.input.chars().count();

        state.update_autocomplete();

        assert!(!state.autocomplete.visible);
        assert!(state.autocomplete.matches.is_empty());
    }

    #[test]
    fn file_autocomplete_respects_gitignore() {
        let directory = tempfile::tempdir().expect("tempdir");
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(directory.path())
            .status()
            .expect("run git init");
        assert!(status.success());
        std::fs::write(directory.path().join(".gitignore"), "ignored.txt\n")
            .expect("write gitignore");
        std::fs::write(directory.path().join("ignored.txt"), "ignored")
            .expect("write ignored file");
        std::fs::write(directory.path().join("visible.txt"), "visible")
            .expect("write visible file");

        let mut state = AppState::new();
        state.session_cwd = directory.path().to_path_buf();
        state.input = "@ignored".to_string();
        state.input_cursor = state.input.chars().count();
        state.update_autocomplete();
        finish_file_autocomplete_scan(&mut state);
        assert!(!state.autocomplete.visible);

        state.input = "@visible".to_string();
        state.input_cursor = state.input.chars().count();
        state.update_autocomplete();
        assert!(state.autocomplete.visible);
        let path = state.autocomplete.matches[0];
        assert_eq!(state.autocomplete_file_path(path), Some("visible.txt"));
    }

    #[test]
    fn file_autocomplete_indexes_additional_workspace_roots() {
        let primary = tempfile::tempdir().expect("primary tempdir");
        let additional = tempfile::tempdir().expect("additional tempdir");
        std::fs::write(additional.path().join("notes.md"), "notes").expect("write notes");

        let mut state = AppState::new();
        state.session_cwd = primary.path().to_path_buf();
        state.additional_workspace_roots = vec![additional.path().to_path_buf()];
        state.input = "@notes".to_string();
        state.input_cursor = state.input.chars().count();
        state.update_autocomplete();
        finish_file_autocomplete_scan(&mut state);

        assert!(state.autocomplete.visible);
        let path = state.autocomplete.matches[0];
        let root_label = mj_core::paths::folder_label(additional.path());
        assert_eq!(
            state.autocomplete_file_path(path),
            Some(format!("{root_label}/notes.md").as_str())
        );
        assert!(state.autocomplete_accept());
        assert!(state.file_attachments[0].resource.uri.contains("notes.md"));
    }

    #[test]
    fn autocomplete_keeps_selection_on_same_command_when_filter_narrows() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "/r".to_string();
        s.update_autocomplete();
        // Walk down to "research_codebase" (second of the two `/r*` matches).
        s.autocomplete_move(1);
        let chosen = s.available_commands[s.autocomplete.matches[s.autocomplete.selected]]
            .name
            .clone();
        assert_eq!(chosen, "research_codebase");

        s.input = "/res".to_string();
        s.update_autocomplete();
        let still_chosen = s.available_commands[s.autocomplete.matches[s.autocomplete.selected]]
            .name
            .clone();
        assert_eq!(
            still_chosen, "research_codebase",
            "selection should follow the command across filter changes"
        );
    }

    #[test]
    fn autocomplete_stays_visible_during_streaming() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "/cre".to_string();
        s.record_user_prompt("placeholder".to_string());
        s.input = "/cre".to_string();
        s.update_autocomplete();
        assert!(
            s.autocomplete.visible,
            "input remains editable during streaming; popover should stay available"
        );
    }

    #[test]
    fn autocomplete_remains_visible_when_streaming_finishes() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.record_user_prompt("placeholder".to_string());
        s.input = "/cre".to_string();
        s.update_autocomplete();
        assert!(s.autocomplete.visible);

        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        assert!(s.autocomplete.visible);
    }

    #[test]
    fn autocomplete_hides_when_permission_request_arrives() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "/cre".to_string();
        s.update_autocomplete();
        assert!(s.autocomplete.visible);

        s.apply_event(UiEvent::PermissionRequest(permission_prompt()));
        assert!(!s.autocomplete.visible);
    }

    #[test]
    fn autocomplete_hidden_after_runtime_closes() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.mark_runtime_closed();
        s.input = "/".to_string();

        s.update_autocomplete();

        assert!(!s.autocomplete.visible);
        assert!(s.autocomplete.matches.is_empty());
    }

    #[test]
    fn is_streaming_tracks_connection_state_across_full_turn_lifecycle() {
        // Pins the state helpers: is_streaming mirrors prompt-turn states,
        // while is_busy also covers lifecycle operations such as fork.
        let mut s = AppState::new();
        seed_commands(&mut s);

        // Launching / Initializing / Ready: input is editable, popover
        // shows, Ctrl-C quits rather than cancelling.
        assert!(!s.is_streaming(), "Launching must not count as streaming");
        assert!(!s.is_busy(), "Launching must not count as busy");
        s.apply_event(UiEvent::Connected {
            agent_name: Some("opencode".into()),
            agent_version: None,
            prompt_images_supported: false,
            session_fork_supported: false,
            session_load_supported: false,
            side_session_supported: false,
            side_session_unsupported_reason: None,
        });
        assert!(
            !s.is_streaming(),
            "Initializing must not count as streaming"
        );
        assert!(!s.is_busy(), "Initializing must not count as busy");
        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
            resumed: false,
        });
        assert!(!s.is_streaming(), "Ready must not count as streaming");
        assert!(!s.is_busy(), "Ready must not count as busy");
        s.input = "/cre".to_string();
        s.update_autocomplete();
        assert!(s.autocomplete.visible, "Ready: popover must be visible");

        // Forking is busy for submission gating but not a prompt stream.
        s.mark_forking();
        assert_eq!(s.connection_state, ConnectionState::Forking);
        assert!(!s.is_streaming(), "Forking must not count as streaming");
        assert!(s.is_busy(), "Forking must count as busy");
        s.apply_event(UiEvent::SessionStarted {
            session_id: "forked-sess".into(),
            resumed: false,
        });
        assert_eq!(s.connection_state, ConnectionState::Ready);
        assert!(!s.is_busy(), "Ready after fork must not count as busy");

        // Streaming: input stays editable, popover remains available, Ctrl-C cancels.
        s.input.clear();
        s.record_user_prompt("hi".to_string());
        assert_eq!(s.connection_state, ConnectionState::Streaming);
        assert!(s.is_streaming(), "Streaming must count as streaming");
        assert!(s.is_busy(), "Streaming must count as busy");
        s.input = "/cre".to_string();
        s.update_autocomplete();
        assert!(s.autocomplete.visible, "Streaming: popover must be visible");

        // Cancelling: still a turn in flight; popover stays available, the
        // prompt timer keeps running, duplicate user chunks stay suppressed.
        s.mark_cancelling();
        assert_eq!(s.connection_state, ConnectionState::Cancelling);
        assert!(s.is_streaming(), "Cancelling must still count as streaming");
        assert!(s.is_busy(), "Cancelling must count as busy");
        s.update_autocomplete();
        assert!(
            s.autocomplete.visible,
            "Cancelling: popover must remain visible"
        );
        assert!(
            s.active_turn_elapsed().is_some(),
            "Cancelling: turn timer must still tick"
        );

        // PromptDone returns to Ready: popover reappears, input editable again.
        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::Cancelled,
            usage: None,
        });
        assert_eq!(s.connection_state, ConnectionState::Ready);
        assert!(!s.is_streaming(), "Ready (after turn) must not stream");
        assert!(!s.is_busy(), "Ready (after turn) must not be busy");
        assert!(
            s.autocomplete.visible,
            "Ready (after turn): popover must reappear"
        );

        // Fatal/Closed: input gating gives way to runtime_closed, but
        // is_streaming itself must report false either way.
        s.apply_event(UiEvent::Fatal("kaboom".into()));
        assert!(!s.is_streaming(), "Fatal must not count as streaming");
        assert!(!s.is_busy(), "Fatal must not count as busy");

        let mut s = AppState::new();
        s.mark_runtime_closed();
        assert!(!s.is_streaming(), "Closed must not count as streaming");
        assert!(!s.is_busy(), "Closed must not count as busy");
    }

    // -- Prompt history tests -------------------------------------------------

    #[test]
    fn prompt_history_previous_next_navigates_and_restores() {
        let mut s = AppState::new();
        s.record_user_prompt("first".into());
        s.record_user_prompt("second".into());
        s.record_user_prompt("third".into());

        // Start with empty input.
        s.input = "".into();
        s.input_cursor = 0;

        // Up navigates to most recent (third).
        assert!(s.prompt_history_previous());
        assert_eq!(s.input, "third");
        assert!(s.prompt_history_previous());
        assert_eq!(s.input, "second");
        assert!(s.prompt_history_previous());
        assert_eq!(s.input, "first");
        // Already at oldest — no-op.
        assert!(!s.prompt_history_previous());
        assert_eq!(s.input, "first");

        // Down forward to newest.
        assert!(s.prompt_history_next());
        assert_eq!(s.input, "second");
        assert!(s.prompt_history_next());
        assert_eq!(s.input, "third");
        // Past the end restores saved input (empty).
        assert!(s.prompt_history_next());
        assert_eq!(s.input, "");

        // Further Down is a no-op.
        assert!(!s.prompt_history_next());
    }

    #[test]
    fn prompt_history_saves_and_restores_partial_input() {
        let mut s = AppState::new();
        s.record_user_prompt("hello".into());

        s.input = "draft".into();
        s.input_cursor = 5;

        // Up → history.
        assert!(s.prompt_history_previous());
        assert_eq!(s.input, "hello");
        // Down past most recent → saved input restored.
        assert!(s.prompt_history_next());
        assert_eq!(s.input, "draft");
        // history_cursor is None, so no more forward.
        assert!(!s.prompt_history_next());
    }

    #[test]
    fn prompt_history_restores_file_resource_links_and_saved_draft_chips() {
        let history_resource = PromptResource {
            name: "src/app.rs".to_string(),
            uri: "file:///workspace/src/app.rs".to_string(),
            size: Some(42),
        };
        let draft_resource = PromptResource {
            name: "README.md".to_string(),
            uri: "file:///workspace/README.md".to_string(),
            size: Some(7),
        };
        let mut state = AppState::new();
        state.record_user_prompt_with_resources(
            "Review @src/app.rs".to_string(),
            vec![history_resource.clone()],
        );
        state.input = "Then ".to_string();
        state.input_cursor = state.input.chars().count();
        state.file_attachments = vec![FileAttachment {
            id: 99,
            position: 5,
            display_path: draft_resource.name.clone(),
            resource: draft_resource.clone(),
        }];

        assert!(state.prompt_history_previous());
        assert_eq!(state.input, "Review ");
        assert_eq!(state.file_attachments.len(), 1);
        assert_eq!(state.file_attachments[0].position, 7);
        assert_eq!(state.file_attachments[0].resource, history_resource);

        assert!(state.prompt_history_next());
        assert_eq!(state.input, "Then ");
        assert_eq!(state.file_attachments.len(), 1);
        assert_eq!(state.file_attachments[0].resource, draft_resource);
    }

    #[test]
    fn prompt_history_empty_history_does_nothing() {
        let mut s = AppState::new();
        s.input = "abc".into();
        assert!(!s.prompt_history_previous());
        assert!(!s.prompt_history_next());
        assert_eq!(s.input, "abc");
    }

    #[test]
    fn prompt_history_editing_resets_navigation() {
        let mut s = AppState::new();
        s.record_user_prompt("historical".into());
        s.input.clear();
        s.prompt_history_previous();
        assert_eq!(s.input, "historical");

        // Simulate typing a character (the UI calls reset_history_navigation
        // inside insert_text_at_cursor).
        s.reset_history_navigation();
        // After reset, Down shouldn't navigate.
        assert!(!s.prompt_history_next());
        // And Up starts a fresh navigation from the last entry.
        assert!(s.prompt_history_previous());
        assert_eq!(s.input, "historical");
    }

    #[test]
    fn prompt_history_deduplicates_consecutive_identical_prompts() {
        let mut s = AppState::new();
        s.record_user_prompt("dup".into());
        s.record_user_prompt("dup".into());
        s.record_user_prompt("unique".into());
        s.record_user_prompt("dup".into());

        assert_eq!(s.prompt_history.len(), 3);
        assert_eq!(s.prompt_history[0], "dup");
        assert_eq!(s.prompt_history[1], "unique");
        assert_eq!(s.prompt_history[2], "dup");
    }

    #[test]
    fn prompt_history_reset_on_autocomplete_accept() {
        let mut s = AppState::new();
        s.available_commands
            .push(AvailableCommand::new("greet", "a friendly greeting"));
        s.record_user_prompt("greetings".into());

        // Navigate into history.
        s.input.clear();
        s.prompt_history_previous();
        assert_eq!(s.input, "greetings");

        // Simulate autocomplete accept: manual overwrite + reset.
        s.input = "/greet ".into();
        s.input_cursor = s.input.chars().count();
        s.reset_history_navigation();

        // After reset, history is no longer active.
        assert!(!s.prompt_history_next());
    }

    #[test]
    fn prompt_history_starts_new_navigation_after_submit() {
        let mut s = AppState::new();
        s.record_user_prompt("a".into());
        s.input = "prev".into();
        s.prompt_history_previous();
        assert_eq!(s.input, "a");

        // Submit a new prompt (record_user_prompt resets navigation).
        s.input = "b".into();
        s.record_user_prompt("b".into());
        assert_eq!(s.prompt_history.len(), 2);

        // New navigation starts from "b".
        s.input.clear();
        assert!(s.prompt_history_previous());
        assert_eq!(s.input, "b");
    }

    // ---- Ragnarok arena state ----------------------------------------------

    fn ragnarok_card(id: usize, name: &str) -> crate::ragnarok::FighterCard {
        crate::ragnarok::FighterCard {
            id,
            agent_source_id: format!("agent-{id}"),
            model_value: name.to_lowercase(),
            model_name: name.to_string(),
            pass_at_1_bps: 1400 + id as u32,
            mean_cost_usd: 0.0,
        }
    }

    fn arena_state() -> AppState {
        let mut s = AppState::new();
        let (abort_tx, _abort_rx) = tokio::sync::watch::channel(false);
        let (proceed_tx, _proceed_rx) = tokio::sync::watch::channel(false);
        s.ragnarok = Some(RagnarokUi::new(
            "build a thing".into(),
            abort_tx,
            proceed_tx,
        ));
        s
    }

    #[test]
    fn ragnarok_launch_request_roundtrips() {
        let mut s = AppState::new();
        assert!(s.take_ragnarok_launch().is_none());
        s.request_ragnarok("forge a hammer".into());
        assert_eq!(s.take_ragnarok_launch().as_deref(), Some("forge a hammer"));
        assert!(s.take_ragnarok_launch().is_none(), "request is one-shot");
    }

    #[test]
    fn ragnarok_observation_mirrors_the_reduced_arena() {
        use crate::ragnarok::{FighterState, Phase, RagnarokEvent};

        let mut s = arena_state();
        s.apply_ragnarok_event(RagnarokEvent::Roster(vec![ragnarok_card(0, "Opus")]));
        s.apply_ragnarok_event(RagnarokEvent::Phase(Phase::Approval));
        s.apply_ragnarok_event(RagnarokEvent::FighterState {
            id: 0,
            state: FighterState::Fighting,
        });
        s.apply_ragnarok_event(RagnarokEvent::FighterWorktree {
            id: 0,
            name: "ragnarok-opus".into(),
            path: PathBuf::from("/tmp/ragnarok-opus"),
            base_sha: "base-opus".into(),
        });

        let observation = s.ragnarok.as_ref().expect("arena").observation();
        assert_eq!(observation.task, "build a thing");
        assert_eq!(observation.phase, Phase::Approval);
        assert!(observation.awaiting_approval);
        assert_eq!(observation.fighters.len(), 1);
        assert_eq!(observation.fighters[0].agent_source_id, "agent-0");
        assert_eq!(observation.fighters[0].model_name, "Opus");
        assert_eq!(observation.fighters[0].state, FighterState::Fighting);
        assert_eq!(
            observation.fighters[0].worktree_name.as_deref(),
            Some("ragnarok-opus")
        );
    }

    #[test]
    fn ragnarok_events_fold_into_arena_state() {
        use crate::ragnarok::{ActionKind, FighterState, Phase, RagnarokEvent, TextLane, Verdict};
        let mut s = arena_state();
        s.apply_ragnarok_event(RagnarokEvent::Phase(Phase::Combat));
        s.apply_ragnarok_event(RagnarokEvent::Roster(vec![
            ragnarok_card(0, "Opus"),
            ragnarok_card(1, "GPT-5.5"),
        ]));
        s.apply_ragnarok_event(RagnarokEvent::FighterState {
            id: 1,
            state: FighterState::Fighting,
        });
        s.apply_ragnarok_event(RagnarokEvent::FighterAction {
            id: 1,
            action: ActionKind::Forge,
            detail: "src/main.rs".into(),
        });
        s.apply_ragnarok_event(RagnarokEvent::FighterText {
            id: 1,
            lane: TextLane::Message,
            chunk: "I shall".into(),
        });
        s.apply_ragnarok_event(RagnarokEvent::FighterText {
            id: 1,
            lane: TextLane::Review,
            chunk: "their code is bad".into(),
        });
        s.apply_ragnarok_event(RagnarokEvent::FighterWorktree {
            id: 1,
            name: "ragnarok-gpt".into(),
            path: PathBuf::from("/tmp/ragnarok-gpt"),
            base_sha: "base-gpt".into(),
        });
        s.apply_ragnarok_event(RagnarokEvent::FighterJoined(ragnarok_card(2, "Judge")));
        s.apply_ragnarok_event(RagnarokEvent::FighterState {
            id: 0,
            state: FighterState::Slain("tripped on a rune".into()),
        });

        let arena = s.ragnarok.as_ref().expect("arena");
        assert_eq!(arena.phase, Phase::Combat);
        assert_eq!(arena.fighters.len(), 3);
        let f1 = arena.fighter(1).expect("fighter 1");
        assert_eq!(f1.state, FighterState::Fighting);
        assert_eq!(f1.transcript, "I shall");
        assert_eq!(f1.review_transcript, "their code is bad");
        assert_eq!(f1.worktree_name.as_deref(), Some("ragnarok-gpt"));
        assert_eq!(f1.actions_seen, 1);
        assert_eq!(
            arena.fighter(2).expect("late judge").card.model_name,
            "Judge"
        );
        // The slain fighter's reason lands in the feed.
        assert!(
            arena
                .feed
                .iter()
                .any(|(id, text)| *id == Some(0) && text.contains("tripped on a rune"))
        );

        s.apply_ragnarok_event(RagnarokEvent::Verdict(Box::new(Verdict {
            clear_winner: Some(1),
            finalists: None,
            ranking: vec![1, 0],
            review_verdicts: Vec::new(),
            reasoning: "the hammer spoke".into(),
            thor_fallback: false,
        })));
        s.apply_ragnarok_event(RagnarokEvent::Done);
        let arena = s.ragnarok.as_ref().expect("arena");
        assert!(arena.battle_over());
        assert_eq!(arena.verdict.as_ref().and_then(|v| v.clear_winner), Some(1));
    }

    #[test]
    fn fighter_transcript_separates_message_thought_and_tool_segments() {
        use crate::ragnarok::{RagnarokEvent, TextLane};
        let mut s = arena_state();
        s.apply_ragnarok_event(RagnarokEvent::Roster(vec![ragnarok_card(0, "Opus")]));

        for (lane, chunk) in [
            (TextLane::Message, "I will forge "),
            (TextLane::Message, "the hammer."),
            (TextLane::Thought, "but first I must read"),
            (TextLane::Tool, "\n⚙ [read] src/main.rs\n"),
            (TextLane::Message, "Reading the file now."),
        ] {
            s.apply_ragnarok_event(RagnarokEvent::FighterText {
                id: 0,
                lane,
                chunk: chunk.into(),
            });
        }

        let arena = s.ragnarok.as_ref().expect("arena");
        let body = &arena.fighter(0).expect("fighter 0").transcript;

        assert!(
            body.contains("I will forge the hammer."),
            "message deltas should not be broken mid-sentence: {body:?}"
        );
        assert!(
            body.contains("the hammer.\n\n🧠 thinking\nbut first I must read"),
            "thought should start a new labeled block: {body:?}"
        );
        assert!(
            body.contains("but first I must read\n\n⚙ [read] src/main.rs"),
            "tool header should be preserved after a clean break: {body:?}"
        );
        assert!(
            body.contains("src/main.rs\n\n💬 message\nReading the file now."),
            "message after a thought should start a new labeled block: {body:?}"
        );
        assert!(
            !body.contains("readReading"),
            "thought must not bleed into the following message: {body:?}"
        );
    }

    #[test]
    fn close_ragnarok_pushes_summary_with_winner_and_worktrees() {
        use crate::ragnarok::{RagnarokEvent, Verdict};
        let mut s = arena_state();
        s.apply_ragnarok_event(RagnarokEvent::Roster(vec![
            ragnarok_card(0, "Opus"),
            ragnarok_card(1, "GPT-5.5"),
        ]));
        s.apply_ragnarok_event(RagnarokEvent::FighterWorktree {
            id: 0,
            name: "ragnarok-opus".into(),
            path: PathBuf::from("/tmp/ragnarok-opus"),
            base_sha: "base-opus".into(),
        });
        s.apply_ragnarok_event(RagnarokEvent::Verdict(Box::new(Verdict {
            clear_winner: Some(0),
            finalists: None,
            ranking: vec![0, 1],
            review_verdicts: Vec::new(),
            reasoning: "flawless".into(),
            thor_fallback: false,
        })));
        s.apply_ragnarok_event(RagnarokEvent::DraftPrPublished {
            winner: 0,
            url: "https://github.com/example/repo/pull/123".into(),
        });
        s.close_ragnarok();
        assert!(s.ragnarok.is_none());
        let Some(Entry::System(summary)) = s.transcript.last() else {
            panic!("expected a system summary entry");
        };
        assert!(summary.contains("winner"), "summary: {summary}");
        assert!(summary.contains("Opus"), "summary: {summary}");
        assert!(summary.contains("ragnarok-opus"), "summary: {summary}");
        assert!(summary.contains("mj --worktree"), "summary: {summary}");
        assert!(
            summary.contains("https://github.com/example/repo/pull/123"),
            "summary: {summary}"
        );
        // Closing twice is harmless.
        s.close_ragnarok();
    }

    #[test]
    fn ragnarok_split_summary_marks_the_users_pick() {
        use crate::ragnarok::{RagnarokEvent, Verdict};
        let mut s = arena_state();
        s.apply_ragnarok_event(RagnarokEvent::Roster(vec![
            ragnarok_card(0, "Opus"),
            ragnarok_card(1, "GPT-5.5"),
        ]));
        s.apply_ragnarok_event(RagnarokEvent::Verdict(Box::new(Verdict {
            clear_winner: None,
            finalists: Some((0, 1)),
            ranking: vec![0, 1],
            review_verdicts: Vec::new(),
            reasoning: "dead heat".into(),
            thor_fallback: false,
        })));
        if let Some(arena) = s.ragnarok.as_mut() {
            arena.chosen_finalist = Some(1);
        }
        s.close_ragnarok();
        let Some(Entry::System(summary)) = s.transcript.last() else {
            panic!("expected a system summary entry");
        };
        assert!(summary.contains("split decision"), "summary: {summary}");
        let pick_line = summary
            .lines()
            .find(|l| l.contains("← your pick"))
            .expect("pick marker");
        assert!(pick_line.contains("GPT-5.5"), "line: {pick_line}");
    }

    #[test]
    fn ragnarok_approval_gate_unleashes_via_watch() {
        let mut s = AppState::new();
        let (abort_tx, _abort_rx) = tokio::sync::watch::channel(false);
        let (proceed_tx, proceed_rx) = tokio::sync::watch::channel(false);
        s.ragnarok = Some(RagnarokUi::new("task".into(), abort_tx, proceed_tx));
        s.apply_ragnarok_event(crate::ragnarok::RagnarokEvent::Phase(
            crate::ragnarok::Phase::Approval,
        ));
        let arena = s.ragnarok.as_ref().expect("arena");
        assert!(arena.awaiting_approval());
        assert!(!*proceed_rx.borrow());
        arena.unleash();
        assert!(*proceed_rx.borrow());
        // Once combat starts the gate is no longer pending.
        s.apply_ragnarok_event(crate::ragnarok::RagnarokEvent::Phase(
            crate::ragnarok::Phase::Combat,
        ));
        assert!(!s.ragnarok.as_ref().unwrap().awaiting_approval());
    }

    #[test]
    fn push_capped_trims_front_at_char_boundary() {
        let mut buf = String::new();
        push_capped(&mut buf, "hello", 1000);
        assert_eq!(buf, "hello");
        let mut buf = "⚔".repeat(100);
        push_capped(&mut buf, &"x".repeat(400), 300);
        assert!(buf.len() <= 300 + 4, "len {}", buf.len());
        assert!(buf.starts_with('…'));
        assert!(buf.ends_with('x'));
    }

    #[test]
    fn ragnarok_cycle_fighter_wraps() {
        let mut s = arena_state();
        s.apply_ragnarok_event(crate::ragnarok::RagnarokEvent::Roster(vec![
            ragnarok_card(0, "A"),
            ragnarok_card(1, "B"),
            ragnarok_card(2, "C"),
        ]));
        let arena = s.ragnarok.as_mut().expect("arena");
        arena.cycle_fighter(-1);
        assert_eq!(arena.selected_fighter, 2);
        arena.cycle_fighter(1);
        assert_eq!(arena.selected_fighter, 0);
    }

    #[test]
    fn ragnarok_feed_scroll_preserves_scrolled_view() {
        let mut s = arena_state();
        for line in ["one", "two", "three", "four"] {
            s.apply_ragnarok_event(crate::ragnarok::RagnarokEvent::Log {
                fighter: None,
                text: line.to_string(),
            });
        }
        let arena = s.ragnarok.as_mut().expect("arena");

        assert_eq!(arena.feed_scroll_for_rows(2), 0);
        arena.scroll_feed(1);
        assert_eq!(arena.feed_scroll_for_rows(2), 1);
        arena.scroll_feed(99);
        assert_eq!(arena.feed_scroll_for_rows(2), 2);

        arena.push_feed(None, "five".to_string());
        assert_eq!(arena.feed_scroll_for_rows(2), 3);

        arena.scroll_feed(-2);
        assert_eq!(arena.feed_scroll_for_rows(2), 2);
        arena.scroll_feed(-99);
        assert_eq!(arena.feed_scroll_for_rows(2), 0);
    }

    fn feature_capabilities() -> FeatureHintCapabilities {
        FeatureHintCapabilities {
            subagents: true,
            ragnarok: true,
            voice: true,
            fork: true,
            side: true,
            images: true,
            fullscreen: true,
        }
    }

    /// Capabilities where exactly `requirement` is (un)satisfied and every
    /// other gate stays off, so a hint's eligibility flips with `enabled`.
    fn capabilities_for(
        requirement: FeatureHintRequirement,
        enabled: bool,
    ) -> FeatureHintCapabilities {
        let mut caps = FeatureHintCapabilities {
            subagents: false,
            ragnarok: false,
            voice: false,
            fork: false,
            side: false,
            images: false,
            fullscreen: false,
        };
        match requirement {
            FeatureHintRequirement::Always => {}
            FeatureHintRequirement::Subagents => caps.subagents = enabled,
            FeatureHintRequirement::Ragnarok => caps.ragnarok = enabled,
            FeatureHintRequirement::Voice => caps.voice = enabled,
            FeatureHintRequirement::Fork => caps.fork = enabled,
            FeatureHintRequirement::Side => caps.side = enabled,
            FeatureHintRequirement::Images => caps.images = enabled,
            FeatureHintRequirement::Fullscreen => caps.fullscreen = enabled,
            FeatureHintRequirement::Inline => caps.fullscreen = !enabled,
        }
        caps
    }

    #[test]
    fn every_gated_feature_hint_follows_its_own_capability() {
        for (index, hint) in FEATURE_HINTS.iter().enumerate() {
            if hint.requirement == FeatureHintRequirement::Always {
                continue;
            }

            let mut state = AppState::new();
            state.feature_hint_cursor = index;
            state.completed_turns_since_hint = FEATURE_HINT_INTERVAL_TURNS - 1;
            assert!(state.maybe_record_feature_hint(capabilities_for(hint.requirement, true)));
            let Some(Entry::FeatureHint(text)) = state.transcript.last() else {
                panic!("expected feature hint at index {index}");
            };
            assert_eq!(
                text.as_str(),
                hint.text,
                "cursor at index {index} should select its own hint when supported"
            );

            let mut state = AppState::new();
            state.feature_hint_cursor = index;
            state.completed_turns_since_hint = FEATURE_HINT_INTERVAL_TURNS - 1;
            assert!(state.maybe_record_feature_hint(capabilities_for(hint.requirement, false)));
            let Some(Entry::FeatureHint(text)) = state.transcript.last() else {
                panic!("expected fallback feature hint at index {index}");
            };
            assert_ne!(
                text.as_str(),
                hint.text,
                "hint at index {index} must be skipped when unsupported"
            );
        }
    }

    /// The loop test above skips `Always` hints, so it cannot notice a gated
    /// hint whose requirement was accidentally widened to `Always`. Pin each
    /// gated hint's declared requirement here instead.
    #[test]
    fn gated_feature_hints_keep_their_capability_requirements() {
        let expected = [
            (
                "F8 opens the nested-agent viewer",
                FeatureHintRequirement::Subagents,
            ),
            ("/ragnarok", FeatureHintRequirement::Ragnarok),
            ("dictate", FeatureHintRequirement::Voice),
            ("/fork", FeatureHintRequirement::Fork),
            ("/side", FeatureHintRequirement::Side),
            ("Paste an image", FeatureHintRequirement::Images),
            (
                "With an empty prompt, Ctrl+F",
                FeatureHintRequirement::Fullscreen,
            ),
            ("Press Alt+T", FeatureHintRequirement::Fullscreen),
            ("Open the transcript reader", FeatureHintRequirement::Inline),
        ];
        for (needle, requirement) in expected {
            let matches: Vec<_> = FEATURE_HINTS
                .iter()
                .filter(|hint| hint.text.contains(needle))
                .collect();
            assert_eq!(
                matches.len(),
                1,
                "expected exactly one hint containing {needle:?}"
            );
            assert_eq!(
                matches[0].requirement, requirement,
                "hint containing {needle:?} has the wrong requirement"
            );
        }
        let gated = FEATURE_HINTS
            .iter()
            .filter(|hint| hint.requirement != FeatureHintRequirement::Always)
            .count();
        assert_eq!(
            gated,
            expected.len(),
            "gated hints changed; update this table to match"
        );
    }

    #[test]
    fn feature_hints_are_infrequent_and_rotate() {
        let mut state = AppState::new();
        for _ in 0..FEATURE_HINT_INTERVAL_TURNS - 1 {
            assert!(!state.maybe_record_feature_hint(feature_capabilities()));
        }
        assert!(state.maybe_record_feature_hint(feature_capabilities()));
        let first = match state.transcript.last() {
            Some(Entry::FeatureHint(text)) => text.clone(),
            other => panic!("expected feature hint, got {other:?}"),
        };

        for _ in 0..FEATURE_HINT_INTERVAL_TURNS - 1 {
            assert!(!state.maybe_record_feature_hint(feature_capabilities()));
        }
        assert!(state.maybe_record_feature_hint(feature_capabilities()));
        let second = match state.transcript.last() {
            Some(Entry::FeatureHint(text)) => text,
            other => panic!("expected feature hint, got {other:?}"),
        };
        assert_ne!(&first, second);
    }

    #[test]
    fn feature_hints_include_thought_output_configuration() {
        assert!(FEATURE_HINTS.iter().any(|hint| {
            hint.requirement == FeatureHintRequirement::Always
                && hint.text.contains("Default or Full thought output")
                && hint.text.contains("/mjconfig")
        }));
    }

    #[test]
    fn feature_hints_include_spinner_and_ansi_appearance_configuration() {
        let spinner = FEATURE_HINTS
            .iter()
            .find(|hint| {
                hint.requirement == FeatureHintRequirement::Always
                    && hint.text.contains("activity spinner")
                    && hint.text.contains("/mjconfig")
            })
            .expect("spinner appearance hint");
        let ansi = FEATURE_HINTS
            .iter()
            .find(|hint| {
                hint.requirement == FeatureHintRequirement::Always
                    && hint.text.contains("strict 16-color ANSI theme")
                    && hint.text.contains("terminal or multiplexer")
                    && hint.text.contains("/mjconfig")
            })
            .expect("ANSI appearance hint");

        assert_ne!(
            spinner.text, ansi.text,
            "spinner and ANSI tips must remain separate"
        );
    }

    #[test]
    fn feature_hints_skip_unsupported_capabilities() {
        let mut state = AppState::new();
        state.feature_hint_cursor = FEATURE_HINTS
            .iter()
            .position(|hint| hint.requirement == FeatureHintRequirement::Subagents)
            .expect("subagent hint");
        state.completed_turns_since_hint = FEATURE_HINT_INTERVAL_TURNS - 1;

        assert!(state.maybe_record_feature_hint(FeatureHintCapabilities {
            subagents: false,
            ragnarok: false,
            voice: false,
            fork: false,
            side: false,
            images: false,
            fullscreen: false,
        }));
        let Some(Entry::FeatureHint(text)) = state.transcript.last() else {
            panic!("expected feature hint");
        };
        assert!(!text.contains("subagent"));
        assert!(!text.contains("ragnarok"));
        assert!(!text.contains("Ctrl+R"));
        assert!(!text.contains("/fork"));
        assert!(!text.contains("/side"));
        assert!(!text.contains("Ctrl+V"));
    }

    #[test]
    fn feature_hints_can_be_disabled_and_never_enter_prompt_history() {
        let mut state = AppState::new();
        state.feature_hints_enabled = false;
        for _ in 0..FEATURE_HINT_INTERVAL_TURNS * 2 {
            assert!(!state.maybe_record_feature_hint(feature_capabilities()));
        }
        assert!(state.transcript.is_empty());

        state.feature_hints_enabled = true;
        for _ in 0..FEATURE_HINT_INTERVAL_TURNS {
            state.maybe_record_feature_hint(feature_capabilities());
        }
        assert!(matches!(
            state.transcript.last(),
            Some(Entry::FeatureHint(_))
        ));
        assert!(state.prompt_history().is_empty());
    }

    #[test]
    fn keep_awake_follows_busy_connection_states() {
        let mut state = AppState::new();
        state.keep_awake.set_enabled(true);
        assert!(!state.keep_awake.wants_hold());

        state.set_connection_state(ConnectionState::Streaming);
        assert!(state.keep_awake.wants_hold());
        state.set_connection_state(ConnectionState::Cancelling);
        assert!(state.keep_awake.wants_hold());
        state.set_connection_state(ConnectionState::Ready);
        assert!(!state.keep_awake.wants_hold());

        state.set_connection_state(ConnectionState::Forking);
        assert!(state.keep_awake.wants_hold());
        state.set_connection_state(ConnectionState::Closed);
        assert!(!state.keep_awake.wants_hold());

        // The config switch gates the assertion even while streaming.
        state.set_connection_state(ConnectionState::Streaming);
        state.keep_awake.set_enabled(false);
        assert!(!state.keep_awake.wants_hold());
    }
}
