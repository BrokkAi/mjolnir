//! Pure target and resume compatibility contracts shared by control surfaces.

use anyhow::{Result, bail};
use hel::hel_config::{HelConfig, TargetTemplate, is_bare_project_target};
use hel::hel_state::{ManagedWorktreeTarget, SessionRecord};

// Published from containers/Containerfile.agent-dev by
// .github/workflows/publish-agent-dev-image.yml. It already carries Node, Rust,
// Git, gh, and the pinned ACP bridges, so a first session does not have to
// install them.
pub const DEFAULT_IMAGE: &str = "ghcr.io/brokkai/mjolnir/agent-dev:latest";

/// Convert a configured bare target to the durable target identity stored on
/// managed worktrees.
pub fn managed_worktree_target(template: &TargetTemplate) -> Result<ManagedWorktreeTarget> {
    match template {
        TargetTemplate::LocalBare => Ok(ManagedWorktreeTarget::Local),
        TargetTemplate::SshBare { ssh, .. } => {
            let destination = match &ssh.user {
                Some(user) => format!("{user}@{}", ssh.host),
                None => ssh.host.clone(),
            };
            // Keep this in lockstep with the controller's SSH backend. These
            // options are part of the target identity because the managed
            // worktree compares it when deciding whether a resume stays put.
            let mut ssh_args = vec![
                "-o".to_owned(),
                "BatchMode=yes".to_owned(),
                "-o".to_owned(),
                "StrictHostKeyChecking=accept-new".to_owned(),
                "-o".to_owned(),
                "ConnectTimeout=15".to_owned(),
            ];
            ssh_args.extend(ssh.extra_args.iter().cloned());
            if let Some(identity) = &ssh.identity_file {
                ssh_args.push("-i".to_owned());
                ssh_args.push(identity.to_string_lossy().into_owned());
            }
            Ok(ManagedWorktreeTarget::Ssh {
                destination,
                ssh_args,
            })
        }
        _ => bail!("managed raw worktrees require a bare target"),
    }
}

/// What a resume has to do to the session record before it provisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumePlan {
    /// Keep the session in the representation it already has.
    InPlace,
    /// Move a raw checkout session into a workspace target as a bundle session.
    RawToWorkspace,
    /// Move a bundle session out of its workspace into a raw local worktree.
    WorkspaceToRaw,
}

/// Whether `session` may resume on `target_id`, and what the resume must do to
/// the session record. The error is shown to the person choosing the target, so
/// it says where the session is tied down and what to pick instead.
///
/// This decides representation only. It performs no I/O, so it can run on every
/// row of a target picker.
pub fn resume_compatibility(
    session: &SessionRecord,
    config: &HelConfig,
    target_id: &str,
) -> Result<ResumePlan, String> {
    let Some(target) = config.targets.get(target_id) else {
        return Err(format!("target {target_id} is no longer configured"));
    };
    let Some(project_directory) = &session.project_directory else {
        if matches!(target, TargetTemplate::LocalBare) {
            return workspace_to_raw_compatibility(session, config);
        }
        return Ok(ResumePlan::InPlace);
    };
    let directory = project_directory.display();
    let Some(worktree) = &session.managed_worktree else {
        let Some(previous) = config.targets.get(&session.target_template_id) else {
            return Err(
                "the bare target this session last used is no longer configured".to_owned(),
            );
        };
        if is_bare_project_target(target) {
            if matches!(previous, TargetTemplate::LocalBare)
                == matches!(target, TargetTemplate::LocalBare)
            {
                return Ok(ResumePlan::InPlace);
            }
            return Err(format!(
                "this session opens {directory} directly on its host; resume it on the same kind of bare target"
            ));
        }
        if matches!(previous, TargetTemplate::LocalBare) {
            return Err("raw sessions do not have isolated network repository provenance; resume on a bare target or start a new isolated session".to_owned());
        }
        return Err(format!(
            "this session opens {directory} on an SSH host; resume it on a bare target there"
        ));
    };
    match managed_worktree_target(target) {
        Ok(resume_target) if resume_target == worktree.target => Ok(ResumePlan::InPlace),
        Ok(_) => Err(format!(
            "this session's working tree lives on {}; resume it there",
            managed_worktree_location(&worktree.target)
        )),
        Err(_) if worktree.target != ManagedWorktreeTarget::Local => Err(format!(
            "this session works directly in {directory} on {}; resume it on a bare target there",
            managed_worktree_location(&worktree.target)
        )),
        // Reject this in the target picker and live-move preparation, before
        // any source is stopped: raw checkpoints cannot seed isolated clones.
        Err(_) if Some(&worktree.worktree_root) == session.project_directory.as_ref() => {
            Err("raw sessions do not have isolated network repository provenance; resume on a bare target or start a new isolated session".to_owned())
        }
        Err(_) => Err(format!(
            "this session opens {directory}, a subdirectory of its checkout; resume it on a bare target"
        )),
    }
}

/// Why a bundle session cannot resume on a local bare target. A bare target has
/// no managed workspace to restore the bundle into.
const BUNDLE_ON_LOCAL_BARE: &str = "this session was created from a project bundle; a local bare target only hosts raw project sessions — resume it on a container, SSH, or EC2 target";

/// Whether a bundle session can leave its workspace for a checkout on this
/// machine. Only a single repository already on this machine can become one.
fn workspace_to_raw_compatibility(
    session: &SessionRecord,
    config: &HelConfig,
) -> Result<ResumePlan, String> {
    let Some(bundle) = config.bundles.get(&session.bundle_id) else {
        return Err(BUNDLE_ON_LOCAL_BARE.to_owned());
    };
    let [repository] = bundle.repositories.as_slice() else {
        return Err(format!(
            "this session's project has {} repositories; a local bare target holds one checkout — resume it on a container, SSH, or EC2 target",
            bundle.repositories.len()
        ));
    };
    if repository.local.is_none() {
        return Err(
            "this session's project came from GitHub; resume it on a container, SSH, or EC2 target"
                .to_owned(),
        );
    }
    Ok(ResumePlan::WorkspaceToRaw)
}

/// Where a managed worktree's checkout physically lives, in words a user
/// reads.
fn managed_worktree_location(target: &ManagedWorktreeTarget) -> String {
    match target {
        ManagedWorktreeTarget::Local => "this machine".to_owned(),
        ManagedWorktreeTarget::Ssh { destination, .. } => destination.clone(),
    }
}
