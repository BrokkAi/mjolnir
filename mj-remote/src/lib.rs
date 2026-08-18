//! Remote-control server and web viewer for Mjolnir.

mod qr;
pub mod remote;
#[allow(dead_code)]
mod settings;
mod tailscale;

pub use remote::*;

pub use qr::render_qr;
pub use tailscale::Tailscale;

pub const VIEWER_HTML: &str = include_str!("remote_viewer.html");
pub const SERVICE_WORKER: &str = include_str!("remote_service_worker.js");
pub const WEB_MANIFEST: &str = include_str!("remote_manifest.json");
