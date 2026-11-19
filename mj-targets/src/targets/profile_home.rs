//! Where a session's harness profile lives on its target.

use super::*;

pub fn target_profile_home(
    locator: &TargetLocator,
    session_id: &str,
    profile: &mj_core::config::HarnessProfile,
) -> String {
    let root = removable_profile_root(locator, session_id, profile);
    if profile.kind.nested_home() {
        mj_core::target_path::text(&mj_core::target_path::join(
            Path::new(&root),
            Path::new(profile.kind.id()),
        ))
    } else {
        root
    }
}

/// The per-session profile directory a session's harness home lives in, which
/// the session owns and an in-place harness replacement or teardown may delete.
///
/// This is the root that [`target_profile_home`] derives its answer from, not
/// that answer itself: a harness with an XDG-nested home keeps its home in a
/// subdirectory of a per-session root under the data directory, and the whole
/// root is what belongs to the session.
pub fn removable_profile_root(
    locator: &TargetLocator,
    session_id: &str,
    profile: &mj_core::config::HarnessProfile,
) -> String {
    match locator {
        TargetLocator::LocalBare { .. } if profile.kind.nested_home() => {
            local_nested_profile_root(session_id)
                .to_string_lossy()
                .into_owned()
        }
        TargetLocator::LocalBare { worker_root } => mj_core::target_path::text(
            &mj_core::target_path::join(Path::new(worker_root), Path::new("profile")),
        ),
        TargetLocator::LocalPodman { .. }
        | TargetLocator::LocalDocker { .. }
        | TargetLocator::AppleContainer { .. }
        | TargetLocator::SshPodman { .. }
        | TargetLocator::SshDocker { .. }
        | TargetLocator::SshAppleContainer { .. }
        | TargetLocator::SshNitro { .. } => {
            format!("/var/lib/hel/profiles/{session_id}")
        }
        TargetLocator::AwsEc2 { .. } | TargetLocator::SshBare { .. } => {
            format!(".local/share/hel/profiles/{session_id}")
        }
    }
}
