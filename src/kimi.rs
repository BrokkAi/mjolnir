//! Discovery and installation of the official Kimi Code ACP binary.

use std::path::PathBuf;

use anyhow::Result;

use crate::managed_acp::{Detection, Spec};

static SPEC: Spec = Spec {
    registry_id: "kimi",
    display_name: "Kimi Code",
    vendor: crate::auth::AuthVendor::Kimi,
};

pub fn detect() -> Detection {
    crate::managed_acp::detect(&SPEC)
}

pub fn start_background_install() {
    crate::managed_acp::start_background_install(&SPEC);
}

pub async fn wait_until_ready() -> Result<PathBuf> {
    crate::managed_acp::wait_until_ready(&SPEC).await
}
