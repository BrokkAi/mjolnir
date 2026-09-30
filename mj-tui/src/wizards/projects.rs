//! Project discovery state and controls. All filesystem and network work is
//! requested as dashboard actions; this module only edits the draft.
use super::*;
use mj_core::project_picker::{
    ProjectDiscovery, ProjectDiscoveryRequest, ProjectEntry, ProjectEntryKind,
};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ProjectTab {
    #[default]
    Recent,
    Github,
    Folders,
    Url,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectPicker {
    pub tab: ProjectTab,
    pub query: PathInput,
    pub folder_filter: PathInput,
    pub entries: Vec<ProjectEntry>,
    pub selected: usize,
    pub directory: Option<String>,
    pub parent: Option<String>,
    pub truncated: bool,
    pub error: Option<String>,
    pub creation_error: Option<String>,
    pub multiple: bool,
    pub loading: bool,
    pub request: Option<ProjectDiscoveryRequest>,
    pub pending: bool,
    pub focus_results: bool,
    id: u64,
    generation: u64,
}

impl Default for ProjectPicker {
    fn default() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            tab: ProjectTab::Recent,
            query: PathInput::new(),
            folder_filter: PathInput::new(),
            entries: Vec::new(),
            selected: 0,
            directory: None,
            parent: None,
            truncated: false,
            error: None,
            creation_error: None,
            multiple: false,
            loading: false,
            request: None,
            pending: false,
            focus_results: false,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            generation: 0,
        }
    }
}

impl ProjectPicker {
    pub(super) fn context(&self) -> String {
        format!("{}:{}", self.id, self.generation)
    }

    fn load(&mut self, request: ProjectDiscoveryRequest) {
        self.generation = self.generation.wrapping_add(1);
        self.request = Some(request);
        self.pending = true;
        self.focus_results = true;
        self.loading = true;
        self.error = None;
        self.entries.clear();
        self.selected = 0;
        self.truncated = false;
    }
}

impl NewWizard {
    pub(super) fn open_projects(&mut self, dashboard: &DashboardState) {
        self.step = WizardStep::NewBundle;
        self.choose_project_tab(dashboard, ProjectTab::Recent);
    }

    pub(super) fn choose_project_tab(&mut self, dashboard: &DashboardState, tab: ProjectTab) {
        self.project_picker.tab = tab;
        self.project_picker.generation = self.project_picker.generation.wrapping_add(1);
        self.project_picker.loading = false;
        self.project_picker.pending = false;
        self.project_picker.error = None;
        self.project_picker.creation_error = None;
        self.project_picker.entries.clear();
        self.project_picker.selected = 0;
        self.project_picker.truncated = false;
        match tab {
            ProjectTab::Recent => {
                let mut entries = Vec::new();
                for path in dashboard
                    .launch_project_directory
                    .iter()
                    .chain(dashboard.state.project_directories("local").iter())
                {
                    let source = path.to_string_lossy().into_owned();
                    if entries
                        .iter()
                        .any(|entry: &ProjectEntry| entry.source == source)
                    {
                        continue;
                    }
                    let current = dashboard.launch_project_directory.as_ref() == Some(path);
                    entries.push(ProjectEntry {
                        name: path
                            .file_name()
                            .unwrap_or(path.as_os_str())
                            .to_string_lossy()
                            .into_owned(),
                        source: source.clone(),
                        description: if current {
                            format!("Current project · {source}")
                        } else {
                            source
                        },
                        kind: ProjectEntryKind::Repository,
                    });
                }
                self.project_picker.entries = entries;
            }
            ProjectTab::Github => self.project_picker.load(ProjectDiscoveryRequest::Github {
                query: self.project_picker.query.trim().to_owned(),
            }),
            ProjectTab::Folders => self
                .project_picker
                .load(ProjectDiscoveryRequest::Directory {
                    path: self.project_picker.directory.clone().unwrap_or_default(),
                    filter: self.project_picker.folder_filter.trim().to_owned(),
                }),
            ProjectTab::Url => {}
        }
        if tab == ProjectTab::Github {
            self.project_picker.focus_results = false;
        }
        let initial = match tab {
            ProjectTab::Url => WizardControl::NewBundleSource,
            ProjectTab::Github => WizardControl::ProjectQuery,
            _ if !self.project_picker.entries.is_empty() => WizardControl::ProjectResults,
            _ => WizardControl::ProjectGithub,
        };
        let mut form = self.form.borrow_mut();
        form.begin_update();
        self.declare_project_controls(&mut form);
        form.end_frame(initial);
        form.focus(initial);
    }

    pub(super) fn declare_project_controls(&self, form: &mut Dialog<WizardControl>) {
        let ready = !self.bundle_creation_in_flight;
        for id in [
            WizardControl::ProjectRecent,
            WizardControl::ProjectGithub,
            WizardControl::ProjectFolders,
            WizardControl::ProjectUrl,
        ] {
            form.declare_with_enabled(id, ControlKind::Button, ready);
        }
        if matches!(
            self.project_picker.tab,
            ProjectTab::Github | ProjectTab::Folders
        ) {
            form.declare_with_enabled(WizardControl::ProjectQuery, ControlKind::TextField, ready);
            if self.project_picker.tab == ProjectTab::Github {
                form.declare_with_enabled(WizardControl::ProjectSearch, ControlKind::Button, ready);
            }
        }
        if self.project_picker.tab == ProjectTab::Folders {
            form.declare_with_enabled(
                WizardControl::ProjectUp,
                ControlKind::Button,
                ready && !self.project_picker.loading && self.project_picker.parent.is_some(),
            );
            form.declare_with_enabled(WizardControl::ProjectHome, ControlKind::Button, ready);
            form.declare_with_enabled(
                WizardControl::ProjectOpenFolder,
                ControlKind::Button,
                ready && !self.project_picker.loading && !self.project_picker.entries.is_empty(),
            );
        }
        if self.project_picker.error.is_some() || self.project_picker.creation_error.is_some() {
            form.declare_with_enabled(WizardControl::ProjectRetry, ControlKind::Button, ready);
        }
        if self.project_picker.tab == ProjectTab::Url {
            form.declare_with_enabled(
                WizardControl::NewBundleSource,
                self.new_bundle_source.control_kind(),
                ready,
            );
        } else {
            form.declare_with_enabled(
                WizardControl::ProjectResults,
                ControlKind::ChoiceList {
                    len: self.project_picker.entries.len(),
                    selected: self.project_picker.selected,
                },
                ready && !self.project_picker.loading && !self.project_picker.entries.is_empty(),
            );
        }
        form.declare_with_enabled(WizardControl::ProjectMultiple, ControlKind::Checkbox, ready);
        if self.project_picker.multiple {
            form.declare_with_enabled(
                WizardControl::NewBundleRepositories,
                ControlKind::ChoiceList {
                    len: self.new_bundle_repositories.len(),
                    selected: self.new_bundle_selected,
                },
                ready && !self.new_bundle_repositories.is_empty(),
            );
            form.declare_with_enabled(
                WizardControl::ProjectMakePrimary,
                ControlKind::Button,
                ready && self.new_bundle_repositories.len() > 1,
            );
            form.declare_with_enabled(
                WizardControl::NewBundleRemove,
                ControlKind::Button,
                ready && !self.new_bundle_repositories.is_empty(),
            );
            if self.project_picker.tab == ProjectTab::Url {
                form.declare_with_enabled(
                    WizardControl::Add,
                    ControlKind::Button,
                    ready && !self.new_bundle_source.trim().is_empty(),
                );
            }
        }
        form.declare_with_enabled(WizardControl::Back, ControlKind::Button, ready);
        form.declare_with_enabled(WizardControl::Cancel, ControlKind::Button, ready);
        let can_use = !self.new_bundle_repositories.is_empty()
            || (self.project_picker.tab == ProjectTab::Url
                && !self.new_bundle_source.trim().is_empty())
            || (!self.project_picker.multiple && !self.project_picker.entries.is_empty());
        form.declare_with_enabled(WizardControl::Next, ControlKind::Button, ready && can_use);
        form.set_default_action(WizardControl::Next);
    }
}

impl DashboardState {
    pub fn take_project_discovery(&mut self) -> Option<DashboardAction> {
        let Mode::New(wizard) = &mut self.mode else {
            return None;
        };
        if wizard.step != WizardStep::NewBundle || !wizard.project_picker.pending {
            return None;
        }
        wizard.project_picker.pending = false;
        Some(DashboardAction::DiscoverProjects {
            context: wizard.project_picker.context(),
            request: wizard.project_picker.request.clone()?,
        })
    }

    pub fn apply_project_discovery(
        &mut self,
        context: &str,
        result: Result<ProjectDiscovery, String>,
    ) {
        let Mode::New(wizard) = &mut self.mode else {
            return;
        };
        if wizard.step != WizardStep::NewBundle || wizard.project_picker.context() != context {
            return;
        }
        let picker = &mut wizard.project_picker;
        picker.loading = false;
        match result {
            Ok(result) => {
                picker.entries = result.entries;
                if picker.tab == ProjectTab::Folders {
                    picker.directory = result.directory;
                    picker.parent = result.parent;
                }
                picker.truncated = result.truncated;
                picker.selected = 0;
                picker.error = None;
                // A late result must not steal focus from an edited search.
                if picker.focus_results && !picker.entries.is_empty() {
                    wizard.form.get_mut().focus(WizardControl::ProjectResults);
                }
                picker.focus_results = false;
            }
            Err(error) => picker.error = Some(error),
        }
    }

    pub(super) fn activate_project_control(
        &mut self,
        mut wizard: NewWizard,
        id: WizardControl,
    ) -> DashboardAction {
        match id {
            WizardControl::ProjectRecent => wizard.choose_project_tab(self, ProjectTab::Recent),
            WizardControl::ProjectGithub => wizard.choose_project_tab(self, ProjectTab::Github),
            WizardControl::ProjectFolders => wizard.choose_project_tab(self, ProjectTab::Folders),
            WizardControl::ProjectUrl => wizard.choose_project_tab(self, ProjectTab::Url),
            WizardControl::ProjectQuery | WizardControl::ProjectSearch => {
                let request = if wizard.project_picker.tab == ProjectTab::Folders {
                    ProjectDiscoveryRequest::Directory {
                        path: wizard.project_picker.directory.clone().unwrap_or_default(),
                        filter: wizard.project_picker.folder_filter.trim().to_owned(),
                    }
                } else {
                    ProjectDiscoveryRequest::Github {
                        query: wizard.project_picker.query.trim().to_owned(),
                    }
                };
                wizard.project_picker.load(request);
                wizard.form.get_mut().focus(WizardControl::ProjectResults);
            }
            WizardControl::ProjectHome => {
                wizard.project_picker.folder_filter.clear();
                wizard
                    .project_picker
                    .load(ProjectDiscoveryRequest::Directory {
                        path: String::new(),
                        filter: String::new(),
                    });
            }
            WizardControl::ProjectUp => {
                if let Some(path) = wizard.project_picker.parent.clone() {
                    wizard.project_picker.folder_filter.clear();
                    wizard
                        .project_picker
                        .load(ProjectDiscoveryRequest::Directory {
                            path,
                            filter: String::new(),
                        });
                }
            }
            WizardControl::ProjectOpenFolder => {
                if let Some(entry) = wizard
                    .project_picker
                    .entries
                    .get(wizard.project_picker.selected)
                    .cloned()
                {
                    wizard.project_picker.folder_filter.clear();
                    wizard
                        .project_picker
                        .load(ProjectDiscoveryRequest::Directory {
                            path: entry.source,
                            filter: String::new(),
                        });
                }
            }
            WizardControl::ProjectMakePrimary => {
                if wizard.new_bundle_selected < wizard.new_bundle_repositories.len() {
                    let source = wizard
                        .new_bundle_repositories
                        .remove(wizard.new_bundle_selected);
                    wizard.new_bundle_repositories.insert(0, source);
                    wizard.new_bundle_selected = 0;
                }
            }
            WizardControl::ProjectRetry => {
                if wizard.project_picker.creation_error.is_some() {
                    return self.submit_new_bundle(wizard);
                }
                if let Some(request) = wizard.project_picker.request.clone() {
                    wizard.project_picker.load(request);
                }
            }
            WizardControl::ProjectResults | WizardControl::Next => {
                if id == WizardControl::Next && !wizard.new_bundle_repositories.is_empty() {
                    return self.submit_new_bundle(wizard);
                }
                if wizard.project_picker.tab == ProjectTab::Url {
                    return self.submit_new_bundle(wizard);
                }
                if let Some(entry) = wizard
                    .project_picker
                    .entries
                    .get(wizard.project_picker.selected)
                    .cloned()
                {
                    if entry.kind == ProjectEntryKind::Directory {
                        wizard.project_picker.folder_filter.clear();
                        wizard
                            .project_picker
                            .load(ProjectDiscoveryRequest::Directory {
                                path: entry.source,
                                filter: String::new(),
                            });
                    } else if wizard.project_picker.multiple {
                        if let Some(index) = wizard
                            .new_bundle_repositories
                            .iter()
                            .position(|source| source == &entry.source)
                        {
                            wizard.new_bundle_repositories.remove(index);
                        } else {
                            wizard.new_bundle_repositories.push(entry.source);
                        }
                        wizard.new_bundle_selected =
                            wizard.new_bundle_repositories.len().saturating_sub(1);
                    } else {
                        wizard.new_bundle_source.set_value(&entry.source);
                        return self.submit_new_bundle(wizard);
                    }
                }
            }
            _ => {}
        }
        self.mode = Mode::New(wizard);
        DashboardAction::None
    }
}

pub(super) fn render_project_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    wizard: &NewWizard,
    form: &mut Dialog<WizardControl>,
    surfaces: &mut FrameSurfaces,
) {
    let picker = &wizard.project_picker;
    let popup = centered_modal(frame, surfaces, 88, 32, area);
    let inner = DialogShell::padded_inner(popup);
    let layout = DialogShell::layout(inner, 1);
    let ready = !wizard.bundle_creation_in_flight;
    let title = dismissible_modal_title(form, popup, "Choose a project", theme::title(true), ready);
    frame.render_widget(theme::modal().title(title), popup);
    let description = wrap_lines(
        [Line::styled(
            match picker.tab {
                ProjectTab::Recent => {
                    "Pick up a recent project, or find another on GitHub or in Folders."
                }
                ProjectTab::Github => {
                    "Your GitHub repositories. Search by name or owner/repository."
                }
                ProjectTab::Folders => "Browse folders on this computer.",
                ProjectTab::Url => {
                    "Paste a GitHub URL, owner/repository, or local repository path."
                }
            },
            theme::muted(),
        )],
        inner.width,
    );
    let directory = wrap_lines(
        [Line::raw(picker.directory.as_deref().unwrap_or("Home"))],
        inner.width,
    );
    let input_row = description.len() as u16 + 3;
    let folder_actions_row = input_row + directory.len() as u16 + 1;
    let filter_row = folder_actions_row + 2;
    let list_row = match picker.tab {
        ProjectTab::Recent => input_row,
        ProjectTab::Github => input_row + 4,
        ProjectTab::Folders => filter_row + 2,
        ProjectTab::Url => input_row + 2,
    };
    let error = picker.creation_error.as_ref().or(picker.error.as_ref());
    let message = if picker.loading {
        Some("Loading projects…")
    } else if let Some(error) = error {
        Some(error.as_str())
    } else if picker.entries.is_empty() && picker.tab != ProjectTab::Url {
        Some(match picker.tab {
            ProjectTab::Recent => {
                "No recent projects yet. Choose GitHub or Folders above to get started."
            }
            ProjectTab::Github => {
                "No matching repositories. Try another search or paste a repository URL."
            }
            _ => "No project folders here. Go up one folder or return Home.",
        })
    } else {
        None
    };
    let message = wrap_lines(message.into_iter().map(Line::raw), inner.width);
    let list_height = if !message.is_empty() {
        message.len() as u16
    } else if picker.tab == ProjectTab::Url {
        0
    } else {
        picker.entries.len().clamp(1, 5) as u16
    };
    let retry_row = list_row + list_height + 1;
    let status = if wizard.bundle_creation_in_flight {
        Some("Preparing project…")
    } else if picker.truncated {
        Some("Showing the first matches. Search or browse further to narrow the list.")
    } else {
        None
    };
    let status = wrap_lines(
        status
            .into_iter()
            .map(|text| Line::styled(text, theme::muted())),
        inner.width,
    );
    let status_row = if error.is_some() {
        retry_row + 2
    } else {
        list_row + list_height + u16::from(list_height > 0)
    };
    let multiple_row = status_row
        + if status.is_empty() {
            0
        } else {
            status.len() as u16 + 1
        };
    let basket_row = multiple_row + 2;
    let basket_height = wizard.new_bundle_repositories.len().min(3) as u16;
    let repo_actions_row = basket_row + basket_height + u16::from(basket_height > 0);
    let height = if picker.multiple {
        repo_actions_row + 1
    } else {
        multiple_row + 1
    };
    let focused_row = match form.focused() {
        Some(
            WizardControl::ProjectRecent
            | WizardControl::ProjectGithub
            | WizardControl::ProjectFolders
            | WizardControl::ProjectUrl,
        ) => Some(0),
        Some(WizardControl::ProjectQuery) => Some(if picker.tab == ProjectTab::Folders {
            filter_row
        } else {
            input_row
        }),
        Some(WizardControl::NewBundleSource) => Some(input_row),
        Some(WizardControl::ProjectSearch) => Some(input_row + 2),
        Some(
            WizardControl::ProjectUp
            | WizardControl::ProjectHome
            | WizardControl::ProjectOpenFolder,
        ) => Some(folder_actions_row),
        Some(WizardControl::ProjectResults) => Some(list_row + list_height.saturating_sub(1)),
        Some(WizardControl::ProjectRetry) => Some(retry_row),
        Some(WizardControl::ProjectMultiple) => Some(multiple_row),
        Some(WizardControl::NewBundleRepositories) => {
            Some(basket_row + basket_height.saturating_sub(1))
        }
        Some(
            WizardControl::Add | WizardControl::ProjectMakePrimary | WizardControl::NewBundleRemove,
        ) => Some(repo_actions_row),
        _ => None,
    };
    let viewport = FormViewport::new(layout.body, height, 0, focused_row);
    let row = |start| viewport.row(start, 1);
    let tabs = [
        (WizardControl::ProjectRecent, "Recent", ProjectTab::Recent),
        (WizardControl::ProjectGithub, "GitHub", ProjectTab::Github),
        (
            WizardControl::ProjectFolders,
            "Folders",
            ProjectTab::Folders,
        ),
        (WizardControl::ProjectUrl, "Paste URL", ProjectTab::Url),
    ];
    let labels: Vec<String> = tabs
        .iter()
        .map(|(_, label, tab)| {
            if *tab == picker.tab {
                format!("{}{label}", if theme::ascii() { "*" } else { "●" })
            } else {
                (*label).into()
            }
        })
        .collect();
    let actions: Vec<_> = tabs
        .iter()
        .zip(&labels)
        .map(|((id, _, _), label)| (*id, label.as_str(), ready))
        .collect();
    Dialog::render_actions(frame, row(0), &actions, form);
    // Render wrapped text one row at a time so partial viewport clipping keeps
    // the text and control geometry on the same rows.
    for (index, line) in description.into_iter().enumerate() {
        frame.render_widget(Paragraph::new(line), row(2 + index as u16));
    }
    match picker.tab {
        ProjectTab::Github => {
            PathField::render_within(
                frame,
                layout.body,
                row(input_row),
                &picker.query,
                form,
                WizardControl::ProjectQuery,
            );
            Dialog::render_actions(
                frame,
                row(input_row + 2),
                &[(WizardControl::ProjectSearch, "Search", ready)],
                form,
            );
        }
        ProjectTab::Folders => {
            for (index, line) in directory.into_iter().enumerate() {
                frame.render_widget(Paragraph::new(line), row(input_row + index as u16));
            }
            Dialog::render_actions(
                frame,
                row(folder_actions_row),
                &[
                    (
                        WizardControl::ProjectOpenFolder,
                        "Open folder",
                        ready && !picker.loading && !picker.entries.is_empty(),
                    ),
                    (
                        WizardControl::ProjectUp,
                        "Up one folder",
                        ready && !picker.loading && picker.parent.is_some(),
                    ),
                    (WizardControl::ProjectHome, "Home", ready),
                ],
                form,
            );
            let filter = row(filter_row);
            let label_width = 8.min(filter.width);
            frame.render_widget(
                Paragraph::new("Filter:"),
                Rect::new(filter.x, filter.y, label_width, filter.height),
            );
            PathField::render_within(
                frame,
                layout.body,
                Rect::new(
                    filter.x + label_width,
                    filter.y,
                    filter.width - label_width,
                    filter.height,
                ),
                &picker.folder_filter,
                form,
                WizardControl::ProjectQuery,
            );
        }
        ProjectTab::Url => PathField::render_within(
            frame,
            layout.body,
            row(input_row),
            &wizard.new_bundle_source,
            form,
            WizardControl::NewBundleSource,
        ),
        ProjectTab::Recent => {}
    }
    let list_area = viewport.row(list_row, list_height);
    if !message.is_empty() {
        for (index, line) in message.into_iter().enumerate() {
            frame.render_widget(Paragraph::new(line), row(list_row + index as u16));
        }
    } else if picker.tab != ProjectTab::Url {
        let rows: Vec<Line<'_>> = picker
            .entries
            .iter()
            .map(|entry| {
                let marker = if entry.kind == ProjectEntryKind::Directory {
                    if theme::ascii() { ">" } else { "▸" }
                } else if wizard.new_bundle_repositories.contains(&entry.source) {
                    if theme::ascii() { "+" } else { "✓" }
                } else {
                    " "
                };
                Line::from(vec![
                    Span::raw(format!("{marker} {}  ", entry.name)),
                    Span::styled(&entry.description, theme::muted()),
                ])
            })
            .collect();
        ChoiceList::render(
            frame,
            list_area,
            &rows,
            picker.selected,
            form,
            WizardControl::ProjectResults,
        );
    }
    if error.is_some() {
        Dialog::render_actions(
            frame,
            row(retry_row),
            &[(WizardControl::ProjectRetry, "Retry", ready)],
            form,
        );
    }
    for (index, line) in status.into_iter().enumerate() {
        frame.render_widget(Paragraph::new(line), row(status_row + index as u16));
    }
    Checkbox::render(
        frame,
        row(multiple_row),
        "Use several repositories together",
        picker.multiple,
        ready,
        form,
        WizardControl::ProjectMultiple,
    );
    if picker.multiple {
        let rows: Vec<_> = wizard
            .new_bundle_repositories
            .iter()
            .enumerate()
            .map(|(i, source)| {
                Line::raw(format!(
                    "{}  {source}",
                    if i == 0 { "Main" } else { "Also" }
                ))
            })
            .collect();
        ChoiceList::render(
            frame,
            viewport.row(basket_row, basket_height),
            &rows,
            wizard.new_bundle_selected,
            form,
            WizardControl::NewBundleRepositories,
        );
        Dialog::render_actions(
            frame,
            row(repo_actions_row),
            &[
                (
                    WizardControl::Add,
                    "Add repository",
                    ready
                        && picker.tab == ProjectTab::Url
                        && !wizard.new_bundle_source.trim().is_empty(),
                ),
                (
                    WizardControl::ProjectMakePrimary,
                    "Make main",
                    ready && wizard.new_bundle_repositories.len() > 1,
                ),
                (
                    WizardControl::NewBundleRemove,
                    "Remove",
                    ready && !wizard.new_bundle_repositories.is_empty(),
                ),
            ],
            form,
        );
    }
    let use_label = if wizard.bundle_creation_in_flight {
        "Preparing…".into()
    } else if picker.multiple && !wizard.new_bundle_repositories.is_empty() {
        format!(
            "Use {}",
            crate::widgets::counted(
                wizard.new_bundle_repositories.len(),
                "repository",
                "repositories"
            )
        )
    } else {
        "Use project".into()
    };
    Dialog::render_actions(
        frame,
        layout.actions,
        &[
            (WizardControl::Back, "Back", ready),
            (WizardControl::Cancel, "Cancel", ready),
            (
                WizardControl::Next,
                &use_label,
                ready
                    && (!wizard.new_bundle_repositories.is_empty()
                        || (picker.tab == ProjectTab::Url
                            && !wizard.new_bundle_source.trim().is_empty())
                        || (!picker.multiple && !picker.entries.is_empty())),
            ),
        ],
        form,
    );
    wizard.declare_project_controls(form);
}
