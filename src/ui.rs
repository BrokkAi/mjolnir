//! Ratatui-based terminal UI.
//!
//! Owns the Ratatui viewport and the crossterm event stream.
//! Pulls `UiEvent`s from the ACP runtime through `event_rx`, folds them
//! into `AppState`, redraws on every tick, and emits `UiCommand`s back
//! to the runtime when the user submits prompts or cancels.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::io::{self, Stdout, Write};
use std::ops::Range;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent_client_protocol::schema::v1::{
    AvailableCommandInput, SessionConfigOption, SessionConfigValueId, SessionUpdate, StopReason,
    ToolCallStatus,
};
use anyhow::{Context, Result};
use crossterm::cursor::MoveTo;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    Clear as CrosstermClear, ClearType as CrosstermClearType, EnterAlternateScreen,
    LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::backend::{Backend, ClearType};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Widget, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    AgentPickerStep, AppState, ArenaPane, ConfigValueChoice, ConnectionState,
    CurrentBranchPullRequest, ElicitationFormFieldKind, ElicitationView, Entry,
    PRIMARY_EFFORT_OPTIONS, PastedAttachment, PastedImageAttachment, PendingElicitation,
    PendingPermission, QUEUED_PROMPT_PREVIEW_WIDTH, QueuedPrompt, RagnarokDraftPrStatus,
    RagnarokFighterUi, RagnarokUi, StatusKind, StatusMessage, SubagentStatus, ToolCallOutput,
    UiExitReason, classify_elicitation, config_option_choices, config_option_current_value_label,
    primary_effort_value,
};
use crate::clipboard::{
    ClipboardImage, copy_to_clipboard, load_image_path_as_png, read_clipboard_image_as_png,
};
use crate::config;
use crate::event::{
    PermissionDecision, PermissionPrompt, PromptImage, ReviewTarget, SessionConfigTarget,
    SubagentEvent, SubagentOutcome, UiCommand, UiEvent,
};
use crate::notifications::TerminalNotificationBackend;
use crate::palette::TerminalTheme;
use crate::ragnarok;
use crate::ragnarok_sprites::{self, SpriteKind};
use crate::settings::{SettingsAction, draw_settings_panel};
use crate::speech::{dictation_error_message, run_dictation, voice_input_supported};
use crate::spinner::SpinnerStyle;
use crate::term::TrackedBackend;
use crate::text::truncate_text_to_width;
use crate::theme::TerminalThemeKind;
use crate::version::mjolnir_version_label;

const TRANSCRIPT_SCROLL_PAGE_STEP: usize = 5;
const TRANSCRIPT_SCROLL_WHEEL_STEP: usize = 3;
const PROMPT_SIDE_PADDING: u16 = 1;
pub const INLINE_CHAT_HEIGHT: u16 = 8;
const INLINE_EXPANDED_MAX_HEIGHT: u16 = 20;
const INLINE_TRANSCRIPT_TAIL_MAX_ROWS: usize = 12;
const INLINE_HELP_HEIGHT: u16 = 18;
const HELP_SCROLL_PAGE_STEP: u16 = 10;
/// Inline viewport height for the `/mjconfig` overlay (border + two sections).
const INLINE_MJCONFIG_HEIGHT: u16 = 24;
const QUEUED_PROMPT_VISIBLE_ROWS: usize = 3;
/// Workflow progress rows rendered before the area folds into a "… N more"
/// line. Normal orchestration has at most delegation and review active.
const WORKFLOW_PROGRESS_VISIBLE_ROWS: usize = 2;
const CURRENT_BRANCH_PR_POLL_INTERVAL: Duration = Duration::from_secs(5);
const CURSOR_POSITION_TIMEOUT_MESSAGE: &str =
    "The cursor position could not be read within a normal duration";
const INLINE_SETUP_RETRY_DELAY: Duration = Duration::from_millis(75);
const INLINE_NON_CURSOR_SETUP_ATTEMPTS: usize = 3;
const PASTE_BURST_CHAR_INTERVAL: Duration = Duration::from_millis(8);
const PASTE_BURST_IDLE_TIMEOUT: Duration = Duration::from_millis(16);
const PASTE_BURST_MIN_CHARS: usize = 3;
const NOTIFICATION_PREVIEW_CHARS: usize = 80;
const INLINE_RESIZE_REFLOW_DEBOUNCE: Duration = Duration::from_millis(75);
/// Codex's streaming commit cadence: one completed source line per 120 FPS
/// frame while output is keeping up.
const STREAM_COMMIT_INTERVAL: Duration = Duration::from_nanos(8_333_334);
const STREAM_CATCH_UP_LINES: usize = 8;
const STREAM_CATCH_UP_AGE: Duration = Duration::from_millis(120);

#[derive(Debug)]
struct CurrentBranchPrProbe {
    cwd: PathBuf,
    branch: Option<String>,
    gh_succeeded: bool,
    pull_request: Option<CurrentBranchPullRequest>,
}

#[derive(Deserialize)]
struct GhPullRequestView {
    number: u64,
    url: String,
    state: String,
}

async fn probe_current_branch_pull_request(cwd: PathBuf) -> CurrentBranchPrProbe {
    let branch = tokio::process::Command::new("git")
        .current_dir(&cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["branch", "--show-current"])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|branch| branch.trim().to_string())
        .filter(|branch| !branch.is_empty());

    let gh_output = tokio::process::Command::new("gh")
        .current_dir(&cwd)
        .env("GH_PROMPT_DISABLED", "1")
        .args(["pr", "view", "--json", "number,url,state"])
        .stdin(Stdio::null())
        .output()
        .await;
    let (gh_succeeded, pull_request) = match gh_output {
        Ok(output) if output.status.success() => {
            let pull_request = String::from_utf8(output.stdout)
                .ok()
                .and_then(|output| serde_json::from_str::<GhPullRequestView>(&output).ok())
                .filter(|pull_request| pull_request.state.eq_ignore_ascii_case("open"))
                .map(|pull_request| CurrentBranchPullRequest {
                    number: pull_request.number,
                    url: pull_request.url,
                });
            (true, pull_request)
        }
        _ => (false, None),
    };

    CurrentBranchPrProbe {
        cwd,
        branch,
        gh_succeeded,
        pull_request,
    }
}

fn apply_current_branch_pr_probe(state: &mut AppState, probe: CurrentBranchPrProbe) -> bool {
    if probe.cwd != state.session_cwd {
        return false;
    }

    let previous_branch = state.current_branch_pull_request_branch.clone();
    let previous_pull_request = state.current_branch_pull_request.clone();
    if previous_branch != probe.branch {
        state.current_branch_pull_request_branch = probe.branch;
        state.current_branch_pull_request = None;
    }
    if probe.gh_succeeded {
        state.current_branch_pull_request = probe.pull_request;
    }

    previous_branch != state.current_branch_pull_request_branch
        || previous_pull_request != state.current_branch_pull_request
}

/// Do not wait for a source newline forever. ACP prose chunks are arbitrary
/// text deltas, so a normal single-paragraph response may never contain one.
const STREAM_PARTIAL_COMMIT_AGE: Duration = Duration::from_millis(40);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProseKind {
    PrimaryMessage,
    PrimaryThought,
    SubagentMessage,
    SubagentThought,
}

#[derive(Debug, Default)]
struct StreamRevealLane {
    observed_bytes: usize,
    visible_bytes: usize,
    unterminated: String,
    unterminated_since: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct StreamCommit {
    entry_index: usize,
    bytes: usize,
    queued_at: Instant,
}

/// Keeps canonical transcript entries whole while giving their live rendering
/// Codex-style newline pacing. Incomplete source stays out of the terminal;
/// complete source lines become visible one tick at a time.
#[derive(Debug, Default)]
struct StreamRevealController {
    lanes: BTreeMap<usize, StreamRevealLane>,
    queued: VecDeque<StreamCommit>,
}

impl StreamRevealController {
    /// Reattach to a state that may have continued streaming while another UI
    /// state was visible. Existing source starts visible; only future deltas
    /// are paced.
    fn resume(state: &mut AppState) -> Self {
        state.clear_stream_visibility();
        let lanes = state
            .transcript
            .iter()
            .enumerate()
            .filter_map(|(entry_index, entry)| {
                if transcript_entry_is_stable(state, entry_index, entry) {
                    return None;
                }
                let text = entry_prose_text(entry)?;
                Some((
                    entry_index,
                    StreamRevealLane {
                        observed_bytes: text.len(),
                        visible_bytes: text.len(),
                        unterminated: String::new(),
                        unterminated_since: None,
                    },
                ))
            })
            .collect();
        Self {
            lanes,
            queued: VecDeque::new(),
        }
    }

    /// Detach without leaving renderer-only prefixes behind in `AppState`.
    fn release(&mut self, state: &mut AppState) {
        state.clear_stream_visibility();
        self.lanes.clear();
        self.queued.clear();
    }

    fn has_pending(&self) -> bool {
        !self.queued.is_empty()
            || self
                .lanes
                .values()
                .any(|lane| !lane.unterminated.is_empty())
    }

    fn flush_for_event(&mut self, state: &mut AppState, event: &UiEvent) -> bool {
        let Some(next_kind) = prose_kind_for_event(event) else {
            return self.flush_entries(state, self.lanes.keys().copied().collect());
        };
        let entries = self
            .lanes
            .iter()
            .filter_map(|(&entry_index, _)| {
                (entry_prose_kind(Some(state.transcript.get(entry_index)?)) != Some(next_kind))
                    .then_some(entry_index)
            })
            .collect();
        self.flush_entries(state, entries)
    }

    fn observe(&mut self, state: &mut AppState) -> bool {
        self.observe_at(state, Instant::now())
    }

    fn observe_at(&mut self, state: &mut AppState, now: Instant) -> bool {
        let mut active = BTreeSet::new();
        let mut visibility_updates = Vec::new();
        let mut changed = false;

        for (entry_index, entry) in state.transcript.iter().enumerate() {
            if transcript_entry_is_stable(state, entry_index, entry) {
                continue;
            }
            let Some(text) = entry_prose_text(entry) else {
                continue;
            };
            active.insert(entry_index);
            let lane = self.lanes.entry(entry_index).or_default();
            if lane.observed_bytes > text.len() || !text.is_char_boundary(lane.observed_bytes) {
                lane.observed_bytes = 0;
                lane.visible_bytes = 0;
                lane.unterminated.clear();
                lane.unterminated_since = None;
            }
            if lane.observed_bytes < text.len() {
                let was_unterminated_empty = lane.unterminated.is_empty();
                lane.unterminated.push_str(&text[lane.observed_bytes..]);
                lane.observed_bytes = text.len();
                if was_unterminated_empty && !lane.unterminated.is_empty() {
                    lane.unterminated_since = Some(now);
                }
                while let Some(newline) = lane.unterminated.find('\n') {
                    let bytes = newline + 1;
                    lane.unterminated.drain(..bytes);
                    self.queued.push_back(StreamCommit {
                        entry_index,
                        bytes,
                        queued_at: now,
                    });
                    lane.unterminated_since = (!lane.unterminated.is_empty()).then_some(now);
                }
            }
            visibility_updates.push((entry_index, lane.visible_bytes));
        }

        let closed = self
            .lanes
            .keys()
            .copied()
            .filter(|entry_index| !active.contains(entry_index))
            .collect::<Vec<_>>();
        for entry_index in closed {
            self.lanes.remove(&entry_index);
            changed |= state.clear_stream_visible_bytes(entry_index);
        }
        for (entry_index, visible_bytes) in visibility_updates {
            changed |= state.set_stream_visible_bytes(entry_index, visible_bytes);
        }
        changed
    }

    fn commit_one(&mut self, state: &mut AppState) -> bool {
        self.commit_one_at(state, Instant::now())
    }

    fn commit_one_at(&mut self, state: &mut AppState, now: Instant) -> bool {
        let Some(first) = self.queued.front() else {
            return self.commit_unterminated_at(state, now);
        };
        let catch_up = self.queued.len() >= STREAM_CATCH_UP_LINES
            || now.saturating_duration_since(first.queued_at) >= STREAM_CATCH_UP_AGE;
        let commits = if catch_up { self.queued.len() } else { 1 };
        let mut changed = false;
        for _ in 0..commits {
            let Some(commit) = self.queued.pop_front() else {
                break;
            };
            let Some(lane) = self.lanes.get_mut(&commit.entry_index) else {
                continue;
            };
            let Some(text) = state
                .transcript
                .get(commit.entry_index)
                .and_then(entry_prose_text)
            else {
                continue;
            };
            lane.visible_bytes = lane
                .visible_bytes
                .saturating_add(commit.bytes)
                .min(text.len());
            state.set_stream_visible_bytes(commit.entry_index, lane.visible_bytes);
            changed = true;
        }
        changed
    }

    fn commit_unterminated_at(&mut self, state: &mut AppState, now: Instant) -> bool {
        let Some(entry_index) = self.lanes.iter().find_map(|(&entry_index, lane)| {
            let queued_at = lane.unterminated_since?;
            (!lane.unterminated.is_empty()
                && now.saturating_duration_since(queued_at) >= STREAM_PARTIAL_COMMIT_AGE)
                .then_some(entry_index)
        }) else {
            return false;
        };
        let Some(lane) = self.lanes.get_mut(&entry_index) else {
            return false;
        };
        let bytes = lane.unterminated.len();
        lane.unterminated.clear();
        lane.unterminated_since = None;
        let Some(text) = state.transcript.get(entry_index).and_then(entry_prose_text) else {
            return false;
        };
        lane.visible_bytes = lane.visible_bytes.saturating_add(bytes).min(text.len());
        state.set_stream_visible_bytes(entry_index, lane.visible_bytes)
    }

    fn flush_entries(&mut self, state: &mut AppState, entries: Vec<usize>) -> bool {
        if entries.is_empty() {
            return false;
        }
        let entries = entries.into_iter().collect::<BTreeSet<_>>();
        self.queued
            .retain(|commit| !entries.contains(&commit.entry_index));
        let mut changed = false;
        for entry_index in entries {
            let Some(text) = state.transcript.get(entry_index).and_then(entry_prose_text) else {
                continue;
            };
            if let Some(lane) = self.lanes.get_mut(&entry_index) {
                lane.observed_bytes = text.len();
                lane.visible_bytes = text.len();
                lane.unterminated.clear();
                lane.unterminated_since = None;
            }
            changed |= state.set_stream_visible_bytes(entry_index, text.len());
        }
        changed
    }
}

fn entry_prose_text(entry: &Entry) -> Option<&str> {
    match entry {
        Entry::AgentMessage(text) | Entry::SubagentMessage(text) => Some(text),
        Entry::AgentThought(thought) | Entry::SubagentThought(thought) => Some(&thought.text),
        _ => None,
    }
}

fn entry_prose_kind(entry: Option<&Entry>) -> Option<ProseKind> {
    match entry? {
        Entry::AgentMessage(_) => Some(ProseKind::PrimaryMessage),
        Entry::AgentThought(_) => Some(ProseKind::PrimaryThought),
        Entry::SubagentMessage(_) => Some(ProseKind::SubagentMessage),
        Entry::SubagentThought(_) => Some(ProseKind::SubagentThought),
        _ => None,
    }
}

fn prose_kind_for_event(event: &UiEvent) -> Option<ProseKind> {
    let update = match event {
        UiEvent::SessionUpdate(update) => update,
        UiEvent::Subagent(SubagentEvent::SessionUpdate { update, .. }) => update,
        _ => return None,
    };
    match update {
        SessionUpdate::AgentMessageChunk(_) => match event {
            UiEvent::Subagent(_) => Some(ProseKind::SubagentMessage),
            _ => Some(ProseKind::PrimaryMessage),
        },
        SessionUpdate::AgentThoughtChunk(_) => match event {
            UiEvent::Subagent(_) => Some(ProseKind::SubagentThought),
            _ => Some(ProseKind::PrimaryThought),
        },
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    InlineChat,
    FullscreenTui,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderLabels {
    pub project: String,
    pub worktree: Option<String>,
    pub additional_roots: usize,
    pub session_title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TerminalRequest {
    None,
    ToggleTextSelectionMode,
    StartDictation,
    StopDictation,
    ForceInlineRepair,
    CopyText(String),
    Authenticate(crate::auth::AuthVendor),
}

fn terminal_request_forces_inline_repair(request: &TerminalRequest) -> bool {
    matches!(request, TerminalRequest::ForceInlineRepair)
}

fn inline_transcript_viewer_accepts_input(state: &AppState) -> bool {
    state.transcript_viewer && !state.has_pending_permission() && !state.has_pending_elicitation()
}

fn inline_reader_accepts_input(state: &AppState) -> bool {
    inline_transcript_viewer_accepts_input(state)
        || (state.nested_agent_viewer
            && !state.has_pending_permission()
            && !state.has_pending_elicitation())
        || (state.workspace_diff_viewer
            && !state.has_pending_permission()
            && !state.has_pending_elicitation())
}

fn inline_reader_is_open(state: &AppState) -> bool {
    state.transcript_viewer || state.nested_agent_viewer || state.workspace_diff_viewer
}

#[derive(Debug)]
enum DictationEvent {
    Partial(String),
    Level(f32),
    Status(String),
    Finished(std::result::Result<String, String>),
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalFeature {
    RawMode,
    AlternateScreen,
    MouseCapture,
    BracketedPaste,
}

#[cfg(test)]
fn terminal_setup_features(mode: UiMode) -> Vec<TerminalFeature> {
    let mut features = vec![TerminalFeature::RawMode];
    match mode {
        UiMode::InlineChat => {
            features.push(TerminalFeature::BracketedPaste);
        }
        UiMode::FullscreenTui => {
            features.extend([
                TerminalFeature::AlternateScreen,
                TerminalFeature::MouseCapture,
                TerminalFeature::BracketedPaste,
            ]);
        }
    }
    features
}

#[derive(Debug, Default)]
struct TranscriptScrollState {
    last_rendered_lines: Option<(usize, u16)>,
    /// Cached `Vec<Line>` + wrapped `line_count`, keyed by
    /// `(transcript_revision, width)`. Rebuilding these requires
    /// running `unicode_segmentation` / `unicode_width` over the entire
    /// transcript, which dominates UI CPU on long sessions; caching cuts
    /// it out when nothing visible changed (e.g. while the user is
    /// typing in the input box or navigating modals).
    cache: Option<TranscriptCache>,
}

#[derive(Debug)]
struct TranscriptCache {
    revision: u64,
    width: u16,
    lines: Vec<Line<'static>>,
    line_count: usize,
}

#[derive(Debug, Default)]
struct TranscriptSink {
    emitted_entries: usize,
    /// A tool call was flushed while it was the last transcript entry, so its
    /// trailing separator blank is held back until we know what follows. The
    /// tool *content* still flushes immediately (streaming promptness); only
    /// the one blank row waits, so a following tool call can abut instead of
    /// being pushed off by a separator that scrollback can never retract.
    deferred_tool_separator: bool,
}

impl TranscriptSink {
    fn pending_lines(&mut self, state: &AppState, width: u16) -> Vec<Line<'static>> {
        let mut out = Vec::new();

        // Resolve a separator held back from an earlier flush now that we can
        // see (or wait for) what follows the trailing tool call.
        if self.deferred_tool_separator {
            let successor = state.transcript.get(self.emitted_entries);
            let successor_is_tool_call = successor.is_some_and(
                |entry| matches!(entry, Entry::ToolCall(id) if state.tool_calls.contains_key(id)),
            );
            if successor_is_tool_call {
                // The next entry is a tool call: let the rails abut.
                self.deferred_tool_separator = false;
            } else if successor.is_some() || !state.is_streaming() {
                // A non-tool entry follows, or the turn ended with nothing
                // after the tool call — the separator is owed now.
                out.push(Line::from(""));
                self.deferred_tool_separator = false;
            }
            // Otherwise still streaming with nothing new yet: keep holding it.
        }
        let stable_entries = stable_transcript_entry_count(state);
        if stable_entries > self.emitted_entries {
            let mut lines = render_transcript_entry_range(
                state,
                width,
                self.emitted_entries..stable_entries,
                transcript_collapse_limit(state),
                state.theme,
                false,
            );
            // If the batch ends on a tool call that is (for now) the last
            // transcript entry, its successor is unknown, so hold its trailing
            // blank back rather than commit a separator we can't take back.
            if state.is_streaming()
                && stable_entries == state.transcript.len()
                && matches!(state.transcript.last(), Some(Entry::ToolCall(_)))
                && lines.last().is_some_and(is_blank_line)
            {
                lines.pop();
                self.deferred_tool_separator = true;
            }
            out.append(&mut lines);
            self.emitted_entries = stable_entries;
        }

        out
    }

    fn pending_lines_for_exit(&mut self, state: &AppState, width: u16) -> Vec<Line<'static>> {
        let mut out = self.pending_lines(state, width);
        // A completed trailing tool call may already have emitted all of its
        // content while holding back only its separator. There will never be a
        // successor once we exit, so commit the separator now.
        if self.deferred_tool_separator {
            out.push(Line::from(""));
            self.deferred_tool_separator = false;
        }
        if self.emitted_entries < state.transcript.len() {
            out.extend(render_transcript_entry_range(
                state,
                width,
                self.emitted_entries..state.transcript.len(),
                transcript_collapse_limit(state),
                state.theme,
                false,
            ));
            self.emitted_entries = state.transcript.len();
            self.deferred_tool_separator = false;
        }
        out
    }

    fn mark_emitted(&mut self, entries: usize) {
        self.emitted_entries = entries;
        // The resize rebuild re-renders the whole stable range in one pass,
        // trailing blank included, so nothing is owed afterward.
        self.deferred_tool_separator = false;
    }
}

fn is_blank_line(line: &Line<'static>) -> bool {
    line.spans.iter().all(|span| span.content.trim().is_empty())
}

#[derive(Debug, Default)]
struct InlineResizeReflow {
    last_observed_size: Option<Size>,
    pending_until: Option<Instant>,
}

impl InlineResizeReflow {
    fn note_resize(&mut self, size: Size, now: Instant) {
        if self.last_observed_size == Some(size) {
            return;
        }
        self.last_observed_size = Some(size);
        self.pending_until = Some(now + INLINE_RESIZE_REFLOW_DEBOUNCE);
    }

    fn is_pending(&self) -> bool {
        self.pending_until.is_some()
    }

    fn is_due(&self, now: Instant) -> bool {
        self.pending_until.is_some_and(|deadline| now >= deadline)
    }

    fn waiting(&self, now: Instant) -> bool {
        self.pending_until.is_some_and(|deadline| now < deadline)
    }

    fn clear(&mut self) {
        self.pending_until = None;
    }
}

fn stable_transcript_entry_count(state: &AppState) -> usize {
    let mut stable = 0;
    for (idx, entry) in state.transcript.iter().enumerate() {
        if transcript_entry_is_stable(state, idx, entry) {
            stable = idx + 1;
        } else {
            break;
        }
    }
    stable
}

/// A derived prompt-bounded view of the source transcript. It deliberately
/// contains indexes rather than copied entries so the full reader and export
/// can always render the original ordered activity without reconstruction.
#[derive(Debug, Clone)]
struct TranscriptTurn {
    prompt_index: usize,
    end: usize,
    is_compactable: bool,
    elapsed: Option<Duration>,
    tool_summary: Option<TurnToolSummary>,
    final_response_index: Option<usize>,
}

#[derive(Debug, Clone)]
struct TurnToolSummary {
    tools: usize,
    failures: usize,
    changed_paths: BTreeSet<String>,
}

fn transcript_turns(state: &AppState) -> Vec<TranscriptTurn> {
    let prompt_indexes = state
        .transcript
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| matches!(entry, Entry::UserPrompt(_)).then_some(index))
        .collect::<Vec<_>>();

    prompt_indexes
        .iter()
        .enumerate()
        .map(|(position, &prompt_index)| {
            let end = prompt_indexes
                .get(position + 1)
                .copied()
                .unwrap_or(state.transcript.len());
            let entries_stable =
                state.transcript[prompt_index..end]
                    .iter()
                    .enumerate()
                    .all(|(offset, entry)| {
                        transcript_entry_is_stable(state, prompt_index + offset, entry)
                    });
            let has_lifecycle = state.has_prompt_turn(prompt_index);
            let is_compactable =
                has_lifecycle && state.prompt_turn_completed(prompt_index) && entries_stable;
            let tool_summary = is_compactable
                .then(|| turn_tool_summary(state, prompt_index, end))
                .flatten();
            let final_response_index = is_compactable
                .then(|| turn_final_response_index(state, prompt_index, end))
                .flatten();
            TranscriptTurn {
                prompt_index,
                end,
                is_compactable,
                elapsed: state.prompt_turn_elapsed(prompt_index),
                tool_summary,
                final_response_index,
            }
        })
        .collect()
}

fn turn_final_response_index(state: &AppState, start: usize, end: usize) -> Option<usize> {
    // The primary agent's last response is the canonical turn conclusion. A
    // nested actor can report after it, but should not steal this marker.
    (start..end)
        .rev()
        .find(|&index| matches!(state.transcript[index], Entry::AgentMessage(_)))
        .or_else(|| {
            (start..end)
                .rev()
                .find(|&index| matches!(&state.transcript[index], Entry::SubagentMessage(_)))
        })
}

fn turn_tool_summary(state: &AppState, start: usize, end: usize) -> Option<TurnToolSummary> {
    let mut summary = TurnToolSummary {
        tools: 0,
        failures: 0,
        changed_paths: BTreeSet::new(),
    };
    for entry in &state.transcript[start..end] {
        match entry {
            Entry::ToolCall(id) | Entry::SubagentToolCall(id) => {
                // Count each source entry exactly once, even if a malformed
                // transcript no longer has its associated live view.
                summary.tools += 1;
                if let Some(view) = state.tool_calls.get(id) {
                    if view.status == ToolCallStatus::Failed {
                        summary.failures += 1;
                    }
                    // A failed call can include a diff-shaped payload, but it
                    // is not evidence that a file was successfully changed.
                    if view.status == ToolCallStatus::Completed {
                        for output in &view.body {
                            if let ToolCallOutput::Diff { path, .. } = output {
                                summary.changed_paths.insert(path.clone());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    (summary.tools > 0).then_some(summary)
}

fn transcript_entry_is_stable(state: &AppState, idx: usize, entry: &Entry) -> bool {
    match entry {
        Entry::UserPrompt(_)
        | Entry::System(_)
        | Entry::SessionBoundary(_)
        | Entry::Plan(_)
        | Entry::SubagentPlan(_)
        | Entry::InternalMessage(_) => true,
        Entry::AgentThought(thought) => thought.completed,
        Entry::SubagentThought(thought) => thought.completed,
        // An actor may append coordination activity after its active message.
        // Hold only that exact owned entry; historical messages from the same
        // actor are already immutable and must remain flushable.
        Entry::AgentMessage(_) => {
            !state.is_streaming() || state.agent_open_message_index() != Some(idx)
        }
        // Nested prose now lives in an actor-owned transcript rather than the
        // primary stream. Legacy/replayed entries that still reach the main
        // transcript are therefore immutable.
        Entry::SubagentMessage(_) => true,
        Entry::ToolCall(id) | Entry::SubagentToolCall(id) => {
            state.tool_calls.get(id).is_some_and(|view| {
                matches!(
                    view.status,
                    ToolCallStatus::Completed | ToolCallStatus::Failed
                ) && view.body.iter().all(|output| {
                    !matches!(
                        output,
                        ToolCallOutput::Terminal {
                            exit_status: None,
                            ..
                        }
                    )
                })
            })
        }
    }
}

impl TranscriptScrollState {
    /// Preserve the visible transcript when new wrapped lines arrive
    /// or the terminal is resized.
    fn reconcile(&mut self, scroll_offset: &mut usize, rendered_lines: usize, visible_rows: u16) {
        if let Some((previous_lines, previous_visible_rows)) = self.last_rendered_lines
            && *scroll_offset > 0
        {
            let previous_top = previous_lines
                .saturating_sub(previous_visible_rows as usize)
                .saturating_sub(*scroll_offset);
            let current_top = rendered_lines.saturating_sub(visible_rows as usize);
            let preserved_top = previous_top.min(current_top);
            let next_offset = current_top.saturating_sub(preserved_top);
            *scroll_offset = next_offset;
        }
        self.last_rendered_lines = Some((rendered_lines, visible_rows));
    }
}

/// Run the UI loop until the user quits or asks for a new session. The
/// caller owns the terminal lifecycle (`setup_fullscreen_terminal` or
/// `setup_inline_chat_terminal`, with the matching restore function).
/// Returns the reason the loop exited so `main` knows whether to
/// terminate or run the picker again.
///
/// Prompt history is loaded from `history_path` (if set) and persisted
/// on exit. `initial_agent_label` pre-populates the agent section of
/// the header so we show the configured agent name immediately instead
/// of waiting for the agent to report its own name during handshake.
#[derive(Clone, Copy, Default)]
pub struct UiPersistencePaths<'a> {
    pub history_path: Option<&'a Path>,
    pub transcript_export_dir: Option<&'a Path>,
    pub config_path: Option<&'a Path>,
}

#[derive(Clone)]
pub struct UiRunOptions<'a> {
    pub persistence: UiPersistencePaths<'a>,
    pub mode: UiMode,
    pub theme_kind: TerminalThemeKind,
    pub spinner_style: SpinnerStyle,
    pub active_agent_launch: Option<ragnarok::Launch>,
    pub session_boundary: Option<String>,
    /// The ACP session cwd; `/ragnarok` battles are rooted here.
    pub session_cwd: PathBuf,
    pub model_choices: Vec<crate::roster::ModelChoice>,
    pub acp_inventory: crate::roster::AcpInventory,
    pub configured_models: crate::config::ModelsConfig,
    pub active_models: crate::config::ModelsConfig,
    pub review_enabled: bool,
    pub ragnarok_models: Vec<crate::roster::ResolvedAgent>,
    pub primary_acp_name: String,
    pub termination: CancellationToken,
}

pub struct UiRunResult {
    pub reason: UiExitReason,
    pub session_id: Option<String>,
    pub session_title: Option<String>,
    pub theme_kind: TerminalThemeKind,
    pub spinner_style: SpinnerStyle,
}

struct UiInitialState {
    header_labels: HeaderLabels,
    agent_label: Option<String>,
    agent_source_id: Option<String>,
    active_agent_launch: Option<ragnarok::Launch>,
    history: Vec<String>,
    transcript_export_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    theme_kind: TerminalThemeKind,
    spinner_style: SpinnerStyle,
    session_boundary: Option<String>,
    session_cwd: PathBuf,
    model_choices: Vec<crate::roster::ModelChoice>,
    acp_inventory: crate::roster::AcpInventory,
    configured_models: crate::config::ModelsConfig,
    active_models: crate::config::ModelsConfig,
    review_enabled: bool,
    ragnarok_models: Vec<crate::roster::ResolvedAgent>,
    primary_acp_name: String,
}

/// Internal result of [`ui_loop`]. `run` unpacks it into the public
/// [`UiRunResult`] and persists `history`.
struct UiLoopOutcome {
    reason: UiExitReason,
    session_id: Option<String>,
    session_title: Option<String>,
    theme_kind: TerminalThemeKind,
    spinner_style: SpinnerStyle,
    history: Vec<String>,
}

pub async fn run(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    event_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    header_labels: HeaderLabels,
    initial_agent_label: Option<String>,
    initial_agent_source_id: Option<String>,
    options: UiRunOptions<'_>,
) -> Result<UiRunResult> {
    let initial_history = options
        .persistence
        .history_path
        .map(config::load_history)
        .unwrap_or_default();
    let UiLoopOutcome {
        reason,
        session_id,
        session_title,
        theme_kind,
        spinner_style,
        history,
    } = ui_loop(
        terminal,
        cmd_tx,
        event_rx,
        UiInitialState {
            header_labels,
            agent_label: initial_agent_label,
            agent_source_id: initial_agent_source_id,
            active_agent_launch: options.active_agent_launch.clone(),
            history: initial_history,
            transcript_export_dir: options
                .persistence
                .transcript_export_dir
                .map(Path::to_path_buf),
            config_path: options.persistence.config_path.map(Path::to_path_buf),
            theme_kind: options.theme_kind,
            spinner_style: options.spinner_style,
            session_boundary: options.session_boundary,
            session_cwd: options.session_cwd,
            model_choices: options.model_choices,
            acp_inventory: options.acp_inventory,
            configured_models: options.configured_models,
            active_models: options.active_models,
            review_enabled: options.review_enabled,
            ragnarok_models: options.ragnarok_models,
            primary_acp_name: options.primary_acp_name,
        },
        options.mode,
        options.termination,
    )
    .await?;
    if let Some(path) = options.persistence.history_path
        && let Err(e) = config::save_history(path, &history)
    {
        tracing::warn!("save_history {path:?}: {e:#}");
    }
    Ok(UiRunResult {
        reason,
        session_id,
        session_title,
        theme_kind,
        spinner_style,
    })
}

/// Maximum redraw rate for interactive local UI work such as typing,
/// overlays, and picker updates.
const FRAME_BUDGET: Duration = Duration::from_millis(33);

/// Slower redraw rate for streaming transcript updates in the fullscreen TUI.
/// User input is intentionally not throttled by this budget.
const STREAMING_FRAME_BUDGET: Duration = STREAM_COMMIT_INTERVAL;

/// Slower redraw rate for streaming transcript updates in inline chat. Spinner
/// animation has its own cadence, so queued-prompt typing can stay responsive.
const INLINE_STREAMING_FRAME_BUDGET: Duration = STREAM_COMMIT_INTERVAL;

/// Spinner-only redraw cadence. Tied to `SPINNER_FRAME_INTERVAL_MS` so the
/// wall-clock frame selection and idle animation wakeups cannot drift.
const SPINNER_FRAME_BUDGET: Duration =
    Duration::from_millis(crate::spinner::SPINNER_FRAME_INTERVAL_MS as u64);

/// Redraw cadence while the `/mjconfig` overlay is idly previewing spinners.
/// Keypresses in the menu are still rendered with the interactive budget.
#[cfg(test)]
const MJCONFIG_FRAME_BUDGET: Duration = SPINNER_FRAME_BUDGET;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedrawCause {
    /// Local user-visible edits: typing, queueing, modal navigation, status
    /// changes, or lifecycle events that should echo promptly.
    Interactive,
    /// Remote transcript/output updates that can be coalesced while streaming.
    Stream,
    /// Timer-only animation such as spinners and elapsed-time labels.
    Animation,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PendingRedraw {
    interactive: bool,
    stream: bool,
    animation: bool,
}

impl PendingRedraw {
    fn from_failed_initial_draw(rendered: bool) -> Self {
        let mut pending = Self::default();
        if !rendered {
            pending.mark(RedrawCause::Interactive);
        }
        pending
    }

    fn mark(&mut self, cause: RedrawCause) {
        match cause {
            RedrawCause::Interactive => self.interactive = true,
            RedrawCause::Stream => self.stream = true,
            RedrawCause::Animation => self.animation = true,
        }
    }

    fn mark_interactive(&mut self) {
        self.mark(RedrawCause::Interactive);
    }

    fn mark_animation(&mut self) {
        self.mark(RedrawCause::Animation);
    }

    fn any(self) -> bool {
        self.interactive || self.stream || self.animation
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    fn budget(self, mode: UiMode) -> Duration {
        if self.interactive {
            FRAME_BUDGET
        } else if self.stream {
            streaming_redraw_budget(mode)
        } else if self.animation {
            SPINNER_FRAME_BUDGET
        } else {
            FRAME_BUDGET
        }
    }
}

fn streaming_redraw_budget(mode: UiMode) -> Duration {
    match mode {
        UiMode::InlineChat => INLINE_STREAMING_FRAME_BUDGET,
        UiMode::FullscreenTui => STREAMING_FRAME_BUDGET,
    }
}

fn ui_event_redraw_cause(event: &UiEvent) -> RedrawCause {
    match event {
        UiEvent::Side(event) => ui_event_redraw_cause(event),
        UiEvent::SideStartFailed { .. } => RedrawCause::Interactive,
        UiEvent::SessionUpdate(_) | UiEvent::TerminalOutput(_) => RedrawCause::Stream,
        // Nested activity only rewrites private actor detail, so it coalesces
        // with streaming output. Lifecycle events also update transcript and
        // viewer structure, so they remain interactive below.
        UiEvent::Subagent(
            crate::event::SubagentEvent::SessionUpdate { .. }
            | crate::event::SubagentEvent::TerminalOutput { .. }
            | crate::event::SubagentEvent::Activity { .. },
        ) => RedrawCause::Stream,
        UiEvent::Connected { .. }
        | UiEvent::SessionStarted { .. }
        | UiEvent::ContextCompacted
        | UiEvent::SessionConfigOptions { .. }
        | UiEvent::WorkspaceDiff(_)
        | UiEvent::PermissionRequest(_)
        | UiEvent::ElicitationRequest(_)
        | UiEvent::CancelPendingPermissions
        | UiEvent::PromptDone { .. }
        | UiEvent::ClaudeUsage(_)
        | UiEvent::CodexUsage(_)
        | UiEvent::AgentUsage(_)
        | UiEvent::SubagentPoolModelChanged { .. }
        | UiEvent::PromptFailed { .. }
        | UiEvent::SessionForkFailed { .. }
        | UiEvent::RemotePermissionDecision { .. }
        | UiEvent::Warning(_)
        | UiEvent::Info(_)
        | UiEvent::InternalMessage(_)
        | UiEvent::Fatal(_)
        | UiEvent::RosterUpdate { .. }
        | UiEvent::Workflow(_)
        | UiEvent::Subagent(_) => RedrawCause::Interactive,
    }
}

fn side_main_notice(event: &UiEvent) -> Option<&'static str> {
    match event {
        UiEvent::PermissionRequest(_) | UiEvent::ElicitationRequest(_) => Some("Main needs input"),
        UiEvent::PromptDone { .. } => Some("Main complete"),
        UiEvent::PromptFailed { .. } | UiEvent::Fatal(_) => Some("Main failed"),
        _ => None,
    }
}

fn drain_hidden_main_prompt(state: &mut AppState, cmd_tx: &mpsc::UnboundedSender<UiCommand>) {
    let (main_tx, mut main_rx) = mpsc::unbounded_channel();
    drain_queued_prompt(state, &main_tx);
    while let Ok(command) = main_rx.try_recv() {
        let _ = cmd_tx.send(UiCommand::Main(Box::new(command)));
    }
}

/// Maximum number of lines we render from each tool-output entry when
/// transcript details are collapsed. Picked to keep the head of long
/// stdout / diff dumps visible without flushing the surrounding
/// conversation out of the viewport while a turn is streaming.
const TOOL_OUTPUT_COLLAPSED_LINES: usize = 6;
const TOOL_OUTPUT_COLLAPSED_CHARS: usize = 600;
const MESSAGE_COLLAPSED_LINES: usize = 6;
const MESSAGE_COLLAPSED_CHARS: usize = 600;

async fn ui_loop(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    event_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    initial: UiInitialState,
    mode: UiMode,
    termination: CancellationToken,
) -> Result<UiLoopOutcome> {
    let mut state = AppState::new();
    state.set_prompt_history(initial.history);
    state.project_label = initial.header_labels.project;
    state.worktree_label = initial.header_labels.worktree;
    state.additional_roots = initial.header_labels.additional_roots;
    if let Some(title) = initial.header_labels.session_title {
        state.set_session_title(&title);
    }
    if let Some(label) = initial.agent_label {
        state.agent_label = label;
    }
    if let Some(source_id) = initial.agent_source_id {
        state.agent_source_id = source_id;
    }
    state.active_agent_launch = initial.active_agent_launch;
    state.session_cwd = initial.session_cwd;
    state.model_choices = initial.model_choices;
    state.acp_inventory = initial.acp_inventory;
    state.configured_models = initial.configured_models;
    state.active_models = initial.active_models;
    state.review_enabled = initial.review_enabled;
    state.ragnarok_models = initial.ragnarok_models;
    state.set_primary_acp_name(initial.primary_acp_name);
    state.transcript_export_dir = initial.transcript_export_dir;
    state.set_theme(initial.theme_kind);
    state.set_spinner_style(initial.spinner_style);
    state.config_path = initial.config_path;
    if let Some(boundary) = initial.session_boundary {
        state.push_session_boundary(boundary);
    }
    let mut main_state: Option<AppState> = None;
    let mut transcript_scroll = TranscriptScrollState::default();
    let mut transcript_sink = TranscriptSink::default();
    let mut main_transcript_scroll: Option<TranscriptScrollState> = None;
    let mut main_transcript_sink: Option<TranscriptSink> = None;
    let mut inline_resize_reflow = InlineResizeReflow::default();
    let mut stream_reveal = StreamRevealController::default();
    let mut notification_backend = TerminalNotificationBackend::detect();
    let mut crossterm_events = EventStream::new();
    let (dictation_tx, mut dictation_rx) = mpsc::unbounded_channel::<DictationEvent>();
    let mut dictation_cancel_tx: Option<std_mpsc::Sender<()>> = None;
    let (current_pr_tx, mut current_pr_rx) = mpsc::unbounded_channel::<CurrentBranchPrProbe>();
    let mut current_pr_probe_in_flight = false;
    // Ragnarok battles report through their own channel (the sender stays
    // alive here for the whole loop, so `recv` pends rather than closing).
    let (ragnarok_tx, mut ragnarok_rx) = mpsc::unbounded_channel::<ragnarok::RagnarokEvent>();
    let mut inline_height = INLINE_CHAT_HEIGHT;
    // Wake-up timers so queued input can render at the interactive cadence
    // while spinner-only animation advances at a calmer progress cadence.
    // `Delay` keeps either timer from burst-firing after a long busy period.
    let mut redraw_tick = tokio::time::interval(FRAME_BUDGET);
    redraw_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut animation_tick = tokio::time::interval(SPINNER_FRAME_BUDGET);
    animation_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut stream_commit_tick = tokio::time::interval(STREAM_COMMIT_INTERVAL);
    stream_commit_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut current_pr_tick = tokio::time::interval(CURRENT_BRANCH_PR_POLL_INTERVAL);
    current_pr_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    if mode == UiMode::InlineChat {
        sync_inline_terminal_height(terminal, &state, &mut inline_height)?;
    }
    let initial_rendered = draw_terminal_frame(terminal, &mut state, &mut transcript_scroll, mode)?;
    let mut pending_redraw = PendingRedraw::from_failed_initial_draw(initial_rendered);
    let mut last_draw = Instant::now();
    let mut force_inline_repair = false;
    let mut force_soft_inline_repair = false;
    let mut restore_inline_after_reader = false;

    loop {
        tokio::select! {
            biased;
            _ = termination.cancelled() => {
                state.exit_reason = Some(UiExitReason::Quit);
            }
            _ = stream_commit_tick.tick(), if stream_reveal.has_pending() => {
                if stream_reveal.commit_one(&mut state) {
                    pending_redraw.mark(RedrawCause::Stream);
                }
            }
            maybe_ct = crossterm_events.next() => {
                match maybe_ct {
                    Some(Ok(ev)) => {
                        if mode == UiMode::InlineChat
                            && let CtEvent::Resize(width, height) = &ev
                        {
                            inline_resize_reflow.note_resize(
                                Size {
                                    width: *width,
                                    height: *height,
                                },
                                Instant::now(),
                            );
                        }
                        if should_force_inline_repair_for_event(mode, &state, &ev) {
                            force_inline_repair = true;
                        }
                        let inline_reader_was_active =
                            mode == UiMode::InlineChat && inline_reader_accepts_input(&state);
                        let inline_reader_was_open =
                            mode == UiMode::InlineChat && inline_reader_is_open(&state);
                        let request = handle_crossterm(&mut state, cmd_tx, ev, mode);
                        if mode == UiMode::InlineChat
                            && inline_reader_was_active != inline_reader_accepts_input(&state)
                        {
                            set_mouse_capture(
                                terminal,
                                inline_reader_accepts_input(&state),
                            )?;
                        }
                        if mode == UiMode::InlineChat
                            && terminal_request_forces_inline_repair(&request)
                        {
                            force_soft_inline_repair = true;
                        }
                        if inline_reader_was_open && !inline_reader_is_open(&state) {
                            restore_inline_after_reader = true;
                            force_soft_inline_repair = false;
                        }
                        apply_terminal_request(
                            terminal,
                            &mut state,
                            request,
                            &dictation_tx,
                            &mut dictation_cancel_tx,
                            mode,
                        )
                        .await?;
                        if let Some(task) = state.take_ragnarok_launch() {
                            start_ragnarok(&mut state, task, ragnarok_tx.clone());
                            force_soft_inline_repair = mode == UiMode::InlineChat;
                        }
                        drain_ragnarok_draft_pr_publish(&mut state, &ragnarok_tx);
                    }
                    Some(Err(e)) => {
                        state.record_status_message(
                            StatusKind::Warning,
                            format!("input error: {e}"),
                        );
                    }
                    None => break,
                }
                pending_redraw.mark_interactive();
            }
            maybe_dictation = dictation_rx.recv() => {
                match maybe_dictation {
                    Some(DictationEvent::Partial(text)) => {
                        update_dictation_partial(&mut state, &text);
                        pending_redraw.mark_interactive();
                    }
                    Some(DictationEvent::Level(level)) => {
                        update_dictation_level(&mut state, level);
                        pending_redraw.mark_interactive();
                    }
                    Some(DictationEvent::Status(message)) => {
                        update_dictation_status(&mut state, message);
                        pending_redraw.mark_interactive();
                    }
                    Some(DictationEvent::Finished(result)) => {
                        dictation_cancel_tx = None;
                        finish_dictation(&mut state, result);
                        pending_redraw.mark_interactive();
                    }
                    None => {}
                }
            }
            maybe_rag = ragnarok_rx.recv() => {
                if let Some(ev) = maybe_rag {
                    let cause = match &ev {
                        ragnarok::RagnarokEvent::FighterText { .. }
                        | ragnarok::RagnarokEvent::FighterAction { .. }
                        | ragnarok::RagnarokEvent::ThorAction(_)
                        | ragnarok::RagnarokEvent::ThorSpeaks(_)
                        | ragnarok::RagnarokEvent::Log { .. } => RedrawCause::Stream,
                        _ => RedrawCause::Interactive,
                    };
                    state.apply_ragnarok_event(ev);
                    drain_ragnarok_draft_pr_publish(&mut state, &ragnarok_tx);
                    pending_redraw.mark(cause);
                }
            }
            maybe_probe = current_pr_rx.recv() => {
                current_pr_probe_in_flight = false;
                if let Some(probe) = maybe_probe
                    && apply_current_branch_pr_probe(&mut state, probe)
                {
                    pending_redraw.mark_interactive();
                }
            }
            _ = current_pr_tick.tick(), if !current_pr_probe_in_flight => {
                current_pr_probe_in_flight = true;
                let cwd = state.session_cwd.clone();
                let tx = current_pr_tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(probe_current_branch_pull_request(cwd).await);
                });
            }
            // Use the unconditional form (no `Some(ev) = ...`) so the
            // None case (runtime dropped the sender) reaches the match
            // arm and exits the loop. The conditional pattern disables
            // the branch when the channel closes, which would leave the
            // TUI spinning on tick + crossterm forever.
            maybe_ev = event_rx.recv(), if !state.runtime_closed || main_state.is_some() => {
                match maybe_ev {
                    Some(ev) => {
                        let ev = match ev {
                            UiEvent::Side(event) if main_state.is_some() => *event,
                            UiEvent::Side(_) => continue,
                            UiEvent::SideStartFailed { message } => {
                                stream_reveal.release(&mut state);
                                if let Some(mut main) = main_state.take() {
                                    main.record_status_message(StatusKind::Warning, message);
                                    state = main;
                                    stream_reveal = StreamRevealController::resume(&mut state);
                                    transcript_scroll = main_transcript_scroll
                                        .take()
                                        .unwrap_or_default();
                                    transcript_sink =
                                        main_transcript_sink.take().unwrap_or_default();
                                } else {
                                    state.record_status_message(StatusKind::Warning, message);
                                }
                                pending_redraw.mark_interactive();
                                continue;
                            }
                            main_event if main_state.is_some() => {
                                if let Some(notice) = side_main_notice(&main_event) {
                                    state.side_main_notice = Some(notice.to_string());
                                }
                                let main = main_state.as_mut().expect("checked main state");
                                main.apply_event(main_event);
                                finalize_startup_prompt(main);
                                drain_hidden_main_prompt(main, cmd_tx);
                                pending_redraw.mark_interactive();
                                continue;
                            }
                            event => event,
                        };
                        let inline_reader_was_active =
                            mode == UiMode::InlineChat && inline_reader_accepts_input(&state);
                        let inline_reader_was_open =
                            mode == UiMode::InlineChat && inline_reader_is_open(&state);
                        let redraw_cause = ui_event_redraw_cause(&ev);
                        let force_repair_for_event =
                            should_force_inline_repair_for_ui_event(mode, &ev);
                        let notification = notification_message_for_event(mode, &state, &ev);
                        let prose_event = prose_kind_for_event(&ev).is_some();
                        let failed_side_start = state.is_side
                            && state.session_id.is_none()
                            && matches!(&ev, UiEvent::Fatal(_));
                        let flushed_prose = stream_reveal.flush_for_event(&mut state, &ev);
                        state.apply_event(ev);
                        let visibility_changed = stream_reveal.observe(&mut state);
                        finalize_startup_prompt(&mut state);
                        if failed_side_start {
                            state.side_exit_requested = true;
                        }
                        if state.is_side
                            && state.session_id.is_some()
                            && let Some(question) = state.side_initial_question.take()
                        {
                            state.record_user_prompt(question.clone());
                            let _ = cmd_tx.send(UiCommand::SendPrompt {
                                text: question,
                                images: Vec::new(),
                            });
                        }
                        if state.runtime_closed
                            && std::env::var_os("MJ_E2E_EXIT_ON_RUNTIME_CLOSE").is_some()
                        {
                            state.exit_reason = Some(UiExitReason::Quit);
                        }
                        drain_queued_prompt(&mut state, cmd_tx);
                        if mode == UiMode::InlineChat
                            && inline_reader_was_active != inline_reader_accepts_input(&state)
                        {
                            set_mouse_capture(
                                terminal,
                                inline_reader_accepts_input(&state),
                            )?;
                        }
                        if inline_reader_was_open && !inline_reader_is_open(&state) {
                            restore_inline_after_reader = true;
                        }
                        if force_repair_for_event {
                            force_inline_repair = true;
                            // Defer the repair while a resize reflow is pending:
                            // the reflow rebuilds the viewport from transcript
                            // state, and the deferred repair path picks up
                            // `force_inline_repair` afterward. Repairing now
                            // would paint at mid-resize geometry that the reflow
                            // immediately discards.
                            if !inline_resize_reflow.is_pending() {
                                sync_inline_terminal_height(terminal, &state, &mut inline_height)?;
                                let repaired = repair_inline_viewport(
                                    terminal,
                                    &mut state,
                                    &mut transcript_scroll,
                                    InlineRepairMode::Hard,
                                );
                                if repaired {
                                    last_draw = Instant::now();
                                    force_inline_repair = false;
                                }
                            }
                        }
                        post_terminal_notification(
                            terminal,
                            &mut notification_backend,
                            notification.as_deref(),
                        );
                        if !prose_event || flushed_prose || visibility_changed {
                            pending_redraw.mark(redraw_cause);
                        }
                    }
                    None => {
                        state.mark_runtime_closed();
                        // Process-level PTY tests use a scripted agent that exits
                        // after its assertions. Let the normal UI shutdown path
                        // run without requiring a second synthetic keystroke.
                        if std::env::var_os("MJ_E2E_EXIT_ON_RUNTIME_CLOSE").is_some() {
                            state.exit_reason = Some(UiExitReason::Quit);
                        }
                        pending_redraw.mark_interactive();
                    }
                }
            }
            _ = redraw_tick.tick() => {
                state.poll_mjconfig_background();
                if flush_input_paste_burst_if_due(&mut state, Instant::now(), false) {
                    pending_redraw.mark_interactive();
                }
                if timer_driven_live_redraw(mode, &state) {
                    pending_redraw.mark_animation();
                }
            }
            _ = animation_tick.tick() => {
                state.poll_mjconfig_background();
                if timer_driven_live_redraw(mode, &state) {
                    pending_redraw.mark_animation();
                }
            }
        }

        if state.side_start_requested && main_state.is_none() {
            state.side_start_requested = false;
            let question = state.side_initial_question.take();
            stream_reveal.release(&mut state);
            let side_state = state.side_conversation(question.clone());
            let main = std::mem::replace(&mut state, side_state);
            main_state = Some(main);
            let _ = question;
            let _ = cmd_tx.send(UiCommand::StartSide);
            main_transcript_scroll = Some(std::mem::take(&mut transcript_scroll));
            main_transcript_sink = Some(std::mem::take(&mut transcript_sink));
            stream_reveal = StreamRevealController::resume(&mut state);
            pending_redraw.mark_interactive();
        }

        if state.side_exit_requested {
            state.side_exit_requested = false;
            let side_failure = (state.is_side && state.runtime_closed)
                .then(|| state.status_line.as_ref().map(|status| status.text.clone()))
                .flatten();
            let _ = cmd_tx.send(UiCommand::ExitSide);
            if let Some(mut main) = main_state.take() {
                main.side_main_notice = None;
                if let Some(message) = side_failure {
                    main.record_status_message(
                        StatusKind::Warning,
                        format!("side conversation failed: {message}"),
                    );
                }
                state = main;
                transcript_scroll = main_transcript_scroll.take().unwrap_or_default();
                transcript_sink = main_transcript_sink.take().unwrap_or_default();
                stream_reveal = StreamRevealController::resume(&mut state);
                pending_redraw.mark_interactive();
            }
        }

        if !inline_resize_reflow.is_pending()
            && should_attempt_inline_repair_before_flush(force_inline_repair, mode, &state)
        {
            force_inline_repair = false;
            sync_inline_terminal_height(terminal, &state, &mut inline_height)?;
            let repaired = repair_inline_viewport(
                terminal,
                &mut state,
                &mut transcript_scroll,
                InlineRepairMode::Hard,
            );
            if repaired {
                last_draw = Instant::now();
                pending_redraw.clear();
            } else {
                pending_redraw.mark_interactive();
            }
        }

        if maybe_run_inline_resize_reflow(
            terminal,
            &mut inline_resize_reflow,
            &mut transcript_sink,
            &state,
            &mut inline_height,
        )? {
            restore_inline_after_reader = false;
            pending_redraw.mark_interactive();
        }

        if mode == UiMode::InlineChat
            && restore_inline_after_reader
            && !inline_resize_reflow.is_pending()
        {
            force_soft_inline_repair = false;
            restore_inline_after_reader = !restore_inline_viewport_after_reader(
                terminal,
                &mut transcript_sink,
                &state,
                &mut inline_height,
            )?;
            pending_redraw.mark_interactive();
        }

        // Pause scrollback flushing while the full-transcript reader owns the
        // viewport: `insert_before` would scroll the terminal under the user
        // mid-read. Entries that go stable meanwhile are flushed on close.
        if mode == UiMode::InlineChat
            && !state.transcript_viewer
            && !state.nested_agent_viewer
            && !state.workspace_diff_viewer
            && state.ragnarok.is_none()
            && !inline_resize_reflow.is_pending()
        {
            flush_transcript_to_scrollback(terminal, &mut transcript_sink, &state)?;
        }

        if let Some(reason) = state.exit_reason {
            if reason != UiExitReason::LoadSession {
                let _ = cmd_tx.send(UiCommand::Shutdown);
            }
            cancel_dictation_for_exit(&mut state, &mut dictation_cancel_tx);
            if mode == UiMode::InlineChat {
                flush_transcript_to_scrollback_for_exit(terminal, &mut transcript_sink, &state)?;
                sync_inline_terminal_height(terminal, &state, &mut inline_height)?;
            }
            let _ = draw_terminal_frame(terminal, &mut state, &mut transcript_scroll, mode)?;
            if mode == UiMode::FullscreenTui {
                reset_text_selection_mode_for_exit(&mut state, |enabled| {
                    set_mouse_capture(terminal, enabled)
                })?;
            }
            let outcome_state = main_state.as_ref().unwrap_or(&state);
            return Ok(UiLoopOutcome {
                reason,
                session_id: outcome_state.session_id.clone(),
                session_title: outcome_state.session_title.clone(),
                theme_kind: state.theme_kind,
                spinner_style: state.spinner_style,
                history: outcome_state.prompt_history(),
            });
        }

        if force_soft_inline_repair && !inline_resize_reflow.is_pending() {
            force_soft_inline_repair = false;
            sync_inline_terminal_height(terminal, &state, &mut inline_height)?;
            let repaired = repair_inline_viewport(
                terminal,
                &mut state,
                &mut transcript_scroll,
                InlineRepairMode::Soft,
            );
            if repaired {
                last_draw = Instant::now();
                pending_redraw.clear();
            } else {
                pending_redraw.mark_interactive();
            }
        }

        if !inline_resize_reflow.is_pending()
            && should_attempt_inline_repair(force_inline_repair, mode)
        {
            force_inline_repair = false;
            sync_inline_terminal_height(terminal, &state, &mut inline_height)?;
            let repaired = repair_inline_viewport(
                terminal,
                &mut state,
                &mut transcript_scroll,
                InlineRepairMode::Hard,
            );
            if repaired {
                last_draw = Instant::now();
                pending_redraw.clear();
            } else {
                pending_redraw.mark_interactive();
            }
        }

        // Throttle by redraw cause. Under a flood of runtime events (`biased`
        // select keeps picking event arms before timers), this elapsed-time
        // check coalesces stream chunks. Interactive input remains on the fast
        // budget even while a spinner is active.
        if pending_redraw.any() && last_draw.elapsed() >= pending_redraw.budget(mode) {
            if mode == UiMode::InlineChat && inline_resize_reflow.waiting(Instant::now()) {
                continue;
            }
            if mode == UiMode::InlineChat {
                sync_inline_terminal_height(terminal, &state, &mut inline_height)?;
            }
            let rendered = draw_terminal_frame(terminal, &mut state, &mut transcript_scroll, mode)?;
            if rendered {
                pending_redraw.clear();
            } else {
                pending_redraw.mark_interactive();
            }
            last_draw = Instant::now();
        }
    }
    if mode == UiMode::InlineChat {
        flush_transcript_to_scrollback_for_exit(terminal, &mut transcript_sink, &state)?;
    }
    cancel_dictation_for_exit(&mut state, &mut dictation_cancel_tx);
    if mode == UiMode::FullscreenTui {
        reset_text_selection_mode_for_exit(&mut state, |enabled| {
            set_mouse_capture(terminal, enabled)
        })?;
    }
    Ok(UiLoopOutcome {
        reason: UiExitReason::Quit,
        session_id: None,
        session_title: None,
        theme_kind: state.theme_kind,
        spinner_style: state.spinner_style,
        history: state.prompt_history(),
    })
}

fn notification_message_for_event(
    mode: UiMode,
    state: &AppState,
    event: &UiEvent,
) -> Option<String> {
    if mode == UiMode::InlineChat && matches!(event, UiEvent::PermissionRequest(_)) {
        return None;
    }

    match event {
        UiEvent::PromptDone { stop_reason, .. } => {
            if *stop_reason == StopReason::Cancelled {
                return None;
            }
            Some(
                preview_notification_text(
                    &state
                        .last_agent_message()
                        .unwrap_or_else(|| "Agent turn complete".to_string()),
                )
                .unwrap_or_else(|| "Agent turn complete".to_string()),
            )
        }
        UiEvent::PromptFailed { message } => Some(format!(
            "Prompt failed: {}",
            preview_notification_text(message).unwrap_or_else(|| "agent error".to_string())
        )),
        UiEvent::PermissionRequest(prompt) => Some(permission_request_notification(prompt)),
        UiEvent::Subagent(crate::event::SubagentEvent::PermissionRequest {
            subagent_id,
            prompt,
        }) => Some(format!(
            "subagent #{subagent_id} · {}",
            permission_request_notification(prompt)
        )),
        _ => None,
    }
}

fn permission_request_notification(prompt: &PermissionPrompt) -> String {
    match prompt
        .tool_call
        .fields
        .title
        .as_deref()
        .and_then(preview_notification_text)
    {
        Some(title) => format!("Permission requested: {title}"),
        None => "Permission requested".to_string(),
    }
}

fn preview_notification_text(text: &str) -> Option<String> {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }

    let char_count = normalized.chars().count();
    if char_count <= NOTIFICATION_PREVIEW_CHARS {
        return Some(normalized);
    }

    let truncated = normalized
        .chars()
        .take(NOTIFICATION_PREVIEW_CHARS.saturating_sub(3))
        .collect::<String>();
    Some(format!("{truncated}..."))
}

fn post_terminal_notification(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    backend: &mut Option<TerminalNotificationBackend>,
    message: Option<&str>,
) {
    let Some(message) = message else {
        return;
    };
    let Some(active_backend) = backend.as_mut() else {
        return;
    };

    if let Err(e) = active_backend.notify(terminal.backend_mut(), message) {
        tracing::warn!("terminal notification failed; disabling notifications: {e}");
        *backend = None;
    }
}

fn draw_terminal_frame(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    state: &mut AppState,
    transcript_scroll: &mut TranscriptScrollState,
    mode: UiMode,
) -> Result<bool> {
    match terminal.draw(|f| draw(f, state, transcript_scroll, mode)) {
        Ok(_) => Ok(true),
        Err(e) if mode == UiMode::InlineChat && is_cursor_position_timeout_io(&e) => {
            trace_inline_cursor_position_timeout("redraw", &e);
            Ok(false)
        }
        Err(e) => Err(e).context("draw terminal"),
    }
}

fn should_force_inline_repair_for_event(mode: UiMode, state: &AppState, ev: &CtEvent) -> bool {
    if mode != UiMode::InlineChat {
        return false;
    }

    if matches!(ev, CtEvent::FocusGained) {
        return true;
    }

    if matches!(ev, CtEvent::Paste(_))
        && !state.help_overlay
        && !state.has_pending_permission()
        && !state.has_pending_elicitation()
        && state.config_picker.is_none()
        && !state.transcript_viewer
        && !state.nested_agent_viewer
        && !state.workspace_diff_viewer
    {
        return true;
    }

    // Permission and elicitation prompts get a few early repair attempts
    // right after opening, and a hard repair when the inline viewport is
    // resized while the modal is open.
    (state.has_pending_permission() || state.has_pending_elicitation())
        && matches!(ev, CtEvent::Resize(_, _))
}

fn should_force_inline_repair_for_ui_event(mode: UiMode, ev: &UiEvent) -> bool {
    // A remote decision can dismiss the inline permission view, which
    // needs the same viewport repair as the view appearing.
    mode == UiMode::InlineChat
        && matches!(
            ev,
            UiEvent::PermissionRequest(_)
                | UiEvent::CancelPendingPermissions
                | UiEvent::RemotePermissionDecision { .. }
                | UiEvent::ElicitationRequest(_)
                | UiEvent::Subagent(
                    crate::event::SubagentEvent::PermissionRequest { .. }
                        | crate::event::SubagentEvent::ElicitationRequest { .. }
                        | crate::event::SubagentEvent::CancelPendingPermissions { .. }
                )
        )
}

fn should_attempt_inline_repair_before_flush(
    force_inline_repair: bool,
    mode: UiMode,
    state: &AppState,
) -> bool {
    mode == UiMode::InlineChat
        && force_inline_repair
        && (state.has_pending_permission() || state.has_pending_elicitation())
}

fn should_attempt_inline_repair(force_inline_repair: bool, mode: UiMode) -> bool {
    mode == UiMode::InlineChat && force_inline_repair
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineRepairMode {
    Soft,
    Hard,
}

fn repair_inline_viewport(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    state: &mut AppState,
    transcript_scroll: &mut TranscriptScrollState,
    mode: InlineRepairMode,
) -> bool {
    match terminal.autoresize() {
        Ok(()) => {}
        Err(e) if is_cursor_position_timeout_io(&e) => {
            trace_inline_cursor_position_timeout("repair autoresize", &e);
            return false;
        }
        Err(e) => {
            tracing::warn!("skip inline repair autoresize: {e}");
            return false;
        }
    }

    if matches!(mode, InlineRepairMode::Hard) {
        match terminal.clear() {
            Ok(()) => {}
            Err(e) if is_cursor_position_timeout_io(&e) => {
                trace_inline_cursor_position_timeout("repair clear", &e);
                return false;
            }
            Err(e) => {
                tracing::warn!("skip inline repair: {e}");
                return false;
            }
        }
    }

    match draw_terminal_frame(terminal, state, transcript_scroll, UiMode::InlineChat) {
        Ok(rendered) => rendered,
        Err(e) => {
            tracing::warn!("skip inline repair redraw: {e:#}");
            false
        }
    }
}

fn timer_driven_live_redraw(mode: UiMode, state: &AppState) -> bool {
    // The arena animates (poses, banners, elapsed time) as long as it is on
    // screen, in both UI modes.
    if state.ragnarok.is_some() {
        return true;
    }
    if mode == UiMode::InlineChat && state.is_busy() {
        // Workflow progress animates on wall-clock time, so it needs the timer
        // even in the inline mode that otherwise suppresses it.
        return should_show_spinner(state) || state.has_active_workflows();
    }

    needs_live_redraw(state)
}

fn should_show_spinner(state: &AppState) -> bool {
    matches!(
        state.connection_state(),
        ConnectionState::Launching
            | ConnectionState::Initializing
            | ConnectionState::Streaming
            | ConnectionState::Cancelling
            | ConnectionState::Forking
    )
}

fn needs_live_redraw(state: &AppState) -> bool {
    state.voice_input_active
        || state.help_overlay
        || state.has_pending_permission()
        || state.has_pending_elicitation()
        || state.config_picker.is_some()
        // Keep redrawing so the menu's live spinner previews keep animating.
        || state.mjconfig_menu.is_some()
        // Background workflows can outlive the primary's turn.
        || state.has_active_workflows()
        || should_show_spinner(state)
}

fn flush_transcript_to_scrollback(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    sink: &mut TranscriptSink,
    state: &AppState,
) -> Result<()> {
    flush_transcript_lines_to_scrollback(terminal, sink, state, false)
}

fn flush_transcript_to_scrollback_for_exit(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    sink: &mut TranscriptSink,
    state: &AppState,
) -> Result<()> {
    flush_transcript_lines_to_scrollback(terminal, sink, state, true)
}

fn flush_transcript_lines_to_scrollback(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    sink: &mut TranscriptSink,
    state: &AppState,
    exiting: bool,
) -> Result<()> {
    let width = match terminal.size() {
        Ok(size) => size.width,
        Err(e) if is_cursor_position_timeout_io(&e) => {
            trace_inline_cursor_position_timeout("transcript flush size query", &e);
            return Ok(());
        }
        Err(e) => return Err(e).context("query terminal size for transcript flush"),
    };
    if width == 0 {
        return Ok(());
    }
    let lines = if exiting {
        sink.pending_lines_for_exit(state, width)
    } else {
        sink.pending_lines(state, width)
    };
    if lines.is_empty() {
        return Ok(());
    }
    insert_lines_before_inline_viewport(terminal, lines, width)
}

fn maybe_run_inline_resize_reflow(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    reflow: &mut InlineResizeReflow,
    sink: &mut TranscriptSink,
    state: &AppState,
    inline_height: &mut u16,
) -> Result<bool> {
    if !should_run_inline_resize_reflow(reflow, state, Instant::now()) {
        return Ok(false);
    }
    reflow.clear();
    rebuild_inline_scrollback(terminal, sink, state, inline_height)?;
    Ok(true)
}

fn should_run_inline_resize_reflow(
    reflow: &InlineResizeReflow,
    state: &AppState,
    now: Instant,
) -> bool {
    reflow.is_due(now)
        && !state.transcript_viewer
        && !state.nested_agent_viewer
        && !state.workspace_diff_viewer
}

struct InlineResizeReflowSnapshot {
    width: u16,
    desired_height: u16,
    actual_height: u16,
    stable_entries: usize,
    lines: Vec<Line<'static>>,
}

fn inline_resize_reflow_snapshot(
    state: &AppState,
    size: Size,
) -> Option<InlineResizeReflowSnapshot> {
    if size.width == 0 || size.height == 0 {
        return None;
    }

    let desired_height = desired_inline_height(state, size);
    let actual_height = clamped_inline_height(desired_height, size);
    let stable_entries = stable_transcript_entry_count(state);
    let lines = render_transcript_entry_range(
        state,
        size.width,
        0..stable_entries,
        transcript_collapse_limit(state),
        state.theme,
        false,
    );

    Some(InlineResizeReflowSnapshot {
        width: size.width,
        desired_height,
        actual_height,
        stable_entries,
        lines,
    })
}

fn rebuild_inline_scrollback(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    sink: &mut TranscriptSink,
    state: &AppState,
    inline_height: &mut u16,
) -> Result<()> {
    let size = match terminal.size() {
        Ok(size) => size,
        Err(e) if is_cursor_position_timeout_io(&e) => {
            trace_inline_cursor_position_timeout("resize reflow size query", &e);
            return Ok(());
        }
        Err(e) => return Err(e).context("query terminal size for resize reflow"),
    };
    let Some(snapshot) = inline_resize_reflow_snapshot(state, size) else {
        return Ok(());
    };

    reset_inline_terminal_for_reflow(terminal, snapshot.desired_height, size)?;
    *inline_height = snapshot.actual_height;
    sink.mark_emitted(snapshot.stable_entries);
    insert_lines_before_inline_viewport(terminal, snapshot.lines, snapshot.width)
}

fn reset_inline_terminal_for_reflow(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    desired_height: u16,
    size: Size,
) -> Result<u16> {
    let height = clamped_inline_height(desired_height, size);
    let origin = Position::new(0, size.height.saturating_sub(height));

    terminal
        .backend_mut()
        .write_all(b"\x1b[r\x1b[0m")
        .context("reset terminal state for resize reflow")?;
    // Purge (`\x1b[3J`) drops the scrollback, not just the visible screen.
    // This deliberately discards the inline transcript rows we previously
    // pushed up with `insert_before`: the terminal hard-wrapped them at the
    // old width and would otherwise reflow them into garbled rows. We rebuild
    // them from transcript state at the new width below. Tradeoff: any
    // pre-existing terminal content above the inline viewport (shell history,
    // earlier command output) is purged too and is not restored — only
    // Mjolnir's transcript is replayed. We keep this deliberate tradeoff
    // because there is no portable terminal primitive for deleting only the
    // stale Mjolnir-owned scrollback rows while preserving foreign scrollback.
    execute!(
        terminal.backend_mut(),
        CrosstermClear(CrosstermClearType::All),
        CrosstermClear(CrosstermClearType::Purge)
    )
    .context("clear terminal for resize reflow")?;
    terminal
        .backend_mut()
        .set_cursor_position(origin)
        .context("position inline viewport for resize reflow")?;
    Write::flush(terminal.backend_mut()).context("flush resize reflow clear")?;

    let backend = TrackedBackend::with_cursor_position(io::stdout(), origin);
    let next = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
    .context("recreate inline terminal for resize reflow")?;
    *terminal = next;
    Ok(height)
}

fn clamped_inline_height(desired_height: u16, size: Size) -> u16 {
    desired_height.min(size.height).max(1)
}

fn insert_lines_before_inline_viewport(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    lines: Vec<Line<'static>>,
    width: u16,
) -> Result<()> {
    if lines.is_empty() || width == 0 {
        return Ok(());
    }
    let height = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(width)
        .min(u16::MAX as usize) as u16;
    if height == 0 {
        return Ok(());
    }
    terminal
        .insert_before(height, |buf| {
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .render(buf.area, buf);
        })
        .or_else(|e| {
            if is_cursor_position_timeout_io(&e) {
                trace_inline_cursor_position_timeout("transcript flush", &e);
                Ok(())
            } else {
                Err(e).context("insert transcript lines into scrollback")
            }
        })?;
    Ok(())
}

fn insert_stable_transcript_tail_before_inline_viewport<B>(
    terminal: &mut Terminal<B>,
    state: &AppState,
    width: u16,
    max_rows: u16,
) -> Result<bool>
where
    B: Backend,
    B::Error: Error + Send + Sync + 'static,
{
    if width == 0 || max_rows == 0 {
        return Ok(false);
    }
    let stable_entries = stable_transcript_entry_count(state);
    let lines = render_transcript_entry_range(
        state,
        width,
        0..stable_entries,
        transcript_collapse_limit(state),
        state.theme,
        false,
    );
    if lines.is_empty() {
        return Ok(true);
    }
    let total = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(width);
    let height = total.min(usize::from(max_rows)).min(usize::from(u16::MAX)) as u16;
    if height == 0 {
        return Ok(true);
    }
    let top = total
        .saturating_sub(usize::from(height))
        .min(usize::from(u16::MAX)) as u16;
    match terminal.insert_before(height, |buf| {
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((top, 0))
            .render(buf.area, buf);
    }) {
        Ok(()) => Ok(true),
        Err(e) if is_cursor_position_timeout_error(&e) => {
            trace_inline_cursor_position_timeout("reader transcript restore", &e);
            Ok(false)
        }
        Err(e) => Err(e).context("restore transcript after inline reader"),
    }
}

/// A full-height inline reader temporarily paints over transcript rows on the
/// visible screen. Restore the stable transcript tail into those rows before
/// returning to the compact viewport; older terminal scrollback is left
/// untouched.
fn restore_inline_viewport_after_reader(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    sink: &mut TranscriptSink,
    state: &AppState,
    current_height: &mut u16,
) -> Result<bool> {
    let size = match terminal.size() {
        Ok(size) => size,
        Err(e) if is_cursor_position_timeout_io(&e) => {
            trace_inline_cursor_position_timeout("reader restore size query", &e);
            return Ok(false);
        }
        Err(e) => {
            tracing::warn!("skip inline reader restore: size query failed: {e}");
            return Ok(false);
        }
    };
    let height = clamped_inline_height(desired_inline_height(state, size), size);
    let origin = Position::new(0, size.height.saturating_sub(height));

    if let Err(e) = terminal.backend_mut().write_all(b"\x1b[r\x1b[0m") {
        tracing::warn!("skip inline reader restore: reset failed: {e}");
        return Ok(false);
    }
    if let Err(e) = terminal.backend_mut().clear_region(ClearType::All) {
        tracing::warn!("skip inline reader restore: clear failed: {e}");
        return Ok(false);
    }
    if let Err(e) = terminal.backend_mut().set_cursor_position(origin) {
        if is_cursor_position_timeout_io(&e) {
            trace_inline_cursor_position_timeout("reader restore cursor move", &e);
        } else {
            tracing::warn!("skip inline reader restore: cursor move failed: {e}");
        }
        return Ok(false);
    }
    if let Err(e) = Write::flush(terminal.backend_mut()) {
        tracing::warn!("skip inline reader restore: flush failed: {e}");
        return Ok(false);
    }

    let backend = TrackedBackend::with_cursor_position(io::stdout(), origin);
    let next = match Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    ) {
        Ok(next) => next,
        Err(e) => {
            tracing::warn!("skip inline reader restore: recreate failed: {e:#}");
            return Ok(false);
        }
    };
    *terminal = next;
    *current_height = height;
    let restored = insert_stable_transcript_tail_before_inline_viewport(
        terminal,
        state,
        size.width,
        size.height.saturating_sub(height),
    )?;
    if restored {
        sink.mark_emitted(stable_transcript_entry_count(state));
    }
    Ok(restored)
}

fn sync_inline_terminal_height(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    state: &AppState,
    current_height: &mut u16,
) -> Result<()> {
    let size = match terminal.size() {
        Ok(size) => size,
        Err(e) if is_cursor_position_timeout_io(&e) => {
            trace_inline_cursor_position_timeout("viewport resize size query", &e);
            return Ok(());
        }
        Err(e) => return Err(e).context("query terminal size for inline viewport resize"),
    };
    let area = terminal.get_frame().area();
    let Some(plan) = inline_viewport_resize_plan(state, area, size, *current_height) else {
        return Ok(());
    };

    if let Err(e) = terminal.backend_mut().set_cursor_position(plan.origin) {
        if is_cursor_position_timeout_io(&e) {
            trace_inline_cursor_position_timeout("viewport resize cursor move", &e);
            return Ok(());
        }
        tracing::warn!("skip inline viewport resize: set cursor position failed: {e}");
        return Ok(());
    }
    let clear_type = if plan.clear_visible_screen {
        ClearType::All
    } else {
        ClearType::AfterCursor
    };
    if let Err(e) = terminal.backend_mut().clear_region(clear_type) {
        tracing::warn!("skip inline viewport resize: clear region failed: {e}");
        return Ok(());
    }
    if let Err(e) = Write::flush(terminal.backend_mut()) {
        tracing::warn!("skip inline viewport resize: flush failed: {e}");
        return Ok(());
    }

    // Seed the new backend with the anchor the cursor was just moved to, so
    // creating the inline viewport never issues a CPR query. A real query
    // here deadlocks against the crossterm EventStream lock and times out,
    // leaving the freshly cleared region blank until the next input event.
    let backend = TrackedBackend::with_cursor_position(io::stdout(), plan.origin);
    let next = match Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(plan.height),
        },
    )
    .context("resize inline terminal")
    {
        Ok(next) => next,
        Err(e) => {
            tracing::warn!("skip inline viewport resize: {e:#}");
            return Ok(());
        }
    };
    *terminal = next;
    *current_height = plan.height;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InlineViewportResizePlan {
    height: u16,
    origin: Position,
    clear_visible_screen: bool,
}

fn inline_viewport_resize_plan(
    state: &AppState,
    current_area: Rect,
    size: Size,
    current_height: u16,
) -> Option<InlineViewportResizePlan> {
    let height = clamped_inline_height(desired_inline_height(state, size), size);
    let reader_active = inline_reader_accepts_input(state);
    let reader_geometry_changed = reader_active
        && (current_area.width != size.width
            || current_area.height != height
            || current_area.y != size.height.saturating_sub(height));
    if height == current_height && !reader_geometry_changed {
        return None;
    }

    let origin = if reader_active {
        Position::new(0, size.height.saturating_sub(height))
    } else {
        current_area.as_position()
    };
    Some(InlineViewportResizePlan {
        height,
        origin,
        clear_visible_screen: reader_active,
    })
}

fn desired_inline_height(state: &AppState, terminal_size: Size) -> u16 {
    // Full-history readers take the whole terminal (minus one row) so long
    // documents and complete nested-actor rosters are calm to page through.
    if (state.transcript_viewer || state.nested_agent_viewer || state.workspace_diff_viewer)
        && !state.has_pending_permission()
        && !state.has_pending_elicitation()
    {
        return terminal_size
            .height
            .saturating_sub(1)
            .max(INLINE_CHAT_HEIGHT);
    }
    // URL setup steps contain OAuth links plus QR codes. The QR must keep its
    // aspect ratio and quiet zone, so let this view use the full inline pane
    // instead of the generic compact overlay cap.
    if matches!(state.elicitation_view(), Some(ElicitationView::Url { .. })) {
        return terminal_size
            .height
            .saturating_sub(1)
            .max(INLINE_CHAT_HEIGHT);
    }
    // The Ragnarok arena also takes the whole terminal; battles deserve a
    // stage. Pending permission/elicitation prompts still win (above rule
    // does not apply — they render inside the arena-sized viewport fine).
    if state.ragnarok.is_some() {
        return terminal_size
            .height
            .saturating_sub(1)
            .max(INLINE_CHAT_HEIGHT);
    }
    let max_height = terminal_size
        .height
        .saturating_sub(1)
        .clamp(INLINE_CHAT_HEIGHT, INLINE_EXPANDED_MAX_HEIGHT);
    let width = terminal_size.width.saturating_sub(2).max(1);
    let desired = if state.help_overlay {
        usize::from(INLINE_HELP_HEIGHT)
    } else if state.mjconfig_menu.is_some() {
        usize::from(INLINE_MJCONFIG_HEIGHT)
    } else if let Some(picker) = state.agent_picker.as_ref() {
        picker.role_indices.len().saturating_add(4)
    } else if state.review_picker.is_some() {
        6
    } else if let Some(pending) = state.pending_permission() {
        permission_view_lines(
            pending,
            state.pending_permission_count(),
            width,
            state.theme,
        )
        .len()
            + 1
    } else if let Some(pending) = state.pending_elicitation() {
        elicitation_view_lines(
            pending,
            state.pending_elicitation_count(),
            width,
            state.theme,
        )
        .lines
        .len()
            + 1
    } else if state.config_picker.is_some() {
        inline_config_view_line_count(state, width)
    } else {
        // Queued prompts render above the input; request extra rows so
        // the input box keeps its full height while the queue is visible.
        usize::from(INLINE_CHAT_HEIGHT)
            + usize::from(queued_prompt_row_count(state))
            + usize::from(workflow_progress_row_count(state))
            + usize::from(current_branch_pr_row_count(state))
            + usage_quota_row_count(state, width)
    };

    (desired.min(usize::from(u16::MAX)) as u16).clamp(INLINE_CHAT_HEIGHT, max_height)
}

fn handle_crossterm(
    state: &mut AppState,
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    ev: CtEvent,
    mode: UiMode,
) -> TerminalRequest {
    let key = match ev {
        CtEvent::Key(k) => k,
        CtEvent::Paste(text) => {
            // Route paste into an active free-text elicitation field -- users
            // paste API keys/tokens there. Strip control characters so a
            // trailing newline can't pre-submit or split the field.
            if state.elicitation_accepts_text_input() {
                let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
                if let Some(pending) = state.pending_elicitation_mut() {
                    pending.input.push_str(&cleaned);
                }
                return TerminalRequest::None;
            }
            // Skip paste when another modal is active;
            // the input buffer isn't focused and pasted text would land
            // invisibly in the background.
            if state.help_overlay
                || state.has_pending_permission()
                || state.has_pending_elicitation()
                || state.agent_picker.is_some()
                || state.config_picker.is_some()
                || state.mjconfig_menu.is_some()
                || state.ragnarok.is_some()
                || state.transcript_viewer
                || state.nested_agent_viewer
                || state.workspace_diff_viewer
            {
                return TerminalRequest::None;
            }
            state.input_paste_burst.clear();
            handle_paste(state, &text);
            return TerminalRequest::None;
        }
        CtEvent::Mouse(mouse) => {
            // The diff reader does not support mouse scrolling yet. In
            // particular, do not mutate the hidden transcript's offset.
            if state.workspace_diff_viewer || state.nested_agent_viewer || state.review_issue_viewer
            {
                return TerminalRequest::None;
            } else if mode == UiMode::InlineChat && inline_transcript_viewer_accepts_input(state) {
                handle_transcript_viewer_mouse(state, mouse);
            } else if mode == UiMode::FullscreenTui {
                handle_mouse(state, mouse);
            }
            return TerminalRequest::None;
        }
        _ => return TerminalRequest::None,
    };
    if key.kind != KeyEventKind::Press {
        return TerminalRequest::None;
    }

    if mode == UiMode::FullscreenTui
        && is_text_selection_key(key.modifiers, key.code)
        && can_toggle_text_selection_mode(state)
    {
        return TerminalRequest::ToggleTextSelectionMode;
    }

    if key.modifiers == KeyModifiers::CONTROL
        && matches!(key.code, KeyCode::Char('c'))
        && state.is_side
        && state.input.is_empty()
        && attachment_count(state) == 0
    {
        state.side_exit_requested = true;
        return TerminalRequest::None;
    }

    if key.modifiers == KeyModifiers::CONTROL
        && matches!(key.code, KeyCode::Char('c'))
        && state.is_streaming()
    {
        cancel_current_turn(state, cmd_tx);
        return TerminalRequest::None;
    }

    if state.help_overlay {
        if is_help_key(key.modifiers, key.code) || matches!(key.code, KeyCode::Esc) {
            state.help_overlay = false;
            return inline_repair_request(mode);
        }
        scroll_help_overlay(state, key.code);
        return TerminalRequest::None;
    }

    // The /mjconfig overlay owns the keyboard while it is open, but yields to a
    // pending permission prompt: that modal is drawn on top of the menu and must
    // stay actionable (the menu can be opened mid-turn, before the prompt
    // arrives). Mirrors the transcript-viewer carve-out below.
    if state.mjconfig_menu.is_some()
        && !state.has_pending_permission()
        && !state.has_pending_elicitation()
    {
        return handle_mjconfig_menu_key(state, cmd_tx, key.modifiers, key.code, mode);
    }

    if should_open_help(key.modifiers, key.code) {
        open_help_overlay(state);
        return TerminalRequest::None;
    }

    if !state.has_pending_permission()
        && !state.has_pending_elicitation()
        && state.ragnarok.is_none()
        && state.agent_picker.is_none()
        && state.config_picker.is_none()
        && key.modifiers.is_empty()
        && matches!(key.code, KeyCode::F(9))
    {
        if state.review_issue_viewer {
            state.close_review_issue_viewer();
        } else {
            state.open_review_issue_viewer();
        }
        return inline_repair_request(mode);
    }

    if !state.has_pending_permission()
        && !state.has_pending_elicitation()
        && state.ragnarok.is_none()
        && state.agent_picker.is_none()
        && state.config_picker.is_none()
        && key.modifiers.is_empty()
        && matches!(key.code, KeyCode::F(11))
    {
        if state.nested_agent_viewer {
            state.close_nested_agent_viewer();
        } else if !state.open_nested_agent_viewer() {
            state.status_line = Some(StatusMessage::info("no nested agents to inspect"));
        }
        return inline_repair_request(mode);
    }

    if !state.has_pending_permission()
        && !state.has_pending_elicitation()
        && state.ragnarok.is_none()
        && state.agent_picker.is_none()
        && state.config_picker.is_none()
        && key.modifiers == KeyModifiers::CONTROL
        && matches!(key.code, KeyCode::Char('g' | 'G'))
    {
        if state.workspace_diff_viewer {
            state.close_workspace_diff_viewer();
        } else {
            state.open_workspace_diff_viewer();
        }
        return inline_repair_request(mode);
    }

    // The full-transcript reader owns the keyboard while open so scrolling
    // keys don't leak into the prompt. A pending permission prompt takes
    // precedence: it suspends the reader (drawn over it) until resolved.
    if state.workspace_diff_viewer
        && !state.has_pending_permission()
        && !state.has_pending_elicitation()
    {
        return handle_workspace_diff_viewer_key(state, key.modifiers, key.code, mode);
    }
    if state.review_issue_viewer
        && !state.has_pending_permission()
        && !state.has_pending_elicitation()
    {
        match key.code {
            KeyCode::Esc | KeyCode::F(9) => state.close_review_issue_viewer(),
            KeyCode::Up | KeyCode::Char('k') => {
                state.review_issue_scroll_offset =
                    state.review_issue_scroll_offset.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                state.review_issue_scroll_offset =
                    state.review_issue_scroll_offset.saturating_add(1)
            }
            KeyCode::PageUp => {
                state.review_issue_scroll_offset =
                    state.review_issue_scroll_offset.saturating_sub(5)
            }
            KeyCode::PageDown => {
                state.review_issue_scroll_offset =
                    state.review_issue_scroll_offset.saturating_add(5)
            }
            KeyCode::Home => state.review_issue_scroll_offset = 0,
            KeyCode::End => state.review_issue_scroll_offset = usize::MAX,
            _ => {}
        }
        return inline_repair_request(mode);
    }
    if state.nested_agent_viewer
        && !state.has_pending_permission()
        && !state.has_pending_elicitation()
    {
        return handle_nested_agent_viewer_key(state, key.modifiers, key.code, mode);
    }
    if state.transcript_viewer
        && !state.has_pending_permission()
        && !state.has_pending_elicitation()
    {
        return handle_transcript_viewer_key(state, key.modifiers, key.code, mode);
    }

    if state.runtime_closed {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c'))
            | (KeyModifiers::CONTROL, KeyCode::Char('d'))
            | (_, KeyCode::Esc) => {
                state.exit_reason = Some(UiExitReason::Quit);
                return TerminalRequest::None;
            }
            (_, code) if should_open_help(key.modifiers, code) => {
                open_help_overlay(state);
                return TerminalRequest::None;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('n')) => {
                state.exit_reason = Some(UiExitReason::NewSession);
                return TerminalRequest::None;
            }
            (KeyModifiers::ALT, KeyCode::Char('t' | 'T')) if mode == UiMode::FullscreenTui => {
                toggle_latest_visible_tool(state, true);
                return TerminalRequest::None;
            }
            (modifiers, KeyCode::Char('t' | 'T'))
                if modifiers.contains(KeyModifiers::CONTROL)
                    && !modifiers.intersects(
                        KeyModifiers::ALT
                            | KeyModifiers::SUPER
                            | KeyModifiers::HYPER
                            | KeyModifiers::META,
                    ) =>
            {
                toggle_transcript_expansion(state, mode);
                return TerminalRequest::None;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('y')) => {
                copy_last_agent_message(state);
                return TerminalRequest::None;
            }
            (_, KeyCode::PageUp) if mode == UiMode::FullscreenTui => {
                state.scroll_offset = state.scroll_offset.saturating_add(5);
                return TerminalRequest::None;
            }
            (_, KeyCode::PageDown) if mode == UiMode::FullscreenTui => {
                state.scroll_offset = state.scroll_offset.saturating_sub(5);
                return TerminalRequest::None;
            }
            (_, KeyCode::Up) if mode == UiMode::FullscreenTui => {
                state.scroll_offset = state.scroll_offset.saturating_add(1);
                return TerminalRequest::None;
            }
            (_, KeyCode::Down) if mode == UiMode::FullscreenTui => {
                state.scroll_offset = state.scroll_offset.saturating_sub(1);
                return TerminalRequest::None;
            }
            (_, KeyCode::Home) if mode == UiMode::FullscreenTui => {
                scroll_to_top(state);
                return TerminalRequest::None;
            }
            (_, KeyCode::End) if mode == UiMode::FullscreenTui => {
                scroll_to_bottom(state);
                return TerminalRequest::None;
            }
            _ => {}
        }
    }

    // Permission modal owns the keyboard while it's open.
    if state.has_pending_permission() {
        return handle_permission_key(state, key.code, mode);
    }

    // Elicitation modal owns the keyboard next. Permission is safety-critical
    // and wins if both are somehow pending (its check runs first above).
    if state.has_pending_elicitation() {
        return handle_elicitation_key(state, key.code, mode);
    }

    // The Ragnarok arena owns the keyboard while a battle is on screen. It
    // yields only to the safety-critical modals above.
    if state.ragnarok.is_some() {
        return handle_ragnarok_key(state, key.modifiers, key.code, mode);
    }

    if state.agent_picker.is_some() {
        return handle_agent_picker_key(state, key.modifiers, key.code, mode);
    }

    if state.review_picker.is_some() {
        return handle_review_picker_key(state, cmd_tx, key.modifiers, key.code, mode);
    }

    if state.config_picker.is_some() {
        return handle_config_picker_key(state, cmd_tx, key.modifiers, key.code, mode);
    }

    if matches!(key.code, KeyCode::BackTab) {
        if !state.open_agent_picker() {
            state.status_line = Some(StatusMessage::info("no ACP agent is currently available"));
        }
        return TerminalRequest::None;
    }

    if !is_plain_character_input(key.modifiers, key.code) {
        flush_input_paste_burst_if_due(state, Instant::now(), true);
    }

    // Slash-command autocomplete owns Tab and Up/Down while it's
    // visible, and intercepts Enter/Esc before the normal handlers see
    // them. Plain typing still falls through so the user can refine the
    // filter.
    if state.autocomplete.visible && !state.runtime_closed {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Tab) | (KeyModifiers::NONE, KeyCode::Enter) => {
                state.autocomplete_accept();
                return inline_repair_request(mode);
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                state.autocomplete_move(-1);
                return TerminalRequest::None;
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                state.autocomplete_move(1);
                return TerminalRequest::None;
            }
            (_, KeyCode::Esc) => {
                state.autocomplete_dismiss();
                return inline_repair_request(mode);
            }
            _ => {}
        }
    }

    if mode == UiMode::FullscreenTui && key.modifiers == KeyModifiers::CONTROL {
        match key.code {
            KeyCode::PageUp => {
                state.scroll_offset = state
                    .scroll_offset
                    .saturating_add(TRANSCRIPT_SCROLL_PAGE_STEP);
                return TerminalRequest::None;
            }
            KeyCode::PageDown => {
                state.scroll_offset = state
                    .scroll_offset
                    .saturating_sub(TRANSCRIPT_SCROLL_PAGE_STEP);
                return TerminalRequest::None;
            }
            KeyCode::Up => {
                state.scroll_offset = state.scroll_offset.saturating_add(1);
                return TerminalRequest::None;
            }
            KeyCode::Down => {
                state.scroll_offset = state.scroll_offset.saturating_sub(1);
                return TerminalRequest::None;
            }
            KeyCode::Home => {
                scroll_to_top(state);
                return TerminalRequest::None;
            }
            KeyCode::End => {
                scroll_to_bottom(state);
                return TerminalRequest::None;
            }
            _ => {}
        }
    }

    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            if state.is_streaming() {
                cancel_current_turn(state, cmd_tx);
            } else if state.input.is_empty() && attachment_count(state) == 0 {
                state.exit_reason = Some(UiExitReason::Quit);
            } else if !state.input.is_empty() {
                state.input.clear();
                state.input_cursor = 0;
                state.reset_history_navigation();
                state.scroll_input_to_bottom();
                state.update_autocomplete();
            } else {
                clear_attachments(state);
                state.reset_history_navigation();
                state.scroll_input_to_bottom();
                state.update_autocomplete();
            }
        }
        (_, KeyCode::Esc) if state.is_streaming() => {
            cancel_current_turn(state, cmd_tx);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('d'))
            if state.input.is_empty() && attachment_count(state) == 0 =>
        {
            state.exit_reason = Some(UiExitReason::Quit);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('n')) => {
            state.exit_reason = Some(UiExitReason::NewSession);
        }
        (KeyModifiers::ALT, KeyCode::Char('t' | 'T')) if mode == UiMode::FullscreenTui => {
            toggle_latest_visible_tool(state, true);
        }
        (modifiers, KeyCode::Char('t' | 'T'))
            if modifiers.contains(KeyModifiers::CONTROL)
                && !modifiers.intersects(
                    KeyModifiers::ALT
                        | KeyModifiers::SUPER
                        | KeyModifiers::HYPER
                        | KeyModifiers::META,
                ) =>
        {
            toggle_transcript_expansion(state, mode);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('y')) => {
            copy_last_agent_message(state);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('r')) => {
            return dictation_request_for_state(state, voice_input_supported());
        }
        (modifiers, KeyCode::Char('v'))
            if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            paste_clipboard_image(state);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('o')) => {
            state.exit_reason = Some(UiExitReason::LoadSession);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
            move_input_cursor_to_line_start(state);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
            move_input_cursor_to_line_end(state);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('b')) => {
            move_input_cursor_left(state);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('f')) => {
            move_input_cursor_right(state);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
            delete_to_line_end(state);
            state.update_autocomplete();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
            delete_to_line_start(state);
            state.update_autocomplete();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
            delete_previous_word(state);
            state.update_autocomplete();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
            if !delete_at_cursor(state) && state.input.is_empty() && !pop_last_attachment(state) {
                state.exit_reason = Some(UiExitReason::Quit);
                return TerminalRequest::None;
            }
            state.update_autocomplete();
        }
        // Insert a literal newline in the input buffer, so the user can
        // draft multi-line prompts without submitting.
        (modifiers, code) if is_prompt_newline_key(modifiers, code) => {
            insert_text_at_cursor(state, "\n");
            state.update_autocomplete();
        }
        (_, KeyCode::Enter) => {
            submit_prompt(state, cmd_tx);
            if state.voice_input_active {
                return TerminalRequest::StopDictation;
            }
        }
        (KeyModifiers::ALT, KeyCode::Backspace) => {
            delete_previous_word(state);
            state.update_autocomplete();
        }
        (KeyModifiers::ALT, KeyCode::Char('b')) => {
            move_input_cursor_word_left(state);
        }
        (KeyModifiers::ALT, KeyCode::Char('f')) => {
            move_input_cursor_word_right(state);
        }
        (_, KeyCode::Backspace) => {
            let mut handled = state.input.is_empty() && pop_last_attachment(state);
            if !handled {
                handled = pop_inline_attachment_at_cursor(state);
            }
            if !handled {
                handled = delete_before_cursor(state);
            }
            if !handled {
                // Remove the last attachment chip when the input buffer is empty.
                pop_last_attachment(state);
            }
            state.update_autocomplete();
        }
        (_, KeyCode::Delete) => {
            if !pop_inline_attachment_at_cursor(state) {
                delete_at_cursor(state);
            }
            state.update_autocomplete();
        }
        (_, KeyCode::Left) => {
            move_input_cursor_left(state);
        }
        (_, KeyCode::Right) => {
            move_input_cursor_right(state);
        }
        (_, KeyCode::Up) => move_input_cursor_up_or_history(state, 1),
        (_, KeyCode::Down) => move_input_cursor_down_or_history(state, 1),
        (_, KeyCode::PageUp) => move_input_cursor_up(state, TRANSCRIPT_SCROLL_PAGE_STEP),
        (_, KeyCode::PageDown) => move_input_cursor_down(state, TRANSCRIPT_SCROLL_PAGE_STEP),
        (_, KeyCode::Home) => move_input_cursor_to_line_start(state),
        (_, KeyCode::End) => move_input_cursor_to_line_end(state),
        (_, KeyCode::Char(c)) => {
            let cursor_before_insert = state.input_cursor;
            insert_text_at_cursor(state, &c.to_string());
            note_plain_input_char(state, cursor_before_insert, c, Instant::now());
            state.update_autocomplete();
        }
        (_, KeyCode::Esc) => {
            state.input.clear();
            state.input_cursor = 0;
            clear_attachments(state);
            state.reset_history_navigation();
            state.scroll_input_to_bottom();
            state.update_autocomplete();
        }
        _ => {}
    }
    TerminalRequest::None
}

fn cancel_current_turn(state: &mut AppState, cmd_tx: &mpsc::UnboundedSender<UiCommand>) {
    if state.connection_state() != ConnectionState::Streaming {
        return;
    }
    let _ = cmd_tx.send(UiCommand::CancelPrompt);
    state.mark_cancelling();
    let queued = state.queued_prompt_count();
    let msg = if queued > 0 {
        format!("cancelling current turn... ({queued} queued)")
    } else {
        "cancelling current turn...".to_string()
    };
    state.status_line = Some(StatusMessage::info(msg));
}

fn dictation_request_for_state(state: &AppState, voice_input_supported: bool) -> TerminalRequest {
    if !voice_input_supported {
        TerminalRequest::None
    } else if state.voice_input_active {
        TerminalRequest::StopDictation
    } else {
        TerminalRequest::StartDictation
    }
}

fn handle_mouse(state: &mut AppState, mouse: MouseEvent) {
    if state.text_selection_mode
        || state.help_overlay
        || state.has_pending_permission()
        || state.has_pending_elicitation()
        || state.agent_picker.is_some()
        || state.config_picker.is_some()
    {
        return;
    }

    match mouse.kind {
        MouseEventKind::ScrollUp => {
            state.scroll_offset = state
                .scroll_offset
                .saturating_add(TRANSCRIPT_SCROLL_WHEEL_STEP);
        }
        MouseEventKind::ScrollDown => {
            state.scroll_offset = state
                .scroll_offset
                .saturating_sub(TRANSCRIPT_SCROLL_WHEEL_STEP);
        }
        _ => {}
    }
}

fn handle_transcript_viewer_mouse(state: &mut AppState, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            state.scroll_offset = state
                .scroll_offset
                .saturating_sub(TRANSCRIPT_SCROLL_WHEEL_STEP);
        }
        MouseEventKind::ScrollDown => {
            state.scroll_offset = state
                .scroll_offset
                .saturating_add(TRANSCRIPT_SCROLL_WHEEL_STEP);
        }
        _ => {}
    }
}

async fn apply_terminal_request(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    state: &mut AppState,
    request: TerminalRequest,
    dictation_tx: &mpsc::UnboundedSender<DictationEvent>,
    dictation_cancel_tx: &mut Option<std_mpsc::Sender<()>>,
    mode: UiMode,
) -> Result<()> {
    match request {
        TerminalRequest::None => Ok(()),
        TerminalRequest::ToggleTextSelectionMode => {
            let next = !state.text_selection_mode;
            set_mouse_capture(terminal, !next)?;
            state.text_selection_mode = next;
            state.status_line = Some(StatusMessage::info(if next {
                "text selection mode: mouse selection enabled; press F12 to resume wheel scrolling"
            } else {
                "wheel scrolling enabled; press F12 to select text with the mouse"
            }));
            Ok(())
        }
        TerminalRequest::StartDictation => {
            start_dictation(state, dictation_tx, dictation_cancel_tx);
            Ok(())
        }
        TerminalRequest::StopDictation => {
            stop_dictation(state, dictation_cancel_tx);
            Ok(())
        }
        TerminalRequest::ForceInlineRepair => Ok(()),
        TerminalRequest::CopyText(text) => {
            copy_text_to_clipboard(state, &text, Some("URL"));
            Ok(())
        }
        TerminalRequest::Authenticate(vendor) => {
            restore_terminal_for_auth(terminal, mode)?;
            let login = crate::auth::run_login(vendor).await;
            let resumed = resume_terminal_after_auth(terminal, mode);
            let notice = match (login, resumed) {
                (Ok(outcome), Ok(())) => outcome.into_message(),
                (Err(error), Ok(())) => format!("Sign-in failed: {error:#}"),
                (_, Err(error)) => return Err(error.context("restore UI after sign-in")),
            };
            if let Some(menu) = state.mjconfig_menu.as_mut() {
                menu.editor.refresh_after_auth(notice);
            }
            Ok(())
        }
    }
}

fn set_mouse_capture(terminal: &mut Terminal<TrackedBackend<Stdout>>, enabled: bool) -> Result<()> {
    if enabled {
        execute!(terminal.backend_mut(), EnableMouseCapture).context("enable mouse capture")
    } else {
        execute!(terminal.backend_mut(), DisableMouseCapture).context("disable mouse capture")
    }
}

fn reset_text_selection_mode_for_exit<F>(state: &mut AppState, mut set_capture: F) -> Result<()>
where
    F: FnMut(bool) -> Result<()>,
{
    if state.text_selection_mode {
        set_capture(true)?;
        state.text_selection_mode = false;
    }
    Ok(())
}

fn input_char_count(text: &str) -> usize {
    text.chars().count()
}

fn input_byte_index_at_char(text: &str, char_index: usize) -> usize {
    if char_index == 0 {
        return 0;
    }
    text.char_indices()
        .nth(char_index)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

fn insert_text_at_cursor(state: &mut AppState, text: &str) {
    state.reset_history_navigation();
    let cursor = state.input_cursor.min(input_char_count(&state.input));
    let byte_index = input_byte_index_at_char(&state.input, cursor);
    state.input.insert_str(byte_index, text);
    let inserted = input_char_count(text);
    for attachment in &mut state.attachments {
        if attachment.position > cursor {
            attachment.position = attachment.position.saturating_add(inserted);
        }
    }
    for attachment in &mut state.image_attachments {
        if attachment.position > cursor {
            attachment.position = attachment.position.saturating_add(inserted);
        }
    }
    state.input_cursor = cursor + inserted;
}

fn delete_input_range(state: &mut AppState, start: usize, end: usize, new_cursor: usize) -> bool {
    state.reset_history_navigation();
    let len = input_char_count(&state.input);
    let start = start.min(len);
    let end = end.min(len);
    if start >= end {
        return false;
    }

    let byte_start = input_byte_index_at_char(&state.input, start);
    let byte_end = input_byte_index_at_char(&state.input, end);
    state.input.drain(byte_start..byte_end);
    let removed = end - start;
    for attachment in &mut state.attachments {
        if attachment.position > end {
            attachment.position = attachment.position.saturating_sub(removed);
        } else if attachment.position > start {
            attachment.position = start;
        }
    }
    for attachment in &mut state.image_attachments {
        if attachment.position > end {
            attachment.position = attachment.position.saturating_sub(removed);
        } else if attachment.position > start {
            attachment.position = start;
        }
    }
    state.input_cursor = new_cursor.min(input_char_count(&state.input));
    true
}

fn replace_input_range(
    state: &mut AppState,
    start: usize,
    end: usize,
    text: &str,
) -> (usize, usize) {
    state.reset_history_navigation();
    let len = input_char_count(&state.input);
    let start = start.min(len);
    let end = end.min(len).max(start);
    let byte_start = input_byte_index_at_char(&state.input, start);
    let byte_end = input_byte_index_at_char(&state.input, end);
    state.input.replace_range(byte_start..byte_end, text);
    let next_end = start + input_char_count(text);
    let removed = end - start;
    let inserted = input_char_count(text);
    for attachment in &mut state.attachments {
        if attachment.position > end {
            attachment.position = attachment
                .position
                .saturating_sub(removed)
                .saturating_add(inserted);
        } else if attachment.position > start {
            attachment.position = start + inserted;
        }
    }
    for attachment in &mut state.image_attachments {
        if attachment.position > end {
            attachment.position = attachment
                .position
                .saturating_sub(removed)
                .saturating_add(inserted);
        } else if attachment.position > start {
            attachment.position = start + inserted;
        }
    }
    state.input_cursor = next_end;
    (start, next_end)
}

fn delete_before_cursor(state: &mut AppState) -> bool {
    let cursor = state.input_cursor.min(input_char_count(&state.input));
    if cursor == 0 {
        return false;
    }
    delete_input_range(state, cursor - 1, cursor, cursor - 1)
}

fn delete_at_cursor(state: &mut AppState) -> bool {
    let cursor = state.input_cursor.min(input_char_count(&state.input));
    delete_input_range(state, cursor, cursor + 1, cursor)
}

fn move_input_cursor_left(state: &mut AppState) {
    let len = input_char_count(&state.input);
    state.input_cursor = state.input_cursor.min(len).saturating_sub(1);
}

fn move_input_cursor_right(state: &mut AppState) {
    let len = input_char_count(&state.input);
    state.input_cursor = state.input_cursor.min(len);
    if state.input_cursor < len {
        state.input_cursor += 1;
    }
}

fn input_char_at(text: &str, char_index: usize) -> Option<char> {
    text.chars().nth(char_index)
}

fn input_prev_word_boundary(text: &str, cursor_char_index: usize) -> usize {
    let len = input_char_count(text);
    let mut index = cursor_char_index.min(len);

    while index > 0
        && input_char_at(text, index - 1)
            .map(|c| c.is_whitespace())
            .unwrap_or(false)
    {
        index -= 1;
    }

    while index > 0
        && input_char_at(text, index - 1)
            .map(|c| !c.is_whitespace())
            .unwrap_or(false)
    {
        index -= 1;
    }

    index
}

fn input_next_word_boundary(text: &str, cursor_char_index: usize) -> usize {
    let len = input_char_count(text);
    let mut index = cursor_char_index.min(len);

    while index < len
        && input_char_at(text, index)
            .map(|c| !c.is_whitespace())
            .unwrap_or(false)
    {
        index += 1;
    }

    while index < len
        && input_char_at(text, index)
            .map(|c| c.is_whitespace())
            .unwrap_or(false)
    {
        index += 1;
    }

    index
}

fn move_input_cursor_word_left(state: &mut AppState) {
    state.input_cursor = input_prev_word_boundary(&state.input, state.input_cursor);
}

fn move_input_cursor_word_right(state: &mut AppState) {
    state.input_cursor = input_next_word_boundary(&state.input, state.input_cursor);
}

fn input_line_cursor_position(text: &str, cursor_char_index: usize) -> (usize, usize, usize) {
    let cursor = cursor_char_index.min(input_char_count(text));
    let mut consumed = 0usize;
    let total_lines = text.split('\n').count().max(1);

    for (line_index, line) in text.split('\n').enumerate() {
        let line_len = line.chars().count();
        if cursor <= consumed + line_len {
            return (line_index, cursor - consumed, total_lines);
        }
        consumed += line_len + 1;
    }

    (total_lines.saturating_sub(1), 0, total_lines)
}

fn input_cursor_index_for_line_position(
    text: &str,
    target_line: usize,
    target_col: usize,
) -> usize {
    let mut chars_before = 0usize;

    for (line_index, line) in text.split('\n').enumerate() {
        let line_len = line.chars().count();
        if line_index == target_line {
            return chars_before + target_col.min(line_len);
        }
        chars_before += line_len + 1;
    }

    input_char_count(text)
}

fn move_input_cursor_to_line_start(state: &mut AppState) {
    let (line, _, _) = input_line_cursor_position(&state.input, state.input_cursor);
    state.input_cursor = input_cursor_index_for_line_position(&state.input, line, 0);
}

fn move_input_cursor_to_line_end(state: &mut AppState) {
    state.input_cursor = input_current_line_end_index(&state.input, state.input_cursor);
}

fn input_current_line_start_index(text: &str, cursor_char_index: usize) -> usize {
    let (line, _, _) = input_line_cursor_position(text, cursor_char_index);
    input_cursor_index_for_line_position(text, line, 0)
}

fn input_current_line_end_index(text: &str, cursor_char_index: usize) -> usize {
    let (line, _, _) = input_line_cursor_position(text, cursor_char_index);
    let line_len = input_line_length(text, line);
    input_cursor_index_for_line_position(text, line, line_len)
}

fn input_line_length(text: &str, line_index: usize) -> usize {
    text.split('\n')
        .nth(line_index)
        .map(|line| line.chars().count())
        .unwrap_or(0)
}

fn delete_to_line_start(state: &mut AppState) -> bool {
    let start = input_current_line_start_index(&state.input, state.input_cursor);
    delete_input_range(state, start, state.input_cursor, start)
}

fn delete_to_line_end(state: &mut AppState) -> bool {
    let end = input_current_line_end_index(&state.input, state.input_cursor);
    delete_input_range(state, state.input_cursor, end, state.input_cursor)
}

fn delete_previous_word(state: &mut AppState) -> bool {
    let cursor = state.input_cursor.min(input_char_count(&state.input));
    let start = input_prev_word_boundary(&state.input, cursor);
    delete_input_range(state, start, cursor, start)
}

#[cfg(test)]
fn input_cursor_visual_position(
    text: &str,
    cursor_char_index: usize,
    inner_w: usize,
) -> (usize, usize, usize) {
    let layout = input_wrapped_layout(text, cursor_char_index, inner_w);
    (
        layout.cursor_row,
        layout.cursor_col,
        layout.rows.len().max(1),
    )
}

fn move_input_cursor_vertical(state: &mut AppState, delta_rows: isize) {
    let (line, col, total_lines) = input_line_cursor_position(&state.input, state.input_cursor);
    if total_lines == 0 {
        return;
    }

    let max_line = total_lines.saturating_sub(1);
    let target_line = if delta_rows.is_negative() {
        line.saturating_sub(delta_rows.unsigned_abs())
    } else {
        line.saturating_add(delta_rows as usize)
    }
    .min(max_line);

    state.input_cursor = input_cursor_index_for_line_position(&state.input, target_line, col);
}

/// Move the cursor up one line in the input buffer. When the cursor is
/// already on the first line, navigate to the previous (older) prompt in
/// history instead (Up-at-top = shell-style reverse history search).
///
/// This matches bash/zsh behavior: pressing Up on line 0 of a multiline
/// prompt navigates history rather than being a no-op at the top.
fn move_input_cursor_up_or_history(state: &mut AppState, lines: usize) {
    let (line, _, _) = input_line_cursor_position(&state.input, state.input_cursor);
    if line == 0 && state.prompt_history_previous() {
        return;
    }
    move_input_cursor_up(state, lines);
}

/// Move the cursor down one line in the input buffer. When the cursor is
/// already on the last line and the user is browsing history, navigate
/// to the next (newer) prompt (Down-at-bottom = forward history).
///
/// This matches bash/zsh behavior: pressing Down on the last line of a
/// multiline prompt navigates history forward rather than being a no-op
/// at the bottom.
fn move_input_cursor_down_or_history(state: &mut AppState, lines: usize) {
    let (line, _, total_lines) = input_line_cursor_position(&state.input, state.input_cursor);
    if line + 1 >= total_lines && state.prompt_history_next() {
        return;
    }
    move_input_cursor_down(state, lines);
}

fn move_input_cursor_up(state: &mut AppState, lines: usize) {
    move_input_cursor_vertical(state, -(lines as isize));
}

fn move_input_cursor_down(state: &mut AppState, lines: usize) {
    move_input_cursor_vertical(state, lines as isize);
}

fn attachment_count(state: &AppState) -> usize {
    state.attachments.len() + state.image_attachments.len()
}

fn clear_attachments(state: &mut AppState) {
    state.attachments.clear();
    state.image_attachments.clear();
}

fn pop_last_attachment(state: &mut AppState) -> bool {
    let text_id = state.attachments.last().map(|attachment| attachment.id);
    let image_id = state
        .image_attachments
        .last()
        .map(|attachment| attachment.id);

    match (text_id, image_id) {
        (Some(t), Some(i)) if t > i => state.attachments.pop().is_some(),
        (Some(_), Some(_)) => state.image_attachments.pop().is_some(),
        (Some(_), None) => state.attachments.pop().is_some(),
        (None, Some(_)) => state.image_attachments.pop().is_some(),
        (None, None) => false,
    }
}

fn pop_inline_attachment_at_cursor(state: &mut AppState) -> bool {
    let cursor = state.input_cursor.min(input_char_count(&state.input));
    let text = state
        .attachments
        .iter()
        .enumerate()
        .filter(|(_, attachment)| attachment.position.min(input_char_count(&state.input)) == cursor)
        .max_by_key(|(_, attachment)| attachment.id)
        .map(|(index, attachment)| (attachment.id, index));
    let image = state
        .image_attachments
        .iter()
        .enumerate()
        .filter(|(_, attachment)| attachment.position.min(input_char_count(&state.input)) == cursor)
        .max_by_key(|(_, attachment)| attachment.id)
        .map(|(index, attachment)| (attachment.id, index));

    match (text, image) {
        (Some((text_id, text_index)), Some((image_id, _))) if text_id > image_id => {
            state.attachments.remove(text_index);
            true
        }
        (Some(_), Some((_, image_index))) => {
            state.image_attachments.remove(image_index);
            true
        }
        (Some((_, text_index)), None) => {
            state.attachments.remove(text_index);
            true
        }
        (None, Some((_, image_index))) => {
            state.image_attachments.remove(image_index);
            true
        }
        (None, None) => false,
    }
}

fn is_plain_character_input(modifiers: KeyModifiers, code: KeyCode) -> bool {
    matches!(code, KeyCode::Char(_))
        && !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

fn note_plain_input_char(state: &mut AppState, start_cursor: usize, ch: char, now: Instant) {
    let burst = &mut state.input_paste_burst;
    let continues_burst = burst
        .last_char_at
        .is_some_and(|last| now.duration_since(last) <= PASTE_BURST_CHAR_INTERVAL);

    if continues_burst && !burst.text.is_empty() {
        burst.text.push(ch);
    } else {
        burst.start_cursor = start_cursor;
        burst.text.clear();
        burst.text.push(ch);
    }
    burst.last_char_at = Some(now);
}

fn flush_input_paste_burst_if_due(state: &mut AppState, now: Instant, force: bool) -> bool {
    let Some(last_char_at) = state.input_paste_burst.last_char_at else {
        return false;
    };
    if !force && now.duration_since(last_char_at) <= PASTE_BURST_IDLE_TIMEOUT {
        return false;
    }

    let start = state.input_paste_burst.start_cursor;
    let text = state.input_paste_burst.text.clone();
    state.input_paste_burst.clear();

    if text.chars().count() < PASTE_BURST_MIN_CHARS || !state.prompt_images_supported {
        return false;
    }

    let end = start + input_char_count(&text);
    let input_len = input_char_count(&state.input);
    if end > input_len {
        return false;
    }
    let byte_start = input_byte_index_at_char(&state.input, start);
    let byte_end = input_byte_index_at_char(&state.input, end);
    if state.input.get(byte_start..byte_end) != Some(text.as_str()) {
        return false;
    }

    let Some((path, image)) = pasted_image_from_path_text(&text) else {
        return false;
    };

    delete_input_range(state, start, end, start);
    attach_clipboard_image(state, image);
    state.record_status_message(
        StatusKind::Info,
        format!("attached image from {}", display_pasted_path(&path)),
    );
    state.update_autocomplete();
    true
}

/// Translate a bracketed paste event into input buffer edits or an anchored chip.
/// Normalizes CRLF to LF and strips control characters (except tab and
/// newline) so pasted text from browsers or terminals renders predictably.
fn handle_paste(state: &mut AppState, text: &str) {
    let cleaned = normalize_paste(text);

    if cleaned.chars().count() > 1
        && state.prompt_images_supported
        && attach_pasted_image_path(state, &cleaned)
    {
        return;
    }

    let line_count = cleaned.lines().count();
    if line_count > 3 {
        let id = state.next_attachment_id;
        state.next_attachment_id += 1;
        state.attachments.push(PastedAttachment {
            id,
            position: state.input_cursor.min(input_char_count(&state.input)),
            content: cleaned,
        });
    } else {
        insert_text_at_cursor(state, &cleaned);
    }
    state.scroll_input_to_bottom();
    state.update_autocomplete();
}

fn attach_pasted_image_path(state: &mut AppState, pasted: &str) -> bool {
    let Some((path, image)) = pasted_image_from_path_text(pasted) else {
        return false;
    };

    attach_clipboard_image(state, image);
    state.record_status_message(
        StatusKind::Info,
        format!("attached image from {}", display_pasted_path(&path)),
    );
    true
}

fn pasted_image_from_path_text(pasted: &str) -> Option<(PathBuf, ClipboardImage)> {
    let path = normalize_pasted_image_path(pasted)?;
    let image = load_image_path_as_png(&path).ok()?;
    Some((path, image))
}

fn display_pasted_path(path: &Path) -> String {
    path.display().to_string()
}

fn normalize_pasted_image_path(pasted: &str) -> Option<PathBuf> {
    let pasted = pasted.trim();
    if pasted.is_empty() || pasted.contains('\n') {
        return None;
    }

    let unquoted = pasted
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| pasted.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(pasted);

    if let Ok(url) = url::Url::parse(unquoted)
        && url.scheme() == "file"
    {
        return url.to_file_path().ok();
    }

    if let Some(path) = normalize_windows_path(unquoted) {
        return Some(path);
    }

    let parts = shell_words::split(pasted).ok()?;
    if parts.len() != 1 {
        return None;
    }
    let part = parts.into_iter().next()?;
    normalize_windows_path(&part).or_else(|| Some(PathBuf::from(part)))
}

#[cfg(target_os = "linux")]
fn is_probably_wsl() -> bool {
    if let Ok(version) = std::fs::read_to_string("/proc/version") {
        let version_lower = version.to_lowercase();
        if version_lower.contains("microsoft") || version_lower.contains("wsl") {
            return true;
        }
    }

    std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some()
}

#[cfg(target_os = "linux")]
fn convert_windows_path_to_wsl(input: &str) -> Option<PathBuf> {
    if input.starts_with("\\\\") {
        return None;
    }

    let drive_letter = input.chars().next()?.to_ascii_lowercase();
    if !drive_letter.is_ascii_lowercase() || input.get(1..2) != Some(":") {
        return None;
    }

    let mut result = PathBuf::from(format!("/mnt/{drive_letter}"));
    for component in input
        .get(2..)?
        .trim_start_matches(['\\', '/'])
        .split(['\\', '/'])
        .filter(|component| !component.is_empty())
    {
        result.push(component);
    }
    Some(result)
}

fn normalize_windows_path(input: &str) -> Option<PathBuf> {
    let drive = input
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
        && input.get(1..2) == Some(":")
        && input.get(2..3).is_some_and(|s| s == "\\" || s == "/");
    let unc = input.starts_with("\\\\");
    if !drive && !unc {
        return None;
    }

    #[cfg(target_os = "linux")]
    {
        if is_probably_wsl()
            && let Some(converted) = convert_windows_path_to_wsl(input)
        {
            return Some(converted);
        }
    }

    Some(PathBuf::from(input))
}

fn attach_clipboard_image(state: &mut AppState, image: ClipboardImage) {
    let id = state.next_attachment_id;
    state.next_attachment_id += 1;
    state.image_attachments.push(PastedImageAttachment {
        id,
        position: state.input_cursor.min(input_char_count(&state.input)),
        data_base64: image.data_base64,
        mime_type: image.mime_type,
        width: image.width,
        height: image.height,
        byte_len: image.byte_len,
    });
    state.scroll_input_to_bottom();
    state.update_autocomplete();
}

fn paste_clipboard_image(state: &mut AppState) {
    if !state.prompt_images_supported {
        state.record_status_message(
            StatusKind::Warning,
            "this agent does not advertise image prompt support",
        );
        return;
    }

    match read_clipboard_image_as_png() {
        Ok(image) => {
            let width = image.width;
            let height = image.height;
            let byte_len = image.byte_len;
            attach_clipboard_image(state, image);
            state.record_status_message(
                StatusKind::Info,
                format!("attached image {width}x{height} ({byte_len} bytes)"),
            );
        }
        Err(e) => {
            state.record_status_message(StatusKind::Warning, format!("image paste failed: {e}"));
        }
    }
}

fn start_dictation(
    state: &mut AppState,
    dictation_tx: &mpsc::UnboundedSender<DictationEvent>,
    dictation_cancel_tx: &mut Option<std_mpsc::Sender<()>>,
) {
    if state.voice_input_active {
        state.status_line = Some(StatusMessage::info("voice input is already active..."));
        return;
    }

    state.input_paste_burst.clear();
    state.voice_input_active = true;
    state.voice_input_level = None;
    let cursor = state.input_cursor.min(input_char_count(&state.input));
    state.voice_input_range = Some((cursor, cursor));
    state.status_line = Some(StatusMessage::info("preparing voice input..."));

    let (cancel_tx, cancel_rx) = std_mpsc::channel();
    *dictation_cancel_tx = Some(cancel_tx);
    let dictation_tx = dictation_tx.clone();
    tokio::task::spawn_blocking(move || {
        let partial_tx = dictation_tx.clone();
        let level_tx = dictation_tx.clone();
        let status_tx = dictation_tx.clone();
        let result = run_dictation(
            move |text| {
                let _ = partial_tx.send(DictationEvent::Partial(text));
            },
            move |level| {
                let _ = level_tx.send(DictationEvent::Level(level));
            },
            move |message| {
                let _ = status_tx.send(DictationEvent::Status(message));
            },
            cancel_rx,
        )
        .map_err(|e| dictation_error_message(&e));
        let _ = dictation_tx.send(DictationEvent::Finished(result));
    });
}

fn stop_dictation(state: &mut AppState, dictation_cancel_tx: &mut Option<std_mpsc::Sender<()>>) {
    let was_active = cancel_active_dictation(state, dictation_cancel_tx);
    if was_active {
        state.voice_input_active = false;
        state.voice_input_range = None;
        state.status_line = Some(StatusMessage::info("stopped voice input"));
        state.voice_input_level = None;
    }
}

fn cancel_dictation_for_exit(
    state: &mut AppState,
    dictation_cancel_tx: &mut Option<std_mpsc::Sender<()>>,
) {
    if cancel_active_dictation(state, dictation_cancel_tx) {
        state.voice_input_active = false;
        state.voice_input_range = None;
        state.voice_input_level = None;
    }
}

fn cancel_active_dictation(
    state: &AppState,
    dictation_cancel_tx: &mut Option<std_mpsc::Sender<()>>,
) -> bool {
    if let Some(cancel_tx) = dictation_cancel_tx.take() {
        let _ = cancel_tx.send(());
    }
    state.voice_input_active
}

fn update_dictation_partial(state: &mut AppState, text: &str) {
    if !state.voice_input_active {
        return;
    }
    let range = state
        .voice_input_range
        .unwrap_or((state.input_cursor, state.input_cursor));
    state.voice_input_range = Some(replace_input_range(state, range.0, range.1, text));
    state.scroll_input_to_bottom();
    state.update_autocomplete();
    state.status_line = Some(StatusMessage::info("listening..."));
}

fn update_dictation_level(state: &mut AppState, level: f32) {
    if state.voice_input_active {
        state.voice_input_level = Some(level.clamp(0.0, 1.0));
    }
}

fn update_dictation_status(state: &mut AppState, message: String) {
    if state.voice_input_active {
        state.status_line = Some(StatusMessage::info(message));
    }
}

fn finish_dictation(state: &mut AppState, result: std::result::Result<String, String>) {
    if !state.voice_input_active {
        return;
    }
    state.voice_input_active = false;
    state.voice_input_level = None;
    match result {
        Ok(text) => {
            let range = state
                .voice_input_range
                .take()
                .unwrap_or((state.input_cursor, state.input_cursor));
            replace_input_range(state, range.0, range.1, &text);
            state.scroll_input_to_bottom();
            state.update_autocomplete();
            state.status_line = Some(StatusMessage::info("inserted voice input"));
        }
        Err(message) => {
            state.voice_input_range = None;
            state.record_status_message(StatusKind::Warning, message);
        }
    }
}

fn dictation_prompt_title(state: &AppState) -> String {
    if let Some(level) = state.voice_input_level {
        return format!(" 🎙 {} Ctrl-R stop ", voice_level_meter(Some(level)));
    }

    let message = state
        .status_line
        .as_ref()
        .filter(|status| status.kind == StatusKind::Info)
        .map(|status| status.text.as_str())
        .unwrap_or("preparing voice input...");
    format!(" 🎙 {message} Ctrl-R stop ")
}

fn normalize_paste(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                normalized.push('\n');
            }
            '\n' | '\t' => normalized.push(c),
            c if !c.is_control() => normalized.push(c),
            _ => {}
        }
    }

    normalized
}

/// Copy arbitrary text to the system clipboard and surface the result.
fn copy_text_to_clipboard(state: &mut AppState, text: &str, label: Option<&str>) {
    match copy_to_clipboard(text) {
        Ok(lease) => {
            let preview_len = text.chars().count().min(60);
            let preview: String = text.chars().take(preview_len).collect();
            let suffix = if text.chars().count() > 60 { "…" } else { "" };
            let copied = label
                .map(|label| format!("copied {label} to clipboard"))
                .unwrap_or_else(|| "copied to clipboard".to_string());
            state.record_status_message(
                StatusKind::Info,
                format!("{copied}: \"{preview}{suffix}\""),
            );
            // Store the lease to keep the clipboard handle alive on Linux/X11
            state.clipboard_lease = lease;
        }
        Err(e) => {
            state.record_status_message(StatusKind::Warning, format!("clipboard error: {e}"));
        }
    }
}

/// Copy the text of the most recent agent message to the system clipboard.
/// Records a system message so the user knows whether it worked.
fn copy_last_agent_message(state: &mut AppState) {
    let Some(text) = state.last_agent_message() else {
        state.record_status_message(StatusKind::Warning, "no agent message to copy");
        return;
    };

    copy_text_to_clipboard(state, &text, None);
}

/// `Home` jumps to the oldest line. `usize::MAX` is clamped by
/// `TranscriptScrollState::reconcile` to the actual transcript height on
/// the next draw, so we don't need to know the current line count here.
fn scroll_to_top(state: &mut AppState) {
    state.scroll_offset = usize::MAX;
}

fn scroll_to_bottom(state: &mut AppState) {
    state.scroll_offset = 0;
}

/// Ctrl-T behaviour. The fullscreen TUI can retroactively toggle all compacted
/// transcript details. Inline scrollback is immutable once flushed, so Ctrl-T
/// opens a full reader with every message and tool output expanded.
fn toggle_transcript_expansion(state: &mut AppState, mode: UiMode) {
    if mode == UiMode::InlineChat {
        state.open_transcript_viewer();
    } else {
        state.toggle_expand_transcript_details();
    }
}

/// The latest tool which is both rendered in this view and has an output body.
/// Compact completed turns omit successful tools, but failed tools remain visible.
fn latest_visible_tool_call_id(state: &AppState, compact_completed_turns: bool) -> Option<String> {
    let turns = compact_completed_turns.then(|| transcript_turns(state));
    state
        .transcript
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, entry)| {
            let (Entry::ToolCall(id) | Entry::SubagentToolCall(id)) = entry else {
                return None;
            };
            let hidden = turns.as_ref().is_some_and(|turns| {
                turns.iter().any(|turn| {
                    turn.is_compactable
                        && (turn.prompt_index..turn.end).contains(&index)
                        && tool_entry_is_successful(state, entry)
                })
            });
            (!hidden
                && state
                    .tool_calls
                    .get(id)
                    .is_some_and(|view| !view.body.is_empty()))
            .then(|| id.clone())
        })
}

fn toggle_latest_visible_tool(state: &mut AppState, compact_completed_turns: bool) {
    let Some(id) = latest_visible_tool_call_id(state, compact_completed_turns) else {
        state.status_line = Some(StatusMessage::info("no visible tool output to toggle"));
        return;
    };
    let default_expanded = !compact_completed_turns || state.expand_transcript_details;
    if !state.toggle_tool_detail(&id, default_expanded) {
        state.status_line = Some(StatusMessage::info("no visible tool output to toggle"));
    }
}

/// Keyboard handling while the inline full-transcript reader is open. The
/// reader reuses `scroll_offset` as the index of the top visible line (0 =
/// top); it is clamped to the last screen of content during draw, so adding
/// past the end and `usize::MAX` (jump to bottom) are both safe here.
fn handle_transcript_viewer_key(
    state: &mut AppState,
    modifiers: KeyModifiers,
    code: KeyCode,
    mode: UiMode,
) -> TerminalRequest {
    let closes_reader = matches!(code, KeyCode::Esc)
        || (modifiers == KeyModifiers::NONE && matches!(code, KeyCode::Char('q')))
        || (modifiers.contains(KeyModifiers::CONTROL)
            && !modifiers.intersects(
                KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
            )
            && matches!(code, KeyCode::Char('t' | 'T')));
    if closes_reader {
        state.close_transcript_viewer();
        // Shrinking the viewport back down needs an inline repair so the
        // vacated rows are cleared cleanly.
        return inline_repair_request(mode);
    }

    if modifiers == KeyModifiers::ALT && matches!(code, KeyCode::Char('t' | 'T')) {
        toggle_latest_visible_tool(state, false);
        return TerminalRequest::None;
    }

    match code {
        KeyCode::Up => state.scroll_offset = state.scroll_offset.saturating_sub(1),
        KeyCode::Down => state.scroll_offset = state.scroll_offset.saturating_add(1),
        KeyCode::PageUp => {
            state.scroll_offset = state
                .scroll_offset
                .saturating_sub(TRANSCRIPT_SCROLL_PAGE_STEP)
        }
        KeyCode::PageDown => {
            state.scroll_offset = state
                .scroll_offset
                .saturating_add(TRANSCRIPT_SCROLL_PAGE_STEP)
        }
        KeyCode::Home => state.scroll_offset = 0,
        KeyCode::End => state.scroll_offset = usize::MAX,
        _ => {}
    }
    TerminalRequest::None
}

fn latest_nested_tool_call_id(state: &AppState) -> Option<String> {
    state
        .selected_nested_agent()?
        .1
        .transcript
        .iter()
        .rev()
        .find_map(|entry| match entry {
            Entry::SubagentToolCall(id) if state.tool_calls.contains_key(id) => Some(id.clone()),
            _ => None,
        })
}

fn handle_nested_agent_viewer_key(
    state: &mut AppState,
    modifiers: KeyModifiers,
    code: KeyCode,
    mode: UiMode,
) -> TerminalRequest {
    if matches!(code, KeyCode::Esc)
        || (modifiers.is_empty() && matches!(code, KeyCode::F(11)))
        || (modifiers.is_empty() && matches!(code, KeyCode::Char('q')))
    {
        state.close_nested_agent_viewer();
        return inline_repair_request(mode);
    }

    if modifiers == KeyModifiers::ALT && matches!(code, KeyCode::Char('t' | 'T')) {
        let Some(id) = latest_nested_tool_call_id(state) else {
            state.status_line = Some(StatusMessage::info("no nested tool output to toggle"));
            return TerminalRequest::None;
        };
        if !state.toggle_tool_detail(&id, true) {
            state.status_line = Some(StatusMessage::info("no nested tool output to toggle"));
        }
        return TerminalRequest::None;
    }

    match code {
        KeyCode::Left | KeyCode::Char('p') if modifiers.is_empty() => {
            state.select_nested_agent(false)
        }
        KeyCode::Right | KeyCode::Char('n') | KeyCode::Tab if modifiers.is_empty() => {
            state.select_nested_agent(true)
        }
        KeyCode::BackTab => state.select_nested_agent(false),
        KeyCode::Up => {
            state.nested_agent_scroll_offset = state.nested_agent_scroll_offset.saturating_sub(1)
        }
        KeyCode::Down => {
            state.nested_agent_scroll_offset = state.nested_agent_scroll_offset.saturating_add(1)
        }
        KeyCode::PageUp => {
            state.nested_agent_scroll_offset = state
                .nested_agent_scroll_offset
                .saturating_sub(TRANSCRIPT_SCROLL_PAGE_STEP)
        }
        KeyCode::PageDown => {
            state.nested_agent_scroll_offset = state
                .nested_agent_scroll_offset
                .saturating_add(TRANSCRIPT_SCROLL_PAGE_STEP)
        }
        KeyCode::Home => state.nested_agent_scroll_offset = 0,
        KeyCode::End => state.nested_agent_scroll_offset = usize::MAX,
        _ => {}
    }
    TerminalRequest::None
}

fn handle_workspace_diff_viewer_key(
    state: &mut AppState,
    modifiers: KeyModifiers,
    code: KeyCode,
    mode: UiMode,
) -> TerminalRequest {
    let ctrl_g = modifiers.contains(KeyModifiers::CONTROL)
        && !modifiers.intersects(
            KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
        )
        && matches!(code, KeyCode::Char('g' | 'G'));
    if matches!(code, KeyCode::Esc) || ctrl_g {
        state.close_workspace_diff_viewer();
        return inline_repair_request(mode);
    }
    match code {
        KeyCode::Up => {
            state.workspace_diff_scroll_offset =
                state.workspace_diff_scroll_offset.saturating_sub(1)
        }
        KeyCode::Down => {
            state.workspace_diff_scroll_offset =
                state.workspace_diff_scroll_offset.saturating_add(1)
        }
        KeyCode::PageUp => {
            state.workspace_diff_scroll_offset = state
                .workspace_diff_scroll_offset
                .saturating_sub(TRANSCRIPT_SCROLL_PAGE_STEP)
        }
        KeyCode::PageDown => {
            state.workspace_diff_scroll_offset = state
                .workspace_diff_scroll_offset
                .saturating_add(TRANSCRIPT_SCROLL_PAGE_STEP)
        }
        KeyCode::Home => state.workspace_diff_scroll_offset = 0,
        KeyCode::End => state.workspace_diff_scroll_offset = usize::MAX,
        KeyCode::Char('n') if modifiers.is_empty() => state.select_workspace_diff_file(true),
        KeyCode::Char('p') if modifiers.is_empty() => state.select_workspace_diff_file(false),
        _ => {}
    }
    TerminalRequest::None
}

fn is_help_key(modifiers: KeyModifiers, code: KeyCode) -> bool {
    modifiers.is_empty() && matches!(code, KeyCode::F(10))
}

fn open_help_overlay(state: &mut AppState) {
    state.help_overlay = true;
    state.help_scroll = 0;
}

fn scroll_help_overlay(state: &mut AppState, code: KeyCode) {
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            state.help_scroll = state.help_scroll.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.help_scroll = state.help_scroll.saturating_add(1);
        }
        KeyCode::PageUp => {
            state.help_scroll = state.help_scroll.saturating_sub(HELP_SCROLL_PAGE_STEP);
        }
        KeyCode::PageDown => {
            state.help_scroll = state.help_scroll.saturating_add(HELP_SCROLL_PAGE_STEP);
        }
        KeyCode::Home => state.help_scroll = 0,
        KeyCode::End => state.help_scroll = u16::MAX,
        _ => {}
    }
}

fn is_text_selection_key(modifiers: KeyModifiers, code: KeyCode) -> bool {
    modifiers.is_empty() && matches!(code, KeyCode::F(12))
}

fn can_toggle_text_selection_mode(state: &AppState) -> bool {
    !state.help_overlay
        && !state.has_pending_permission()
        && !state.has_pending_elicitation()
        && state.config_picker.is_none()
}

#[cfg(target_os = "macos")]
const PROMPT_NEWLINE_HINT: &str = "Ctrl-J";

#[cfg(not(target_os = "macos"))]
const PROMPT_NEWLINE_HINT: &str = "Shift/Alt+Enter";

fn is_prompt_newline_key(modifiers: KeyModifiers, code: KeyCode) -> bool {
    // Shift+Enter requires keyboard enhancement support; Alt+Enter is
    // reported only when the terminal treats Alt/Option as a modifier.
    matches!(
        (modifiers, code),
        (KeyModifiers::SHIFT, KeyCode::Enter) | (KeyModifiers::ALT, KeyCode::Enter)
    ) || (modifiers == KeyModifiers::CONTROL && matches!(code, KeyCode::Char('j')))
}

fn should_open_help(modifiers: KeyModifiers, code: KeyCode) -> bool {
    modifiers.is_empty() && matches!(code, KeyCode::F(10))
}

fn input_text_with_attachments(input: &str, attachments: &[PastedAttachment]) -> String {
    let input_len = input_char_count(input);
    let mut ordered: Vec<&PastedAttachment> = attachments.iter().collect();
    ordered.sort_by_key(|attachment| (attachment.position.min(input_len), attachment.id));

    let mut combined = String::new();
    let mut text_start = 0usize;
    for attachment in ordered {
        let position = attachment.position.min(input_len);
        combined.push_str(input_char_slice(input, text_start, position));
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&attachment.content);
        if position < input_len && !combined.ends_with('\n') {
            combined.push('\n');
        }
        text_start = position;
    }
    combined.push_str(input_char_slice(input, text_start, input_len));
    combined
}

fn submit_prompt(state: &mut AppState, cmd_tx: &mpsc::UnboundedSender<UiCommand>) {
    let combined = input_text_with_attachments(&state.input, &state.attachments);

    let input_len = input_char_count(&state.input);
    let mut ordered_images: Vec<&PastedImageAttachment> = state.image_attachments.iter().collect();
    ordered_images.sort_by_key(|attachment| (attachment.position.min(input_len), attachment.id));
    let images: Vec<PromptImage> = ordered_images
        .into_iter()
        .map(|attachment| PromptImage {
            data_base64: attachment.data_base64.clone(),
            mime_type: attachment.mime_type.clone(),
            width: attachment.width,
            height: attachment.height,
        })
        .collect();

    let text = combined.trim().to_string();
    if text.is_empty() && images.is_empty() {
        return;
    }

    // Client-side commands are handled here without forwarding anything
    // to the agent.
    if images.is_empty() && text == "/new" {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        state.exit_reason = Some(UiExitReason::NewSession);
        return;
    }

    if images.is_empty() && text == "/clear" {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        state.exit_reason = Some(UiExitReason::ClearSession);
        return;
    }

    if images.is_empty() && text == "/compact" {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        let _ = cmd_tx.send(UiCommand::CompactPrimary);
        state.record_status_message(StatusKind::Info, "compacting agent context…");
        return;
    }

    if images.is_empty() && text == "/load" {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        state.exit_reason = Some(UiExitReason::LoadSession);
        return;
    }

    if images.is_empty() && text == "/mjconfig" {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        state.open_mjconfig_menu();
        return;
    }

    if images.is_empty()
        && let Some(argument) = models_slash_argument(&text)
    {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        match argument {
            "" => state.open_mjconfig_menu(),
            "refresh" => match crate::roster::invalidate_model_cache() {
                Ok(()) => state.record_status_message(
                    StatusKind::Info,
                    "model cache cleared; /new or /clear will reprobe enabled ACP adapters",
                ),
                Err(error) => state.record_status_message(
                    StatusKind::Warning,
                    format!("failed to clear model cache: {error:#}"),
                ),
            },
            _ => state.record_status_message(StatusKind::Warning, "usage: /models [refresh]"),
        }
        return;
    }

    if images.is_empty() && text == "/agents" {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        state.push_system_message(active_models_and_usage_report(state));
        return;
    }

    if images.is_empty()
        && let Some(rest) = text.strip_prefix("/review")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        if state.is_side {
            state.record_status_message(
                StatusKind::Warning,
                "manual review is unavailable in side conversations",
            );
        } else if state.runtime_closed || state.session_id.is_none() {
            state.record_status_message(StatusKind::Warning, "the primary agent is unavailable");
        } else if state.is_busy() {
            state.record_status_message(
                StatusKind::Warning,
                "manual review is only available while the primary agent is idle",
            );
        } else if rest.trim().is_empty() {
            state.open_review_picker();
        } else if let Some(target) = parse_review_target(rest.trim()) {
            state.record_status_message(StatusKind::Info, "preparing manual review…");
            let _ = cmd_tx.send(UiCommand::RunReview { target });
        } else {
            state.record_status_message(
                StatusKind::Warning,
                "usage: /review [recent|uncommitted|head]",
            );
        }
        return;
    }

    if images.is_empty() && matches!(text.as_str(), "/export" | "/export full") {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        let include_nested = text == "/export full";
        match export_transcript(state, include_nested) {
            Ok(path) => state.record_status_message(
                StatusKind::Info,
                format!(
                    "{} transcript exported to {}",
                    if include_nested { "full" } else { "primary" },
                    path.display()
                ),
            ),
            Err(e) => state.record_status_message(
                StatusKind::Warning,
                format!("transcript export failed: {e:#}"),
            ),
        }
        return;
    }

    if images.is_empty() && text == "/subagents" {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        if !state.open_nested_agent_viewer() {
            state.record_status_message(StatusKind::Info, "no nested agents to inspect");
        }
        return;
    }

    if images.is_empty()
        && let Some(rest) = text.strip_prefix("/side")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        if state.is_side {
            state.record_status_message(
                StatusKind::Warning,
                "nested side conversations are not supported",
            );
        } else if state.runtime_closed {
            state.record_status_message(StatusKind::Warning, "the ACP runtime is closed");
        } else if state.session_id.is_none() {
            state.announce_waiting_for_primary();
        } else if !state.side_session_supported {
            state.record_status_message(
                StatusKind::Warning,
                state
                    .side_session_unsupported_reason
                    .clone()
                    .unwrap_or_else(|| {
                        "side conversations are not supported by this agent".to_string()
                    }),
            );
        } else if state.side_start_requested {
            state.record_status_message(StatusKind::Info, "side conversation is already opening");
        } else {
            state.side_start_requested = true;
            state.side_initial_question =
                (!rest.trim().is_empty()).then(|| rest.trim().to_string());
        }
        return;
    }

    if images.is_empty() && text == "/fork" {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        if state.runtime_closed {
            state.record_status_message(
                StatusKind::Info,
                "acp runtime closed; type /clear for the same agent, /new for the picker, or Ctrl-C to quit",
            );
        } else if state.session_id.is_none() {
            state.announce_waiting_for_primary();
        } else if !state.session_fork_supported {
            state.record_status_message(
                StatusKind::Warning,
                "session fork is not supported by this agent (unstable ACP extension not advertised)",
            );
        } else if state.is_busy() {
            state.record_status_message(
                StatusKind::Warning,
                "session fork is only supported while idle",
            );
        } else {
            state.mark_forking();
            state.record_status_message(StatusKind::Info, "forking session...");
            let _ = cmd_tx.send(UiCommand::ForkSession);
        }
        return;
    }

    if images.is_empty()
        && let Some(rest) = text.strip_prefix("/ragnarok")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        let task = rest.trim().to_string();
        state.record_prompt_history(text.clone());
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        if state.ragnarok.is_some() {
            state.record_status_message(
                StatusKind::Warning,
                "a Ragnarok is already raging (press q twice in the arena to quit)",
            );
        } else if task.is_empty() {
            state
                .record_status_message(StatusKind::Warning, "usage: /ragnarok <task to implement>");
        } else {
            state.push_system_message(format!("⚡ Ragnarok summoned — task: {task}"));
            state.request_ragnarok(task);
        }
        return;
    }

    if images.is_empty()
        && let Some(rest) = text.strip_prefix("/mj:")
    {
        let other = rest.trim();
        state.record_status_message(
            StatusKind::Warning,
            format!("unknown mj command: /mj:{other}"),
        );
        return;
    }

    if state.runtime_closed {
        state.record_status_message(
            StatusKind::Info,
            "acp runtime closed; type /clear for the same agent, /new for the picker, or Ctrl-C to quit",
        );
        return;
    }
    if state.session_id.is_none() {
        if state.has_startup_prompt() {
            state.announce_waiting_for_primary();
            return;
        }

        let prompt = QueuedPrompt {
            text: text.clone(),
            images: images.clone(),
            display_text: prompt_display_text(&text, images.len()),
        };
        if cmd_tx.send(UiCommand::SendPrompt { text, images }).is_err() {
            state.record_status_message(
                StatusKind::Warning,
                "the ACP runtime closed before the startup prompt could be queued",
            );
            return;
        }
        debug_assert!(state.stage_startup_prompt(prompt));
        state.announce_waiting_for_primary();
        return;
    }

    let display_text = prompt_display_text(&text, images.len());
    state.input.clear();
    clear_attachments(state);
    state.input_cursor = 0;
    state.scroll_input_to_bottom();

    if state.is_busy() {
        // The previous turn is still running. Stash this submission and
        // keep it out of the transcript until it is actually sent.
        let preview = queued_prompt_preview(&display_text);
        state.push_queued_prompt(QueuedPrompt {
            text,
            images,
            display_text,
        });
        let queued = state.queued_prompt_count();
        state.status_line = Some(StatusMessage::info(format!("queued {queued}: {preview}")));
        return;
    }

    state.record_user_prompt(display_text);
    let _ = cmd_tx.send(UiCommand::SendPrompt { text, images });
}

fn parse_review_target(value: &str) -> Option<ReviewTarget> {
    match value.to_ascii_lowercase().as_str() {
        "recent" => Some(ReviewTarget::Recent),
        "uncommitted" => Some(ReviewTarget::Uncommitted),
        "head" => Some(ReviewTarget::Head),
        _ => None,
    }
}

fn models_slash_argument(value: &str) -> Option<&str> {
    let rest = value.strip_prefix("/models")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim())
}

fn handle_mjconfig_menu_key(
    state: &mut AppState,
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    modifiers: KeyModifiers,
    code: KeyCode,
    mode: UiMode,
) -> TerminalRequest {
    if modifiers == KeyModifiers::CONTROL && code == KeyCode::Char('c') {
        state.mjconfig_menu_cancel();
        return inline_repair_request(mode);
    }
    match state.mjconfig_menu_key(code) {
        SettingsAction::Cancel => {
            state.mjconfig_menu_cancel();
            inline_repair_request(mode)
        }
        SettingsAction::Save => {
            if let Some(config) = state.mjconfig_menu_accept() {
                persist_mjconfig_selection(state, cmd_tx, config);
            }
            inline_repair_request(mode)
        }
        SettingsAction::Authenticate(vendor) => {
            if state.is_busy() || state.has_pending_permission() || state.has_pending_elicitation()
            {
                if let Some(menu) = state.mjconfig_menu.as_mut() {
                    menu.editor.notice =
                        Some("Wait for the current turn or prompt before signing in".to_string());
                }
                TerminalRequest::None
            } else if vendor == crate::auth::AuthVendor::OpenAi
                && crate::auth::executable(vendor).is_none()
            {
                if let Some(menu) = state.mjconfig_menu.as_mut() {
                    menu.editor.notice = Some(format!(
                        "{} CLI is not installed; run `{}`",
                        vendor.label(),
                        crate::auth::install_hint(vendor)
                    ));
                }
                TerminalRequest::None
            } else {
                TerminalRequest::Authenticate(vendor)
            }
        }
        SettingsAction::None | SettingsAction::Changed => TerminalRequest::None,
    }
}

/// Persist the shared settings selection and apply review switches immediately.
fn persist_mjconfig_selection(
    state: &mut AppState,
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    mut config: config::Config,
) {
    let theme = config.theme;
    let style = config.spinner;
    let review_changed = state.review_enabled != config.agent.discrete_review;
    let live_session_updates = live_primary_session_config_updates(state, &config);
    if let Some(path) = state.config_path.clone() {
        match config::save_user_config_preserving_session_routes(&path, &mut config) {
            Ok(()) => {
                state.configured_models = config.model_names();
                state.acp_inventory =
                    crate::roster::rediscover_inventory(&config, &state.acp_inventory);
                state.review_enabled = config.agent.discrete_review;
                if review_changed {
                    let _ = cmd_tx.send(UiCommand::SetReviewPolicy {
                        enabled: config.agent.discrete_review,
                    });
                }
                for (target, value) in live_session_updates {
                    let _ = cmd_tx.send(UiCommand::SetSessionConfigOption { target, value });
                }
                state.record_status_message(
                    StatusKind::Info,
                    format!("config saved — theme {theme}, spinner {style}; session options update the active primary, while model, permission, and ACP routing changes apply on /new or /clear"),
                );
            }
            Err(e) => state.record_status_message(
                StatusKind::Warning,
                format!("config changed but save failed: {e:#}"),
            ),
        }
    } else {
        state.record_status_message(StatusKind::Info, format!("theme {theme}, spinner {style}"));
    }
}

fn live_primary_session_config_updates(
    state: &AppState,
    config: &config::Config,
) -> Vec<(SessionConfigTarget, SessionConfigValueId)> {
    if state.session_id.is_none() {
        return Vec::new();
    }
    let Some(source_id) = state.active_models.primary_source.as_deref() else {
        return Vec::new();
    };
    let Some(defaults) = config
        .session_config
        .get(source_id)
        .map(|saved| &saved.defaults)
    else {
        return Vec::new();
    };

    state
        .session_config_options
        .iter()
        .zip(state.session_config_targets.iter())
        .filter_map(|(option, target)| {
            let desired = defaults.get(&crate::acp::session_config_option_key(&option.id))?;
            let current = crate::app::config_option_current_value_id(option)?;
            let desired_value = SessionConfigValueId::from(desired.clone());
            if !crate::acp::session_config_option_contains_value(option, &desired_value) {
                return None;
            }
            (current.to_string() != *desired).then(|| (target.clone(), desired_value))
        })
        .collect()
}

fn draw_mjconfig_menu(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let Some(menu) = state.mjconfig_menu.as_ref() else {
        return;
    };
    draw_settings_panel(f, area, &menu.editor, "mj config");
}

fn export_transcript(state: &AppState, include_nested: bool) -> Result<PathBuf> {
    let Some(dir) = &state.transcript_export_dir else {
        anyhow::bail!("transcript export directory is not configured");
    };
    create_private_export_dir(dir)?;
    let body = transcript_export_markdown_with_nested(state, include_nested);
    for suffix in 0..1000 {
        let path = export_path(dir, export_timestamp_millis(), suffix);
        match write_private_new_file(&path, body.as_bytes()) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("write transcript export {}", path.display()));
            }
        }
    }
    anyhow::bail!("could not allocate unique transcript export filename")
}

fn export_path(dir: &Path, timestamp_millis: u128, suffix: u16) -> PathBuf {
    let suffix = if suffix == 0 {
        String::new()
    } else {
        format!("-{suffix}")
    };
    dir.join(format!("mjolnir-transcript-{timestamp_millis}{suffix}.md"))
}

fn export_timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn create_private_export_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create transcript export directory {}", dir.display()))?;
    #[cfg(unix)]
    {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).with_context(
            || {
                format!(
                    "set transcript export directory permissions {}",
                    dir.display()
                )
            },
        )?;
    }
    Ok(())
}

fn write_private_new_file(path: &Path, body: &[u8]) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(body)
}

#[cfg(test)]
fn transcript_export_markdown(state: &AppState) -> String {
    transcript_export_markdown_with_nested(state, false)
}

fn transcript_export_markdown_with_nested(state: &AppState, include_nested: bool) -> String {
    let mut out = String::from("# Mjolnir Transcript\n\n");
    if let Some(title) = &state.session_title {
        out.push_str(&format!("- Session: {}\n", escape_markdown_text(title)));
    }
    if let Some(id) = &state.session_id {
        out.push_str(&format!("- Session ID: {}\n", escape_markdown_text(id)));
    }
    if !state.agent_label.is_empty() {
        out.push_str(&format!(
            "- Agent: {}\n",
            escape_markdown_text(&state.agent_label)
        ));
    }
    out.push('\n');

    push_export_entries(&mut out, &state.transcript, state);

    if include_nested && state.nested_agents().next().is_some() {
        out.push_str("# Nested Agent Transcripts\n\n");
        for (subagent_id, actor) in state.nested_agents() {
            let role = actor
                .role
                .as_ref()
                .map(crate::app::nested_role_label)
                .unwrap_or_else(|| "subagent".to_string());
            let backend = nested_actor_backend(actor);
            let state_label = nested_actor_state_label(actor);
            out.push_str(&format!(
                "## Subagent #{}: {}\n\n- Role: {}\n- Backend: {}\n- State: {}\n",
                subagent_id,
                escape_markdown_text(&actor.label),
                escape_markdown_text(&role),
                escape_markdown_text(&backend),
                escape_markdown_text(&state_label),
            ));
            if !actor.objective.is_empty() {
                out.push_str(&format!(
                    "- Objective: {}\n",
                    escape_markdown_text(&actor.objective)
                ));
            }
            out.push('\n');
            if let Some(history) = actor.archived_history_markdown() {
                out.push_str(&history);
                if !history.ends_with('\n') {
                    out.push('\n');
                }
            }
            push_export_entries(&mut out, &actor.transcript, state);
        }
    }

    out
}

fn push_export_entries(out: &mut String, entries: &[Entry], state: &AppState) {
    for entry in entries {
        match entry {
            Entry::UserPrompt(text) => push_export_text(out, "You", text),
            Entry::AgentMessage(text) => push_export_text(out, "Agent", text),
            Entry::AgentThought(thought) => push_export_text(out, "Thought", &thought.text),
            Entry::SubagentMessage(text) => push_export_text(out, "subagent", text),
            Entry::SubagentThought(thought) => {
                push_export_text(out, "subagent Thought", &thought.text)
            }
            Entry::InternalMessage(message) => {
                let heading = match message.kind {
                    crate::event::InternalMessageKind::Delegation => {
                        format!("{} → {} delegation", message.source, message.target)
                    }
                    crate::event::InternalMessageKind::DiscreteReview => {
                        format!("{} discrete review", message.source)
                    }
                    crate::event::InternalMessageKind::ReviewLane => {
                        format!("{} review lane", message.source)
                    }
                    crate::event::InternalMessageKind::ReviewProgress => {
                        format!("{} review progress", message.source)
                    }
                    crate::event::InternalMessageKind::ReviewSynthesis => {
                        format!("{} review synthesis", message.source)
                    }
                };
                push_export_text(out, &heading, &message.text);
            }
            Entry::System(text) => push_export_text(out, "System", text),
            Entry::SessionBoundary(text) => push_export_text(out, "Session", text),
            Entry::Plan(entries) | Entry::SubagentPlan(entries) => {
                let heading = if matches!(entry, Entry::SubagentPlan(_)) {
                    "## subagent Plan\n\n"
                } else {
                    "## Plan\n\n"
                };
                out.push_str(heading);
                for entry in entries {
                    out.push_str(&format!(
                        "- {} / {}: {}\n",
                        plan_priority_label(&entry.priority),
                        plan_status_label(&entry.status),
                        escape_markdown_text(&entry.content)
                    ));
                }
                out.push('\n');
            }
            Entry::ToolCall(id) | Entry::SubagentToolCall(id) => {
                if let Some(view) = state.tool_calls.get(id) {
                    let label = if matches!(entry, Entry::SubagentToolCall(_)) {
                        "subagent Tool"
                    } else {
                        "Tool"
                    };
                    out.push_str(&format!(
                        "## {label}: {}\n\n- Kind: {}\n- Status: {}\n\n",
                        escape_markdown_text(&view.title),
                        tool_kind_label(view.kind),
                        tool_status_label(view.status)
                    ));
                    for output in &view.body {
                        push_export_tool_output(out, output, view.status);
                    }
                }
            }
        }
    }
}

pub(crate) fn nested_actor_history_markdown(state: &AppState, actor: &SubagentStatus) -> String {
    let mut out = String::new();
    push_export_entries(&mut out, &actor.transcript, state);
    out
}

fn push_export_text(out: &mut String, heading: &str, text: &str) {
    out.push_str(&format!("## {heading}\n\n"));
    out.push_str(&escape_markdown_text(text));
    out.push_str("\n\n");
}

fn push_export_tool_output(out: &mut String, output: &ToolCallOutput, tool_status: ToolCallStatus) {
    match output {
        ToolCallOutput::Text(text) => push_export_fence(out, text),
        ToolCallOutput::Diff {
            path,
            old_text: _,
            new_text,
        } => {
            out.push_str(&format!("### Diff: {}\n\n", escape_markdown_text(path)));
            // Exports the post-edit content for compact before/after review.
            push_export_fence(out, new_text);
        }
        ToolCallOutput::Terminal {
            output,
            truncated,
            exit_status,
            ..
        } => {
            out.push_str("### Terminal output\n\n");
            if *truncated {
                out.push_str("_Output truncated._\n\n");
            }
            if !output.trim().is_empty() {
                push_export_fence(out, output);
            } else if exit_status.is_some() {
                out.push_str("_No stdout/stderr captured._\n\n");
            } else {
                out.push_str(&format!(
                    "_{}._\n\n",
                    terminal_empty_state_label(tool_status)
                ));
            }
            if let Some(status) = exit_status {
                out.push_str(&format!(
                    "Exit status: {}\n\n",
                    terminal_exit_status_label(status)
                ));
            }
        }
        ToolCallOutput::Note(note) => {
            out.push_str(&format!("_Note: {}_\n\n", escape_markdown_text(note)));
        }
    }
}

fn push_export_fence(out: &mut String, text: &str) {
    let fence = "`".repeat(longest_backtick_run(text).saturating_add(1).max(3));
    out.push_str(&fence);
    out.push_str("text\n");
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&fence);
    out.push_str("\n\n");
}

fn longest_backtick_run(text: &str) -> usize {
    let mut best = 0;
    let mut current = 0;
    for ch in text.chars() {
        if ch == '`' {
            current += 1;
            best = best.max(current);
        } else {
            current = 0;
        }
    }
    best
}

fn escape_markdown_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(
            ch,
            '\\' | '`'
                | '*'
                | '_'
                | '{'
                | '}'
                | '['
                | ']'
                | '('
                | ')'
                | '#'
                | '+'
                | '-'
                | '.'
                | '!'
                | '|'
                | '>'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn plan_priority_label(
    priority: &agent_client_protocol::schema::v1::PlanEntryPriority,
) -> &'static str {
    match priority {
        agent_client_protocol::schema::v1::PlanEntryPriority::High => "high",
        agent_client_protocol::schema::v1::PlanEntryPriority::Medium => "medium",
        agent_client_protocol::schema::v1::PlanEntryPriority::Low => "low",
        _ => "unknown",
    }
}

fn plan_status_label(status: &agent_client_protocol::schema::v1::PlanEntryStatus) -> &'static str {
    match status {
        agent_client_protocol::schema::v1::PlanEntryStatus::Pending => "pending",
        agent_client_protocol::schema::v1::PlanEntryStatus::InProgress => "running",
        agent_client_protocol::schema::v1::PlanEntryStatus::Completed => "done",
        _ => "unknown",
    }
}

fn plan_status_style(
    status: &agent_client_protocol::schema::v1::PlanEntryStatus,
    theme: TerminalTheme,
) -> Style {
    let color = match status {
        agent_client_protocol::schema::v1::PlanEntryStatus::Pending => theme.muted,
        agent_client_protocol::schema::v1::PlanEntryStatus::InProgress => theme.primary,
        agent_client_protocol::schema::v1::PlanEntryStatus::Completed => theme.success,
        _ => theme.error,
    };
    Style::default().fg(color)
}

fn plan_row(
    entry: &agent_client_protocol::schema::v1::PlanEntry,
    theme: TerminalTheme,
) -> Line<'static> {
    use agent_client_protocol::schema::v1::{PlanEntryPriority, PlanEntryStatus};

    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            format!("[{}]", plan_status_label(&entry.status)),
            plan_status_style(&entry.status, theme),
        ),
    ];
    match entry.priority {
        PlanEntryPriority::Medium => {}
        PlanEntryPriority::High => spans.push(Span::styled(
            format!(" [{}]", plan_priority_label(&entry.priority)),
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        )),
        PlanEntryPriority::Low => spans.push(Span::styled(
            format!(" [{}]", plan_priority_label(&entry.priority)),
            Style::default().fg(theme.muted),
        )),
        _ => spans.push(Span::styled(
            format!(" [{}]", plan_priority_label(&entry.priority)),
            Style::default().fg(theme.error),
        )),
    }
    let content_style = if matches!(entry.status, PlanEntryStatus::Completed) {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    };
    spans.push(Span::raw(" "));
    spans.push(Span::styled(entry.content.clone(), content_style));
    Line::from(spans)
}

/// Re-issue a previously queued prompt now that the in-flight turn has
/// finished. This fires after either a natural `PromptDone` or a
/// `PromptDone(Cancelled)` from Ctrl-C.
/// Mirrors the final dispatch in `submit_prompt`. No-ops if nothing is
/// queued, the runtime closed, or another turn already started (e.g.
/// because the user typed another prompt between two `PromptDone`
/// events — handled by the next drain).
fn drain_queued_prompt(state: &mut AppState, cmd_tx: &mpsc::UnboundedSender<UiCommand>) {
    if state.is_busy() || state.runtime_closed || state.session_id.is_none() {
        return;
    }
    let Some(queued) = state.take_queued_prompt() else {
        return;
    };
    state.record_user_prompt(queued.display_text);
    let _ = cmd_tx.send(UiCommand::SendPrompt {
        text: queued.text,
        images: queued.images,
    });
}

/// Commit the prompt already queued on the runtime channel once the primary
/// ACP session is ready. The editor remains recoverable if startup fails. If
/// the user edited the retained draft meanwhile, preserve those edits as the
/// next prompt instead of erasing them.
fn finalize_startup_prompt(state: &mut AppState) {
    if state.runtime_closed || state.session_id.is_none() {
        return;
    }
    let Some(prompt) = state.take_startup_prompt() else {
        return;
    };

    let current_text = input_text_with_attachments(&state.input, &state.attachments)
        .trim()
        .to_string();
    let input_len = input_char_count(&state.input);
    let mut ordered_images: Vec<&PastedImageAttachment> = state.image_attachments.iter().collect();
    ordered_images.sort_by_key(|attachment| (attachment.position.min(input_len), attachment.id));
    let current_images: Vec<PromptImage> = ordered_images
        .into_iter()
        .map(|attachment| PromptImage {
            data_base64: attachment.data_base64.clone(),
            mime_type: attachment.mime_type.clone(),
            width: attachment.width,
            height: attachment.height,
        })
        .collect();
    if current_text == prompt.text && current_images == prompt.images {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
    }

    state.record_user_prompt(prompt.display_text);
}

/// Truncate the display text to a short single-line preview for the
/// queued-prompt indicator. Newlines collapse to spaces.
fn queued_prompt_preview(display_text: &str) -> String {
    let flat: String = display_text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = flat.trim();
    if trimmed.chars().count() <= QUEUED_PROMPT_PREVIEW_WIDTH {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(QUEUED_PROMPT_PREVIEW_WIDTH).collect();
        format!("{head}...")
    }
}

fn prompt_display_text(text: &str, image_count: usize) -> String {
    let mut display = text.to_string();
    for _ in 0..image_count {
        if !display.is_empty() {
            display.push('\n');
        }
        display.push_str("[image]");
    }
    display
}

fn clamp_permission_selected(selected: usize, option_count: usize) -> usize {
    selected.min(option_count.saturating_sub(1))
}

fn handle_permission_key(state: &mut AppState, code: KeyCode, mode: UiMode) -> TerminalRequest {
    let Some(pending) = state.pending_permission_mut() else {
        return TerminalRequest::None;
    };
    let len = pending.prompt.options.len().max(1);
    pending.selected = clamp_permission_selected(pending.selected, pending.prompt.options.len());
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            if pending.selected == 0 {
                pending.selected = len - 1;
            } else {
                pending.selected -= 1;
            }
            pending.scroll_offset = None;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            pending.selected = (pending.selected + 1) % len;
            pending.scroll_offset = None;
        }
        KeyCode::PageUp => {
            let current = pending.scroll_offset.unwrap_or(0);
            pending.scroll_offset = Some(current.saturating_sub(5));
        }
        KeyCode::PageDown => {
            let current = pending.scroll_offset.unwrap_or(0);
            pending.scroll_offset = Some(current.saturating_add(5));
        }
        KeyCode::Home => {
            pending.scroll_offset = Some(0);
        }
        KeyCode::End => {
            pending.scroll_offset = Some(usize::MAX);
        }
        KeyCode::Enter => {
            let pending = state.take_pending_permission().expect("checked above");
            let PendingPermission {
                prompt, selected, ..
            } = pending;
            let decision = prompt
                .options
                .get(selected)
                .map(|o| PermissionDecision::Selected(o.option_id.to_string()))
                .unwrap_or(PermissionDecision::Cancelled);
            let _ = prompt.responder.send(decision);
            state.update_autocomplete();
            return inline_repair_request(mode);
        }
        KeyCode::Esc => {
            let pending = state.take_pending_permission().expect("checked above");
            let _ = pending.prompt.responder.send(PermissionDecision::Cancelled);
            state.update_autocomplete();
            return inline_repair_request(mode);
        }
        _ => {}
    }
    TerminalRequest::None
}

/// Keyboard handler for the elicitation modal. Up/Down move an option cursor,
/// Space toggles multi-select choices, and Enter advances/submits the form.
/// PgUp/PgDn/Home/End scroll content taller than the modal.
fn handle_elicitation_key(state: &mut AppState, code: KeyCode, mode: UiMode) -> TerminalRequest {
    let Some(view) = state.elicitation_view() else {
        return TerminalRequest::None;
    };
    // A free-text field captures typed characters first -- including `j`/`k`,
    // which are option-navigation keys for single-select views. Editing is
    // append/backspace at the end of the buffer.
    if state.elicitation_accepts_text_input() {
        match code {
            KeyCode::Char(c) => {
                if let Some(pending) = state.pending_elicitation_mut() {
                    pending.input.push(c);
                }
            }
            KeyCode::Backspace => {
                if let Some(pending) = state.pending_elicitation_mut() {
                    pending.input.pop();
                }
            }
            KeyCode::Enter => {
                state.resolve_elicitation_accept();
                return inline_repair_request(mode);
            }
            KeyCode::Esc => {
                state.resolve_elicitation_dismiss();
                return inline_repair_request(mode);
            }
            KeyCode::PageUp => {
                if let Some(pending) = state.pending_elicitation_mut() {
                    let current = pending.scroll_offset.unwrap_or(0);
                    pending.scroll_offset = Some(current.saturating_sub(5));
                }
            }
            KeyCode::PageDown => {
                if let Some(pending) = state.pending_elicitation_mut() {
                    let current = pending.scroll_offset.unwrap_or(0);
                    pending.scroll_offset = Some(current.saturating_add(5));
                }
            }
            KeyCode::Home => {
                if let Some(pending) = state.pending_elicitation_mut() {
                    pending.scroll_offset = Some(0);
                }
            }
            KeyCode::End => {
                if let Some(pending) = state.pending_elicitation_mut() {
                    pending.scroll_offset = Some(usize::MAX);
                }
            }
            _ => {}
        }
        return TerminalRequest::None;
    }
    match code {
        KeyCode::PageUp => {
            if let Some(pending) = state.pending_elicitation_mut() {
                let current = pending.scroll_offset.unwrap_or(0);
                pending.scroll_offset = Some(current.saturating_sub(5));
            }
        }
        KeyCode::PageDown => {
            if let Some(pending) = state.pending_elicitation_mut() {
                let current = pending.scroll_offset.unwrap_or(0);
                pending.scroll_offset = Some(current.saturating_add(5));
            }
        }
        KeyCode::Home => {
            if let Some(pending) = state.pending_elicitation_mut() {
                pending.scroll_offset = Some(0);
            }
        }
        KeyCode::End => {
            if let Some(pending) = state.pending_elicitation_mut() {
                pending.scroll_offset = Some(usize::MAX);
            }
        }
        KeyCode::Char('c' | 'C') if matches!(view, ElicitationView::Url { .. }) => {
            if let ElicitationView::Url { url } = view {
                return TerminalRequest::CopyText(url);
            }
        }
        KeyCode::Char(' ')
            if matches!(
                view,
                ElicitationView::Form { ref fields, .. }
                    if state
                        .pending_elicitation()
                        .and_then(|pending| fields.get(pending.form_field))
                        .is_some_and(|field| matches!(field.kind, ElicitationFormFieldKind::MultiSelect { .. }))
            ) =>
        {
            state.elicitation_multi_toggle();
        }
        // No-op for URL / unsupported views (they have no selectable options).
        KeyCode::Up | KeyCode::Char('k') => state.elicitation_select_move(-1),
        KeyCode::Down | KeyCode::Char('j') => state.elicitation_select_move(1),
        KeyCode::Enter => {
            state.resolve_elicitation_accept();
            return inline_repair_request(mode);
        }
        KeyCode::Esc => {
            state.resolve_elicitation_dismiss();
            return inline_repair_request(mode);
        }
        _ => {}
    }
    TerminalRequest::None
}

fn inline_repair_request(mode: UiMode) -> TerminalRequest {
    if mode == UiMode::InlineChat {
        TerminalRequest::ForceInlineRepair
    } else {
        TerminalRequest::None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerKeyAction {
    Cancel,
    Accept,
    Move(i32),
    Other,
}

fn picker_key_action(modifiers: KeyModifiers, code: KeyCode) -> PickerKeyAction {
    match (modifiers, code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) | (_, KeyCode::Esc) => PickerKeyAction::Cancel,
        (_, KeyCode::Enter) => PickerKeyAction::Accept,
        (_, KeyCode::Up) | (_, KeyCode::Char('k')) => PickerKeyAction::Move(-1),
        (_, KeyCode::Down) | (_, KeyCode::Char('j')) => PickerKeyAction::Move(1),
        _ => PickerKeyAction::Other,
    }
}

fn handle_agent_picker_key(
    state: &mut AppState,
    modifiers: KeyModifiers,
    code: KeyCode,
    mode: UiMode,
) -> TerminalRequest {
    let Some(step) = state.agent_picker.as_ref().map(|picker| picker.step) else {
        return TerminalRequest::None;
    };
    let action = match code {
        KeyCode::BackTab => PickerKeyAction::Move(-1),
        KeyCode::Tab => PickerKeyAction::Move(1),
        _ => picker_key_action(modifiers, code),
    };
    match action {
        PickerKeyAction::Cancel => {
            if !state.agent_picker_back() {
                state.agent_picker = None;
            }
            inline_repair_request(mode)
        }
        PickerKeyAction::Accept => {
            match step {
                AgentPickerStep::ConfirmSave => {
                    if let Some((role, effort)) = state.agent_picker_selection()
                        && persist_primary_picker_selection(state, role, effort)
                        && let Some(picker) = state.agent_picker.as_mut()
                    {
                        picker.step = AgentPickerStep::StartNewSession;
                    }
                }
                AgentPickerStep::StartNewSession => {
                    let start_new_session = state
                        .agent_picker
                        .as_ref()
                        .is_some_and(|picker| picker.start_new_session);
                    state.agent_picker = None;
                    if start_new_session {
                        state.exit_reason = Some(UiExitReason::NewSession);
                    } else {
                        state.record_status_message(
                            StatusKind::Info,
                            "model and effort saved; start /new or /clear when ready",
                        );
                    }
                }
                AgentPickerStep::Model | AgentPickerStep::Effort => {
                    state.agent_picker_advance();
                }
            }
            inline_repair_request(mode)
        }
        PickerKeyAction::Move(delta) => {
            match step {
                AgentPickerStep::Model => state.agent_picker_move(delta),
                AgentPickerStep::Effort => state.agent_picker_move_effort(delta),
                AgentPickerStep::StartNewSession => state.agent_picker_toggle_start_new_session(),
                AgentPickerStep::ConfirmSave => {}
            }
            TerminalRequest::None
        }
        PickerKeyAction::Other => TerminalRequest::None,
    }
}

fn persist_primary_picker_selection(
    state: &mut AppState,
    role: crate::roster::ResolvedAgent,
    effort: Option<String>,
) -> bool {
    let Some(path) = state.config_path.as_deref() else {
        state.record_status_message(StatusKind::Warning, "config path is unavailable");
        return false;
    };
    let mut config = match config::Config::load(path) {
        Ok(config) => config,
        Err(error) => {
            state.record_status_message(
                StatusKind::Warning,
                format!("could not load config: {error:#}"),
            );
            return false;
        }
    };
    config.agent.model.clone_from(&role.model.model);
    config.agent.reasoning_effort = effort;
    match config.save(path) {
        Ok(()) => {
            state.configured_models = config.model_names();
            state.record_status_message(
                StatusKind::Info,
                format!("{} and reasoning effort saved", role.model.model),
            );
            true
        }
        Err(error) => {
            state.record_status_message(
                StatusKind::Warning,
                format!("model and effort were not saved: {error:#}"),
            );
            false
        }
    }
}

fn handle_config_picker_key(
    state: &mut AppState,
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    modifiers: KeyModifiers,
    code: KeyCode,
    mode: UiMode,
) -> TerminalRequest {
    let action = if matches!(code, KeyCode::Tab) {
        PickerKeyAction::Accept
    } else {
        picker_key_action(modifiers, code)
    };
    match action {
        PickerKeyAction::Cancel => {
            state.dismiss_config_picker();
            inline_repair_request(mode)
        }
        PickerKeyAction::Accept => {
            if let Some((target, value)) = state.config_picker_accept() {
                state.status_line = Some(StatusMessage::info("updating config..."));
                let _ = cmd_tx.send(UiCommand::SetSessionConfigOption { target, value });
                inline_repair_request(mode)
            } else {
                TerminalRequest::None
            }
        }
        PickerKeyAction::Move(delta) => {
            state.config_picker_move(delta);
            TerminalRequest::None
        }
        PickerKeyAction::Other if matches!(code, KeyCode::Backspace) => {
            if let Some(picker) = state.config_picker.as_mut()
                && picker.search_query.pop().is_some()
            {
                let query = picker.search_query.clone();
                state.config_picker_set_search(query);
            }
            TerminalRequest::None
        }
        PickerKeyAction::Other
            if matches!(code, KeyCode::Char(_))
                && (modifiers.is_empty() || modifiers == KeyModifiers::SHIFT) =>
        {
            let KeyCode::Char(c) = code else {
                unreachable!();
            };
            state.config_picker_set_search({
                let mut query = state
                    .config_picker
                    .as_ref()
                    .map(|p| p.search_query.clone())
                    .unwrap_or_default();
                query.push(c);
                query
            });
            TerminalRequest::None
        }
        PickerKeyAction::Other => TerminalRequest::None,
    }
}

fn handle_review_picker_key(
    state: &mut AppState,
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    modifiers: KeyModifiers,
    code: KeyCode,
    mode: UiMode,
) -> TerminalRequest {
    match picker_key_action(modifiers, code) {
        PickerKeyAction::Cancel => {
            state.review_picker = None;
            inline_repair_request(mode)
        }
        PickerKeyAction::Accept => {
            if let Some(target) = state.review_picker_accept() {
                state.record_status_message(StatusKind::Info, "preparing manual review…");
                let _ = cmd_tx.send(UiCommand::RunReview { target });
                inline_repair_request(mode)
            } else {
                TerminalRequest::None
            }
        }
        PickerKeyAction::Move(delta) => {
            state.review_picker_move(delta);
            TerminalRequest::None
        }
        PickerKeyAction::Other => TerminalRequest::None,
    }
}

pub fn setup_fullscreen_terminal() -> Result<Terminal<TrackedBackend<Stdout>>> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();

    if let Err(error) = execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    ) {
        rollback_fullscreen_terminal_setup(&mut stdout);
        return Err(error).context("enter alt screen");
    }
    let backend = TrackedBackend::new(stdout);
    match Terminal::new(backend) {
        Ok(terminal) => Ok(terminal),
        Err(error) => {
            // `Terminal::new` returns its backend only on success, so use a
            // fresh stdout handle to undo the raw/alternate-screen setup.
            let mut stdout = io::stdout();
            rollback_fullscreen_terminal_setup(&mut stdout);
            Err(error).context("ratatui terminal")
        }
    }
}

fn rollback_fullscreen_terminal_setup(stdout: &mut Stdout) {
    let _ = execute!(
        stdout,
        DisableMouseCapture,
        LeaveAlternateScreen,
        DisableBracketedPaste
    );
    let _ = disable_raw_mode();
}

pub fn restore_fullscreen_terminal(terminal: &mut Terminal<TrackedBackend<Stdout>>) -> Result<()> {
    // Attempt every cleanup operation before returning the first error.  A
    // partial failure must not strand the alternate screen or hidden cursor.
    let raw_mode = disable_raw_mode();
    let screen = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        DisableBracketedPaste
    );
    let cursor = terminal.show_cursor();
    raw_mode?;
    screen?;
    cursor?;
    Ok(())
}

pub fn clear_terminal_screen(terminal: &mut Terminal<TrackedBackend<Stdout>>) -> Result<()> {
    execute!(
        terminal.backend_mut(),
        CrosstermClear(CrosstermClearType::All),
        CrosstermClear(CrosstermClearType::Purge),
        MoveTo(0, 0)
    )?;
    Write::flush(terminal.backend_mut())?;
    Ok(())
}

pub fn setup_inline_chat_terminal(initial_height: u16) -> Result<Terminal<TrackedBackend<Stdout>>> {
    let mut stdout = io::stdout();
    stdout.flush().context("flush stdout before inline setup")?;
    let mut stderr = io::stderr();
    let _ = stderr.flush();

    let mut attempt = 0;
    let final_error = loop {
        if attempt > 0 {
            std::thread::sleep(INLINE_SETUP_RETRY_DELAY);
        }

        enable_raw_mode().context("enable raw mode")?;
        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(err).context("enable bracketed paste");
        }
        if let Err(err) = stdout.flush() {
            let mut stdout = io::stdout();
            let _ = execute!(stdout, DisableBracketedPaste);
            let _ = disable_raw_mode();
            return Err(err).context("ratatui inline terminal");
        }

        let backend = TrackedBackend::new(stdout);
        match Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(initial_height),
            },
        ) {
            Ok(terminal) => return Ok(terminal),
            Err(err) => {
                let cursor_position_timeout = is_cursor_position_timeout_io(&err);
                let mut stdout = io::stdout();
                let _ = execute!(stdout, DisableBracketedPaste);
                let _ = disable_raw_mode();
                if cursor_position_timeout {
                    tracing::warn!(
                        "inline terminal setup is waiting for cursor position response; retrying"
                    );
                } else if attempt + 1 >= INLINE_NON_CURSOR_SETUP_ATTEMPTS {
                    break err;
                }
            }
        }
        attempt += 1;
    };

    Err(final_error).context("ratatui inline terminal")
}

pub fn restore_inline_chat_terminal(terminal: &mut Terminal<TrackedBackend<Stdout>>) -> Result<()> {
    if let Err(e) = clear_inline_viewport_for_exit(terminal) {
        tracing::warn!("skip inline exit cleanup: {e}");
    } else if let Err(e) = Write::flush(terminal.backend_mut()) {
        tracing::warn!("skip inline exit cleanup flush: {e}");
    }
    // As with fullscreen restoration, attempt all terminal cleanup before
    // reporting a failure so Drop's one restoration attempt is comprehensive.
    let terminal_modes = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
    let raw_mode = disable_raw_mode();
    let cursor = terminal.show_cursor();
    terminal_modes?;
    raw_mode?;
    cursor?;
    Ok(())
}

pub(crate) fn restore_terminal_for_auth(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    mode: UiMode,
) -> Result<()> {
    match mode {
        UiMode::InlineChat => restore_inline_chat_terminal(terminal),
        UiMode::FullscreenTui => restore_fullscreen_terminal(terminal),
    }
}

pub(crate) fn resume_terminal_after_auth(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    mode: UiMode,
) -> Result<()> {
    enable_raw_mode().context("enable raw mode after sign-in")?;
    let modes = match mode {
        UiMode::InlineChat => execute!(terminal.backend_mut(), EnableBracketedPaste),
        UiMode::FullscreenTui => execute!(
            terminal.backend_mut(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        ),
    };
    if let Err(error) = modes {
        let _ = disable_raw_mode();
        return Err(error).context("restore terminal modes after sign-in");
    }
    match terminal.autoresize() {
        Ok(()) => {}
        Err(error) if is_cursor_position_timeout_io(&error) => {
            trace_inline_cursor_position_timeout("post-sign-in autoresize", &error);
        }
        Err(error) => return Err(error).context("resize terminal after sign-in"),
    }
    match terminal.clear() {
        Ok(()) => {}
        Err(error) if is_cursor_position_timeout_io(&error) => {
            trace_inline_cursor_position_timeout("post-sign-in clear", &error);
        }
        Err(error) => return Err(error).context("clear terminal after sign-in"),
    }
    Ok(())
}

fn clear_inline_viewport_for_exit<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
    let origin = terminal.get_frame().area().as_position();
    terminal.backend_mut().set_cursor_position(origin)?;
    terminal
        .backend_mut()
        .clear_region(ClearType::CurrentLine)?;
    terminal
        .backend_mut()
        .clear_region(ClearType::AfterCursor)?;
    terminal.backend_mut().set_cursor_position(origin)?;
    Ok(())
}

fn is_cursor_position_timeout_io(error: &io::Error) -> bool {
    is_cursor_position_timeout_error(error)
}

fn trace_inline_cursor_position_timeout(action: &str, error: &(dyn Error + 'static)) {
    tracing::trace!(
        "ignored transient inline cursor-position timeout during {action}; keeping inline UI active: {error}"
    );
}

fn is_cursor_position_timeout_error(error: &(dyn Error + 'static)) -> bool {
    let mut cause = Some(error);
    while let Some(current) = cause {
        if let Some(io_error) = current.downcast_ref::<io::Error>()
            && io_error.kind() == io::ErrorKind::Other
            && is_cursor_position_timeout_message(&io_error.to_string())
        {
            return true;
        }
        cause = current.source();
    }

    is_cursor_position_timeout_message(&error.to_string())
}

fn is_cursor_position_timeout_message(message: &str) -> bool {
    message.contains(CURSOR_POSITION_TIMEOUT_MESSAGE)
        || (message.contains("cursor position") && message.contains("normal duration"))
}

/// Minimum input box height: three text rows between top and bottom borders.
const MIN_INPUT_HEIGHT: u16 = 5;
/// Maximum input box height so the transcript stays usable even when
/// the user pastes or drafts a long multi-line prompt.
const MAX_INPUT_HEIGHT: u16 = 16;

fn draw(
    f: &mut ratatui::Frame,
    state: &mut AppState,
    transcript_scroll: &mut TranscriptScrollState,
    mode: UiMode,
) {
    if mode == UiMode::InlineChat {
        draw_inline_chat(f, state);
        return;
    }

    // The Ragnarok arena replaces the whole chat surface while a battle is on
    // screen. The safety-critical permission/elicitation modals still render
    // on top of it.
    if state.ragnarok.is_some() {
        draw_ragnarok(f, f.area(), state);
        if let Some(pending) = state.pending_permission() {
            draw_permission_modal(
                f,
                f.area(),
                pending,
                state.pending_permission_count(),
                state.theme,
            );
        } else if let Some(pending) = state.pending_elicitation() {
            draw_elicitation_modal(
                f,
                f.area(),
                pending,
                state.pending_elicitation_count(),
                state.theme,
            );
        }
        return;
    }

    let usage_quota_rows = usage_quota_row_count(state, f.area().width) as u16;

    // Dynamic input height: borders (2) + chip rows + text lines, clamped.
    let chip_rows = attachment_count(state);
    let input_lines = 1 + state.input.chars().filter(|c| *c == '\n').count();
    let input_height = (chip_rows + input_lines + 2) as u16;
    let input_height = input_height.clamp(MIN_INPUT_HEIGHT, MAX_INPUT_HEIGHT);

    let queued_row = queued_prompt_row_count(state);
    let workflow_rows = workflow_progress_row_count(state);
    let current_pr_rows = current_branch_pr_row_count(state);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(workflow_rows),
            Constraint::Length(current_pr_rows),
            Constraint::Length(queued_row),
            Constraint::Length(input_height),
            Constraint::Length(usage_quota_rows),
        ])
        .split(f.area());

    if state.review_issue_viewer {
        draw_review_issue_viewer(f, chunks[0], state);
    } else if state.nested_agent_viewer {
        draw_nested_agent_viewer(f, chunks[0], state, false);
    } else if state.workspace_diff_viewer {
        draw_workspace_diff_viewer(f, chunks[0], state, false);
    } else {
        draw_transcript(f, chunks[0], state, transcript_scroll);
    }
    draw_header(f, chunks[1], state);
    draw_workflow_progress_rows(f, chunks[2], state);
    draw_current_branch_pr_row(f, chunks[3], state);
    draw_queued_prompt_row(f, chunks[4], state);
    draw_input(f, chunks[5], state, mode);
    draw_usage_quota_row(f, chunks[6], state);

    // Autocomplete sits above the input box (so it doesn't collide with
    // the cursor) and is rendered last among the input-area widgets so
    // it overlays the transcript pane. The permission modal trumps it
    // and renders on top.
    if state.autocomplete.visible {
        draw_autocomplete_popover(f, chunks[1], state);
    }

    if state.agent_picker.is_some() {
        draw_agent_picker_modal(f, f.area(), state);
    }

    if state.review_picker.is_some() {
        draw_review_picker_modal(f, f.area(), state);
    }

    if state.config_picker.is_some() {
        draw_config_value_picker_modal(f, f.area(), state);
    }

    if state.help_overlay {
        draw_help_modal(f, f.area(), mode, state.theme, &mut state.help_scroll);
    }

    if state.mjconfig_menu.is_some() {
        draw_mjconfig_menu(f, f.area(), state);
    }

    if let Some(pending) = state.pending_permission() {
        draw_permission_modal(
            f,
            f.area(),
            pending,
            state.pending_permission_count(),
            state.theme,
        );
    } else if let Some(pending) = state.pending_elicitation() {
        // Drawn only when no permission is pending so the safety-critical
        // permission modal always renders on top.
        draw_elicitation_modal(
            f,
            f.area(),
            pending,
            state.pending_elicitation_count(),
            state.theme,
        );
    }
}

fn inline_transcript_tail_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    if width == 0 || state.help_overlay {
        return Vec::new();
    }
    let stable_entries = stable_transcript_entry_count(state);
    render_transcript_entry_range(
        state,
        width,
        stable_entries..state.transcript.len(),
        transcript_collapse_limit(state),
        state.theme,
        false,
    )
}

fn draw_inline_transcript_tail(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let lines = inline_transcript_tail_lines(state, area.width);
    if lines.is_empty() {
        return;
    }
    let total = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(area.width);
    let top = total
        .saturating_sub(usize::from(area.height))
        .min(u16::MAX as usize) as u16;
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((top, 0)),
        area,
    );
}

fn inline_transcript_tail_height(state: &AppState, area: Rect) -> u16 {
    if state.help_overlay {
        return 0;
    }
    let reserved_rows = 1u16
        .saturating_add(workflow_progress_row_count(state))
        .saturating_add(current_branch_pr_row_count(state))
        .saturating_add(queued_prompt_row_count(state))
        .saturating_add(MIN_INPUT_HEIGHT)
        .saturating_add(usage_quota_row_count(state, area.width) as u16);
    area.height
        .saturating_sub(reserved_rows)
        .min(INLINE_TRANSCRIPT_TAIL_MAX_ROWS as u16)
}

fn inline_chat_layout(state: &AppState, area: Rect) -> [Rect; 7] {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(inline_transcript_tail_height(state, area)),
            Constraint::Length(1),
            Constraint::Length(workflow_progress_row_count(state)),
            Constraint::Length(current_branch_pr_row_count(state)),
            Constraint::Length(queued_prompt_row_count(state)),
            Constraint::Min(MIN_INPUT_HEIGHT),
            Constraint::Length(usage_quota_row_count(state, area.width) as u16),
        ])
        .split(area);
    std::array::from_fn(|index| chunks[index])
}

fn draw_inline_chat(f: &mut ratatui::Frame, state: &mut AppState) {
    if let Some(pending) = state.pending_permission() {
        draw_inline_permission_view(
            f,
            f.area(),
            pending,
            state.pending_permission_count(),
            state.theme,
        );
        return;
    }

    if let Some(pending) = state.pending_elicitation() {
        draw_inline_elicitation_view(
            f,
            f.area(),
            pending,
            state.pending_elicitation_count(),
            state.theme,
        );
        return;
    }

    if state.ragnarok.is_some() {
        draw_ragnarok(f, f.area(), state);
        return;
    }

    if state.agent_picker.is_some() {
        draw_inline_agent_picker(f, f.area(), state);
        return;
    }

    if state.review_picker.is_some() {
        draw_inline_review_picker(f, f.area(), state);
        return;
    }

    if state.config_picker.is_some() {
        draw_inline_config_value_picker(f, f.area(), state);
        return;
    }

    if state.mjconfig_menu.is_some() {
        draw_mjconfig_menu(f, f.area(), state);
        return;
    }

    if state.transcript_viewer {
        draw_inline_transcript_viewer(f, f.area(), state);
        return;
    }

    if state.review_issue_viewer {
        draw_review_issue_viewer(f, f.area(), state);
        return;
    }

    if state.nested_agent_viewer {
        draw_nested_agent_viewer(f, f.area(), state, true);
        return;
    }

    if state.workspace_diff_viewer {
        draw_workspace_diff_viewer(f, f.area(), state, true);
        return;
    }

    let chunks = inline_chat_layout(state, f.area());

    draw_inline_transcript_tail(f, chunks[0], state);
    draw_header(f, chunks[1], state);
    draw_workflow_progress_rows(f, chunks[2], state);
    draw_current_branch_pr_row(f, chunks[3], state);
    draw_queued_prompt_row(f, chunks[4], state);
    draw_input(f, chunks[5], state, UiMode::InlineChat);
    draw_usage_quota_row(f, chunks[6], state);

    if state.autocomplete.visible
        && !state.has_pending_permission()
        && !state.has_pending_elicitation()
    {
        draw_inline_autocomplete_popover(f, f.area(), state);
    }

    if state.help_overlay {
        draw_help_modal(
            f,
            f.area(),
            UiMode::InlineChat,
            state.theme,
            &mut state.help_scroll,
        );
    }
}

fn inline_content_rect(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(2),
        area.height,
    )
}

fn draw_inline_permission_view(
    f: &mut ratatui::Frame,
    area: Rect,
    pending: &PendingPermission,
    queue_len: usize,
    theme: TerminalTheme,
) {
    f.render_widget(Clear, area);
    let content = inline_content_rect(area);
    if content.width == 0 || content.height < 4 {
        return;
    }

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(content);

    let lines = permission_view_lines(pending, queue_len, content.width, theme);
    let visible_lines =
        visible_permission_content_lines(pending, &lines, content.width, layout[0].height);
    f.render_widget(Paragraph::new(visible_lines), layout[0]);

    f.render_widget(
        Paragraph::new("Up/Down choose | PgUp/PgDn read | Enter to confirm | Esc cancel")
            .style(Style::default().fg(theme.muted)),
        layout[1],
    );
}

fn centered_visible_range(total: usize, selected: usize, visible: usize) -> Range<usize> {
    if total <= visible {
        return 0..total;
    }
    let start = selected
        .saturating_sub(visible / 2)
        .min(total.saturating_sub(visible));
    start..(start + visible).min(total)
}

fn agent_picker_items(state: &AppState, width: u16, visible: usize) -> Vec<ListItem<'static>> {
    let Some(picker) = state.agent_picker.as_ref() else {
        return Vec::new();
    };
    if picker.step == AgentPickerStep::Effort {
        let range = centered_visible_range(
            PRIMARY_EFFORT_OPTIONS.len(),
            picker.effort_selected,
            visible,
        );
        return range
            .map(|index| {
                let label = primary_effort_label(index);
                truncate_line(label, width, index == picker.effort_selected, state.theme)
            })
            .collect();
    }
    let range = centered_visible_range(picker.role_indices.len(), picker.selected, visible);
    picker.role_indices[range.clone()]
        .iter()
        .enumerate()
        .filter_map(|(offset, &role_index)| {
            let position = range.start + offset;
            let role = state.ragnarok_models.get(role_index)?;
            let current = role.model.model == state.active_models.primary;
            let suffix = if current { "  current" } else { "" };
            Some(truncate_line(
                format!("{} via {}{suffix}", role.model.model, role.launch.source_id),
                width,
                position == picker.selected,
                state.theme,
            ))
        })
        .collect()
}

fn primary_effort_label(index: usize) -> String {
    match primary_effort_value(index) {
        Some(effort) => effort.to_string(),
        None => "adapter default".to_string(),
    }
}

fn agent_picker_selection_summary(state: &AppState) -> String {
    let Some(picker) = state.agent_picker.as_ref() else {
        return String::new();
    };
    let model = picker
        .role_indices
        .get(picker.selected)
        .and_then(|&index| state.ragnarok_models.get(index))
        .map(|role| role.model.model.as_str())
        .unwrap_or("primary model");
    format!("{model} · {}", primary_effort_label(picker.effort_selected))
}

fn draw_inline_agent_picker(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    f.render_widget(Clear, area);
    let content = inline_content_rect(area);
    if content.width == 0 || content.height < 4 {
        return;
    }
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(content);
    f.render_widget(
        Paragraph::new("Configure primary model").style(
            Style::default()
                .fg(state.theme.primary)
                .add_modifier(Modifier::BOLD),
        ),
        layout[0],
    );
    let step = state
        .agent_picker
        .as_ref()
        .map(|picker| picker.step)
        .unwrap_or(AgentPickerStep::Model);
    let detail = match step {
        AgentPickerStep::Model => "Choose a model for the primary agent",
        AgentPickerStep::Effort => "Choose its reasoning effort",
        AgentPickerStep::ConfirmSave => "Save this model and effort?",
        AgentPickerStep::StartNewSession => "Saved. Start a new session now to apply the change?",
    };
    f.render_widget(
        Paragraph::new(detail).style(Style::default().fg(state.theme.muted)),
        layout[1],
    );
    if step == AgentPickerStep::ConfirmSave {
        f.render_widget(
            Paragraph::new(agent_picker_selection_summary(state)),
            layout[2],
        );
    } else if step == AgentPickerStep::StartNewSession {
        let start_new_session = state
            .agent_picker
            .as_ref()
            .is_some_and(|picker| picker.start_new_session);
        let start = if start_new_session {
            "› start new session"
        } else {
            "  start new session"
        };
        let keep = if start_new_session {
            "  keep current session"
        } else {
            "› keep current session"
        };
        f.render_widget(
            Paragraph::new(vec![Line::from(start), Line::from(keep)]),
            layout[2],
        );
    } else {
        f.render_widget(
            List::new(agent_picker_items(
                state,
                layout[2].width,
                usize::from(layout[2].height),
            )),
            layout[2],
        );
    }
    let footer = match step {
        AgentPickerStep::Model | AgentPickerStep::Effort => {
            "Up/Down or Tab/Shift-Tab choose | Enter continue | Esc back"
        }
        AgentPickerStep::ConfirmSave => "Enter save | Esc back",
        AgentPickerStep::StartNewSession => "Up/Down choose | Enter confirm | Esc keep current",
    };
    f.render_widget(
        Paragraph::new(footer).style(Style::default().fg(state.theme.muted)),
        layout[3],
    );
}

fn draw_inline_config_value_picker(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    f.render_widget(Clear, area);
    let content = inline_content_rect(area);
    if content.width == 0 || content.height < 5 {
        return;
    }

    let Some(picker) = state.config_picker.as_ref() else {
        return;
    };
    let Some(option) = state.session_config_options.get(picker.selected_option) else {
        return;
    };
    let Some(choices) = config_option_choices(option) else {
        return;
    };

    let title = format!("{} values", option.name);
    let detail = option
        .description
        .clone()
        .unwrap_or_else(|| config_option_current_value_label(option));
    let detail_lines = wrap_text_to_width(&detail, content.width)
        .into_iter()
        .take(2)
        .map(Line::from)
        .collect::<Vec<_>>();
    let detail_height = detail_lines.len().max(1).min(u16::MAX as usize) as u16;
    // Score attribution, rendered as its own row just above the footer.
    let legend = model_score_legend(state, option);
    let legend_rows = u16::from(legend.is_some());

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(detail_height),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(legend_rows),
            Constraint::Length(1),
        ])
        .split(content);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title,
            Style::default()
                .fg(state.theme.primary)
                .add_modifier(Modifier::BOLD),
        ))),
        layout[0],
    );
    f.render_widget(Paragraph::new(detail_lines), layout[1]);

    let search_text = if picker.search_query.is_empty() {
        "filter:".to_string()
    } else {
        format!("filter: {}", picker.search_query)
    };
    f.render_widget(
        Paragraph::new(search_text).style(Style::default().fg(state.theme.muted)),
        layout[2],
    );

    let total = picker.filtered_indices.len();
    if total == 0 {
        f.render_widget(
            Paragraph::new("No matches").style(Style::default().fg(state.theme.muted)),
            layout[3],
        );
    } else {
        let selected = picker.selected_value;
        let range = centered_visible_range(total, selected, usize::from(layout[3].height));
        let start = range.start;
        let items = picker.filtered_indices[range]
            .iter()
            .enumerate()
            .map(|(offset, &full_idx)| {
                let absolute = start + offset;
                let marker = if absolute == selected { ">" } else { " " };
                let choice = &choices[full_idx];
                let score = model_choice_score(state, option, choice);
                let line = config_value_row_text(choice, score.as_deref(), layout[3].width);
                truncate_line(line, layout[3].width, marker == ">", state.theme)
            })
            .collect::<Vec<ListItem>>();
        f.render_widget(List::new(items), layout[3]);
    }

    if let Some(legend) = legend {
        f.render_widget(
            Paragraph::new(legend).style(Style::default().fg(state.theme.muted)),
            layout[4],
        );
    }

    let footer = if picker.search_query.is_empty() {
        "Up/Down choose | type to filter | Enter apply | Esc cancel"
    } else {
        "Up/Down choose | Backspace clear | Enter apply | Esc cancel"
    };
    f.render_widget(
        Paragraph::new(footer).style(Style::default().fg(state.theme.muted)),
        layout[5],
    );
}

/// Full-screen inline reader for the entire transcript with all details
/// expanded. `scroll_offset` is the index of the top visible line and is
/// clamped here so End / PageDown can never scroll past the final screen.
fn draw_inline_transcript_viewer(f: &mut ratatui::Frame, area: Rect, state: &mut AppState) {
    f.render_widget(Clear, area);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" transcript — full history · Alt-T toggles latest tool ")
        .style(Style::default().fg(state.theme.agent));
    let inner = block.inner(layout[0]);
    f.render_widget(block, layout[0]);

    if inner.width > 0 && inner.height > 0 {
        let lines = render_full_transcript_lines(state, inner.width);
        let total = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(inner.width);
        let max_offset = total.saturating_sub(usize::from(inner.height));
        state.scroll_offset = state.scroll_offset.min(max_offset);
        let top = state.scroll_offset.min(u16::MAX as usize) as u16;
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((top, 0)),
            inner,
        );
    }

    f.render_widget(
        Paragraph::new(
            "Alt-T latest tool · Up/Down PgUp/PgDn scroll · Home/End top/bottom · Esc or Ctrl-T to close",
        )
            .style(Style::default().fg(state.theme.muted)),
        layout[1],
    );
}

fn draw_review_issue_viewer(f: &mut ratatui::Frame, area: Rect, state: &mut AppState) {
    use crate::workflow::ReviewIssueStatus;

    f.render_widget(Clear, area);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" review issues — session ledger ")
        .style(Style::default().fg(state.theme.agent));
    let inner = block.inner(layout[0]);
    f.render_widget(block, layout[0]);

    let mut issues = state
        .workflows
        .iter()
        .flat_map(|workflow| {
            workflow
                .issues
                .iter()
                .map(move |issue| (workflow.id, issue))
        })
        .collect::<Vec<_>>();
    issues
        .sort_by_key(|(workflow_id, issue)| (workflow_id.turn_id, workflow_id.operation, issue.id));
    let found = issues.len();
    let validated = issues
        .iter()
        .filter(|(_, issue)| issue.status == ReviewIssueStatus::Validated)
        .count();
    let fixed = issues
        .iter()
        .filter(|(_, issue)| issue.status == ReviewIssueStatus::Fixed)
        .count();
    let invalidated = issues
        .iter()
        .filter(|(_, issue)| issue.status == ReviewIssueStatus::Invalidated)
        .count();
    let mut lines = vec![Line::from(Span::styled(
        format!(
            " found {found} · validated {validated} · fixed {fixed} · invalidated {invalidated}"
        ),
        Style::default()
            .fg(state.theme.accent)
            .add_modifier(Modifier::BOLD),
    ))];
    if issues.is_empty() {
        lines.push(Line::from(Span::styled(
            " No review issues recorded yet.",
            Style::default().fg(state.theme.muted),
        )));
    } else {
        lines.extend(issues.into_iter().map(|(_, issue)| {
            let color = match issue.status {
                ReviewIssueStatus::Validated => state.theme.warning,
                ReviewIssueStatus::Fixed => state.theme.success,
                ReviewIssueStatus::Invalidated => state.theme.muted,
            };
            Line::from(Span::styled(
                format!(
                    " #{} · {} · pass {} · {}",
                    issue.id,
                    issue.status.as_str(),
                    issue.pass + 1,
                    issue.summary
                ),
                Style::default().fg(color),
            ))
        }));
    }
    let total = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(inner.width);
    let max_offset = total.saturating_sub(usize::from(inner.height));
    state.review_issue_scroll_offset = state.review_issue_scroll_offset.min(max_offset);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((
            state.review_issue_scroll_offset.min(u16::MAX as usize) as u16,
            0,
        )),
        inner,
    );
    f.render_widget(
        Paragraph::new("F9/Esc close · Up/Down PgUp/PgDn Home/End scroll")
            .style(Style::default().fg(state.theme.muted)),
        layout[1],
    );
}

fn nested_actor_backend(actor: &SubagentStatus) -> String {
    match (actor.adapter.trim(), actor.model.as_deref().map(str::trim)) {
        ("", None | Some("")) => "unknown backend".to_string(),
        (adapter, None | Some("")) => adapter.to_string(),
        ("", Some(model)) => model.to_string(),
        (adapter, Some(model)) => format!("{adapter}/{model}"),
    }
}

fn nested_actor_state_label(actor: &SubagentStatus) -> String {
    use crate::workflow::WorkflowActorLifecycle;

    match actor.lifecycle.as_ref() {
        Some(WorkflowActorLifecycle::Running) => "running".to_string(),
        Some(WorkflowActorLifecycle::Waiting {
            dependency,
            remaining,
            requires_user_action,
        }) => {
            let mut label = format!("waiting on {dependency}");
            if let Some(remaining) = remaining {
                label.push_str(&format!(" ({remaining} remaining)"));
            }
            if *requires_user_action {
                label.push_str(" · user action required");
            }
            label
        }
        Some(WorkflowActorLifecycle::Completed) => "completed".to_string(),
        Some(WorkflowActorLifecycle::Failed(message)) => {
            format!("failed: {}", ragnarok::first_line(message, 80))
        }
        Some(WorkflowActorLifecycle::Cancelled) => "cancelled".to_string(),
        None => actor
            .outcome()
            .map(SubagentOutcome::label)
            .unwrap_or("starting")
            .to_string(),
    }
}

fn nested_agent_roster_line(
    subagent_id: u64,
    actor: &SubagentStatus,
    selected: bool,
    now: Instant,
    width: usize,
    theme: TerminalTheme,
) -> Line<'static> {
    let marker = if selected { "›" } else { " " };
    let role = actor
        .role
        .as_ref()
        .map(crate::app::nested_role_label)
        .unwrap_or_else(|| "subagent".to_string());
    let outcome = actor
        .outcome()
        .map(|outcome| format!(" · outcome {}", outcome.label()))
        .unwrap_or_default();
    let text = format!(
        "{marker} #{subagent_id} {} · {role} · {} · {} · {} · {}{outcome}",
        actor.label,
        nested_actor_backend(actor),
        nested_actor_state_label(actor),
        ragnarok::first_line(&actor.activity, 80),
        format_duration(actor.elapsed_at(now)),
    );
    Line::from(Span::styled(
        fit_width(text, width),
        if selected {
            Style::default()
                .fg(theme.selection_fg)
                .bg(theme.selection_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted)
        },
    ))
}

fn nested_internal_message_title(message: &crate::event::InternalMessage) -> String {
    let chars = message.text.chars().count();
    match message.kind {
        crate::event::InternalMessageKind::Delegation => {
            format!("delegation brief · {}", message_size_label(chars))
        }
        crate::event::InternalMessageKind::DiscreteReview => {
            format!("review brief · {}", message_size_label(chars))
        }
        crate::event::InternalMessageKind::ReviewLane => {
            format!("specialist report · {}", message_size_label(chars))
        }
        crate::event::InternalMessageKind::ReviewProgress => {
            format!("supervisor checkpoint · {}", message_size_label(chars))
        }
        crate::event::InternalMessageKind::ReviewSynthesis => {
            format!("final synthesis · {}", message_size_label(chars))
        }
    }
}

fn nested_internal_message_style(
    kind: crate::event::InternalMessageKind,
    theme: TerminalTheme,
) -> Style {
    let color = match kind {
        crate::event::InternalMessageKind::ReviewProgress => theme.secondary,
        crate::event::InternalMessageKind::ReviewSynthesis => theme.accent,
        crate::event::InternalMessageKind::ReviewLane => theme.tool,
        crate::event::InternalMessageKind::Delegation
        | crate::event::InternalMessageKind::DiscreteReview => theme.muted,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

fn render_nested_agent_lines(
    state: &AppState,
    actor: &SubagentStatus,
    width: u16,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if let Some(history) = actor.archived_history_markdown() {
        push_markdown_lines(&mut out, history, 0, width, state.theme);
    }
    for (entry_index, entry) in actor.transcript.iter().enumerate() {
        match entry {
            Entry::UserPrompt(text) => {
                push_role_plain_message(
                    &mut out,
                    USER_GLYPH,
                    state.theme.user,
                    text,
                    false,
                    width,
                    state.theme,
                );
            }
            Entry::AgentMessage(text) | Entry::SubagentMessage(text) => {
                push_role_markdown_message(
                    &mut out,
                    SUBAGENT_GLYPH,
                    state.theme.secondary,
                    text,
                    false,
                    width,
                    state.theme,
                );
            }
            Entry::AgentThought(thought) | Entry::SubagentThought(thought) => {
                push_role_thinking(
                    &mut out,
                    (THOUGHT_GLYPH, state.theme.thought),
                    &thought.text,
                    thought.completed,
                    false,
                    width,
                    state.theme,
                );
            }
            Entry::InternalMessage(message) => {
                out.push(Line::from(Span::styled(
                    nested_internal_message_title(message),
                    nested_internal_message_style(message.kind, state.theme),
                )));
                push_markdown_message(&mut out, &message.text, false, width, state.theme);
            }
            Entry::Plan(entries) | Entry::SubagentPlan(entries) => {
                out.push(Line::from(Span::styled(
                    "plan",
                    Style::default()
                        .fg(state.theme.tool)
                        .add_modifier(Modifier::BOLD),
                )));
                for entry in entries {
                    out.push(plan_row(entry, state.theme));
                }
                out.push(Line::from(""));
            }
            Entry::ToolCall(id) | Entry::SubagentToolCall(id) => {
                if let Some(view) = state.tool_calls.get(id) {
                    let color = tool_status_color(view.status, state.theme);
                    let terminal_exit_status = view.body.iter().rev().find_map(|output| {
                        if let ToolCallOutput::Terminal { exit_status, .. } = output {
                            exit_status.as_ref()
                        } else {
                            None
                        }
                    });
                    let status = match (view.status, terminal_exit_status) {
                        (ToolCallStatus::Completed, _) | (_, Some(_)) => String::new(),
                        _ => format!("[{}] ", tool_status_label(view.status)),
                    };
                    let mut spans = tool_header_spans(view, &status, false, state.theme);
                    if let Some(exit_status) = terminal_exit_status {
                        spans.push(Span::styled(
                            format!(" · {}", terminal_header_outcome_label(exit_status)),
                            terminal_header_outcome_style(exit_status, state.theme),
                        ));
                    }
                    let content_width = width.saturating_sub(TOOL_GUTTER_WIDTH);
                    let mut block = vec![Line::from(spans)];
                    let collapse_limit = match state.tool_detail_expanded(id) {
                        Some(false) => Some(TOOL_OUTPUT_COLLAPSED_LINES),
                        _ => None,
                    };
                    push_tool_outputs(
                        &mut block,
                        &view.body,
                        view.status,
                        content_width,
                        collapse_limit,
                        state.theme,
                    );
                    for line in block {
                        for row in wrap_tool_line(line, content_width as usize) {
                            out.push(with_tool_gutter(row, color));
                        }
                    }
                    let next_is_tool = actor.transcript.get(entry_index + 1).is_some_and(|next| {
                        matches!(
                            next,
                            Entry::ToolCall(next_id) | Entry::SubagentToolCall(next_id)
                                if state.tool_calls.contains_key(next_id)
                        )
                    });
                    if !next_is_tool {
                        out.push(Line::from(""));
                    }
                }
            }
            Entry::System(text) => {
                push_styled_message(&mut out, text, state.theme.accent, false, state.theme);
            }
            Entry::SessionBoundary(text) => {
                out.push(Line::from(""));
                out.push(session_boundary_line(text, width, state.theme));
                out.push(Line::from(""));
            }
        }
    }
    out
}

/// On-demand reader for every nested implementation and review actor. The
/// same rendering path is used in inline and fullscreen modes so opening the
/// reader never changes terminal ownership.
fn draw_nested_agent_viewer(
    f: &mut ratatui::Frame,
    area: Rect,
    state: &mut AppState,
    inline: bool,
) {
    if inline {
        f.render_widget(Clear, area);
    }
    if area.width == 0 || area.height == 0 {
        return;
    }

    #[cfg(target_os = "macos")]
    let footer =
        "Esc close · Left/Right agent · Up/Down scroll · Fn+Up/Down page · Home/End · Alt-T tool";
    #[cfg(not(target_os = "macos"))]
    let footer =
        "Esc close · Left/Right agent · Up/Down scroll · PgUp/PgDn page · Home/End · Alt-T tool";
    let footer_height = Paragraph::new(footer)
        .wrap(Wrap { trim: false })
        .line_count(area.width)
        .max(1)
        .min(usize::from(u16::MAX)) as u16;
    let actor_ids = state.nested_agent_viewer_ids();
    let actor_count = state.nested_agents().count();
    let roster_rows = actor_ids.len().clamp(1, usize::from(u16::MAX)) as u16;
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(roster_rows.saturating_add(2)),
            Constraint::Min(1),
            Constraint::Length(footer_height),
        ])
        .split(area);

    let now = Instant::now();
    let roster_title = if actor_count > actor_ids.len() {
        format!(
            " nested agents — {} newest of {actor_count} retained ",
            actor_ids.len()
        )
    } else {
        " nested agents — retained for this session ".to_string()
    };
    let roster_block = Block::default()
        .borders(Borders::ALL)
        .title(roster_title)
        .style(Style::default().fg(state.theme.agent));
    let roster_inner = roster_block.inner(layout[0]);
    f.render_widget(roster_block, layout[0]);
    if roster_inner.width > 0 && roster_inner.height > 0 {
        let roster = actor_ids
            .iter()
            .filter_map(|id| {
                let actor = state.nested_agent(*id)?;
                Some(nested_agent_roster_line(
                    *id,
                    actor,
                    state.nested_agent_selected == Some(*id),
                    now,
                    usize::from(roster_inner.width),
                    state.theme,
                ))
            })
            .collect::<Vec<_>>();
        let selected = state
            .nested_agent_selected
            .and_then(|selected| actor_ids.iter().position(|id| *id == selected))
            .unwrap_or(0);
        let visible = usize::from(roster_inner.height);
        let start = selected
            .saturating_sub(visible.saturating_sub(1))
            .min(roster.len().saturating_sub(visible));
        f.render_widget(
            Paragraph::new(
                roster
                    .into_iter()
                    .skip(start)
                    .take(visible)
                    .collect::<Vec<_>>(),
            ),
            roster_inner,
        );
    }

    let selected = state.selected_nested_agent().map(|(id, actor)| {
        let title = format!(
            " #{} {} · {} · {} ",
            id,
            actor.label,
            actor
                .role
                .as_ref()
                .map(crate::app::nested_role_label)
                .unwrap_or_else(|| "subagent".to_string()),
            nested_actor_backend(actor),
        );
        let lines = render_nested_agent_lines(state, actor, layout[1].width.saturating_sub(2));
        (title, lines)
    });
    let title = selected
        .as_ref()
        .map(|(title, _)| title.as_str())
        .unwrap_or(" nested agent transcript ");
    let transcript_block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(state.theme.secondary));
    let transcript_inner = transcript_block.inner(layout[1]);
    f.render_widget(transcript_block, layout[1]);
    if transcript_inner.width > 0 && transcript_inner.height > 0 {
        let lines = selected
            .map(|(_, lines)| lines)
            .filter(|lines| !lines.is_empty())
            .unwrap_or_else(|| {
                vec![Line::from(Span::styled(
                    "No nested transcript events have arrived yet.",
                    Style::default().fg(state.theme.muted),
                ))]
            });
        let total = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(transcript_inner.width);
        let max_offset = total.saturating_sub(usize::from(transcript_inner.height));
        state.nested_agent_scroll_offset = state.nested_agent_scroll_offset.min(max_offset);
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((
                state.nested_agent_scroll_offset.min(u16::MAX as usize) as u16,
                0,
            )),
            transcript_inner,
        );
    }
    f.render_widget(
        Paragraph::new(footer)
            .style(Style::default().fg(state.theme.muted))
            .wrap(Wrap { trim: false }),
        layout[2],
    );
}

/// Reader for the latest native workspace-diff event. Unlike tool output,
/// this deliberately has no transcript side effects and no compact row budget.
fn draw_workspace_diff_viewer(
    f: &mut ratatui::Frame,
    area: Rect,
    state: &mut AppState,
    inline: bool,
) {
    if inline {
        f.render_widget(Clear, area);
    }
    let footer = "Ctrl-G/Esc close · Up/Down PgUp/PgDn Home/End scroll · n/p previous/next file";
    let footer_height = Paragraph::new(footer)
        .wrap(Wrap { trim: false })
        .line_count(area.width)
        .max(1)
        .min(usize::from(u16::MAX)) as u16;
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(footer_height)])
        .split(area);
    let Some(event) = state.workspace_diffs.last() else {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" workspace diff — no workspace changes ")
            .style(Style::default().fg(state.theme.agent));
        let inner = block.inner(layout[0]);
        f.render_widget(block, layout[0]);
        if inner.width > 0 && inner.height > 0 {
            f.render_widget(
                Paragraph::new("No workspace changes have been captured for this session.")
                    .style(Style::default().fg(state.theme.muted)),
                inner,
            );
        }
        f.render_widget(
            Paragraph::new(footer)
                .style(Style::default().fg(state.theme.muted))
                .wrap(Wrap { trim: false }),
            layout[1],
        );
        return;
    };
    let retained = event.diffs.len();
    let selected = state
        .workspace_diff_selected_file
        .min(retained.saturating_sub(1));
    state.workspace_diff_selected_file = selected;
    let noun = if event.total_files == 1 {
        "file"
    } else {
        "files"
    };
    let title = if let Some(diff) = event.diffs.get(selected) {
        let capped = event.truncated || retained < event.total_files;
        let suffix = if capped {
            format!(" — showing {retained} of {}", event.total_files)
        } else {
            String::new()
        };
        format!(
            " workspace diff — {} {noun} changed — retained {}/{} {}{} ",
            event.total_files,
            selected + 1,
            retained,
            diff.path.display(),
            suffix
        )
    } else if event.total_files == 0 {
        " workspace diff — no workspace changes ".to_string()
    } else {
        format!(
            " workspace diff — {} {noun} changed — no retained file ",
            event.total_files
        )
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(state.theme.agent));
    let inner = block.inner(layout[0]);
    f.render_widget(block, layout[0]);
    if let Some(diff) = event.diffs.get(selected) {
        if inner.width > 0 && inner.height > 0 {
            let lines = render_prepared_diff_rows_full(
                &prepared_diff_rows(diff.old_text.as_deref(), &diff.new_text, usize::MAX),
                state.theme,
            );
            let total = Paragraph::new(lines.clone())
                .wrap(Wrap { trim: false })
                .line_count(inner.width);
            let max_offset = total.saturating_sub(usize::from(inner.height));
            state.workspace_diff_scroll_offset = state.workspace_diff_scroll_offset.min(max_offset);
            f.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((
                    state.workspace_diff_scroll_offset.min(u16::MAX as usize) as u16,
                    0,
                )),
                inner,
            );
        }
    } else if inner.width > 0 && inner.height > 0 {
        let message = if event.total_files == 0 {
            "No workspace changes were captured for this event."
        } else {
            "No retained diff is available for this event."
        };
        f.render_widget(
            Paragraph::new(message).style(Style::default().fg(state.theme.muted)),
            inner,
        );
    }
    f.render_widget(
        Paragraph::new(footer)
            .style(Style::default().fg(state.theme.muted))
            .wrap(Wrap { trim: false }),
        layout[1],
    );
}

fn inline_config_view_line_count(state: &AppState, width: u16) -> usize {
    let Some(picker) = state.config_picker.as_ref() else {
        return usize::from(INLINE_CHAT_HEIGHT);
    };
    let Some(option) = state.session_config_options.get(picker.selected_option) else {
        return usize::from(INLINE_CHAT_HEIGHT);
    };
    let detail = option
        .description
        .clone()
        .unwrap_or_else(|| config_option_current_value_label(option));
    let detail_rows = wrap_text_to_width(&detail, width).len().max(1);
    let option_rows = picker.filtered_indices.len().max(1);
    let legend_rows = usize::from(model_score_legend(state, option).is_some());
    1 + detail_rows + 1 + option_rows + legend_rows + 1
}

fn draw_header(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let inner = area;

    let width = area.width as usize;
    let mut spans = vec![
        Span::styled(
            mjolnir_version_label(),
            Style::default().fg(state.theme.accent),
        ),
        Span::raw("   "),
    ];
    let agent_label = state
        .subagent_label
        .as_deref()
        .or(Some(state.agent_label.as_str()))
        .map(str::trim)
        .filter(|label| !label.is_empty());
    if let Some(agent_label) = agent_label {
        spans.push(Span::styled(
            agent_label.to_string(),
            Style::default().fg(state.theme.primary),
        ));
        spans.push(Span::raw("   "));
    }
    if let Some(notice) = state.side_main_notice.as_deref() {
        spans.push(Span::styled(
            notice.to_string(),
            Style::default().fg(state.theme.warning),
        ));
        spans.push(Span::raw("   "));
    }
    let project_label = state.project_label.trim();
    if !project_label.is_empty() {
        let max_width = match width {
            0..=89 => 18,
            90..=139 => 28,
            140..=179 => 40,
            _ => 56,
        };
        spans.push(Span::styled(
            compact_middle_display(project_label, max_width),
            Style::default().fg(state.theme.secondary),
        ));
        spans.push(Span::raw("   "));
    }
    if state.additional_roots > 0 {
        let label = if state.additional_roots == 1 {
            "+1 root".to_string()
        } else {
            format!("+{} roots", state.additional_roots)
        };
        spans.push(Span::styled(
            label,
            Style::default().fg(state.theme.warning),
        ));
        spans.push(Span::raw("   "));
    }
    spans.push(Span::styled(
        header_token_usage_label(state, width),
        Style::default().fg(state.theme.tool),
    ));
    if let Some(title) = state.session_title.as_deref() {
        let title = title.trim();
        if !title.is_empty() {
            // The session title is appended LAST and consumes whatever width
            // remains after the preceding spans (version/project/token
            // usage) plus a 3-cell separator. This relies on every other
            // width-consuming span having already been pushed above.
            let separator_width = 3;
            let used: usize = spans.iter().map(|span| span.content.width()).sum();
            let max_width = width.saturating_sub(used).saturating_sub(separator_width);
            if max_width > 0 {
                spans.push(Span::raw("   "));
                spans.push(Span::styled(
                    compact_middle_display(title, max_width),
                    Style::default()
                        .fg(state.theme.terminal)
                        .add_modifier(Modifier::ITALIC),
                ));
            }
        }
    }
    let p = Paragraph::new(Line::from(spans));
    f.render_widget(p, inner);
}

fn compact_middle_display(text: &str, max_width: usize) -> String {
    if text.width() <= max_width {
        return text.to_string();
    }
    if max_width <= 3 {
        return text.chars().take(max_width).collect();
    }

    let prefix_width = (max_width - 3) / 3;
    let suffix_width = max_width - 3 - prefix_width;
    let prefix = take_display_prefix(text, prefix_width);
    let suffix = take_display_suffix(text, suffix_width);
    format!("{prefix}...{suffix}")
}

fn take_display_prefix(text: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out
}

fn take_display_suffix(text: &str, max_width: usize) -> String {
    let mut chars = Vec::new();
    let mut width = 0;
    for ch in text.chars().rev() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        chars.push(ch);
        width += ch_width;
    }
    chars.into_iter().rev().collect()
}

fn turn_elapsed_value_label(state: &AppState) -> Option<String> {
    match state.connection_state() {
        ConnectionState::Launching | ConnectionState::Initializing => {
            Some(format_duration(state.connection_state_elapsed()))
        }
        ConnectionState::Ready => state.last_turn_elapsed().map(format_duration),
        ConnectionState::Streaming | ConnectionState::Cancelling | ConnectionState::Forking => {
            state.active_turn_elapsed().map(format_duration)
        }
        ConnectionState::Closed | ConnectionState::Fatal => None,
    }
}

pub(crate) fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    let minutes = secs / 60;
    let seconds = secs % 60;
    if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn token_usage_label(state: &AppState) -> String {
    let usage = state.displayed_token_usage();
    let mut parts = Vec::new();

    if let Some(input) = usage.input_tokens {
        parts.push(format!("in: {}", compact_count(input)));
    }
    if let Some(output) = usage.output_tokens {
        parts.push(format!("out: {}", compact_count(output)));
    }
    if let Some(used) = usage.context_used {
        parts.push(format!("ctx: {}", compact_count(used)));
    }
    // The Claude rate-limit status is surfaced in the transcript (see
    // `apply_usage_update`), not the header, so it is intentionally omitted
    // here.

    if !parts.is_empty() {
        return parts.join(" · ");
    }

    "in: - · out: - · ctx: -".to_string()
}

fn header_token_usage_label(state: &AppState, _width: usize) -> String {
    let usage = &state.agent_usage;
    if usage.total().prompts > 0 {
        let mut label = format!(
            "primary {} · subagents {}",
            compact_count(usage.primary.total_tokens),
            compact_count(usage.subagents.total_tokens),
        );
        if usage.review.total_tokens > 0 {
            label.push_str(&format!(
                " · review {}",
                compact_count(usage.review.total_tokens)
            ));
        }
        return label;
    }
    token_usage_label(state)
}

fn seat_usage_label(usage: &crate::agent_usage::RoleUsage) -> String {
    let mut label = format!("{} tokens", usage.total_tokens);
    for (currency, amount) in &usage.costs {
        label.push_str(&format!(" · {amount:.4} {currency}"));
    }
    label
}

/// The `/agents` panel body: the models each seat is currently bound to,
/// per-seat usage, and a per-model breakdown when more than the two bound
/// models did work.
fn active_models_and_usage_report(state: &AppState) -> String {
    let usage = &state.agent_usage;
    let primary = state.active_models.primary_source.as_deref().map_or_else(
        || state.active_models.primary.clone(),
        |source| format!("{} via {source}", state.active_models.primary),
    );
    let review = state.active_models.review_source.as_deref().map_or_else(
        || state.active_models.review.clone(),
        |source| format!("{} via {source}", state.active_models.review),
    );
    let subagent = state.active_models.subagent_source.as_deref().map_or_else(
        || state.active_models.subagent.clone(),
        |source| format!("{} via {source}", state.active_models.subagent),
    );
    let mut report = format!(
        "Active models\nprimary    {}\nreview     {}\nsubagents  {}\n\nUsage (tokens)\nprimary    {}\nsubagents  {}\nreview     {}",
        primary,
        review,
        subagent,
        seat_usage_label(&usage.primary),
        seat_usage_label(&usage.subagents),
        seat_usage_label(&usage.review),
    );
    if !usage.per_model.is_empty() {
        report.push_str("\n\nBy model");
        for (model, model_usage) in &usage.per_model {
            report.push_str(&format!("\n{model}  {}", seat_usage_label(model_usage)));
        }
    }
    report
}

fn compact_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 10_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn draw_transcript(
    f: &mut ratatui::Frame,
    area: Rect,
    state: &mut AppState,
    transcript_scroll: &mut TranscriptScrollState,
) {
    let title = transcript_block_title(state);
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Avoid rebuilding the lines and re-running `Paragraph::line_count`
    // (both O(text) with unicode segmentation) when neither the
    // transcript nor the terminal width has changed since the last
    // frame. Caching is keyed by `(revision, width)`; any mutation to
    // `transcript` / `tool_calls` bumps the revision.
    let revision = state.transcript_revision();
    let cache_hit = matches!(
        transcript_scroll.cache.as_ref(),
        Some(c) if c.revision == revision && c.width == inner.width
    );
    if !cache_hit {
        let lines = render_transcript_lines(state, inner.width);
        let line_count = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(inner.width);
        transcript_scroll.cache = Some(TranscriptCache {
            revision,
            width: inner.width,
            lines,
            line_count,
        });
    }
    let cache = transcript_scroll
        .cache
        .as_ref()
        .expect("cache populated above");
    let total = cache.line_count;
    // Clone the cached lines because `Paragraph::new` consumes the
    // `Vec<Line>`. This still avoids the dominant cost (word-wrap +
    // unicode tables) which only runs inside `render_widget`.
    let lines = cache.lines.clone();

    transcript_scroll.reconcile(&mut state.scroll_offset, total, inner.height);
    let top = total
        .saturating_sub(inner.height as usize)
        .saturating_sub(state.scroll_offset)
        .min(u16::MAX as usize) as u16;
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((top, 0));
    f.render_widget(paragraph, inner);
}

/// Block title for the transcript pane. Adds a scroll indicator when
/// `scroll_offset > 0` so the user knows they're no longer following the
/// stream and can press End / scroll down to re-attach. The expand
/// state for compacted transcript details is appended so Ctrl-T's effect is visible.
fn transcript_block_title(state: &AppState) -> String {
    let mut title = String::from(" transcript ");
    if state.scroll_offset > 0 {
        title.push_str(&format!(
            "[scrolled +{} | End to follow] ",
            state.scroll_offset
        ));
    }
    if state.expand_transcript_details {
        title.push_str("[details: expanded | Ctrl-T] ");
    }
    title
}

fn render_transcript_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    render_transcript_entry_range(
        state,
        width,
        0..state.transcript.len(),
        transcript_collapse_limit(state),
        state.theme,
        true,
    )
}

/// Render the whole transcript with every message and tool output expanded,
/// regardless of the session collapse setting. Used by the inline reader.
fn render_full_transcript_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    render_transcript_entry_range(
        state,
        width,
        0..state.transcript.len(),
        None,
        state.theme,
        false,
    )
}

/// Detail budget for the transcript: `None` when expanded, otherwise the
/// collapsed default for stable long prose and tool output.
fn transcript_collapse_limit(state: &AppState) -> Option<usize> {
    if state.expand_transcript_details {
        None
    } else {
        Some(TOOL_OUTPUT_COLLAPSED_LINES)
    }
}

fn render_transcript_entry_range(
    state: &AppState,
    width: u16,
    entry_range: Range<usize>,
    collapse_limit: Option<usize>,
    theme: TerminalTheme,
    compact_completed_turns: bool,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let turns = transcript_turns(state);
    for (offset, entry) in state.transcript[entry_range.clone()].iter().enumerate() {
        let entry_index = entry_range.start + offset;
        let compact_turn = if compact_completed_turns {
            turns.iter().find(|turn| {
                turn.is_compactable && (turn.prompt_index..turn.end).contains(&entry_index)
            })
        } else {
            None
        };
        // Completed successful tools are represented by the turn summary, so
        // skip their entry-specific rendering entirely.
        if matches!(entry, Entry::ToolCall(_) | Entry::SubagentToolCall(_))
            && compact_turn.is_some()
            && tool_entry_is_successful(state, entry)
        {
            continue;
        }
        let is_final_response = turns
            .iter()
            .any(|turn| turn.is_compactable && turn.final_response_index == Some(entry_index));
        let collapse_message = collapse_limit.is_some()
            && !is_final_response
            && transcript_entry_is_stable(state, entry_index, entry);
        if compact_turn.is_some() && is_final_response {
            push_turn_final_response_label(&mut out, theme);
        }
        match entry {
            Entry::UserPrompt(text) => {
                push_role_plain_message(
                    &mut out,
                    USER_GLYPH,
                    theme.user,
                    text,
                    collapse_message,
                    width,
                    theme,
                );
                if let Some(turn) = compact_turn {
                    push_turn_header(&mut out, turn.elapsed, theme);
                    if let Some(summary) = &turn.tool_summary {
                        push_turn_tool_summary(&mut out, summary, theme);
                    }
                }
            }
            Entry::AgentMessage(text) => {
                // Answers are the durable result of a turn. Keep every answer
                // fully readable while restoring the v1.0.2 role marker and
                // hanging indent that made message boundaries easy to scan.
                push_role_markdown_message(
                    &mut out,
                    AGENT_GLYPH,
                    theme.agent,
                    state.stream_visible_text(entry_index, text),
                    false,
                    width,
                    theme,
                )
            }
            Entry::SubagentMessage(text) => push_role_markdown_message(
                &mut out,
                SUBAGENT_GLYPH,
                theme.secondary,
                state.stream_visible_text(entry_index, text),
                false,
                width,
                theme,
            ),
            Entry::AgentThought(thought) => push_role_thinking(
                &mut out,
                (THOUGHT_GLYPH, theme.thought),
                state.stream_visible_text(entry_index, &thought.text),
                thought.completed,
                collapse_limit.is_some(),
                width,
                theme,
            ),
            Entry::SubagentThought(thought) => push_role_thinking(
                &mut out,
                (SUBAGENT_THOUGHT_GLYPH, theme.secondary),
                state.stream_visible_text(entry_index, &thought.text),
                thought.completed,
                collapse_limit.is_some(),
                width,
                theme,
            ),
            Entry::InternalMessage(message) => {
                let chars = message.text.chars().count();
                let title = match message.kind {
                    crate::event::InternalMessageKind::Delegation => {
                        format!(
                            "delegated to {} · {}",
                            message.target,
                            message_size_label(chars)
                        )
                    }
                    crate::event::InternalMessageKind::DiscreteReview => {
                        format!("discrete review brief · {}", message_size_label(chars))
                    }
                    crate::event::InternalMessageKind::ReviewLane => {
                        format!(
                            "review lane {} · {}",
                            message.source,
                            message_size_label(chars)
                        )
                    }
                    crate::event::InternalMessageKind::ReviewProgress => {
                        format!("review supervisor · {}", message_size_label(chars))
                    }
                    crate::event::InternalMessageKind::ReviewSynthesis => {
                        format!("review synthesis · {}", message_size_label(chars))
                    }
                };
                out.push(Line::from(Span::styled(
                    title,
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::BOLD),
                )));
                push_markdown_message(&mut out, &message.text, collapse_message, width, theme);
            }
            Entry::Plan(entries) | Entry::SubagentPlan(entries) => {
                let mut heading = Vec::new();
                if matches!(entry, Entry::SubagentPlan(_)) {
                    heading.push(Span::styled(
                        format!("{SUBAGENT_GLYPH} "),
                        Style::default()
                            .fg(theme.secondary)
                            .add_modifier(Modifier::BOLD),
                    ));
                    heading.push(Span::styled(
                        "subagent plan",
                        Style::default().fg(theme.tool).add_modifier(Modifier::BOLD),
                    ));
                } else {
                    heading.push(Span::styled(
                        "plan",
                        Style::default().fg(theme.tool).add_modifier(Modifier::BOLD),
                    ));
                }
                out.push(Line::from(heading));
                for e in entries {
                    out.push(plan_row(e, theme));
                }
                out.push(Line::from(""));
            }
            Entry::ToolCall(id) | Entry::SubagentToolCall(id) => {
                if let Some(view) = state.tool_calls.get(id) {
                    let color = tool_status_color(view.status, theme);
                    let terminal_exit_status = view.body.iter().rev().find_map(|output| {
                        if let ToolCallOutput::Terminal { exit_status, .. } = output {
                            exit_status.as_ref()
                        } else {
                            None
                        }
                    });
                    let status = match (view.status, terminal_exit_status) {
                        (agent_client_protocol::schema::v1::ToolCallStatus::Completed, _) => {
                            String::new()
                        }
                        (_, Some(_)) => String::new(),
                        _ => format!("[{}] ", tool_status_label(view.status)),
                    };
                    let mut spans = tool_header_spans(
                        view,
                        &status,
                        matches!(entry, Entry::SubagentToolCall(_)),
                        theme,
                    );
                    if let Some(exit_status) = terminal_exit_status {
                        spans.push(Span::styled(
                            format!(" · {}", terminal_header_outcome_label(exit_status)),
                            terminal_header_outcome_style(exit_status, theme),
                        ));
                    }
                    // Render the whole tool call — header plus outputs — into a
                    // temporary buffer, wrap each line to the width left of the
                    // gutter, then frame every resulting row with a colored left
                    // rail so the block reads as one unit, visually distinct from
                    // the role-marked agent prose around it. Wrapping here — rather
                    // than letting the transcript Paragraph wrap — keeps the rail
                    // on continuation rows; a rail prepended to a single logical
                    // line would land only on the first wrapped row. The rail
                    // color carries the tool status. See issue #257.
                    let content_width = width.saturating_sub(TOOL_GUTTER_WIDTH);
                    let mut block: Vec<Line<'static>> = vec![Line::from(spans)];
                    let tool_collapse_limit = match state.tool_detail_expanded(id) {
                        Some(true) => None,
                        Some(false) => Some(TOOL_OUTPUT_COLLAPSED_LINES),
                        None => collapse_limit,
                    };
                    push_tool_outputs(
                        &mut block,
                        &view.body,
                        view.status,
                        content_width,
                        tool_collapse_limit,
                        theme,
                    );
                    for line in block {
                        for row in wrap_tool_line(line, content_width as usize) {
                            out.push(with_tool_gutter(row, color));
                        }
                    }
                    // Consecutive tool calls read as one activity run: let
                    // their rails abut instead of separating every call with
                    // a blank row.
                    let next_is_tool_call =
                        state.transcript.get(entry_index + 1).is_some_and(|next| {
                            matches!(
                                next,
                                Entry::ToolCall(next_id) | Entry::SubagentToolCall(next_id)
                                    if state.tool_calls.contains_key(next_id)
                            )
                        });
                    if !next_is_tool_call {
                        out.push(Line::from(""));
                    }
                }
            }
            Entry::System(text) => {
                push_styled_message(&mut out, text, theme.accent, collapse_message, theme);
            }
            Entry::SessionBoundary(text) => {
                if !text.starts_with("subagent ·") {
                    out.push(Line::from(""));
                    out.push(session_boundary_line(text, width, theme));
                    out.push(Line::from(""));
                }
            }
        }
    }
    out
}

fn tool_entry_is_successful(state: &AppState, entry: &Entry) -> bool {
    let (Entry::ToolCall(id) | Entry::SubagentToolCall(id)) = entry else {
        return false;
    };
    state
        .tool_calls
        .get(id)
        .is_some_and(|view| view.status == ToolCallStatus::Completed)
}

fn push_turn_header(out: &mut Vec<Line<'static>>, elapsed: Option<Duration>, theme: TerminalTheme) {
    let label = elapsed
        .map(|elapsed| format!("agent · {}", format_duration(elapsed)))
        .unwrap_or_else(|| "agent".to_string());
    out.push(Line::from(Span::styled(
        label,
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    )));
}

fn push_turn_tool_summary(
    out: &mut Vec<Line<'static>>,
    summary: &TurnToolSummary,
    theme: TerminalTheme,
) {
    let mut facts = vec![format!(
        "{} {}",
        summary.tools,
        if summary.tools == 1 { "tool" } else { "tools" }
    )];
    if !summary.changed_paths.is_empty() {
        facts.push(format!(
            "{} {} changed",
            summary.changed_paths.len(),
            if summary.changed_paths.len() == 1 {
                "file"
            } else {
                "files"
            }
        ));
    }
    if summary.failures > 0 {
        facts.push(format!("{} failed", summary.failures));
    }
    out.push(Line::from(Span::styled(
        format!("│ {}", facts.join(" · ")),
        Style::default().fg(theme.muted),
    )));
}

fn push_turn_final_response_label(out: &mut Vec<Line<'static>>, theme: TerminalTheme) {
    out.push(Line::from(Span::styled(
        "└─ final response",
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    )));
}

fn session_boundary_line(text: &str, width: u16, theme: TerminalTheme) -> Line<'static> {
    let label = format!(" {text} ");
    let label_width = label.width();
    let total_width = usize::from(width);
    let remaining = total_width.saturating_sub(label_width);
    let left = remaining / 2;
    let right = remaining.saturating_sub(left);
    Line::from(vec![
        Span::styled("─".repeat(left), Style::default().fg(theme.muted)),
        Span::styled(
            label,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("─".repeat(right), Style::default().fg(theme.muted)),
    ])
}

/// Role glyphs and hanging indents restore the compact visual language used by
/// mj 1.0.2. A distinct subagent diamond preserves provenance introduced by
/// the newer multi-agent transcript model.
const USER_GLYPH: &str = "❯";
const AGENT_GLYPH: &str = "●";
const SUBAGENT_GLYPH: &str = "◆";
const THOUGHT_GLYPH: &str = "○";
const SUBAGENT_THOUGHT_GLYPH: &str = "◇";
const ROLE_GUTTER_WIDTH: u16 = 2;

fn push_role_plain_message(
    out: &mut Vec<Line<'static>>,
    glyph: &str,
    color: Color,
    text: &str,
    collapse: bool,
    width: u16,
    theme: TerminalTheme,
) {
    let (preview, collapsed) = message_preview(text, collapse);
    let content_width = usize::from(width.saturating_sub(ROLE_GUTTER_WIDTH)).max(1);
    let mut rows = Vec::new();
    for raw in preview.split('\n') {
        rows.extend(wrap_tool_line(Line::from(raw.to_string()), content_width));
    }
    if collapsed {
        push_message_collapse_hint(&mut rows, theme);
    }
    push_role_rows(out, glyph, color, rows);
}

fn push_role_markdown_message(
    out: &mut Vec<Line<'static>>,
    glyph: &str,
    color: Color,
    text: &str,
    collapse: bool,
    width: u16,
    theme: TerminalTheme,
) {
    let (preview, collapsed) = message_preview(text, collapse);
    let content_width = usize::from(width.saturating_sub(ROLE_GUTTER_WIDTH)).max(1);
    let mut rows = Vec::new();
    push_wrapped_role_markdown_lines(&mut rows, preview, content_width as u16, theme);
    if collapsed {
        push_message_collapse_hint(&mut rows, theme);
    }
    push_role_rows(out, glyph, color, rows);
}

fn push_role_thinking(
    out: &mut Vec<Line<'static>>,
    role: (&str, Color),
    source: &str,
    completed: bool,
    compact: bool,
    width: u16,
    theme: TerminalTheme,
) {
    let mut body = Vec::new();
    push_thinking(&mut body, source, completed, compact, theme);
    let content_width = usize::from(width.saturating_sub(ROLE_GUTTER_WIDTH)).max(1);
    let rows = body
        .into_iter()
        .flat_map(|line| wrap_tool_line(line, content_width))
        .collect();
    push_role_rows(out, role.0, role.1, rows);
}

/// Prefix the first visible row with a colored role glyph and keep every
/// continuation row aligned beneath the content. Empty Markdown rows remain
/// truly empty so paragraph spacing stays airy instead of turning into rails.
fn push_role_rows(
    out: &mut Vec<Line<'static>>,
    glyph: &str,
    color: Color,
    rows: Vec<Line<'static>>,
) {
    debug_assert_eq!(
        glyph.width() + 1,
        ROLE_GUTTER_WIDTH as usize,
        "role glyph marker must be exactly ROLE_GUTTER_WIDTH cells wide"
    );
    let mut glyph_pending = true;
    for row in rows {
        let row_is_empty = row.spans.iter().all(|span| span.content.trim().is_empty());
        if row_is_empty {
            out.push(Line::from(""));
            continue;
        }
        let marker = if glyph_pending {
            format!("{glyph} ")
        } else {
            " ".repeat(ROLE_GUTTER_WIDTH as usize)
        };
        glyph_pending = false;
        let mut spans = vec![Span::styled(
            marker,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )];
        spans.extend(row.spans);
        out.push(Line::from(spans));
    }
    if !glyph_pending {
        out.push(Line::from(""));
    }
}

const ACTIVE_THOUGHT_TAIL_LINES: usize = 3;
const ACTIVE_THOUGHT_TAIL_CHARS: usize = 360;

fn push_thinking(
    out: &mut Vec<Line<'static>>,
    source: &str,
    completed: bool,
    compact: bool,
    theme: TerminalTheme,
) {
    let mut in_html_comment = false;
    let text = source
        .split('\n')
        .map(|line| strip_html_comments(line, &mut in_html_comment))
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return;
    }
    let thought_style = Style::default().fg(theme.thought);
    if compact && completed {
        let lines = text.lines().count();
        let unit = if lines == 1 { "line" } else { "lines" };
        out.push(Line::from(Span::styled(
            format!("thought · {lines} {unit}"),
            thought_style,
        )));
    } else {
        let text = if compact {
            active_thought_tail(&text)
        } else {
            text
        };
        for line in text.lines() {
            out.push(Line::from(inline_markdown_spans_with_style(
                line,
                theme,
                thought_style,
            )));
        }
    }
}

fn active_thought_tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines
        .iter()
        .rev()
        .take(ACTIVE_THOUGHT_TAIL_LINES)
        .copied()
        .collect::<Vec<_>>();
    let mut tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
    if tail.chars().count() > ACTIVE_THOUGHT_TAIL_CHARS {
        let keep = tail.chars().count() - ACTIVE_THOUGHT_TAIL_CHARS;
        tail = format!("…{}", tail.chars().skip(keep).collect::<String>());
    }
    tail
}

fn push_styled_message(
    out: &mut Vec<Line<'static>>,
    text: &str,
    color: Color,
    collapse: bool,
    theme: TerminalTheme,
) {
    let (preview, collapsed) = message_preview(text, collapse);
    for raw in preview.split('\n') {
        out.push(Line::from(Span::styled(
            raw.to_string(),
            Style::default().fg(color),
        )));
    }
    if collapsed {
        push_message_collapse_hint(out, theme);
    }
    out.push(Line::from(""));
}

fn push_markdown_message(
    out: &mut Vec<Line<'static>>,
    text: &str,
    collapse: bool,
    width: u16,
    theme: TerminalTheme,
) {
    let (preview, collapsed) = message_preview(text, collapse);
    // Pre-wrap role prose here instead of leaving it to Paragraph. Markdown
    // prefixes need a different continuation indentation from ordinary prose,
    // and Paragraph only sees the already-flattened logical line.
    push_wrapped_role_markdown_lines(out, preview, width, theme);
    if collapsed {
        push_message_collapse_hint(out, theme);
    }
    out.push(Line::from(""));
}

fn push_wrapped_role_markdown_lines(
    out: &mut Vec<Line<'static>>,
    text: String,
    width: u16,
    theme: TerminalTheme,
) {
    let mut markdown = Vec::new();
    push_markdown_lines(&mut markdown, text, 0, width, theme);
    for line in markdown {
        out.extend(wrap_markdown_line(line, width as usize));
    }
}

fn message_preview(text: &str, collapse: bool) -> (String, bool) {
    let total_chars = text.chars().count();
    let total_lines = text.split('\n').count();
    let collapsed = collapse
        && (total_chars > MESSAGE_COLLAPSED_CHARS || total_lines > MESSAGE_COLLAPSED_LINES);
    if !collapsed {
        return (text.to_string(), false);
    }

    let mut preview = String::new();
    let mut remaining = MESSAGE_COLLAPSED_CHARS;
    for (index, line) in text.split('\n').take(MESSAGE_COLLAPSED_LINES).enumerate() {
        if index > 0 {
            if remaining == 0 {
                break;
            }
            preview.push('\n');
            remaining -= 1;
        }
        if remaining == 0 {
            break;
        }
        let mut taken = 0;
        for ch in line.chars().take(remaining) {
            preview.push(ch);
            taken += 1;
        }
        remaining -= taken;
        if taken < line.chars().count() {
            break;
        }
    }
    (preview, true)
}

fn push_message_collapse_hint(out: &mut Vec<Line<'static>>, theme: TerminalTheme) {
    out.push(Line::from(Span::styled(
        "… details hidden",
        Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::ITALIC),
    )));
}

fn message_size_label(chars: usize) -> String {
    if chars >= 1_000 {
        format!("{:.1}k chars", chars as f64 / 1_000.0)
    } else {
        format!("{chars} chars")
    }
}

fn push_markdown_lines(
    out: &mut Vec<Line<'static>>,
    text: String,
    indent: usize,
    width: u16,
    theme: TerminalTheme,
) {
    push_markdown_lines_limited_inner(out, text, indent, width, None, theme, false);
}

fn push_tool_markdown_lines_limited(
    out: &mut Vec<Line<'static>>,
    text: String,
    indent: usize,
    width: u16,
    collapse_limit: Option<usize>,
    theme: TerminalTheme,
) {
    let (_, hidden) = tool_output_preview(&text, collapse_limit);
    if let Some(ToolOutputHidden::Lines(lines)) = hidden {
        push_tool_collapse_hint(out, indent, ToolOutputHidden::Lines(lines), theme);
        push_markdown_lines_limited_inner(out, text, indent, width, collapse_limit, theme, true);
    } else {
        let (preview, hidden) = tool_output_preview(&text, collapse_limit);
        push_markdown_lines_limited_inner(out, preview, indent, width, None, theme, true);
        if let Some(ToolOutputHidden::Details) = hidden {
            push_tool_collapse_hint(out, indent, ToolOutputHidden::Details, theme);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolOutputHidden {
    Lines(usize),
    Details,
}

fn tool_output_preview(
    text: &str,
    collapse_limit: Option<usize>,
) -> (String, Option<ToolOutputHidden>) {
    let Some(line_limit) = collapse_limit else {
        return (text.to_string(), None);
    };
    let total_chars = text.chars().count();
    let total_lines = text.split('\n').count();
    let chars_over = total_chars > TOOL_OUTPUT_COLLAPSED_CHARS;
    let lines_over = total_lines > line_limit;
    if !chars_over && !lines_over {
        return (text.to_string(), None);
    }

    if lines_over && !chars_over {
        let lines: Vec<&str> = text.split('\n').collect();
        let hidden = lines.len().saturating_sub(line_limit);
        return (
            lines[hidden..].join("\n"),
            Some(ToolOutputHidden::Lines(hidden)),
        );
    }

    let mut preview = String::new();
    let mut remaining_chars = TOOL_OUTPUT_COLLAPSED_CHARS;
    for (index, line) in text.split('\n').take(line_limit).enumerate() {
        if index > 0 {
            if remaining_chars == 0 {
                break;
            }
            preview.push('\n');
            remaining_chars -= 1;
        }
        for ch in line.chars().take(remaining_chars) {
            preview.push(ch);
            remaining_chars -= 1;
        }
        if remaining_chars == 0 {
            break;
        }
    }

    let hidden = if chars_over {
        ToolOutputHidden::Details
    } else {
        ToolOutputHidden::Lines(total_lines.saturating_sub(line_limit))
    };
    (preview, Some(hidden))
}

fn push_markdown_lines_limited_inner(
    out: &mut Vec<Line<'static>>,
    text: String,
    indent: usize,
    width: u16,
    collapse_limit: Option<usize>,
    theme: TerminalTheme,
    use_tool_output_style: bool,
) {
    let prefix = " ".repeat(indent);
    let mut code_fence: Option<(char, usize)> = None;
    let mut in_html_comment = false;
    let mut code_lang = String::new();
    let lines: Vec<&str> = text.split('\n').collect();
    // Collapse keeps the *tail*: for tool output the end is where the signal
    // lives (the error, the test summary, the exit status), so hiding the head
    // keeps exactly the lines the user wanted. The hint sits on top, standing
    // in for the elided head.
    let hidden = collapsed_head_len(lines.len(), collapse_limit);
    // Replay parser state across the hidden head so a tail that starts inside
    // a code block or HTML comment renders consistently with the full text.
    for raw in &lines[..hidden] {
        if let Some((marker, length)) = code_fence {
            if markdown_fence(raw).is_some_and(|(next, count, _)| next == marker && count >= length)
            {
                code_fence = None;
            }
        } else {
            let filtered = strip_html_comments(raw, &mut in_html_comment);
            if !in_html_comment && let Some((marker, length, _)) = markdown_fence(&filtered) {
                code_fence = Some((marker, length));
            }
        }
    }
    if hidden > 0 {
        push_collapse_hint(out, indent, hidden, theme);
    }
    let mut line_index = hidden;
    while line_index < lines.len() {
        let original = lines[line_index];
        if let Some((marker, length)) = code_fence {
            if markdown_fence(original)
                .is_some_and(|(next, count, _)| next == marker && count >= length)
            {
                code_fence = None;
                code_lang.clear();
                line_index += 1;
                continue;
            }
            out.push(Line::from(Span::styled(
                format!("{prefix}  {original}"),
                Style::default().fg(theme.quote),
            )));
            line_index += 1;
            continue;
        }

        let filtered = strip_html_comments(original, &mut in_html_comment);
        if filtered.trim().is_empty() && !original.trim().is_empty() {
            line_index += 1;
            continue;
        }
        let raw = filtered.as_str();
        if !in_html_comment && let Some((marker, length, language)) = markdown_fence(raw) {
            code_fence = Some((marker, length));
            code_lang = language.to_string();
            let title = if code_lang.is_empty() {
                "code".to_string()
            } else {
                format!("code {code_lang}")
            };
            out.push(Line::from(Span::styled(
                format!("{prefix}{title}"),
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            )));
            line_index += 1;
            continue;
        }
        let trimmed = raw.trim_start();

        if raw.trim().is_empty() {
            out.push(Line::from(""));
            line_index += 1;
            continue;
        }

        let base_style = if use_tool_output_style {
            tool_output_line_style(raw, theme)
        } else {
            Style::default()
        };

        if let Some(header) = markdown_table_header(raw, lines.get(line_index + 1)) {
            push_markdown_table_row(out, &prefix, &header, true, theme, base_style);
            line_index += 2;
            while let Some(row) = lines
                .get(line_index)
                .and_then(|row| markdown_table_row(row))
            {
                push_markdown_table_row(out, &prefix, &row, false, theme, base_style);
                line_index += 1;
            }
            continue;
        }

        if let Some((level, heading)) = markdown_heading(raw) {
            let marker = "#".repeat(level);
            let heading_style =
                markdown_heading_style(level, theme, base_style, use_tool_output_style);
            out.push(Line::from(vec![
                Span::styled(format!("{prefix}{marker} "), heading_style),
                Span::styled(heading.to_string(), heading_style),
            ]));
            line_index += 1;
            continue;
        }

        if markdown_rule(raw) {
            out.push(Line::from(Span::styled(
                format!(
                    "{prefix}{}",
                    "─".repeat(usize::from(width).saturating_sub(indent).max(1))
                ),
                base_style.fg(if use_tool_output_style {
                    theme.subtle
                } else {
                    theme.muted
                }),
            )));
            line_index += 1;
            continue;
        }

        if let Some(quoted) = trimmed.strip_prefix("> ") {
            out.push(Line::from(vec![
                Span::styled(format!("{prefix}> "), Style::default().fg(theme.muted)),
                Span::styled(quoted.to_string(), Style::default().fg(theme.quote)),
            ]));
            line_index += 1;
            continue;
        }

        if let Some((source_indent, item)) = markdown_unordered_item(raw) {
            let mut spans = vec![Span::styled(
                format!("{prefix}{source_indent}- "),
                Style::default().fg(theme.muted),
            )];
            spans.extend(inline_markdown_spans_with_style(item, theme, base_style));
            out.push(Line::from(spans));
            line_index += 1;
            continue;
        }

        if let Some((source_indent, number, item)) = markdown_ordered_item(raw) {
            let mut spans = vec![Span::styled(
                format!("{prefix}{source_indent}{number}. "),
                Style::default().fg(theme.muted),
            )];
            spans.extend(inline_markdown_spans_with_style(item, theme, base_style));
            out.push(Line::from(spans));
            line_index += 1;
            continue;
        }

        let mut spans = vec![Span::styled(prefix.clone(), base_style)];
        spans.extend(inline_markdown_spans_with_style(raw, theme, base_style));
        out.push(Line::from(spans));
        line_index += 1;
    }
}

fn markdown_fence(raw: &str) -> Option<(char, usize, &str)> {
    let trimmed = raw.trim_start();
    let marker = trimmed.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let length = trimmed.chars().take_while(|ch| *ch == marker).count();
    (length >= 3).then(|| (marker, length, trimmed[length..].trim()))
}

fn strip_html_comments(raw: &str, in_comment: &mut bool) -> String {
    let mut visible = String::with_capacity(raw.len());
    let mut index = 0;

    while index < raw.len() {
        if *in_comment {
            let Some(relative_end) = raw[index..].find("-->") else {
                return visible;
            };
            *in_comment = false;
            index += relative_end + 3;
            continue;
        }

        if raw[index..].starts_with("<!--") {
            *in_comment = true;
            index += 4;
            continue;
        }

        if raw.as_bytes()[index] == b'`' {
            let delimiter_len = raw[index..]
                .bytes()
                .take_while(|byte| *byte == b'`')
                .count();
            let delimiter = &raw[index..index + delimiter_len];
            if let Some(relative_end) = raw[index + delimiter_len..].find(delimiter) {
                let end = index + delimiter_len + relative_end + delimiter_len;
                visible.push_str(&raw[index..end]);
                index = end;
                continue;
            }
        }

        let ch = raw[index..]
            .chars()
            .next()
            .expect("valid character boundary");
        visible.push(ch);
        index += ch.len_utf8();
    }

    visible
}

fn markdown_heading(raw: &str) -> Option<(usize, &str)> {
    let trimmed = raw.trim_start();
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&level) && trimmed.as_bytes().get(level) == Some(&b' ') {
        Some((level, trimmed[level + 1..].trim()))
    } else {
        None
    }
}

fn markdown_rule(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.len() >= 3
        && (trimmed.chars().all(|c| c == '-')
            || trimmed.chars().all(|c| c == '*')
            || trimmed.chars().all(|c| c == '_'))
}

fn markdown_table_header<'a>(raw: &'a str, next: Option<&&str>) -> Option<Vec<&'a str>> {
    let header = markdown_table_row(raw)?;
    let separator = markdown_table_row(next?)?;
    (header.len() == separator.len()
        && header.len() >= 2
        && separator
            .iter()
            .all(|cell| markdown_table_separator_cell(cell)))
    .then_some(header)
}

fn markdown_table_row(raw: &str) -> Option<Vec<&str>> {
    let trimmed = raw.trim();
    trimmed.contains('|').then(|| {
        trimmed
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect()
    })
}

fn markdown_table_separator_cell(cell: &str) -> bool {
    let content = cell.trim_matches(':');
    content.len() >= 3 && content.chars().all(|ch| ch == '-')
}

fn push_markdown_table_row(
    out: &mut Vec<Line<'static>>,
    prefix: &str,
    cells: &[&str],
    header: bool,
    theme: TerminalTheme,
    base_style: Style,
) {
    let mut spans = vec![Span::styled(prefix.to_string(), base_style)];
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" | ", base_style.fg(theme.muted)));
        }
        let style = if header {
            base_style.add_modifier(Modifier::BOLD)
        } else {
            base_style
        };
        spans.extend(inline_markdown_spans_with_style(cell, theme, style));
    }
    out.push(Line::from(spans));
}

fn markdown_heading_style(
    level: usize,
    theme: TerminalTheme,
    base_style: Style,
    tool_output: bool,
) -> Style {
    if tool_output {
        return base_style.add_modifier(match level {
            1 | 2 => Modifier::BOLD,
            3 | 4 => Modifier::UNDERLINED,
            _ => Modifier::ITALIC,
        });
    }
    match level {
        1 => Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
        2 => Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
        3 => Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        4 => Style::default()
            .fg(theme.secondary)
            .add_modifier(Modifier::BOLD),
        5 => Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::UNDERLINED),
        _ => Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::ITALIC),
    }
}

fn markdown_unordered_item(raw: &str) -> Option<(&str, &str)> {
    let source_indent = &raw[..raw.len() - raw.trim_start().len()];
    let trimmed = raw.trim_start();
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .map(|item| (source_indent, item))
}

fn markdown_ordered_item(raw: &str) -> Option<(&str, &str, &str)> {
    let source_indent = &raw[..raw.len() - raw.trim_start().len()];
    let trimmed = raw.trim_start();
    let dot = trimmed.find(". ")?;
    let number = &trimmed[..dot];
    if number.chars().all(|c| c.is_ascii_digit()) {
        Some((source_indent, number, &trimmed[dot + 2..]))
    } else {
        None
    }
}

fn inline_markdown_spans_with_style(
    raw: &str,
    theme: TerminalTheme,
    base_style: Style,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = raw;
    let mut previous = None;
    while !rest.is_empty() {
        if let Some(after_label) = rest.strip_prefix('[')
            && let Some(label_end) = after_label.find("](")
            && let Some(url_end) = after_label[label_end + 2..].find(')')
        {
            let label = &after_label[..label_end];
            let url_start = label_end + 2;
            let url = &after_label[url_start..url_start + url_end];
            spans.extend(inline_markdown_spans_with_style(label, theme, base_style));
            spans.push(Span::styled(
                format!(" ({url})"),
                base_style.fg(theme.muted),
            ));
            rest = &after_label[url_start + url_end + 1..];
            previous = url.chars().next_back();
            continue;
        }
        if let Some(after) = rest.strip_prefix("`")
            && let Some(end) = after.find('`')
        {
            let (code, tail) = after.split_at(end);
            spans.push(Span::styled(code.to_string(), base_style.fg(theme.code)));
            rest = &tail[1..];
            previous = code.chars().next_back();
            continue;
        }
        if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**")
        {
            let (strong, tail) = after.split_at(end);
            spans.extend(inline_markdown_spans_with_style(
                strong,
                theme,
                base_style.add_modifier(Modifier::BOLD),
            ));
            rest = &tail[2..];
            previous = strong.chars().next_back();
            continue;
        }
        if let Some(after) = rest.strip_prefix("__")
            && underscore_can_open(previous, after)
            && let Some(end) = find_underscore_emphasis_end(after, "__")
        {
            let (strong, tail) = after.split_at(end);
            spans.extend(inline_markdown_spans_with_style(
                strong,
                theme,
                base_style.add_modifier(Modifier::BOLD),
            ));
            rest = &tail[2..];
            previous = strong.chars().next_back();
            continue;
        }
        if let Some(after) = rest.strip_prefix("*")
            && let Some(end) = after.find('*')
        {
            let (em, tail) = after.split_at(end);
            spans.extend(inline_markdown_spans_with_style(
                em,
                theme,
                base_style.add_modifier(Modifier::ITALIC),
            ));
            rest = &tail[1..];
            previous = em.chars().next_back();
            continue;
        }
        if let Some(after) = rest.strip_prefix("_")
            && underscore_can_open(previous, after)
            && let Some(end) = find_underscore_emphasis_end(after, "_")
        {
            let (em, tail) = after.split_at(end);
            spans.extend(inline_markdown_spans_with_style(
                em,
                theme,
                base_style.add_modifier(Modifier::ITALIC),
            ));
            rest = &tail[1..];
            previous = em.chars().next_back();
            continue;
        }

        let next = rest
            .char_indices()
            .skip(1)
            .find_map(|(idx, ch)| (ch == '`' || ch == '*' || ch == '_' || ch == '[').then_some(idx))
            .unwrap_or(rest.len());
        let (plain, tail) = rest.split_at(next);
        spans.push(Span::styled(plain.to_string(), base_style));
        previous = plain.chars().next_back().or(previous);
        rest = tail;
    }
    spans
}

fn underscore_can_open(previous: Option<char>, after: &str) -> bool {
    let Some(next) = after.chars().next() else {
        return false;
    };
    !next.is_whitespace() && !previous.is_some_and(|ch| ch.is_alphanumeric())
}

fn find_underscore_emphasis_end(after: &str, marker: &str) -> Option<usize> {
    after.match_indices(marker).find_map(|(idx, _)| {
        let before = after[..idx].chars().next_back()?;
        let next = after[idx + marker.len()..].chars().next();
        (!(before.is_whitespace()
            || before.is_alphanumeric() && next.is_some_and(|ch| ch.is_alphanumeric())))
        .then_some(idx)
    })
}

/// Left rail drawn before every line of a tool-call block, and its width in
/// cells. The rail frames tool output as a distinct unit so it never blurs
/// into the role-marked agent messages around it. See issue #257. The two must
/// stay in sync; the `debug_assert` in `with_tool_gutter` guards against drift
/// if the glyph ever changes (`str::width` is not usable in a `const`).
const TOOL_GUTTER: &str = "│ ";
const TOOL_GUTTER_WIDTH: u16 = 2;

/// Prefix an already-rendered tool-call line with the colored gutter rail.
/// The color reflects the tool's status (green when done, red on failure, …)
/// so a glance at the rail communicates both "this is a tool block" and how
/// it ended.
fn with_tool_gutter(line: Line<'static>, color: Color) -> Line<'static> {
    debug_assert_eq!(TOOL_GUTTER.width(), TOOL_GUTTER_WIDTH as usize);
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::styled(TOOL_GUTTER, Style::default().fg(color)));
    spans.extend(line.spans);
    Line::from(spans)
}

/// Word-wrap a rendered tool-call line to `width` display cells, preserving
/// each span's style, and return one `Line` per visual row. Doing the wrap
/// here — instead of relying on the transcript `Paragraph`'s own wrapping —
/// lets the caller prefix every row with the gutter rail; a rail prepended to
/// one logical line would otherwise appear only on the first wrapped row,
/// leaving continuation rows reading as un-railed prose (issue #257).
///
/// Wrapping mirrors [`wrap_text_to_width`]: break between words, drop the
/// whitespace at a break, and hard-split a word longer than `width`. Leading
/// indentation on the first row is preserved — it is meaningful for tool
/// output — so only whitespace pushed past the edge is dropped.
fn wrap_tool_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    wrap_markdown_line(line, width)
}

/// Wrap a rendered Markdown line while retaining its leading block indentation
/// and, for recognized list and quote prefixes, hanging subsequent rows under
/// the item text. Styles stay attached to individual characters as wrapping
/// crosses inline spans.
fn wrap_markdown_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);

    // Flatten to (char, style) so wrapping can cross span boundaries while
    // keeping each character's original style, then regroup into tokens of
    // one whitespace-ness (a run of spaces or a run of word characters).
    let mut tokens: Vec<Vec<(char, Style)>> = Vec::new();
    let mut token: Vec<(char, Style)> = Vec::new();
    let mut token_ws: Option<bool> = None;
    for span in &line.spans {
        for ch in span.content.chars() {
            let is_ws = ch.is_whitespace();
            if token_ws != Some(is_ws) {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
                token_ws = Some(is_ws);
            }
            token.push((ch, span.style));
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }

    let cell_width =
        |t: &[(char, Style)]| t.iter().map(|(c, _)| c.width().unwrap_or(0)).sum::<usize>();
    let continuation_width = markdown_continuation_width(&line);
    let continuation_style = line
        .spans
        .first()
        .map(|span| span.style)
        .unwrap_or_default();
    // Leave at least one cell for content on narrow terminals. This keeps the
    // result bounded even when a source indentation is wider than the viewport.
    let continuation_width = continuation_width.min(width.saturating_sub(1));
    let continuation = vec![(' ', continuation_style); continuation_width];

    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_w = 0usize;
    for tok in tokens {
        let tok_w = cell_width(&tok);
        if cur_w + tok_w <= width {
            cur.extend(tok);
            cur_w += tok_w;
            continue;
        }
        let is_ws = tok.first().is_some_and(|(c, _)| c.is_whitespace());
        if is_ws {
            // Break here; the run of whitespace at the break is dropped.
            rows.push(std::mem::take(&mut cur));
            cur = continuation.clone();
            cur_w = continuation_width;
        } else if tok_w + continuation_width <= width {
            // Word fits on a fresh row.
            if cur.len() > continuation.len() {
                while cur.last().is_some_and(|(ch, _)| ch.is_whitespace()) {
                    cur.pop();
                }
                rows.push(std::mem::take(&mut cur));
            }
            cur = continuation.clone();
            cur.extend(tok);
            cur_w = continuation_width + tok_w;
        } else {
            // Word longer than a full row: fill the current row, then hard-split.
            for (ch, style) in tok {
                let ch_w = ch.width().unwrap_or(0);
                if cur_w + ch_w > width && !cur.is_empty() {
                    while cur.last().is_some_and(|(ch, _)| ch.is_whitespace()) {
                        cur.pop();
                    }
                    rows.push(std::mem::take(&mut cur));
                    cur = continuation.clone();
                    cur_w = continuation_width;
                }
                cur.push((ch, style));
                cur_w += ch_w;
            }
        }
    }
    // Keep a final partial row, and preserve blank lines as one empty row so
    // the gutter rail runs unbroken through them.
    if !cur.is_empty() || rows.is_empty() {
        rows.push(cur);
    }

    rows.into_iter()
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut buf = String::new();
            let mut buf_style: Option<Style> = None;
            for (ch, style) in row {
                if buf_style != Some(style) {
                    if let Some(prev) = buf_style {
                        spans.push(Span::styled(std::mem::take(&mut buf), prev));
                    }
                    buf_style = Some(style);
                }
                buf.push(ch);
            }
            if let Some(prev) = buf_style {
                spans.push(Span::styled(buf, prev));
            }
            Line::from(spans)
        })
        .collect()
}

/// The Markdown parser above recognizes only `- ` / `* `, ASCII-numbered
/// items, and `> ` quotes. Compute the matching rendered prefix in display
/// cells so continuation rows align beneath the text rather than the marker.
/// For ordinary tool output, retaining its initial indentation is still useful
/// block indentation and prevents continuation rows from jumping left.
fn markdown_continuation_width(line: &Line<'_>) -> usize {
    let text = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    let leading_end = text
        .char_indices()
        .find_map(|(index, ch)| (!ch.is_whitespace()).then_some(index))
        .unwrap_or(text.len());
    let rest = &text[leading_end..];
    let prefix_len = if rest.starts_with("> ") || rest.starts_with("- ") {
        leading_end + 2
    } else {
        let digits = rest.chars().take_while(|ch| ch.is_ascii_digit()).count();
        if digits > 0 && rest[digits..].starts_with(". ") {
            leading_end + digits + 2
        } else {
            leading_end
        }
    };
    text[..prefix_len].width()
}

fn push_tool_outputs(
    out: &mut Vec<Line<'static>>,
    outputs: &[ToolCallOutput],
    tool_status: agent_client_protocol::schema::v1::ToolCallStatus,
    width: u16,
    collapse_limit: Option<usize>,
    theme: TerminalTheme,
) {
    for output in outputs {
        match output {
            ToolCallOutput::Text(text) => {
                push_tool_markdown_lines_limited(out, text.clone(), 2, width, collapse_limit, theme)
            }
            ToolCallOutput::Diff {
                path,
                old_text,
                new_text,
            } => push_diff_output(
                out,
                path,
                old_text.as_deref(),
                new_text,
                width,
                collapse_limit,
                theme,
            ),
            ToolCallOutput::Terminal {
                output,
                truncated,
                exit_status,
                ..
            } => {
                if *truncated {
                    out.push(Line::from(Span::styled(
                        "  [output truncated]",
                        Style::default().fg(theme.muted),
                    )));
                }
                if !output.trim().is_empty() {
                    push_tool_text_lines(
                        out,
                        output.trim_end_matches(['\r', '\n']).to_string(),
                        2,
                        collapse_limit,
                        theme,
                    );
                } else if exit_status.is_some() {
                    out.push(Line::from(Span::styled(
                        "  no stdout/stderr captured",
                        Style::default().fg(theme.muted),
                    )));
                } else {
                    let state = terminal_empty_state_label(tool_status);
                    out.push(Line::from(Span::styled(
                        format!("  {state}"),
                        Style::default().fg(theme.muted),
                    )));
                }
            }
            ToolCallOutput::Note(note) => {
                out.push(Line::from(Span::styled(
                    format!("  [{note}]"),
                    Style::default().fg(theme.muted),
                )));
            }
        }
    }
}

fn terminal_empty_state_label(tool_status: ToolCallStatus) -> &'static str {
    match tool_status {
        ToolCallStatus::Pending | ToolCallStatus::InProgress => "waiting for output",
        _ => "no terminal output received",
    }
}

fn terminal_exit_status_label(
    status: &agent_client_protocol::schema::v1::TerminalExitStatus,
) -> String {
    match (&status.exit_code, &status.signal) {
        (Some(code), Some(signal)) => format!("code {code}, signal {signal}"),
        (Some(code), None) => format!("code {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "unknown".to_string(),
    }
}

fn terminal_header_outcome_label(
    status: &agent_client_protocol::schema::v1::TerminalExitStatus,
) -> String {
    match (&status.exit_code, &status.signal) {
        (Some(code), Some(signal)) => format!("exit {code}, signal {signal}"),
        (Some(code), None) => format!("exit {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "exit unknown".to_string(),
    }
}

fn terminal_header_outcome_style(
    status: &agent_client_protocol::schema::v1::TerminalExitStatus,
    theme: TerminalTheme,
) -> Style {
    if status.exit_code == Some(0) && status.signal.is_none() {
        Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::ITALIC)
    } else {
        Style::default()
            .fg(theme.error)
            .add_modifier(Modifier::BOLD)
    }
}

fn push_tool_text_lines(
    out: &mut Vec<Line<'static>>,
    text: String,
    indent: usize,
    collapse_limit: Option<usize>,
    theme: TerminalTheme,
) {
    let (preview, hidden) = tool_output_preview(&text, collapse_limit);
    let prefix = " ".repeat(indent);
    for raw in preview.split('\n') {
        let line = format!("{prefix}{raw}");
        out.push(Line::from(Span::styled(
            line,
            tool_output_line_style(raw, theme),
        )));
    }
    if let Some(hidden) = hidden {
        push_tool_collapse_hint(out, indent, hidden, theme);
    }
}

fn push_tool_collapse_hint(
    out: &mut Vec<Line<'static>>,
    indent: usize,
    hidden: ToolOutputHidden,
    theme: TerminalTheme,
) {
    match hidden {
        ToolOutputHidden::Lines(lines) => push_collapse_hint(out, indent, lines, theme),
        ToolOutputHidden::Details => {
            let prefix = " ".repeat(indent);
            out.push(Line::from(Span::styled(
                format!("{prefix}… details hidden"),
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::ITALIC),
            )));
        }
    }
}

/// Number of leading lines to hide so a collapsed markdown block keeps its
/// last `limit` lines. Returns `0` when there is no limit or the block fits.
fn collapsed_head_len(total_lines: usize, collapse_limit: Option<usize>) -> usize {
    match collapse_limit {
        Some(limit) if total_lines > limit => total_lines - limit,
        _ => 0,
    }
}

/// Leading "K earlier lines hidden" hint shown above collapsed tool outputs
/// so the user can tell the head was elided rather than assuming the output
/// started there.
fn push_collapse_hint(
    out: &mut Vec<Line<'static>>,
    indent: usize,
    hidden: usize,
    theme: TerminalTheme,
) {
    let prefix = " ".repeat(indent);
    out.push(Line::from(Span::styled(
        format!("{prefix}... {hidden} earlier lines hidden"),
        Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::ITALIC),
    )));
}

fn tool_output_line_style(raw: &str, theme: TerminalTheme) -> Style {
    let lower = raw.to_ascii_lowercase();
    let trimmed = lower.trim_start();
    let failed = word_is_nonzero_or_uncounted(&lower, "failed");
    let error = trimmed == "error"
        || trimmed.starts_with("error:")
        || trimmed.starts_with("error[")
        || trimmed.starts_with("fatal:")
        || lower.contains("panicked at");
    let success = contains_word(&lower, "success")
        || contains_word(&lower, "successful")
        || contains_word(&lower, "passed") && !failed
        || lower == "ok"
        || lower.ends_with(" ok")
        || lower.contains("test result: ok");
    if error || failed {
        Style::default()
            .fg(theme.error)
            .add_modifier(Modifier::BOLD)
    } else if contains_word(&lower, "warning") || contains_word(&lower, "warn") {
        Style::default().fg(theme.warning)
    } else if success {
        Style::default().fg(theme.success)
    } else if raw.trim_start().starts_with('$') {
        Style::default().fg(theme.primary)
    } else {
        Style::default().fg(theme.subtle)
    }
}

fn contains_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + word.len()..].chars().next();
        !before.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            && !after.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    })
}

fn word_is_nonzero_or_uncounted(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(start, _)| {
        let before_char = text[..start].chars().next_back();
        let after_char = text[start + word.len()..].chars().next();
        if before_char.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            || after_char.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            return false;
        }
        let before = text[..start].trim_end();
        let count = before
            .split_whitespace()
            .next_back()
            .and_then(|token| token.parse::<u64>().ok());
        count != Some(0)
    })
}

fn push_diff_output(
    out: &mut Vec<Line<'static>>,
    path: &str,
    old_text: Option<&str>,
    new_text: &str,
    width: u16,
    collapse_limit: Option<usize>,
    theme: TerminalTheme,
) {
    let diff_budget = collapse_limit.unwrap_or(80);
    let rows = prepared_diff_rows(old_text, new_text, diff_budget);

    let added = rows
        .iter()
        .filter(|row| row.kind == DiffLineKind::Added)
        .count();
    let removed = rows
        .iter()
        .filter(|row| row.kind == DiffLineKind::Removed)
        .count();
    let mut header = vec![
        Span::styled("  diff ", Style::default().fg(theme.muted)),
        Span::styled(path.to_string(), Style::default().fg(theme.primary)),
    ];
    if added > 0 {
        header.push(Span::styled(
            format!("  +{added}"),
            Style::default().fg(theme.diff_added),
        ));
    }
    if removed > 0 {
        header.push(Span::styled(
            format!(" -{removed}"),
            Style::default().fg(theme.diff_removed),
        ));
    }
    out.push(Line::from(header));

    out.extend(render_prepared_diff_rows(&rows, width, theme));
}

/// The shared native-diff preparation pipeline. The transcript uses its
/// compact budget; the dedicated reader passes an unlimited budget.
fn prepared_diff_rows(old_text: Option<&str>, new_text: &str, limit: usize) -> Vec<DiffLine> {
    let old_lines: Vec<&str> = old_text.unwrap_or("").lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    compact_line_diff(&old_lines, &new_lines, limit)
}

fn render_prepared_diff_rows(
    rows: &[DiffLine],
    width: u16,
    theme: TerminalTheme,
) -> Vec<Line<'static>> {
    let gutter_width = rows
        .iter()
        .filter_map(DiffLine::gutter_line)
        .max()
        .map_or(1, |number| number.to_string().len());
    rows.iter()
        .map(|row| render_diff_row(row, gutter_width, width as usize, theme))
        .collect()
}

/// Dedicated diff readers wrap complete rows in their `Paragraph`; unlike the
/// compact transcript renderer, they must never discard the tail of a line.
fn render_prepared_diff_rows_full(rows: &[DiffLine], theme: TerminalTheme) -> Vec<Line<'static>> {
    let gutter_width = rows
        .iter()
        .filter_map(DiffLine::gutter_line)
        .max()
        .map_or(1, |number| number.to_string().len());
    rows.iter()
        .map(|row| render_diff_row_full(row, gutter_width, theme))
        .collect()
}

fn render_diff_row_full(
    row: &DiffLine,
    gutter_width: usize,
    theme: TerminalTheme,
) -> Line<'static> {
    if row.kind == DiffLineKind::Omitted {
        return Line::from(Span::styled(
            format!("  {:>gutter_width$} ··· {}", "", row.text()),
            Style::default().fg(theme.muted),
        ));
    }
    let (marker, accent, row_bg, emph_bg) = match row.kind {
        DiffLineKind::Added => (
            "+",
            theme.diff_added,
            theme.diff_added_bg,
            theme.diff_added_emph_bg,
        ),
        DiffLineKind::Removed => (
            "-",
            theme.diff_removed,
            theme.diff_removed_bg,
            theme.diff_removed_emph_bg,
        ),
        _ => (" ", theme.diff_context, None, None),
    };
    let number = row
        .gutter_line()
        .map_or_else(String::new, |number| number.to_string());
    let prefix = format!("  {number:>gutter_width$} {marker} ");
    let mut spans = vec![Span::styled(
        prefix,
        match row_bg {
            Some(bg) => Style::default().fg(accent).bg(bg),
            None => Style::default().fg(accent),
        },
    )];
    spans.extend(row.segments.iter().map(|segment| {
        let style = match (row_bg, segment.emphasized.then_some(emph_bg).flatten()) {
            (None, _) => Style::default().fg(accent),
            (Some(bg), None) => Style::default().fg(theme.text).bg(bg),
            (Some(_), Some(emph)) => Style::default().fg(theme.text).bg(emph),
        };
        Span::styled(segment.text.clone(), style)
    }));
    Line::from(spans)
}

fn render_diff_row(
    row: &DiffLine,
    gutter_width: usize,
    width: usize,
    theme: TerminalTheme,
) -> Line<'static> {
    if row.kind == DiffLineKind::Omitted {
        return Line::from(Span::styled(
            format!("  {:>gutter_width$} ··· {}", "", row.text()),
            Style::default().fg(theme.muted),
        ));
    }
    let (marker, accent, row_bg, emph_bg) = match row.kind {
        DiffLineKind::Added => (
            "+",
            theme.diff_added,
            theme.diff_added_bg,
            theme.diff_added_emph_bg,
        ),
        DiffLineKind::Removed => (
            "-",
            theme.diff_removed,
            theme.diff_removed_bg,
            theme.diff_removed_emph_bg,
        ),
        _ => (" ", theme.diff_context, None, None),
    };
    let on_row = move |style: Style| match row_bg {
        Some(bg) => style.bg(bg),
        None => style,
    };
    let number = row
        .gutter_line()
        .map_or_else(String::new, |number| number.to_string());
    let prefix = format!("  {number:>gutter_width$} {marker} ");
    let prefix_width = prefix.chars().count();
    let mut used = prefix_width;
    let mut spans = vec![Span::styled(prefix, on_row(Style::default().fg(accent)))];
    for segment in truncate_segments(&row.segments, width.saturating_sub(prefix_width)) {
        used += segment.text.chars().count();
        let style = match (row_bg, segment.emphasized.then_some(emph_bg).flatten()) {
            // Foreground-only fallback: context rows and ANSI palettes.
            (None, _) => Style::default().fg(accent),
            (Some(bg), None) => Style::default().fg(theme.text).bg(bg),
            (Some(_), Some(emph)) => Style::default().fg(theme.text).bg(emph),
        };
        spans.push(Span::styled(segment.text, style));
    }
    if row_bg.is_some() && used < width {
        spans.push(Span::styled(
            " ".repeat(width - used),
            on_row(Style::default()),
        ));
    }
    Line::from(spans)
}

fn truncate_segments(segments: &[DiffSegment], budget: usize) -> Vec<DiffSegment> {
    let total: usize = segments
        .iter()
        .map(|segment| segment.text.chars().count())
        .sum();
    if total <= budget {
        return segments.to_vec();
    }
    if budget <= 3 {
        let text: String = segments
            .iter()
            .flat_map(|segment| segment.text.chars())
            .take(budget)
            .collect();
        return vec![DiffSegment {
            text,
            emphasized: false,
        }];
    }
    let mut remaining = budget - 3;
    let mut out = Vec::new();
    for segment in segments {
        if remaining == 0 {
            break;
        }
        let text: String = if segment.text.chars().count() <= remaining {
            segment.text.clone()
        } else {
            segment.text.chars().take(remaining).collect()
        };
        remaining -= text.chars().count();
        out.push(DiffSegment {
            text,
            emphasized: segment.emphasized,
        });
    }
    out.push(DiffSegment {
        text: "...".to_string(),
        emphasized: false,
    });
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffLineKind {
    Added,
    Removed,
    Context,
    Omitted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffSegment {
    text: String,
    emphasized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffLine {
    kind: DiffLineKind,
    old_line: Option<usize>,
    new_line: Option<usize>,
    segments: Vec<DiffSegment>,
}

impl DiffLine {
    fn plain(
        kind: DiffLineKind,
        old_line: Option<usize>,
        new_line: Option<usize>,
        text: &str,
    ) -> Self {
        Self {
            kind,
            old_line,
            new_line,
            segments: vec![DiffSegment {
                text: text.to_string(),
                emphasized: false,
            }],
        }
    }

    fn omitted(text: String) -> Self {
        Self {
            kind: DiffLineKind::Omitted,
            old_line: None,
            new_line: None,
            segments: vec![DiffSegment {
                text,
                emphasized: false,
            }],
        }
    }

    fn text(&self) -> String {
        self.segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect()
    }

    /// Line number shown in the gutter: old numbering for removals, new
    /// numbering for additions and context.
    fn gutter_line(&self) -> Option<usize> {
        match self.kind {
            DiffLineKind::Removed => self.old_line,
            _ => self.new_line,
        }
    }
}

/// Unchanged lines kept around each change; longer stretches collapse to an
/// omitted marker so whole-file diffs read as hunks.
const DIFF_CONTEXT_LINES: usize = 3;

fn compact_line_diff(old_lines: &[&str], new_lines: &[&str], limit: usize) -> Vec<DiffLine> {
    if limit == 0 {
        return Vec::new();
    }

    let mut lines = if old_lines.len().saturating_mul(new_lines.len()) <= 40_000 {
        lcs_line_diff(old_lines, new_lines)
    } else {
        positional_line_diff(old_lines, new_lines)
    };
    emphasize_replacements(&mut lines);
    let mut lines = compact_context(lines);

    if lines.len() > limit {
        let omitted = lines.len() - limit;
        lines.truncate(limit);
        lines.push(DiffLine::omitted(format!("{omitted} diff lines omitted")));
    }
    lines
}

fn compact_context(lines: Vec<DiffLine>) -> Vec<DiffLine> {
    let is_change =
        |line: &DiffLine| matches!(line.kind, DiffLineKind::Added | DiffLineKind::Removed);
    if lines.is_empty() {
        return lines;
    }
    if !lines.iter().any(is_change) {
        return vec![DiffLine::omitted(unchanged_label(lines.len()))];
    }
    let mut keep = vec![false; lines.len()];
    for (idx, line) in lines.iter().enumerate() {
        if is_change(line) {
            let from = idx.saturating_sub(DIFF_CONTEXT_LINES);
            let to = (idx + DIFF_CONTEXT_LINES).min(lines.len() - 1);
            for flag in &mut keep[from..=to] {
                *flag = true;
            }
        }
    }
    // An omitted marker replacing a single line saves nothing: keep the line.
    for idx in 0..keep.len() {
        if !keep[idx] && (idx == 0 || keep[idx - 1]) && (idx + 1 == keep.len() || keep[idx + 1]) {
            keep[idx] = true;
        }
    }
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for (idx, line) in lines.into_iter().enumerate() {
        if keep[idx] {
            if skipped > 0 {
                out.push(DiffLine::omitted(unchanged_label(skipped)));
                skipped = 0;
            }
            out.push(line);
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        out.push(DiffLine::omitted(unchanged_label(skipped)));
    }
    out
}

fn unchanged_label(count: usize) -> String {
    if count == 1 {
        "1 unchanged line".to_string()
    } else {
        format!("{count} unchanged lines")
    }
}

/// Pair each removed run with the added run that follows it and highlight
/// the tokens that actually changed within each line pair.
fn emphasize_replacements(lines: &mut [DiffLine]) {
    let mut idx = 0;
    while idx < lines.len() {
        if lines[idx].kind != DiffLineKind::Removed {
            idx += 1;
            continue;
        }
        let removed_start = idx;
        while idx < lines.len() && lines[idx].kind == DiffLineKind::Removed {
            idx += 1;
        }
        let added_start = idx;
        while idx < lines.len() && lines[idx].kind == DiffLineKind::Added {
            idx += 1;
        }
        let pairs = (added_start - removed_start).min(idx - added_start);
        for pair in 0..pairs {
            let old_text = lines[removed_start + pair].text();
            let new_text = lines[added_start + pair].text();
            if let Some((old_segments, new_segments)) = intra_line_segments(&old_text, &new_text) {
                lines[removed_start + pair].segments = old_segments;
                lines[added_start + pair].segments = new_segments;
            }
        }
    }
}

/// Word-level diff of a removed/added line pair. Returns per-line segments
/// with the differing tokens emphasized, or `None` when the lines share too
/// little for token-level highlights to help.
fn intra_line_segments(old: &str, new: &str) -> Option<(Vec<DiffSegment>, Vec<DiffSegment>)> {
    let old_tokens = split_word_tokens(old);
    let new_tokens = split_word_tokens(new);
    if old_tokens.is_empty()
        || new_tokens.is_empty()
        || old_tokens.len().saturating_mul(new_tokens.len()) > 10_000
    {
        return None;
    }

    let old_len = old_tokens.len();
    let new_len = new_tokens.len();
    let mut dp = vec![vec![0usize; new_len + 1]; old_len + 1];
    for old_idx in (0..old_len).rev() {
        for new_idx in (0..new_len).rev() {
            dp[old_idx][new_idx] = if old_tokens[old_idx] == new_tokens[new_idx] {
                dp[old_idx + 1][new_idx + 1] + 1
            } else {
                dp[old_idx + 1][new_idx].max(dp[old_idx][new_idx + 1])
            };
        }
    }
    let mut old_common = vec![false; old_len];
    let mut new_common = vec![false; new_len];
    let (mut old_idx, mut new_idx) = (0, 0);
    while old_idx < old_len && new_idx < new_len {
        if old_tokens[old_idx] == new_tokens[new_idx] {
            old_common[old_idx] = true;
            new_common[new_idx] = true;
            old_idx += 1;
            new_idx += 1;
        } else if dp[old_idx + 1][new_idx] >= dp[old_idx][new_idx + 1] {
            old_idx += 1;
        } else {
            new_idx += 1;
        }
    }

    let common_chars: usize = old_tokens
        .iter()
        .zip(&old_common)
        .filter(|(_, common)| **common)
        .map(|(token, _)| token.chars().count())
        .sum();
    let longest = old.chars().count().max(new.chars().count());
    // Mostly-different lines read better as plainly replaced rows than as a
    // wall of emphasis.
    if common_chars.saturating_mul(10) < longest.saturating_mul(3) {
        return None;
    }
    Some((
        tokens_to_segments(&old_tokens, &old_common),
        tokens_to_segments(&new_tokens, &new_common),
    ))
}

fn tokens_to_segments(tokens: &[&str], common: &[bool]) -> Vec<DiffSegment> {
    let mut segments: Vec<DiffSegment> = Vec::new();
    for (token, common) in tokens.iter().zip(common) {
        let emphasized = !common;
        match segments.last_mut() {
            Some(last) if last.emphasized == emphasized => last.text.push_str(token),
            _ => segments.push(DiffSegment {
                text: (*token).to_string(),
                emphasized,
            }),
        }
    }
    // Fold unchanged whitespace bridges between two emphasized runs so a
    // changed phrase reads as one highlight, not per-word confetti.
    let mut folded: Vec<DiffSegment> = Vec::new();
    let mut idx = 0;
    while idx < segments.len() {
        let segment = &segments[idx];
        if !segment.emphasized
            && segment.text.chars().all(char::is_whitespace)
            && folded
                .last()
                .is_some_and(|prev: &DiffSegment| prev.emphasized)
            && segments.get(idx + 1).is_some_and(|next| next.emphasized)
        {
            let prev = folded.last_mut().expect("checked above");
            prev.text.push_str(&segment.text);
            prev.text.push_str(&segments[idx + 1].text);
            idx += 2;
            continue;
        }
        folded.push(segment.clone());
        idx += 1;
    }
    folded
}

/// Tokens for intra-line diffing: word runs, whitespace runs, and single
/// punctuation characters.
fn split_word_tokens(text: &str) -> Vec<&str> {
    #[derive(PartialEq, Clone, Copy)]
    enum TokenClass {
        Word,
        Space,
        Punct,
    }
    fn classify(ch: char) -> TokenClass {
        if ch.is_alphanumeric() || ch == '_' {
            TokenClass::Word
        } else if ch.is_whitespace() {
            TokenClass::Space
        } else {
            TokenClass::Punct
        }
    }
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut current: Option<TokenClass> = None;
    for (idx, ch) in text.char_indices() {
        let class = classify(ch);
        if current != Some(class) || class == TokenClass::Punct {
            if idx > start {
                tokens.push(&text[start..idx]);
            }
            start = idx;
            current = Some(class);
        }
    }
    if text.len() > start {
        tokens.push(&text[start..]);
    }
    tokens
}

fn lcs_line_diff(old_lines: &[&str], new_lines: &[&str]) -> Vec<DiffLine> {
    let old_len = old_lines.len();
    let new_len = new_lines.len();
    let mut dp = vec![vec![0usize; new_len + 1]; old_len + 1];

    for old_idx in (0..old_len).rev() {
        for new_idx in (0..new_len).rev() {
            dp[old_idx][new_idx] = if old_lines[old_idx] == new_lines[new_idx] {
                dp[old_idx + 1][new_idx + 1] + 1
            } else {
                dp[old_idx + 1][new_idx].max(dp[old_idx][new_idx + 1])
            };
        }
    }

    let mut lines = Vec::new();
    let mut old_idx = 0;
    let mut new_idx = 0;
    while old_idx < old_len && new_idx < new_len {
        if old_lines[old_idx] == new_lines[new_idx] {
            lines.push(DiffLine::plain(
                DiffLineKind::Context,
                Some(old_idx + 1),
                Some(new_idx + 1),
                old_lines[old_idx],
            ));
            old_idx += 1;
            new_idx += 1;
        } else if dp[old_idx + 1][new_idx] >= dp[old_idx][new_idx + 1] {
            lines.push(DiffLine::plain(
                DiffLineKind::Removed,
                Some(old_idx + 1),
                None,
                old_lines[old_idx],
            ));
            old_idx += 1;
        } else {
            lines.push(DiffLine::plain(
                DiffLineKind::Added,
                None,
                Some(new_idx + 1),
                new_lines[new_idx],
            ));
            new_idx += 1;
        }
    }

    while old_idx < old_len {
        lines.push(DiffLine::plain(
            DiffLineKind::Removed,
            Some(old_idx + 1),
            None,
            old_lines[old_idx],
        ));
        old_idx += 1;
    }
    while new_idx < new_len {
        lines.push(DiffLine::plain(
            DiffLineKind::Added,
            None,
            Some(new_idx + 1),
            new_lines[new_idx],
        ));
        new_idx += 1;
    }
    lines
}

fn positional_line_diff(old_lines: &[&str], new_lines: &[&str]) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    let max = old_lines.len().max(new_lines.len());
    for idx in 0..max {
        let line_no = idx + 1;
        match (old_lines.get(idx), new_lines.get(idx)) {
            (Some(old), Some(new)) if old == new => lines.push(DiffLine::plain(
                DiffLineKind::Context,
                Some(line_no),
                Some(line_no),
                old,
            )),
            (Some(old), Some(new)) => {
                lines.push(DiffLine::plain(
                    DiffLineKind::Removed,
                    Some(line_no),
                    None,
                    old,
                ));
                lines.push(DiffLine::plain(
                    DiffLineKind::Added,
                    None,
                    Some(line_no),
                    new,
                ));
            }
            (Some(old), None) => lines.push(DiffLine::plain(
                DiffLineKind::Removed,
                Some(line_no),
                None,
                old,
            )),
            (None, Some(new)) => lines.push(DiffLine::plain(
                DiffLineKind::Added,
                None,
                Some(line_no),
                new,
            )),
            (None, None) => {}
        }
    }
    lines
}

fn tool_header_spans(
    view: &crate::app::ToolCallView,
    status: &str,
    subagent: bool,
    theme: TerminalTheme,
) -> Vec<Span<'static>> {
    let color = tool_status_color(view.status, theme);
    let mut spans = Vec::new();
    if subagent {
        spans.push(Span::styled(
            "subagent ",
            Style::default()
                .fg(theme.secondary)
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans.extend([
        Span::styled(
            format!("tool {status}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} ", tool_kind_label(view.kind)),
            Style::default().fg(theme.muted),
        ),
    ]);
    if matches!(
        view.kind,
        agent_client_protocol::schema::v1::ToolKind::Execute
    ) {
        spans.extend(highlight_command(&view.title, theme));
    } else {
        spans.push(Span::styled(
            view.title.clone(),
            Style::default().fg(theme.text),
        ));
    }
    spans
}

fn is_shell_operator(token: &str) -> bool {
    matches!(
        token,
        "|" | "||" | "&&" | "&" | ";" | ">" | ">>" | "<" | "<<" | "2>" | "2>&1" | "|&"
    )
}

/// Lightweight syntax highlighting for commands displayed in tool headers.
/// It deliberately preserves spacing and only distinguishes the program,
/// first subcommand, flags, operators, and ordinary arguments.
fn highlight_command(cmd: &str, theme: TerminalTheme) -> Vec<Span<'static>> {
    let program_style = Style::default()
        .fg(theme.primary)
        .add_modifier(Modifier::BOLD);
    let subcommand_style = Style::default().fg(theme.secondary);
    let flag_style = Style::default().fg(theme.accent);
    let operator_style = Style::default().fg(theme.muted);
    let arg_style = Style::default().fg(theme.text);
    let mut spans = Vec::new();
    let mut expect_program = true;
    let mut subcommand_seen = false;
    let mut rest = cmd;

    while !rest.is_empty() {
        let ws_len: usize = rest
            .chars()
            .take_while(|ch| ch.is_whitespace())
            .map(char::len_utf8)
            .sum();
        if ws_len > 0 {
            spans.push(Span::raw(rest[..ws_len].to_string()));
            rest = &rest[ws_len..];
            continue;
        }

        let token_len: usize = rest
            .chars()
            .take_while(|ch| !ch.is_whitespace())
            .map(char::len_utf8)
            .sum();
        let token = &rest[..token_len];
        rest = &rest[token_len..];
        let style = if is_shell_operator(token) {
            expect_program = true;
            subcommand_seen = false;
            operator_style
        } else if expect_program {
            if token.contains('=') && !token.starts_with('-') {
                arg_style
            } else {
                expect_program = false;
                program_style
            }
        } else if token.starts_with('-') {
            flag_style
        } else if !subcommand_seen
            && !token.contains('/')
            && !token.contains('.')
            && !token.contains('=')
        {
            subcommand_seen = true;
            subcommand_style
        } else {
            arg_style
        };
        spans.push(Span::styled(token.to_string(), style));
    }

    spans
}

fn tool_kind_label(kind: agent_client_protocol::schema::v1::ToolKind) -> &'static str {
    match kind {
        agent_client_protocol::schema::v1::ToolKind::Read => "read",
        agent_client_protocol::schema::v1::ToolKind::Edit => "edit",
        agent_client_protocol::schema::v1::ToolKind::Delete => "delete",
        agent_client_protocol::schema::v1::ToolKind::Move => "move",
        agent_client_protocol::schema::v1::ToolKind::Search => "search",
        agent_client_protocol::schema::v1::ToolKind::Execute => "exec",
        agent_client_protocol::schema::v1::ToolKind::Think => "think",
        agent_client_protocol::schema::v1::ToolKind::Fetch => "fetch",
        agent_client_protocol::schema::v1::ToolKind::SwitchMode => "mode",
        _ => "other",
    }
}

fn tool_status_label(status: agent_client_protocol::schema::v1::ToolCallStatus) -> &'static str {
    match status {
        agent_client_protocol::schema::v1::ToolCallStatus::Pending => "pending",
        agent_client_protocol::schema::v1::ToolCallStatus::InProgress => "running",
        agent_client_protocol::schema::v1::ToolCallStatus::Completed => "done",
        agent_client_protocol::schema::v1::ToolCallStatus::Failed => "failed",
        _ => "?",
    }
}

fn tool_status_color(
    status: agent_client_protocol::schema::v1::ToolCallStatus,
    theme: TerminalTheme,
) -> Color {
    match status {
        agent_client_protocol::schema::v1::ToolCallStatus::Failed => theme.error,
        agent_client_protocol::schema::v1::ToolCallStatus::Completed => theme.success,
        agent_client_protocol::schema::v1::ToolCallStatus::InProgress => theme.primary,
        agent_client_protocol::schema::v1::ToolCallStatus::Pending => theme.muted,
        _ => theme.warning,
    }
}

struct InputWrappedLayout {
    rows: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
}

/// Split prompt text into the exact visual rows we render in the input
/// editor and compute the cursor position in those rows. Empty logical
/// lines still consume one row.
fn input_wrapped_layout(
    text: &str,
    cursor_char_index: usize,
    inner_w: usize,
) -> InputWrappedLayout {
    let width = inner_w.max(1);
    let cursor = cursor_char_index.min(input_char_count(text));
    let mut rows = Vec::new();
    let mut global_char_index = 0usize;
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;
    let mut cursor_set = false;
    let logical_lines: Vec<&str> = text.split('\n').collect();

    for (line_index, logical_line) in logical_lines.iter().enumerate() {
        let mut row = String::new();
        let mut row_width = 0usize;

        if cursor == global_char_index && !cursor_set {
            cursor_row = rows.len();
            cursor_col = 0;
            cursor_set = true;
        }

        let chars: Vec<char> = logical_line.chars().collect();
        let mut token_start = 0usize;

        while token_start < chars.len() {
            let token_is_whitespace = chars[token_start].is_whitespace();
            let mut token_end = token_start;
            let mut token_width = 0usize;

            while token_end < chars.len() && chars[token_end].is_whitespace() == token_is_whitespace
            {
                token_width += input_wrap_char_width(chars[token_end], width);
                token_end += 1;
            }

            if !token_is_whitespace && row_width > 0 && row_width + token_width > width {
                input_push_wrapped_row(&mut rows, &mut row, &mut row_width);
            }

            for ch in &chars[token_start..token_end] {
                input_append_wrapped_char(
                    *ch,
                    width,
                    cursor,
                    global_char_index,
                    &mut rows,
                    &mut row,
                    &mut row_width,
                    &mut cursor_row,
                    &mut cursor_col,
                    &mut cursor_set,
                );
                global_char_index += 1;
            }

            token_start = token_end;
        }

        if cursor == global_char_index && !cursor_set {
            cursor_row = rows.len();
            cursor_col = row_width;
            cursor_set = true;
        }

        rows.push(row);

        if line_index + 1 < logical_lines.len() {
            global_char_index += 1;
        }
    }

    InputWrappedLayout {
        rows,
        cursor_row,
        cursor_col,
    }
}

fn input_wrap_char_width(ch: char, width: usize) -> usize {
    let ch_width = ch.width().unwrap_or(0);
    if ch_width > width {
        width.max(1)
    } else {
        ch_width
    }
}

fn input_push_wrapped_row(rows: &mut Vec<String>, row: &mut String, row_width: &mut usize) {
    rows.push(std::mem::take(row));
    *row_width = 0;
}

#[expect(clippy::too_many_arguments)]
fn input_append_wrapped_char(
    ch: char,
    width: usize,
    cursor: usize,
    char_index: usize,
    rows: &mut Vec<String>,
    row: &mut String,
    row_width: &mut usize,
    cursor_row: &mut usize,
    cursor_col: &mut usize,
    cursor_set: &mut bool,
) {
    let ch_width = input_wrap_char_width(ch, width);
    if ch_width > 0 && *row_width + ch_width > width {
        input_push_wrapped_row(rows, row, row_width);
    }

    if cursor == char_index && !*cursor_set {
        *cursor_row = rows.len();
        *cursor_col = *row_width;
        *cursor_set = true;
    }

    row.push(ch);
    *row_width += ch_width;
}

fn input_char_slice(text: &str, start: usize, end: usize) -> &str {
    let start = start.min(input_char_count(text));
    let end = end.min(input_char_count(text)).max(start);
    let byte_start = input_byte_index_at_char(text, start);
    let byte_end = input_byte_index_at_char(text, end);
    &text[byte_start..byte_end]
}

fn text_attachment_label(attachment: &PastedAttachment) -> String {
    let line_count = attachment.content.lines().count();
    let char_count = attachment.content.chars().count();
    format!(
        "📎 {} line{} · {} char{}",
        line_count,
        if line_count == 1 { "" } else { "s" },
        char_count,
        if char_count == 1 { "" } else { "s" }
    )
}

fn image_attachment_label(attachment: &PastedImageAttachment) -> String {
    format!(
        "🖼 image {}x{} · {}",
        attachment.width,
        attachment.height,
        format_bytes(attachment.byte_len)
    )
}

fn attachment_span(label: String, theme: TerminalTheme) -> Span<'static> {
    Span::styled(
        label,
        Style::default()
            .fg(theme.selection_fg)
            .bg(theme.selection_bg)
            .add_modifier(Modifier::BOLD),
    )
}

struct InlineAttachmentChip {
    id: usize,
    position: usize,
    span: Span<'static>,
}

struct InputAttachmentLayout {
    lines: Vec<Line<'static>>,
    cursor_row: usize,
    cursor_col: usize,
}

struct InputInlineBuilder {
    width: usize,
    cursor: usize,
    cursor_row: usize,
    cursor_col: usize,
    cursor_set: bool,
    rows: Vec<Line<'static>>,
    row_spans: Vec<Span<'static>>,
    row_width: usize,
}

impl InputInlineBuilder {
    fn new(width: usize, cursor: usize) -> Self {
        Self {
            width: width.max(1),
            cursor,
            cursor_row: 0,
            cursor_col: 0,
            cursor_set: false,
            rows: Vec::new(),
            row_spans: Vec::new(),
            row_width: 0,
        }
    }

    fn set_cursor_if_here(&mut self, char_index: usize) {
        if self.cursor == char_index && !self.cursor_set {
            self.cursor_row = self.rows.len();
            self.cursor_col = self.row_width;
            self.cursor_set = true;
        }
    }

    fn push_row(&mut self) {
        self.rows
            .push(Line::from(std::mem::take(&mut self.row_spans)));
        self.row_width = 0;
    }

    fn append_text(&mut self, text: &str, start: usize, end: usize) {
        let mut char_index = start;
        for ch in input_char_slice(text, start, end).chars() {
            self.set_cursor_if_here(char_index);
            if ch == '\n' {
                self.push_row();
                char_index += 1;
                continue;
            }

            let ch_width = input_wrap_char_width(ch, self.width);
            if ch_width > 0 && self.row_width > 0 && self.row_width + ch_width > self.width {
                self.push_row();
            }
            self.row_spans.push(Span::raw(ch.to_string()));
            self.row_width += ch_width;
            char_index += 1;
        }
    }

    fn append_attachment(&mut self, span: Span<'static>) {
        let width = span.content.width().min(self.width);
        if width > 0 && self.row_width > 0 && self.row_width + width > self.width {
            self.push_row();
        }
        self.row_spans.push(span);
        self.row_width += width;
    }

    fn finish(mut self) -> InputAttachmentLayout {
        self.set_cursor_if_here(self.cursor);
        if !self.row_spans.is_empty() || self.rows.is_empty() {
            self.push_row();
        }
        InputAttachmentLayout {
            lines: self.rows,
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
        }
    }
}

fn input_layout_with_attachments(state: &AppState, inner_w: usize) -> InputAttachmentLayout {
    let input_len = input_char_count(&state.input);
    if state.attachments.is_empty() && state.image_attachments.is_empty() {
        let layout = input_wrapped_layout(&state.input, state.input_cursor, inner_w);
        return InputAttachmentLayout {
            lines: layout.rows.into_iter().map(Line::from).collect(),
            cursor_row: layout.cursor_row,
            cursor_col: layout.cursor_col,
        };
    }

    let mut attachments: Vec<InlineAttachmentChip> = state
        .attachments
        .iter()
        .map(|attachment| InlineAttachmentChip {
            id: attachment.id,
            position: attachment.position.min(input_len),
            span: attachment_span(text_attachment_label(attachment), state.theme),
        })
        .chain(
            state
                .image_attachments
                .iter()
                .map(|attachment| InlineAttachmentChip {
                    id: attachment.id,
                    position: attachment.position.min(input_len),
                    span: attachment_span(image_attachment_label(attachment), state.theme),
                }),
        )
        .collect();
    attachments.sort_by_key(|attachment| (attachment.position, attachment.id));

    let mut builder = InputInlineBuilder::new(inner_w, state.input_cursor.min(input_len));
    let mut text_start = 0usize;
    for attachment in attachments {
        let position = attachment.position;
        if position > text_start {
            builder.append_text(&state.input, text_start, position);
        }
        builder.append_attachment(attachment.span);
        text_start = position;
    }

    builder.append_text(&state.input, text_start, input_len);
    builder.finish()
}

fn input_lines_with_attachments(state: &AppState, inner_w: usize) -> Vec<Line<'static>> {
    input_layout_with_attachments(state, inner_w).lines
}

fn input_row_count_with_attachments(state: &AppState, inner_w: usize) -> usize {
    input_layout_with_attachments(state, inner_w).lines.len()
}

fn input_cursor_visual_position_with_attachments(
    state: &AppState,
    inner_w: usize,
) -> (usize, usize) {
    let layout = input_layout_with_attachments(state, inner_w);
    (layout.cursor_row, layout.cursor_col)
}

/// Compute the cursor position for a multi-line input buffer. Accounts
/// for explicit newlines _and_ line wrapping at the text area width, so
/// the cursor lands on the correct visual row even when a single
/// logical line spans multiple terminal columns. `chip_rows` is added
/// as a prefix offset (paste-attachment badges rendered above the text).
#[cfg(test)]
fn input_cursor_position(
    area: Rect,
    text: &str,
    cursor_char_index: usize,
    chip_rows: usize,
    scroll_offset: u16,
) -> (u16, u16) {
    let inner_w = area.width as usize;
    let inner_h = area.height as usize;

    let (text_cursor_row, cursor_x_offset, _) =
        input_cursor_visual_position(text, cursor_char_index, inner_w);

    // Combined row in the full content (chips above + text below).
    let total_cursor_row = chip_rows + text_cursor_row;
    let visible_row = total_cursor_row.saturating_sub(scroll_offset as usize);
    let cursor_x = area.x + cursor_x_offset.min(inner_w.saturating_sub(1)) as u16;
    let cursor_y = area.y + visible_row.min(inner_h.saturating_sub(1)) as u16;

    (cursor_x, cursor_y)
}

fn format_bytes(bytes: usize) -> String {
    if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.1} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{bytes} B")
    }
}

fn voice_level_meter(level: Option<f32>) -> String {
    const METER_WIDTH: usize = 10;
    let filled = (level.unwrap_or(0.0).clamp(0.0, 1.0) * METER_WIDTH as f32).round() as usize;
    let filled = filled.min(METER_WIDTH);
    format!(
        "[{}{}]",
        "|".repeat(filled),
        ".".repeat(METER_WIDTH - filled)
    )
}

fn prompt_activity_ornament(state: &AppState) -> &'static str {
    if should_show_spinner(state) {
        state.spinner_style.current_frame()
    } else {
        state.spinner_style.idle_frame()
    }
}

fn prompt_title_label(state: &AppState) -> String {
    let ornament = prompt_activity_ornament(state);
    if let Some(elapsed) = turn_elapsed_value_label(state) {
        format!("{ornament} {elapsed}")
    } else {
        ornament.to_string()
    }
}

fn idle_prompt_title(
    state: &AppState,
    voice_input_supported: bool,
    text_selection_hint: &str,
) -> String {
    let label = prompt_title_label(state);
    if voice_input_supported {
        format!(
            " {label} (Enter send | {PROMPT_NEWLINE_HINT} newline | Shift-Tab model/effort | 🎙 Ctrl-R voice | F10 help | Ctrl-C quit{text_selection_hint}) "
        )
    } else {
        format!(
            " {label} (Enter send | {PROMPT_NEWLINE_HINT} newline | Shift-Tab model/effort | F10 help | Ctrl-C quit{text_selection_hint}) "
        )
    }
}

fn busy_prompt_title(state: &AppState) -> Option<String> {
    let queued = state.queued_prompt_count();
    let label = prompt_title_label(state);
    // Matched exhaustively (no `_` arm) on purpose: this and
    // turn_elapsed_value_label must both be revisited when a variant is added,
    // and the missing-arm compile error is what forces that.
    let hint = match state.connection_state() {
        ConnectionState::Streaming | ConnectionState::Cancelling => {
            if queued > 0 {
                format!("{queued} queued | Enter queue next | Ctrl-C/Esc cancel current")
            } else {
                "Enter queue next | Ctrl-C/Esc cancel current".to_string()
            }
        }
        ConnectionState::Forking => {
            if queued > 0 {
                format!("{queued} queued | Enter queue next")
            } else {
                "Enter queue next".to_string()
            }
        }
        ConnectionState::Launching
        | ConnectionState::Initializing
        | ConnectionState::Ready
        | ConnectionState::Closed
        | ConnectionState::Fatal => return None,
    };

    Some(format!(" {label} ({hint}) "))
}

fn queued_prompt_row_count(state: &AppState) -> u16 {
    let count = state.queued_prompt_count();
    if count == 0 {
        return 0;
    }
    let visible = count.min(QUEUED_PROMPT_VISIBLE_ROWS);
    let overflow = usize::from(count > QUEUED_PROMPT_VISIBLE_ROWS);
    (visible + overflow).min(u16::MAX as usize) as u16
}

/// Height of the dedicated workflow progress area. Actor launch and finish
/// events cannot change it: one row is allocated at workflow start, shows the
/// terminal outcome, and is retired when the next user turn begins.
fn workflow_progress_row_count(state: &AppState) -> u16 {
    let count = state.visible_workflows().count();
    if count == 0 {
        return 0;
    }
    let visible = count.min(WORKFLOW_PROGRESS_VISIBLE_ROWS);
    let overflow = usize::from(count > WORKFLOW_PROGRESS_VISIBLE_ROWS);
    (visible + overflow).min(u16::MAX as usize) as u16
}

fn current_branch_pr_row_count(state: &AppState) -> u16 {
    u16::from(state.current_branch_pull_request.is_some())
}

fn draw_current_branch_pr_row(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let Some(pull_request) = state.current_branch_pull_request.as_ref() else {
        return;
    };
    if area.width == 0 || area.height == 0 {
        return;
    }

    let width = usize::from(area.width);
    let label = fit_width(
        format!(" PR #{} · {}", pull_request.number, pull_request.url),
        width,
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            label,
            Style::default()
                .fg(state.theme.accent)
                .add_modifier(Modifier::BOLD),
        ))),
        area,
    );
}

/// One stable line per visible delegation or review workflow, shared by inline
/// and fullscreen layouts. `/subagents` opens the actor-level transcripts.
fn draw_workflow_progress_rows(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let now = Instant::now();
    let mut workflows = state.visible_workflows().collect::<Vec<_>>();
    if workflows.is_empty() {
        return;
    }
    // Keep live work ahead of terminal history, then prefer the newest turn.
    // Actor churn cannot affect this ordering.
    workflows.sort_by(|left, right| {
        left.outcome
            .is_some()
            .cmp(&right.outcome.is_some())
            .then_with(|| right.id.turn_id.cmp(&left.id.turn_id))
            .then_with(|| left.id.operation.cmp(&right.id.operation))
    });
    let total = workflows.len();
    let capacity = usize::from(area.height);
    let visible = if total > capacity {
        capacity.saturating_sub(1)
    } else {
        total.min(WORKFLOW_PROGRESS_VISIBLE_ROWS)
    };
    let spinner = state.spinner_style.compact_frame();
    let width = usize::from(area.width);
    let mut lines: Vec<Line<'static>> = workflows
        .iter()
        .take(visible)
        .map(|workflow| {
            // `/subagents` opens a session-wide roster rather than a
            // workflow-scoped drill-down. Advertise it only on rows that
            // contribute at least one retained nested actor to that roster.
            let show_details = workflow.actors.iter().any(|(actor_id, actor)| {
                if actor.role.is_internal_review_session() {
                    return false;
                }
                let crate::workflow::WorkflowActorId::Subagent(subagent_id) = actor_id else {
                    return false;
                };
                state.nested_agent(*subagent_id).is_some()
            });
            workflow_progress_line(
                workflow,
                spinner,
                state.workflow_elapsed_at(workflow.id, now),
                width,
                state.theme,
                show_details,
            )
        })
        .collect();
    if total > visible && lines.len() < capacity {
        lines.push(Line::from(Span::styled(
            fit_width(format!(" … {} more", total - visible), width),
            Style::default().fg(state.theme.muted),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn workflow_progress_line(
    workflow: &crate::workflow::WorkflowState,
    spinner: &str,
    elapsed: Duration,
    width: usize,
    theme: TerminalTheme,
    show_details: bool,
) -> Line<'static> {
    use crate::workflow::{
        WorkflowActorLifecycle, WorkflowActorRole, WorkflowCoverage, WorkflowKind, WorkflowOutcome,
        WorkflowPhase,
    };

    let title = match workflow.kind {
        WorkflowKind::Delegation => "Subagents",
        WorkflowKind::Review => "Review",
    };
    let mark = match workflow.outcome {
        Some(WorkflowOutcome::Completed | WorkflowOutcome::Clean) => "✔".to_string(),
        Some(WorkflowOutcome::Degraded) => "⚠".to_string(),
        Some(WorkflowOutcome::Failed) => "✘".to_string(),
        Some(WorkflowOutcome::Cancelled) => "⊘".to_string(),
        None => spinner.to_string(),
    };
    let phase = match workflow.outcome {
        Some(WorkflowOutcome::Completed | WorkflowOutcome::Clean | WorkflowOutcome::Degraded) => {
            "complete"
        }
        Some(WorkflowOutcome::Failed) => "failed",
        Some(WorkflowOutcome::Cancelled) => "cancelled",
        None => match workflow.stage.phase {
            WorkflowPhase::Delegating => "delegating",
            WorkflowPhase::IntentAnalysis => "analyzing intent",
            WorkflowPhase::Supervision => "supervising",
            WorkflowPhase::SpecialistReview => "specialist review",
            WorkflowPhase::Synthesis => "synthesizing",
            WorkflowPhase::Correction => "correcting",
            WorkflowPhase::Fallback => "fallback review",
            WorkflowPhase::Terminal => "finishing",
        },
    };
    let elapsed = format_duration(elapsed);
    let details_hint =
        if show_details && format!(" {mark} {title} [/subagents] · {elapsed} ").width() <= width {
            " [/subagents]"
        } else {
            ""
        };
    let head = format!(" {mark} {title}{details_hint} · {elapsed} ");
    let head = fit_width(head, width);
    let head_width = head.width();
    let mut details = vec![phase.to_string()];
    if let Some(waiting) = workflow.waiting.as_ref() {
        if waiting.requires_user_action {
            details.push("waiting for user action".to_string());
        } else {
            details.push(match waiting.remaining {
                Some(1) => "waiting for 1 automatic result".to_string(),
                Some(remaining) => format!("waiting for {remaining} automatic results"),
                None => format!(
                    "waiting · {}",
                    crate::ragnarok::first_line(&waiting.dependency, 48)
                ),
            });
        }
    }

    let running = workflow.running_count();
    let waiting_actors = workflow.waiting_count();
    let completed = workflow.completed_count();
    let failed = workflow.failed_count();
    let cancelled = workflow.cancelled_count();
    if workflow.coverage == WorkflowCoverage::Degraded {
        details.push("degraded coverage".to_string());
    }
    if failed > 0 {
        details.push(format!("{failed} failed"));
    }
    if cancelled > 0 {
        details.push(format!("{cancelled} cancelled"));
    }
    let selected = workflow.selected_count();
    if selected > 0 {
        let reported = workflow
            .actors
            .values()
            .filter(|actor| {
                matches!(actor.role, WorkflowActorRole::SpecialistReviewer { .. })
                    && matches!(
                        actor.lifecycle,
                        WorkflowActorLifecycle::Completed
                            | WorkflowActorLifecycle::Failed(_)
                            | WorkflowActorLifecycle::Cancelled
                    )
            })
            .count();
        details.push(format!("reviewers {reported}/{selected}"));
    }
    if running > 0 {
        details.push(format!("{running} running"));
    }
    if waiting_actors > 0 {
        details.push(format!("{waiting_actors} waiting"));
    }
    if completed > 0 {
        details.push(format!("{completed} done"));
    }
    if !workflow.issues.is_empty() {
        use crate::workflow::ReviewIssueStatus;
        let validated = workflow
            .issues
            .iter()
            .filter(|issue| issue.status == ReviewIssueStatus::Validated)
            .count();
        let fixed = workflow
            .issues
            .iter()
            .filter(|issue| issue.status == ReviewIssueStatus::Fixed)
            .count();
        let invalidated = workflow
            .issues
            .iter()
            .filter(|issue| issue.status == ReviewIssueStatus::Invalidated)
            .count();
        details.push(format!(
            "issues {} found · {validated} validated · {fixed} fixed · {invalidated} invalidated · F9",
            workflow.issues.len()
        ));
    }

    let requires_user_action = workflow
        .waiting
        .as_ref()
        .is_some_and(|waiting| waiting.requires_user_action);
    let detail_color = if failed > 0 || workflow.outcome == Some(WorkflowOutcome::Failed) {
        theme.error
    } else if cancelled > 0
        || requires_user_action
        || workflow.coverage == WorkflowCoverage::Degraded
        || matches!(
            workflow.outcome,
            Some(WorkflowOutcome::Degraded | WorkflowOutcome::Cancelled)
        )
    {
        theme.warning
    } else {
        theme.tool
    };
    let head_color = match workflow.outcome {
        Some(WorkflowOutcome::Completed | WorkflowOutcome::Clean) => theme.success,
        Some(WorkflowOutcome::Degraded | WorkflowOutcome::Cancelled) => theme.warning,
        Some(WorkflowOutcome::Failed) => theme.error,
        None => theme.accent,
    };
    let detail = fit_width(
        format!("· {}", details.join(" · ")),
        width.saturating_sub(head_width),
    );
    Line::from(vec![
        Span::styled(head, Style::default().fg(head_color)),
        Span::styled(detail, Style::default().fg(detail_color)),
    ])
}

/// Render queued prompts directly above the input box. Visible only while
/// prompts are waiting behind the active turn. Styled as distinct chips so
/// they read as "waiting to send", never as messages already in the
/// transcript.
fn draw_queued_prompt_row(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    let total = state.queued_prompt_count();
    if total == 0 {
        return;
    };
    let visible = usize::from(area.height)
        .min(total)
        .min(QUEUED_PROMPT_VISIBLE_ROWS);
    let mut lines = state
        .queued_prompts()
        .take(visible)
        .enumerate()
        .map(|(idx, queued)| {
            let label = format!(
                " ↳ queued {}/{}: {} ",
                idx + 1,
                total,
                queued_prompt_preview(&queued.display_text)
            );
            Line::from(Span::styled(
                label,
                Style::default()
                    .fg(state.theme.selection_fg)
                    .bg(if idx == 0 {
                        state.theme.warning
                    } else {
                        state.theme.permission
                    })
                    .add_modifier(Modifier::BOLD),
            ))
        })
        .collect::<Vec<_>>();
    if total > visible && lines.len() < usize::from(area.height) {
        lines.push(Line::from(Span::styled(
            format!(" ↳ ... {} more queued ", total - visible),
            Style::default().fg(state.theme.warning),
        )));
    }
    let chip = Paragraph::new(lines);
    f.render_widget(chip, area);
}

fn draw_input(f: &mut ratatui::Frame, area: Rect, state: &AppState, mode: UiMode) {
    let text_selection_hint = match mode {
        UiMode::InlineChat => String::new(),
        UiMode::FullscreenTui => {
            if state.text_selection_mode {
                " | F12 resume wheel".to_string()
            } else {
                " | F12 select text".to_string()
            }
        }
    };
    let title = if state.runtime_closed {
        " runtime closed (/clear same agent | /new picker | Ctrl-C quit) ".to_string()
    } else if let Some(title) = busy_prompt_title(state) {
        title
    } else if state.voice_input_active {
        dictation_prompt_title(state)
    } else {
        idle_prompt_title(state, voice_input_supported(), &text_selection_hint)
    };
    let style = if state.runtime_closed {
        Style::default().fg(state.theme.muted)
    } else {
        Style::default()
    };
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .title(title);

    // Build lines with attachment chips interleaved with input text.
    let mut lines: Vec<Line> = Vec::new();

    f.render_widget(block, area);

    let inner = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(2),
    );
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let side_padding = PROMPT_SIDE_PADDING.min(inner.width / 4);
    // Reserve space for the "> " prompt prefix in the gutter.
    const PROMPT_PREFIX_WIDTH: u16 = 2;
    let content_width = inner
        .width
        .saturating_sub(side_padding * 2 + PROMPT_PREFIX_WIDTH)
        .max(1);
    let inner_h = inner.height as usize;
    let total_visual_rows = input_row_count_with_attachments(state, content_width as usize);
    let visible_rows = total_visual_rows.max(1).min(inner_h);
    let top_padding = if total_visual_rows < inner_h {
        ((inner_h - total_visual_rows) / 2) as u16
    } else {
        0
    };
    let content_area = Rect::new(
        inner.x + side_padding + PROMPT_PREFIX_WIDTH,
        inner.y + top_padding,
        content_width,
        visible_rows as u16,
    );

    // Add input rows after the content width is known so cursor
    // placement and rendering use the same wrap boundaries.
    lines.extend(input_lines_with_attachments(state, content_width as usize));

    let scroll = if total_visual_rows > visible_rows {
        let cursor_row =
            input_cursor_visual_position_with_attachments(state, content_width as usize).0;
        let desired = cursor_row.saturating_sub(visible_rows / 2);
        desired.min(total_visual_rows - visible_rows) as u16
    } else {
        0
    };

    let paragraph = Paragraph::new(lines).style(style).scroll((scroll, 0));
    f.render_widget(paragraph, content_area);

    // Draw the ">" prompt prefix in the gutter to the left of the input text.
    let gutter_area = Rect::new(
        inner.x + side_padding,
        content_area.y,
        PROMPT_PREFIX_WIDTH,
        content_area.height,
    );
    let gutter_style = if state.runtime_closed {
        Style::default().fg(state.theme.muted)
    } else {
        Style::default()
            .fg(state.theme.primary)
            .add_modifier(Modifier::BOLD)
    };
    let gutter = Paragraph::new(">").style(gutter_style);
    f.render_widget(gutter, gutter_area);

    if !state.runtime_closed
        && !state.has_pending_permission()
        && !state.has_pending_elicitation()
        && state.config_picker.is_none()
        && !state.help_overlay
        && state.mjconfig_menu.is_none()
        && (mode == UiMode::InlineChat || !state.text_selection_mode)
    {
        let (cursor_row, cursor_col) =
            input_cursor_visual_position_with_attachments(state, content_width as usize);
        let total_cursor_row = cursor_row;
        let visible_row = total_cursor_row.saturating_sub(scroll as usize);
        let cursor_x =
            content_area.x + cursor_col.min(content_width.saturating_sub(1) as usize) as u16;
        let cursor_y =
            content_area.y + visible_row.min(content_area.height.saturating_sub(1) as usize) as u16;
        f.set_cursor_position((cursor_x, cursor_y));
    }
}

fn draw_usage_quota_row(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let Some(label) = usage_quota_label(state) else {
        return;
    };
    if area.height == 0 || area.width == 0 {
        return;
    }

    let wraps = matches!(
        state.anvil_quota_source,
        Some(crate::app::AnvilQuotaSource::DeepSeek)
    );
    let paragraph = if let Some(quota_items) = attributed_usage_quota_items(state) {
        let lines = quota_items
            .into_iter()
            .map(|(owner, quota)| {
                let color = match owner {
                    UsageQuotaOwner::Primary => state.theme.primary,
                    UsageQuotaOwner::Subagents => state.theme.secondary,
                    UsageQuotaOwner::Anvil => state.theme.warning,
                };
                Line::from(vec![
                    Span::styled(
                        format!("[{}]", owner.display_label()),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(quota, Style::default().fg(color)),
                ])
            })
            .collect::<Vec<_>>();
        let paragraph = Paragraph::new(lines);
        if wraps {
            paragraph.wrap(Wrap { trim: false })
        } else {
            paragraph
        }
    } else if wraps {
        Paragraph::new(label)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(state.theme.warning))
    } else {
        Paragraph::new(truncate_text_to_width(label, area.width))
            .style(Style::default().fg(state.theme.warning))
    };
    f.render_widget(paragraph, area);
}

fn usage_quota_row_count(state: &AppState, width: u16) -> usize {
    let Some(label) = usage_quota_label(state) else {
        return 0;
    };
    if width == 0 {
        return 0;
    }
    if let Some(quota_items) = attributed_usage_quota_items(state) {
        if !matches!(
            state.anvil_quota_source,
            Some(crate::app::AnvilQuotaSource::DeepSeek)
        ) {
            return quota_items.len();
        }
        return quota_items
            .into_iter()
            .map(|(owner, quota)| {
                Paragraph::new(format!("[{}] {quota}", owner.display_label()))
                    .wrap(Wrap { trim: false })
                    .line_count(width)
                    .max(1)
            })
            .sum();
    }
    if !matches!(
        state.anvil_quota_source,
        Some(crate::app::AnvilQuotaSource::DeepSeek)
    ) {
        return 1;
    }
    Paragraph::new(label)
        .wrap(Wrap { trim: false })
        .line_count(width)
        .max(1)
}

fn usage_quota_label(state: &AppState) -> Option<String> {
    if let Some(quota_items) = attributed_usage_quota_items(state) {
        return Some(
            quota_items
                .into_iter()
                .map(|(owner, label)| format!("{} {label}", owner.plain_label()))
                .collect::<Vec<_>>()
                .join(" · "),
        );
    }

    usage_quota_source_label(state, "anvil")
        .or_else(|| usage_quota_source_label(state, "codex-acp"))
        .or_else(|| usage_quota_source_label(state, "claude-acp"))
}

#[derive(Clone, Copy)]
enum UsageQuotaOwner {
    Primary,
    Subagents,
    Anvil,
}

impl UsageQuotaOwner {
    fn display_label(self) -> &'static str {
        match self {
            Self::Primary => "PRIMARY",
            Self::Subagents => "SUBAGENTS",
            Self::Anvil => "ANVIL",
        }
    }

    fn plain_label(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Subagents => "subagents",
            Self::Anvil => "anvil",
        }
    }
}

fn attributed_usage_quota_items(state: &AppState) -> Option<Vec<(UsageQuotaOwner, String)>> {
    let primary_source = state.active_models.primary_source.as_deref()?;
    let mut labels = usage_quota_source_label(state, primary_source)
        .map(|label| (UsageQuotaOwner::Primary, label))
        .into_iter()
        .collect::<Vec<_>>();
    let subagent_source = state.active_models.subagent_source.as_deref();
    if let Some(subagent_source) = subagent_source
        && subagent_source != primary_source
        && let Some(label) = usage_quota_source_label(state, subagent_source)
    {
        labels.push((UsageQuotaOwner::Subagents, label));
    }
    if primary_source != "anvil"
        && subagent_source != Some("anvil")
        && let Some(label) = usage_quota_source_label(state, "anvil")
    {
        labels.push((UsageQuotaOwner::Anvil, label));
    }
    // No seat resolved to a quota provider (e.g. an `anvil` primary with no
    // configured anvil balance source). Fall through to the priority chain so a
    // still-live poller keeps the row populated instead of blanking it.
    (!labels.is_empty()).then_some(labels)
}

fn usage_quota_source_label(state: &AppState, source: &str) -> Option<String> {
    match source {
        "anvil" => match state.anvil_quota_source {
            Some(crate::app::AnvilQuotaSource::Bedrock) => state
                .bedrock_credits
                .as_ref()
                .map(crate::bedrock_credits::BedrockCreditsStatus::compact_label),
            Some(crate::app::AnvilQuotaSource::OpenRouter) => state
                .openrouter_balance
                .as_ref()
                .map(crate::openrouter_balance::OpenRouterBalanceStatus::compact_label),
            Some(crate::app::AnvilQuotaSource::DeepSeek) => state
                .deepseek_balance
                .as_ref()
                .map(crate::deepseek_balance::DeepSeekBalanceStatus::compact_label),
            None => None,
        },
        "codex-acp" => state
            .codex_usage
            .as_ref()
            .map(crate::codex_usage::CodexUsageStatus::compact_label),
        "claude-acp" => state
            .claude_usage
            .as_ref()
            .map(crate::claude_usage::ClaudeUsageStatus::compact_label),
        _ => None,
    }
}

#[cfg(test)]
fn draw_config_shortcuts_row(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let options = state.selectable_config_options();
    if options.is_empty() {
        return;
    }

    let mut chips = Vec::with_capacity(options.len());
    for (_, option, shortcut) in options {
        let current = config_option_current_value_label(option);
        let chip = match shortcut {
            Some(shortcut) => format!("[F{shortcut} {}: {current}]", option.name),
            None => format!("[{}: {current}]", option.name),
        };
        chips.push(chip);
    }

    let text = chips.join(" ");

    let paragraph = Paragraph::new(text).style(Style::default().fg(state.theme.primary));
    f.render_widget(paragraph, area);
}

fn draw_permission_modal(
    f: &mut ratatui::Frame,
    area: Rect,
    pending: &PendingPermission,
    queue_len: usize,
    theme: TerminalTheme,
) {
    const HORIZONTAL_PADDING: u16 = 2;
    const VERTICAL_PADDING: u16 = 1;

    let footer_text = "Up/Down choose | PgUp/PgDn read | Enter to confirm | Esc cancel";

    let max_width = area.width.saturating_sub(4);
    if max_width < 16 || area.height == 0 {
        return;
    }
    let max_content_width = max_width.saturating_sub(2 + HORIZONTAL_PADDING * 2);
    if max_content_width == 0 {
        return;
    }

    let title = permission_detail_text(pending);
    let longest_option_width = pending
        .prompt
        .options
        .iter()
        .map(|opt| format!("> {}", opt.name).width())
        .max()
        .unwrap_or(0);
    let desired_content_width = longest_option_width
        .max(title.width())
        .max(footer_text.width())
        .max(60)
        .min(max_content_width as usize) as u16;
    let width = desired_content_width
        .saturating_add(2)
        .saturating_add(HORIZONTAL_PADDING * 2)
        .min(max_width);

    let view_lines = permission_view_lines(pending, queue_len, desired_content_width, theme);
    let view_rows = view_lines.len().min(u16::MAX as usize) as u16;

    let max_height = area.height.saturating_sub(2);
    let height = view_rows
        .saturating_add(3)
        .saturating_add(VERTICAL_PADDING * 2)
        .min(max_height);
    if height < 7 {
        return;
    }

    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(area.x + x, area.y + y, width, height);

    f.render_widget(Clear, rect);
    // Surface queue depth so the user knows another prompt is waiting
    // behind this one rather than wondering why one just popped up.
    let title = if queue_len > 1 {
        format!(" permission request (1 of {queue_len}) ")
    } else {
        " permission request ".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(theme.permission));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let content = Rect::new(
        inner.x.saturating_add(HORIZONTAL_PADDING),
        inner.y.saturating_add(VERTICAL_PADDING),
        inner.width.saturating_sub(HORIZONTAL_PADDING * 2),
        inner.height.saturating_sub(VERTICAL_PADDING * 2),
    );
    if content.width == 0 || content.height < 3 {
        return;
    }

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(content);

    let visible_lines = visible_permission_content_lines(
        pending,
        &view_lines,
        desired_content_width,
        layout[0].height,
    );
    f.render_widget(Paragraph::new(visible_lines), layout[0]);

    let footer = Paragraph::new(footer_text).style(Style::default().fg(theme.muted));
    f.render_widget(footer, layout[1]);
}

fn permission_option_lines(
    pending: &PendingPermission,
    selected: usize,
    width: u16,
    theme: TerminalTheme,
) -> Vec<(usize, Vec<Line<'static>>)> {
    pending
        .prompt
        .options
        .iter()
        .enumerate()
        .map(|(i, opt)| {
            let label = opt.name.clone();
            let marker = if i == selected { "> " } else { "  " };
            let style = if i == selected {
                Style::default()
                    .fg(theme.selection_fg)
                    .bg(theme.permission)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let lines = wrap_prefixed_text_to_width(&label, width, marker, "  ")
                .into_iter()
                .map(|line| {
                    let line = if i == selected {
                        pad_text_to_width(line, width)
                    } else {
                        line
                    };
                    Line::from(Span::styled(line, style))
                })
                .collect();
            (i, lines)
        })
        .collect()
}

fn permission_detail_text(pending: &PendingPermission) -> String {
    pending
        .prompt
        .tool_call
        .fields
        .title
        .clone()
        .map(|title| title.replace("\\n", "\n"))
        .unwrap_or_else(|| pending.prompt.tool_call.tool_call_id.to_string())
}

fn permission_view_lines(
    pending: &PendingPermission,
    queue_len: usize,
    width: u16,
    theme: TerminalTheme,
) -> Vec<Line<'static>> {
    let selected = clamp_permission_selected(pending.selected, pending.prompt.options.len());
    let source = pending
        .subagent_id
        .map(|id| format!("subagent #{id} permission"))
        .unwrap_or_else(|| "permission request".to_string());
    let title = if queue_len > 1 {
        format!("{source} (1 of {queue_len})")
    } else {
        source
    };
    let mut lines = vec![Line::from(Span::styled(
        title,
        Style::default()
            .fg(theme.permission)
            .add_modifier(Modifier::BOLD),
    ))];

    lines.extend(
        wrap_text_to_width(&permission_detail_text(pending), width)
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(theme.text)))),
    );
    lines.push(Line::from(""));
    lines.extend(
        permission_option_lines(pending, selected, width, theme)
            .into_iter()
            .flat_map(|(_, option_lines)| option_lines),
    );
    lines
}

fn visible_permission_content_lines(
    pending: &PendingPermission,
    lines: &[Line<'static>],
    width: u16,
    visible_rows: u16,
) -> Vec<Line<'static>> {
    let visible_rows = usize::from(visible_rows);
    if visible_rows == 0 {
        return Vec::new();
    }
    let max_start = lines.len().saturating_sub(visible_rows);
    let auto_start = selected_permission_content_row(pending, width)
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(max_start);
    let start = pending.scroll_offset.unwrap_or(auto_start).min(max_start);

    lines
        .iter()
        .skip(start)
        .take(visible_rows)
        .cloned()
        .collect()
}

fn selected_permission_content_row(pending: &PendingPermission, width: u16) -> usize {
    let selected = clamp_permission_selected(pending.selected, pending.prompt.options.len());
    let detail_rows = wrap_text_to_width(&permission_detail_text(pending), width)
        .len()
        .max(1);
    let option_rows_before = pending
        .prompt
        .options
        .iter()
        .take(selected)
        .map(|opt| {
            wrap_prefixed_text_to_width(&opt.name, width, "> ", "  ")
                .len()
                .max(1)
        })
        .sum::<usize>();

    1 + detail_rows + 1 + option_rows_before
}

/// Rendered elicitation modal content plus the row of the selected option, so
/// the windowing logic can auto-scroll to keep it visible.
struct ElicitationContent {
    lines: Vec<Line<'static>>,
    /// Row index of the active option or input field. Points at the heading
    /// (0) for URL / unsupported views, which have no selection to follow.
    selected_row: usize,
}

/// Build the modal's content lines for single fields, sequential forms, URLs,
/// and the unsupported-shape notice.
fn elicitation_view_lines(
    pending: &PendingElicitation,
    queue_len: usize,
    width: u16,
    theme: TerminalTheme,
) -> ElicitationContent {
    let view = classify_elicitation(&pending.prompt);
    let source = pending
        .subagent_id
        .map(|id| format!("subagent #{id} setup"))
        .unwrap_or_else(|| "setup request".to_string());
    let heading = if queue_len > 1 {
        format!("{source} (1 of {queue_len})")
    } else {
        source
    };
    let mut lines = vec![Line::from(Span::styled(
        heading,
        Style::default()
            .fg(theme.permission)
            .add_modifier(Modifier::BOLD),
    ))];

    // The agent's human-readable prompt message.
    lines.extend(
        wrap_text_to_width(&pending.prompt.message, width)
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(theme.text)))),
    );

    let mut selected_row = 0;
    match view {
        ElicitationView::SingleSelect { title, options, .. } => {
            if let Some(title) = title.filter(|t| !t.is_empty()) {
                lines.push(Line::from(""));
                lines.extend(
                    wrap_text_to_width(&title, width).into_iter().map(|line| {
                        Line::from(Span::styled(line, Style::default().fg(theme.muted)))
                    }),
                );
            }
            lines.push(Line::from(""));
            let selected = pending.selected.min(options.len().saturating_sub(1));
            for (i, opt) in options.iter().enumerate() {
                if i == selected {
                    selected_row = lines.len();
                }
                let marker = if i == selected { "> " } else { "  " };
                let style = if i == selected {
                    Style::default()
                        .fg(theme.selection_fg)
                        .bg(theme.permission)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text)
                };
                for line in wrap_prefixed_text_to_width(&opt.title, width, marker, "  ") {
                    let line = if i == selected {
                        pad_text_to_width(line, width)
                    } else {
                        line
                    };
                    lines.push(Line::from(Span::styled(line, style)));
                }
            }
        }
        ElicitationView::Url { url } => {
            lines.push(Line::from(""));
            let label = "URL (press c to copy): ";
            if label.width() + url.width() <= usize::from(width) {
                lines.push(Line::from(vec![
                    Span::styled(label, Style::default().fg(theme.muted)),
                    Span::styled(url.clone(), Style::default().fg(theme.accent)),
                ]));
            } else {
                lines.push(Line::from(Span::styled(
                    label.trim_end().to_string(),
                    Style::default().fg(theme.muted),
                )));
                lines.extend(
                    wrap_text_to_width(&url, width).into_iter().map(|line| {
                        Line::from(Span::styled(line, Style::default().fg(theme.accent)))
                    }),
                );
            }
            lines.push(Line::from(""));
            match crate::qr::render_qr(&url) {
                Ok(qr) => {
                    let qr_width = qr.lines().map(|line| line.width()).max().unwrap_or(0);
                    if qr_width <= usize::from(width) {
                        lines.extend(qr.lines().map(|line| {
                            Line::from(Span::styled(
                                line.to_string(),
                                Style::default().fg(theme.text),
                            ))
                        }));
                    } else {
                        lines.push(Line::from(Span::styled(
                            "(terminal too narrow for QR; press c to copy URL)".to_string(),
                            Style::default().fg(theme.muted),
                        )));
                    }
                }
                Err(_) => lines.push(Line::from(Span::styled(
                    "(could not render QR code; use the URL above)".to_string(),
                    Style::default().fg(theme.muted),
                ))),
            }
        }
        ElicitationView::Text {
            title, description, ..
        } => {
            if let Some(title) = title.filter(|t| !t.is_empty()) {
                lines.push(Line::from(""));
                lines.extend(
                    wrap_text_to_width(&title, width).into_iter().map(|line| {
                        Line::from(Span::styled(line, Style::default().fg(theme.muted)))
                    }),
                );
            }
            if let Some(description) = description.filter(|d| !d.is_empty()) {
                lines.extend(
                    wrap_text_to_width(&description, width)
                        .into_iter()
                        .map(|line| {
                            Line::from(Span::styled(line, Style::default().fg(theme.muted)))
                        }),
                );
            }
            lines.push(Line::from(""));
            // The typed value with a trailing cursor block, padded so the field
            // reads as an input box even while empty.
            let shown = pad_text_to_width(format!("{}\u{2588}", pending.input), width);
            lines.push(Line::from(Span::styled(
                shown,
                Style::default().fg(theme.selection_fg).bg(theme.permission),
            )));
        }
        ElicitationView::Form { title, fields } => {
            if let Some(title) = title.filter(|title| !title.is_empty()) {
                lines.push(Line::from(""));
                lines.extend(
                    wrap_text_to_width(&title, width).into_iter().map(|line| {
                        Line::from(Span::styled(line, Style::default().fg(theme.muted)))
                    }),
                );
            }
            let field_index = pending.form_field.min(fields.len().saturating_sub(1));
            let Some(field) = fields.get(field_index) else {
                return ElicitationContent {
                    lines,
                    selected_row,
                };
            };
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("Field {} of {}", field_index + 1, fields.len()),
                Style::default()
                    .fg(theme.permission)
                    .add_modifier(Modifier::BOLD),
            )));
            if let Some(title) = field.title.as_deref().filter(|title| !title.is_empty()) {
                lines.extend(
                    wrap_text_to_width(title, width).into_iter().map(|line| {
                        Line::from(Span::styled(line, Style::default().fg(theme.text)))
                    }),
                );
            }
            if let Some(description) = field
                .description
                .as_deref()
                .filter(|description| !description.is_empty())
            {
                lines.extend(
                    wrap_text_to_width(description, width)
                        .into_iter()
                        .map(|line| {
                            Line::from(Span::styled(line, Style::default().fg(theme.muted)))
                        }),
                );
            }
            lines.push(Line::from(""));
            match &field.kind {
                ElicitationFormFieldKind::SingleSelect { options } => {
                    let selected = pending.selected.min(options.len().saturating_sub(1));
                    for (index, option) in options.iter().enumerate() {
                        if index == selected {
                            selected_row = lines.len();
                        }
                        let marker = if index == selected { "> " } else { "  " };
                        let style = if index == selected {
                            Style::default()
                                .fg(theme.selection_fg)
                                .bg(theme.permission)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(theme.text)
                        };
                        for line in wrap_prefixed_text_to_width(&option.title, width, marker, "  ")
                        {
                            lines.push(Line::from(Span::styled(
                                if index == selected {
                                    pad_text_to_width(line, width)
                                } else {
                                    line
                                },
                                style,
                            )));
                        }
                    }
                }
                ElicitationFormFieldKind::MultiSelect { options, .. } => {
                    let selected = pending.selected.min(options.len().saturating_sub(1));
                    for (index, option) in options.iter().enumerate() {
                        if index == selected {
                            selected_row = lines.len();
                        }
                        let checked = if pending.multi_selected.contains(&index) {
                            "[x] "
                        } else {
                            "[ ] "
                        };
                        let marker = if index == selected {
                            format!("> {checked}")
                        } else {
                            format!("  {checked}")
                        };
                        let continuation = " ".repeat(marker.width());
                        let style = if index == selected {
                            Style::default()
                                .fg(theme.selection_fg)
                                .bg(theme.permission)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(theme.text)
                        };
                        for line in wrap_prefixed_text_to_width(
                            &option.title,
                            width,
                            &marker,
                            &continuation,
                        ) {
                            lines.push(Line::from(Span::styled(
                                if index == selected {
                                    pad_text_to_width(line, width)
                                } else {
                                    line
                                },
                                style,
                            )));
                        }
                    }
                }
                ElicitationFormFieldKind::Text
                | ElicitationFormFieldKind::Number { .. }
                | ElicitationFormFieldKind::Integer { .. } => {
                    selected_row = lines.len();
                    let shown = pad_text_to_width(format!("{}\u{2588}", pending.input), width);
                    lines.push(Line::from(Span::styled(
                        shown,
                        Style::default().fg(theme.selection_fg).bg(theme.permission),
                    )));
                }
                ElicitationFormFieldKind::Boolean => {
                    let selected = pending.selected.min(1);
                    for (index, label) in ["No", "Yes"].into_iter().enumerate() {
                        if index == selected {
                            selected_row = lines.len();
                        }
                        let marker = if index == selected { "> " } else { "  " };
                        let style = if index == selected {
                            Style::default()
                                .fg(theme.selection_fg)
                                .bg(theme.permission)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(theme.text)
                        };
                        let line = format!("{marker}{label}");
                        lines.push(Line::from(Span::styled(
                            if index == selected {
                                pad_text_to_width(line, width)
                            } else {
                                line
                            },
                            style,
                        )));
                    }
                }
            }
        }
        ElicitationView::Unsupported => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "This setup step isn't supported in this build.".to_string(),
                Style::default().fg(theme.warning),
            )));
        }
    }

    ElicitationContent {
        lines,
        selected_row,
    }
}

fn elicitation_footer_text(view: &ElicitationView) -> &'static str {
    match view {
        ElicitationView::SingleSelect { .. } => "Up/Down choose | Enter confirm | Esc cancel",
        ElicitationView::Url { .. } => {
            "c copy URL | Enter acknowledge | PgUp/PgDn scroll | Esc cancel"
        }
        ElicitationView::Text { .. } => "Type value | Backspace delete | Enter submit | Esc cancel",
        ElicitationView::Form { fields, .. } => {
            if fields
                .iter()
                .any(|field| matches!(field.kind, ElicitationFormFieldKind::MultiSelect { .. }))
            {
                "Up/Down choose | Space toggle | Enter next/submit | PgUp/PgDn scroll | Esc cancel"
            } else {
                "Up/Down choose | Enter next/submit | PgUp/PgDn scroll | Esc cancel"
            }
        }
        ElicitationView::Unsupported => "Enter / Esc to skip",
    }
}

/// Natural (unwrapped) content width for sizing the modal: the widest of the
/// message, the option labels / property title, and (for URL) the QR width.
fn elicitation_content_width_hint(view: &ElicitationView, message: &str) -> usize {
    let message_width = message.lines().map(|line| line.width()).max().unwrap_or(0);
    match view {
        ElicitationView::SingleSelect { title, options, .. } => {
            let option_width = options
                .iter()
                .map(|opt| format!("> {}", opt.title).width())
                .max()
                .unwrap_or(0);
            let title_width = title.as_deref().map(|t| t.width()).unwrap_or(0);
            message_width.max(option_width).max(title_width)
        }
        ElicitationView::Url { url } => {
            let qr_width = crate::qr::render_qr(url)
                .ok()
                .and_then(|qr| qr.lines().map(|line| line.chars().count()).max())
                .unwrap_or(0);
            message_width
                .max(format!("URL (press c to copy): {url}").width())
                .max(qr_width)
        }
        ElicitationView::Text {
            title, description, ..
        } => {
            let title_width = title.as_deref().map(|t| t.width()).unwrap_or(0);
            let description_width = description.as_deref().map(|d| d.width()).unwrap_or(0);
            // Reserve a comfortable field width for pasted keys/tokens.
            message_width
                .max(title_width)
                .max(description_width)
                .max(48)
        }
        ElicitationView::Form { title, fields } => {
            let title_width = title.as_deref().map(|title| title.width()).unwrap_or(0);
            let field_width = fields
                .iter()
                .map(|field| {
                    let heading = field
                        .title
                        .as_deref()
                        .map(|title| title.width())
                        .unwrap_or(0);
                    let description = field
                        .description
                        .as_deref()
                        .map(|description| description.width())
                        .unwrap_or(0);
                    let options = match &field.kind {
                        ElicitationFormFieldKind::SingleSelect { options }
                        | ElicitationFormFieldKind::MultiSelect { options, .. } => options
                            .iter()
                            .map(|option| option.title.width() + 6)
                            .max()
                            .unwrap_or(0),
                        ElicitationFormFieldKind::Text
                        | ElicitationFormFieldKind::Number { .. }
                        | ElicitationFormFieldKind::Integer { .. } => 48,
                        ElicitationFormFieldKind::Boolean => 8,
                    };
                    heading.max(description).max(options)
                })
                .max()
                .unwrap_or(0);
            message_width.max(title_width).max(field_width)
        }
        ElicitationView::Unsupported => {
            message_width.max("This setup step isn't supported in this build.".width())
        }
    }
}

/// Window `content` to `visible_rows`, honoring a manual `scroll_offset` or
/// auto-scrolling to keep the selected option visible. Mirrors
/// [`visible_permission_content_lines`].
fn elicitation_visible_window(
    content: &ElicitationContent,
    scroll_offset: Option<usize>,
    visible_rows: u16,
) -> Vec<Line<'static>> {
    let visible_rows = usize::from(visible_rows);
    if visible_rows == 0 {
        return Vec::new();
    }
    let max_start = content.lines.len().saturating_sub(visible_rows);
    let auto_start = content
        .selected_row
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(max_start);
    let start = scroll_offset.unwrap_or(auto_start).min(max_start);
    content
        .lines
        .iter()
        .skip(start)
        .take(visible_rows)
        .cloned()
        .collect()
}

fn draw_elicitation_modal(
    f: &mut ratatui::Frame,
    area: Rect,
    pending: &PendingElicitation,
    queue_len: usize,
    theme: TerminalTheme,
) {
    const HORIZONTAL_PADDING: u16 = 2;
    const VERTICAL_PADDING: u16 = 1;

    let view = classify_elicitation(&pending.prompt);
    let footer_text = elicitation_footer_text(&view);

    let max_width = area.width.saturating_sub(4);
    if max_width < 16 || area.height == 0 {
        return;
    }
    let max_content_width = max_width.saturating_sub(2 + HORIZONTAL_PADDING * 2);
    if max_content_width == 0 {
        return;
    }

    let desired_content_width = elicitation_content_width_hint(&view, &pending.prompt.message)
        .max(footer_text.width())
        .max(40)
        .min(max_content_width as usize) as u16;
    let width = desired_content_width
        .saturating_add(2)
        .saturating_add(HORIZONTAL_PADDING * 2)
        .min(max_width);

    let content_lines = elicitation_view_lines(pending, queue_len, desired_content_width, theme);
    let view_rows = content_lines.lines.len().min(u16::MAX as usize) as u16;

    let max_height = area.height.saturating_sub(2);
    let height = view_rows
        .saturating_add(3)
        .saturating_add(VERTICAL_PADDING * 2)
        .min(max_height);
    if height < 7 {
        return;
    }

    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(area.x + x, area.y + y, width, height);

    f.render_widget(Clear, rect);
    let title = if queue_len > 1 {
        format!(" setup request (1 of {queue_len}) ")
    } else {
        " setup request ".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(theme.permission));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let content = Rect::new(
        inner.x.saturating_add(HORIZONTAL_PADDING),
        inner.y.saturating_add(VERTICAL_PADDING),
        inner.width.saturating_sub(HORIZONTAL_PADDING * 2),
        inner.height.saturating_sub(VERTICAL_PADDING * 2),
    );
    if content.width == 0 || content.height < 3 {
        return;
    }

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(content);

    let visible_lines =
        elicitation_visible_window(&content_lines, pending.scroll_offset, layout[0].height);
    f.render_widget(Paragraph::new(visible_lines), layout[0]);

    let footer = Paragraph::new(footer_text).style(Style::default().fg(theme.muted));
    f.render_widget(footer, layout[1]);
}

fn draw_inline_elicitation_view(
    f: &mut ratatui::Frame,
    area: Rect,
    pending: &PendingElicitation,
    queue_len: usize,
    theme: TerminalTheme,
) {
    f.render_widget(Clear, area);
    let content = inline_content_rect(area);
    if content.width == 0 || content.height < 4 {
        return;
    }

    let view = classify_elicitation(&pending.prompt);
    let content_width = if matches!(view, ElicitationView::Url { .. }) {
        elicitation_content_width_hint(&view, &pending.prompt.message)
            .max(elicitation_footer_text(&view).width())
            .min(content.width as usize) as u16
    } else {
        content.width
    };
    let x = content
        .x
        .saturating_add((content.width.saturating_sub(content_width)) / 2);
    let content = Rect::new(x, content.y, content_width, content.height);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(content);

    let content_lines = elicitation_view_lines(pending, queue_len, content.width, theme);
    let visible_lines =
        elicitation_visible_window(&content_lines, pending.scroll_offset, layout[0].height);
    f.render_widget(Paragraph::new(visible_lines), layout[0]);

    f.render_widget(
        Paragraph::new(elicitation_footer_text(&view)).style(Style::default().fg(theme.muted)),
        layout[1],
    );
}

fn wrap_prefixed_text_to_width(
    text: &str,
    width: u16,
    first_prefix: &str,
    continuation_prefix: &str,
) -> Vec<String> {
    let prefix_width = first_prefix.width().max(continuation_prefix.width());
    let body_width = usize::from(width).saturating_sub(prefix_width).max(1) as u16;
    wrap_text_to_width(text, body_width)
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let prefix = if i == 0 {
                first_prefix
            } else {
                continuation_prefix
            };
            format!("{prefix}{line}")
        })
        .collect()
}

fn wrap_text_to_width(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width).max(1);
    let mut out = Vec::new();
    for raw_line in text.lines() {
        if raw_line.is_empty() {
            out.push(String::new());
            continue;
        }

        let mut line = String::new();
        let mut token_start = 0;
        let mut token_whitespace = None;
        for (idx, ch) in raw_line.char_indices() {
            let is_whitespace = ch.is_whitespace();
            match token_whitespace {
                None => token_whitespace = Some(is_whitespace),
                Some(current) if current != is_whitespace => {
                    append_wrapped_token(
                        &raw_line[token_start..idx],
                        current,
                        width,
                        &mut line,
                        &mut out,
                    );
                    token_start = idx;
                    token_whitespace = Some(is_whitespace);
                }
                Some(_) => {}
            }
        }
        if let Some(is_whitespace) = token_whitespace {
            append_wrapped_token(
                &raw_line[token_start..],
                is_whitespace,
                width,
                &mut line,
                &mut out,
            );
        }

        if !line.is_empty() {
            out.push(line);
        }
    }

    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn append_wrapped_token(
    token: &str,
    is_whitespace: bool,
    width: usize,
    line: &mut String,
    out: &mut Vec<String>,
) {
    if token.is_empty() {
        return;
    }
    let token_width = token.width();
    if token_width == 0 {
        line.push_str(token);
        return;
    }

    let line_width = line.width();
    if !is_whitespace && line_width > 0 && line_width + token_width > width {
        out.push(std::mem::take(line));
    }
    append_segment_to_width(token, width, line, out);
}

fn append_segment_to_width(segment: &str, width: usize, line: &mut String, out: &mut Vec<String>) {
    if line.is_empty() {
        let mut rows = split_word_to_width(segment, width);
        if let Some(last) = rows.pop() {
            out.extend(rows);
            *line = last;
        }
        return;
    }

    for ch in segment.chars() {
        let ch_width = ch.width().unwrap_or(0);
        let line_width = line.width();
        if line_width + ch_width > width && line_width > 0 {
            out.push(std::mem::take(line));
        }
        line.push(ch);
    }
}

fn split_word_to_width(word: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    for ch in word.chars() {
        let ch_width = ch.width().unwrap_or(0);
        let row_width = row.width();
        if row_width + ch_width > width && row_width > 0 {
            rows.push(std::mem::take(&mut row));
        }
        row.push(ch);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

fn pad_text_to_width(mut line: String, width: u16) -> String {
    let width = usize::from(width);
    let len = line.width();
    if len < width {
        line.push_str(&" ".repeat(width - len));
    }
    line
}

fn draw_help_modal(
    f: &mut ratatui::Frame,
    area: Rect,
    mode: UiMode,
    theme: TerminalTheme,
    help_scroll: &mut u16,
) {
    let width = area.width.saturating_sub(2).min(82);
    let height = 23.min(area.height.saturating_sub(4));
    if width < 24 || height < 6 {
        return;
    }
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(area.x + x, area.y + y, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" help ")
        .title_bottom(" Up/Down PgUp/PgDn scroll · F10/Esc close ")
        .style(Style::default().fg(theme.success));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let lines = help_modal_lines(mode, voice_input_supported(), theme);

    let paragraph = Paragraph::new(lines)
        .style(Style::default().fg(theme.text))
        .wrap(Wrap { trim: false });
    let max_scroll = paragraph
        .line_count(inner.width)
        .saturating_sub(usize::from(inner.height))
        .min(u16::MAX as usize) as u16;
    *help_scroll = (*help_scroll).min(max_scroll);
    let paragraph = paragraph.scroll((*help_scroll, 0));
    f.render_widget(paragraph, inner);
}

fn help_modal_lines(
    mode: UiMode,
    voice_input_supported: bool,
    theme: TerminalTheme,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        help_section_line("Agent seats", theme),
        help_binding_line_with_color(
            "agent",
            "owns the request, plan, verification, corrections, and final answer",
            theme.primary,
            theme,
        ),
        help_binding_line_with_color(
            "subagents",
            "fresh write-capable sessions for bounded parallel work; primary verifies reports",
            theme.code,
            theme,
        ),
        help_binding_line_with_color(
            "review",
            "read-only intent analyst, primary-route supervisor, and selected specialists",
            theme.warning,
            theme,
        ),
        help_binding_line(
            "automatic review",
            "checks every changed turn after writers drain, even without delegation",
            theme,
        ),
        help_binding_line(
            "routing / usage",
            "primary, subagent, and review seats are routed and accounted separately",
            theme,
        ),
        help_blank_line(),
    ];
    lines.extend(general_help_lines(voice_input_supported, theme));
    if mode == UiMode::FullscreenTui {
        lines.extend([
            help_binding_line(
                "F12",
                "toggle mouse text selection / wheel scrolling",
                theme,
            ),
            help_blank_line(),
            help_section_line("Scroll transcript", theme),
            help_binding_line(
                "Wheel / Ctrl+Up/Down / Ctrl+PageUp/Down / Ctrl+Home/End / Ctrl-T",
                "",
                theme,
            ),
            help_binding_line("Alt-T", "expand/collapse latest visible tool output", theme),
            help_blank_line(),
        ]);
    } else {
        lines.extend([
            help_section_line("Read transcript", theme),
            help_binding_line(
                "Ctrl-T",
                "open full transcript reader (Up/Down/PgUp/PgDn, Esc closes)",
                theme,
            ),
            help_blank_line(),
        ]);
    }
    lines.extend([
        help_section_line("Overlays", theme),
        help_binding_line(
            "/subagents",
            "inspect retained implementation and review agent transcripts",
            theme,
        ),
        help_binding_line("F9", "open the review issue ledger and status counters", theme),
        help_binding_line("F11", "open retained subagent transcripts", theme),
        help_binding_line(
            "F10 / Tab",
            "help toggle / accept selected slash command",
            theme,
        ),
        help_blank_line(),
        help_section_line("Config", theme),
        help_binding_line(
            "/mjconfig → ACP Sessions",
            "edit per-server session defaults",
            theme,
        ),
        help_blank_line(),
        help_command_line(
            "Built-in commands:",
            "/clear keeps model; /new applies saved models; /load opens session picker; /export full includes nested agents",
            theme,
        ),
    ]);
    lines
}

fn general_help_lines(voice_input_supported: bool, theme: TerminalTheme) -> Vec<Line<'static>> {
    let mut lines = vec![
        help_section_line("General", theme),
        help_binding_line("Ctrl-N", "new session", theme),
        help_binding_line("Ctrl-O", "load session", theme),
        help_binding_line(
            "Shift-Tab",
            "choose the primary model and reasoning effort",
            theme,
        ),
        help_binding_line("Enter", "send prompt / accept selected item", theme),
        help_binding_line(PROMPT_NEWLINE_HINT, "insert a newline in the prompt", theme),
        help_binding_line("Left/Right", "move the prompt cursor", theme),
        help_binding_line(
            "Up/Down",
            "cursor line or browse prompt history (top/bottom)",
            theme,
        ),
        help_binding_line("PageUp/Down", "move the cursor five lines", theme),
        help_binding_line(
            "Home/End",
            "jump to the start / end of the current line",
            theme,
        ),
        help_binding_line("Ctrl-A/E/B/F", "line start/end and char left/right", theme),
        help_binding_line(
            "Ctrl-K/U/W",
            "delete to end/start of line or previous word",
            theme,
        ),
        help_binding_line(
            "Ctrl-D",
            "delete at cursor; quit when input and chips are empty",
            theme,
        ),
        help_binding_line(
            "Ctrl-C",
            "cancel streaming; clear input/chips; quit when empty",
            theme,
        ),
    ];
    if voice_input_supported {
        lines.push(help_binding_line(
            "🎙 Ctrl-R",
            "start/stop microphone dictation into the prompt",
            theme,
        ));
    }
    lines.extend([
        help_binding_line("Ctrl-V/Ctrl-Alt-V", "paste image from clipboard", theme),
        help_binding_line("Ctrl-Y", "copy last agent message to clipboard", theme),
        help_binding_line(
            "Esc",
            "cancel streaming; clear input, chips, and browsing history",
            theme,
        ),
        help_blank_line(),
        help_section_line("Attachment chips", theme),
        help_binding_line(
            "Backspace / Esc / Enter",
            "remove chip / clear / send chips + input",
            theme,
        ),
        help_blank_line(),
    ]);
    lines
}

// Keep help body text on high-contrast semantic roles: section labels use
// header styling, keybindings use accent + bold, and descriptions use the
// normal text color instead of inheriting the green help-modal chrome.
fn help_section_line(label: &'static str, theme: TerminalTheme) -> Line<'static> {
    Line::from(Span::styled(
        label,
        Style::default()
            .fg(theme.header)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    ))
}

fn help_binding_line(
    binding: &'static str,
    description: &'static str,
    theme: TerminalTheme,
) -> Line<'static> {
    help_binding_line_with_color(binding, description, theme.accent, theme)
}

fn help_binding_line_with_color(
    binding: &'static str,
    description: &'static str,
    binding_color: Color,
    theme: TerminalTheme,
) -> Line<'static> {
    const HELP_BINDING_WIDTH: usize = 27;
    let binding_width = binding.width();
    let gap = HELP_BINDING_WIDTH.saturating_sub(binding_width).max(1);
    let mut spans = vec![
        Span::styled("  ", Style::default().fg(theme.muted)),
        Span::styled(
            binding,
            Style::default()
                .fg(binding_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ".repeat(gap), Style::default().fg(theme.muted)),
    ];
    if !description.is_empty() {
        spans.push(Span::styled(description, Style::default().fg(theme.text)));
    }
    Line::from(spans)
}

fn help_command_line(
    prefix: &'static str,
    description: &'static str,
    theme: TerminalTheme,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            prefix,
            Style::default()
                .fg(theme.header)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", Style::default().fg(theme.text)),
        Span::styled(description, Style::default().fg(theme.text)),
    ])
}

fn help_blank_line() -> Line<'static> {
    Line::from(Span::styled("", Style::default()))
}

fn centered_modal_rect(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn review_picker_lines(state: &AppState) -> Vec<Line<'static>> {
    let selected = state
        .review_picker
        .as_ref()
        .map_or(0, |picker| picker.selected);
    [
        ("Most recent changes", "retained change-producing user turn"),
        ("All uncommitted changes", "staged, unstaged, and untracked"),
        ("HEAD", "changes introduced by the current commit"),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (name, description))| {
        let marker = if index == selected { "› " } else { "  " };
        let style = if index == selected {
            Style::default()
                .fg(state.theme.primary)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(state.theme.text)
        };
        Line::from(vec![
            Span::styled(marker, style),
            Span::styled(name, style),
            Span::styled(
                format!(" — {description}"),
                Style::default().fg(state.theme.muted),
            ),
        ])
    })
    .collect()
}

fn draw_review_picker_modal(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let width = area.width.saturating_sub(8).min(72);
    let height = 7.min(area.height.saturating_sub(2));
    if width < 24 || height < 6 {
        return;
    }
    let rect = centered_modal_rect(area, width, height);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Review target ")
        .style(Style::default().fg(state.theme.primary));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(inner);
    f.render_widget(Paragraph::new(review_picker_lines(state)), layout[0]);
    f.render_widget(
        Paragraph::new("Up/Down choose | Enter review | Esc cancel")
            .style(Style::default().fg(state.theme.muted)),
        layout[1],
    );
}

fn draw_inline_review_picker(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    f.render_widget(Clear, area);
    let content = inline_content_rect(area);
    if content.width == 0 || content.height < 5 {
        return;
    }
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(content);
    f.render_widget(
        Paragraph::new("Review target").style(
            Style::default()
                .fg(state.theme.primary)
                .add_modifier(Modifier::BOLD),
        ),
        layout[0],
    );
    f.render_widget(Paragraph::new(review_picker_lines(state)), layout[1]);
    f.render_widget(
        Paragraph::new("Up/Down choose | Enter review | Esc cancel")
            .style(Style::default().fg(state.theme.muted)),
        layout[2],
    );
}

fn draw_agent_picker_modal(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let Some(picker) = state.agent_picker.as_ref() else {
        return;
    };
    let item_count = match picker.step {
        AgentPickerStep::Model => picker.role_indices.len(),
        AgentPickerStep::Effort => PRIMARY_EFFORT_OPTIONS.len(),
        AgentPickerStep::ConfirmSave => 1,
        AgentPickerStep::StartNewSession => 2,
    };
    let rows = (item_count as u16).min(8);
    let height = (rows + 5).min(area.height.saturating_sub(2));
    let width = area.width.saturating_sub(8).min(72);
    if height < 6 || width < 20 {
        return;
    }
    let rect = centered_modal_rect(area, width, height);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Configure primary model and effort ")
        .style(Style::default().fg(state.theme.primary));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let header = match picker.step {
        AgentPickerStep::Model => vec![
            Line::from("Choose a model for the primary agent."),
            Line::from("Enter continues to reasoning effort."),
        ],
        AgentPickerStep::Effort => vec![
            Line::from("Choose the primary agent's reasoning effort."),
            Line::from("Enter continues to save."),
        ],
        AgentPickerStep::ConfirmSave => vec![
            Line::from("Save this model and reasoning effort?"),
            Line::from(agent_picker_selection_summary(state)),
        ],
        AgentPickerStep::StartNewSession => vec![
            Line::from("Saved. Start a new session now to apply the change?"),
            Line::from("Choose an action below."),
        ],
    };
    f.render_widget(Paragraph::new(header), layout[0]);
    match picker.step {
        AgentPickerStep::ConfirmSave => {
            f.render_widget(Paragraph::new("Press Enter to save."), layout[1]);
        }
        AgentPickerStep::StartNewSession => {
            let start = if picker.start_new_session {
                "› start new session"
            } else {
                "  start new session"
            };
            let keep = if picker.start_new_session {
                "  keep current session"
            } else {
                "› keep current session"
            };
            f.render_widget(
                Paragraph::new(vec![Line::from(start), Line::from(keep)]),
                layout[1],
            );
        }
        AgentPickerStep::Model | AgentPickerStep::Effort => {
            f.render_widget(
                List::new(agent_picker_items(
                    state,
                    layout[1].width,
                    usize::from(layout[1].height),
                )),
                layout[1],
            );
        }
    }
    let footer = match picker.step {
        AgentPickerStep::Model | AgentPickerStep::Effort => "Up/Down or Tab/Shift-Tab to choose",
        AgentPickerStep::ConfirmSave => "Enter save | Esc back",
        AgentPickerStep::StartNewSession => "Up/Down choose | Enter confirm | Esc keep current",
    };
    f.render_widget(
        Paragraph::new(footer).style(Style::default().fg(state.theme.muted)),
        layout[2],
    );
}

fn draw_config_value_picker_modal(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let Some(picker) = state.config_picker.as_ref() else {
        return;
    };

    let Some(option) = state.session_config_options.get(picker.selected_option) else {
        return;
    };
    let Some(choices) = config_option_choices(option) else {
        return;
    };
    let title = format!(" {} values ", option.name);
    let detail = option
        .description
        .clone()
        .unwrap_or_else(|| config_option_current_value_label(option));
    let legend = model_score_legend(state, option);
    let legend_rows = u16::from(legend.is_some());
    let total = picker.filtered_indices.len();
    let selected = picker.selected_value;
    let rows = 8u16;

    let desired_rows = if total == 0 {
        1
    } else {
        (total as u16).min(rows)
    };
    let max_height = if area.height <= 10 {
        area.height
    } else {
        area.height.saturating_sub(4)
    };
    let height = (desired_rows + 5 + legend_rows).min(max_height);
    if height < 6 {
        return;
    }
    let width = area.width.saturating_sub(8).min(90);
    let rect = centered_modal_rect(area, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(state.theme.primary));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2 + legend_rows),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let mut header_lines = vec![Line::from(detail)];
    if let Some(legend) = legend {
        header_lines.push(Line::from(Span::styled(
            legend,
            Style::default().fg(state.theme.muted),
        )));
    }
    header_lines.push(Line::from("Enter to apply | Esc cancel".to_string()));
    let header = Paragraph::new(header_lines).wrap(Wrap { trim: false });
    f.render_widget(header, layout[0]);

    // Search input box
    let search_text = if picker.search_query.is_empty() {
        Line::from(Span::styled(
            "🔍 type to filter...",
            Style::default().fg(state.theme.muted),
        ))
    } else {
        Line::from(vec![
            Span::styled("🔍 ", Style::default().fg(state.theme.muted)),
            Span::raw(picker.search_query.clone()),
        ])
    };
    let search = Paragraph::new(search_text);
    f.render_widget(search, layout[1]);

    if total == 0 {
        let no_matches = Paragraph::new("No matches").style(Style::default().fg(state.theme.muted));
        f.render_widget(no_matches, layout[2]);

        let footer = Paragraph::new("Backspace to clear | Esc cancel")
            .style(Style::default().fg(state.theme.muted));
        f.render_widget(footer, layout[3]);
        return;
    }

    let range = centered_visible_range(total, selected, usize::from(layout[2].height));
    let start = range.start;
    let items = picker.filtered_indices[range]
        .iter()
        .enumerate()
        .map(|(offset, &full_idx)| {
            let absolute = start + offset;
            let marker = if absolute == selected { ">" } else { " " };
            let choice = &choices[full_idx];
            let score = model_choice_score(state, option, choice);
            let line = config_value_row_text(choice, score.as_deref(), layout[2].width);
            truncate_line(line, layout[2].width, marker == ">", state.theme)
        })
        .collect::<Vec<ListItem>>();
    let list = List::new(items);
    f.render_widget(list, layout[2]);

    let filter_hint = if picker.search_query.is_empty() {
        "Up/Down to choose | type to filter | Enter to apply | Esc cancel"
    } else {
        "Up/Down to choose | Backspace to clear | Enter to apply | Esc cancel"
    };
    let footer = Paragraph::new(filter_hint).style(Style::default().fg(state.theme.muted));
    f.render_widget(footer, layout[3]);
}

/// Slash-command autocomplete popover. Anchored to the top edge of the
/// input box and grows upward into the transcript pane so it never
/// covers the user's cursor. Width matches the input box; height caps
/// at 8 visible rows + 2 borders.
fn draw_autocomplete_popover(f: &mut ratatui::Frame, input_area: Rect, state: &AppState) {
    let max_visible_rows = 8u16;
    let desired_rows = (state.autocomplete.matches.len() as u16).min(max_visible_rows);
    if desired_rows == 0 {
        return;
    }
    // Place the popover so its bottom border sits just above the input
    // box. If the transcript pane is short, shrink the number of rows
    // to keep the highlighted item visible.
    let height = (desired_rows + 2).min(input_area.y);
    if height < 3 {
        return;
    }
    let visible_rows = (height - 2) as usize;
    let rect = Rect::new(
        input_area.x,
        input_area.y - height,
        input_area.width,
        height,
    );

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" commands (Tab/Enter accept, Esc cancel) ")
        .style(Style::default().fg(state.theme.primary));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let total = state.autocomplete.matches.len();
    let selected = state.autocomplete.selected;
    let range = centered_visible_range(total, selected, visible_rows);
    let start = range.start;

    let items: Vec<ListItem> = state.autocomplete.matches[range]
        .iter()
        .enumerate()
        .map(|(offset, &cmd_idx)| {
            let absolute = start + offset;
            let cmd = &state.available_commands[cmd_idx];
            let marker = if absolute == selected { ">" } else { " " };
            let hint = cmd
                .input
                .as_ref()
                .map(|i| match i {
                    AvailableCommandInput::Unstructured(u) => format!(" <{}>", u.hint),
                    _ => String::new(),
                })
                .unwrap_or_default();
            let mut line = format!("{marker} /{}{hint}", cmd.name);
            // Append a trimmed description on the same row.
            let description = cmd.description.trim();
            if !description.is_empty() {
                line.push_str("  -- ");
                line.push_str(description);
            }
            truncate_line(line, inner.width, absolute == selected, state.theme)
        })
        .collect();

    let list = List::new(items);
    f.render_widget(list, inner);
}

fn draw_inline_autocomplete_popover(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let max_visible_rows = 8u16;
    let desired_rows = (state.autocomplete.matches.len() as u16).min(max_visible_rows);
    if desired_rows == 0 || area.height < 4 {
        return;
    }
    let height = (desired_rows + 2).min(area.height.saturating_sub(1));
    if height < 3 {
        return;
    }
    let rect = Rect::new(area.x, area.y, area.width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" commands (Tab/Enter accept, Esc cancel) ")
        .style(Style::default().fg(state.theme.primary));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let visible_rows = usize::from(inner.height);
    let total = state.autocomplete.matches.len();
    let selected = state.autocomplete.selected;
    let range = centered_visible_range(total, selected, visible_rows);
    let start = range.start;

    let items: Vec<ListItem> = state.autocomplete.matches[range]
        .iter()
        .enumerate()
        .map(|(offset, &cmd_idx)| {
            let absolute = start + offset;
            let cmd = &state.available_commands[cmd_idx];
            let marker = if absolute == selected { ">" } else { " " };
            let hint = cmd
                .input
                .as_ref()
                .map(|i| match i {
                    AvailableCommandInput::Unstructured(u) => format!(" <{}>", u.hint),
                    _ => String::new(),
                })
                .unwrap_or_default();
            let mut line = format!("{marker} /{}{hint}", cmd.name);
            let description = cmd.description.trim();
            if !description.is_empty() {
                line.push_str("  -- ");
                line.push_str(description);
            }
            truncate_line(line, inner.width, marker == ">", state.theme)
        })
        .collect();
    f.render_widget(List::new(items), inner);
}

fn truncate_line(
    line: String,
    width: u16,
    selected: bool,
    theme: TerminalTheme,
) -> ListItem<'static> {
    let mut line = truncate_text_to_width(line, width);
    if line.is_empty() {
        line.push(' ');
    }
    let style = if selected {
        Style::default()
            .fg(theme.selection_fg)
            .bg(theme.selection_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    ListItem::new(line).style(style)
}

fn config_value_row_text(choice: &ConfigValueChoice, score: Option<&str>, width: u16) -> String {
    let mut line = if let Some(group) = choice.group.as_ref() {
        format!("{group} / {}", choice.name)
    } else {
        choice.name.clone()
    };
    if let Some(description) = choice.description.as_ref()
        && !description.trim().is_empty()
    {
        line.push_str("  -- ");
        line.push_str(description.trim());
    }
    let Some(score) = score else {
        return line;
    };
    let suffix = format!("  {score}");
    let suffix_width = suffix.width();
    let width = usize::from(width);
    if suffix_width >= width {
        return truncate_text_to_width(score.to_string(), width as u16);
    }
    let prefix_width = width - suffix_width;
    let prefix = truncate_text_to_width(line, prefix_width as u16);
    format!("{prefix}{suffix}")
}

/// Attribution shown under a model-selection picker explaining the trailing
/// number, or `None` when scores aren't being rendered (not a model option, or
/// scoring disabled). Keeps a blank score readable as "not ranked".
fn model_score_legend(_state: &AppState, _option: &SessionConfigOption) -> Option<&'static str> {
    None
}

/// The score suffix for one model choice, or `None` when this option isn't a
/// model option or scoring is disabled/uninstalled (so nothing is appended).
fn model_choice_score(
    _state: &AppState,
    _option: &SessionConfigOption,
    _choice: &ConfigValueChoice,
) -> Option<String> {
    None
}

// ===========================================================================
// Ragnarok arena: launch, keys, and the gloriously silly battle scenes
// ===========================================================================

/// How long an action flourish keeps driving a fighter's pose.
const RAGNAROK_ACTION_TTL: Duration = Duration::from_secs(12);
/// Animation frame cadence (shares the spinner heartbeat).
const RAGNAROK_FRAME_MS: u128 = 250;
const RAGNAROK_CARD_MIN_WIDTH: u16 = 24;
// Borders (2) + agent/pass_at_1 line + 7 half-block sprite rows + vigor bar +
// action caption.
const RAGNAROK_CARD_HEIGHT: u16 = 12;
const RAGNAROK_THOR_STRIP_HEIGHT: u16 = 3;
const RAGNAROK_FEED_MIN_HEIGHT: u16 = 3;

/// Arena-local convenience over [`truncate_text_to_width`], which takes an
/// owned `String` and a `u16` width.
fn fit_width(text: impl Into<String>, width: usize) -> String {
    truncate_text_to_width(text.into(), width.min(u16::MAX as usize) as u16)
}

/// Spawn the battle task for a validated `/ragnarok` request.
fn start_ragnarok(
    state: &mut AppState,
    task: String,
    tx: mpsc::UnboundedSender<ragnarok::RagnarokEvent>,
) {
    let (abort_tx, abort_rx) = tokio::sync::watch::channel(false);
    let (proceed_tx, proceed_rx) = tokio::sync::watch::channel(false);
    let cfg = ragnarok::BattleConfig {
        task: task.clone(),
        cwd: state.session_cwd.clone(),
        available_models: state.ragnarok_models.clone(),
        thor_host: active_thor_host(state),
    };
    state.ragnarok = Some(RagnarokUi::new(task, abort_tx, proceed_tx));
    tokio::spawn(ragnarok::run_battle(cfg, tx, abort_rx, proceed_rx));
}

fn drain_ragnarok_draft_pr_publish(
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<ragnarok::RagnarokEvent>,
) {
    while let Some(req) = state.take_ragnarok_draft_pr_publish_request() {
        spawn_ragnarok_draft_pr_publish(req, tx.clone());
    }
}

fn spawn_ragnarok_draft_pr_publish(
    req: ragnarok::DraftPrRequest,
    tx: mpsc::UnboundedSender<ragnarok::RagnarokEvent>,
) {
    let winner = req.winner;
    let _ = tx.send(ragnarok::RagnarokEvent::DraftPrPublishing { winner });
    tokio::spawn(async move {
        let ev = match ragnarok::publish_draft_pr(req).await {
            Ok(url) => ragnarok::RagnarokEvent::DraftPrPublished { winner, url },
            Err(e) => ragnarok::RagnarokEvent::DraftPrFailed {
                winner,
                message: format!("{e:#}"),
            },
        };
        let _ = tx.send(ev);
    });
}

fn active_thor_host(state: &AppState) -> Option<ragnarok::ThorHost> {
    let launch = state.active_agent_launch.clone()?;
    let model = active_model_config(state);
    Some(ragnarok::ThorHost {
        agent_source_id: state.agent_source_id.clone(),
        launch,
        model_value: model.as_ref().map(|(value, _)| value.clone()),
        model_name: model.map(|(_, name)| name),
    })
}

fn active_model_config(state: &AppState) -> Option<(String, String)> {
    state
        .session_config_options
        .iter()
        .find(|option| crate::app::is_model_config_option(option))
        .and_then(|option| {
            let value = crate::app::config_option_current_value_id(option)?.to_string();
            let name = crate::app::config_option_current_value_label(option);
            Some((value, name))
        })
}

fn handle_ragnarok_key(
    state: &mut AppState,
    modifiers: KeyModifiers,
    code: KeyCode,
    mode: UiMode,
) -> TerminalRequest {
    let Some(arena) = state.ragnarok.as_mut() else {
        return TerminalRequest::None;
    };
    let over = arena.battle_over();

    // Ctrl-C always aborts, raging battle or not.
    if modifiers == KeyModifiers::CONTROL && matches!(code, KeyCode::Char('c')) {
        state.close_ragnarok();
        return inline_repair_request(mode);
    }

    let disarm_quit = !matches!(code, KeyCode::Char('q') | KeyCode::Char('Q'));
    match code {
        KeyCode::Esc => {
            arena.quit_armed = false;
            if arena.pane == ArenaPane::Transcript {
                arena.pane = ArenaPane::Arena;
                return inline_repair_request(mode);
            }
            if over {
                state.close_ragnarok();
                return inline_repair_request(mode);
            }
        }
        KeyCode::Char('q') | KeyCode::Char('Q') if over => {
            state.close_ragnarok();
            return inline_repair_request(mode);
        }
        KeyCode::Char('q') | KeyCode::Char('Q') => {
            if arena.quit_armed {
                arena.abort();
                arena.quit_armed = false;
            } else {
                arena.quit_armed = true;
            }
        }
        KeyCode::Up | KeyCode::Char('k') => arena.scroll_feed(1),
        KeyCode::Down | KeyCode::Char('j') => arena.scroll_feed(-1),
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('[') => arena.cycle_fighter(-1),
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(']') => arena.cycle_fighter(1),
        KeyCode::Char('r') => arena.show_review_lane = !arena.show_review_lane,
        KeyCode::Char(c @ '1'..='9') => {
            let picked = (c as u8 - b'1') as usize;
            if over {
                if arena.chosen_finalist.is_none()
                    && let Some(verdict) = &arena.verdict
                    && let Some((a, b)) = verdict.finalists
                {
                    match picked {
                        0 => {
                            arena.chosen_finalist = Some(a);
                            arena.queue_draft_pr_publish(a);
                        }
                        1 => {
                            arena.chosen_finalist = Some(b);
                            arena.queue_draft_pr_publish(b);
                        }
                        _ => {}
                    }
                }
            } else if picked < arena.fighters.len() {
                arena.selected_fighter = picked;
            }
        }
        KeyCode::Enter if over => {
            state.close_ragnarok();
            return inline_repair_request(mode);
        }
        KeyCode::Enter if arena.awaiting_approval() => {
            arena.unleash();
        }
        KeyCode::Enter | KeyCode::Tab | KeyCode::Char('t') | KeyCode::Char('T') => {
            arena.quit_armed = false;
            toggle_ragnarok_pane(arena);
            return inline_repair_request(mode);
        }
        _ => {}
    }
    if disarm_quit && let Some(arena) = state.ragnarok.as_mut() {
        arena.quit_armed = false;
    }
    TerminalRequest::None
}

fn toggle_ragnarok_pane(arena: &mut RagnarokUi) {
    arena.pane = match arena.pane {
        ArenaPane::Arena => ArenaPane::Transcript,
        ArenaPane::Transcript => ArenaPane::Arena,
    };
}

/// Wall-clock animation frame, shared by every arena element.
fn arena_frame() -> usize {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| (elapsed.as_millis() / RAGNAROK_FRAME_MS) as usize)
        .unwrap_or(0)
}

fn battle_clock(arena: &RagnarokUi) -> String {
    let secs = arena.started_at.elapsed().as_secs();
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}

/// A stable, distinct-ish color per fighter.
fn fighter_color(theme: TerminalTheme, id: ragnarok::FighterId) -> Color {
    let cycle = [
        theme.primary,
        theme.secondary,
        theme.success,
        theme.warning,
        theme.tool,
        theme.accent,
        theme.user,
        theme.terminal,
        theme.error,
        theme.quote,
    ];
    cycle[id % cycle.len()]
}

fn draw_ragnarok(f: &mut ratatui::Frame, area: Rect, state: &mut AppState) {
    let Some(arena) = state.ragnarok.as_ref() else {
        return;
    };
    let theme = state.theme;
    f.render_widget(Clear, area);
    if area.width < 10 || area.height < 4 {
        let line = Line::from(Span::styled(
            "⚡ RAGNAROK rages (terminal too small for the arena)",
            Style::default().fg(theme.warning),
        ));
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    match arena.pane {
        ArenaPane::Arena => draw_ragnarok_arena_pane(f, area, arena, theme),
        ArenaPane::Transcript => draw_ragnarok_transcript_pane(f, area, arena, theme),
    }
}

fn ragnarok_banner_line(arena: &RagnarokUi, theme: TerminalTheme, width: u16) -> Line<'static> {
    let frame = arena_frame();
    let bolts = ["⚡", "🔥", "⚡", "☄"];
    let bolt = bolts[frame % bolts.len()];
    let phase = if arena.failed.is_some() {
        "THE BATTLE IS LOST".to_string()
    } else {
        arena.phase.banner().to_string()
    };
    let text = format!(
        "{bolt} RAGNAROK ━ {} ━ {} {bolt}",
        phase,
        battle_clock(arena)
    );
    let text = fit_width(&text, width as usize);
    Line::from(Span::styled(
        text,
        Style::default()
            .fg(if arena.failed.is_some() {
                theme.error
            } else {
                theme.warning
            })
            .add_modifier(Modifier::BOLD),
    ))
    .centered()
}

fn ragnarok_footer_line(arena: &RagnarokUi, theme: TerminalTheme, width: u16) -> Line<'static> {
    let over = arena.battle_over();
    let hints = if arena.quit_armed {
        "⚠ q again to quit Ragnarok (Esc cancels) ⚠".to_string()
    } else if over {
        match arena.verdict.as_ref().and_then(|v| v.finalists) {
            Some(_) if arena.chosen_finalist.is_none() => {
                "1/2 choose finalist · Enter accept & close · t transcripts · q close".to_string()
            }
            None => {
                "Enter/q close · t transcripts · ↑/↓ feed · ←/→ fighter · r review lane".to_string()
            }
            Some(_) => {
                "Enter/q close · t transcripts · ↑/↓ feed · ←/→ fighter · r review lane".to_string()
            }
        }
    } else if arena.awaiting_approval() {
        "⚔ Enter to UNLEASH RAGNAROK (no combat spend yet) · ↑/↓ feed · q quit".to_string()
    } else {
        match arena.pane {
            ArenaPane::Arena => {
                "Enter transcript · ↑/↓ feed · ←/→ fighter · 1-9 select · q quit".to_string()
            }
            ArenaPane::Transcript => {
                "Esc arena · Enter arena · ←/→ fighter · r review lane · q quit".to_string()
            }
        }
    };
    if arena.awaiting_approval() && !arena.quit_armed {
        let style = Style::default()
            .fg(theme.warning)
            .add_modifier(Modifier::BOLD);
        return Line::from(Span::styled(fit_width(&hints, width as usize), style)).centered();
    }
    let style = if arena.quit_armed {
        Style::default()
            .fg(theme.error)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.muted)
    };
    Line::from(Span::styled(fit_width(&hints, width as usize), style)).centered()
}

fn ragnarok_stage_height(arena: &RagnarokUi, width: u16, max_height: u16) -> u16 {
    if max_height == 0 {
        return 0;
    }

    match arena.phase {
        ragnarok::Phase::Judgment | ragnarok::Phase::Verdict => return max_height,
        ragnarok::Phase::Mustering | ragnarok::Phase::Routing if arena.fighters.is_empty() => {
            return max_height.clamp(1, 10);
        }
        _ => {}
    }

    if arena.fighters.is_empty() {
        return max_height.clamp(1, 8);
    }

    let n = arena.fighters.len() as u16;
    let cols = (width / RAGNAROK_CARD_MIN_WIDTH).clamp(1, n);
    let rows = n.div_ceil(cols);
    let thor_height = if arena.thor_action.is_some() {
        RAGNAROK_THOR_STRIP_HEIGHT
    } else {
        0
    };
    rows.saturating_mul(RAGNAROK_CARD_HEIGHT)
        .saturating_add(thor_height)
        .min(max_height)
        .max(1)
}

fn draw_ragnarok_arena_pane(
    f: &mut ratatui::Frame,
    area: Rect,
    arena: &RagnarokUi,
    theme: TerminalTheme,
) {
    // Banner / task / stage / feed / footer.
    let middle_height = area.height.saturating_sub(3);
    let feed_min = if middle_height > RAGNAROK_FEED_MIN_HEIGHT {
        RAGNAROK_FEED_MIN_HEIGHT
    } else {
        middle_height.saturating_sub(1)
    };
    let stage_max = middle_height.saturating_sub(feed_min);
    let stage_height = ragnarok_stage_height(arena, area.width, stage_max);
    let feed_height = middle_height.saturating_sub(stage_height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(stage_height),
            Constraint::Length(feed_height),
            Constraint::Length(1),
        ])
        .split(area);

    f.render_widget(
        Paragraph::new(ragnarok_banner_line(arena, theme, chunks[0].width)),
        chunks[0],
    );
    let task_line = Line::from(vec![
        Span::styled("quest: ", Style::default().fg(theme.muted)),
        Span::styled(
            fit_width(
                arena.task.replace('\n', " "),
                chunks[1].width.saturating_sub(7) as usize,
            ),
            Style::default().fg(theme.text),
        ),
    ]);
    f.render_widget(Paragraph::new(task_line), chunks[1]);

    match arena.phase {
        ragnarok::Phase::Verdict => draw_ragnarok_verdict(f, chunks[2], arena, theme),
        ragnarok::Phase::Judgment => draw_ragnarok_judgment(f, chunks[2], arena, theme),
        ragnarok::Phase::Mustering | ragnarok::Phase::Routing if arena.fighters.is_empty() => {
            draw_ragnarok_summoning(f, chunks[2], arena, theme)
        }
        _ => draw_ragnarok_fighters(f, chunks[2], arena, theme),
    }

    draw_ragnarok_feed(f, chunks[3], arena, theme);
    f.render_widget(
        Paragraph::new(ragnarok_footer_line(arena, theme, chunks[4].width)),
        chunks[4],
    );
}

fn current_thor_action(arena: &RagnarokUi) -> ragnarok::ThorAction {
    arena.thor_action.unwrap_or(match arena.phase {
        ragnarok::Phase::Routing => ragnarok::ThorAction::Deciding,
        ragnarok::Phase::Review => ragnarok::ThorAction::Assigning,
        ragnarok::Phase::Judgment | ragnarok::Phase::Verdict => ragnarok::ThorAction::Judging,
        _ => ragnarok::ThorAction::Descending,
    })
}

fn thor_action_lines(arena: &RagnarokUi, theme: TerminalTheme, width: u16) -> Vec<Line<'static>> {
    let frame = arena_frame();
    let action = current_thor_action(arena);
    let pulse = ["·", "✦", "✶", "✦"][frame % 4];
    let drift = ["  ", " ", "", " "][frame % 4];
    let (title, art): (&str, [&str; 2]) = match action {
        ragnarok::ThorAction::Descending => (
            "THOR DESCENDS",
            [
                "      storm splits open     ",
                "        helm first           ",
            ],
        ),
        ragnarok::ThorAction::Deciding => (
            "THOR DECIDES",
            ["   [ task ] <=?=> [ field ]", "       runes turn in place "],
        ),
        ragnarok::ThorAction::Assigning => (
            "THOR ASSIGNS RIVALS",
            [
                "   champion -> rival -> champion",
                "       blades cross on command ",
            ],
        ),
        ragnarok::ThorAction::Judging => (
            "THOR JUDGES",
            [
                "          verdict scales       ",
                "       hammer over the record  ",
            ],
        ),
        ragnarok::ThorAction::Mercy => (
            "THOR WEIGHS MERCY",
            [
                "      hourglass against hammer ",
                "        spare or strike now    ",
            ],
        ),
    };
    let age = arena.thor_action_at.elapsed().as_millis() / RAGNAROK_FRAME_MS;
    let intensity = match age {
        0..=3 => "!!!",
        4..=10 => "!! ",
        _ => "!  ",
    };
    [
        format!("{pulse} {title} {intensity} {pulse}"),
        format!("{drift}{}", art[0]),
        format!("{}{}", " ".repeat(frame % 3), art[1]),
    ]
    .into_iter()
    .map(|line| {
        Line::from(Span::styled(
            fit_width(line, width as usize),
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        ))
        .centered()
    })
    .collect()
}

fn draw_thor_action_strip(
    f: &mut ratatui::Frame,
    area: Rect,
    arena: &RagnarokUi,
    theme: TerminalTheme,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let lines: Vec<Line> = thor_action_lines(arena, theme, area.width)
        .into_iter()
        .take(area.height as usize)
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

fn thor_descending_scene_rows(frame: usize) -> Vec<String> {
    let drop = ["      ", "    ", "  ", " "][frame % 4];
    let cape = [" /|\\ ", " \\|/ ", " /|\\ ", "\\ | /"][frame % 4];
    let sparks = ["  \\ | /", "-- ᚦ --", "  / | \\", "-- ⚡ --"][frame % 4];
    vec![
        format!("{drop}{sparks}"),
        format!("{drop}        _/\\_        "),
        format!("{drop}      _/ᛏ  ᛏ\\_      "),
        format!("{drop}     /_( ᚨ ᚨ )_\\     "),
        format!("{drop}       \\_===_/   __==#"),
        format!("{drop}       {cape}   /"),
        format!("{drop}      /_| ᛉ  |_\\/ "),
        format!("{drop}        /_/ \\_\\     "),
    ]
}

fn thor_summoning_scene_rows(arena: &RagnarokUi, frame: usize) -> Vec<String> {
    match current_thor_action(arena) {
        ragnarok::ThorAction::Descending => thor_descending_scene_rows(frame),
        ragnarok::ThorAction::Deciding => {
            let glow = ["✦", "✧", "✶", "✧"][frame % 4];
            vec![
                format!("        {glow} ᚱ  ᚢ  ᚾ  ᛖ {glow}        "),
                "      .-----------------.     ".to_string(),
                "      | task | field | cost | ".to_string(),
                "      '-----------------'     ".to_string(),
                "          \\  judgment  /      ".to_string(),
                "           \\_  ___  _/        ".to_string(),
                "             /_/ \\_\\          ".to_string(),
            ]
        }
        _ => {
            let spark = ["✦", "✧", "✶", "✧"][frame % 4];
            vec![
                format!("          {spark} THOR STANDS READY {spark}"),
                "              _/\\_              ".to_string(),
                "            _/ᛏ  ᛏ\\_            ".to_string(),
                "            (  ᚨ ᚨ  )     __==# ".to_string(),
                "             \\_===_/     /      ".to_string(),
                "             /| ᛉ |\\   /       ".to_string(),
                "            /_|___|_\\          ".to_string(),
            ]
        }
    }
}

/// Pre-roster splash: Thor descends and the route visibly changes state.
fn draw_ragnarok_summoning(
    f: &mut ratatui::Frame,
    area: Rect,
    arena: &RagnarokUi,
    theme: TerminalTheme,
) {
    let frame = arena_frame();
    let mut lines = thor_action_lines(arena, theme, area.width);
    lines.push(Line::default());
    let art = thor_summoning_scene_rows(arena, frame);
    lines.extend(
        art.into_iter()
            .map(|l| Line::from(Span::styled(l, Style::default().fg(theme.accent))).centered()),
    );
    lines.push(
        Line::from(Span::styled(
            match arena.phase {
                ragnarok::Phase::Mustering => "« the war horn calls champions to the arena »",
                _ => "« Thor weighs the quest upon his scales »",
            },
            Style::default().fg(theme.muted),
        ))
        .centered(),
    );
    f.render_widget(Paragraph::new(lines), area);
}

/// The main combat grid: one animated card per champion.
fn draw_ragnarok_fighters(
    f: &mut ratatui::Frame,
    area: Rect,
    arena: &RagnarokUi,
    theme: TerminalTheme,
) {
    if arena.fighters.is_empty() {
        return;
    }
    let cards_area = if arena.thor_action.is_some() && area.height > RAGNAROK_THOR_STRIP_HEIGHT {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(RAGNAROK_THOR_STRIP_HEIGHT),
                Constraint::Min(0),
            ])
            .split(area);
        draw_thor_action_strip(f, chunks[0], arena, theme);
        chunks[1]
    } else {
        area
    };
    if cards_area.height == 0 {
        return;
    }

    let n = arena.fighters.len() as u16;
    let cols = (cards_area.width / RAGNAROK_CARD_MIN_WIDTH).clamp(1, n);
    let rows = n.div_ceil(cols);

    // Not enough vertical room for cards: compact one-line-per-fighter view.
    if cards_area.height < rows * RAGNAROK_CARD_HEIGHT {
        let lines: Vec<Line> = arena
            .fighters
            .iter()
            .enumerate()
            .take(cards_area.height as usize)
            .map(|(i, fighter)| compact_fighter_line(arena, fighter, i, theme, cards_area.width))
            .collect();
        f.render_widget(Paragraph::new(lines), cards_area);
        return;
    }

    let row_rects = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            std::iter::repeat_n(Constraint::Length(RAGNAROK_CARD_HEIGHT), rows as usize)
                .chain(std::iter::once(Constraint::Min(0)))
                .collect::<Vec<_>>(),
        )
        .split(cards_area);
    for row in 0..rows {
        let start = (row * cols) as usize;
        let in_row = arena
            .fighters
            .len()
            .saturating_sub(start)
            .min(cols as usize);
        if in_row == 0 {
            break;
        }
        let col_rects = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(vec![Constraint::Ratio(1, in_row as u32); in_row])
            .split(row_rects[row as usize]);
        for col in 0..in_row {
            let idx = start + col;
            draw_fighter_card(f, col_rects[col], arena, &arena.fighters[idx], idx, theme);
        }
    }
}

fn compact_fighter_line(
    arena: &RagnarokUi,
    fighter: &RagnarokFighterUi,
    index: usize,
    theme: TerminalTheme,
    width: u16,
) -> Line<'static> {
    let selected = index == arena.selected_fighter;
    let marker = if selected { "▶" } else { " " };
    let (state_word, state_color) = fighter_state_label(&fighter.state, theme);
    let action = fighter
        .action
        .as_ref()
        .filter(|(_, _, at)| at.elapsed() < RAGNAROK_ACTION_TTL)
        .map(|(kind, detail, _)| format!(" {} {detail}", action_glyph(*kind)))
        .or_else(|| ambient_combat_caption(fighter).map(|caption| format!(" {caption}")))
        .unwrap_or_default();
    let text = format!(
        "{marker}{} {} — {state_word}{action}",
        pose_for(fighter)[1].trim(),
        fighter.card.tag(),
    );
    Line::from(Span::styled(
        fit_width(&text, width as usize),
        Style::default().fg(state_color),
    ))
}

fn fighter_state_label(state: &ragnarok::FighterState, theme: TerminalTheme) -> (String, Color) {
    match state {
        ragnarok::FighterState::Summoned => ("SUMMONED".to_string(), theme.muted),
        ragnarok::FighterState::Forging => ("FORGING CAMP".to_string(), theme.accent),
        ragnarok::FighterState::Connecting => ("APPROACHING".to_string(), theme.accent),
        ragnarok::FighterState::Fighting => ("FIGHTING".to_string(), theme.warning),
        ragnarok::FighterState::Capturing => ("TALLYING".to_string(), theme.secondary),
        ragnarok::FighterState::Standing => ("STANDING".to_string(), theme.success),
        ragnarok::FighterState::Slain(_) => ("SLAIN".to_string(), theme.error),
    }
}

fn action_glyph(kind: ragnarok::ActionKind) -> &'static str {
    match kind {
        ragnarok::ActionKind::Forge => "🔨",
        ragnarok::ActionKind::Strike => "⚡",
        ragnarok::ActionKind::Scry => "🔮",
        ragnarok::ActionKind::Chant => "🎵",
        ragnarok::ActionKind::Ponder => "💭",
        ragnarok::ActionKind::Wound => "🩸",
        ragnarok::ActionKind::Guard => "🛡",
    }
}

fn live_action_kind(fighter: &RagnarokFighterUi) -> Option<ragnarok::ActionKind> {
    fighter
        .action
        .as_ref()
        .filter(|(_, _, at)| at.elapsed() < RAGNAROK_ACTION_TTL)
        .map(|(kind, _, _)| *kind)
}

fn ambient_combat_action(fighter: &RagnarokFighterUi) -> Option<ragnarok::ActionKind> {
    if !matches!(
        fighter.state,
        ragnarok::FighterState::Fighting | ragnarok::FighterState::Capturing
    ) || live_action_kind(fighter).is_some()
    {
        return None;
    }
    let beat = (arena_frame() / 2)
        .wrapping_add(fighter.card.id * 2)
        .wrapping_add(fighter.actions_seen as usize);
    Some(match beat % 6 {
        0 | 3 => ragnarok::ActionKind::Strike,
        1 | 4 => ragnarok::ActionKind::Forge,
        2 => ragnarok::ActionKind::Scry,
        _ => ragnarok::ActionKind::Chant,
    })
}

fn animated_action_kind(fighter: &RagnarokFighterUi) -> Option<ragnarok::ActionKind> {
    live_action_kind(fighter).or_else(|| ambient_combat_action(fighter))
}

fn ambient_combat_caption(fighter: &RagnarokFighterUi) -> Option<&'static str> {
    let action = ambient_combat_action(fighter)?;
    Some(match action {
        ragnarok::ActionKind::Strike => "⚡ leaping strike",
        ragnarok::ActionKind::Forge => "🔨 hammer feint",
        ragnarok::ActionKind::Scry => "🔮 reading the field",
        ragnarok::ActionKind::Chant => "🎵 rallying cry",
        _ => "⚔ pressing the attack",
    })
}

fn fighter_bounce_offset(fighter: &RagnarokFighterUi) -> usize {
    if !matches!(
        fighter.state,
        ragnarok::FighterState::Fighting | ragnarok::FighterState::Capturing
    ) {
        return 0;
    }
    let beat = arena_frame()
        .wrapping_add(fighter.card.id)
        .wrapping_add(fighter.actions_seen as usize);
    usize::from(beat % 4 == 1)
}

/// Which pixel-art animation a fighter plays right now, plus the accent color
/// for its `M` pixels (sparks, lightning, orb, notes, blood).
fn sprite_for(fighter: &RagnarokFighterUi, theme: TerminalTheme) -> (SpriteKind, Color) {
    const GOLD: Color = Color::Rgb(240, 196, 60);
    const BLOOD: Color = Color::Rgb(202, 44, 44);
    match &fighter.state {
        ragnarok::FighterState::Slain(_) => (SpriteKind::Slain, BLOOD),
        ragnarok::FighterState::Standing => (SpriteKind::Victor, GOLD),
        ragnarok::FighterState::Summoned
        | ragnarok::FighterState::Forging
        | ragnarok::FighterState::Connecting => (SpriteKind::March, theme.muted),
        _ => match animated_action_kind(fighter) {
            Some(ragnarok::ActionKind::Forge) => (SpriteKind::Swing, Color::Rgb(255, 150, 60)),
            Some(ragnarok::ActionKind::Strike) => (SpriteKind::Swing, Color::Rgb(250, 224, 84)),
            Some(ragnarok::ActionKind::Scry) => (SpriteKind::Cast, Color::Rgb(196, 112, 240)),
            Some(ragnarok::ActionKind::Ponder) => (SpriteKind::Cast, Color::Rgb(176, 176, 188)),
            Some(ragnarok::ActionKind::Chant) => (SpriteKind::Cast, GOLD),
            Some(ragnarok::ActionKind::Wound) => (SpriteKind::Wound, BLOOD),
            Some(ragnarok::ActionKind::Guard) | None => (SpriteKind::Idle, theme.accent),
        },
    }
}

/// The 3-line ASCII pose for a fighter, chosen by state + recent action and
/// animated by the shared wall-clock frame.
fn pose_for(fighter: &RagnarokFighterUi) -> [&'static str; 3] {
    type Pose = [&'static str; 3];
    const IDLE: [Pose; 2] = [[" o ", "/|\\", "/ \\"], [" o ", "\\|/", "/ \\"]];
    const MARCH: [Pose; 2] = [[" o ", "/|\\", "/< "], [" o ", "/|\\", " >\\"]];
    const FORGE: [Pose; 4] = [
        [" o_T", "/| ", "/ \\"],
        [" oT ", "/|\\", "/ \\"],
        ["_o  ", "T|\\", "/ \\"],
        [" o__", "/|T", "/ \\"],
    ];
    const STRIKE: [Pose; 4] = [
        ["\\o~z", " |  ", "/ \\"],
        [" o~z", "/|  ", "/ \\"],
        [" o  ", "/|~z", "/ \\"],
        ["\\o/ ", " |z ", "/ \\"],
    ];
    const SCRY: [Pose; 2] = [[" o ", "/|(@)", "/ \\"], [" o ", "/|(o)", "/ \\"]];
    const CHANT: [Pose; 2] = [["\\o/", " | d", "/ \\"], [" o/", "/| b", "/ \\"]];
    const PONDER: [Pose; 2] = [[".oO", " |\\", "/ \\"], ["oO°", " |\\", "/ \\"]];
    const WOUND: [Pose; 2] = [[" o ", "x|/", "/ \\"], ["\\o ", "x| ", "_/\\"]];
    const VICTOR: [Pose; 2] = [["\\o/", " | ", "/ \\"], [" o ", "\\|/", "/ \\"]];
    const SLAIN: [Pose; 1] = [["   ", "x_x", "_/\\"]];

    let frame = arena_frame();
    let pick = |poses: &'static [Pose]| poses[frame % poses.len()];
    match &fighter.state {
        ragnarok::FighterState::Slain(_) => pick(&SLAIN),
        ragnarok::FighterState::Standing => pick(&VICTOR),
        ragnarok::FighterState::Summoned
        | ragnarok::FighterState::Forging
        | ragnarok::FighterState::Connecting => pick(&MARCH),
        _ => match animated_action_kind(fighter) {
            Some(ragnarok::ActionKind::Forge) => pick(&FORGE),
            Some(ragnarok::ActionKind::Strike) => pick(&STRIKE),
            Some(ragnarok::ActionKind::Scry) => pick(&SCRY),
            Some(ragnarok::ActionKind::Chant) => pick(&CHANT),
            Some(ragnarok::ActionKind::Ponder) => pick(&PONDER),
            Some(ragnarok::ActionKind::Wound) => pick(&WOUND),
            Some(ragnarok::ActionKind::Guard) => pick(&IDLE),
            None => pick(&IDLE),
        },
    }
}

/// Animated "vigor" bar: marching while fighting, full when standing,
/// skulls when slain.
fn vigor_bar(fighter: &RagnarokFighterUi, width: usize) -> String {
    let width = width.max(4);
    match &fighter.state {
        ragnarok::FighterState::Slain(_) => "☠ ".repeat(width / 2),
        ragnarok::FighterState::Standing => "▓".repeat(width),
        ragnarok::FighterState::Fighting | ragnarok::FighterState::Capturing => {
            let frame = arena_frame().wrapping_add(fighter.card.id * 3);
            (0..width)
                .map(|i| {
                    if (i + frame).is_multiple_of(4) {
                        '░'
                    } else {
                        '▓'
                    }
                })
                .collect()
        }
        _ => "░".repeat(width),
    }
}

fn draw_fighter_card(
    f: &mut ratatui::Frame,
    area: Rect,
    arena: &RagnarokUi,
    fighter: &RagnarokFighterUi,
    index: usize,
    theme: TerminalTheme,
) {
    if area.width < 6 || area.height < 3 {
        return;
    }
    let selected = index == arena.selected_fighter;
    let color = fighter_color(theme, fighter.card.id);
    let (state_word, state_color) = fighter_state_label(&fighter.state, theme);
    let border_style = if selected {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.muted)
    };
    let title = format!(
        " {}{} ",
        if selected { "▶ " } else { "" },
        fighter.card.model_name
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(
            fit_width(&title, area.width.saturating_sub(2) as usize),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let inner_width = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        fit_width(
            format!(
                "{} ⚡{:.1}% · ${:.2}",
                fighter.card.agent_source_id,
                fighter.card.pass_at_1_bps as f64 / 100.0,
                fighter.card.mean_cost_usd
            ),
            inner_width,
        ),
        Style::default().fg(theme.muted),
    )));

    let (sprite_kind, accent) = sprite_for(fighter, theme);
    let frame_set = ragnarok_sprites::frames(sprite_kind);
    // Offset each viking's animation by their id so the shield wall doesn't
    // march in eerie unison.
    let frame = &frame_set[arena_frame().wrapping_add(fighter.card.id) % frame_set.len()];
    let pad = " ".repeat(inner_width.saturating_sub(ragnarok_sprites::SPRITE_W) / 2);
    let sprite_lines = ragnarok_sprites::render(frame, color, accent);
    let bounce = fighter_bounce_offset(fighter);
    let sprite_rows = sprite_lines.len();
    if bounce > 0 {
        lines.push(Line::default());
    }
    for sprite_line in sprite_lines
        .into_iter()
        .take(sprite_rows.saturating_sub(bounce))
    {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(sprite_line.spans.len() + 1);
        spans.push(Span::raw(pad.clone()));
        spans.extend(sprite_line.spans);
        lines.push(Line::from(spans));
    }

    let bar_width = inner_width.saturating_sub(state_word.len() + 2).max(4);
    lines.push(Line::from(vec![
        Span::styled(
            vigor_bar(fighter, bar_width),
            Style::default().fg(state_color),
        ),
        Span::raw(" "),
        Span::styled(
            state_word,
            Style::default()
                .fg(state_color)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    let caption = match &fighter.state {
        ragnarok::FighterState::Slain(reason) => format!("☠ {reason}"),
        _ => fighter
            .action
            .as_ref()
            .filter(|(_, _, at)| at.elapsed() < RAGNAROK_ACTION_TTL)
            .map(|(kind, detail, _)| format!("{} {detail}", action_glyph(*kind)))
            .or_else(|| {
                fighter.review_progress.map(|p| {
                    match p {
                        ragnarok::ReviewProgress::Connecting => "🗡 sharpening the quill…",
                        ragnarok::ReviewProgress::Reviewing => "🗡 dissecting a rival…",
                        ragnarok::ReviewProgress::Done => "🗡 review delivered",
                        ragnarok::ReviewProgress::Failed => "🗡 review lost",
                    }
                    .to_string()
                })
            })
            .or_else(|| ambient_combat_caption(fighter).map(str::to_string))
            .unwrap_or_default(),
    };
    lines.push(Line::from(Span::styled(
        fit_width(&caption, inner_width),
        Style::default().fg(theme.subtle),
    )));

    lines.truncate(inner.height as usize);
    f.render_widget(Paragraph::new(lines), inner);
}

/// The scrolling battle feed, colored per fighter.
fn draw_ragnarok_feed(
    f: &mut ratatui::Frame,
    area: Rect,
    arena: &RagnarokUi,
    theme: TerminalTheme,
) {
    let visible_rows = area.height.saturating_sub(1) as usize;
    let scroll = arena.feed_scroll_for_rows(visible_rows);
    let max_scroll = arena.feed_max_scroll_for_rows(visible_rows);
    let title = if max_scroll == 0 {
        " ⚔ battle feed ".to_string()
    } else if scroll == 0 {
        format!(" ⚔ battle feed · live · ↑ {max_scroll} ")
    } else {
        format!(" ⚔ battle feed · {scroll}/{max_scroll} older · ↓ live ")
    };
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme.muted))
        .title(Span::styled(
            fit_width(title, area.width as usize),
            Style::default().fg(theme.muted),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let take = inner.height as usize;
    let scroll = arena.feed_scroll_for_rows(take);
    let end = arena.feed.len().saturating_sub(scroll);
    let start = end.saturating_sub(take);
    let lines: Vec<Line> = arena
        .feed
        .iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|(fighter, text)| {
            let color = match fighter {
                Some(id) => fighter_color(theme, *id),
                None => theme.text,
            };
            Line::from(Span::styled(
                fit_width(text, inner.width as usize),
                Style::default().fg(color),
            ))
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

/// Judgment scene: Mjölnir hovers while Thor's verdict streams in.
fn draw_ragnarok_judgment(
    f: &mut ratatui::Frame,
    area: Rect,
    arena: &RagnarokUi,
    theme: TerminalTheme,
) {
    let frame = arena_frame();
    let aura = ["✦", "✧", "✶", "✧"][frame % 4];
    let art = [
        format!("{aura}  ______  {aura}"),
        " [______]___".to_string(),
        "    ||      ".to_string(),
        format!("    ||   « THOR SITS IN JUDGMENT {aura} »"),
    ];
    let mut lines: Vec<Line> = art
        .into_iter()
        .map(|l| Line::from(Span::styled(l, Style::default().fg(theme.warning))).centered())
        .collect();
    lines.push(Line::default());
    let remaining = (area.height as usize).saturating_sub(lines.len());
    if remaining > 0 {
        let width = area.width.saturating_sub(2) as usize;
        for l in wrap_tail_lines(&arena.thor_text, width.max(8), remaining) {
            lines.push(Line::from(Span::styled(
                l,
                Style::default().fg(theme.thought),
            )));
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Verdict scene: crown the winner, or stage the finalists for the user.
fn draw_ragnarok_verdict(
    f: &mut ratatui::Frame,
    area: Rect,
    arena: &RagnarokUi,
    theme: TerminalTheme,
) {
    let Some(verdict) = arena.verdict.as_ref() else {
        return;
    };
    let width = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();

    match (verdict.clear_winner, verdict.finalists) {
        (Some(id), _) => {
            let tag = arena
                .fighter(id)
                .map(|f| f.card.tag())
                .unwrap_or_else(|| format!("champion {id}"));
            let crown = ["👑", "✨👑✨", "👑"][arena_frame() % 3];
            lines.push(
                Line::from(Span::styled(
                    format!("{crown} VICTOR: {tag} {crown}"),
                    Style::default()
                        .fg(theme.success)
                        .add_modifier(Modifier::BOLD),
                ))
                .centered(),
            );
            if let Some(name) = arena.fighter(id).and_then(|f| f.worktree_name.clone()) {
                lines.push(
                    Line::from(Span::styled(
                        format!("Thor recommends this work — adopt it: mj --worktree {name}"),
                        Style::default().fg(theme.accent),
                    ))
                    .centered(),
                );
            }
        }
        (None, Some((a, b))) => {
            lines.push(
                Line::from(Span::styled(
                    "⚖ SPLIT DECISION — choose your champion ⚖",
                    Style::default()
                        .fg(theme.warning)
                        .add_modifier(Modifier::BOLD),
                ))
                .centered(),
            );
            for (n, id) in [a, b].into_iter().enumerate() {
                let chosen = arena.chosen_finalist == Some(id);
                let tag = arena
                    .fighter(id)
                    .map(|f| f.card.tag())
                    .unwrap_or_else(|| format!("champion {id}"));
                let wt = arena
                    .fighter(id)
                    .and_then(|f| f.worktree_name.clone())
                    .unwrap_or_default();
                let marker = if chosen { " ← your pick" } else { "" };
                lines.push(Line::from(Span::styled(
                    fit_width(
                        format!("  [{}] {tag} — worktree {wt}{marker}", n + 1),
                        width,
                    ),
                    if chosen {
                        Style::default()
                            .fg(theme.success)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(fighter_color(theme, id))
                    },
                )));
            }
        }
        (None, None) => {}
    }

    if let Some(line) = ragnarok_draft_pr_status_line(arena, width) {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            line,
            Style::default().fg(theme.accent),
        )));
    }

    if verdict.thor_fallback {
        lines.push(Line::from(Span::styled(
            fit_width(
                "(Thor's judgment was garbled; finalists stand in Pass@1 order)",
                width,
            ),
            Style::default().fg(theme.muted),
        )));
    }

    if !verdict.ranking.is_empty() {
        let names: Vec<String> = verdict
            .ranking
            .iter()
            .map(|id| arena.fighter_name(*id))
            .collect();
        lines.push(Line::from(Span::styled(
            fit_width(format!("ranking: {}", names.join(" > ")), width),
            Style::default().fg(theme.subtle),
        )));
    }
    for rv in &verdict.review_verdicts {
        lines.push(Line::from(Span::styled(
            fit_width(
                format!(
                    "🔍 {} on {} — honesty {}/10, validity {}/10: {}",
                    arena.fighter_name(rv.reviewer),
                    arena.fighter_name(rv.defender),
                    rv.honesty,
                    rv.validity,
                    rv.notes
                ),
                width,
            ),
            Style::default().fg(theme.thought),
        )));
    }

    lines.push(Line::default());
    let used = lines.len();
    let remaining = (area.height as usize).saturating_sub(used);
    if remaining > 0 {
        for l in wrap_tail_lines(&verdict.reasoning, width.max(8), remaining) {
            lines.push(Line::from(Span::styled(l, Style::default().fg(theme.text))));
        }
    }
    lines.truncate(area.height as usize);
    f.render_widget(Paragraph::new(lines), area);
}

fn ragnarok_draft_pr_status_line(arena: &RagnarokUi, width: usize) -> Option<String> {
    let status = arena.draft_pr_status.as_ref()?;
    let line = match status {
        RagnarokDraftPrStatus::Publishing { winner } => {
            format!("Draft PR: publishing {}...", arena.fighter_name(*winner))
        }
        RagnarokDraftPrStatus::Published { winner, url } => {
            format!("Draft PR for {}: {url}", arena.fighter_name(*winner))
        }
        RagnarokDraftPrStatus::Failed { winner, message } => format!(
            "Draft PR for {} failed: {message}",
            arena.fighter_name(*winner)
        ),
    };
    Some(fit_width(line, width))
}

/// Transcript pane: live per-fighter output (combat work or their review).
fn draw_ragnarok_transcript_pane(
    f: &mut ratatui::Frame,
    area: Rect,
    arena: &RagnarokUi,
    theme: TerminalTheme,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    f.render_widget(
        Paragraph::new(ragnarok_banner_line(arena, theme, chunks[0].width)),
        chunks[0],
    );

    if arena.fighters.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no champions yet — the muster is still on",
                Style::default().fg(theme.muted),
            ))),
            chunks[2],
        );
        f.render_widget(
            Paragraph::new(ragnarok_footer_line(arena, theme, chunks[3].width)),
            chunks[3],
        );
        return;
    }

    let idx = arena.selected_fighter.min(arena.fighters.len() - 1);
    let fighter = &arena.fighters[idx];
    let lane = if arena.show_review_lane {
        "review"
    } else {
        "combat"
    };
    let header = format!(
        "◀ {} ▶  ({}/{})  lane: {lane} (r toggles)",
        fighter.card.tag(),
        idx + 1,
        arena.fighters.len()
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            fit_width(&header, chunks[1].width as usize),
            Style::default()
                .fg(fighter_color(theme, fighter.card.id))
                .add_modifier(Modifier::BOLD),
        ))),
        chunks[1],
    );

    let body = if arena.show_review_lane {
        &fighter.review_transcript
    } else {
        &fighter.transcript
    };
    let width = chunks[2].width.saturating_sub(1) as usize;
    let lines: Vec<Line> = if body.is_empty() {
        vec![Line::from(Span::styled(
            match (arena.show_review_lane, &fighter.state) {
                (true, _) => "…no review words yet (their quill is dry)",
                (false, ragnarok::FighterState::Summoned) => "…awaiting the horn",
                _ => "…silence on the battlefield",
            },
            Style::default().fg(theme.muted),
        ))]
    } else {
        wrap_tail_lines(body, width.max(8), chunks[2].height as usize)
            .into_iter()
            .map(|l| Line::from(Span::styled(l, Style::default().fg(theme.text))))
            .collect()
    };
    f.render_widget(Paragraph::new(lines), chunks[2]);
    f.render_widget(
        Paragraph::new(ragnarok_footer_line(arena, theme, chunks[3].width)),
        chunks[3],
    );
}

/// Wrap `text` to `width` display columns and keep only the last `max_lines`
/// lines. Works on a bounded tail slice so huge buffers stay cheap.
fn wrap_tail_lines(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
    // Only the tail can be visible; keep a generous margin for wrapping.
    let budget = width.saturating_mul(max_lines).saturating_mul(4).max(256);
    let mut start = text.len().saturating_sub(budget);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    let tail = &text[start..];

    let mut lines: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    let push_line = |lines: &mut std::collections::VecDeque<String>, line: String| {
        lines.push_back(line);
        while lines.len() > max_lines {
            lines.pop_front();
        }
    };
    for ch in tail.chars() {
        if ch == '\n' {
            push_line(&mut lines, std::mem::take(&mut current));
            current_width = 0;
            continue;
        }
        if ch == '\r' {
            continue;
        }
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + w > width && !current.is_empty() {
            push_line(&mut lines, std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += w;
    }
    if !current.is_empty() {
        push_line(&mut lines, current);
    }
    lines.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::app::StatusKind;
    use crate::claude_usage::{ClaudeUsageReport, ClaudeUsageStatus};
    use crate::event::{
        ElicitationPrompt, InternalMessage, SessionConfigTarget, SubagentEvent,
        TerminalOutputSnapshot,
    };
    use crate::workflow::{
        WorkflowActorId, WorkflowActorRole, WorkflowCoverage, WorkflowEvent, WorkflowId,
        WorkflowKind, WorkflowOutcome, WorkflowPhase, WorkflowStage, WorkflowTransition,
    };

    use super::*;

    fn subagent_session_update(update: SessionUpdate) -> UiEvent {
        UiEvent::Subagent(SubagentEvent::SessionUpdate {
            subagent_id: 1,
            update,
        })
    }

    fn subagent_finished(outcome: SubagentOutcome) -> UiEvent {
        UiEvent::Subagent(SubagentEvent::Finished {
            subagent_id: 1,
            outcome,
        })
    }

    fn start_subagent(state: &mut AppState, subagent_id: u64, label: &str, objective: &str) {
        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id,
            resumed: false,
            label: label.to_string(),
            model: Some("gpt-y".to_string()),
            agent: "codex-acp".to_string(),
            objective: objective.to_string(),
        }));
    }

    fn apply_workflow(
        state: &mut AppState,
        workflow_id: WorkflowId,
        transition: WorkflowTransition,
    ) {
        state.apply_event(UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            transition,
        )));
    }

    fn start_workflow(
        state: &mut AppState,
        workflow_id: WorkflowId,
        kind: WorkflowKind,
        phase: WorkflowPhase,
    ) {
        apply_workflow(
            state,
            workflow_id,
            WorkflowTransition::Started {
                kind,
                stage: WorkflowStage::new(0, phase),
            },
        );
    }

    #[test]
    fn stream_reveal_hides_partial_source_and_paces_complete_lines() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Streaming);
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("first line\npartial"),
        )));
        let entry_index = state.agent_open_message_index().expect("open message");
        let mut reveal = StreamRevealController::default();

        let _ = reveal.observe(&mut state);
        let Entry::AgentMessage(source) = &state.transcript[entry_index] else {
            panic!("expected agent message");
        };
        assert_eq!(source, "first line\npartial", "canonical source is intact");
        assert_eq!(state.stream_visible_text(entry_index, source), "");

        assert!(reveal.commit_one(&mut state));
        let Entry::AgentMessage(source) = &state.transcript[entry_index] else {
            panic!("expected agent message");
        };
        assert_eq!(
            state.stream_visible_text(entry_index, source),
            "first line\n"
        );

        let canonical = source.to_string();
        reveal.flush_entries(&mut state, vec![entry_index]);
        assert_eq!(
            state.stream_visible_text(entry_index, &canonical),
            canonical
        );
    }

    #[test]
    fn stream_reveal_catches_up_when_eight_lines_are_queued() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Streaming);
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("1\n2\n3\n4\n5\n6\n7\n8\n"),
        )));
        let entry_index = state.agent_open_message_index().expect("open message");
        let mut reveal = StreamRevealController::default();

        let _ = reveal.observe(&mut state);
        assert!(reveal.commit_one(&mut state));
        let Entry::AgentMessage(source) = &state.transcript[entry_index] else {
            panic!("expected agent message");
        };
        assert_eq!(state.stream_visible_text(entry_index, source), source);
        assert!(reveal.queued.is_empty());
    }

    #[test]
    fn stream_reveal_reveals_unterminated_source_after_short_delay() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Streaming);
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("single paragraph without a newline"),
        )));
        let entry_index = state.agent_open_message_index().expect("open message");
        let observed_at = Instant::now();
        let mut reveal = StreamRevealController::default();

        let _ = reveal.observe_at(&mut state, observed_at);
        let Entry::AgentMessage(source) = &state.transcript[entry_index] else {
            panic!("expected agent message");
        };
        assert_eq!(state.stream_visible_text(entry_index, source), "");
        assert!(reveal.has_pending());
        assert!(!reveal.commit_one_at(
            &mut state,
            observed_at + STREAM_PARTIAL_COMMIT_AGE - Duration::from_nanos(1),
        ));
        assert!(reveal.commit_one_at(&mut state, observed_at + STREAM_PARTIAL_COMMIT_AGE,));

        let Entry::AgentMessage(source) = &state.transcript[entry_index] else {
            panic!("expected agent message");
        };
        assert_eq!(state.stream_visible_text(entry_index, source), source);
        assert!(!reveal.has_pending());
    }

    #[test]
    fn stream_reveal_release_prevents_hidden_completion_from_truncating_source() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Streaming);
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("visible line\nhidden partial"),
        )));
        let entry_index = state.agent_open_message_index().expect("open message");
        let mut reveal = StreamRevealController::default();
        let _ = reveal.observe(&mut state);
        assert!(reveal.commit_one(&mut state));

        reveal.release(&mut state);
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk(" completed while side is open"),
        )));
        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });

        let Entry::AgentMessage(source) = &state.transcript[entry_index] else {
            panic!("expected agent message");
        };
        assert_eq!(state.stream_visible_text(entry_index, source), source);
        let resumed = StreamRevealController::resume(&mut state);
        assert!(
            resumed.lanes.is_empty(),
            "completed source needs no reveal lane"
        );
        assert!(!resumed.has_pending());
    }

    #[test]
    fn stream_reveal_resume_keeps_existing_active_source_visible() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Streaming);
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("source received while hidden"),
        )));
        let entry_index = state.agent_open_message_index().expect("open message");
        let mut reveal = StreamRevealController::resume(&mut state);

        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk(" then streamed"),
        )));
        let observed_at = Instant::now();
        let _ = reveal.observe_at(&mut state, observed_at);
        let Entry::AgentMessage(source) = &state.transcript[entry_index] else {
            panic!("expected agent message");
        };
        assert_eq!(
            state.stream_visible_text(entry_index, source),
            "source received while hidden"
        );
        assert!(reveal.commit_one_at(&mut state, observed_at + STREAM_PARTIAL_COMMIT_AGE,));
        let Entry::AgentMessage(source) = &state.transcript[entry_index] else {
            panic!("expected agent message");
        };
        assert_eq!(state.stream_visible_text(entry_index, source), source);
    }

    #[test]
    fn stream_reveal_reports_when_completed_prose_becomes_fully_visible() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Streaming);
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
            text_chunk("visible line\nhidden commentary"),
        )));
        let entry_index = state.transcript.len() - 1;
        let mut reveal = StreamRevealController::default();
        let _ = reveal.observe(&mut state);
        assert!(reveal.commit_one(&mut state));

        let Entry::AgentThought(thought) = &mut state.transcript[entry_index] else {
            panic!("expected agent thought");
        };
        thought.completed = true;

        assert!(
            reveal.observe(&mut state),
            "removing the live prefix must request a redraw"
        );
        let Entry::AgentThought(thought) = &state.transcript[entry_index] else {
            panic!("expected agent thought");
        };
        assert_eq!(
            state.stream_visible_text(entry_index, &thought.text),
            thought.text
        );
        assert!(!reveal.has_pending());
    }

    use agent_client_protocol::schema::v1::{
        AvailableCommand, ContentBlock, ContentChunk, ElicitationFormMode, ElicitationId,
        ElicitationMode, ElicitationSchema, ElicitationSessionScope, ElicitationUrlMode,
        EnumOption, PermissionOption, PermissionOptionKind, PlanEntry, PlanEntryPriority,
        PlanEntryStatus, SessionConfigOption, SessionConfigOptionCategory,
        SessionConfigSelectOption, SessionConfigValueId, SessionUpdate, StopReason,
        StringPropertySchema, TerminalExitStatus, TextContent, ToolCall, ToolCallStatus,
        ToolCallUpdate, ToolCallUpdateFields, ToolKind, UsageUpdate,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use ratatui::backend::{Backend, TestBackend};
    use ratatui::layout::Position;

    fn key(code: KeyCode) -> CtEvent {
        key_with_modifiers(code, KeyModifiers::NONE)
    }

    fn key_with_modifiers(code: KeyCode, modifiers: KeyModifiers) -> CtEvent {
        CtEvent::Key(KeyEvent::new(code, modifiers))
    }

    fn mouse(kind: MouseEventKind) -> CtEvent {
        CtEvent::Mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn long_tool_output(hidden_marker: &str, visible_marker: &str) -> String {
        [
            hidden_marker.to_string(),
            format!("{hidden_marker}_SECOND"),
            "tool output line 3".to_string(),
            "tool output line 4".to_string(),
            "tool output line 5".to_string(),
            "tool output line 6".to_string(),
            "tool output line 7".to_string(),
            visible_marker.to_string(),
        ]
        .join("\n")
    }

    fn insert_tool_output(state: &mut AppState, id: &str, status: ToolCallStatus, output: String) {
        state.tool_calls.insert(
            id.to_string(),
            crate::app::ToolCallView {
                title: id.to_string(),
                kind: ToolKind::Execute,
                status,
                body: vec![ToolCallOutput::Text(output)],
            },
        );
        state.transcript.push(Entry::ToolCall(id.to_string()));
    }

    #[test]
    fn alt_t_in_fullscreen_expands_only_latest_visible_failed_tool() {
        let mut state = AppState::new();
        insert_tool_output(
            &mut state,
            "older",
            ToolCallStatus::Completed,
            long_tool_output("OLDER_HIDDEN_HEAD", "OLDER_VISIBLE_TAIL"),
        );
        insert_tool_output(
            &mut state,
            "latest-failed",
            ToolCallStatus::Failed,
            long_tool_output("LATEST_HIDDEN_HEAD", "LATEST_VISIBLE_TAIL"),
        );
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let before = render_transcript_lines(&state, 100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(!before.iter().any(|line| line.contains("OLDER_HIDDEN_HEAD")));
        assert!(
            !before
                .iter()
                .any(|line| line.contains("LATEST_HIDDEN_HEAD"))
        );
        assert!(
            before
                .iter()
                .any(|line| line.contains("OLDER_VISIBLE_TAIL"))
        );
        assert!(
            before
                .iter()
                .any(|line| line.contains("LATEST_VISIBLE_TAIL"))
        );

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('t'), KeyModifiers::ALT),
        );

        assert!(!state.expand_transcript_details);
        assert_eq!(state.tool_detail_expanded("older"), None);
        assert_eq!(state.tool_detail_expanded("latest-failed"), Some(true));
        let after = render_transcript_lines(&state, 100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(!after.iter().any(|line| line.contains("OLDER_HIDDEN_HEAD")));
        assert!(after.iter().any(|line| line.contains("LATEST_HIDDEN_HEAD")));
        assert!(after.iter().any(|line| line.contains("OLDER_VISIBLE_TAIL")));
        assert!(
            after
                .iter()
                .any(|line| line.contains("LATEST_VISIBLE_TAIL"))
        );
    }

    #[test]
    fn alt_t_in_inline_reader_collapses_only_latest_failed_tool() {
        let mut state = AppState::new();
        insert_tool_output(
            &mut state,
            "older",
            ToolCallStatus::Completed,
            long_tool_output("INLINE_OLDER_HIDDEN_HEAD", "INLINE_OLDER_VISIBLE_TAIL"),
        );
        insert_tool_output(
            &mut state,
            "latest-failed",
            ToolCallStatus::Failed,
            long_tool_output("INLINE_LATEST_HIDDEN_HEAD", "INLINE_LATEST_VISIBLE_TAIL"),
        );
        state.open_transcript_viewer();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let before = render_full_transcript_lines(&state, 100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(
            before
                .iter()
                .any(|line| line.contains("INLINE_OLDER_HIDDEN_HEAD"))
        );
        assert!(
            before
                .iter()
                .any(|line| line.contains("INLINE_LATEST_HIDDEN_HEAD"))
        );

        handle_inline_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('t'), KeyModifiers::ALT),
        );

        assert!(state.transcript_viewer);
        assert!(!state.expand_transcript_details);
        assert_eq!(state.tool_detail_expanded("older"), None);
        assert_eq!(state.tool_detail_expanded("latest-failed"), Some(false));
        let after = render_full_transcript_lines(&state, 100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(
            after
                .iter()
                .any(|line| line.contains("INLINE_OLDER_HIDDEN_HEAD"))
        );
        assert!(
            !after
                .iter()
                .any(|line| line.contains("INLINE_LATEST_HIDDEN_HEAD"))
        );
        assert!(
            after
                .iter()
                .any(|line| line.contains("INLINE_OLDER_VISIBLE_TAIL"))
        );
        assert!(
            after
                .iter()
                .any(|line| line.contains("INLINE_LATEST_VISIBLE_TAIL"))
        );
        assert!(
            after
                .iter()
                .filter(|line| line.contains("hidden"))
                .all(|line| !line.contains("Ctrl-T")),
            "collapsed inline rows must not promise a Ctrl-T expansion: {after:?}"
        );
    }

    #[test]
    fn alt_t_skips_successful_tool_omitted_by_compact_completed_turn() {
        let mut state = AppState::new();
        state.record_user_prompt("prompt".to_string());
        for (id, status, text) in [
            ("successful", ToolCallStatus::Completed, "SUCCESS_UNIQUE"),
            ("failed", ToolCallStatus::Failed, "FAILED_UNIQUE"),
        ] {
            state.tool_calls.insert(
                id.to_string(),
                crate::app::ToolCallView {
                    title: id.to_string(),
                    kind: ToolKind::Execute,
                    status,
                    body: vec![ToolCallOutput::Text(text.to_string())],
                },
            );
            state.transcript.push(Entry::ToolCall(id.to_string()));
        }
        state
            .transcript
            .push(Entry::AgentMessage("done".to_string()));
        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('t'), KeyModifiers::ALT),
        );

        assert_eq!(state.tool_detail_expanded("failed"), Some(true));
        assert_eq!(state.tool_detail_expanded("successful"), None);
        let rendered: Vec<String> = render_transcript_lines(&state, 100)
            .into_iter()
            .map(|line| line.to_string())
            .collect();
        assert!(rendered.iter().any(|line| line.contains("FAILED_UNIQUE")));
        assert!(!rendered.iter().any(|line| line.contains("SUCCESS_UNIQUE")));
    }

    fn test_clipboard_image() -> ClipboardImage {
        ClipboardImage {
            data_base64: "aW1hZ2U=".to_string(),
            mime_type: "image/png".to_string(),
            width: 640,
            height: 480,
            byte_len: 12_345,
        }
    }

    fn test_image_attachment_with_id(id: usize) -> PastedImageAttachment {
        let image = test_clipboard_image();
        PastedImageAttachment {
            id,
            position: 0,
            data_base64: image.data_base64,
            mime_type: image.mime_type,
            width: image.width,
            height: image.height,
            byte_len: image.byte_len,
        }
    }

    #[test]
    fn config_value_row_keeps_score_visible_with_long_description() {
        let choice = ConfigValueChoice {
            value: SessionConfigValueId::new("gpt-5.5"),
            name: "GPT-5.5".to_string(),
            description: Some(
                "A very long model description that would normally consume the whole row"
                    .to_string(),
            ),
            group: None,
        };

        let row = config_value_row_text(&choice, Some("1463 pass_at_1"), 32);

        assert!(row.ends_with("  1463 pass_at_1"), "{row}");
        assert!(row.width() <= 32, "{row}");
    }

    fn write_test_png(path: &Path) {
        let image = image::RgbaImage::from_pixel(2, 3, image::Rgba([255, 0, 0, 255]));
        image.save(path).expect("write test image");
    }

    fn text_chunk(s: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(s)))
    }

    fn handle_crossterm(
        state: &mut AppState,
        cmd_tx: &mpsc::UnboundedSender<UiCommand>,
        ev: CtEvent,
    ) -> TerminalRequest {
        super::handle_crossterm(state, cmd_tx, ev, UiMode::FullscreenTui)
    }

    fn handle_inline_crossterm(
        state: &mut AppState,
        cmd_tx: &mpsc::UnboundedSender<UiCommand>,
        ev: CtEvent,
    ) -> TerminalRequest {
        super::handle_crossterm(state, cmd_tx, ev, UiMode::InlineChat)
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn plan_rows_use_readable_status_and_priority_labels_in_every_transcript_view() {
        let mut state = AppState::new();
        state.transcript.push(Entry::Plan(vec![
            PlanEntry::new(
                "write tests",
                PlanEntryPriority::Medium,
                PlanEntryStatus::Pending,
            ),
            PlanEntry::new(
                "render output",
                PlanEntryPriority::High,
                PlanEntryStatus::InProgress,
            ),
            PlanEntry::new(
                "document behavior",
                PlanEntryPriority::Low,
                PlanEntryStatus::Completed,
            ),
        ]));

        let expected = vec![
            "plan",
            "  [pending] write tests",
            "  [running] [high] render output",
            "  [done] [low] document behavior",
            "",
        ];
        let normal = render_transcript_lines(&state, 80);
        let full = render_full_transcript_lines(&state, 80);
        assert_eq!(normal.iter().map(line_text).collect::<Vec<_>>(), expected);
        assert_eq!(full.iter().map(line_text).collect::<Vec<_>>(), expected);
        assert!(!normal.iter().any(|line| line_text(line).contains("[!]")));
        assert!(!normal.iter().any(|line| line_text(line).contains("[*]")));

        assert_eq!(normal[1].spans[1].style.fg, Some(state.theme.muted));
        assert_eq!(normal[2].spans[1].style.fg, Some(state.theme.primary));
        assert_eq!(normal[3].spans[1].style.fg, Some(state.theme.success));
        assert_eq!(normal[2].spans[2].style.fg, Some(state.theme.warning));
        assert!(
            normal[2].spans[2]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(normal[3].spans[2].style.fg, Some(state.theme.muted));
        assert!(
            normal[3]
                .spans
                .last()
                .expect("completed content")
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
    }

    #[test]
    fn subagent_plans_keep_visible_actor_provenance() {
        let mut state = AppState::new();
        state
            .transcript
            .push(Entry::SubagentPlan(vec![PlanEntry::new(
                "inspect the renderer",
                PlanEntryPriority::Medium,
                PlanEntryStatus::InProgress,
            )]));

        let normal = render_transcript_lines(&state, 80);
        let full = render_full_transcript_lines(&state, 80);
        let expected = vec!["◆ subagent plan", "  [running] inspect the renderer", ""];
        assert_eq!(normal.iter().map(line_text).collect::<Vec<_>>(), expected);
        assert_eq!(full.iter().map(line_text).collect::<Vec<_>>(), expected);
        assert_eq!(normal[0].spans[0].style.fg, Some(state.theme.secondary));
        assert!(
            normal[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(normal[0].spans[1].style.fg, Some(state.theme.tool));
    }

    #[test]
    fn plan_rows_wrap_without_truncating_content_at_narrow_widths() {
        let mut state = AppState::new();
        state.transcript.push(Entry::Plan(vec![PlanEntry::new(
            "narrow content stays readable",
            PlanEntryPriority::High,
            PlanEntryStatus::InProgress,
        )]));

        let width = 18;
        let lines = render_full_transcript_lines(&state, width);
        let paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
        let line_count = paragraph.line_count(width);
        assert!(line_count > lines.len());

        let area = Rect::new(0, 0, width, line_count as u16);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        paragraph.render(area, &mut buffer);
        let rendered = buffer_lines(&buffer).join("\n");
        for word in ["running", "high", "narrow", "content", "stays", "readable"] {
            assert!(rendered.contains(word), "missing {word:?} in {rendered:?}");
        }
    }

    fn buffer_lines(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
        (0..buffer.area().height)
            .map(|y| {
                (0..buffer.area().width)
                    .map(|x| buffer.cell((x, y)).expect("cell").symbol())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn session_boundary_renders_as_separator_and_is_stable() {
        let mut state = AppState::new();
        state.push_session_boundary("new claude-acp session started");

        assert_eq!(stable_transcript_entry_count(&state), 1);
        let rendered: Vec<String> = render_transcript_lines(&state, 50)
            .iter()
            .map(line_text)
            .collect();

        assert_eq!(rendered.len(), 3);
        assert!(rendered[0].is_empty());
        assert!(rendered[1].contains("new claude-acp session started"));
        assert!(rendered[1].contains("─"));
        assert!(rendered[2].is_empty());
    }

    fn contains_prompt_activity_frame(text: &str) -> bool {
        // The rendered ornament is one animation frame of the active style.
        // Tests build an `AppState::new()` (default style), but check every
        // style's frames so the helper stays correct if a test sets another.
        SpinnerStyle::ALL
            .iter()
            .flat_map(|style| style.frames())
            .any(|frame| text.contains(frame.as_str()))
    }

    #[test]
    fn token_usage_label_prefers_in_out_and_ctx_format() {
        let mut state = AppState::new();
        state.token_usage.input_tokens = Some(1233);
        state.token_usage.output_tokens = Some(1282);
        state.token_usage.context_used = Some(944);
        state.token_usage.context_size = Some(128_000);
        state.token_usage.total_tokens = Some(2515);

        assert_eq!(token_usage_label(&state), "in: 1233 · out: 1282 · ctx: 944");
        assert_eq!(
            header_token_usage_label(&state, 80),
            "in: 1233 · out: 1282 · ctx: 944"
        );
    }

    #[test]
    fn header_token_usage_label_reports_seat_totals_once_a_seat_has_billed() {
        let mut state = AppState::new();
        state.token_usage.input_tokens = Some(10);
        state.token_usage.output_tokens = Some(20);
        state.token_usage.context_used = Some(30);

        // No seat usage yet: the header falls back to the plain token label.
        assert_eq!(
            header_token_usage_label(&state, 80),
            "in: 10 · out: 20 · ctx: 30"
        );

        state.agent_usage.observe(crate::agent_usage::Record {
            seat: crate::agent_usage::Seat::Primary,
            model: Some("gpt-primary".to_string()),
            usage: Some(agent_client_protocol::schema::v1::Usage::new(
                1200, 900, 300,
            )),
            update: None,
            session_id: Some("p1".to_string()),
        });
        state.agent_usage.observe(crate::agent_usage::Record {
            seat: crate::agent_usage::Seat::Subagent,
            model: Some("gpt-worker".to_string()),
            usage: Some(agent_client_protocol::schema::v1::Usage::new(
                3400, 3000, 400,
            )),
            update: None,
            session_id: Some("s1".to_string()),
        });
        assert_eq!(
            header_token_usage_label(&state, 80),
            "primary 1200 · subagents 3400"
        );

        // Review overhead only appears once it is nonzero.
        state.agent_usage.observe(crate::agent_usage::Record {
            seat: crate::agent_usage::Seat::Review,
            model: Some("gpt-worker".to_string()),
            usage: Some(agent_client_protocol::schema::v1::Usage::new(500, 400, 100)),
            update: None,
            session_id: Some("r1".to_string()),
        });
        assert_eq!(
            header_token_usage_label(&state, 80),
            "primary 1200 · subagents 3400 · review 500"
        );
    }

    #[test]
    fn slash_agents_panel_breaks_usage_down_per_model() {
        let mut state = AppState::new();
        state.active_models = crate::config::ModelsConfig {
            primary: "claude-opus".to_string(),
            review: "gpt-worker".to_string(),
            subagent: "gpt-worker".to_string(),
            primary_source: None,
            review_source: None,
            subagent_source: None,
        };
        for (seat, model, total) in [
            (crate::agent_usage::Seat::Primary, "claude-opus", 100),
            (crate::agent_usage::Seat::Subagent, "gpt-worker", 40),
            (crate::agent_usage::Seat::Review, "gpt-worker", 60),
        ] {
            state.agent_usage.observe(crate::agent_usage::Record {
                seat,
                model: Some(model.to_string()),
                usage: Some(agent_client_protocol::schema::v1::Usage::new(
                    total, total, 0,
                )),
                update: None,
                session_id: Some(format!("{model}-{total}")),
            });
        }

        let report = active_models_and_usage_report(&state);
        assert!(report.contains("primary    100 tokens"), "{report}");
        assert!(report.contains("subagents  40 tokens"), "{report}");
        assert!(report.contains("review     60 tokens"), "{report}");
        // Per-model lines aggregate across seats: the worker model billed both
        // the subagent run and the review lane.
        assert!(report.contains("\nBy model"), "{report}");
        assert!(report.contains("\nclaude-opus  100 tokens"), "{report}");
        assert!(report.contains("\ngpt-worker  100 tokens"), "{report}");
    }

    #[test]
    fn token_usage_label_omits_rate_limit_from_header() {
        // The Claude rate-limit status belongs in the transcript, not the
        // header, so the header label must not surface it even when present.
        let mut state = AppState::new();
        state.token_usage.input_tokens = Some(1233);
        state.token_usage.output_tokens = Some(1282);
        state.token_usage.context_used = Some(944);
        state.token_usage.rate_limit = Some("Current session: 85% used".to_string());

        let label = token_usage_label(&state);
        assert_eq!(label, "in: 1233 · out: 1282 · ctx: 944");
        assert!(!label.contains("rl:"), "label: {label}");
        assert!(!label.contains("85%"), "label: {label}");
    }

    #[test]
    fn token_usage_label_never_falls_back_to_tok_or_think() {
        let mut state = AppState::new();
        state.token_usage.total_tokens = Some(411_400);
        state.token_usage.input_tokens = Some(261_300);
        state.token_usage.output_tokens = Some(3905);
        state.token_usage.thought_tokens = Some(327);
        state.token_usage.context_used = Some(944);

        let label = token_usage_label(&state);
        assert_eq!(label, "in: 261.3k · out: 3905 · ctx: 944");
        assert!(!label.contains("tok:"), "label: {label}");
        assert!(!label.contains("think:"), "label: {label}");
    }

    #[test]
    fn token_usage_label_uses_dash_format_when_usage_is_missing() {
        let state = AppState::new();
        assert_eq!(token_usage_label(&state), "in: - · out: - · ctx: -");
    }

    #[test]
    fn header_switches_to_fresh_subagent_usage_and_restores_primary_afterward() {
        let mut state = AppState::new();
        state.token_usage.context_used = Some(42_000);
        state.token_usage.context_size = Some(128_000);
        assert_eq!(token_usage_label(&state), "ctx: 42.0k");

        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id: 1,
            resumed: false,
            model: None,
            agent: "codex-acp".to_string(),
            objective: String::new(),
            label: "subagent".to_string(),
        }));
        assert_eq!(
            token_usage_label(&state),
            "ctx: 42.0k",
            "a subagent that has reported no usage yet must not blank the header"
        );

        state.apply_event(subagent_session_update(SessionUpdate::UsageUpdate(
            UsageUpdate::new(900, 128_000),
        )));
        assert_eq!(token_usage_label(&state), "ctx: 900");

        state.apply_event(subagent_finished(SubagentOutcome::Completed));
        assert_eq!(token_usage_label(&state), "ctx: 42.0k");
    }

    #[test]
    fn header_surfaces_full_directory_path() {
        let mut state = AppState::new();
        state.agent_label = "anvil".to_string();
        state.project_label = "~/code/project-a".to_string();
        let backend = TestBackend::new(140, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_header(frame, frame.area(), &state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains(&mjolnir_version_label()),
            "rendered:\n{rendered}"
        );
        assert!(rendered.contains("anvil"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("~/code/project-a"),
            "rendered:\n{rendered}"
        );
        assert!(!rendered.contains("agent "), "rendered:\n{rendered}");
        assert!(!rendered.contains("project "), "rendered:\n{rendered}");
        assert!(!rendered.contains("cwd"), "rendered:\n{rendered}");
        assert!(!rendered.contains("worktree"), "rendered:\n{rendered}");
    }

    #[test]
    fn header_shows_project_path_worktree_and_session_title_without_session_id() {
        let mut state = AppState::new();
        state.agent_label = "uvx".to_string();
        state.project_label = "~/code/mjolnir/.mjolnir/worktrees/bold-willow".to_string();
        state.worktree_label = Some("bold-willow".to_string());
        state.session_id = Some("48c95a78-cdbf-416a-807a-b0c5124fcc72".to_string());
        state.session_title = Some("Review payment flow".to_string());
        let backend = TestBackend::new(200, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_header(frame, frame.area(), &state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("mjolnir v"), "rendered:\n{rendered}");
        assert!(rendered.contains("uvx"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("~/code/mjolnir/.mjolnir/worktrees/bold-willow"),
            "rendered:\n{rendered}"
        );
        assert_eq!(
            rendered.matches("bold-willow").count(),
            1,
            "worktree name should only appear as part of the project path:\n{rendered}"
        );
        assert!(!rendered.contains("worktree "), "rendered:\n{rendered}");
        assert!(!rendered.contains("agent "), "rendered:\n{rendered}");
        assert!(!rendered.contains("project "), "rendered:\n{rendered}");
        assert!(!rendered.contains("/Users/"), "rendered:\n{rendered}");
        assert!(!rendered.contains("session"), "rendered:\n{rendered}");
        assert!(!rendered.contains("48c95a78"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("Review payment flow"),
            "rendered:\n{rendered}"
        );
    }

    #[test]
    fn header_shows_additional_workspace_root_count() {
        let mut state = AppState::new();
        state.agent_label = "codex-acp".to_string();
        state.project_label = "~/code/mjolnir".to_string();
        state.additional_roots = 2;
        let backend = TestBackend::new(120, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_header(frame, frame.area(), &state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("+2 roots"), "rendered:\n{rendered}");
    }

    #[test]
    fn header_uses_remaining_width_for_long_session_title() {
        let mut state = AppState::new();
        state.agent_label = "codex-acp".to_string();
        state.project_label = "~/code/mjolnir".to_string();
        let title = "Investigate inline prompt title spacing and streaming status rendering";
        state.session_title = Some(title.to_string());
        let backend = TestBackend::new(180, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_header(frame, frame.area(), &state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains(title),
            "wide headers should render the full session title:\n{rendered}"
        );
    }

    fn permission_pending_with_options(
        title: &str,
        option_names: &[&str],
        selected: usize,
    ) -> PendingPermission {
        let (responder, _rx) = tokio::sync::oneshot::channel();
        let mut fields = ToolCallUpdateFields::default();
        fields.title = Some(title.to_string());
        let options = option_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                PermissionOption::new(
                    format!("option-{i}"),
                    (*name).to_string(),
                    PermissionOptionKind::AllowOnce,
                )
            })
            .collect();

        PendingPermission {
            prompt: crate::event::PermissionPrompt {
                tool_call: ToolCallUpdate::new("call-1", fields),
                options,
                responder,
            },
            selected,
            scroll_offset: None,
            subagent_id: None,
        }
    }

    fn single_select_elicitation_prompt() -> ElicitationPrompt {
        let (responder, _rx) = tokio::sync::oneshot::channel();
        let schema = ElicitationSchema::new().title("Choose a model").property(
            "model",
            StringPropertySchema::new().one_of(vec![
                EnumOption::new("fast", "Fast model"),
                EnumOption::new("smart", "Smart model"),
            ]),
            true,
        );
        ElicitationPrompt {
            message: "Pick a model".to_string(),
            mode: ElicitationMode::from(ElicitationFormMode::new(
                ElicitationSessionScope::new("setup".to_string()),
                schema,
            )),
            responder,
        }
    }

    fn claude_form_elicitation_prompt() -> ElicitationPrompt {
        let (responder, _rx) = tokio::sync::oneshot::channel();
        let schema = ElicitationSchema::new()
            .property(
                "question_0",
                StringPropertySchema::new()
                    .title("Choose a model")
                    .description("Select the model to use for this task.")
                    .one_of(vec![
                        EnumOption::new("fast", "Fast model"),
                        EnumOption::new("smart", "Smart model"),
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
        ElicitationPrompt {
            message: "Configure the task".to_string(),
            mode: ElicitationMode::from(ElicitationFormMode::new(
                ElicitationSessionScope::new("setup".to_string()),
                schema,
            )),
            responder,
        }
    }

    fn url_elicitation_prompt() -> ElicitationPrompt {
        url_elicitation_prompt_with_url("https://example.com/oauth/authorize?client_id=abc")
    }

    fn url_elicitation_prompt_with_url(url: &str) -> ElicitationPrompt {
        let (responder, _rx) = tokio::sync::oneshot::channel();
        ElicitationPrompt {
            message: "Open this URL to sign in".to_string(),
            mode: ElicitationMode::from(ElicitationUrlMode::new(
                ElicitationSessionScope::new("setup".to_string()),
                ElicitationId::new("login-1"),
                url,
            )),
            responder,
        }
    }

    #[test]
    fn elicitation_modal_renders_single_select_options() {
        let pending = PendingElicitation::new(single_select_elicitation_prompt(), None);
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| {
                draw_elicitation_modal(
                    frame,
                    frame.area(),
                    &pending,
                    1,
                    TerminalThemeKind::default().palette(),
                )
            })
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        for expected in ["setup request", "Pick a model", "Fast model", "Smart model"] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}; rendered:\n{rendered}"
            );
        }
    }

    #[test]
    fn multi_field_elicitation_renders_each_field_in_sequence() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::ElicitationRequest(claude_form_elicitation_prompt()));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let render = |state: &mut AppState| {
            let backend = TestBackend::new(80, 14);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| draw_inline_chat(frame, state))
                .expect("draw");
            buffer_lines(terminal.backend().buffer()).join("\n")
        };

        let first = render(&mut state);
        assert!(first.contains("Field 1 of 2"), "rendered:\n{first}");
        assert!(first.contains("Fast model"), "rendered:\n{first}");
        assert!(first.contains("Smart model"), "rendered:\n{first}");

        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        let second = render(&mut state);
        assert!(second.contains("Field 2 of 2"), "rendered:\n{second}");
        assert!(second.contains("Other"), "rendered:\n{second}");
        assert!(second.contains('█'), "rendered:\n{second}");
    }

    #[test]
    fn elicitation_url_modal_renders_qr_without_panicking() {
        // Acceptance: URL + QR renders without panicking for an OAuth URL.
        let pending = PendingElicitation::new(url_elicitation_prompt(), None);
        let backend = TestBackend::new(100, 60);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| {
                draw_elicitation_modal(
                    frame,
                    frame.area(),
                    &pending,
                    1,
                    TerminalThemeKind::default().palette(),
                )
            })
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("setup request"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("example.com/oauth"),
            "URL must be shown; rendered:\n{rendered}"
        );
        assert!(
            rendered.contains('█') || rendered.contains('▀') || rendered.contains('▄'),
            "QR should render as half-block glyphs; rendered:\n{rendered}"
        );
    }

    #[test]
    fn inline_chat_replaces_content_with_elicitation_view() {
        let mut state = AppState::new();
        state.agent_label = "anvil".to_string();
        state.record_user_prompt("hello".to_string());
        state.apply_event(UiEvent::ElicitationRequest(
            single_select_elicitation_prompt(),
        ));
        let backend = TestBackend::new(100, INLINE_CHAT_HEIGHT);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("setup request"), "rendered:\n{rendered}");
        assert!(
            !rendered.contains("agent anvil"),
            "elicitation view must replace the chat header; rendered:\n{rendered}"
        );
    }

    #[test]
    fn inline_elicitation_view_handles_keyboard_selection() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::ElicitationRequest(
            single_select_elicitation_prompt(),
        ));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Down));

        let pending = state.pending_elicitation().expect("pending elicitation");
        assert_eq!(pending.selected, 1);
    }

    #[test]
    fn url_elicitation_copies_url_on_c() {
        let mut state = AppState::new();
        let url = "https://example.com/oauth/authorize?client_id=abc";
        state.apply_event(UiEvent::ElicitationRequest(
            url_elicitation_prompt_with_url(url),
        ));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let request = handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('c')));

        assert_eq!(request, TerminalRequest::CopyText(url.to_string()));
        assert!(
            state.has_pending_elicitation(),
            "copy must not dismiss login prompt"
        );

        let request = handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('C')));
        assert_eq!(request, TerminalRequest::CopyText(url.to_string()));
    }

    #[test]
    fn inline_url_elicitation_uses_full_height_and_preserves_qr_width() {
        let mut state = AppState::new();
        let url = "https://auth.openai.com/oauth/authorize?client_id=codex_cli&scope=openid%20profile%20email&code_challenge=abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789&state=abcdefghijklmnopqrstuvwxyz0123456789";
        state.apply_event(UiEvent::ElicitationRequest(
            url_elicitation_prompt_with_url(url),
        ));
        let terminal_size = Size {
            width: 140,
            height: 50,
        };

        let desired = desired_inline_height(&state, terminal_size);
        assert_eq!(desired, terminal_size.height - 1);
        assert!(desired > INLINE_EXPANDED_MAX_HEIGHT);

        let qr_width = crate::qr::render_qr(url)
            .expect("qr")
            .lines()
            .map(|line| line.width())
            .max()
            .expect("qr lines");
        assert!(qr_width <= usize::from(terminal_size.width - 2));

        let backend = TestBackend::new(terminal_size.width, desired);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("press c to copy"),
            "rendered:\n{rendered}"
        );
        assert!(
            rendered.contains('█') || rendered.contains('▀') || rendered.contains('▄'),
            "QR should render in the inline URL view; rendered:\n{rendered}"
        );
    }

    fn text_elicitation_prompt() -> ElicitationPrompt {
        let (responder, _rx) = tokio::sync::oneshot::channel();
        let schema = ElicitationSchema::new().property(
            "key",
            StringPropertySchema::new()
                .title("OpenRouter API key")
                .description("Paste your key."),
            true,
        );
        ElicitationPrompt {
            message: "Enter your OpenRouter API key".to_string(),
            mode: ElicitationMode::from(ElicitationFormMode::new(
                ElicitationSessionScope::new("setup".to_string()),
                schema,
            )),
            responder,
        }
    }

    #[test]
    fn elicitation_modal_renders_text_input() {
        // The typed value and a cursor block render inside the modal.
        let mut pending = PendingElicitation::new(text_elicitation_prompt(), None);
        pending.input = "sk-or-abc".to_string();
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| {
                draw_elicitation_modal(
                    frame,
                    frame.area(),
                    &pending,
                    1,
                    TerminalThemeKind::default().palette(),
                )
            })
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("setup request"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("OpenRouter API key"),
            "field title must show; rendered:\n{rendered}"
        );
        assert!(
            rendered.contains("sk-or-abc"),
            "typed value must show; rendered:\n{rendered}"
        );
        assert!(
            rendered.contains('█'),
            "cursor block must show; rendered:\n{rendered}"
        );
    }

    #[test]
    fn inline_elicitation_text_field_captures_typing() {
        // A free-text field captures typed characters -- including `j`/`k`,
        // which navigate option lists for single-select views -- and Backspace
        // deletes the last one.
        let mut state = AppState::new();
        state.apply_event(UiEvent::ElicitationRequest(text_elicitation_prompt()));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        for c in ['s', 'k', '-', 'j'] {
            handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Char(c)));
        }
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Backspace));

        let pending = state.pending_elicitation().expect("pending elicitation");
        assert_eq!(pending.input, "sk-");
    }

    #[test]
    fn inline_elicitation_text_field_accepts_paste() {
        // Pasting a key (with a trailing newline) lands in the field with
        // control characters stripped, so it can't pre-submit.
        let mut state = AppState::new();
        state.apply_event(UiEvent::ElicitationRequest(text_elicitation_prompt()));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_inline_crossterm(
            &mut state,
            &cmd_tx,
            CtEvent::Paste("sk-or-xyz\n".to_string()),
        );

        let pending = state.pending_elicitation().expect("pending elicitation");
        assert_eq!(pending.input, "sk-or-xyz");
    }

    #[test]
    fn permission_modal_wins_keyboard_over_elicitation() {
        // Both modals pending: the safety-critical permission modal must own
        // the keyboard. Down should move the permission cursor, not elicitation.
        let mut state = AppState::new();
        state.apply_event(UiEvent::ElicitationRequest(
            single_select_elicitation_prompt(),
        ));
        let permission = permission_pending_with_options("run cmd", &["Allow", "Reject"], 0);
        state.apply_event(UiEvent::PermissionRequest(permission.prompt));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Down));

        assert_eq!(
            state.pending_permission().expect("permission").selected,
            1,
            "permission cursor should move"
        );
        assert_eq!(
            state.pending_elicitation().expect("elicitation").selected,
            0,
            "elicitation cursor must stay put while permission owns keys"
        );
    }

    #[test]
    fn inline_help_close_requests_one_repair() {
        let mut state = AppState::new();
        state.help_overlay = true;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let request = handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));

        assert_eq!(request, TerminalRequest::ForceInlineRepair);
        assert!(!state.help_overlay);
    }

    #[test]
    fn inline_autocomplete_accept_requests_one_repair() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.available_commands = vec![
            AvailableCommand::new("help", "show help"),
            AvailableCommand::new("hello", "say hello"),
        ];
        state.input = "/he".to_string();
        state.input_cursor = state.input.chars().count();
        state.update_autocomplete();
        assert!(state.autocomplete.visible);
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let request = handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Tab));

        assert_eq!(request, TerminalRequest::ForceInlineRepair);
        assert!(!state.autocomplete.visible);
    }

    #[test]
    fn inline_autocomplete_dismiss_requests_one_repair() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.available_commands = vec![
            AvailableCommand::new("help", "show help"),
            AvailableCommand::new("hello", "say hello"),
        ];
        state.input = "/he".to_string();
        state.input_cursor = state.input.chars().count();
        state.update_autocomplete();
        assert!(state.autocomplete.visible);
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let request = handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));

        assert_eq!(request, TerminalRequest::ForceInlineRepair);
        assert!(!state.autocomplete.visible);
    }

    #[test]
    fn inline_permission_close_requests_one_repair() {
        let pending = permission_pending_with_options("run shell command", &["Allow once"], 0);
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let request = handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));

        assert_eq!(request, TerminalRequest::ForceInlineRepair);
        assert!(!state.has_pending_permission());
    }

    #[test]
    fn inline_config_picker_close_requests_one_repair() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "claude-sonnet",
            vec![
                SessionConfigSelectOption::new("claude-sonnet", "Claude Sonnet"),
                SessionConfigSelectOption::new("gpt-4.1", "GPT-4.1"),
            ],
        )];
        state.session_config_targets = vec![SessionConfigTarget::LegacyModel];
        assert!(state.open_config_value_picker(0));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let request = handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));

        assert_eq!(request, TerminalRequest::ForceInlineRepair);
        assert!(state.config_picker.is_none());
    }

    #[test]
    fn runtime_closed_allows_new_session_command() {
        let mut state = AppState::new();
        state.runtime_closed = true;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        for ch in ['/', 'n', 'e', 'w'] {
            handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Char(ch)));
        }
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));

        assert_eq!(state.exit_reason, Some(UiExitReason::NewSession));
        assert!(state.input.is_empty());
    }

    #[test]
    fn force_inline_repair_requests_one_soft_repair() {
        assert!(terminal_request_forces_inline_repair(
            &TerminalRequest::ForceInlineRepair
        ));
        assert!(!terminal_request_forces_inline_repair(
            &TerminalRequest::None
        ));
    }

    #[test]
    fn inline_streaming_uses_slow_spinner_timer_without_repair_heartbeat() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Streaming);

        assert!(needs_live_redraw(&state));
        assert!(timer_driven_live_redraw(UiMode::InlineChat, &state));
        assert!(timer_driven_live_redraw(UiMode::FullscreenTui, &state));
        assert_eq!(
            PendingRedraw {
                animation: true,
                ..PendingRedraw::default()
            }
            .budget(UiMode::InlineChat),
            SPINNER_FRAME_BUDGET
        );
        assert_eq!(
            PendingRedraw {
                interactive: true,
                animation: true,
                ..PendingRedraw::default()
            }
            .budget(UiMode::InlineChat),
            FRAME_BUDGET
        );
        assert_eq!(SPINNER_FRAME_BUDGET, Duration::from_millis(250));
    }

    #[test]
    fn current_branch_pr_probe_surfaces_open_pr_and_retires_it_on_branch_change() {
        let mut state = AppState::new();
        state.session_cwd = PathBuf::from("/repo");

        assert!(apply_current_branch_pr_probe(
            &mut state,
            CurrentBranchPrProbe {
                cwd: PathBuf::from("/repo"),
                branch: Some("feature".to_string()),
                gh_succeeded: true,
                pull_request: Some(CurrentBranchPullRequest {
                    number: 487,
                    url: "https://github.com/BrokkAi/mjolnir/pull/487".to_string(),
                }),
            },
        ));
        assert_eq!(
            state.current_branch_pull_request,
            Some(CurrentBranchPullRequest {
                number: 487,
                url: "https://github.com/BrokkAi/mjolnir/pull/487".to_string(),
            })
        );

        assert!(apply_current_branch_pr_probe(
            &mut state,
            CurrentBranchPrProbe {
                cwd: PathBuf::from("/repo"),
                branch: Some("other".to_string()),
                gh_succeeded: false,
                pull_request: None,
            },
        ));
        assert!(state.current_branch_pull_request.is_none());
    }

    #[test]
    fn current_branch_pr_row_is_compact_at_narrow_widths() {
        let mut state = AppState::new();
        state.current_branch_pull_request = Some(CurrentBranchPullRequest {
            number: 487,
            url: "https://github.com/BrokkAi/mjolnir/pull/487".to_string(),
        });
        let backend = TestBackend::new(24, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_current_branch_pr_row(frame, frame.area(), &state))
            .expect("draw PR row");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("PR #487"));
        assert_eq!(current_branch_pr_row_count(&state), 1);
    }

    #[test]
    fn workflow_progress_row_is_stable_across_rapid_out_of_order_actor_churn() {
        let mut state = AppState::new();
        let workflow_id = WorkflowId::delegation(7);
        assert_eq!(
            workflow_progress_row_count(&state),
            0,
            "no workflow means no layout change"
        );
        start_workflow(
            &mut state,
            workflow_id,
            WorkflowKind::Delegation,
            WorkflowPhase::Delegating,
        );
        assert_eq!(workflow_progress_row_count(&state), 1);

        for id in 1..=6 {
            apply_workflow(
                &mut state,
                workflow_id,
                WorkflowTransition::ActorStarted {
                    actor_id: WorkflowActorId::Subagent(id),
                    role: WorkflowActorRole::Implementation,
                },
            );
            assert_eq!(
                workflow_progress_row_count(&state),
                1,
                "actor launch must not move the input"
            );
        }
        for id in [4, 1] {
            apply_workflow(
                &mut state,
                workflow_id,
                WorkflowTransition::ActorFinished {
                    actor_id: WorkflowActorId::Subagent(id),
                    outcome: SubagentOutcome::Completed,
                },
            );
            assert_eq!(
                workflow_progress_row_count(&state),
                1,
                "actor finish must not move the input"
            );
        }

        let backend = TestBackend::new(100, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_workflow_progress_rows(frame, frame.area(), &state))
            .expect("draw workflow progress");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("Subagents [/subagents]"), "{rendered}");
        assert!(rendered.contains("delegating"), "{rendered}");
        assert!(rendered.contains("4 running"), "{rendered}");
        assert!(rendered.contains("2 done"), "{rendered}");

        for id in [2, 3, 5, 6] {
            apply_workflow(
                &mut state,
                workflow_id,
                WorkflowTransition::ActorFinished {
                    actor_id: WorkflowActorId::Subagent(id),
                    outcome: SubagentOutcome::Completed,
                },
            );
        }
        apply_workflow(
            &mut state,
            workflow_id,
            WorkflowTransition::Terminal {
                outcome: WorkflowOutcome::Completed,
                coverage: WorkflowCoverage::Complete,
            },
        );
        assert_eq!(
            workflow_progress_row_count(&state),
            1,
            "the terminal outcome remains visible without a TTL"
        );
        terminal
            .draw(|frame| draw_workflow_progress_rows(frame, frame.area(), &state))
            .expect("draw terminal workflow outcome");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("✔ Subagents [/subagents]"), "{rendered}");
        assert!(rendered.contains("complete"), "{rendered}");

        state.record_user_prompt("next task".to_string());
        assert_eq!(
            workflow_progress_row_count(&state),
            0,
            "the next user turn retires the prior outcome"
        );
    }

    #[test]
    fn review_progress_shows_wait_failure_cancel_coverage_and_narrow_details_hint() {
        let mut state = AppState::new();
        let workflow_id = WorkflowId::review(9);
        start_workflow(
            &mut state,
            workflow_id,
            WorkflowKind::Review,
            WorkflowPhase::SpecialistReview,
        );
        for (id, lane) in [(11, "Týr"), (12, "Heimdall"), (13, "Freya")] {
            apply_workflow(
                &mut state,
                workflow_id,
                WorkflowTransition::ActorStarted {
                    actor_id: WorkflowActorId::Subagent(id),
                    role: WorkflowActorRole::SpecialistReviewer {
                        lane: lane.to_string(),
                    },
                },
            );
        }
        apply_workflow(
            &mut state,
            workflow_id,
            WorkflowTransition::ActorFinished {
                actor_id: WorkflowActorId::Subagent(12),
                outcome: SubagentOutcome::Completed,
            },
        );
        apply_workflow(
            &mut state,
            workflow_id,
            WorkflowTransition::ActorFinished {
                actor_id: WorkflowActorId::Subagent(11),
                outcome: SubagentOutcome::Failed("adapter exited".to_string()),
            },
        );
        apply_workflow(
            &mut state,
            workflow_id,
            WorkflowTransition::ActorWaiting {
                actor_id: WorkflowActorId::Subagent(13),
                dependency: "automatic specialist reviewer reports".to_string(),
                remaining: Some(1),
                requires_user_action: false,
            },
        );
        apply_workflow(
            &mut state,
            workflow_id,
            WorkflowTransition::Waiting {
                dependency: "automatic specialist reviewer reports".to_string(),
                remaining: Some(1),
                requires_user_action: false,
            },
        );
        apply_workflow(
            &mut state,
            workflow_id,
            WorkflowTransition::CoverageChanged {
                coverage: WorkflowCoverage::Degraded,
            },
        );

        let backend = TestBackend::new(180, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_workflow_progress_rows(frame, frame.area(), &state))
            .expect("draw review progress");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("Review [/subagents]"), "{rendered}");
        assert!(rendered.contains("waiting for 1"), "{rendered}");
        assert!(rendered.contains("reviewers 2/3"), "{rendered}");
        assert!(rendered.contains("1 waiting"), "{rendered}");
        assert!(rendered.contains("1 failed"), "{rendered}");
        assert!(rendered.contains("degraded coverage"), "{rendered}");

        let backend = TestBackend::new(22, 1);
        let mut narrow = Terminal::new(backend).expect("terminal");
        narrow
            .draw(|frame| draw_workflow_progress_rows(frame, frame.area(), &state))
            .expect("draw narrow progress");
        let rendered = buffer_lines(narrow.backend().buffer()).join("\n");
        assert!(rendered.contains("Review"), "{rendered}");
        assert!(!rendered.contains("/subagents"), "{rendered}");

        apply_workflow(
            &mut state,
            workflow_id,
            WorkflowTransition::ActorResumed {
                actor_id: WorkflowActorId::Subagent(13),
            },
        );
        assert_eq!(workflow_progress_row_count(&state), 1);
        apply_workflow(
            &mut state,
            workflow_id,
            WorkflowTransition::ActorFinished {
                actor_id: WorkflowActorId::Subagent(13),
                outcome: SubagentOutcome::Cancelled,
            },
        );
        apply_workflow(
            &mut state,
            workflow_id,
            WorkflowTransition::Waiting {
                dependency: "approval".to_string(),
                remaining: None,
                requires_user_action: true,
            },
        );
        let line = workflow_progress_line(
            state.visible_workflows().next().expect("workflow"),
            "⠋",
            Duration::ZERO,
            120,
            state.theme,
            true,
        );
        assert!(line_text(&line).contains("waiting for user action"));

        apply_workflow(
            &mut state,
            workflow_id,
            WorkflowTransition::Terminal {
                outcome: WorkflowOutcome::Degraded,
                coverage: WorkflowCoverage::Degraded,
            },
        );
        let line = workflow_progress_line(
            state.visible_workflows().next().expect("terminal workflow"),
            "⠋",
            Duration::ZERO,
            120,
            state.theme,
            true,
        );
        let line = line_text(&line);
        assert!(line.contains("⚠ Review [/subagents]"), "{line}");
        assert!(line.contains("complete"), "{line}");
        assert!(line.contains("1 failed"), "{line}");
        assert!(line.contains("1 cancelled"), "{line}");
        assert!(!state.has_active_workflows());
    }

    #[test]
    fn nested_agent_viewer_switches_between_separate_implementation_and_review_histories() {
        use crate::workflow::{
            WorkflowActorId, WorkflowActorRole, WorkflowEvent, WorkflowId, WorkflowKind,
            WorkflowPhase, WorkflowStage, WorkflowTransition,
        };

        let mut state = AppState::new();
        start_subagent(&mut state, 1, "implementer", "build the feature");
        state.apply_event(UiEvent::Subagent(SubagentEvent::SessionUpdate {
            subagent_id: 1,
            update: SessionUpdate::AgentMessageChunk(text_chunk("IMPLEMENTATION_ONLY")),
        }));
        state.apply_event(UiEvent::Subagent(SubagentEvent::Finished {
            subagent_id: 1,
            outcome: SubagentOutcome::Completed,
        }));

        let workflow_id = WorkflowId::review(3);
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
                actor_id: WorkflowActorId::Subagent(2),
                role: WorkflowActorRole::SpecialistReviewer {
                    lane: "Týr".to_string(),
                },
            },
        )));
        start_subagent(&mut state, 2, "review · Týr", "inspect correctness");
        state.apply_event(UiEvent::Subagent(SubagentEvent::SessionUpdate {
            subagent_id: 2,
            update: SessionUpdate::AgentThoughtChunk(text_chunk("REVIEW_ONLY")),
        }));

        assert!(state.open_nested_agent_viewer());
        assert_eq!(
            state.nested_agent_selected,
            Some(2),
            "opening must select the newest in-progress actor"
        );
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_nested_agent_viewer(frame, frame.area(), &mut state, false))
            .expect("draw reviewer");
        let reviewer = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(reviewer.contains("reviewer Týr"), "{reviewer}");
        assert!(reviewer.contains("REVIEW_ONLY"), "{reviewer}");
        assert!(!reviewer.contains("IMPLEMENTATION_ONLY"), "{reviewer}");

        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Right));
        assert_eq!(state.nested_agent_selected, Some(1));
        terminal
            .draw(|frame| draw_nested_agent_viewer(frame, frame.area(), &mut state, false))
            .expect("draw implementation");
        let implementation = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            implementation.contains("IMPLEMENTATION_ONLY"),
            "{implementation}"
        );
        assert!(!implementation.contains("REVIEW_ONLY"), "{implementation}");
    }

    #[test]
    fn nested_agent_viewer_shows_ten_newest_actors_and_keeps_attribution() {
        let mut state = AppState::new();
        for id in 1..=15 {
            start_subagent(&mut state, id, &format!("actor-{id}"), "work");
        }
        let terminal_size = Size {
            width: 100,
            height: 40,
        };
        assert!(state.open_nested_agent_viewer());
        assert_eq!(
            desired_inline_height(&state, terminal_size),
            terminal_size.height - 1,
            "the nested viewer needs enough height for every retained actor"
        );
        assert_eq!(state.nested_agent_selected, Some(15));
        state.close_nested_agent_viewer();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let ctrl_l = handle_inline_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('l'), KeyModifiers::CONTROL),
        );
        assert!(!state.nested_agent_viewer, "Ctrl-L remains unclaimed");
        assert_eq!(ctrl_l, TerminalRequest::None);
        let request = handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::F(11)));
        assert!(state.nested_agent_viewer);
        assert_ne!(request, TerminalRequest::None);
        state.nested_agent_scroll_offset = 100;
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::PageUp));
        assert_eq!(
            state.nested_agent_scroll_offset,
            100 - TRANSCRIPT_SCROLL_PAGE_STEP
        );
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::PageDown));
        assert_eq!(state.nested_agent_scroll_offset, 100);

        let backend = TestBackend::new(24, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_nested_agent_viewer(frame, frame.area(), &mut state, true))
            .expect("narrow inline viewer");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        for id in 6..=15 {
            assert!(
                rendered.contains(&format!("#{id}")),
                "actor #{id} must remain visible:\n{rendered}"
            );
        }
        for id in 1..=5 {
            assert!(
                !rendered.contains(&format!("#{id} ")),
                "older actor #{id} must not displace a recent actor:\n{rendered}"
            );
        }
        assert!(rendered.contains("nested agents — 10 ne"), "{rendered}");
        #[cfg(target_os = "macos")]
        assert!(rendered.contains("Fn+Up/Down"), "{rendered}");
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::F(11)));
        assert!(!state.nested_agent_viewer, "F11 closes the viewer");

        let mut pending =
            permission_pending_with_options("run a long command", &["Allow", "Reject"], 0);
        pending.subagent_id = Some(8);
        let permission = permission_view_lines(&pending, 1, 24, state.theme)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            permission.contains("subagent #8 permission"),
            "{permission}"
        );
    }

    #[test]
    fn simultaneous_workflows_are_distinct_and_pathological_overflow_is_bounded() {
        let mut state = AppState::new();
        start_workflow(
            &mut state,
            WorkflowId::delegation(1),
            WorkflowKind::Delegation,
            WorkflowPhase::Delegating,
        );
        start_workflow(
            &mut state,
            WorkflowId::delegation(2),
            WorkflowKind::Delegation,
            WorkflowPhase::Delegating,
        );
        start_workflow(
            &mut state,
            WorkflowId::review(2),
            WorkflowKind::Review,
            WorkflowPhase::Supervision,
        );

        let rows = workflow_progress_row_count(&state);
        assert_eq!(rows, WORKFLOW_PROGRESS_VISIBLE_ROWS as u16 + 1);
        let backend = TestBackend::new(80, rows);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_workflow_progress_rows(frame, frame.area(), &state))
            .expect("draw workflow progress");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");

        assert!(rendered.contains("Subagents"), "{rendered}");
        assert!(
            rendered.contains("Review"),
            "the current turn's review must not fold behind prior work: {rendered}"
        );
        assert!(rendered.contains("… 1 more"), "{rendered}");
    }

    #[test]
    fn named_only_workflow_does_not_advertise_unavailable_nested_details() {
        let mut state = AppState::new();
        let workflow_id = WorkflowId::review(4);
        start_workflow(
            &mut state,
            workflow_id,
            WorkflowKind::Review,
            WorkflowPhase::Fallback,
        );
        apply_workflow(
            &mut state,
            workflow_id,
            WorkflowTransition::ActorStarted {
                actor_id: WorkflowActorId::Named("primary-single-review".to_string()),
                role: WorkflowActorRole::FallbackReviewer,
            },
        );

        let mut terminal = Terminal::new(TestBackend::new(60, 1)).expect("terminal");
        terminal
            .draw(|frame| draw_workflow_progress_rows(frame, frame.area(), &state))
            .expect("draw named workflow");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("Review"), "{rendered}");
        assert!(!rendered.contains("F11"), "{rendered}");
        assert!(!state.open_nested_agent_viewer());
    }

    #[test]
    fn active_workflow_forces_timer_redraws_until_terminal() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        assert!(!should_show_spinner(&state));
        assert!(!needs_live_redraw(&state));
        assert!(!timer_driven_live_redraw(UiMode::InlineChat, &state));
        assert!(!timer_driven_live_redraw(UiMode::FullscreenTui, &state));

        let workflow_id = WorkflowId::delegation(1);
        start_workflow(
            &mut state,
            workflow_id,
            WorkflowKind::Delegation,
            WorkflowPhase::Delegating,
        );
        assert!(
            needs_live_redraw(&state),
            "elapsed time must keep ticking with an idle primary"
        );
        assert!(timer_driven_live_redraw(UiMode::InlineChat, &state));
        assert!(timer_driven_live_redraw(UiMode::FullscreenTui, &state));

        apply_workflow(
            &mut state,
            workflow_id,
            WorkflowTransition::Terminal {
                outcome: WorkflowOutcome::Completed,
                coverage: WorkflowCoverage::Complete,
            },
        );
        assert_eq!(
            workflow_progress_row_count(&state),
            1,
            "the frozen terminal outcome remains visible"
        );
        assert!(!needs_live_redraw(&state));
        assert!(!timer_driven_live_redraw(UiMode::InlineChat, &state));
        assert!(!timer_driven_live_redraw(UiMode::FullscreenTui, &state));
    }

    #[test]
    fn subagent_events_pick_their_redraw_cause() {
        assert_eq!(
            ui_event_redraw_cause(&UiEvent::Subagent(SubagentEvent::Activity {
                subagent_id: 1,
                activity: "reading".to_string(),
            })),
            RedrawCause::Stream,
            "activity only rewrites one row's text"
        );
        for event in [
            UiEvent::Subagent(SubagentEvent::Started {
                subagent_id: 1,
                resumed: false,
                label: "fix-tests".to_string(),
                model: None,
                agent: "codex-acp".to_string(),
                objective: "fix".to_string(),
            }),
            UiEvent::Subagent(SubagentEvent::Finished {
                subagent_id: 1,
                outcome: SubagentOutcome::Completed,
            }),
        ] {
            assert_eq!(
                ui_event_redraw_cause(&event),
                RedrawCause::Interactive,
                "start and finish update transcript and viewer structure"
            );
        }
    }

    #[test]
    fn permission_open_uses_hard_inline_repair() {
        assert_eq!(InlineRepairMode::Hard, InlineRepairMode::Hard);
    }

    #[test]
    fn permission_open_repairs_before_inline_flush() {
        let pending = permission_pending_with_options("run shell command", &["Allow once"], 0);
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));

        assert!(should_attempt_inline_repair_before_flush(
            true,
            UiMode::InlineChat,
            &state,
        ));
        assert!(!should_attempt_inline_repair_before_flush(
            false,
            UiMode::InlineChat,
            &state,
        ));
        assert!(!should_attempt_inline_repair_before_flush(
            true,
            UiMode::FullscreenTui,
            &state,
        ));

        let mut ready = AppState::new();
        ready.set_connection_state(ConnectionState::Ready);
        assert!(!should_attempt_inline_repair_before_flush(
            true,
            UiMode::InlineChat,
            &ready,
        ));
    }

    #[test]
    fn inline_streaming_keeps_timer_redraws_but_disables_repair_heartbeat() {
        let mut state = AppState::new();

        state.set_connection_state(ConnectionState::Launching);
        assert!(timer_driven_live_redraw(UiMode::InlineChat, &state));
        assert!(timer_driven_live_redraw(UiMode::FullscreenTui, &state));

        state.set_connection_state(ConnectionState::Streaming);
        assert!(needs_live_redraw(&state));
        assert!(should_show_spinner(&state));
        assert!(timer_driven_live_redraw(UiMode::InlineChat, &state));
        assert!(timer_driven_live_redraw(UiMode::FullscreenTui, &state));

        state.set_connection_state(ConnectionState::Cancelling);
        assert!(timer_driven_live_redraw(UiMode::InlineChat, &state));
        assert!(timer_driven_live_redraw(UiMode::FullscreenTui, &state));
    }

    #[test]
    fn pending_redraw_budget_prioritizes_interactive_input_over_streaming_and_animation() {
        assert_eq!(
            PendingRedraw {
                stream: true,
                ..PendingRedraw::default()
            }
            .budget(UiMode::FullscreenTui),
            STREAMING_FRAME_BUDGET
        );
        assert_eq!(
            PendingRedraw {
                stream: true,
                ..PendingRedraw::default()
            }
            .budget(UiMode::InlineChat),
            INLINE_STREAMING_FRAME_BUDGET
        );
        assert_eq!(
            PendingRedraw {
                animation: true,
                ..PendingRedraw::default()
            }
            .budget(UiMode::InlineChat),
            SPINNER_FRAME_BUDGET
        );
        assert_eq!(
            PendingRedraw {
                interactive: true,
                stream: true,
                animation: true,
            }
            .budget(UiMode::InlineChat),
            FRAME_BUDGET
        );
    }

    #[test]
    fn mjconfig_animation_uses_diff_redraws_but_keys_stay_interactive() {
        let mut state = AppState::new();
        state.open_mjconfig_menu();

        assert!(state.mjconfig_menu.is_some());
        assert!(timer_driven_live_redraw(UiMode::InlineChat, &state));
        assert!(!should_attempt_inline_repair(false, UiMode::InlineChat));
        assert_ne!(MJCONFIG_FRAME_BUDGET, FRAME_BUDGET);
        assert_eq!(MJCONFIG_FRAME_BUDGET, SPINNER_FRAME_BUDGET);
        assert_eq!(
            PendingRedraw {
                animation: true,
                ..PendingRedraw::default()
            }
            .budget(UiMode::FullscreenTui),
            MJCONFIG_FRAME_BUDGET
        );
        assert_eq!(
            PendingRedraw {
                interactive: true,
                animation: true,
                ..PendingRedraw::default()
            }
            .budget(UiMode::FullscreenTui),
            FRAME_BUDGET
        );
    }

    #[test]
    fn streaming_uses_timer_redraws_at_the_expected_budget() {
        let mut state = AppState::new();

        state.set_connection_state(ConnectionState::Launching);
        assert!(needs_live_redraw(&state));

        state.set_connection_state(ConnectionState::Initializing);
        assert!(needs_live_redraw(&state));

        state.set_connection_state(ConnectionState::Streaming);
        assert!(state.is_streaming());
        assert!(should_show_spinner(&state));
        assert_eq!(
            PendingRedraw {
                stream: true,
                ..PendingRedraw::default()
            }
            .budget(UiMode::InlineChat),
            INLINE_STREAMING_FRAME_BUDGET
        );
        assert_eq!(
            PendingRedraw {
                stream: true,
                ..PendingRedraw::default()
            }
            .budget(UiMode::FullscreenTui),
            STREAMING_FRAME_BUDGET
        );
        assert!(needs_live_redraw(&state));
        assert!(timer_driven_live_redraw(UiMode::InlineChat, &state));

        state.set_connection_state(ConnectionState::Cancelling);
        assert!(state.is_streaming());
        assert!(should_show_spinner(&state));
        assert!(needs_live_redraw(&state));
        assert!(timer_driven_live_redraw(UiMode::InlineChat, &state));
    }

    #[test]
    fn inline_focus_gain_forces_repair_even_without_live_redraw_state() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);

        assert!(should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::FocusGained
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::FullscreenTui,
            &state,
            &CtEvent::FocusGained
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::FocusLost
        ));
    }

    #[test]
    fn inline_paste_forces_repair_when_input_is_focused() {
        let state = AppState::new();

        assert!(should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Paste("clipboard".to_string())
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::FullscreenTui,
            &state,
            &CtEvent::Paste("clipboard".to_string())
        ));
    }

    #[test]
    fn inline_paste_does_not_force_repair_when_modal_owns_input() {
        let mut state = AppState::new();
        state.help_overlay = true;

        assert!(!should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Paste("clipboard".to_string())
        ));
    }

    #[test]
    fn permission_resize_forces_inline_repair() {
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));

        assert!(should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Resize(120, 40)
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::FullscreenTui,
            &state,
            &CtEvent::Resize(120, 40)
        ));
    }

    #[test]
    fn permission_key_no_longer_forces_inline_repair() {
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));

        assert!(!should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        ));
        assert!(should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Resize(120, 40)
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::FullscreenTui,
            &state,
            &CtEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        ));
    }

    #[test]
    fn inline_repairs_require_an_explicit_inline_request() {
        assert!(should_attempt_inline_repair(true, UiMode::InlineChat));
        assert!(!should_attempt_inline_repair(false, UiMode::InlineChat));
        assert!(!should_attempt_inline_repair(true, UiMode::FullscreenTui));
    }

    #[test]
    fn inline_permission_request_skips_terminal_notification() {
        let pending = permission_pending_with_options("run shell command", &["Allow once"], 0);
        let state = AppState::new();

        assert!(
            notification_message_for_event(
                UiMode::FullscreenTui,
                &state,
                &UiEvent::PermissionRequest(pending.prompt),
            )
            .is_some()
        );

        let other_pending =
            permission_pending_with_options("run shell command", &["Allow once"], 0);
        assert!(
            notification_message_for_event(
                UiMode::InlineChat,
                &state,
                &UiEvent::PermissionRequest(other_pending.prompt),
            )
            .is_none()
        );
    }

    #[test]
    fn inline_permission_ui_event_forces_repair() {
        let pending = permission_pending_with_options("run shell command", &["Allow once"], 0);

        assert!(should_force_inline_repair_for_ui_event(
            UiMode::InlineChat,
            &UiEvent::PermissionRequest(pending.prompt)
        ));
        let other_pending =
            permission_pending_with_options("run shell command", &["Allow once"], 0);
        assert!(!should_force_inline_repair_for_ui_event(
            UiMode::FullscreenTui,
            &UiEvent::PermissionRequest(other_pending.prompt)
        ));
    }

    #[test]
    fn streaming_inline_help_overlay_renders_after_f10() {
        let mut state = AppState::new();
        state.record_user_prompt("hello".to_string());
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::F(10)));

        assert!(state.is_streaming());
        assert!(state.help_overlay);

        let desired = desired_inline_height(
            &state,
            Size {
                width: 100,
                height: 40,
            },
        );
        assert!(
            desired > INLINE_CHAT_HEIGHT,
            "help overlay must request enough inline rows to render while streaming"
        );

        let backend = TestBackend::new(100, desired);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("help"), "rendered:\n{rendered}");
        assert!(rendered.contains("Agent seats"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("automatic review"),
            "rendered:\n{rendered}"
        );
    }

    #[test]
    fn streaming_inline_config_shortcut_does_not_disrupt_help_overlay() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![
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
        state.record_user_prompt("hello".to_string());
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::F(10)));
        let overlay_height = desired_inline_height(
            &state,
            Size {
                width: 100,
                height: 40,
            },
        );
        assert!(overlay_height > INLINE_CHAT_HEIGHT);

        handle_inline_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('1'), KeyModifiers::CONTROL),
        );

        assert!(state.is_streaming());
        assert!(
            state.help_overlay,
            "help overlay should remain open while streaming"
        );
        assert!(
            state.config_picker.is_none(),
            "streaming must not open config picker"
        );
        assert!(
            state.status_line.is_none(),
            "help overlay should keep unrelated shortcuts from mutating status"
        );
        assert_eq!(
            desired_inline_height(
                &state,
                Size {
                    width: 100,
                    height: 40,
                },
            ),
            overlay_height,
            "streaming config shortcut must not collapse the inline overlay"
        );

        let backend = TestBackend::new(100, overlay_height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("help"), "rendered:\n{rendered}");
        assert!(rendered.contains("Agent seats"), "rendered:\n{rendered}");
        assert!(!rendered.contains("Model values"), "rendered:\n{rendered}");
    }

    #[test]
    fn terminal_setup_features_keep_inline_out_of_alt_screen_and_mouse_capture() {
        let inline = terminal_setup_features(UiMode::InlineChat);
        assert!(inline.contains(&TerminalFeature::RawMode));
        assert!(inline.contains(&TerminalFeature::BracketedPaste));
        assert!(!inline.contains(&TerminalFeature::AlternateScreen));
        assert!(!inline.contains(&TerminalFeature::MouseCapture));

        let fullscreen = terminal_setup_features(UiMode::FullscreenTui);
        assert!(fullscreen.contains(&TerminalFeature::AlternateScreen));
        assert!(fullscreen.contains(&TerminalFeature::MouseCapture));
    }

    #[derive(Debug)]
    struct WrappedError {
        source: std::io::Error,
    }

    impl std::fmt::Display for WrappedError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "wrapped terminal error")
        }
    }

    impl std::error::Error for WrappedError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.source)
        }
    }

    #[test]
    fn cursor_position_timeout_detection_matches_crossterm_error_shape() {
        let err = std::io::Error::other(CURSOR_POSITION_TIMEOUT_MESSAGE);
        assert!(is_cursor_position_timeout_io(&err));

        let wrapped = WrappedError {
            source: std::io::Error::other(CURSOR_POSITION_TIMEOUT_MESSAGE),
        };
        assert!(is_cursor_position_timeout_error(&wrapped));

        let contextualized = std::io::Error::other(format!(
            "ratatui inline terminal: {CURSOR_POSITION_TIMEOUT_MESSAGE}"
        ));
        assert!(is_cursor_position_timeout_io(&contextualized));

        let phrasing_variant =
            std::io::Error::other("failed to read cursor position within a normal duration");
        assert!(is_cursor_position_timeout_io(&phrasing_variant));

        let other = std::io::Error::other("terminal unavailable");
        assert!(!is_cursor_position_timeout_io(&other));
    }

    #[test]
    fn inline_reader_restore_replays_the_stable_transcript_tail() {
        let mut state = AppState::new();
        for id in 0..12 {
            state.push_system_message(format!("stable transcript marker {id}"));
        }
        let mut backend = TestBackend::new(80, 24);
        backend
            .set_cursor_position(Position::new(0, 20))
            .expect("cursor position");
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(4),
            },
        )
        .expect("terminal");

        assert!(
            insert_stable_transcript_tail_before_inline_viewport(&mut terminal, &state, 80, 4,)
                .expect("restore transcript tail")
        );
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("stable transcript marker 11"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("stable transcript marker 0"),
            "restore must replay the tail rather than the oldest rows: {rendered}"
        );
    }

    #[test]
    fn inline_chat_draw_survives_nonzero_viewport_origin_after_insert_before() {
        let mut backend = TestBackend::new(80, 24);
        backend
            .set_cursor_position(Position::new(0, 20))
            .expect("cursor position");
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(INLINE_CHAT_HEIGHT),
            },
        )
        .expect("terminal");
        let mut state = AppState::new();
        let mut transcript_scroll = TranscriptScrollState::default();

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &mut state,
                    &mut transcript_scroll,
                    UiMode::InlineChat,
                )
            })
            .expect("initial draw");
        terminal
            .insert_before(2, |buf| {
                Paragraph::new(vec![Line::from("one"), Line::from("two")]).render(buf.area, buf);
            })
            .expect("insert before");
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &mut state,
                    &mut transcript_scroll,
                    UiMode::InlineChat,
                )
            })
            .expect("draw after insert");
    }

    #[test]
    fn inline_exit_clears_viewport_and_resets_cursor_to_prompt_origin() {
        let mut backend = TestBackend::new(60, 16);
        backend
            .set_cursor_position(Position::new(0, 12))
            .expect("cursor position");
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(INLINE_CHAT_HEIGHT),
            },
        )
        .expect("terminal");

        terminal
            .insert_before(2, |buf| {
                Paragraph::new(vec![
                    Line::from("transcript one"),
                    Line::from("transcript two"),
                ])
                .render(buf.area, buf);
            })
            .expect("insert before");

        let mut state = AppState::new();
        state.input = "hello world".to_string();
        state.input_cursor = state.input.chars().count();
        let mut transcript_scroll = TranscriptScrollState::default();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &mut state,
                    &mut transcript_scroll,
                    UiMode::InlineChat,
                )
            })
            .expect("draw");

        let origin = terminal.get_frame().area().as_position();
        clear_inline_viewport_for_exit(&mut terminal).expect("clear inline viewport");

        terminal.backend_mut().assert_cursor_position(origin);

        let rendered = buffer_lines(terminal.backend().buffer());
        assert!(
            rendered
                .iter()
                .take(origin.y as usize)
                .any(|line| line.contains("transcript one")),
            "transcript above inline viewport should remain visible:\n{}",
            rendered.join("\n")
        );
        assert!(
            rendered
                .iter()
                .skip(origin.y as usize)
                .all(|line| line.trim().is_empty()),
            "inline viewport should be blank after exit cleanup:\n{}",
            rendered.join("\n")
        );
    }

    #[test]
    fn runtime_closed_quits_on_ctrl_c() {
        let mut state = AppState::new();
        state.runtime_closed = true;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            CtEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );

        assert_eq!(state.exit_reason, Some(UiExitReason::Quit));
    }

    #[test]
    fn runtime_closed_quits_on_ctrl_c_even_with_pending_permission() {
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.runtime_closed = true;
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            CtEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );

        assert_eq!(state.exit_reason, Some(UiExitReason::Quit));
        assert!(
            state.has_pending_permission(),
            "quit should not require dismissing the prompt"
        );
    }

    #[test]
    fn runtime_closed_submit_notice_deduplicates_in_transcript() {
        let mut state = AppState::new();
        state.runtime_closed = true;
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();

        state.input = "first".to_string();
        submit_prompt(&mut state, &cmd_tx);
        state.input = "second".to_string();
        submit_prompt(&mut state, &cmd_tx);

        assert!(cmd_rx.try_recv().is_err());
        assert_eq!(state.transcript.len(), 1);
        match &state.transcript[0] {
            Entry::System(text) => assert_eq!(
                text,
                "acp runtime closed; type /clear for the same agent, /new for the picker, or Ctrl-C to quit"
            ),
            other => panic!("unexpected entry: {other:?}"),
        }
    }

    #[test]
    fn help_overlay_opens_and_closes_from_keyboard() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::F(10)));
        assert!(state.help_overlay);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));
        assert!(!state.help_overlay);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::F(10)));
        assert!(state.help_overlay);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::F(10)));
        assert!(!state.help_overlay);
    }

    #[test]
    fn shift_tab_saves_primary_model_and_effort_then_offers_a_new_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        config::Config::default()
            .save(&config_path)
            .expect("save config");
        let mut state = AppState::new();
        state.config_path = Some(config_path.clone());
        state.agent_source_id = "codex-acp".to_string();
        state.active_models.primary = "codex-acp".to_string();
        state.ragnarok_models = [
            (crate::roster::AdapterKind::Codex, "codex-acp"),
            (crate::roster::AdapterKind::Claude, "claude-acp"),
        ]
        .into_iter()
        .map(|(kind, source_id)| crate::roster::ResolvedAgent {
            model: crate::deepswe::Row {
                model: source_id.to_string(),
                reasoning_effort: None,
                pass_at_1: 0.5,
                mean_cost_usd: 1.0,
            },
            model_value: source_id.to_string(),
            launch: crate::roster::AdapterLaunch {
                kind,
                source_id: source_id.to_string(),
                command: PathBuf::from(source_id),
                args: Vec::new(),
                env: Default::default(),
            },
            ranked: true,
            reasoning_effort: None,
        })
        .collect();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::BackTab));

        let picker = state.agent_picker.as_ref().expect("agent selector");
        assert_eq!(picker.role_indices, vec![0, 1]);
        assert_eq!(picker.selected, 0);
        assert_eq!(state.exit_reason, None);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Down));
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));

        assert!(
            state
                .agent_picker
                .as_ref()
                .is_some_and(|picker| picker.step == AgentPickerStep::Effort)
        );
        assert_eq!(state.exit_reason, None);

        for _ in 0..5 {
            handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Down));
        }
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        assert!(
            state
                .agent_picker
                .as_ref()
                .is_some_and(|picker| picker.step == AgentPickerStep::ConfirmSave)
        );
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));

        assert!(
            state
                .agent_picker
                .as_ref()
                .is_some_and(|picker| picker.step == AgentPickerStep::StartNewSession)
        );
        let saved = config::Config::load(&config_path).expect("load config");
        assert_eq!(saved.agent.model, "claude-acp");
        assert_eq!(saved.agent.reasoning_effort.as_deref(), Some("high"));
        assert!(
            state
                .status_line
                .as_ref()
                .is_some_and(|status| status.text.contains("reasoning effort saved"))
        );

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        assert!(state.agent_picker.is_none());
        assert_eq!(state.exit_reason, Some(UiExitReason::NewSession));
    }

    #[test]
    fn declining_new_session_offer_keeps_current_session() {
        let mut state = AppState::new();
        state.agent_picker = Some(crate::app::AgentPicker {
            selected: 0,
            role_indices: Vec::new(),
            effort_selected: 0,
            step: AgentPickerStep::StartNewSession,
            start_new_session: true,
        });
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Down));
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));

        assert!(state.agent_picker.is_none());
        assert_eq!(state.exit_reason, None);
        assert!(
            state
                .status_line
                .as_ref()
                .is_some_and(|status| { status.text.contains("start /new or /clear when ready") })
        );
    }

    #[test]
    fn agent_selector_navigation_wraps() {
        let mut state = AppState::new();
        state.agent_source_id = "codex-acp".to_string();
        state.active_models.primary = "codex-acp".to_string();
        state.ragnarok_models = ["codex-acp", "claude-acp"]
            .into_iter()
            .map(|source_id| crate::roster::ResolvedAgent {
                model: crate::deepswe::Row {
                    model: source_id.to_string(),
                    reasoning_effort: None,
                    pass_at_1: 0.5,
                    mean_cost_usd: 1.0,
                },
                model_value: source_id.to_string(),
                launch: crate::roster::AdapterLaunch {
                    kind: crate::roster::AdapterKind::Custom,
                    source_id: source_id.to_string(),
                    command: PathBuf::from(source_id),
                    args: Vec::new(),
                    env: Default::default(),
                },
                ranked: true,
                reasoning_effort: None,
            })
            .collect();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::BackTab));
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Up));
        assert_eq!(state.agent_picker.as_ref().expect("picker").selected, 1);
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Down));
        assert_eq!(state.agent_picker.as_ref().expect("picker").selected, 0);
    }

    #[test]
    fn selecting_current_agent_advances_to_effort() {
        let mut state = AppState::new();
        state.agent_source_id = "codex-acp".to_string();
        state.active_models.primary = "codex-acp".to_string();
        state.ragnarok_models = ["codex-acp", "claude-acp"]
            .into_iter()
            .map(|source_id| crate::roster::ResolvedAgent {
                model: crate::deepswe::Row {
                    model: source_id.to_string(),
                    reasoning_effort: None,
                    pass_at_1: 0.5,
                    mean_cost_usd: 1.0,
                },
                model_value: source_id.to_string(),
                launch: crate::roster::AdapterLaunch {
                    kind: crate::roster::AdapterKind::Custom,
                    source_id: source_id.to_string(),
                    command: PathBuf::from(source_id),
                    args: Vec::new(),
                    env: Default::default(),
                },
                ranked: true,
                reasoning_effort: None,
            })
            .collect();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::BackTab));
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));

        assert!(
            state
                .agent_picker
                .as_ref()
                .is_some_and(|picker| picker.step == AgentPickerStep::Effort)
        );
        assert_eq!(state.exit_reason, None);
    }

    #[test]
    fn primary_effort_picker_uses_concise_option_labels() {
        let labels = (0..PRIMARY_EFFORT_OPTIONS.len())
            .map(primary_effort_label)
            .collect::<Vec<_>>();

        assert_eq!(
            labels,
            [
                "adapter default",
                "off",
                "minimal",
                "low",
                "medium",
                "high",
                "xhigh",
                "max"
            ]
        );
    }

    #[test]
    fn shift_tab_opens_effort_picker_with_only_one_primary_model() {
        let mut state = AppState::new();
        state.active_models.primary = "codex-acp".to_string();
        state.ragnarok_models = vec![crate::roster::ResolvedAgent {
            model: crate::deepswe::Row {
                model: "codex-acp".to_string(),
                reasoning_effort: None,
                pass_at_1: 0.5,
                mean_cost_usd: 1.0,
            },
            model_value: "codex-acp".to_string(),
            launch: crate::roster::AdapterLaunch {
                kind: crate::roster::AdapterKind::Codex,
                source_id: "codex-acp".to_string(),
                command: PathBuf::from("codex-acp"),
                args: Vec::new(),
                env: Default::default(),
            },
            ranked: true,
            reasoning_effort: None,
        }];
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::BackTab));
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));

        assert!(
            state
                .agent_picker
                .as_ref()
                .is_some_and(|picker| picker.step == AgentPickerStep::Effort)
        );
    }

    #[test]
    fn shift_tab_can_stage_a_primary_model_change_during_a_turn() {
        let mut state = AppState::new();
        state.ragnarok_models = ["codex-acp", "claude-acp"]
            .into_iter()
            .map(|source_id| crate::roster::ResolvedAgent {
                model: crate::deepswe::Row {
                    model: source_id.to_string(),
                    reasoning_effort: None,
                    pass_at_1: 0.5,
                    mean_cost_usd: 1.0,
                },
                model_value: source_id.to_string(),
                launch: crate::roster::AdapterLaunch {
                    kind: crate::roster::AdapterKind::Custom,
                    source_id: source_id.to_string(),
                    command: PathBuf::from(source_id),
                    args: Vec::new(),
                    env: Default::default(),
                },
                ranked: true,
                reasoning_effort: None,
            })
            .collect();
        state.set_connection_state(ConnectionState::Streaming);
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::BackTab));

        assert_eq!(state.exit_reason, None);
        assert!(state.agent_picker.is_some());
    }

    #[test]
    fn question_mark_types_even_when_input_is_empty() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('?')));

        assert!(!state.help_overlay);
        assert_eq!(state.input, "?");
    }

    #[test]
    fn slash_new_triggers_new_session_exit_reason() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "/new".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert_eq!(state.exit_reason, Some(UiExitReason::NewSession));
        // Must not forward the command to the agent.
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn slash_load_triggers_load_session_exit_reason() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "/load".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert_eq!(state.exit_reason, Some(UiExitReason::LoadSession));
        // Must not forward the command to the agent.
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn slash_clear_triggers_clear_session_exit_reason() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "/clear".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert_eq!(state.exit_reason, Some(UiExitReason::ClearSession));
        // Must not forward the command to the agent.
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn slash_compact_routes_to_the_orchestrator() {
        let mut state = AppState::new();
        state.input = "/compact".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(state.input.is_empty());
        assert!(matches!(cmd_rx.try_recv(), Ok(UiCommand::CompactPrimary)));
    }

    #[test]
    fn slash_review_opens_picker_and_direct_target_routes_locally() {
        let mut state = ready_state_with_session();
        state.input = "/review".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);
        assert!(state.review_picker.is_some());
        assert!(cmd_rx.try_recv().is_err());

        state.review_picker = None;
        state.input = "/review uncommitted".to_string();
        submit_prompt(&mut state, &cmd_tx);
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(UiCommand::RunReview {
                target: ReviewTarget::Uncommitted
            })
        ));
    }

    #[test]
    fn slash_review_rejects_busy_turn_without_queueing() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("active".to_string());
        state.input = "/review recent".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(cmd_rx.try_recv().is_err());
        assert_eq!(state.queued_prompt_count(), 0);
        assert!(state.status_line.as_ref().is_some_and(|status| {
            status
                .text
                .contains("only available while the primary agent is idle")
        }));
    }

    #[test]
    fn slash_mjconfig_opens_menu() {
        let mut state = AppState::new();
        state.input = "/mjconfig".to_string();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(state.mjconfig_menu.is_some(), "menu should be open");
        assert!(state.input.is_empty(), "input should be consumed");
    }

    #[test]
    fn slash_models_opens_shared_menu_on_agents_tab() {
        let mut state = AppState::new();
        state.input = "/models".to_string();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        let menu = state.mjconfig_menu.as_ref().expect("menu should be open");
        assert_eq!(menu.editor.tab, crate::settings::SettingsTab::Agents);
        assert!(state.input.is_empty(), "input should be consumed");
    }

    #[test]
    fn models_slash_argument_accepts_refresh_without_matching_prefixes() {
        assert_eq!(models_slash_argument("/models"), Some(""));
        assert_eq!(models_slash_argument("/models refresh"), Some("refresh"));
        assert_eq!(
            models_slash_argument("/models   refresh  "),
            Some("refresh")
        );
        assert_eq!(models_slash_argument("/models other"), Some("other"));
        assert_eq!(models_slash_argument("/models-refresh"), None);
    }

    #[test]
    fn slash_agents_adds_active_models_system_entry() {
        let mut state = AppState::new();
        state.active_models = crate::config::ModelsConfig {
            primary: "claude-opus".to_string(),
            review: "gpt-5.6".to_string(),
            subagent: "gpt-5.5".to_string(),
            primary_source: Some("claude-acp".to_string()),
            review_source: Some("codex-acp".to_string()),
            subagent_source: Some("anvil".to_string()),
        };
        state.input = "/agents".to_string();
        state.input_cursor = 2;
        state.attachments.push(crate::app::PastedAttachment {
            id: 1,
            position: state.input.chars().count(),
            content: String::new(),
        });
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(cmd_rx.try_recv().is_err(), "command must remain local");
        assert!(state.input.is_empty());
        assert!(state.attachments.is_empty());
        assert!(state.image_attachments.is_empty());
        assert_eq!(state.input_cursor, 0);
        assert!(matches!(
            state.transcript.last(),
            Some(Entry::System(text))
                if text
                    == "Active models\nprimary    claude-opus via claude-acp\nreview     gpt-5.6 via codex-acp\nsubagents  gpt-5.5 via anvil\n\nUsage (tokens)\nprimary    0 tokens\nsubagents  0 tokens\nreview     0 tokens"
        ));
    }

    #[test]
    fn slash_subagents_opens_the_retained_actor_viewer_locally() {
        let mut state = AppState::new();
        start_subagent(&mut state, 7, "implementation", "fix the parser");
        state.input = "/subagents".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(cmd_rx.try_recv().is_err());
        assert!(state.nested_agent_viewer);
        assert_eq!(state.nested_agent_selected, Some(7));
        assert!(state.input.is_empty());
    }

    #[test]
    fn mjconfig_menu_previews_live_and_persists_on_accept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut config = config::Config::default();
        config.set_acp_server_policy("codex-acp", config::AcpServerPolicy::Enabled);
        config.save(&path).expect("save initial config");
        let mut state = AppState::new();
        state.config_path = Some(path.clone());
        state.open_mjconfig_menu();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();

        // ACP Servers tab: toggle Codex off.
        state.mjconfig_menu_key(KeyCode::Tab);
        state.mjconfig_menu.as_mut().expect("menu").editor.selected =
            crate::settings::SERVER_ROW_OFFSET;
        handle_mjconfig_menu_key(
            &mut state,
            &cmd_tx,
            KeyModifiers::NONE,
            KeyCode::Char(' '),
            UiMode::FullscreenTui,
        );

        // Appearance tab: preview theme and spinner live.
        state.mjconfig_menu_key(KeyCode::Tab);
        state.mjconfig_menu_key(KeyCode::Tab);
        state.mjconfig_menu_key(KeyCode::Tab);
        state.mjconfig_menu_key(KeyCode::Right);
        let previewed_theme = state.theme_kind;
        state.mjconfig_menu_key(KeyCode::Down);
        state.mjconfig_menu_key(KeyCode::Right);
        let previewed = state.spinner_style;

        // Agents tab: toggle discrete review and apply it to the running session.
        state.mjconfig_menu_key(KeyCode::Tab);
        for _ in 0..3 {
            state.mjconfig_menu_key(KeyCode::Down);
        }
        state.mjconfig_menu_key(KeyCode::Char(' '));

        handle_mjconfig_menu_key(
            &mut state,
            &cmd_tx,
            KeyModifiers::NONE,
            KeyCode::Enter,
            UiMode::FullscreenTui,
        );

        assert!(state.mjconfig_menu.is_none(), "menu closes on accept");
        let saved = config::Config::load(&path).expect("load saved config");
        assert_eq!(saved.spinner, previewed);
        assert_eq!(saved.theme, previewed_theme);
        assert_eq!(
            saved.acp.policy("codex-acp"),
            crate::config::AcpServerPolicy::Disabled
        );
        assert!(!saved.agent.discrete_review);
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(UiCommand::SetReviewPolicy { enabled: false })
        ));
    }

    #[test]
    fn saving_mjconfig_preserves_probed_session_options() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let config = config::Config::default();
        let mut state = AppState::new();
        state.config_path = Some(path);
        state.acp_inventory = crate::roster::discover_inventory(&config);
        let server = state
            .acp_inventory
            .servers
            .first_mut()
            .expect("visible ACP server");
        let server_id = server.id.clone();
        server.session_config = vec![SessionConfigOption::select(
            "service_tier",
            "Service tier",
            "default",
            vec![SessionConfigSelectOption::new("default", "Default")],
        )];
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        persist_mjconfig_selection(&mut state, &cmd_tx, config);

        let server = state
            .acp_inventory
            .servers
            .iter()
            .find(|server| server.id == server_id)
            .expect("same server");
        assert_eq!(server.session_config[0].id.to_string(), "service_tier");
    }

    #[test]
    fn saving_mjconfig_updates_the_live_primary_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut config = config::Config::default();
        config
            .session_config
            .entry("codex-acp".to_string())
            .or_default()
            .defaults
            .insert("config:service_tier".to_string(), "priority".to_string());
        let mut state = AppState::new();
        state.config_path = Some(path);
        state.session_id = Some("session-1".to_string());
        state.active_models.primary_source = Some("codex-acp".to_string());
        state.session_config_options = vec![SessionConfigOption::select(
            "service_tier",
            "Service tier",
            "default",
            vec![
                SessionConfigSelectOption::new("default", "Default"),
                SessionConfigSelectOption::new("priority", "Priority"),
            ],
        )];
        state.session_config_targets = vec![SessionConfigTarget::ConfigOption {
            config_id: "service_tier".into(),
        }];
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();

        persist_mjconfig_selection(&mut state, &cmd_tx, config);

        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(UiCommand::SetSessionConfigOption {
                target: SessionConfigTarget::ConfigOption { config_id },
                value,
            }) if config_id.to_string() == "service_tier" && value.to_string() == "priority"
        ));
    }

    #[test]
    fn mjconfig_menu_cancel_reverts_live_preview() {
        let mut state = AppState::new();
        let orig_theme = state.theme_kind;
        let orig_spinner = state.spinner_style;
        state.open_mjconfig_menu();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        // Preview different values in both sections.
        state.mjconfig_menu_key(KeyCode::Tab);
        state.mjconfig_menu_key(KeyCode::Tab);
        state.mjconfig_menu_key(KeyCode::Tab);
        state.mjconfig_menu_key(KeyCode::Tab);
        state.mjconfig_menu_key(KeyCode::Right);
        state.mjconfig_menu_key(KeyCode::Down);
        state.mjconfig_menu_key(KeyCode::Right);
        assert!(state.theme_kind != orig_theme || state.spinner_style != orig_spinner);

        handle_mjconfig_menu_key(
            &mut state,
            &cmd_tx,
            KeyModifiers::NONE,
            KeyCode::Esc,
            UiMode::FullscreenTui,
        );

        assert!(state.mjconfig_menu.is_none(), "menu closes on cancel");
        assert_eq!(state.theme_kind, orig_theme, "theme reverted");
        assert_eq!(state.spinner_style, orig_spinner, "spinner reverted");
    }

    #[test]
    fn mjconfig_menu_yields_keyboard_to_pending_permission() {
        // The menu can be opened mid-turn; a permission prompt may then arrive
        // and is drawn on top of it. Keys must drive the prompt, not the hidden
        // menu's live preview.
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));
        state.open_mjconfig_menu();
        assert!(state.has_pending_permission());
        assert!(state.mjconfig_menu.is_some());
        let theme_before = state.theme_kind;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            CtEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );

        assert_eq!(
            state.pending_permission().expect("still pending").selected,
            1,
            "Down should move the permission selection"
        );
        assert_eq!(
            state.theme_kind, theme_before,
            "menu must not consume keys while a permission prompt is up"
        );
        assert!(state.mjconfig_menu.is_some(), "menu stays open underneath");
    }

    #[test]
    fn mjconfig_menu_renders_shared_tabbed_settings() {
        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut state = AppState::new();
        state.open_mjconfig_menu();
        let mut transcript_scroll = TranscriptScrollState::default();

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &mut state,
                    &mut transcript_scroll,
                    UiMode::FullscreenTui,
                )
            })
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("mj config"), "rendered:\n{rendered}");
        assert!(rendered.contains("Agents"), "rendered:\n{rendered}");
        assert!(rendered.contains("ACP Priority"), "rendered:\n{rendered}");
        assert!(rendered.contains("ACP Servers"), "rendered:\n{rendered}");
        assert!(rendered.contains("Appearance"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("primary model; plans, implements, and answers"),
            "rendered:\n{rendered}"
        );
    }

    #[test]
    fn transcript_export_markdown_escapes_markdown_and_sizes_fences() {
        let mut state = AppState::new();
        state.agent_label = "agent [x]".to_string();
        state.session_id = Some("session-1".to_string());
        state
            .transcript
            .push(Entry::UserPrompt("# hello".to_string()));
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "cargo `test`".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![
                    ToolCallOutput::Text("```\nnot markdown".to_string()),
                    ToolCallOutput::Terminal {
                        terminal_id: "call_q403CLAwcOWdujDT6Xylsua6".to_string(),
                        output: String::new(),
                        truncated: false,
                        exit_status: None,
                    },
                ],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let markdown = transcript_export_markdown(&state);

        assert!(markdown.contains("- Agent: agent \\[x\\]"));
        assert!(markdown.contains("## You\n\n\\# hello"));
        assert!(markdown.contains("## Tool: cargo \\`test\\`"));
        assert!(markdown.contains("- Kind: exec"));
        assert!(markdown.contains("- Status: done"));
        assert!(markdown.contains("````text\n```\nnot markdown\n````"));
        assert!(markdown.contains("### Terminal output"));
        assert!(markdown.contains("_no terminal output received._"));
        assert!(
            !markdown.contains("call_q403"),
            "terminal ids should not leak into exported transcript markdown: {markdown}"
        );
    }

    #[test]
    fn slash_export_writes_transcript_without_runtime_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut state = AppState::new();
        state.transcript_export_dir = Some(dir.path().to_path_buf());
        state
            .transcript
            .push(Entry::UserPrompt("hello".to_string()));
        state.input = "/export".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(cmd_rx.try_recv().is_err());
        let status = state.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Info);
        assert!(status.text.contains("transcript exported to"));
        let files: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read export dir")
            .collect::<Result<Vec<_>, _>>()
            .expect("dir entries");
        assert_eq!(files.len(), 1);
        let body = std::fs::read_to_string(files[0].path()).expect("export body");
        assert!(body.contains("## You\n\nhello"));
    }

    #[test]
    fn full_export_includes_nested_transcripts_but_default_export_does_not() {
        let mut state = AppState::new();
        state
            .transcript
            .push(Entry::UserPrompt("delegate privately".to_string()));
        start_subagent(&mut state, 1, "implementer", "private objective");
        state.apply_event(subagent_session_update(SessionUpdate::AgentMessageChunk(
            text_chunk("PRIVATE_NESTED_RESULT"),
        )));

        let primary = transcript_export_markdown_with_nested(&state, false);
        let full = transcript_export_markdown_with_nested(&state, true);

        assert!(!primary.contains("PRIVATE_NESTED_RESULT"));
        assert!(!primary.contains("Nested Agent Transcripts"));
        assert!(full.contains("Nested Agent Transcripts"));
        assert!(full.contains("Subagent #1: implementer"));
        assert!(full.contains("PRIVATE\\_NESTED\\_RESULT"));
    }

    #[test]
    fn full_export_includes_offloaded_nested_history() {
        let mut state = AppState::new();
        start_subagent(&mut state, 1, "large", "offload me");
        state.apply_event(subagent_session_update(SessionUpdate::AgentMessageChunk(
            text_chunk("history line\nOFFLOADED_EXACT_SUFFIX"),
        )));
        state.apply_event(subagent_finished(SubagentOutcome::Completed));
        state.force_offload_nested_actor_for_test(1);

        let full = transcript_export_markdown_with_nested(&state, true);
        assert!(full.contains("OFFLOADED\\_EXACT\\_SUFFIX"));
        let rendered =
            render_nested_agent_lines(&state, state.nested_agent(1).expect("offloaded actor"), 100)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>();
        assert!(rendered.iter().any(|line| line.contains("OFFLOADED")));
    }

    #[test]
    fn resumed_offloaded_actor_shows_archived_and_live_history() {
        let mut state = AppState::new();
        start_subagent(&mut state, 1, "worker", "first turn");
        state.apply_event(subagent_session_update(SessionUpdate::AgentMessageChunk(
            text_chunk("ARCHIVED_TURN"),
        )));
        state.apply_event(subagent_finished(SubagentOutcome::Completed));
        state.force_offload_nested_actor_for_test(1);

        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id: 1,
            resumed: true,
            label: "worker".to_string(),
            model: Some("gpt-y".to_string()),
            agent: "codex-acp".to_string(),
            objective: "second turn".to_string(),
        }));
        state.apply_event(subagent_session_update(SessionUpdate::AgentMessageChunk(
            text_chunk("RESUMED_TURN"),
        )));

        let full = transcript_export_markdown_with_nested(&state, true);
        assert!(full.contains("ARCHIVED\\_TURN"));
        assert!(full.contains("RESUMED\\_TURN"));
        let rendered =
            render_nested_agent_lines(&state, state.nested_agent(1).expect("resumed actor"), 100)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>();
        assert!(rendered.iter().any(|line| line.contains("ARCHIVED")));
        assert!(rendered.iter().any(|line| line.contains("RESUMED")));

        state.apply_event(subagent_finished(SubagentOutcome::Completed));
        state.force_offload_nested_actor_for_test(1);
        let actor = state.nested_agent(1).expect("twice-offloaded actor");
        assert_eq!(actor.archived_history_segments(), 2);
        let history = actor.archived_history_markdown().expect("all segments");
        assert!(history.contains("ARCHIVED\\_TURN"));
        assert!(history.contains("RESUMED\\_TURN"));
    }

    #[test]
    fn slash_fork_sends_fork_session_command() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.session_fork_supported = true;
        state.input = "/fork".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(state.exit_reason.is_none());
        assert!(matches!(cmd_rx.try_recv(), Ok(UiCommand::ForkSession)));
        assert_eq!(state.connection_state(), ConnectionState::Forking);
        assert!(state.is_busy());
        assert!(state.input.is_empty());
        let status = state.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Info);
        assert_eq!(status.text, "forking session...");
    }

    #[test]
    fn slash_side_with_question_requests_an_isolated_view() {
        let mut state = AppState::new();
        state.session_id = Some("main-session".to_string());
        state.side_session_supported = true;
        state.input = "/side explain the failure".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(state.side_start_requested);
        assert_eq!(
            state.side_initial_question.as_deref(),
            Some("explain the failure")
        );
        assert!(state.transcript.is_empty());
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn slash_side_reports_missing_capabilities_without_failing_runtime() {
        let mut state = AppState::new();
        state.session_id = Some("main-session".to_string());
        state.side_session_unsupported_reason = Some(
            "side conversations are not supported by this agent; missing session/delete"
                .to_string(),
        );
        state.input = "/side".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(!state.side_start_requested);
        assert!(!state.runtime_closed);
        assert!(cmd_rx.try_recv().is_err());
        assert_eq!(
            state
                .status_line
                .as_ref()
                .map(|status| status.text.as_str()),
            Some("side conversations are not supported by this agent; missing session/delete")
        );
    }

    #[test]
    fn nested_side_command_is_rejected() {
        let mut state = AppState::new();
        state.is_side = true;
        state.session_id = Some("side-session".to_string());
        state.input = "/side nested".to_string();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(!state.side_start_requested);
        assert_eq!(
            state
                .status_line
                .as_ref()
                .map(|status| status.text.as_str()),
            Some("nested side conversations are not supported")
        );
    }

    #[test]
    fn prompt_submitted_during_fork_is_queued_until_fork_starts() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.session_fork_supported = true;
        state.input = "/fork".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(matches!(cmd_rx.try_recv(), Ok(UiCommand::ForkSession)));
        assert_eq!(state.connection_state(), ConnectionState::Forking);

        state.input = "queued prompt".to_string();
        submit_prompt(&mut state, &cmd_tx);

        assert!(cmd_rx.try_recv().is_err());
        assert_eq!(state.queued_prompt_count(), 1);
        assert!(
            !state
                .transcript
                .iter()
                .any(|entry| matches!(entry, Entry::UserPrompt(_))),
            "queued prompt must not be echoed until it is sent"
        );

        state.apply_event(UiEvent::SessionStarted {
            session_id: "forked-session".to_string(),
            resumed: false,
        });
        drain_queued_prompt(&mut state, &cmd_tx);

        match cmd_rx.try_recv() {
            Ok(UiCommand::SendPrompt { text, images }) => {
                assert_eq!(text, "queued prompt");
                assert!(images.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert_eq!(state.queued_prompt_count(), 0);
        let user_prompts: Vec<_> = state
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                Entry::UserPrompt(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(user_prompts, vec!["queued prompt"]);
    }

    #[test]
    fn slash_fork_warns_when_agent_does_not_support_fork() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "/fork".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(state.exit_reason.is_none());
        assert!(cmd_rx.try_recv().is_err());
        let status = state.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Warning);
        assert_eq!(
            status.text,
            "session fork is not supported by this agent (unstable ACP extension not advertised)"
        );
    }

    #[test]
    fn unknown_slash_mj_command_warns_without_exit() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "/mj:bogus".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(state.exit_reason.is_none());
        assert!(cmd_rx.try_recv().is_err());
        let warn = state.status_line.expect("warning");
        assert_eq!(warn.kind, StatusKind::Warning);
        assert!(warn.text.contains("/mj:bogus"), "msg: {}", warn.text);
        assert_eq!(state.transcript.len(), 1);
        match &state.transcript[0] {
            Entry::System(text) => assert_eq!(text, "warning: unknown mj command: /mj:bogus"),
            other => panic!("unexpected entry: {other:?}"),
        }
    }

    #[test]
    fn transcript_scroll_stays_pinned_to_bottom_when_following() {
        let mut tracker = TranscriptScrollState::default();
        let mut offset = 0;

        tracker.reconcile(&mut offset, 80, 20);
        tracker.reconcile(&mut offset, 100, 20);

        assert_eq!(offset, 0);
    }

    #[test]
    fn transcript_scroll_preserves_position_when_new_rows_arrive() {
        let mut tracker = TranscriptScrollState::default();
        let mut offset = 0;

        tracker.reconcile(&mut offset, 100, 20);
        offset = 12;
        tracker.reconcile(&mut offset, 112, 20);

        assert_eq!(offset, 24);
    }

    #[test]
    fn transcript_scroll_adjusts_for_resize() {
        let mut tracker = TranscriptScrollState::default();
        let mut offset = 0;

        tracker.reconcile(&mut offset, 100, 20);
        offset = 12;
        tracker.reconcile(&mut offset, 100, 28);

        assert_eq!(offset, 4);
    }

    #[test]
    fn transcript_scroll_reconciles_offsets_above_u16_max() {
        let mut tracker = TranscriptScrollState::default();
        let mut offset = 0;

        tracker.reconcile(&mut offset, 80_000, 24);
        offset = u16::MAX as usize + 5;
        tracker.reconcile(&mut offset, 80_050, 24);

        assert_eq!(offset, u16::MAX as usize + 55);
    }

    /// Integration of the three scrolling concerns that fired together in
    /// practice: the user scrolls up, more chunks arrive, then the
    /// terminal resizes. The visible top-of-window must stay anchored to
    /// whatever the user was reading. Individual concerns are covered by
    /// the tests above; this exercises them in sequence.
    #[test]
    fn streaming_chunks_and_resize_preserve_user_scroll_anchor() {
        let mut tracker = TranscriptScrollState::default();
        let mut offset = 0;

        // Initial frame: 100 wrapped rows visible in a 20-row window,
        // pinned to bottom.
        tracker.reconcile(&mut offset, 100, 20);

        // User scrolls up by 12 rows.
        offset = 12;

        // Streaming chunks grow the transcript by 8 rows.
        tracker.reconcile(&mut offset, 108, 20);
        // Top-of-window should still be at the same content line, so the
        // offset grows by exactly the number of new rows.
        assert_eq!(offset, 20, "new rows must not shift the user's view");

        // Terminal resizes taller (28 rows visible).
        tracker.reconcile(&mut offset, 108, 28);
        // Window grew by 8 rows so the same top-line is now 8 rows
        // closer to bottom; offset drops by 8.
        assert_eq!(offset, 12, "resize must not shift the user's view");

        // More chunks arrive after the resize.
        tracker.reconcile(&mut offset, 116, 28);
        assert_eq!(
            offset, 20,
            "subsequent rows still grow the offset by their count"
        );
    }

    #[test]
    fn transcript_sink_emits_each_stable_entry_once() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.push_system_message("first");
        let first: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(first, vec!["first", ""]);

        assert!(sink.pending_lines(&state, 80).is_empty());

        state.push_system_message("second");
        let second: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(second, vec!["second", ""]);
    }

    #[test]
    fn transcript_sink_has_no_connection_announcement_to_finalize() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();
        state.set_primary_acp_name("Claude Code");

        state.announce_waiting_for_primary();
        assert!(sink.pending_lines(&state, 80).is_empty());

        state.apply_event(UiEvent::Connected {
            agent_name: Some("claude-agent-acp".into()),
            agent_version: Some("1.0".into()),
            prompt_images_supported: false,
            session_fork_supported: false,
            side_session_supported: false,
            side_session_unsupported_reason: None,
        });
        assert!(sink.pending_lines(&state, 80).is_empty());
        assert!(sink.pending_lines(&state, 80).is_empty());
    }

    #[test]
    fn transcript_sink_can_resync_after_resize_replay() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.push_system_message("first");
        state.push_system_message("second");
        sink.mark_emitted(stable_transcript_entry_count(&state));

        assert!(sink.pending_lines(&state, 80).is_empty());

        state.push_system_message("third");
        let pending: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(pending, vec!["third", ""]);
    }

    #[test]
    fn transcript_sink_abuts_sequential_tool_calls_like_full_render() {
        // Regression: the streaming scrollback used to commit a trailing blank
        // after a completed tool call before the next one existed, so two
        // sequentially-run tool calls got a permanent blank between their rails
        // — the opposite of the abutment the full render produces.
        fn push_completed_tool_call(state: &mut AppState, id: &str, title: &str) {
            state.tool_calls.insert(
                id.to_string(),
                crate::app::ToolCallView {
                    title: title.to_string(),
                    kind: ToolKind::Execute,
                    status: ToolCallStatus::Completed,
                    body: Vec::new(),
                },
            );
            state.transcript.push(Entry::ToolCall(id.to_string()));
        }

        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();
        state.record_user_prompt("go".to_string()); // turn in flight (streaming)

        let mut emitted: Vec<String> = Vec::new();

        // First tool call completes while it is still the last entry: its
        // content flushes right away (promptness), but its trailing separator
        // is held back until we know a following tool call could abut it.
        push_completed_tool_call(&mut state, "call-1", "first");
        emitted.extend(sink.pending_lines(&state, 80).iter().map(line_text));
        assert!(
            emitted.iter().any(|l| l.contains("first")),
            "tool output must flush promptly, not wait for the next entry: {emitted:?}"
        );
        assert_ne!(
            emitted.last().map(String::as_str),
            Some(""),
            "the trailing separator must be held back: {emitted:?}"
        );

        // Second tool call arrives; now call-1's held separator is dropped so
        // the rails abut.
        push_completed_tool_call(&mut state, "call-2", "second");
        emitted.extend(sink.pending_lines(&state, 80).iter().map(line_text));

        // Turn ends; the final held separator is emitted.
        state.set_connection_state(ConnectionState::Ready);
        emitted.extend(sink.pending_lines(&state, 80).iter().map(line_text));

        // The incremental scrollback must match a single full render.
        let full: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(
            emitted, full,
            "incremental flush diverged from full render: {emitted:?} vs {full:?}"
        );

        let first = emitted
            .iter()
            .position(|l| l.contains("first"))
            .expect("first tool row");
        let second = emitted
            .iter()
            .position(|l| l.contains("second"))
            .expect("second tool row");
        assert_eq!(
            second,
            first + 1,
            "sequential tool calls must abut in scrollback: {emitted:?}"
        );
    }

    #[test]
    fn inline_resize_reflow_debounces_until_terminal_size_settles() {
        let mut reflow = InlineResizeReflow::default();
        let start = Instant::now();

        reflow.note_resize(
            Size {
                width: 120,
                height: 40,
            },
            start,
        );
        assert!(reflow.is_pending());
        assert!(reflow.waiting(start + INLINE_RESIZE_REFLOW_DEBOUNCE / 2));

        reflow.note_resize(
            Size {
                width: 100,
                height: 40,
            },
            start + INLINE_RESIZE_REFLOW_DEBOUNCE / 2,
        );
        assert!(!reflow.is_due(start + INLINE_RESIZE_REFLOW_DEBOUNCE));
        assert!(reflow.is_due(start + INLINE_RESIZE_REFLOW_DEBOUNCE * 2));

        reflow.clear();
        assert!(!reflow.is_pending());
    }

    #[test]
    fn inline_resize_reflow_records_clamped_inline_height() {
        let size = Size {
            width: 80,
            height: 4,
        };

        assert_eq!(clamped_inline_height(INLINE_CHAT_HEIGHT, size), 4);
        assert_eq!(clamped_inline_height(2, size), 2);
        assert_eq!(clamped_inline_height(0, size), 1);
    }

    #[test]
    fn transcript_reader_width_resize_clears_reserved_margin() {
        let mut state = AppState::new();
        state.open_transcript_viewer();

        let plan = inline_viewport_resize_plan(
            &state,
            Rect::new(0, 1, 80, 22),
            Size {
                width: 60,
                height: 23,
            },
            22,
        )
        .expect("reader width change needs a viewport repair");

        assert_eq!(plan.height, 22);
        assert_eq!(plan.origin, Position::new(0, 1));
        assert!(plan.clear_visible_screen);
    }

    #[test]
    fn compact_inline_height_change_keeps_clear_scoped_to_viewport() {
        let state = AppState::new();
        let plan = inline_viewport_resize_plan(
            &state,
            Rect::new(0, 17, 80, 7),
            Size {
                width: 80,
                height: 24,
            },
            7,
        )
        .expect("height change needs a viewport repair");

        assert_eq!(plan.height, INLINE_CHAT_HEIGHT);
        assert_eq!(plan.origin, Position::new(0, 17));
        assert!(!plan.clear_visible_screen);
    }

    #[test]
    fn inline_resize_reflow_snapshot_replays_streamed_prefix_at_new_width() {
        let mut state = AppState::new();
        state.record_user_prompt("hello from the resize test".to_string());
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("streaming is not stable yet"),
        )));

        let snapshot = inline_resize_reflow_snapshot(
            &state,
            Size {
                width: 12,
                height: 4,
            },
        )
        .expect("snapshot");

        assert_eq!(snapshot.actual_height, 4);
        assert_eq!(snapshot.stable_entries, 1);
        let replayed: Vec<String> = snapshot.lines.iter().map(line_text).collect();
        assert!(replayed.join("\n").contains("hello from"));
    }

    #[test]
    fn inline_resize_reflow_waits_while_transcript_viewer_is_open() {
        let mut reflow = InlineResizeReflow::default();
        let start = Instant::now();
        reflow.note_resize(
            Size {
                width: 120,
                height: 40,
            },
            start,
        );
        let due = start + INLINE_RESIZE_REFLOW_DEBOUNCE;
        let mut state = AppState::new();

        assert!(should_run_inline_resize_reflow(&reflow, &state, due));

        state.open_transcript_viewer();
        assert!(!should_run_inline_resize_reflow(&reflow, &state, due));

        state.close_transcript_viewer();
        assert!(should_run_inline_resize_reflow(&reflow, &state, due));
    }

    #[test]
    fn transcript_sink_streams_stable_prefix_during_foreground_turn() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.record_user_prompt("hello".to_string());
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("world"),
        )));

        let pending: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(pending, vec!["❯ hello", ""]);

        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        let rendered: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(rendered, vec!["● world", ""]);
        assert!(sink.pending_lines(&state, 80).is_empty());
    }

    #[test]
    fn transcript_sink_flushes_visible_streaming_answer_on_exit() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.record_user_prompt("hello".to_string());
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("visible final answer"),
        )));

        let before_exit: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(before_exit, vec!["❯ hello", ""]);

        let on_exit: Vec<String> = sink
            .pending_lines_for_exit(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(on_exit, vec!["● visible final answer", ""]);
        assert!(sink.pending_lines_for_exit(&state, 80).is_empty());
    }

    #[test]
    fn transcript_sink_exit_commits_deferred_trailing_tool_separator() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.record_user_prompt("run it".to_string());
        let mut emitted: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "cargo test".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: Vec::new(),
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        emitted.extend(sink.pending_lines(&state, 80).iter().map(line_text));
        assert!(sink.deferred_tool_separator);
        assert_ne!(emitted.last().map(String::as_str), Some(""));

        let on_exit: Vec<String> = sink
            .pending_lines_for_exit(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(on_exit, vec![""]);
        emitted.extend(on_exit);

        let full: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(emitted, full);
        assert!(!sink.deferred_tool_separator);
        assert!(sink.pending_lines_for_exit(&state, 80).is_empty());
    }

    #[test]
    fn transcript_sink_flushes_old_agent_message_while_later_one_streams() {
        // Do not drain the first turn before beginning the second. The sink
        // must still flush its completed agent result even though a later agent
        // entry is the exact one currently receiving chunks.
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.record_user_prompt("first prompt".to_string());
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("first result"),
        )));
        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });

        state.record_user_prompt("second prompt".to_string());
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("second result"),
        )));

        let flushed = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert_eq!(
            flushed,
            vec![
                "❯ first prompt",
                "",
                "● first result",
                "",
                "❯ second prompt",
                "",
            ]
        );
        assert!(sink.pending_lines(&state, 80).is_empty());
    }

    #[test]
    fn transcript_sink_holds_mutable_primary_thought_during_subagent_activity() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.record_user_prompt("delegate this".to_string());
        let _ = sink.pending_lines(&state, 80);
        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id: 1,
            resumed: false,
            model: None,
            agent: "codex-acp".to_string(),
            objective: String::new(),
            label: "subagent".to_string(),
        }));
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
            text_chunk("I"),
        )));
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
            text_chunk(" need"),
        )));
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
            text_chunk(" to inspect this"),
        )));

        assert!(matches!(
            state.transcript.as_slice(),
            [Entry::UserPrompt(_), Entry::System(started), Entry::AgentThought(text)]
                if started.contains("subagent #1") && text.text == "I need to inspect this"
        ));
        // The subagent's one permanent start record is immediately flushable;
        // nothing else about the subagent ever rewrites the transcript.
        assert_eq!(stable_transcript_entry_count(&state), 2);
        let started_flush = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(started_flush.contains("subagent #1"), "{started_flush}");

        let tail = inline_transcript_tail_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert_eq!(tail, vec!["○ I need to inspect this", ""]);

        state.apply_event(subagent_session_update(SessionUpdate::AgentThoughtChunk(
            text_chunk("implementing"),
        )));
        state.apply_event(subagent_session_update(SessionUpdate::AgentMessageChunk(
            text_chunk("done"),
        )));
        assert_eq!(stable_transcript_entry_count(&state), 2);

        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
            text_chunk(" and report"),
        )));
        assert!(matches!(
            &state.transcript[2],
            Entry::AgentThought(thought)
                if thought.text == "I need to inspect this and report" && !thought.completed
        ));

        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("Here is the result"),
        )));
        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        let flushed = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert_eq!(
            flushed,
            vec!["○ thought · 1 line", "", "● Here is the result", "",]
        );
        let nested =
            render_nested_agent_lines(&state, state.nested_agent(1).expect("nested actor"), 80)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join("\n");
        assert!(nested.contains("implementing"), "{nested}");
        assert!(nested.contains("done"), "{nested}");
    }

    #[test]
    fn replayed_and_local_turns_stream_in_order() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        // Session replay uses UserMessageChunk while idle, so it has no local
        // PromptTurn metadata and must not become an inline flush barrier.
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::UserMessageChunk(
            text_chunk("replayed prompt"),
        )));
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("replayed answer"),
        )));
        assert_eq!(
            stable_transcript_entry_count(&state),
            state.transcript.len()
        );
        let replayed = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert_eq!(
            replayed,
            vec!["❯ replayed prompt", "", "● replayed answer", ""]
        );

        state.record_user_prompt("local prompt".to_string());
        state.tool_calls.insert(
            "local-tool".to_string(),
            crate::app::ToolCallView {
                title: "write src/lib.rs".to_string(),
                kind: ToolKind::Edit,
                status: ToolCallStatus::Completed,
                body: Vec::new(),
            },
        );
        state
            .transcript
            .push(Entry::ToolCall("local-tool".to_string()));
        assert_eq!(stable_transcript_entry_count(&state), 4);

        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        let streamed = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        let streamed = streamed.join("\n");
        assert!(streamed.contains("local prompt"), "{streamed}");
        assert!(streamed.contains("write src/lib.rs"), "{streamed}");

        let snapshot = inline_resize_reflow_snapshot(
            &state,
            Size {
                width: 20,
                height: 4,
            },
        )
        .expect("snapshot");
        assert_eq!(snapshot.stable_entries, state.transcript.len());
        let reflowed = snapshot
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(reflowed.contains("replayed answer"), "{reflowed}");
        assert!(reflowed.contains("write"), "{reflowed}");
        assert!(reflowed.contains("src/lib.rs"), "{reflowed}");
    }

    #[test]
    fn finalized_turn_summarizes_successes_but_keeps_failures_and_full_reader_data() {
        let mut state = AppState::new();
        state.record_user_prompt("make the change".to_string());
        state.tool_calls.insert(
            "write-lib".to_string(),
            crate::app::ToolCallView {
                title: "write src/lib.rs".to_string(),
                kind: ToolKind::Edit,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Diff {
                    path: "src/lib.rs".to_string(),
                    old_text: Some("old".to_string()),
                    new_text: "new".to_string(),
                }],
            },
        );
        state
            .transcript
            .push(Entry::ToolCall("write-lib".to_string()));
        state.tool_calls.insert(
            "nested-write".to_string(),
            crate::app::ToolCallView {
                title: "write src/main.rs".to_string(),
                kind: ToolKind::Edit,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Diff {
                    path: "src/main.rs".to_string(),
                    old_text: None,
                    new_text: "fn main() {}".to_string(),
                }],
            },
        );
        state
            .transcript
            .push(Entry::SubagentToolCall("nested-write".to_string()));
        state.tool_calls.insert(
            "failed-test".to_string(),
            crate::app::ToolCallView {
                title: "cargo test -p mjolnir".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Failed,
                body: vec![ToolCallOutput::Text("error: regression".to_string())],
            },
        );
        state
            .transcript
            .push(Entry::ToolCall("failed-test".to_string()));
        state
            .transcript
            .push(Entry::AgentMessage("Here is what I changed.".to_string()));

        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });

        let compact = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            compact.contains("3 tools · 2 files changed · 1 failed"),
            "{compact}"
        );
        assert!(compact.contains("cargo test -p mjolnir"), "{compact}");
        assert!(compact.contains("error: regression"), "{compact}");
        assert!(compact.contains("└─ final response"), "{compact}");
        assert!(!compact.contains("write src/lib.rs"), "{compact}");
        assert!(!compact.contains("write src/main.rs"), "{compact}");

        let narrow = render_transcript_lines(&state, 18)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(
            narrow.iter().any(|line| line.contains("cargo test")),
            "{narrow:?}"
        );
        assert!(
            narrow.iter().any(|line| line.contains("mjolnir")),
            "{narrow:?}"
        );
        assert!(
            narrow.iter().any(|line| line.contains("regression")),
            "{narrow:?}"
        );

        let full = render_full_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(full.contains("write src/lib.rs"), "{full}");
        assert!(full.contains("write src/main.rs"), "{full}");
        assert!(full.contains("src/lib.rs"), "{full}");
        assert!(full.contains("src/main.rs"), "{full}");
        assert!(full.contains("subagent"), "{full}");

        let markdown = transcript_export_markdown(&state);
        assert!(markdown.contains("write src/lib\\.rs"));
        assert!(markdown.contains("write src/main\\.rs"));
        assert!(markdown.contains("src/lib\\.rs"));
        assert!(markdown.contains("src/main\\.rs"));
    }

    #[test]
    fn foreground_handoff_streams_completed_subagent_activity_to_scrollback() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.record_user_prompt("delegate this".to_string());
        let initial: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(initial, vec!["❯ delegate this", ""]);

        let bridge = ToolCall::new("bridge-call", "mcp.mj-subagents.create_subagent")
            .status(ToolCallStatus::InProgress)
            .raw_input(serde_json::json!({
                "server": "mj-subagents",
                "tool": "create_subagent",
                "arguments": { "prompt": "forge the change" }
            }));
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::ToolCall(bridge)));
        state.apply_event(UiEvent::InternalMessage(InternalMessage {
            source: "primary".to_string(),
            target: "subagent".to_string(),
            kind: crate::event::InternalMessageKind::Delegation,
            text: "forge the change".to_string(),
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
        state.apply_event(subagent_session_update(SessionUpdate::ToolCall(
            ToolCall::new("nested-call", "completed nested command")
                .status(ToolCallStatus::Completed),
        )));

        assert_eq!(
            state
                .tool_calls
                .get("bridge-call")
                .expect("parent bridge")
                .status,
            ToolCallStatus::InProgress
        );
        assert_eq!(
            stable_transcript_entry_count(&state),
            state.transcript.len()
        );

        let streamed = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(streamed.contains("subagent #1"), "{streamed}");
        assert!(!streamed.contains("completed nested command"), "{streamed}");
        assert!(!streamed.contains("mcp.mj-subagents"), "{streamed}");
        let nested =
            render_nested_agent_lines(&state, state.nested_agent(1).expect("nested actor"), 80)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join("\n");
        assert!(nested.contains("delegation brief"), "{nested}");
        assert!(nested.contains("forge the change"), "{nested}");
        assert!(nested.contains("completed nested command"), "{nested}");
        assert!(inline_transcript_tail_lines(&state, 80).is_empty());
    }

    #[test]
    fn foreground_delegation_has_a_distinct_handoff_and_live_subagent_activity() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.record_user_prompt("trace startup".to_string());
        let _ = sink.pending_lines(&state, 80);
        state.apply_event(UiEvent::InternalMessage(InternalMessage {
            source: "primary".to_string(),
            target: "subagent #1".to_string(),
            kind: crate::event::InternalMessageKind::Delegation,
            text: "trace startup".to_string(),
            owner_subagent_id: Some(1),
        }));
        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id: 1,
            resumed: false,
            model: None,
            agent: "codex-acp".to_string(),
            objective: String::new(),
            label: "explorer".to_string(),
        }));
        state.apply_event(subagent_session_update(SessionUpdate::AgentThoughtChunk(
            text_chunk("searching entry points"),
        )));

        let streamed = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(streamed.contains("subagent #1"), "{streamed}");
        let live = inline_transcript_tail_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!live.contains("searching entry points"), "{live}");
        let nested =
            render_nested_agent_lines(&state, state.nested_agent(1).expect("nested actor"), 80)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join("\n");
        assert!(nested.contains("searching entry points"), "{nested}");
        assert!(!transcript_export_markdown(&state).contains("delegation"));
        assert!(
            transcript_export_markdown_with_nested(&state, true)
                .contains("primary → subagent #1 delegation")
        );
    }

    #[test]
    fn inline_chat_streams_primary_and_subagent_through_one_transcript_tail() {
        let mut state = AppState::new();
        state.agent_label = "gpt-primary".to_string();
        state.record_user_prompt("delegate this".to_string());
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
            text_chunk("planning the handoff"),
        )));
        let primary_tail = inline_transcript_tail_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert_eq!(primary_tail, vec!["○ planning the handoff", ""]);

        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id: 1,
            resumed: false,
            model: None,
            agent: "codex-acp".to_string(),
            objective: String::new(),
            label: "subagent · gpt-builder".to_string(),
        }));
        state.apply_event(subagent_session_update(SessionUpdate::AgentThoughtChunk(
            text_chunk("working now"),
        )));

        let live = inline_transcript_tail_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert_eq!(
            live,
            vec![
                "○ planning the handoff",
                "",
                "subagent #1 · subagent · gpt-builder · started",
                ""
            ]
        );
        assert_eq!(
            desired_inline_height(
                &state,
                Size {
                    width: 80,
                    height: 40,
                },
            ),
            INLINE_CHAT_HEIGHT,
            "streamed transcript rows must not resize the inline viewport"
        );

        let backend = TestBackend::new(120, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_header(frame, frame.area(), &state))
            .expect("draw active header");
        let active = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(active.contains("subagent · gpt-builder"), "{active}");
        assert!(!active.contains("gpt-primary"), "{active}");

        state.apply_event(subagent_finished(SubagentOutcome::Completed));
        assert_eq!(
            inline_transcript_tail_lines(&state, 80)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>(),
            vec![
                "○ planning the handoff",
                "",
                "subagent #1 · subagent · gpt-builder · started",
                "",
                "subagent #1 · subagent · gpt-builder · completed · 0s",
                ""
            ]
        );
        let nested =
            render_nested_agent_lines(&state, state.nested_agent(1).expect("nested actor"), 80)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join("\n");
        assert!(nested.contains("working now"), "{nested}");
        terminal
            .draw(|frame| draw_header(frame, frame.area(), &state))
            .expect("draw restored header");
        let restored = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(restored.contains("gpt-primary"), "{restored}");
    }

    #[test]
    fn streamed_wrap_boundaries_keep_inline_geometry_and_input_height_stable() {
        let mut state = AppState::new();
        state.record_user_prompt("write a long answer".to_string());
        let terminal_size = Size {
            width: 40,
            height: 30,
        };
        let baseline = desired_inline_height(&state, terminal_size);
        let area = Rect::new(0, 0, terminal_size.width, baseline);
        let baseline_tail_height = inline_transcript_tail_height(&state, area);
        let baseline_input_area = inline_chat_layout(&state, area)[5];
        let mut terminal =
            Terminal::new(TestBackend::new(terminal_size.width, baseline)).expect("terminal");
        terminal
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("draw empty tail");
        let baseline_header_row = buffer_lines(terminal.backend().buffer())
            .iter()
            .position(|line| line.contains(&mjolnir_version_label()))
            .expect("header row");

        for chunk in [
            "one two three four five six seven eight nine ten ",
            "eleven twelve thirteen fourteen fifteen sixteen ",
            "seventeen eighteen nineteen twenty twenty-one ",
        ] {
            state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
                text_chunk(chunk),
            )));
            assert_eq!(
                desired_inline_height(&state, terminal_size),
                baseline,
                "crossing a streamed wrap boundary must not resize the viewport"
            );
            assert_eq!(
                inline_transcript_tail_height(&state, area),
                baseline_tail_height,
                "streamed rows must not change the space reserved above the header"
            );
        }

        terminal
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("draw streamed tail");
        let streamed_header_row = buffer_lines(terminal.backend().buffer())
            .iter()
            .position(|line| line.contains(&mjolnir_version_label()))
            .expect("header row");
        assert_eq!(
            streamed_header_row, baseline_header_row,
            "streaming must not move the header inside the fixed viewport"
        );
        let streamed_input_area = inline_chat_layout(&state, area)[5];
        assert_eq!(
            streamed_input_area, baseline_input_area,
            "the input panel rendered by the inline layout must not move or resize"
        );
        assert_eq!(
            streamed_input_area.height, MIN_INPUT_HEIGHT,
            "the compact inline layout must retain the full input allocation"
        );
    }

    #[test]
    fn foreground_handoff_holds_active_subagent_result_and_reattaches_primary() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();
        state.record_user_prompt("delegate this".to_string());
        assert_eq!(
            sink.pending_lines(&state, 80)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>(),
            vec!["❯ delegate this", ""]
        );

        state.apply_event(UiEvent::InternalMessage(InternalMessage {
            source: "primary".to_string(),
            target: "subagent".to_string(),
            kind: crate::event::InternalMessageKind::Delegation,
            text: "forge it".to_string(),
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
        assert!(state.subagent_active);
        let handoff = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(handoff.contains("subagent #1"), "{handoff}");

        state.apply_event(subagent_session_update(SessionUpdate::AgentMessageChunk(
            text_chunk("first subagent segment"),
        )));
        assert!(sink.pending_lines(&state, 80).is_empty());

        let live = inline_transcript_tail_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!live.contains("first subagent segment"), "{live}");

        state.apply_event(subagent_session_update(SessionUpdate::AgentMessageChunk(
            text_chunk("subagent final"),
        )));
        assert!(sink.pending_lines(&state, 80).is_empty());
        state.apply_event(subagent_finished(SubagentOutcome::Completed));
        assert!(!state.subagent_active);
        let subagent_final = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(subagent_final.contains("completed"), "{subagent_final}");
        assert!(
            !subagent_final.contains("subagent final"),
            "{subagent_final}"
        );
        let nested =
            render_nested_agent_lines(&state, state.nested_agent(1).expect("nested actor"), 80)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join("\n");
        assert!(nested.contains("first subagent segment"), "{nested}");
        assert!(nested.contains("subagent final"), "{nested}");

        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("primary resumed"),
        )));
        assert!(sink.pending_lines(&state, 80).is_empty());
        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        let primary_resumed = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            primary_resumed.contains("primary resumed"),
            "{primary_resumed}"
        );
    }

    #[test]
    fn transcript_sink_keeps_alternating_primary_activity_out_of_subagent_result_fragments() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();
        let first = "SUB-FIRST ".repeat(30);
        let second = "SUB-SECOND ".repeat(30);
        let full_result = format!("{first}{second}");

        state.record_user_prompt("forge this".to_string());
        let _ = sink.pending_lines(&state, 20);
        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id: 1,
            resumed: false,
            model: None,
            agent: "codex-acp".to_string(),
            objective: String::new(),
            label: "subagent".to_string(),
        }));
        // The start record is immutable and flushes at once; everything the
        // subagent streams afterwards stays held.
        let started = sink
            .pending_lines(&state, 20)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(started.contains("subagent #1"), "{started}");
        state.apply_event(subagent_session_update(SessionUpdate::AgentMessageChunk(
            text_chunk(&first),
        )));
        assert!(sink.pending_lines(&state, 20).is_empty());

        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
            text_chunk("waiting for the subagent's first result"),
        )));
        assert!(sink.pending_lines(&state, 20).is_empty());
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
            text_chunk("; coordinating the next step"),
        )));
        assert!(sink.pending_lines(&state, 20).is_empty());

        state.apply_event(subagent_session_update(SessionUpdate::AgentMessageChunk(
            text_chunk(&second),
        )));
        assert!(sink.pending_lines(&state, 20).is_empty());
        assert!(matches!(
            state.transcript.as_slice(),
            [Entry::UserPrompt(_), Entry::System(_), Entry::AgentThought(thought)]
                if thought.text
                        == "waiting for the subagent's first result; coordinating the next step"
                    && !thought.completed
        ));
        assert!(matches!(
            state
                .nested_agent(1)
                .expect("nested actor")
                .transcript
                .as_slice(),
            [Entry::SubagentMessage(result)] if result == &full_result
        ));

        state.apply_event(subagent_finished(SubagentOutcome::Completed));
        let subagent = sink
            .pending_lines(&state, 20)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!subagent.contains("SUB-FIRST"), "{subagent}");
        assert!(!subagent.contains("SUB-SECOND"), "{subagent}");
        assert!(!subagent.contains("waiting for the subagent"), "{subagent}");

        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("primary completed coordination."),
        )));
        let primary_thought = sink
            .pending_lines(&state, 20)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            primary_thought.contains("thought · 1 line"),
            "{primary_thought}"
        );
        assert!(!primary_thought.contains("SUB-FIRST"), "{primary_thought}");
        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        let primary = sink
            .pending_lines(&state, 20)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(primary.contains("primary completed"), "{primary}");
        assert!(primary.contains("coordination."), "{primary}");
        assert!(!primary.contains("SUB-FIRST"), "{primary}");
        assert!(!primary.contains("SUB-SECOND"), "{primary}");
    }

    #[test]
    fn transcript_sink_emits_completed_tool_call_during_streaming_turn() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.record_user_prompt("run tests".to_string());
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "cargo test".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::InProgress,
                body: vec![ToolCallOutput::Text("running".to_string())],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let prompt: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(prompt, vec!["❯ run tests", ""]);
        assert!(sink.pending_lines(&state, 80).is_empty());

        let view = state.tool_calls.get_mut("call-1").expect("tool call");
        view.status = ToolCallStatus::Completed;
        view.body = vec![ToolCallOutput::Text("ok".to_string())];

        // The tool content flushes immediately (streaming promptness); its
        // trailing separator is held back until the successor is known so a
        // following tool call could abut it.
        let rendered: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(rendered, vec!["│ tool exec cargo test", "│   ok"]);
        assert!(sink.pending_lines(&state, 80).is_empty());

        // When the turn ends with nothing after the tool call, the held
        // separator is finally emitted.
        state.set_connection_state(ConnectionState::Ready);
        let separator: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(separator, vec![""]);
    }

    #[test]
    fn transcript_sink_waits_for_completed_terminal_exit_snapshot() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.record_user_prompt("run tests".to_string());
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "cargo test".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Terminal {
                    terminal_id: "term-1".to_string(),
                    output: String::new(),
                    truncated: false,
                    exit_status: None,
                }],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let prompt: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(prompt, vec!["❯ run tests", ""]);
        assert!(
            sink.pending_lines(&state, 80).is_empty(),
            "completed terminal tool call must not flush before terminal exit status arrives"
        );

        state.apply_event(UiEvent::TerminalOutput(TerminalOutputSnapshot {
            terminal_id: "term-1".to_string(),
            output: "ok\n".to_string(),
            truncated: false,
            exit_status: Some(TerminalExitStatus::new().exit_code(0)),
        }));

        // Content flushes on the exit snapshot; the trailing separator is
        // held back (streaming) until the successor is known.
        let rendered: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(rendered, vec!["│ tool exec cargo test · exit 0", "│   ok"]);
    }

    #[test]
    fn transcript_sink_does_not_block_after_cancelled_tool_call() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.record_user_prompt("run tests".to_string());
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "cargo test".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::InProgress,
                body: vec![ToolCallOutput::Text("running".to_string())],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let first_prompt: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(first_prompt, vec!["❯ run tests", ""]);

        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::Cancelled,
            usage: None,
        });
        let cancelled_tool: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        let cancelled_tool = cancelled_tool.join("\n");
        assert!(cancelled_tool.contains("tool"), "{cancelled_tool}");
        assert!(cancelled_tool.contains("[failed]"), "{cancelled_tool}");
        assert!(cancelled_tool.contains("cargo test"), "{cancelled_tool}");
        assert!(cancelled_tool.contains("running"), "{cancelled_tool}");

        state.record_user_prompt("next prompt".to_string());
        let next_prompt: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(next_prompt, vec!["❯ next prompt", ""]);
    }

    #[test]
    fn runtime_closed_keeps_transcript_scrolling_active() {
        let mut state = AppState::new();
        state.runtime_closed = true;
        state.scroll_offset = 0;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::PageUp, KeyModifiers::CONTROL),
        );
        assert_eq!(state.scroll_offset, 5);

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::PageDown, KeyModifiers::CONTROL),
        );
        assert_eq!(state.scroll_offset, 0);
        assert!(state.exit_reason.is_none());
    }

    #[test]
    fn mouse_wheel_scrolls_transcript() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, mouse(MouseEventKind::ScrollUp));
        assert_eq!(state.scroll_offset, TRANSCRIPT_SCROLL_WHEEL_STEP);

        handle_crossterm(&mut state, &cmd_tx, mouse(MouseEventKind::ScrollDown));
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn text_selection_mode_ignores_mouse_wheel() {
        let mut state = AppState::new();
        state.text_selection_mode = true;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, mouse(MouseEventKind::ScrollUp));

        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn f12_requests_text_selection_mode_toggle() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let request = handle_crossterm(&mut state, &cmd_tx, key(KeyCode::F(12)));

        assert_eq!(request, TerminalRequest::ToggleTextSelectionMode);
    }

    #[test]
    fn inline_mode_ignores_mouse_wheel_and_f12_selection_toggle() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        super::handle_crossterm(
            &mut state,
            &cmd_tx,
            mouse(MouseEventKind::ScrollUp),
            UiMode::InlineChat,
        );
        assert_eq!(state.scroll_offset, 0);

        let request =
            super::handle_crossterm(&mut state, &cmd_tx, key(KeyCode::F(12)), UiMode::InlineChat);
        assert_eq!(request, TerminalRequest::None);
        assert!(!state.text_selection_mode);
    }

    #[test]
    fn inline_mode_does_not_scroll_transcript_with_keyboard_shortcuts() {
        let mut state = AppState::new();
        state.runtime_closed = true;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        super::handle_crossterm(
            &mut state,
            &cmd_tx,
            key(KeyCode::PageUp),
            UiMode::InlineChat,
        );
        assert_eq!(state.scroll_offset, 0);

        state.runtime_closed = false;
        super::handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Up, KeyModifiers::CONTROL),
            UiMode::InlineChat,
        );
        assert_eq!(state.scroll_offset, 0);
    }

    fn workspace_diff_event(
        diffs: Vec<crate::event::WorkspaceDiff>,
        total_files: usize,
    ) -> crate::event::WorkspaceDiffEvent {
        crate::event::WorkspaceDiffEvent {
            turn_id: 1,
            diffs,
            total_files,
            max_files: 20,
            truncated: false,
        }
    }

    #[test]
    fn fullscreen_workspace_diff_viewer_owns_ctrl_g_navigation_and_prompt_input() {
        let mut state = AppState::new();
        state.runtime_closed = true;
        state.workspace_diffs.push(workspace_diff_event(
            vec![
                crate::event::WorkspaceDiff {
                    path: "first.rs".into(),
                    old_text: Some("old\n".into()),
                    new_text: "new\n".into(),
                },
                crate::event::WorkspaceDiff {
                    path: "second.rs".into(),
                    old_text: Some("before\n".into()),
                    new_text: "after\n".into(),
                },
            ],
            2,
        ));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('g'), KeyModifiers::CONTROL),
        );
        assert!(state.workspace_diff_viewer);
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Home));
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Down));
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::PageDown));
        assert_eq!(
            state.workspace_diff_scroll_offset,
            1 + TRANSCRIPT_SCROLL_PAGE_STEP
        );
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Up));
        assert_eq!(
            state.workspace_diff_scroll_offset,
            TRANSCRIPT_SCROLL_PAGE_STEP
        );
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::End));
        assert_eq!(state.workspace_diff_scroll_offset, usize::MAX);
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('n')));
        assert_eq!(state.workspace_diff_selected_file, 1);
        assert_eq!(state.workspace_diff_scroll_offset, 0);
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('n')));
        assert_eq!(state.workspace_diff_selected_file, 1, "next clamps at end");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('p')));
        assert_eq!(state.workspace_diff_selected_file, 0);
        assert_eq!(state.workspace_diff_scroll_offset, 0);
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('p')));
        assert_eq!(
            state.workspace_diff_selected_file, 0,
            "previous clamps at start"
        );
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('x')));
        assert!(state.input.is_empty());
        handle_crossterm(&mut state, &cmd_tx, mouse(MouseEventKind::ScrollDown));
        assert_eq!(
            state.scroll_offset, 0,
            "diff mouse input must not mutate fullscreen transcript state"
        );
        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('g'), KeyModifiers::CONTROL),
        );
        assert!(!state.workspace_diff_viewer, "Ctrl-G closes the reader");
        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('g'), KeyModifiers::CONTROL),
        );
        assert!(
            state.workspace_diff_viewer,
            "Ctrl-G reopens after runtime closure"
        );
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));
        assert!(!state.workspace_diff_viewer);
        assert!(
            state.exit_reason.is_none(),
            "Esc closes the reader before runtime-close quit handling"
        );
    }

    #[test]
    fn inline_workspace_diff_viewer_handles_keys_and_ignores_mouse() {
        let mut state = AppState::new();
        state.workspace_diffs.push(workspace_diff_event(
            vec![
                crate::event::WorkspaceDiff {
                    path: "one.rs".into(),
                    old_text: Some("one\n".into()),
                    new_text: "ONE\n".into(),
                },
                crate::event::WorkspaceDiff {
                    path: "two.rs".into(),
                    old_text: Some("two\n".into()),
                    new_text: "TWO\n".into(),
                },
            ],
            2,
        ));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        handle_inline_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('g'), KeyModifiers::CONTROL),
        );
        assert!(state.workspace_diff_viewer);
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::PageUp));
        assert_eq!(
            state.workspace_diff_scroll_offset, 0,
            "page up clamps at top"
        );
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::PageDown));
        assert_eq!(
            state.workspace_diff_scroll_offset,
            TRANSCRIPT_SCROLL_PAGE_STEP
        );
        handle_inline_crossterm(&mut state, &cmd_tx, mouse(MouseEventKind::ScrollDown));
        assert_eq!(
            state.workspace_diff_scroll_offset,
            TRANSCRIPT_SCROLL_PAGE_STEP
        );
        assert_eq!(
            state.scroll_offset, 0,
            "diff mouse input must not mutate transcript state"
        );
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('n')));
        assert_eq!(state.workspace_diff_selected_file, 1);
        assert_eq!(state.workspace_diff_scroll_offset, 0);
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('x')));
        assert!(state.input.is_empty());
        assert!(!should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Paste("hidden prompt text".to_string())
        ));
        handle_inline_crossterm(
            &mut state,
            &cmd_tx,
            CtEvent::Paste("hidden prompt text".to_string()),
        );
        assert!(
            state.input.is_empty(),
            "paste must not leak into the prompt"
        );
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));
        assert!(!state.workspace_diff_viewer);
    }

    #[test]
    fn workspace_diff_shortcut_yields_to_existing_overlay_owners() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let ctrl_g = || key_with_modifiers(KeyCode::Char('g'), KeyModifiers::CONTROL);
        let mut state = AppState::new();

        state.agent_picker = Some(crate::app::AgentPicker {
            selected: 0,
            role_indices: Vec::new(),
            effort_selected: 0,
            step: AgentPickerStep::Model,
            start_new_session: true,
        });
        handle_crossterm(&mut state, &cmd_tx, ctrl_g());
        assert!(!state.workspace_diff_viewer);

        state.agent_picker = None;
        let (abort_tx, _abort_rx) = tokio::sync::watch::channel(false);
        let (proceed_tx, _proceed_rx) = tokio::sync::watch::channel(false);
        state.ragnarok = Some(RagnarokUi::new("task".into(), abort_tx, proceed_tx));
        handle_crossterm(&mut state, &cmd_tx, ctrl_g());
        assert!(!state.workspace_diff_viewer);
    }

    #[test]
    fn workspace_diff_viewer_renders_title_navigation_and_diff_colors() {
        let mut state = AppState::new();
        state.workspace_diffs.push(workspace_diff_event(
            vec![
                crate::event::WorkspaceDiff {
                    path: "first.rs".into(),
                    old_text: Some("old token\n".into()),
                    new_text: "new token\n".into(),
                },
                crate::event::WorkspaceDiff {
                    path: "second.rs".into(),
                    old_text: Some("before\n".into()),
                    new_text: "after marker\n".into(),
                },
            ],
            2,
        ));
        state.open_workspace_diff_viewer();
        let backend = TestBackend::new(100, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_workspace_diff_viewer(frame, frame.area(), &mut state, false))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let rendered = buffer_lines(buffer).join("\n");
        assert!(rendered.contains("2 files changed"), "{rendered}");
        assert!(rendered.contains("retained 1/2 first.rs"), "{rendered}");
        assert!(
            rendered.contains("old token") && rendered.contains("new token"),
            "{rendered}"
        );
        let mut cells = (0..buffer.area().height).flat_map(|y| {
            (0..buffer.area().width).map(move |x| buffer.cell((x, y)).expect("cell"))
        });
        assert!(cells.clone().any(|cell| cell.symbol() == "-"
            && cell.style().bg == Some(state.theme.diff_removed_bg.expect("removed background"))));
        assert!(cells.any(|cell| cell.symbol() == "+"
            && cell.style().bg == Some(state.theme.diff_added_bg.expect("added background"))));

        state.select_workspace_diff_file(true);
        terminal
            .draw(|frame| draw_workspace_diff_viewer(frame, frame.area(), &mut state, false))
            .expect("draw");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("retained 2/2 second.rs"), "{rendered}");
        assert!(
            rendered.contains("after marker") && !rendered.contains("new token"),
            "{rendered}"
        );
    }

    #[test]
    fn workspace_diff_viewer_explains_capped_and_empty_events_without_panicking_when_narrow() {
        let mut state = AppState::new();
        state
            .workspace_diffs
            .push(crate::event::WorkspaceDiffEvent {
                turn_id: 1,
                diffs: vec![crate::event::WorkspaceDiff {
                    path: "kept.rs".into(),
                    old_text: None,
                    new_text: "kept content\n".into(),
                }],
                total_files: 4,
                max_files: 1,
                truncated: true,
            });
        state.open_workspace_diff_viewer();
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).expect("terminal");
        terminal
            .draw(|frame| draw_workspace_diff_viewer(frame, frame.area(), &mut state, false))
            .expect("draw");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("4 files changed")
                && rendered.contains("retained 1/1")
                && rendered.contains("showing 1 of 4"),
            "{rendered}"
        );

        state.workspace_diffs.push(workspace_diff_event(vec![], 3));
        terminal
            .draw(|frame| draw_workspace_diff_viewer(frame, frame.area(), &mut state, false))
            .expect("draw");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("3 files changed")
                && rendered.contains("No retained diff is available"),
            "{rendered}"
        );

        state.workspace_diffs.push(workspace_diff_event(vec![], 0));
        terminal
            .draw(|frame| draw_workspace_diff_viewer(frame, frame.area(), &mut state, false))
            .expect("draw");
        assert!(
            buffer_lines(terminal.backend().buffer())
                .join("\n")
                .contains("No workspace changes were captured")
        );

        let mut narrow = Terminal::new(TestBackend::new(2, 2)).expect("terminal");
        narrow
            .draw(|frame| draw_workspace_diff_viewer(frame, frame.area(), &mut state, false))
            .expect("narrow draw");
    }

    #[test]
    fn workspace_diff_viewer_wraps_long_lines_and_complete_footer_when_narrow() {
        let long_line =
            "release-build-long-diff-line-that-must-remain-readable-through-the-final-token";
        let mut state = AppState::new();
        state.workspace_diffs.push(workspace_diff_event(
            vec![crate::event::WorkspaceDiff {
                path: "long.txt".into(),
                old_text: None,
                new_text: format!("{long_line}\n"),
            }],
            1,
        ));
        state.open_workspace_diff_viewer();

        let mut terminal = Terminal::new(TestBackend::new(68, 20)).expect("terminal");
        terminal
            .draw(|frame| draw_workspace_diff_viewer(frame, frame.area(), &mut state, false))
            .expect("draw");
        let rendered = buffer_lines(terminal.backend().buffer());
        let joined = rendered.join("");
        let body_text = rendered
            .iter()
            .map(|line| line.trim_matches(['│', ' ']))
            .collect::<String>();

        assert!(body_text.contains(long_line), "rendered: {rendered:?}");
        assert!(!joined.contains("..."), "rendered: {rendered:?}");
        assert!(joined.contains("n/p"), "rendered: {rendered:?}");
        assert!(
            joined.contains("previous/next file"),
            "rendered: {rendered:?}"
        );
    }

    #[test]
    fn workspace_diff_viewer_uses_full_inline_height_and_fullscreen_transcript_pane() {
        let mut state = AppState::new();
        state.workspace_diffs.push(workspace_diff_event(
            vec![crate::event::WorkspaceDiff {
                path: "pane.rs".into(),
                old_text: None,
                new_text: "pane content\n".into(),
            }],
            1,
        ));
        state.open_workspace_diff_viewer();
        assert_eq!(
            desired_inline_height(
                &state,
                Size {
                    width: 100,
                    height: 40
                }
            ),
            39
        );
        let mut inline = Terminal::new(TestBackend::new(100, 40)).expect("terminal");
        inline
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("inline draw");
        assert!(
            buffer_lines(inline.backend().buffer())
                .join("\n")
                .contains("pane content")
        );

        let mut fullscreen = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        let mut scroll = TranscriptScrollState::default();
        fullscreen
            .draw(|frame| draw(frame, &mut state, &mut scroll, UiMode::FullscreenTui))
            .expect("fullscreen draw");
        let rendered = buffer_lines(fullscreen.backend().buffer()).join("\n");
        assert!(
            rendered.contains("pane content") && rendered.contains("Ctrl-G/Esc close"),
            "{rendered}"
        );
        assert!(
            rendered.contains("mj") || rendered.contains("You"),
            "fullscreen retains its non-transcript chrome: {rendered}"
        );
    }

    #[test]
    fn both_ui_modes_reserve_the_same_workflow_progress_area_below_the_header() {
        let mut state = AppState::new();
        let baseline = desired_inline_height(
            &state,
            Size {
                width: 100,
                height: 40,
            },
        );
        start_workflow(
            &mut state,
            WorkflowId::delegation(3),
            WorkflowKind::Delegation,
            WorkflowPhase::Delegating,
        );
        assert_eq!(
            desired_inline_height(
                &state,
                Size {
                    width: 100,
                    height: 40
                }
            ),
            baseline + 1,
            "the inline viewport grows by exactly one workflow row"
        );

        let mut inline = Terminal::new(TestBackend::new(100, 40)).expect("terminal");
        inline
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("inline draw");
        assert!(
            buffer_lines(inline.backend().buffer())
                .join("\n")
                .contains("Subagents"),
            "inline mode must render workflow progress"
        );

        let mut fullscreen = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        let mut scroll = TranscriptScrollState::default();
        fullscreen
            .draw(|frame| draw(frame, &mut state, &mut scroll, UiMode::FullscreenTui))
            .expect("fullscreen draw");
        let rendered = buffer_lines(fullscreen.backend().buffer());
        let row = rendered
            .iter()
            .position(|line| line.contains("Subagents"))
            .expect("fullscreen mode must render workflow progress");
        let header = rendered
            .iter()
            .position(|line| line.contains(&mjolnir_version_label()))
            .expect("header row");
        assert!(
            row > header,
            "the workflow area sits below the header: {rendered:?}"
        );
    }

    #[test]
    fn f12_ignores_text_selection_toggle_while_overlay_owns_input() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let mut help_state = AppState::new();
        help_state.help_overlay = true;
        assert_eq!(
            handle_crossterm(&mut help_state, &cmd_tx, key(KeyCode::F(12))),
            TerminalRequest::None
        );
        assert!(help_state.help_overlay);

        let mut permission_state = AppState::new();
        let pending = permission_pending_with_options("run shell command", &["Allow", "Reject"], 0);
        permission_state.apply_event(UiEvent::PermissionRequest(pending.prompt));
        assert_eq!(
            handle_crossterm(&mut permission_state, &cmd_tx, key(KeyCode::F(12))),
            TerminalRequest::None
        );
        assert!(permission_state.has_pending_permission());

        let mut config_state = AppState::new();
        config_state.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "model-1",
            vec![
                SessionConfigSelectOption::new("model-1", "Model 1"),
                SessionConfigSelectOption::new("model-2", "Model 2"),
            ],
        )];
        assert!(config_state.open_config_value_picker(0));
        assert_eq!(
            handle_crossterm(&mut config_state, &cmd_tx, key(KeyCode::F(12))),
            TerminalRequest::None
        );
        assert!(config_state.config_picker.is_some());
    }

    #[test]
    fn exit_reset_reenables_mouse_capture_after_text_selection_mode() {
        let mut state = AppState::new();
        state.text_selection_mode = true;
        let mut calls = Vec::new();

        reset_text_selection_mode_for_exit(&mut state, |enabled| {
            calls.push(enabled);
            Ok(())
        })
        .expect("reset text selection mode");

        assert_eq!(calls, vec![true]);
        assert!(!state.text_selection_mode);
    }

    #[test]
    fn exit_reset_leaves_mouse_capture_unchanged_when_not_selecting_text() {
        let mut state = AppState::new();
        let mut calls = Vec::new();

        reset_text_selection_mode_for_exit(&mut state, |enabled| {
            calls.push(enabled);
            Ok(())
        })
        .expect("reset text selection mode");

        assert!(calls.is_empty());
        assert!(!state.text_selection_mode);
    }

    #[test]
    fn ctrl_arrow_keys_scroll_transcript_one_line() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Up, KeyModifiers::CONTROL),
        );
        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Up, KeyModifiers::CONTROL),
        );
        assert_eq!(state.scroll_offset, 2);

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Down, KeyModifiers::CONTROL),
        );
        assert_eq!(state.scroll_offset, 1);
    }

    #[test]
    fn ctrl_home_jumps_to_top_and_ctrl_end_re_attaches_to_stream() {
        let mut state = AppState::new();
        state.scroll_offset = 12;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Home, KeyModifiers::CONTROL),
        );
        // `usize::MAX` is the sentinel that `reconcile` clamps to the top
        // of the actual transcript on the next draw.
        assert_eq!(state.scroll_offset, usize::MAX);

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::End, KeyModifiers::CONTROL),
        );
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn ctrl_t_toggles_tool_output_expansion() {
        let mut state = AppState::new();
        assert!(!state.expand_transcript_details);
        let starting_revision = state.transcript_revision();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('t'), KeyModifiers::CONTROL),
        );

        assert!(state.expand_transcript_details);
        assert_ne!(
            state.transcript_revision(),
            starting_revision,
            "toggle must bump revision so the renderer cache is invalidated"
        );
        // 't' character must not leak into the input buffer.
        assert!(state.input.is_empty());

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('t'), KeyModifiers::CONTROL),
        );
        assert!(!state.expand_transcript_details);
    }

    #[test]
    fn ctrl_shift_t_also_toggles_tool_output_expansion() {
        let mut state = AppState::new();
        assert!(!state.expand_transcript_details);
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(
                KeyCode::Char('T'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
        );

        assert!(state.expand_transcript_details);
        assert!(state.input.is_empty());
    }

    #[test]
    fn inline_ctrl_t_opens_transcript_reader_instead_of_toggling() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_inline_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('t'), KeyModifiers::CONTROL),
        );

        assert!(state.transcript_viewer, "inline Ctrl-T opens the reader");
        assert!(
            !state.expand_transcript_details,
            "inline Ctrl-T must not flip the collapse setting"
        );
        assert!(state.input.is_empty(), "'t' must not leak into the prompt");

        // While open, Ctrl-T closes the reader again.
        handle_inline_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('t'), KeyModifiers::CONTROL),
        );
        assert!(!state.transcript_viewer);
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn transcript_reader_scrolls_with_arrows_and_closes_on_esc() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        state.open_transcript_viewer();
        assert_eq!(
            state.scroll_offset,
            usize::MAX,
            "reader opens at the bottom"
        );

        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Home));
        assert_eq!(state.scroll_offset, 0);
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Down));
        assert_eq!(state.scroll_offset, 1);
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::PageDown));
        assert_eq!(state.scroll_offset, 1 + TRANSCRIPT_SCROLL_PAGE_STEP);
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Up));
        assert_eq!(state.scroll_offset, TRANSCRIPT_SCROLL_PAGE_STEP);
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::End));
        assert_eq!(state.scroll_offset, usize::MAX);
        // Typing while the reader owns the keyboard must not edit the prompt.
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('a')));
        assert!(state.input.is_empty());

        let request = handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));
        assert!(!state.transcript_viewer);
        assert_eq!(state.scroll_offset, 0);
        assert!(
            terminal_request_forces_inline_repair(&request),
            "closing the reader must repair the shrunken inline viewport"
        );
    }

    #[test]
    fn transcript_reader_scrolls_with_mouse_wheel() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        state.open_transcript_viewer();
        state.scroll_offset = 10;

        handle_inline_crossterm(&mut state, &cmd_tx, mouse(MouseEventKind::ScrollUp));
        assert_eq!(state.scroll_offset, 10 - TRANSCRIPT_SCROLL_WHEEL_STEP);

        handle_inline_crossterm(&mut state, &cmd_tx, mouse(MouseEventKind::ScrollDown));
        assert_eq!(state.scroll_offset, 10);
    }

    #[test]
    fn transcript_reader_mouse_wheel_pauses_for_permission_modal() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        state.open_transcript_viewer();
        state.scroll_offset = 10;
        let pending = permission_pending_with_options("run command", &["allow"], 0);
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));

        handle_inline_crossterm(&mut state, &cmd_tx, mouse(MouseEventKind::ScrollUp));

        assert_eq!(state.scroll_offset, 10);
    }

    #[test]
    fn transcript_reader_renders_collapsed_tool_output_in_full() {
        let mut state = AppState::new();
        let long = (1..=20)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "log".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Text(long)],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));
        // The session is still in collapsed mode...
        assert!(!state.expand_transcript_details);
        state.open_transcript_viewer();

        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        // ...yet the reader shows every line, with no truncation hint.
        assert!(rendered.contains("line 1"), "rendered:\n{rendered}");
        assert!(rendered.contains("line 20"), "rendered:\n{rendered}");
        assert!(
            !rendered.contains("lines hidden"),
            "reader must not collapse output, rendered:\n{rendered}"
        );
        assert!(rendered.contains("transcript"), "rendered:\n{rendered}");
        assert!(rendered.contains("Esc"), "rendered:\n{rendered}");
    }

    #[test]
    fn transcript_reader_requests_full_inline_height() {
        let mut state = AppState::new();
        state.open_transcript_viewer();

        let desired = desired_inline_height(
            &state,
            Size {
                width: 100,
                height: 40,
            },
        );
        assert_eq!(desired, 39, "reader takes the whole terminal minus one row");
        assert!(desired > INLINE_EXPANDED_MAX_HEIGHT);
    }

    #[test]
    fn system_status_messages_use_visible_transcript_color() {
        let mut state = AppState::new();
        state.record_status_message(
            StatusKind::Info,
            "transcript exported to /tmp/mjolnir/transcript.md",
        );

        let rendered = render_transcript_lines(&state, 80);
        let system_line = rendered
            .iter()
            .find(|line| line_text(line).contains("transcript exported to"))
            .expect("export status line rendered");

        assert_eq!(system_line.spans[0].style.fg, Some(Color::LightBlue));
    }

    #[test]
    fn stable_agent_answers_never_collapse() {
        let mut state = AppState::new();
        let long = (1..=7)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.transcript.extend([
            Entry::UserPrompt(long.clone()),
            Entry::AgentMessage(long.clone()),
            Entry::SubagentMessage(long.clone()),
            Entry::System(long),
        ]);

        let rendered = render_transcript_lines(&state, 100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert_eq!(
            rendered
                .iter()
                .filter(|line| line.trim() == "… details hidden")
                .count(),
            2,
            "rendered: {rendered:?}"
        );
        assert_eq!(
            rendered
                .iter()
                .filter(|line| line.trim() == "line 7")
                .count(),
            2,
            "primary and subagent answer tails must remain visible: {rendered:?}"
        );
    }

    #[test]
    fn message_collapse_thresholds_are_unicode_safe_and_preserve_markdown() {
        let exact_chars = "λ".repeat(MESSAGE_COLLAPSED_CHARS);
        assert_eq!(message_preview(&exact_chars, true), (exact_chars, false));

        let over_chars = format!("**important** {}TAIL", "🦀".repeat(MESSAGE_COLLAPSED_CHARS));
        let (preview, collapsed) = message_preview(&over_chars, true);
        assert!(collapsed);
        assert_eq!(preview.chars().count(), MESSAGE_COLLAPSED_CHARS);
        assert!(!preview.contains("TAIL"));

        let mut state = AppState::new();
        state.transcript.push(Entry::AgentMessage(over_chars));
        let rendered = render_transcript_lines(&state, 100);
        let content = rendered
            .iter()
            .find(|line| line_text(line).starts_with("● important"))
            .expect("markdown preview");
        assert!(
            content
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD))
        );

        let six_lines = (1..=MESSAGE_COLLAPSED_LINES)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!message_preview(&six_lines, true).1);
        assert!(message_preview(&format!("{six_lines}\nline 7"), true).1);
    }

    #[test]
    fn completed_final_response_stays_expanded() {
        let mut state = AppState::new();
        state.record_user_prompt("start".to_string());
        let long = format!("{}FINAL_RESPONSE_TAIL", "x".repeat(MESSAGE_COLLAPSED_CHARS));
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk(&long),
        )));

        let streaming = render_transcript_lines(&state, 100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(
            streaming
                .iter()
                .any(|line| line.contains("FINAL_RESPONSE_TAIL"))
        );
        assert!(!streaming.iter().any(|line| line.contains("details hidden")));

        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        let stable = render_transcript_lines(&state, 100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(
            stable
                .iter()
                .any(|line| line.contains("FINAL_RESPONSE_TAIL")),
            "final answer must remain fully readable: {stable:?}"
        );
        assert!(!stable.iter().any(|line| line.contains("details hidden")));
    }

    #[test]
    fn nested_viewer_and_full_export_reveal_complete_internal_message() {
        let mut state = AppState::new();
        let full = format!("{}INTERNAL_EXACT_SUFFIX", "brief ".repeat(150));
        state.apply_event(UiEvent::InternalMessage(crate::event::InternalMessage {
            source: "primary".to_string(),
            target: "subagent".to_string(),
            kind: crate::event::InternalMessageKind::Delegation,
            text: full.clone(),
            owner_subagent_id: Some(1),
        }));

        let primary = render_transcript_lines(&state, 100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(
            !primary
                .iter()
                .any(|line| line.contains("INTERNAL_EXACT_SUFFIX"))
        );

        let nested =
            render_nested_agent_lines(&state, state.nested_agent(1).expect("nested actor"), 100)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>();
        assert!(
            nested
                .iter()
                .any(|line| line.contains("INTERNAL_EXACT_SUFFIX"))
        );

        let primary_export = transcript_export_markdown(&state);
        assert!(!primary_export.contains("INTERNAL\\_EXACT\\_SUFFIX"));
        let full_export = transcript_export_markdown_with_nested(&state, true);
        assert!(full_export.contains("## primary → subagent delegation"));
        assert!(full_export.contains("INTERNAL\\_EXACT\\_SUFFIX"));
    }

    #[test]
    fn tool_output_collapses_long_text_with_hint_by_default() {
        let mut state = AppState::new();
        let long = (1..=20)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "log".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Text(long)],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let rendered: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();

        // The last TOOL_OUTPUT_COLLAPSED_LINES lines are visible (framed by
        // the tool gutter) — the tail is where errors and summaries live.
        let hidden = 20 - TOOL_OUTPUT_COLLAPSED_LINES;
        assert!(
            rendered
                .iter()
                .any(|line| line == &format!("│   line {}", hidden + 1))
        );
        assert!(rendered.iter().any(|line| line == "│   line 20"));
        // Everything before the tail is hidden.
        assert!(
            !rendered
                .iter()
                .any(|line| line == &format!("│   line {hidden}"))
        );
        // And a leading hint tells the user the head was elided.
        assert!(
            rendered
                .iter()
                .any(|line| line == &format!("│   ... {hidden} earlier lines hidden")),
            "missing collapse hint, got: {rendered:?}"
        );

        // After expanding, every line is rendered and the hint disappears.
        state.expand_transcript_details = true;
        let expanded: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert!(expanded.iter().any(|line| line == "│   line 1"));
        assert!(expanded.iter().any(|line| line == "│   line 20"));
        assert!(!expanded.iter().any(|line| line.contains("lines hidden")));
    }

    #[test]
    fn tool_output_collapses_a_single_huge_logical_line_by_character_count() {
        let unicode = format!("{}SUFFIX", "é".repeat(700));
        let (unicode_preview, hidden) =
            tool_output_preview(&unicode, Some(TOOL_OUTPUT_COLLAPSED_LINES));
        assert_eq!(unicode_preview.chars().count(), TOOL_OUTPUT_COLLAPSED_CHARS);
        assert_eq!(hidden, Some(ToolOutputHidden::Details));
        assert!(!unicode_preview.contains("SUFFIX"));

        let mut state = AppState::new();
        let long = format!("{{\"body\":\"{}ONE_LINE_SUFFIX\"}}", "x".repeat(900));
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "gh issue view 350".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Terminal {
                    terminal_id: "term-1".to_string(),
                    output: long,
                    truncated: false,
                    exit_status: Some(TerminalExitStatus::new().exit_code(0)),
                }],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let collapsed = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(
            !collapsed
                .iter()
                .any(|line| line.contains("ONE_LINE_SUFFIX"))
        );
        assert!(collapsed.iter().any(|line| line.contains("details hidden")));
        assert!(
            !collapsed
                .iter()
                .any(|line| line.contains("terminal output"))
        );

        state.expand_transcript_details = true;
        let expanded_lines = render_transcript_lines(&state, 80);
        let expanded = expanded_lines.iter().collect::<Vec<_>>();
        let expanded_tool_content = expanded
            .iter()
            .filter(|line| line_text(line).starts_with(TOOL_GUTTER))
            .flat_map(|line| line.spans.iter().skip(1))
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(
            expanded_tool_content.contains("ONE") && expanded_tool_content.contains("_LINE_SUFFIX"),
            "expanded tool content: {expanded_tool_content:?}"
        );
        assert!(
            !expanded
                .iter()
                .map(|line| line_text(line))
                .any(|line| line.contains("details hidden"))
        );
    }

    #[test]
    fn transcript_block_title_surfaces_scroll_and_expand_state() {
        let mut state = AppState::new();
        assert_eq!(transcript_block_title(&state), " transcript ");

        state.scroll_offset = 7;
        assert!(transcript_block_title(&state).contains("[scrolled +7"));
        assert!(transcript_block_title(&state).contains("End to follow"));

        state.scroll_offset = 0;
        state.expand_transcript_details = true;
        assert!(transcript_block_title(&state).contains("details: expanded"));
    }

    #[test]
    fn input_title_includes_text_selection_shortcut() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        let backend = TestBackend::new(180, 5);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_input(frame, frame.area(), &state, UiMode::FullscreenTui))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("──────────── (Enter send"),
            "rendered:\n{rendered}"
        );
        assert!(rendered.contains("Ctrl-C quit"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("Shift-Tab model/effort"),
            "rendered:\n{rendered}"
        );
        assert!(
            rendered.contains("F12 select text"),
            "rendered:\n{rendered}"
        );
        assert!(!rendered.contains("prompt"), "rendered:\n{rendered}");
        assert!(!rendered.contains("ready"), "rendered:\n{rendered}");
        assert!(!rendered.contains("streaming"), "rendered:\n{rendered}");
        assert!(!rendered.contains("elapsed"), "rendered:\n{rendered}");

        state.text_selection_mode = true;
        terminal
            .draw(|frame| draw_input(frame, frame.area(), &state, UiMode::FullscreenTui))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("F12 resume wheel"),
            "rendered:\n{rendered}"
        );
    }

    #[test]
    fn input_box_omits_side_borders() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        let backend = TestBackend::new(80, 5);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_input(frame, frame.area(), &state, UiMode::InlineChat))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer());
        assert!(
            rendered.first().is_some_and(|line| line.contains('─')),
            "top border missing:\n{}",
            rendered.join("\n")
        );
        assert!(
            rendered.last().is_some_and(|line| line.contains('─')),
            "bottom border missing:\n{}",
            rendered.join("\n")
        );
        for line in &rendered[1..rendered.len() - 1] {
            assert!(!line.starts_with('│'), "left border rendered: {line:?}");
            assert!(!line.ends_with('│'), "right border rendered: {line:?}");
        }
    }

    #[test]
    fn inline_input_title_omits_text_selection_shortcut() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        let backend = TestBackend::new(140, 5);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_input(frame, frame.area(), &state, UiMode::InlineChat))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("──────────── (Enter send"),
            "rendered:\n{rendered}"
        );
        assert!(rendered.contains("Ctrl-C quit"), "rendered:\n{rendered}");
        assert!(rendered.contains("F10 help"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("Shift-Tab model/effort"),
            "rendered:\n{rendered}"
        );
        assert!(!rendered.contains("F12"), "rendered:\n{rendered}");
        assert!(!rendered.contains("prompt"), "rendered:\n{rendered}");
        assert!(!rendered.contains("ready"), "rendered:\n{rendered}");
        assert!(!rendered.contains("streaming"), "rendered:\n{rendered}");
        assert!(!rendered.contains("elapsed"), "rendered:\n{rendered}");

        state.record_user_prompt("hello".to_string());
        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        terminal
            .draw(|frame| draw_input(frame, frame.area(), &state, UiMode::InlineChat))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("──────────── 0s (Enter send"),
            "rendered:\n{rendered}"
        );
        assert!(!rendered.contains("prompt"), "rendered:\n{rendered}");
        assert!(!rendered.contains("ready"), "rendered:\n{rendered}");
        assert!(!rendered.contains("streaming"), "rendered:\n{rendered}");
        assert!(!rendered.contains("elapsed"), "rendered:\n{rendered}");
    }

    #[test]
    fn busy_input_title_uses_activity_ornament_without_status_words() {
        let mut state = AppState::new();
        let backend = TestBackend::new(120, 5);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_input(frame, frame.area(), &state, UiMode::InlineChat))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            contains_prompt_activity_frame(&rendered),
            "rendered:\n{rendered}"
        );
        assert!(rendered.contains("0s"), "rendered:\n{rendered}");
        assert!(!rendered.contains("launching"), "rendered:\n{rendered}");
        assert!(!rendered.contains("prompt ("), "rendered:\n{rendered}");
        assert!(!rendered.contains("elapsed"), "rendered:\n{rendered}");

        state.record_user_prompt("hello".to_string());
        terminal
            .draw(|frame| draw_input(frame, frame.area(), &state, UiMode::InlineChat))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            contains_prompt_activity_frame(&rendered),
            "rendered:\n{rendered}"
        );
        assert!(rendered.contains("0s"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("Ctrl-C/Esc cancel current"),
            "rendered:\n{rendered}"
        );
        assert!(!rendered.contains("streaming"), "rendered:\n{rendered}");
        assert!(!rendered.contains("prompt ("), "rendered:\n{rendered}");
        assert!(!rendered.contains("elapsed"), "rendered:\n{rendered}");
    }

    #[test]
    fn prompt_activity_ornament_uses_selected_style() {
        // Per-style frame width and loop length are covered in `spinner`'s own
        // tests; here we verify the ornament wiring picks the active style and
        // switches between its idle and animation frames with connection state.
        for style in SpinnerStyle::ALL {
            let mut state = AppState::new();
            state.set_spinner_style(style);

            state.set_connection_state(ConnectionState::Ready);
            assert_eq!(
                prompt_activity_ornament(&state),
                style.idle_frame(),
                "{style} idle ornament"
            );

            state.set_connection_state(ConnectionState::Streaming);
            let busy = prompt_activity_ornament(&state);
            assert!(
                style.frames().iter().any(|frame| frame == busy),
                "{style} busy ornament {busy:?} is not one of its frames"
            );
        }
    }

    #[test]
    fn busy_prompt_title_preserves_cancelling_forking_and_queue_affordances() {
        let mut state = AppState::new();

        state.set_connection_state(ConnectionState::Cancelling);
        let cancelling = busy_prompt_title(&state).expect("cancelling title");
        assert!(contains_prompt_activity_frame(&cancelling), "{cancelling}");
        assert!(cancelling.contains("Enter queue next"), "{cancelling}");
        assert!(
            cancelling.contains("Ctrl-C/Esc cancel current"),
            "{cancelling}"
        );
        assert!(!cancelling.contains("cancelling"), "{cancelling}");
        assert!(!cancelling.contains("streaming"), "{cancelling}");
        assert!(!cancelling.contains("prompt"), "{cancelling}");

        state.push_queued_prompt(QueuedPrompt {
            text: "next".to_string(),
            images: Vec::new(),
            display_text: "next".to_string(),
        });
        let queued = busy_prompt_title(&state).expect("queued title");
        assert!(queued.contains("1 queued"), "{queued}");
        assert!(queued.contains("Ctrl-C/Esc cancel current"), "{queued}");

        state.set_connection_state(ConnectionState::Forking);
        let forking = busy_prompt_title(&state).expect("forking title");
        assert!(contains_prompt_activity_frame(&forking), "{forking}");
        assert!(forking.contains("1 queued"), "{forking}");
        assert!(forking.contains("Enter queue next"), "{forking}");
        assert!(!forking.contains("Ctrl-C/Esc cancel current"), "{forking}");
        assert!(!forking.contains("forking"), "{forking}");
        assert!(!forking.contains("prompt"), "{forking}");
    }

    #[test]
    fn header_omits_connection_status() {
        let mut state = AppState::new();
        let backend = TestBackend::new(140, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");

        state.set_connection_state(ConnectionState::Ready);
        terminal
            .draw(|frame| draw_header(frame, frame.area(), &state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(!rendered.contains("ready"), "rendered:\n{rendered}");
        assert!(!rendered.contains("elapsed"), "rendered:\n{rendered}");
        assert!(
            rendered.contains(&mjolnir_version_label()),
            "rendered:\n{rendered}"
        );

        state.set_connection_state(ConnectionState::Streaming);
        terminal
            .draw(|frame| draw_header(frame, frame.area(), &state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(!rendered.contains("streaming"), "rendered:\n{rendered}");
        assert!(!rendered.contains("elapsed"), "rendered:\n{rendered}");
        assert!(
            rendered.contains(&mjolnir_version_label()),
            "rendered:\n{rendered}"
        );
    }

    #[test]
    fn inline_help_overlay_expands_viewport_and_renders() {
        let mut state = AppState::new();
        state.help_overlay = true;

        let desired = desired_inline_height(
            &state,
            Size {
                width: 100,
                height: 40,
            },
        );
        assert!(
            desired > INLINE_CHAT_HEIGHT,
            "help overlay must request enough inline rows to render"
        );

        let backend = TestBackend::new(100, desired);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("help"), "rendered:\n{rendered}");
        assert!(rendered.contains("Agent seats"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("automatic review"),
            "rendered:\n{rendered}"
        );
    }

    #[test]
    fn inline_chat_replaces_content_with_permission_view() {
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.agent_label = "anvil".to_string();
        state.record_user_prompt("hello".to_string());
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));
        let backend = TestBackend::new(100, INLINE_CHAT_HEIGHT);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("permission request"),
            "rendered:\n{rendered}"
        );
        assert!(
            rendered.contains("run shell command"),
            "rendered:\n{rendered}"
        );
        assert!(rendered.contains("Allow once"), "rendered:\n{rendered}");
        assert!(
            !rendered.contains("agent anvil"),
            "permission view must replace the chat header; rendered:\n{rendered}"
        );
        assert!(
            !rendered.contains("prompt ("),
            "permission view must replace the prompt editor; rendered:\n{rendered}"
        );
    }

    #[test]
    fn inline_permission_view_handles_keyboard_selection() {
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Down));

        let pending = state.pending_permission().expect("pending permission");
        assert_eq!(pending.selected, 1);
    }

    #[test]
    fn permission_prompt_keeps_keyboard_priority_over_help_overlay() {
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.help_overlay = true;
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Down));

        let pending = state.pending_permission().expect("pending permission");
        assert_eq!(pending.selected, 1);
        assert!(
            !state.help_overlay,
            "permission request should dismiss stale help before taking focus"
        );
    }

    #[test]
    fn permission_modal_renders_all_short_options() {
        let pending = permission_pending_with_options(
            "run shell command",
            &["Allow once", "Allow always", "Reject"],
            0,
        );
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| {
                draw_permission_modal(
                    frame,
                    frame.area(),
                    &pending,
                    1,
                    TerminalThemeKind::default().palette(),
                )
            })
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        for expected in ["Allow once", "Allow always", "Reject", "Enter to confirm"] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}; rendered:\n{rendered}"
            );
        }
        assert!(
            !rendered.contains("(allow once)"),
            "permission options should not duplicate ACP kind labels; rendered:\n{rendered}"
        );
    }

    #[test]
    fn wrap_text_to_width_preserves_existing_spacing() {
        assert_eq!(
            wrap_text_to_width("  run   command", 80),
            vec!["  run   command"]
        );
        assert_eq!(
            wrap_text_to_width("cmd   --flag", 6),
            vec!["cmd   ", "--flag"]
        );
    }

    #[test]
    fn split_word_to_width_does_not_emit_visual_blank_before_wide_char() {
        assert_eq!(split_word_to_width("界", 1), vec!["界"]);
        assert_eq!(
            split_word_to_width("\u{0301}界x", 1),
            vec!["\u{0301}界", "x"]
        );
    }

    #[test]
    fn permission_modal_wraps_long_options_without_truncating() {
        let pending = permission_pending_with_options(
            "run shell command",
            &[
                "Allow reading the complete destination path before running the deployment command with production credentials",
                "Reject",
            ],
            0,
        );
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| {
                draw_permission_modal(
                    frame,
                    frame.area(),
                    &pending,
                    1,
                    TerminalThemeKind::default().palette(),
                )
            })
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            !rendered.contains("..."),
            "permission text must wrap, not truncate; rendered:\n{rendered}"
        );
        assert!(
            rendered.contains("complete destination path"),
            "missing first wrapped segment; rendered:\n{rendered}"
        );
        assert!(
            rendered.contains("production credentials"),
            "missing final wrapped segment; rendered:\n{rendered}"
        );
    }

    #[test]
    fn permission_modal_expands_literal_newlines_in_prompt_title() {
        let pending = permission_pending_with_options(
            "git checkout\\n--force feature-branch",
            &["Allow once", "Reject"],
            0,
        );
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| {
                draw_permission_modal(
                    frame,
                    frame.area(),
                    &pending,
                    1,
                    TerminalThemeKind::default().palette(),
                )
            })
            .expect("draw");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(
            lines
                .iter()
                .any(|l| l.contains("git checkout") && !l.contains("--force")),
            "first command segment should be on its own terminal row; lines:\n{}",
            lines.join("\n")
        );
        assert!(
            lines.iter().any(|l| l.contains("--force feature-branch")),
            "second command segment should be on its own terminal row; lines:\n{}",
            lines.join("\n")
        );
        assert!(
            !lines.iter().any(|l| l.contains("\\n")),
            "literal backslash-n escape must not appear; lines:\n{}",
            lines.join("\n")
        );
    }

    #[test]
    fn permission_modal_clamps_out_of_bounds_selected_option() {
        let pending = permission_pending_with_options(
            "run shell command",
            &["Allow once", "Allow always", "Reject"],
            99,
        );
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| {
                draw_permission_modal(
                    frame,
                    frame.area(),
                    &pending,
                    1,
                    TerminalThemeKind::default().palette(),
                )
            })
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("> Reject"),
            "clamped selection should be rendered; rendered:\n{rendered}"
        );
    }

    #[test]
    fn fullscreen_permission_modal_renders_above_help_overlay() {
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.help_overlay = true;
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));
        let mut scroll = TranscriptScrollState::default();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw(frame, &mut state, &mut scroll, UiMode::FullscreenTui))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("permission request"),
            "permission modal should remain visible above help overlay:\n{rendered}"
        );
        assert!(
            rendered.contains("run shell command"),
            "permission details should remain visible above help overlay:\n{rendered}"
        );
    }

    #[test]
    fn transcript_renders_markdown_blocks() {
        let mut state = AppState::new();
        state.transcript.push(Entry::AgentMessage(
            "# Result\n- **bold** item\n```rs\nlet x = 1;\n```".to_string(),
        ));

        let rendered: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();

        assert!(rendered.iter().any(|line| line == "● # Result"));
        assert!(rendered.iter().any(|line| line == "  - bold item"));
        assert!(rendered.iter().any(|line| line == "  code rs"));
        assert!(rendered.iter().any(|line| line == "    let x = 1;"));
    }

    #[test]
    fn multiline_system_messages_preserve_logical_lines() {
        let mut state = AppState::new();
        state.transcript.push(Entry::System(
            "Active models\n\nConfigured\n  primary    auto\n  subagents  auto".to_string(),
        ));

        let rendered: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();

        assert_eq!(
            rendered,
            vec![
                "Active models",
                "",
                "Configured",
                "  primary    auto",
                "  subagents  auto",
                "",
            ]
        );
    }

    #[test]
    fn thinking_is_compact_and_actor_glyphs_keep_provenance() {
        let mut state = AppState::new();
        let theme = state.theme;
        state
            .transcript
            .push(Entry::AgentThought(crate::app::ThoughtEntry {
                text: "Planning initial\n\n<!-- -->\n\ncode_agent   invocation".to_string(),
                completed: true,
            }));
        state
            .transcript
            .push(Entry::SubagentThought(crate::app::ThoughtEntry {
                text: "Checking the implementation".to_string(),
                completed: true,
            }));
        let rendered = render_transcript_lines(&state, 80);
        let text = rendered.iter().map(line_text).collect::<Vec<_>>();
        assert_eq!(
            text,
            vec!["○ thought · 5 lines", "", "◇ thought · 1 line", "",]
        );
        assert_eq!(rendered[0].spans[0].style.fg, Some(theme.thought));
        assert_eq!(rendered[2].spans[0].style.fg, Some(theme.secondary));
        assert_eq!(rendered[0].spans[1].style.fg, Some(theme.thought));
        assert_eq!(rendered[2].spans[1].style.fg, Some(theme.thought));
    }

    #[test]
    fn active_thought_uses_bounded_tail_and_completed_thought_expands() {
        let mut state = AppState::new();
        let theme = state.theme;
        state
            .transcript
            .push(Entry::AgentThought(crate::app::ThoughtEntry {
                text: "old one\nold two\nnew one\nnew two\nnew three".to_string(),
                completed: false,
            }));

        let active = render_transcript_lines(&state, 80);
        let active_text = active.iter().map(line_text).collect::<Vec<_>>();
        assert!(!active_text.iter().any(|line| line.contains("old one")));
        assert!(!active_text.iter().any(|line| line.contains("old two")));
        assert!(active_text.iter().any(|line| line == "○ new one"));
        assert!(active_text.iter().any(|line| line == "  new two"));
        assert!(active_text.iter().any(|line| line == "  new three"));

        let tail = active_thought_tail(&format!(
            "{}TAIL",
            "x".repeat(ACTIVE_THOUGHT_TAIL_CHARS + 40)
        ));
        assert!(tail.starts_with('…'));
        assert!(tail.ends_with("TAIL"));
        assert!(tail.chars().count() <= ACTIVE_THOUGHT_TAIL_CHARS + 1);

        let Entry::AgentThought(thought) = &mut state.transcript[0] else {
            panic!("thought entry");
        };
        thought.text = "first line\nsecond line".to_string();
        thought.completed = true;

        let compact = render_transcript_lines(&state, 80);
        assert_eq!(
            compact.iter().map(line_text).collect::<Vec<_>>(),
            vec!["○ thought · 2 lines", ""]
        );

        state.expand_transcript_details = true;
        let expanded = render_transcript_lines(&state, 80);
        assert_eq!(
            expanded.iter().map(line_text).collect::<Vec<_>>(),
            vec!["○ first line", "  second line", ""]
        );
        for line in expanded.iter().take(2) {
            assert!(
                line.spans
                    .iter()
                    .skip(1)
                    .all(|span| span.style.fg == Some(theme.thought))
            );
        }

        state.expand_transcript_details = false;
        assert_eq!(
            render_full_transcript_lines(&state, 80)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>(),
            vec!["○ first line", "  second line", ""]
        );
    }

    #[test]
    fn role_glyphs_mark_each_message_boundary_and_preserve_actor_provenance() {
        let mut state = AppState::new();
        state
            .transcript
            .push(Entry::UserPrompt("build it".to_string()));
        state
            .transcript
            .push(Entry::AgentMessage("delegating".to_string()));
        state.tool_calls.insert(
            "primary-tool".to_string(),
            crate::app::ToolCallView {
                title: "call a subagent".to_string(),
                kind: ToolKind::Other,
                status: ToolCallStatus::Completed,
                body: Vec::new(),
            },
        );
        state
            .transcript
            .push(Entry::ToolCall("primary-tool".to_string()));
        state
            .transcript
            .push(Entry::AgentMessage("handoff accepted".to_string()));
        state
            .transcript
            .push(Entry::SubagentMessage("forging".to_string()));
        state.tool_calls.insert(
            "subagent-tool".to_string(),
            crate::app::ToolCallView {
                title: "edit file".to_string(),
                kind: ToolKind::Edit,
                status: ToolCallStatus::Completed,
                body: Vec::new(),
            },
        );
        state
            .transcript
            .push(Entry::SubagentToolCall("subagent-tool".to_string()));
        state
            .transcript
            .push(Entry::SubagentMessage("finished".to_string()));
        state
            .transcript
            .push(Entry::AgentMessage("here is the result".to_string()));

        let rendered = render_transcript_lines(&state, 80);
        let role_lines = rendered
            .iter()
            .filter(|line| {
                line_text(line).starts_with(USER_GLYPH)
                    || line_text(line).starts_with(AGENT_GLYPH)
                    || line_text(line).starts_with(SUBAGENT_GLYPH)
            })
            .collect::<Vec<_>>();

        assert_eq!(
            role_lines
                .iter()
                .map(|line| line_text(line))
                .collect::<Vec<_>>(),
            vec![
                "❯ build it",
                "● delegating",
                "● handoff accepted",
                "◆ forging",
                "◆ finished",
                "● here is the result",
            ]
        );
        for line in role_lines {
            assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));
        }

        let primary_tool = rendered
            .iter()
            .find(|line| line_text(line) == "│ tool other call a subagent")
            .expect("primary tool header");
        assert_eq!(primary_tool.spans[1].content.as_ref(), "tool ");

        let subagent_tool = rendered
            .iter()
            .find(|line| line_text(line) == "│ subagent tool edit edit file")
            .expect("subagent tool header");
        assert_eq!(subagent_tool.spans[1].content.as_ref(), "subagent ");
        assert_eq!(subagent_tool.spans[1].style.fg, Some(state.theme.secondary));
        assert!(
            subagent_tool.spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    fn command_spans(command: &str) -> Vec<(String, Style)> {
        let theme = TerminalThemeKind::Dark.palette();
        highlight_command(command, theme)
            .into_iter()
            .map(|span| (span.content.into_owned(), span.style))
            .collect()
    }

    fn command_style<'a>(spans: &'a [(String, Style)], token: &str) -> &'a Style {
        &spans
            .iter()
            .find(|(content, _)| content == token)
            .unwrap_or_else(|| panic!("missing command token {token:?}: {spans:?}"))
            .1
    }

    #[test]
    fn execute_tool_headers_restore_command_syntax_colors() {
        let theme = TerminalThemeKind::Dark.palette();
        let spans = command_spans("FOO=bar cargo test --all | grep failed");
        assert_eq!(
            spans
                .iter()
                .map(|(text, _)| text.as_str())
                .collect::<String>(),
            "FOO=bar cargo test --all | grep failed"
        );
        assert_eq!(command_style(&spans, "FOO=bar").fg, Some(theme.text));
        assert_eq!(command_style(&spans, "cargo").fg, Some(theme.primary));
        assert_eq!(command_style(&spans, "test").fg, Some(theme.secondary));
        assert_eq!(command_style(&spans, "--all").fg, Some(theme.accent));
        assert_eq!(command_style(&spans, "|").fg, Some(theme.muted));
        assert_eq!(command_style(&spans, "grep").fg, Some(theme.primary));

        let mut state = AppState::new();
        state.theme = theme;
        state.tool_calls.insert(
            "execute".to_string(),
            crate::app::ToolCallView {
                title: "FOO=bar cargo test --all | grep failed".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: Vec::new(),
            },
        );
        state
            .transcript
            .push(Entry::ToolCall("execute".to_string()));

        let rendered = render_transcript_lines(&state, 80);
        let header = rendered
            .iter()
            .find(|line| line_text(line) == "│ tool exec FOO=bar cargo test --all | grep failed")
            .expect("rendered execute tool header");
        let rendered_spans = header
            .spans
            .iter()
            .map(|span| (span.content.to_string(), span.style))
            .collect::<Vec<_>>();
        assert_eq!(
            command_style(&rendered_spans, "FOO=bar").fg,
            Some(theme.text)
        );
        assert_eq!(
            command_style(&rendered_spans, "cargo").fg,
            Some(theme.primary)
        );
        assert_eq!(
            command_style(&rendered_spans, "test").fg,
            Some(theme.secondary)
        );
        assert_eq!(
            command_style(&rendered_spans, "--all").fg,
            Some(theme.accent)
        );
        assert_eq!(command_style(&rendered_spans, "|").fg, Some(theme.muted));
        assert_eq!(
            command_style(&rendered_spans, "grep").fg,
            Some(theme.primary)
        );
    }

    #[test]
    fn transcript_renders_structured_tool_outputs() {
        let mut state = AppState::new();
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "run checks".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![
                    ToolCallOutput::Text("## Output\n`ok`".to_string()),
                    ToolCallOutput::Diff {
                        path: "src/main.rs".to_string(),
                        old_text: Some("old\nsame".to_string()),
                        new_text: "new\nsame".to_string(),
                    },
                    ToolCallOutput::Terminal {
                        terminal_id: "term-1".to_string(),
                        output: String::new(),
                        truncated: false,
                        exit_status: None,
                    },
                ],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let rendered: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();

        assert!(rendered.iter().any(|line| line == "│ tool exec run checks"));
        assert!(rendered.iter().any(|line| line == "│   ## Output"));
        assert!(rendered.iter().any(|line| line == "│   ok"));
        assert!(
            rendered
                .iter()
                .any(|line| line == "│   diff src/main.rs  +1 -1")
        );
        assert!(rendered.iter().any(|line| line.trim_end() == "│   1 - old"));
        assert!(rendered.iter().any(|line| line.trim_end() == "│   1 + new"));
        assert!(
            rendered
                .iter()
                .any(|line| line == "│   no terminal output received")
        );
        assert!(
            !rendered.iter().any(|line| line.contains("term-1")),
            "terminal ids should not leak into user-facing transcript rows: {rendered:?}"
        );
    }

    #[test]
    fn transcript_terminal_output_renders_state_without_raw_id() {
        let mut state = AppState::new();
        state.tool_calls.insert(
            "call-q403".to_string(),
            crate::app::ToolCallView {
                title: "cargo test".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Failed,
                body: vec![ToolCallOutput::Terminal {
                    terminal_id: "call_q403CLAwcOWdujDT6Xylsua6".to_string(),
                    output: "error: test failed\n".to_string(),
                    truncated: true,
                    exit_status: Some(TerminalExitStatus::new().exit_code(101)),
                }],
            },
        );
        state
            .transcript
            .push(Entry::ToolCall("call-q403".to_string()));

        let rendered_lines = render_transcript_lines(&state, 80);
        let rendered: Vec<String> = rendered_lines.iter().map(line_text).collect();

        assert!(
            rendered
                .iter()
                .any(|line| line == "│ tool exec cargo test · exit 101")
        );
        assert!(rendered.iter().any(|line| line == "│   [output truncated]"));
        assert!(rendered.iter().any(|line| line == "│   error: test failed"));
        assert!(!rendered.iter().any(|line| line.contains("[failed]")));
        assert!(!rendered.iter().any(|line| line.contains("exit code")));
        let header = rendered_lines
            .iter()
            .find(|line| line_text(line) == "│ tool exec cargo test · exit 101")
            .expect("terminal tool header");
        let outcome = header.spans.last().expect("terminal outcome span");
        assert_eq!(outcome.style.fg, Some(state.theme.error));
        assert!(outcome.style.add_modifier.contains(Modifier::BOLD));
        assert!(
            !rendered.iter().any(|line| line.contains("call_q403")),
            "terminal ids should not leak into user-facing transcript rows: {rendered:?}"
        );
    }

    #[test]
    fn transcript_renders_markdown_in_tool_text_output() {
        let mut state = AppState::new();
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "activate_skill".to_string(),
                kind: ToolKind::Read,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Text(
                    "_Auto permissions **approved** this tool call._\n\nReason: `read/search/fetch`\n\n- visible from anvil"
                        .to_string(),
                )],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let rendered: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();

        assert!(
            rendered
                .iter()
                .any(|line| line == "│   Auto permissions approved this tool call."),
            "rendered lines: {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line == "│   Reason: read/search/fetch"),
            "rendered lines: {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line == "│   - visible from anvil"),
            "rendered lines: {rendered:?}"
        );
    }

    #[test]
    fn markdown_wrapping_hangs_prefixes_and_preserves_inline_styles() {
        let mut state = AppState::new();
        let theme = state.theme;
        state.transcript.push(Entry::AgentMessage(
            "- **bold** *italic* `code` tail\n  123. **wide界** tail words\n> quoted words here"
                .to_string(),
        ));

        let width = 16;
        let lines = render_transcript_lines(&state, width);
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(
            rendered,
            [
                "● - bold italic",
                "    code tail",
                "    123. wide界",
                "         tail",
                "         words",
                "  > quoted words",
                "    here",
                "",
            ],
            "rendered role rows"
        );
        for row in &lines[..7] {
            assert!(
                line_text(row).width() <= width as usize,
                "row exceeds {width} cells: {:?}",
                line_text(row)
            );
        }

        let span = |content: &str| {
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content.as_ref() == content)
                .unwrap_or_else(|| panic!("missing span {content:?}: {rendered:?}"))
        };
        assert!(span("bold").style.add_modifier.contains(Modifier::BOLD));
        assert!(span("italic").style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(span("code").style.fg, Some(theme.code));
        assert!(span("wide界").style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn tool_markdown_wrapping_keeps_gutter_and_hanging_indent() {
        let mut state = AppState::new();
        let theme = state.theme;
        state.tool_calls.insert(
            "call-344".to_string(),
            crate::app::ToolCallView {
                title: "log".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Text(
                    "- **bold** *italic* `code` tail".to_string(),
                )],
            },
        );
        state
            .transcript
            .push(Entry::ToolCall("call-344".to_string()));

        let width = 16;
        let lines = render_transcript_lines(&state, width);
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        let body: Vec<&Line<'static>> = lines
            .iter()
            .filter(|line| {
                matches!(
                    line_text(line).as_str(),
                    "│   - bold" | "│     italic" | "│     code tail"
                )
            })
            .collect();
        assert_eq!(
            body.iter().map(|line| line_text(line)).collect::<Vec<_>>(),
            ["│   - bold", "│     italic", "│     code tail"],
            "rendered tool rows: {rendered:?}"
        );
        for row in &body {
            assert!(
                line_text(row).width() <= width as usize,
                "too wide: {row:?}"
            );
        }
        let span = |content: &str| {
            body.iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content.as_ref() == content)
                .unwrap_or_else(|| panic!("missing span {content:?}: {rendered:?}"))
        };
        assert!(span("bold").style.add_modifier.contains(Modifier::BOLD));
        assert!(span("italic").style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(span("code").style.fg, Some(theme.code));
    }

    #[test]
    fn transcript_markdown_links_tables_lists_headings_and_rules_share_reader_rendering() {
        let mut state = AppState::new();
        let theme = state.theme;
        state.transcript.push(Entry::AgentMessage(
            "# Top\n###### Bottom\n[docs](https://example.test/docs) and [more](https://example.test/more)\nname | value\n--- | :---:\n**alpha** | `beta`\n  - nested bullet\n    2. nested number\n---"
                .to_string(),
        ));
        state.expand_transcript_details = true;

        let width = 26;
        let normal = render_transcript_lines(&state, width);
        let full = render_full_transcript_lines(&state, width);
        let signature = |lines: &[Line<'static>]| {
            lines
                .iter()
                .map(|line| {
                    (
                        line_text(line),
                        line.spans
                            .iter()
                            .map(|span| (span.content.to_string(), span.style))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(signature(&normal), signature(&full));

        let rendered: Vec<String> = normal.iter().map(line_text).collect();
        let content = rendered
            .iter()
            .filter(|line| !line.is_empty())
            .map(|line| {
                line.chars()
                    .skip(ROLE_GUTTER_WIDTH as usize)
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(
            content
                .join("")
                .contains("docs (https://example.test/docs)")
                && content
                    .join("")
                    .contains("more (https://example.test/more)")
        );
        assert!(content.iter().any(|line| line == "name | value"));
        assert!(content.iter().any(|line| line == "alpha | beta"));
        assert!(!content.iter().any(|line| line.contains(":---:")));
        assert!(content.iter().any(|line| line == "  - nested bullet"));
        assert!(content.iter().any(|line| line == "    2. nested number"));
        assert!(
            content
                .iter()
                .any(|line| line == &"─".repeat((width - ROLE_GUTTER_WIDTH) as usize))
        );

        let top = normal
            .iter()
            .find(|line| line_text(line) == "● # Top")
            .unwrap();
        let bottom = normal
            .iter()
            .find(|line| line_text(line) == "  ###### Bottom")
            .unwrap();
        assert_ne!(top.spans[1].style, bottom.spans[1].style);
        assert_eq!(top.spans[1].style.fg, Some(theme.primary));
        assert_eq!(bottom.spans[1].style.fg, Some(theme.muted));

        let paragraph = Paragraph::new(normal).wrap(Wrap { trim: false });
        let height = paragraph.line_count(width);
        let area = Rect::new(0, 0, width, height as u16);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        paragraph.render(area, &mut buffer);
        let narrow = buffer_lines(&buffer).join("");
        let narrow_without_layout_space = narrow
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>();
        assert!(
            narrow_without_layout_space.contains("example.test/docs"),
            "narrow Markdown rendering lost wrapped URL: {narrow:?}"
        );
        for content in [
            "docs",
            "name",
            "value",
            "alpha",
            "beta",
            "nested bullet",
            "nested number",
        ] {
            assert!(
                narrow.contains(content),
                "narrow Markdown rendering lost {content:?}: {narrow:?}"
            );
        }
    }

    #[test]
    fn tool_markdown_constructs_stay_desaturated_and_fit_narrow_gutter() {
        let mut state = AppState::new();
        let theme = state.theme;
        state.tool_calls.insert(
            "call-343".to_string(),
            crate::app::ToolCallView {
                title: "log".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Text(
                    "# heading\n[label](https://example.test/a-very-long-path)\nkey | value\n--- | ---\n**left** | *right*\n  - nested\n---"
                        .to_string(),
                )],
            },
        );
        state
            .transcript
            .push(Entry::ToolCall("call-343".to_string()));

        let width = 24u16;
        let lines = render_transcript_lines(&state, width);
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        let tool_content = lines
            .iter()
            .filter(|line| line_text(line).starts_with(TOOL_GUTTER))
            .map(line_text)
            .collect::<String>();
        assert!(
            tool_content.contains("label (https://examp")
                && tool_content.contains("le.test/a-very-long-")
                && tool_content.contains("path)"),
            "wrapped tool rows lost link content: {rendered:?}"
        );
        assert!(rendered.iter().any(|line| line == "│   key | value"));
        assert!(rendered.iter().any(|line| line == "│     - nested"));
        for line in lines.iter().filter(|line| {
            let text = line_text(line);
            text.starts_with(TOOL_GUTTER) && !text.starts_with("│ tool ")
        }) {
            assert!(
                line_text(line).width() <= width as usize,
                "too wide: {line:?}"
            );
            for span in line.spans.iter().skip(1) {
                assert!(
                    span.style.fg == Some(theme.subtle) || span.style.fg == Some(theme.muted),
                    "tool markdown recolored content: {line:?}"
                );
            }
        }
        let emphasis = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.as_ref() == "left")
            .expect("bold table cell");
        assert_eq!(emphasis.style.fg, Some(theme.subtle));
        assert!(emphasis.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn transcript_tool_markdown_preserves_technical_underscores() {
        let mut state = AppState::new();
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "log".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Text(
                    "src/my_file.rs\nfoo_bar_baz\n_Auto permissions approved._".to_string(),
                )],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let rendered: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();

        assert!(rendered.iter().any(|line| line == "│   src/my_file.rs"));
        assert!(rendered.iter().any(|line| line == "│   foo_bar_baz"));
        assert!(
            rendered
                .iter()
                .any(|line| line == "│   Auto permissions approved."),
            "rendered lines: {rendered:?}"
        );
    }

    #[test]
    fn transcript_tool_markdown_output_uses_semantic_status_color() {
        let mut state = AppState::new();
        let theme = state.theme;
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "log".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Text(
                    "warning: **check**\ntest result: ok. 1324 passed; 0 failed; 0 ignored\nerror: test failed"
                        .to_string(),
                )],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let lines = render_transcript_lines(&state, 80);
        let warning_line = lines
            .iter()
            .find(|line| line_text(line) == "│   warning: check")
            .unwrap_or_else(|| {
                panic!(
                    "rendered lines: {:?}",
                    lines.iter().map(line_text).collect::<Vec<_>>()
                )
            });

        assert!(
            warning_line
                .spans
                .iter()
                .skip(1)
                .all(|span| span.style.fg == Some(theme.warning)),
            "warning output should be easy to spot: {warning_line:?}"
        );
        assert!(
            warning_line.spans.iter().skip(1).any(|span| {
                span.content.as_ref() == "check"
                    && span.style.fg == Some(theme.warning)
                    && span.style.add_modifier.contains(Modifier::BOLD)
            }),
            "inline markdown should preserve emphasis with semantic color: {warning_line:?}"
        );

        let success_line = lines
            .iter()
            .find(|line| line_text(line) == "│   test result: ok. 1324 passed; 0 failed; 0 ignored")
            .expect("successful test summary");
        assert!(
            success_line
                .spans
                .iter()
                .skip(1)
                .all(|span| span.style.fg == Some(theme.success)),
            "zero failures must not override a successful summary: {success_line:?}"
        );

        let error_line = lines
            .iter()
            .find(|line| line_text(line) == "│   error: test failed")
            .expect("failed test summary");
        assert!(
            error_line.spans.iter().skip(1).all(|span| {
                span.style.fg == Some(theme.error)
                    && span.style.add_modifier.contains(Modifier::BOLD)
            }),
            "real failures should remain prominent: {error_line:?}"
        );
    }

    #[test]
    fn tool_output_semantic_colors_ignore_incidental_failure_words() {
        let theme = TerminalThemeKind::Dark.palette();
        for line in [
            "0 errors",
            "Permission denied inside a deliberate check",
            "src/error_handling.rs",
            "src/panic_handler.rs",
        ] {
            assert_eq!(
                tool_output_line_style(line, theme).fg,
                Some(theme.subtle),
                "incidental status word in {line:?}"
            );
        }
        assert_eq!(
            tool_output_line_style("1 passed; 1 failed", theme).fg,
            Some(theme.error)
        );
    }

    #[test]
    fn tool_calls_framed_by_status_colored_gutter_agent_messages_are_not() {
        let mut state = AppState::new();
        let theme = state.theme;
        state
            .transcript
            .push(Entry::AgentMessage("hi there".to_string()));
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "cargo test".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Text("ok".to_string())],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let lines = render_transcript_lines(&state, 80);

        // Both the tool header and its output are framed by the gutter rail.
        let call_line = lines
            .iter()
            .find(|line| line_text(line) == "│ tool exec cargo test")
            .expect("tool call line");
        assert_eq!(call_line.spans[1].style.fg, Some(theme.success));
        assert!(
            call_line.spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(lines.iter().any(|l| line_text(l) == "│   ok"));

        // The rail on every framed line carries the status color — success
        // here, because the call completed.
        for line in lines
            .iter()
            .filter(|l| line_text(l).starts_with(TOOL_GUTTER))
        {
            assert_eq!(line.spans[0].content.as_ref(), TOOL_GUTTER);
            assert_eq!(line.spans[0].style.fg, Some(theme.success));
        }

        // Agent prose uses its own role gutter rather than the tool rail.
        assert!(lines.iter().any(|l| line_text(l) == "● hi there"));
        assert!(
            !lines
                .iter()
                .any(|l| line_text(l).starts_with(TOOL_GUTTER) && line_text(l).contains("hi there"))
        );
    }

    #[test]
    fn tool_output_wraps_with_gutter_on_every_row() {
        let mut state = AppState::new();
        // One output line far wider than the render width, so it must wrap.
        let long = "abcdefghij ".repeat(12);
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "log".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Text(long)],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let width = 24u16;
        let lines = render_transcript_lines(&state, width);
        let rendered: Vec<String> = lines.iter().map(line_text).collect();

        // Every non-blank row of the tool block must keep the gutter rail (so
        // wrapped continuation rows never read as flush-left agent prose) and
        // must fit inside the render width (so the transcript Paragraph does
        // not re-wrap it and strip the rail). See issue #257.
        assert_eq!(
            rendered.first().map(String::as_str),
            Some("│ tool exec log")
        );
        let block_rows: Vec<&String> = rendered.iter().filter(|line| !line.is_empty()).collect();
        assert!(
            block_rows.len() > 2,
            "expected the long line to wrap into several rows, got {rendered:?}"
        );
        for row in &block_rows {
            assert!(
                row.starts_with(TOOL_GUTTER),
                "row lost the gutter rail: {row:?}"
            );
            assert!(
                row.width() <= width as usize,
                "row {row:?} is {} cells, wider than the {width}-cell pane",
                row.width()
            );
        }
    }

    #[test]
    fn user_prompts_render_plain_text_tool_text_renders_markdown() {
        let mut state = AppState::new();
        state.transcript.push(Entry::UserPrompt(
            "# literal\n`code` and **bold**".to_string(),
        ));
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "log".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Text(
                    "# stdout\n`ok` and **bold**".to_string(),
                )],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let rendered: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();

        assert!(rendered.iter().any(|line| line == "❯ # literal"));
        assert!(rendered.iter().any(|line| line == "  `code` and **bold**"));
        assert!(rendered.iter().any(|line| line == "│   # stdout"));
        assert!(rendered.iter().any(|line| line == "│   ok and bold"));
    }

    #[test]
    fn consecutive_tool_calls_render_without_blank_row_between() {
        let mut state = AppState::new();
        for (id, title) in [("call-1", "first"), ("call-2", "second")] {
            state.tool_calls.insert(
                id.to_string(),
                crate::app::ToolCallView {
                    title: title.to_string(),
                    kind: ToolKind::Execute,
                    status: ToolCallStatus::Completed,
                    body: Vec::new(),
                },
            );
            state.transcript.push(Entry::ToolCall(id.to_string()));
        }

        let rendered: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();

        let first = rendered
            .iter()
            .position(|line| line.contains("first"))
            .expect("first tool row");
        let second = rendered
            .iter()
            .position(|line| line.contains("second"))
            .expect("second tool row");
        assert_eq!(
            second,
            first + 1,
            "consecutive tool rails should abut, got {rendered:?}"
        );
        // The run still ends with a separator row before whatever follows.
        assert_eq!(rendered.last().map(String::as_str), Some(""));
    }

    #[test]
    fn thought_blocks_render_dimmed_with_role_glyph() {
        let mut state = AppState::new();
        state.expand_transcript_details = true;
        let theme = state.theme;
        state
            .transcript
            .push(Entry::AgentThought(crate::app::ThoughtEntry {
                text: "weighing the options".to_string(),
                completed: true,
            }));

        let lines = render_transcript_lines(&state, 80);
        let row = lines
            .iter()
            .find(|l| line_text(l).contains("weighing"))
            .expect("thought row");
        assert!(
            lines
                .iter()
                .any(|line| line_text(line) == "○ weighing the options")
        );
        for span in &row.spans {
            assert_eq!(
                span.style.fg,
                Some(theme.thought),
                "thought body must read as secondary text: {row:?}"
            );
        }
    }

    #[test]
    fn thought_markdown_heading_is_dimmed_not_left_at_reply_contrast() {
        // A heading carries theme.text (the primary reply color); inside a
        // thought it must still read as dimmed reasoning, not like a real
        // reply heading.
        let mut state = AppState::new();
        state.expand_transcript_details = true;
        let theme = state.theme;
        state
            .transcript
            .push(Entry::AgentThought(crate::app::ThoughtEntry {
                text: "# Plan\nthen do it".to_string(),
                completed: true,
            }));

        let lines = render_transcript_lines(&state, 80);
        let heading = lines
            .iter()
            .find(|l| line_text(l).contains("Plan"))
            .expect("heading row");
        // Before the fix the heading kept theme.text (White in the default
        // Dark theme, != theme.thought DarkGray), so this catches the regress.
        assert!(
            heading
                .spans
                .iter()
                .all(|span| span.style.fg == Some(theme.thought)),
            "thought heading must be dimmed, not left at reply contrast: {heading:?}"
        );
    }

    #[test]
    fn agent_markdown_hides_html_comments_but_keeps_them_in_code() {
        let mut state = AppState::new();
        state.expand_transcript_details = true;
        state.transcript.push(Entry::AgentMessage(
            "before <!-- inline --> after\n`<!-- inline code -->`\n``<!-- multi-tick code -->``\nunmatched ` <!-- hidden after unmatched tick -->visible\n<!-- standalone -->\n<!-- multiline\nstill hidden -->visible\n```html\n<!-- literal -->\n```\n~~~html\n<!-- tilde literal -->\n~~~"
                .to_string(),
        ));

        let rendered: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();

        assert!(rendered.iter().any(|line| line.ends_with("before  after")));
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("<!-- inline code -->"))
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("<!-- multi-tick code -->"))
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.ends_with("unmatched ` visible"))
        );
        assert!(rendered.iter().any(|line| line.contains("visible")));
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("<!-- literal -->"))
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("<!-- tilde literal -->"))
        );
        assert!(!rendered.iter().any(|line| line.contains("standalone")));
        assert!(!rendered.iter().any(|line| line.contains("multiline")));
        assert!(!rendered.iter().any(|line| line.contains("still hidden")));
    }

    #[test]
    fn collapsed_tool_markdown_replays_html_comment_state() {
        let mut state = AppState::new();
        let mut lines: Vec<String> = (1..TOOL_OUTPUT_COLLAPSED_LINES)
            .map(|line| format!("line {line}"))
            .collect();
        lines.push("<!-- hidden metadata".to_string());
        lines.extend(["still hidden".to_string(), "-->visible result".to_string()]);
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "log".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Text(lines.join("\n"))],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let rendered = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert!(rendered.iter().any(|line| line.contains("visible result")));
        assert!(!rendered.iter().any(|line| line.contains("still hidden")));
        assert!(!rendered.iter().any(|line| line.contains("-->")));
    }

    #[test]
    fn leading_blank_agent_message_keeps_speaker_separate_from_content() {
        // A body that begins with a blank line must not strand an attribution
        // marker on an empty row while the first real content is lost.
        let mut state = AppState::new();
        state
            .transcript
            .push(Entry::AgentMessage("\nhello".to_string()));

        let rendered: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();

        assert!(
            rendered.iter().any(|line| line == "● hello"),
            "role marker must stay attached to the first content row: {rendered:?}"
        );
    }

    #[test]
    fn collapsed_tool_markdown_tail_keeps_code_fence_state() {
        let mut state = AppState::new();
        let theme = state.theme;
        // The opening fence lands in the hidden head: 3 intro lines + the
        // fence + 6 code lines, with a budget of 6, hides "intro"s and "```".
        let mut text: Vec<String> = (1..=3).map(|n| format!("intro {n}")).collect();
        text.push("```rs".to_string());
        text.extend((1..=6).map(|n| format!("code line {n}")));
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "log".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Text(text.join("\n"))],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let lines = render_transcript_lines(&state, 80);
        let hint_idx = lines
            .iter()
            .position(|l| line_text(l).contains("4 earlier lines hidden"))
            .expect("collapse hint above the tail");
        let code_idx = lines
            .iter()
            .position(|l| line_text(l).contains("code line 1"))
            .expect("code row");
        assert!(hint_idx < code_idx, "hint must lead the visible tail");
        let code_row = &lines[code_idx];
        // The tail starts inside the fence, so it still renders as code.
        assert!(
            code_row
                .spans
                .iter()
                .any(|span| span.style.fg == Some(theme.quote)
                    && span.content.contains("code line 1")),
            "tail must keep the code-block style: {code_row:?}"
        );
    }

    #[test]
    fn compact_line_diff_handles_insertions() {
        let old = ["a", "b", "c"];
        let new = ["a", "inserted", "b", "c"];

        let diff = compact_line_diff(&old, &new, 20);

        let summary: Vec<(DiffLineKind, String, Option<usize>, Option<usize>)> = diff
            .iter()
            .map(|line| (line.kind, line.text(), line.old_line, line.new_line))
            .collect();
        assert_eq!(
            summary,
            vec![
                (DiffLineKind::Context, "a".to_string(), Some(1), Some(1)),
                (DiffLineKind::Added, "inserted".to_string(), None, Some(2)),
                (DiffLineKind::Context, "b".to_string(), Some(2), Some(3)),
                (DiffLineKind::Context, "c".to_string(), Some(3), Some(4)),
            ]
        );
    }

    #[test]
    fn diff_rendering_truncates_to_available_width() {
        let old = ["short"];
        let new = ["abcdefghijklmnopqrstuvwxyz"];
        let diff = compact_line_diff(&old, &new, 20);
        assert!(
            diff.iter()
                .any(|line| line.text() == "abcdefghijklmnopqrstuvwxyz")
        );

        let mut out = Vec::new();
        push_diff_output(
            &mut out,
            "file.txt",
            Some("short"),
            "abcdefghijklmnopqrstuvwxyz",
            12,
            None,
            TerminalThemeKind::default().palette(),
        );
        let rendered: Vec<String> = out.iter().map(line_text).collect();

        assert!(
            rendered
                .iter()
                .any(|line| line.trim_end() == "  1 + abc...")
        );
    }

    #[test]
    fn intra_line_word_diff_emphasizes_changed_tokens() {
        let diff = compact_line_diff(&["let x = 1;"], &["let x = 2;"], 20);

        let emphasized = |line: &DiffLine| -> String {
            line.segments
                .iter()
                .filter(|segment| segment.emphasized)
                .map(|segment| segment.text.as_str())
                .collect()
        };
        let removed = diff
            .iter()
            .find(|line| line.kind == DiffLineKind::Removed)
            .expect("removed row");
        let added = diff
            .iter()
            .find(|line| line.kind == DiffLineKind::Added)
            .expect("added row");
        assert_eq!(emphasized(removed), "1");
        assert_eq!(emphasized(added), "2");
        assert_eq!(removed.text(), "let x = 1;");
        assert_eq!(added.text(), "let x = 2;");
    }

    #[test]
    fn dissimilar_replacement_lines_skip_word_emphasis() {
        let diff = compact_line_diff(&["alpha beta gamma"], &["zz qq ww"], 20);
        assert!(
            diff.iter()
                .all(|line| line.segments.iter().all(|segment| !segment.emphasized))
        );
    }

    #[test]
    fn long_unchanged_stretches_collapse_to_omitted_rows() {
        let old: Vec<String> = (1..=30).map(|idx| format!("line {idx}")).collect();
        let mut new = old.clone();
        new[14] = "changed".to_string();
        let old_refs: Vec<&str> = old.iter().map(String::as_str).collect();
        let new_refs: Vec<&str> = new.iter().map(String::as_str).collect();

        let diff = compact_line_diff(&old_refs, &new_refs, 200);

        assert_eq!(
            diff.iter()
                .filter(|line| line.kind == DiffLineKind::Omitted)
                .count(),
            2
        );
        // One removed, one added, three context lines on each side.
        assert_eq!(
            diff.iter()
                .filter(|line| line.kind != DiffLineKind::Omitted)
                .count(),
            8
        );
        assert!(
            diff.iter()
                .any(|line| line.kind == DiffLineKind::Removed && line.old_line == Some(15))
        );
        assert!(
            diff.iter()
                .any(|line| line.kind == DiffLineKind::Added && line.new_line == Some(15))
        );
    }

    #[test]
    fn ctrl_digit_no_longer_opens_config_value_picker() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![
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
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('2'), KeyModifiers::CONTROL),
        );

        assert!(state.config_picker.is_none());
    }

    #[test]
    fn ctrl_shift_digit_no_longer_opens_config_value_picker() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![
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
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(
                KeyCode::Char('2'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
        );

        assert!(state.config_picker.is_none());
    }

    #[test]
    fn ctrl_azerty_number_row_key_no_longer_opens_config_value_picker() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![
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
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('\u{e9}'), KeyModifiers::CONTROL),
        );

        assert!(state.config_picker.is_none());
    }

    #[test]
    fn function_key_no_longer_opens_config_value_picker() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![
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
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::F(2)));

        assert!(state.config_picker.is_none());
    }

    #[test]
    fn inline_ctrl_digit_no_longer_opens_config_value_picker() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![
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
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_inline_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('2'), KeyModifiers::CONTROL),
        );

        assert!(state.config_picker.is_none());
    }

    #[test]
    fn inline_function_key_no_longer_opens_config_value_picker() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![
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
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::F(2)));

        assert!(state.config_picker.is_none());
    }

    #[test]
    fn usage_quota_row_renders_between_input_and_config_shortcuts() {
        let mut state = AppState::new();
        state.set_claude_usage(ClaudeUsageStatus::Available(ClaudeUsageReport {
            five_hour: Some(crate::claude_usage::ClaudeUsageWindow {
                remaining_percent: 88,
                reset_context: None,
            }),
            week: Some(crate::claude_usage::ClaudeUsageWindow {
                remaining_percent: 63,
                reset_context: None,
            }),
        }));
        state.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "model-1",
            vec![SessionConfigSelectOption::new("model-1", "Model 1")],
        )];

        let backend = TestBackend::new(100, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(1), Constraint::Length(1)])
                    .split(frame.area());
                draw_usage_quota_row(frame, chunks[0], &state);
                draw_config_shortcuts_row(frame, chunks[1], &state);
            })
            .expect("draw");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines[0].contains("Claude usage: 5H 88% left · week 63% left"));
        assert!(lines[1].contains("[F1 Model: Model 1]"));
    }

    #[test]
    fn usage_quota_row_renders_claude_unavailable_reason() {
        let mut state = AppState::new();
        state.set_claude_usage(ClaudeUsageStatus::Unavailable("not signed in".to_string()));

        let backend = TestBackend::new(100, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_usage_quota_row(frame, frame.area(), &state))
            .expect("draw");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines[0].contains("Claude usage unavailable: not signed in"));
    }

    #[test]
    fn usage_quota_row_renders_bedrock_available_and_unavailable() {
        let mut state = AppState::new();
        state.set_bedrock_credits(crate::bedrock_credits::BedrockCreditsStatus::Available(
            crate::bedrock_credits::BedrockCreditsReport {
                amounts: vec![crate::bedrock_credits::CreditAmount {
                    currency: "USD".to_string(),
                    amount: 12.5,
                }],
                earliest_expiration: Some("2026-12-31".to_string()),
                as_of: "2026-07-15".to_string(),
            },
        ));
        assert_eq!(
            usage_quota_label(&state).as_deref(),
            Some("Bedrock credits: USD 12.50 · expires 2026-12-31 · as of 2026-07-15")
        );
        let backend = TestBackend::new(100, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_usage_quota_row(frame, frame.area(), &state))
            .expect("draw");
        assert!(
            buffer_lines(terminal.backend().buffer())[0].contains("Bedrock credits: USD 12.50")
        );

        state.set_bedrock_credits(crate::bedrock_credits::BedrockCreditsStatus::Unavailable(
            "request timed out".to_string(),
        ));
        assert_eq!(
            usage_quota_label(&state).as_deref(),
            Some("Bedrock credits unavailable: request timed out")
        );
        terminal
            .draw(|frame| draw_usage_quota_row(frame, frame.area(), &state))
            .expect("draw");
        assert!(
            buffer_lines(terminal.backend().buffer())[0]
                .contains("Bedrock credits unavailable: request timed out")
        );
    }

    #[test]
    fn usage_quota_row_renders_openrouter_available_zero_and_unavailable() {
        let mut state = AppState::new();
        state.set_openrouter_balance(
            crate::openrouter_balance::OpenRouterBalanceStatus::Available(
                crate::openrouter_balance::OpenRouterBalanceReport {
                    remaining_usd: 12.5,
                    total_credits_usd: 20.0,
                    total_usage_usd: 7.5,
                    as_of: "2026-07-15T18:42:00Z".to_string(),
                },
            ),
        );
        assert_eq!(
            usage_quota_label(&state).as_deref(),
            Some("OpenRouter balance: USD 12.50 remaining · as of 2026-07-15T18:42:00Z")
        );

        state.set_openrouter_balance(
            crate::openrouter_balance::OpenRouterBalanceStatus::Available(
                crate::openrouter_balance::OpenRouterBalanceReport {
                    remaining_usd: 0.0,
                    total_credits_usd: 20.0,
                    total_usage_usd: 20.0,
                    as_of: "2026-07-15T18:43:00Z".to_string(),
                },
            ),
        );
        assert_eq!(
            usage_quota_label(&state).as_deref(),
            Some("OpenRouter balance: USD 0.00 remaining · as of 2026-07-15T18:43:00Z")
        );

        state.set_openrouter_balance(
            crate::openrouter_balance::OpenRouterBalanceStatus::Unavailable(
                "billing credentials are unavailable".to_string(),
            ),
        );
        let backend = TestBackend::new(100, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_usage_quota_row(frame, frame.area(), &state))
            .expect("draw");
        assert!(
            buffer_lines(terminal.backend().buffer())[0]
                .contains("OpenRouter balance unavailable: billing credentials are unavailable")
        );
    }

    #[test]
    fn usage_quota_row_renders_all_deepseek_balance_records() {
        let mut state = AppState::new();
        state.set_deepseek_balance(crate::deepseek_balance::DeepSeekBalanceStatus::Available(
            crate::deepseek_balance::DeepSeekBalanceReport {
                balances: vec![
                    crate::deepseek_balance::DeepSeekBalance {
                        currency: "CNY".to_string(),
                        total_balance: "123.4500".to_string(),
                        granted_balance: "23.0000".to_string(),
                        topped_up_balance: "100.4500".to_string(),
                    },
                    crate::deepseek_balance::DeepSeekBalance {
                        currency: "USD".to_string(),
                        total_balance: "0.010".to_string(),
                        granted_balance: "0.000".to_string(),
                        topped_up_balance: "0.010".to_string(),
                    },
                ],
                as_of: "2026-07-15T18:42:00Z".to_string(),
            },
        ));
        assert_eq!(
            usage_quota_label(&state).as_deref(),
            Some(
                "DeepSeek balance: CNY total 123.4500 · granted 23.0000 · topped up 100.4500 · USD total 0.010 · granted 0.000 · topped up 0.010 · as of 2026-07-15T18:42:00Z"
            )
        );

        let backend = TestBackend::new(50, 5);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_usage_quota_row(frame, frame.area(), &state))
            .expect("draw");
        let rendered = buffer_lines(terminal.backend().buffer());
        for expected in [
            "CNY total",
            "123.4500",
            "granted",
            "23.0000",
            "topped up",
            "100.4500",
            "USD total",
            "0.010",
            "0.000",
            "2026-07-15T18:42:00Z",
        ] {
            assert!(
                rendered.iter().any(|line| line.contains(expected)),
                "missing intact {expected}: {rendered:?}"
            );
        }
        assert!(usage_quota_row_count(&state, 50) > 1);

        state.set_deepseek_balance(crate::deepseek_balance::DeepSeekBalanceStatus::Unavailable(
            crate::deepseek_balance::DeepSeekBalanceUnavailable {
                reason: "billing credentials are unavailable".to_string(),
                as_of: "2026-07-15T18:43:00Z".to_string(),
            },
        ));
        assert_eq!(
            usage_quota_label(&state).as_deref(),
            Some(
                "DeepSeek balance unavailable: billing credentials are unavailable · as of 2026-07-15T18:43:00Z"
            )
        );
    }

    #[test]
    fn peer_quota_rows_remain_single_line_and_truncated() {
        let mut state = AppState::new();
        state.set_bedrock_credits(crate::bedrock_credits::BedrockCreditsStatus::Unavailable(
            "request timed out".to_string(),
        ));
        assert_eq!(usage_quota_row_count(&state, 12), 1);

        let backend = TestBackend::new(12, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_usage_quota_row(frame, frame.area(), &state))
            .expect("draw");
        assert_eq!(
            buffer_lines(terminal.backend().buffer())[0],
            truncate_text_to_width(
                "Bedrock credits unavailable: request timed out".to_string(),
                12
            )
        );

        state.set_openrouter_balance(
            crate::openrouter_balance::OpenRouterBalanceStatus::Unavailable(
                "request timed out".to_string(),
            ),
        );
        assert_eq!(usage_quota_row_count(&state, 12), 1);
    }

    #[test]
    fn usage_quota_label_prefers_selected_anvil_provider_over_background_pollers() {
        let mut state = AppState::new();
        state.set_bedrock_credits(crate::bedrock_credits::BedrockCreditsStatus::Unavailable(
            "bedrock unavailable".to_string(),
        ));
        assert_eq!(
            usage_quota_label(&state).as_deref(),
            Some("Bedrock credits unavailable: bedrock unavailable")
        );

        state.set_openrouter_balance(
            crate::openrouter_balance::OpenRouterBalanceStatus::Unavailable(
                "openrouter unavailable".to_string(),
            ),
        );
        assert_eq!(
            usage_quota_label(&state).as_deref(),
            Some("OpenRouter balance unavailable: openrouter unavailable")
        );

        state.set_bedrock_credits(crate::bedrock_credits::BedrockCreditsStatus::Unavailable(
            "newer bedrock unavailable".to_string(),
        ));
        assert_eq!(
            usage_quota_label(&state).as_deref(),
            Some("Bedrock credits unavailable: newer bedrock unavailable")
        );

        state.set_codex_usage(crate::codex_usage::CodexUsageStatus::Unavailable(
            "codex unavailable".to_string(),
        ));
        state.set_claude_usage(ClaudeUsageStatus::Unavailable(
            "claude unavailable".to_string(),
        ));
        assert_eq!(
            usage_quota_label(&state).as_deref(),
            Some("Bedrock credits unavailable: newer bedrock unavailable")
        );
    }

    #[test]
    fn usage_quota_label_attributes_distinct_primary_and_subagent_sources() {
        let mut state = AppState::new();
        state.active_models.primary_source = Some("claude-acp".to_string());
        state.active_models.subagent_source = Some("codex-acp".to_string());
        state.set_claude_usage(ClaudeUsageStatus::Unavailable(
            "claude unavailable".to_string(),
        ));
        state.set_codex_usage(crate::codex_usage::CodexUsageStatus::Unavailable(
            "codex unavailable".to_string(),
        ));
        assert_eq!(
            usage_quota_label(&state).as_deref(),
            Some(
                "primary Claude usage unavailable: claude unavailable · subagents Codex usage unavailable: codex unavailable"
            )
        );

        assert_eq!(usage_quota_row_count(&state, 160), 2);

        let backend = TestBackend::new(160, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_usage_quota_row(frame, frame.area(), &state))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let rendered = buffer_lines(buffer);
        assert!(rendered[0].starts_with("[PRIMARY] Claude usage unavailable"));
        assert!(rendered[1].starts_with("[SUBAGENTS] Codex usage unavailable"));
        assert_eq!(
            buffer.cell((1, 0)).expect("primary cell").style().fg,
            Some(state.theme.primary)
        );
        let primary_quota_x = rendered[0].find("Claude usage").expect("primary quota") as u16;
        assert_eq!(
            buffer
                .cell((primary_quota_x, 0))
                .expect("primary quota cell")
                .style()
                .fg,
            Some(state.theme.primary)
        );
        assert_eq!(
            buffer.cell((1, 1)).expect("subagents cell").style().fg,
            Some(state.theme.secondary)
        );
        let subagent_quota_x = rendered[1].find("Codex usage").expect("subagent quota") as u16;
        assert_eq!(
            buffer
                .cell((subagent_quota_x, 1))
                .expect("subagent quota cell")
                .style()
                .fg,
            Some(state.theme.secondary)
        );
    }

    #[test]
    fn usage_quota_label_omits_duplicate_shared_adapter_quota() {
        let mut state = AppState::new();
        state.active_models.primary_source = Some("codex-acp".to_string());
        state.active_models.subagent_source = Some("codex-acp".to_string());
        state.set_codex_usage(crate::codex_usage::CodexUsageStatus::Unavailable(
            "codex unavailable".to_string(),
        ));
        assert_eq!(
            usage_quota_label(&state).as_deref(),
            Some("primary Codex usage unavailable: codex unavailable")
        );
    }

    #[test]
    fn usage_quota_rows_include_background_anvil_balance() {
        let mut state = AppState::new();
        state.active_models.primary_source = Some("claude-acp".to_string());
        state.active_models.subagent_source = Some("codex-acp".to_string());
        state.set_claude_usage(ClaudeUsageStatus::Unavailable(
            "claude unavailable".to_string(),
        ));
        state.set_codex_usage(crate::codex_usage::CodexUsageStatus::Unavailable(
            "codex unavailable".to_string(),
        ));
        state.set_bedrock_credits(crate::bedrock_credits::BedrockCreditsStatus::Unavailable(
            "bedrock unavailable".to_string(),
        ));

        assert_eq!(usage_quota_row_count(&state, 160), 3);

        let backend = TestBackend::new(160, 3);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_usage_quota_row(frame, frame.area(), &state))
            .expect("draw");
        let rendered = buffer_lines(terminal.backend().buffer());
        assert!(rendered[0].starts_with("[PRIMARY] Claude usage unavailable"));
        assert!(rendered[1].starts_with("[SUBAGENTS] Codex usage unavailable"));
        assert!(rendered[2].starts_with("[ANVIL] Bedrock credits unavailable"));
    }

    #[test]
    fn usage_quota_rows_match_rendered_lines_when_deepseek_wraps() {
        let mut state = AppState::new();
        state.active_models.primary_source = Some("claude-acp".to_string());
        state.active_models.subagent_source = Some("codex-acp".to_string());
        state.set_claude_usage(ClaudeUsageStatus::Unavailable(
            "claude quota request timed out after thirty seconds".to_string(),
        ));
        state.set_codex_usage(crate::codex_usage::CodexUsageStatus::Unavailable(
            "codex quota request timed out after thirty seconds".to_string(),
        ));
        state.set_deepseek_balance(crate::deepseek_balance::DeepSeekBalanceStatus::Unavailable(
            crate::deepseek_balance::DeepSeekBalanceUnavailable {
                reason: "billing credentials are unavailable".to_string(),
                as_of: "2026-07-15T18:43:00Z".to_string(),
            },
        ));

        for width in [40u16, 60, 80] {
            let count = usage_quota_row_count(&state, width) as u16;
            let backend = TestBackend::new(width, count.max(1) + 6);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| draw_usage_quota_row(frame, frame.area(), &state))
                .expect("draw");
            let rendered = buffer_lines(terminal.backend().buffer());
            let non_blank = rendered.iter().filter(|l| !l.trim().is_empty()).count();
            assert_eq!(
                count as usize, non_blank,
                "width {width} row count mismatch"
            );
        }
    }

    #[test]
    fn usage_quota_label_reports_shared_anvil_seats_once_as_primary() {
        let mut state = AppState::new();
        state.active_models.primary_source = Some("anvil".to_string());
        state.active_models.subagent_source = Some("anvil".to_string());
        state.set_bedrock_credits(crate::bedrock_credits::BedrockCreditsStatus::Unavailable(
            "bedrock unavailable".to_string(),
        ));

        assert_eq!(
            usage_quota_label(&state).as_deref(),
            Some("primary Bedrock credits unavailable: bedrock unavailable")
        );
        assert_eq!(usage_quota_row_count(&state, 160), 1);
    }

    #[test]
    fn usage_quota_label_skips_primary_seat_without_a_quota_source() {
        let mut state = AppState::new();
        state.active_models.primary_source = Some("kimi".to_string());
        state.active_models.subagent_source = Some("codex-acp".to_string());
        state.set_codex_usage(crate::codex_usage::CodexUsageStatus::Unavailable(
            "codex unavailable".to_string(),
        ));

        assert_eq!(
            usage_quota_label(&state).as_deref(),
            Some("subagents Codex usage unavailable: codex unavailable")
        );
        assert_eq!(usage_quota_row_count(&state, 160), 1);
    }

    #[test]
    fn usage_quota_label_falls_back_when_no_seat_resolves_a_quota_source() {
        let mut state = AppState::new();
        state.active_models.primary_source = Some("anvil".to_string());
        state.set_codex_usage(crate::codex_usage::CodexUsageStatus::Unavailable(
            "codex unavailable".to_string(),
        ));

        assert!(attributed_usage_quota_items(&state).is_none());
        assert_eq!(
            usage_quota_label(&state).as_deref(),
            Some("Codex usage unavailable: codex unavailable")
        );
        assert_eq!(usage_quota_row_count(&state, 160), 1);
    }

    #[test]
    fn usage_quota_label_uses_priority_fallback_without_seat_attribution() {
        let mut state = AppState::new();
        state.set_claude_usage(ClaudeUsageStatus::Unavailable(
            "claude unavailable".to_string(),
        ));
        state.set_codex_usage(crate::codex_usage::CodexUsageStatus::Unavailable(
            "codex unavailable".to_string(),
        ));
        assert_eq!(
            usage_quota_label(&state).as_deref(),
            Some("Codex usage unavailable: codex unavailable")
        );
    }

    #[test]
    fn inline_config_shortcut_does_not_open_picker() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![
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
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::F(2)));

        let backend = TestBackend::new(100, INLINE_CHAT_HEIGHT);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(!rendered.contains("Mode values"), "rendered:\n{rendered}");
        assert!(state.config_picker.is_none());
    }

    #[test]
    fn ctrl_n_triggers_new_session_exit_reason() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "model-1",
            vec![
                SessionConfigSelectOption::new("model-1", "Model 1"),
                SessionConfigSelectOption::new("model-2", "Model 2"),
            ],
        )];
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('n'), KeyModifiers::CONTROL),
        );

        assert!(state.config_picker.is_none());
        assert_eq!(state.exit_reason, Some(UiExitReason::NewSession));
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn ctrl_o_triggers_load_session_exit_reason() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "model-1",
            vec![
                SessionConfigSelectOption::new("model-1", "Model 1"),
                SessionConfigSelectOption::new("model-2", "Model 2"),
            ],
        )];
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('o'), KeyModifiers::CONTROL),
        );

        assert!(state.config_picker.is_none());
        assert_eq!(state.exit_reason, Some(UiExitReason::LoadSession));
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn config_picker_renders_no_matches_state() {
        let mut state = AppState::new();
        state.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "model-1",
            vec![
                SessionConfigSelectOption::new("model-1", "Model 1"),
                SessionConfigSelectOption::new("model-2", "Model 2"),
            ],
        )];
        assert!(state.open_config_value_picker(0));
        state.config_picker_set_search("zzz");

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_config_value_picker_modal(frame, frame.area(), &state))
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let rendered_lines: Vec<String> = (0..buffer.area().height)
            .map(|y| {
                (0..buffer.area().width)
                    .map(|x| buffer.cell((x, y)).expect("cell").symbol())
                    .collect()
            })
            .collect();

        assert!(
            rendered_lines
                .iter()
                .any(|line| line.contains("No matches")),
            "rendered lines: {rendered_lines:?}"
        );
        assert!(
            rendered_lines
                .iter()
                .any(|line| line.contains("Backspace to clear")),
            "rendered lines: {rendered_lines:?}"
        );
    }

    #[test]
    fn bracketed_paste_appends_cleaned_text_to_input() {
        let mut state = AppState::new();
        state.input = "prefix ".to_string();
        state.input_cursor = state.input.chars().count();

        handle_paste(&mut state, "hello\nworld\r\n!");

        assert_eq!(state.input, "prefix hello\nworld\n!");
        assert_eq!(state.input_cursor, state.input.chars().count());
    }

    #[test]
    fn bracketed_paste_inserts_cleaned_text_at_cursor() {
        let mut state = AppState::new();
        state.input = "before after".to_string();
        state.input_cursor = "before ".chars().count();

        handle_paste(&mut state, "pasted ");

        assert_eq!(state.input, "before pasted after");
        assert_eq!(state.input_cursor, "before pasted ".chars().count());
    }

    #[test]
    fn bracketed_paste_strips_control_characters_except_tab_and_newline() {
        let mut state = AppState::new();

        handle_paste(&mut state, "a\x00b\x07c\t\t\n");

        assert_eq!(state.input, "abc\t\t\n");
    }

    #[test]
    fn bracketed_paste_normalizes_carriage_returns_to_newlines() {
        let mut state = AppState::new();

        handle_paste(&mut state, "one\rtwo\rthree");

        assert_eq!(state.input, "one\ntwo\nthree");
        assert!(state.attachments.is_empty());
    }

    #[test]
    fn shift_enter_inserts_newline_without_submitting() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "line 1".to_string();
        state.input_cursor = state.input.chars().count();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Enter, KeyModifiers::SHIFT),
        );

        assert_eq!(state.input, "line 1\n");
        assert!(cmd_rx.try_recv().is_err(), "must not submit");
    }

    #[test]
    fn alt_enter_inserts_newline_without_submitting() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "first".to_string();
        state.input_cursor = state.input.chars().count();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Enter, KeyModifiers::ALT),
        );

        assert_eq!(state.input, "first\n");
        assert!(cmd_rx.try_recv().is_err(), "must not submit");
    }

    #[test]
    fn ctrl_j_inserts_newline_without_submitting() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "first".to_string();
        state.input_cursor = state.input.chars().count();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('j'), KeyModifiers::CONTROL),
        );

        assert_eq!(state.input, "first\n");
        assert!(cmd_rx.try_recv().is_err(), "must not submit");
    }

    #[test]
    fn prompt_cursor_moves_and_edits_in_place() {
        let mut state = AppState::new();
        state.input = "ab".to_string();
        state.input_cursor = 1;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('x')));
        assert_eq!(state.input, "axb");
        assert_eq!(state.input_cursor, 2);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Backspace));
        assert_eq!(state.input, "ab");
        assert_eq!(state.input_cursor, 1);
    }

    #[test]
    fn prompt_cursor_arrows_move_through_lines() {
        let mut state = AppState::new();
        state.input = "abc\ndef".to_string();
        state.input_cursor = state.input.chars().count();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Up));
        assert_eq!(state.input_cursor, 3);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Down));
        assert_eq!(state.input_cursor, 7);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Home));
        assert_eq!(state.input_cursor, 4);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::End));
        assert_eq!(state.input_cursor, 7);
    }

    #[test]
    fn prompt_ctrl_a_and_ctrl_e_jump_to_line_edges() {
        let mut state = AppState::new();
        state.input = "abc\ndef".to_string();
        state.input_cursor = state.input.chars().count();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.input_cursor, 4);

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('e'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.input_cursor, 7);
    }

    #[test]
    fn prompt_ctrl_b_and_ctrl_f_move_one_character() {
        let mut state = AppState::new();
        state.input = "abc".to_string();
        state.input_cursor = 1;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('b'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.input_cursor, 0);

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('f'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.input_cursor, 1);
    }

    #[test]
    fn ctrl_r_requests_voice_dictation_start() {
        let state = AppState::new();

        assert!(state.input.is_empty());
        assert_eq!(
            dictation_request_for_state(&state, true),
            TerminalRequest::StartDictation
        );
    }

    #[test]
    fn ctrl_r_requests_voice_dictation_stop_when_active() {
        let mut state = AppState::new();
        state.voice_input_active = true;

        assert_eq!(
            dictation_request_for_state(&state, true),
            TerminalRequest::StopDictation
        );
    }

    #[test]
    fn enter_while_dictating_submits_prompt_and_requests_stop() {
        let mut state = ready_state_with_session();
        state.voice_input_active = true;
        state.input = "spoken prompt".to_string();
        state.input_cursor = state.input.chars().count();
        state.voice_input_range = Some((0, state.input_cursor));
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();

        let request = handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));

        assert_eq!(request, TerminalRequest::StopDictation);
        match cmd_rx.try_recv().expect("spoken prompt dispatched") {
            UiCommand::SendPrompt { text, images } => {
                assert_eq!(text, "spoken prompt");
                assert!(images.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn ctrl_r_is_ignored_when_voice_input_is_unsupported() {
        let mut state = AppState::new();

        assert_eq!(
            dictation_request_for_state(&state, false),
            TerminalRequest::None
        );

        state.voice_input_active = true;
        assert_eq!(
            dictation_request_for_state(&state, false),
            TerminalRequest::None
        );
    }

    #[test]
    fn android_prompt_title_hides_voice_shortcut() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        let title = idle_prompt_title(&state, false, "");

        assert!(!title.contains("Ctrl-R"));
        assert!(!title.contains("voice"));
    }

    #[test]
    fn android_help_hides_voice_shortcut() {
        let help = general_help_lines(false, TerminalThemeKind::Dark.palette())
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!help.contains("Ctrl-R"));
        assert!(!help.contains("dictation"));
    }

    #[test]
    fn help_revisits_the_three_role_product_model() {
        let help = help_modal_lines(UiMode::InlineChat, false, TerminalThemeKind::Dark.palette())
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        for expected in [
            "owns the request",
            "fresh write-capable sessions",
            "read-only intent analyst",
            "every changed turn",
            "accounted separately",
        ] {
            assert!(help.contains(expected), "missing {expected:?}:\n{help}");
        }
    }

    #[test]
    fn help_lines_style_headings_bindings_and_descriptions_separately() {
        let theme = TerminalThemeKind::Dark.palette();
        let lines = help_modal_lines(UiMode::InlineChat, false, theme);

        let heading = lines
            .iter()
            .find(|line| line_text(line) == "General")
            .expect("general heading");
        assert_eq!(heading.spans[0].style.fg, Some(theme.header));
        assert!(heading.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(
            heading.spans[0]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );

        let ctrl_n = lines
            .iter()
            .find(|line| line_text(line).contains("Ctrl-N"))
            .expect("Ctrl-N line");
        let binding = ctrl_n
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "Ctrl-N")
            .expect("binding span");
        assert_eq!(binding.style.fg, Some(theme.accent));
        assert!(binding.style.add_modifier.contains(Modifier::BOLD));

        let description = ctrl_n
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "new session")
            .expect("description span");
        assert_eq!(description.style.fg, Some(theme.text));
        assert!(!description.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn stopping_dictation_keeps_live_prompt_text() {
        let mut state = AppState::new();
        state.voice_input_active = true;
        state.input = "hello".to_string();
        state.input_cursor = state.input.chars().count();
        state.voice_input_range = Some((0, state.input_cursor));
        let (cancel_tx, _cancel_rx) = std_mpsc::channel();
        let mut cancel_tx = Some(cancel_tx);

        stop_dictation(&mut state, &mut cancel_tx);
        finish_dictation(&mut state, Ok("ignored".to_string()));

        assert!(!state.voice_input_active);
        assert!(state.voice_input_range.is_none());
        assert_eq!(state.input, "hello");
        assert!(cancel_tx.is_none());
    }

    #[test]
    fn exit_cancels_dictation_without_status_message() {
        let mut state = AppState::new();
        state.voice_input_active = true;
        state.voice_input_level = Some(0.5);
        state.voice_input_range = Some((0, 0));
        let (cancel_tx, cancel_rx) = std_mpsc::channel();
        let mut cancel_tx = Some(cancel_tx);

        cancel_dictation_for_exit(&mut state, &mut cancel_tx);

        assert!(!state.voice_input_active);
        assert!(state.voice_input_range.is_none());
        assert!(state.voice_input_level.is_none());
        assert!(state.status_line.is_none());
        assert!(cancel_tx.is_none());
        assert!(cancel_rx.try_recv().is_ok());
    }

    #[test]
    fn dictation_level_updates_voice_meter_state() {
        let mut state = AppState::new();
        state.voice_input_active = true;

        update_dictation_level(&mut state, 1.7);

        assert_eq!(state.voice_input_level, Some(1.0));
        assert_eq!(voice_level_meter(state.voice_input_level), "[||||||||||]");
    }

    #[test]
    fn voice_level_meter_renders_empty_when_no_level_seen() {
        assert_eq!(voice_level_meter(None), "[..........]");
        assert_eq!(voice_level_meter(Some(0.35)), "[||||......]");
    }

    #[test]
    fn dictation_prompt_title_shows_setup_status_before_microphone_levels() {
        let mut state = AppState::new();
        state.voice_input_active = true;
        state.voice_input_level = None;
        state.status_line = Some(StatusMessage::info(
            "downloading voice model (one-time): 42% of 464 MB",
        ));

        let title = dictation_prompt_title(&state);

        assert!(title.contains("downloading voice model (one-time): 42% of 464 MB"));
        assert!(title.contains("Ctrl-R stop"));
    }

    #[test]
    fn dictation_prompt_title_switches_to_meter_after_microphone_levels_arrive() {
        let mut state = AppState::new();
        state.voice_input_active = true;
        state.voice_input_level = Some(0.35);
        state.status_line = Some(StatusMessage::info("listening..."));

        let title = dictation_prompt_title(&state);

        assert!(title.contains("[||||......]"));
        assert!(!title.contains("listening..."));
    }

    #[tokio::test]
    async fn starting_dictation_shows_preparing_until_microphone_levels_arrive() {
        let mut state = AppState::new();
        let (dictation_tx, _dictation_rx) = mpsc::unbounded_channel();
        let mut cancel_tx = None;

        start_dictation(&mut state, &dictation_tx, &mut cancel_tx);

        assert!(state.voice_input_active);
        assert!(state.voice_input_level.is_none());
        let status = state.status_line.as_ref().expect("status");
        assert_eq!(status.kind, StatusKind::Info);
        assert_eq!(status.text, "preparing voice input...");
        assert!(cancel_tx.is_some());
        stop_dictation(&mut state, &mut cancel_tx);
    }

    #[test]
    fn dictation_partial_updates_prompt_text() {
        let mut state = AppState::new();
        state.input = "before after".to_string();
        state.input_cursor = "before ".chars().count();
        state.voice_input_active = true;
        state.voice_input_range = Some((state.input_cursor, state.input_cursor));

        update_dictation_partial(&mut state, "hello");
        update_dictation_partial(&mut state, "hello world ");

        assert_eq!(state.input, "before hello world after");
        assert_eq!(state.input_cursor, "before hello world ".chars().count());
        let status = state.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Info);
        assert_eq!(status.text, "listening...");
    }

    #[test]
    fn dictation_finish_replaces_live_partial_text() {
        let mut state = AppState::new();
        state.input = "before after".to_string();
        state.input_cursor = "before ".chars().count();
        state.voice_input_active = true;
        state.voice_input_range = Some((state.input_cursor, state.input_cursor));

        update_dictation_partial(&mut state, "rough draft");
        finish_dictation(&mut state, Ok("voice ".to_string()));

        assert!(!state.voice_input_active);
        assert_eq!(state.input, "before voice after");
        assert_eq!(state.input_cursor, "before voice ".chars().count());
        assert!(state.voice_input_range.is_none());
    }

    #[test]
    fn prompt_ctrl_k_and_ctrl_u_delete_to_line_edges() {
        let mut state = AppState::new();
        state.input = "hello world".to_string();
        state.input_cursor = 5;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.input, "hello");
        assert_eq!(state.input_cursor, 5);

        state.input = "hello world".to_string();
        state.input_cursor = 5;

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('u'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.input, " world");
        assert_eq!(state.input_cursor, 0);
    }

    #[test]
    fn prompt_word_shortcuts_move_and_delete_words() {
        let mut state = AppState::new();
        state.input = "hello world".to_string();
        state.input_cursor = state.input.chars().count();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('b'), KeyModifiers::ALT),
        );
        assert_eq!(state.input_cursor, 6);

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('f'), KeyModifiers::ALT),
        );
        assert_eq!(state.input_cursor, 11);

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('w'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.input, "hello ");
        assert_eq!(state.input_cursor, 6);

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Backspace, KeyModifiers::ALT),
        );
        assert_eq!(state.input, "");
        assert_eq!(state.input_cursor, 0);
    }

    #[test]
    fn prompt_ctrl_d_deletes_char_or_quits_when_empty() {
        let mut state = AppState::new();
        state.input = "ab".to_string();
        state.input_cursor = 0;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('d'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.input, "b");
        assert_eq!(state.input_cursor, 0);

        let mut empty = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut empty,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('d'), KeyModifiers::CONTROL),
        );
        assert_eq!(empty.exit_reason, Some(UiExitReason::Quit));
    }

    #[test]
    fn input_cursor_tracks_last_line_in_multiline_buffer() {
        let area = Rect::new(2, 3, 40, 10);

        let (x, y) = input_cursor_position(area, "hello", 5, 0, 0);
        assert_eq!((x, y), (7, 3));

        let (x, y) = input_cursor_position(area, "line one\nsecond", 15, 0, 0);
        assert_eq!((x, y), (8, 4));

        let (x, y) = input_cursor_position(area, "a\nbb\nccc", 8, 0, 0);
        assert_eq!((x, y), (5, 5));
    }

    #[test]
    fn input_cursor_does_not_panic_on_narrow_terminal() {
        // width=1, height=1: no room for content, but must not panic
        let area = Rect::new(0, 0, 1, 1);
        let (x, y) = input_cursor_position(area, "abc\ndef", 7, 0, 0);
        assert_eq!((x, y), (0, 0));
    }

    #[test]
    fn input_cursor_scrolls_with_offset() {
        let area = Rect::new(0, 0, 40, 5); // inner height = 3 visible lines
        // 5 lines, cursor on line 5 (index 4), scroll offset = 2
        let (x, y) = input_cursor_position(area, "a\nb\nc\nd\ne", 9, 0, 2);
        assert_eq!((x, y), (1, 2));
    }

    #[test]
    fn input_cursor_accounts_for_chip_rows() {
        let area = Rect::new(0, 0, 40, 10);
        // Single line "hello" at text row 0, but 2 chip rows above.
        let (x, y) = input_cursor_position(area, "hello", 5, 2, 0);
        assert_eq!((x, y), (5, 2));
    }

    #[test]
    fn input_cursor_uses_display_width_for_wrapped_prompt() {
        let area = Rect::new(0, 0, 4, 3);
        let (x, y) = input_cursor_position(area, "ab界c", 4, 0, 0);
        assert_eq!((x, y), (1, 1));
    }

    #[test]
    fn input_wrapping_keeps_glyph_wider_than_row() {
        let layout = input_wrapped_layout("界", 1, 1);
        assert_eq!(layout.rows, vec!["界".to_string()]);
        assert_eq!(layout.cursor_row, 0);
        assert_eq!(layout.cursor_col, 1);
    }

    #[test]
    fn prompt_word_wraps_input_so_cursor_tracks_insert_position() {
        let mut state = AppState::new();
        state.input = "hello abcdef".to_string();
        state.input_cursor = state.input.chars().count();

        let mut terminal = Terminal::new(TestBackend::new(14, 6)).expect("terminal");
        terminal
            .draw(|frame| draw_input(frame, frame.area(), &state, UiMode::FullscreenTui))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer());
        assert!(
            rendered.iter().any(|line| line.contains("hello ")),
            "first wrapped row missing; rendered:\n{}",
            rendered.join("\n")
        );
        assert!(
            rendered.iter().any(|line| line.contains("abcdef")),
            "second wrapped row missing; rendered:\n{}",
            rendered.join("\n")
        );
        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(9, 3));
    }

    #[test]
    fn multiline_submit_sends_trimmed_text() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "line one\nline two\nline three".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        let cmd = cmd_rx.try_recv().expect("prompt was sent");
        match cmd {
            UiCommand::SendPrompt { text, images } => {
                assert_eq!(text, "line one\nline two\nline three");
                assert!(images.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(state.input.is_empty());
    }

    #[test]
    fn paste_over_three_lines_creates_attachment_chip_at_cursor() {
        let mut state = AppState::new();
        state.attachments = Vec::new();
        state.input = "typed".to_string();
        state.input_cursor = 0;

        handle_paste(&mut state, "a\nb\nc\nd");

        assert_eq!(state.input, "typed");
        assert_eq!(state.attachments.len(), 1);
        assert_eq!(state.attachments[0].position, 0);
        assert_eq!(state.attachments[0].content, "a\nb\nc\nd");
    }

    #[test]
    fn paste_over_three_carriage_return_lines_creates_attachment_chip_at_cursor() {
        let mut state = AppState::new();
        state.input = "before after".to_string();
        state.input_cursor = "before ".chars().count();

        handle_paste(&mut state, "a\rb\rc\rd\re");

        assert_eq!(state.input, "before after");
        assert_eq!(state.attachments.len(), 1);
        assert_eq!(state.attachments[0].position, "before ".chars().count());
        assert_eq!(state.attachments[0].content, "a\nb\nc\nd\ne");
    }

    #[test]
    fn bracketed_paste_event_creates_attachment_chip_at_cursor() {
        let mut state = AppState::new();
        state.input = "typed".to_string();
        state.input_cursor = state.input.chars().count();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            CtEvent::Paste("a\rb\rc\rd\re".to_string()),
        );

        assert_eq!(state.input, "typed");
        assert_eq!(state.attachments.len(), 1);
        assert_eq!(state.attachments[0].position, "typed".chars().count());
        assert_eq!(state.attachments[0].content, "a\nb\nc\nd\ne");
    }

    #[test]
    fn text_attachment_chip_renders_at_its_cursor_position() {
        let mut state = AppState::new();
        state.input = "this is me typing my text".to_string();
        state.input_cursor = 0;

        handle_paste(&mut state, "a\nb\nc\nd");

        let rendered: Vec<String> = input_lines_with_attachments(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered[0].starts_with("📎 4 lines"),
            "chip should render before text when pasted at cursor 0: {rendered:?}"
        );
        assert!(
            rendered[0].ends_with("this is me typing my text"),
            "text should stay on the chip line when it fits: {rendered:?}"
        );

        let mut state = AppState::new();
        state.input = "this is me typing my text".to_string();
        state.input_cursor = state.input.chars().count();

        handle_paste(&mut state, "a\nb\nc\nd");

        let rendered: Vec<String> = input_lines_with_attachments(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered[0].starts_with("this is me typing my text📎 4 lines"),
            "chip should render inline after text when pasted at the end: {rendered:?}"
        );
    }

    #[test]
    fn image_attachment_chip_renders_at_its_cursor_position() {
        let mut state = AppState::new();
        state.input = "describe this".to_string();
        state.input_cursor = 0;

        attach_clipboard_image(&mut state, test_clipboard_image());

        let rendered: Vec<String> = input_lines_with_attachments(&state, 120)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered[0].starts_with("🖼 image 640x480"),
            "image chip should render before text when attached at cursor 0: {rendered:?}"
        );
        assert!(
            rendered[0].ends_with("describe this"),
            "text should stay on the image chip line when it fits: {rendered:?}"
        );

        let mut state = AppState::new();
        state.input = "describe this".to_string();
        state.input_cursor = state.input.chars().count();

        attach_clipboard_image(&mut state, test_clipboard_image());

        let rendered: Vec<String> = input_lines_with_attachments(&state, 120)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered[0].starts_with("describe this🖼 image 640x480"),
            "image chip should render inline after text when attached at the end: {rendered:?}"
        );
    }

    #[test]
    fn pasting_image_path_creates_image_chip_when_supported() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("pasted image.png");
        write_test_png(&path);
        let mut state = AppState::new();
        state.prompt_images_supported = true;

        handle_paste(&mut state, &format!("'{}'", path.display()));

        assert!(state.input.is_empty());
        assert!(state.attachments.is_empty());
        assert_eq!(state.image_attachments.len(), 1);
        assert_eq!(state.image_attachments[0].width, 2);
        assert_eq!(state.image_attachments[0].height, 3);
    }

    #[test]
    fn pasting_file_url_image_path_creates_image_chip_when_supported() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("pasted-url.png");
        write_test_png(&path);
        let url = url::Url::from_file_path(&path).expect("file url");
        let mut state = AppState::new();
        state.prompt_images_supported = true;

        handle_paste(&mut state, url.as_str());

        assert!(state.input.is_empty());
        assert_eq!(state.image_attachments.len(), 1);
    }

    #[test]
    fn pasting_image_path_stays_text_when_agent_does_not_support_images() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("unsupported.png");
        write_test_png(&path);
        let mut state = AppState::new();

        handle_paste(&mut state, &path.to_string_lossy());

        assert_eq!(state.input, path.to_string_lossy());
        assert!(state.image_attachments.is_empty());
    }

    #[test]
    fn fast_typed_image_path_burst_becomes_image_chip() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("dragged.png");
        write_test_png(&path);
        let path_text = path.to_string_lossy();
        let mut state = AppState::new();
        state.prompt_images_supported = true;
        let start = Instant::now();

        for (i, ch) in path_text.chars().enumerate() {
            let cursor_before_insert = state.input_cursor;
            insert_text_at_cursor(&mut state, &ch.to_string());
            note_plain_input_char(
                &mut state,
                cursor_before_insert,
                ch,
                start + Duration::from_millis(i as u64),
            );
        }

        assert_eq!(state.input, path_text);
        assert!(flush_input_paste_burst_if_due(
            &mut state,
            start + Duration::from_millis(100),
            false,
        ));
        assert!(state.input.is_empty());
        assert_eq!(state.image_attachments.len(), 1);
        assert_eq!(state.image_attachments[0].width, 2);
        assert_eq!(state.image_attachments[0].height, 3);
    }

    #[test]
    fn slow_typed_image_path_does_not_become_image_chip() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("typed.png");
        write_test_png(&path);
        let path_text = path.to_string_lossy();
        let mut state = AppState::new();
        state.prompt_images_supported = true;
        let start = Instant::now();

        for (i, ch) in path_text.chars().enumerate() {
            let cursor_before_insert = state.input_cursor;
            insert_text_at_cursor(&mut state, &ch.to_string());
            note_plain_input_char(
                &mut state,
                cursor_before_insert,
                ch,
                start + Duration::from_millis((i as u64) * 20),
            );
        }

        assert!(!flush_input_paste_burst_if_due(
            &mut state,
            start + Duration::from_secs(5),
            false,
        ));
        assert_eq!(state.input, path_text);
        assert!(state.image_attachments.is_empty());
    }

    #[test]
    fn forced_fast_typed_image_path_flushes_before_enter() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("enter.png");
        write_test_png(&path);
        let path_text = path.to_string_lossy();
        let mut state = AppState::new();
        state.prompt_images_supported = true;
        let start = Instant::now();

        for (i, ch) in path_text.chars().enumerate() {
            let cursor_before_insert = state.input_cursor;
            insert_text_at_cursor(&mut state, &ch.to_string());
            note_plain_input_char(
                &mut state,
                cursor_before_insert,
                ch,
                start + Duration::from_millis(i as u64),
            );
        }

        assert!(flush_input_paste_burst_if_due(
            &mut state,
            start + Duration::from_millis(1),
            true,
        ));
        assert!(state.input.is_empty());
        assert_eq!(state.image_attachments.len(), 1);
    }

    #[test]
    fn attach_clipboard_image_creates_image_chip() {
        let mut state = AppState::new();

        attach_clipboard_image(&mut state, test_clipboard_image());

        assert_eq!(state.image_attachments.len(), 1);
        assert_eq!(state.image_attachments[0].mime_type, "image/png");
        assert_eq!(state.image_attachments[0].width, 640);
        assert_eq!(state.image_attachments[0].height, 480);
        assert_eq!(state.image_attachments[0].byte_len, 12_345);
    }

    #[test]
    fn ctrl_v_warns_when_agent_does_not_support_images() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('v'), KeyModifiers::CONTROL),
        );

        let status = state.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Warning);
        assert_eq!(
            status.text,
            "this agent does not advertise image prompt support"
        );
        assert!(state.input.is_empty());
        assert!(state.image_attachments.is_empty());
    }

    #[test]
    fn paste_three_or_fewer_lines_stays_inline() {
        let mut state = AppState::new();

        handle_paste(&mut state, "hello\nworld\r\n!");

        assert_eq!(state.input, "hello\nworld\n!");
        assert!(state.attachments.is_empty());
    }

    #[test]
    fn backspace_on_empty_input_removes_last_attachment() {
        let mut state = AppState::new();
        state.attachments.push(crate::app::PastedAttachment {
            id: 1,
            position: 0,
            content: "first".to_string(),
        });
        state.attachments.push(crate::app::PastedAttachment {
            id: 2,
            position: 0,
            content: "second".to_string(),
        });
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Backspace));

        assert_eq!(
            state.attachments.len(),
            1,
            "only the last chip should be removed"
        );
        assert_eq!(state.attachments[0].id, 1);
    }

    #[test]
    fn backspace_at_text_attachment_position_removes_that_chip() {
        let mut state = AppState::new();
        state.input = "typed".to_string();
        state.input_cursor = state.input.chars().count();
        state.attachments.push(crate::app::PastedAttachment {
            id: 1,
            position: state.input_cursor,
            content: "a\nb\nc\nd".to_string(),
        });
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Backspace));

        assert!(state.attachments.is_empty());
        assert_eq!(state.input, "typed");
        assert_eq!(state.input_cursor, "typed".chars().count());
    }

    #[test]
    fn backspace_at_image_attachment_position_removes_that_chip() {
        let mut state = AppState::new();
        state.input = "typed".to_string();
        state.input_cursor = state.input.chars().count();
        let mut image = test_image_attachment_with_id(1);
        image.position = state.input_cursor;
        state.image_attachments.push(image);
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Backspace));

        assert!(state.image_attachments.is_empty());
        assert_eq!(state.input, "typed");
        assert_eq!(state.input_cursor, "typed".chars().count());
    }

    #[test]
    fn backspace_on_empty_input_removes_last_image_attachment() {
        let mut state = AppState::new();
        state.attachments.push(crate::app::PastedAttachment {
            id: 1,
            position: 0,
            content: "first".to_string(),
        });
        state
            .image_attachments
            .push(test_image_attachment_with_id(2));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Backspace));

        assert_eq!(state.attachments.len(), 1);
        assert!(state.image_attachments.is_empty());
    }

    #[test]
    fn submit_combines_attachment_contents_and_input_text() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.attachments.push(crate::app::PastedAttachment {
            id: 1,
            position: 0,
            content: "pasted-1".to_string(),
        });
        state.attachments.push(crate::app::PastedAttachment {
            id: 2,
            position: 0,
            content: "pasted-2".to_string(),
        });
        state.input = "typed".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        let cmd = cmd_rx.try_recv().expect("prompt was sent");
        match cmd {
            UiCommand::SendPrompt { text, images } => {
                assert_eq!(text, "pasted-1\npasted-2\ntyped");
                assert!(images.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(state.input.is_empty());
        assert!(state.attachments.is_empty());
    }

    #[test]
    fn submit_sends_text_and_image_blocks() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state
            .image_attachments
            .push(test_image_attachment_with_id(1));
        state.input = "describe this".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        let cmd = cmd_rx.try_recv().expect("prompt was sent");
        match cmd {
            UiCommand::SendPrompt { text, images } => {
                assert_eq!(text, "describe this");
                assert_eq!(images.len(), 1);
                assert_eq!(images[0].data_base64, "aW1hZ2U=");
                assert_eq!(images[0].mime_type, "image/png");
                assert_eq!(images[0].width, 640);
                assert_eq!(images[0].height, 480);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(state.input.is_empty());
        assert!(state.image_attachments.is_empty());
        assert!(matches!(
            state.transcript.last(),
            Some(Entry::UserPrompt(text)) if text == "describe this\n[image]"
        ));
    }

    #[test]
    fn startup_submission_queues_once_and_commits_on_session_readiness() {
        let mut state = AppState::new();
        state.set_primary_acp_name("Codex");
        state
            .image_attachments
            .push(test_image_attachment_with_id(1));
        state.input = "describe this".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);
        submit_prompt(&mut state, &cmd_tx);

        let queued = cmd_rx.try_recv().expect("startup prompt queued");
        match queued {
            UiCommand::SendPrompt { text, images } => {
                assert_eq!(text, "describe this");
                assert_eq!(images.len(), 1);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(
            cmd_rx.try_recv().is_err(),
            "second Enter must not duplicate"
        );
        assert!(state.has_startup_prompt());
        assert_eq!(state.input, "describe this");
        assert_eq!(state.image_attachments.len(), 1);
        let status = state.status_line.as_ref().expect("status");
        assert_eq!(status.kind, StatusKind::Info);
        assert_eq!(status.text, "session is still starting");
        assert!(state.transcript.is_empty());

        state.apply_event(UiEvent::Connected {
            agent_name: Some("slow Codex".into()),
            agent_version: None,
            prompt_images_supported: true,
            session_fork_supported: false,
            side_session_supported: false,
            side_session_unsupported_reason: None,
        });
        finalize_startup_prompt(&mut state);
        assert_eq!(state.input, "describe this", "Connected is not ready");
        assert!(state.has_startup_prompt());

        state.apply_event(UiEvent::SessionStarted {
            session_id: "slow-session".into(),
            resumed: false,
        });
        finalize_startup_prompt(&mut state);

        assert!(!state.has_startup_prompt());
        assert!(state.input.is_empty());
        assert!(state.image_attachments.is_empty());
        assert!(state.is_streaming());
        assert!(matches!(
            state.transcript.last(),
            Some(Entry::UserPrompt(text)) if text == "describe this\n[image]"
        ));
        finalize_startup_prompt(&mut state);
        assert!(cmd_rx.try_recv().is_err(), "readiness must not resend");
    }

    #[test]
    fn startup_failure_preserves_the_submitted_editor_draft() {
        let mut state = AppState::new();
        state.input = "recover this prompt".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(UiCommand::SendPrompt { text, .. }) if text == "recover this prompt"
        ));

        state.apply_event(UiEvent::Fatal("startup failed".to_string()));
        finalize_startup_prompt(&mut state);

        assert_eq!(state.input, "recover this prompt");
        assert!(state.has_startup_prompt());
        assert!(state.runtime_closed);
        assert_eq!(state.connection_state(), ConnectionState::Fatal);
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn ragnarok_command_without_task_warns_usage() {
        let mut state = AppState::new();
        state.input = "/ragnarok".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        submit_prompt(&mut state, &cmd_tx);
        assert!(cmd_rx.try_recv().is_err(), "nothing goes to the agent");
        assert!(state.take_ragnarok_launch().is_none());
        assert_eq!(state.prompt_history(), vec!["/ragnarok".to_string()]);
        let status = state.status_line.clone().expect("status");
        assert_eq!(status.kind, StatusKind::Warning);
        assert!(status.text.contains("usage: /ragnarok"));
    }

    #[test]
    fn ragnarok_command_requests_launch_without_touching_the_agent() {
        let mut state = AppState::new();
        // No session, runtime not even connected: /ragnarok must still work —
        // battles run on their own ACP connections.
        state.input = "/ragnarok forge me a hammer".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        submit_prompt(&mut state, &cmd_tx);
        assert!(cmd_rx.try_recv().is_err(), "nothing goes to the agent");
        assert_eq!(
            state.take_ragnarok_launch().as_deref(),
            Some("forge me a hammer")
        );
        assert_eq!(
            state.prompt_history(),
            vec!["/ragnarok forge me a hammer".to_string()]
        );
        assert!(state.input.is_empty());
        assert!(state.prompt_history_previous());
        assert_eq!(state.input, "/ragnarok forge me a hammer");
        assert!(matches!(
            state.transcript.last(),
            Some(Entry::System(text)) if text.contains("Ragnarok summoned")
        ));
    }

    #[test]
    fn active_thor_host_uses_current_agent_and_model_config() {
        let mut state = AppState::new();
        state.agent_source_id = "anvil".to_string();
        state.active_agent_launch = Some(ragnarok::Launch {
            program: PathBuf::from("anvil"),
            args: vec!["--max-turns".to_string(), "7".to_string()],
            env: HashMap::from([("ANVIL_TEST".to_string(), "1".to_string())]),
        });
        state.session_config_options = vec![
            SessionConfigOption::select(
                "mode",
                "Mode",
                "code",
                vec![SessionConfigSelectOption::new("code", "Code")],
            )
            .category(Some(SessionConfigOptionCategory::Mode)),
            SessionConfigOption::select(
                "model",
                "Model",
                "codex::gpt-5-codex",
                vec![
                    SessionConfigSelectOption::new("codex::gpt-5-codex", "GPT-5 Codex"),
                    SessionConfigSelectOption::new("bedrock::us.anthropic.claude-opus-4-8", "Opus"),
                ],
            )
            .category(Some(SessionConfigOptionCategory::Model)),
        ];

        let host = active_thor_host(&state).expect("host");

        assert_eq!(host.agent_source_id, "anvil");
        assert_eq!(host.launch.program, PathBuf::from("anvil"));
        assert_eq!(host.launch.args, vec!["--max-turns", "7"]);
        assert_eq!(
            host.launch.env.get("ANVIL_TEST").map(String::as_str),
            Some("1")
        );
        assert_eq!(host.model_value.as_deref(), Some("codex::gpt-5-codex"));
        assert_eq!(host.model_name.as_deref(), Some("GPT-5 Codex"));
    }

    #[test]
    fn ragnarok_command_warns_when_battle_already_raging() {
        let mut state = AppState::new();
        let (abort_tx, _abort_rx) = tokio::sync::watch::channel(false);
        let (proceed_tx, _proceed_rx) = tokio::sync::watch::channel(false);
        state.ragnarok = Some(RagnarokUi::new("first".into(), abort_tx, proceed_tx));
        state.input = "/ragnarok second task".to_string();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        submit_prompt(&mut state, &cmd_tx);
        assert!(state.take_ragnarok_launch().is_none());
        assert_eq!(
            state.prompt_history(),
            vec!["/ragnarok second task".to_string()]
        );
        let status = state.status_line.clone().expect("status");
        assert_eq!(status.kind, StatusKind::Warning);
        assert!(status.text.contains("already raging"));
    }

    #[test]
    fn ragnarok_prefix_must_be_word_aligned() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "/ragnarokish".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        submit_prompt(&mut state, &cmd_tx);
        // Not our command: falls through to a normal prompt send.
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(UiCommand::SendPrompt { text, .. }) if text == "/ragnarokish"
        ));
        assert!(state.take_ragnarok_launch().is_none());
    }

    #[test]
    fn ragnarok_keys_drive_the_arena() {
        let mut state = AppState::new();
        let (abort_tx, abort_rx) = tokio::sync::watch::channel(false);
        let (proceed_tx, proceed_rx) = tokio::sync::watch::channel(false);
        state.ragnarok = Some(RagnarokUi::new("task".into(), abort_tx, proceed_tx));
        state.apply_ragnarok_event(ragnarok::RagnarokEvent::Roster(vec![
            crate::ragnarok::FighterCard {
                id: 0,
                agent_source_id: "a".into(),
                model_value: "m0".into(),
                model_name: "M0".into(),
                pass_at_1_bps: 1400,
                mean_cost_usd: 0.0,
            },
            crate::ragnarok::FighterCard {
                id: 1,
                agent_source_id: "b".into(),
                model_value: "m1".into(),
                model_name: "M1".into(),
                pass_at_1_bps: 1500,
                mean_cost_usd: 0.0,
            },
        ]));

        // Enter opens the selected fighter transcript during active combat.
        handle_ragnarok_key(
            &mut state,
            KeyModifiers::NONE,
            KeyCode::Enter,
            UiMode::InlineChat,
        );
        assert_eq!(state.ragnarok.as_ref().unwrap().pane, ArenaPane::Transcript);
        handle_ragnarok_key(
            &mut state,
            KeyModifiers::NONE,
            KeyCode::Enter,
            UiMode::InlineChat,
        );
        assert_eq!(state.ragnarok.as_ref().unwrap().pane, ArenaPane::Arena);
        // Tab remains as a compatibility alias, and t is the mnemonic fallback
        // when Enter is reserved for approval or closing.
        handle_ragnarok_key(
            &mut state,
            KeyModifiers::NONE,
            KeyCode::Char('t'),
            UiMode::InlineChat,
        );
        assert_eq!(state.ragnarok.as_ref().unwrap().pane, ArenaPane::Transcript);
        state.ragnarok.as_mut().unwrap().quit_armed = true;
        handle_ragnarok_key(
            &mut state,
            KeyModifiers::NONE,
            KeyCode::Tab,
            UiMode::InlineChat,
        );
        assert_eq!(state.ragnarok.as_ref().unwrap().pane, ArenaPane::Arena);
        assert!(
            !state.ragnarok.as_ref().unwrap().quit_armed,
            "pane toggles disarm the quit confirmation"
        );
        handle_ragnarok_key(
            &mut state,
            KeyModifiers::NONE,
            KeyCode::Enter,
            UiMode::InlineChat,
        );
        assert_eq!(state.ragnarok.as_ref().unwrap().pane, ArenaPane::Transcript);
        handle_ragnarok_key(
            &mut state,
            KeyModifiers::NONE,
            KeyCode::Esc,
            UiMode::InlineChat,
        );
        assert_eq!(
            state.ragnarok.as_ref().unwrap().pane,
            ArenaPane::Arena,
            "Esc exits the transcript pane"
        );
        // Arrows cycle fighters.
        handle_ragnarok_key(
            &mut state,
            KeyModifiers::NONE,
            KeyCode::Right,
            UiMode::InlineChat,
        );
        assert_eq!(state.ragnarok.as_ref().unwrap().selected_fighter, 1);
        for line in ["one", "two", "three"] {
            state.apply_ragnarok_event(ragnarok::RagnarokEvent::Log {
                fighter: None,
                text: line.to_string(),
            });
        }
        handle_ragnarok_key(
            &mut state,
            KeyModifiers::NONE,
            KeyCode::Up,
            UiMode::InlineChat,
        );
        assert_eq!(state.ragnarok.as_ref().unwrap().feed_scroll, 1);
        handle_ragnarok_key(
            &mut state,
            KeyModifiers::NONE,
            KeyCode::Down,
            UiMode::InlineChat,
        );
        assert_eq!(state.ragnarok.as_ref().unwrap().feed_scroll, 0);

        // Enter at the approval gate unleashes combat without closing.
        state.apply_ragnarok_event(ragnarok::RagnarokEvent::Phase(ragnarok::Phase::Approval));
        assert!(!*proceed_rx.borrow());
        handle_ragnarok_key(
            &mut state,
            KeyModifiers::NONE,
            KeyCode::Enter,
            UiMode::InlineChat,
        );
        assert!(state.ragnarok.is_some(), "gate Enter must not close");
        assert!(*proceed_rx.borrow(), "gate Enter fires the proceed watch");
        state.apply_ragnarok_event(ragnarok::RagnarokEvent::Phase(ragnarok::Phase::Combat));

        // q arms, second q fires the abort watch.
        handle_ragnarok_key(
            &mut state,
            KeyModifiers::NONE,
            KeyCode::Char('q'),
            UiMode::InlineChat,
        );
        assert!(state.ragnarok.as_ref().unwrap().quit_armed);
        assert!(!*abort_rx.borrow());
        handle_ragnarok_key(
            &mut state,
            KeyModifiers::NONE,
            KeyCode::Esc,
            UiMode::InlineChat,
        );
        assert!(
            !state.ragnarok.as_ref().unwrap().quit_armed,
            "Esc cancels quit confirmation"
        );
        assert!(!*abort_rx.borrow());
        handle_ragnarok_key(
            &mut state,
            KeyModifiers::NONE,
            KeyCode::Char('q'),
            UiMode::InlineChat,
        );
        handle_ragnarok_key(
            &mut state,
            KeyModifiers::NONE,
            KeyCode::Char('q'),
            UiMode::InlineChat,
        );
        assert!(*abort_rx.borrow(), "second q quits the battle");
        // The arena stays up until the battle task reports Failed/Done.
        assert!(state.ragnarok.is_some());
    }

    #[test]
    fn ragnarok_stage_height_uses_card_rows_instead_of_dead_air() {
        let mut state = AppState::new();
        let (abort_tx, _abort_rx) = tokio::sync::watch::channel(false);
        let (proceed_tx, _proceed_rx) = tokio::sync::watch::channel(false);
        state.ragnarok = Some(RagnarokUi::new("task".into(), abort_tx, proceed_tx));
        state.apply_ragnarok_event(ragnarok::RagnarokEvent::Roster(vec![
            crate::ragnarok::FighterCard {
                id: 0,
                agent_source_id: "a".into(),
                model_value: "m0".into(),
                model_name: "M0".into(),
                pass_at_1_bps: 1400,
                mean_cost_usd: 0.0,
            },
            crate::ragnarok::FighterCard {
                id: 1,
                agent_source_id: "b".into(),
                model_value: "m1".into(),
                model_name: "M1".into(),
                pass_at_1_bps: 1500,
                mean_cost_usd: 0.0,
            },
        ]));
        state.apply_ragnarok_event(ragnarok::RagnarokEvent::ThorAction(
            ragnarok::ThorAction::Deciding,
        ));
        state.apply_ragnarok_event(ragnarok::RagnarokEvent::Phase(ragnarok::Phase::Combat));
        let arena = state.ragnarok.as_ref().unwrap();

        assert_eq!(
            ragnarok_stage_height(arena, 200, 54),
            RAGNAROK_THOR_STRIP_HEIGHT + RAGNAROK_CARD_HEIGHT
        );
    }

    #[test]
    fn thor_descending_scene_uses_viking_figure() {
        let scene = thor_descending_scene_rows(0).join("\n");

        assert!(scene.contains("_/\\_"), "scene:\n{scene}");
        assert!(scene.contains("ᛏ"), "scene:\n{scene}");
        assert!(scene.contains("__==#"), "scene:\n{scene}");
        assert!(!scene.contains("MJÖLNIR"), "scene:\n{scene}");
    }

    #[test]
    fn ragnarok_fighters_use_ambient_combat_animation_without_events() {
        let mut state = AppState::new();
        let (abort_tx, _abort_rx) = tokio::sync::watch::channel(false);
        let (proceed_tx, _proceed_rx) = tokio::sync::watch::channel(false);
        state.ragnarok = Some(RagnarokUi::new("task".into(), abort_tx, proceed_tx));
        state.apply_ragnarok_event(ragnarok::RagnarokEvent::Roster(vec![
            crate::ragnarok::FighterCard {
                id: 0,
                agent_source_id: "a".into(),
                model_value: "m0".into(),
                model_name: "M0".into(),
                pass_at_1_bps: 1400,
                mean_cost_usd: 0.0,
            },
        ]));
        state.apply_ragnarok_event(ragnarok::RagnarokEvent::FighterState {
            id: 0,
            state: ragnarok::FighterState::Fighting,
        });
        let arena = state.ragnarok.as_ref().unwrap();
        let fighter = &arena.fighters[0];

        let (sprite, _) = sprite_for(fighter, state.theme);
        assert!(matches!(sprite, SpriteKind::Swing | SpriteKind::Cast));
        assert!(ambient_combat_caption(fighter).is_some());
    }

    #[test]
    fn ragnarok_finalist_pick_and_close_after_verdict() {
        let mut state = AppState::new();
        let (abort_tx, _abort_rx) = tokio::sync::watch::channel(false);
        let (proceed_tx, _proceed_rx) = tokio::sync::watch::channel(false);
        state.ragnarok = Some(RagnarokUi::new("task".into(), abort_tx, proceed_tx));
        state.apply_ragnarok_event(ragnarok::RagnarokEvent::Roster(vec![
            crate::ragnarok::FighterCard {
                id: 0,
                agent_source_id: "a".into(),
                model_value: "m0".into(),
                model_name: "M0".into(),
                pass_at_1_bps: 1400,
                mean_cost_usd: 0.0,
            },
            crate::ragnarok::FighterCard {
                id: 1,
                agent_source_id: "b".into(),
                model_value: "m1".into(),
                model_name: "M1".into(),
                pass_at_1_bps: 1500,
                mean_cost_usd: 0.0,
            },
        ]));
        state.apply_ragnarok_event(ragnarok::RagnarokEvent::Verdict(Box::new(
            crate::ragnarok::Verdict {
                clear_winner: None,
                finalists: Some((0, 1)),
                ranking: vec![0, 1],
                review_verdicts: Vec::new(),
                reasoning: "close".into(),
                thor_fallback: false,
            },
        )));
        handle_ragnarok_key(
            &mut state,
            KeyModifiers::NONE,
            KeyCode::Char('2'),
            UiMode::InlineChat,
        );
        assert_eq!(state.ragnarok.as_ref().unwrap().chosen_finalist, Some(1));
        handle_ragnarok_key(
            &mut state,
            KeyModifiers::NONE,
            KeyCode::Enter,
            UiMode::InlineChat,
        );
        assert!(state.ragnarok.is_none(), "Enter closes after the verdict");
        assert!(matches!(
            state.transcript.last(),
            Some(Entry::System(text)) if text.contains("your pick")
        ));
    }

    #[test]
    fn wrap_tail_lines_wraps_and_keeps_only_the_tail() {
        let text = "alpha\nbeta gamma delta\nepsilon";
        let lines = wrap_tail_lines(text, 6, 10);
        assert_eq!(
            lines,
            vec!["alpha", "beta g", "amma d", "elta", "epsilo", "n"]
        );
        // Only the last N lines are kept.
        let lines = wrap_tail_lines(text, 6, 2);
        assert_eq!(lines, vec!["epsilo", "n"]);
        // Wide glyphs never split mid-cell.
        let wide = "⚔⚔⚔⚔";
        let lines = wrap_tail_lines(wide, 3, 10);
        assert!(
            lines
                .iter()
                .all(|l| UnicodeWidthStr::width(l.as_str()) <= 3)
        );
        assert!(wrap_tail_lines("", 10, 5).is_empty());
    }

    #[test]
    fn esc_clears_input_and_attachments() {
        let mut state = AppState::new();
        state.input = "draft".to_string();
        state.attachments.push(crate::app::PastedAttachment {
            id: 1,
            position: 0,
            content: "x".to_string(),
        });
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));

        assert!(state.input.is_empty());
        assert!(state.attachments.is_empty());
    }

    #[test]
    fn ctrl_c_clears_attachments_when_input_is_empty() {
        let mut state = AppState::new();
        state.attachments.push(crate::app::PastedAttachment {
            id: 1,
            position: 0,
            content: "x".to_string(),
        });
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );

        assert!(state.input.is_empty());
        assert!(state.attachments.is_empty());
        assert!(
            state.exit_reason.is_none(),
            "first Ctrl-C clears attachments, not quits"
        );

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );

        assert_eq!(
            state.exit_reason,
            Some(UiExitReason::Quit),
            "second Ctrl-C quits when everything is empty"
        );
    }

    #[test]
    fn ctrl_c_with_empty_side_composer_returns_even_while_streaming() {
        let mut state = ready_state_with_session();
        state.is_side = true;
        state.record_user_prompt("long answer".to_string());
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );

        assert!(state.side_exit_requested);
        assert!(
            cmd_rx.try_recv().is_err(),
            "cleanup is owned by the UI loop"
        );
        assert!(state.exit_reason.is_none());
    }

    #[test]
    fn prompt_done_notification_uses_last_agent_message_preview() {
        let mut state = AppState::new();
        state.transcript.push(Entry::AgentMessage(
            "  first line\nsecond line  ".to_string(),
        ));

        let message = notification_message_for_event(
            UiMode::FullscreenTui,
            &state,
            &UiEvent::PromptDone {
                stop_reason: StopReason::EndTurn,
                usage: None,
            },
        );

        assert_eq!(message.as_deref(), Some("first line second line"));
    }

    #[test]
    fn cancelled_prompt_done_does_not_notify() {
        let state = AppState::new();

        let message = notification_message_for_event(
            UiMode::FullscreenTui,
            &state,
            &UiEvent::PromptDone {
                stop_reason: StopReason::Cancelled,
                usage: None,
            },
        );

        assert!(message.is_none());
    }

    #[test]
    fn permission_request_notification_uses_tool_title() {
        let (responder, _rx) = tokio::sync::oneshot::channel();
        let prompt = PermissionPrompt {
            tool_call: agent_client_protocol::schema::v1::ToolCallUpdate::new(
                "call-1".to_string(),
                agent_client_protocol::schema::v1::ToolCallUpdateFields::default()
                    .title("run dangerous command"),
            ),
            options: vec![],
            responder,
        };

        let message = permission_request_notification(&prompt);

        assert_eq!(message, "Permission requested: run dangerous command");
    }

    #[test]
    fn preview_notification_text_truncates_long_messages() {
        let long = "a".repeat(100);
        let result = preview_notification_text(&long).expect("non-empty");
        assert_eq!(result.len(), NOTIFICATION_PREVIEW_CHARS);
        assert!(result.ends_with("..."));
        assert_eq!(result.chars().count(), NOTIFICATION_PREVIEW_CHARS);
    }

    fn ready_state_with_session() -> AppState {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.set_connection_state(ConnectionState::Ready);
        state
    }

    fn type_string(state: &mut AppState, cmd_tx: &mpsc::UnboundedSender<UiCommand>, text: &str) {
        for c in text.chars() {
            handle_crossterm(state, cmd_tx, key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn enter_during_streaming_queues_without_cancelling() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());
        assert!(state.is_streaming());

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        type_string(&mut state, &cmd_tx, "next one");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));

        assert!(
            cmd_rx.try_recv().is_err(),
            "queued prompt must not be sent until the active turn finishes"
        );
        assert_eq!(
            state.connection_state(),
            ConnectionState::Streaming,
            "submitting while streaming must not cancel the active turn"
        );
        let queued = state.queued_prompts().next().expect("prompt queued");
        assert_eq!(queued.text, "next one");
        assert_eq!(queued.display_text, "next one");
        assert_eq!(state.queued_prompt_count(), 1);
        assert!(state.input.is_empty(), "input cleared after queueing");
        assert_eq!(state.input_cursor, 0);
    }

    #[test]
    fn queued_prompts_render_above_input_and_stay_out_of_transcript() {
        // Queued prompts must show as persistent chips above the input box
        // while they wait, and must NOT be recorded into the transcript;
        // they have not been sent yet.
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        type_string(&mut state, &cmd_tx, "alpha");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        type_string(&mut state, &cmd_tx, "beta");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        assert!(
            cmd_rx.try_recv().is_err(),
            "queueing must not send commands"
        );

        assert_eq!(state.queued_prompt_count(), 2);
        assert!(
            !state
                .transcript
                .iter()
                .any(|e| matches!(e, Entry::UserPrompt(t) if t == "alpha" || t == "beta")),
            "queued prompts must not enter the transcript while pending"
        );

        for render_mode in [UiMode::FullscreenTui, UiMode::InlineChat] {
            let backend = TestBackend::new(80, 14);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let mut scroll = TranscriptScrollState::default();
            terminal
                .draw(|frame| draw(frame, &mut state, &mut scroll, render_mode))
                .expect("draw");
            let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
            assert!(
                rendered.contains("queued 1/2: alpha") && rendered.contains("queued 2/2: beta"),
                "{render_mode:?} must show the queued list above the input:\n{rendered}"
            );
        }
    }

    #[test]
    fn queued_chip_disappears_after_the_queue_drains() {
        // Once the in-flight turn ends and the queue drains into the next
        // turn, the chip must clear and the prompt then appears in the
        // transcript as a real turn.
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        type_string(&mut state, &cmd_tx, "queued body");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        assert!(cmd_rx.try_recv().is_err());

        // Agent ends the active turn; the drain fires the queued prompt as
        // the next turn.
        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        drain_queued_prompt(&mut state, &cmd_tx);
        assert!(state.queued_prompts().next().is_none(), "queue drained");
        match cmd_rx.try_recv().expect("queued prompt dispatched") {
            UiCommand::SendPrompt { text, images } => {
                assert_eq!(text, "queued body");
                assert!(images.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut scroll = TranscriptScrollState::default();
        terminal
            .draw(|frame| draw(frame, &mut state, &mut scroll, UiMode::FullscreenTui))
            .expect("draw");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            !rendered.contains("↳ queued") && !rendered.contains("queued 1/"),
            "queued chip must clear once the queue drains:\n{rendered}"
        );
        assert!(
            state
                .transcript
                .iter()
                .any(|e| matches!(e, Entry::UserPrompt(t) if t == "queued body")),
            "drained prompt must now appear in the transcript"
        );
    }

    #[test]
    fn second_enter_while_streaming_appends_fifo_without_sending_cancel() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        type_string(&mut state, &cmd_tx, "alpha");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        type_string(&mut state, &cmd_tx, "beta");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));

        assert!(
            cmd_rx.try_recv().is_err(),
            "Enter while streaming must only queue locally"
        );
        let queued = state
            .queued_prompts()
            .map(|prompt| prompt.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(queued, vec!["alpha", "beta"]);
    }

    #[test]
    fn cancelled_turn_landing_drains_the_oldest_queued_prompt() {
        // Simulates user queueing prompts, then pressing Ctrl-C. When the
        // agent acknowledges with PromptDone(Cancelled), the oldest queued
        // prompt fires immediately as the next turn.
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        type_string(&mut state, &cmd_tx, "alpha");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        type_string(&mut state, &cmd_tx, "beta");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        assert!(cmd_rx.try_recv().is_err());

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        match cmd_rx.try_recv().expect("cancel dispatched") {
            UiCommand::CancelPrompt => {}
            other => panic!("unexpected command: {other:?}"),
        }
        assert_eq!(state.connection_state(), ConnectionState::Cancelling);

        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::Cancelled,
            usage: None,
        });
        super::drain_queued_prompt(&mut state, &cmd_tx);

        assert_eq!(state.queued_prompt_count(), 1, "only oldest prompt drained");
        assert_eq!(
            state
                .queued_prompts()
                .next()
                .expect("remaining prompt")
                .text,
            "beta"
        );
        assert!(
            state.is_streaming(),
            "the queued prompt becomes the next active turn"
        );
        let cmd = cmd_rx.try_recv().expect("send prompt dispatched");
        match cmd {
            UiCommand::SendPrompt { text, images } => {
                assert_eq!(text, "alpha");
                assert!(images.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn prompt_done_does_not_clobber_the_queue_status_line() {
        // Regression: PromptDone(Cancelled) used to overwrite queued
        // status with "turn done: Cancelled" before the queued prompt
        // started streaming, leaving a misleading status through the new
        // turn.
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        type_string(&mut state, &cmd_tx, "redirect");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        assert!(cmd_rx.try_recv().is_err());
        let queued_status = state
            .status_line
            .clone()
            .expect("queue status set after submit");
        assert!(
            queued_status.text.starts_with("queued 1: "),
            "expected queue status, got {:?}",
            queued_status.text
        );

        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::Cancelled,
            usage: None,
        });

        let after_cancel = state
            .status_line
            .clone()
            .expect("status line preserved across cancel");
        assert_eq!(
            after_cancel.text, queued_status.text,
            "PromptDone(Cancelled) must not clobber the queue status",
        );
    }

    #[test]
    fn natural_prompt_done_still_drains_queued_prompt() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());
        state.push_queued_prompt(QueuedPrompt {
            text: "queued body".to_string(),
            images: Vec::new(),
            display_text: "queued body".to_string(),
        });

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        super::drain_queued_prompt(&mut state, &cmd_tx);

        assert!(state.queued_prompts().next().is_none(), "queue drained");
        assert!(
            state.is_streaming(),
            "draining a queued prompt starts the next turn"
        );
        let cmd = cmd_rx.try_recv().expect("send prompt dispatched");
        match cmd {
            UiCommand::SendPrompt { text, images } => {
                assert_eq!(text, "queued body");
                assert!(images.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn ctrl_c_during_streaming_preserves_queued_prompt() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());
        state.push_queued_prompt(QueuedPrompt {
            text: "keep me".to_string(),
            images: Vec::new(),
            display_text: "keep me".to_string(),
        });

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );

        assert_eq!(state.queued_prompt_count(), 1, "queue preserved by Ctrl-C");
        assert_eq!(
            state.queued_prompts().next().expect("queued prompt").text,
            "keep me"
        );
        assert_eq!(state.connection_state(), ConnectionState::Cancelling);
        match cmd_rx.try_recv().expect("cancel dispatched") {
            UiCommand::CancelPrompt => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn ctrl_c_cancels_streaming_when_help_overlay_has_focus() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());
        state.help_overlay = true;

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );

        assert!(
            state.help_overlay,
            "Ctrl-C should not spend itself closing help"
        );
        assert_eq!(state.connection_state(), ConnectionState::Cancelling);
        match cmd_rx.try_recv().expect("cancel dispatched") {
            UiCommand::CancelPrompt => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn ctrl_c_cancels_streaming_when_permission_prompt_has_focus() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());
        let pending = permission_pending_with_options("run shell command", &["Allow once"], 0);
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );

        assert!(state.has_pending_permission());
        assert_eq!(state.connection_state(), ConnectionState::Cancelling);
        match cmd_rx.try_recv().expect("cancel dispatched") {
            UiCommand::CancelPrompt => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn esc_during_streaming_preserves_queued_prompt() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());
        state.push_queued_prompt(QueuedPrompt {
            text: "keep me".to_string(),
            images: Vec::new(),
            display_text: "keep me".to_string(),
        });

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));

        assert_eq!(state.queued_prompt_count(), 1, "queue preserved by Esc");
        assert_eq!(
            state.queued_prompts().next().expect("queued prompt").text,
            "keep me"
        );
        assert_eq!(state.connection_state(), ConnectionState::Cancelling);
        match cmd_rx.try_recv().expect("cancel dispatched") {
            UiCommand::CancelPrompt => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn repeated_ctrl_c_during_cancelling_does_not_dispatch_duplicate_cancel() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        match cmd_rx.try_recv().expect("first cancel dispatched") {
            UiCommand::CancelPrompt => {}
            other => panic!("unexpected command: {other:?}"),
        }

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );

        assert_eq!(state.connection_state(), ConnectionState::Cancelling);
        assert!(
            cmd_rx.try_recv().is_err(),
            "second Ctrl-C while cancelling must not enqueue another cancel"
        );
    }

    #[test]
    fn ctrl_c_cancels_whole_turn_while_a_subagent_is_active() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("delegate".to_string());
        state.apply_event(UiEvent::Subagent(SubagentEvent::Started {
            subagent_id: 1,
            resumed: false,
            model: None,
            agent: "codex-acp".to_string(),
            objective: String::new(),
            label: "subagent".to_string(),
        }));

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );

        assert_eq!(state.connection_state(), ConnectionState::Cancelling);
        assert!(matches!(cmd_rx.try_recv(), Ok(UiCommand::CancelPrompt)));

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(
            cmd_rx.try_recv().is_err(),
            "second Ctrl-C must not dispatch another whole-turn cancellation"
        );
    }

    #[test]
    fn repeated_esc_during_cancelling_does_not_dispatch_duplicate_cancel() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));
        match cmd_rx.try_recv().expect("first cancel dispatched") {
            UiCommand::CancelPrompt => {}
            other => panic!("unexpected command: {other:?}"),
        }

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));

        assert_eq!(state.connection_state(), ConnectionState::Cancelling);
        assert!(
            cmd_rx.try_recv().is_err(),
            "second Esc while cancelling must not enqueue another cancel"
        );
    }

    #[test]
    fn esc_during_streaming_dismisses_autocomplete_without_interrupting() {
        let mut state = ready_state_with_session();
        state.available_commands = vec![AvailableCommand::new("help", "show help")];
        state.record_user_prompt("first".to_string());
        state.input = "/he".to_string();
        state.input_cursor = state.input.chars().count();
        state.update_autocomplete();
        assert!(state.autocomplete.visible);

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));

        assert_eq!(state.connection_state(), ConnectionState::Streaming);
        assert!(
            cmd_rx.try_recv().is_err(),
            "Esc should stay local to autocomplete"
        );
        assert!(!state.autocomplete.visible);
    }

    #[test]
    fn runtime_close_clears_queued_prompt() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());
        state.push_queued_prompt(QueuedPrompt {
            text: "stale".to_string(),
            images: Vec::new(),
            display_text: "stale".to_string(),
        });

        state.mark_runtime_closed();

        assert!(state.queued_prompts().next().is_none());
    }

    #[test]
    fn drain_is_a_no_op_when_nothing_is_queued() {
        let mut state = ready_state_with_session();

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        super::drain_queued_prompt(&mut state, &cmd_tx);

        assert!(cmd_rx.try_recv().is_err());
        assert!(!state.is_streaming());
    }

    #[test]
    fn queued_prompt_preview_truncates_long_text_with_ellipsis() {
        let long = "x".repeat(QUEUED_PROMPT_PREVIEW_WIDTH * 2);
        let preview = super::queued_prompt_preview(&long);
        assert!(preview.ends_with("..."));
        assert_eq!(
            preview.chars().count(),
            QUEUED_PROMPT_PREVIEW_WIDTH + 3,
            "ellipsis adds three chars"
        );
    }

    #[test]
    fn queued_prompt_preview_collapses_newlines() {
        let preview = super::queued_prompt_preview("line one\nline two\r\nline three");
        assert!(!preview.contains('\n'));
        assert!(!preview.contains('\r'));
        assert!(preview.starts_with("line one"));
    }
}
