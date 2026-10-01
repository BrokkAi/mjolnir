//! Plays the startup splash while the dashboard loads in the background.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use mj_tui::splash::{SplashFrame, SplashTimeline, render_splash};
use ratatui::style::Color;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt as _;

use crate::daemon::StartupNoticeRoute;
use crate::dashboard::DashboardScreen;

/// About thirty frames a second. The animation follows the clock, so a
/// terminal that cannot keep up drops frames rather than slowing it down.
const FRAME: Duration = Duration::from_millis(33);

pub(crate) enum SplashOutcome<T> {
    Ready(T),
    Failed(anyhow::Error),
    /// The user pressed Ctrl+C, or the process was asked to stop.
    Cancelled,
}

/// Whether this launch shows the splash. It needs color, Unicode half
/// blocks, and the dashboard's minimum width. The configuration is not
/// loaded yet, so the symbol set is the terminal's own guess.
pub(crate) fn wanted() -> bool {
    !mj_chat::theme::no_color_requested()
        && mj_chat::theme::symbols_for(None) == mj_chat::theme::SymbolSet::Unicode
        && crossterm::terminal::size().is_ok_and(|(width, _)| mj_tui::splash::fits(width))
}

/// Animates until `loading` has finished and the splash has played out.
/// `background` names the color the splash dissolves into, from the loaded
/// value, so the dashboard's first frame lands on matching cells.
pub(crate) async fn play_while<T: Send + 'static>(
    screen: &mut DashboardScreen,
    mut loading: JoinHandle<Result<T>>,
    background: impl Fn(&T) -> Color,
) -> Result<SplashOutcome<T>> {
    let (_route, mut notices) = StartupNoticeRoute::open();
    let started = Instant::now();
    let mut timeline = SplashTimeline::default();
    let mut status = None::<String>;
    let mut loaded = None::<T>;
    let mut dissolve_into = Color::Reset;
    let mut frames = tokio::time::interval(FRAME);
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = frames.tick() => {
                let elapsed = started.elapsed();
                if timeline.finished(elapsed)
                    && let Some(value) = loaded.take()
                {
                    return Ok(SplashOutcome::Ready(value));
                }
                screen
                    .terminal
                    .terminal
                    .draw(|frame| {
                        let area = frame.area();
                        render_splash(
                            area,
                            frame.buffer_mut(),
                            &SplashFrame {
                                elapsed,
                                timeline,
                                status: status.as_deref(),
                                background: dissolve_into,
                            },
                        );
                    })
                    .context("draw the startup splash")?;
            }
            result = &mut loading, if !timeline.is_ready() => {
                match result.context("dashboard startup task failed").and_then(|loaded| loaded) {
                    Ok(value) => {
                        dissolve_into = background(&value);
                        loaded = Some(value);
                        timeline.mark_ready(started.elapsed());
                    }
                    Err(error) => return Ok(SplashOutcome::Failed(error)),
                }
            }
            Some(notice) = notices.recv() => status = Some(notice),
            () = screen.termination.cancelled() => {
                loading.abort();
                return Ok(SplashOutcome::Cancelled);
            }
            event = screen.events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('c'))
                    {
                        loading.abort();
                        return Ok(SplashOutcome::Cancelled);
                    }
                    timeline.skip(started.elapsed());
                }
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
