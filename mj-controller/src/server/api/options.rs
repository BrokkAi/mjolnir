use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::controller::LocalEngineReadiness;
use crate::server::{ViewerTarget, ViewerTargetCapacity};

/// Report what a caller may launch, and which pair to use when it names none.
///
/// Read-only and total. A caller that cannot read one saved preference still
/// needs the lists, and a target nobody has checked yet is `unknown` rather
/// than an error, so this route has no failure of its own.
pub(super) async fn options(State(state): State<ServerState>) -> Json<LaunchOptions> {
    let snapshot = state.snapshot_rx.borrow().clone();
    let kinds = snapshot
        .targets
        .iter()
        .map(|target| target.kind.clone())
        .collect::<BTreeSet<_>>();
    let checks = state.engine_checks.clone();
    // The probes run commands, so they stay off the async runtime. A failed
    // join leaves every engine unchecked, which reads as the host reading alone.
    let engines = tokio::task::spawn_blocking(move || checks.readiness(kinds))
        .await
        .unwrap_or_default();
    Json(launch_options(&snapshot, &state.preferences_path, &engines))
}

/// The engine check behind a local container target, as the launch preflight
/// runs it. Injected so tests do not depend on the engines this host has.
pub(crate) type EngineProbe = Arc<dyn Fn(&str) -> Option<LocalEngineReadiness> + Send + Sync>;

/// Local engine readiness for the options route, cached with the same
/// lifetimes the dashboard's new-session wizard uses, so repeated reads do not
/// run `docker version` every time.
pub(crate) struct LocalEngineChecks {
    probe: EngineProbe,
    cache: std::sync::Mutex<BTreeMap<String, (Instant, LocalEngineReadiness)>>,
}

/// How long a successful engine check stands (the wizard's `TARGET_READINESS_TTL`).
const ENGINE_READY_TTL: Duration = Duration::from_secs(30 * 60);
/// How long a failed engine check stands (the wizard's
/// `TARGET_READINESS_FAILURE_TTL`), so a person who just installed or
/// started the engine does not wait long to see it.
const ENGINE_FAILURE_TTL: Duration = Duration::from_secs(60);

impl LocalEngineChecks {
    pub(crate) fn new(probe: EngineProbe) -> Self {
        Self {
            probe,
            cache: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// Run the real engine probes on this host.
    pub(crate) fn on_this_host() -> Self {
        Self::new(Arc::new(|kind| {
            crate::controller::local_engine_readiness(kind, &crate::targets::ProcessExecutor)
        }))
    }

    /// Readiness for each target kind that has a local engine; other kinds
    /// are left out.
    fn readiness(&self, kinds: BTreeSet<String>) -> BTreeMap<String, LocalEngineReadiness> {
        let now = Instant::now();
        let mut found = BTreeMap::new();
        for kind in kinds {
            let cached = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&kind)
                .copied();
            let readiness = match cached {
                Some((checked, readiness))
                    if now.duration_since(checked)
                        < match readiness {
                            LocalEngineReadiness::Ready => ENGINE_READY_TTL,
                            _ => ENGINE_FAILURE_TTL,
                        } =>
                {
                    Some(readiness)
                }
                _ => {
                    let readiness = (self.probe)(&kind);
                    if let Some(readiness) = readiness {
                        self.cache
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(kind.clone(), (Instant::now(), readiness));
                    }
                    readiness
                }
            };
            if let Some(readiness) = readiness {
                found.insert(kind, readiness);
            }
        }
        found
    }
}

/// Build the launch options from one projection.
///
/// Kept apart from the handler so the mapping is a pure function of the
/// projection and the preferences path, which is what the contract tests
/// exercise. Everything here comes from the viewer projection, which is
/// already the controller's redacted answer to "what is configured"; this
/// function must not reach past it into `Config`.
pub(super) fn launch_options(
    snapshot: &ViewerSnapshot,
    preferences_path: &Path,
    engines: &BTreeMap<String, LocalEngineReadiness>,
) -> LaunchOptions {
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
            .map(|target| launch_target(target, &snapshot.capacity, engines.get(&target.kind)))
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
///
/// A local container target is also only as ready as its engine: a host that
/// answers but has no Docker cannot run a `local-docker` session. `engine` is
/// the launch preflight's engine check, the same one `mj doctor` and the
/// dashboard's Targets pane report, so the three agree.
fn launch_target(
    target: &ViewerTarget,
    hosts: &[ViewerTargetCapacity],
    engine: Option<&LocalEngineReadiness>,
) -> LaunchTarget {
    let host = hosts
        .iter()
        .find(|host| host.target_ids.iter().any(|id| id == &target.id));
    let engine_failure = match engine {
        Some(LocalEngineReadiness::NotInstalled) => Some(format!(
            "{} is not installed on this host",
            engine_name(&target.kind)
        )),
        Some(LocalEngineReadiness::NotReady) => Some(format!(
            "{} did not answer its check on this host; start it and try again",
            engine_name(&target.kind)
        )),
        Some(LocalEngineReadiness::Ready) | None => None,
    };
    let availability = match host {
        _ if engine_failure.is_some() => LaunchAvailability::Unavailable,
        None => LaunchAvailability::Unknown,
        Some(host) if host.has_error => LaunchAvailability::Unavailable,
        Some(host) if host.stale => LaunchAvailability::Stale,
        Some(_) => LaunchAvailability::Ready,
    };
    let unavailable_reason = match (availability, host) {
        _ if engine_failure.is_some() => engine_failure,
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

/// How a person names the engine behind a local container target kind.
fn engine_name(kind: &str) -> &'static str {
    match kind {
        "local-podman" => "Podman",
        "local-docker" => "Docker",
        "apple-container" => "Apple container",
        _ => "The container engine",
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
