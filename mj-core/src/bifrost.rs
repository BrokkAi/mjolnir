//! Bifrost package selection and npm registry discovery.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use semver::Version;
use serde::Deserialize;

pub const NPX_PACKAGE: &str = "@brokkai/bifrost";
pub const DEFAULT_VERSION: &str = "latest";
pub const RECENT_VERSION_LIMIT: usize = 5;

const REGISTRY_URL: &str = "https://registry.npmjs.org/%40brokkai%2Fbifrost";
const REGISTRY_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Deserialize)]
struct RegistryPackage {
    // Only the version keys matter; skip each version's manifest.
    versions: HashMap<String, serde::de::IgnoredAny>,
}

/// Fetch the newest stable Bifrost versions advertised by npm.
pub async fn fetch_recent_versions() -> Result<Vec<String>> {
    let body = reqwest::Client::builder()
        .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
        .timeout(REGISTRY_TIMEOUT)
        .build()
        .context("build npm registry client")?
        .get(REGISTRY_URL)
        // The abbreviated document still carries the version keys at a
        // fraction of the full packument's size.
        .header(
            reqwest::header::ACCEPT,
            "application/vnd.npm.install-v1+json",
        )
        .send()
        .await
        .context("query npm for Bifrost versions")?
        .error_for_status()
        .context("npm returned an error for Bifrost versions")?
        .text()
        .await
        .context("read Bifrost version catalog")?;
    parse_recent_versions(&body)
}

fn parse_recent_versions(body: &str) -> Result<Vec<String>> {
    let package: RegistryPackage =
        serde_json::from_str(body).context("parse Bifrost version catalog")?;
    let mut versions = package
        .versions
        .into_keys()
        .filter_map(|raw| {
            let parsed = Version::parse(&raw).ok()?;
            parsed.pre.is_empty().then_some((parsed, raw))
        })
        .collect::<Vec<_>>();
    versions.sort_unstable_by(|left, right| right.0.cmp(&left.0));
    versions.truncate(RECENT_VERSION_LIMIT);
    Ok(versions.into_iter().map(|(_, raw)| raw).collect())
}

/// Choices shown by `/mjconfig`: moving `latest`, the saved pin when it is
/// older than the recent window, then the newest explicit stable versions.
pub fn version_choices(selected: Option<&str>, recent: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut choices = Vec::with_capacity(recent.len() + 2);
    let old_pin = selected.filter(|selected| !recent.iter().any(|recent| recent == *selected));
    for version in std::iter::once(DEFAULT_VERSION)
        .chain(old_pin)
        .chain(recent.iter().map(String::as_str))
    {
        if seen.insert(version.to_string()) {
            choices.push(version.to_string());
        }
    }
    choices
}

/// npm package argument used by both Bifrost CLI and MCP launches.
pub fn package_spec(version: Option<&str>) -> String {
    version.map_or_else(
        || NPX_PACKAGE.to_string(),
        |version| format!("{NPX_PACKAGE}@{version}"),
    )
}

pub fn is_valid_explicit_version(version: &str) -> bool {
    Version::parse(version).is_ok()
}

/// One authority for the `"latest"` ⇄ `None` sentinel and pin validation, so
/// the config loader, the HTTP apply, and the TUI cycle cannot drift.
pub fn parse_selection(version: &str) -> std::result::Result<Option<String>, String> {
    if version == DEFAULT_VERSION {
        return Ok(None);
    }
    if is_valid_explicit_version(version) {
        return Ok(Some(version.to_string()));
    }
    Err(format!("invalid Bifrost version: {version}"))
}

/// The display form of a stored selection: the pin itself, or `latest`.
pub fn selection_label(selection: Option<&str>) -> &str {
    selection.unwrap_or(DEFAULT_VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_catalog_returns_five_newest_stable_versions() {
        let body = r#"{
            "versions": {
                "0.9.9": {},
                "0.10.0-beta.1": {},
                "0.9.10": {},
                "1.0.0": {},
                "0.8.7": {},
                "0.10.0": {},
                "0.9.8": {}
            }
        }"#;

        assert_eq!(
            parse_recent_versions(body).expect("catalog"),
            ["1.0.0", "0.10.0", "0.9.10", "0.9.9", "0.9.8"]
        );
    }

    #[test]
    fn choices_preserve_an_old_saved_pin_after_latest() {
        let recent = vec!["1.0.0".to_string(), "0.9.10".to_string()];
        assert_eq!(
            version_choices(Some("0.8.7"), &recent),
            ["latest", "0.8.7", "1.0.0", "0.9.10"]
        );
        assert_eq!(
            version_choices(Some("1.0.0"), &recent),
            ["latest", "1.0.0", "0.9.10"]
        );
    }

    #[test]
    fn package_spec_leaves_latest_unpinned() {
        assert_eq!(package_spec(None), NPX_PACKAGE);
        assert_eq!(package_spec(Some("0.9.10")), "@brokkai/bifrost@0.9.10");
    }

    #[test]
    fn selection_round_trips_the_latest_sentinel() {
        assert_eq!(parse_selection("latest"), Ok(None));
        assert_eq!(parse_selection("0.9.10"), Ok(Some("0.9.10".to_string())));
        assert_eq!(
            parse_selection("next"),
            Err("invalid Bifrost version: next".to_string())
        );
        assert_eq!(selection_label(None), "latest");
        assert_eq!(selection_label(Some("0.9.10")), "0.9.10");
    }
}
