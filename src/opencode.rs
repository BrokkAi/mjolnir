//! Discovery and installation of the official OpenCode ACP binary.
//!
//! OpenCode's ACP session advertises only the models it can actually serve:
//! providers the user authenticated (`opencode auth login`, provider config
//! blocks, API-key variables) plus its free-tier Zen models. Mjolnir trusts
//! that list as-is rather than second-guessing OpenCode's own provider
//! resolution.

use std::path::PathBuf;

use anyhow::Result;

use crate::managed_acp::{Detection, Spec};

static SPEC: Spec = Spec {
    registry_id: "opencode",
    display_name: "OpenCode",
    vendor: crate::auth::AuthVendor::OpenCode,
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
