//! Anvil sidecar discovery for the Android build. Every Anvil-specific fact
//! lives in this crate; `mj-core` only ever sees a generic external adapter.

use std::collections::HashMap;
use std::path::PathBuf;

use mj_core::roster::ExternalAdapter;

/// Anvil release pinned by the Android packaging job in
/// `.github/workflows/release.yml`.
pub const VERSION: &str = "0.25.0";

/// The ACP source id Anvil registers and persists under.
pub const SOURCE_ID: &str = "anvil";

/// Register the bundled platform adapter when it is present. Detection and
/// every Anvil-specific fact remain owned by this crate.
pub fn register() {
    if let Some(adapter) = detect() {
        mj_core::roster::register_external_adapter(adapter);
    }
}

/// Locate the Anvil ACP server: an `MJ_ANVIL_PATH` override wins, otherwise
/// the `anvil` binary bundled next to the running `mj` executable. `None`
/// when neither points at a file; an override that does not exist is not
/// silently replaced by the sibling.
pub fn detect() -> Option<ExternalAdapter> {
    detect_at(
        std::env::var_os("MJ_ANVIL_PATH").map(PathBuf::from),
        std::env::current_exe().ok(),
    )
}

fn detect_at(
    override_path: Option<PathBuf>,
    current_exe: Option<PathBuf>,
) -> Option<ExternalAdapter> {
    if let Some(path) = override_path {
        return path.is_file().then(|| adapter(path, "MJ_ANVIL_PATH"));
    }
    let sibling = current_exe?.parent()?.join("anvil");
    sibling
        .is_file()
        .then(|| adapter(sibling, "bundled sibling"))
}

fn adapter(path: PathBuf, origin: &str) -> ExternalAdapter {
    ExternalAdapter {
        id: SOURCE_ID.to_string(),
        label: "Anvil".to_string(),
        evidence: format!("{origin}: {}", path.display()),
        command: path,
        args: Vec::new(),
        env: HashMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_binary(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("mj-anvil-test-{}-{name}", std::process::id()));
        std::fs::write(&path, b"stub").expect("write stub binary");
        path
    }

    #[test]
    fn override_path_wins_over_sibling() {
        let anvil = temp_binary("override");
        let found = detect_at(Some(anvil.clone()), None).expect("override detected");
        assert_eq!(found.command, anvil);
        assert_eq!(found.id, SOURCE_ID);
        assert!(found.evidence.starts_with("MJ_ANVIL_PATH"));
        std::fs::remove_file(anvil).ok();
    }

    #[test]
    fn missing_override_is_not_replaced_by_sibling() {
        let sibling = temp_binary("anvil");
        let exe = sibling.with_file_name("mj");
        assert!(
            detect_at(Some(PathBuf::from("/nonexistent/anvil")), Some(exe)).is_none(),
            "a dangling override must not fall back"
        );
        std::fs::remove_file(sibling).ok();
    }

    #[test]
    fn sibling_binary_is_detected() {
        let dir = std::env::temp_dir().join(format!("mj-anvil-test-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let anvil = dir.join("anvil");
        std::fs::write(&anvil, b"stub").expect("write stub binary");
        let found = detect_at(None, Some(dir.join("mj"))).expect("sibling detected");
        assert_eq!(found.command, anvil);
        assert!(found.evidence.starts_with("bundled sibling"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn absent_binary_yields_no_adapter() {
        let dir = std::env::temp_dir().join(format!("mj-anvil-test-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        assert!(detect_at(None, Some(dir.join("mj"))).is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn registered_adapter_becomes_the_implicit_platform_team() {
        let anvil = temp_binary("registered-team");
        mj_core::roster::register_external_adapter(adapter(anvil.clone(), "test"));

        let mut config = mj_core::config::Config::default();
        assert!(mj_core::config::has_valid_team(&config));
        assert!(config.apply_registered_external_team());
        assert_eq!(config.agent.acp_source.as_deref(), Some(SOURCE_ID));
        assert_eq!(config.review.acp_source.as_deref(), Some(SOURCE_ID));
        assert_eq!(config.subagents.acp_source.as_deref(), Some(SOURCE_ID));
        assert!(config.agent.discrete_review);

        let inventory = mj_core::roster::discover_inventory(&config);
        assert_eq!(inventory.servers.len(), 1);
        assert_eq!(inventory.servers[0].id, SOURCE_ID);
        std::fs::remove_file(anvil).ok();
    }

    #[test]
    fn release_workflow_pin_matches_the_adapter_version() {
        let workflow = include_str!("../../.github/workflows/release.yml");
        assert!(
            workflow.contains(&format!("ANVIL_VERSION: \"{VERSION}\"")),
            "release.yml must download the Anvil version registered by mj-anvil"
        );
    }
}
