//! The resume dialog: the one surface that lists sessions which are not live.
//!
//! Its Hel tab lists Hel's own stopped, lost, and destroyed records. Its Import
//! tab lists native sessions scanned out of each harness home. A Hel record and
//! the native session it was imported from are the same conversation, so the
//! native copy is omitted: the Hel record carries the checkpoint and durable
//! queue.
//!
//! Nothing here reads the filesystem. Native scans arrive from background tasks
//! as [`ImportProfileOption`] updates, and the merge below is a pure function
//! over what has already been received.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyEventKind};
use mj_chat::components::{ChoiceList, ControlKind, Dialog, Interaction, TabStrip, TextField};
use mj_chat::theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use mj_client::daemon::WikiRow;
use mj_core::config::{Config, HarnessKind};
use mj_core::state::{MoveOperation, SessionRecord, SessionState, State};

use mj_chat::selection::{FrameSurfaces, SurfaceFrame, SurfaceId};
use mj_chat::text_input::TextInput;

use crate::dialogs::{ConfirmDialog, Confirmation, ImportProfileOption};
use crate::render::render_session_scrollbar;
use crate::widgets::{
    Truncate, centered_modal, centered_rect, dismissible_modal_title, format_resource_bytes,
    truncate_to_cells,
};
use crate::{DashboardAction, DashboardState, Mode};

/// Origin shown for a native session that has never run under Hel.
pub(crate) const LOCAL_ORIGIN: &str = "local";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeFocus {
    Tabs,
    Search,
    Sessions,
    Cancel,
    Destroy,
    Open,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeTab {
    Hel,
    Import,
    /// Sessions SessionWiki kept after the tool that ran them deleted its own
    /// copy. They have no checkpoint; Enter restores one into a new session.
    Archive,
}

impl ResumeTab {
    fn index(self) -> usize {
        match self {
            Self::Hel => 0,
            Self::Import => 1,
            Self::Archive => 2,
        }
    }

    fn from_index(index: usize) -> Self {
        match index {
            0 => Self::Hel,
            1 => Self::Import,
            _ => Self::Archive,
        }
    }

    fn includes(self, row: &ResumeRow) -> bool {
        matches!(
            (self, &row.key),
            (Self::Hel, ResumeRowKey::Hel(_))
                | (Self::Import, ResumeRowKey::Native(..))
                | (Self::Archive, ResumeRowKey::Archive(_))
        )
    }
}

/// Identity of one row, stable across rescans so the selection survives an
/// incremental scan update.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ResumeRowKey {
    /// A Hel session record, keyed by its Hel session id.
    Hel(String),
    /// A native session with no Hel record, keyed by harness and native id.
    Native(HarnessKind, String),
    /// An archived session that lives only in the SessionWiki index, keyed by
    /// its SessionWiki id.
    Archive(String),
}

/// What selecting the row does, and whether it may be selected at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeRowStatus {
    /// A checkpointed Hel record: Enter opens the resume wizard.
    Resumable,
    /// A native session Hel has never adopted: Enter imports it.
    Importable,
    /// Only SessionWiki's copy is left: Enter restores it into a new session
    /// carrying a summary of the old transcript.
    Restorable,
    /// The target vanished without a verified checkpoint.
    Lost,
    /// Force-destroyed. There is nothing left to restore.
    DataLoss,
}

impl ResumeRowStatus {
    pub(crate) fn is_recoverable(self) -> bool {
        matches!(self, Self::Resumable | Self::Importable | Self::Restorable)
    }

    /// Short marker shown in the origin column, sized to fit beside it.
    pub(crate) fn warning(self) -> Option<&'static str> {
        match self {
            Self::Lost => Some("⚠ lost"),
            Self::DataLoss => Some("⚠ data lost"),
            Self::Resumable | Self::Importable | Self::Restorable => None,
        }
    }

    /// Why the row cannot be resumed, in full, for the details line and the
    /// notice a rejected Enter leaves behind.
    pub(crate) fn explanation(self) -> Option<&'static str> {
        match self {
            Self::Lost => Some("lost without a verified checkpoint"),
            Self::DataLoss => Some("force-destroyed; nothing is left to restore"),
            Self::Resumable | Self::Importable | Self::Restorable => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResumeRow {
    pub(crate) key: ResumeRowKey,
    pub(crate) profile_id: String,
    pub(crate) title: String,
    /// Where the session ran and the project it opened directly, matching the
    /// live one-line summary. Native sessions use `local/<project>` because Hel
    /// has not chosen their import destination yet. A stored target missing
    /// from config is shown verbatim because its kind is no longer known.
    pub(crate) origin: String,
    pub(crate) details: String,
    pub(crate) last_activity_ms: i64,
    pub(crate) status: ResumeRowStatus,
    /// Reported by the native harness. This metadata is informational only;
    /// it does not affect visibility or dispatch a provider write.
    pub(crate) natively_archived: bool,
    pub(crate) unavailable_reason: Option<String>,
    /// A retained failed/cancelled Move for this record, when recovery is
    /// possible. The dialog turns Enter into an explicit recovery choice.
    pub(crate) move_recovery: Option<MoveOperation>,
    /// The SessionWiki session a search matched to this live row, so the
    /// preview can show the same transcript the hit came from.
    pub(crate) wiki_match: Option<String>,
}

impl ResumeRow {
    pub(crate) fn session_id(&self) -> Option<&str> {
        match &self.key {
            ResumeRowKey::Hel(session_id) => Some(session_id),
            ResumeRowKey::Native(..) | ResumeRowKey::Archive(_) => None,
        }
    }

    /// The SessionWiki session this row previews, which an archived row always
    /// has and a live row has when a search matched it.
    pub(crate) fn wiki_id(&self) -> Option<&str> {
        match &self.key {
            ResumeRowKey::Archive(wiki_id) => Some(wiki_id),
            _ => self.wiki_match.as_deref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResumeDialog {
    pub(crate) discovery_id: u64,
    /// Native scan results, one entry per configured profile. Entries appear
    /// immediately as placeholders and fill in as background scans report.
    ///
    /// A harness home holds thousands of sessions, and the dialog is copied
    /// whenever a confirmation interrupts it, so the scan results are shared
    /// rather than duplicated.
    pub(crate) profiles: Arc<Vec<ImportProfileOption>>,
    pub(crate) tab: ResumeTab,
    pub(crate) selected: Option<ResumeRowKey>,
    pub(crate) row_index: usize,
    pub(crate) search: TextInput,
    pub(crate) form: RefCell<Dialog<ResumeFocus>>,
    pub(crate) opened_at: Instant,
    /// Whether SessionWiki is switched on. With it off the Archived tab shows
    /// one line saying so and no search runs.
    pub(crate) wiki_enabled: bool,
    /// The newest search results, or the most recent sessions before anything
    /// has been typed. Shared rather than copied: a confirmation clones the
    /// whole dialog.
    pub(crate) wiki: Arc<Vec<WikiRow>>,
    /// The search this dialog last asked for. A result that names an older
    /// request is stale and dropped.
    pub(crate) wiki_request_id: u64,
    /// Briefings already fetched, by SessionWiki id, for the dialog's life.
    pub(crate) previews: Arc<BTreeMap<String, String>>,
    /// The briefing being fetched now, so one selection asks only once.
    pub(crate) preview_pending: Option<String>,
}

impl ResumeDialog {
    pub(crate) fn focused(&self) -> ResumeFocus {
        self.form
            .borrow()
            .focused()
            .unwrap_or(ResumeFocus::Sessions)
    }

    fn prepare(&self, rows: &[ResumeRow]) {
        use ResumeFocus::*;
        let mut form = self.form.borrow_mut();
        form.begin_update();
        form.declare_with_enabled(
            Tabs,
            ControlKind::Tabs {
                len: 3,
                selected: self.tab.index(),
            },
            true,
        );
        form.declare_with_enabled(Search, ControlKind::TextField, true);
        form.declare_with_enabled(
            Sessions,
            ControlKind::ChoiceList {
                len: rows.len(),
                selected: self.row_index,
            },
            !rows.is_empty(),
        );
        form.declare_with_enabled(Cancel, ControlKind::Button, true);
        if self.tab == ResumeTab::Hel {
            form.declare_with_enabled(Destroy, ControlKind::Button, self.can_destroy(rows));
        }
        form.declare_with_enabled(Open, ControlKind::Button, self.can_open(rows));
        form.set_list_identity(
            ResumeFocus::Sessions,
            format!("{:?}", rows.iter().map(|row| &row.key).collect::<Vec<_>>()),
        );
        form.end_frame(Sessions);
    }

    fn can_open(&self, rows: &[ResumeRow]) -> bool {
        rows.get(self.row_index).is_some_and(|row| {
            row.status.explanation().is_none() && row.unavailable_reason.is_none()
        })
    }

    /// Only rows with a Mjolnir session record can be destroyed.
    fn can_destroy(&self, rows: &[ResumeRow]) -> bool {
        self.tab == ResumeTab::Hel
            && rows
                .get(self.row_index)
                .is_some_and(|row| row.session_id().is_some())
    }

    pub(crate) fn is_scanning(&self) -> bool {
        self.profiles.iter().any(|profile| {
            profile.error.is_none()
                && profile
                    .scan_progress
                    .is_none_or(|(scanned, total)| scanned < total)
        })
    }

    /// Scanned and total counts summed across every profile still loading.
    pub(crate) fn scan_progress(&self) -> (usize, usize) {
        self.profiles
            .iter()
            .filter_map(|profile| profile.scan_progress)
            .fold(
                (0, 0),
                |(scanned, total), (profile_scanned, profile_total)| {
                    (scanned + profile_scanned, total + profile_total)
                },
            )
    }

    /// The briefing to show under the list for the selected row: the cached
    /// text, or a line saying it is on its way. `None` when the row has no
    /// SessionWiki session behind it, and the pane is then not drawn at all.
    pub(crate) fn preview_text(&self, rows: &[ResumeRow]) -> Option<String> {
        if !self.wiki_enabled {
            return None;
        }
        let index = selected_index(self, rows.len())?;
        let wiki_id = rows.get(index)?.wiki_id()?;
        Some(
            self.previews
                .get(wiki_id)
                .cloned()
                .unwrap_or_else(|| "Loading the archived transcript…".to_owned()),
        )
    }

    pub(crate) fn errors(&self) -> Vec<String> {
        self.profiles
            .iter()
            .filter_map(|profile| {
                profile
                    .error
                    .as_ref()
                    .map(|error| format!("{}: {error}", profile.profile_id))
            })
            .collect()
    }
}

/// The row the dialog points at, clamped to the list it actually has. A state
/// reload can shrink the list under a selection that was valid a moment ago.
fn selected_index(dialog: &ResumeDialog, len: usize) -> Option<usize> {
    (len > 0).then(|| dialog.row_index.min(len - 1))
}

/// Epoch milliseconds for an RFC 3339 timestamp, or `None` when it cannot be
/// parsed. An unparseable timestamp must not silently sort as "now".
fn timestamp_ms(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|parsed| parsed.timestamp_millis())
}

fn hel_row_status(session: &SessionRecord) -> ResumeRowStatus {
    match session.state {
        SessionState::Lost => ResumeRowStatus::Lost,
        SessionState::DestroyedWithDataLoss => ResumeRowStatus::DataLoss,
        _ => ResumeRowStatus::Resumable,
    }
}

const SEVEN_DAYS_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

pub(crate) fn format_last_active<Tz>(now: &chrono::DateTime<Tz>, then_ms: i64) -> String
where
    Tz: chrono::TimeZone,
    Tz::Offset: std::fmt::Display,
{
    if then_ms <= 0 {
        return "unknown".to_owned();
    }
    let Some(then) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(then_ms) else {
        return "unknown".to_owned();
    };
    let elapsed_ms = now.timestamp_millis().saturating_sub(then_ms).max(0);
    if elapsed_ms > SEVEN_DAYS_MS {
        return then
            .with_timezone(&now.timezone())
            .format("%b %-d, %Y")
            .to_string();
    }

    let seconds = elapsed_ms / 1_000;
    if seconds < 60 {
        "just now".to_owned()
    } else if seconds < 3_600 {
        relative_time(seconds / 60, "minute")
    } else if seconds < 86_400 {
        relative_time(seconds / 3_600, "hour")
    } else {
        relative_time(seconds / 86_400, "day")
    }
}

fn relative_time(value: i64, unit: &str) -> String {
    let plural = if value == 1 { "" } else { "s" };
    format!("{value} {unit}{plural} ago")
}

/// Merge Hel's non-live records with the scanned native sessions into one list,
/// newest first. Rows are returned unfiltered; the dialog applies the tab and
/// search on top.
///
/// Dedupe rule: a Hel record whose `native_session_id` matches a scanned native
/// session of the same harness replaces that native row entirely.
/// The Mjolnir harness a SessionWiki tool name stands for, when one does.
/// Tools Mjolnir cannot run have no harness and are never deduplicated against
/// an import row.
fn harness_of_tool(tool: &str) -> Option<HarnessKind> {
    match tool {
        "claude-code" => Some(HarnessKind::Claude),
        "codex" => Some(HarnessKind::Codex),
        _ => None,
    }
}

pub(crate) fn merged_resume_rows(
    config: &Config,
    state: &State,
    profiles: &[ImportProfileOption],
    wiki: &[WikiRow],
) -> Vec<ResumeRow> {
    let mut adopted = BTreeSet::new();
    let mut rows = Vec::new();
    for session in state.sessions.values() {
        // Every record adopts its native session, live ones included: the
        // native file of a session Hel is running now must not be offered as
        // a second import.
        if let Some(native_session_id) = &session.native_session_id {
            adopted.insert((session.harness_kind, native_session_id.clone()));
        }
        if session.state.is_active() {
            continue;
        }
        let last_activity_ms = session
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| timestamp_ms(&checkpoint.created_at))
            .or_else(|| timestamp_ms(&session.updated_at))
            .unwrap_or(0);
        let status = hel_row_status(session);
        let project = session.project_name(config);
        let details = match (&session.checkpoint, status.explanation()) {
            (_, Some(reason)) => format!("{reason} · {project}"),
            (None, None) => format!("no checkpoint · {project}"),
            (Some(_), None) => project,
        };
        rows.push(ResumeRow {
            key: ResumeRowKey::Hel(session.id.clone()),
            profile_id: session.last_profile.clone(),
            title: session.display_title().to_owned(),
            origin: session.project_target(config, &session.target_template_id),
            details,
            last_activity_ms,
            status,
            natively_archived: false,
            unavailable_reason: None,
            move_recovery: None,
            wiki_match: None,
        });
    }
    for profile in profiles {
        for native in &profile.sessions {
            let key = (profile.harness_kind, native.native_session_id.clone());
            if adopted.contains(&key) {
                continue;
            }
            rows.push(ResumeRow {
                key: ResumeRowKey::Native(profile.harness_kind, native.native_session_id.clone()),
                profile_id: profile.profile_id.clone(),
                title: native.title.clone(),
                origin: native_project_target(&native.project_directory),
                details: native.details.clone(),
                last_activity_ms: native.last_activity_ms,
                status: ResumeRowStatus::Importable,
                natively_archived: native.natively_archived,
                unavailable_reason: native.unavailable_reason.clone(),
                move_recovery: None,
                wiki_match: None,
            });
        }
    }
    // SessionWiki rows either annotate a row already here or become archived
    // rows of their own. A row is archived only when nothing on this machine
    // still holds the session: no Mjolnir record, and no native file an import
    // could adopt.
    for hit in wiki {
        let native = hit
            .native_id
            .as_deref()
            .zip(harness_of_tool(&hit.tool))
            .map(|(native_id, harness)| ResumeRowKey::Native(harness, native_id.to_owned()));
        let existing = hit
            .hel_session_id
            .as_deref()
            .map(|session_id| ResumeRowKey::Hel(session_id.to_owned()))
            .filter(|key| rows.iter().any(|row| &row.key == key))
            .or_else(|| native.filter(|key| rows.iter().any(|row| &row.key == key)));
        if let Some(key) = existing {
            if let Some(row) = rows.iter_mut().find(|row| row.key == key) {
                row.wiki_match = Some(hit.id.clone());
                if let Some(snippet) = snippet_text(hit) {
                    row.details = format!("{} · {snippet}", row.details);
                }
            }
            continue;
        }
        if !hit.archived {
            continue;
        }
        rows.push(ResumeRow {
            key: ResumeRowKey::Archive(hit.id.clone()),
            profile_id: hit.tool.clone(),
            title: if hit.title.trim().is_empty() {
                hit.id.clone()
            } else {
                hit.title.clone()
            },
            origin: archive_origin(&hit.project),
            details: archive_details(hit),
            last_activity_ms: hit
                .started
                .as_deref()
                .and_then(timestamp_ms)
                .unwrap_or_default(),
            status: ResumeRowStatus::Restorable,
            natively_archived: false,
            unavailable_reason: None,
            move_recovery: None,
            wiki_match: Some(hit.id.clone()),
        });
    }
    // Newest first across the whole merged list; the key breaks ties so the
    // order is stable between incremental scan updates.
    rows.sort_by(|left, right| {
        right
            .last_activity_ms
            .cmp(&left.last_activity_ms)
            .then_with(|| left.key.cmp(&right.key))
    });
    rows
}

/// The matching text a search returned, on one line and without the markers
/// SessionWiki wraps a hit in.
fn snippet_text(hit: &WikiRow) -> Option<String> {
    let snippet = hit.snippet.as_deref()?;
    let text = snippet
        .replace(['\n', '\r'], " ")
        .replace(['\u{2}', '\u{3}'], "");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

/// Where an archived session ran, in the same shape the other tabs use.
fn archive_origin(project: &str) -> String {
    std::path::Path::new(project).file_name().map_or_else(
        || LOCAL_ORIGIN.to_owned(),
        |project| format!("{LOCAL_ORIGIN}/{}", project.to_string_lossy()),
    )
}

fn archive_details(hit: &WikiRow) -> String {
    let mut details = format!("archived · {} messages", hit.msgs);
    if let Some(snippet) = snippet_text(hit) {
        details.push_str(" · ");
        details.push_str(&snippet);
    } else if let Some(preview) = hit
        .preview
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        details.push_str(" · ");
        details.push_str(preview.trim());
    }
    details
}

/// The rows one dialog tab shows: the merged sources split by ownership, with
/// checkpoint sizes appended and search applied.
fn build_resume_rows(
    config: &Config,
    state: &State,
    dialog: &ResumeDialog,
    checkpoint_archive_sizes: &BTreeMap<String, Option<u64>>,
    now: &chrono::DateTime<chrono::Local>,
) -> Vec<ResumeRow> {
    let needle = dialog.search.to_lowercase();
    merged_resume_rows(config, state, &dialog.profiles, &dialog.wiki)
        .into_iter()
        .filter(|row| dialog.tab.includes(row))
        .map(|mut row| {
            // The checkpoint's size is loaded in the background, so it is
            // appended here rather than folded into the pure merge.
            if let Some(size) = row
                .session_id()
                .and_then(|id| checkpoint_archive_sizes.get(id))
                .copied()
                .flatten()
            {
                row.details
                    .push_str(&format!(" · {}", format_resource_bytes(size)));
            }
            row
        })
        .filter(|row| {
            // An archived row is here because the index matched the query, so
            // the local filter must not drop it for lacking the literal text.
            if matches!(row.key, ResumeRowKey::Archive(_)) {
                return true;
            }
            let activity = format_last_active(now, row.last_activity_ms).to_lowercase();
            needle.is_empty()
                || row.title.to_lowercase().contains(&needle)
                || row.details.to_lowercase().contains(&needle)
                || row.profile_id.to_lowercase().contains(&needle)
                || row.origin.to_lowercase().contains(&needle)
                || activity.contains(&needle)
        })
        .collect()
}

impl DashboardState {
    /// Rebuilds the open dialog's rows from what they are derived from: the
    /// Hel records, the scanned native sessions, the checkpoint sizes, the
    /// search, and the clock the activity labels read. Every mutation of those
    /// inputs calls this, and the dashboard's one-second clock calls it again
    /// so searches keep moving.
    /// Moving the selection only reads the rows.
    pub fn rebuild_resume_rows(&mut self) {
        let Mode::ResumeDialog(dialog) = &self.mode else {
            self.resume_rows.clear();
            return;
        };
        self.resume_rows = build_resume_rows(
            &self.config,
            &self.state,
            dialog,
            &self.checkpoint_archive_sizes,
            &chrono::Local::now(),
        );
        self.resume_rows.retain(|row| {
            row.session_id().is_none_or(|id| {
                self.session_operations
                    .get(id)
                    .is_none_or(|operation| operation.kind.transition_kind().is_none())
            })
        });
        for row in &mut self.resume_rows {
            row.move_recovery = row
                .session_id()
                .and_then(|session_id| self.move_operations.get(session_id))
                .filter(|operation| {
                    matches!(
                        operation.phase,
                        mj_core::state::MovePhase::Failed | mj_core::state::MovePhase::Cancelled
                    ) && (operation.checkpoint.is_some()
                        || (operation.queue_admission_started
                            && !operation.queue_admission_finished))
                })
                .cloned();
        }
        dialog.prepare(&self.resume_rows);
        // Background state updates can remove the selected row.
        // Repair the key and index together so the form never points outside
        // the freshly rebuilt list.
        self.resync_resume_selection();
    }

    /// The rows the open dialog shows; empty when no dialog is open.
    pub(crate) fn resume_rows(&self) -> &[ResumeRow] {
        &self.resume_rows
    }

    /// Whether anything on screen animates on its own and so needs a redraw
    /// faster than the one-second clock: loading dialogs, session activity,
    /// or an in-flight lifecycle transition.
    pub fn needs_fast_tick(&self) -> bool {
        let dialog_animates = match &self.mode {
            Mode::Importing(_) => true,
            Mode::TargetActions(dialog) => dialog.testing.is_some(),
            Mode::ResumeDialog(dialog) => dialog.is_scanning(),
            Mode::Setup(_) | Mode::Help(_) => self.review_settings_discovery_active(),
            _ => false,
        };
        dialog_animates
            || self.opening_session.is_some()
            || self.ordered_sessions().iter().any(|session| {
                self.session_operations.contains_key(&session.id)
                    || (session.last_error.is_none()
                        && matches!(
                            session.state,
                            SessionState::Provisioning
                                | SessionState::Checkpointing
                                | SessionState::Closing
                                | SessionState::Destroying
                        ))
                    || (session.state == SessionState::Running
                        && !self.unreachable_sessions.contains(&session.id)
                        && self.session_details.get(&session.id).is_some_and(|detail| {
                            detail.activity.is_working(
                                detail.current_turn_started_at,
                                !detail.pending_elicitations.is_empty(),
                            )
                        }))
                    || self
                        .session_reviews
                        .get(&session.id)
                        .is_some_and(|review| review.is_working())
            })
    }

    pub fn show_resume_dialog(&mut self, discovery_id: u64, profiles: Vec<ImportProfileOption>) {
        self.mode = Mode::ResumeDialog(ResumeDialog {
            discovery_id,
            profiles: Arc::new(profiles),
            tab: ResumeTab::Hel,
            selected: None,
            row_index: 0,
            search: TextInput::new(),
            form: RefCell::new(Dialog::default()),
            opened_at: Instant::now(),
            wiki_enabled: self.config.sessionwiki.enabled,
            wiki: Arc::new(Vec::new()),
            wiki_request_id: 0,
            previews: Arc::new(BTreeMap::new()),
            preview_pending: None,
        });
        self.rebuild_resume_rows();
        // Record which row the initial selection lands on, so the first
        // incremental scan result cannot slide the selection out from under it.
        self.resync_resume_selection();
    }

    /// Fold one profile's scan result into the open dialog, keeping the
    /// selection on the same row.
    pub fn apply_resume_profile(&mut self, discovery_id: u64, profile: ImportProfileOption) {
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return;
        };
        if dialog.discovery_id != discovery_id {
            return;
        }
        let profiles = Arc::make_mut(&mut dialog.profiles);
        let changed = match profiles
            .iter()
            .position(|candidate| candidate.profile_id == profile.profile_id)
        {
            Some(index) if profiles[index] == profile => false,
            Some(index) => {
                profiles[index] = profile;
                true
            }
            None => {
                profiles.push(profile);
                true
            }
        };
        if !changed {
            return;
        }
        self.rebuild_resume_rows();
        self.resync_resume_selection();
    }

    /// Fold one SessionWiki search result into the open dialog. A result for
    /// an older request is dropped: the person has typed since.
    pub fn apply_wiki_search(&mut self, request_id: u64, rows: Vec<WikiRow>) {
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return;
        };
        if dialog.wiki_request_id != request_id || *dialog.wiki == rows {
            return;
        }
        dialog.wiki = Arc::new(rows);
        self.rebuild_resume_rows();
        self.resync_resume_selection();
    }

    /// Fold one fetched briefing into the open dialog's preview cache.
    pub fn apply_wiki_brief(&mut self, wiki_id: String, markdown: String) {
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return;
        };
        if dialog.preview_pending.as_deref() == Some(wiki_id.as_str()) {
            dialog.preview_pending = None;
        }
        Arc::make_mut(&mut dialog.previews).insert(wiki_id, markdown);
    }

    /// Ask for the briefing of the selected row, unless it is cached or
    /// already in flight.
    fn wiki_preview_action(&mut self) -> DashboardAction {
        let wiki_id = self
            .selected_resume_row()
            .and_then(|row| row.wiki_id().map(ToOwned::to_owned));
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return DashboardAction::None;
        };
        let Some(wiki_id) = wiki_id.filter(|_| dialog.wiki_enabled) else {
            return DashboardAction::None;
        };
        if dialog.previews.contains_key(&wiki_id)
            || dialog.preview_pending.as_deref() == Some(wiki_id.as_str())
        {
            return DashboardAction::None;
        }
        dialog.preview_pending = Some(wiki_id.clone());
        DashboardAction::LoadArchivedBrief { wiki_id }
    }

    /// Ask for a new search after the query changed. The debounce and the
    /// dropping of stale answers live with the request id, not with a timer
    /// here.
    fn wiki_search_action(&mut self) -> DashboardAction {
        match self.next_wiki_search() {
            Some((request_id, query)) => {
                DashboardAction::SearchArchivedSessions { request_id, query }
            }
            None => DashboardAction::None,
        }
    }

    /// Claim the next search request id for the open dialog, with the query it
    /// should run. `None` when no dialog is open or SessionWiki is off.
    pub fn next_wiki_search(&mut self) -> Option<(u64, String)> {
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return None;
        };
        if !dialog.wiki_enabled {
            return None;
        }
        dialog.wiki_request_id = dialog.wiki_request_id.wrapping_add(1);
        Some((dialog.wiki_request_id, dialog.search.to_string()))
    }

    /// Keeps `row_index` pointed at the selected row after the list changed.
    fn resync_resume_selection(&mut self) {
        let Mode::ResumeDialog(dialog) = &self.mode else {
            return;
        };
        let rows = self.resume_rows();
        let index = dialog
            .selected
            .as_ref()
            .and_then(|key| rows.iter().position(|row| &row.key == key))
            .unwrap_or_else(|| dialog.row_index.min(rows.len().saturating_sub(1)));
        let key = rows.get(index).map(|row| row.key.clone());
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return;
        };
        dialog.row_index = index;
        dialog.selected = key;
        dialog.prepare(&self.resume_rows);
    }

    fn switch_resume_tab(&mut self, tab: ResumeTab) -> bool {
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return false;
        };
        if dialog.tab == tab {
            return false;
        }
        dialog.tab = tab;
        dialog.selected = None;
        dialog.row_index = 0;
        self.rebuild_resume_rows();
        self.resync_resume_selection();
        true
    }

    pub(crate) fn select_resume_row(&mut self, index: usize) {
        let key = self.resume_rows().get(index).map(|row| row.key.clone());
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return;
        };
        dialog.row_index = index;
        dialog.selected = key;
        dialog.prepare(&self.resume_rows);
    }

    /// The row the open dialog points at.
    pub(crate) fn selected_resume_row(&self) -> Option<ResumeRow> {
        let Mode::ResumeDialog(dialog) = &self.mode else {
            return None;
        };
        let rows = self.resume_rows();
        let index = selected_index(dialog, rows.len())?;
        rows.get(index).cloned()
    }

    pub(crate) fn handle_resume_dialog_event(&mut self, event: Event) -> DashboardAction {
        use ResumeFocus::*;
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return DashboardAction::None;
        };
        let focused = dialog.focused();
        if let Event::Key(key) = &event
            && key.kind == KeyEventKind::Press
            && key.modifiers.is_empty()
        {
            if focused == Search && key.code == KeyCode::Down {
                dialog.form.get_mut().focus(Sessions);
                return DashboardAction::None;
            }
            if focused != Search {
                match key.code {
                    KeyCode::Char('/') => {
                        dialog.form.get_mut().focus(Search);
                        return DashboardAction::None;
                    }
                    KeyCode::Delete if focused == Sessions => {
                        return self.destroy_selected_resume_row();
                    }
                    // Keep list navigation shortcuts; arrows in fields belong to editing.
                    KeyCode::Left | KeyCode::Right if focused == Sessions => {
                        let step = if key.code == KeyCode::Left { 2 } else { 1 };
                        let next = ResumeTab::from_index((dialog.tab.index() + step) % 3);
                        self.switch_resume_tab(next);
                        return self.wiki_preview_action();
                    }
                    _ => {}
                }
            }
        }
        let event = match event {
            Event::Key(mut key) if focused == Sessions && key.modifiers.is_empty() => {
                key.code = match key.code {
                    KeyCode::Char('j') => KeyCode::Down,
                    KeyCode::Char('k') => KeyCode::Up,
                    code => code,
                };
                Event::Key(key)
            }
            event => event,
        };
        let result = dialog.form.get_mut().handle(&event);
        self.last_event_consumed.set(result.consumed);
        let interaction = result.action;
        match interaction {
            Some(Interaction::Cancel | Interaction::Activate(Cancel)) => self.cancel_modal(),
            Some(Interaction::Edit(Search, edit)) => {
                TextField::apply(&mut dialog.search, edit);
                self.rebuild_resume_rows();
                self.select_resume_row(0);
                return self.wiki_search_action();
            }
            Some(Interaction::Select(Tabs, index)) => {
                self.switch_resume_tab(ResumeTab::from_index(index));
                return self.wiki_preview_action();
            }
            Some(Interaction::Select(Sessions, index)) => {
                self.select_resume_row(index);
                return self.wiki_preview_action();
            }
            Some(Interaction::Activate(Search | Tabs)) => {
                dialog.form.get_mut().focus(Sessions);
            }
            Some(Interaction::Activate(Sessions | Open)) => {
                let row = self.selected_resume_row();
                return self.activate_selected_resume_row(row);
            }
            Some(Interaction::Activate(Destroy)) => return self.destroy_selected_resume_row(),
            _ => {}
        }
        DashboardAction::None
    }

    /// Asks for confirmation before destroying the selected row's session record.
    fn destroy_selected_resume_row(&mut self) -> DashboardAction {
        let Some(row) = self.selected_resume_row() else {
            return DashboardAction::None;
        };
        let Some(session_id) = row.session_id().map(ToOwned::to_owned) else {
            self.notices
                .set("Mjolnir never destroys a harness's own session.");
            return DashboardAction::None;
        };
        self.cancel_component_pointer();
        let Mode::ResumeDialog(dialog) = std::mem::replace(&mut self.mode, Mode::Dashboard) else {
            return DashboardAction::None;
        };
        self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::DestroyStopped {
            session_id,
            reopen: Some(Box::new(dialog)),
        }));
        self.rebuild_resume_rows();
        DashboardAction::None
    }

    fn activate_selected_resume_row(&mut self, row: Option<ResumeRow>) -> DashboardAction {
        let Some(row) = row else {
            return DashboardAction::None;
        };
        if let Some(reason) = row.status.explanation() {
            self.notices.set(format!(
                "This session was {reason}. Use Destroy to remove its record."
            ));
            return DashboardAction::None;
        }
        if let Some(reason) = &row.unavailable_reason {
            self.notices.set(format!("Cannot resume: {reason}"));
            return DashboardAction::None;
        }
        match row.key {
            ResumeRowKey::Hel(session_id) => {
                if let Some(operation) = row.move_recovery {
                    self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::RecoverMove {
                        operation: Box::new(operation),
                    }));
                    return DashboardAction::None;
                }
                self.cancel_modal();
                self.begin_resume_for(&session_id)
            }
            ResumeRowKey::Archive(wiki_id) => {
                self.cancel_modal();
                self.begin_archive_restore(wiki_id, row.title)
            }
            ResumeRowKey::Native(_, native_session_id) => {
                let profile_id = row.profile_id;
                let display_title = row.title;
                self.cancel_modal();
                DashboardAction::ImportSession {
                    profile_id,
                    native_session_id,
                    display_title,
                }
            }
        }
    }
}

/// Column widths for the row text, derived from the pane width.
struct RowLayout {
    title: usize,
    profile: usize,
    origin: usize,
    activity: usize,
}

fn row_layout(width: u16) -> RowLayout {
    let width = usize::from(width);
    let profile = 14.min(width / 5).max(6);
    let origin = 24.min(width / 3).max(8);
    let activity = 14.min(width / 4).max(8);
    RowLayout {
        title: width
            .saturating_sub(profile + origin + activity + 8)
            .max(10),
        profile,
        origin,
        activity,
    }
}

fn native_project_target(project_directory: &str) -> String {
    std::path::Path::new(project_directory)
        .file_name()
        .map_or_else(
            || LOCAL_ORIGIN.to_owned(),
            |project| format!("{LOCAL_ORIGIN}/{}", project.to_string_lossy()),
        )
}

/// How tall the preview pane under the list is when the dialog shows one.
const PREVIEW_HEIGHT: u16 = 6;

/// The dialog body split into its bands: tabs, search, list, preview, footer.
/// The preview band is present only when the dialog has something to show in
/// it, so the list keeps the whole height when it does not.
fn resume_bands(inner: Rect, preview: bool) -> std::rc::Rc<[Rect]> {
    let mut constraints = vec![
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(5),
    ];
    if preview {
        constraints.push(Constraint::Length(PREVIEW_HEIGHT));
    }
    constraints.push(Constraint::Length(4));
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner)
}

pub(crate) fn resume_sessions_pane(area: Rect, preview: bool) -> Rect {
    let popup = centered_rect(84, 24, area);
    let inner = popup.inner(Margin {
        vertical: 1,
        horizontal: 1,
    });
    resume_bands(inner, preview)[2]
}

pub(crate) fn render_resume_dialog(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    dialog: &ResumeDialog,
    surfaces: &mut FrameSurfaces,
) {
    let popup = centered_modal(frame, surfaces, 84, 24, area);
    let (scanned, total) = dialog.scan_progress();
    let title = if dialog.is_scanning() {
        format!(" Resume a session · scanning {scanned}/{total} ")
    } else {
        " Resume a session ".to_owned()
    };
    let inner = theme::modal().inner(popup);
    let preview = dialog.preview_text(dashboard.resume_rows());
    let bands = resume_bands(inner, preview.is_some());
    let rows = bands.as_ref();
    // The footer is the last band whether or not a preview sits above it.
    let footer_band = rows[rows.len() - 1];

    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title_line =
        dismissible_modal_title(&mut form, popup, title.trim(), theme::title(true), true);
    frame.render_widget(theme::modal().title(title_line), popup);
    TabStrip::render(
        frame,
        rows[0],
        &[" Mjolnir ", " Import ", " Archived "],
        dialog.tab.index(),
        &mut form,
        ResumeFocus::Tabs,
    );
    let search_focused = form.is_focused(ResumeFocus::Search);
    let search_area = Rect::new(rows[1].x, rows[1].y, rows[1].width, rows[1].height.min(1));
    let label_width = 8.min(search_area.width);
    frame.render_widget(
        Line::raw("Search: "),
        Rect::new(
            search_area.x,
            search_area.y,
            label_width,
            search_area.height,
        ),
    );
    TextField::render(
        frame,
        Rect::new(
            search_area.x + label_width,
            search_area.y,
            search_area.width - label_width,
            search_area.height,
        ),
        &dialog.search,
        &mut form,
        ResumeFocus::Search,
    );
    let list_rows = dashboard.resume_rows();
    let sessions_focused = form.is_focused(ResumeFocus::Sessions);
    let block = theme::panel(sessions_focused || search_focused).title(match dialog.tab {
        ResumeTab::Hel => " Mjolnir sessions · newest first ",
        ResumeTab::Import => " Importable sessions · newest first ",
        ResumeTab::Archive => " Archived sessions · newest first ",
    });
    let list_area = block.inner(rows[2]);
    // Registered after the dialog body so a drag over the rows selects the
    // list rather than the popup around it.
    surfaces.push(SurfaceFrame::fixed(SurfaceId::ResumeList, list_area));
    frame.render_widget(block, rows[2]);
    let table_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(list_area);
    let header_area = Rect::new(
        table_rows[0].x.saturating_add(2),
        table_rows[0].y,
        table_rows[0].width.saturating_sub(2),
        table_rows[0].height,
    );
    let list_area = table_rows[1];
    let layout = row_layout(list_area.width.saturating_sub(2));
    frame.render_widget(Paragraph::new(resume_header_line(&layout)), header_area);
    let now = chrono::Local::now();
    if list_rows.is_empty() {
        let message = match (dialog.tab, dialog.is_scanning(), dialog.search.is_empty()) {
            (ResumeTab::Archive, ..) if !dialog.wiki_enabled => {
                "Enable SessionWiki in Setup to search and archive sessions"
            }
            (ResumeTab::Import, true, _) => "Scanning native sessions…",
            (ResumeTab::Hel, _, true) => "No stopped Mjolnir sessions",
            (ResumeTab::Import, _, true) => "No importable sessions",
            (ResumeTab::Archive, _, true) => "No archived sessions",
            _ => "No matching sessions",
        };
        frame.render_widget(Line::raw(message), list_area);
        form.register(
            ResumeFocus::Sessions,
            ControlKind::ChoiceList {
                len: 0,
                selected: 0,
            },
            list_area,
            false,
        );
    } else {
        let items = list_rows
            .iter()
            .map(|row| resume_row_line(row, &layout, &now))
            .collect::<Vec<_>>();
        ChoiceList::render(
            frame,
            list_area,
            &items,
            dialog.row_index,
            &mut form,
            ResumeFocus::Sessions,
        );
    }
    render_session_scrollbar(
        frame,
        rows[2],
        list_rows.len(),
        form.list_offset(ResumeFocus::Sessions),
        usize::from(list_area.height).max(1),
    );

    if let Some(preview) = preview {
        let preview_band = rows[3];
        let block = theme::panel(false).title(" Archived transcript ");
        let body = block.inner(preview_band);
        frame.render_widget(block, preview_band);
        frame.render_widget(
            Paragraph::new(
                preview
                    .lines()
                    .map(|line| Line::raw(line.to_owned()))
                    .collect::<Vec<_>>(),
            )
            .wrap(Wrap { trim: false }),
            body,
        );
    }
    let selected = selected_index(dialog, list_rows.len()).and_then(|index| list_rows.get(index));
    let mut footer = Vec::new();
    if let Some(detail) = selected {
        footer.push(Line::styled(
            truncate_to_cells(
                &detail.details,
                usize::from(footer_band.width),
                Truncate::SUMMARY,
            ),
            Style::default().fg(theme::palette().muted),
        ));
    }
    let errors = dialog.errors();
    if dialog.tab == ResumeTab::Import
        && let Some(error) = errors.first()
    {
        footer.push(Line::styled(
            truncate_to_cells(
                &format!("Scan failed for {error}"),
                usize::from(footer_band.width),
                Truncate::SUMMARY,
            ),
            Style::default().fg(theme::palette().warning),
        ));
    }
    footer.push(Line::styled(
        match dialog.tab {
            ResumeTab::Hel => "Enter resumes · Delete destroys · ←/→ tabs · / searches · Tab moves",
            ResumeTab::Import => "Enter imports · ←/→ tabs · / searches · Tab moves",
            ResumeTab::Archive => "Enter restores · ←/→ tabs · / searches · Tab moves",
        },
        Style::default().fg(theme::palette().muted),
    ));
    let note_area = Rect::new(
        footer_band.x,
        footer_band.y,
        footer_band.width,
        footer_band.height.saturating_sub(1),
    );
    frame.render_widget(
        Paragraph::new(footer)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        note_area,
    );
    let button_area = Rect::new(
        footer_band.x,
        footer_band.bottom().saturating_sub(1),
        footer_band.width,
        u16::from(footer_band.height > 0),
    );
    let mut buttons = vec![(ResumeFocus::Cancel, "Cancel", true)];
    if dialog.tab == ResumeTab::Hel {
        buttons.push((
            ResumeFocus::Destroy,
            "Destroy",
            dialog.can_destroy(list_rows),
        ));
    }
    buttons.push((
        ResumeFocus::Open,
        match dialog.tab {
            ResumeTab::Hel => "Resume",
            ResumeTab::Import => "Import",
            ResumeTab::Archive => "Restore",
        },
        dialog.can_open(list_rows),
    ));
    Dialog::render_actions(frame, button_area, &buttons, &mut form);
    form.end_frame(ResumeFocus::Sessions);
    if dialog.is_scanning() {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                mj_chat::spinner::compact_span(
                    dashboard.config.spinner,
                    dialog.opened_at.elapsed().as_millis(),
                ),
                Span::styled(" Scanning…", theme::muted()),
            ])),
            Rect::new(
                note_area.right().saturating_sub(14).max(note_area.x),
                note_area.y,
                note_area.width.min(14),
                note_area.height.min(1),
            ),
        );
    }
}

fn resume_header_line(layout: &RowLayout) -> Line<'static> {
    let style = Style::default()
        .fg(theme::palette().muted)
        .add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled(padded_cell("PROFILE", layout.profile), style),
        Span::raw("  "),
        Span::styled(padded_cell("TARGET", layout.origin), style),
        Span::raw("  "),
        Span::styled(padded_cell("LAST ACTIVE", layout.activity), style),
        Span::raw("  "),
        Span::styled(
            truncate_to_cells("SESSION", layout.title, Truncate::SUMMARY),
            style,
        ),
    ])
}

fn padded_cell(text: &str, width: usize) -> String {
    format!(
        "{:<width$}",
        truncate_to_cells(text, width, Truncate::SUMMARY),
        width = width
    )
}

fn resume_row_line<Tz>(
    row: &ResumeRow,
    layout: &RowLayout,
    now: &chrono::DateTime<Tz>,
) -> Line<'static>
where
    Tz: chrono::TimeZone,
    Tz::Offset: std::fmt::Display,
{
    let title_style = if row.status.is_recoverable() {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::palette().warning)
    };
    let origin = match row.status.warning() {
        Some(warning) => Span::styled(
            format!("{:<width$}", warning, width = layout.origin),
            Style::default().fg(theme::palette().warning),
        ),
        None => Span::styled(
            format!(
                "{:<width$}",
                truncate_to_cells(&row.origin, layout.origin, Truncate::SUMMARY),
                width = layout.origin
            ),
            Style::default().fg(theme::palette().accent),
        ),
    };
    let mut marks = String::new();
    if row.unavailable_reason.is_some() {
        marks.push_str("  [unavailable]");
    }
    if let Some(operation) = &row.move_recovery {
        if operation.queue_admission_started && !operation.queue_admission_finished {
            marks.push_str("  [move queue needs retry]");
        } else {
            marks.push_str("  [move needs recovery]");
        }
    }
    Line::from(vec![
        Span::styled(
            padded_cell(&row.profile_id, layout.profile),
            Style::default().fg(theme::palette().secondary),
        ),
        Span::raw("  "),
        origin,
        Span::raw("  "),
        Span::styled(
            padded_cell(
                &format_last_active(now, row.last_activity_ms),
                layout.activity,
            ),
            Style::default().fg(theme::palette().muted),
        ),
        Span::raw("  "),
        Span::styled(
            truncate_to_cells(&row.title, layout.title, Truncate::SUMMARY),
            title_style,
        ),
        Span::styled(marks, Style::default().fg(theme::palette().muted)),
    ])
}

/// Placeholder entries so every configured profile shows before its scan
/// reports anything.
pub fn resume_profile_placeholders(
    profiles: impl IntoIterator<Item = (String, HarnessKind)>,
) -> Vec<ImportProfileOption> {
    profiles
        .into_iter()
        .map(|(profile_id, harness_kind)| ImportProfileOption {
            profile_id,
            harness_kind,
            sessions: Vec::new(),
            scan_progress: None,
            error: None,
        })
        .collect()
}

#[cfg(test)]
mod tests;
