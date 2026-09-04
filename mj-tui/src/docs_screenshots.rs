//! Deterministic documentation captures rendered by the real terminal UI.
//!
//! The test is ignored because it writes committed documentation assets. Run
//! it explicitly whenever the terminal surface changes:
//!
//!     cargo test -p brokk-mj-tui generate_documentation_screenshots -- --ignored --nocapture

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use hel::hel_config::{HarnessKind, HarnessProfile, ProjectBundle, ProjectRepository};
use hel::hel_state::{HelState, MaterializedExecutionState, STATE_VERSION, SessionState};
use hel::hel_targets::{DeploymentCapacityKind, DeploymentCapacityTarget, DeploymentCapacityUsage};
use mj_controller::hel_quota::{ProfileQuota, QuotaWindow};

use crate::render::render;
use crate::test_support::{
    agent_message, alt_key, config, key, materialized_session_for, running_session, transcript_item,
};
use crate::{DashboardState, PaneSize, SupportPane};

const COLUMNS: u16 = 140;
const ROWS: u16 = 42;
const CELL_WIDTH: u16 = 9;
const CELL_HEIGHT: u16 = 18;
const PADDING: u16 = 14;
const TERMINAL_BACKGROUND: &str = "#09070e";

#[test]
#[ignore = "writes the committed documentation screenshots"]
fn generate_documentation_screenshots() {
    let output = documentation_screenshot_directory();
    fs::create_dir_all(&output).expect("create documentation screenshot directory");

    let mut dashboard = documentation_dashboard();
    capture(
        &output.join("dashboard.svg"),
        "Mjolnir terminal dashboard",
        "The current Mjolnir terminal surface with active sessions, the conversation and prompt regions, target capacity, quota, and the contextual key footer.",
        &mut dashboard,
    );

    let mut wizard = documentation_dashboard();
    wizard.handle_key(alt_key('n'));
    capture(
        &output.join("new-session.svg"),
        "Mjolnir new-session wizard",
        "The current Mjolnir new-session wizard over the terminal dashboard, at the profile-selection step.",
        &mut wizard,
    );

    let mut palette = documentation_dashboard();
    palette.handle_key(key(KeyCode::F(2)));
    capture(
        &output.join("command-palette.svg"),
        "Mjolnir command palette",
        "The current Mjolnir command palette over the terminal dashboard, with session, pane, and global commands grouped together.",
        &mut palette,
    );

    println!("wrote documentation screenshots to {}", output.display());
}

fn documentation_screenshot_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("mj-tui belongs to the workspace")
        .join("docs/src/assets/screenshots")
}

fn documentation_dashboard() -> DashboardState {
    let mut config = config();
    config.bundles = BTreeMap::from([(
        "mjolnir".into(),
        ProjectBundle {
            primary_repo: "mjolnir".into(),
            repositories: vec![ProjectRepository {
                id: "mjolnir".into(),
                github: Some("BrokkAi/mjolnir".into()),
                local: None,
                destination: PathBuf::from("mjolnir"),
                git_ref: None,
            }],
        },
    )]);
    config.profiles.insert(
        "kimi-1".into(),
        HarnessProfile {
            kind: HarnessKind::Kimi,
            home: PathBuf::from("/profiles/kimi"),
            environment: BTreeMap::new(),
            context_window_bytes: None,
        },
    );

    let now = chrono::Utc::now().to_rfc3339();
    let mut sessions = BTreeMap::new();
    for (index, (id, title, profile, harness, target)) in [
        (
            "docs-control-plane",
            "Document the v2 control plane",
            "codex-1",
            HarnessKind::Codex,
            "podman",
        ),
        (
            "recovery-audit",
            "Audit checkpoint recovery",
            "claude-1",
            HarnessKind::Claude,
            "podman",
        ),
        (
            "target-guides",
            "Expand the target guides",
            "kimi-1",
            HarnessKind::Kimi,
            "podman",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut session = running_session();
        session.id = id.into();
        session.title = title.into();
        session.acp_session_title = None;
        session.session_title_override = Some(title.into());
        session.harness_kind = harness;
        session.last_profile = profile.into();
        session.bundle_id = "mjolnir".into();
        session.target_template_id = target.into();
        session.native_session_id = Some(format!("native-{index}"));
        session.created_at.clone_from(&now);
        session.updated_at.clone_from(&now);
        session.state = SessionState::Running;
        sessions.insert(session.id.clone(), session);
    }

    let refreshed = now_epoch_seconds();
    let quotas = BTreeMap::from([
        (
            "claude-1".into(),
            profile_quota("claude-1", HarnessKind::Claude, 73, 84, refreshed),
        ),
        (
            "codex-1".into(),
            profile_quota("codex-1", HarnessKind::Codex, 61, 92, refreshed),
        ),
        (
            "codex-2".into(),
            profile_quota("codex-2", HarnessKind::Codex, 38, 77, refreshed),
        ),
        (
            "kimi-1".into(),
            ProfileQuota {
                profile_id: "kimi-1".into(),
                harness: HarnessKind::Kimi,
                windows: Vec::new(),
                extra: Some("API".into()),
                error: None,
                refreshed_at_epoch_seconds: refreshed,
            },
        ),
    ]);
    let mut dashboard = DashboardState::new(
        config,
        HelState {
            version: STATE_VERSION,
            sessions,
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        quotas,
    );
    dashboard.set_workspace_name("Mjolnir docs".into());
    dashboard.select_active_session("docs-control-plane");
    dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Maximized);

    apply_documentation_transcript(
        &mut dashboard,
        "docs-control-plane",
        vec![
            transcript_item(
                1,
                hel::hel_state::TranscriptBody::User {
                    content: vec![serde_json::json!({
                        "type": "text",
                        "text": "Rebuild the documentation around the current control plane."
                    })],
                },
            ),
            agent_message(
                2,
                "The guide hierarchy is mapped; I am validating every configuration field now.",
            ),
        ],
        true,
        refreshed,
    );
    apply_documentation_transcript(
        &mut dashboard,
        "recovery-audit",
        vec![agent_message(
            1,
            "Checkpoint verification and fresh-target resume paths are covered.",
        )],
        true,
        refreshed,
    );
    apply_documentation_transcript(
        &mut dashboard,
        "target-guides",
        vec![agent_message(
            1,
            "Podman, Docker, SSH, Apple container, and EC2 guides are linked.",
        )],
        true,
        refreshed,
    );

    dashboard.set_deployment_capacity_targets(vec![DeploymentCapacityTarget {
        id: "local".into(),
        host: "local".into(),
        target_ids: vec!["podman".into()],
        kind: DeploymentCapacityKind::Host,
        local: true,
        probes: Vec::new(),
        probe_error: None,
    }]);
    dashboard.apply_deployment_capacity(
        "local",
        Ok(Some(DeploymentCapacityUsage {
            cpu_percent: Some(27),
            memory_used_bytes: 19 * 1024 * 1024 * 1024,
            memory_total_bytes: 64 * 1024 * 1024 * 1024,
            logical_cores: 16,
            disk_total_bytes: Some(1_000 * 1024 * 1024 * 1024),
        })),
        refreshed,
    );
    dashboard
}

fn apply_documentation_transcript(
    dashboard: &mut DashboardState,
    session_id: &str,
    transcript: Vec<std::sync::Arc<hel::hel_state::TranscriptItem>>,
    running: bool,
    now_epoch_seconds: u64,
) {
    let now_ms = i64::try_from(now_epoch_seconds).unwrap_or(i64::MAX / 1_000) * 1_000;
    let mut materialized = materialized_session_for(session_id, transcript);
    materialized.execution = if running {
        MaterializedExecutionState::Running {
            started_at_ms: now_ms - 74_000,
        }
    } else {
        MaterializedExecutionState::Idle
    };
    materialized.last_activity_at_ms = Some(now_ms - 3_000);
    dashboard.apply_materialized_session(&materialized);
}

fn profile_quota(
    profile_id: &str,
    harness: HarnessKind,
    weekly_remaining: u8,
    five_hour_remaining: u8,
    refreshed: u64,
) -> ProfileQuota {
    ProfileQuota {
        profile_id: profile_id.into(),
        harness,
        windows: vec![
            QuotaWindow {
                label: "Weekly".into(),
                remaining_percent: Some(weekly_remaining),
                used: None,
                limit: None,
                resets: Some("4d".into()),
                resets_at_epoch_seconds: None,
            },
            QuotaWindow {
                label: "5h".into(),
                remaining_percent: Some(five_hour_remaining),
                used: None,
                limit: None,
                resets: Some("2h14m".into()),
                resets_at_epoch_seconds: None,
            },
        ],
        extra: None,
        error: None,
        refreshed_at_epoch_seconds: refreshed,
    }
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn capture(path: &Path, title: &str, description: &str, dashboard: &mut DashboardState) {
    let mut terminal = Terminal::new(TestBackend::new(COLUMNS, ROWS)).expect("test terminal");
    terminal
        .draw(|frame| render(frame, dashboard))
        .expect("render documentation frame");
    let svg = buffer_svg(terminal.backend().buffer(), title, description);
    fs::write(path, svg).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}

fn buffer_svg(buffer: &Buffer, title: &str, description: &str) -> String {
    let width = buffer.area.width * CELL_WIDTH + PADDING * 2;
    let height = buffer.area.height * CELL_HEIGHT + PADDING * 2;
    let mut svg = String::new();
    writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-labelledby="title description">"#
    )
    .unwrap();
    writeln!(svg, "  <title id=\"title\">{}</title>", xml_escape(title)).unwrap();
    writeln!(
        svg,
        "  <desc id=\"description\">{}</desc>",
        xml_escape(description)
    )
    .unwrap();
    writeln!(
        svg,
        "  <rect width=\"100%\" height=\"100%\" rx=\"8\" fill=\"{TERMINAL_BACKGROUND}\"/>"
    )
    .unwrap();
    writeln!(
        svg,
        "  <g font-family=\"JetBrains Mono, Menlo, Consolas, monospace\" font-size=\"14\" font-variant-ligatures=\"none\">"
    )
    .unwrap();

    for y in buffer.area.y..buffer.area.bottom() {
        for x in buffer.area.x..buffer.area.right() {
            let cell = &buffer[(x, y)];
            let reversed = cell.modifier.contains(Modifier::REVERSED);
            let foreground = if reversed { cell.bg } else { cell.fg };
            let background = if reversed { cell.fg } else { cell.bg };
            let draw_x = PADDING + (x - buffer.area.x) * CELL_WIDTH;
            let draw_y = PADDING + (y - buffer.area.y) * CELL_HEIGHT;

            if background != Color::Reset {
                writeln!(
                    svg,
                    "    <rect x=\"{draw_x}\" y=\"{draw_y}\" width=\"{CELL_WIDTH}\" height=\"{CELL_HEIGHT}\" fill=\"{}\"/>",
                    color_hex(background, TERMINAL_BACKGROUND)
                )
                .unwrap();
            }

            let symbol = cell.symbol();
            if symbol.trim().is_empty() {
                continue;
            }
            let mut attributes = String::new();
            if cell.modifier.contains(Modifier::BOLD) {
                attributes.push_str(" font-weight=\"700\"");
            }
            if cell.modifier.contains(Modifier::DIM) {
                attributes.push_str(" opacity=\"0.62\"");
            }
            if cell.modifier.contains(Modifier::ITALIC) {
                attributes.push_str(" font-style=\"italic\"");
            }
            if cell.modifier.contains(Modifier::UNDERLINED) {
                attributes.push_str(" text-decoration=\"underline\"");
            }
            writeln!(
                svg,
                "    <text x=\"{draw_x}\" y=\"{}\" fill=\"{}\"{attributes}>{}</text>",
                draw_y + 14,
                color_hex(foreground, "#d8d4df"),
                xml_escape(symbol)
            )
            .unwrap();
        }
    }
    svg.push_str("  </g>\n</svg>\n");
    svg
}

fn color_hex(color: Color, fallback: &'static str) -> &'static str {
    match color {
        Color::Reset => fallback,
        Color::Black => "#09070e",
        Color::Red => "#ff6b6b",
        Color::Green => "#64d98b",
        Color::Yellow => "#f2c94c",
        Color::Blue => "#69a7ff",
        Color::Magenta => "#c792ff",
        Color::Cyan => "#70d7e8",
        Color::Gray => "#b8adc9",
        Color::DarkGray => "#71677f",
        Color::LightRed => "#ff9292",
        Color::LightGreen => "#8ee8aa",
        Color::LightYellow => "#ffe184",
        Color::LightBlue => "#9bc4ff",
        Color::LightMagenta => "#d9b5ff",
        Color::LightCyan => "#a5ecf5",
        Color::White => "#f4f0fa",
        Color::Indexed(_) | Color::Rgb(_, _, _) => fallback,
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
