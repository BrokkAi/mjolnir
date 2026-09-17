//! Hel's versioned user configuration and domain model.
//!
//! This is intentionally a clean namespace. Nothing in this module reads or
//! migrates the legacy `mj` configuration tree.
//!
//! This file holds [`Config`] itself and the sections it owns directly. The
//! rest is split by responsibility and re-exported here, so every path a
//! caller already uses stays the same:
//!
//! * `harness` -- [`HarnessKind`], [`HarnessProfile`] and execution policy.
//! * `targets` -- projects, containers and [`TargetTemplate`].
//! * `ui` -- terminal appearance settings.
//! * `loading` -- instance names, directories, and atomic file writes.

mod harness;
mod loading;
mod targets;
mod ui;

pub use harness::*;
pub use loading::*;
pub use targets::*;
pub use ui::*;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use sha2::{Digest, Sha256};

/// Stable context identity for a bare project at the serialized path boundary.
pub fn raw_project_context_id(project_directory: &str) -> String {
    let digest = Sha256::digest(project_directory.trim().as_bytes());
    let suffix = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("remote-project-{suffix}")
}

fn default_phone_bind() -> String {
    "127.0.0.1:3765".to_owned()
}

const fn default_true() -> bool {
    true
}

const fn is_true(value: &bool) -> bool {
    *value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhoneConfig {
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default = "default_phone_bind")]
    pub bind: String,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub tailscale_detect: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key: Option<PathBuf>,
}

impl Default for PhoneConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: default_phone_bind(),
            tailscale_detect: true,
            tls_cert: None,
            tls_key: None,
        }
    }
}

impl PhoneConfig {
    fn validate(&self) -> Result<()> {
        let bind: std::net::SocketAddr = self
            .bind
            .parse()
            .with_context(|| format!("parse phone bind address {:?}", self.bind))?;
        if self.tls_cert.is_some() != self.tls_key.is_some() {
            bail!("phone TLS requires both `tls_cert` and `tls_key`");
        }
        if !bind.ip().is_loopback() && self.tls_cert.is_none() {
            bail!("a non-loopback phone bind requires TLS");
        }
        Ok(())
    }
}

/// Automatic cross-harness review of every completed coding turn.
///
/// Review is armed here, in the one file that belongs to the machine rather
/// than to any surface: a session driven from a phone is reviewed on the same
/// terms as one driven from the terminal, and the person who set it can see
/// what they set. `profile` names a harness profile defined in this same file
/// -- the reviewer runs under that profile, and it must not be the profile the
/// session under review is using, or the "second opinion" is the same opinion.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewConfig {
    /// Whether every completed turn is reviewed automatically. A one-off
    /// `/review` works whether or not this is set, as long as `profile` names
    /// a reviewer.
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_default_tier")]
    pub tier: crate::review::lanes::ReviewTier,
    /// The harness profile the reviewing agents run under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Model and effort applied to every reviewing role, when the reviewing
    /// harness advertises such a selector. Absent means the profile's own
    /// default, which is what most configurations want.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Indexing every checkpointed session into the user's SessionWiki index.
///
/// SessionWiki is a separate tool that keeps one searchable index of AI coding
/// sessions across every tool the user runs. When this is enabled the daemon
/// writes Mjolnir's own closed sessions into it under the tool name
/// `mjolnir`, so one search covers every harness.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionWikiConfig {
    /// Whether the daemon indexes closed sessions into SessionWiki.
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    /// Age after which a stopped session Mjolnir has indexed is removed from
    /// Mjolnir's own storage. `None` keeps every session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_after_days: Option<u32>,
}

impl SessionWikiConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// Rejects an age that would archive a session the moment it stops.
    fn validate(&self) -> Result<()> {
        if self.archive_after_days == Some(0) {
            bail!(
                "[sessionwiki] archive_after_days = 0 would remove a session as soon as it stops; \
                 use 1 or more, or leave it empty to keep every session"
            );
        }
        Ok(())
    }
}

const fn is_false(value: &bool) -> bool {
    !*value
}

fn is_default_tier(tier: &crate::review::lanes::ReviewTier) -> bool {
    *tier == crate::review::lanes::ReviewTier::default()
}

impl ReviewConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// Rejects a configuration that cannot review.
    ///
    /// Arming review without naming a reviewer is a configuration mistake with
    /// no sensible default -- Mjolnir will not pick a profile on the user's behalf,
    /// because which agent reviews is the most consequential review setting.
    /// Naming a disabled profile is invalid because neither automatic nor
    /// one-off reviews may start new work with it.
    fn validate(&self, profiles: &BTreeMap<String, HarnessProfile>) -> Result<()> {
        if let Some(profile_id) = self.profile.as_ref()
            && let Some(profile) = profiles.get(profile_id)
        {
            if !profile.enabled {
                bail!("[review] profile {profile_id:?} is disabled");
            }
            if !profile.kind.supports_injected_mcp() {
                bail!(
                    "Muse Code cannot be a reviewer because muse-acp does not accept the required MCP tools"
                );
            }
        }
        if self.enabled && self.profile.is_none() {
            bail!(
                "[review] enabled = true needs `profile` naming the harness profile that reviews"
            );
        }
        if let Some(profile) = &self.profile
            && !profiles.contains_key(profile)
        {
            bail!("[review] profile {profile:?} is not a profile defined in this config");
        }
        Ok(())
    }

    /// Whether a turn review can run at all: it needs a reviewer, armed or not.
    #[must_use]
    pub fn reviewer_profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }
}

fn default_subagent_limit() -> usize {
    6
}

fn is_default_subagent_limit(value: &usize) -> bool {
    *value == default_subagent_limit()
}

/// Global policy for Mjolnir-managed child agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SubagentConfig {
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(
        default = "default_subagent_limit",
        skip_serializing_if = "is_default_subagent_limit"
    )]
    pub max_concurrent: usize,
    /// Profiles available in addition to the parent's own enabled profile.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub eligible_profiles: BTreeMap<String, bool>,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_concurrent: default_subagent_limit(),
            eligible_profiles: BTreeMap::new(),
        }
    }
}

impl SubagentConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    fn validate(&self, profiles: &BTreeMap<String, HarnessProfile>) -> Result<()> {
        if !(1..=64).contains(&self.max_concurrent) {
            bail!("[subagents] `max_concurrent` must be between 1 and 64");
        }
        for profile_id in self
            .eligible_profiles
            .iter()
            .filter_map(|(profile_id, eligible)| eligible.then_some(profile_id))
        {
            validate_id("sub-agent profile", profile_id)?;
            match profiles.get(profile_id) {
                // A disabled profile is simply not offered for sub-agent use
                // (see `profile_is_eligible` and the spawn gate), so it does not
                // stop the daemon from starting; `mj doctor` warns about the
                // contradiction instead. A profile that is not defined at all is
                // a configuration mistake, so it still fails to load.
                Some(_) => {}
                None => bail!(
                    "[subagents] eligible profile {profile_id:?} is not defined in this config"
                ),
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn profile_is_eligible(&self, parent: &str, candidate: &str) -> bool {
        self.enabled
            && (parent == candidate
                || self
                    .eligible_profiles
                    .get(candidate)
                    .copied()
                    .unwrap_or(false))
    }
}

/// Global switch for the mbx build cache shared by Rust container sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BuildCacheConfig {
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
}

impl Default for BuildCacheConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl BuildCacheConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

pub const CONFIG_VERSION: u32 = 10;
pub const PRODUCT_DIR: &str = "mjolnir";
pub const DEFAULT_CONTAINER_IMAGE: &str = "ghcr.io/brokkai/mjolnir/agent-dev:latest";

// Old config files may still contain [startup]. Accept it without retaining
// settings that could recreate automatic first-session behavior on save.
fn discard_legacy_startup<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<(), D::Error> {
    serde::de::IgnoredAny::deserialize(deserializer).map(|_| ())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Deprecated session filtering preference retained for read compatibility.
    /// It is ignored and omitted from newly written configurations.
    #[serde(default, skip_serializing)]
    pub show_stopped_sessions: bool,
    #[serde(default, skip_serializing_if = "SessionsSide::is_default")]
    pub sessions_side: SessionsSide,
    #[serde(default, skip_serializing_if = "AdvancedConfig::is_default")]
    pub advanced: AdvancedConfig,
    pub version: u32,
    /// Client-side activity animation; omitted configurations retain the classic scan.
    #[serde(default, skip_serializing_if = "SpinnerStyle::is_default")]
    pub spinner: SpinnerStyle,
    #[serde(default, skip_serializing_if = "UiTheme::is_default")]
    pub theme: UiTheme,
    #[serde(default, skip_serializing_if = "PhoneConfig::is_default")]
    pub phone: PhoneConfig,
    #[serde(default, skip_serializing_if = "ReviewConfig::is_default")]
    pub review: ReviewConfig,
    #[serde(default, skip_serializing_if = "SessionWikiConfig::is_default")]
    pub sessionwiki: SessionWikiConfig,
    #[serde(default, skip_serializing_if = "SubagentConfig::is_default")]
    pub subagents: SubagentConfig,
    #[serde(default, skip_serializing_if = "BuildCacheConfig::is_default")]
    pub build_cache: BuildCacheConfig,
    #[serde(
        default,
        rename = "startup",
        skip_serializing,
        deserialize_with = "discard_legacy_startup"
    )]
    pub legacy_startup: (),
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, HarnessProfile>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bundles: BTreeMap<String, ProjectBundle>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub targets: BTreeMap<String, TargetTemplate>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sessions_side: SessionsSide::default(),
            advanced: AdvancedConfig::default(),
            show_stopped_sessions: false,
            version: CONFIG_VERSION,
            spinner: SpinnerStyle::default(),
            theme: Default::default(),
            phone: PhoneConfig::default(),
            review: ReviewConfig::default(),
            sessionwiki: SessionWikiConfig::default(),
            subagents: SubagentConfig::default(),
            build_cache: BuildCacheConfig::default(),
            legacy_startup: (),
            profiles: BTreeMap::new(),
            bundles: BTreeMap::new(),
            targets: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Suggest additions for setup without replacing existing entries. Both
    /// setup surfaces use the same identities and collision-free names.
    pub fn setup_additions(&self, discovered: &Self) -> Self {
        fn additions<T: Clone>(
            existing: &BTreeMap<String, T>,
            discovered: &BTreeMap<String, T>,
            same: impl Fn(&T, &T) -> bool,
            base: impl Fn(&str, &T) -> String,
        ) -> BTreeMap<String, T> {
            let mut known = existing.clone();
            let mut added = BTreeMap::new();
            for (id, value) in discovered {
                if known.values().any(|entry| same(entry, value)) {
                    continue;
                }
                let id = unique_config_id(&known, &base(id, value));
                known.insert(id.clone(), value.clone());
                added.insert(id, value.clone());
            }
            added
        }
        Self {
            profiles: additions(
                &self.profiles,
                &discovered.profiles,
                HarnessProfile::same_installation,
                |id, _| id.to_owned(),
            ),
            bundles: additions(
                &self.bundles,
                &discovered.bundles,
                PartialEq::eq,
                |_, bundle| bundle.primary_repo.clone(),
            ),
            targets: additions(
                &self.targets,
                &discovered.targets,
                PartialEq::eq,
                |id, _| id.to_owned(),
            ),
            ..Self::default()
        }
    }

    pub fn is_unconfigured(&self) -> bool {
        self.profiles.is_empty() && self.bundles.is_empty() && self.targets.is_empty()
    }

    /// Profiles available for new work, user-facing selectors, and probes.
    pub fn enabled_profiles(&self) -> impl Iterator<Item = (&str, &HarnessProfile)> {
        self.profiles
            .iter()
            .filter(|(_, profile)| profile.enabled)
            .map(|(id, profile)| (id.as_str(), profile))
    }

    /// One profile when it exists and is available for new work.
    pub fn enabled_profile(&self, id: &str) -> Option<&HarnessProfile> {
        self.profiles.get(id).filter(|profile| profile.enabled)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != CONFIG_VERSION {
            bail!(
                "unsupported Mjolnir config version {}; expected {CONFIG_VERSION}",
                self.version
            );
        }
        self.phone.validate()?;
        for (id, profile) in &self.profiles {
            profile.validate(id)?;
        }
        // Checked after the profiles, so a review pointing at a malformed
        // profile reports the profile's own error first.
        self.review.validate(&self.profiles)?;
        self.sessionwiki.validate()?;
        self.subagents.validate(&self.profiles)?;
        for (id, bundle) in &self.bundles {
            bundle.validate(id)?;
        }
        for (id, target) in &self.targets {
            target.validate(id)?;
        }
        Ok(())
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&config_path()).map(Self::with_local_targets)
    }

    /// Supply standard local choices without requiring setup or writing a file.
    /// These are candidates: callers must check availability before offering launch.
    /// Explicit entries with the same name override the standard defaults.
    pub fn with_local_targets(mut self) -> Self {
        let container = ContainerTemplate {
            build_cache: None,
            image: DEFAULT_CONTAINER_IMAGE.into(),
            pull_policy: Default::default(),
            platform: None,
            cpus: None,
            memory: None,
            environment: BTreeMap::new(),
            workspace_storage: Default::default(),
        };
        #[cfg(unix)]
        self.targets
            .entry("localhost".into())
            .or_insert(TargetTemplate::LocalBare);
        self.targets
            .entry("podman".into())
            .or_insert_with(|| TargetTemplate::LocalPodman {
                container: container.clone(),
            });
        self.targets
            .entry("docker".into())
            .or_insert_with(|| TargetTemplate::LocalDocker {
                container: container.clone(),
            });
        #[cfg(target_os = "macos")]
        self.targets
            .entry("apple-container".into())
            .or_insert_with(|| TargetTemplate::AppleContainer { container });
        self
    }

    /// Read the config from `path`, returning [`Config::default`] when the
    /// file is missing or empty and an error when it is malformed or was
    /// written by a newer Mjolnir.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(path)
            .with_context(|| format!("read Mjolnir config {}", path.display()))?;
        if contents.trim().is_empty() {
            return Ok(Self::default());
        }
        let document: toml::Value = contents
            .parse()
            .with_context(|| format!("parse Mjolnir config {}", path.display()))?;
        if let Some(found) = newer_version(&document) {
            bail!(
                "{} was written by a newer Mjolnir (config version {found}; this build supports \
                 {CONFIG_VERSION}). Update Mjolnir",
                path.display()
            );
        }
        reject_removed_profile_overrides(&contents)?;
        reject_non_bare_permissions(&contents)?;
        let mut config: Self = toml::from_str(&contents)
            .with_context(|| format!("parse Mjolnir config {}", path.display()))?;
        // Version 2 adds Podman workspace storage; version 3 restores the
        // spinner preference; version 4 adds stopped-session visibility;
        // version 5 adds the terminal theme preference; version 6 adds
        // optional advanced settings; version 7 restores stopped-session
        // visibility as an advanced setting; version 8 lets profiles be
        // disabled; version 9 adds sub-agent policy; version 10 adds the
        // SessionWiki section. Earlier configs acquire defaults in memory and
        // upgrade on the next ordinary save.
        if matches!(config.version, 1..=9) {
            config.version = CONFIG_VERSION;
        }
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path())
    }

    /// Load the latest config, apply one edit, validate it, and save it while
    /// holding the config lock for the complete transaction.
    ///
    /// The returned config includes runtime local defaults, and the second
    /// value is whatever the edit returned. Keeping the load and edit under
    /// the same lock is what lets independent processes update disjoint
    /// sections without one stale full-config save erasing the other.
    pub fn update<T, F>(edit: F) -> Result<(Self, T)>
    where
        F: FnOnce(&mut Self) -> Result<T>,
    {
        Self::update_to(&config_path(), |config| {
            // Edits see the same local candidates as reads, but unrelated
            // changes must not write implicit defaults into the user's file.
            let mut runtime = config.clone().with_local_targets();
            let implicit: Vec<_> = runtime
                .targets
                .iter()
                .filter(|(id, _)| !config.targets.contains_key(*id))
                .map(|(id, target)| (id.clone(), target.clone()))
                .collect();
            let value = edit(&mut runtime)?;
            for (id, target) in implicit {
                if runtime.targets.get(&id) == Some(&target) {
                    runtime.targets.remove(&id);
                }
            }
            *config = runtime;
            Ok(value)
        })
        .map(|(config, value)| (config.with_local_targets(), value))
    }

    /// As [`Self::update`], using an explicit config path.
    pub fn update_to<T, F>(path: &Path, edit: F) -> Result<(Self, T)>
    where
        F: FnOnce(&mut Self) -> Result<T>,
    {
        let _lock = ConfigLock::acquire(path)?;
        let mut config = Self::load_from(path)?;
        let value = edit(&mut config)?;
        config.save_to_locked(path)?;
        Ok((config, value))
    }

    /// Loads the current file, replaces only its global review section, and
    /// writes it atomically. Callers use this for the dashboard editor so a
    /// stale dashboard snapshot cannot overwrite profiles, bundles, targets,
    /// or phone settings changed concurrently by another client.
    pub fn save_review(review: ReviewConfig) -> Result<Self> {
        Self::save_review_to(&config_path(), review).map(Self::with_local_targets)
    }

    /// As [`Self::save_review`], using an explicit path for tests and tools.
    pub fn save_review_to(path: &Path, review: ReviewConfig) -> Result<Self> {
        let (config, ()) = Self::update_to(path, |config| {
            config.review = review;
            Ok(())
        })?;
        Ok(config)
    }

    /// Refuses when the file belongs to a newer Mjolnir -- judged by the marker
    /// this config loaded with *and* a fresh look at the file, since a newer
    /// build may have written it since. Overwriting would silently drop
    /// settings this build cannot represent.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let _lock = ConfigLock::acquire(path)?;
        self.save_to_locked(path)
    }

    /// Save while the caller already owns [`ConfigLock`]. This is separate
    /// from [`Self::save_to`] so a transaction does not try to lock the same
    /// sibling file recursively.
    fn save_to_locked(&self, path: &Path) -> Result<()> {
        Self::ensure_writable(path)?;
        self.validate()?;
        let body = toml::to_string_pretty(self).context("serialize Mjolnir config")?;
        atomic_write(path, body.as_bytes())
    }

    /// Refuse to overwrite a file that a newer Mjolnir wrote after this
    /// config was loaded.
    fn ensure_writable(path: &Path) -> Result<()> {
        if let Some(found) = newer_version_on_disk(path) {
            bail!(
                "{} was written by a newer Mjolnir (config version {found}; this build writes \
                 {CONFIG_VERSION}). Update Mjolnir, or change settings with the newer build",
                path.display()
            );
        }
        Ok(())
    }
}

/// Cross-process lock for one config path. The lock has a stable inode beside
/// the config because the config itself is replaced atomically after the lock
/// is acquired.
pub(crate) struct ConfigLock {
    _file: File,
}

impl ConfigLock {
    pub(crate) fn acquire(config_path: &Path) -> Result<Self> {
        let lock_path = config_lock_path(config_path);
        let parent = lock_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("create config lock directory {}", parent.display()))?;

        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&lock_path)
            .with_context(|| format!("open config lock {}", lock_path.display()))?;
        file.lock()
            .with_context(|| format!("lock config {}", config_path.display()))?;
        Ok(Self { _file: file })
    }
}

fn config_lock_path(path: &Path) -> PathBuf {
    let Some(file_name) = path.file_name() else {
        return path.with_extension("lock");
    };
    let mut lock_name = OsString::from(file_name);
    lock_name.push(".lock");
    path.with_file_name(lock_name)
}

impl PhoneConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[cfg(test)]
mod tests;
