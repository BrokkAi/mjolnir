//! Anvil adapter registration for the Android build. Every Anvil-specific
//! fact lives in this crate; `brokk-mj-core` only ever sees a generic external
//! adapter.

use std::collections::HashMap;
use std::path::PathBuf;

use mj_core::roster::ExternalAdapter;

/// The ACP source id Anvil registers and persists under.
pub const SOURCE_ID: &str = "anvil";

/// Anvil ships as an npm package and launches through `npx -y`, exactly like
/// the built-in adapters.
pub const PACKAGE: &str = "@brokkai/anvil";

/// Register Anvil as the implicit platform team. An `MJ_ANVIL_PATH` override
/// pointing at a local binary wins; otherwise the adapter launches
/// `npx -y @brokkai/anvil`. A dangling override is honored as-is so it fails
/// loudly at launch instead of being silently replaced.
pub fn register() {
    mj_core::roster::register_external_adapter(adapter(
        std::env::var_os("MJ_ANVIL_PATH").map(PathBuf::from),
    ));
}

fn adapter(override_path: Option<PathBuf>) -> ExternalAdapter {
    let (command, args, evidence) = match override_path {
        Some(path) => {
            let evidence = format!("MJ_ANVIL_PATH: {}", path.display());
            (path, Vec::new(), evidence)
        }
        None => (
            PathBuf::from("npx"),
            vec!["-y".to_string(), PACKAGE.to_string()],
            format!("npx -y {PACKAGE}"),
        ),
    };
    ExternalAdapter {
        id: SOURCE_ID.to_string(),
        label: "Anvil".to_string(),
        evidence,
        command,
        args,
        env: HashMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_launch_is_npx() {
        let found = adapter(None);
        assert_eq!(found.command, PathBuf::from("npx"));
        assert_eq!(found.args, vec!["-y".to_string(), PACKAGE.to_string()]);
        assert_eq!(found.id, SOURCE_ID);
        assert_eq!(found.evidence, format!("npx -y {PACKAGE}"));
    }

    #[test]
    fn override_path_replaces_npx() {
        let found = adapter(Some(PathBuf::from("/opt/anvil/anvil")));
        assert_eq!(found.command, PathBuf::from("/opt/anvil/anvil"));
        assert!(found.args.is_empty());
        assert!(found.evidence.starts_with("MJ_ANVIL_PATH"));
    }

    #[test]
    fn registered_adapter_becomes_the_implicit_platform_team() {
        mj_core::roster::register_external_adapter(adapter(None));

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
    }
}
