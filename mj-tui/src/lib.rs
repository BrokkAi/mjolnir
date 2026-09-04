//! State and input handling for Hel's combined terminal surface.
//!
//! One screen holds the Sessions pane, the conversation, the Prompt composer,
//! and the Targets and Quota summaries; [`crate::combined::render_combined`]
//! draws it. This module owns what that surface knows and what a key press
//! means to it.
//!
//! It deliberately has no provisioning or persistence side effects. Input is
//! reduced to [`DashboardAction`] values for the controller to run.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use hel::hel_config::{HarnessKind, HelConfig, TargetTemplate as HelTargetTemplate};
use hel::hel_state::{
    HelState, ProjectSourceIdentity, SessionRecord, SessionResourceAllocation, SessionState,
};
use hel::hel_targets::AdditionalMount;
use mj_chat::hel_chat::Notices;
use mj_chat::hel_selection::FrameSurfaces;
use mj_controller::hel_quota::ProfileQuota;

use crate::dialogs::{
    ConfigIdEditor, ConfirmDialog, Confirmation, ContainerEditor, FORCE_STOP_CONFIRMATION,
    ImportBundleConfirmation, ImportProgress, RenameEditor, RenameFocus, RepositoryOriginDialog,
    TargetActionsDialog, WebDialog,
};
use crate::help::HelpOverlay;
use crate::ingest::{CapacityDetail, SessionDetail, SessionOperationDisplay};
use crate::palette::CommandPalette;
use crate::resume::ResumeDialog;
use crate::wizards::{MountFocus, NewWizard, ResumeWizard, WizardStep};

mod actions;
mod combined;
mod dialogs;
mod help;
mod ingest;
mod palette;
mod render;
mod resume;
mod widgets;
mod wizards;

#[cfg(test)]
mod docs_screenshots;
#[cfg(test)]
mod test_support;

pub use crate::actions::{CommandId, global_chord};
pub use crate::combined::render_combined;
pub use crate::dialogs::{ImportProfileOption, ImportSessionOption};
pub use crate::ingest::{
    MaterializedProjectionCache, PreparedMaterializedSessionDetail,
    PreparedMaterializedSessionSummary,
};
pub use crate::resume::resume_profile_placeholders;

/// One drawn row of the Sessions pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionsRow {
    /// A project name with its 1-9 toggle number. Focused mode only.
    ProjectHeading {
        key: String,
        label: String,
        number: Option<usize>,
    },
    /// A live session, by index into `ordered_sessions()`. `expanded` picks
    /// the four-row form over the one-line form.
    Session { index: usize, expanded: bool },
}

/// Sessions, targets, and quotas. Sessions that are not live live in
/// the resume dialog instead of a support pane.
pub(crate) const DASHBOARD_PANE_COUNT: usize = 3;

/// Maximum gap between two left clicks on the same session row for the pair
/// to count as a double click.
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// A side effect requested by the dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashboardAction {
    None,
    Open {
        session_id: String,
    },
    CreateSession {
        profile_id: String,
        bundle_id: String,
        project_directory: Option<std::path::PathBuf>,
        target_template_id: String,
        additional_mounts: Vec<AdditionalMount>,
        allow_dirty_local: bool,
        resource_allocation: Option<SessionResourceAllocation>,
    },
    CompleteMountSource {
        target_template_id: String,
        prefix: String,
    },
    ValidateMountSource {
        target_template_id: String,
        source: String,
    },
    ValidateSessionMounts {
        target_template_id: String,
        mounts: Vec<AdditionalMount>,
        launch: Box<DashboardAction>,
    },
    ValidateProjectDirectory {
        target_template_id: String,
        directory: String,
    },
    ResumeSession {
        session_id: String,
        profile_id: String,
        target_template_id: String,
        additional_mounts: Vec<AdditionalMount>,
        resource_allocation: Option<SessionResourceAllocation>,
        discard_queue: bool,
    },
    PreflightResumeRepositories {
        launch: Box<DashboardAction>,
    },
    ReplaceResumeRepositoryOrigin {
        session_id: String,
        repository_id: String,
        replacement: String,
        launch: Box<DashboardAction>,
    },
    CancelOperation {
        session_id: String,
        kind: SessionOperationKind,
    },
    ResolveAwsResourceOptions {
        target_template_ids: Vec<String>,
    },
    CreateBundle {
        source: String,
    },
    Close {
        session_id: String,
    },
    ForceStop {
        session_id: String,
    },
    DestroyStopped {
        session_id: String,
    },
    ForceDestroy {
        session_id: String,
    },
    RenameSession {
        session_id: String,
        title: String,
    },
    /// Re-probe every target's capacity and ask every profile for its quota
    /// again. One key does both, so there is one action rather than two.
    RefreshAll,
    RenameProfile {
        old_id: String,
        new_id: String,
    },
    RenameTarget {
        old_id: String,
        new_id: String,
    },
    TestTarget {
        target_id: String,
    },
    CancelTargetTest,
    LoadWebAccess,
    /// Read the system clipboard on a worker before applying its contents.
    /// Clipboard providers may perform IPC and must never run on the TUI loop.
    PasteFromClipboard,
    MarkAllRead {
        receipts: Vec<(String, u64)>,
    },
    OpenResumeDialog,
    /// Hide or reveal one Hel session record in the resume dialog.
    SetSessionArchived {
        session_id: String,
        archived: bool,
    },
    /// Hide or reveal one native session in the resume dialog. Hel records the
    /// choice in its own database; the harness home is never written.
    SetNativeSessionHidden {
        harness_kind: HarnessKind,
        native_session_id: String,
        hidden: bool,
    },
    ImportSession {
        profile_id: String,
        native_session_id: String,
        display_title: String,
    },
    CancelImport,
    ConfirmImportBundle {
        accepted: bool,
        include_untracked: bool,
    },
    OpenConfig,
    /// Per-session container provisioning inputs, taking effect the next time
    /// the container is created.
    SaveContainerSettings {
        session_id: String,
        cpus: Option<String>,
        memory: Option<String>,
        additional_mounts: Vec<AdditionalMount>,
        mount_history: Vec<std::path::PathBuf>,
    },
    OpenWorkspacePicker,
    QuitDetach,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebViewerAccess {
    Ready {
        viewer_url: String,
        viewer_code: String,
        qr_login_url: Option<String>,
        fallback_reason: Option<String>,
    },
    Unavailable(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOperationKind {
    Launching,
    Resuming,
    Stopping,
    Destroying,
    Connecting,
    Importing,
}

impl SessionOperationKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Launching => "Launch",
            Self::Resuming => "Resuming",
            Self::Stopping => "Stopping",
            Self::Destroying => "Destroying",
            Self::Connecting => "Connecting",
            Self::Importing => "Importing",
        }
    }
}

/// Which part of the combined surface owns the keyboard.
///
/// The three support panes and the composer are one Tab ring; the transcript
/// is not a stop on it, because it is read with the wheel and PageUp/PageDown
/// rather than driven from the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sessions,
    Quota,
    Targets,
    Prompt,
}

/// A dashboard pane whose height the user can control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportPane {
    Sessions,
    Targets,
    Quota,
}

impl SupportPane {
    #[must_use]
    pub const fn focus(self) -> Focus {
        match self {
            Self::Sessions => Focus::Sessions,
            Self::Targets => Focus::Targets,
            Self::Quota => Focus::Quota,
        }
    }
}

impl Focus {
    #[must_use]
    pub const fn support_pane(self) -> Option<SupportPane> {
        match self {
            Self::Sessions => Some(SupportPane::Sessions),
            Self::Targets => Some(SupportPane::Targets),
            Self::Quota => Some(SupportPane::Quota),
            Self::Prompt => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionDirection {
    Up,
    Down,
}

/// Tab order, which follows the layout down the screen: Sessions above the
/// conversation, the composer under it, then the two support panes. Shift-Tab
/// walks it backwards.
pub(crate) const FOCUS_ORDER: [Focus; 4] =
    [Focus::Sessions, Focus::Prompt, Focus::Targets, Focus::Quota];

/// The explicit height requested for one support pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneSize {
    Minimized,
    #[default]
    Standard,
    Maximized,
}

impl PaneSize {
    /// The next title-bar control, wrapping from maximum to minimum.
    #[must_use]
    pub fn cycled(self) -> Self {
        match self {
            Self::Minimized => Self::Standard,
            Self::Standard => Self::Maximized,
            Self::Maximized => Self::Minimized,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PaneSizes {
    sessions: PaneSize,
    targets: PaneSize,
    quota: PaneSize,
}

impl PaneSizes {
    fn get(self, pane: SupportPane) -> PaneSize {
        match pane {
            SupportPane::Sessions => self.sessions,
            SupportPane::Targets => self.targets,
            SupportPane::Quota => self.quota,
        }
    }

    fn get_mut(&mut self, pane: SupportPane) -> &mut PaneSize {
        match pane {
            SupportPane::Sessions => &mut self.sessions,
            SupportPane::Targets => &mut self.targets,
            SupportPane::Quota => &mut self.quota,
        }
    }

    fn all_standard(self) -> bool {
        [self.sessions, self.targets, self.quota]
            .into_iter()
            .all(|size| size == PaneSize::Standard)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mode {
    Dashboard,
    New(NewWizard),
    Resume(ResumeWizard),
    /// The unified picker for every session that is not live.
    ResumeDialog(ResumeDialog),
    RepositoryOrigin(RepositoryOriginDialog),
    ConfigId(ConfigIdEditor),
    TargetActions(TargetActionsDialog),
    Web(WebDialog),
    Rename(RenameEditor),
    EditContainer(ContainerEditor),
    Importing(ImportProgress),
    ConfirmImportBundle(ImportBundleConfirmation),
    Confirm(ConfirmDialog),
    /// The `F1` key reference, drawn over the mode it opened on top of. It
    /// carries that mode so closing help puts it back untouched.
    Help(HelpOverlay),
    /// The `F2` command palette: every command that applies right now.
    Palette(CommandPalette),
}

/// What a key press means for a focusable button row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ButtonKey {
    Focus(usize),
    Activate(usize),
    Cancel,
    Ignored,
}

pub(crate) fn button_row_key(code: KeyCode, focus: usize, count: usize) -> ButtonKey {
    match code {
        KeyCode::Tab | KeyCode::Right => ButtonKey::Focus(cycle_button_focus(focus, count, false)),
        KeyCode::BackTab | KeyCode::Left => {
            ButtonKey::Focus(cycle_button_focus(focus, count, true))
        }
        KeyCode::Enter => ButtonKey::Activate(focus),
        KeyCode::Esc => ButtonKey::Cancel,
        _ => ButtonKey::Ignored,
    }
}

pub(crate) fn cycle_button_focus(focus: usize, count: usize, reverse: bool) -> usize {
    if count == 0 {
        return 0;
    }
    if reverse {
        focus.min(count - 1).checked_sub(1).unwrap_or(count - 1)
    } else {
        (focus + 1) % count
    }
}

pub(crate) fn cycle_control<T: Copy + PartialEq>(current: T, order: &[T], reverse: bool) -> T {
    let index = order
        .iter()
        .position(|candidate| *candidate == current)
        .unwrap_or(0);
    let next = if reverse {
        index.checked_sub(1).unwrap_or(order.len() - 1)
    } else {
        (index + 1) % order.len()
    };
    order[next]
}

/// Stateful, renderable projection of controller configuration and state.
pub struct DashboardState {
    pub(crate) config: HelConfig,
    pub(crate) state: HelState,
    pub(crate) quotas: BTreeMap<String, ProfileQuota>,
    pub(crate) quota_refreshing: BTreeSet<String>,
    pub(crate) session_details: BTreeMap<String, SessionDetail>,
    /// Sessions whose relay worker the controller currently cannot reach. Their
    /// summary band renders red so an unreachable target is obvious at a glance.
    pub(crate) unreachable_sessions: BTreeSet<String>,
    /// Sessions with a second opinion in progress. Stopping one destroys its
    /// target, and the reviewer's conversation goes with it, so the stop
    /// confirmation says so first.
    pub(crate) sessions_with_review: BTreeSet<String>,
    pub(crate) project_sources: BTreeMap<String, ProjectSourceIdentity>,
    pub(crate) checkpoint_archive_sizes: BTreeMap<String, Option<u64>>,
    pub(crate) session_operations: BTreeMap<String, SessionOperationDisplay>,
    pub(crate) capacity_details: BTreeMap<String, CapacityDetail>,
    /// Selection anchor for the Sessions pane, by id rather than position: the
    /// pane shows different row sets at different explicit sizes, so a
    /// position could silently point at a different session after resizing.
    pub(crate) selected_session_id: Option<String>,
    /// Persisted scroll offsets for the three list panes, so each scrolls only
    /// far enough to keep its selection visible instead of jumping back to the
    /// top every frame. Written by the renderer after it lets the table settle.
    pub(crate) sessions_scroll: Cell<usize>,
    pub(crate) targets_scroll: Cell<usize>,
    pub(crate) quota_scroll: Cell<usize>,
    /// The next render keeps one row beyond the selection visible in the
    /// direction of the latest keyboard or wheel navigation, when it fits.
    /// This is consumed by that pane's renderer after the movement.
    pub(crate) scroll_lookahead: Cell<Option<(Focus, SelectionDirection)>>,
    pub(crate) capacity_index: usize,
    pub(crate) quota_index: usize,
    pub(crate) focus: Focus,
    /// The independently selected sizes of Sessions, Targets, and Quota.
    pane_sizes: PaneSizes,
    /// The session whose conversation is on screen, or is being opened. It
    /// decides which project the compact Sessions list belongs to.
    pub(crate) current_session_id: Option<String>,
    /// The session an attach is running for, while it is still in flight.
    /// The conversation band draws as empty for as long as this is set to a
    /// session other than the one on screen, so the transcript never belongs
    /// to a different row than the highlight.
    opening_session: Option<String>,
    pub(crate) pane_areas: Option<[Rect; DASHBOARD_PANE_COUNT]>,
    /// Where the conversation's transcript and composer sat on the last
    /// frame, so the controller can route a mouse event by what the pointer
    /// is over rather than by what has focus.
    pub(crate) chat_transcript_area: Option<Rect>,
    pub(crate) chat_prompt_area: Option<Rect>,
    pub(crate) resume_sessions_area: Option<Rect>,
    /// Selectable surfaces, rebuilt by every frame in render order so the
    /// selection engine can hit-test the screen the user is looking at.
    pub(crate) frame_surfaces: FrameSurfaces,
    /// Native sessions the resume dialog hides, loaded from Hel's database.
    pub(crate) hidden_native_sessions: BTreeSet<(HarnessKind, String)>,
    /// The rows the open resume dialog shows, derived from the records, the
    /// scans, and the dialog's own search. Rebuilt where those change and once
    /// a second for the activity labels; empty when no dialog is open.
    pub(crate) resume_rows: Vec<crate::resume::ResumeRow>,
    /// Row hitboxes for the Active pane, keyed by the row's index into the
    /// active session list. Each rect spans the summary line and every
    /// visible preview line beneath it, so a click anywhere on the row
    /// selects it.
    pub(crate) session_row_areas: Vec<(usize, Rect)>,
    pub(crate) project_heading_areas: Vec<(String, Rect)>,
    /// Click targets for the three size controls in each support-pane title.
    pub(crate) pane_size_control_areas: Vec<(SupportPane, PaneSize, Rect)>,
    /// Projects the user has collapsed in the focused Sessions pane. Absent
    /// means expanded, so a project that appears later starts expanded without
    /// any extra bookkeeping.
    pub(crate) collapsed_project_keys: BTreeSet<String>,
    /// The pane, row index, and time of the most recent left click on a
    /// session row, so the next click can be recognized as a double click.
    last_row_click: Option<(Focus, usize, Instant)>,
    pub(crate) mode: Mode,
    pub(crate) notices: Notices,
    /// The workspace name, shown at the right of the Sessions title bar.
    pub(crate) workspace_name: String,
}

impl DashboardState {
    pub fn new(config: HelConfig, state: HelState, quotas: BTreeMap<String, ProfileQuota>) -> Self {
        let mut dashboard = Self {
            config,
            state,
            quotas,
            quota_refreshing: BTreeSet::new(),
            session_details: BTreeMap::new(),
            unreachable_sessions: BTreeSet::new(),
            sessions_with_review: BTreeSet::new(),
            project_sources: BTreeMap::new(),
            checkpoint_archive_sizes: BTreeMap::new(),
            session_operations: BTreeMap::new(),
            capacity_details: BTreeMap::new(),
            selected_session_id: None,
            sessions_scroll: Cell::new(0),
            targets_scroll: Cell::new(0),
            quota_scroll: Cell::new(0),
            scroll_lookahead: Cell::new(None),
            capacity_index: 0,
            quota_index: 0,
            focus: Focus::Sessions,
            pane_sizes: PaneSizes::default(),
            current_session_id: None,
            opening_session: None,
            pane_areas: None,
            chat_transcript_area: None,
            chat_prompt_area: None,
            resume_sessions_area: None,
            frame_surfaces: FrameSurfaces::new(),
            hidden_native_sessions: BTreeSet::new(),
            resume_rows: Vec::new(),
            session_row_areas: Vec::new(),
            project_heading_areas: Vec::new(),
            pane_size_control_areas: Vec::new(),
            collapsed_project_keys: BTreeSet::new(),
            last_row_click: None,
            mode: Mode::Dashboard,
            notices: Notices::default(),
            workspace_name: String::new(),
        };
        dashboard.session_details = dashboard
            .state
            .sessions
            .keys()
            .map(|id| (id.clone(), SessionDetail::default()))
            .collect();
        dashboard.clamp_selections();
        dashboard
    }

    /// Moves the Sessions selection onto `session_id` without changing focus.
    pub fn select_active_session(&mut self, session_id: &str) {
        if self
            .state
            .sessions
            .get(session_id)
            .is_some_and(|session| session.state.is_active())
        {
            self.selected_session_id = Some(session_id.to_owned());
        }
    }

    /// The part of the combined surface that owns the keyboard.
    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn prompt_has_focus(&self) -> bool {
        self.focus == Focus::Prompt
    }

    pub fn focus_prompt(&mut self) {
        self.focus = Focus::Prompt;
    }

    /// Focuses the Sessions pane without changing its explicit size.
    pub fn focus_sessions(&mut self) {
        self.focus = Focus::Sessions;
        self.clamp_selections();
    }

    /// Moves focus one stop along the Tab ring.
    ///
    /// Pane sizes are the user's setting, so Tab never changes them.
    pub fn cycle_focus(&mut self, reverse: bool) {
        self.focus = cycle_control(self.focus, &FOCUS_ORDER, reverse);
        self.clamp_selections();
    }

    #[must_use]
    pub fn pane_size(&self, pane: SupportPane) -> PaneSize {
        self.pane_sizes.get(pane)
    }

    /// Selects one pane size. A maximum is exclusive; the previous maximum
    /// becomes Standard while every other explicit choice stays untouched.
    pub fn set_pane_size(&mut self, pane: SupportPane, size: PaneSize) {
        if size == PaneSize::Maximized {
            for other in [
                SupportPane::Sessions,
                SupportPane::Targets,
                SupportPane::Quota,
            ] {
                if other != pane && self.pane_sizes.get(other) == PaneSize::Maximized {
                    *self.pane_sizes.get_mut(other) = PaneSize::Standard;
                }
            }
        }
        *self.pane_sizes.get_mut(pane) = size;
        self.clamp_selections();
    }

    /// Cycles the focused support pane without moving the keyboard. Prompt is
    /// not resizable, so it explains how to choose a pane instead.
    pub fn cycle_focused_pane_size(&mut self) {
        let Some(pane) = self.focus.support_pane() else {
            self.set_notice("Select Sessions, Targets, or Quota before pressing Alt-Z.");
            return;
        };
        self.set_pane_size(pane, self.pane_size(pane).cycled());
    }

    /// Alt-G's stable global preset: restore any custom arrangement to all
    /// Standard; from all Standard, spend the screen on Sessions.
    pub fn toggle_pane_preset(&mut self) {
        if self.pane_sizes.all_standard() {
            self.pane_sizes = PaneSizes {
                sessions: PaneSize::Maximized,
                targets: PaneSize::Minimized,
                quota: PaneSize::Minimized,
            };
        } else {
            self.pane_sizes = PaneSizes::default();
        }
        self.clamp_selections();
    }

    #[must_use]
    pub fn sessions_minimized(&self) -> bool {
        self.pane_size(SupportPane::Sessions) == PaneSize::Minimized
    }

    /// Number of pending agent questions across the sessions shown by the
    /// navigator. The minimized navigator uses this as its one compact
    /// aggregate while expanded rows identify the individual sessions.
    pub(crate) fn pending_input_count(&self) -> usize {
        self.session_details
            .values()
            .map(|detail| detail.pending_elicitations.len())
            .sum()
    }

    /// The pending questions from the latest accepted full projection. A
    /// startup summary intentionally returns `None`, because it does not
    /// carry the complete request list and must not invalidate a local draft.
    pub fn pending_elicitations(
        &self,
        session_id: &str,
    ) -> Option<(u64, &[hel::hel_elicitation::ElicitationRequest])> {
        let detail = self.session_details.get(session_id)?;
        detail
            .pending_elicitations_applied_event_ordinal
            .map(|ordinal| (ordinal, detail.pending_elicitations.as_slice()))
    }

    fn focused_rows_visible(&self) -> bool {
        self.focus.support_pane().is_none_or(|pane| {
            pane == SupportPane::Sessions || self.pane_size(pane) != PaneSize::Minimized
        })
    }

    /// Whether a modal dialog or wizard owns the keyboard.
    pub fn modal_open(&self) -> bool {
        !matches!(self.mode, Mode::Dashboard)
    }

    /// Records the conversation on screen, which decides which project the
    /// compact Sessions list belongs to.
    pub fn set_current_session(&mut self, session_id: Option<&str>) {
        self.current_session_id = session_id.map(str::to_owned);
        self.clamp_selections();
    }

    /// When this session's materialized projection last changed, in
    /// milliseconds since the epoch. `None` while nothing has been projected
    /// for it yet.
    pub fn session_activity_at_ms(&self, session_id: &str) -> Option<u64> {
        self.session_details
            .get(session_id)
            .and_then(|detail| detail.last_activity_at_ms)
    }

    pub fn current_session_id(&self) -> Option<&str> {
        self.current_session_id.as_deref()
    }

    /// Records the session an attach is running for, or clears it when the
    /// attach settles.
    pub fn set_opening_session(&mut self, session_id: Option<&str>) {
        self.opening_session = session_id.map(str::to_owned);
    }

    /// The session an attach is still running for, if any.
    pub fn opening_session(&self) -> Option<&str> {
        self.opening_session.as_deref()
    }

    /// The session the Sessions pane has selected. The conversation on screen
    /// follows this, so moving the selection moves the transcript.
    pub fn selected_session_id(&self) -> Option<&str> {
        self.selected_session_id.as_deref()
    }

    /// Whether the pointer is over the conversation the surface is drawing.
    /// A click there belongs to the chat, whatever has focus.
    pub fn chat_region_contains(&self, column: u16, row: u16) -> bool {
        [self.chat_transcript_area, self.chat_prompt_area]
            .into_iter()
            .flatten()
            .any(|area| rect_contains(area, column, row))
    }

    /// Opens the web-access dialog and asks the controller to load it.
    pub fn open_web_dialog(&mut self) -> DashboardAction {
        self.mode = Mode::Web(WebDialog::loading());
        DashboardAction::LoadWebAccess
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DashboardAction {
        self.handle_key_at(key, Instant::now())
    }

    /// Handles one key with an explicit reading of the clock. `now` decides
    /// whether the notice on screen has been readable long enough for this
    /// key press to dismiss it.
    pub fn handle_key_at(&mut self, key: KeyEvent, now: Instant) -> DashboardAction {
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return DashboardAction::None;
        }
        if is_paste_shortcut(key) {
            return DashboardAction::PasteFromClipboard;
        }
        let text_focused = self.text_input_focused();
        if text_focused
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('c')
        {
            self.cancel_modal();
            return DashboardAction::None;
        }
        // Ctrl-C belongs to the prompt or a text field. Everywhere else it is
        // intentionally inert, including modal controls that happen to use
        // the letter `c` for another purpose.
        if dashboard_accelerator(key.modifiers) && key.code == KeyCode::Char('c') {
            return DashboardAction::None;
        }

        // Retire the notice this key press is stepping past, but only once it
        // has been on screen long enough to read: for a background failure
        // this bar is the only report there is.
        self.notices.dismiss(now);
        // For one release, the two chords that moved off Control say where
        // they went instead of doing nothing. Remove this arm in the release
        // after the one that introduces Alt-G and Alt-Q.
        if dashboard_accelerator(key.modifiers)
            && let KeyCode::Char(moved @ ('g' | 'q')) = key.code
        {
            self.set_notice(if moved == 'g' {
                "Ctrl-G moved to Alt-G"
            } else {
                "Ctrl-Q moved to Alt-Q"
            });
            return DashboardAction::None;
        }
        // The resume dialog carries every scanned native session, so it is
        // handled where it lives rather than through a copy of the mode.
        if matches!(self.mode, Mode::ResumeDialog(_)) {
            return self.handle_resume_dialog_key(key);
        }
        // The palette is edited where it lives too: its entry list is rebuilt
        // on every keystroke and there is no reason to copy it first.
        if matches!(self.mode, Mode::Palette(_)) {
            return self.handle_palette_key(key);
        }
        match self.mode.clone() {
            Mode::Dashboard => self.handle_dashboard_key(key),
            Mode::New(wizard) => self.handle_new_key(key, wizard),
            Mode::Resume(wizard) => self.handle_resume_key(key, wizard),
            Mode::ResumeDialog(_) => unreachable!("the resume dialog is handled in place"),
            Mode::RepositoryOrigin(dialog) => self.handle_repository_origin_key(key, dialog),
            Mode::ConfigId(editor) => self.handle_config_id_key(key, editor),
            Mode::TargetActions(dialog) => self.handle_target_actions_key(key, dialog),
            Mode::Web(_) => match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.cancel_modal();
                    DashboardAction::None
                }
                _ => DashboardAction::None,
            },
            Mode::Rename(editor) => self.handle_rename_key(key, editor),
            Mode::EditContainer(editor) => self.handle_container_edit_key(key, editor),
            // The only control is the Cancel button, so Enter presses it too.
            Mode::Importing(_) => match key.code {
                KeyCode::Esc | KeyCode::Enter => DashboardAction::CancelImport,
                _ => DashboardAction::None,
            },
            Mode::ConfirmImportBundle(confirmation) => {
                self.handle_import_bundle_key(key.code, confirmation)
            }
            Mode::Confirm(dialog) => self.handle_confirmation_key(key, dialog),
            Mode::Help(overlay) => self.handle_help_key(key, overlay),
            Mode::Palette(_) => unreachable!("the command palette is handled in place"),
        }
    }

    fn text_input_focused(&self) -> bool {
        match &self.mode {
            Mode::Rename(editor) => editor.focus == RenameFocus::Field,
            Mode::RepositoryOrigin(dialog) => dialog.focus == dialogs::RepositoryOriginFocus::Field,
            Mode::EditContainer(editor) => editor.field().is_some(),
            Mode::ResumeDialog(dialog) => dialog.focus == crate::resume::ResumeFocus::Search,
            // The palette's query is a text field, so Ctrl-C closes it and a
            // paste lands in the query rather than on the dashboard.
            Mode::Palette(_) => true,
            Mode::New(wizard) => wizard.text_input_focused(),
            Mode::Resume(wizard) => wizard.text_input_focused(),
            Mode::Confirm(ConfirmDialog {
                confirmation: Confirmation::ForceStop { .. } | Confirmation::ForceDestroy { .. },
                ..
            }) => true,
            _ => false,
        }
    }

    pub fn handle_paste(&mut self, pasted: &str) {
        let pasted = single_line_paste(pasted);
        if pasted.is_empty() {
            return;
        }
        // The palette rebuilds its list from the query, so its paste is
        // handled before the borrow the match below takes.
        if let Mode::Palette(palette) = &mut self.mode {
            palette.query.push_str(&pasted);
            self.rebuild_palette_entries();
            return;
        }
        match &mut self.mode {
            Mode::Rename(editor) if editor.focus == RenameFocus::Field => {
                let remaining = 64_usize.saturating_sub(editor.title.chars().count());
                editor.title.extend(pasted.chars().take(remaining));
            }
            Mode::New(wizard) => match wizard.step {
                WizardStep::ProjectDirectory => {
                    wizard.project_directory.push_str(&pasted);
                    wizard.project_directory_error = None;
                }
                WizardStep::NewBundle => wizard.new_bundle_source.push_str(&pasted),
                WizardStep::Mounts => match wizard.mounts.focus {
                    MountFocus::Source => wizard.mounts.source.push_str(&pasted),
                    MountFocus::Destination => wizard.mounts.destination.push_str(&pasted),
                    _ => {}
                },
                _ => {}
            },
            Mode::Resume(wizard) if wizard.step == WizardStep::Mounts => {
                match wizard.mounts.focus {
                    MountFocus::Source => wizard.mounts.source.push_str(&pasted),
                    MountFocus::Destination => wizard.mounts.destination.push_str(&pasted),
                    _ => {}
                }
            }
            Mode::Confirm(ConfirmDialog {
                confirmation: Confirmation::ForceStop { typed, .. },
                ..
            }) => {
                let remaining = FORCE_STOP_CONFIRMATION.len().saturating_sub(typed.len());
                typed.extend(
                    pasted
                        .chars()
                        .filter(char::is_ascii_alphabetic)
                        .take(remaining)
                        .map(|character| character.to_ascii_uppercase()),
                );
            }
            Mode::Confirm(ConfirmDialog {
                confirmation:
                    Confirmation::ForceDestroy {
                        expected, typed, ..
                    },
                ..
            }) => {
                let remaining = expected
                    .chars()
                    .count()
                    .saturating_sub(typed.chars().count());
                typed.extend(
                    pasted
                        .chars()
                        .filter(char::is_ascii_hexdigit)
                        .take(remaining)
                        .map(|character| character.to_ascii_lowercase()),
                );
            }
            Mode::RepositoryOrigin(dialog)
                if dialog.focus == dialogs::RepositoryOriginFocus::Field =>
            {
                dialog.replacement.push_str(&pasted);
                dialog.error = None;
            }
            _ => {}
        }
    }

    /// The surfaces the last frame registered, for the selection engine.
    pub fn frame_surfaces(&self) -> &FrameSurfaces {
        &self.frame_surfaces
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> DashboardAction {
        if matches!(self.mode, Mode::ResumeDialog(_)) {
            let Some(area) = self.resume_sessions_area else {
                return DashboardAction::None;
            };
            if !rect_contains(area, mouse.column, mouse.row) {
                return DashboardAction::None;
            }
            // The resume list is a list, so a wheel notch moves by one row.
            let delta = match mouse.kind {
                MouseEventKind::ScrollUp => -1,
                MouseEventKind::ScrollDown => 1,
                _ => return DashboardAction::None,
            };
            let len = self.resume_rows().len();
            let Mode::ResumeDialog(dialog) = &mut self.mode else {
                return DashboardAction::None;
            };
            dialog.focus = crate::resume::ResumeFocus::Sessions;
            let index = offset_index(dialog.row_index, len, delta);
            self.select_resume_row(index);
            return DashboardAction::None;
        }
        if !matches!(self.mode, Mode::Dashboard) {
            return DashboardAction::None;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some(&(pane, size, _)) = self
                .pane_size_control_areas
                .iter()
                .find(|(_, _, area)| rect_contains(*area, mouse.column, mouse.row))
            {
                self.set_pane_size(pane, size);
                return DashboardAction::None;
            }
            if let Some((project_key, _)) = self
                .project_heading_areas
                .iter()
                .find(|(_, area)| rect_contains(*area, mouse.column, mouse.row))
            {
                let project_key = project_key.clone();
                self.focus_sessions();
                self.toggle_project(&project_key);
                return DashboardAction::None;
            }
            if let Some(&(index, _)) = self
                .session_row_areas
                .iter()
                .find(|(_, area)| rect_contains(*area, mouse.column, mouse.row))
            {
                return self.handle_row_click(Focus::Sessions, index);
            }
            // The click missed every row; forget any pending double click so
            // a stray click elsewhere can't pair up with the next row click.
            self.last_row_click = None;
        }
        let hovered = self.pane_areas.and_then(|areas| {
            areas
                .into_iter()
                .position(|area| rect_contains(area, mouse.column, mouse.row))
                .map(|index| match index {
                    0 => Focus::Sessions,
                    1 => Focus::Targets,
                    2 => Focus::Quota,
                    _ => unreachable!("the surface has exactly three support panes"),
                })
        });
        let Some(hovered) = hovered else {
            return DashboardAction::None;
        };
        // Minimized Targets and Quota show no selected row. Their summary can
        // take focus so Alt-Z can restore it, but hidden rows do not move or
        // activate underneath the user.
        let rows_visible = hovered == Focus::Sessions
            || hovered
                .support_pane()
                .is_some_and(|pane| self.pane_size(pane) != PaneSize::Minimized);
        match mouse.kind {
            MouseEventKind::ScrollUp if rows_visible => self.scroll_selection_for(hovered, -1),
            MouseEventKind::ScrollDown if rows_visible => self.scroll_selection_for(hovered, 1),
            MouseEventKind::Down(MouseButton::Left) => {
                self.focus = hovered;
                self.clamp_selections();
            }
            _ => {}
        }
        DashboardAction::None
    }

    /// Selects the clicked row and, if it's the second click on the same row
    /// within `DOUBLE_CLICK_INTERVAL`, performs the same action Enter would.
    fn handle_row_click(&mut self, focus: Focus, index: usize) -> DashboardAction {
        // Clicking a row selects it wherever the dial has left the pane; the
        // grid draws focus and its selection, so there is nothing to open.
        self.scroll_lookahead.set(None);
        self.focus = focus;
        if focus == Focus::Sessions {
            let clicked = self
                .ordered_sessions()
                .get(index)
                .map(|session| session.id.clone());
            if clicked.is_some() {
                self.selected_session_id = clicked;
            }
        } else {
            self.set_selection_for(focus, index);
        }
        let now = Instant::now();
        let is_double_click = matches!(
            self.last_row_click,
            Some((last_focus, last_index, last_time))
                if last_focus == focus
                    && last_index == index
                    && now.saturating_duration_since(last_time) <= DOUBLE_CLICK_INTERVAL
        );
        if is_double_click {
            self.last_row_click = None;
            self.open_selected_session()
        } else {
            self.last_row_click = Some((focus, index, now));
            DashboardAction::None
        }
    }

    /// Keys for the combined surface's panes.
    ///
    /// The composer is a separate focus and never reaches here, so the pane
    /// actions are plain letters rather than accelerated ones: no key typed at
    /// a pane can be mistaken for text.
    ///
    /// Everything that runs a named command is looked up in the action
    /// registry ([`crate::actions`]) rather than matched here, so the keys, the
    /// footer, and the help overlay are all reading one table. What stays as
    /// hand-written arms is the input that is not a command: list
    /// navigation, and the two keys whose meaning depends on state.
    fn handle_dashboard_key(&mut self, key: KeyEvent) -> DashboardAction {
        let command = dashboard_accelerator(key.modifiers);
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        match (key.code, command) {
            // Shift-Tab is the reverse of the registry's Tab.
            (KeyCode::BackTab, _) => {
                self.cycle_focus(true);
                return DashboardAction::None;
            }
            // Escape belongs to the composer and to modals. On a pane it does
            // nothing: the combined surface is quit with Alt-Q, and a stray
            // Escape must never take the whole screen away.
            (KeyCode::Esc, _) => return DashboardAction::None,
            _ => {}
        }
        // List navigation, shared by visible lists. It comes before the
        // registry so `j`, `k`, Ctrl-N, and Ctrl-P keep moving the selection.
        if self.focused_rows_visible() {
            match (key.code, command) {
                (KeyCode::Up | KeyCode::Char('k'), false) | (KeyCode::Char('p'), true) => {
                    self.move_selection(-1);
                    return DashboardAction::None;
                }
                (KeyCode::Down | KeyCode::Char('j'), false) | (KeyCode::Char('n'), true) => {
                    self.move_selection(1);
                    return DashboardAction::None;
                }
                (KeyCode::Home, _) => {
                    self.set_selection_for(self.focus, 0);
                    return DashboardAction::None;
                }
                (KeyCode::End, _) => {
                    let len = self.focus_len_for(self.focus);
                    self.set_selection_for(self.focus, len.saturating_sub(1));
                    return DashboardAction::None;
                }
                _ => {}
            }
        }
        // Setup and the session editor never both apply: setup only opens
        // while the config is empty, and an empty config has no sessions. The
        // registry cannot resolve this on the key alone, because `e` is also
        // the Sessions, Targets, and Quota panes' key, so the ambiguity is
        // settled here and `Scope::Setup` is left out of `spec_for_key`.
        if plain && key.code == KeyCode::Char('e') && self.config_is_empty() {
            return self.dispatch_command(CommandId::OpenConfig);
        }
        // A digit picks a project by its number, and a registry command
        // carries no argument, so this one stays a hand-written arm.
        if self.focus == Focus::Sessions
            && plain
            && let KeyCode::Char(digit @ '1'..='9') = key.code
        {
            self.toggle_project_number(digit.to_digit(10).unwrap_or(0) as usize);
            return DashboardAction::None;
        }
        match crate::actions::spec_for_key(key, self.focus) {
            Some(id) => self.dispatch_command(id),
            None => DashboardAction::None,
        }
    }

    /// Opens the selected session's conversation and hands the keyboard to its
    /// composer.
    ///
    /// The conversation already follows the selection, so Enter's job is to
    /// take the user to the prompt for the row they are on. A failed session
    /// is the one exception: it asks first, because reading what it did and
    /// putting it back on a fresh target are both reasonable answers to the
    /// same key. That is a prompt rather than the silent diversion into the
    /// resume wizard this used to do - the row is red, and the dialog says
    /// what failed.
    fn open_selected_session(&mut self) -> DashboardAction {
        let Some(session) = self.selected_session() else {
            return DashboardAction::None;
        };
        if let Some(operation) = self.session_operations.get(&session.id) {
            self.notices.set(format!(
                "{} is in progress; press Alt-X to cancel it.",
                operation.kind.label()
            ));
            return DashboardAction::None;
        }
        // A failed session has two reasonable answers - read what it did, or
        // put it back on a fresh target - and recovery replaces the target, so
        // the surface asks rather than guessing.
        if session.state == SessionState::Error {
            let confirmation = Confirmation::RecoverFailed {
                session_id: session.id.clone(),
                error: session.last_error.clone(),
                recoverable: session.checkpoint.is_some(),
            };
            self.mode = Mode::Confirm(ConfirmDialog::new(confirmation));
            return DashboardAction::None;
        }
        let session_id = session.id.clone();
        self.focus = Focus::Prompt;
        DashboardAction::Open { session_id }
    }

    pub(crate) fn selected_session(&self) -> Option<&SessionRecord> {
        let selected = self.selected_session_id.as_deref()?;
        self.ordered_sessions()
            .into_iter()
            .find(|session| session.id == selected)
    }

    /// The live sessions the Sessions pane is showing, as indices into
    /// [`Self::ordered_sessions`].
    pub(crate) fn visible_session_indices(&self) -> Vec<usize> {
        self.sessions_rows()
            .into_iter()
            .filter_map(|row| match row {
                SessionsRow::Session { index, .. } => Some(index),
                _ => None,
            })
            .collect()
    }

    /// The rows the Sessions pane draws at every explicit size: a heading per
    /// project and one row per live session.
    ///
    /// Standard and Maximized use four-line session rows; Minimized packs the
    /// same row set into a compact three-column grid. Focus never changes the
    /// representation.
    pub(crate) fn sessions_rows(&self) -> Vec<SessionsRow> {
        let sessions = self.ordered_sessions();
        self.expanded_sessions_rows(&sessions)
    }

    fn expanded_sessions_rows(&self, sessions: &[&SessionRecord]) -> Vec<SessionsRow> {
        // Two projects can share a short name, in which case both need their
        // full names to stay distinguishable.
        let mut short_names = BTreeMap::<String, BTreeSet<String>>::new();
        for session in sessions {
            let source = self.project_source(session);
            short_names
                .entry(source.short)
                .or_default()
                .insert(source.key);
        }
        let numbered = self.project_keys().len() > 1;
        let mut rows = Vec::new();
        let mut previous = None;
        let mut number = 0;
        for (index, session) in sessions.iter().enumerate() {
            let source = self.project_source(session);
            if previous.as_ref() != Some(&source.key) {
                number += 1;
                let label = if short_names
                    .get(&source.short)
                    .is_some_and(|projects| projects.len() > 1)
                {
                    source.full.clone()
                } else {
                    source.short.clone()
                };
                rows.push(SessionsRow::ProjectHeading {
                    key: source.key.clone(),
                    label,
                    number: (numbered && number <= 9).then_some(number),
                });
                previous = Some(source.key.clone());
            }
            rows.push(SessionsRow::Session {
                index,
                expanded: !self.collapsed_project_keys.contains(&source.key),
            });
        }
        rows
    }

    /// Where the selection sits among the rows on screen, for the table's
    /// highlight. `None` when nothing is selected or the selection is not on
    /// screen.
    pub(crate) fn selected_visible_index(&self) -> Option<usize> {
        let selected = self.selected_session_id.as_deref()?;
        let sessions = self.ordered_sessions();
        self.visible_session_indices()
            .into_iter()
            .position(|index| sessions.get(index).is_some_and(|s| s.id == selected))
    }

    /// The sessions the dashboard lists, in creation order. Only live
    /// sessions appear here; everything else belongs to the resume dialog.
    pub(crate) fn ordered_sessions(&self) -> Vec<&SessionRecord> {
        let active = partition_sessions(self.state.sessions.values()).0;
        let mut groups = BTreeMap::<(String, String, String), Vec<&SessionRecord>>::new();
        for session in active {
            let source = self.project_source(session);
            groups
                .entry((source.short.to_lowercase(), source.full, source.key))
                .or_default()
                .push(session);
        }
        groups.into_values().flatten().collect()
    }

    pub fn project_source(&self, session: &SessionRecord) -> ProjectSourceIdentity {
        self.project_sources
            .get(&session.id)
            .cloned()
            .unwrap_or_else(|| session.project_source(&self.config))
    }

    pub fn has_resolved_project_source(&self, session_id: &str) -> bool {
        self.project_sources.contains_key(session_id)
    }

    pub fn set_project_source(&mut self, session_id: &str, source: ProjectSourceIdentity) {
        if self.state.sessions.contains_key(session_id) {
            self.project_sources.insert(session_id.to_owned(), source);
            self.clamp_selections();
        }
    }

    pub(crate) fn project_keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        for session in self.ordered_sessions() {
            let key = self.project_source(session).key;
            if keys.last() != Some(&key) {
                keys.push(key);
            }
        }
        keys
    }

    /// Whether this session's project draws its full four-row form. Projects
    /// default to expanded; only an explicit collapse takes that away.
    pub fn project_is_expanded(&self, session: &SessionRecord) -> bool {
        !self
            .collapsed_project_keys
            .contains(&self.project_source(session).key)
    }

    /// Collapses an expanded project or expands a collapsed one, leaving
    /// every other project alone.
    fn toggle_project(&mut self, project_key: &str) {
        if !self.collapsed_project_keys.remove(project_key) {
            self.collapsed_project_keys.insert(project_key.to_owned());
        }
    }

    fn toggle_selected_project(&mut self) {
        let key = self
            .selected_session()
            .map(|session| self.project_source(session).key);
        if let Some(key) = key {
            self.toggle_project(&key);
        }
    }

    fn toggle_project_number(&mut self, number: usize) {
        if number == 0 {
            return;
        }
        if let Some(key) = self.project_keys().get(number - 1).cloned() {
            self.toggle_project(&key);
        }
    }

    fn mark_all_read(&mut self) -> DashboardAction {
        let mut receipts = Vec::new();
        for (session_id, detail) in &mut self.session_details {
            if !detail.has_unread() {
                continue;
            }
            let Some(through) = detail.materialized_applied_event_ordinal else {
                continue;
            };
            let Some(session) = self.state.sessions.get_mut(session_id) else {
                continue;
            };
            if through > session.viewed_through_event_ordinal {
                session.viewed_through_event_ordinal = through;
                detail.clear_unread();
                receipts.push((session_id.clone(), through));
            }
        }
        if receipts.is_empty() {
            self.set_notice("No unread sessions.");
            DashboardAction::None
        } else {
            self.set_notice("Marked all sessions read.");
            DashboardAction::MarkAllRead { receipts }
        }
    }

    pub(crate) fn compatible_profiles(&self, session_id: &str) -> Vec<(&String, HarnessKind)> {
        if !self.state.sessions.contains_key(session_id) {
            return Vec::new();
        }
        self.config
            .profiles
            .iter()
            .map(|(id, profile)| (id, profile.kind))
            .collect()
    }

    pub(crate) fn profile_choice(&self, id: &str, harness: HarnessKind) -> String {
        let quota = if self.quota_refreshing.contains(id) {
            "refreshing".to_string()
        } else {
            self.quotas
                .get(id)
                .map(ProfileQuota::compact)
                .unwrap_or_else(|| "refreshing".to_string())
        };
        let danger = match harness.unsandboxed_guardian_warning() {
            Some(warning) => format!("  ⚠ {warning}"),
            None => String::new(),
        };
        format!("{id}  {}  ·  {quota}{danger}", harness.display_name())
    }

    /// The selected session, if its target template creates a container.
    pub(crate) fn selected_container_session(&self) -> Option<&SessionRecord> {
        let session = self.selected_session()?;
        matches!(
            self.config.targets.get(&session.target_template_id)?,
            HelTargetTemplate::LocalPodman { .. }
                | HelTargetTemplate::LocalDocker { .. }
                | HelTargetTemplate::AppleContainer { .. }
                | HelTargetTemplate::SshPodman { .. }
        )
        .then_some(session)
    }

    pub(crate) fn config_is_empty(&self) -> bool {
        self.config.profiles.is_empty() || self.config.targets.is_empty()
    }

    pub(crate) fn cancel_modal(&mut self) {
        self.mode = Mode::Dashboard;
        self.rebuild_resume_rows();
    }

    fn focus_len_for(&self, focus: Focus) -> usize {
        match focus {
            Focus::Sessions => self.visible_session_indices().len(),
            Focus::Targets => self.capacity_details.len(),
            Focus::Quota => self.config.profiles.len(),
            Focus::Prompt => 0,
        }
    }

    /// Moves the focused list's selection to `index`, counted among the rows
    /// currently on screen.
    fn set_selection_for(&mut self, focus: Focus, index: usize) {
        self.scroll_lookahead.set(None);
        match focus {
            Focus::Sessions => {
                let sessions = self.ordered_sessions();
                self.selected_session_id = self
                    .visible_session_indices()
                    .get(index)
                    .and_then(|session| sessions.get(*session))
                    .map(|session| session.id.clone());
            }
            Focus::Targets => self.capacity_index = index,
            Focus::Quota => self.quota_index = index,
            Focus::Prompt => {}
        }
    }

    fn selection_for(&self, focus: Focus) -> usize {
        match focus {
            Focus::Sessions => self.selected_visible_index().unwrap_or(0),
            Focus::Targets => self.capacity_index,
            Focus::Quota => self.quota_index,
            Focus::Prompt => 0,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        self.scroll_selection_for(self.focus, delta);
    }

    fn scroll_selection_for(&mut self, focus: Focus, delta: isize) {
        let len = self.focus_len_for(focus);
        if len == 0 {
            self.set_selection_for(focus, 0);
            return;
        }
        let mut index = self.selection_for(focus).min(len.saturating_sub(1));
        let previous = index;
        move_index(&mut index, len, delta);
        self.set_selection_for(focus, index);
        if index != previous {
            self.scroll_lookahead.set(Some((
                focus,
                if delta < 0 {
                    SelectionDirection::Up
                } else {
                    SelectionDirection::Down
                },
            )));
        }
    }

    pub(crate) fn clamp_selections(&mut self) {
        // The selection is anchored by id, so it survives the list changing
        // under it; it only moves when the session it named stopped being on
        // screen.
        let sessions = self.ordered_sessions();
        let visible = self
            .visible_session_indices()
            .into_iter()
            .filter_map(|index| sessions.get(index).map(|session| session.id.clone()))
            .collect::<Vec<_>>();
        if !self
            .selected_session_id
            .as_ref()
            .is_some_and(|id| visible.contains(id))
        {
            self.selected_session_id = visible.first().cloned();
        }
        let project_keys = self.project_keys();
        self.collapsed_project_keys
            .retain(|key| project_keys.contains(key));
        self.quota_index = self
            .quota_index
            .min(self.config.profiles.len().saturating_sub(1));
        self.capacity_index = self
            .capacity_index
            .min(self.capacity_details.len().saturating_sub(1));
    }
}

/// Split sessions into the ones the dashboard lists and the ones the resume
/// dialog lists. The dashboard shows only live sessions; every other state
/// belongs to the dialog, and nothing appears in both.
pub(crate) fn partition_sessions<'a>(
    sessions: impl IntoIterator<Item = &'a SessionRecord>,
) -> (Vec<&'a SessionRecord>, Vec<&'a SessionRecord>) {
    let mut active = Vec::new();
    let mut terminal = Vec::new();
    for session in sessions {
        if session.state.is_active() {
            active.push(session);
        } else {
            terminal.push(session);
        }
    }
    let sequence = |left: &&SessionRecord, right: &&SessionRecord| left.compare_by_creation(right);
    active.sort_by(sequence);
    terminal.sort_by(sequence);
    (active, terminal)
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

fn offset_index(index: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    if delta.is_negative() {
        index.saturating_sub(delta.unsigned_abs())
    } else {
        index
            .saturating_add(delta as usize)
            .min(len.saturating_sub(1))
    }
}

pub(crate) fn move_index(index: &mut usize, len: usize, delta: isize) {
    if len == 0 {
        *index = 0;
        return;
    }
    if delta.is_negative() {
        *index = index.saturating_sub(delta.unsigned_abs());
    } else {
        *index = index
            .saturating_add(delta as usize)
            .min(len.saturating_sub(1));
    }
}

pub(crate) fn nth_key<T>(map: &BTreeMap<String, T>, index: usize) -> String {
    map.keys()
        .nth(index)
        .cloned()
        .expect("wizard is only opened for non-empty configuration")
}

fn is_paste_shortcut(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('v')
        && key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
}

#[cfg(target_os = "macos")]
fn dashboard_accelerator(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::SUPER)
}

#[cfg(not(target_os = "macos"))]
fn dashboard_accelerator(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL)
}

fn single_line_paste(pasted: &str) -> String {
    pasted.trim_matches(['\r', '\n']).replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use hel::hel_state::{HelState, STATE_VERSION, SessionState};

    use super::*;
    use crate::test_support::*;

    use crate::render::render;

    /// Opens the rename editor the way the surface offers it now: `F2`, type
    /// enough of "rename" to pick it out, Enter. There is no `e` any more.
    fn open_rename_through_the_palette(dashboard: &mut DashboardState) {
        dashboard.focus_sessions();
        dashboard.handle_key(key(KeyCode::F(2)));
        for character in "rename".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(
            matches!(dashboard.mode, Mode::Rename(_)),
            "{:?}",
            dashboard.mode
        );
    }

    /// The composer is a separate focus, so a pane's actions are plain
    /// letters: nothing typed at a pane can be mistaken for prompt text.
    #[test]
    fn plain_keys_drive_the_focused_pane() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);

        assert_eq!(
            dashboard.handle_key(alt_key('s')),
            DashboardAction::OpenResumeDialog
        );
        dashboard.cancel_modal();
        assert_eq!(dashboard.handle_key(alt_key('n')), DashboardAction::None);
        assert!(matches!(dashboard.mode, Mode::New(_)));
        dashboard.cancel_modal();
        // `e` was the session edit dialog's key. The command palette replaced
        // that dialog, so nothing answers `e` on the Sessions pane now.
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('e'))),
            DashboardAction::None
        );
        assert_eq!(dashboard.mode, Mode::Dashboard);
        // A pane key that belongs to a different pane does nothing here.
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('r'))),
            DashboardAction::None
        );

        assert_eq!(
            dashboard.handle_key(key(KeyCode::F(3))),
            DashboardAction::OpenWorkspacePicker
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::F(4))),
            DashboardAction::LoadWebAccess
        );
        dashboard.cancel_modal();
        assert_eq!(
            dashboard.handle_key(ctrl_key('v')),
            DashboardAction::PasteFromClipboard
        );
    }

    #[test]
    fn pane_actions_follow_the_focused_pane() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);

        dashboard.handle_key(key(KeyCode::Tab));
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(dashboard.focus, Focus::Targets);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::TargetActions(_)));
        dashboard.cancel_modal();
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('n'))),
            DashboardAction::None,
            "the plain letter creates nothing anywhere"
        );

        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(dashboard.focus, Focus::Quota);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('e'))),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::ConfigId(_)));
    }

    #[test]
    fn minimized_summary_panes_do_not_operate_on_hidden_rows() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);

        dashboard.focus = Focus::Targets;
        dashboard.set_pane_size(SupportPane::Targets, PaneSize::Minimized);
        dashboard.handle_key(key(KeyCode::Down));
        dashboard.handle_key(key(KeyCode::Enter));
        assert_eq!(dashboard.capacity_index, 0);
        assert_eq!(dashboard.mode, Mode::Dashboard);

        dashboard.focus = Focus::Quota;
        dashboard.set_pane_size(SupportPane::Quota, PaneSize::Minimized);
        dashboard.handle_key(key(KeyCode::Down));
        dashboard.handle_key(key(KeyCode::Char('e')));
        assert_eq!(dashboard.quota_index, 0);
        assert_eq!(dashboard.mode, Mode::Dashboard);
    }

    /// Refreshing moved off the two panes onto one global key, so the letter
    /// the panes used to answer must now do nothing at all.
    #[test]
    fn plain_r_no_longer_refreshes() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);

        for wanted in [Focus::Targets, Focus::Quota] {
            while dashboard.focus != wanted {
                dashboard.cycle_focus(false);
            }
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Char('r'))),
                DashboardAction::None,
                "plain r still acts at {wanted:?}"
            );
            assert_eq!(dashboard.mode, Mode::Dashboard);
        }

        // F5 is the one refresh key, and it answers from every pane.
        assert_eq!(
            dashboard.handle_key(key(KeyCode::F(5))),
            DashboardAction::RefreshAll
        );
    }

    #[test]
    fn tab_walks_the_layout_order_and_back() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        assert_eq!(dashboard.focus, Focus::Sessions);

        // The ring follows the layout down the screen.
        for expected in [Focus::Prompt, Focus::Targets, Focus::Quota, Focus::Sessions] {
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Tab)),
                DashboardAction::None
            );
            assert_eq!(dashboard.focus, expected);
        }
    }

    #[test]
    fn shift_tab_walks_the_reverse_order() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);

        for expected in [Focus::Quota, Focus::Targets, Focus::Prompt, Focus::Sessions] {
            dashboard.handle_key(key(KeyCode::BackTab));
            assert_eq!(dashboard.focus, expected);
        }
    }

    #[test]
    fn alt_g_toggles_standard_and_the_sessions_preset_without_moving_focus() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        dashboard.focus = Focus::Quota;
        assert_eq!(
            dashboard.pane_size(SupportPane::Sessions),
            PaneSize::Standard
        );

        assert_eq!(dashboard.handle_key(alt_key('g')), DashboardAction::None);
        assert_eq!(
            dashboard.pane_size(SupportPane::Sessions),
            PaneSize::Maximized
        );
        assert_eq!(
            dashboard.pane_size(SupportPane::Targets),
            PaneSize::Minimized
        );
        assert_eq!(dashboard.pane_size(SupportPane::Quota), PaneSize::Minimized);
        assert_eq!(dashboard.focus, Focus::Quota);

        assert_eq!(dashboard.handle_key(alt_key('g')), DashboardAction::None);
        for pane in [
            SupportPane::Sessions,
            SupportPane::Targets,
            SupportPane::Quota,
        ] {
            assert_eq!(dashboard.pane_size(pane), PaneSize::Standard);
        }
        assert_eq!(dashboard.focus, Focus::Quota);

        assert_eq!(dashboard.handle_key(ctrl_key('g')), DashboardAction::None);
        assert_eq!(dashboard.notice().as_deref(), Some("Ctrl-G moved to Alt-G"));
    }

    #[test]
    fn alt_z_cycles_the_focused_pane_and_a_new_maximum_demotes_the_old_one() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus = Focus::Sessions;
        dashboard.handle_key(alt_key('z'));
        assert_eq!(
            dashboard.pane_size(SupportPane::Sessions),
            PaneSize::Maximized
        );
        assert_eq!(dashboard.focus, Focus::Sessions);

        dashboard.focus = Focus::Targets;
        dashboard.handle_key(alt_key('z'));
        assert_eq!(
            dashboard.pane_size(SupportPane::Targets),
            PaneSize::Maximized
        );
        assert_eq!(
            dashboard.pane_size(SupportPane::Sessions),
            PaneSize::Standard
        );
        assert_eq!(dashboard.pane_size(SupportPane::Quota), PaneSize::Standard);

        dashboard.handle_key(alt_key('z'));
        assert_eq!(
            dashboard.pane_size(SupportPane::Targets),
            PaneSize::Minimized
        );
        dashboard.handle_key(alt_key('z'));
        assert_eq!(
            dashboard.pane_size(SupportPane::Targets),
            PaneSize::Standard
        );
    }

    #[test]
    fn alt_z_on_prompt_explains_that_prompt_is_not_resizable() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_prompt();
        dashboard.handle_key(alt_key('z'));
        assert_eq!(
            dashboard.notice().as_deref(),
            Some("Select Sessions, Targets, or Quota before pressing Alt-Z.")
        );
        assert_eq!(dashboard.focus, Focus::Prompt);
    }

    /// Plain letters are pane-local only. New session, resume, and mark read
    /// answer from everywhere, so each has one spelling — its chord — and the
    /// letters that used to alias them do nothing at all.
    #[test]
    fn plain_n_a_and_s_no_longer_act() {
        for character in ['n', 'a', 's'] {
            let mut dashboard = dashboard_with_session(running_session());
            dashboard.focus_sessions();

            assert_eq!(
                dashboard.handle_key(key(KeyCode::Char(character))),
                DashboardAction::None,
                "{character}"
            );
            assert_eq!(dashboard.mode, Mode::Dashboard, "{character}");
            assert_eq!(dashboard.notice(), None, "{character}");
        }

        // The chords still do what the letters used to.
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        assert_eq!(dashboard.handle_key(alt_key('n')), DashboardAction::None);
        assert!(matches!(dashboard.mode, Mode::New(_)));
        dashboard.cancel_modal();

        assert_eq!(
            dashboard.handle_key(alt_key('s')),
            DashboardAction::OpenResumeDialog
        );
        assert_eq!(dashboard.handle_key(alt_key('a')), DashboardAction::None);
        assert_eq!(dashboard.notice().as_deref(), Some("No unread sessions."));
    }

    /// Muscle memory for the old quit chord meets a sentence rather than
    /// silence, for one release.
    #[test]
    fn ctrl_q_explains_the_move_instead_of_quitting() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();

        assert_eq!(dashboard.handle_key(ctrl_key('q')), DashboardAction::None);
        assert_eq!(dashboard.notice().as_deref(), Some("Ctrl-Q moved to Alt-Q"));
        assert_eq!(dashboard.mode, Mode::Dashboard);
    }

    #[test]
    fn ctrl_c_is_inert_on_an_empty_dashboard_and_every_pane() {
        for focus in [Focus::Sessions, Focus::Prompt, Focus::Targets, Focus::Quota] {
            let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
            dashboard.focus = focus;

            for _ in 0..2 {
                assert_eq!(
                    dashboard.handle_key(ctrl_key('c')),
                    DashboardAction::None,
                    "Ctrl-C should not quit from {focus:?}"
                );
                assert_eq!(dashboard.mode, Mode::Dashboard, "{focus:?}");
                assert_eq!(dashboard.focus, focus, "{focus:?}");
            }
        }
    }

    #[test]
    fn ctrl_c_cancels_text_modal_but_is_inert_on_non_text_modal_controls() {
        let mut rename = dashboard_with_session(running_session());
        open_rename_through_the_palette(&mut rename);
        assert_eq!(rename.handle_key(ctrl_key('c')), DashboardAction::None);
        assert_eq!(rename.mode, Mode::Dashboard);
        // A second press remains harmless after the text modal has closed.
        assert_eq!(rename.handle_key(ctrl_key('c')), DashboardAction::None);
        assert_eq!(rename.mode, Mode::Dashboard);

        let mut new_session = dashboard_with_session(running_session());
        assert_eq!(new_session.handle_key(alt_key('n')), DashboardAction::None);
        let mode_before_ctrl_c = new_session.mode.clone();
        assert!(matches!(mode_before_ctrl_c, Mode::New(_)));
        assert_eq!(new_session.handle_key(ctrl_key('c')), DashboardAction::None);
        assert_eq!(new_session.mode, mode_before_ctrl_c);
        // The wizard is still open, and repeated Ctrl-C does not activate a
        // control whose label happens to contain the letter `c`.
        assert_eq!(new_session.handle_key(ctrl_key('c')), DashboardAction::None);
        assert!(matches!(new_session.mode, Mode::New(_)));
    }

    #[test]
    fn tab_reaches_every_pane_without_changing_explicit_sizes() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        dashboard.set_pane_size(SupportPane::Targets, PaneSize::Minimized);
        dashboard.set_pane_size(SupportPane::Quota, PaneSize::Minimized);
        let sizes = dashboard.pane_sizes;
        for expected in [Focus::Prompt, Focus::Targets, Focus::Quota, Focus::Sessions] {
            dashboard.handle_key(key(KeyCode::Tab));
            assert_eq!(dashboard.focus, expected);
            assert_eq!(dashboard.pane_sizes, sizes);
        }
        for expected in [Focus::Quota, Focus::Targets, Focus::Prompt, Focus::Sessions] {
            dashboard.handle_key(key(KeyCode::BackTab));
            assert_eq!(dashboard.focus, expected);
            assert_eq!(dashboard.pane_sizes, sizes);
        }
    }

    /// The combined surface is quit with Alt-Q. A stray Escape must never
    /// take the conversation off the screen.
    #[test]
    fn escape_never_quits_the_combined_surface() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);

        for expected in [Focus::Sessions, Focus::Prompt, Focus::Targets, Focus::Quota] {
            assert_eq!(dashboard.focus, expected);
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Esc)),
                DashboardAction::None,
                "{expected:?}"
            );
            dashboard.handle_key(key(KeyCode::Tab));
        }
    }

    #[test]
    fn ctrl_n_and_ctrl_p_move_the_focused_list() {
        let sessions = (0..3)
            .map(|index| {
                let mut session = stopped_session();
                session.id = format!("session-{index}");
                session.state = SessionState::Running;
                (session.id.clone(), session)
            })
            .collect();
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );

        assert_eq!(dashboard.selected_visible_index(), Some(0));
        dashboard.handle_key(ctrl_key('n'));
        dashboard.handle_key(ctrl_key('n'));
        assert_eq!(dashboard.selected_visible_index(), Some(2));
        dashboard.handle_key(ctrl_key('p'));
        assert_eq!(dashboard.selected_visible_index(), Some(1));

        dashboard.handle_key(key(KeyCode::BackTab));
        assert_eq!(dashboard.focus, Focus::Quota);
        dashboard.handle_key(ctrl_key('n'));
        assert_eq!(dashboard.quota_index, 1);
        dashboard.handle_key(ctrl_key('p'));
        assert_eq!(dashboard.quota_index, 0);
    }

    /// Builds `count` live sessions, `per_project` of them in each project,
    /// so the compact list's threshold and grouping can be exercised.
    fn dashboard_with_live_sessions(count: usize, per_project: usize) -> DashboardState {
        let sessions = (0..count)
            .map(|index| {
                let mut session = stopped_session();
                session.id = format!("session-{index}");
                session.state = SessionState::Running;
                session.created_at = format!("2026-08-{:02}T00:00:00Z", index + 1);
                session.project_directory =
                    Some(format!("/projects/p{}", index / per_project.max(1)).into());
                (session.id.clone(), session)
            })
            .collect();
        DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        )
    }

    fn session_row_indices(dashboard: &DashboardState) -> Vec<usize> {
        dashboard
            .sessions_rows()
            .into_iter()
            .filter_map(|row| match row {
                SessionsRow::Session { index, .. } => Some(index),
                _ => None,
            })
            .collect()
    }

    /// Every explicit size lists every session across every project.
    #[test]
    fn every_pane_size_lists_every_session_across_projects() {
        for size in [PaneSize::Minimized, PaneSize::Standard, PaneSize::Maximized] {
            let mut dashboard = dashboard_with_live_sessions(6, 2);
            dashboard.set_pane_size(SupportPane::Sessions, size);
            dashboard.set_current_session(Some("session-0"));

            assert_eq!(
                session_row_indices(&dashboard),
                [0, 1, 2, 3, 4, 5],
                "{size:?}"
            );
            assert_eq!(
                dashboard.visible_session_indices(),
                [0, 1, 2, 3, 4, 5],
                "{size:?}"
            );
            // Three projects, each with a heading.
            let headings = dashboard
                .sessions_rows()
                .into_iter()
                .filter(|row| matches!(row, SessionsRow::ProjectHeading { .. }))
                .count();
            assert_eq!(headings, 3, "{size:?}");
        }
    }

    #[test]
    fn every_project_starts_expanded_and_collapsing_one_leaves_the_others() {
        let mut dashboard = dashboard_with_live_sessions(4, 2);
        dashboard.focus_sessions();
        let expanded = |dashboard: &DashboardState| {
            dashboard
                .sessions_rows()
                .into_iter()
                .filter_map(|row| match row {
                    SessionsRow::Session { expanded, .. } => Some(expanded),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(expanded(&dashboard), [true, true, true, true]);

        dashboard.handle_key(key(KeyCode::Char('2')));
        assert_eq!(expanded(&dashboard), [true, true, false, false]);
        dashboard.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(
            expanded(&dashboard),
            [false, false, false, false],
            "Space collapses the selected session's own project"
        );
    }

    /// A session that disappears must not take the selection somewhere the
    /// user did not put it: the anchor is an id, so it lands on a real row.
    #[test]
    fn the_selection_survives_the_list_changing_under_it() {
        let mut dashboard = dashboard_with_live_sessions(3, 3);
        dashboard.focus_sessions();
        dashboard.select_active_session("session-1");
        assert_eq!(dashboard.selected_session().unwrap().id, "session-1");

        // Focus moving away and back does not move the selection.
        dashboard.handle_key(key(KeyCode::Tab));
        dashboard.handle_key(key(KeyCode::BackTab));
        assert_eq!(dashboard.selected_session().unwrap().id, "session-1");

        let mut state = dashboard.state.clone();
        state.sessions.get_mut("session-1").unwrap().state = SessionState::Stopped;
        dashboard.set_state(state);
        assert_eq!(
            dashboard.selected_session().unwrap().id,
            "session-0",
            "the selection falls back to the first row still on screen"
        );
    }

    /// Each numbered project answers only for itself, so several can be
    /// collapsed at once and the rest stay expanded.
    #[test]
    fn digits_toggle_projects_independently() {
        let sessions = (0..3)
            .map(|index| {
                let mut session = stopped_session();
                session.id = format!("session-{index}");
                session.state = SessionState::Running;
                session.project_directory = Some(format!("/projects/p{index}").into());
                (session.id.clone(), session)
            })
            .collect();
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        let keys = dashboard.project_keys();
        assert_eq!(keys.len(), 3);
        let expanded = |dashboard: &DashboardState| {
            dashboard
                .project_keys()
                .into_iter()
                .map(|key| !dashboard.collapsed_project_keys.contains(&key))
                .collect::<Vec<_>>()
        };
        assert_eq!(expanded(&dashboard), [true, true, true]);

        dashboard.handle_key(key(KeyCode::Char('1')));
        assert_eq!(expanded(&dashboard), [false, true, true]);
        dashboard.handle_key(key(KeyCode::Char('3')));
        assert_eq!(expanded(&dashboard), [false, true, false]);
        dashboard.handle_key(key(KeyCode::Char('1')));
        assert_eq!(expanded(&dashboard), [true, true, false]);
    }

    #[test]
    fn remote_operation_cancel_action_carries_the_operation_kind() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let session_id = session.id.clone();
        let mut dashboard = dashboard_with_session(session);
        dashboard.begin_session_operation(
            session_id.clone(),
            SessionOperationKind::Launching,
            None,
        );

        assert_eq!(
            dashboard.handle_key(alt_key('x')),
            DashboardAction::CancelOperation {
                session_id,
                kind: SessionOperationKind::Launching,
            }
        );
    }

    /// The notice bar is the only report a background failure gets, so a key
    /// press that happens to arrive while one is fresh must not wipe it.
    #[test]
    fn a_fresh_notice_survives_a_key_press_and_clears_once_it_has_been_readable() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);

        dashboard.set_notice("Rename failed: relay unreachable");
        let shown_at = Instant::now();

        assert_eq!(
            dashboard.handle_key_at(key(KeyCode::Down), shown_at),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.notice().as_deref(),
            Some("Rename failed: relay unreachable")
        );

        assert_eq!(
            dashboard.handle_key_at(
                key(KeyCode::Down),
                shown_at + mj_chat::hel_chat::NOTICE_MINIMUM_DISPLAY
            ),
            DashboardAction::None
        );
        assert_eq!(dashboard.notice(), None);
    }

    /// A key press that reports something of its own replaces the notice
    /// whatever its age; the display period only defends against incidental
    /// keys.
    #[test]
    fn a_key_press_with_its_own_notice_replaces_a_fresh_one() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);

        dashboard.set_notice("Rename failed: relay unreachable");
        let shown_at = Instant::now();

        assert_eq!(
            dashboard.handle_key_at(alt_key('a'), shown_at),
            DashboardAction::None
        );
        assert_eq!(dashboard.notice().as_deref(), Some("No unread sessions."));
    }

    #[test]
    fn alt_q_quits_without_mutating_any_dashboard_modal() {
        let mut new_session = DashboardState::new(config(), HelState::default(), BTreeMap::new());
        assert_eq!(new_session.handle_key(alt_key('n')), DashboardAction::None);

        let mut resume = dashboard_with_session(stopped_session());
        assert_eq!(open_resume_wizard(&mut resume), DashboardAction::None);

        let mut running = stopped_session();
        running.state = SessionState::Running;
        running.checkpoint = None;
        let mut rename = dashboard_with_session(running);
        open_rename_through_the_palette(&mut rename);

        let mut importing = dashboard_with_session(stopped_session());
        importing.show_import_progress("Chosen session".into());

        let mut confirm_import = dashboard_with_session(stopped_session());
        confirm_import.show_import_bundle_confirmation(Vec::new(), Vec::new(), Vec::new(), false);

        let mut confirm = dashboard_with_session(stopped_session());
        confirm.show_dirty_local_confirmation(DashboardAction::None, vec!["project".into()]);

        let mut resume_dialog = dashboard_with_session(stopped_session());
        resume_dialog.show_resume_dialog(1, Vec::new());

        for (label, mut dashboard) in [
            ("new session", new_session),
            ("resume", resume),
            ("resume dialog", resume_dialog),
            ("rename", rename),
            ("import progress", importing),
            ("import confirmation", confirm_import),
            ("confirmation", confirm),
        ] {
            assert!(!matches!(dashboard.mode, Mode::Dashboard), "{label}");
            let mode_before_quit = dashboard.mode.clone();

            // Alt-Q is a global chord: the controller answers it before the
            // surface sees the key, so this drives the same path the
            // controller's pre-filter drives.
            let command = crate::global_chord(&alt_key('q')).expect("Alt-Q is a global chord");
            assert!(dashboard.global_chord_allowed(command), "{label}");
            assert_eq!(
                dashboard.dispatch_command(command),
                DashboardAction::QuitDetach,
                "{label}"
            );
            assert_eq!(dashboard.mode, mode_before_quit, "{label}");
        }
    }

    #[test]
    fn the_partition_keeps_terminal_states_off_the_dashboard() {
        let mut running = stopped_session();
        running.id = "session-0".into();
        running.state = SessionState::Running;
        let stopped = stopped_session();
        let mut lost = stopped_session();
        lost.id = "session-2".into();
        lost.state = SessionState::Lost;
        let state = HelState {
            version: STATE_VERSION,
            sessions: BTreeMap::from([
                (running.id.clone(), running),
                (stopped.id.clone(), stopped),
                (lost.id.clone(), lost),
            ]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        };
        let (active, terminal) = partition_sessions(state.sessions.values());
        assert_eq!(
            active
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            ["session-0"]
        );
        assert_eq!(
            terminal
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            ["session-1", "session-2"]
        );

        // Only the live session is on the dashboard; Tab leaves the session
        // pane rather than walking into a second one.
        let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
        dashboard.handle_key(key(KeyCode::Down));
        assert_eq!(
            dashboard
                .selected_session()
                .map(|session| session.id.as_str()),
            Some("session-0")
        );
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(dashboard.focus, Focus::Prompt);
    }

    #[test]
    fn sessions_are_ordered_by_creation_sequence_ascending() {
        let mut oldest = stopped_session();
        oldest.id = "session-z".into();
        oldest.created_at = "2026-08-09T01:00:00Z".into();
        let mut newest = stopped_session();
        newest.id = "session-y".into();
        newest.created_at = "2026-08-09T00:30:00-02:00".into();
        let mut invalid_timestamp = stopped_session();
        invalid_timestamp.id = "session-a".into();
        invalid_timestamp.created_at = "unknown".into();

        let (_, terminal) = partition_sessions([&invalid_timestamp, &oldest, &newest]);

        assert_eq!(
            terminal
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            ["session-z", "session-y", "session-a"]
        );
    }

    #[test]
    fn resolved_git_origin_groups_differently_named_raw_worktrees() {
        let mut first = stopped_session();
        first.id = "bifrost-fird".into();
        first.state = SessionState::Running;
        first.project_directory = Some("/mnt/optane/bifrost-fird".into());
        let mut second = stopped_session();
        second.id = "bifrost-fuzz".into();
        second.state = SessionState::Running;
        second.project_directory = Some("/home/dev/bifrost-fuzz".into());
        let state = HelState {
            version: STATE_VERSION,
            sessions: [first, second]
                .into_iter()
                .map(|session| (session.id.clone(), session))
                .collect(),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        };
        let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
        let source =
            ProjectSourceIdentity::git_remote("git@github.com:BrokkAi/bifrost-dev.git").unwrap();

        dashboard.set_project_source("bifrost-fird", source.clone());
        dashboard.set_project_source("bifrost-fuzz", source);

        assert_eq!(dashboard.project_keys(), ["github:brokkai/bifrost-dev"]);
        assert!(
            dashboard
                .ordered_sessions()
                .iter()
                .all(|session| dashboard.project_is_expanded(session))
        );
    }

    #[test]
    fn mark_all_read_advances_a_materialized_session_and_returns_its_receipt() {
        let mut dashboard = dashboard_with_session(running_session());
        apply_materialized_transcript(&mut dashboard, vec![agent_message(4, "unread response")]);
        assert_eq!(
            dashboard.session_details["session-1"].unread_agent_messages,
            1
        );

        assert_eq!(
            dashboard.handle_key(alt_key('a')),
            DashboardAction::MarkAllRead {
                receipts: vec![("session-1".into(), 4)]
            }
        );
        assert_eq!(
            dashboard.session_details["session-1"].unread_agent_messages,
            0
        );
        assert_eq!(
            dashboard.state.sessions["session-1"].viewed_through_event_ordinal,
            4
        );
    }

    #[test]
    fn mark_all_read_includes_a_restart_only_session() {
        let mut dashboard = dashboard_with_session(running_session());
        apply_materialized_transcript(&mut dashboard, vec![session_restart(3)]);
        assert_eq!(
            dashboard.session_details["session-1"].unread_session_restarts,
            1
        );

        assert_eq!(
            dashboard.handle_key(alt_key('a')),
            DashboardAction::MarkAllRead {
                receipts: vec![("session-1".into(), 3)]
            }
        );
        let detail = &dashboard.session_details["session-1"];
        assert_eq!(detail.unread_session_restarts, 0);
        assert!(!detail.has_unread());
    }

    #[test]
    fn bracketed_paste_populates_dashboard_text_editors() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);

        open_rename_through_the_palette(&mut dashboard);
        let Mode::Rename(editor) = &mut dashboard.mode else {
            panic!("expected rename editor")
        };
        editor.title.clear();
        dashboard.handle_paste("pasted title\n");

        let Mode::Rename(editor) = &dashboard.mode else {
            panic!("expected rename editor")
        };
        assert_eq!(editor.title, "pasted title");
    }

    #[test]
    fn the_tab_ring_visits_every_pane_and_keeps_the_session_selection() {
        let mut active = stopped_session();
        active.id = "session-0".into();
        active.state = SessionState::Running;
        let archived = stopped_session();
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions: BTreeMap::from([
                    (active.id.clone(), active),
                    (archived.id.clone(), archived),
                ]),
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);

        assert_eq!(dashboard.focus, Focus::Sessions);
        assert_eq!(dashboard.selected_session().unwrap().id, "session-0");
        dashboard.handle_key(key(KeyCode::Down));
        assert_eq!(dashboard.selected_session().unwrap().id, "session-0");

        // The selection is anchored by session id, so it survives focus
        // moving away and Tab lands the user back where they were.
        for expected in [Focus::Prompt, Focus::Targets, Focus::Quota, Focus::Sessions] {
            dashboard.handle_key(key(KeyCode::Tab));
            assert_eq!(dashboard.focus, expected);
            assert_eq!(dashboard.selected_session().unwrap().id, "session-0");
        }

        for expected in [Focus::Quota, Focus::Targets, Focus::Prompt, Focus::Sessions] {
            dashboard.handle_key(key(KeyCode::BackTab));
            assert_eq!(dashboard.focus, expected);
        }
    }

    #[test]
    fn keyboard_selection_stops_at_the_active_panes_ends_instead_of_wrapping() {
        let sessions = (0..3)
            .map(|index| {
                let mut session = stopped_session();
                session.id = format!("session-{index}");
                session.state = SessionState::Running;
                (session.id.clone(), session)
            })
            .collect();
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );

        assert_eq!(dashboard.selected_visible_index(), Some(0));
        dashboard.handle_key(key(KeyCode::Up));
        assert_eq!(
            dashboard.selected_visible_index(),
            Some(0),
            "Up at the first row stays put"
        );

        dashboard.handle_key(key(KeyCode::Down));
        dashboard.handle_key(key(KeyCode::Down));
        assert_eq!(dashboard.selected_visible_index(), Some(2));
        dashboard.handle_key(key(KeyCode::Down));
        assert_eq!(
            dashboard.selected_visible_index(),
            Some(2),
            "Down at the last row stays put"
        );
    }

    /// Every project starts expanded, and a collapsed one is still a list of
    /// sessions: Enter opens the row under the caret rather than spending the
    /// key on the group.
    #[test]
    fn enter_opens_the_selected_session_even_inside_a_collapsed_project() {
        let sessions = (0..3)
            .map(|index| {
                let mut session = stopped_session();
                session.id = format!("session-{index}");
                session.state = SessionState::Running;
                session.project_directory = Some(if index < 2 {
                    "/projects/shared".into()
                } else {
                    "/projects/other".into()
                });
                session.created_at = format!("2026-08-1{}T00:00:00Z", index + 1);
                (session.id.clone(), session)
            })
            .collect();
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        dashboard.select_active_session("session-1");
        assert!(
            dashboard.project_is_expanded(dashboard.selected_session().unwrap()),
            "projects default to expanded"
        );

        // Collapsing the selected session's project leaves the selection, and
        // Enter still opens it.
        dashboard.handle_key(key(KeyCode::Char(' ')));
        assert!(!dashboard.project_is_expanded(dashboard.selected_session().unwrap()));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::Open {
                session_id: "session-1".into()
            }
        );
        assert_eq!(dashboard.selected_session().unwrap().id, "session-1");
    }

    #[test]
    fn mouse_wheel_scrolls_the_hovered_pane_without_changing_focus() {
        let sessions = (0..5)
            .map(|index| {
                let mut session = stopped_session();
                session.id = format!("session-{index}");
                session.state = SessionState::Running;
                (session.id.clone(), session)
            })
            .collect();
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw pane hitboxes");
        let pane_areas = dashboard.pane_areas.expect("dashboard pane hitboxes");

        // The sessions pane moves one row per wheel notch, like a single arrow.
        dashboard.handle_mouse(mouse_in(MouseEventKind::ScrollDown, pane_areas[0]));
        assert_eq!(dashboard.selected_visible_index().unwrap_or(0), 1);
        dashboard.handle_mouse(mouse_in(MouseEventKind::ScrollUp, pane_areas[0]));
        assert_eq!(dashboard.selected_visible_index().unwrap_or(0), 0);

        // The quota pane is also a list: one notch moves its selection by one.
        assert_eq!(dashboard.focus, Focus::Sessions);
        dashboard.handle_mouse(mouse_in(MouseEventKind::ScrollDown, pane_areas[2]));
        assert_eq!(dashboard.quota_index, 1);
        assert_eq!(dashboard.selected_visible_index().unwrap_or(0), 0);
        assert_eq!(dashboard.focus, Focus::Sessions);
    }

    #[test]
    fn clicking_an_active_rows_tail_line_selects_that_session() {
        let mut dashboard = dashboard_with_conversations(3);
        dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Maximized);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw active row hitboxes");
        assert_eq!(
            dashboard.selected_visible_index().unwrap_or(0),
            0,
            "starts on the first session"
        );

        let (_, row) = *dashboard
            .session_row_areas
            .iter()
            .find(|(index, _)| *index == 2)
            .expect("the third active row has a recorded hitbox");
        assert!(
            row.height > 1,
            "an unselected row still spans its preview lines"
        );
        // Click the row's bottom line, i.e. its conversation tail, not the
        // one-line summary at the top.
        dashboard.handle_mouse(mouse_at_row(
            MouseEventKind::Down(MouseButton::Left),
            row,
            row.height - 1,
        ));

        assert_eq!(
            dashboard.selected_visible_index().unwrap_or(0),
            2,
            "clicking the tail line selected the row, not just its summary line"
        );
        assert_eq!(dashboard.focus, Focus::Sessions);
    }

    #[test]
    fn a_single_click_on_a_row_selects_but_reports_no_action() {
        let mut dashboard = dashboard_with_conversations(3);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw active row hitboxes");

        let (_, row) = *dashboard
            .session_row_areas
            .iter()
            .find(|(index, _)| *index == 1)
            .expect("the second active row has a recorded hitbox");
        let action = dashboard.handle_mouse(mouse_at_row(
            MouseEventKind::Down(MouseButton::Left),
            row,
            0,
        ));

        assert_eq!(action, DashboardAction::None);
        assert_eq!(dashboard.selected_visible_index().unwrap_or(0), 1);
    }

    #[test]
    fn a_double_click_on_an_active_row_opens_it_like_enter() {
        let mut dashboard = dashboard_with_conversations(3);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw active row hitboxes");

        let (_, row) = *dashboard
            .session_row_areas
            .iter()
            .find(|(index, _)| *index == 1)
            .expect("the second active row has a recorded hitbox");
        let click = || mouse_at_row(MouseEventKind::Down(MouseButton::Left), row, 0);

        let first = dashboard.handle_mouse(click());
        assert_eq!(first, DashboardAction::None, "the first click just selects");

        let second = dashboard.handle_mouse(click());
        assert_eq!(
            second,
            DashboardAction::Open {
                session_id: "session-1".into(),
            },
            "a quick second click on the same row opens it, matching Enter"
        );
        assert_eq!(dashboard.selected_visible_index().unwrap_or(0), 1);
    }

    #[test]
    fn clicks_on_different_rows_do_not_count_as_a_double_click() {
        let mut dashboard = dashboard_with_conversations(3);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw active row hitboxes");

        let row_for = |index: usize| {
            *dashboard
                .session_row_areas
                .iter()
                .find(|(row_index, _)| *row_index == index)
                .map(|(_, area)| area)
                .expect("row has a recorded hitbox")
        };
        let first_row = row_for(0);
        let second_row = row_for(1);

        let first = dashboard.handle_mouse(mouse_at_row(
            MouseEventKind::Down(MouseButton::Left),
            first_row,
            0,
        ));
        assert_eq!(first, DashboardAction::None);

        // A click on a different row is a fresh first click, not the second
        // half of a double click on row 0.
        let second = dashboard.handle_mouse(mouse_at_row(
            MouseEventKind::Down(MouseButton::Left),
            second_row,
            0,
        ));
        assert_eq!(second, DashboardAction::None);
        assert_eq!(
            dashboard.selected_visible_index().unwrap_or(0),
            1,
            "the second click's row is selected"
        );
    }

    /// A dashboard with `count` running sessions, each carrying a numbered
    /// conversation long enough to scroll.
    fn dashboard_with_conversations(count: usize) -> DashboardState {
        let sessions = (0..count)
            .map(|index| {
                let mut session = stopped_session();
                session.id = format!("session-{index}");
                session.state = SessionState::Running;
                (session.id.clone(), session)
            })
            .collect();
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        let transcript = numbered_conversation(14);
        for index in 0..count {
            apply_materialized_transcript_for(
                &mut dashboard,
                &format!("session-{index}"),
                transcript.clone(),
            );
        }
        dashboard
    }

    #[test]
    fn newly_ready_session_can_be_selected_after_state_refresh() {
        let mut new_session = stopped_session();
        new_session.id = "new-session".into();
        new_session.state = SessionState::Running;
        let mut other = stopped_session();
        other.id = "other".into();
        other.state = SessionState::Running;
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions: BTreeMap::from([(other.id.clone(), other)]),
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        dashboard.focus = Focus::Quota;

        let mut refreshed = dashboard.state.clone();
        refreshed
            .sessions
            .insert(new_session.id.clone(), new_session);
        dashboard.set_state(refreshed);
        dashboard.select_active_session("new-session");

        // Selecting a session no longer steals the keyboard: the caller
        // decides where focus belongs, so a background arrival cannot pull it
        // out of the composer.
        assert_eq!(dashboard.focus, Focus::Quota);
        assert_eq!(dashboard.selected_session().unwrap().id, "new-session");
    }

    /// Stopping the last session empties the dashboard rather than moving the
    /// row to another pane: it belongs to the resume dialog now.
    #[test]
    fn stopping_the_last_session_empties_the_dashboard_and_panes_still_cycle() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        assert_eq!(dashboard.focus, Focus::Sessions);

        let mut state = dashboard.state.clone();
        state.sessions.get_mut("session-1").unwrap().state = SessionState::Stopped;
        dashboard.set_state(state);
        assert_eq!(dashboard.focus, Focus::Sessions);
        assert!(dashboard.ordered_sessions().is_empty());
        assert!(dashboard.selected_session().is_none());

        for expected in [Focus::Prompt, Focus::Targets, Focus::Quota, Focus::Sessions] {
            dashboard.handle_key(key(KeyCode::Tab));
            assert_eq!(dashboard.focus, expected);
        }
    }

    #[test]
    fn opening_an_active_session_returns_controller_action() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        session.checkpoint = None;
        let mut dashboard = dashboard_with_session(session);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::Open {
                session_id: "session-1".into()
            }
        );
    }

    /// A failed session has two reasonable answers to Enter, and recovery
    /// replaces the target, so the surface asks instead of guessing. The row
    /// is red before the key is ever pressed, so the dialog is not a surprise.
    #[test]
    fn enter_on_a_failed_session_offers_recovery_and_the_transcript() {
        let mut session = stopped_session();
        session.state = SessionState::Error;
        session.last_error = Some("worker bootstrap failed: upload failed".into());
        let mut dashboard = dashboard_with_session(session);

        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        let Mode::Confirm(dialog) = &dashboard.mode else {
            panic!(
                "expected the failed-session prompt, got {:?}",
                dashboard.mode
            )
        };
        assert_eq!(
            dialog.confirmation,
            Confirmation::RecoverFailed {
                session_id: "session-1".into(),
                error: Some("worker bootstrap failed: upload failed".into()),
                recoverable: true,
            }
        );

        // The prompt draws, names what failed, and offers both answers.
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).expect("terminal");
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .expect("draw the failed-session prompt");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("Session failed"), "{rendered}");
        assert!(rendered.contains("worker bootstrap failed"), "{rendered}");
        assert!(rendered.contains("Open transcript"), "{rendered}");
        assert!(rendered.contains("Recover"), "{rendered}");

        // Reading what it did changes nothing about the session.
        dashboard.handle_key(key(KeyCode::Left));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::Open {
                session_id: "session-1".into()
            }
        );
        assert_eq!(dashboard.focus, Focus::Prompt);
    }

    /// Recovery is only on offer when there is a verified copy to recover
    /// from; without one the prompt says so rather than showing a button that
    /// cannot work.
    #[test]
    fn a_failed_session_without_a_recovery_copy_is_not_offered_recovery() {
        let mut session = stopped_session();
        session.state = SessionState::Error;
        session.checkpoint = None;
        session.last_error = Some("worker bootstrap failed".into());
        let mut dashboard = dashboard_with_session(session);

        dashboard.handle_key(key(KeyCode::Enter));
        let Mode::Confirm(dialog) = &dashboard.mode else {
            panic!("expected the failed-session prompt")
        };
        assert_eq!(
            dialog.confirmation,
            Confirmation::RecoverFailed {
                session_id: "session-1".into(),
                error: Some("worker bootstrap failed".into()),
                recoverable: false,
            }
        );
    }
}
