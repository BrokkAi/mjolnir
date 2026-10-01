//! Plays the startup splash while the dashboard starts behind it.
//!
//! The splash runs in two stages over one [`SplashPlayback`]. Until the store
//! has loaded there is no dashboard yet, so [`play_until_loaded`] owns the
//! screen. From then on the dashboard's own loop keeps drawing the splash
//! while its background work (session summaries, the startup conversation)
//! is already running, and ends it once the playback has finished.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use mj_tui::splash::{SplashFrame, SplashTimeline, render_splash};
use ratatui::Frame;
use ratatui::style::Color;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt as _;

use crate::daemon::StartupNoticeRoute;
use crate::dashboard::DashboardScreen;

/// About thirty frames a second. The animation follows the clock, so a
/// terminal that cannot keep up drops frames rather than slowing it down.
pub(crate) const FRAME: Duration = Duration::from_millis(33);

pub(crate) enum SplashOutcome<T> {
    Ready(T),
    Failed(anyhow::Error),
    /// The user pressed Ctrl+C, or the process was asked to stop. Raw mode
    /// turns Ctrl+C into a key, so this is how startup stays interruptible.
    Cancelled,
}

/// Whether this launch shows the splash. It needs color, Unicode half
/// blocks, and the dashboard's minimum width. The symbol set is the one the
/// configuration names; with no setting, or a config that does not load
/// (normal startup reports that), it is the terminal's own guess.
pub(crate) fn wanted() -> bool {
    !mj_chat::theme::no_color_requested()
        && symbols_for_splash(&mj_core::config::config_path()) == mj_chat::theme::SymbolSet::Unicode
        && crossterm::terminal::size().is_ok_and(|(width, _)| mj_tui::splash::fits(width))
}

fn symbols_for_splash(config_path: &std::path::Path) -> mj_chat::theme::SymbolSet {
    let configured = mj_core::config::Config::load_from(config_path)
        .ok()
        .and_then(|config| config.advanced.symbols);
    mj_chat::theme::symbols_for(configured)
}

/// One showing of the splash: its clock, and what it waits for.
pub(crate) struct SplashPlayback {
    started: Instant,
    timeline: SplashTimeline,
    status: Option<String>,
    background: Color,
}

impl SplashPlayback {
    fn start() -> Self {
        Self {
            started: Instant::now(),
            timeline: SplashTimeline::default(),
            status: None,
            background: Color::Reset,
        }
    }

    /// The dashboard can draw; the splash dissolves into `background` once
    /// it has played out.
    fn mark_ready(&mut self, background: Color) {
        self.background = background;
        self.timeline.mark_ready(self.started.elapsed());
    }

    pub(crate) fn finished(&self) -> bool {
        self.timeline.finished(self.started.elapsed())
    }

    pub(crate) fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        render_splash(
            area,
            frame.buffer_mut(),
            &SplashFrame {
                elapsed: self.started.elapsed(),
                timeline: self.timeline,
                status: self.status.as_deref(),
                background: self.background,
            },
        );
    }
}

fn is_interrupt(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(key)
            if key.kind == KeyEventKind::Press
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && key.code == KeyCode::Char('c')
    )
}

/// Animates until `loading` finishes, then hands back its value and the
/// playback, still running, for the dashboard to finish. `background`
/// names the color the splash dissolves into, from the loaded value.
pub(crate) async fn play_until_loaded<T: Send + 'static>(
    screen: &mut DashboardScreen,
    mut loading: JoinHandle<Result<T>>,
    background: impl Fn(&T) -> Color,
) -> Result<SplashOutcome<(T, SplashPlayback)>> {
    let (_route, mut notices) = StartupNoticeRoute::open();
    let mut playback = SplashPlayback::start();
    let mut frames = tokio::time::interval(FRAME);
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = frames.tick() => {
                screen
                    .terminal
                    .terminal
                    .draw(|frame| playback.render(frame))
                    .context("draw the startup splash")?;
            }
            result = &mut loading => {
                return Ok(
                    match result.context("dashboard startup task failed").and_then(|loaded| loaded) {
                        Ok(value) => {
                            playback.mark_ready(background(&value));
                            SplashOutcome::Ready((value, playback))
                        }
                        Err(error) => SplashOutcome::Failed(error),
                    },
                );
            }
            Some(notice) = notices.recv() => playback.status = Some(notice),
            () = screen.termination.cancelled() => {
                loading.abort();
                return Ok(SplashOutcome::Cancelled);
            }
            event = screen.events.next() => match event {
                Some(Ok(event)) if is_interrupt(&event) => {
                    loading.abort();
                    return Ok(SplashOutcome::Cancelled);
                }
                // There is no dashboard yet to take other input.
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    loading.abort();
                    return Err(error).context("read terminal input during the startup splash");
                }
                None => {
                    loading.abort();
                    anyhow::bail!("terminal input closed during the startup splash");
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_chat::theme::SymbolSet;

    #[test]
    fn configured_ascii_symbols_skip_the_splash_and_unreadable_config_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                "version = {}\n[advanced]\nsymbols = \"ascii\"\n",
                mj_core::config::CONFIG_VERSION
            ),
        )
        .unwrap();
        assert_eq!(symbols_for_splash(&path), SymbolSet::Ascii);
        std::fs::write(
            &path,
            format!(
                "version = {}\n[advanced]\nsymbols = \"unicode\"\n",
                mj_core::config::CONFIG_VERSION
            ),
        )
        .unwrap();
        assert_eq!(symbols_for_splash(&path), SymbolSet::Unicode);
        std::fs::write(&path, "not [valid toml").unwrap();
        assert_eq!(symbols_for_splash(&path), mj_chat::theme::symbols_for(None));
    }
}
