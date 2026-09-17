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
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use mj_chat::chat::wrap_styled_line;
use mj_chat::components::{ChoiceList, ControlKind, Dialog, Interaction, TabStrip, TextField};
use mj_chat::theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use mj_client::daemon::{
    WikiHitBlock, WikiHitTranscript, WikiIndexState, WikiRow, WikiSearchPage, WikiStatus,
};
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

/// How long the dialog waits before asking again while the first index build
/// is still running.
pub(crate) const WIKI_INDEXING_POLL: Duration = Duration::from_secs(5);
/// How long the dialog waits before repeating the current query while a top-up
/// sync is running. The wait grows and then settles, so a short sync is
/// followed closely and a long one is still followed rather than abandoned.
pub(crate) const WIKI_TOP_UP_BACKOFF: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(10),
];

/// How many rows one wheel notch moves the preview pane, matching the chat
/// transcript's step.
const PREVIEW_SCROLL_ROWS: usize = 3;

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
    /// Where the matching hit sat in the index's answer. A query lists only
    /// rows that have one, in this order, so the index's ranking is what the
    /// dialog shows.
    pub(crate) wiki_rank: Option<usize>,
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
    /// The newest search results, or the most recent sessions before anything
    /// has been typed. Shared rather than copied: a confirmation clones the
    /// whole dialog.
    pub(crate) wiki: Arc<Vec<WikiRow>>,
    /// The search this dialog last asked for. A result that names an older
    /// request is stale and dropped.
    pub(crate) wiki_request_id: u64,
    /// What the last answer said about the index: whether it can be searched
    /// at all, and whether a sync is adding to it right now.
    pub(crate) wiki_status: WikiStatus,
    /// The dialog opens with the search box focused, but the box cannot take
    /// the focus until the index says it is ready. This stays set until that
    /// answer arrives, or until the person presses or clicks something first,
    /// which is a choice about where the focus goes.
    pub(crate) search_focus_pending: bool,
    /// How many times the current query has been re-issued because a sync was
    /// still running. It picks the wait before the next repeat.
    pub(crate) wiki_top_ups: u32,
    /// Whether a search answer is still outstanding, so the list can say it is
    /// searching rather than showing an empty list as a finished answer.
    pub(crate) wiki_pending: bool,
    /// Briefings already fetched, by SessionWiki id, for the dialog's life.
    pub(crate) previews: Arc<BTreeMap<String, String>>,
    /// The briefing being fetched now, so one selection asks only once.
    pub(crate) preview_pending: Option<String>,
    /// Matching passages already fetched, by SessionWiki id and query. A
    /// different query over the same session is a different answer.
    pub(crate) hits: Arc<BTreeMap<PreviewKey, WikiHitTranscript>>,
    /// The passages being fetched now, so one selection asks only once.
    pub(crate) hits_pending: Option<PreviewKey>,
    /// First wrapped row the preview pane shows.
    pub(crate) preview_scroll: usize,
    /// Which hit of the shown transcript the pane is sitting on, so `n` and
    /// `N` move from where the reader is.
    pub(crate) preview_hit: usize,
    /// The SessionWiki session and query the scroll offset belongs to. Either
    /// one changing starts the pane at the top, or at the new query's first
    /// hit.
    pub(crate) preview_key: Option<PreviewKey>,
}

/// What the preview pane is showing: a SessionWiki session, and the query
/// whose passages are shown in it. The query is empty when the pane is
/// showing the briefing instead.
pub(crate) type PreviewKey = (String, String);

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
        form.declare_with_enabled(Search, ControlKind::TextField, self.search_enabled());
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

    /// Whether the search box accepts typing. Search is the index's answer, so
    /// there is nothing to type into until the index can answer.
    pub(crate) fn search_enabled(&self) -> bool {
        self.wiki_status.state == WikiIndexState::Ready
    }

    /// What stands in the search box while it cannot be typed into.
    pub(crate) fn search_placeholder(&self) -> Option<&'static str> {
        match self.wiki_status.state {
            WikiIndexState::Ready => None,
            WikiIndexState::Indexing => Some("Indexing…"),
            WikiIndexState::VersionMismatch => Some("SessionWiki index is at a different version"),
        }
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

    /// The SessionWiki session the preview pane is showing, which is the one
    /// behind the selected row. `None` when the row has none, and the pane is
    /// then not drawn at all.
    pub(crate) fn preview_wiki_id<'rows>(&self, rows: &'rows [ResumeRow]) -> Option<&'rows str> {
        let index = selected_index(self, rows.len())?;
        rows.get(index)?.wiki_id()
    }

    /// Whether the dialog has a preview pane this frame.
    pub(crate) fn has_preview(&self, rows: &[ResumeRow]) -> bool {
        self.preview_wiki_id(rows).is_some()
    }

    /// The query the pane is previewing passages for, or `None` when there is
    /// nothing typed and the pane shows the briefing instead.
    pub(crate) fn active_query(&self) -> Option<String> {
        let query = self.search.to_string();
        (!query.trim().is_empty()).then_some(query)
    }

    /// The preview body as logical lines of spans, before wrapping, together
    /// with the logical line each hit starts on so the pane can be opened on a
    /// hit before the lines are wrapped.
    ///
    /// The body is the query's matching passages while a query is active, and
    /// the cached briefing otherwise, or one line saying it is on its way.
    /// Spans rather than a string because a hit preview highlights the matched
    /// words inside a line.
    pub(crate) fn preview_body(
        &self,
        rows: &[ResumeRow],
    ) -> Option<(Vec<Line<'static>>, Vec<usize>)> {
        let wiki_id = self.preview_wiki_id(rows)?;
        let muted = Style::default().fg(theme::palette().muted);
        let Some(query) = self.active_query() else {
            let Some(brief) = self.previews.get(wiki_id) else {
                return Some((
                    vec![Line::styled("Loading the archived transcript…", muted)],
                    Vec::new(),
                ));
            };
            // The briefing's first line is the SessionWiki crate's own
            // `# Previous session: <title>` heading, which repeats the title
            // of the row the pane sits under.
            return Some((
                brief
                    .lines()
                    .skip(1)
                    .map(|line| Line::raw(line.to_owned()))
                    .collect(),
                Vec::new(),
            ));
        };
        let Some(transcript) = self.hits.get(&(wiki_id.to_owned(), query)) else {
            return Some((
                vec![Line::styled("Loading the matching passages…", muted)],
                Vec::new(),
            ));
        };
        Some(hit_transcript_lines(transcript))
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
            wiki_rank: None,
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
                wiki_rank: None,
            });
        }
    }
    // SessionWiki rows either annotate a row already here or become archived
    // rows of their own. A row is archived only when nothing on this machine
    // still holds the session: no Mjolnir record, and no native file an import
    // could adopt.
    for (rank, hit) in wiki.iter().enumerate() {
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
                row.wiki_rank = Some(rank);
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
            wiki_rank: Some(rank),
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
/// checkpoint sizes appended.
///
/// There is one search path. With an empty query a tab lists everything it
/// owns, newest first. With a query it lists only the rows the index returned,
/// in the order the index ranked them, so what the dialog shows and what
/// SessionWiki found are the same thing.
fn build_resume_rows(
    config: &Config,
    state: &State,
    dialog: &ResumeDialog,
    checkpoint_archive_sizes: &BTreeMap<String, Option<u64>>,
) -> (Vec<ResumeRow>, [usize; 3]) {
    let searching = !dialog.search.is_empty();
    let merged = merged_resume_rows(config, state, &dialog.profiles, &dialog.wiki);
    // Counted before the tab filter: the other tabs' hits are already ranked
    // here, and throwing them away is what hid where a query matched.
    let mut hits = [0usize; 3];
    if searching {
        for row in merged.iter().filter(|row| row.wiki_rank.is_some()) {
            for tab in [ResumeTab::Hel, ResumeTab::Import, ResumeTab::Archive] {
                if tab.includes(row) {
                    hits[tab.index()] += 1;
                }
            }
        }
    }
    let mut rows = merged
        .into_iter()
        .filter(|row| dialog.tab.includes(row))
        .filter(|row| !searching || row.wiki_rank.is_some())
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
        .collect::<Vec<_>>();
    if searching {
        // The index ranked these; the newest-first order the merge applied is
        // not the answer's order and would hide the best hit below the rest.
        rows.sort_by_key(|row| row.wiki_rank.unwrap_or(usize::MAX));
    }
    (rows, hits)
}

impl DashboardState {
    /// Rebuilds the open dialog's rows from what they are derived from: the
    /// Hel records, the scanned native sessions, the checkpoint sizes, and the
    /// newest search answer. Every mutation of those inputs calls this.
    /// Moving the selection only reads the rows.
    pub fn rebuild_resume_rows(&mut self) {
        let Mode::ResumeDialog(dialog) = &self.mode else {
            self.resume_rows.clear();
            self.resume_hit_counts = [0; 3];
            return;
        };
        let (rows, hits) = build_resume_rows(
            &self.config,
            &self.state,
            dialog,
            &self.checkpoint_archive_sizes,
        );
        self.resume_rows = rows;
        self.resume_hit_counts = hits;
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
            // The scan notice lives on the Import tab, so it animates only
            // while that tab is the one on screen.
            Mode::ResumeDialog(dialog) => {
                (dialog.is_scanning() && dialog.tab == ResumeTab::Import) || dialog.wiki_pending
            }
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
            wiki: Arc::new(Vec::new()),
            wiki_request_id: 0,
            wiki_status: WikiStatus::default(),
            search_focus_pending: true,
            wiki_top_ups: 0,
            wiki_pending: false,
            previews: Arc::new(BTreeMap::new()),
            preview_pending: None,
            hits: Arc::new(BTreeMap::new()),
            hits_pending: None,
            preview_scroll: 0,
            preview_hit: 0,
            preview_key: None,
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
    pub fn apply_wiki_search(&mut self, request_id: u64, page: WikiSearchPage) {
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return;
        };
        if dialog.wiki_request_id != request_id {
            return;
        }
        // Only the answer to the outstanding request ends the wait. A late
        // answer to a query the person has typed past leaves it running.
        dialog.wiki_pending = false;
        // The status moves even when the rows do not: a build that finished
        // between two identical answers is what re-enables the search box.
        dialog.wiki_status = page.status;
        if dialog.search_focus_pending && dialog.search_enabled() {
            dialog.search_focus_pending = false;
            dialog.form.get_mut().focus(ResumeFocus::Search);
        }
        if *dialog.wiki == page.rows {
            self.rebuild_resume_rows();
            return;
        }
        dialog.wiki = Arc::new(page.rows);
        self.rebuild_resume_rows();
        self.resync_resume_selection();
    }

    /// The preview the open dialog still needs for the row under its
    /// selection: the query's matching passages while a query is active, and
    /// the briefing otherwise. [`DashboardAction::None`] when it has it
    /// already, or has asked for it.
    ///
    /// Moving the selection asks for one. A search answer can also put a
    /// different row under an unmoved selection, and that row's transcript is
    /// what the preview pane is already promising, so it is asked for here.
    pub fn next_wiki_preview(&mut self) -> DashboardAction {
        let wiki_id = self
            .selected_resume_row()
            .and_then(|row| row.wiki_id().map(ToOwned::to_owned));
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return DashboardAction::None;
        };
        let Some(wiki_id) = wiki_id else {
            return DashboardAction::None;
        };
        let Some(query) = dialog.active_query() else {
            if dialog.previews.contains_key(&wiki_id)
                || dialog.preview_pending.as_deref() == Some(wiki_id.as_str())
            {
                return DashboardAction::None;
            }
            dialog.preview_pending = Some(wiki_id.clone());
            return DashboardAction::LoadArchivedBrief { wiki_id };
        };
        let key = (wiki_id.clone(), query.clone());
        if dialog.hits.contains_key(&key) || dialog.hits_pending.as_ref() == Some(&key) {
            return DashboardAction::None;
        }
        dialog.hits_pending = Some(key);
        DashboardAction::LoadArchivedHits { wiki_id, query }
    }

    /// The query to re-issue, and how long to wait first, after an answer said
    /// the index is still changing. `None` when the answer was final.
    ///
    /// Two reasons to ask again. The first build has not finished, so the
    /// whole answer will change: poll every five seconds until it is ready,
    /// which also re-enables the search box without reopening the dialog. Or a
    /// top-up sync is running, so this query may gain rows: repeat it on the
    /// [`WIKI_TOP_UP_BACKOFF`] schedule for as long as the sync runs, so a long
    /// sync is followed to its end instead of leaving the pane promising rows
    /// that never arrive.
    pub fn next_wiki_refresh(&mut self) -> Option<(u64, String, Duration)> {
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return None;
        };
        let delay = match dialog.wiki_status.state {
            WikiIndexState::VersionMismatch => return None,
            WikiIndexState::Indexing => WIKI_INDEXING_POLL,
            WikiIndexState::Ready => {
                if !dialog.wiki_status.topping_up {
                    return None;
                }
                let step = usize::try_from(dialog.wiki_top_ups)
                    .unwrap_or(usize::MAX)
                    .min(WIKI_TOP_UP_BACKOFF.len() - 1);
                dialog.wiki_top_ups = dialog.wiki_top_ups.saturating_add(1);
                WIKI_TOP_UP_BACKOFF[step]
            }
        };
        dialog.wiki_request_id = dialog.wiki_request_id.wrapping_add(1);
        dialog.wiki_pending = true;
        Some((dialog.wiki_request_id, dialog.search.to_string(), delay))
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

    /// Fold one query's matching passages into the open dialog's cache. `None`
    /// means the index no longer holds that session, which the pane shows the
    /// same way as a session with no match.
    pub fn apply_wiki_hits(
        &mut self,
        wiki_id: String,
        query: String,
        transcript: Option<WikiHitTranscript>,
    ) {
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return;
        };
        let key = (wiki_id, query);
        if dialog.hits_pending.as_ref() == Some(&key) {
            dialog.hits_pending = None;
        }
        let showing = dialog.preview_key.as_ref() == Some(&key);
        Arc::make_mut(&mut dialog.hits).insert(key, transcript.unwrap_or_default());
        if showing {
            // The passages are what the pane is already showing, so open it on
            // the first match rather than at the top of the excerpt.
            self.focus_preview_hit(0);
        }
    }

    /// Move the preview pane onto hit `index`, reporting whether there was
    /// such a hit. The lines are wrapped here with the width the pane was last
    /// drawn at, so the stored offset is the row the reader will see.
    fn focus_preview_hit(&mut self, index: usize) -> bool {
        let surface = self
            .frame_surfaces
            .surface(SurfaceId::ResumePreview)
            .copied();
        let Mode::ResumeDialog(dialog) = &self.mode else {
            return false;
        };
        let Some((lines, hit_lines)) = dialog.preview_body(&self.resume_rows) else {
            return false;
        };
        let Some(&logical) = hit_lines.get(index) else {
            return false;
        };
        let scroll = match surface {
            Some(surface) => {
                let width = usize::from(surface.rect.width);
                let viewport = usize::from(surface.rect.height);
                let total = wrap_preview_lines(&lines, width).len();
                wrap_preview_lines(&lines[..logical], width)
                    .len()
                    .min(total.saturating_sub(viewport))
            }
            // The pane has not been drawn yet, so its width is unknown; the
            // next frame shows the excerpt from its top.
            None => 0,
        };
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return false;
        };
        dialog.preview_hit = index;
        dialog.preview_scroll = scroll;
        true
    }

    /// Move `step` hits from the one the pane is on, stopping at either end,
    /// and report whether the keystroke belonged to the pane.
    fn step_preview_hit(&mut self, step: isize) -> bool {
        let Mode::ResumeDialog(dialog) = &self.mode else {
            return false;
        };
        let Some((_, hit_lines)) = dialog.preview_body(&self.resume_rows) else {
            return false;
        };
        if hit_lines.is_empty() {
            return false;
        }
        let next = dialog
            .preview_hit
            .saturating_add_signed(step)
            .min(hit_lines.len() - 1);
        self.focus_preview_hit(next)
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
    /// should run. `None` when no dialog is open.
    pub fn next_wiki_search(&mut self) -> Option<(u64, String)> {
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return None;
        };
        dialog.wiki_request_id = dialog.wiki_request_id.wrapping_add(1);
        // A new query starts the backoff again; the old one's is spent.
        dialog.wiki_top_ups = 0;
        dialog.wiki_pending = true;
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
        self.sync_resume_preview_key();
    }

    /// Keeps the preview's scroll offset with the excerpt it was made for. A
    /// different transcript, or the same one under a new query, opens at its
    /// top and then on its first hit.
    fn sync_resume_preview_key(&mut self) {
        let key = {
            let Mode::ResumeDialog(dialog) = &self.mode else {
                return;
            };
            dialog.preview_wiki_id(&self.resume_rows).map(|wiki_id| {
                (
                    wiki_id.to_owned(),
                    dialog.active_query().unwrap_or_default(),
                )
            })
        };
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return;
        };
        if dialog.preview_key == key {
            return;
        }
        dialog.preview_key = key;
        dialog.preview_scroll = 0;
        dialog.preview_hit = 0;
        self.focus_preview_hit(0);
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
        self.sync_resume_preview_key();
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

    /// Moves the preview pane when a wheel notch lands on it or a page key is
    /// pressed with the list focused, and reports whether it took the event.
    ///
    /// The pane registers a scrollable surface every frame, so the last frame's
    /// registration carries both the hitbox and the wrapped line count the
    /// offset clamps against.
    fn scroll_resume_preview(&mut self, event: &Event, focused: ResumeFocus) -> bool {
        let showing = match &self.mode {
            // The registration is last frame's, so a selection that has just
            // moved to a row without a transcript must not keep the pane's
            // keys and hitbox until the next frame.
            Mode::ResumeDialog(dialog) => dialog.has_preview(&self.resume_rows),
            _ => false,
        };
        if !showing {
            return false;
        }
        let Some(surface) = self
            .frame_surfaces
            .surface(SurfaceId::ResumePreview)
            .copied()
        else {
            return false;
        };
        let viewport = usize::from(surface.rect.height);
        let step = match event {
            Event::Mouse(mouse)
                if surface
                    .rect
                    .contains(Position::new(mouse.column, mouse.row)) =>
            {
                match mouse.kind {
                    MouseEventKind::ScrollUp => -(PREVIEW_SCROLL_ROWS as isize),
                    MouseEventKind::ScrollDown => PREVIEW_SCROLL_ROWS as isize,
                    _ => return false,
                }
            }
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && key.modifiers.is_empty()
                    && focused == ResumeFocus::Sessions =>
            {
                match key.code {
                    KeyCode::PageUp => -(viewport.max(1) as isize),
                    KeyCode::PageDown => viewport.max(1) as isize,
                    _ => return false,
                }
            }
            _ => return false,
        };
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return false;
        };
        let maximum = surface.total_rows.saturating_sub(viewport);
        let next = dialog
            .preview_scroll
            .saturating_add_signed(step)
            .min(maximum);
        let moved = next != dialog.preview_scroll;
        dialog.preview_scroll = next;
        // The gesture belongs to the pane whether or not it had room to move,
        // so a wheel at the end of the text does not fall through to the list.
        self.last_event_consumed.set(moved);
        true
    }

    pub(crate) fn handle_resume_dialog_event(&mut self, event: Event) -> DashboardAction {
        use ResumeFocus::*;
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return DashboardAction::None;
        };
        // A press or click before the index is ready is the person choosing
        // where the focus goes; the box must not take it back later.
        if matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Press)
            || matches!(&event, Event::Mouse(mouse) if !matches!(mouse.kind, MouseEventKind::Moved))
        {
            dialog.search_focus_pending = false;
        }
        let dialog = &*dialog;
        let focused = dialog.focused();
        if self.scroll_resume_preview(&event, focused) {
            return DashboardAction::None;
        }
        // Moving between the query's matches, with the list focused. `N` comes
        // with Shift, so these cannot sit under the modifier-free block below.
        if let Event::Key(key) = &event
            && key.kind == KeyEventKind::Press
            && focused == ResumeFocus::Sessions
            && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
        {
            let step = match key.code {
                KeyCode::Char('n') | KeyCode::Char(']') => Some(1),
                KeyCode::Char('N') | KeyCode::Char('[') => Some(-1),
                _ => None,
            };
            if let Some(step) = step
                && self.step_preview_hit(step)
            {
                self.last_event_consumed.set(true);
                return DashboardAction::None;
            }
        }
        let Mode::ResumeDialog(dialog) = &mut self.mode else {
            return DashboardAction::None;
        };
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
                        // A disabled box cannot take the focus, and asking for
                        // it would leave the focus pending until it can.
                        if dialog.search_enabled() {
                            dialog.form.get_mut().focus(Search);
                        }
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
                        return self.next_wiki_preview();
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
                return self.next_wiki_preview();
            }
            Some(Interaction::Select(Sessions, index)) => {
                self.select_resume_row(index);
                return self.next_wiki_preview();
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

/// The least room the preview pane is worth drawing in: a border plus four
/// rows of text.
const PREVIEW_MINIMUM_HEIGHT: u16 = 6;

/// How tall the preview pane under the list is when the dialog shows one: two
/// fifths of the body, and never less than [`PREVIEW_MINIMUM_HEIGHT`].
fn preview_height(inner: Rect) -> u16 {
    PREVIEW_MINIMUM_HEIGHT.max(inner.height * 2 / 5)
}

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
        constraints.push(Constraint::Length(preview_height(inner)));
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
    let inner = theme::modal().inner(popup);
    let preview = dialog.preview_body(dashboard.resume_rows());
    let bands = resume_bands(inner, preview.is_some());
    let rows = bands.as_ref();
    // The footer is the last band whether or not a preview sits above it.
    let footer_band = rows[rows.len() - 1];

    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title_line = dismissible_modal_title(
        &mut form,
        popup,
        "Resume a session",
        theme::title(true),
        true,
    );
    frame.render_widget(theme::modal().title(title_line), popup);
    let tab_labels = resume_tab_labels(dashboard, dialog);
    TabStrip::render(
        frame,
        rows[0],
        &tab_labels.iter().map(String::as_str).collect::<Vec<_>>(),
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
    let field_area = Rect::new(
        search_area.x + label_width,
        search_area.y,
        search_area.width - label_width,
        search_area.height,
    );
    // Search is the index's answer. While the index cannot answer, the box
    // says why instead of taking text nothing would act on; the tabs and the
    // list keep working.
    if let Some(placeholder) = dialog.search_placeholder() {
        form.register(
            ResumeFocus::Search,
            ControlKind::TextField,
            field_area,
            false,
        );
        frame.render_widget(
            Paragraph::new(Line::styled(
                truncate_to_cells(
                    placeholder,
                    usize::from(field_area.width),
                    Truncate::SUMMARY,
                ),
                Style::default().fg(theme::palette().muted),
            )),
            field_area,
        );
    } else {
        TextField::render(
            frame,
            field_area,
            &dialog.search,
            &mut form,
            ResumeFocus::Search,
        );
    }
    let list_rows = dashboard.resume_rows();
    let sessions_focused = form.is_focused(ResumeFocus::Sessions);
    let block = theme::panel(sessions_focused || search_focused).title(resume_list_title(
        dashboard,
        dialog,
        list_rows.len(),
    ));
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
            (ResumeTab::Import, true, _) => "Scanning native sessions…".to_owned(),
            (ResumeTab::Hel, _, true) => "No stopped Mjolnir sessions".to_owned(),
            (ResumeTab::Import, _, true) => "No importable sessions".to_owned(),
            (ResumeTab::Archive, _, true) => "No archived sessions".to_owned(),
            _ => empty_search_message(dashboard, dialog),
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

    if let Some((preview, hit_lines)) = preview {
        let preview_band = rows[3];
        let title = match (dialog.active_query(), hit_lines.len()) {
            (None, _) => " Archived transcript ".to_owned(),
            (Some(_), 0) => " Transcript · no hits ".to_owned(),
            (Some(_), hits) => format!(
                " Transcript · hit {}/{hits} ",
                dialog.preview_hit.min(hits - 1) + 1
            ),
        };
        let block = theme::panel(false).title(title);
        let body = block.inner(preview_band);
        frame.render_widget(block, preview_band);
        // Wrapped here rather than by the paragraph, so the scroll offset, the
        // clamp and the scrollbar all count the same rows.
        let wrapped = wrap_preview_lines(&preview, usize::from(body.width));
        let viewport = usize::from(body.height);
        let length = wrapped.len();
        let offset = dialog.preview_scroll.min(length.saturating_sub(viewport));
        surfaces.push(SurfaceFrame::scrollable(
            SurfaceId::ResumePreview,
            body,
            offset,
            length.max(viewport),
        ));
        frame.render_widget(
            Paragraph::new(wrapped).scroll((u16::try_from(offset).unwrap_or(u16::MAX), 0)),
            body,
        );
        render_session_scrollbar(frame, preview_band, length, offset, viewport.max(1));
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
}

/// The tab labels, carrying the scan progress on Import and, while a query is
/// active, how many rows each tab matched.
fn resume_tab_labels(dashboard: &DashboardState, dialog: &ResumeDialog) -> Vec<String> {
    let hits = dashboard.resume_hit_counts;
    let searching = !dialog.search.is_empty();
    let (scanned, total) = dialog.scan_progress();
    [
        (ResumeTab::Hel, "Mjolnir"),
        (ResumeTab::Import, "Import"),
        (ResumeTab::Archive, "Archived"),
    ]
    .into_iter()
    .map(|(tab, name)| {
        let mut label = format!(" {name}");
        if searching {
            label.push_str(&format!(" · {}", hits[tab.index()]));
        }
        if tab == ResumeTab::Import && dialog.is_scanning() {
            label.push_str(&format!(" · scanning {scanned}/{total}"));
        }
        label.push(' ');
        label
    })
    .collect()
}

/// The list panel's title: what the tab lists, how the current search is
/// going, and, on Import, whether the native scan is still running.
fn resume_list_title(
    dashboard: &DashboardState,
    dialog: &ResumeDialog,
    rows: usize,
) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    if dialog.search.is_empty() {
        spans.push(Span::raw(match dialog.tab {
            ResumeTab::Hel => "Mjolnir sessions · newest first",
            ResumeTab::Import => "Importable sessions · newest first",
            ResumeTab::Archive => "Archived sessions · newest first",
        }));
    } else if dialog.wiki_status.state == WikiIndexState::Indexing {
        spans.push(Span::raw("Index building…"));
    } else if dialog.wiki_pending && rows == 0 {
        spans.push(mj_chat::spinner::compact_span(
            dashboard.config.spinner,
            dialog.opened_at.elapsed().as_millis(),
        ));
        spans.push(Span::raw(" Searching…"));
    } else if dialog.wiki_status.topping_up && rows > 0 {
        spans.push(Span::raw(format!(
            "{rows} matches · index syncing, more may arrive"
        )));
    } else {
        spans.push(Span::raw(format!("{rows} matches")));
    }
    if dialog.tab == ResumeTab::Import && dialog.is_scanning() {
        let (scanned, total) = dialog.scan_progress();
        spans.push(Span::raw(" · "));
        spans.push(mj_chat::spinner::compact_span(
            dashboard.config.spinner,
            dialog.opened_at.elapsed().as_millis(),
        ));
        spans.push(Span::raw(format!(" scanning {scanned}/{total}")));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// What an empty list says while a query is running: where the query's hits
/// are, when they are on another tab. The dialog never switches tabs by
/// itself, so the message has to say where to look.
fn empty_search_message(dashboard: &DashboardState, dialog: &ResumeDialog) -> String {
    let hits = dashboard.resume_hit_counts;
    let elsewhere = [
        (ResumeTab::Hel, "Mjolnir"),
        (ResumeTab::Import, "Import"),
        (ResumeTab::Archive, "Archived"),
    ]
    .into_iter()
    .filter(|(tab, _)| *tab != dialog.tab && hits[tab.index()] > 0)
    .map(|(tab, name)| format!("{} on {name}", hits[tab.index()]))
    .collect::<Vec<_>>();
    if elsewhere.is_empty() {
        return "No matching sessions".to_owned();
    }
    format!("No matches here · {}", elsewhere.join(", "))
}

/// The label a matching message carries. The roles are the briefing's own:
/// anything else, such as the one-line message an error is shown as, carries
/// no label.
fn hit_role_label(role: &str) -> Option<&'static str> {
    match role {
        "user" => Some("User: "),
        "assistant" => Some("Assistant: "),
        "tool" => Some("[tool] "),
        _ => None,
    }
}

fn omitted_marker(messages: usize) -> String {
    let plural = if messages == 1 { "message" } else { "messages" };
    format!("*[… {messages} {plural} omitted …]*")
}

/// One matching message as lines of spans, with the matched ranges styled and
/// the logical line each match starts on collected for hit navigation.
///
/// `hits` are byte ranges into the whole message, so a match that straddles a
/// newline is split across the lines it covers and only its first line counts
/// as the hit's position.
fn hit_block_lines(
    block: &WikiHitBlock,
    lines: &mut Vec<Line<'static>>,
    hit_lines: &mut Vec<usize>,
) {
    let hit_style = Style::default()
        .fg(theme::palette().accent)
        .add_modifier(Modifier::BOLD);
    let mut prefix = hit_role_label(&block.role)
        .map(|label| Span::styled(label, Style::default().fg(theme::palette().muted)));
    let mut start = 0usize;
    for text in block.text.split('\n') {
        let end = start + text.len();
        let mut spans = Vec::new();
        if let Some(prefix) = prefix.take() {
            spans.push(prefix);
        }
        let mut cursor = start;
        for &(hit_start, hit_end) in &block.hits {
            let from = hit_start.max(cursor);
            let to = hit_end.min(end);
            if from >= to {
                continue;
            }
            if from > cursor {
                spans.push(Span::raw(block.text[cursor..from].to_owned()));
            }
            if hit_start >= start {
                hit_lines.push(lines.len());
            }
            spans.push(Span::styled(block.text[from..to].to_owned(), hit_style));
            cursor = to;
        }
        if cursor < end {
            spans.push(Span::raw(block.text[cursor..end].to_owned()));
        }
        lines.push(Line::from(spans));
        // Past the newline that ended this line.
        start = end + 1;
    }
    if block.truncated {
        lines.push(Line::styled(
            "[… truncated …]",
            Style::default().fg(theme::palette().muted),
        ));
    }
}

/// The matching passages as logical lines of spans, with the logical line each
/// hit starts on.
fn hit_transcript_lines(transcript: &WikiHitTranscript) -> (Vec<Line<'static>>, Vec<usize>) {
    let muted = Style::default().fg(theme::palette().muted);
    if transcript.blocks.is_empty() {
        return (
            vec![Line::styled("No matching passages", muted)],
            Vec::new(),
        );
    }
    let mut lines = Vec::new();
    let mut hit_lines = Vec::new();
    for (index, block) in transcript.blocks.iter().enumerate() {
        if index > 0 {
            lines.push(Line::raw(String::new()));
        }
        if block.omitted_before > 0 {
            lines.push(Line::styled(omitted_marker(block.omitted_before), muted));
        }
        hit_block_lines(block, &mut lines, &mut hit_lines);
    }
    if transcript.omitted_after > 0 {
        lines.push(Line::styled(
            omitted_marker(transcript.omitted_after),
            muted,
        ));
    }
    (lines, hit_lines)
}

/// Wrap preview lines to `width` cells, keeping each span's style.
///
/// The pane wraps its own text so the wrapped row count is exact: the scroll
/// offset, the clamp and the scrollbar all count the rows the renderer draws.
/// Continuation rows are not indented, so a wrapped passage stays flush with
/// the pane's left edge.
fn wrap_preview_lines(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    lines
        .iter()
        .flat_map(|line| wrap_styled_line(line.clone(), width, 0))
        .collect()
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
