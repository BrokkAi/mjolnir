//! Client-facing contracts shared by Mjolnir's daemon and control surfaces.

#[cfg(test)]
mj_core::enable_test_fixtures_on_startup!();

pub mod build_identity;
pub mod image;
pub mod logging;
pub mod quota;
pub mod review;
pub mod runtime_feed;
pub mod session;
pub mod settings;
pub mod target;
pub mod ui_request;
pub mod web;

pub mod transcript;
pub mod usage_format;

pub mod operations;

pub mod daemon;
pub mod executable;
pub mod http;

pub mod availability;

pub mod discovery;

pub mod session_image;
