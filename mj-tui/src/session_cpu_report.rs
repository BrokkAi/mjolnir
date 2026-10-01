//! A live report built from the same CPU table as the session rows.
use crate::DashboardState;
use mj_chat::theme;
use mj_client::runtime_feed::SessionCpuView;
use mj_client::usage_format::format_cpu_permille;
use mj_core::state::{SessionRecord, live_session_ids};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::collections::BTreeMap;

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
    let hourly = |session: &SessionRecord| match dashboard.session_cpu.get(&session.id) {
        Some(SessionCpuView::Measured { usage }) => Some(usage.hourly_permille),
        _ => None,
    };
    let mut groups: Vec<_> = groups
        .into_iter()
        .map(|(machine, mut sessions)| {
            sessions.sort_by(|a, b| hourly(b).cmp(&hourly(a)).then_with(|| a.id.cmp(&b.id)));
            let sum: u32 = sessions
                .iter()
                .filter_map(|session| hourly(session))
                .map(u32::from)
                .sum();
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
    for (machine, sessions, sum) in groups {
        lines.push(Line::styled(
            format!(
                "{machine}  {} hourly",
                format_cpu_permille(sum.min(u32::from(u16::MAX)) as u16)
            ),
            Style::default()
                .fg(theme::palette().text)
                .add_modifier(Modifier::BOLD),
        ));
        for session in sessions {
            let mut spans = vec![Span::raw(format!(
                "  {}  [{}]  ",
                crate::render::session_name(session),
                session.last_profile
            ))];
            match dashboard.session_cpu.get(&session.id) {
                Some(SessionCpuView::Measured { usage }) => {
                    spans.push(Span::raw(format!(
                        "{} hourly / {} recent",
                        format_cpu_permille(usage.hourly_permille),
                        format_cpu_permille(usage.recent_permille)
                    )));
                    if usage.hourly_covered_secs < 3600 {
                        let seconds = usage.hourly_covered_secs;
                        let coverage = if seconds < 60 {
                            format!("{seconds}s")
                        } else {
                            format!("{}m", seconds / 60)
                        };
                        spans.push(Span::styled(format!(" ({coverage})"), theme::muted()));
                    }
                }
                Some(SessionCpuView::Unavailable { reason }) => spans.push(Span::styled(
                    format!("CPU unavailable: {reason}"),
                    theme::muted(),
                )),
                None => spans.push(Span::styled("no CPU data yet", theme::muted())),
            }
            lines.push(Line::from(spans));
        }
    }
    lines
}
