//! A live report built from the same CPU table as the session rows.
//!
//! A parent's line shows its tree total (itself plus its live sub-agents,
//! from [`DashboardState::cpu_rollup`]) and its own share; the sub-agents are
//! indented below it.
use crate::DashboardState;
use crate::session_view::clamp_permille;
use mj_chat::theme;
use mj_client::runtime_feed::SessionCpuView;
use mj_client::usage_format::format_cpu_permille;
use mj_core::state::{SessionRecord, live_session_ids};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn report_lines(dashboard: &DashboardState) -> Vec<Line<'static>> {
    let operations = dashboard.session_operations.keys().cloned().collect();
    let live = live_session_ids(
        &dashboard.state.sessions,
        &dashboard.state.subagents,
        &operations,
    );
    let mut groups = BTreeMap::<String, Vec<&SessionRecord>>::new();
    for session in dashboard
        .state
        .sessions
        .values()
        .filter(|session| live.contains(&session.id))
    {
        let machine = dashboard
            .capacity_details
            .values()
            .find(|detail| {
                detail
                    .target
                    .target_ids
                    .contains(&session.target_template_id)
            })
            .map(|detail| detail.target.host.clone())
            .unwrap_or_else(|| {
                crate::render::session_target_label(
                    &dashboard.state,
                    session,
                    None,
                    &dashboard.config,
                )
            });
        groups.entry(machine).or_default().push(session);
    }
    let own_hourly = |session: &SessionRecord| match dashboard.session_cpu.get(&session.id) {
        Some(SessionCpuView::Measured { usage }) => Some(usage.hourly_permille),
        _ => None,
    };
    let tree_hourly = |session: &SessionRecord| dashboard.cpu_rollup(&session.id).hourly_permille;
    // A live sub-agent is drawn under its parent when the parent is listed.
    let listed: BTreeSet<&str> = groups
        .values()
        .flatten()
        .map(|session| session.id.as_str())
        .collect();
    let nested: BTreeSet<String> = listed
        .iter()
        .flat_map(|id| dashboard.cpu_children(id))
        .filter(|child| listed.contains(child.as_str()))
        .collect();
    let mut groups: Vec<_> = groups
        .into_iter()
        .map(|(machine, mut sessions)| {
            // The machine total sums each session's own share once.
            let sum: u32 = sessions
                .iter()
                .filter_map(|session| own_hourly(session))
                .map(u32::from)
                .sum();
            sessions.retain(|session| !nested.contains(&session.id));
            sessions.sort_by(|a, b| {
                tree_hourly(b)
                    .cmp(&tree_hourly(a))
                    .then_with(|| a.id.cmp(&b.id))
            });
            (machine, sessions, sum)
        })
        .collect();
    groups.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    let mut lines = Vec::new();
    if !groups.is_empty() {
        lines.push(Line::styled(
            "Machine CPU share · hourly average / recent",
            theme::muted(),
        ));
    }
    for (index, (machine, sessions, sum)) in groups.into_iter().enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        lines.push(Line::styled(
            format!(
                "{machine}  {} hourly",
                format_cpu_permille(clamp_permille(sum))
            ),
            Style::default()
                .fg(theme::palette().text)
                .add_modifier(Modifier::BOLD),
        ));
        // Each root with its nested sub-agents is a block; one empty line
        // separates blocks.
        for (index, session) in sessions.into_iter().enumerate() {
            if index > 0 {
                lines.push(Line::default());
            }
            push_tree(dashboard, session, &listed, 1, &mut lines);
        }
    }
    lines
}

/// How one session's CPU reads, in the report and in the session menu.
pub(crate) struct CpuSummary {
    pub(crate) main: String,
    /// True when `main` is a statement about missing data, not a figure.
    pub(crate) muted: bool,
    /// Context for `main`: tree membership, own share and coverage for a
    /// parent, or the covered period of a short history.
    pub(crate) detail: Option<String>,
    /// True for the tree breakdown, which is too long to share a line.
    pub(crate) tree: bool,
}

impl CpuSummary {
    pub(crate) fn main_span(&self) -> Span<'static> {
        if self.muted {
            Span::styled(self.main.clone(), theme::muted())
        } else {
            Span::raw(self.main.clone())
        }
    }
}

/// The one wording of a session's CPU, read from the rollup owner.
pub(crate) fn cpu_summary(dashboard: &DashboardState, session_id: &str) -> CpuSummary {
    let rollup = dashboard.cpu_rollup(session_id);
    let own = dashboard.session_cpu.get(session_id);
    if rollup.has_descendants() && rollup.measured > 0 {
        let marker = if rollup.is_partial() { "+" } else { "" };
        let own_text = match own {
            Some(SessionCpuView::Measured { usage }) => {
                format!("own {}", format_cpu_permille(usage.recent_permille))
            }
            _ => "own unmeasured".to_owned(),
        };
        let measured = if rollup.is_partial() {
            format!(", {} of {} measured", rollup.measured, rollup.members)
        } else {
            String::new()
        };
        return CpuSummary {
            main: format!(
                "{}{marker} hourly / {}{marker} recent",
                format_cpu_permille(clamp_permille(rollup.hourly_permille)),
                format_cpu_permille(clamp_permille(rollup.recent_permille)),
            ),
            muted: false,
            detail: Some(format!("tree of {}; {own_text}{measured}", rollup.members)),
            tree: true,
        };
    }
    match own {
        Some(SessionCpuView::Measured { usage }) => {
            let seconds = usage.hourly_covered_secs;
            let detail = (seconds < 3600).then(|| {
                if seconds < 60 {
                    format!("{seconds}s")
                } else {
                    format!("{}m", seconds / 60)
                }
            });
            CpuSummary {
                main: format!(
                    "{} hourly / {} recent",
                    format_cpu_permille(usage.hourly_permille),
                    format_cpu_permille(usage.recent_permille)
                ),
                muted: false,
                detail,
                tree: false,
            }
        }
        Some(SessionCpuView::Unavailable { reason }) => CpuSummary {
            main: format!("CPU unavailable: {reason}"),
            muted: true,
            detail: None,
            tree: false,
        },
        None => CpuSummary {
            main: "no CPU data yet".to_owned(),
            muted: true,
            detail: None,
            tree: false,
        },
    }
}

/// One session's line, then its live sub-agents indented below it.
fn push_tree(
    dashboard: &DashboardState,
    session: &SessionRecord,
    listed: &BTreeSet<&str>,
    depth: usize,
    lines: &mut Vec<Line<'static>>,
) {
    let summary = cpu_summary(dashboard, &session.id);
    let mut spans = vec![
        Span::raw(format!(
            "{}{}  [{}]  ",
            "  ".repeat(depth),
            crate::render::session_name(session),
            session.last_profile
        )),
        summary.main_span(),
    ];
    if let Some(detail) = &summary.detail {
        spans.push(Span::styled(format!(" ({detail})"), theme::muted()));
    }
    lines.push(Line::from(spans));
    let mut children: Vec<&SessionRecord> = dashboard
        .cpu_children(&session.id)
        .iter()
        .filter(|child| listed.contains(child.as_str()))
        .filter_map(|child| dashboard.state.sessions.get(child))
        .collect();
    let subtree_hourly = |s: &SessionRecord| dashboard.cpu_rollup(&s.id).hourly_permille;
    children.sort_by(|a, b| {
        subtree_hourly(b)
            .cmp(&subtree_hourly(a))
            .then_with(|| a.id.cmp(&b.id))
    });
    for child in children {
        push_tree(dashboard, child, listed, depth + 1, lines);
    }
}
