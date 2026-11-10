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
//! * `keys` -- the `[keys]` section: the prefix key and every bindable action.
//! * `machines` -- hosts, the stored runtime shape, and the conversion
//!   between them and [`TargetTemplate`].
//! * `targets` -- projects, containers and [`TargetTemplate`].
//! * `ui` -- terminal appearance settings.
//! * `loading` -- instance names, directories, and atomic file writes.
//! * `secrets` -- environment entries that name a secret held elsewhere.

mod aws;
mod daemon;
mod document;
mod dsh_credentials;
mod github;
mod harness;
mod keys;
mod loading;
mod machines;
mod secrets;
mod targets;
mod ui;

pub use aws::*;
pub use daemon::*;
pub use github::*;
pub use harness::*;
pub use keys::*;
pub use loading::*;
pub use machines::*;
pub use secrets::*;
pub use targets::*;
pub use ui::*;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};

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

/// Ports a named instance's viewer may default to. The default instance keeps
/// 3765; each named instance hashes its name into this range so a test build
/// beside the live one does not collide with it, and so its viewer URL (and
/// the bookmarked QR login) stays the same across daemon restarts.
pub const INSTANCE_VIEWER_PORTS: std::ops::RangeInclusive<u16> = 38000..=38999;

fn default_phone_bind() -> String {
    default_phone_bind_for(instance_name().as_deref())
}

/// The default `[phone] bind` for the default instance (`None`) or a named one.
pub fn default_phone_bind_for(instance: Option<&str>) -> String {
    let Some(name) = instance else {
        return "127.0.0.1:3765".to_owned();
    };
    let digest = Sha256::digest(name.as_bytes());
    let span = u32::from(INSTANCE_VIEWER_PORTS.end() - INSTANCE_VIEWER_PORTS.start()) + 1;
    let offset = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % span;
    let port = u32::from(*INSTANCE_VIEWER_PORTS.start()) + offset;
    format!("127.0.0.1:{port}")
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

/// Ordered reviewer choices. Review runs before push exactly when reviewers
/// are configured; there is no separate switch. See [`ReviewConfig::trigger`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ReviewConfigInput", into = "ReviewConfigOutput")]
pub struct ReviewConfig {
    /// A configuration written before reviewer lists that asked for review
    /// but named no model. Its reviewers are chosen automatically once, after
    /// which this is cleared. It counts as configured while it is set.
    pub legacy: Option<LegacyReview>,
    pub reviewers: Vec<SubagentChoice>,
    /// Set only by [`ReviewConfig::for_session`] for a session that chose
    /// Off. Never read from or written to a file.
    pub disabled: bool,
}

pub const MAX_REVIEWERS: usize = 3;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegacyReview {
    pub count: Option<std::num::NonZeroUsize>,
    pub profile: Option<String>,
    pub effort: Option<String>,
}

impl ReviewConfig {
    pub fn is_legacy(&self) -> bool {
        self.legacy.is_some()
    }

    pub fn clear_legacy(&mut self) {
        self.legacy = None;
    }

    /// Review as an older configuration that asked for it without naming a
    /// model loads: reviewers still to be chosen automatically.
    pub fn legacy_default() -> Self {
        Self {
            legacy: Some(LegacyReview::default()),
            ..Self::default()
        }
    }
}

/// What a save writes. The removed `trigger` key appears only while reviewers
/// still have to be chosen automatically, so a restart before that finishes
/// does not forget that review was wanted. Once reviewers are configured the
/// next save drops it.
#[derive(Serialize)]
struct ReviewConfigOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    trigger: Option<ReviewTrigger>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reviewers: Option<Vec<SubagentChoice>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<std::num::NonZeroUsize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
}

impl From<ReviewConfig> for ReviewConfigOutput {
    fn from(value: ReviewConfig) -> Self {
        let legacy = value.legacy;
        Self {
            trigger: legacy.is_some().then_some(ReviewTrigger::Push),
            reviewers: legacy.is_none().then_some(value.reviewers),
            count: legacy.as_ref().and_then(|old| old.count),
            profile: legacy.as_ref().and_then(|old| old.profile.clone()),
            effort: legacy.and_then(|old| old.effort),
        }
    }
}

/// Whether review runs before push for a session. It is derived, never
/// chosen globally: see [`ReviewConfig::trigger`]. Per-turn review was
/// removed; a stored `turn` reads as `push`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewTrigger {
    #[default]
    Off,
    #[serde(alias = "turn")]
    Push,
}

impl std::fmt::Display for ReviewTrigger {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Off => "off",
            Self::Push => "push",
        })
    }
}

/// The value of the removed `[review] trigger` key. Any value loads: only
/// `push` and `turn` mean the user wanted review.
#[derive(Deserialize)]
#[serde(untagged)]
enum RemovedTrigger {
    Name(String),
    Other(serde::de::IgnoredAny),
}

impl RemovedTrigger {
    fn wanted_review(&self) -> bool {
        matches!(self, Self::Name(name) if matches!(name.as_str(), "push" | "turn"))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewConfigInput {
    #[serde(default)]
    trigger: Option<RemovedTrigger>,
    #[serde(default)]
    reviewers: Option<Vec<SubagentChoice>>,
    #[serde(default)]
    count: Option<std::num::NonZeroUsize>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    #[expect(dead_code, reason = "accepted only so that old files still load")]
    tier: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
}

impl From<ReviewConfigInput> for ReviewConfig {
    fn from(input: ReviewConfigInput) -> Self {
        // The old profile pin and tier are dropped: reviewers are
        // load-balanced across enabled profiles.
        let ReviewConfigInput {
            trigger,
            reviewers,
            count,
            enabled,
            tier: _,
            profile,
            model,
            effort,
        } = input;
        // Review is on exactly when reviewers are configured. A table that
        // names none but asked for review (the removed `trigger` or `enabled`
        // keys) keeps asking for automatic reviewers; one that did not has
        // nothing to configure and review stays off.
        let wanted = trigger.map_or(enabled == Some(true), |trigger| trigger.wanted_review());
        let legacy = (reviewers.is_none() && model.is_none() && wanted).then_some(LegacyReview {
            count,
            profile,
            effort: effort.clone(),
        });
        Self {
            legacy,
            reviewers: legacy_reviewers(reviewers, count, model, effort).unwrap_or_default(),
            disabled: false,
        }
    }
}

/// One session's review choice, made when the session is created. It
/// overrides `[review]` for that session only; a session without one follows
/// `[review]`. A stored `turn` (or `on`) reads as `push`, with the same
/// reviewers, and is never written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "SessionReviewInput", into = "SessionReviewOutput")]
pub enum SessionReview {
    Legacy {
        trigger: Option<ReviewTrigger>,
        count: Option<std::num::NonZeroUsize>,
        model: Option<String>,
        effort: Option<String>,
    },
    Inherit {
        reviewers: Option<Vec<SubagentChoice>>,
    },
    Off,
    Push {
        reviewers: Option<Vec<SubagentChoice>>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
enum SessionReviewInput {
    Inherit {
        reviewers: Option<Vec<SubagentChoice>>,
        count: Option<std::num::NonZeroUsize>,
        model: Option<String>,
        effort: Option<String>,
        #[expect(dead_code, reason = "accepted only so that old files still load")]
        tier: Option<String>,
    },
    Off,
    #[serde(alias = "turn", alias = "on")]
    Push {
        reviewers: Option<Vec<SubagentChoice>>,
        count: Option<std::num::NonZeroUsize>,
        model: Option<String>,
        effort: Option<String>,
        #[expect(dead_code, reason = "accepted only so that old files still load")]
        tier: Option<String>,
    },
}

#[derive(Serialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
enum SessionReviewOutput {
    #[serde(rename = "inherit")]
    LegacyInherit {
        #[serde(skip_serializing_if = "Option::is_none")]
        count: Option<std::num::NonZeroUsize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    #[serde(rename = "push")]
    LegacyPush {
        #[serde(skip_serializing_if = "Option::is_none")]
        count: Option<std::num::NonZeroUsize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    Inherit {
        #[serde(skip_serializing_if = "Option::is_none")]
        reviewers: Option<Vec<SubagentChoice>>,
    },
    Off,
    Push {
        #[serde(skip_serializing_if = "Option::is_none")]
        reviewers: Option<Vec<SubagentChoice>>,
    },
}

/// The reviewer list, or the one a configuration written before reviewer
/// lists implies: `count` copies of its model (two when `count` is absent,
/// the old default), and none when it named no model (old Auto).
fn legacy_reviewers(
    reviewers: Option<Vec<SubagentChoice>>,
    count: Option<std::num::NonZeroUsize>,
    model: Option<String>,
    effort: Option<String>,
) -> Option<Vec<SubagentChoice>> {
    reviewers.or_else(|| {
        model.map(|model| {
            vec![SubagentChoice { model, effort }; count.map_or(2, |c| c.get()).min(MAX_REVIEWERS)]
        })
    })
}

impl From<SessionReviewInput> for SessionReview {
    fn from(input: SessionReviewInput) -> Self {
        match input {
            SessionReviewInput::Inherit {
                reviewers,
                count,
                model,
                effort,
                tier: _,
            } => {
                if reviewers.is_none() && (count.is_some() || model.is_some() || effort.is_some()) {
                    Self::Legacy {
                        trigger: None,
                        count,
                        model,
                        effort,
                    }
                } else {
                    Self::Inherit { reviewers }
                }
            }
            SessionReviewInput::Off => Self::Off,
            SessionReviewInput::Push {
                reviewers,
                count,
                model,
                effort,
                tier: _,
            } => {
                if reviewers.is_none() && (count.is_some() || model.is_some() || effort.is_some()) {
                    Self::Legacy {
                        trigger: Some(ReviewTrigger::Push),
                        count,
                        model,
                        effort,
                    }
                } else {
                    Self::Push { reviewers }
                }
            }
        }
    }
}
impl From<SessionReview> for SessionReviewOutput {
    fn from(input: SessionReview) -> Self {
        match input {
            SessionReview::Legacy {
                trigger,
                count,
                model,
                effort,
            } => match trigger {
                None | Some(ReviewTrigger::Off) => Self::LegacyInherit {
                    count,
                    model,
                    effort,
                },
                Some(ReviewTrigger::Push) => Self::LegacyPush {
                    count,
                    model,
                    effort,
                },
            },
            SessionReview::Inherit { reviewers } => Self::Inherit { reviewers },
            SessionReview::Off => Self::Off,
            SessionReview::Push { reviewers } => Self::Push { reviewers },
        }
    }
}
impl SessionReview {
    pub fn trigger(&self) -> Option<ReviewTrigger> {
        match self {
            Self::Legacy { trigger, .. } => *trigger,
            Self::Inherit { .. } => None,
            Self::Off => Some(ReviewTrigger::Off),
            Self::Push { .. } => Some(ReviewTrigger::Push),
        }
    }
}

impl ReviewConfig {
    /// Whether review runs before push. This is the only answer: review is on
    /// exactly when reviewers are configured (or still to be chosen
    /// automatically), unless the session chose Off.
    pub fn trigger(&self) -> ReviewTrigger {
        if !self.disabled && (self.legacy.is_some() || !self.reviewers.is_empty()) {
            ReviewTrigger::Push
        } else {
            ReviewTrigger::Off
        }
    }

    pub fn is_push(&self) -> bool {
        self.trigger() == ReviewTrigger::Push
    }

    /// This configuration as it applies to a session with `session`'s choice.
    /// The effective reviewer list enables review unless the session chose Off.
    #[must_use]
    pub fn for_session(&self, session: Option<&SessionReview>) -> Self {
        let mut config = self.clone();
        match session {
            None => {}
            Some(SessionReview::Off) => config.disabled = true,
            Some(choice) => {
                config.disabled = false;
                if let SessionReview::Legacy {
                    count,
                    model,
                    effort,
                    ..
                } = choice
                {
                    if let Some(model) = model {
                        config.reviewers = apply_legacy_override(
                            &config.reviewers,
                            *count,
                            Some(model),
                            effort.as_ref(),
                            self,
                        );
                        config.clear_legacy();
                        return config;
                    }
                    config.reviewers = apply_legacy_override(
                        &config.reviewers,
                        *count,
                        None,
                        effort.as_ref(),
                        self,
                    );
                    return config;
                }
                let reviewers = match choice {
                    SessionReview::Legacy { .. } => unreachable!(),
                    SessionReview::Inherit { reviewers } | SessionReview::Push { reviewers } => {
                        reviewers
                    }
                    SessionReview::Off => unreachable!(),
                };
                if let Some(reviewers) = reviewers {
                    config.reviewers.clone_from(reviewers);
                    config.clear_legacy();
                }
            }
        }
        config
    }
}

pub fn apply_legacy_override(
    list: &[SubagentChoice],
    count: Option<std::num::NonZeroUsize>,
    model: Option<&String>,
    effort: Option<&String>,
    global: &ReviewConfig,
) -> Vec<SubagentChoice> {
    if let Some(model) = model {
        let uniform = global
            .reviewers
            .first()
            .filter(|first| global.reviewers.iter().all(|slot| slot == *first));
        let n = count
            .map(|n| n.get())
            .or_else(|| uniform.map(|_| global.reviewers.len()))
            .or_else(|| {
                global
                    .legacy
                    .as_ref()
                    .and_then(|old| old.count.map(|n| n.get()))
            })
            .unwrap_or(2)
            .clamp(1, MAX_REVIEWERS);
        let inherited_effort = effort
            .cloned()
            .or_else(|| uniform.and_then(|slot| slot.effort.clone()))
            .or_else(|| global.legacy.as_ref().and_then(|old| old.effort.clone()));
        return vec![
            SubagentChoice {
                model: model.clone(),
                effort: inherited_effort
            };
            n
        ];
    }
    let mut result = list.to_vec();
    if let Some(effort) = effort {
        for slot in &mut result {
            slot.effort = Some(effort.clone());
        }
    }
    if let Some(count) = count
        && let Some(last) = result.last().cloned()
    {
        result.resize(count.get().clamp(1, MAX_REVIEWERS), last);
    }
    result
}

/// Indexing every session into the user's SessionWiki index.
///
/// SessionWiki is a separate tool that keeps one searchable index of AI coding
/// sessions across every tool the user runs. The daemon always writes
/// Mjolnir's own sessions into it under the tool name `mjolnir`, so one search
/// covers every harness. Only archiving is a setting, because only archiving
/// deletes data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionWikiConfig {
    /// Deprecated switch retained for read compatibility with configurations
    /// written while indexing was optional. It is ignored and omitted from
    /// newly written configurations.
    #[serde(default, skip_serializing)]
    pub enabled: bool,
    /// Age after which a stopped session Mjolnir has indexed is removed from
    /// Mjolnir's own storage. `None` keeps every session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_after_days: Option<u32>,
}

impl SessionWikiConfig {
    /// The deprecated `enabled` key is never written, so a section holding
    /// only that key is still a default section and stays out of the file.
    fn is_default(&self) -> bool {
        self.archive_after_days.is_none()
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

impl ReviewConfig {
    fn validate(&self, _profiles: &BTreeMap<String, HarnessProfile>) -> Result<()> {
        if self.reviewers.len() > MAX_REVIEWERS {
            bail!("[review] supports at most {MAX_REVIEWERS} reviewers");
        }
        if self
            .reviewers
            .iter()
            .any(|reviewer| reviewer.model.trim().is_empty())
        {
            bail!("[review] reviewer model must not be empty");
        }
        Ok(())
    }
}

fn default_subagent_limit() -> usize {
    6
}

fn is_default_subagent_limit(value: &usize) -> bool {
    *value == default_subagent_limit()
}

/// Global policy for Mjolnir-managed child agents.
///
/// Sessions choose Native or Mjolnir; task model and effort choices are global.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SubagentConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub straightforward: Option<SubagentChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtle: Option<SubagentChoice>,
    /// The read-only advisor behind `ask_oracle`. Optional, and independent
    /// of the other two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle: Option<SubagentChoice>,
    /// Deprecated global switch retained for read compatibility with
    /// configurations written before the per-session choice existed. It is
    /// ignored and omitted from newly written configurations.
    #[serde(default, skip_serializing)]
    pub enabled: bool,
    #[serde(
        default = "default_subagent_limit",
        skip_serializing_if = "is_default_subagent_limit"
    )]
    pub max_concurrent: usize,
    /// Old allowlist, accepted for compatibility and dropped when saved.
    #[serde(default, rename = "eligible_profiles", skip_serializing)]
    #[doc(hidden)]
    pub legacy_eligible_profiles: BTreeMap<String, bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentChoice {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            straightforward: None,
            subtle: None,
            oracle: None,
            enabled: false,
            max_concurrent: default_subagent_limit(),
            legacy_eligible_profiles: BTreeMap::new(),
        }
    }
}

impl SubagentConfig {
    pub fn choice(&self, task_type: crate::subagent::SubagentTaskType) -> Option<&SubagentChoice> {
        match task_type {
            crate::subagent::SubagentTaskType::Straightforward => self.straightforward.as_ref(),
            crate::subagent::SubagentTaskType::Subtle => self.subtle.as_ref(),
            crate::subagent::SubagentTaskType::Oracle => self.oracle.as_ref(),
        }
    }
    /// A session's choice for `task_type`: its own override, else the global one.
    pub fn effective_choice<'a>(
        &'a self,
        policy: &'a crate::subagent::SubagentPolicy,
        task_type: crate::subagent::SubagentTaskType,
    ) -> Option<&'a SubagentChoice> {
        policy
            .override_choice(task_type)
            .or_else(|| self.choice(task_type))
    }
    /// Whether a parent with `policy` may spawn subtle children. Without a
    /// subtle choice, every child uses the straightforward choice.
    pub fn offers_subtle(&self, policy: &crate::subagent::SubagentPolicy) -> bool {
        self.effective_choice(policy, crate::subagent::SubagentTaskType::Subtle)
            .is_some()
    }
    /// Whether sessions can be offered an oracle.
    pub fn oracle_configured(&self) -> bool {
        self.oracle
            .as_ref()
            .is_some_and(|choice| !choice.model.trim().is_empty())
    }
    /// Straightforward is required; subtle is optional.
    pub fn is_configured(&self) -> bool {
        self.straightforward
            .as_ref()
            .is_some_and(|choice| !choice.model.trim().is_empty())
    }
    /// The deprecated `enabled` key is never written, so a section holding
    /// only that key is still a default section and stays out of the file.
    fn is_default(&self) -> bool {
        self.max_concurrent == default_subagent_limit()
            && self.straightforward.is_none()
            && self.subtle.is_none()
            && self.oracle.is_none()
    }

    fn validate(&self) -> Result<()> {
        for task_type in [
            crate::subagent::SubagentTaskType::Straightforward,
            crate::subagent::SubagentTaskType::Subtle,
            crate::subagent::SubagentTaskType::Oracle,
        ] {
            if let Some(choice) = self.choice(task_type) {
                anyhow::ensure!(
                    choice.effort.as_deref() != Some("adaptive"),
                    "[subagents.{}] adaptive effort is no longer supported",
                    task_type.id()
                );
            }
        }
        anyhow::ensure!(
            self.subtle.is_none() || self.straightforward.is_some(),
            "[subagents.subtle] requires [subagents.straightforward]; configure straightforward alone or both"
        );
        if !(1..=64).contains(&self.max_concurrent) {
            bail!("[subagents] `max_concurrent` must be between 1 and 64");
        }
        Ok(())
    }
}

/// Read-only legacy policy, folded into machine settings when loading.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LegacyBuildCacheConfig {
    enabled: bool,
}

impl Default for LegacyBuildCacheConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// The one switch for the hosted Jev service. When it is off, Mjolnir sends
/// nothing to Jev: no turn or GitHub-item classification, no automatic
/// continuation verdicts, and no semantic help search. `[jev] enabled = false`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JevConfig {
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
}

impl Default for JevConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl JevConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// Where the daemon runs the quota checks that start a harness program: the
/// Codex app-server, Grok's agent, and Claude Code's login refresh.
/// `[quota] runtime = "auto"`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaConfig {
    #[serde(skip_serializing_if = "QuotaRuntimeChoice::is_default")]
    pub runtime: QuotaRuntimeChoice,
}

impl QuotaConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// `Auto` runs a check on the host when the host already has the harness it
/// starts (Codex's managed installation, `grok`, or `claude`). Otherwise it
/// runs the check in the image of an enabled local Podman runtime, else an
/// enabled local Docker runtime, when its engine is installed, and on the host
/// when there is no such runtime. `Podman` and `Docker` insist on that engine
/// (with its runtime's image, or the release's agent-dev image without one),
/// whether or not the host has the harness; `Host` never uses a container.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuotaRuntimeChoice {
    #[default]
    Auto,
    Podman,
    Docker,
    Host,
}

impl QuotaRuntimeChoice {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Legacy GitHub event source switch, read but never written.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LegacyMailboxConfig {
    pub enabled: bool,
}

impl Default for LegacyMailboxConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Config {
    /// Identity of the configured profile list and the managed harness builds
    /// its probes would run, excluding policies and UI defaults.
    pub fn profiles_discovery_key(&self) -> String {
        use sha2::{Digest, Sha256};
        let inputs: Vec<_> = self
            .profiles
            .iter()
            .map(|(id, profile)| profile.capabilities_key(id))
            .collect();
        crate::hex::lower_hex(Sha256::digest(
            serde_json::to_vec(&inputs).expect("profile identities serialize"),
        ))
    }

    /// The models this configuration selects on profile `id`, whose efforts
    /// its settings offer: the sub-agent choices, which any enabled profile
    /// may serve, and the reviewer's model on the review profile. Only these
    /// and each profile's current model have their efforts discovered ahead
    /// of a request; aggregating harnesses advertise hundreds of models.
    pub fn selected_models(&self, id: &str) -> BTreeSet<String> {
        let mut models = [
            &self.subagents.straightforward,
            &self.subagents.subtle,
            &self.subagents.oracle,
        ]
        .into_iter()
        .flatten()
        .map(|choice| choice.model.clone())
        .collect::<BTreeSet<_>>();
        let _ = id;
        models.extend(
            self.review
                .reviewers
                .iter()
                .map(|choice| choice.model.clone()),
        );
        models
    }

    /// Identity of what capability hydration covers for this configuration:
    /// its profile definitions and the models each enabled profile is
    /// selected for.
    pub fn capability_demand_key(&self) -> String {
        use sha2::{Digest, Sha256};
        let selected: BTreeMap<_, _> = self
            .enabled_profiles()
            .map(|(id, _)| (id, self.selected_models(id)))
            .collect();
        crate::hex::lower_hex(Sha256::digest(
            serde_json::to_vec(&(self.profiles_discovery_key(), selected))
                .expect("capability demand serializes"),
        ))
    }

    /// An opaque identity safe to send to control surfaces without exposing
    /// profile environments. Policies and unrelated preferences are absent.
    pub fn subagent_discovery_key(&self, parent: &str, model: Option<&str>) -> String {
        use sha2::{Digest, Sha256};
        let inputs = serde_json::to_vec(&(parent, model, self.subagent_discovery_inputs()))
            .expect("discovery inputs serialize");
        crate::hex::lower_hex(Sha256::digest(inputs))
    }

    /// Enabled installations determine sub-agent choices.
    pub fn subagent_discovery_inputs(&self) -> BTreeMap<String, ProfileDiscoveryInputs> {
        self.profiles
            .iter()
            .filter(|(_, profile)| profile.enabled)
            .map(|(id, profile)| (id.clone(), profile.discovery_inputs()))
            .collect()
    }

    /// Automatic continuation asks Jev whether a finished turn left work
    /// undone, so it runs only while `[continuation]` and `[jev]` are both on.
    #[must_use]
    pub fn automatic_continuation_enabled(&self) -> bool {
        self.continuation.enabled && self.jev.enabled
    }

    /// The GitHub watcher uses Jev to classify activity, so both switches
    /// must be enabled. Neither switch disables mailbox delivery.
    #[must_use]
    pub fn github_events_enabled(&self) -> bool {
        self.github.watch.enabled && self.jev.enabled
    }
}

// Version 20 adds daemon placement; 21 adds review counts; 22 adds the session
// GitHub credential default. Older strict
// configuration readers reject the new fields instead of dropping them.
/// Historical per-session EC2 records remain readable, but new work uses a
/// persistent SSH machine with a container runtime.
pub const LEGACY_EC2_TARGET_NOTICE: &str = "Per-session EC2 targets were removed. Choose a persistent Podman target, or create one with `mj ec2 create <instance-type> --zone <zone>`.";

pub const CONFIG_VERSION: u32 = 22;
/// The page that documents config.toml, schema version included.
pub const CONFIGURATION_DOCUMENTATION_URL: &str = "https://mjolnir.brokk.ai/configuration/";
pub const PRODUCT_DIR: &str = "mjolnir";
/// The registry repository every session image name is built from.
pub const CONTAINER_IMAGE_REPOSITORY: &str = "ghcr.io/brokkai/mjolnir/agent-dev";
/// The container image a session uses when its target names none.
///
/// A release build names the immutable image published for its own version; a
/// development build names the floating image published from master.
/// `mj-core/build.rs` resolves which one this build is and bakes the answer in.
pub const DEFAULT_CONTAINER_IMAGE: &str = env!("MJ_AGENT_DEV_IMAGE");
/// Whether this is a release build whose [`DEFAULT_CONTAINER_IMAGE`] carries
/// its own worker. The release workflow publishes that image with the worker
/// built from the same commit, whose identity equals this build's, so a
/// container on it needs no worker from the host. Neither a development build
/// nor a crates.io package, whose identity falls back to the commit, is.
pub const RELEASE_IMAGE_CARRIES_WORKER: bool =
    matches!(env!("MJ_RELEASE_IMAGE_CARRIES_WORKER").as_bytes(), b"1");
/// The image name every configuration up to [`LEGACY_DEFAULT_IMAGE_LAST_VERSION`]
/// wrote into a container target a user had touched.
///
/// Loading such a file replaces this literal with [`DEFAULT_CONTAINER_IMAGE`],
/// so an upgraded install follows the image built for its own version instead
/// of pinning whatever was current when the file was written.
pub const LEGACY_DEFAULT_CONTAINER_IMAGE: &str = "ghcr.io/brokkai/mjolnir/agent-dev:latest";
/// The last configuration version that wrote the default image into a file.
const LEGACY_DEFAULT_IMAGE_LAST_VERSION: u32 = 14;

/// The rule [`DEFAULT_CONTAINER_IMAGE`] is resolved from.
///
/// It is a function so one test build can exercise both channels; a single
/// `build.rs` run compiles only one of them.
#[must_use]
pub fn container_image_for(version: &str, release: bool) -> String {
    if release {
        format!("{CONTAINER_IMAGE_REPOSITORY}:{version}")
    } else {
        format!("{CONTAINER_IMAGE_REPOSITORY}:latest")
    }
}

/// Move a container target still carrying the historical default image onto
/// `default_image`.
///
/// A target the user customized -- any other image -- is left alone. Split from
/// [`TryFrom<StoredConfig>`] so a test build can exercise the release channel's
/// rewrite without compiling a release `build.rs`.
fn migrate_legacy_default_image(target: &mut TargetTemplate, default_image: &str) {
    if let Some(container) = target.container_mut()
        && container.image == LEGACY_DEFAULT_CONTAINER_IMAGE
    {
        container.image = default_image.to_owned();
    }
}

// Old config files may still contain [startup]. Accept it without retaining
// settings that could recreate automatic first-session behavior on save.
fn discard_legacy_startup<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<(), D::Error> {
    serde::de::IgnoredAny::deserialize(deserializer).map(|_| ())
}

/// The resolved configuration every part of Mjolnir works with.
///
/// Each entry of `targets` is a [`TargetTemplate`]: a host and a runtime
/// fused together. The file stores the two apart, so this type is read and
/// written through [`StoredConfig`], which is what `[machines.<id>]` and
/// `[targets.<id>]` actually look like. `machines` is carried through from
/// the file so the Settings screen can edit hosts directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(into = "StoredConfig", try_from = "StoredConfig")]
pub struct Config {
    pub sessions_side: SessionsSide,
    /// Shared Sessions sidebar size, independent of the selected workspace.
    pub sidebar_size: crate::workspace::PaneSize,
    pub advanced: AdvancedConfig,
    pub notify: NotifyConfig,
    pub version: u32,
    /// Client-side activity animation; omitted configurations retain the classic scan.
    pub spinner: SpinnerStyle,
    pub theme: ThemeChoice,
    pub phone: PhoneConfig,
    /// Where the daemon runs: on this computer or in a container.
    pub daemon: DaemonConfig,
    pub github: GithubConfig,
    pub review: ReviewConfig,
    pub continuation: crate::continuation::ContinuationConfig,
    pub sessionwiki: SessionWikiConfig,
    pub subagents: SubagentConfig,
    pub jev: JevConfig,
    pub quota: QuotaConfig,
    pub keys: KeysConfig,
    pub legacy_startup: (),
    pub profiles: BTreeMap<String, HarnessProfile>,
    pub bundles: BTreeMap<String, ProjectBundle>,
    /// The hosts runtimes run on. `local` is implied even when it is absent.
    pub machines: BTreeMap<String, Machine>,
    pub targets: BTreeMap<String, TargetTemplate>,
    /// Ids in `targets` that [`Config::with_local_targets`] inserted because
    /// the file names no target of that id. They are candidates, not choices
    /// the user made, and they are never written to the file. The file is
    /// the only source of an origin: this set is not serialized.
    pub default_targets: BTreeSet<String>,
    /// Ids in `targets` the user switched off with `enabled = false`. A
    /// disabled runtime stays configured, so detection never adds its kind
    /// again, but nothing new is launched on it.
    pub disabled_targets: BTreeSet<String>,
}

/// The configuration file's own shape: hosts under `[machines.<id>]` and
/// runtimes under `[targets.<id>]`, each runtime naming its machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredConfig {
    /// A retired session filter. Files written before version 7 may still
    /// carry it, so it is accepted and dropped.
    #[serde(default, skip_serializing)]
    #[expect(dead_code, reason = "accepted only so that old files still load")]
    show_stopped_sessions: bool,
    #[serde(default, skip_serializing_if = "SessionsSide::is_default")]
    sessions_side: SessionsSide,
    #[serde(
        default,
        skip_serializing_if = "crate::workspace::PaneSize::is_default"
    )]
    sidebar_size: crate::workspace::PaneSize,
    #[serde(default, skip_serializing_if = "AdvancedConfig::is_default")]
    advanced: AdvancedConfig,
    #[serde(default, skip_serializing_if = "NotifyConfig::is_default")]
    notify: NotifyConfig,
    version: u32,
    #[serde(default, skip_serializing_if = "SpinnerStyle::is_default")]
    spinner: SpinnerStyle,
    #[serde(default, skip_serializing_if = "ThemeChoice::is_default")]
    theme: ThemeChoice,
    #[serde(default, skip_serializing_if = "PhoneConfig::is_default")]
    phone: PhoneConfig,
    #[serde(default, skip_serializing_if = "DaemonConfig::is_default")]
    daemon: DaemonConfig,
    #[serde(default, skip_serializing_if = "GithubConfig::is_default")]
    github: GithubConfig,
    #[serde(default)]
    review: ReviewConfig,
    #[serde(
        default = "crate::continuation::ContinuationConfig::enabled",
        skip_serializing_if = "crate::continuation::ContinuationConfig::is_default"
    )]
    continuation: crate::continuation::ContinuationConfig,
    #[serde(default, skip_serializing)]
    mailbox: LegacyMailboxConfig,
    #[serde(default, skip_serializing_if = "SessionWikiConfig::is_default")]
    sessionwiki: SessionWikiConfig,
    #[serde(default, skip_serializing_if = "SubagentConfig::is_default")]
    subagents: SubagentConfig,
    #[serde(default, skip_serializing)]
    build_cache: LegacyBuildCacheConfig,
    #[serde(default, skip_serializing_if = "JevConfig::is_default")]
    jev: JevConfig,
    #[serde(default, skip_serializing_if = "QuotaConfig::is_default")]
    quota: QuotaConfig,
    #[serde(default, skip_serializing_if = "KeysConfig::is_default")]
    keys: KeysConfig,
    #[serde(
        default,
        rename = "startup",
        skip_serializing,
        deserialize_with = "discard_legacy_startup"
    )]
    legacy_startup: (),
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    profiles: BTreeMap<String, HarnessProfile>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    bundles: BTreeMap<String, ProjectBundle>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    machines: BTreeMap<String, Machine>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    targets: BTreeMap<String, TargetEntry>,
}

/// One `[targets.<id>]` table. Reading keeps it raw, because whether an old
/// `kind` is allowed depends on the file's `version`, which is a sibling key;
/// writing always produces the current shape.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum TargetEntry {
    Stored(Box<StoredTarget>),
    Disabled(DisabledTarget),
    Raw(serde_json::Value),
}

/// A runtime written with `enabled = false`. Only a disabled runtime carries
/// the key; an enabled one omits it, as profiles do.
#[derive(Debug, Clone, Serialize)]
struct DisabledTarget {
    #[serde(flatten)]
    target: Box<StoredTarget>,
    enabled: bool,
}

/// Take `enabled` out of a raw `[targets.<id>]` table, so the runtime's own
/// keys are interpreted without it. Absent means enabled.
fn take_target_enabled(id: &str, value: &mut serde_json::Value) -> Result<bool> {
    let Some(table) = value.as_object_mut() else {
        return Ok(true);
    };
    match table.remove("enabled") {
        None => Ok(true),
        Some(serde_json::Value::Bool(enabled)) => Ok(enabled),
        Some(other) => bail!("target {id:?} has enabled = {other}; write true or false"),
    }
}

impl<'de> Deserialize<'de> for TargetEntry {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        serde_json::Value::deserialize(deserializer).map(Self::Raw)
    }
}

/// A `[targets.<id>]` table read from a file, once its `kind` has been judged
/// against the file's version.
enum InterpretedTarget {
    Stored(StoredTarget),
    /// A pre-version-12 table, which fuses a host and a runtime and is
    /// migrated into a machine plus a runtime.
    Legacy(TargetTemplate),
}

/// Keys of a `[targets.<id>]` table that its runtime kind does not read,
/// leaving out any already reported by this process.
///
/// Serde cannot refuse them: a container runtime's settings are flattened into
/// the tagged `StoredTarget`, and flattening turns `deny_unknown_fields` off.
/// A misspelled or invented setting is therefore ignored, and saying so once
/// is what stops a person from believing it applies (R4-6). The configuration
/// is read many times in one process, so each key is reported only the first
/// time.
fn newly_unknown_target_keys(
    id: &str,
    kind: &str,
    table: &serde_json::Map<String, serde_json::Value>,
) -> Vec<String> {
    static REPORTED: std::sync::Mutex<BTreeSet<(String, String)>> =
        std::sync::Mutex::new(BTreeSet::new());
    let known = |key: &str| {
        matches!(key, "kind" | "machine")
            || match kind {
                "bare" => key == "permissions",
                "podman" | "docker" | "apple-container" => {
                    targets::CONTAINER_TEMPLATE_KEYS.contains(&key)
                }
                _ => true,
            }
    };
    let mut reported = REPORTED.lock().unwrap_or_else(|error| error.into_inner());
    table
        .keys()
        .filter(|key| !known(key))
        .filter(|key| reported.insert((id.to_owned(), (*key).clone())))
        .cloned()
        .collect()
}

fn interpret_target(
    id: &str,
    value: &serde_json::Value,
    version: u32,
) -> Result<InterpretedTarget> {
    let Some(table) = value.as_object() else {
        bail!("target {id:?} must be a table");
    };
    let Some(kind) = table.get("kind").and_then(serde_json::Value::as_str) else {
        bail!(
            "target {id:?} needs a kind: one of {}",
            STORED_TARGET_KINDS.join(", ")
        );
    };
    if table.contains_key("permissions") && !matches!(kind, "bare" | "ssh-bare") {
        bail!("target {id:?} sets `permissions`, which only applies to a bare runtime");
    }
    // The message must stand alone: a failure here becomes a plain string
    // inside the TOML or JSON deserializer's own error, which keeps no source
    // chain.
    let from_value = || {
        serde_json::from_value::<StoredTarget>(value.clone())
            .map_err(|error| anyhow::anyhow!("target {id:?} ({kind}): {error}"))
    };
    if STORED_TARGET_KINDS.contains(&kind) {
        // `apple-container` is spelled the same before and after the split.
        // In an old file without a `machine` key it is the fused kind.
        let fused = kind == "apple-container" && !table.contains_key("machine") && version <= 11;
        if !fused {
            for key in newly_unknown_target_keys(id, kind, table) {
                tracing::warn!(
                    target = id,
                    key,
                    "target {id:?} ({kind}) has a key Mjolnir does not use, {key:?}; it is ignored"
                );
            }
            return from_value().map(InterpretedTarget::Stored);
        }
    } else if legacy_kind_advice(kind).is_none() {
        bail!(
            "target {id:?} has unknown kind {kind:?}; use one of {}",
            STORED_TARGET_KINDS.join(", ")
        );
    } else if version > 11 {
        let (new_kind, machine) = legacy_kind_advice(kind).expect("checked above");
        bail!(
            "target {id:?} uses the old kind {kind:?}; write kind = {new_kind:?} and machine = {machine:?}"
        );
    }
    serde_json::from_value::<TargetTemplate>(value.clone())
        .map_err(|error| anyhow::anyhow!("target {id:?} ({kind}): {error}"))
        .map(InterpretedTarget::Legacy)
}

impl TryFrom<StoredConfig> for Config {
    type Error = anyhow::Error;

    fn try_from(stored: StoredConfig) -> Result<Self> {
        let StoredConfig {
            show_stopped_sessions: _,
            sessions_side,
            sidebar_size,
            advanced,
            notify,
            version,
            spinner,
            theme,
            phone,
            daemon,
            mut github,
            review,
            continuation,
            mailbox,
            sessionwiki,
            subagents,
            build_cache,
            jev,
            quota,
            keys,
            legacy_startup,
            mut profiles,
            bundles,
            mut machines,
            targets,
        } = stored;
        github.watch.enabled &= mailbox.enabled;
        github.validate()?;
        for profile in profiles.values_mut() {
            profile.inherit_provider_key();
        }
        let mut runtimes: BTreeMap<String, StoredTarget> = BTreeMap::new();
        let mut disabled_targets = BTreeSet::new();
        for (id, entry) in targets {
            let runtime = match entry {
                TargetEntry::Stored(target) => *target,
                TargetEntry::Disabled(DisabledTarget { target, enabled }) => {
                    if !enabled {
                        disabled_targets.insert(id.clone());
                    }
                    *target
                }
                TargetEntry::Raw(mut value) => {
                    if !take_target_enabled(&id, &mut value)? {
                        disabled_targets.insert(id.clone());
                    }
                    match interpret_target(&id, &value, version)? {
                        InterpretedTarget::Stored(target) => target,
                        // Migration and saving are the same operation, so an
                        // old file becomes exactly what the next save would write.
                        InterpretedTarget::Legacy(target) => {
                            stored_target(&id, &target, &mut machines)
                        }
                    }
                }
            };
            runtimes.insert(id, runtime);
        }
        // Fold the old global veto into each machine before resolving runtimes.
        // Local is implicit even in files that only name remote machines.
        if !build_cache.enabled {
            machines
                .entry(LOCAL_MACHINE_ID.to_owned())
                .or_insert(Machine::Local { build_cache: None });
            for machine in machines.values_mut() {
                match machine {
                    Machine::Local { build_cache } | Machine::Ssh { build_cache, .. } => {
                        build_cache.get_or_insert_default().enabled = Some(false);
                    }
                    Machine::AwsEc2 { .. } => {}
                }
            }
        }
        let mut resolved = BTreeMap::new();
        for (id, runtime) in &runtimes {
            resolved.insert(id.clone(), resolve_target(id, runtime, &machines)?);
        }
        // Up to version 14, a container target the user had touched carried
        // the then-current default image in the file. Move that literal back
        // onto this build's default so an upgraded install follows the image
        // built for its own version; a customized image is left alone.
        if version <= LEGACY_DEFAULT_IMAGE_LAST_VERSION {
            for target in resolved.values_mut() {
                migrate_legacy_default_image(target, DEFAULT_CONTAINER_IMAGE);
            }
        }
        Ok(Self {
            sessions_side,
            sidebar_size,
            advanced,
            notify,
            // Versions 1 through 19 acquire this build's defaults in memory
            // and upgrade on the next ordinary save. Version 12 splits
            // machines from runtimes; version 13 adds automatic continuation;
            // version 14 moves the global cache opt-out onto machines; version
            // 15 stops writing the default image into a container target;
            // 16 adds native tool cache storage; 17 replaces per-profile
            // delegation selectors with global task choices, without migrating them;
            // 18 moves the GitHub event switch to github.watch.enabled; 19 adds
            // `enabled` to runtimes; 20 adds `[daemon]`; 21 adds review counts;
            // 22 adds the session GitHub credential default.
            version: if matches!(version, 1..=21) {
                CONFIG_VERSION
            } else {
                version
            },
            spinner,
            theme,
            phone,
            daemon,
            github,
            review,
            continuation,
            sessionwiki,
            subagents,
            jev,
            quota,
            keys,
            legacy_startup,
            profiles,
            bundles,
            machines,
            targets: resolved,
            default_targets: BTreeSet::new(),
            disabled_targets,
        })
    }
}

impl From<Config> for StoredConfig {
    fn from(config: Config) -> Self {
        let mut machines = config.machines;
        let mut targets = BTreeMap::new();
        for (id, target) in &config.targets {
            let runtime = Box::new(stored_target(id, target, &mut machines));
            let entry = if config.disabled_targets.contains(id) {
                TargetEntry::Disabled(DisabledTarget {
                    target: runtime,
                    enabled: false,
                })
            } else {
                TargetEntry::Stored(runtime)
            };
            targets.insert(id.clone(), entry);
        }
        // This machine is implied, so a file only names it when it carries
        // settings of its own.
        if machines.get(LOCAL_MACHINE_ID) == Some(&Machine::Local { build_cache: None }) {
            machines.remove(LOCAL_MACHINE_ID);
        }
        Self {
            show_stopped_sessions: false,
            sessions_side: config.sessions_side,
            sidebar_size: config.sidebar_size,
            advanced: config.advanced,
            notify: config.notify,
            version: config.version,
            spinner: config.spinner,
            theme: config.theme,
            phone: config.phone,
            daemon: config.daemon,
            github: config.github,
            review: config.review,
            continuation: config.continuation,
            mailbox: LegacyMailboxConfig::default(),
            sessionwiki: config.sessionwiki,
            subagents: config.subagents,
            build_cache: LegacyBuildCacheConfig::default(),
            jev: config.jev,
            quota: config.quota,
            keys: config.keys,
            legacy_startup: config.legacy_startup,
            profiles: config.profiles,
            bundles: config.bundles,
            machines,
            targets,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sessions_side: SessionsSide::default(),
            sidebar_size: crate::workspace::PaneSize::default(),
            advanced: AdvancedConfig::default(),
            notify: NotifyConfig::default(),
            version: CONFIG_VERSION,
            spinner: SpinnerStyle::default(),
            theme: Default::default(),
            phone: PhoneConfig::default(),
            daemon: DaemonConfig::default(),
            github: GithubConfig::default(),
            review: ReviewConfig::default(),
            continuation: crate::continuation::ContinuationConfig::enabled(),
            sessionwiki: SessionWikiConfig::default(),
            subagents: SubagentConfig::default(),
            jev: JevConfig::default(),
            quota: QuotaConfig::default(),
            keys: KeysConfig::default(),
            legacy_startup: (),
            profiles: BTreeMap::new(),
            bundles: BTreeMap::new(),
            machines: BTreeMap::new(),
            targets: BTreeMap::new(),
            default_targets: BTreeSet::new(),
            disabled_targets: BTreeSet::new(),
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
            machines: additions(
                &self.machines,
                &discovered.machines,
                PartialEq::eq,
                |id, _| id.to_owned(),
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

    /// The runtimes that run on machine `machine_id`.
    pub fn machine_runtimes<'a>(
        &'a self,
        machine_id: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a TargetTemplate)> + 'a {
        self.targets
            .iter()
            .filter(move |(_, target)| {
                target_machine_id(target, &self.machines) == Some(machine_id)
            })
            .map(|(id, target)| (id.as_str(), target))
    }

    /// The SSH and EC2 machines no runtime runs on, which no session can
    /// use until one does.
    pub fn machines_without_runtimes(&self) -> impl Iterator<Item = (&str, &Machine)> {
        self.machines
            .iter()
            .filter(|(id, machine)| {
                !matches!(machine, Machine::Local { .. })
                    && self.machine_runtimes(id).next().is_none()
            })
            .map(|(id, machine)| (id.as_str(), machine))
    }

    /// The runtimes from `found` to add for machine `machine_id`, under
    /// collision-free ids: each kind (bare, podman, docker) the machine does
    /// not run yet. `probed` is the machine as it was when `found` was
    /// learned; when the machine has since changed or gone, nothing is
    /// added, because what was found describes a host this configuration no
    /// longer names. Every surface that adds a machine's runtimes, from the
    /// file under its lock to the Settings draft, decides here.
    pub fn machine_runtime_additions(
        &self,
        machine_id: &str,
        probed: &Machine,
        found: impl IntoIterator<Item = (String, TargetTemplate)>,
    ) -> BTreeMap<String, TargetTemplate> {
        if self.machines.get(machine_id) != Some(probed) {
            return BTreeMap::new();
        }
        let mut kinds: BTreeSet<&str> = self
            .machine_runtimes(machine_id)
            .map(|(_, target)| runtime_kind_name(target))
            .collect();
        let mut known = self.targets.clone();
        let mut added = BTreeMap::new();
        for (base, target) in found {
            if target_machine_id(&target, &self.machines) != Some(machine_id)
                || !kinds.insert(runtime_kind_name(&target))
            {
                continue;
            }
            let id = unique_config_id(&known, &base);
            known.insert(id.clone(), target.clone());
            added.insert(id, target);
        }
        added
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

    /// Runtimes available for new work: every one not switched off.
    pub fn enabled_targets(&self) -> impl Iterator<Item = (&str, &TargetTemplate)> {
        self.targets
            .iter()
            .filter(|(id, _)| self.enabled_target(id).is_some())
            .map(|(id, target)| (id.as_str(), target))
    }

    /// One runtime when it exists and is available for new work.
    pub fn enabled_target(&self, id: &str) -> Option<&TargetTemplate> {
        self.targets.get(id).filter(|target| {
            target.launch_refusal().is_none() && !self.disabled_targets.contains(id)
        })
    }

    /// Refuse new work on runtime `id` when it is not configured or is
    /// switched off. Every launch, resume and move asks this.
    pub fn ensure_target_enabled(&self, id: &str) -> Result<&TargetTemplate> {
        let target = self
            .targets
            .get(id)
            .with_context(|| format!("unknown target template {id:?}"))?;
        if let Some(notice) = target.launch_refusal() {
            bail!("{notice}");
        }
        if self.disabled_targets.contains(id) {
            return Err(crate::refusal::Refusal::precondition(format!(
                "runtime {id:?} is turned off; turn it on in Settings › Runtimes or choose another"
            ))
            .into());
        }
        Ok(target)
    }

    /// Whether a session recorded under `source_profile_id`, running
    /// `source_kind`, keeps its native conversation when it continues under
    /// `destination_profile_id`. When it does not, a resume or move gives a
    /// fresh native session the transcript as compacted text.
    ///
    /// The harness must match, and so must the model service, because two
    /// Codex profiles can point at different providers. Continuity needs
    /// evidence: when the source profile is gone or now runs another harness,
    /// only a harness whose service does not depend on the profile continues.
    pub fn native_continuity(
        &self,
        source_kind: HarnessKind,
        source_profile_id: &str,
        destination_profile_id: &str,
    ) -> Result<bool> {
        let destination = self
            .profiles
            .get(destination_profile_id)
            .with_context(|| format!("unknown profile {destination_profile_id:?}"))?;
        if destination.kind != source_kind {
            return Ok(false);
        }
        if destination_profile_id == source_profile_id {
            return Ok(true);
        }
        let source_service = match self.profiles.get(source_profile_id) {
            Some(source) if source.kind == source_kind => Some(source.model_service()?),
            _ => ModelService::of_harness(source_kind),
        };
        let Some(source_service) = source_service else {
            return Ok(false);
        };
        Ok(source_service == destination.model_service()?)
    }

    /// Check what this file itself declares. Whether a profile or target can
    /// start depends on harness homes, secrets and variables outside it, and
    /// is asked of [`HarnessProfile::ensure_ready`] and
    /// [`TargetTemplate::ensure_ready`] where they are used, so one unusable
    /// profile never stops the configuration, and the daemon, from loading.
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
        self.subagents.validate()?;
        for (id, bundle) in &self.bundles {
            bundle.validate(id)?;
        }
        machines::validate_machines(&self.machines)?;
        for (id, target) in &self.targets {
            target.validate(id)?;
        }
        self.keys.resolve()?;
        Ok(())
    }

    /// The key bindings in force.
    ///
    /// A `Config` in hand has already passed `validate`, so the fallback to
    /// the defaults here can only be reached by an unvalidated value.
    pub fn keybinds(&self) -> Keybinds {
        self.keys.resolve().unwrap_or_default()
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&config_path()).map(Self::with_local_targets)
    }

    /// Supply standard local choices without requiring setup or writing a file.
    /// These are candidates: callers must check availability before offering launch.
    /// Explicit entries with the same name override the standard defaults.
    // The default list is built with per-platform pushes.
    #[allow(clippy::vec_init_then_push)]
    pub fn with_local_targets(mut self) -> Self {
        let container = ContainerTemplate {
            build_cache: None,
            image: DEFAULT_CONTAINER_IMAGE.into(),
            pull_policy: Default::default(),
            platform: None,
            cpus: None,
            memory: None,
            environment: Default::default(),
            workspace_storage: Default::default(),
        };
        let mut defaults: Vec<(&str, TargetTemplate)> = Vec::new();
        if crate::targets::HOST_RUNS_WORKERS {
            defaults.push(("localhost", TargetTemplate::LocalBare));
        }
        defaults.push((
            "podman",
            TargetTemplate::LocalPodman {
                container: container.clone(),
            },
        ));
        defaults.push((
            "docker",
            TargetTemplate::LocalDocker {
                container: container.clone(),
            },
        ));
        #[cfg(target_os = "macos")]
        defaults.push((
            "apple-container",
            TargetTemplate::AppleContainer { container },
        ));
        for (id, template) in defaults {
            if let std::collections::btree_map::Entry::Vacant(entry) = self.targets.entry(id.into())
            {
                entry.insert(template);
                self.default_targets.insert(id.into());
            }
        }
        self
    }

    /// Whether target `id` is a default candidate that
    /// [`Self::with_local_targets`] supplied, rather than an entry the user
    /// wrote in the config file. Read this, do not compare names: a user who
    /// writes `[targets.docker]` configured it.
    pub fn is_default_target(&self, id: &str) -> bool {
        self.default_targets.contains(id)
    }

    /// Whether the user configured target `id`: the file names it and the
    /// entry differs from the standard local target of the same name that
    /// [`Self::with_local_targets`] supplies. An entry that repeats a
    /// standard target word for word adds nothing the user chose, so callers
    /// that treat built-in targets gently treat it as built-in too.
    pub fn configures_target(&self, id: &str) -> bool {
        self.targets.get(id).is_some_and(|target| {
            Self::default().with_local_targets().targets.get(id) != Some(target)
        })
    }

    /// Read the config from `path`, returning [`Config::default`] when the
    /// file is missing and an error when it is malformed or was
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
        let mut document: toml::Value = contents
            .parse()
            .with_context(|| format!("parse Mjolnir config {}", path.display()))?;
        if let Some(found) = newer_version(&document) {
            bail!(
                "{} was written by a newer Mjolnir (config version {found}; this build supports \
                 {CONFIG_VERSION}). Update Mjolnir",
                path.display()
            );
        }
        // Every file Mjolnir writes starts with this line, so only a
        // hand-written one lacks it. Serde's "missing field `version`" does
        // not say which value to add (R14-4).
        if document.get("version").is_none() {
            bail!(
                "{}: config.toml needs a `version = {CONFIG_VERSION}` line at the top (the \
                 current configuration schema); see {CONFIGURATION_DOCUMENTATION_URL}",
                path.display()
            );
        }
        reject_removed_profile_overrides(&contents)?;
        let removed = drop_removed_harness_profiles(&mut document);
        let contents = if removed.is_empty() {
            contents
        } else {
            tracing::warn!(
                config = %path.display(),
                profiles = ?removed,
                "ignoring profiles for a harness Mjolnir no longer supports; the next save removes them"
            );
            toml::to_string(&document)
                .with_context(|| format!("rewrite Mjolnir config {}", path.display()))?
        };
        // Version 2 adds Podman workspace storage; version 3 restores the
        // spinner preference; version 4 adds stopped-session visibility;
        // version 5 adds the terminal theme preference; version 6 adds
        // optional advanced settings; version 7 restores stopped-session
        // visibility as an advanced setting; version 8 lets profiles be
        // disabled; version 9 adds sub-agent policy; version 10 adds the
        // SessionWiki section; version 11 adds the key bindings; version 12
        // splits machines from runtimes; version 13 adds automatic continuation.
        // Earlier configs acquire defaults in
        // memory, and their fused target kinds become machines plus runtimes,
        // on the next ordinary save. The version bump itself happens in
        // `TryFrom<StoredConfig>`, which is also what decides whether an old
        // `kind` is still accepted.
        // Environment references resolve against the secrets file beside
        // this configuration and the process environment. One that does not
        // resolve makes its profile or target unusable, not this file
        // unreadable; see `HarnessProfile::ensure_ready`.
        let config: Self =
            with_secret_resolver(SecretResolver::beside(path), || toml::from_str(&contents))
                .with_context(|| format!("parse Mjolnir config {}", path.display()))?;
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
        Self::update_with_local_targets_at(&config_path(), edit)
    }

    /// As [`Self::update`], using an explicit config path.
    pub fn update_with_local_targets_at<T, F>(path: &Path, edit: F) -> Result<(Self, T)>
    where
        F: FnOnce(&mut Self) -> Result<T>,
    {
        Self::update_to(path, |config| {
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
                // A standard runtime switched off is the user's choice and
                // is written with it.
                if runtime.targets.get(&id) == Some(&target)
                    && !runtime.disabled_targets.contains(&id)
                {
                    runtime.targets.remove(&id);
                }
            }
            runtime.default_targets.clear();
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
        let body = Self::edited_file(path, &body).unwrap_or(body);
        atomic_write(path, body.as_bytes())
    }

    /// The file at `path` edited in place to hold `body`, keeping its
    /// comments and layout. `None` when there is no file to keep, or it
    /// cannot be edited in place.
    fn edited_file(path: &Path, body: &str) -> Option<String> {
        let existing = fs::read_to_string(path).ok()?;
        let loaded = toml::to_string_pretty(&Self::load_from(path).ok()?).ok()?;
        document::edit_in_place(&existing, &loaded, body)
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
    _file: crate::file_lock::FileLock,
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
        Ok(Self {
            _file: crate::file_lock::FileLock::new(file),
        })
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
