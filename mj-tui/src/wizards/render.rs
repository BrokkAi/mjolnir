use super::*;

/// Whether the target step sets a size for `target`: containers get CPU and
/// memory, EC2 an instance type. Other targets run with what the host has.
pub(crate) fn target_is_sized(target: &TargetTemplate) -> bool {
    mj_core::config::is_container_target(target) || matches!(target, TargetTemplate::AwsEc2 { .. })
}

/// The target step's table and the help lines under it, shared by New session
/// and Resume/Move so their rows match. A row carries a short status; the full
/// reason a target is unavailable goes under the table, where it wraps
/// instead of being cut off.
fn target_step_choices<W: WizardDraft>(
    dashboard: &DashboardState,
    wizard: &W,
    sizing_error: Option<&str>,
) -> (Vec<PickerChoice>, Vec<Line<'static>>, usize) {
    let warning = Style::default().fg(theme::palette().warning);
    let mut help = Vec::new();
    // A target whose runtime is not on this host is not listed at all; one
    // whose host did not answer is, with its status.
    let offered = dashboard.offered_target_indices();
    let selected_row = dashboard.target_row(wizard.target());
    let rows = dashboard
        .config
        .targets
        .iter()
        .enumerate()
        .filter(|(index, _)| offered.contains(index))
        .map(|(index, (id, target))| {
            let selected = index == wizard.target();
            let rejection = wizard.target_rejection(dashboard, id);
            let checking = rejection
                .as_deref()
                .is_some_and(|reason| reason.starts_with("checking"));
            let (cpu, memory) = match wizard.resource_allocation() {
                Some(allocation) if selected && target_is_sized(target) => (
                    allocation_cpus(allocation).to_string(),
                    memory_gib_text(allocation_memory(allocation)),
                ),
                _ => ("—".into(), "—".into()),
            };
            let status = match &rejection {
                None if selected && sizing_error.is_some() => {
                    PickerCell::styled("sizing failed", warning)
                }
                None => PickerCell::blank(),
                Some(_) if checking => PickerCell::styled("checking…", theme::muted()),
                Some(_) => PickerCell::styled("unavailable", warning),
            };
            // Unavailable rows cannot be selected, so each one's reason is
            // listed under the table rather than only the selected row's.
            if let Some(reason) = rejection.as_deref().filter(|_| !checking) {
                let reason = reason.strip_prefix("unavailable: ").unwrap_or(reason);
                help.push(Line::styled(format!("{id}: {reason}"), warning));
            }
            if selected {
                if mj_core::config::is_container_target(target)
                    && dashboard.host_limits(id).is_none()
                {
                    help.push(Line::styled(
                        "Host totals unavailable; resource limits cannot be checked.",
                        warning,
                    ));
                }
                if let Some(error) = sizing_error.filter(|_| rejection.is_none()) {
                    help.push(Line::styled(format!("Sizing: {error}"), warning));
                }
            }
            let row = PickerChoice::table(vec![
                PickerCell::blank(),
                PickerCell::text(id.as_str()),
                PickerCell::text(target_label(target)),
                PickerCell::text(cpu),
                PickerCell::text(memory),
                status,
            ]);
            if rejection.is_some() {
                row.into_disabled()
            } else {
                row
            }
        })
        .collect();
    if let Some(key) = dashboard.first_key_label(crate::CommandId::Refresh) {
        help.push(picker_help(&format!("{key} recheck availability")));
    }
    (target_table(rows), help, selected_row)
}

pub(crate) fn step_initial(step: WizardStep) -> WizardControl {
    match step {
        WizardStep::Profile => WizardControl::ProfileList,
        WizardStep::Target => WizardControl::TargetList,
        WizardStep::Bundle => WizardControl::BundleList,
        WizardStep::ProjectDirectory => WizardControl::ProjectDirectory,
        WizardStep::NewBundle => WizardControl::ProjectResults,
        WizardStep::Mounts => WizardControl::MountSource,
        WizardStep::Review | WizardStep::Launching => WizardControl::Submit,
        WizardStep::MoveFiles => WizardControl::Next,
    }
}

/// Whether the wizard's project is the repository `mj` was started in, which
/// the project step fills in without being asked.
fn launch_directory_chosen(dashboard: &DashboardState, wizard: &NewWizard) -> bool {
    dashboard
        .launch_project_directory
        .as_deref()
        .is_some_and(|launch| std::path::Path::new(wizard.project_directory.trim()) == launch)
}

/// Whether the wizard's title leaves the target step out of its step
/// numbers. On the profile step this is a forecast of whether Next there
/// would pass the target step.
fn target_step_hidden<W: WizardDraft>(dashboard: &DashboardState, wizard: &W) -> bool {
    match wizard.step() {
        WizardStep::Profile => dashboard.lone_target(wizard).is_some(),
        _ => wizard.target_step_skipped(),
    }
}

/// A step's place in its wizard's title, such as `2/4`. `position` and
/// `total` count every step, with the target step second; a hidden target
/// step is taken out of both.
fn step_counter<W: WizardDraft>(
    position: usize,
    total: usize,
    target_hidden: bool,
    wizard: &W,
) -> String {
    let profile_hidden = wizard.profile_step_skipped();
    let review_hidden = profile_hidden && target_hidden;
    let skipped =
        usize::from(profile_hidden) + usize::from(target_hidden) + usize::from(review_hidden);
    let before =
        usize::from(profile_hidden && position > 1) + usize::from(target_hidden && position > 2);
    format!("{}/{}", position - before, total - skipped)
}

fn render_launching(
    frame: &mut Frame<'_>,
    area: Rect,
    form: &mut Dialog<WizardControl>,
    surfaces: &mut FrameSurfaces,
    title: &str,
    error: Option<&str>,
) {
    let lines = wrap_lines(
        [Line::raw(error.unwrap_or("Checking prerequisites…"))],
        68.min(area.width.saturating_sub(4)),
    );
    let popup = centered_modal(frame, surfaces, 72, (lines.len() as u16 + 6).max(8), area);
    let inner = DialogShell::padded_inner(popup);
    let title = dismissible_modal_title(form, popup, title, theme::title(true), true);
    frame.render_widget(theme::modal().title(title), popup);
    let layout = DialogShell::layout(inner, 1);
    frame.render_widget(Paragraph::new(lines), layout.body);
    let mut buttons = vec![(WizardControl::Cancel, "Cancel", true)];
    if error.is_some() {
        buttons.push((WizardControl::Submit, "Retry", true));
    }
    Dialog::render_actions(frame, layout.actions, &buttons, form);
}

pub(crate) fn begin_form_frame(form: &mut Dialog<WizardControl>, _initial: WizardControl) {
    form.begin_frame();
}

pub(crate) fn render_new_wizard(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    wizard: &NewWizard,
    surfaces: &mut FrameSurfaces,
) {
    let mut form = wizard.form.borrow_mut();
    // Read focus before the frame starts: `begin_frame` hides last frame's
    // registrations, so `is_focused` is false for a control drawn before it
    // registers again, as the recent-directory rows are.
    let focused_before_frame = form.focused();
    let initial = step_initial(wizard.step);
    begin_form_frame(&mut form, initial);
    let target_hidden = target_step_hidden(dashboard, wizard);
    if wizard.step == WizardStep::Launching {
        render_launching(
            frame,
            area,
            &mut form,
            surfaces,
            "Creating session",
            wizard.launch_error(),
        );
        form.end_frame(initial);
        return;
    }
    if wizard.step == WizardStep::Review {
        let target_id = nth_key(&dashboard.config.targets, wizard.target);
        let raw_project = is_bare_project_target(&dashboard.config.targets[&target_id]);
        let title = format!(
            " New session · {} review ",
            step_counter(4, 4, target_hidden, wizard)
        );
        let bundle_id = (!raw_project)
            .then(|| nth_bundle_key(&dashboard.config, &dashboard.state, wizard.bundle));
        render_review_wizard(
            frame,
            area,
            dashboard,
            ReviewWizardView {
                // Isolated targets always provide the workspace, so the choice
                // only exists for a bare project directory.
                worktree: raw_project.then(|| {
                    (
                        wizard.create_managed_worktree,
                        wizard
                            .selected_worktree_options(&dashboard.config)
                            .is_some_and(|options| options.available),
                    )
                }),
                profile_id: &nth_enabled_profile(&dashboard.config, wizard.profile),
                project_label: if raw_project {
                    "Project directory"
                } else {
                    "Project"
                },
                project: if raw_project {
                    wizard.project_directory.trim()
                } else {
                    bundle_id.as_deref().expect("bundle selected")
                },
                project_note: if raw_project && launch_directory_chosen(dashboard, wizard) {
                    "  (the repository mj was started in)"
                } else {
                    ""
                },
                target_id: &target_id,
                allocation: wizard.resource_allocation.as_ref(),
                mounts: &wizard.mounts,
                title: &title,
                submit_label: if wizard.remote_preflight_error.is_some() {
                    "Retry"
                } else {
                    "Create"
                },
                moving: false,
                preparing: false,
                preparation_error: None,
                submit_enabled: (!raw_project
                    || wizard
                        .selected_worktree_options(&dashboard.config)
                        .is_some()
                    || wizard.remote_preflight_error.is_some()),
                active_interruption: false,
                in_place_move: false,
                source_unavailable: false,
                stopped_subagents: 0,
                subagents: None,
                clear_resource_allocation: false,
                queue: None,
                queued_entries: &[],
                prepared_entries: &[],
                remote_repositories: wizard.remote_repositories.as_deref(),
                remote_preflight_in_flight: wizard.remote_preflight_in_flight,
                remote_preflight_error: wizard.remote_preflight_error.as_deref(),
                local_changes_excluded: !raw_project,
                conversion: None,
            },
            &mut form,
            surfaces,
        );
        form.end_frame(initial);
        return;
    }
    if wizard.step == WizardStep::ProjectDirectory {
        let target_id = nth_key(&dashboard.config.targets, wizard.target);
        let local = matches!(
            dashboard.config.targets[&target_id],
            TargetTemplate::LocalBare
        );
        let width = centered_rect(76, 1, area).width.saturating_sub(4);
        let intro = wrap_lines(
            [Line::raw(if local {
                "Absolute project directory on this machine:"
            } else {
                "Absolute project directory on the remote machine:"
            })],
            width,
        );
        let field_row = intro.len() as u16 + 1;
        let mut details = vec![Line::raw("")];
        if wizard.profile_step_skipped
            && dashboard
                .config
                .enabled_profiles()
                .nth(wizard.profile)
                .is_some_and(|(_, profile)| needs_guardian_warning(profile.kind))
        {
            details.extend(wrap_lines([guardian_footnote()], width));
            details.push(Line::raw(""));
        }

        if let Some(error) = &wizard.project_directory_error {
            details.extend(wrap_lines(
                [Line::styled(
                    format!("Error: {error}"),
                    Style::default().fg(theme::palette().error),
                )],
                width,
            ));
            details.push(Line::raw(""));
        }
        if local && launch_directory_chosen(dashboard, wizard) {
            details.extend(wrap_lines(
                [Line::styled(
                    "Filled in with the repository mj was started in.",
                    Style::default().fg(theme::palette().warning),
                )],
                width,
            ));
            details.push(Line::raw(""));
        }
        let mut recent_rows = Vec::new();
        if !wizard.project_history.is_empty() {
            details.push(Line::styled("Recent on this host:", theme::muted()));
            details.push(Line::raw(""));
            for (index, directory) in wizard.project_history.iter().take(5).enumerate() {
                let start = field_row + 1 + details.len() as u16;
                let rows = wrap_lines(
                    [Line::styled(
                        format!(
                            "{} {}",
                            if index == wizard.project_history_index {
                                "›"
                            } else {
                                " "
                            },
                            directory.display()
                        ),
                        if focused_before_frame == Some(WizardControl::RecentProject(index)) {
                            theme::selection(true)
                        } else if index == wizard.project_history_index {
                            Style::default().fg(theme::palette().text)
                        } else {
                            theme::muted()
                        },
                    )],
                    width,
                );
                recent_rows.push((index, start, rows.len() as u16));
                details.extend(rows);
            }
        }
        while details.last().is_some_and(|line| line.width() == 0) {
            details.pop();
        }
        let height = field_row
            + 1
            + (details.len() as u16).max(PathField::popup_rows(&wizard.project_directory));
        let popup = centered_modal(frame, surfaces, 76, (height + 5).max(9), area);
        let inner = DialogShell::padded_inner(popup);
        let layout = DialogShell::layout(inner, 1);
        let focused_row = match focused_before_frame {
            Some(WizardControl::RecentProject(index)) => recent_rows
                .iter()
                .find(|(i, _, _)| *i == index)
                .map(|(_, row, _)| *row),
            _ => Some(field_row),
        };
        let viewport = FormViewport::new(layout.body, height, 0, focused_row);
        let title = dismissible_modal_title(
            &mut form,
            popup,
            format!(
                "New session · {} {} project",
                step_counter(3, 4, target_hidden, wizard),
                if local { "local" } else { "remote" }
            ),
            theme::title(true),
            true,
        );
        frame.render_widget(theme::modal().title(title), popup);
        for (index, line) in intro.into_iter().enumerate() {
            frame.render_widget(Paragraph::new(line), viewport.row(index as u16, 1));
        }
        for (index, line) in details.into_iter().enumerate() {
            frame.render_widget(
                Paragraph::new(line),
                viewport.row(field_row + 1 + index as u16, 1),
            );
        }
        for (index, start, height) in recent_rows {
            form.register(
                WizardControl::RecentProject(index),
                ControlKind::Button,
                viewport.row(start, height),
                true,
            );
        }
        PathField::render_within(
            frame,
            layout.body,
            viewport.row(field_row, 1),
            &wizard.project_directory,
            &mut form,
            WizardControl::ProjectDirectory,
        );
        let mut buttons = vec![(WizardControl::Cancel, "Cancel", true)];
        if wizard.has_back() {
            buttons.push((WizardControl::Back, "Back", true));
        }
        buttons.push((
            WizardControl::Next,
            if wizard.skips_review() {
                "Create"
            } else {
                "Next"
            },
            true,
        ));
        Dialog::render_actions(frame, layout.actions, &buttons, &mut form);
        form.end_frame(initial);
        return;
    }
    if wizard.step == WizardStep::Mounts {
        render_mount_wizard(
            frame,
            area,
            dashboard,
            wizard.target,
            &wizard.mounts,
            &mut form,
            " Add attached directory ",
            surfaces,
        );
        form.end_frame(initial);
        return;
    }
    if wizard.step == WizardStep::NewBundle {
        projects::render_project_picker(frame, area, wizard, &mut form, surfaces);
        form.end_frame(initial);
        return;
    }
    let mut step_help = Vec::new();
    let (title, choices, selected): (_, Vec<PickerChoice>, _) = match wizard.step {
        WizardStep::Profile => (
            format!(
                " New session · {} profile ",
                step_counter(1, 4, target_hidden, wizard)
            ),
            profile_table(
                dashboard
                    .config
                    .enabled_profiles()
                    .map(|(id, profile)| dashboard.profile_choice(id, profile.kind))
                    .collect(),
            ),
            wizard.profile,
        ),
        WizardStep::Bundle => (
            format!(
                " New session · {} choose a project ",
                step_counter(3, 4, target_hidden, wizard)
            ),
            {
                let ids = bundle_ids_by_recent_creation(&dashboard.config, &dashboard.state);
                if let Some(id) = ids.get(wizard.bundle) {
                    step_help = bundle_details(
                        id,
                        &dashboard.config.bundles[*id],
                        &dashboard.missing_project_directories(&dashboard.config.bundles[*id]),
                    );
                }
                ids.into_iter()
                    .map(|id| {
                        let bundle = &dashboard.config.bundles[id];
                        let mut summary = bundle
                            .repositories
                            .first()
                            .map(|repository| compact_path(&repository_source(repository)))
                            .unwrap_or_default();
                        if bundle.repositories.len() > 1 {
                            summary.push_str(&format!("  +{} more", bundle.repositories.len() - 1));
                        }
                        if !dashboard.missing_project_directories(bundle).is_empty() {
                            summary.push_str("  (unavailable)");
                        }
                        PickerChoice::table(vec![
                            PickerCell::text(id),
                            PickerCell::styled(summary, theme::muted()),
                        ])
                    })
                    .collect()
            },
            wizard.bundle,
        ),
        WizardStep::Target => {
            let (rows, lines, selected_row) =
                target_step_choices(dashboard, wizard, wizard.sizing_error.as_deref());
            step_help = lines;
            (
                format!(
                    " New session · {} target ",
                    step_counter(2, 4, target_hidden, wizard)
                ),
                rows,
                selected_row,
            )
        }
        WizardStep::MoveFiles => unreachable!("file selection belongs to Move"),
        WizardStep::Review | WizardStep::Launching => unreachable!("review was rendered above"),
        WizardStep::Mounts => unreachable!("mount input was rendered above"),
        WizardStep::NewBundle => unreachable!("bundle input was rendered above"),
        WizardStep::ProjectDirectory => unreachable!("project directory input was rendered above"),
    };
    let mut help = if matches!(wizard.step, WizardStep::Target | WizardStep::Bundle) {
        step_help
    } else {
        Vec::new()
    };
    if wizard.step == WizardStep::Profile
        && dashboard
            .config
            .enabled_profiles()
            .any(|(_, profile)| needs_guardian_warning(profile.kind))
    {
        help.push(guardian_footnote());
    }
    let bundle_actions = [
        (WizardControl::Add, "Add…", true),
        (
            WizardControl::RemoveBundle,
            "Remove",
            !dashboard.config.bundles.is_empty(),
        ),
    ];
    render_picker(
        frame,
        area,
        &title,
        choices,
        help,
        PickerNavigation {
            resources: target_resources(dashboard, wizard),
            has_back: wizard.has_back(),
            selected,
            control: match wizard.step {
                WizardStep::Profile => WizardControl::ProfileList,
                WizardStep::Bundle => WizardControl::BundleList,
                WizardStep::Target => WizardControl::TargetList,
                _ => unreachable!("picker step has a list control"),
            },
            next_enabled: match wizard.step {
                WizardStep::Target => target_advance_enabled(dashboard, wizard),
                // Without a bundle there is nothing to review; the pinned
                // action is the only way forward.
                WizardStep::Bundle => !dashboard.config.bundles.is_empty(),
                _ => true,
            },
            side_actions: if wizard.step == WizardStep::Bundle {
                &bundle_actions
            } else {
                &[]
            },
            empty_hint: (wizard.step == WizardStep::Bundle && dashboard.config.bundles.is_empty())
                .then_some("Choose a project to get started."),
        },
        &mut form,
        surfaces,
    );
    form.end_frame(match wizard.step {
        WizardStep::Profile => WizardControl::ProfileList,
        WizardStep::Bundle => WizardControl::BundleList,
        WizardStep::Target => WizardControl::TargetList,
        _ => unreachable!("picker step has a list control"),
    });
}

/// Where a project repository comes from, as the user gave it: the GitHub
/// name, else the local path with the home directory written as `~`.
fn repository_source(repository: &mj_core::config::ProjectRepository) -> String {
    if let Some(github) = &repository.github {
        return github.clone();
    }
    let Some(path) = &repository.local else {
        return repository.id.clone();
    };
    if let Some(home) = std::env::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_owned()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

/// Longest source a project row shows before it cuts the middle out.
const PROJECT_SOURCE_CHARS: usize = 40;

/// Shortens a long source by cutting its middle: the start says where it
/// lives and the end says which one it is. The details under the list show
/// it whole.
fn compact_path(source: &str) -> String {
    let chars = source.chars().collect::<Vec<_>>();
    if chars.len() <= PROJECT_SOURCE_CHARS {
        return source.to_owned();
    }
    let head = PROJECT_SOURCE_CHARS / 3;
    let tail = PROJECT_SOURCE_CHARS - head - 1;
    let mut short = chars[..head].iter().collect::<String>();
    short.push('…');
    short.extend(&chars[chars.len() - tail..]);
    short
}

/// The selected project's full sources, drawn under the project list so a
/// long path or a second repository is never cut off.
fn bundle_details(
    id: &str,
    bundle: &mj_core::config::ProjectBundle,
    missing: &[&std::path::Path],
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        format!(
            "{id} · {}",
            crate::widgets::counted(bundle.repositories.len(), "repository", "repositories")
        ),
        theme::muted(),
    )];
    let multiple = bundle.repositories.len() > 1;
    for repository in &bundle.repositories {
        lines.push(Line::raw(""));
        let primary = multiple && repository.id == bundle.primary_repo;
        lines.push(Line::raw(format!(
            "  {}{}",
            repository_source(repository),
            if primary { "  (primary)" } else { "" }
        )));
        if let Some(path) = repository
            .local
            .as_deref()
            .filter(|path| missing.contains(path))
        {
            lines.push(Line::styled(
                format!(
                    "  Unavailable: the directory {} does not exist.",
                    path.display()
                ),
                Style::default().fg(theme::palette().error),
            ));
        }
    }
    lines
}

pub(crate) struct ReviewWizardView<'a> {
    worktree: Option<(bool, bool)>,
    pub(crate) profile_id: &'a str,
    pub(crate) project_label: &'a str,
    pub(crate) project: &'a str,
    pub(crate) project_note: &'a str,
    pub(crate) target_id: &'a str,
    pub(crate) allocation: Option<&'a SessionResourceAllocation>,
    pub(crate) mounts: &'a MountWizard,
    pub(crate) title: &'a str,
    submit_label: &'a str,
    moving: bool,
    preparing: bool,
    preparation_error: Option<&'a str>,
    submit_enabled: bool,
    active_interruption: bool,
    /// True when the prepared move keeps the environment and replaces only the
    /// harness and profile, so the review must not promise a fresh environment.
    in_place_move: bool,
    source_unavailable: bool,
    /// Sub-agents the Move stops that had not handed back; idle ones are
    /// stopped without a word, as a suspend stops them.
    stopped_subagents: usize,
    subagents: Option<&'a subagents::SubagentDraft>,
    clear_resource_allocation: bool,
    queue: Option<(usize, bool)>,
    queued_entries: &'a [mj_core::relay::QueuedPrompt],
    prepared_entries: &'a [MaterializedQueuedPrompt],
    remote_repositories: Option<&'a [RemoteRepositoryPreview]>,
    remote_preflight_in_flight: bool,
    remote_preflight_error: Option<&'a str>,
    local_changes_excluded: bool,
    /// Present only when a move converts a local checkout into an isolated
    /// workspace, so the review can say what travels before the confirmation.
    conversion: Option<&'a mj_core::state::RawConversionPreview>,
}

pub(crate) fn render_review_wizard(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    view: ReviewWizardView<'_>,
    form: &mut Dialog<WizardControl>,
    surfaces: &mut FrameSurfaces,
) {
    let ReviewWizardView {
        worktree,
        profile_id,
        project_label,
        project,
        project_note,
        target_id,
        allocation,
        mounts,
        title,
        submit_label,
        moving,
        preparing,
        preparation_error,
        submit_enabled,
        active_interruption,
        in_place_move,
        source_unavailable,
        stopped_subagents,
        subagents,
        clear_resource_allocation,
        queue,
        queued_entries,
        prepared_entries,
        remote_repositories,
        remote_preflight_in_flight,
        remote_preflight_error,
        local_changes_excluded,
        conversion,
    } = view;
    let target = &dashboard.config.targets[target_id];
    let can_attach = mount_history_host(target).is_some();
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Profile: ", theme::muted()),
            Span::styled(
                profile_id,
                Style::default()
                    .fg(theme::palette().secondary)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(format!("{project_label}: "), theme::muted()),
            Span::styled(
                project,
                Style::default()
                    .fg(theme::palette().text)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(project_note, theme::muted()),
        ]),
        Line::from(vec![
            Span::styled("Target: ", theme::muted()),
            Span::styled(target_id, Style::default().fg(theme::palette().accent)),
            Span::styled(format!(" ({})", target_label(target)), theme::muted()),
        ]),
        Line::from(vec![
            Span::styled("Compute: ", theme::muted()),
            Span::raw(resource_allocation_description(allocation)),
        ]),
    ];
    if moving && source_unavailable {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Source is unavailable; Move will recover its saved data without starting its old harness.",
            Style::default().fg(theme::palette().warning),
        ));
    }
    if moving && in_place_move {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Only the harness and profile are replaced; the environment and workspace are kept.",
            theme::muted(),
        ));
    }
    if moving && stopped_subagents > 0 {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!(
                "{} will be stopped; the session is told which when it resumes.",
                crate::widgets::counted(
                    stopped_subagents,
                    "working sub-agent",
                    "working sub-agents"
                )
            ),
            Style::default().fg(theme::palette().warning),
        ));
    }
    if moving && active_interruption {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            if in_place_move {
                "Active work will be interrupted; the session keeps its environment."
            } else {
                "Active work will be interrupted; the session is restored into a fresh environment."
            },
            Style::default().fg(theme::palette().warning),
        ));
        if clear_resource_allocation {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Fixed/default destination resources will replace the source sizing.",
                Style::default().fg(theme::palette().warning),
            ));
        }
    }
    if let Some(conversion) = conversion.filter(|_| moving) {
        lines.push(Line::raw(""));
        lines.push(Line::raw(conversion.summary_line()));
        for warning in conversion.warning_lines() {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                warning,
                Style::default().fg(theme::palette().warning),
            ));
        }
    }
    if moving {
        if preparing {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Checking move destination…",
                Style::default().fg(theme::palette().muted),
            ));
        } else if let Some(error) = preparation_error {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!("Move preparation failed: {error}"),
                Style::default().fg(theme::palette().error),
            ));
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Press Retry to check the destination again.",
                Style::default().fg(theme::palette().muted),
            ));
        }
    }
    if remote_preflight_in_flight {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Checking prerequisites…",
            Style::default().fg(theme::palette().muted),
        ));
    } else if let Some(error) = remote_preflight_error {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("Prerequisite check failed: {error}"),
            Style::default().fg(theme::palette().error),
        ));
    } else if let Some(repositories) = remote_repositories {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            if local_changes_excluded {
                "Network clone plan (local commits and dirty files excluded):"
            } else {
                "Network clone plan:"
            },
            theme::muted(),
        ));
        for repository in repositories {
            let pushes = if repository.push_urls.is_empty() {
                "none".to_owned()
            } else {
                repository.push_urls.join(", ")
            };
            lines.push(Line::raw(format!(
                "  {}: fetch {} @ {}; push {}",
                repository.repository_id, repository.fetch_url, repository.default_branch, pushes
            )));
        }
    }
    let worktree_row = worktree.map(|(checked, available)| {
        lines.push(Line::raw(""));
        let row = lines.len() as u16;
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            if checked && available {
                "Create a separate session-owned clone on the selected or default branch."
            } else {
                "Use the selected directory directly."
            },
            theme::muted(),
        ));
        row
    });
    let queue_label = queue.map(|(count, _)| format!("Queued prompts: {count}"));
    if let Some(label) = &queue_label {
        lines.push(Line::raw(""));
        lines.push(Line::raw(label.clone()));
    }
    // Guardian targets rely on the harness's own approval mode rather than
    // Hel-managed isolation.
    if (matches!(target, TargetTemplate::LocalBare)
        || target.permission_mode() == Some(mj_core::config::PermissionMode::Guardian))
        && let Some(kind) = dashboard
            .config
            .profiles
            .get(profile_id)
            .map(|profile| profile.kind)
        && let Some(warning) = kind.unsandboxed_guardian_warning()
    {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("⚠ {warning}"),
            Style::default()
                .fg(theme::palette().error)
                .add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::raw(""));
    if can_attach {
        lines.push(Line::from(vec![
            Span::styled("Attached directories: ", theme::muted()),
            Span::styled(mounts.mounts.len().to_string(), theme::title(false)),
        ]));
        lines.push(Line::raw(""));
    }

    let mut subagent_model_row = None;
    let mut subagent_effort_row = None;
    let subagent_row = subagents.map(|wizard| {
        lines.push(Line::raw(""));
        let row = lines.len() as u16;
        lines.push(Line::raw(""));
        if matches!(
            wizard.policy,
            mj_core::subagent::SubagentPolicy::SingleModel { .. }
        ) {
            lines.push(Line::raw(""));
            let height = 1;
            subagent_model_row = Some((lines.len() as u16, height));
            for _ in 0..height {
                lines.push(Line::raw(""));
            }
            lines.push(Line::raw(""));
            let height = 1;
            subagent_effort_row = Some((lines.len() as u16, height));
            for _ in 0..height {
                lines.push(Line::raw(""));
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Configure profiles in Settings → Profiles; additional eligible profiles in Settings → Sub-agents. Your own profile is always eligible.",
                theme::muted(),
            ));

            if let Some(error) = wizard.error() {
                lines.push(Line::raw(error));
            }
            if let Some(options) = wizard.options() {
                for error in &options.unavailable {
                    lines.push(Line::raw(error.clone()));
                }
            }
        }
        row
    });
    let last_control_row = [
        worktree_row,
        subagent_row,
        subagent_model_row.map(|(row, _)| row),
        subagent_effort_row.map(|(row, _)| row),
    ]
    .into_iter()
    .flatten()
    .map(|row| usize::from(row) + 1)
    .max()
    .unwrap_or(0);
    if mounts.mounts.is_empty() || !can_attach {
        while lines.len() > last_control_row && lines.last().is_some_and(|line| line.width() == 0) {
            lines.pop();
        }
    }
    let width = centered_rect(84, 1, area).width.saturating_sub(4);
    let mut wrapped = Vec::new();
    let mut offsets = Vec::new();
    for line in lines {
        offsets.push(wrapped.len() as u16);
        wrapped.extend(wrap_lines([line], width));
    }
    let lines = wrapped;
    let map_row = |row: u16| offsets[usize::from(row)];
    let worktree_row = worktree_row.map(map_row);
    let subagent_row = subagent_row.map(map_row);
    let subagent_model_row = subagent_model_row.map(|(row, height)| (map_row(row), height));
    let subagent_effort_row = subagent_effort_row.map(|(row, height)| (map_row(row), height));
    let summary_height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let list_height = if can_attach {
        u16::try_from(mounts.mounts.len()).unwrap_or(u16::MAX)
    } else {
        0
    };
    let queue_height = if queue.is_some() {
        1_u16.saturating_add(
            u16::try_from(queued_entries.len().saturating_add(prepared_entries.len()))
                .unwrap_or(u16::MAX),
        )
    } else {
        0
    };
    let total_height = summary_height
        .saturating_add(list_height)
        .saturating_add(queue_height);
    let popup = centered_modal(frame, surfaces, 84, (total_height + 5).clamp(13, 36), area);
    let inner = DialogShell::padded_inner(popup);
    let title_line = dismissible_modal_title(form, popup, title.trim(), theme::title(true), true);
    frame.render_widget(theme::modal().title(title_line), popup);
    let body = DialogShell::layout(inner, 1).body;
    let focused_row = match form.focused() {
        Some(WizardControl::CreateManagedWorktree) => worktree_row,
        Some(WizardControl::Subagents) => subagent_row,
        Some(WizardControl::SubagentModel) => subagent_model_row.map(|(row, _)| row),
        Some(WizardControl::SubagentEffort) => subagent_effort_row.map(|(row, _)| row),
        Some(WizardControl::ReviewAttachments) => Some(
            summary_height.saturating_add(
                mounts
                    .history_index
                    .min(mounts.mounts.len().saturating_sub(1)) as u16,
            ),
        ),
        Some(WizardControl::DiscardQueue) => Some(summary_height.saturating_add(list_height)),
        _ => None,
    };
    let viewport = FormViewport::new(body, total_height, 0, focused_row);
    for (index, line) in lines.iter().enumerate() {
        frame.render_widget(
            Paragraph::new(line.clone()),
            viewport.row(u16::try_from(index).unwrap_or(u16::MAX), 1),
        );
    }
    if let Some(((checked, available), row)) = worktree.zip(worktree_row) {
        Checkbox::render(
            frame,
            viewport.row(row, 1),
            "Create isolated checkout",
            checked && available,
            available,
            form,
            WizardControl::CreateManagedWorktree,
        );
    }
    let mut expanded_subagent_combo = None;
    if let Some((wizard, row)) = subagents.zip(subagent_row) {
        let mut selectors = vec![(
            WizardControl::Subagents,
            "Subagents",
            row,
            wizard.policies(),
            wizard.policy_index(),
            true,
        )];
        if let Some((row, _)) = subagent_model_row {
            selectors.push((
                WizardControl::SubagentModel,
                "Model",
                row,
                wizard.models(),
                wizard.model_index(),
                wizard.models_ready(),
            ));
        }
        if let Some((row, _)) = subagent_effort_row {
            selectors.push((
                WizardControl::SubagentEffort,
                "Effort",
                row,
                wizard.efforts(),
                wizard.effort_index(),
                wizard.options().is_some(),
            ));
        }
        for (id, label, row, values, committed, enabled) in selectors {
            let area = viewport.row(row, 1);
            let label_width = 11.min(area.width);
            frame.render_widget(
                Line::raw(label),
                Rect::new(area.x, area.y, label_width, area.height),
            );
            let field = Rect::new(
                area.x + label_width,
                area.y,
                area.width - label_width,
                area.height,
            );
            let selected = wizard.combo.selection(id, committed);
            let value = values.get(selected).cloned().unwrap_or_default();
            let options = values.into_iter().map(Line::raw).collect::<Vec<_>>();
            ComboBox::render(
                frame,
                inner,
                field,
                &value,
                &options,
                selected,
                false,
                enabled,
                " Values ",
                PopupSide::Below,
                form,
                id,
            );
            if wizard.combo.is_open(id) {
                expanded_subagent_combo = Some((id, field, value, options, selected, enabled));
            }
        }
    }
    if can_attach && !mounts.mounts.is_empty() {
        let list_area = viewport.row(summary_height, list_height);
        let rows = mounts
            .mounts
            .iter()
            .map(|mount| {
                Line::raw(format!(
                    "{} → {}{}",
                    mount.source.display(),
                    mount.destination.display(),
                    access_marker(mount.access)
                ))
            })
            .collect::<Vec<_>>();
        ChoiceList::render(
            frame,
            list_area,
            &rows,
            mounts.history_index,
            form,
            WizardControl::ReviewAttachments,
        );
    }
    if let Some((count, discard)) = queue {
        let queue_area = viewport.row(summary_height.saturating_add(list_height), 1);
        Checkbox::render(
            frame,
            queue_area,
            &format!(
                "{} {} {}",
                if discard { "Discard" } else { "Start" },
                crate::widgets::counted(count, "queued command", "queued commands"),
                if moving { "after move" } else { "on resume" },
            ),
            discard,
            true,
            form,
            WizardControl::DiscardQueue,
        );
        for (index, entry) in queued_entries.iter().enumerate() {
            let text = if entry.text.trim().is_empty() {
                "[empty command]".to_owned()
            } else {
                entry.text.replace('\n', " ")
            };
            let attachment_count = entry.attachments.len();
            let attachment_note = match attachment_count {
                0 => String::new(),
                1 => " · 1 attachment".to_owned(),
                count => format!(" · {count} attachments"),
            };
            let text = truncate_to_cells(
                &text,
                inner.width.saturating_sub(4) as usize,
                Truncate::SUMMARY,
            );
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!("  {}. {text}{attachment_note}", index + 1),
                    Style::default().fg(theme::palette().muted),
                )),
                viewport.row(
                    summary_height
                        .saturating_add(list_height)
                        .saturating_add(1)
                        .saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
                    1,
                ),
            );
        }
        for (index, entry) in prepared_entries.iter().enumerate() {
            let (kind, text) = match &entry.kind {
                mj_core::state::QueuedCommandKind::Prompt => (
                    "prompt",
                    mj_core::transcript::materialized_content_text(&entry.content),
                ),
                mj_core::state::QueuedCommandKind::SetConfig { key, value } => {
                    ("config", mj_core::state::config_command_text(key, value))
                }
            };
            let text = if text.trim().is_empty() {
                format!("[{kind}]")
            } else {
                format!("{kind}: {}", text.replace('\n', " "))
            };
            let text = truncate_to_cells(
                &text,
                inner.width.saturating_sub(4) as usize,
                Truncate::SUMMARY,
            );
            let row = queued_entries.len().saturating_add(index);
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!("  {}. {text}", row + 1),
                    Style::default().fg(theme::palette().muted),
                )),
                viewport.row(
                    summary_height
                        .saturating_add(list_height)
                        .saturating_add(1)
                        .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                    1,
                ),
            );
        }
    }
    let mut buttons = vec![
        (WizardControl::Cancel, "Cancel", true),
        (WizardControl::Back, "Back", true),
    ];
    if can_attach {
        buttons.push((WizardControl::Add, "Add directory…", true));
    }
    if moving && !in_place_move {
        buttons.push((
            WizardControl::ChooseMoveFiles,
            "Choose files…",
            !preparing && preparation_error.is_none(),
        ));
    }
    buttons.push((
        WizardControl::Submit,
        submit_label,
        submit_enabled
            && !remote_preflight_in_flight
            && (remote_repositories.is_some()
                || !local_changes_excluded
                || remote_preflight_error.is_some())
            && (allocation.is_some() || !matches!(target, TargetTemplate::AwsEc2 { .. })),
    ));
    Dialog::render_actions(frame, DialogShell::layout(inner, 1).actions, &buttons, form);
    // Paint the active popup last so it overlays the remaining review fields.
    if let Some((id, field, value, options, selected, enabled)) = expanded_subagent_combo {
        ComboBox::render(
            frame,
            inner,
            field,
            &value,
            &options,
            selected,
            true,
            enabled,
            " Values ",
            PopupSide::Below,
            form,
            id,
        );
    }
}

/// Suffix that shows an attached directory's access mode in a list row.
pub(crate) fn access_marker(access: MountAccess) -> String {
    format!(" · {}", access.label())
}

/// The access modes offered for an attachment, as the shared rule in
/// [`MountAccess::offered`] defines them.
pub(crate) fn access_choices(overlay_unavailable: bool) -> Vec<MountAccess> {
    MountAccess::offered(!overlay_unavailable)
}

fn access_description(access: MountAccess) -> &'static str {
    match access {
        MountAccess::Ro => "ro · read-only",
        MountAccess::Cow => "cow · container-private copy-on-write",
        MountAccess::Rw => "rw · writes reach the host directory",
    }
}

/// The form control kind for an access-mode combobox.
pub(crate) fn access_combo_kind<K: Copy + Eq>(
    combo: &ComboBoxState<K>,
    choices: &[MountAccess],
    access: MountAccess,
    id: K,
) -> ControlKind {
    ControlKind::ComboBox {
        len: choices.len(),
        selected: combo.selection(id, access_index(choices, access)),
        expanded: combo.is_open(id),
    }
}

pub(crate) fn access_index(choices: &[MountAccess], access: MountAccess) -> usize {
    choices
        .iter()
        .position(|choice| *choice == access)
        .unwrap_or(0)
}

/// Draw the access-mode combobox for an attachment under edit. Screens call
/// this once in place and, while it is open, again after everything else so
/// the popup stays on top.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_access_combo<K: Copy + Eq>(
    frame: &mut Frame<'_>,
    bounds: Rect,
    field: Rect,
    access: MountAccess,
    choices: &[MountAccess],
    combo: &ComboBoxState<K>,
    expanded: bool,
    form: &mut Form<K>,
    id: K,
) {
    let selected = combo.selection(id, access_index(choices, access));
    // `ComboBox::render` adds the dropdown glyph itself.
    let value = choices
        .get(selected)
        .map(|choice| access_description(*choice))
        .unwrap_or_default();
    let options = choices
        .iter()
        .map(|choice| Line::raw(access_description(*choice)))
        .collect::<Vec<_>>();
    ComboBox::render(
        frame,
        bounds,
        field,
        value,
        &options,
        selected,
        expanded,
        true,
        " Access ",
        PopupSide::Below,
        form,
        id,
    );
}

// Domain mount data, navigation, and the shared form are separate inputs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_mount_wizard(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    target_index: usize,
    mounts: &MountWizard,
    form: &mut Dialog<WizardControl>,
    title: &str,
    surfaces: &mut FrameSurfaces,
) {
    let target_id = nth_key(&dashboard.config.targets, target_index);
    let target = dashboard
        .config
        .targets
        .get(&target_id)
        .expect("selected target index is present in config");
    let protection = match target {
        TargetTemplate::AppleContainer { .. } => {
            "Apple Container has no :O overlay mode; each extra bind is read-only."
        }
        TargetTemplate::LocalPodman { .. } | TargetTemplate::SshPodman { .. } => {
            "Podman uses :O copy-on-write overlays; read-only skips the overlay."
        }
        TargetTemplate::LocalDocker { .. } | TargetTemplate::SshDocker { .. } => {
            "Docker uses session-owned OverlayFS volumes on the Docker host; read-only skips the overlay."
        }
        TargetTemplate::AwsEc2 { .. } => {
            "EC2 directories stream as tar.gz through one SSH connection into the destination."
        }
        TargetTemplate::LocalBare | TargetTemplate::SshBare { .. } => {
            unreachable!("bare targets do not attach resources")
        }
    };
    let mut lines = vec![
        Line::raw(format!("Target: {target_id} ({})", target_label(target))),
        Line::raw(""),
        Line::styled(protection, Style::default().fg(theme::palette().warning)),
    ];
    if !mounts.mounts.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::raw("Already attached:"));
        lines.extend(mounts.mounts.iter().map(|mount| {
            Line::raw(format!(
                "  {} → {}{}",
                mount.source.display(),
                mount.destination.display(),
                access_marker(mount.access)
            ))
        }));
    }
    if form.is_focused(WizardControl::MountSource)
        && mounts.source.is_empty()
        && !mounts.history.is_empty()
    {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Recent sources:",
            Style::default().fg(theme::palette().muted),
        ));
        lines.extend(
            mounts
                .history
                .iter()
                .take(5)
                .enumerate()
                .map(|(index, source)| {
                    let marker = if index == mounts.history_index {
                        "› "
                    } else {
                        "  "
                    };
                    Line::raw(format!("{marker}{}", source.display()))
                }),
        );
    }
    if let Some(error) = &mounts.error {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            error,
            Style::default().fg(theme::palette().error),
        ));
    }

    lines.push(Line::raw(""));
    let lines = wrap_lines(lines, centered_rect(84, 1, area).width.saturating_sub(4));
    let info_height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let total_height = info_height.saturating_add(5);
    let popup = centered_modal(frame, surfaces, 84, (total_height + 5).clamp(13, 32), area);
    let inner = DialogShell::padded_inner(popup);
    let title_line = dismissible_modal_title(form, popup, title.trim(), theme::title(true), true);
    frame.render_widget(theme::modal().title(title_line), popup);
    let body = DialogShell::layout(inner, 1).body;
    let focused_row = match form.focused() {
        Some(WizardControl::MountSource) => Some(info_height),
        Some(WizardControl::MountDestination) => Some(info_height.saturating_add(2)),
        Some(WizardControl::MountAccess) => Some(info_height.saturating_add(4)),
        _ => None,
    };
    let viewport = FormViewport::new(body, total_height, 0, focused_row);
    for (index, line) in lines.iter().enumerate() {
        let row = viewport.row(u16::try_from(index).unwrap_or(u16::MAX), 1);
        frame.render_widget(Paragraph::new(line.clone()), row);
    }
    // A completion popup may cover the rows below its field, but not the
    // button row and not the dialog's frame.
    let popup_bounds = Rect::new(inner.x, inner.y, inner.width, body.height);
    let field_width = inner.width.saturating_sub(12);
    let source_row = viewport.row(info_height, 1);
    frame.render_widget(
        Paragraph::new("Source:"),
        Rect::new(
            source_row.x,
            source_row.y,
            10.min(source_row.width),
            source_row.height,
        ),
    );
    let source_field = Rect::new(
        source_row.x.saturating_add(10),
        source_row.y,
        field_width,
        source_row.height,
    );
    PathField::render_within(
        frame,
        popup_bounds,
        source_field,
        &mounts.source,
        form,
        WizardControl::MountSource,
    );
    let destination_row = viewport.row(info_height.saturating_add(2), 1);
    frame.render_widget(
        Paragraph::new("Destination:"),
        Rect::new(
            destination_row.x,
            destination_row.y,
            10.min(destination_row.width),
            destination_row.height,
        ),
    );
    PathField::render_within(
        frame,
        popup_bounds,
        Rect::new(
            destination_row.x.saturating_add(10),
            destination_row.y,
            field_width,
            destination_row.height,
        ),
        &mounts.destination,
        form,
        WizardControl::MountDestination,
    );
    let access_row = viewport.row(info_height.saturating_add(4), 1);
    frame.render_widget(
        Paragraph::new("Access:"),
        Rect::new(
            access_row.x,
            access_row.y,
            10.min(access_row.width),
            access_row.height,
        ),
    );
    let access_field = Rect::new(
        access_row.x.saturating_add(10),
        access_row.y,
        field_width,
        access_row.height,
    );
    let access_choices = mounts.access_choices();
    render_access_combo(
        frame,
        inner,
        access_field,
        mounts.access,
        &access_choices,
        &mounts.access_combo,
        false,
        form,
        WizardControl::MountAccess,
    );
    Dialog::render_actions(
        frame,
        DialogShell::layout(inner, 1).actions,
        &[
            (WizardControl::Cancel, "Cancel", true),
            (WizardControl::Back, "Back", true),
            (WizardControl::Add, "Add directory", true),
        ],
        form,
    );
    // The completion popup hangs over the rows below the source, so it is
    // drawn again once those rows are on the screen.
    if form.focused() == Some(WizardControl::MountSource) {
        PathField::render_within(
            frame,
            popup_bounds,
            source_field,
            &mounts.source,
            form,
            WizardControl::MountSource,
        );
    }
    if mounts.access_combo.is_open(WizardControl::MountAccess) {
        render_access_combo(
            frame,
            inner,
            access_field,
            mounts.access,
            &access_choices,
            &mounts.access_combo,
            true,
            form,
            WizardControl::MountAccess,
        );
    }
}

/// The title bar of one step of the resume wizard.
///
/// The same three steps start a resumed session, a moved one, and a session
/// restored from an archived transcript, so the first word says which.
fn resume_wizard_title(wizard: &ResumeWizard, step: &str, moving_step: &str) -> String {
    match (wizard.source, wizard.moving) {
        (_, true) => format!(" Move · {moving_step} "),
        (ResumeSource::Archive, false) => format!(" Restore · {step} "),
        (ResumeSource::Session, false) => format!(" Resume · {step} "),
    }
}

pub(crate) fn render_resume_wizard(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    wizard: &ResumeWizard,
    surfaces: &mut FrameSurfaces,
) {
    if wizard.step == WizardStep::MoveFiles {
        move_files::render(frame, area, wizard, surfaces);
        return;
    }
    let mut form = wizard.form.borrow_mut();
    let initial = step_initial(wizard.step);
    begin_form_frame(&mut form, initial);
    if wizard.step == WizardStep::Launching {
        render_launching(
            frame,
            area,
            &mut form,
            surfaces,
            if wizard.moving {
                "Moving session"
            } else {
                "Opening session"
            },
            wizard.launch_error(),
        );
        form.end_frame(initial);
        return;
    }
    if wizard.step == WizardStep::Review {
        let profile_id = dashboard
            .resume_wizard_profiles(wizard)
            .get(wizard.profile)
            .map(|(id, _)| id.as_str())
            .unwrap_or("unknown");
        let session = dashboard.state.sessions.get(&wizard.session_id);
        let bundle_id = session
            .map(|session| session.bundle_id.as_str())
            .unwrap_or("unknown");
        let target_id = nth_key(&dashboard.config.targets, wizard.target);
        let reused_project_directory = session
            .filter(|session| {
                mj_client::target::resume_compatibility(session, &dashboard.config, &target_id)
                    == Ok(mj_client::target::ResumePlan::InPlace)
            })
            .and_then(|session| session.project_directory.as_deref())
            .map(|directory| directory.display().to_string());
        // An archived session has no record here, so its own title is what
        // identifies it; a plain resume names the project it reopens.
        let (project_label, project, project_note) = match wizard.source {
            ResumeSource::Archive => ("Archived session", wizard.title.as_str(), ""),
            ResumeSource::Session => {
                if let Some(directory) = reused_project_directory.as_deref() {
                    ("Project directory", directory, " (reused)")
                } else {
                    ("Project", bundle_id, "")
                }
            }
        };
        let counter = step_counter(3, 3, target_step_hidden(dashboard, wizard), wizard);
        let review_title = resume_wizard_title(
            wizard,
            &format!("{counter} review"),
            &format!("{counter} confirm"),
        );
        render_review_wizard(
            frame,
            area,
            dashboard,
            ReviewWizardView {
                worktree: None,
                profile_id,
                project_label,
                project,
                project_note,
                target_id: &target_id,
                allocation: wizard.resource_allocation.as_ref(),
                mounts: &wizard.mounts,
                title: &review_title,
                submit_label: if wizard.moving && wizard.preparation_error.is_some() {
                    "Retry"
                } else if wizard.moving {
                    "Move"
                } else if wizard.source == ResumeSource::Archive {
                    "Restore"
                } else {
                    "Resume"
                },
                moving: wizard.moving,
                preparing: wizard.preparing,
                preparation_error: wizard.preparation_error.as_deref(),
                submit_enabled: (wizard.subagent_change(dashboard).is_none()
                    || wizard.subagents.error().is_none())
                    && (!wizard.moving
                        || wizard.preparation.is_some()
                        || wizard.preparation_error.is_some()),
                source_unavailable: wizard
                    .preparation
                    .as_ref()
                    .is_some_and(|p| p.source_unavailable),
                subagents: wizard
                    .subagent_choice_applies(dashboard)
                    .then_some(&*wizard.subagents),
                stopped_subagents: if wizard.moving {
                    dashboard
                        .subagents_not_handed_back(&wizard.session_id)
                        .len()
                } else {
                    0
                },
                active_interruption: wizard
                    .preparation
                    .as_ref()
                    .map_or(wizard.moving, |preparation| preparation.active),
                in_place_move: wizard
                    .preparation
                    .as_ref()
                    .is_some_and(|preparation| preparation.in_place),
                clear_resource_allocation: wizard.preparation.as_ref().map_or_else(
                    || {
                        wizard.moving
                            && dashboard
                                .state
                                .sessions
                                .get(&wizard.session_id)
                                .is_some_and(|session| session.resource_allocation.is_some())
                            && matches!(
                                dashboard.config.targets.get(&target_id),
                                Some(
                                    mj_core::config::TargetTemplate::LocalBare
                                        | mj_core::config::TargetTemplate::SshBare { .. }
                                )
                            )
                    },
                    |preparation| preparation.selection.clear_resource_allocation,
                ),
                queue: wizard.preparation.as_ref().map_or_else(
                    || {
                        dashboard
                            .session_details
                            .get(&wizard.session_id)
                            .map(|detail| detail.queued_prompts.len())
                            .filter(|count| *count > 0)
                            .map(|count| (count, wizard.discard_queue))
                    },
                    |preparation| {
                        (!preparation.queued_commands.is_empty())
                            .then_some((preparation.queued_commands.len(), wizard.discard_queue))
                    },
                ),
                queued_entries: if wizard.preparation.is_some() {
                    &[][..]
                } else {
                    dashboard
                        .session_details
                        .get(&wizard.session_id)
                        .map_or(&[][..], |detail| detail.queued_prompts.as_slice())
                },
                prepared_entries: wizard.preparation.as_ref().map_or(&[][..], |preparation| {
                    preparation.queued_commands.as_slice()
                }),
                remote_repositories: None,
                remote_preflight_in_flight: false,
                remote_preflight_error: None,
                local_changes_excluded: false,
                conversion: wizard
                    .preparation
                    .as_ref()
                    .and_then(|preparation| preparation.conversion.as_deref()),
            },
            &mut form,
            surfaces,
        );
        form.end_frame(initial);
        return;
    }
    if wizard.step == WizardStep::Mounts {
        render_mount_wizard(
            frame,
            area,
            dashboard,
            wizard.target,
            &wizard.mounts,
            &mut form,
            " Add attached directory ",
            surfaces,
        );
        form.end_frame(initial);
        return;
    }
    let target_hidden = target_step_hidden(dashboard, wizard);
    let (title, choices, selected, mut help) = match wizard.step {
        WizardStep::Profile => {
            let profiles = dashboard.resume_wizard_profiles(wizard);
            let session_harness = dashboard
                .state
                .sessions
                .get(&wizard.session_id)
                .map(|session| session.harness_kind);
            let rows = profiles
                .iter()
                .map(|(id, harness)| {
                    let choice = dashboard.profile_choice(id, *harness);
                    if session_harness.is_some_and(|current| current != *harness) {
                        choice.with_note("(lossy: text-only transcript)")
                    } else {
                        choice
                    }
                })
                .collect();
            // Only a change of harness hands over a text transcript. The
            // footnote describes the selected profile, so a same-harness
            // resume is not warned about a loss it does not have.
            let selected_is_lossy = profiles.get(wizard.profile).is_some_and(|(_, harness)| {
                session_harness.is_some_and(|current| current != *harness)
            });
            let mut help = Vec::new();
            if selected_is_lossy {
                help.push(picker_help(
                    "Lossy: text only; tool calls + reasoning dropped.",
                ));
            } else if session_harness.is_some() {
                help.push(picker_help(
                    "Same agent: the conversation continues natively.",
                ));
            }
            if profiles
                .iter()
                .any(|(_, harness)| needs_guardian_warning(*harness))
            {
                help.push(guardian_footnote());
            }
            let step = format!(
                "{} profile (cross-harness supported)",
                step_counter(1, 3, target_hidden, wizard)
            );
            (
                resume_wizard_title(wizard, &step, &step),
                profile_table(rows),
                wizard.profile,
                help,
            )
        }
        WizardStep::Target => {
            let (rows, help, selected_row) =
                target_step_choices(dashboard, wizard, wizard.sizing_error.as_deref());
            let step = format!("{} new target", step_counter(2, 3, target_hidden, wizard));
            (
                resume_wizard_title(wizard, &step, &step),
                rows,
                selected_row,
                help,
            )
        }
        WizardStep::Bundle => unreachable!("resume does not select a bundle"),
        WizardStep::Review | WizardStep::Launching => unreachable!("review was rendered above"),
        WizardStep::MoveFiles => unreachable!("Move files were rendered above"),
        WizardStep::Mounts => unreachable!("mount input was rendered above"),
        WizardStep::NewBundle => unreachable!("resume does not create bundles"),
        WizardStep::ProjectDirectory => unreachable!("resume does not select a project directory"),
    };
    if wizard.source == ResumeSource::Archive {
        // An archived session has no record to read a name from, so the title
        // the index kept is shown beside the choices.
        help.insert(0, picker_help(&format!("Restoring: {}", wizard.title)));
    }
    render_picker(
        frame,
        area,
        &title,
        choices,
        help,
        PickerNavigation {
            resources: target_resources(dashboard, wizard),
            has_back: wizard.has_back(),
            selected,
            control: match wizard.step {
                WizardStep::Profile => WizardControl::ProfileList,
                WizardStep::Target => WizardControl::TargetList,
                _ => unreachable!("resume picker step has a list control"),
            },
            next_enabled: wizard.step != WizardStep::Target
                || target_advance_enabled(dashboard, wizard),
            side_actions: &[],
            empty_hint: None,
        },
        &mut form,
        surfaces,
    );
    form.end_frame(match wizard.step {
        WizardStep::Profile => WizardControl::ProfileList,
        WizardStep::Target => WizardControl::TargetList,
        _ => unreachable!("resume picker step has a list control"),
    });
}
