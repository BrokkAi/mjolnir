//! Shared fixtures for the chat module's tests.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::hel_state::{QueuedCommandKind, TranscriptBody, TranscriptItem};
use crate::hel_worker::WorkerSnapshot;
use agent_client_protocol::schema::v1::{
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    SessionConfigSelectOptions, SessionMode, SessionModeState,
};

use super::active::render_full_frame as render;
use super::transcript::transcript_lines;
use super::{ChatState, QueuedPrompt};

pub(super) fn snapshot() -> WorkerSnapshot {
    serde_json::from_value(serde_json::json!({
        "session_id": "1234567890",
        "phase": "idle",
        "latest_seq": 0,
        "last_checkpoint_seq": null,
        "active_prompt": null,
        "config": {},
        "handled_requests": {}
    }))
    .unwrap()
}

pub(super) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(super) fn ctrl(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
}

/// An Alt chord. Alt is one modifier everywhere, so it never goes through
/// [`ctrl`].
pub(super) fn alt(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::ALT)
}

/// A wheel event over `area`, which is what decides where it scrolls.
pub(super) fn mouse_in(kind: MouseEventKind, area: Rect) -> MouseEvent {
    MouseEvent {
        kind,
        column: area.x.saturating_add(1),
        row: area.y.saturating_add(1),
        modifiers: KeyModifiers::NONE,
    }
}

pub(super) fn queued(id: &str, text: &str) -> QueuedPrompt {
    QueuedPrompt {
        id: id.into(),
        text: text.into(),
        kind: QueuedCommandKind::Prompt,
    }
}

pub(super) fn transcript_text(chat: &mut ChatState, width: u16) -> Vec<String> {
    transcript_lines(chat, width)
        .into_iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect()
}

pub(super) fn line_text(lines: Vec<Line<'static>>) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect()
}

/// Draw the chat and return the transcript rows currently on screen.
pub(super) fn drawn_transcript(chat: &mut ChatState, width: u16, height: u16) -> Vec<String> {
    drawn_transcript_selecting(chat, width, height, false)
}

/// Like `drawn_transcript`, but says whether the selection engine still owns a
/// transcript selection, which is what freezes the transcript's row space.
pub(super) fn drawn_transcript_selecting(
    chat: &mut ChatState,
    width: u16,
    height: u16,
    transcript_selected: bool,
) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| render(frame, chat, transcript_selected))
        .expect("draw chat");
    let buffer = terminal.backend().buffer();
    (buffer.area.y..buffer.area.bottom())
        .map(|y| {
            (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

pub(super) fn agent_transcript_item(stable_id: &str, ordinal: u64) -> Arc<TranscriptItem> {
    agent_message_item(stable_id, ordinal, "hello")
}

pub(super) fn agent_message_item(stable_id: &str, ordinal: u64, text: &str) -> Arc<TranscriptItem> {
    Arc::new(TranscriptItem {
        stable_id: stable_id.to_owned(),
        position: ordinal,
        latest_content_event_ordinal: Some(ordinal),
        created_at_ms: 0,
        last_changed_at_ms: 0,
        body: TranscriptBody::Agent {
            chunks: vec![serde_json::json!({
                "content": {"type": "text", "text": text}
            })],
            streaming: false,
        },
    })
}

pub(super) fn grok_chat() -> ChatState {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_session_modes(Some(SessionModeState::new(
        "default",
        vec![
            SessionMode::new("default", "Default"),
            SessionMode::new("plan", "Plan"),
        ],
    )));
    chat
}

pub(super) fn mode_config_option(current: &str, values: &[&str]) -> SessionConfigOption {
    select_config_option("interaction_mode", current, values)
        .category(SessionConfigOptionCategory::Mode)
}

pub(super) fn fast_mode_option(current: &str) -> SessionConfigOption {
    let mut option = select_config_option("fast-mode", current, &["off", "on"]);
    option.name = "Fast mode".into();
    option
}

pub(super) fn select_config_option(
    id: &str,
    current: &str,
    values: &[&str],
) -> SessionConfigOption {
    SessionConfigOption::select(
        id.to_owned(),
        "Mode",
        current.to_owned(),
        SessionConfigSelectOptions::Ungrouped(
            values
                .iter()
                .map(|value| {
                    SessionConfigSelectOption::new((*value).to_owned(), (*value).to_owned())
                })
                .collect(),
        ),
    )
}

pub(super) fn advertise(chat: &mut ChatState, seq: u64, names: &[&str]) {
    chat.apply_session_update(
        seq,
        &serde_json::json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": names
                .iter()
                .map(|name| serde_json::json!({"name": name, "description": "d"}))
                .collect::<Vec<_>>(),
        }),
    );
}
