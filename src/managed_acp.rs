//! Discovery and installation of first-class ACP agent binaries that mj
//! manages itself: a PATH binary wins, otherwise the official binary is
//! downloaded from the ACP registry and recorded in a per-agent manifest.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::registry::{Agent, BinaryTarget};

/// Identity of one managed agent. `registry_id` doubles as the ACP registry
/// `agents[].id` and the directory name under the install root.
pub struct Spec {
    pub registry_id: &'static str,
    pub display_name: &'static str,
    pub vendor: crate::auth::AuthVendor,
}

static INSTALL_STATES: LazyLock<Mutex<HashMap<&'static str, InstallState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone)]
enum InstallState {
    Idle,
    Installing,
    Ready(ManagedLaunch),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ManagedLaunch {
    version: String,
    command: PathBuf,
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Detection {
    pub path: Option<PathBuf>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub evidence: String,
    pub installing: bool,
    pub error: Option<String>,
}

fn install_state(spec: &Spec) -> InstallState {
    INSTALL_STATES
        .lock()
        .map(|states| {
            states
                .get(spec.registry_id)
                .cloned()
                .unwrap_or(InstallState::Idle)
        })
        .unwrap_or_else(|_| {
            InstallState::Failed(format!(
                "{} installer state is unavailable",
                spec.display_name
            ))
        })
}

fn set_install_state(spec: &'static Spec, state: InstallState) {
    if let Ok(mut states) = INSTALL_STATES.lock() {
        states.insert(spec.registry_id, state);
    }
}

pub fn detect(spec: &Spec) -> Detection {
    if let Some(path) = crate::auth::executable(spec.vendor) {
        return detected_path(spec, path);
    }
    if let Some(launch) = read_manifest(spec).filter(|launch| valid_launch(spec, launch)) {
        return detected_managed(spec, launch);
    }
    detection_for_state(spec, install_state(spec))
}

fn detected_path(spec: &Spec, path: PathBuf) -> Detection {
    Detection {
        path: Some(path),
        args: vec!["acp".to_string()],
        env: HashMap::new(),
        evidence: format!("{} on PATH", spec.display_name),
        installing: false,
        error: None,
    }
}

fn detection_for_state(spec: &Spec, state: InstallState) -> Detection {
    match state {
        InstallState::Idle => Detection {
            path: None,
            args: vec!["acp".to_string()],
            env: HashMap::new(),
            evidence: format!("{} is not installed", spec.display_name),
            installing: false,
            error: None,
        },
        InstallState::Installing => Detection {
            path: None,
            args: vec!["acp".to_string()],
            env: HashMap::new(),
            evidence: format!("installing managed {}", spec.display_name),
            installing: true,
            error: None,
        },
        InstallState::Ready(launch) => detected_managed(spec, launch),
        InstallState::Failed(error) => Detection {
            path: None,
            args: vec!["acp".to_string()],
            env: HashMap::new(),
            evidence: format!("managed {} install failed", spec.display_name),
            installing: false,
            error: Some(error),
        },
    }
}

fn detected_managed(spec: &Spec, launch: ManagedLaunch) -> Detection {
    Detection {
        path: Some(launch.command),
        args: launch.args,
        env: launch.env,
        evidence: format!("managed {} {}", spec.display_name, launch.version),
        installing: false,
        error: None,
    }
}

pub fn start_background_install(spec: &'static Spec) {
    let detection = detect(spec);
    if detection.path.is_some() || detection.installing {
        return;
    }
    if let Ok(mut states) = INSTALL_STATES.lock() {
        let state = states.entry(spec.registry_id).or_insert(InstallState::Idle);
        if !begin_install(state) {
            return;
        }
    } else {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        set_install_state(
            spec,
            InstallState::Failed(format!(
                "{} background installation requires an async runtime",
                spec.display_name
            )),
        );
        return;
    };
    runtime.spawn(async move {
        let result = install_latest(spec).await;
        set_install_state(spec, completed_install(result));
    });
}

fn begin_install(state: &mut InstallState) -> bool {
    if !matches!(*state, InstallState::Idle | InstallState::Failed(_)) {
        return false;
    }
    *state = InstallState::Installing;
    true
}

fn completed_install(result: Result<ManagedLaunch>) -> InstallState {
    match result {
        Ok(launch) => InstallState::Ready(launch),
        Err(error) => InstallState::Failed(format!("{error:#}")),
    }
}

pub async fn wait_until_ready(spec: &'static Spec) -> Result<PathBuf> {
    start_background_install(spec);
    loop {
        if let Some(result) = ready_result(detect(spec)) {
            return result;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn ready_result(detection: Detection) -> Option<Result<PathBuf>> {
    if let Some(path) = detection.path {
        Some(Ok(path))
    } else {
        detection.error.map(|error| Err(anyhow::anyhow!(error)))
    }
}

async fn install_latest(spec: &Spec) -> Result<ManagedLaunch> {
    let registry = crate::registry::load().await?;
    let agent = find_registry_agent(spec, registry)?;
    install_agent(spec, agent).await
}

fn find_registry_agent(spec: &Spec, registry: crate::registry::Registry) -> Result<Agent> {
    registry
        .agents
        .into_iter()
        .find(|agent| agent.id == spec.registry_id)
        .with_context(|| format!("{} is absent from the ACP registry", spec.display_name))
}

async fn install_agent(spec: &Spec, agent: Agent) -> Result<ManagedLaunch> {
    let platform = crate::registry::current_platform();
    let target = binary_target_for(spec, &agent, &platform)?;
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let (command, args) =
        crate::install::install_or_resolve(spec.registry_id, &agent.version, &target, progress_tx)
            .await?;
    let launch = managed_launch(&agent, &target, command, args);
    write_manifest(spec, &launch)?;
    Ok(launch)
}

fn binary_target_for(spec: &Spec, agent: &Agent, platform: &str) -> Result<BinaryTarget> {
    agent
        .distribution
        .binary
        .as_ref()
        .and_then(|targets| targets.get(platform).cloned())
        .with_context(|| format!("no {} binary for {platform}", spec.display_name))
}

fn managed_launch(
    agent: &Agent,
    target: &BinaryTarget,
    command: PathBuf,
    args: Vec<String>,
) -> ManagedLaunch {
    ManagedLaunch {
        version: agent.version.clone(),
        command,
        args,
        env: target.env.clone(),
    }
}

fn manifest_path(spec: &Spec) -> PathBuf {
    manifest_path_at(spec, &crate::install::default_install_root())
}

fn manifest_path_at(spec: &Spec, install_root: &Path) -> PathBuf {
    install_root.join(spec.registry_id).join("current.json")
}

fn read_manifest(spec: &Spec) -> Option<ManagedLaunch> {
    read_manifest_from(&manifest_path(spec))
}

#[cfg(test)]
fn read_manifest_at(spec: &Spec, install_root: &Path) -> Option<ManagedLaunch> {
    read_manifest_from(&manifest_path_at(spec, install_root))
}

fn read_manifest_from(path: &Path) -> Option<ManagedLaunch> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_manifest(spec: &Spec, launch: &ManagedLaunch) -> Result<()> {
    write_manifest_to(spec, &manifest_path(spec), launch)
}

#[cfg(test)]
fn write_manifest_at(spec: &Spec, install_root: &Path, launch: &ManagedLaunch) -> Result<()> {
    write_manifest_to(spec, &manifest_path_at(spec, install_root), launch)
}

fn write_manifest_to(spec: &Spec, path: &Path, launch: &ManagedLaunch) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} manifest has no parent", spec.display_name))?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temp, launch)?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn valid_launch(spec: &Spec, launch: &ManagedLaunch) -> bool {
    valid_launch_at(spec, &crate::install::default_install_root(), launch)
}

fn valid_launch_at(spec: &Spec, install_root: &Path, launch: &ManagedLaunch) -> bool {
    let root = install_root.join(spec.registry_id);
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    let Ok(command) = std::fs::canonicalize(&launch.command) else {
        return false;
    };
    command.starts_with(root) && command.is_file()
}

#[cfg(test)]
mod tests {
    use crate::registry::{Distribution, Registry};

    use super::*;

    const TEST_SPEC: Spec = Spec {
        registry_id: "kimi",
        display_name: "Kimi Code",
        vendor: crate::auth::AuthVendor::Kimi,
    };

    fn agent_with_target(platform: &str) -> Agent {
        Agent {
            id: "kimi".to_string(),
            name: "Kimi Code".to_string(),
            version: "1.2.3".to_string(),
            description: "test agent".to_string(),
            distribution: Distribution {
                binary: Some(HashMap::from([(
                    platform.to_string(),
                    BinaryTarget {
                        archive: "https://example.com/kimi.zip".to_string(),
                        sha256: "abc123".to_string(),
                        cmd: "./kimi".to_string(),
                        args: vec!["acp".to_string()],
                        env: HashMap::from([("KIMI_TOKEN".to_string(), "secret".to_string())]),
                    },
                )])),
                ..Distribution::default()
            },
        }
    }

    fn launch(command: PathBuf) -> ManagedLaunch {
        ManagedLaunch {
            version: "1.2.3".to_string(),
            command,
            args: vec!["acp".to_string()],
            env: HashMap::from([("KIMI_TOKEN".to_string(), "secret".to_string())]),
        }
    }

    #[test]
    fn path_and_install_state_detections_preserve_launch_contracts() {
        let path = PathBuf::from("/tools/kimi");
        let on_path = detected_path(&TEST_SPEC, path.clone());
        assert_eq!(on_path.path, Some(path));
        assert_eq!(on_path.args, vec!["acp"]);
        assert!(on_path.env.is_empty());
        assert_eq!(on_path.evidence, "Kimi Code on PATH");
        assert!(!on_path.installing);
        assert!(on_path.error.is_none());

        let idle = detection_for_state(&TEST_SPEC, InstallState::Idle);
        assert!(idle.path.is_none());
        assert_eq!(idle.args, vec!["acp"]);
        assert_eq!(idle.evidence, "Kimi Code is not installed");
        assert!(!idle.installing);
        assert!(idle.error.is_none());

        let installing = detection_for_state(&TEST_SPEC, InstallState::Installing);
        assert!(installing.path.is_none());
        assert!(installing.installing);
        assert_eq!(installing.evidence, "installing managed Kimi Code");
        assert!(installing.error.is_none());

        let managed_launch = launch(PathBuf::from("/managed/kimi"));
        let ready = detection_for_state(&TEST_SPEC, InstallState::Ready(managed_launch.clone()));
        assert_eq!(ready.path, Some(managed_launch.command));
        assert_eq!(ready.args, managed_launch.args);
        assert_eq!(ready.env, managed_launch.env);
        assert_eq!(ready.evidence, "managed Kimi Code 1.2.3");
        assert!(!ready.installing);
        assert!(ready.error.is_none());

        let failed = detection_for_state(
            &TEST_SPEC,
            InstallState::Failed("network failed".to_string()),
        );
        assert!(failed.path.is_none());
        assert_eq!(failed.args, vec!["acp"]);
        assert_eq!(failed.evidence, "managed Kimi Code install failed");
        assert!(!failed.installing);
        assert_eq!(failed.error.as_deref(), Some("network failed"));
    }

    #[test]
    fn install_state_transitions_and_ready_results_are_explicit() {
        let mut idle = InstallState::Idle;
        assert!(begin_install(&mut idle));
        assert!(matches!(idle, InstallState::Installing));
        assert!(!begin_install(&mut idle));

        let mut failed = InstallState::Failed("old failure".to_string());
        assert!(begin_install(&mut failed));
        assert!(matches!(failed, InstallState::Installing));

        let ready_launch = launch(PathBuf::from("/managed/kimi"));
        let mut already_ready = InstallState::Ready(ready_launch.clone());
        assert!(!begin_install(&mut already_ready));
        assert!(matches!(already_ready, InstallState::Ready(_)));

        assert!(matches!(
            completed_install(Ok(ready_launch.clone())),
            InstallState::Ready(ready) if ready == ready_launch
        ));
        assert!(matches!(
            completed_install(Err(anyhow::anyhow!("download failed").context("install Kimi"))),
            InstallState::Failed(error) if error == "install Kimi: download failed"
        ));

        let ready = ready_result(detection_for_state(
            &TEST_SPEC,
            InstallState::Ready(ready_launch.clone()),
        ))
        .expect("ready result")
        .expect("ready path");
        assert_eq!(ready, ready_launch.command);

        let error = ready_result(detection_for_state(
            &TEST_SPEC,
            InstallState::Failed("boom".to_string()),
        ))
        .expect("failed result")
        .expect_err("failure");
        assert_eq!(error.to_string(), "boom");
        assert!(ready_result(detection_for_state(&TEST_SPEC, InstallState::Installing)).is_none());
    }

    #[test]
    fn registry_and_platform_selection_find_only_a_supported_binary() {
        let platform = crate::registry::current_platform();
        let mut other = agent_with_target(&platform);
        other.id = "other".to_string();
        let kimi = agent_with_target(&platform);

        let selected = find_registry_agent(
            &TEST_SPEC,
            Registry {
                agents: vec![other, kimi],
            },
        )
        .expect("Kimi registry entry");
        assert_eq!(selected.id, "kimi");
        assert_eq!(selected.version, "1.2.3");

        let target = binary_target_for(&TEST_SPEC, &selected, &platform).expect("platform target");
        assert_eq!(target.cmd, "./kimi");
        assert_eq!(target.args, vec!["acp"]);
        assert_eq!(
            target.env.get("KIMI_TOKEN").map(String::as_str),
            Some("secret")
        );

        let launch = managed_launch(
            &selected,
            &target,
            PathBuf::from("/installed/kimi"),
            target.args.clone(),
        );
        assert_eq!(launch.version, "1.2.3");
        assert_eq!(launch.args, vec!["acp"]);
        assert_eq!(launch.env, target.env);

        assert_eq!(
            binary_target_for(&TEST_SPEC, &selected, "unsupported-platform")
                .expect_err("unsupported platform")
                .to_string(),
            "no Kimi Code binary for unsupported-platform"
        );
        assert_eq!(
            find_registry_agent(&TEST_SPEC, Registry::default())
                .expect_err("missing Kimi")
                .to_string(),
            "Kimi Code is absent from the ACP registry"
        );
    }

    #[test]
    fn manifest_round_trip_and_validation_stay_inside_the_install_root() {
        let root = tempfile::tempdir().expect("install root");
        assert!(read_manifest_at(&TEST_SPEC, root.path()).is_none());
        assert_eq!(
            manifest_path_at(&TEST_SPEC, root.path()),
            root.path().join("kimi/current.json")
        );

        let command = root.path().join("kimi/1.2.3/bin/kimi");
        std::fs::create_dir_all(command.parent().expect("command parent"))
            .expect("create command directory");
        std::fs::write(&command, b"binary").expect("write command");
        let managed = launch(command.clone());

        write_manifest_at(&TEST_SPEC, root.path(), &managed).expect("write manifest");
        let restored = read_manifest_at(&TEST_SPEC, root.path()).expect("read manifest");
        assert_eq!(restored, managed);
        assert!(valid_launch_at(&TEST_SPEC, root.path(), &restored));

        let detection = detected_managed(&TEST_SPEC, restored);
        assert_eq!(detection.path.as_deref(), Some(command.as_path()));
        assert_eq!(detection.args, vec!["acp"]);
        assert_eq!(
            detection.env.get("KIMI_TOKEN").map(String::as_str),
            Some("secret")
        );
        assert_eq!(detection.evidence, "managed Kimi Code 1.2.3");

        let outside = tempfile::NamedTempFile::new().expect("outside command");
        assert!(!valid_launch_at(
            &TEST_SPEC,
            root.path(),
            &launch(outside.path().to_path_buf())
        ));
        assert!(!valid_launch_at(
            &TEST_SPEC,
            root.path(),
            &launch(root.path().join("kimi/missing"))
        ));
        assert!(!valid_launch_at(
            &TEST_SPEC,
            root.path(),
            &launch(root.path().join("kimi/1.2.3/bin"))
        ));
        assert!(!valid_launch_at(
            &TEST_SPEC,
            &root.path().join("missing-root"),
            &managed
        ));

        std::fs::write(manifest_path_at(&TEST_SPEC, root.path()), b"{not json")
            .expect("write malformed manifest");
        assert!(read_manifest_at(&TEST_SPEC, root.path()).is_none());
    }

    #[test]
    fn manifests_are_scoped_per_agent() {
        let root = tempfile::tempdir().expect("install root");
        const OTHER_SPEC: Spec = Spec {
            registry_id: "opencode",
            display_name: "OpenCode",
            vendor: crate::auth::AuthVendor::OpenCode,
        };
        assert_eq!(
            manifest_path_at(&OTHER_SPEC, root.path()),
            root.path().join("opencode/current.json")
        );

        let command = root.path().join("kimi/1.2.3/kimi");
        std::fs::create_dir_all(command.parent().expect("command parent"))
            .expect("create command directory");
        std::fs::write(&command, b"binary").expect("write command");
        let managed = launch(command);
        write_manifest_at(&TEST_SPEC, root.path(), &managed).expect("write manifest");

        assert!(read_manifest_at(&OTHER_SPEC, root.path()).is_none());
        // A manifest pointing into another agent's install root is rejected.
        assert!(!valid_launch_at(&OTHER_SPEC, root.path(), &managed));
    }

    #[test]
    fn manifest_deserialization_defaults_a_missing_environment() {
        let root = tempfile::tempdir().expect("install root");
        let path = manifest_path_at(&TEST_SPEC, root.path());
        std::fs::create_dir_all(path.parent().expect("manifest parent"))
            .expect("create manifest directory");
        let command = root.path().join("kimi/1.2.3/kimi");
        let document = serde_json::json!({
            "version": "1.2.3",
            "command": command,
            "args": ["acp"]
        });
        std::fs::write(
            &path,
            serde_json::to_vec(&document).expect("serialize manifest"),
        )
        .expect("write manifest");

        let restored = read_manifest_at(&TEST_SPEC, root.path()).expect("read manifest");
        assert_eq!(restored.version, "1.2.3");
        assert_eq!(restored.args, vec!["acp"]);
        assert!(restored.env.is_empty());
    }
}
