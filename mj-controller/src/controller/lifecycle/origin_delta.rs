use super::*;
use crate::controller::worktree::ExecutorGit;
use mj_core::worker_launch::{WorkerLaunchConfig, WorkerRunMode};
use mj_core::worker_suspend::{self as wire, SuspendReceipt, SuspendRequest};
use std::path::Path;

const MISSING_WIRE_EXIT_STATUS: i32 = 44;

/// One poll of the original worker during a stop.
enum WorkerPoll {
    Exited,
    Alive(Option<wire::SuspendProgress>),
}

pub(super) struct OriginDeltaSuspend {
    session_id: String,
    backend: targets::TargetLocator,
    root: String,
    checkout: PathBuf,
    child_sessions: Vec<String>,
    operation: String,
    request_recorded: bool,
    accepted: bool,
}

/// Shell function the batched wire reads probe through: print the file, or
/// exit [`MISSING_WIRE_EXIT_STATUS`] when it does not exist.
const READ_FILE_SH: &str =
    "read_file() { if [ -e \"$1\" ]; then exec cat -- \"$1\"; fi; exit 44; }\n";

/// Read `names` under `root` at the worker's target, one `Option` per name:
/// `None` when the file does not exist. Everything one decision needs is read
/// in one command (a local worker reads the files directly), so callers read
/// a fresh batch at each decision point and consume it at once; a batch kept
/// across a worker upgrade, a delivery readback, a wait, or a start would be
/// stale.
pub(super) fn read_wire_batch(
    backend: &targets::TargetLocator,
    session_id: &str,
    root: &str,
    names: &[&str],
    purpose: &str,
    executor: &(impl CommandExecutor + Sync),
) -> Result<Vec<Option<Vec<u8>>>> {
    if matches!(backend, targets::TargetLocator::LocalBare { .. }) {
        return names
            .iter()
            .map(|name| {
                let path = Path::new(root).join(name);
                match std::fs::read(&path) {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
                }
            })
            .collect();
    }
    let script = format!(
        "{}{READ_FILE_SH}root=$1; shift\nfor file in \"$@\"; do probe \"$file\" read_file \"$root/$file\"; done\nexit 0\n",
        targets::PROBE_HELPER_SH
    );
    let mut args = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        script,
        "mj-read-suspend-records".to_owned(),
        root.to_owned(),
    ];
    args.extend(names.iter().map(|name| (*name).to_owned()));
    let command = targets::command_on_locator(
        targets::SshTrafficClass::Control,
        backend,
        session_id,
        args,
        purpose,
    )?;
    let output = mj_core::runtime::off_async_worker(|| executor.execute(&command))?;
    ensure!(
        output.status == 0,
        "{purpose} failed (status {}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let mut probes = targets::parse_framed_probes(&output.stdout, targets::DEFAULT_PROBE_FRAMES);
    names
        .iter()
        .map(|name| wire_frame(&mut probes, name))
        .collect()
}

fn wire_frame(
    probes: &mut std::collections::BTreeMap<String, targets::CommandOutput>,
    name: &str,
) -> Result<Option<Vec<u8>>> {
    let frame = probes
        .remove(name)
        .with_context(|| format!("the suspend record probe returned no answer for {name}"))?;
    match frame.status {
        0 => Ok(Some(frame.stdout)),
        MISSING_WIRE_EXIT_STATUS => Ok(None),
        status => anyhow::bail!(
            "read suspend record {name} failed (status {status}): {}",
            String::from_utf8_lossy(&frame.stderr).trim()
        ),
    }
}

fn decode_wire<T: serde::de::DeserializeOwned>(
    root: &str,
    name: &str,
    bytes: Option<Vec<u8>>,
) -> Result<Option<T>> {
    bytes
        .map(|bytes| {
            serde_json::from_slice(&bytes).with_context(|| format!("decode {root}/{name}"))
        })
        .transpose()
}

pub(in crate::controller) fn read_wire<T: serde::de::DeserializeOwned>(
    backend: &targets::TargetLocator,
    session_id: &str,
    root: &str,
    name: &str,
    executor: &(impl CommandExecutor + Sync),
) -> Result<Option<T>> {
    let [bytes] = read_wire_batch(
        backend,
        session_id,
        root,
        &[name],
        &format!("read suspend record {name}"),
        executor,
    )?
    .try_into()
    .map_err(|_| anyhow::anyhow!("read suspend record {name}: unexpected answer count"))?;
    decode_wire(root, name, bytes)
}

fn write_wire<T: serde::Serialize>(
    backend: &targets::TargetLocator,
    session_id: &str,
    root: &str,
    name: &str,
    value: &T,
    executor: &(impl CommandExecutor + Sync),
) -> Result<()> {
    if matches!(backend, targets::TargetLocator::LocalBare { .. }) {
        return wire::write(Path::new(root), name, value);
    }
    let path = format!("{root}/{name}");
    let tmp = format!("{path}.tmp.$$");
    let script = format!(
        "path={}; tmp={}; mkdir -p -- \"$(dirname -- \"$path\")\" && cat > \"$tmp\" && mv -f -- \"$tmp\" \"$path\"",
        targets::posix_quote(&path),
        targets::posix_quote(&tmp),
    );
    let command = targets::command_on_locator(
        targets::SshTrafficClass::Control,
        backend,
        session_id,
        vec!["sh".into(), "-c".into(), script],
        format!("write suspend record {name}"),
    )?
    .with_sensitive_stdin(serde_json::to_vec(value)?);
    mj_core::runtime::off_async_worker(|| super::super::execute_checked(executor, command))?;
    Ok(())
}

fn remove_wire_files(
    backend: &targets::TargetLocator,
    session_id: &str,
    root: &str,
    names: &[&str],
    executor: &(impl CommandExecutor + Sync),
) -> Result<()> {
    if matches!(backend, targets::TargetLocator::LocalBare { .. }) {
        for name in names {
            match std::fs::remove_file(Path::new(root).join(name)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("remove suspend record {name}"));
                }
            }
        }
        return Ok(());
    }
    let mut args = vec!["rm".to_owned(), "-f".to_owned(), "--".to_owned()];
    for name in names {
        args.push(format!("{root}/{name}"));
    }
    let command = targets::command_on_locator(
        targets::SshTrafficClass::Control,
        backend,
        session_id,
        args,
        "clear worker suspend records",
    )?;
    mj_core::runtime::off_async_worker(|| super::super::execute_checked(executor, command))?;
    Ok(())
}

fn read_launch_config(
    backend: &targets::TargetLocator,
    session_id: &str,
    root: &str,
    executor: &(impl CommandExecutor + Sync),
) -> Result<WorkerLaunchConfig> {
    if matches!(backend, targets::TargetLocator::LocalBare { .. }) {
        return WorkerLaunchConfig::read(&Path::new(root).join("launch.json"));
    }
    read_wire::<WorkerLaunchConfig>(backend, session_id, root, "launch.json", executor)?
        .with_context(|| format!("read {root}/launch.json: file not found"))
}

fn worker_identity_is_alive(
    backend: &targets::TargetLocator,
    session_id: &str,
    root: &str,
    worker: wire::WorkerIdentity,
    executor: &(impl CommandExecutor + Sync),
    allow_unsupported: bool,
) -> Result<bool> {
    if matches!(backend, targets::TargetLocator::LocalBare { .. }) {
        return mj_core::runtime::off_async_worker(|| worker.is_alive());
    }
    let command = targets::command_on_locator(
        targets::SshTrafficClass::Control,
        backend,
        session_id,
        vec![
            format!("{root}/hel"),
            "worker".into(),
            "identity-alive".into(),
            "--pid".into(),
            worker.pid.to_string(),
            "--start".into(),
            worker.start.to_string(),
        ],
        "check suspend worker liveness",
    )?;
    let output = mj_core::runtime::off_async_worker(|| executor.execute(&command))?;
    if output.status != 0 {
        if allow_unsupported {
            return Ok(false);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "check suspend worker liveness failed (status {}): {}",
            output.status,
            stderr.trim()
        );
    }
    match output.stdout.as_slice() {
        b"alive\n" => Ok(true),
        b"dead\n" => Ok(false),
        other => anyhow::bail!(
            "unexpected worker identity-alive output: {:?}",
            String::from_utf8_lossy(other)
        ),
    }
}

fn checkout_has_origin_remote(
    git: &impl mj_checkpoint::archive::GitCommandRunner,
    repository: &Path,
) -> Result<bool> {
    let origin = git.run(
        repository,
        &mj_checkpoint::archive::GitCommand {
            arguments: ["config", "--get", "remote.origin.url"]
                .map(Into::into)
                .into(),
            ..Default::default()
        },
    )?;
    Ok(origin.status == 0 && !origin.stdout.trim_ascii().is_empty())
}

pub(in crate::controller) fn managed_origin_override(
    worktree: &mj_core::state::ManagedWorktree,
) -> Option<String> {
    if !matches!(
        worktree.target,
        mj_core::state::ManagedWorktreeTarget::Local
    ) {
        return None;
    }
    mj_core::runtime::off_async_worker(|| {
        use mj_checkpoint::archive::GitCommandRunner;
        let origin = mj_checkpoint::archive::SystemGit
            .run(
                &worktree.worktree_root,
                &mj_checkpoint::archive::GitCommand {
                    arguments: ["config", "--get", "remote.origin.url"]
                        .map(Into::into)
                        .into(),
                    ..Default::default()
                },
            )
            .ok()?;
        if origin.status == 0 && !origin.stdout.trim_ascii().is_empty() {
            return None;
        }
        if !worktree.source_repository.exists() {
            return None;
        }
        let git_dir = mj_checkpoint::archive::SystemGit
            .run(
                &worktree.source_repository,
                &mj_checkpoint::archive::GitCommand {
                    arguments: ["rev-parse", "--git-dir"].map(Into::into).into(),
                    ..Default::default()
                },
            )
            .ok()?;
        if git_dir.status == 0 {
            Some(worktree.source_repository.to_string_lossy().into_owned())
        } else {
            None
        }
    })
}

const ALL_SUSPEND_WIRE_FILES: &[&str] = &[
    wire::ACCEPTED,
    wire::REQUEST,
    wire::RECEIPT,
    wire::PROGRESS,
    wire::RESTART,
];

/// Clear any OriginDelta suspend request, receipt, and progress wire files left
/// in `backend`'s worker root for `session_id` so a subsequent Resume and later
/// Suspend cannot mistake an abandoned Move's records for a fresh quiescence.
pub(in crate::controller) fn clear_suspend_records_at_target(
    backend: &targets::TargetLocator,
    session_id: &str,
    executor: &(impl CommandExecutor + Sync),
) -> Result<()> {
    let root = targets::worker_root(backend, session_id)?;
    remove_wire_files(backend, session_id, &root, ALL_SUSPEND_WIRE_FILES, executor)
}

/// The checkouts whose `origin` remote decides whether a new suspend captures
/// OriginDelta.
enum OriginPlan<'a> {
    NotApplicable,
    Worktree {
        worktree: &'a mj_core::state::ManagedWorktree,
        repositories: [PathBuf; 1],
    },
    Workspace {
        repositories: Vec<PathBuf>,
    },
}

impl<'a> OriginPlan<'a> {
    fn for_session(
        controller: &'a Controller,
        session_id: &str,
        backend: &targets::TargetLocator,
    ) -> Result<Self> {
        match controller.state.checkout(session_id)? {
            mj_core::state::Checkout::ManagedWorktree { worktree, .. } => {
                if !worktree.uses_origin_delta_suspend() {
                    return Ok(Self::NotApplicable);
                }
                Ok(Self::Worktree {
                    worktree,
                    repositories: [worktree.worktree_root.clone()],
                })
            }
            mj_core::state::Checkout::ManagedWorkspace => {
                let session = controller
                    .state
                    .sessions
                    .get(session_id)
                    .with_context(|| format!("unknown session {session_id}"))?;
                let Some(bundle) = session.project_bundle(&controller.config) else {
                    return Ok(Self::NotApplicable);
                };
                if bundle.repositories.is_empty() {
                    return Ok(Self::NotApplicable);
                }
                let workspace_root = PathBuf::from(crate::controller::network_git::workspace_root(
                    backend,
                    session.container_workspace.as_deref(),
                    &session.id,
                ));
                Ok(Self::Workspace {
                    repositories: bundle
                        .repositories
                        .iter()
                        .map(|repository| workspace_root.join(&repository.destination))
                        .collect(),
                })
            }
            mj_core::state::Checkout::Attached { .. }
            | mj_core::state::Checkout::Borrowed { .. } => Ok(Self::NotApplicable),
        }
    }

    fn repositories(&self) -> &[PathBuf] {
        match self {
            Self::NotApplicable => &[],
            Self::Worktree { repositories, .. } => repositories,
            Self::Workspace { repositories } => repositories,
        }
    }

    /// `origins` answers, in order, whether each repository has an `origin`
    /// remote; it may stop after the first repository that has none.
    fn decide(&self, origins: &[bool]) -> Result<bool> {
        match self {
            Self::NotApplicable => Ok(false),
            Self::Worktree { worktree, .. } => {
                Ok(origins.first() == Some(&true) || managed_origin_override(worktree).is_some())
            }
            Self::Workspace { repositories } => {
                Ok(origins.len() == repositories.len() && origins.iter().all(|origin| *origin))
            }
        }
    }
}

/// Read whether a suspend was accepted and, if not, which `repositories` have
/// an `origin` remote, in one command. `None` means it was accepted.
fn read_accepted_and_origins(
    backend: &targets::TargetLocator,
    session_id: &str,
    root: &str,
    repositories: &[PathBuf],
    executor: &(impl CommandExecutor + Sync),
) -> Result<Option<Vec<bool>>> {
    const PURPOSE: &str = "read suspend acceptance and checkout origins";
    let script = format!(
        "{}{READ_FILE_SH}root=$1; shift\nprobe accepted read_file \"$root/{}\" && exit 0\ni=0\nfor repository in \"$@\"; do probe \"origin$i\" git -C \"$repository\" config --get remote.origin.url; i=$((i + 1)); done\nexit 0\n",
        targets::PROBE_HELPER_SH,
        wire::ACCEPTED
    );
    let mut args = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        script,
        "mj-read-suspend-origins".to_owned(),
        root.to_owned(),
    ];
    args.extend(
        repositories
            .iter()
            .map(|repository| repository.to_string_lossy().into_owned()),
    );
    let command = targets::command_on_locator(
        targets::SshTrafficClass::Control,
        backend,
        session_id,
        args,
        PURPOSE,
    )?;
    let output = mj_core::runtime::off_async_worker(|| executor.execute(&command))?;
    ensure!(
        output.status == 0,
        "{PURPOSE} failed (status {}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let mut probes = targets::parse_framed_probes(&output.stdout, targets::DEFAULT_PROBE_FRAMES);
    if wire_frame(&mut probes, "accepted")?.is_some() {
        return Ok(None);
    }
    (0..repositories.len())
        .map(|index| {
            let origin = probes.remove(&format!("origin{index}")).with_context(|| {
                format!("the origin probe returned no answer for repository {index}")
            })?;
            Ok(origin.status == 0 && !origin.stdout.trim_ascii().is_empty())
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

impl OriginDeltaSuspend {
    /// Whether this suspend captures OriginDelta. An accepted suspend keeps
    /// the capture it started with, even after its worker has exited. A new
    /// one applies to any managed checkout (local bare, SSH bare, or container
    /// workspace) with an `origin` remote or local source repository to delta
    /// against; if the running worker predates `suspend-worker.json`, suspend
    /// upgrades it in place instead of falling back to version-2 capture.
    pub(super) fn applies(
        controller: &Controller,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<bool> {
        let (backend, root) = controller.worker_placement(session_id)?;
        // The checkout lookup can fail; an accepted suspend still applies.
        let plan = OriginPlan::for_session(controller, session_id, &backend);
        if matches!(backend, targets::TargetLocator::LocalBare { .. }) {
            if read_wire::<String>(&backend, session_id, &root, wire::ACCEPTED, executor)?.is_some()
            {
                return Ok(true);
            }
            let plan = plan?;
            let git = ExecutorGit::for_session(executor, &backend, session_id);
            let mut origins = Vec::new();
            for repository in plan.repositories() {
                let has_origin = mj_core::runtime::off_async_worker(|| {
                    checkout_has_origin_remote(&git, repository)
                })?;
                origins.push(has_origin);
                if !has_origin {
                    break;
                }
            }
            return plan.decide(&origins);
        }
        let repositories = plan
            .as_ref()
            .map(|plan| plan.repositories().to_vec())
            .unwrap_or_default();
        let Some(origins) =
            read_accepted_and_origins(&backend, session_id, &root, &repositories, executor)?
        else {
            return Ok(true);
        };
        plan?.decide(&origins)
    }

    /// Whether the session's live worker advertises origin-delta quiescence
    /// capability (`suspend-worker.json` with a live PID in the target
    /// namespace).
    pub(super) fn worker_capable(&self, executor: &(impl CommandExecutor + Sync)) -> Result<bool> {
        match read_wire::<wire::WorkerIdentity>(
            &self.backend,
            &self.session_id,
            &self.root,
            wire::CAPABILITY,
            executor,
        )? {
            Some(worker) => worker_identity_is_alive(
                &self.backend,
                &self.session_id,
                &self.root,
                worker,
                executor,
                true,
            ),
            None => Ok(false),
        }
    }

    pub(super) fn load(
        controller: &Controller,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<Self> {
        let session = controller
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let checkout = controller.state.checkout(session_id)?;
        let (backend, root) = controller.worker_placement(session_id)?;
        let checkout_path = match checkout {
            mj_core::state::Checkout::ManagedWorktree { worktree, .. } => {
                worktree.worktree_root.clone()
            }
            mj_core::state::Checkout::ManagedWorkspace => {
                PathBuf::from(crate::controller::network_git::workspace_root(
                    &backend,
                    session.container_workspace.as_deref(),
                    &session.id,
                ))
            }
            mj_core::state::Checkout::Attached { path } => path.to_path_buf(),
            _ => bail!("managed checkout is missing"),
        };
        let [accepted, request] = read_wire_batch(
            &backend,
            session_id,
            &root,
            &[wire::ACCEPTED, wire::REQUEST],
            "read suspend acceptance and request",
            executor,
        )?
        .try_into()
        .map_err(|_| anyhow::anyhow!("suspend record batch returned an unexpected count"))?;
        let accepted = decode_wire::<String>(&root, wire::ACCEPTED, accepted)?;
        let request = decode_wire::<SuspendRequest>(&root, wire::REQUEST, request)?
            .filter(|request| accepted.as_ref() == Some(&request.operation));
        let request_recorded = request.is_some();
        let child_sessions = if let Some(request) = request {
            ensure!(
                request.checkout == checkout_path,
                "accepted suspend request belongs to a different checkout"
            );
            // A stopped borrower can still have detached writers. Keep the
            // scope of the original stop after its worker has exited.
            request.child_sessions
        } else {
            crate::database::environment_quiescence_session_ids(session_id)?
        };
        Ok(Self {
            session_id: session_id.to_owned(),
            backend,
            root,
            checkout: checkout_path,
            child_sessions,
            request_recorded,
            accepted: accepted.is_some(),
            operation: match accepted {
                Some(operation) => operation,
                None => new_command_id("origin-delta-suspend")?,
            },
        })
    }

    pub(super) fn accepted(&self) -> bool {
        self.accepted
    }

    pub(super) fn already_quiescent(&self) -> bool {
        self.operation.starts_with("parked-origin-delta-suspend-")
    }

    pub(super) fn accept(
        &mut self,
        executor: &(impl CommandExecutor + Sync),
        parked: bool,
    ) -> Result<()> {
        self.operation = new_command_id(if parked {
            "parked-origin-delta-suspend"
        } else {
            "origin-delta-suspend"
        })?;
        write_wire(
            &self.backend,
            &self.session_id,
            &self.root,
            wire::ACCEPTED,
            &self.operation,
            executor,
        )?;
        self.request_recorded = false;
        self.accepted = true;
        Ok(())
    }

    pub(super) fn request_recorded(&self) -> bool {
        self.request_recorded
    }

    pub(super) fn capture_scope(&self) -> wire::SuspendCapture {
        wire::SuspendCapture {
            operation: self.operation.clone(),
            checkout: self.checkout.clone(),
            child_sessions: self.child_sessions.clone(),
        }
    }

    pub(super) fn restart_reason(
        &self,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<Option<String>> {
        Ok(read_wire::<SuspendReceipt>(
            &self.backend,
            &self.session_id,
            &self.root,
            wire::RESTART,
            executor,
        )?
        .filter(|receipt| receipt.operation == self.operation)
        .and_then(|receipt| receipt.error))
    }

    /// The restart reason (when `want_restart`) and this operation's receipt,
    /// read together at the one decision that needs both.
    pub(super) fn restart_and_receipt(
        &self,
        executor: &(impl CommandExecutor + Sync),
        want_restart: bool,
    ) -> Result<(Option<String>, Option<SuspendReceipt>)> {
        let names: &[&str] = if want_restart {
            &[wire::RESTART, wire::RECEIPT]
        } else {
            &[wire::RECEIPT]
        };
        let mut records = read_wire_batch(
            &self.backend,
            &self.session_id,
            &self.root,
            names,
            "read suspend restart and receipt",
            executor,
        )?
        .into_iter();
        let restart = if want_restart {
            decode_wire::<SuspendReceipt>(&self.root, wire::RESTART, records.next().flatten())?
                .filter(|receipt| receipt.operation == self.operation)
                .and_then(|receipt| receipt.error)
        } else {
            None
        };
        let receipt =
            decode_wire::<SuspendReceipt>(&self.root, wire::RECEIPT, records.next().flatten())?
                .filter(|receipt| receipt.operation == self.operation);
        Ok((restart, receipt))
    }

    pub(super) fn record_restart(
        &self,
        error: &anyhow::Error,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        write_wire(
            &self.backend,
            &self.session_id,
            &self.root,
            wire::RESTART,
            &SuspendReceipt {
                operation: self.operation.clone(),
                error: Some(format!("{error:#}")),
            },
            executor,
        )
    }

    pub(super) fn clear_after_restart(
        &self,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        remove_wire_files(
            &self.backend,
            &self.session_id,
            &self.root,
            ALL_SUSPEND_WIRE_FILES,
            executor,
        )
    }

    pub(super) fn receipt(
        &self,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<Option<SuspendReceipt>> {
        Ok(read_wire::<SuspendReceipt>(
            &self.backend,
            &self.session_id,
            &self.root,
            wire::RECEIPT,
            executor,
        )?
        .filter(|receipt| receipt.operation == self.operation))
    }

    pub(super) fn request_stop(&mut self, executor: &(impl CommandExecutor + Sync)) -> Result<()> {
        if self.receipt(executor)?.is_some() {
            return Ok(());
        }
        let mut args = vec![
            format!("{}/hel", self.root),
            "worker".to_owned(),
            "quiesce".to_owned(),
            "--root".to_owned(),
            self.root.clone(),
            "--operation".to_owned(),
            self.operation.clone(),
            "--checkout".to_owned(),
            self.checkout.to_string_lossy().into_owned(),
        ];
        for id in &self.child_sessions {
            args.extend(["--child-session".to_owned(), id.clone()]);
        }
        let command = targets::command_on_locator(
            targets::SshTrafficClass::Control,
            &self.backend,
            &self.session_id,
            args,
            "request managed checkout process quiescence",
        )?;
        if let Err(error) =
            mj_core::runtime::off_async_worker(|| super::super::execute_checked(executor, command))
        {
            // Delivery can fail after the worker's durable request is written.
            match read_wire::<SuspendRequest>(
                &self.backend,
                &self.session_id,
                &self.root,
                wire::REQUEST,
                executor,
            ) {
                Ok(Some(request)) if request.operation == self.operation => {}
                Ok(_) => return Err(error),
                Err(readback) => {
                    return Err(error
                        .context(format!(
                            "could not confirm delivery of the stop request: {readback:#}"
                        ))
                        .context(LifecycleFailureReason::SuspendPending));
                }
            }
        }
        self.request_recorded = true;
        Ok(())
    }

    /// The accepted request and the restart record, read together.
    fn request_and_restart(
        &self,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<(Option<SuspendRequest>, bool)> {
        let [request, restart] = read_wire_batch(
            &self.backend,
            &self.session_id,
            &self.root,
            &[wire::REQUEST, wire::RESTART],
            "read suspend request and restart",
            executor,
        )?
        .try_into()
        .map_err(|_| anyhow::anyhow!("suspend record batch returned an unexpected count"))?;
        let restart = decode_wire::<SuspendReceipt>(&self.root, wire::RESTART, restart)?
            .filter(|receipt| receipt.operation == self.operation)
            .and_then(|receipt| receipt.error)
            .is_some();
        Ok((
            decode_wire::<SuspendRequest>(&self.root, wire::REQUEST, request)?,
            restart,
        ))
    }

    /// Wait for the original worker to exit, or for the next progress report.
    /// A target in another namespace answers from one bounded remote loop
    /// instead of one round trip per poll.
    fn poll_worker(
        &self,
        worker: wire::WorkerIdentity,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<WorkerPoll> {
        if matches!(self.backend, targets::TargetLocator::LocalBare { .. }) {
            if !worker_identity_is_alive(
                &self.backend,
                &self.session_id,
                &self.root,
                worker,
                executor,
                false,
            )? {
                return Ok(WorkerPoll::Exited);
            }
            return Ok(WorkerPoll::Alive(read_wire::<wire::SuspendProgress>(
                &self.backend,
                &self.session_id,
                &self.root,
                wire::PROGRESS,
                executor,
            )?));
        }
        const PURPOSE: &str = "wait for suspend worker exit";
        // Each pass polls at the worker's old 100 ms cadence, so one command
        // holds the connection about two seconds. A worker that is still alive
        // then reports its progress and the caller asks again.
        let script = format!(
            "{}{READ_FILE_SH}root=$1; pid=$2; start=$3\nn=0\nwhile [ \"$n\" -lt 20 ]; do\n    probe alive \"$root/hel\" worker identity-alive --pid \"$pid\" --start \"$start\" || exit 0\n    [ \"$out\" = alive ] || exit 0\n    n=$((n + 1))\n    sleep 0.1 2>/dev/null || sleep 1\ndone\nprobe progress read_file \"$root/{}\"\nexit 0\n",
            targets::PROBE_HELPER_SH,
            wire::PROGRESS
        );
        let command = targets::command_on_locator(
            targets::SshTrafficClass::Control,
            &self.backend,
            &self.session_id,
            vec![
                "sh".into(),
                "-c".into(),
                script,
                "mj-wait-suspend-worker".into(),
                self.root.clone(),
                worker.pid.to_string(),
                worker.start.to_string(),
            ],
            PURPOSE,
        )?;
        let output = mj_core::runtime::off_async_worker(|| executor.execute(&command))?;
        ensure!(
            output.status == 0,
            "{PURPOSE} failed (status {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let mut probes =
            targets::parse_framed_probes(&output.stdout, targets::DEFAULT_PROBE_FRAMES);
        let alive = probes
            .remove("alive")
            .context("the suspend worker probe returned no liveness answer")?;
        if alive.status != 0 {
            anyhow::bail!(
                "check suspend worker liveness failed (status {}): {}",
                alive.status,
                String::from_utf8_lossy(&alive.stderr).trim()
            );
        }
        match alive.stdout.as_slice() {
            b"dead" => return Ok(WorkerPoll::Exited),
            b"alive" => {}
            other => anyhow::bail!(
                "unexpected worker identity-alive output: {:?}",
                String::from_utf8_lossy(other)
            ),
        }
        let progress = wire_frame(&mut probes, "progress")?;
        Ok(WorkerPoll::Alive(decode_wire(
            &self.root,
            wire::PROGRESS,
            progress,
        )?))
    }

    /// Wait for the original worker to finish the accepted stop. After a
    /// person cancels, the wait still has to read and write the stop's
    /// records, so it runs on the shutdown-only view: only daemon shutdown
    /// interrupts those commands.
    pub(super) async fn wait_for_stop(
        &self,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        let cleanup = executor.shutdown_only();
        executor.begin_resumable_work()?;
        let outcome = async {
            let initial_cancelled =
                executor.cancellation_requested() && !executor.shutdown_interrupted();
            let (request, restart_recorded) = if initial_cancelled {
                self.request_and_restart(&cleanup)?
            } else {
                self.request_and_restart(executor)?
            };
            let worker = request
                .context("worker did not record the suspend request")?
                .worker;
            let mut cancellation_recorded = restart_recorded;
            let mut displayed_notice = String::new();
            loop {
                ensure!(
                    !executor.shutdown_interrupted(),
                    "suspend process wait was handed off"
                );
                if executor.cancellation_requested() && !cancellation_recorded {
                    self.record_restart(&anyhow::anyhow!("suspend was cancelled"), &cleanup)?;
                    cancellation_recorded = true;
                }
                let after_cancellation = cancellation_recorded || executor.cancellation_requested();
                // A receipt can precede the worker's last cleanup tasks.
                let polled = if after_cancellation {
                    self.poll_worker(worker, &cleanup)
                } else {
                    self.poll_worker(worker, executor)
                };
                let progress = match polled {
                    Ok(WorkerPoll::Exited) => return Ok(()),
                    Ok(WorkerPoll::Alive(progress)) => progress,
                    // A cancellation or handoff kills the in-flight wait. Go
                    // back to the top to record the restart or report the
                    // handoff; the next poll no longer uses this executor.
                    Err(_)
                        if !after_cancellation
                            && (executor.shutdown_interrupted()
                                || executor.cancellation_requested()) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let mut notice = if cancellation_recorded {
                    "Restarting after the previous agent exits".to_owned()
                } else {
                    "Waiting for the session's processes to stop".to_owned()
                };
                if let Some(progress) = progress
                    && progress.operation == self.operation
                    && !progress.remaining_pids.is_empty()
                {
                    let pids = progress
                        .remaining_pids
                        .iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>();
                    notice.push_str(&format!(" (PIDs {})", pids.join(", ")));
                }
                if displayed_notice != notice {
                    executor.notify_notice(&notice);
                    displayed_notice = notice;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        .await;
        executor
            .end_resumable_work("record suspend quiescence outcome")
            .and(outcome)
            .context(LifecycleFailureReason::SuspendPending)
    }

    pub(super) fn live_worker_mode(
        &self,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<Option<WorkerRunMode>> {
        let existing = mj_core::runtime::off_async_worker(|| {
            super::super::worker_binary::probe_worker_liveness(
                executor,
                &self.backend,
                &self.session_id,
                &self.root,
            )
        })?;
        ensure!(
            existing.status == 0,
            "cannot verify the retained worker's liveness: {}",
            String::from_utf8_lossy(&existing.stderr).trim()
        );
        match existing.stdout.as_slice() {
            b"dead\n" => Ok(None),
            b"alive\n" | b"starting\n" => {
                let mode =
                    read_launch_config(&self.backend, &self.session_id, &self.root, executor)
                        .map(|launch| Some(launch.run_mode))?;
                Ok(mode)
            }
            _ => bail!("unexpected retained worker liveness response"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn start_worker<'a>(
        &'a self,
        controller: &'a Controller,
        admission: &'a crate::recovery_gate::SessionAdmission,
        session_id: &'a str,
        executor: &'a (impl CommandExecutor + Sync),
        manager: Option<&'a SessionManagerControl>,
        run_mode: WorkerRunMode,
        reattach_only: bool,
    ) -> std::pin::Pin<
        Box<impl Future<Output = Result<super::super::checkpoint::ControllerRelayLease>> + 'a>,
    > {
        use super::super::checkpoint::adopt_restarted_checkpoint_relay;
        use super::super::worker_restart::{InstalledWorkerRestart, RESTART_FOR_CHECKPOINT};

        // Suspend can start a checkpoint worker and restart the harness on
        // rollback. Keep these futures out of its stack frame so it has room
        // to decode relay replies during checkpoint capture.
        Box::pin(async move {
            let reconnect = targets::reconnect_plan(&self.backend, session_id)?
                .commands
                .into_iter()
                .next()
                .context("reconnect plan is missing")?;
            let mut launch = controller.current_worker_launch_config(session_id, &self.backend)?;
            launch.run_mode = run_mode;
            launch.suspend_capture =
                (run_mode == WorkerRunMode::CheckpointOnly).then(|| self.capture_scope());
            let worker_root = self.root.as_str();
            let mut restart = InstalledWorkerRestart {
                backend: &self.backend,
                worker_root,
                reconnect: &reconnect,
                launch: Some(&launch),
                prepared: false,
                messages: &RESTART_FOR_CHECKPOINT,
                staged_aws_profile: None,
                swap_admission: None,
                discard_queued_prompts: None,
            };
            // Quiescence proved the original worker exited. If a worker has since
            // booted in the requested mode, observe it through the existing launch
            // owner, including when the daemon exited before recording readiness.
            let connection = if self.live_worker_mode(executor)? == Some(run_mode) {
                let pending = crate::database::load_worker_restart(session_id)?;
                executor.begin_resumable_work()?;
                let result = controller
                    .observe_started_worker(session_id, executor, restart)
                    .await;
                executor.end_resumable_work("reattach to the retained suspend worker")?;
                let connection = result?;
                if let Some(pending) = pending {
                    crate::database::finish_worker_restart(session_id, &pending.operation_id)?;
                }
                connection
            } else {
                ensure!(
                    !reattach_only,
                    "the retained suspend worker exited before reattachment"
                );
                const LABEL: &str = "start managed clone suspend worker";
                // A draining handoff refuses the swap. Step aside instead of
                // failing the accepted suspend: either the handoff closes the
                // gate, which interrupts this lifecycle for shutdown and the
                // next daemon resumes it from its durable intent, or the drain
                // lapses and the swap is admitted.
                restart.swap_admission = Some(loop {
                    match crate::upgrade::activity_unless_draining(LABEL) {
                        Ok(work) => break work,
                        Err(_) if crate::upgrade::is_draining() => {
                            executor.begin_resumable_work()?;
                            executor.end_resumable_work(LABEL)?;
                        }
                        Err(error) => return Err(error),
                    }
                });
                let result = controller
                    .restart_worker_with_installed_binary(session_id, executor, restart)
                    .await;
                // The restart releases admission only once the launch succeeds.
                // Release it here too (idempotent) so a failed launch never
                // waits for a draining handoff that is waiting for this hold.
                executor.begin_resumable_work()?;
                executor.end_resumable_work("record managed clone suspend worker readiness")?;
                result?
            };
            adopt_restarted_checkpoint_relay(admission, session_id, manager, connection).await
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use mj_core::targets::{CommandOutput, CommandSpec, SshTarget};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    const SESSION: &str = "0123456789abcdef0123456789abcdef";
    const OPERATION: &str = "origin-delta-suspend-test";

    /// A target's shell: runs the remote command line an SSH command carries in
    /// a local `sh`, as the SSH daemon would, and records each command's
    /// purpose.
    #[derive(Default)]
    struct RemoteShell {
        purposes: Mutex<Vec<String>>,
        notices: Mutex<Vec<String>>,
        /// Reported as the operation's cancellation.
        cancelled: AtomicBool,
        /// A cancellation kills the first wait, as it kills its SSH process.
        cancel_during_first_wait: bool,
        after_wait: Option<Box<dyn Fn() + Send + Sync>>,
    }

    impl RemoteShell {
        fn count(&self, purpose: &str) -> usize {
            self.purposes
                .lock()
                .unwrap()
                .iter()
                .filter(|seen| seen.as_str() == purpose)
                .count()
        }
    }

    impl CommandExecutor for RemoteShell {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.purposes.lock().unwrap().push(command.purpose.clone());
            if self.cancelled.load(Ordering::SeqCst) && !command.is_shutdown_only() {
                anyhow::bail!("a cancelled operation must run on the shutdown-only view");
            }
            let waiting = command.purpose == "wait for suspend worker exit";
            if waiting
                && self.cancel_during_first_wait
                && !self.cancelled.swap(true, Ordering::SeqCst)
            {
                anyhow::bail!("ssh was killed by the cancellation");
            }
            let mut local = CommandSpec::new("sh", ["-c", command.args.last().unwrap().as_str()]);
            if let Some(input) = command.sensitive_stdin() {
                local = local.with_sensitive_stdin(input.to_vec());
            }
            let output = ProcessExecutor.execute(&local)?;
            if waiting && let Some(hook) = &self.after_wait {
                hook();
            }
            Ok(output)
        }

        fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &CommandSpec,
            _input: &mut (dyn std::io::Read + Send),
        ) -> Result<CommandOutput> {
            self.execute(command)
        }

        fn cancellation_requested(&self) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn notify_notice(&self, notice: &str) {
            self.notices.lock().unwrap().push(notice.to_owned());
        }

        fn pause_deadline(&self) -> targets::DeadlinePause {
            targets::DeadlinePause::none()
        }
    }

    fn backend() -> targets::TargetLocator {
        targets::TargetLocator::SshBare {
            ssh: SshTarget {
                destination: "user@remote.example".into(),
                ssh_args: Vec::new(),
            },
            workspace: "/remote/clone".into(),
            worker_id: Some(SESSION.into()),
        }
    }

    /// A worker root with spaces in its path, holding a `hel` that reports the
    /// worker alive until it has been asked `dead_after` times.
    fn worker_root(dead_after: u32) -> (tempfile::TempDir, String) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("worker root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("dead_after"), dead_after.to_string()).unwrap();
        mj_core::test_hooks::install_fake_command(
            &root,
            "hel",
            r#"#!/bin/sh
root=$(dirname "$0")
[ "$1 $2" = "worker identity-alive" ] || { echo "unrecognized subcommand '$1'" >&2; exit 2; }
[ ! -e "$root/unsupported" ] || { echo "error: unrecognized subcommand 'identity-alive'" >&2; exit 2; }
asked=$(( $(cat "$root/asked" 2>/dev/null || echo 0) + 1 ))
echo "$asked" > "$root/asked"
if [ "$asked" -ge "$(cat "$root/dead_after")" ]; then echo dead; else echo alive; fi
"#,
        );
        let root = root.to_string_lossy().into_owned();
        (directory, root)
    }

    fn attempt(root: &str) -> OriginDeltaSuspend {
        OriginDeltaSuspend {
            session_id: SESSION.into(),
            backend: backend(),
            root: root.to_owned(),
            checkout: PathBuf::from("/remote/clone"),
            child_sessions: Vec::new(),
            operation: OPERATION.into(),
            request_recorded: true,
            accepted: true,
        }
    }

    fn record_request(root: &str) {
        std::fs::write(
            Path::new(root).join(wire::REQUEST),
            serde_json::to_vec(&SuspendRequest {
                operation: OPERATION.into(),
                worker: wire::WorkerIdentity {
                    pid: 4242,
                    start: 99,
                },
                checkout: PathBuf::from("/remote/clone"),
                child_sessions: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();
    }

    fn asked(root: &str) -> u32 {
        std::fs::read_to_string(Path::new(root).join("asked"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    #[test]
    fn a_batch_of_wire_reads_tells_present_missing_and_failing_files_apart_in_one_command() {
        let (_directory, root) = worker_root(1);
        std::fs::write(Path::new(&root).join(wire::ACCEPTED), "\"op\"\n").unwrap();
        // A file that exists but cannot be read is an error, not a missing file.
        std::fs::create_dir(Path::new(&root).join(wire::PROGRESS)).unwrap();
        let shell = RemoteShell::default();

        let read = |names: &[&str]| {
            read_wire_batch(
                &backend(),
                SESSION,
                &root,
                names,
                "read test records",
                &shell,
            )
        };
        let answers = read(&[wire::REQUEST, wire::ACCEPTED]).unwrap();
        assert_eq!(answers, [None, Some(b"\"op\"".to_vec())]);
        assert_eq!(shell.count("read test records"), 1);

        let error = format!("{:#}", read(&[wire::ACCEPTED, wire::PROGRESS]).unwrap_err());
        assert!(
            error.contains("read suspend record suspend-progress.json failed (status 1)"),
            "{error}"
        );
        assert_eq!(shell.count("read test records"), 2);
    }

    #[test]
    fn the_acceptance_and_origin_checks_share_one_command_and_stop_at_an_accepted_suspend() {
        let (_directory, root) = worker_root(1);
        let git = |repository: &Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(repository)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        let repositories: Vec<PathBuf> = ["with origin", "without origin"]
            .into_iter()
            .map(|name| {
                let path = Path::new(&root).join(name);
                std::fs::create_dir_all(&path).unwrap();
                git(&path, &["init", "--quiet"]);
                path
            })
            .collect();
        git(
            &repositories[0],
            &["remote", "add", "origin", "https://example.com/app.git"],
        );
        let shell = RemoteShell::default();

        let origins =
            read_accepted_and_origins(&backend(), SESSION, &root, &repositories, &shell).unwrap();
        assert_eq!(origins, Some(vec![true, false]));
        assert_eq!(shell.purposes.lock().unwrap().len(), 1);

        std::fs::write(Path::new(&root).join(wire::ACCEPTED), "\"op\"").unwrap();
        let origins =
            read_accepted_and_origins(&backend(), SESSION, &root, &repositories, &shell).unwrap();
        assert_eq!(origins, None, "an accepted suspend applies without origins");
        assert_eq!(shell.purposes.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn one_bounded_remote_wait_covers_many_liveness_checks_until_the_worker_exits() {
        let (_directory, root) = worker_root(4);
        record_request(&root);
        let shell = RemoteShell::default();
        attempt(&root).wait_for_stop(&shell).await.unwrap();
        // Four checks of the worker, one command on the connection.
        assert_eq!(asked(&root), 4);
        assert_eq!(shell.count("wait for suspend worker exit"), 1);
        assert_eq!(
            shell.purposes.lock().unwrap().len(),
            2,
            "request read, wait"
        );
    }

    #[tokio::test]
    async fn a_worker_still_alive_after_the_bounded_wait_reports_progress_and_is_awaited_again() {
        let (_directory, root) = worker_root(1_000);
        record_request(&root);
        std::fs::write(
            Path::new(&root).join(wire::PROGRESS),
            serde_json::to_vec(&wire::SuspendProgress {
                operation: OPERATION.into(),
                remaining_pids: vec![9002, 9003],
            })
            .unwrap(),
        )
        .unwrap();
        let release = root.clone();
        let shell = RemoteShell {
            // The worker exits while the controller is between waits.
            after_wait: Some(Box::new(move || {
                std::fs::write(Path::new(&release).join("dead_after"), "1").unwrap()
            })),
            ..Default::default()
        };
        attempt(&root).wait_for_stop(&shell).await.unwrap();
        assert_eq!(shell.count("wait for suspend worker exit"), 2);
        assert!(asked(&root) > 20, "the first wait polled until its bound");
        assert_eq!(
            *shell.notices.lock().unwrap(),
            ["Waiting for the session's processes to stop (PIDs 9002, 9003)"]
        );
    }

    #[tokio::test]
    async fn a_cancellation_that_kills_the_remote_wait_is_recorded_and_the_wait_continues_uncancelled()
     {
        let (_directory, root) = worker_root(2);
        record_request(&root);
        let cancellable = RemoteShell {
            cancel_during_first_wait: true,
            ..Default::default()
        };
        attempt(&root).wait_for_stop(&cancellable).await.unwrap();
        // The killed wait, then the wait that continues on the shutdown-only
        // view (the shell refuses any other command once cancelled).
        assert_eq!(cancellable.count("wait for suspend worker exit"), 2);
        let restart: SuspendReceipt =
            serde_json::from_slice(&std::fs::read(Path::new(&root).join(wire::RESTART)).unwrap())
                .unwrap();
        assert_eq!(restart.operation, OPERATION);
        assert_eq!(restart.error.as_deref(), Some("suspend was cancelled"));
    }

    #[tokio::test]
    async fn a_worker_without_identity_alive_fails_the_wait_with_its_own_refusal() {
        let (_directory, root) = worker_root(1);
        record_request(&root);
        std::fs::write(Path::new(&root).join("unsupported"), "").unwrap();
        let shell = RemoteShell::default();
        let error = attempt(&root).wait_for_stop(&shell).await.unwrap_err();
        let error = format!("{error:#}");
        assert!(
            error.contains("check suspend worker liveness failed (status 2)")
                && error.contains("unrecognized subcommand 'identity-alive'"),
            "{error}"
        );
        assert_eq!(shell.count("wait for suspend worker exit"), 1);
    }
}
