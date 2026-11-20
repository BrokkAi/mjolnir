use super::*;

use std::path::Path;

use crate::server::{ViewerTarget, ViewerTargetCapacity};
use mj_client::availability::AvailabilityVerdict;

/// Read the launch verdicts and the user's saved default.
pub(super) async fn options(State(state): State<ServerState>) -> Json<LaunchOptions> {
    let snapshot = state.snapshot_rx.borrow().clone();
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
                availability: launch_availability(&profile.availability),
                unavailable_reason: profile.availability.missing_reason().map(str::to_owned),
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

fn launch_target(target: &ViewerTarget, hosts: &[ViewerTargetCapacity]) -> LaunchTarget {
    let host = hosts
        .iter()
        .find(|host| host.target_ids.iter().any(|id| id == &target.id));
    LaunchTarget {
        id: target.id.clone(),
        kind: target.kind.clone(),
        requires_project_directory: target.requires_project_directory,
        availability: launch_availability(&target.availability),
        unavailable_reason: target.availability.missing_reason().map(str::to_owned),
        runtime_missing: target.runtime_missing,
        default_candidate: target.default_candidate,
        host: host.map(|host| host.label.clone()),
    }
}

fn launch_availability(verdict: &AvailabilityVerdict) -> LaunchAvailability {
    match verdict {
        AvailabilityVerdict::Available => LaunchAvailability::Ready,
        AvailabilityVerdict::Missing { .. } => LaunchAvailability::Unavailable,
        AvailabilityVerdict::Unknown => LaunchAvailability::Unknown,
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
