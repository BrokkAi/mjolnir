//! The `F2` command palette: every command that applies right now, in one
//! searchable list.
//!
//! The palette is a renderer over the action registry ([`crate::actions`]).
//! It groups what it lists the way the user is standing: the selected
//! session's own commands first, under a heading naming that session, then the
//! focused pane's, then the ones that answer from anywhere. Nothing here
//! decides what a command does — [`DashboardState::dispatch_command`] does
//! that, so the palette, the footer, and the keyboard cannot disagree.
//!
//! It replaces the old session edit dialog, which existed only because the
//! footer had no room for three more hints.

use std::cell::RefCell;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use mj_chat::components::{ChoiceList, ControlKind, Dialog, Interaction, TextField};
use mj_chat::theme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use mj_chat::selection::FrameSurfaces;
use mj_chat::text_input::TextInput;

use crate::actions::{Availability, COMMANDS, CommandId, Scope, hidden_from_palette, spec};
use crate::render::render_session_scrollbar;
use crate::widgets::{Truncate, centered_modal, dismissible_modal_title, truncate_to_cells};
use crate::{DashboardAction, DashboardState, Focus, Mode};
use mj_core::state::{ManagedCheckoutKind, SessionRecord};
use mj_core::subagent::SubagentPolicy;

mod workspace;

/// One row of the palette: a command and whether it can be run.
///
/// `Blocked` entries are listed greyed with their reason rather than dropped,
/// for the same reason the help overlay lists them: a list that hides what
/// does not apply leaves the reader unable to tell "there is no such command"
/// from "not here, not now".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaletteEntry {
    pub(crate) id: CommandId,
    pub(crate) availability: Availability,
    /// Listed under the Recent heading because it was run lately, ahead of
    /// its own group.
    pub(crate) recent: bool,
}

/// The open palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandPalette {
    /// What the user has typed. Filtering is by label, then description.
    pub(crate) query: TextInput,
    /// The entries the query matches, in the order they are drawn.
    pub(crate) entries: Vec<PaletteEntry>,
    /// Index into `entries` of the highlighted row.
    pub(crate) selected: usize,
    /// The query `entries` were built for, so a rebuild can tell a new search
    /// from the same search rebuilt because availability moved.
    ranked_for: String,
    pub(crate) form: RefCell<Dialog<PaletteControl>>,
    session_only: bool,
    session_id: Option<String>,
    session_title: Option<String>,
    /// The workspace list open on the "Change workspace" row, if any.
    workspace_popup: Option<workspace::WorkspacePopup>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaletteControl {
    Query,
    Commands,
    Run,
    /// The combobox anchored on the "Change workspace" row.
    Workspace,
}

impl CommandPalette {
    fn prepare(&self) {
        let mut form = self.form.borrow_mut();
        form.begin_update();
        if !self.session_only {
            form.declare_with_enabled(PaletteControl::Query, ControlKind::TextField, true);
        }
        form.declare_with_enabled(
            PaletteControl::Commands,
            ControlKind::ChoiceList {
                len: self.entries.len(),
                selected: self.selected,
            },
            !self.entries.is_empty(),
        );
        if !self.session_only {
            form.declare_with_enabled(
                PaletteControl::Run,
                ControlKind::Button,
                self.entries
                    .get(self.selected)
                    .is_some_and(|entry| entry.availability == Availability::Ready),
            );
        }
        if let Some(popup) = &self.workspace_popup {
            form.declare_with_enabled(PaletteControl::Workspace, popup.kind(), true);
        }
        form.set_menu(true);
        form.set_list_activation(
            PaletteControl::Commands,
            mj_chat::components::ListActivation::SingleClick,
        );
        form.end_frame(if self.session_only {
            PaletteControl::Commands
        } else {
            PaletteControl::Query
        });
    }
}

const SESSION_MENU_COMMANDS: &[CommandId] = &[
    CommandId::ChangedFiles,
    CommandId::RenameSession,
    CommandId::PinSession,
    CommandId::UnpinSession,
    CommandId::ChangeWorkspace,
    CommandId::ContainerSettings,
    CommandId::MoveSession,
    CommandId::SuspendSession,
    CommandId::RestartSession,
    CommandId::InterruptAll,
    CommandId::CopySessionId,
    CommandId::DestroySession,
];

/// The menu of a harness-native child. Its parent's harness owns it, and
/// Mjolnir keeps no record of its own for it to rename, pin, restart or
/// destroy, so reading its conversation is what applies (R10-2).
const NATIVE_AGENT_MENU_COMMANDS: &[CommandId] = &[CommandId::OpenSession];

fn session_menu_entries(dashboard: &DashboardState) -> Vec<PaletteEntry> {
    let commands = if dashboard
        .command_session_id()
        .is_some_and(|id| dashboard.is_native_agent(id))
    {
        NATIVE_AGENT_MENU_COMMANDS
    } else {
        SESSION_MENU_COMMANDS
    };
    commands
        .iter()
        .filter_map(|id| {
            let availability = (spec(*id).available)(dashboard);
            (availability != Availability::Hidden).then_some(PaletteEntry {
                id: *id,
                availability,
                recent: false,
            })
        })
        .collect()
}

/// Facts about the session that its row does not show and that change what
/// the menu's actions do: how it delegates, what it has checked out, and the
/// container settings it carries into its next container.
fn session_facts(dashboard: &DashboardState, session: &SessionRecord) -> Vec<Line<'static>> {
    let mut facts = Vec::new();
    let fact = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{label:<11}"), theme::muted()),
            Span::raw(value),
        ])
    };

    let policy = session.subagents.clone().unwrap_or_default();
    let mut delegation = match &policy {
        SubagentPolicy::Native => "Native".to_owned(),
        SubagentPolicy::AllModels => "Mjolnir, all models".to_owned(),
        SubagentPolicy::SingleModel { model, effort } => match effort.as_deref() {
            Some(mj_core::subagent::ADAPTIVE_EFFORT) => format!(
                "Mjolnir, {model} ({})",
                mj_core::subagent::ADAPTIVE_EFFORT_LABEL
            ),
            Some(effort) => format!("Mjolnir, {model} ({effort})"),
            None => format!("Mjolnir, {model}"),
        },
        SubagentPolicy::None => "None".to_owned(),
    };
    let total = dashboard.subagent_count_for(&session.id);
    if total > 0 {
        delegation.push_str(&format!(
            " · {} of {total} working",
            dashboard.working_subagent_count_for(&session.id)
        ));
    }
    facts.push(fact("Sub-agents", delegation));

    // A full or low filesystem the session writes to changes what every
    // write-bearing action can do.
    for (view, filesystem) in dashboard.session_filesystems(&session.id) {
        let label = match filesystem.condition {
            mj_core::targets::storage::StorageCondition::Ok => continue,
            mj_core::targets::storage::StorageCondition::Low => "disk low",
            mj_core::targets::storage::StorageCondition::Full => "disk full",
        };
        facts.push(fact(
            "Disk",
            format!("{label}: {}", view.explanation(filesystem)),
        ));
    }

    let checkout = session.checkout();
    if let Some(worktree) = checkout.managed_worktree() {
        let kind = match worktree.kind {
            ManagedCheckoutKind::Worktree => "Worktree",
            ManagedCheckoutKind::Clone => "Clone",
        };
        facts.push(fact(
            "Checkout",
            format!("{kind} of {}", worktree.source_project_directory.display()),
        ));
        facts.push(fact(
            "Branch",
            match &worktree.base_commit {
                Some(base) => format!("{} from {}", worktree.branch, short_commit(base)),
                None => worktree.branch.clone(),
            },
        ));
    } else if let Some(directory) = checkout.project_directory() {
        facts.push(fact(
            "Checkout",
            format!("{} (in place)", directory.display()),
        ));
    } else if let Some(checkout) = &session.checkout {
        facts.push(fact(
            "Checkout",
            format!(
                "{} at {}",
                checkout.repository_id,
                short_commit(&checkout.commit)
            ),
        ));
        if let Some(branch) = &checkout.branch {
            facts.push(fact("Branch", branch.clone()));
        }
    }

    let cpu = crate::session_cpu_report::cpu_summary(dashboard, &session.id);
    let mut cpu_line = vec![
        Span::styled(format!("{:<11}", "CPU"), theme::muted()),
        cpu.main_span(),
    ];
    match &cpu.detail {
        Some(detail) if cpu.tree => {
            facts.push(Line::from(cpu_line));
            facts.push(Line::from(vec![
                Span::raw(" ".repeat(11)),
                Span::styled(format!("({detail})"), theme::muted()),
            ]));
        }
        detail => {
            if let Some(detail) = detail {
                cpu_line.push(Span::styled(format!(" ({detail})"), theme::muted()));
            }
            facts.push(Line::from(cpu_line));
        }
    }

    let mut container = Vec::new();
    if let Some(cpus) = &session.container_cpus {
        container.push(format!("{cpus} CPUs"));
    }
    if let Some(memory) = &session.container_memory {
        container.push(format!("{memory} memory"));
    }
    if !session.additional_mounts.is_empty() {
        container.push(crate::widgets::counted(
            session.additional_mounts.len(),
            "mount",
            "mounts",
        ));
    }
    if session.build_cache.is_some() {
        container.push("build cache".to_owned());
    }
    if !container.is_empty() {
        facts.push(fact("Container", container.join(" · ")));
    }
    facts
}

/// The first 10 characters of a commit ID, as `git log --oneline` shows it.
fn short_commit(commit: &str) -> &str {
    commit.get(..10).unwrap_or(commit)
}

/// What the palette says a command does, for the session it would act on.
///
/// Open shows a harness-native child's or a stopped sub-agent's conversation
/// without a composer, so it must not promise that you can type in it (R11-4).
fn command_description(dashboard: &DashboardState, id: CommandId) -> &'static str {
    if id == CommandId::OpenSession
        && dashboard.command_session_id().is_some_and(|session| {
            dashboard.is_native_agent(session) || dashboard.is_stopped_subagent(session)
        })
    {
        return "Show the selected agent's conversation (read-only).";
    }
    spec(id).description
}

fn first_ready(entries: &[PaletteEntry]) -> usize {
    entries
        .iter()
        .position(|entry| entry.availability == Availability::Ready)
        .unwrap_or(0)
}

fn session_menu_label(id: CommandId) -> String {
    let label = match id {
        // Opens a second choice, so it carries the menu's submenu glyph.
        CommandId::ChangeWorkspace => {
            return format!("Change Workspace {}", theme::glyphs().navigate);
        }
        CommandId::OpenSession => "Open",
        CommandId::RenameSession => "Rename…",
        CommandId::PinSession => "Pin…",
        CommandId::UnpinSession => "Unpin",
        CommandId::MoveSession => "Move…",
        CommandId::SuspendSession => "Suspend…",
        CommandId::RestartSession => "Restart",
        CommandId::InterruptAll => "Interrupt all (parent + sub-agents)…",
        CommandId::DestroySession => "Destroy…",
        _ => spec(id).label,
    };
    label.to_owned()
}

/// The heading printed above the group `entry` opens, or `None` when the row
/// continues the group above it.
///
/// The selected session's group is headed by the session's own name, because
/// "Stop" means nothing without saying what it stops.
fn heading_for(dashboard: &DashboardState, scope: Scope) -> String {
    if scope == Scope::Session
        && let Some(session) = dashboard.command_session()
    {
        return if dashboard.go.is_some() {
            dashboard.go_conversation_title(&session.id)
        } else {
            crate::render::session_name(session).to_owned()
        };
    }
    scope.heading().to_owned()
}

/// The pane group the palette lists after the selected session's.
///
/// The composer is not a pane, but every Sessions-pane command answers from it
/// after the prefix, so typing in a conversation lists the Sessions pane's
/// group rather than none at all.
fn pane_scope(focus: Focus) -> Scope {
    match focus {
        Focus::Workspaces => Scope::Global,
        Focus::Targets => Scope::Targets,
        Focus::Quota => Scope::Quota,
        Focus::Sessions | Focus::Prompt => Scope::Sessions,
    }
}

/// The groups the palette walks, in the order it prints them.
fn scope_order(dashboard: &DashboardState) -> Vec<Scope> {
    let mut order = vec![Scope::Session, pane_scope(dashboard.focus)];
    for scope in [Scope::Setup, Scope::Pane, Scope::Settings, Scope::Global] {
        if !order.contains(&scope) {
            order.push(scope);
        }
    }
    order
}

/// How well `query` matches a command, higher being better, or `None` when
/// it does not match at all.
///
/// A label that starts with the query beats everything, then a label that
/// carries the query as one run of characters — `rend` in "rendering" — because
/// that is what a reader means by a match. Below those the query's characters
/// need only appear in the label in order (so `cre` finds "Create session" and
/// `mvs` finds "Move session"), scored by how many land on the start of a word
/// and how many sit next to each other; letters scattered across three words
/// are the weakest kind of label hit and must not outrank a run. A query found
/// only in the description ranks last, so a word from the description still
/// finds the command without outranking a label hit.
pub(crate) fn match_score(label: &str, description: &str, query: &str) -> Option<u32> {
    let query = query.to_lowercase();
    let label_lower = label.to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    if label_lower.starts_with(&query) {
        return Some(10_000);
    }
    if let Some(index) = label_lower.find(&query) {
        // A run that starts a word reads as the word the person typed, so it
        // comes before one buried inside another word.
        let word_start = label_lower[..index]
            .chars()
            .next_back()
            .is_none_or(|previous| !previous.is_alphanumeric());
        return Some(5_000 + if word_start { 30 } else { 0 });
    }
    if let Some(score) = subsequence_score(&label_lower, &query) {
        return Some(1_000 + score);
    }
    description.to_lowercase().contains(&query).then_some(100)
}

/// The in-order match of `query` in `text`, scored for word starts and
/// adjacency, or `None` when a character of the query never appears.
fn subsequence_score(text: &str, query: &str) -> Option<u32> {
    let mut score = 0u32;
    let mut previous_index: Option<usize> = None;
    let mut previous_char = ' ';
    let mut chars = text.char_indices().peekable();
    for wanted in query.chars() {
        loop {
            let (index, found) = chars.next()?;
            if found == wanted {
                if !previous_char.is_alphanumeric() {
                    score += 30;
                }
                if previous_index
                    .is_some_and(|previous| previous + previous_char.len_utf8() == index)
                {
                    score += 20;
                }
                previous_index = Some(index);
                previous_char = found;
                break;
            }
            previous_char = found;
        }
    }
    // Fewer leftover characters means a tighter match.
    Some(score + 10u32.saturating_sub(text.len().saturating_sub(query.len()).min(10) as u32))
}

/// Orders the entries by [`match_score`], keeping registry order among
/// equals so the groups stay together when the query is broad.
fn rank(entries: Vec<PaletteEntry>, query: &str) -> Vec<PaletteEntry> {
    if query.is_empty() {
        return entries;
    }
    let mut scored = entries
        .into_iter()
        .filter_map(|entry| {
            let spec = spec(entry.id);
            match_score(spec.label, spec.description, query).map(|score| (score, entry))
        })
        .collect::<Vec<_>>();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, entry)| entry).collect()
}

/// Every command the palette would list for `query`, in drawing order.
///
/// `Hidden` commands are left out entirely, as are the commands
/// [`hidden_from_palette`] names because a visible dashboard control already
/// runs them.
pub(crate) fn palette_entries(dashboard: &DashboardState, query: &str) -> Vec<PaletteEntry> {
    let mut entries = Vec::new();
    for scope in scope_order(dashboard) {
        for spec in COMMANDS.iter().filter(|spec| spec.scope == scope) {
            if hidden_from_palette(spec.id) {
                continue;
            }
            let availability = (spec.available)(dashboard);
            if availability == Availability::Hidden {
                continue;
            }
            entries.push(PaletteEntry {
                id: spec.id,
                availability,
                recent: false,
            });
        }
    }
    if query.is_empty() {
        // What was run lately leads an unfiltered list; the same commands
        // stay in their own groups below, so nothing moves out of place.
        let recent = dashboard
            .recent_commands
            .iter()
            .filter_map(|id| {
                entries
                    .iter()
                    .find(|entry| entry.id == *id)
                    .map(|entry| PaletteEntry {
                        recent: true,
                        ..entry.clone()
                    })
            })
            .collect::<Vec<_>>();
        entries.splice(0..0, recent);
        return entries;
    }
    rank(entries, query)
}

impl DashboardState {
    /// Opens the palette over the dashboard.
    pub(crate) fn begin_palette(&mut self) {
        let entries = palette_entries(self, "");
        let palette = CommandPalette {
            query: TextInput::new(),
            entries,
            selected: 0,
            ranked_for: String::new(),
            form: RefCell::new(Dialog::default()),
            session_only: false,
            session_id: self.command_session_id().map(str::to_owned),
            session_title: None,
            workspace_popup: None,
        };
        palette.prepare();
        self.mode = Mode::Palette(palette);
    }

    pub(crate) fn begin_session_palette(&mut self) {
        let session_id = self.command_session_id().map(str::to_owned);
        let session_title = self.command_session().map(|session| {
            if self.go.is_some() {
                self.go_conversation_title(&session.id)
            } else {
                crate::render::session_name(session).to_owned()
            }
        });
        let entries = session_menu_entries(self);
        let palette = CommandPalette {
            query: TextInput::new(),
            selected: first_ready(&entries),
            entries,
            ranked_for: String::new(),
            form: RefCell::new(Dialog::default()),
            session_only: true,
            session_id,
            session_title,
            workspace_popup: None,
        };
        palette.prepare();
        self.mode = Mode::Palette(palette);
    }

    /// Refreshes query matches and availability.
    ///
    /// A new query is a new list, so the cursor starts on its top match: Enter
    /// runs the row the cursor is on, and a cursor that followed the command it
    /// happened to sit on — the one the Recent group leads with, say — would run
    /// a command the ranking had moved to the bottom. A rebuild under the same
    /// query is only availability moving, so there the cursor stays put.
    pub(crate) fn rebuild_palette_entries(&mut self) {
        let Mode::Palette(palette) = &self.mode else {
            return;
        };
        let query = palette.query.value().to_owned();
        let session_only = palette.session_only;
        let session_id = palette.session_id.clone();
        let old_selected = palette.selected;
        let old_id = palette.entries.get(old_selected).map(|entry| entry.id);
        let saved_override = self.command_session_override.clone();
        if session_only {
            self.command_session_override = session_id;
        }
        let entries = if session_only {
            session_menu_entries(self)
        } else {
            palette_entries(self, &query)
        };
        self.command_session_override = saved_override;
        let Mode::Palette(palette) = &mut self.mode else {
            return;
        };
        let same_query = palette.ranked_for == query;
        if same_query && palette.entries == entries {
            return;
        }
        palette.selected = if session_only {
            let anchor = old_id
                .and_then(|id| entries.iter().position(|entry| entry.id == id))
                .unwrap_or_else(|| old_selected.min(entries.len().saturating_sub(1)));
            if entries
                .get(anchor)
                .is_some_and(|entry| entry.availability == Availability::Ready)
            {
                anchor
            } else {
                entries
                    .iter()
                    .enumerate()
                    .skip(anchor.saturating_add(1))
                    .find(|(_, entry)| entry.availability == Availability::Ready)
                    .map(|(index, _)| index)
                    .or_else(|| {
                        entries
                            .iter()
                            .enumerate()
                            .take(anchor)
                            .rev()
                            .find(|(_, entry)| entry.availability == Availability::Ready)
                            .map(|(index, _)| index)
                    })
                    .unwrap_or(0)
            }
        } else if same_query {
            palette
                .entries
                .get(palette.selected)
                .map(|entry| entry.id)
                .and_then(|id| entries.iter().position(|entry| entry.id == id))
                .unwrap_or(0)
        } else {
            0
        };
        palette.ranked_for = query;
        palette.form.get_mut().cancel_pointer();
        palette.entries = entries;
        palette.prepare();
    }

    pub(crate) fn handle_palette_event(&mut self, event: Event) -> DashboardAction {
        if matches!(&self.mode, Mode::Palette(palette) if palette.workspace_popup.is_some()) {
            return self.handle_workspace_popup_event(event);
        }
        let Mode::Palette(palette) = &mut self.mode else {
            return DashboardAction::None;
        };
        // Search palettes let arrows browse results while typing remains in the query.
        let browse = match &event {
            Event::Key(key)
                if key.kind != KeyEventKind::Release
                    && palette.form.borrow().is_focused(PaletteControl::Query) =>
            {
                match key.code {
                    KeyCode::Up | KeyCode::Down => {
                        Some(KeyEvent::new(key.code, KeyModifiers::NONE))
                    }
                    KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
                        Some(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
                    }
                    KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
                        Some(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
                    }
                    // The query is a text field, so Ctrl-U and Ctrl-D stay
                    // readline's kill-to-line-start and delete-forward while
                    // there is text to edit. The list borrows them for paging
                    // only once the query is empty and they would do nothing.
                    KeyCode::Char('d' | 'u')
                        if key.modifiers == KeyModifiers::CONTROL && palette.query.is_empty() =>
                    {
                        Some(*key)
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let interaction = if let Some(browse) = browse {
            let form = palette.form.get_mut();
            form.focus(PaletteControl::Commands);
            let result = form.handle(&Event::Key(browse));
            form.focus(PaletteControl::Query);
            self.last_event_consumed.set(result.consumed);
            result.action
        } else {
            let result = palette.form.get_mut().handle(&event);
            self.last_event_consumed.set(result.consumed);
            result.action
        };
        match interaction {
            Some(Interaction::Cancel) => self.cancel_modal(),
            Some(Interaction::Edit(PaletteControl::Query, edit)) => {
                TextField::apply(&mut palette.query, edit);
                self.rebuild_palette_entries();
            }
            Some(Interaction::Select(PaletteControl::Commands, index)) => {
                if palette.selected != index {
                    palette.selected = index;
                }
            }
            Some(Interaction::Activate(
                PaletteControl::Query | PaletteControl::Commands | PaletteControl::Run,
            )) => {
                palette.selected = palette
                    .form
                    .borrow()
                    .selected(PaletteControl::Commands)
                    .unwrap_or(palette.selected);
                let Some(entry) = palette.entries.get(palette.selected).cloned() else {
                    return DashboardAction::None;
                };
                if let Availability::Blocked(reason) = entry.availability {
                    self.notices.set(format!(
                        "{} is unavailable: {reason}.",
                        spec(entry.id).label
                    ));
                    return DashboardAction::None;
                }
                if entry.id == CommandId::ChangeWorkspace {
                    // The choice opens on the row, inside the palette.
                    self.remember_command(entry.id);
                    self.open_workspace_popup();
                    return DashboardAction::None;
                }
                if spec(entry.id).scope == Scope::Session {
                    let Some(id) = palette
                        .session_id
                        .clone()
                        .filter(|id| self.state.sessions.contains_key(id))
                    else {
                        self.set_notice("This session is no longer available.");
                        return DashboardAction::None;
                    };
                    self.command_session_override = Some(id);
                }
                self.mode = Mode::Dashboard;
                // Everything run from here is listed under Recent, including
                // commands that also have a pane key.
                self.remember_command(entry.id);
                let action = self.run_available_command(entry.id);
                self.command_session_override = None;
                return action;
            }
            _ => {}
        }
        DashboardAction::None
    }
}

/// One drawn row: either a group heading or a command.
enum PaletteLine {
    Heading(String),
    Separator,
    /// The entry's index into `entries`, so the highlight can be placed.
    Command(usize),
}

/// The rows the palette draws, with a heading wherever the group changes.
fn palette_lines(dashboard: &DashboardState, palette: &CommandPalette) -> Vec<PaletteLine> {
    if palette.session_only {
        let mut lines = Vec::new();
        let mut section = None;
        for (index, entry) in palette.entries.iter().enumerate() {
            let next = match entry.id {
                CommandId::OpenSession | CommandId::ChangedFiles => 0,
                CommandId::RenameSession
                | CommandId::PinSession
                | CommandId::UnpinSession
                | CommandId::ChangeWorkspace => 1,
                CommandId::ContainerSettings
                | CommandId::MoveSession
                | CommandId::SuspendSession
                | CommandId::RestartSession
                | CommandId::InterruptAll => 2,
                CommandId::CopySessionId | CommandId::DestroySession => 3,
                _ => continue,
            };
            if section != Some(next) {
                match next {
                    0 => lines.push(PaletteLine::Heading("Content".to_owned())),
                    1 => lines.push(PaletteLine::Heading("Organize".to_owned())),
                    2 => lines.push(PaletteLine::Heading("Lifecycle".to_owned())),
                    _ => lines.push(PaletteLine::Separator),
                }
                section = Some(next);
            }
            lines.push(PaletteLine::Command(index));
        }
        return lines;
    }
    let mut lines = Vec::new();
    // `None` is the Recent group, which has no scope of its own.
    let mut previous: Option<Option<Scope>> = None;
    for (index, entry) in palette.entries.iter().enumerate() {
        let group = (!entry.recent).then(|| spec(entry.id).scope);
        if previous != Some(group) {
            lines.push(PaletteLine::Heading(match group {
                Some(scope) => heading_for(dashboard, scope),
                None => "Recent".to_owned(),
            }));
            previous = Some(group);
        }
        lines.push(PaletteLine::Command(index));
    }
    lines
}

pub(crate) fn render_palette(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    palette: &CommandPalette,
    surfaces: &mut FrameSurfaces,
) {
    let lines = palette_lines(dashboard, palette);
    let facts = if palette.session_only {
        palette
            .session_id
            .as_deref()
            // A harness-native child's row is presentation only; it has no
            // record of its own to describe.
            .filter(|id| !dashboard.is_native_agent(id))
            .and_then(|id| dashboard.state.sessions.get(id))
            .map(|session| session_facts(dashboard, session))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    // The facts are followed by one blank row that sets them apart from the
    // commands.
    let facts_height = if facts.is_empty() { 0 } else { facts.len() + 1 };
    // The popup grows with the complete list. The shared modal helper clamps
    // it to the usable terminal bounds when the list cannot fit.
    let extra_height = facts_height + if palette.session_only { 3 } else { 5 };
    let popup_height =
        u16::try_from(lines.len().saturating_add(extra_height).max(5)).unwrap_or(u16::MAX);
    let popup = centered_modal(frame, surfaces, 72, popup_height, area);
    let inner = theme::modal().inner(popup);
    let rows = if palette.session_only {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(u16::try_from(facts_height).unwrap_or(u16::MAX)),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(inner)
    };
    let list_row = rows[1];
    let description_row = rows[2];
    if !facts.is_empty() {
        let facts_row = Rect::new(
            rows[0].x.saturating_add(2),
            rows[0].y,
            rows[0].width.saturating_sub(2),
            rows[0].height.saturating_sub(1),
        );
        frame.render_widget(Paragraph::new(facts), facts_row);
    }

    let mut form = palette.form.borrow_mut();
    form.begin_frame();
    form.set_bounds(popup);
    let title = dismissible_modal_title(
        &mut form,
        popup,
        palette.session_title.as_ref().map_or_else(
            || format!("{} Commands", theme::glyphs().spark),
            |title| crate::fit_session_name(title, usize::from(popup.width).saturating_sub(8)),
        ),
        theme::title(true),
        true,
    );
    let mut outer = theme::modal().title(title);
    if !palette.session_only {
        outer = outer.title(
            Line::styled(
                format!(
                    " {} ",
                    crate::widgets::counted(palette.entries.len(), "command", "commands")
                ),
                theme::muted(),
            )
            .right_aligned(),
        );
    }
    let outer = outer.title_bottom(mj_chat::components::DialogShell::hints(true));
    frame.render_widget(outer, popup);
    if !palette.session_only {
        TextField::render(
            frame,
            rows[0],
            &palette.query,
            &mut form,
            PaletteControl::Query,
        );
        if palette.query.is_empty() {
            frame.render_widget(
                Line::styled(
                    format!("Search commands{}", theme::glyphs().ellipsis),
                    theme::muted().add_modifier(Modifier::ITALIC),
                ),
                rows[0],
            );
        }
    }

    // Keep the rail outside the list's registered area. Besides leaving the
    // command text untouched, this means clicking the scrollbar cannot be
    // interpreted as clicking a command row.
    let list_area = Rect::new(
        list_row.x,
        list_row.y,
        list_row.width.saturating_sub(1),
        list_row.height,
    );
    let scrollbar_area = Rect::new(
        list_row.x.saturating_add(list_area.width),
        list_row.y,
        list_row.width.saturating_sub(list_area.width),
        list_row.height,
    );
    let width = usize::from(list_area.width).saturating_sub(1);
    let mut row_map = Vec::new();
    let mut enabled = Vec::new();
    let items = lines
        .iter()
        .map(|line| match line {
            PaletteLine::Heading(heading) => {
                row_map.push(None);
                enabled.push(false);
                let heading = truncate_to_cells(heading, width.saturating_sub(4), Truncate::PLAIN);
                let rule_width = width.saturating_sub(heading.chars().count() + 4);
                Line::from(vec![
                    Span::styled(format!("  {heading}  "), theme::title(true)),
                    Span::styled(
                        theme::glyphs().rule.repeat(rule_width),
                        theme::border(false),
                    ),
                ])
            }
            PaletteLine::Separator => {
                row_map.push(None);
                enabled.push(false);
                Line::styled(
                    format!("  {}", theme::glyphs().rule.repeat(width.saturating_sub(2))),
                    theme::border(false),
                )
            }
            PaletteLine::Command(index) => {
                row_map.push(Some(*index));
                enabled.push(palette.entries[*index].availability == Availability::Ready);
                let entry = &palette.entries[*index];
                let spec = spec(entry.id);
                let keys = dashboard.key_labels(entry.id).join(" / ");
                // The same form help uses: its own sentence after a dash,
                // dimmed, not a lowercase fragment in brackets.
                let reason = match entry.availability {
                    Availability::Blocked(reason) => {
                        format!(" — {}", crate::help::sentence(reason))
                    }
                    Availability::Ready | Availability::Hidden => String::new(),
                };
                let selected = *index == palette.selected;
                let ready = entry.availability == Availability::Ready;
                // Keep command labels aligned and reserve a visible gap before
                // right-aligned shortcuts, even when a chord has several keys.
                let keys = truncate_to_cells(&keys, width.saturating_sub(4) / 2, Truncate::PLAIN);
                let key_width = Line::raw(keys.as_str()).width();
                let label_width = width.saturating_sub(key_width + 4);
                let label = if palette.session_only {
                    session_menu_label(entry.id)
                } else {
                    spec.label.to_owned()
                };
                let text =
                    truncate_to_cells(&format!("{label}{reason}"), label_width, Truncate::PLAIN);
                let style = if !ready {
                    theme::muted()
                } else if selected {
                    Style::default()
                        .fg(theme::palette().text)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::palette().text)
                };
                let padding = label_width.saturating_sub(Line::raw(text.as_str()).width()) + 2;
                let split = if text.starts_with(label.as_str()) {
                    label.len()
                } else {
                    text.len()
                };
                let (label_text, reason_text) = text.split_at(split);
                Line::from(vec![
                    Span::styled(
                        if selected {
                            theme::glyphs().selected
                        } else {
                            "  "
                        },
                        theme::title(true),
                    ),
                    Span::styled(label_text.to_owned(), style),
                    Span::styled(reason_text.to_owned(), style.add_modifier(Modifier::DIM)),
                    Span::raw(" ".repeat(padding)),
                    Span::styled(
                        keys,
                        Style::default().fg(if ready {
                            theme::palette().secondary
                        } else {
                            theme::palette().muted
                        }),
                    ),
                ])
            }
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        frame.render_widget(Line::raw("No matching command"), list_area);
        form.register(
            PaletteControl::Commands,
            ControlKind::ChoiceList {
                len: 0,
                selected: 0,
            },
            list_area,
            false,
        );
    } else {
        ChoiceList::render_with_rows(
            frame,
            list_area,
            &items,
            palette.selected,
            &row_map,
            &enabled,
            &mut form,
            PaletteControl::Commands,
        );
    }
    render_session_scrollbar(
        frame,
        scrollbar_area,
        items.len(),
        form.list_offset(PaletteControl::Commands),
        usize::from(list_area.height).max(1),
    );
    let workspace_anchor = palette.workspace_popup.as_ref().and_then(|_| {
        let line = lines.iter().position(|line| {
            matches!(line, PaletteLine::Command(index)
                if palette.entries[*index].id == CommandId::ChangeWorkspace)
        })?;
        let row = line.checked_sub(form.list_offset(PaletteControl::Commands))?;
        (row < usize::from(list_area.height))
            .then(|| (list_area.y + u16::try_from(row).unwrap_or(0), list_area.x))
    });
    if !palette.session_only {
        Dialog::render_actions(
            frame,
            rows[3],
            &[(
                PaletteControl::Run,
                "Run",
                palette
                    .entries
                    .get(palette.selected)
                    .is_some_and(|entry| entry.availability == Availability::Ready),
            )],
            &mut form,
        );
    }
    // Drawn after every other control so the list lies over the rows below.
    if let (Some(popup), Some((y, x))) = (&palette.workspace_popup, workspace_anchor) {
        let row = Rect::new(x + 2, y, list_area.width.saturating_sub(2), 1);
        popup.render(frame, area, row, palette.session_only, &mut form);
    }
    form.end_frame(if palette.session_only {
        PaletteControl::Commands
    } else {
        PaletteControl::Query
    });

    let description = palette.entries.get(palette.selected).map_or(
        "Try a command name or a word from its description.",
        |entry| command_description(dashboard, entry.id),
    );
    frame.render_widget(
        Paragraph::new(description).style(theme::muted()),
        description_row,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionOperationKind;
    use crate::render::render;
    use crate::test_support::{
        buffer_lines, dashboard_with_finished_native_child, dashboard_with_session, drawn, key,
        mouse_at, open_palette, operation, point, running_session, stopped_session,
    };
    use crossterm::event::{MouseButton, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn type_query(dashboard: &mut DashboardState, query: &str) {
        for character in query.chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
    }

    /// The row of a drawn palette, or `None` when the text is not on screen.
    fn row_of(lines: &[String], needle: &str) -> Option<usize> {
        lines.iter().position(|line| line.contains(needle))
    }

    fn selected_command(dashboard: &DashboardState) -> Option<CommandId> {
        let Mode::Palette(palette) = &dashboard.mode else {
            return None;
        };
        palette.entries.get(palette.selected).map(|entry| entry.id)
    }

    fn append_golden_state(
        output: &mut String,
        label: &str,
        width: u16,
        height: u16,
        lines: &[String],
    ) {
        output.push_str(&format!("=== {label} ({width}x{height}) ===\n"));
        output.push_str(&lines.join("\n"));
        output.push('\n');
    }

    fn append_golden_value(output: &mut String, label: &str, value: impl std::fmt::Debug) {
        output.push_str(&format!("{label}: {value:?}\n"));
    }

    /// The session menu names a full disk on the session's host, with the
    /// figures the daemon's storage owner measured.
    // Hard-won: 540c9202494a: a full target disk was reported only as unreachable.
    #[test]
    fn session_facts_name_a_full_disk_on_the_sessions_host() {
        let session = crate::test_support::precision_session();
        let mut dashboard = dashboard_with_session(session.clone());
        let text = |dashboard: &DashboardState| {
            session_facts(dashboard, &session)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(!text(&dashboard).contains("Disk"));
        dashboard.set_target_storage(vec![crate::test_support::precision_storage(40 << 30, 0)]);
        let facts = text(&dashboard);
        assert!(
            facts.contains(
                "Disk       disk full: precision-3260 has 0 B free on /home/jonathan/Projects"
            ),
            "{facts}"
        );
        // A healthy filesystem the session writes to says nothing.
        assert_eq!(facts.matches("Disk ").count(), 1, "{facts}");
    }

    /// The query is a text field, so readline's Ctrl-U and Ctrl-D keep editing
    /// it while there is text to edit. The list borrows them for paging only
    /// once the query is empty, where they would otherwise do nothing.
    #[test]
    fn palette_ctrl_u_and_ctrl_d_edit_the_query_until_it_is_empty() {
        let ctrl = |character: char| KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL);
        let query = |dashboard: &DashboardState| {
            let Mode::Palette(palette) = &dashboard.mode else {
                panic!("the palette stays open");
            };
            (palette.query.value().to_owned(), palette.selected)
        };

        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        open_palette(&mut dashboard);
        type_query(&mut dashboard, "rename");
        drawn(&mut dashboard, 120, 30);
        let (_, selected) = query(&dashboard);

        // Ctrl-D deletes forward within the text rather than paging the list.
        dashboard.handle_key(key(KeyCode::Home));
        dashboard.handle_key(ctrl('d'));
        assert_eq!(query(&dashboard), ("ename".to_owned(), selected));

        // Ctrl-U kills back to the start of the line, and the selection stays
        // where the shortened query puts it rather than jumping eight rows.
        dashboard.handle_key(key(KeyCode::End));
        dashboard.handle_key(ctrl('u'));
        let (text, selected) = query(&dashboard);
        assert_eq!(text, "");
        drawn(&mut dashboard, 120, 30);

        // With nothing left to edit, the same chord pages the list.
        dashboard.handle_key(ctrl('d'));
        let (text, paged) = query(&dashboard);
        assert_eq!(text, "");
        assert_ne!(paged, selected, "ctrl+d must page an empty palette");
        dashboard.handle_key(ctrl('u'));
        assert_eq!(query(&dashboard), ("".to_owned(), selected));
    }

    /// A-3, palette half: a blocked row gives its reason as a dimmed,
    /// capitalised sentence after " — ", as help does, not as a lowercase
    /// fragment in brackets.
    // Hard-won: 7ea16bf8220e: blocked palette reasons appeared as raw lowercase bracket fragments.
    #[test]
    fn palette_rows_set_unavailability_reasons_apart_like_help() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.begin_palette();
        type_query(&mut dashboard, "unpin");
        let joined = drawn(&mut dashboard, 120, 30).join("\n");
        assert!(
            joined.contains("Unpin session — This session is not pinned."),
            "{joined}"
        );
        assert!(!joined.contains("(this session"), "{joined}");
    }

    // Hard-won: e063b7f41d77: palette command availability stayed stale after session creation finished.
    #[test]
    fn open_palette_enables_rename_when_session_creation_finishes() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_session_operation(
            "session-1".into(),
            crate::SessionOperationKind::Launching,
            None,
        );
        dashboard.begin_session_palette();
        drawn(&mut dashboard, 120, 30);
        assert_eq!(selected_command(&dashboard), Some(CommandId::PinSession));
        dashboard.finish_session_operation("session-1");
        drawn(&mut dashboard, 120, 30);
        dashboard.handle_key(key(KeyCode::Up));
        assert_eq!(selected_command(&dashboard), Some(CommandId::RenameSession));
        dashboard.handle_key(key(KeyCode::Enter));
        assert!(matches!(dashboard.mode, Mode::Rename(_)));
    }

    /// The menu opens with what the row does not show, set apart from the
    /// commands by a blank row.
    fn measured(recent: u16, hourly: u16) -> mj_client::runtime_feed::SessionCpuView {
        mj_client::runtime_feed::SessionCpuView::Measured {
            usage: mj_core::cpu_usage::SessionCpuUsage {
                recent_permille: recent,
                hourly_permille: hourly,
                hourly_covered_secs: 3600,
                online_cpus: 8,
            },
        }
    }

    /// The menu says what the row's figure is made of: the tree total, the
    /// session's own share, and how many members were measured.
    /// Copy session ID sits under the plain divider, directly above Destroy,
    /// and hands the host the session's full ID to put on the clipboard.
    /// The open session menu as its lines: a named divider as `[name]`, the
    /// unnamed divider as `---`, and each command by its menu label.
    fn session_menu_layout(dashboard: &DashboardState) -> Vec<String> {
        let Mode::Palette(palette) = &dashboard.mode else {
            panic!("the session menu is open");
        };
        palette_lines(dashboard, palette)
            .into_iter()
            .map(|line| match line {
                PaletteLine::Heading(heading) => format!("[{heading}]"),
                PaletteLine::Separator => "---".to_owned(),
                PaletteLine::Command(index) => session_menu_label(palette.entries[index].id),
            })
            .collect()
    }

    /// R10-2: a native child's menu offered Rename, Pin, Restart and Destroy,
    /// none of which a harness-owned child can take: its parent's harness
    /// owns it, and Mjolnir has no record of its own to rename or destroy.
    /// The menu offers what does apply, which is opening its conversation.
    // Hard-won: 936d6ee93c8a: native child menus offered unsupported record actions.
    #[test]
    fn a_native_childs_menu_offers_only_what_applies_to_it() {
        let (mut dashboard, parent_id, id) = dashboard_with_finished_native_child();
        dashboard.open_subagent_workspace(parent_id);
        assert_eq!(dashboard.selected_session_id(), Some(id.as_str()));
        dashboard.focus_sessions();
        dashboard.dispatch_command(CommandId::SessionActions);
        assert_eq!(session_menu_layout(&dashboard), ["[Content]", "Open"]);
        let lines = drawn(&mut dashboard, 120, 40);
        assert!(
            row_of(&lines, "Review calc · completed").is_some(),
            "the menu is titled with the child's name: {lines:#?}"
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::Open { session_id: id }
        );
        assert!(matches!(dashboard.mode, Mode::Dashboard));
    }

    /// Launch finding R11-4: a native child's Open said "Show the selected
    /// session's conversation and type in it.", but its pane is read-only
    /// ("controlled by parent"), and so is a stopped sub-agent's. Open says
    /// so for both, and keeps its description for a session you can type in.
    // Hard-won: e5b2818d4996: Open described a read-only child conversation as writable.
    #[test]
    fn open_describes_a_read_only_conversation_as_read_only() {
        let read_only = "Show the selected agent's conversation (read-only).";
        let (mut dashboard, parent_id, _) = dashboard_with_finished_native_child();
        assert_eq!(
            command_description(&dashboard, CommandId::OpenSession),
            spec(CommandId::OpenSession).description,
            "the parent itself is a session you can type in"
        );
        dashboard.open_subagent_workspace(parent_id);
        dashboard.focus_sessions();
        dashboard.dispatch_command(CommandId::SessionActions);
        let screen = drawn(&mut dashboard, 120, 40).join("\n");
        assert!(screen.contains(read_only), "{screen}");
        assert!(!screen.contains("type in it"), "{screen}");

        let (mut dashboard, parent_id) = crate::test_support::dashboard_with_one_subagent();
        let mut state = dashboard.state.clone();
        state.sessions.get_mut("child-session").unwrap().state =
            mj_core::state::SessionState::Stopped;
        dashboard.set_state(state);
        dashboard.open_subagent_workspace(parent_id);
        dashboard.focus_sessions();
        assert_eq!(dashboard.selected_session_id(), Some("child-session"));
        assert_eq!(
            command_description(&dashboard, CommandId::OpenSession),
            read_only
        );
    }

    /// The palette's whole point: the commands for the session you are looking
    /// at come first, under a heading saying which session that is.
    #[test]
    fn palette_shows_the_selected_row_and_scrolls_the_list_on_a_short_terminal() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        open_palette(&mut dashboard);

        // Focus the list and select its final command before drawing the
        // constrained viewport. The shared list renderer must reveal it and
        // publish the same offset used by the scrollbar.
        dashboard.handle_key(key(KeyCode::Tab));
        dashboard.handle_key(key(KeyCode::End));
        let mut terminal = Terminal::new(TestBackend::new(100, 18)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw the constrained palette");

        let lines = buffer_lines(terminal.backend().buffer());
        let Mode::Palette(palette) = &dashboard.mode else {
            panic!("the palette chord should leave the palette open")
        };
        assert_eq!(
            palette.selected,
            palette.entries.len().saturating_sub(1),
            "End selects the final command"
        );
        assert!(
            palette.form.borrow().list_offset(PaletteControl::Commands) > 0,
            "the selected final command requires scrolling: {lines:#?}"
        );
        assert!(
            row_of(&lines, "Help").is_some(),
            "the selected row is visible"
        );
    }

    /// From the composer the selection is the conversation on screen, so the
    /// palette still leads with that session's commands.
    /// Launch campaign finding A-6: a command run from the palette appears
    /// under Recent even when it also has a pane key, as Create session does,
    /// and even when it opens a wizard.
    // Hard-won: a93d64c805c9: palette commands with pane keys were missing from Recent.
    #[test]
    fn a_command_run_from_the_palette_is_listed_under_recent() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        assert!(!spec(CommandId::NewSessionWizard).pane_keys.is_empty());

        open_palette(&mut dashboard);
        type_query(&mut dashboard, "create session");
        assert_eq!(
            selected_command(&dashboard),
            Some(CommandId::NewSessionWizard)
        );
        dashboard.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(dashboard.mode, Mode::New(_)),
            "{:?}",
            dashboard.mode
        );
        dashboard.handle_key(key(KeyCode::Esc));

        open_palette(&mut dashboard);
        let Mode::Palette(palette) = &dashboard.mode else {
            panic!("the palette stays open");
        };
        assert_eq!(
            palette.entries[0].id,
            CommandId::NewSessionWizard,
            "{palette:?}"
        );
        assert!(palette.entries[0].recent);
    }

    /// Enter runs the row the cursor is on, so the cursor has to follow the
    /// ranking. A command run earlier leads the unfiltered list under Recent,
    /// and the cursor used to ride that command down into the results of the
    /// next search: the screen pointed at the top match while Enter ran the
    /// command from last time.
    // Hard-won: fb0622d0f159: Enter ran a stale Recent command instead of the highlighted match.
    #[test]
    fn a_new_palette_query_puts_the_cursor_on_its_top_match() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();

        open_palette(&mut dashboard);
        type_query(&mut dashboard, "workspaces");
        dashboard.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(dashboard.mode, Mode::WorkspaceManager(_)),
            "{:?}",
            dashboard.mode
        );
        dashboard.handle_key(key(KeyCode::Esc));
        assert_eq!(dashboard.mode, Mode::Dashboard);

        // The unfiltered list leads with what was just run.
        open_palette(&mut dashboard);
        let Mode::Palette(palette) = &dashboard.mode else {
            panic!("the palette stays open");
        };
        assert_eq!(palette.entries[0].id, CommandId::Workspaces, "{palette:?}");
        assert!(palette.entries[0].recent);

        // "Workspaces" carries "rename" in its description, so it still
        // matches the query — at the bottom of the results, where the cursor
        // must not follow it.
        type_query(&mut dashboard, "rename");
        let Mode::Palette(palette) = &dashboard.mode else {
            panic!("the palette stays open");
        };
        assert_eq!(
            palette.entries[0].id,
            CommandId::RenameSession,
            "{:?}",
            palette.entries
        );
        assert_eq!(
            palette.entries.get(palette.selected).map(|entry| entry.id),
            Some(CommandId::RenameSession),
            "the cursor sits on the top match: {:?}",
            palette.entries
        );
        dashboard.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(dashboard.mode, Mode::Rename(_)),
            "{:?}",
            dashboard.mode
        );
    }

    /// A run of the query inside a word is what a reader means by a match.
    /// Letters merely found in order, which any long label can supply, must
    /// not outrank it, and a label that starts with the query still wins.
    // Hard-won: fb0622d0f159: scattered letters outranked a contiguous query match.
    #[test]
    fn a_contiguous_label_match_outranks_a_scattered_one() {
        let contiguous =
            match_score("Toggle transcript rendering", "", "rend").expect("a contiguous match");
        let scattered = match_score("Resize pane down", "", "rend").expect("a scattered match");
        assert!(
            contiguous > scattered,
            "'rend' in 'rendering' beats R-e-n-d across three words: {contiguous} vs {scattered}"
        );
        let prefix = match_score("Rendering", "", "rend").expect("a prefix match");
        assert!(prefix > contiguous, "{prefix} vs {contiguous}");
    }

    /// The same ranking through the palette, on the two commands that showed
    /// the defect.
    // Hard-won: fb0622d0f159: searching “rend” ranked Resize above the literal rendering match.
    #[test]
    fn palette_ranks_toggle_rendering_above_resize_pane_down_for_rend() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        let ranked = palette_entries(&dashboard, "rend")
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        let rendering = ranked
            .iter()
            .position(|id| *id == CommandId::ToggleTranscriptRendering)
            .expect("Toggle transcript rendering");
        let resize = ranked
            .iter()
            .position(|id| *id == CommandId::ResizePaneDown)
            .expect("Resize pane down");
        assert!(rendering < resize, "{ranked:?}");
    }

    /// Container settings make no sense for a session that is not on a
    /// container, so the palette leaves the row out rather than greying it.
    /// A command that is blocked rather than meaningless stays visible and
    /// says why, and pressing Enter on it explains instead of acting.
    #[test]
    fn palette_greys_stop_while_an_operation_is_in_flight_and_explains_why() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.session_operations.insert(
            "session-1".into(),
            operation(SessionOperationKind::Launching, None),
        );

        let entries = palette_entries(&dashboard, "rename");
        assert_eq!(entries[0].id, CommandId::RenameSession);
        assert_eq!(
            entries[0].availability,
            Availability::Blocked("a session transition is in progress")
        );

        open_palette(&mut dashboard);
        type_query(&mut dashboard, "rename");
        let lines = drawn(&mut dashboard, 120, 44).join("\n");
        assert!(
            lines.contains("Rename session — A session transition is in progress."),
            "{lines}"
        );

        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(
            matches!(dashboard.mode, Mode::Palette(_)),
            "the palette stays open"
        );
        assert_eq!(
            dashboard.notice().as_deref(),
            Some("Rename session is unavailable: a session transition is in progress.")
        );
    }

    #[test]
    fn golden_session_action_menu() {
        let mut output = String::new();

        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.begin_session_palette();
        let lines = drawn(&mut dashboard, 120, 40);
        append_golden_state(&mut output, "session action menu", 120, 40, &lines);

        let mut session = running_session();
        session.subagents = Some(SubagentPolicy::SingleModel {
            model: "gpt-5".into(),
            effort: Some(mj_core::subagent::ADAPTIVE_EFFORT.into()),
        });
        session.container_cpus = Some("4".into());
        session.container_memory = Some("8g".into());
        let mut dashboard = dashboard_with_session(session);
        dashboard.focus_sessions();
        dashboard.begin_session_palette();
        let lines = drawn(&mut dashboard, 120, 40);
        append_golden_state(&mut output, "session facts", 120, 40, &lines);

        let (mut dashboard, parent) = crate::test_support::dashboard_with_one_subagent();
        let mut cpu = mj_core::snapshot_map::SnapshotMap::new();
        cpu.insert(parent, measured(30, 20));
        dashboard.set_session_cpu(cpu.clone());
        dashboard.focus_sessions();
        dashboard.begin_session_palette();
        let lines = drawn(&mut dashboard, 120, 40);
        append_golden_state(&mut output, "sub-agent CPU coverage", 120, 40, &lines);

        let mut dashboard = dashboard_with_session(running_session());
        cpu.insert(running_session().id, measured(230, 100));
        dashboard.set_session_cpu(cpu);
        dashboard.focus_sessions();
        dashboard.begin_session_palette();
        let lines = drawn(&mut dashboard, 120, 40);
        append_golden_state(&mut output, "single-session CPU", 120, 40, &lines);

        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.begin_session_palette();
        let lines = drawn(&mut dashboard, 120, 40);
        append_golden_state(&mut output, "CPU not measured", 120, 40, &lines);

        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.begin_session_palette();
        let lines = drawn(&mut dashboard, 120, 40);
        let copy = point(&lines, "Copy session ID");
        append_golden_state(&mut output, "copy session ID menu row", 120, 40, &lines);
        dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), copy));
        let action = dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), copy));
        append_golden_value(&mut output, "action", action);
        let lines = drawn(&mut dashboard, 120, 40);
        append_golden_state(&mut output, "after copying the session ID", 120, 40, &lines);

        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.begin_session_palette();
        let lines = drawn(&mut dashboard, 120, 40);
        let unpin = point(&lines, "Unpin");
        dashboard.handle_key(key(KeyCode::Down));
        dashboard.handle_key(key(KeyCode::Down));
        append_golden_value(
            &mut output,
            "keyboard selection after skipping disabled Unpin",
            selected_command(&dashboard),
        );
        let down = dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), unpin));
        append_golden_value(&mut output, "disabled Unpin mouse-down action", down);
        let up = dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), unpin));
        append_golden_value(&mut output, "disabled Unpin mouse-up action", up);
        append_golden_value(
            &mut output,
            "selection after disabled click",
            selected_command(&dashboard),
        );
        let lines = drawn(&mut dashboard, 120, 40);
        append_golden_state(&mut output, "disabled Unpin remains open", 120, 40, &lines);

        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "session-action-menu", &output);
    }

    #[test]
    fn golden_command_palette() {
        let mut output = String::new();

        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        open_palette(&mut dashboard);
        let lines = drawn(&mut dashboard, 120, 100);
        append_golden_state(&mut output, "F2 command palette", 120, 100, &lines);

        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.handle_key(key(KeyCode::Enter));
        open_palette(&mut dashboard);
        let lines = drawn(&mut dashboard, 120, 90);
        append_golden_state(
            &mut output,
            "palette opened from the composer",
            120,
            90,
            &lines,
        );

        for (label, query) in [
            ("label prefix match", "suspend"),
            ("description match", "unread marker"),
        ] {
            let mut dashboard = dashboard_with_session(running_session());
            dashboard.focus_sessions();
            open_palette(&mut dashboard);
            type_query(&mut dashboard, query);
            let lines = drawn(&mut dashboard, 120, 44);
            append_golden_state(&mut output, label, 120, 44, &lines);
        }

        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        open_palette(&mut dashboard);
        type_query(&mut dashboard, "open settings");
        let lines = drawn(&mut dashboard, 120, 30);
        append_golden_state(&mut output, "search for Open settings", 120, 30, &lines);
        let action = dashboard.handle_key(key(KeyCode::Enter));
        append_golden_value(&mut output, "action after selecting Open settings", action);
        append_golden_value(&mut output, "mode after selecting Open settings", "Setup");
        let lines = drawn(&mut dashboard, 120, 30);
        append_golden_state(
            &mut output,
            "Setup opened from the palette",
            120,
            30,
            &lines,
        );

        let mut session = stopped_session();
        session.target_template_id = "local".into();
        let mut dashboard = dashboard_with_session(session);
        dashboard
            .config
            .targets
            .insert("local".into(), mj_core::config::TargetTemplate::LocalBare);
        dashboard.focus_sessions();
        open_palette(&mut dashboard);
        let lines = drawn(&mut dashboard, 120, 44);
        append_golden_state(
            &mut output,
            "non-container session commands",
            120,
            44,
            &lines,
        );

        let mut session = running_session();
        session.project_directory = Some("/srv/project".into());
        let mut dashboard = dashboard_with_session(session);
        dashboard.focus_sessions();
        open_palette(&mut dashboard);
        type_query(&mut dashboard, "suspend");
        let lines = drawn(&mut dashboard, 120, 44);
        append_golden_state(&mut output, "search for Suspend", 120, 44, &lines);
        let action = dashboard.handle_key(key(KeyCode::Enter));
        append_golden_value(&mut output, "suspend action", action);
        let lines = drawn(&mut dashboard, 120, 44);
        append_golden_state(&mut output, "after Suspend", 120, 44, &lines);

        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        open_palette(&mut dashboard);
        type_query(&mut dashboard, "stop");
        let action = dashboard.handle_key(key(KeyCode::Esc));
        append_golden_value(&mut output, "Escape action", action);
        append_golden_value(&mut output, "notice after Escape", dashboard.notice());
        append_golden_value(
            &mut output,
            "command session id after Escape",
            dashboard
                .command_session()
                .map(|session| session.id.as_str()),
        );
        let lines = drawn(&mut dashboard, 120, 44);
        append_golden_state(&mut output, "dashboard after Escape", 120, 44, &lines);

        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "command-palette", &output);
    }
}
