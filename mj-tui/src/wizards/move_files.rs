use super::*;
use mj_core::move_workspace::*;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MoveFilePicker {
    pub selection: WorkspaceSelection,
    pub expanded: BTreeSet<WorkspacePath>,
    pub show_other: bool,
    pub reviewed: bool,
}

impl MoveFilePicker {
    pub fn rows<'a>(&self, assessment: &'a WorkspaceAssessment) -> Vec<(&'a FileNode, usize)> {
        fn visit<'a>(
            picker: &MoveFilePicker,
            nodes: &'a [FileNode],
            depth: usize,
            rows: &mut Vec<(&'a FileNode, usize)>,
        ) {
            for (index, node) in nodes.iter().enumerate() {
                if !picker.show_other && (node.bytes < SUBSTANTIAL_ROOT_BYTES || index >= 8) {
                    continue;
                }
                rows.push((node, depth));
                if picker.expanded.contains(&node.location) {
                    visit(picker, &node.children, depth + 1, rows);
                }
            }
        }
        let mut rows = Vec::new();
        visit(self, &assessment.roots, 0, &mut rows);
        rows
    }

    pub fn hidden_bytes(&self, assessment: &WorkspaceAssessment) -> u64 {
        fn hidden(picker: &MoveFilePicker, nodes: &[FileNode]) -> u64 {
            nodes
                .iter()
                .enumerate()
                .map(|(index, node)| {
                    if !picker.show_other && (node.bytes < SUBSTANTIAL_ROOT_BYTES || index >= 8) {
                        node.bytes
                    } else if picker.expanded.contains(&node.location) {
                        hidden(picker, &node.children)
                    } else {
                        0
                    }
                })
                .sum()
        }
        hidden(self, &assessment.roots)
    }
}

pub(super) fn declare(wizard: &ResumeWizard, form: &mut Dialog<WizardControl>) {
    if let Some(assessment) = wizard
        .preparation
        .as_ref()
        .and_then(|p| p.workspace.as_ref())
    {
        for (index, (node, _)) in wizard.files.rows(assessment).iter().enumerate() {
            form.declare_with_enabled(WizardControl::MoveFile(index), ControlKind::Checkbox, true);
            if !node.children.is_empty() {
                form.declare_with_enabled(
                    WizardControl::ExpandMoveFile(index),
                    ControlKind::Button,
                    true,
                );
            }
        }
        form.declare_with_enabled(WizardControl::MoveOtherFiles, ControlKind::Button, true);
        super::dashboard::declare_wizard_buttons(
            form,
            true,
            assessment
                .selection_problem(&wizard.files.selection)
                .is_none(),
        );
    }
}

pub(super) fn render(
    frame: &mut Frame,
    area: Rect,
    wizard: &ResumeWizard,
    surfaces: &mut FrameSurfaces,
) {
    let Some(assessment) = wizard
        .preparation
        .as_ref()
        .and_then(|p| p.workspace.as_ref())
    else {
        return;
    };
    let popup = centered_modal(frame, surfaces, 100, 26, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    let mut form = wizard.form.borrow_mut();
    begin_form_frame(&mut form, WizardControl::Next);
    declare(wizard, &mut form);
    let problems = assessment
        .selection_problem(&wizard.files.selection)
        .into_iter()
        .collect::<Vec<_>>();
    let selected = wizard.files.selection.included_bytes(assessment);
    let all = WorkspaceSelection::default().included_bytes(assessment);
    let summary = vec![
        Line::raw("Choose files to transfer"),
        Line::raw(format!(
            "Transfer: {}   Leave behind: {}",
            format_bytes(selected),
            format_bytes(all.saturating_sub(selected))
        )),
        Line::raw(format!(
            "Tracked history and edits: {} (always included)",
            format_bytes(assessment.required_bytes)
        )),
        Line::raw(format!(
            "Not transferred: {} ignored files ({}) and {} credential files",
            assessment.ignored_files,
            format_bytes(assessment.ignored_bytes),
            assessment.credential_files
        )),
        Line::raw(if wizard.files.selection.exclusions.is_empty() {
            "All eligible files are included. Continue accepts this transfer."
        } else {
            "Excluded files stay in the stopped source until explicit cleanup."
        }),
        Line::raw("Space toggles a checkbox · Tab changes focus · Expand opens a directory"),
    ];
    let rows = wizard.files.rows(assessment);
    let focus_row = match form.focused() {
        Some(WizardControl::MoveFile(index) | WizardControl::ExpandMoveFile(index)) => {
            Some((summary.len() + index) as u16)
        }
        Some(WizardControl::MoveOtherFiles) => Some((summary.len() + rows.len()) as u16),
        _ => None,
    };
    let body = Rect {
        height: inner.height.saturating_sub(2),
        ..inner
    };
    let viewport = FormViewport::new(
        body,
        (summary.len() + rows.len() + 1 + problems.len()) as u16,
        0,
        focus_row,
    );
    let summary_rows = summary.len();
    for (index, line) in summary.into_iter().enumerate() {
        frame.render_widget(Paragraph::new(line), viewport.row(index as u16, 1));
    }
    for (index, (node, depth)) in rows.iter().enumerate() {
        let row = viewport.row((summary_rows + index) as u16, 1);
        let state = node.state(assessment, &wizard.files.selection);
        let label = format!(
            "{}{}{}  {}/{}{}",
            "  ".repeat(*depth),
            if state == FileSelectionState::Mixed {
                "[partial] "
            } else {
                ""
            },
            format_bytes(node.bytes),
            node.location.repository,
            node.location.path.display(),
            if node.children.is_empty() { "" } else { "/" }
        );
        Checkbox::render(
            frame,
            Rect {
                width: row.width.saturating_sub(12),
                ..row
            },
            &label,
            state == FileSelectionState::Included,
            true,
            &mut form,
            WizardControl::MoveFile(index),
        );
        if !node.children.is_empty() {
            Dialog::render_actions(
                frame,
                Rect {
                    x: row.x + row.width.saturating_sub(12),
                    width: 12,
                    ..row
                },
                &[(
                    WizardControl::ExpandMoveFile(index),
                    if wizard.files.expanded.contains(&node.location) {
                        "Collapse"
                    } else {
                        "Expand"
                    },
                    true,
                )],
                &mut form,
            );
        }
    }
    let other = format!(
        "{} — {}",
        if wizard.files.show_other {
            "Group smaller files"
        } else {
            "Other files: expand"
        },
        format_bytes(wizard.files.hidden_bytes(assessment))
    );
    Dialog::render_actions(
        frame,
        viewport.row((summary_rows + rows.len()) as u16, 1),
        &[(WizardControl::MoveOtherFiles, other.as_str(), true)],
        &mut form,
    );
    for (index, blocker) in problems.iter().enumerate() {
        frame.render_widget(
            Paragraph::new(blocker.as_str()).style(theme::muted()),
            viewport.row((summary_rows + 1 + rows.len() + index) as u16, 1),
        );
    }
    Dialog::render_actions(
        frame,
        Rect {
            y: inner.y + inner.height.saturating_sub(1),
            height: 1,
            ..inner
        },
        &[
            (WizardControl::Back, "Back", true),
            (
                WizardControl::Next,
                "Continue",
                assessment
                    .selection_problem(&wizard.files.selection)
                    .is_none(),
            ),
        ],
        &mut form,
    );
    form.end_frame(WizardControl::Next);
}
