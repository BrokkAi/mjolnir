//! Shared fixtures for the component unit tests.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

/// A key press with no modifiers held, as the terminal event a component sees.
pub(super) fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}
