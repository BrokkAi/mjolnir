//! Sending a host the shareable content a later reviewer launch will need.
use super::*;
use mj_core::remove_tree;
use mj_core::state::SessionRecord;

/// What one prewarm did for one host and profile.
#[derive(Debug, PartialEq, Eq)]
pub enum PrewarmOutcome {
    /// The host now holds this profile's shareable objects (the negotiation
    /// sent only the ones it lacked).
    Warm,
    /// Nothing to send: why the session's host takes no shared profile content.
    NotApplicable(&'static str),
}

/// The SSH account that holds this locator's shared profile content, when its
/// launches use the shared store. A Nitro guest does not share its profile
/// objects with the hypervisor account or another guest, and a local target
/// has no link to spare.
pub fn shared_content_ssh(locator: &targets::TargetLocator) -> Option<&SshTarget> {
    match locator {
        targets::TargetLocator::AwsEc2 { ssh, .. }
        | targets::TargetLocator::SshBare { ssh, .. }
        | targets::TargetLocator::SshHostBare { ssh, .. }
        | targets::TargetLocator::SshPodman { ssh, .. }
        | targets::TargetLocator::SshDocker { ssh, .. }
        | targets::TargetLocator::SshAppleContainer { ssh, .. } => Some(ssh),
        _ => None,
    }
}

/// Stages `profile_id`'s shareable files the way a reviewer launch stages them and offers the
/// host the shareable objects, so the launch that follows sends only its
/// private bundle. It rides the negotiation a launch uses, on the bulk lane,
/// with no operation deadline: the executor's cancellation is what stops it.
pub fn prewarm_profile_content(
    config: &mj_core::config::Config,
    session: &SessionRecord,
    profile_id: &str,
    executor: &impl CommandExecutor,
) -> Result<PrewarmOutcome> {
    let locator = session
        .target
        .as_ref()
        .context("session target is missing")?;
    let backend = backend_locator(locator, session, config)?;
    let Some(ssh) = shared_content_ssh(&backend) else {
        return Ok(PrewarmOutcome::NotApplicable(
            "the session's target does not use the shared profile store",
        ));
    };
    let profile = config
        .enabled_profile(profile_id)
        .with_context(|| format!("profile {profile_id:?} is unavailable"))?;
    let staging = tempfile::tempdir().context("create profile prewarm staging")?;
    let stage = staging.path().join("profile");
    harness::stage_profile(profile, &stage)?;
    // The provider's model catalog (`models.json`) is a shared file, but
    // fetching it is a network wait a cancelled daemon cannot interrupt. Prewarm
    // offers the catalog only when this daemon already holds a copy from an
    // earlier launch; without one it is left for the launch that needs it.
    if let Err(error) = catalog::stage_codex_catalog(
        profile_id,
        profile,
        &stage,
        &|_, _| anyhow::bail!("prewarm does not fetch model catalogs"),
        &catalog::SharedCatalogCache,
    ) {
        tracing::debug!(
            profile_id,
            error = format!("{error:#}"),
            "prewarm skips the model catalog"
        );
        // A failed catalog step may have left a partial change in the stage;
        // stage the plain profile again rather than offer it.
        remove_tree::remove_dir_all(&stage).ok();
        harness::stage_profile(profile, &stage)?;
    }
    stage_managed_skills(profile.kind, &stage, locator.skills_scope())?;
    let roots = shared_profile_paths(profile.kind);
    let manifest = ProfileContentManifest::from_staged(&stage, &roots)?;
    if manifest.try_transfer(executor, ssh, None)? {
        Ok(PrewarmOutcome::Warm)
    } else {
        Ok(PrewarmOutcome::NotApplicable(
            "the host lacks the profile receiver runtime",
        ))
    }
}
