//! Hel's conversation view state: the chat transcript, its input surfaces, and
//! the usage formatting the surfaces render.

#[cfg(test)]
mj_core::enable_test_fixtures_on_startup!();

pub mod components;
pub mod speech;
pub mod spinner;
pub mod theme;

pub mod chat;
pub mod clipboard;
#[doc(hidden)]
#[doc(hidden)]
pub mod golden;
pub mod modal;
pub mod selection;
pub mod text_input;

pub mod path_input;
