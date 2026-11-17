//! Reproducible, cancellable image builds owned by one daemon incarnation.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail, ensure};
use futures::FutureExt;
use mj_client::session_image::{ImageFollow, ImageRequest, ImageResult, ImageStarted, ImageStatus};
use mj_core::config::{Config, TargetTemplate};
use mj_core::session_image::{ImageBase, MiseArtifact, containerfile, image_tag};
use mj_core::targets::{CommandExecutor, CommandSpec, ImageHost};
use tokio::sync::Notify;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

#[derive(Default)]
/// An operation keeps its whole log and result until the daemon exits, so
/// every follower, including one repeating a request whose response was
/// lost, can read both. Image builds are rare user-started operations.
struct OperationState {
    log: Vec<u8>,
    result: Option<std::result::Result<ImageResult, String>>,
}

struct Operation {
    state: Mutex<OperationState>,
    changed: Notify,
    cancel: CancellationToken,
}

impl Operation {
    fn log(&self, bytes: &[u8]) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .log
            .extend_from_slice(bytes);
        self.changed.notify_waiters();
    }
}

#[derive(Clone, Default)]
pub(crate) struct ImageBuilds {
    operations: Arc<Mutex<BTreeMap<String, Arc<Operation>>>>,
    tasks: TaskTracker,
}

impl ImageBuilds {
    pub(crate) fn start(
        &self,
        request: ImageRequest,
        status: bool,
        shutdown: &CancellationToken,
    ) -> Result<ImageStarted> {
        let _accepted = crate::upgrade::activity_unless_draining("image build start")?;
        ensure!(!shutdown.is_cancelled(), "daemon is shutting down");
        let operation_id = mj_core::state::new_session_id()?;
        let operation = Arc::new(Operation {
            state: Mutex::new(OperationState::default()),
            changed: Notify::new(),
            cancel: shutdown.child_token(),
        });
        self.operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(operation_id.clone(), operation.clone());
        self.tasks.spawn(async move {
            let result = std::panic::AssertUnwindSafe(run(request, status, operation.clone()))
                .catch_unwind()
                .await;
            let result = match result {
                Ok(result) => result.map_err(|error| format!("{error:#}")),
                Err(_) => Err("image build task panicked; rerun mj image build".to_owned()),
            };
            operation
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .result = Some(result);
            operation.changed.notify_waiters();
        });
        Ok(ImageStarted { operation_id })
    }

    pub(crate) async fn follow(&self, id: &str, offset: usize) -> Result<Option<ImageFollow>> {
        let Some(operation) = self
            .operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
        else {
            return Ok(None);
        };
        loop {
            let changed = operation.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = operation
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                ensure!(
                    offset <= state.log.len(),
                    "log offset exceeds the operation's log"
                );
                let start = offset;
                let mut end = start;
                for chunk in state.log[start..].utf8_chunks() {
                    end += chunk.valid().len();
                    if state.result.is_none()
                        && !chunk.invalid().is_empty()
                        && std::str::from_utf8(chunk.invalid())
                            .is_err_and(|error| error.error_len().is_none())
                    {
                        break;
                    }
                    end += chunk.invalid().len();
                }
                if end > start || state.result.is_some() {
                    let follow = ImageFollow {
                        log: String::from_utf8_lossy(&state.log[start..end]).into_owned(),
                        next_offset: end,
                        result: state.result.clone(),
                    };
                    return Ok(Some(follow));
                }
            }
            changed.await;
        }
    }

    pub(crate) fn cancel(&self, id: &str) -> bool {
        let operations = self
            .operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(operation) = operations.get(id) else {
            return false;
        };
        operation.cancel.cancel();
        true
    }

    pub(crate) async fn finish(&self) {
        for operation in self
            .operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            operation.cancel.cancel();
        }
        self.tasks.close();
        self.tasks.wait().await;
    }
}

struct Selection {
    config: Config,
    bundle: String,
    target: Option<String>,
    name: String,
}

fn select(request: &ImageRequest, status: bool) -> Result<Selection> {
    let controller = crate::controller::Controller::load()?;
    let config = controller.config;
    let session = request
        .session_id
        .as_deref()
        .and_then(|id| controller.state.sessions.get(id));
    let bundle = request
        .bundle
        .clone()
        .or_else(|| session.map(|s| s.bundle_id.clone()))
        .or_else(|| {
            config
                .bundles
                .iter()
                .find(|(_, bundle)| {
                    bundle.repositories.iter().any(|repo| {
                        repo.local.as_deref().is_some_and(|local| {
                            local == request.path
                                || local.canonicalize().is_ok_and(|root| root == request.path)
                        })
                    })
                })
                .map(|(id, _)| id.clone())
        })
        .context("cannot find this repository's bundle; use --bundle ID")?;
    ensure!(
        config.bundles.contains_key(&bundle),
        "unknown bundle {bundle:?}"
    );
    let preferences = mj_core::go::GoPreferences::load(&mj_core::go::GoPreferences::path())?;
    let target = request
        .target
        .clone()
        .or_else(|| {
            session
                .filter(|s| {
                    config
                        .targets
                        .get(&s.target_template_id)
                        .is_some_and(|target| target.container().is_some())
                })
                .map(|s| s.target_template_id.clone())
        })
        .or_else(|| {
            preferences
                .recipe(&request.path)
                .filter(|recipe| {
                    config
                        .targets
                        .get(&recipe.target_id)
                        .is_some_and(|target| target.container().is_some())
                })
                .map(|recipe| recipe.target_id)
        });
    ensure!(
        status || target.is_some(),
        "choose a container target with --target ID or save a default target"
    );
    if let Some(target) = &target {
        ensure!(
            config
                .targets
                .get(target)
                .is_some_and(|target| target.container().is_some()),
            "target {target:?} is not a configured container target"
        );
    }
    let name = request.name.clone().unwrap_or_else(|| bundle.clone());
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)),
        "image name must contain lowercase letters, digits, dots, underscores, or hyphens"
    );
    Ok(Selection {
        config,
        bundle,
        target,
        name,
    })
}

/// Only the manifest owner supplies production artifacts. This fixture is a
/// temporary integration seam until Milestone 1 is merged by the parent.
fn mise_artifact(platform: &str) -> Result<MiseArtifact> {
    let arch = match platform {
        "linux/amd64" => "x86_64",
        "linux/arm64" => "aarch64",
        _ => bail!("no pinned mise artifact for {platform}"),
    };
    mj_core::session_image::pinned_mise(arch)
}

async fn platform(config: &Config, target: &str, cancel: &CancellationToken) -> Result<String> {
    let template = config
        .targets
        .get(target)
        .context("unknown image target")?
        .clone();
    let executor = mj_core::targets::CancellableProcessExecutor::new(Default::default())
        .with_cancellation(cancel.clone());
    tokio::task::spawn_blocking(move || {
        crate::controller::backend::session_image_platform(&template, &executor)
    })
    .await
    .context("join image platform probe")?
}

async fn builder(
    config: &Config,
    target: &str,
    platform: &str,
    operation: &Operation,
) -> Result<String> {
    if !matches!(config.targets[target], TargetTemplate::SshNitro { .. }) {
        return Ok(target.to_owned());
    }
    let mut considered = Vec::new();
    let mut candidates: Vec<_> = config
        .targets
        .iter()
        .filter(|(id, target)| {
            !config.disabled_targets.contains(*id)
                && target.container().is_some()
                && !matches!(target, TargetTemplate::SshNitro { .. })
        })
        .collect();
    candidates.sort_by_key(|(_, target)| {
        matches!(
            target,
            TargetTemplate::SshPodman { .. }
                | TargetTemplate::SshDocker { .. }
                | TargetTemplate::SshAppleContainer { .. }
        )
    });
    for (id, _) in candidates {
        match self::platform(config, id, &operation.cancel).await {
            Ok(candidate) => {
                considered.push(format!("{id} ({candidate})"));
                if candidate == platform {
                    operation.log(format!("Building on {id}: Nitro target {target} consumes {platform} images but has no build engine.\n").as_bytes());
                    return Ok(id.clone());
                }
            }
            Err(error) => considered.push(format!("{id} ({error:#})")),
        }
    }
    bail!(
        "Nitro image build needs a configured Podman, Docker, or Apple container target for {platform}; considered: {}",
        if considered.is_empty() {
            "none".into()
        } else {
            considered.join(", ")
        }
    )
}

async fn run(
    request: ImageRequest,
    status: bool,
    operation: Arc<Operation>,
) -> Result<ImageResult> {
    let selecting = request.clone();
    let selection = tokio::task::spawn_blocking(move || select(&selecting, status))
        .await
        .context("join image build selection")??;
    if status {
        return status_images(&selection, &request, operation).await;
    }
    if !request.print {
        ensure!(
            selection
                .config
                .images
                .repository
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()),
            "set [images] repository in config.toml, for example:\n[images]\nrepository = \"ghcr.io/your-account/projects\""
        );
    }
    let target = selection
        .target
        .as_deref()
        .context("image build target is missing")?;
    let platform = platform(&selection.config, target, &operation.cancel).await?;
    let builder_id = builder(&selection.config, target, &platform, &operation).await?;
    let text = prepare(
        &selection.config,
        target,
        &builder_id,
        &platform,
        &request.files,
        operation.clone(),
    )
    .await?;
    if request.print {
        return Ok(ImageResult {
            containerfile: Some(text),
            ..Default::default()
        });
    }
    let repository = selection.config.images.repository.as_deref().filter(|value| !value.trim().is_empty()).context("set [images] repository in config.toml, for example:\n[images]\nrepository = \"ghcr.io/your-account/projects\"")?;
    let reference = format!(
        "{}/{name}:{tag}",
        repository.trim_end_matches('/'),
        name = selection.name,
        tag = image_tag(&text)
    );
    let (host, _) = selection.config.targets[&builder_id]
        .image_host()
        .context("builder has no container engine")?;
    let apple = matches!(
        host,
        ImageHost::AppleContainer | ImageHost::SshAppleContainer(_)
    );
    let mut pull = vec![host.engine().to_owned()];
    if apple {
        pull.push("image".into());
    }
    pull.extend(["pull".into(), reference.clone()]);
    let check = host.command(pull, "check published image tag".into());
    let existing = engine_command(&host, check, Vec::new(), operation.clone()).await;
    let skipped = existing.as_ref().is_ok_and(|output| output.status == 0);
    if !skipped {
        match existing {
            Ok(output) => operation.log(&output.stderr),
            Err(error) => {
                operation.log(format!("Registry pull did not complete: {error:#}\n").as_bytes())
            }
        }
        ensure!(!operation.cancel.is_cancelled(), "image build cancelled");
        operation.log(format!("Building {reference}\n").as_bytes());
        let mut args = vec![
            host.engine().into(),
            "build".into(),
            "--platform".into(),
            platform.clone(),
            "--tag".into(),
            reference.clone(),
        ];
        let context = if apple {
            let command = host.command(
                vec!["mktemp".into(), "-d".into()],
                "create empty image build context".into(),
            );
            let output = engine_command(&host, command, Vec::new(), operation.clone()).await?;
            ensure!(
                output.status == 0,
                "cannot create empty image build context: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Some(String::from_utf8(output.stdout)?.trim().to_owned())
        } else {
            None
        };
        if host.engine() == "docker" {
            // Docker interprets a plain Containerfile on stdin as a build
            // with no context. Podman uses --file - for the same contract.
            args.push("-".into());
        } else {
            args.extend(["--file".into(), "-".into()]);
            if let Some(context) = &context {
                args.push(context.clone());
            }
        }
        let build = host.command(args, "build project image from stdin".into());
        let built = engine_command(&host, build, text.into_bytes(), operation.clone()).await;
        if let Some(context) = &context {
            if built.is_ok() {
                let cleanup = host.command(
                    vec!["rmdir".into(), context.clone()],
                    "remove completed empty image context".into(),
                );
                let output = engine_command(&host, cleanup, Vec::new(), operation.clone()).await?;
                ensure!(
                    output.status == 0,
                    "cannot remove completed empty image context: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            } else if matches!(host, ImageHost::AppleContainer) {
                // The local command was reaped before returning its error.
                tokio::fs::remove_dir(context)
                    .await
                    .context("remove cancelled empty image context")?;
            }
            // A cancelled SSH build may still be using its empty context.
        }
        let built = built?;
        let failure = String::from_utf8_lossy(&built.stderr).to_ascii_lowercase();
        if built.status != 0
            && (failure.contains("checksum")
                || failure.contains("stdin")
                || failure.contains("--file"))
            && (failure.contains("unknown")
                || failure.contains("unsupported")
                || failure.contains("not support")
                || failure.contains("unrecognized"))
        {
            let version = host.command(
                vec![host.engine().into(), "--version".into()],
                "read incompatible image engine version".into(),
            );
            let version = engine_command(&host, version, Vec::new(), operation.clone()).await?;
            bail!(
                "{}: {}. Image builds require ADD --checksum and a Containerfile on stdin with an empty context. {}",
                host.label(),
                String::from_utf8_lossy(&version.stdout).trim(),
                engine_error(&host, repository, &built)
            );
        }
        ensure!(
            built.status == 0,
            "{}",
            engine_error(&host, repository, &built)
        );
        ensure!(!operation.cancel.is_cancelled(), "image build cancelled");
        let push = host.command(
            if apple {
                vec![
                    host.engine().into(),
                    "image".into(),
                    "push".into(),
                    reference.clone(),
                ]
            } else {
                vec![host.engine().into(), "push".into(), reference.clone()]
            },
            "push project image".into(),
        );
        let pushed = engine_command(&host, push, Vec::new(), operation.clone()).await?;
        ensure!(
            pushed.status == 0,
            "{}",
            engine_error(&host, repository, &pushed)
        );
    } else {
        operation.log(
            format!("Image {reference} already in the registry; pulled; skipping build.\n")
                .as_bytes(),
        );
    }
    ensure!(!operation.cancel.is_cancelled(), "image build cancelled");
    let image = reference.clone();
    tokio::task::spawn_blocking(move || {
        let _assignment = crate::upgrade::activity_unless_draining("image assignment")?;
        Config::update(|config| {
            ensure!(
                config.bundles.contains_key(&selection.bundle),
                "bundle was removed during image build"
            );
            config
                .images
                .bundles
                .entry(selection.bundle)
                .or_default()
                .insert(platform, image);
            Ok(())
        })
        .map(|_| ())
    })
    .await
    .context("join project image assignment")??;
    Ok(ImageResult {
        image: Some(reference),
        skipped,
        ..Default::default()
    })
}

fn engine_error(
    host: &ImageHost,
    repository: &str,
    output: &mj_core::targets::CommandOutput,
) -> String {
    let first = repository.split('/').next().unwrap_or(repository);
    let registry = if first.contains('.') || first.contains(':') || first == "localhost" {
        first
    } else {
        "docker.io"
    };
    let mut words = vec![host.engine().to_owned()];
    if matches!(
        host,
        ImageHost::AppleContainer | ImageHost::SshAppleContainer(_)
    ) {
        words.push("registry".into());
    }
    words.extend(["login".into(), registry.into()]);
    let login = match host {
        ImageHost::SshPodman(ssh)
        | ImageHost::SshDocker(ssh)
        | ImageHost::SshAppleContainer(ssh)
        | ImageHost::SshNitro(ssh) => {
            let mut command = vec!["ssh".into()];
            command.extend(ssh.ssh_args.iter().cloned());
            command.push(ssh.destination.clone());
            command.push(mj_core::targets::join_remote_command(&words));
            mj_core::targets::join_remote_command(&command)
        }
        _ => mj_core::targets::join_remote_command(&words),
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let failure = format!("{} exited {}: {stderr}", host.label(), output.status);
    let lower = stderr.to_ascii_lowercase();
    if [
        "unauthorized",
        "authentication required",
        "authentication failed",
        "access denied",
        "authorization failed",
        "no basic auth credentials",
        "denied:",
        "forbidden",
    ]
    .iter()
    .any(|message| lower.contains(message))
    {
        format!("{failure}\nSign in on the engine host with: {login}")
    } else {
        failure
    }
}

async fn prepare(
    config: &Config,
    target: &str,
    builder: &str,
    platform: &str,
    files: &BTreeMap<String, String>,
    operation: Arc<Operation>,
) -> Result<String> {
    let (_, source) = config.targets[target]
        .image_host()
        .context("image target is not a container")?;
    let (host, _) = config.targets[builder]
        .image_host()
        .context("image builder is not a container")?;
    let base = source.image.clone();
    let host_for_pull = host.clone();
    let base_for_pull = base.clone();
    let platform_for_pull = platform.to_owned();
    let executor = mj_core::targets::CancellableProcessExecutor::new(Default::default())
        .with_cancellation(operation.cancel.clone());
    tokio::task::spawn_blocking(move || {
        let lock = crate::image_pull_gate::image_pull_mutex(&host_for_pull, &base_for_pull);
        let _guard = crate::image_pull_gate::hold_image_pull(
            &lock,
            || executor.cancellation_requested(),
            || {},
        )?;
        let refresh = mj_core::targets::image_refresh(
            host_for_pull,
            &base_for_pull,
            Some(&platform_for_pull),
            mj_core::config::ImagePullPolicy::Missing,
        )
        .context("base image refresh missing")?;
        if executor.execute(&refresh.image_id)?.status != 0 {
            let output = executor.execute(&refresh.pull)?;
            ensure!(
                output.status == 0,
                "{}",
                engine_error(&refresh.host, &base_for_pull, &output)
            );
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("join base image pull")??;
    let inspect = host.command(
        vec![
            host.engine().into(),
            "image".into(),
            "inspect".into(),
            base.clone(),
        ],
        "inspect base image digest and defaults".into(),
    );
    let output = engine_command(&host, inspect, Vec::new(), operation).await?;
    ensure!(
        output.status == 0,
        "{}",
        engine_error(&host, &base, &output)
    );
    let values: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("decode engine image metadata")?;
    let metadata = values
        .as_array()
        .and_then(|values| values.first())
        .unwrap_or(&values);
    let apple = matches!(
        host,
        ImageHost::AppleContainer | ImageHost::SshAppleContainer(_)
    );
    let (digest, image_config) = if apple {
        let architecture = platform
            .strip_prefix("linux/")
            .context("unsupported image platform")?;
        let variant = metadata["variants"]
            .as_array()
            .and_then(|variants| {
                variants.iter().find(|variant| {
                    variant["platform"]["os"] == "linux"
                        && variant["platform"]["architecture"] == architecture
                })
            })
            .context("Apple image metadata has no requested platform variant")?;
        (
            variant["digest"]
                .as_str()
                .context("Apple image variant has no manifest digest")?
                .to_owned(),
            &variant["config"]["config"],
        )
    } else {
        let digest = metadata["Digest"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                metadata["RepoDigests"]
                    .as_array()?
                    .first()?
                    .as_str()?
                    .split_once('@')
                    .map(|(_, digest)| digest.to_owned())
            })
            .context("engine image metadata did not report a registry digest")?;
        (digest, &metadata["Config"])
    };
    containerfile(
        &ImageBase {
            reference: format!("{}@{digest}", base.split('@').next().unwrap_or(&base)),
            user: image_config["User"].as_str().unwrap_or_default().into(),
            workdir: image_config["WorkingDir"]
                .as_str()
                .unwrap_or_default()
                .into(),
        },
        platform,
        &mise_artifact(platform)?,
        files,
    )
}

async fn status_images(
    selection: &Selection,
    request: &ImageRequest,
    operation: Arc<Operation>,
) -> Result<ImageResult> {
    let selected = if let Some(target) = selection.target.as_deref() {
        match platform(&selection.config, target, &operation.cancel).await {
            Ok(platform) => Some((target, platform)),
            Err(error) => {
                operation.log(
                    format!("Image freshness for target {target} is unknown: {error:#}\n")
                        .as_bytes(),
                );
                None
            }
        }
    } else {
        None
    };
    let mut status = Vec::new();
    for (platform, image) in selection
        .config
        .images
        .bundles
        .get(&selection.bundle)
        .into_iter()
        .flatten()
    {
        let mut current = None;
        if let Some((target, selected_platform)) = &selected
            && selected_platform == platform
        {
            let builder = builder(&selection.config, target, platform, &operation).await?;
            match prepare(
                &selection.config,
                target,
                &builder,
                platform,
                &request.files,
                operation.clone(),
            )
            .await
            {
                Ok(text) => {
                    current = Some(
                        image
                            .rsplit_once(':')
                            .is_some_and(|(_, tag)| tag == image_tag(&text)),
                    )
                }
                Err(error) => operation.log(
                    format!("Image freshness for target {target} is unknown: {error:#}\n")
                        .as_bytes(),
                ),
            }
        }
        status.push(ImageStatus {
            platform: platform.clone(),
            image: image.clone(),
            current,
        });
    }
    Ok(ImageResult {
        status,
        ..Default::default()
    })
}

async fn engine_command(
    host: &ImageHost,
    command: CommandSpec,
    input: Vec<u8>,
    operation: Arc<Operation>,
) -> Result<mj_core::targets::CommandOutput> {
    ensure!(!operation.cancel.is_cancelled(), "image build cancelled");
    let executor = mj_core::targets::CancellableProcessExecutor::new(Default::default())
        .with_cancellation(operation.cancel.clone());
    let _cancel_on_drop = executor.cancel_on_drop();
    let stream = command.purpose.starts_with("build ") || command.purpose.starts_with("push ");
    let observer: mj_core::targets::CommandOutputObserver = Arc::new(move |bytes| {
        if stream {
            operation.log(bytes);
        }
    });
    let description = format!("{}: {}", host.label(), command.purpose);
    tokio::task::spawn_blocking(move || {
        executor.execute_with_stdin_observed(
            &command,
            &mut std::io::Cursor::new(input),
            Some(observer),
        )
    })
    .await
    .context("join supervised image engine command")?
    .with_context(|| description)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    // Hard-won: #1493: borrowed stdin kept Podman waiting for EOF.
    #[tokio::test]
    async fn image_engine_input_closes_at_eof_while_both_pipes_exceed_capacity() {
        let operation = Arc::new(Operation {
            state: Mutex::new(OperationState::default()),
            changed: Notify::new(),
            cancel: CancellationToken::new(),
        });
        let command = CommandSpec::new(
            "sh",
            ["-c", "head -c 131072 /dev/zero; cat; printf done >&2"],
        )
        .purpose("build EOF regression");
        let bytes = vec![b'x'; 131072];
        // The observed missing-EOF deadlock must fail the test rather than
        // leave its engine process waiting forever.
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            engine_command(&ImageHost::LocalPodman, command, bytes.clone(), operation),
        )
        .await
        .expect("engine waited for stdin EOF")
        .unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(output.stdout, [vec![0; 131072], bytes].concat());
        assert_eq!(output.stderr, b"done");
    }

    #[tokio::test]
    async fn image_log_offsets_preserve_utf8_split_across_engine_reads() {
        let builds = ImageBuilds::default();
        let operation = Arc::new(Operation {
            state: Mutex::new(OperationState::default()),
            changed: Notify::new(),
            cancel: CancellationToken::new(),
        });
        builds
            .operations
            .lock()
            .unwrap()
            .insert("unicode".into(), operation.clone());
        let message = "tool 🦀 ready\n".as_bytes();
        operation.log(&message[..7]);
        let first = builds.follow("unicode", 0).await.unwrap().unwrap();
        assert_eq!(first.log, "tool ");
        let mut follow = Box::pin(builds.follow("unicode", first.next_offset));
        assert!(futures::poll!(&mut follow).is_pending());
        operation.log(&message[7..]);
        let rest = follow.await.unwrap().unwrap();
        assert_eq!(
            format!("{}{}", first.log, rest.log),
            std::str::from_utf8(message).unwrap()
        );
        assert_eq!(rest.next_offset, message.len());
        operation.state.lock().unwrap().result = Some(Err("done".into()));
        let last = builds
            .follow("unicode", rest.next_offset)
            .await
            .unwrap()
            .unwrap();
        assert!(last.result.is_some());
        // Hard-won: review of aca53d0: releasing a finished operation lost its result and log for a repeated follow.
        let repeated = builds.follow("unicode", 0).await.unwrap().unwrap();
        assert_eq!(repeated.log, std::str::from_utf8(message).unwrap());
        assert!(repeated.result.is_some());
    }
}
