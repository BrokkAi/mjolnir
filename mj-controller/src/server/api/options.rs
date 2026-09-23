use super::*;

use std::path::Path;

use crate::server::{ViewerTarget, ViewerTargetCapacity};

/// Report what a caller may launch, and which pair to use when it names none.
///
/// Read-only and total. A caller that cannot read one saved preference still
/// needs the lists, and a target nobody has checked yet is `unknown` rather
/// than an error, so this route has no failure of its own.
pub(super) async fn options(State(state): State<ServerState>) -> Json<LaunchOptions> {
    let snapshot = state.snapshot_rx.borrow();
    Json(launch_options(&snapshot, &state.preferences_path))
}

/// Build the launch options from one projection.
///
/// Kept apart from the handler so the mapping is a pure function of the
/// projection and the preferences path, which is what the contract tests
/// exercise. Everything here comes from the viewer projection, which is
/// already the controller's redacted answer to "what is configured"; this
/// function must not reach past it into `Config`.
pub(super) fn launch_options(snapshot: &ViewerSnapshot, preferences_path: &Path) -> LaunchOptions {
    LaunchOptions {
        revision: snapshot.revision,
        profiles: snapshot
            .profiles
            .iter()
            .map(|profile| LaunchProfile {
                id: profile.id.clone(),
                harness: profile.harness_kind.clone(),
            })
            .collect(),
        targets: snapshot
            .targets
            .iter()
            .map(|target| launch_target(target, &snapshot.capacity))
            .collect(),
        bundles: snapshot
            .bundles
            .iter()
            .map(|bundle| LaunchBundle {
                id: bundle.id.clone(),
                primary_repository: bundle.primary_repository.clone(),
                repositories: bundle
                    .repositories
                    .iter()
                    .map(|repository| LaunchRepository {
                        id: repository.id.clone(),
                        github: repository.github.clone(),
                        destination: repository.destination.clone(),
                    })
                    .collect(),
            })
            .collect(),
        hosts: snapshot
            .capacity
            .iter()
            .map(|host| LaunchHost {
                id: host.id.clone(),
                label: host.label.clone(),
                targets: host.target_ids.clone(),
                stale: host.stale,
                refreshing: host.refreshing,
                has_error: host.has_error,
            })
            .collect(),
        default: saved_default(preferences_path),
    }
}

/// Join one target to the host reading that covers it.
///
/// A target without a reading is `unknown`, never `unavailable`. The capacity
/// poller publishes nothing until it has run, and reporting that silence as a
/// failure would send a caller to fix something that is not broken. The
/// explanatory sentence is composed here because a probe's own message names
/// hosts and commands and is kept on the controller.
fn launch_target(target: &ViewerTarget, hosts: &[ViewerTargetCapacity]) -> LaunchTarget {
    let host = hosts
        .iter()
        .find(|host| host.target_ids.iter().any(|id| id == &target.id));
    let availability = match host {
        None => LaunchAvailability::Unknown,
        Some(host) if host.has_error => LaunchAvailability::Unavailable,
        Some(host) if host.stale => LaunchAvailability::Stale,
        Some(_) => LaunchAvailability::Ready,
    };
    let unavailable_reason = match (availability, host) {
        (LaunchAvailability::Unavailable, Some(host)) => Some(format!(
            "the host \"{}\" did not answer its last check",
            host.label
        )),
        _ => None,
    };
    LaunchTarget {
        id: target.id.clone(),
        kind: target.kind.clone(),
        requires_project_directory: target.requires_project_directory,
        availability,
        unavailable_reason,
        host: host.map(|host| host.label.clone()),
    }
}

/// Read the pair the user saved as their default, which the first setup also
/// becomes.
///
/// A missing file is ordinary, and a damaged one is not worth failing a read
/// the caller can otherwise use, so both answer `None` with the cause recorded
/// at debug level. Session creation resolves an omitted profile or target from
/// the same value, so the pair a caller reads here is the pair it may leave
/// unnamed.
pub(super) fn saved_default(path: &Path) -> Option<LaunchDefault> {
    match mj_core::go::GoPreferences::load(path) {
        Ok(preferences) => preferences.default.map(|recipe| LaunchDefault {
            profile_id: recipe.profile_id,
            target_id: recipe.target_id,
        }),
        Err(error) => {
            tracing::debug!(
                %error,
                path = %path.display(),
                "could not read fast-start preferences for launch options"
            );
            None
        }
    }
}
