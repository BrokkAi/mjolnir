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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};

use hel::hel_config::{HarnessKind, HelConfig};
use hel::hel_selection::{FrameSurfaces, SurfaceFrame, SurfaceId};
use hel::hel_state::{HelState, SessionRecord, SessionState};
use hel::hel_text_input::TextInput;

use crate::dialogs::{ConfirmDialog, Confirmation, ImportProfileOption};
use crate::render::render_session_scrollbar;
use crate::widgets::{
    action_buttons, centered_modal, centered_rect, focus_border, format_resource_bytes,
    truncate_text,
};
use crate::{DashboardAction, DashboardState, Mode, cycle_control, move_index};

/// Origin shown for a native session that has never run under Hel.
pub(crate) const LOCAL_ORIGIN: &str = "local";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeFocus {
    Search,
    Sessions,
    Cancel,
    Open,
}

const RESUME_FOCUS_ORDER: [ResumeFocus; 4] = [
    ResumeFocus::Search,
    ResumeFocus::Sessions,
    ResumeFocus::Cancel,
    ResumeFocus::Open,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeTab {
    Hel,
    Import,
}

impl ResumeTab {
    fn index(self) -> usize {
        match self {
            Self::Hel => 0,
            Self::Import => 1,
        }
    }

    fn includes(self, row: &ResumeRow) -> bool {
        matches!(
            (self, &row.key),
            (Self::Hel, ResumeRowKey::Hel(_)) | (Self::Import, ResumeRowKey::Native(..))
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
}

/// What selecting the row does, and whether it may be selected at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeRowStatus {
    /// A checkpointed Hel record: Enter opens the resume wizard.
    Resumable,
    /// A native session Hel has never adopted: Enter imports it.
    Importable,
    /// The target vanished without a verified checkpoint.
    Lost,
    /// Force-destroyed. There is nothing left to restore.
    DataLoss,
}

impl ResumeRowStatus {
    pub(crate) fn is_recoverable(self) -> bool {
        matches!(self, Self::Resumable | Self::Importable)
    }

    /// Short marker shown in the origin column, sized to fit beside it.
    pub(crate) fn warning(self) -> Option<&'static str> {
        match self {
            Self::Lost => Some("⚠ lost"),
            Self::DataLoss => Some("⚠ data lost"),
            Self::Resumable | Self::Importable => None,
        }
    }

    /// Why the row cannot be resumed, in full, for the details line and the
    /// notice a rejected Enter leaves behind.
    pub(crate) fn explanation(self) -> Option<&'static str> {
        match self {
            Self::Lost => Some("lost without a verified checkpoint"),
            Self::DataLoss => Some("force-destroyed; nothing is left to restore"),
            Self::Resumable | Self::Importable => None,
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
    /// Hidden by Hel, either by the record's flag or the native hidden set.
    pub(crate) archived: bool,
    /// Hidden by the harness itself. Hel cannot clear this.
    pub(crate) natively_archived: bool,
    pub(crate) unavailable_reason: Option<String>,
}

impl ResumeRow {
    pub(crate) fn session_id(&self) -> Option<&str> {
        match &self.key {
            ResumeRowKey::Hel(session_id) => Some(session_id),
            ResumeRowKey::Native(..) => None,
        }
    }

    /// Whether the row is hidden from the default view for any reason.
    pub(crate) fn is_hidden(&self) -> bool {
        self.archived || self.natively_archived
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
    pub(crate) focus: ResumeFocus,
    pub(crate) show_archived: bool,
    pub(crate) opened_at: Instant,
}

impl ResumeDialog {
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

fn format_last_active<Tz>(now: &chrono::DateTime<Tz>, then_ms: i64) -> String
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
/// newest first. Rows are returned unfiltered; the dialog applies the archived
/// toggle and search on top.
///
/// Dedupe rule: a Hel record whose `native_session_id` matches a scanned native
/// session of the same harness replaces that native row entirely.
pub(crate) fn merged_resume_rows(
    config: &HelConfig,
    state: &HelState,
    profiles: &[ImportProfileOption],
    hidden_native: &BTreeSet<(HarnessKind, String)>,
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
            archived: session.archived,
            natively_archived: false,
            unavailable_reason: None,
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
                archived: hidden_native.contains(&key),
                natively_archived: native.natively_archived,
                unavailable_reason: native.unavailable_reason.clone(),
            });
        }
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

/// The rows one dialog tab shows: the merged sources split by ownership, with
/// checkpoint sizes appended, the archived toggle applied, and search applied.
fn build_resume_rows(
    config: &HelConfig,
    state: &HelState,
    dialog: &ResumeDialog,
    hidden_native: &BTreeSet<(HarnessKind, String)>,
    checkpoint_archive_sizes: &BTreeMap<String, Option<u64>>,
    now: &chrono::DateTime<chrono::Local>,
) -> Vec<ResumeRow> {
    let needle = dialog.search.to_lowercase();
    merged_resume_rows(config, state, &dialog.profiles, hidden_native)
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
        .filter(|row| dialog.show_archived || !row.is_hidden())
        .filter(|row| {
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
    /// Hel records, the scanned native sessions, the hidden set, the
    /// checkpoint sizes, the search, the archived toggle, and the clock the
    /// activity labels read. Every mutation of those inputs calls this, and
    /// the dashboard's one-second clock calls it again so searches keep moving.
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
            &self.hidden_native_sessions,
            &self.checkpoint_archive_sizes,
            &chrono::Local::now(),
        );
    }

    /// The rows the open dialog shows; empty when no dialog is open.
    pub(crate) fn resume_rows(&self) -> &[ResumeRow] {
        &self.resume_rows
    }

    /// Whether anything on screen animates on its own and so needs a redraw
    /// faster than the one-second clock: the import progress dialog, or the
    /// resume dialog's scanning spinner.
    pub fn needs_fast_tick(&self) -> bool {
        match &self.mode {
            Mode::Importing(_) => true,
            Mode::ResumeDialog(dialog) => dialog.is_scanning(),
            _ => false,
        }
    }

    pub fn show_resume_dialog(&mut self, discovery_id: u64, profiles: Vec<ImportProfileOption>) {
        self.mode = Mode::ResumeDialog(ResumeDialog {
            discovery_id,
            profiles: Arc::new(profiles),
            tab: ResumeTab::Hel,
            selected: None,
            row_index: 0,
            search: TextInput::new(),
            focus: ResumeFocus::Sessions,
            show_archived: false,
            opened_at: Instant::now(),
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
        match profiles
            .iter()
            .position(|candidate| candidate.profile_id == profile.profile_id)
        {
            Some(index) => profiles[index] = profile,
            None => profiles.push(profile),
        }
        self.rebuild_resume_rows();
        self.resync_resume_selection();
    }

    /// Replaces the hidden set loaded from Hel's database.
    pub fn set_hidden_native_sessions(&mut self, hidden: BTreeSet<(HarnessKind, String)>) {
        self.hidden_native_sessions = hidden;
        self.rebuild_resume_rows();
    }

    /// Applies a hide/reveal locally so the row moves immediately; the caller
    /// persists it in the background and reports a failure as a notice.
    pub fn set_native_session_hidden(
        &mut self,
        harness: HarnessKind,
        native_session_id: String,
        hidden: bool,
    ) {
        if hidden {
            self.hidden_native_sessions
                .insert((harness, native_session_id));
        } else {
            self.hidden_native_sessions
                .remove(&(harness, native_session_id));
        }
        self.rebuild_resume_rows();
    }

    /// Applies an archive/unarchive of a Hel record locally, for the same
    /// reason as [`DashboardState::set_native_session_hidden`].
    pub fn set_session_archived(&mut self, session_id: &str, archived: bool) {
        if let Some(session) = self.state.sessions.get_mut(session_id) {
            session.archived = archived;
        }
        self.rebuild_resume_rows();
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
    }

    fn move_resume_selection(&mut self, delta: isize) {
        let len = self.resume_rows().len();
        let Mode::ResumeDialog(dialog) = &self.mode else {
            return;
        };
        let mut index = dialog.row_index;
        move_index(&mut index, len, delta);
        self.select_resume_row(index);
    }

    fn switch_resume_tab(&mut self, tab: ResumeTab) {
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return;
        };
        if dialog.tab == tab {
            return;
        }
        dialog.tab = tab;
        dialog.selected = None;
        dialog.row_index = 0;
        self.rebuild_resume_rows();
        self.resync_resume_selection();
    }

    pub(crate) fn select_resume_row(&mut self, index: usize) {
        let key = self.resume_rows().get(index).map(|row| row.key.clone());
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return;
        };
        dialog.row_index = index;
        dialog.selected = key;
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

    /// Handles one key for the open dialog. The dialog is edited where it
    /// lives: a scan of a busy harness home fills it with thousands of rows,
    /// which no key press should have to copy.
    pub(crate) fn handle_resume_dialog_key(&mut self, key: KeyEvent) -> DashboardAction {
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return DashboardAction::None;
        };
        let focus = dialog.focus;
        let typing = focus == ResumeFocus::Search;
        match key.code {
            KeyCode::Esc => {
                self.cancel_modal();
                DashboardAction::None
            }
            KeyCode::Tab | KeyCode::BackTab => {
                dialog.focus =
                    cycle_control(focus, &RESUME_FOCUS_ORDER, key.code == KeyCode::BackTab);
                DashboardAction::None
            }
            // Down leaves search for the list, so typing a query and walking
            // its results needs no Tab in between.
            KeyCode::Down if typing => {
                dialog.focus = ResumeFocus::Sessions;
                DashboardAction::None
            }
            // The tabs are advertised on the arrow keys, so they answer from
            // every focus including the search field. A query short enough to
            // type here does not need an in-field cursor.
            KeyCode::Left => {
                self.switch_resume_tab(ResumeTab::Hel);
                DashboardAction::None
            }
            KeyCode::Right => {
                self.switch_resume_tab(ResumeTab::Import);
                DashboardAction::None
            }
            _ if typing => {
                if dialog.search.handle_key(key).changed() {
                    self.rebuild_resume_rows();
                    self.select_resume_row(0);
                }
                DashboardAction::None
            }
            KeyCode::Up | KeyCode::Char('k') if !typing => {
                self.move_resume_selection(-1);
                DashboardAction::None
            }
            KeyCode::Down | KeyCode::Char('j') if !typing => {
                self.move_resume_selection(1);
                DashboardAction::None
            }
            // `/` jumps to search from anywhere, so the list keeps its
            // single-letter action keys.
            KeyCode::Char('/') if !typing => {
                dialog.focus = ResumeFocus::Search;
                DashboardAction::None
            }
            KeyCode::Char('s') if !typing => {
                dialog.show_archived = !dialog.show_archived;
                let shown = dialog.show_archived;
                self.rebuild_resume_rows();
                self.resync_resume_selection();
                self.notices.set(if shown {
                    "Showing archived sessions."
                } else {
                    "Hiding archived sessions."
                });
                DashboardAction::None
            }
            KeyCode::Char('a') if !typing => {
                let row = self.selected_resume_row();
                self.toggle_selected_resume_archive(row)
            }
            KeyCode::Char('d') | KeyCode::Delete if !typing => {
                let Some(row) = self.selected_resume_row() else {
                    return DashboardAction::None;
                };
                let Some(session_id) = row.session_id().map(ToOwned::to_owned) else {
                    self.notices.set(
                        "Mjolnir never destroys a harness's own session. Press a to archive this row.",
                    );
                    return DashboardAction::None;
                };
                // The confirmation carries the dialog so it can reopen on
                // exactly the list the user was looking at.
                let Mode::ResumeDialog(dialog) = std::mem::replace(&mut self.mode, Mode::Dashboard)
                else {
                    return DashboardAction::None;
                };
                self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::DestroyStopped {
                    session_id,
                    reopen: Some(Box::new(dialog)),
                }));
                self.rebuild_resume_rows();
                DashboardAction::None
            }
            KeyCode::Enter if focus == ResumeFocus::Cancel => {
                self.cancel_modal();
                DashboardAction::None
            }
            KeyCode::Enter if focus == ResumeFocus::Search => {
                dialog.focus = ResumeFocus::Sessions;
                DashboardAction::None
            }
            KeyCode::Enter => {
                let row = self.selected_resume_row();
                self.activate_selected_resume_row(row)
            }
            _ => DashboardAction::None,
        }
    }

    fn toggle_selected_resume_archive(&mut self, row: Option<ResumeRow>) -> DashboardAction {
        let Some(row) = row else {
            return DashboardAction::None;
        };
        if row.natively_archived {
            self.notices.set(
                "This session is archived in its own harness; Mjolnir never writes that back.",
            );
            return DashboardAction::None;
        }
        let archived = !row.archived;
        match &row.key {
            ResumeRowKey::Hel(session_id) => {
                let session_id = session_id.clone();
                self.set_session_archived(&session_id, archived);
                self.resync_resume_selection();
                DashboardAction::SetSessionArchived {
                    session_id,
                    archived,
                }
            }
            ResumeRowKey::Native(harness, native_session_id) => {
                let (harness, native_session_id) = (*harness, native_session_id.clone());
                self.set_native_session_hidden(harness, native_session_id.clone(), archived);
                self.resync_resume_selection();
                DashboardAction::SetNativeSessionHidden {
                    harness_kind: harness,
                    native_session_id,
                    hidden: archived,
                }
            }
        }
    }

    fn activate_selected_resume_row(&mut self, row: Option<ResumeRow>) -> DashboardAction {
        let Some(row) = row else {
            return DashboardAction::None;
        };
        if let Some(reason) = row.status.explanation() {
            self.notices.set(format!(
                "This session was {reason}. Press d to destroy its record."
            ));
            return DashboardAction::None;
        }
        if let Some(reason) = &row.unavailable_reason {
            self.notices.set(format!("Cannot resume: {reason}"));
            return DashboardAction::None;
        }
        match row.key {
            ResumeRowKey::Hel(session_id) => {
                self.cancel_modal();
                self.begin_resume_for(&session_id)
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

pub(crate) fn resume_sessions_pane(area: Rect) -> Rect {
    let popup = centered_rect(84, 24, area);
    let inner = popup.inner(Margin {
        vertical: 1,
        horizontal: 1,
    });
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(5),
            Constraint::Length(4),
        ])
        .split(inner)[2]
}

pub(crate) fn render_resume_dialog(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    dialog: &ResumeDialog,
    surfaces: &mut FrameSurfaces,
) {
    let popup = centered_modal(surfaces, 84, 24, area);
    frame.render_widget(Clear, popup);
    let (scanned, total) = dialog.scan_progress();
    let title = if dialog.is_scanning() {
        format!(" Resume a session · scanning {scanned}/{total} ")
    } else {
        " Resume a session ".to_owned()
    };
    let outer = Block::default().borders(Borders::ALL).title(title);
    let inner = outer.inner(popup);
    frame.render_widget(outer, popup);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(5),
            Constraint::Length(4),
        ])
        .split(inner);

    frame.render_widget(
        Tabs::new(["Mjolnir", "Import"])
            .select(dialog.tab.index())
            .divider(" │ ")
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ),
        rows[0],
    );

    let search_focused = dialog.focus == ResumeFocus::Search;
    let search = if search_focused {
        dialog.search.with_cursor_marker("▏")
    } else {
        dialog.search.to_string()
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                format!("Search: {search}"),
                if search_focused {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                } else {
                    Style::default().fg(Color::Gray)
                },
            ),
            Line::styled(
                if dialog.show_archived {
                    "Archived rows shown."
                } else {
                    "Archived rows hidden."
                },
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        rows[1],
    );

    let list_rows = dashboard.resume_rows();
    let sessions_focused = dialog.focus == ResumeFocus::Sessions;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(focus_border(sessions_focused || search_focused))
        .title(match dialog.tab {
            ResumeTab::Hel => " Mjolnir sessions · newest first ",
            ResumeTab::Import => " Importable sessions · newest first ",
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
    let items = if list_rows.is_empty() {
        vec![ListItem::new(
            match (dialog.tab, dialog.is_scanning(), dialog.search.is_empty()) {
                (ResumeTab::Import, true, _) => "Scanning native sessions…",
                (ResumeTab::Hel, _, true) => "No stopped Mjolnir sessions",
                (ResumeTab::Import, _, true) => "No importable sessions",
                _ => "No matching sessions",
            },
        )]
    } else {
        list_rows
            .iter()
            .map(|row| ListItem::new(resume_row_line(row, &layout, &now)))
            .collect()
    };
    let mut state = ListState::default().with_selected(selected_index(dialog, list_rows.len()));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol(if sessions_focused { "› " } else { "  " })
            .highlight_style(if sessions_focused {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::default()
            }),
        list_area,
        &mut state,
    );
    render_session_scrollbar(
        frame,
        rows[2],
        list_rows.len(),
        state.offset(),
        usize::from(list_area.height).max(1),
    );

    let selected = selected_index(dialog, list_rows.len()).and_then(|index| list_rows.get(index));
    let mut footer = Vec::new();
    if let Some(detail) = selected {
        footer.push(Line::styled(
            truncate_text(&detail.details, usize::from(rows[3].width)),
            Style::default().fg(Color::Gray),
        ));
    }
    let errors = dialog.errors();
    if dialog.tab == ResumeTab::Import
        && let Some(error) = errors.first()
    {
        footer.push(Line::styled(
            truncate_text(
                &format!("Scan failed for {error}"),
                usize::from(rows[3].width),
            ),
            Style::default().fg(Color::Yellow),
        ));
    }
    footer.push(Line::styled(
        match dialog.tab {
            ResumeTab::Hel => {
                "Enter resumes · a archives · d destroys · s shows archived · ←/→ tabs · / searches · Tab moves"
            }
            ResumeTab::Import => {
                "Enter imports · a archives · s shows archived · ←/→ tabs · / searches · Tab moves"
            }
        },
        Style::default().fg(Color::DarkGray),
    ));
    footer.push(action_buttons(&[
        ("Cancel", dialog.focus == ResumeFocus::Cancel),
        (
            match dialog.tab {
                ResumeTab::Hel => "Resume",
                ResumeTab::Import => "Import",
            },
            dialog.focus == ResumeFocus::Open,
        ),
    ]));
    frame.render_widget(
        Paragraph::new(footer)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        rows[3],
    );
    if dialog.is_scanning() {
        const SPINNER: [char; 4] = ['|', '/', '-', '\\'];
        let frame_index = (dialog.opened_at.elapsed().as_millis() / 125) as usize;
        frame.render_widget(
            Paragraph::new(format!(
                "{} Scanning…",
                SPINNER[frame_index % SPINNER.len()]
            ))
            .style(Style::default().fg(Color::Gray)),
            Rect::new(rows[3].x, rows[3].bottom().saturating_sub(1), 14, 1),
        );
    }
}

fn resume_header_line(layout: &RowLayout) -> Line<'static> {
    let style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled(padded_cell("PROFILE", layout.profile), style),
        Span::raw("  "),
        Span::styled(padded_cell("TARGET", layout.origin), style),
        Span::raw("  "),
        Span::styled(padded_cell("LAST ACTIVE", layout.activity), style),
        Span::raw("  "),
        Span::styled(truncate_text("SESSION", layout.title), style),
    ])
}

fn padded_cell(text: &str, width: usize) -> String {
    format!("{:<width$}", truncate_text(text, width), width = width)
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
        Style::default().fg(Color::Yellow)
    };
    let origin = match row.status.warning() {
        Some(warning) => Span::styled(
            format!("{:<width$}", warning, width = layout.origin),
            Style::default().fg(Color::Yellow),
        ),
        None => Span::styled(
            format!(
                "{:<width$}",
                truncate_text(&row.origin, layout.origin),
                width = layout.origin
            ),
            Style::default().fg(Color::Cyan),
        ),
    };
    let mut marks = String::new();
    if row.natively_archived {
        marks.push_str("  [archived in harness]");
    } else if row.archived {
        marks.push_str("  [archived]");
    }
    if row.unavailable_reason.is_some() {
        marks.push_str("  [unavailable]");
    }
    Line::from(vec![
        Span::styled(
            padded_cell(&row.profile_id, layout.profile),
            Style::default().fg(Color::Magenta),
        ),
        Span::raw("  "),
        origin,
        Span::raw("  "),
        Span::styled(
            padded_cell(
                &format_last_active(now, row.last_activity_ms),
                layout.activity,
            ),
            Style::default().fg(Color::Gray),
        ),
        Span::raw("  "),
        Span::styled(truncate_text(&row.title, layout.title), title_style),
        Span::styled(marks, Style::default().fg(Color::DarkGray)),
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
mod tests {
    use std::collections::BTreeMap;

    use crossterm::event::KeyCode;
    use hel::hel_state::STATE_VERSION;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::test_support::*;
    use crate::{DashboardState, Focus};

    /// Later than `stopped_session`'s checkpoint, so a native row built with
    /// it sorts above the Hel record.
    const NEWER_THAN_THE_CHECKPOINT: i64 = 4_000_000_000_000;

    fn native(id: &str, title: &str, last_activity_ms: i64) -> crate::ImportSessionOption {
        crate::ImportSessionOption {
            native_session_id: id.into(),
            title: title.into(),
            project_directory: "~/Projects/hel".into(),
            details: "master · 1.0KB · ~/Projects/hel".into(),
            unavailable_reason: None,
            last_activity_ms,
            natively_archived: false,
        }
    }

    fn codex_profile(sessions: Vec<crate::ImportSessionOption>) -> ImportProfileOption {
        ImportProfileOption {
            profile_id: "codex-1".into(),
            harness_kind: HarnessKind::Codex,
            sessions,
            scan_progress: Some((1, 1)),
            error: None,
        }
    }

    fn state_with(sessions: Vec<SessionRecord>) -> HelState {
        HelState {
            version: STATE_VERSION,
            sessions: sessions
                .into_iter()
                .map(|session| (session.id.clone(), session))
                .collect(),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        }
    }

    fn rows(dashboard: &DashboardState) -> Vec<ResumeRow> {
        assert!(
            matches!(dashboard.mode, Mode::ResumeDialog(_)),
            "expected the resume dialog"
        );
        dashboard.resume_rows().to_vec()
    }

    fn titles(rows: &[ResumeRow]) -> Vec<&str> {
        rows.iter().map(|row| row.title.as_str()).collect()
    }

    fn replace_search(dashboard: &mut DashboardState, search: &str) {
        let Mode::ResumeDialog(dialog) = &mut dashboard.mode else {
            panic!("expected the resume dialog");
        };
        dialog.search = search.to_owned().into();
        dashboard.rebuild_resume_rows();
    }

    fn switch_to_import(dashboard: &mut DashboardState) {
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Right)),
            DashboardAction::None
        );
        let Mode::ResumeDialog(dialog) = &dashboard.mode else {
            panic!("expected the resume dialog");
        };
        assert_eq!(dialog.tab, ResumeTab::Import);
    }

    #[test]
    fn last_active_uses_words_through_seven_days_then_a_local_date() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-23T12:00:00-05:00").unwrap();
        let before = |milliseconds| now.timestamp_millis() - milliseconds;

        assert_eq!(format_last_active(&now, before(30_000)), "just now");
        assert_eq!(format_last_active(&now, before(60_000)), "1 minute ago");
        assert_eq!(
            format_last_active(&now, before(2 * 60_000)),
            "2 minutes ago"
        );
        assert_eq!(format_last_active(&now, before(60 * 60_000)), "1 hour ago");
        assert_eq!(
            format_last_active(&now, before(24 * 60 * 60_000)),
            "1 day ago"
        );
        assert_eq!(
            format_last_active(&now, before(SEVEN_DAYS_MS)),
            "7 days ago"
        );
        assert_eq!(
            format_last_active(&now, before(SEVEN_DAYS_MS + 1)),
            "Aug 16, 2026"
        );
        assert_eq!(
            format_last_active(&now, before(9 * 24 * 60 * 60_000)),
            "Aug 14, 2026"
        );
        assert_eq!(
            format_last_active(&now, now.timestamp_millis() + 1),
            "just now"
        );
        assert_eq!(format_last_active(&now, 0), "unknown");
        assert_eq!(format_last_active(&now, i64::MAX), "unknown");
    }

    /// A Hel record and the native session it was imported from are one
    /// conversation, so the dialog shows the Hel record's row and drops the
    /// native duplicate.
    #[test]
    fn a_hel_record_replaces_the_native_session_it_was_imported_from() {
        // `stopped_session` carries native_session_id "native-1".
        let dashboard = DashboardState::new(
            config(),
            state_with(vec![stopped_session()]),
            BTreeMap::new(),
        );
        let merged = merged_resume_rows(
            &dashboard.config,
            &dashboard.state,
            &[codex_profile(vec![
                native("native-1", "Same conversation", 10),
                native("native-2", "A different conversation", 5),
            ])],
            &BTreeSet::new(),
        );

        assert_eq!(merged.len(), 2, "{:?}", titles(&merged));
        let adopted = merged
            .iter()
            .find(|row| row.key == ResumeRowKey::Hel("session-1".into()))
            .expect("the hel record keeps its row");
        assert_eq!(adopted.title, "ACP pretty name");
        assert_eq!(adopted.origin, "podman");
        assert!(
            !merged
                .iter()
                .any(|row| row.key == ResumeRowKey::Native(HarnessKind::Codex, "native-1".into())),
            "the native duplicate is gone"
        );
        // A native session with no Hel record still shows, marked local.
        let native_only = merged
            .iter()
            .find(|row| row.key == ResumeRowKey::Native(HarnessKind::Codex, "native-2".into()))
            .expect("the unadopted native session keeps its row");
        assert_eq!(native_only.origin, "local/hel");
    }

    #[test]
    fn resume_and_import_targets_include_the_project_like_live_summaries() {
        let mut local = stopped_session();
        local.id = "local-session".into();
        local.native_session_id = None;
        local.target_template_id = "localhost".into();
        local.project_directory = Some("/mnt/optane/bifrost-fird".into());

        let mut remote = stopped_session();
        remote.id = "remote-session".into();
        remote.native_session_id = None;
        remote.target_template_id = "precision-3260".into();
        remote.project_directory = Some("/home/jonathan/Projects/bifrost".into());

        let mut config = config();
        config.targets.insert(
            "localhost".into(),
            hel::hel_config::TargetTemplate::LocalBare,
        );
        config.targets.insert(
            "precision-3260".into(),
            hel::hel_config::TargetTemplate::SshBare {
                ssh: hel::hel_config::SshConnection {
                    host: "precision-3260".into(),
                    user: None,
                    identity_file: None,
                    extra_args: Vec::new(),
                },
                permissions: hel::hel_config::PermissionMode::Yolo,
                workspace_prefix: ".local/share/hel/workspaces".into(),
            },
        );
        let mut dashboard =
            DashboardState::new(config, state_with(vec![local, remote]), BTreeMap::new());
        dashboard.show_resume_dialog(
            1,
            vec![codex_profile(vec![native(
                "native-only",
                "Native project",
                NEWER_THAN_THE_CHECKPOINT,
            )])],
        );

        let hel_targets = rows(&dashboard)
            .into_iter()
            .map(|row| row.origin)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            hel_targets,
            BTreeSet::from([
                "localhost/bifrost-fird".to_owned(),
                "precision-3260/bifrost".to_owned(),
            ])
        );

        let mut terminal = Terminal::new(TestBackend::new(140, 34)).expect("terminal");
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .expect("draw the resume dialog");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        for target in ["localhost/bifrost-fird", "precision-3260/bifrost"] {
            assert!(rendered.contains(target), "{rendered}");
        }

        switch_to_import(&mut dashboard);
        assert_eq!(rows(&dashboard)[0].origin, "local/hel");
    }

    /// The native session behind a live Hel session must not be offered as an
    /// import: that would start a second Hel session on the same conversation.
    #[test]
    fn a_live_session_hides_its_native_counterpart_from_the_dialog() {
        let mut live = stopped_session();
        live.state = SessionState::Running;
        let merged = merged_resume_rows(
            &config(),
            &state_with(vec![live]),
            &[codex_profile(vec![
                native("native-1", "Running under Hel right now", 10),
                native("native-2", "Idle native session", 5),
            ])],
            &BTreeSet::new(),
        );
        assert_eq!(titles(&merged), ["Idle native session"]);
    }

    /// A record whose target was renamed or removed from config still reports
    /// the target it actually ran on.
    #[test]
    fn the_origin_chip_shows_the_stored_target_even_when_config_forgot_it() {
        let mut session = stopped_session();
        session.target_template_id = "retired-target".into();
        let mut config = config();
        config.targets.clear();
        let merged = merged_resume_rows(&config, &state_with(vec![session]), &[], &BTreeSet::new());
        assert_eq!(merged[0].origin, "retired-target");
    }

    /// One order across the merged list: newest activity first, whichever
    /// source the row came from. Hel records date from their checkpoint,
    /// native sessions from the file's modification time.
    #[test]
    fn rows_sort_by_last_activity_descending_across_both_sources() {
        let mut old_record = stopped_session();
        old_record.id = "old-record".into();
        old_record.native_session_id = None;
        old_record.checkpoint.as_mut().unwrap().created_at = "2026-01-01T00:00:00Z".into();
        let mut new_record = stopped_session();
        new_record.id = "new-record".into();
        new_record.native_session_id = None;
        new_record.acp_session_title = Some("Newest record".into());
        new_record.checkpoint.as_mut().unwrap().created_at = "2026-06-01T00:00:00Z".into();

        let january = 1_767_225_600_000; // 2026-01-01T00:00:00Z
        let march = 1_772_409_600_000; // 2026-03-01T00:00:00Z
        let july = 1_782_950_400_000; // 2026-07-01T00:00:00Z
        let merged = merged_resume_rows(
            &config(),
            &state_with(vec![old_record, new_record]),
            &[codex_profile(vec![
                native("native-mid", "Native March", march),
                native("native-new", "Native July", july),
            ])],
            &BTreeSet::new(),
        );

        assert_eq!(
            titles(&merged),
            [
                "Native July",
                "Newest record",
                "Native March",
                "ACP pretty name",
            ]
        );
        assert_eq!(merged[3].last_activity_ms, january);
    }

    /// Archiving hides rows from the default view in each source tab, and the
    /// shared toggle brings them back.
    #[test]
    fn the_archived_toggle_covers_the_record_flag_and_the_native_hidden_set() {
        let mut hidden_record = stopped_session();
        hidden_record.id = "hidden-record".into();
        hidden_record.native_session_id = None;
        hidden_record.acp_session_title = Some("Hidden record".into());
        hidden_record.archived = true;
        let mut visible_record = stopped_session();
        visible_record.id = "visible-record".into();
        visible_record.native_session_id = None;
        visible_record.acp_session_title = Some("Visible record".into());

        let mut dashboard = DashboardState::new(
            config(),
            state_with(vec![hidden_record, visible_record]),
            BTreeMap::new(),
        );
        dashboard.set_hidden_native_sessions(BTreeSet::from([(
            HarnessKind::Codex,
            "native-hidden".to_owned(),
        )]));
        dashboard.show_resume_dialog(
            1,
            vec![codex_profile(vec![
                native("native-hidden", "Hidden native", 1),
                native("native-shown", "Shown native", 2),
            ])],
        );

        assert_eq!(titles(&rows(&dashboard)), ["Visible record"]);

        dashboard.handle_key(key(KeyCode::Char('s')));
        assert_eq!(
            titles(&rows(&dashboard)),
            ["Hidden record", "Visible record"]
        );

        switch_to_import(&mut dashboard);
        assert_eq!(titles(&rows(&dashboard)), ["Shown native", "Hidden native"]);

        dashboard.handle_key(key(KeyCode::Char('s')));
        assert_eq!(titles(&rows(&dashboard)), ["Shown native"]);
    }

    /// Codex's own archived threads are mirrored one way: hidden by default,
    /// listed under the toggle, and never unarchivable from Hel.
    #[test]
    fn natively_archived_rows_are_shown_only_under_the_toggle_and_never_unarchived() {
        let mut natively_archived = native("native-codex", "Archived in Codex", 1);
        natively_archived.natively_archived = true;
        let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
        dashboard.show_resume_dialog(1, vec![codex_profile(vec![natively_archived])]);
        switch_to_import(&mut dashboard);

        assert!(rows(&dashboard).is_empty());
        dashboard.handle_key(key(KeyCode::Char('s')));
        assert_eq!(titles(&rows(&dashboard)), ["Archived in Codex"]);

        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('a'))),
            DashboardAction::None
        );
        assert!(
            dashboard
                .notices
                .current()
                .unwrap_or_default()
                .contains("archived in its own harness")
        );
        assert!(rows(&dashboard)[0].natively_archived);
    }

    /// Archiving reports the persistence the caller must do, and hides the row
    /// straight away rather than waiting for that write.
    #[test]
    fn archiving_a_row_hides_it_immediately_and_reports_the_write_to_persist() {
        let mut dashboard = DashboardState::new(
            config(),
            state_with(vec![stopped_session()]),
            BTreeMap::new(),
        );
        dashboard.show_resume_dialog(
            1,
            vec![codex_profile(vec![native(
                "native-2",
                "Native",
                NEWER_THAN_THE_CHECKPOINT,
            )])],
        );
        assert_eq!(titles(&rows(&dashboard)), ["ACP pretty name"]);

        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('a'))),
            DashboardAction::SetSessionArchived {
                session_id: "session-1".into(),
                archived: true,
            }
        );
        assert!(rows(&dashboard).is_empty());

        switch_to_import(&mut dashboard);
        assert_eq!(titles(&rows(&dashboard)), ["Native"]);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('a'))),
            DashboardAction::SetNativeSessionHidden {
                harness_kind: HarnessKind::Codex,
                native_session_id: "native-2".into(),
                hidden: true,
            }
        );
        assert!(rows(&dashboard).is_empty());

        // The shared toggle reveals the current tab, and archiving again
        // reverses the native hidden-set write.
        dashboard.handle_key(key(KeyCode::Char('s')));
        assert_eq!(titles(&rows(&dashboard)), ["Native"]);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('a'))),
            DashboardAction::SetNativeSessionHidden {
                harness_kind: HarnessKind::Codex,
                native_session_id: "native-2".into(),
                hidden: false,
            }
        );
    }

    /// A lost or force-destroyed session cannot be resumed; deleting its
    /// record is the only thing left to do with it.
    #[test]
    fn lost_and_destroyed_rows_are_marked_and_refuse_to_resume() {
        for (state, marker, reason) in [
            (
                SessionState::Lost,
                "⚠ lost",
                "lost without a verified checkpoint",
            ),
            (
                SessionState::DestroyedWithDataLoss,
                "⚠ data lost",
                "force-destroyed",
            ),
        ] {
            let mut session = stopped_session();
            session.state = state;
            let mut dashboard =
                DashboardState::new(config(), state_with(vec![session]), BTreeMap::new());
            dashboard.show_resume_dialog(1, Vec::new());

            let row = &rows(&dashboard)[0];
            assert!(!row.status.is_recoverable());
            assert_eq!(row.status.warning(), Some(marker));
            assert!(row.details.contains(reason), "{}", row.details);

            assert_eq!(
                dashboard.handle_key(key(KeyCode::Enter)),
                DashboardAction::None
            );
            assert!(matches!(dashboard.mode, Mode::ResumeDialog(_)));
            let notice = dashboard.notices.current().unwrap_or_default();
            assert!(notice.contains(reason), "{notice}");
            assert!(notice.contains("destroy its record"), "{notice}");

            dashboard.handle_key(key(KeyCode::Char('d')));
            assert!(matches!(dashboard.mode, Mode::Confirm(_)));
        }
    }

    /// Hel never modifies a harness home, so a native-only row has no destroy action.
    #[test]
    fn a_native_only_row_cannot_be_destroyed_from_hel() {
        let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
        dashboard.show_resume_dialog(
            1,
            vec![codex_profile(vec![native(
                "native-2",
                "Native",
                NEWER_THAN_THE_CHECKPOINT,
            )])],
        );
        switch_to_import(&mut dashboard);

        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('d'))),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::ResumeDialog(_)));
        assert!(
            dashboard
                .notices
                .current()
                .unwrap_or_default()
                .contains("never destroys")
        );
    }

    /// The default Hel tab and the Import tab each expose only the source they
    /// name, with a valid selection after every switch.
    #[test]
    fn tabs_separate_hel_records_from_importable_native_sessions() {
        let mut dashboard = DashboardState::new(
            config(),
            state_with(vec![stopped_session()]),
            BTreeMap::new(),
        );
        dashboard.show_resume_dialog(
            1,
            vec![codex_profile(vec![native(
                "native-2",
                "Native",
                NEWER_THAN_THE_CHECKPOINT,
            )])],
        );

        assert_eq!(titles(&rows(&dashboard)), ["ACP pretty name"]);
        let Mode::ResumeDialog(dialog) = &dashboard.mode else {
            panic!("expected the resume dialog");
        };
        assert_eq!(dialog.tab, ResumeTab::Hel);
        assert_eq!(dialog.selected, Some(ResumeRowKey::Hel("session-1".into())));

        switch_to_import(&mut dashboard);
        assert_eq!(titles(&rows(&dashboard)), ["Native"]);
        let Mode::ResumeDialog(dialog) = &dashboard.mode else {
            panic!("expected the resume dialog");
        };
        assert_eq!(
            dialog.selected,
            Some(ResumeRowKey::Native(HarnessKind::Codex, "native-2".into()))
        );

        dashboard.handle_key(key(KeyCode::Left));
        assert_eq!(titles(&rows(&dashboard)), ["ACP pretty name"]);
    }

    /// The dialog's own footer advertises the arrows as the tab switch, so
    /// they have to answer from the search field too: typing a query is the
    /// most likely thing to be doing when the wrong tab is in front of you.
    #[test]
    fn arrow_keys_switch_tabs_even_while_the_search_field_has_focus() {
        let mut dashboard = DashboardState::new(
            config(),
            state_with(vec![stopped_session()]),
            BTreeMap::new(),
        );
        dashboard.show_resume_dialog(
            1,
            vec![codex_profile(vec![native(
                "native-2",
                "Native",
                NEWER_THAN_THE_CHECKPOINT,
            )])],
        );

        dashboard.handle_key(key(KeyCode::Char('/')));
        for character in "nat".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        let Mode::ResumeDialog(dialog) = &dashboard.mode else {
            panic!("expected the resume dialog");
        };
        assert_eq!(dialog.focus, ResumeFocus::Search);
        assert_eq!(dialog.search.value(), "nat");

        dashboard.handle_key(key(KeyCode::Right));
        let Mode::ResumeDialog(dialog) = &dashboard.mode else {
            panic!("expected the resume dialog");
        };
        assert_eq!(dialog.tab, ResumeTab::Import);
        assert_eq!(dialog.focus, ResumeFocus::Search, "the query is still open");

        dashboard.handle_key(key(KeyCode::Left));
        let Mode::ResumeDialog(dialog) = &dashboard.mode else {
            panic!("expected the resume dialog");
        };
        assert_eq!(dialog.tab, ResumeTab::Hel);
    }

    /// Selecting a row dispatches to the flow that suits its source: the
    /// resume wizard for a Hel record, the import flow for a native session.
    #[test]
    fn selecting_a_row_resumes_a_hel_record_and_imports_a_native_session() {
        let mut dashboard = DashboardState::new(
            config(),
            state_with(vec![stopped_session()]),
            BTreeMap::new(),
        );
        dashboard.show_resume_dialog(
            1,
            vec![codex_profile(vec![native(
                "native-2",
                "Native",
                NEWER_THAN_THE_CHECKPOINT,
            )])],
        );

        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::Resume(_)));

        dashboard.show_resume_dialog(
            2,
            vec![codex_profile(vec![native(
                "native-2",
                "Native",
                NEWER_THAN_THE_CHECKPOINT,
            )])],
        );
        switch_to_import(&mut dashboard);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::ImportSession {
                profile_id: "codex-1".into(),
                native_session_id: "native-2".into(),
                display_title: "Native".into(),
            }
        );
    }

    /// The dashboard lists live sessions; the dialog lists the rest. Nothing
    /// appears in both, and a stop in progress stays on the dashboard until
    /// the state machine reaches Stopped.
    #[test]
    fn the_dashboard_shows_live_sessions_and_the_dialog_shows_the_rest() {
        let mut sessions = Vec::new();
        for (index, state) in [
            SessionState::Provisioning,
            SessionState::Running,
            SessionState::Disconnected,
            SessionState::Checkpointing,
            SessionState::Closing,
            SessionState::Destroying,
            SessionState::Error,
            SessionState::Stopped,
            SessionState::Lost,
            SessionState::DestroyedWithDataLoss,
        ]
        .into_iter()
        .enumerate()
        {
            let mut session = stopped_session();
            session.id = format!("session-{index:02}");
            session.native_session_id = None;
            session.state = state;
            sessions.push(session);
        }
        let mut dashboard = DashboardState::new(config(), state_with(sessions), BTreeMap::new());
        dashboard.show_resume_dialog(1, Vec::new());

        let on_dashboard = dashboard
            .ordered_sessions()
            .iter()
            .map(|session| session.state)
            .collect::<Vec<_>>();
        assert_eq!(on_dashboard.len(), 7);
        assert!(on_dashboard.iter().all(|state| state.is_active()));
        // Closing and Checkpointing are mid-stop and must not vanish.
        assert!(on_dashboard.contains(&SessionState::Closing));
        assert!(on_dashboard.contains(&SessionState::Checkpointing));

        let in_dialog = rows(&dashboard)
            .into_iter()
            .map(|row| row.key)
            .collect::<Vec<_>>();
        assert_eq!(in_dialog.len(), 3);
        let dashboard_ids = dashboard
            .ordered_sessions()
            .iter()
            .map(|session| ResumeRowKey::Hel(session.id.clone()))
            .collect::<Vec<_>>();
        assert!(
            in_dialog.iter().all(|key| !dashboard_ids.contains(key)),
            "no session is listed in both places"
        );
    }

    /// Scans arrive one profile at a time; folding one in must not move the
    /// Import-tab selection off the native row the user was on.
    #[test]
    fn an_incremental_scan_update_keeps_the_selected_row() {
        let mut dashboard = DashboardState::new(
            config(),
            state_with(vec![stopped_session()]),
            BTreeMap::new(),
        );
        dashboard.show_resume_dialog(1, vec![codex_profile(vec![native("native-2", "Older", 1)])]);
        switch_to_import(&mut dashboard);
        assert_eq!(titles(&rows(&dashboard)), ["Older"]);
        let Mode::ResumeDialog(dialog) = &dashboard.mode else {
            panic!("expected the resume dialog");
        };
        assert_eq!(dialog.row_index, 0);

        // A newer native session arrives and sorts above the selected row.
        dashboard.apply_resume_profile(
            1,
            codex_profile(vec![
                native("native-2", "Older", 1),
                native("native-3", "Newer", NEWER_THAN_THE_CHECKPOINT),
            ]),
        );
        let Mode::ResumeDialog(dialog) = &dashboard.mode else {
            panic!("expected the resume dialog");
        };
        assert_eq!(
            dialog.selected,
            Some(ResumeRowKey::Native(HarnessKind::Codex, "native-2".into()))
        );
        assert_eq!(dialog.row_index, 1);
        // A late update for another discovery is ignored.
        dashboard.apply_resume_profile(99, codex_profile(Vec::new()));
        assert_eq!(rows(&dashboard).len(), 2);
    }

    /// A scan that failed is reported rather than dropped.
    #[test]
    fn a_failed_profile_scan_is_reported_in_the_dialog() {
        let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
        dashboard.show_resume_dialog(
            1,
            vec![ImportProfileOption {
                profile_id: "codex-1".into(),
                harness_kind: HarnessKind::Codex,
                sessions: Vec::new(),
                scan_progress: None,
                error: Some("permission denied".into()),
            }],
        );
        switch_to_import(&mut dashboard);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .expect("draw the resume dialog");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("Scan failed for codex-1"), "{rendered}");
    }

    #[test]
    fn resume_table_has_headers_repeated_profiles_and_last_active_values() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
        dashboard.show_resume_dialog(
            1,
            vec![codex_profile(vec![
                native("native-1", "Recent session", now_ms - 2 * 60_000),
                native("native-2", "Older session", now_ms - 60 * 60_000),
            ])],
        );
        switch_to_import(&mut dashboard);

        let mut terminal = Terminal::new(TestBackend::new(140, 34)).expect("terminal");
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .expect("draw the resume dialog");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");

        for heading in ["PROFILE", "TARGET", "LAST ACTIVE", "SESSION"] {
            assert!(rendered.contains(heading), "{rendered}");
        }
        for title in ["Recent session", "Older session"] {
            let row = rendered
                .lines()
                .find(|line| line.contains(title))
                .expect("rendered session row");
            assert!(row.contains("codex-1"), "{row}");
        }
        assert!(rendered.contains("2 minutes ago"), "{rendered}");
        assert!(rendered.contains("1 hour ago"), "{rendered}");
        assert!(rendered.contains("Search:"), "{rendered}");
    }

    /// The dialog is the only surface for non-live sessions, and `Alt-S`
    /// opens it from anywhere.
    #[test]
    fn the_dashboard_opens_the_dialog_and_names_the_key_in_the_footer() {
        let mut dashboard = dashboard_with_session(running_session());
        assert_eq!(
            dashboard.handle_key(alt_key('s')),
            DashboardAction::OpenResumeDialog
        );
        assert_eq!(dashboard.handle_key(ctrl_key('t')), DashboardAction::None);
        assert_eq!(dashboard.focus, Focus::Sessions);

        let mut terminal = Terminal::new(TestBackend::new(140, 30)).expect("terminal");
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .expect("draw the dashboard");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("Alt-S resume"), "{rendered}");
        assert!(!rendered.contains("Import"), "{rendered}");
    }

    /// The rows are derived state, rebuilt where their inputs change. A state
    /// reload, a checkpoint size that arrives from the background, and a
    /// hidden set read out of the database all reach the open dialog straight
    /// away.
    #[test]
    fn the_row_list_follows_state_reloads_and_background_updates_while_the_dialog_is_open() {
        let mut dashboard = DashboardState::new(
            config(),
            state_with(vec![stopped_session()]),
            BTreeMap::new(),
        );
        dashboard.show_resume_dialog(
            1,
            vec![codex_profile(vec![native(
                "native-2",
                "Native",
                NEWER_THAN_THE_CHECKPOINT,
            )])],
        );
        assert_eq!(titles(&rows(&dashboard)), ["ACP pretty name"]);

        let mut reloaded = stopped_session();
        reloaded.id = "session-2".into();
        reloaded.native_session_id = None;
        reloaded.acp_session_title = Some("Reloaded record".into());
        dashboard.set_state(state_with(vec![stopped_session(), reloaded]));
        assert!(
            titles(&rows(&dashboard)).contains(&"Reloaded record"),
            "{:?}",
            titles(&rows(&dashboard))
        );

        dashboard.apply_checkpoint_archive_sizes(BTreeMap::from([(
            "session-1".to_owned(),
            Some(2_048),
        )]));
        let listed = rows(&dashboard);
        let sized = listed
            .iter()
            .find(|row| row.key == ResumeRowKey::Hel("session-1".into()))
            .expect("the checkpointed record");
        assert!(sized.details.contains("2.0K"), "{}", sized.details);

        switch_to_import(&mut dashboard);
        assert_eq!(titles(&rows(&dashboard)), ["Native"]);
        dashboard.set_hidden_native_sessions(BTreeSet::from([(
            HarnessKind::Codex,
            "native-2".to_owned(),
        )]));
        assert!(
            !titles(&rows(&dashboard)).contains(&"Native"),
            "{:?}",
            titles(&rows(&dashboard))
        );
    }

    /// Walking the list moves the selection over rows that stay put: an arrow
    /// key changes nothing the rows are built from.
    #[test]
    fn arrow_navigation_moves_the_selection_without_changing_the_row_list() {
        let mut dashboard = DashboardState::new(
            config(),
            state_with(vec![stopped_session()]),
            BTreeMap::new(),
        );
        dashboard.show_resume_dialog(
            1,
            vec![codex_profile(vec![
                native("native-2", "Native newer", NEWER_THAN_THE_CHECKPOINT),
                native("native-3", "Native older", 1),
            ])],
        );
        switch_to_import(&mut dashboard);
        let before = rows(&dashboard);
        assert_eq!(before.len(), 2, "{:?}", titles(&before));

        dashboard.handle_key(key(KeyCode::Down));

        assert_eq!(rows(&dashboard), before, "navigation rebuilt the rows");
        let Mode::ResumeDialog(dialog) = &dashboard.mode else {
            panic!("expected the resume dialog");
        };
        assert_eq!(dialog.row_index, 1);
        assert_eq!(dialog.selected, Some(before[1].key.clone()));
    }

    /// Search is one of the inputs the rows are built from, so each keystroke
    /// narrows what the dialog lists, and erasing it restores them.
    #[test]
    fn typing_a_search_narrows_the_visible_rows() {
        let mut dashboard = DashboardState::new(
            config(),
            state_with(vec![stopped_session()]),
            BTreeMap::new(),
        );
        dashboard.show_resume_dialog(
            1,
            vec![codex_profile(vec![
                native("native-2", "Native alpha", NEWER_THAN_THE_CHECKPOINT),
                native("native-3", "Native beta", 1),
            ])],
        );
        switch_to_import(&mut dashboard);
        assert_eq!(rows(&dashboard).len(), 2);

        dashboard.handle_key(key(KeyCode::Char('/')));
        for character in "alpha".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(titles(&rows(&dashboard)), ["Native alpha"]);

        for _ in 0.."alpha".len() {
            dashboard.handle_key(key(KeyCode::Backspace));
        }
        assert_eq!(rows(&dashboard).len(), 2);
    }

    #[test]
    fn search_matches_every_row_text_field_case_insensitively() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut dashboard = DashboardState::new(
            config(),
            state_with(vec![stopped_session()]),
            BTreeMap::new(),
        );
        dashboard.show_resume_dialog(
            1,
            vec![codex_profile(vec![
                native("native-2", "Native alpha", now_ms - 2 * 60_000),
                native("native-3", "Native beta", 1),
            ])],
        );

        for (query, expected) in [
            ("ACP PRETTY", vec!["ACP pretty name"]),
            ("PODMAN", vec!["ACP pretty name"]),
            ("CODEX-1", vec!["ACP pretty name"]),
        ] {
            replace_search(&mut dashboard, query);
            assert_eq!(titles(&rows(&dashboard)), expected, "query {query:?}");
        }

        replace_search(&mut dashboard, "");
        switch_to_import(&mut dashboard);
        for (query, expected) in [
            ("LOCAL", vec!["Native alpha", "Native beta"]),
            ("MINUTES AGO", vec!["Native alpha"]),
            ("MASTER", vec!["Native alpha", "Native beta"]),
            ("CODEX-1", vec!["Native alpha", "Native beta"]),
        ] {
            replace_search(&mut dashboard, query);
            assert_eq!(titles(&rows(&dashboard)), expected, "query {query:?}");
        }
    }

    /// Cost of the merged row list and of one keypress, on a dialog the size a
    /// long-lived harness home produces. Run with
    /// `cargo test -p brokk-mj-tui resume_row_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing measurement, not a behavior assertion"]
    fn resume_row_cost_for_a_few_thousand_sessions() {
        const NATIVE: usize = 4_000;
        const RECORDS: usize = 400;
        const ROUNDS: usize = 200;

        let records = (0..RECORDS)
            .map(|index| {
                let mut session = stopped_session();
                session.id = format!("session-{index:04}");
                session.native_session_id = None;
                session.acp_session_title = Some(format!("Record {index}"));
                session
            })
            .collect::<Vec<_>>();
        let sessions = (0..NATIVE)
            .map(|index| {
                native(
                    &format!("native-{index:04}"),
                    &format!("Native conversation {index}"),
                    index as i64,
                )
            })
            .collect::<Vec<_>>();
        let mut dashboard = DashboardState::new(config(), state_with(records), BTreeMap::new());
        dashboard.show_resume_dialog(1, vec![codex_profile(sessions)]);
        switch_to_import(&mut dashboard);

        let Mode::ResumeDialog(dialog) = dashboard.mode.clone() else {
            panic!("expected the resume dialog");
        };
        // What one rebuild costs: the merge, the sizes, and search.
        let started = Instant::now();
        let mut built = 0;
        for _ in 0..ROUNDS {
            built += build_resume_rows(
                &dashboard.config,
                &dashboard.state,
                &dialog,
                &dashboard.hidden_native_sessions,
                &dashboard.checkpoint_archive_sizes,
                &chrono::Local::now(),
            )
            .len();
        }
        let rebuild = started.elapsed();

        // What the dialog actually pays per key press, which reads the rows
        // rather than rebuilding them.
        let started = Instant::now();
        for _ in 0..ROUNDS {
            dashboard.handle_key(key(KeyCode::Down));
        }
        let keypresses = started.elapsed();

        println!(
            "rows={} rebuild={:?} per_rebuild={:?} keypresses={:?} per_key={:?}",
            built / ROUNDS,
            rebuild,
            rebuild / ROUNDS as u32,
            keypresses,
            keypresses / ROUNDS as u32,
        );
    }

    /// The in-dialog keys are advertised where the user can see them.
    #[test]
    fn the_dialog_footer_names_its_own_keys() {
        let mut dashboard = DashboardState::new(
            config(),
            state_with(vec![stopped_session()]),
            BTreeMap::new(),
        );
        dashboard.show_resume_dialog(1, Vec::new());
        let mut terminal = Terminal::new(TestBackend::new(120, 34)).expect("terminal");
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .expect("draw the resume dialog");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        for hint in [
            "Mjolnir",
            "Import",
            "a archives",
            "d destroys",
            "s shows archived",
            "←/→ tabs",
            "/ searches",
        ] {
            assert!(rendered.contains(hint), "{rendered}");
        }

        switch_to_import(&mut dashboard);
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .expect("draw the Import tab");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("Enter imports"), "{rendered}");
        assert!(!rendered.contains("d destroys"), "{rendered}");
    }
}
