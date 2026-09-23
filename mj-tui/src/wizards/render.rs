use super::*;

/// The Target step's resource keys, with the live refresh binding for the
/// recheck rather than a key that may not exist any more.
fn resource_help(dashboard: &DashboardState) -> String {
    let base = "+ double · - halve · c +8 CPU · m +50% memory · r reset";
    match dashboard.first_key_label(crate::CommandId::Refresh) {
        Some(key) => format!("{base} · {key} recheck"),
        None => base.to_owned(),
    }
}

pub(crate) fn step_initial(step: WizardStep) -> WizardControl {
    match step {
        WizardStep::Profile => WizardControl::ProfileList,
        WizardStep::Target => WizardControl::TargetList,
        WizardStep::Bundle => WizardControl::BundleList,
        WizardStep::ProjectDirectory => WizardControl::ProjectDirectory,
        WizardStep::NewBundle => WizardControl::NewBundleSource,
        WizardStep::Mounts => WizardControl::MountSource,
        WizardStep::Review => WizardControl::Submit,
    }
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
    if wizard.step == WizardStep::Review {
        let target_id = nth_key(&dashboard.config.targets, wizard.target);
        let raw_project = is_bare_project_target(&dashboard.config.targets[&target_id]);
        let bundle_id = (!raw_project)
            .then(|| nth_bundle_key(&dashboard.config, &dashboard.state, wizard.bundle));
        render_review_wizard(
            frame,
            area,
            dashboard,
            ReviewWizardView {
                subagents: wizard
                    .subagent_choice_applies(&dashboard.config)
                    .then_some(wizard.mjolnir_subagents),
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
                project_note: "",
                target_id: &target_id,
                allocation: wizard.resource_allocation.as_ref(),
                mounts: &wizard.mounts,
                title: " New session · 4/4 review ",
                submit_label: if wizard.remote_preflight_error.is_some() {
                    "Retry"
                } else {
                    "Create"
                },
                moving: false,
                preparing: false,
                preparation_error: None,
                submit_enabled: !raw_project
                    || wizard
                        .selected_worktree_options(&dashboard.config)
                        .is_some()
                    || wizard.remote_preflight_error.is_some(),
                active_interruption: false,
                in_place_move: false,
                source_unavailable: false,
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
        let mut lines = vec![
            Line::raw(if local {
                "Absolute project directory on this machine:"
            } else {
                "Absolute project directory on the remote machine:"
            }),
            Line::raw(""),
        ];
        if let Some(error) = &wizard.project_directory_error {
            lines.push(Line::styled(
                format!("Error: {error}"),
                Style::default().fg(theme::palette().error),
            ));
            lines.push(Line::raw(""));
        }
        let mut history_start = None;
        if !wizard.project_history.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Recent on this host (click or ↑/↓ selects):",
                Style::default().fg(theme::palette().muted),
            ));
            history_start = Some(lines.len());
            lines.extend(wizard.project_history.iter().take(5).enumerate().map(
                |(index, directory)| {
                    Line::styled(
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
                            Style::default().fg(theme::palette().muted)
                        },
                    )
                },
            ));
        }
        lines.push(Line::styled(
            "Enter validates · Tab moves · Back returns · Esc cancels",
            Style::default().fg(theme::palette().muted),
        ));
        // The dialog holds two intro rows, the field, the detail rows and the
        // button row, so it is sized for all of them: a shorter frame lets the
        // buttons overwrite the last detail rows, which is where the remembered
        // directories and the key hints live. A completion popup gets its own
        // rows between the field and the buttons for the same reason.
        let detail_rows = u16::try_from(lines.len().saturating_sub(2)).unwrap_or(u16::MAX);
        let content_rows = detail_rows
            .max(PathField::popup_rows(&wizard.project_directory))
            .saturating_add(4);
        let popup = centered_modal(
            frame,
            surfaces,
            76,
            content_rows.saturating_add(2).max(9),
            area,
        );
        let content = popup.inner(ratatui::layout::Margin {
            horizontal: 1,
            vertical: 1,
        });
        let title_line = dismissible_modal_title(
            &mut form,
            popup,
            if local {
                "New session · 3/4 local project"
            } else {
                "New session · 3/4 remote project"
            },
            theme::title(true),
            true,
        );
        frame.render_widget(theme::modal().title(title_line), popup);
        let intro = lines.iter().take(2).cloned().collect::<Vec<_>>();
        let details = lines.iter().skip(2).cloned().collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(intro),
            Rect::new(content.x, content.y, content.width, 2.min(content.height)),
        );
        let field_y = content.y.saturating_add(2);
        let button_y = content.bottom().saturating_sub(1);
        frame.render_widget(
            Paragraph::new(details),
            Rect::new(
                content.x,
                field_y.saturating_add(1),
                content.width,
                button_y.saturating_sub(field_y.saturating_add(1)),
            ),
        );
        if let Some(start) = history_start {
            for index in 0..wizard.project_history.len().min(5) {
                let row = content.y.saturating_add((start + index + 1) as u16);
                if row < button_y {
                    form.register(
                        WizardControl::RecentProject(index),
                        ControlKind::Button,
                        Rect::new(content.x, row, content.width, 1),
                        true,
                    );
                }
            }
        }
        PathField::render_within(
            frame,
            Rect::new(
                content.x,
                content.y,
                content.width,
                button_y.saturating_sub(content.y),
            ),
            Rect::new(content.x, field_y, content.width, 1.min(content.height)),
            &wizard.project_directory,
            &mut form,
            WizardControl::ProjectDirectory,
        );
        Dialog::render_actions(
            frame,
            mj_chat::components::DialogShell::layout(content, 0).actions,
            &[
                (WizardControl::Cancel, "Cancel", true),
                (WizardControl::Back, "Back", true),
                (WizardControl::Next, "Next", true),
            ],
            &mut form,
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
    if wizard.step == WizardStep::NewBundle {
        let popup_height = u16::try_from(wizard.new_bundle_repositories.len())
            .unwrap_or(u16::MAX)
            .saturating_add(8)
            .clamp(10, 24);
        let popup = centered_modal(frame, surfaces, 76, popup_height, area);
        let content = popup.inner(ratatui::layout::Margin {
            horizontal: 1,
            vertical: 1,
        });
        let title_line = dismissible_modal_title(
            &mut form,
            popup,
            "New bundle",
            theme::title(true),
            !wizard.bundle_creation_in_flight,
        );
        frame.render_widget(theme::modal().title(title_line), popup);
        frame.render_widget(
            Paragraph::new("Repositories (first is primary):"),
            Rect::new(content.x, content.y, content.width, 1.min(content.height)),
        );
        let list_y = content.y.saturating_add(1);
        let list_height = if wizard.new_bundle_repositories.is_empty() {
            1.min(content.height.saturating_sub(5))
        } else {
            u16::try_from(wizard.new_bundle_repositories.len())
                .unwrap_or(u16::MAX)
                .min(content.height.saturating_sub(6))
        };
        let list_area = Rect::new(content.x, list_y, content.width, list_height);
        if wizard.new_bundle_repositories.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    "No repositories added yet.",
                    Style::default().fg(theme::palette().muted),
                )),
                list_area,
            );
        } else {
            let rows = wizard
                .new_bundle_repositories
                .iter()
                .enumerate()
                .map(|(index, source)| {
                    if index == 0 {
                        Line::raw(format!("primary  {source}"))
                    } else {
                        Line::raw(format!("         {source}"))
                    }
                })
                .collect::<Vec<_>>();
            ChoiceList::render(
                frame,
                list_area,
                &rows,
                wizard.new_bundle_selected,
                &mut form,
                WizardControl::NewBundleRepositories,
            );
            if wizard.bundle_creation_in_flight {
                form.declare_with_enabled(
                    WizardControl::NewBundleRepositories,
                    ControlKind::ChoiceList {
                        len: wizard.new_bundle_repositories.len(),
                        selected: wizard.new_bundle_selected,
                    },
                    false,
                );
            }
        }
        let source_label_y = list_y.saturating_add(list_height);
        frame.render_widget(
            Paragraph::new("GitHub source or local Git path with a network remote:"),
            Rect::new(
                content.x,
                source_label_y,
                content.width,
                1.min(content.height),
            ),
        );
        // Two button rows sit at the foot of this dialog; the popup keeps off
        // both of them.
        let buttons_y = content.bottom().saturating_sub(2);
        PathField::render_within(
            frame,
            Rect::new(
                content.x,
                content.y,
                content.width,
                buttons_y.saturating_sub(content.y),
            ),
            Rect::new(
                content.x,
                source_label_y.saturating_add(1),
                content.width,
                1.min(content.height),
            ),
            &wizard.new_bundle_source,
            &mut form,
            WizardControl::NewBundleSource,
        );
        let help_y = content.bottom().saturating_sub(3);
        frame.render_widget(
            Paragraph::new(Line::styled(
                if wizard.bundle_creation_in_flight {
                    "Creating bundle…"
                } else {
                    "Enter adds · Delete removes · Tab moves focus · Esc cancels"
                },
                Style::default().fg(theme::palette().muted),
            )),
            Rect::new(content.x, help_y, content.width, 1.min(content.height)),
        );
        let action_enabled =
            !wizard.bundle_creation_in_flight && !wizard.new_bundle_source.trim().is_empty();
        Dialog::render_actions(
            frame,
            Rect::new(
                content.x,
                content.bottom().saturating_sub(2),
                content.width,
                1.min(content.height),
            ),
            &[
                (WizardControl::Add, "Add repository", action_enabled),
                (
                    WizardControl::NewBundleRemove,
                    "Remove selected repository",
                    !wizard.bundle_creation_in_flight && !wizard.new_bundle_repositories.is_empty(),
                ),
            ],
            &mut form,
        );
        Dialog::render_actions(
            frame,
            Rect::new(
                content.x,
                content.bottom().saturating_sub(1),
                content.width,
                1.min(content.height),
            ),
            &[
                (
                    WizardControl::Cancel,
                    "Cancel",
                    !wizard.bundle_creation_in_flight,
                ),
                (
                    WizardControl::Back,
                    "Back",
                    !wizard.bundle_creation_in_flight,
                ),
                (
                    WizardControl::Next,
                    if wizard.bundle_creation_in_flight {
                        "Creating…"
                    } else {
                        "Create bundle"
                    },
                    !wizard.bundle_creation_in_flight
                        && !wizard.new_bundle_sources_for_submit().is_empty(),
                ),
            ],
            &mut form,
        );
        form.end_frame(initial);
        return;
    }
    let (title, choices, selected): (_, Vec<PickerChoice>, _) = match wizard.step {
        WizardStep::Profile => (
            " New session · 1/4 profile ",
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
            " New session · 3/4 project bundle ",
            bundle_ids_by_recent_creation(&dashboard.config, &dashboard.state)
                .into_iter()
                .map(|id| {
                    let bundle = &dashboard.config.bundles[id];
                    PickerChoice::text(format!("{id}  {} repositories", bundle.repositories.len()))
                })
                .collect(),
            wizard.bundle,
        ),
        WizardStep::Target => (
            " New session · 2/4 target ",
            dashboard
                .config
                .targets
                .iter()
                .map(|(id, target)| {
                    let size = if id == &nth_key(&dashboard.config.targets, wizard.target) {
                        resource_allocation_label(
                            wizard.resource_allocation.as_ref(),
                            wizard.sizing_error.as_deref(),
                        )
                    } else {
                        String::new()
                    };
                    let label = format!("{id}  {}{size}", target_label(target));
                    match dashboard.target_readiness_rejection(id) {
                        Some(reason) => PickerChoice::disabled(format!("{label} · {reason}")),
                        None => PickerChoice::text(label),
                    }
                })
                .collect(),
            wizard.target,
        ),
        WizardStep::Review => unreachable!("review was rendered above"),
        WizardStep::Mounts => unreachable!("mount input was rendered above"),
        WizardStep::NewBundle => unreachable!("bundle input was rendered above"),
        WizardStep::ProjectDirectory => unreachable!("project directory input was rendered above"),
    };
    let mut help = vec![if wizard.step == WizardStep::Target {
        picker_help(&resource_help(dashboard))
    } else {
        picker_help("↑/↓ select · Tab moves focus · Enter activates")
    }];
    if wizard.step == WizardStep::Profile
        && dashboard
            .config
            .enabled_profiles()
            .any(|(_, profile)| needs_guardian_warning(profile.kind))
    {
        help.push(guardian_footnote());
    }
    render_picker(
        frame,
        area,
        title,
        choices,
        help,
        PickerNavigation {
            has_back: wizard.step != WizardStep::Profile,
            selected,
            control: match wizard.step {
                WizardStep::Profile => WizardControl::ProfileList,
                WizardStep::Bundle => WizardControl::BundleList,
                WizardStep::Target => WizardControl::TargetList,
                _ => unreachable!("picker step has a list control"),
            },
            next_enabled: match wizard.step {
                WizardStep::Target => {
                    dashboard
                        .target_readiness_rejection(&nth_key(
                            &dashboard.config.targets,
                            wizard.target,
                        ))
                        .is_none()
                        && (wizard.resource_allocation.is_some()
                            || !matches!(
                                dashboard
                                    .config
                                    .targets
                                    .get(&nth_key(&dashboard.config.targets, wizard.target)),
                                Some(TargetTemplate::AwsEc2 { .. })
                            ))
                }
                // Without a bundle there is nothing to review; the pinned
                // action is the only way forward.
                WizardStep::Bundle => !dashboard.config.bundles.is_empty(),
                _ => true,
            },
            pinned_action: (wizard.step == WizardStep::Bundle).then_some((
                WizardControl::Add,
                "New bundle…",
                true,
            )),
            empty_hint: (wizard.step == WizardStep::Bundle && dashboard.config.bundles.is_empty())
                .then_some("No bundles yet."),
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

pub(crate) struct ReviewWizardView<'a> {
    worktree: Option<(bool, bool)>,
    /// `Some(checked)` shows the Mjolnir sub-agent checkbox; `None` hides it.
    subagents: Option<bool>,
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
        subagents,
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
            Span::styled("Compute:", theme::muted()),
            Span::raw(resource_allocation_label(allocation, None)),
        ]),
    ];
    if moving && source_unavailable {
        lines.push(Line::styled(
            "Source is unavailable; Move will recover its saved data without starting its old harness.",
            Style::default().fg(theme::palette().warning),
        ));
    }
    if moving && in_place_move {
        lines.push(Line::styled(
            "Only the harness and profile are replaced; the environment and workspace are kept.",
            theme::muted(),
        ));
    }
    if moving && active_interruption {
        lines.push(Line::styled(
            if in_place_move {
                "Active work will be interrupted; the session keeps its environment."
            } else {
                "Active work will be interrupted; the session is restored into a fresh environment."
            },
            Style::default().fg(theme::palette().warning),
        ));
        if clear_resource_allocation {
            lines.push(Line::styled(
                "Fixed/default destination resources will replace the source sizing.",
                Style::default().fg(theme::palette().warning),
            ));
        }
    }
    if let Some(conversion) = conversion.filter(|_| moving) {
        lines.push(Line::raw(conversion.summary_line()));
        for warning in conversion.warning_lines() {
            lines.push(Line::styled(
                warning,
                Style::default().fg(theme::palette().warning),
            ));
        }
    }
    if moving {
        if preparing {
            lines.push(Line::styled(
                "Checking move destination…",
                Style::default().fg(theme::palette().muted),
            ));
        } else if let Some(error) = preparation_error {
            lines.push(Line::styled(
                format!("Move preparation failed: {error}"),
                Style::default().fg(theme::palette().error),
            ));
            lines.push(Line::styled(
                "Press Retry to check the destination again.",
                Style::default().fg(theme::palette().muted),
            ));
        }
    }
    if remote_preflight_in_flight {
        lines.push(Line::styled(
            "Checking prerequisites…",
            Style::default().fg(theme::palette().muted),
        ));
    } else if let Some(error) = remote_preflight_error {
        lines.push(Line::styled(
            format!("Prerequisite check failed: {error}"),
            Style::default().fg(theme::palette().error),
        ));
    } else if let Some(repositories) = remote_repositories {
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
        let row = lines.len() as u16;
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            if checked && available {
                "Create a separate session-owned checkout from the selected checkout's HEAD."
            } else {
                "Use the selected directory directly."
            },
            theme::muted(),
        ));
        row
    });
    let subagent_row = subagents.map(|checked| {
        let row = lines.len() as u16;
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            if checked {
                "Delegation goes to Mjolnir sub-agents that share this session's files."
            } else {
                "Unchecked keeps the harness's own Agent or spawn_agent tools."
            },
            theme::muted(),
        ));
        row
    });
    let queue_label = queue.map(|(count, _)| format!("Queued prompts: {count}"));
    if let Some(label) = &queue_label {
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
    }
    lines.push(Line::styled(
        if can_attach {
            "Tab moves focus · Enter edits selected directory · Delete removes it"
        } else {
            "Tab moves focus · Enter activates"
        },
        Style::default().fg(theme::palette().muted),
    ));
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
    let popup = centered_modal(
        frame,
        surfaces,
        84,
        (total_height.min(16) + 3).clamp(13, 26),
        area,
    );
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    let title_line = dismissible_modal_title(form, popup, title.trim(), theme::title(true), true);
    frame.render_widget(theme::modal().title(title_line), popup);
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let focused_row = match form.focused() {
        Some(WizardControl::CreateManagedWorktree) => worktree_row,
        Some(WizardControl::MjolnirSubagents) => subagent_row,
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
            "Create managed worktree",
            checked && available,
            available,
            form,
            WizardControl::CreateManagedWorktree,
        );
    }
    if let Some((checked, row)) = subagents.zip(subagent_row) {
        Checkbox::render(
            frame,
            viewport.row(row, 1),
            "Use Mjolnir sub-agents",
            checked,
            true,
            form,
            WizardControl::MjolnirSubagents,
        );
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
                "{} {count} queued command{} {}",
                if discard { "Discard" } else { "Start" },
                if count == 1 { "" } else { "s" },
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
    Dialog::render_actions(
        frame,
        mj_chat::components::DialogShell::layout(inner, 0).actions,
        &buttons,
        form,
    );
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
    let value = choices
        .get(selected)
        .map(|choice| ComboBox::display_value(access_description(*choice)))
        .unwrap_or_default();
    let options = choices
        .iter()
        .map(|choice| Line::raw(access_description(*choice)))
        .collect::<Vec<_>>();
    ComboBox::render(
        frame,
        bounds,
        field,
        &value,
        &options,
        selected,
        expanded,
        true,
        " access · ↑/↓ select · Enter accept ",
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
            "Recent sources (↑/↓ when Source is empty):",
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
    lines.push(Line::styled(
        "Ctrl-Space completes · Tab moves focus · Space toggles read-only · Enter continues/adds",
        Style::default().fg(theme::palette().muted),
    ));
    let info_height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let total_height = info_height.saturating_add(3);
    let popup = centered_modal(
        frame,
        surfaces,
        84,
        (total_height.min(16) + 3).clamp(13, 25),
        area,
    );
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    let title_line = dismissible_modal_title(form, popup, title.trim(), theme::title(true), true);
    frame.render_widget(theme::modal().title(title_line), popup);
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let focused_row = match form.focused() {
        Some(WizardControl::MountSource) => Some(info_height),
        Some(WizardControl::MountDestination) => Some(info_height.saturating_add(1)),
        Some(WizardControl::MountAccess) => Some(info_height.saturating_add(2)),
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
    let destination_row = viewport.row(info_height.saturating_add(1), 1);
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
    let access_row = viewport.row(info_height.saturating_add(2), 1);
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
        mj_chat::components::DialogShell::layout(inner, 0).actions,
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
    let mut form = wizard.form.borrow_mut();
    let initial = step_initial(wizard.step);
    begin_form_frame(&mut form, initial);
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
        let review_title = resume_wizard_title(wizard, "3/3 review", "3/3 confirm");
        render_review_wizard(
            frame,
            area,
            dashboard,
            ReviewWizardView {
                worktree: None,
                subagents: None,
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
                submit_enabled: !wizard.moving
                    || wizard.preparation.is_some()
                    || wizard.preparation_error.is_some(),
                source_unavailable: wizard
                    .preparation
                    .as_ref()
                    .is_some_and(|p| p.source_unavailable),
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
            let mut help = vec![
                picker_help("↑/↓ select · Tab moves focus · Enter activates"),
                picker_help("Lossy: text only; tool calls + reasoning dropped."),
            ];
            if profiles
                .iter()
                .any(|(_, harness)| needs_guardian_warning(*harness))
            {
                help.push(guardian_footnote());
            }
            (
                resume_wizard_title(
                    wizard,
                    "1/3 profile (cross-harness supported)",
                    "1/3 profile (cross-harness supported)",
                ),
                profile_table(rows),
                wizard.profile,
                help,
            )
        }
        WizardStep::Target => (
            resume_wizard_title(wizard, "2/3 new target", "2/3 new target"),
            dashboard
                .config
                .targets
                .iter()
                .map(|(id, target)| {
                    let size = if id == &nth_key(&dashboard.config.targets, wizard.target) {
                        resource_allocation_label(
                            wizard.resource_allocation.as_ref(),
                            wizard.sizing_error.as_deref(),
                        )
                    } else {
                        String::new()
                    };
                    match dashboard.resume_target_rejection(&wizard.session_id, id) {
                        Some(reason) => PickerChoice::disabled(format!(
                            "{id}  {}  · {reason}",
                            target_label(target)
                        )),
                        None => PickerChoice::text(format!("{id}  {}{size}", target_label(target))),
                    }
                })
                .collect(),
            wizard.target,
            vec![picker_help(&resource_help(dashboard))],
        ),
        WizardStep::Bundle => unreachable!("resume does not select a bundle"),
        WizardStep::Review => unreachable!("review was rendered above"),
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
            has_back: wizard.step != WizardStep::Profile,
            selected,
            control: match wizard.step {
                WizardStep::Profile => WizardControl::ProfileList,
                WizardStep::Target => WizardControl::TargetList,
                _ => unreachable!("resume picker step has a list control"),
            },
            next_enabled: wizard.step != WizardStep::Target
                || target_advance_enabled(dashboard, wizard),
            pinned_action: None,
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
