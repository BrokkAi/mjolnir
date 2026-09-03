//! Orphan-worker discovery, adoption, and destruction.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::hel_session_manager::StandaloneSession;
use hel::hel_config::{AwsAddressSource, SshConnection, TargetTemplate};
use hel::hel_state::{SessionRecord, SessionState, TargetLocator, normalize_session_title};
use hel::hel_targets::{self, CommandExecutor, CommandOutput, CommandSpec, SshTarget};
use hel::hel_worker_launch::WorkerOwnership;

use super::backend::{ContainerOverrides, backend_locator, backend_target};
use super::readiness::wait_for_native_session;
use super::{Controller, backend_ssh, now, ssh_args_with_identity};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecoveryCandidate {
    pub session_id: String,
    pub target_template_id: String,
    pub locator: TargetLocator,
    pub ownership: Option<WorkerOwnership>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RecoveryScan {
    pub candidates: Vec<RecoveryCandidate>,
    pub warnings: Vec<String>,
}

impl Controller {
    /// Find managed resources which are not represented by the controller's
    /// current state. Labels/tags establish Hel ownership; the worker marker
    /// supplies profile and bundle metadata when it is available.
    pub fn scan_orphan_workers(&self, executor: &impl CommandExecutor) -> RecoveryScan {
        let mut scan = RecoveryScan::default();
        for (target_id, template) in &self.config.targets {
            match scan_target_workers(target_id, template, executor) {
                Ok(candidates) => {
                    scan.candidates
                        .extend(candidates.into_iter().filter(|candidate| {
                            !self.state.sessions.contains_key(&candidate.session_id)
                        }))
                }
                Err(error) => scan.warnings.push(format!("target {target_id}: {error:#}")),
            }
        }
        scan.candidates.sort_by(|left, right| {
            (&left.session_id, &left.target_template_id)
                .cmp(&(&right.session_id, &right.target_template_id))
        });
        scan.candidates.dedup_by(|left, right| {
            left.session_id == right.session_id
                && left.target_template_id == right.target_template_id
        });
        scan
    }

    pub async fn adopt_orphan_worker(
        &mut self,
        session_id: &str,
        target_id: &str,
        profile_override: Option<&str>,
        bundle_override: Option<&str>,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        let (record, newly_adopted) = match self.state.sessions.get(session_id).cloned() {
            // Adoption records its session before the handshake, so a failed
            // handshake leaves a tracked session that never connected. That
            // record is the one to finish, not a reason to refuse the retry.
            Some(existing) if adoption_unfinished(&existing, target_id) => {
                for (flag, requested, adopted) in [
                    ("profile", profile_override, existing.last_profile.as_str()),
                    ("bundle", bundle_override, existing.bundle_id.as_str()),
                ] {
                    if let Some(requested) = requested
                        && requested != adopted
                    {
                        bail!(
                            "session {session_id} was already adopted with {flag} {adopted:?}; retry without --{flag}"
                        );
                    }
                }
                (existing, false)
            }
            Some(_) => bail!("session {session_id} is already tracked"),
            None => {
                let candidate = self
                    .scan_orphan_workers(executor)
                    .candidates
                    .into_iter()
                    .find(|candidate| {
                        candidate.session_id == session_id
                            && candidate.target_template_id == target_id
                    })
                    .with_context(|| {
                        format!("no managed orphan {session_id} was found on target {target_id:?}")
                    })?;
                let profile_id = profile_override
                    .map(str::to_owned)
                    .or_else(|| {
                        candidate
                            .ownership
                            .as_ref()
                            .map(|marker| marker.profile_id.clone())
                    })
                    .context("orphan has no ownership marker; pass --profile")?;
                let bundle_id = bundle_override
                    .map(str::to_owned)
                    .or_else(|| {
                        candidate
                            .ownership
                            .as_ref()
                            .map(|marker| marker.bundle_id.clone())
                    })
                    .context("orphan has no ownership marker; pass --bundle")?;
                let profile = self
                    .config
                    .profiles
                    .get(&profile_id)
                    .with_context(|| format!("unknown profile {profile_id:?}"))?;
                self.config
                    .bundles
                    .get(&bundle_id)
                    .with_context(|| format!("unknown bundle {bundle_id:?}"))?;
                let record = adopted_session_record(
                    session_id,
                    target_id,
                    profile_id,
                    profile.kind,
                    bundle_id,
                    candidate
                        .ownership
                        .as_ref()
                        .map(|ownership| ownership.workspace_id.clone())
                        .unwrap_or_else(|| hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned()),
                    candidate.locator,
                );
                (record, true)
            }
        };
        let locator = record
            .target
            .as_ref()
            .context("adopted session has no target locator")?;
        let backend = backend_locator(locator, &record, &self.config)?;
        let spec = hel_targets::reconnect_plan(&backend, session_id)?
            .commands
            .into_iter()
            .next()
            .context("reconnect plan is empty")?;
        if newly_adopted {
            // Adoption authors the whole record for a session Hel has never
            // tracked, so it writes the whole row, and it writes it before the
            // handshake: a crash in between must not orphan the worker again.
            // The record reaches memory only once it is durable.
            hel::hel_database::save_session(&record)?;
            self.state.sessions.insert(session_id.to_owned(), record);
        }
        match self.complete_adoption(session_id, &spec, executor).await {
            Ok(()) => Ok(()),
            // Provisioning leaves its failure on the session it failed for.
            // Adoption owes the same: the record it already committed is all
            // the user has to see why the worker never connected.
            Err(error) => Err(self.record_adoption_failure(session_id, error)),
        }
    }

    /// Connect the adopted worker's relay and promote the session to running.
    async fn complete_adoption(
        &mut self,
        session_id: &str,
        spec: &CommandSpec,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        let mut relay = StandaloneSession::connect_command(spec, session_id)
            .await
            .context("orphan relay did not complete the v1 handshake")?;
        let native_session_id = wait_for_native_session(&mut relay, executor).await?;
        self.mark_worker_connected(session_id, Some(native_session_id))?;
        if let Some(title) = relay
            .snapshot()
            .materialized
            .session_title
            .as_deref()
            .and_then(normalize_session_title)
        {
            hel::hel_database::set_session_acp_title(session_id, Some(&title))?;
            self.state
                .sessions
                .get_mut(session_id)
                .expect("adopted session disappeared while saving its ACP title")
                .acp_session_title = Some(title);
        }
        Ok(())
    }

    /// Leave a failed adoption on the session itself. The state stays
    /// `Disconnected`, which is the truth — the target exists and no worker is
    /// connected — and keeps the record adoptable so the handshake can be
    /// retried once the worker is reachable again.
    fn record_adoption_failure(&mut self, session_id: &str, error: anyhow::Error) -> anyhow::Error {
        let Some(record) = self.state.sessions.get_mut(session_id) else {
            return error;
        };
        record.updated_at = now();
        record.last_error = Some(format!("orphan adoption failed: {error:#}"));
        match self.persist_session_state(session_id) {
            Ok(()) => error,
            Err(persist_error) => error.context(format!(
                "recorded the adoption failure in memory, but failed to persist it: {persist_error:#}"
            )),
        }
    }

    pub fn destroy_orphan_worker(
        &self,
        session_id: &str,
        target_id: &str,
        confirmation: &str,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        if confirmation != session_id {
            bail!("refusing destructive recovery: --confirm must exactly match the session ID");
        }
        let candidate = self
            .scan_orphan_workers(executor)
            .candidates
            .into_iter()
            .find(|candidate| {
                candidate.session_id == session_id && candidate.target_template_id == target_id
            })
            .with_context(|| {
                format!("no managed orphan {session_id} was found on target {target_id:?}")
            })?;
        let template = self.config.targets.get(target_id).unwrap();
        let backend = recovery_backend_locator(template, &candidate.locator, session_id)?;
        hel_targets::close_plan(&backend, session_id)?
            .execute(executor)
            .map(|_| ())
    }
}

/// The session record adoption commits before it tries the relay handshake.
fn adopted_session_record(
    session_id: &str,
    target_id: &str,
    profile_id: String,
    harness_kind: hel::hel_config::HarnessKind,
    bundle_id: String,
    workspace_id: String,
    locator: TargetLocator,
) -> SessionRecord {
    let now = now();
    SessionRecord {
        workspace_id,
        archived: false,
        container_cpus: None,
        container_memory: None,
        id: session_id.to_owned(),
        title: format!("Recovered {}", &session_id[..session_id.len().min(8)]),
        harness_kind,
        last_profile: profile_id,
        bundle_id,
        project_directory: None,
        managed_worktree: None,
        target_template_id: target_id.to_owned(),
        resource_allocation: None,
        additional_mounts: Vec::new(),
        state: SessionState::Disconnected,
        target: Some(locator),
        native_session_id: None,
        acp_session_title: None,
        session_title_override: None,
        created_at: now.clone(),
        updated_at: now,
        viewed_through_event_ordinal: 0,
        draft_input: String::new(),
        last_error: None,
        last_checkpoint_error: None,
        checkpoint: None,
    }
}

/// Whether a tracked session is one an adoption committed and never finished:
/// it names this target, carries the locator the scan found, and no harness
/// session has ever been observed on it. Such a record is the retry, so
/// adoption completes it instead of refusing it as already tracked.
fn adoption_unfinished(record: &SessionRecord, target_id: &str) -> bool {
    record.state == SessionState::Disconnected
        && record.native_session_id.is_none()
        && record.target_template_id == target_id
        && record.target.is_some()
}

fn scan_target_workers(
    target_id: &str,
    template: &TargetTemplate,
    executor: &impl CommandExecutor,
) -> Result<Vec<RecoveryCandidate>> {
    let mut candidates = match template {
        // Local bare sessions persist their locator in the controller database.
        // Do not infer an adoptable project from Hel's transient worker directory.
        TargetTemplate::LocalBare => Vec::new(),
        TargetTemplate::LocalPodman { .. } => scan_container_engine(
            target_id,
            template,
            "podman",
            vec![
                "ps".into(),
                "--all".into(),
                "--filter".into(),
                format!("label={}=true", hel_targets::MANAGED_LABEL),
                "--format".into(),
                "json".into(),
            ],
            executor,
        )?,
        TargetTemplate::LocalDocker { .. } => scan_container_engine(
            target_id,
            template,
            "docker",
            vec![
                "ps".into(),
                "--all".into(),
                "--filter".into(),
                format!("label={}=true", hel_targets::MANAGED_LABEL),
                "--format".into(),
                "json".into(),
            ],
            executor,
        )?,
        TargetTemplate::AppleContainer { .. } => scan_container_engine(
            target_id,
            template,
            "container",
            vec![
                "list".into(),
                "--all".into(),
                "--format".into(),
                "json".into(),
            ],
            executor,
        )?,
        TargetTemplate::SshPodman { ssh, .. } => {
            let remote = hel_targets::join_remote_command(&[
                "podman".into(),
                "ps".into(),
                "--all".into(),
                "--filter".into(),
                format!("label={}=true", hel_targets::MANAGED_LABEL),
                "--format".into(),
                "json".into(),
            ]);
            let output = execute_scan(
                executor,
                ssh_spec(ssh, [remote]),
                "scan remote Podman workers",
            )?;
            candidates_from_container_json(target_id, template, &output.stdout)?
        }
        TargetTemplate::AwsEc2 {
            aws_profile,
            region,
            address_source,
            ..
        } => {
            let profile = aws_profile.clone().unwrap_or_else(|| "default".into());
            let output = execute_scan(
                executor,
                CommandSpec::new(
                    "aws",
                    [
                        "--profile".into(),
                        profile,
                        "--region".into(),
                        region.clone(),
                        "ec2".into(),
                        "describe-instances".into(),
                        "--filters".into(),
                        format!("Name=tag:{},Values=true", hel_targets::MANAGED_TAG),
                        "Name=instance-state-name,Values=pending,running,stopping,stopped".into(),
                        "--output".into(),
                        "json".into(),
                    ],
                )
                .purpose("scan managed EC2 workers"),
                "scan managed EC2 workers",
            )?;
            candidates_from_aws_json(target_id, address_source.clone(), &output.stdout)?
        }
        TargetTemplate::SshBare { ssh, .. } => {
            let output = execute_scan(
                executor,
                ssh_spec(
                    ssh,
                    [hel_targets::join_remote_command(&[
                        "find".into(),
                        ".local/share/hel/workers".into(),
                        "-mindepth".into(),
                        "2".into(),
                        "-maxdepth".into(),
                        "2".into(),
                        "-name".into(),
                        "ownership.json".into(),
                        "-print".into(),
                    ])],
                ),
                "scan bare SSH worker markers",
            )?;
            output
                .stdout
                .split(|byte| *byte == b'\n')
                .filter_map(|line| {
                    let path = match std::str::from_utf8(line) {
                        Ok(path) => path.trim(),
                        Err(error) => {
                            tracing::debug!(%error, "recovery scan skipped a non-UTF-8 worker marker path");
                            return None;
                        }
                    };
                    let Some(session_id) = Path::new(path)
                        .parent()
                        .and_then(|parent| parent.file_name())
                        .and_then(|name| name.to_str())
                    else {
                        tracing::debug!(path, "recovery scan skipped a malformed worker marker path");
                        return None;
                    };
                    if let Err(error) = hel_targets::resource_name(session_id) {
                        tracing::debug!(session_id, %error, "recovery scan skipped an invalid session id");
                        return None;
                    }
                    let backend = match backend_target(template, None, ContainerOverrides::default()) {
                        Ok(backend) => backend,
                        Err(error) => {
                            tracing::debug!(session_id, %error, "recovery scan could not construct the target backend");
                            return None;
                        }
                    };
                    let workspace = match hel_targets::workspace_for(&backend, session_id) {
                        Ok(workspace) => workspace,
                        Err(error) => {
                            tracing::debug!(session_id, %error, "recovery scan could not derive the target workspace");
                            return None;
                        }
                    };
                    Some(RecoveryCandidate {
                        session_id: session_id.to_owned(),
                        target_template_id: target_id.to_owned(),
                        locator: TargetLocator::SshBare {
                            host: ssh.host.clone(),
                            workspace: PathBuf::from(workspace),
                            worker_id: None,
                        },
                        ownership: None,
                    })
                })
                .collect()
        }
    };
    for candidate in &mut candidates {
        candidate.ownership = read_recovery_ownership(template, candidate, executor);
    }
    Ok(candidates)
}

fn scan_container_engine(
    target_id: &str,
    template: &TargetTemplate,
    engine: &str,
    args: Vec<String>,
    executor: &impl CommandExecutor,
) -> Result<Vec<RecoveryCandidate>> {
    let output = execute_scan(
        executor,
        CommandSpec::new(engine, args).purpose("scan managed container workers"),
        "scan managed container workers",
    )?;
    candidates_from_container_json(target_id, template, &output.stdout)
}

fn candidates_from_container_json(
    target_id: &str,
    template: &TargetTemplate,
    stdout: &[u8],
) -> Result<Vec<RecoveryCandidate>> {
    let sessions = managed_sessions_from_container_json(stdout)?;
    Ok(sessions
        .into_iter()
        .filter_map(|session_id| {
            let generated = match hel_targets::resource_name(&session_id) {
                Ok(generated) => generated,
                Err(error) => {
                    tracing::debug!(%session_id, %error, "recovery scan skipped an invalid managed session id");
                    return None;
                }
            };
            let locator = match template {
                TargetTemplate::LocalPodman { .. } => TargetLocator::LocalPodman {
                    container_id: generated,
                    workspace_storage: Default::default(),
                },
                TargetTemplate::LocalDocker { .. } => TargetLocator::LocalDocker {
                    container_id: generated,
                },
                TargetTemplate::AppleContainer { .. } => TargetLocator::AppleContainer {
                    container_id: generated,
                },
                TargetTemplate::SshPodman { ssh, .. } => TargetLocator::SshPodman {
                    host: ssh.host.clone(),
                    container_id: generated,
                    workspace_storage: Default::default(),
                },
                _ => return None,
            };
            Some(RecoveryCandidate {
                session_id,
                target_template_id: target_id.to_owned(),
                locator,
                ownership: None,
            })
        })
        .collect())
}

pub(super) fn managed_sessions_from_container_json(stdout: &[u8]) -> Result<Vec<String>> {
    let values = serde_json::Deserializer::from_slice(stdout)
        .into_iter::<serde_json::Value>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse container list JSON")?;
    let mut sessions = Vec::new();
    for value in &values {
        collect_managed_sessions(value, &mut sessions);
    }
    sessions.sort();
    sessions.dedup();
    Ok(sessions)
}

pub(super) fn collect_managed_sessions(value: &serde_json::Value, sessions: &mut Vec<String>) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                collect_managed_sessions(value, sessions);
            }
        }
        serde_json::Value::Object(object) => {
            for label_key in ["Labels", "labels"] {
                if let Some(labels) = object.get(label_key) {
                    let managed = label_value(labels, hel_targets::MANAGED_LABEL)
                        .is_some_and(|value| value == "true");
                    if managed
                        && let Some(session) = label_value(labels, hel_targets::SESSION_LABEL)
                    {
                        sessions.push(session);
                    }
                }
            }
            for value in object.values() {
                collect_managed_sessions(value, sessions);
            }
        }
        _ => {}
    }
}

fn label_value(labels: &serde_json::Value, key: &str) -> Option<String> {
    match labels {
        serde_json::Value::Object(object) => object.get(key)?.as_str().map(str::to_owned),
        serde_json::Value::String(text) => text
            .split(',')
            .find_map(|label| {
                label
                    .trim()
                    .split_once('=')
                    .filter(|(name, _)| *name == key)
            })
            .map(|(_, value)| value.to_owned()),
        _ => None,
    }
}

fn candidates_from_aws_json(
    target_id: &str,
    address_source: AwsAddressSource,
    stdout: &[u8],
) -> Result<Vec<RecoveryCandidate>> {
    let value: serde_json::Value =
        serde_json::from_slice(stdout).context("parse AWS instance JSON")?;
    let mut result = Vec::new();
    let reservations = value
        .get("Reservations")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    for instance in reservations.iter().flat_map(|reservation| {
        reservation
            .get("Instances")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
    }) {
        let tags = instance
            .get("Tags")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let tag = |key: &str| {
            tags.iter()
                .find(|tag| tag.get("Key").and_then(serde_json::Value::as_str) == Some(key))
                .and_then(|tag| tag.get("Value"))
                .and_then(serde_json::Value::as_str)
        };
        if tag(hel_targets::MANAGED_TAG) != Some("true") {
            continue;
        }
        let Some(session_id) = tag(hel_targets::SESSION_TAG).map(str::to_owned) else {
            continue;
        };
        hel_targets::resource_name(&session_id)?;
        let instance_id = instance
            .get("InstanceId")
            .and_then(serde_json::Value::as_str)
            .context("managed EC2 instance omitted InstanceId")?
            .to_owned();
        let field = match address_source {
            AwsAddressSource::PublicDns => "PublicDnsName",
            AwsAddressSource::PublicIp => "PublicIpAddress",
            AwsAddressSource::PrivateDns => "PrivateDnsName",
            AwsAddressSource::PrivateIp => "PrivateIpAddress",
        };
        let address = instance
            .get(field)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        result.push(RecoveryCandidate {
            session_id,
            target_template_id: target_id.to_owned(),
            locator: TargetLocator::AwsEc2 {
                instance_id,
                address,
            },
            ownership: None,
        });
    }
    Ok(result)
}

fn execute_scan(
    executor: &impl CommandExecutor,
    command: CommandSpec,
    operation: &str,
) -> Result<CommandOutput> {
    let output = executor.execute(&command)?;
    if output.status != 0 {
        bail!(
            "{operation} failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

fn ssh_spec(ssh: &SshConnection, remote: impl IntoIterator<Item = String>) -> CommandSpec {
    let backend = backend_ssh(ssh);
    let mut args = backend.ssh_args;
    args.push(backend.destination);
    args.extend(remote);
    CommandSpec::new("ssh", args)
}

fn read_recovery_ownership(
    template: &TargetTemplate,
    candidate: &RecoveryCandidate,
    executor: &impl CommandExecutor,
) -> Option<WorkerOwnership> {
    let backend =
        match recovery_backend_locator(template, &candidate.locator, &candidate.session_id) {
            Ok(backend) => backend,
            Err(error) => {
                tracing::debug!(
                    session_id = %candidate.session_id,
                    %error,
                    "could not construct a recovery ownership probe"
                );
                return None;
            }
        };
    let root = match hel_targets::worker_root(&backend, &candidate.session_id) {
        Ok(root) => root,
        Err(error) => {
            tracing::debug!(
                session_id = %candidate.session_id,
                %error,
                "could not derive a recovery worker root"
            );
            return None;
        }
    };
    let command = match hel_targets::command_on_locator(
        &backend,
        &candidate.session_id,
        vec!["cat".into(), format!("{root}/ownership.json")],
        "read worker ownership marker",
    ) {
        Ok(command) => command,
        Err(error) => {
            tracing::debug!(
                session_id = %candidate.session_id,
                %error,
                "could not construct a recovery ownership command"
            );
            return None;
        }
    };
    let output = match executor.execute(&command) {
        Ok(output) => output,
        Err(error) => {
            tracing::debug!(
                session_id = %candidate.session_id,
                %error,
                "could not read a recovery worker ownership marker"
            );
            return None;
        }
    };
    if output.status != 0 {
        tracing::debug!(
            session_id = %candidate.session_id,
            status = output.status,
            "recovery worker ownership probe returned a failure"
        );
        return None;
    }
    let marker: WorkerOwnership = match serde_json::from_slice(&output.stdout) {
        Ok(marker) => marker,
        Err(error) => {
            tracing::debug!(
                session_id = %candidate.session_id,
                %error,
                "recovery worker ownership marker was not valid JSON"
            );
            return None;
        }
    };
    if !(1..=WorkerOwnership::VERSION).contains(&marker.version)
        || marker.session_id != candidate.session_id
        || marker.target_template_id != candidate.target_template_id
    {
        tracing::debug!(
            session_id = %candidate.session_id,
            marker_session_id = %marker.session_id,
            marker_target_template_id = %marker.target_template_id,
            "recovery worker ownership marker did not match the candidate"
        );
        return None;
    }
    Some(marker)
}

fn recovery_backend_locator(
    template: &TargetTemplate,
    locator: &TargetLocator,
    session_id: &str,
) -> Result<hel_targets::TargetLocator> {
    Ok(match (template, locator) {
        (TargetTemplate::LocalBare, TargetLocator::LocalBare { worker_root }) => {
            hel_targets::TargetLocator::LocalBare {
                worker_root: worker_root.to_string_lossy().into_owned(),
            }
        }
        (TargetTemplate::LocalPodman { .. }, TargetLocator::LocalPodman { container_id, .. }) => {
            hel_targets::TargetLocator::LocalPodman {
                container_id: container_id.clone(),
                workspace_storage: Default::default(),
            }
        }
        (TargetTemplate::LocalDocker { .. }, TargetLocator::LocalDocker { container_id }) => {
            hel_targets::TargetLocator::LocalDocker {
                container_id: container_id.clone(),
            }
        }
        (TargetTemplate::AppleContainer { .. }, TargetLocator::AppleContainer { container_id }) => {
            hel_targets::TargetLocator::AppleContainer {
                container_id: container_id.clone(),
            }
        }
        (TargetTemplate::SshPodman { ssh, .. }, TargetLocator::SshPodman { container_id, .. }) => {
            hel_targets::TargetLocator::SshPodman {
                ssh: backend_ssh(ssh),
                container_id: container_id.clone(),
                workspace_storage: Default::default(),
            }
        }
        (TargetTemplate::SshBare { ssh, .. }, TargetLocator::SshBare { workspace, .. }) => {
            hel_targets::TargetLocator::SshBare {
                ssh: backend_ssh(ssh),
                workspace: workspace.to_string_lossy().into_owned(),
            }
        }
        (
            TargetTemplate::AwsEc2 {
                aws_profile,
                region,
                ssh_user,
                identity_file,
                ssh_args,
                ..
            },
            TargetLocator::AwsEc2 {
                instance_id,
                address,
            },
        ) => hel_targets::TargetLocator::AwsEc2 {
            profile: aws_profile.clone().unwrap_or_else(|| "default".into()),
            region: region.clone(),
            instance_id: instance_id.clone(),
            ssh: SshTarget {
                destination: format!(
                    "{ssh_user}@{}",
                    address.as_deref().unwrap_or("unavailable.invalid")
                ),
                ssh_args: ssh_args_with_identity(ssh_args, identity_file.as_deref()),
            },
            workspace: format!(".local/share/hel/workspaces/{session_id}"),
        },
        _ => bail!("recovery target locator does not match target template"),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use hel::hel_config::{
        AwsAddressSource, ContainerTemplate as ConfigContainer, HarnessKind, HelConfig,
        TargetTemplate,
    };
    use hel::hel_state::{HelState, TargetLocator};
    use hel::hel_targets::ProcessExecutor;

    use super::*;

    const FAILED_ADOPTION_CHILD: &str = "MJ_TEST_FAILED_ADOPTION_CHILD";

    #[tokio::test]
    async fn a_failed_adoption_records_the_failure_and_stays_retryable() {
        // MJ_DATA_DIR is process-global, so the database-backed half runs in
        // an exact child test with its own data directory.
        if std::env::var_os(FAILED_ADOPTION_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "hel_controller::recovery_scan::tests::\
                     a_failed_adoption_records_the_failure_and_stays_retryable",
                    "--nocapture",
                ])
                .env(FAILED_ADOPTION_CHILD, "1")
                .env("MJ_DATA_DIR", directory.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated adoption retry test failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = hel::hel_database::install_isolated_test_writer();

        let session_id = "0123456789abcdef0123456789abcdef";
        let workers = tempfile::tempdir().unwrap();
        // Exactly what an adoption commits before its handshake, on a worker
        // root that holds no worker binary, so the handshake cannot succeed.
        let record = adopted_session_record(
            session_id,
            "local-bare",
            "codex".into(),
            HarnessKind::Codex,
            "project".into(),
            hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            TargetLocator::LocalBare {
                worker_root: workers.path().join(session_id),
            },
        );
        assert!(
            adoption_unfinished(&record, "local-bare"),
            "the record adoption commits must be the record adoption can retry"
        );
        hel::hel_database::save_session(&record).unwrap();
        let mut config = HelConfig::default();
        config
            .targets
            .insert("local-bare".into(), TargetTemplate::LocalBare);
        let mut state = HelState::default();
        state.sessions.insert(session_id.to_owned(), record);
        let mut controller = Controller { config, state };

        let failure = controller
            .adopt_orphan_worker(session_id, "local-bare", None, None, &ProcessExecutor)
            .await
            .expect_err("a worker root without a worker cannot complete the handshake");
        assert!(
            format!("{failure:#}").contains("orphan relay"),
            "unexpected failure: {failure:#}"
        );
        let recorded = controller.state.sessions[session_id]
            .last_error
            .clone()
            .expect("the failed handshake was recorded on the session");
        assert!(
            recorded.contains("orphan adoption failed"),
            "unexpected recorded failure: {recorded}"
        );
        assert_eq!(
            controller.state.sessions[session_id].state,
            SessionState::Disconnected
        );
        let stored = hel::hel_database::load_state().unwrap();
        assert_eq!(
            stored.sessions[session_id].last_error.as_deref(),
            Some(recorded.as_str()),
            "the adoption failure was not persisted"
        );

        let retry = controller
            .adopt_orphan_worker(session_id, "local-bare", None, None, &ProcessExecutor)
            .await
            .expect_err("the worker is still unreachable");
        let retry = format!("{retry:#}");
        assert!(
            retry.contains("orphan relay"),
            "adoption did not retry the handshake: {retry}"
        );
        assert!(
            !retry.contains("already tracked"),
            "a session adoption never finished blocked its own retry: {retry}"
        );
    }

    #[test]
    fn a_session_that_completed_its_handshake_is_not_adoptable_again() {
        let mut record = adopted_session_record(
            "0123456789abcdef0123456789abcdef",
            "local-bare",
            "codex".into(),
            HarnessKind::Codex,
            "project".into(),
            hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            TargetLocator::LocalBare {
                worker_root: std::path::PathBuf::from("/workers/0123456789abcdef0123456789abcdef"),
            },
        );
        record.native_session_id = Some("native-session".into());
        assert!(!adoption_unfinished(&record, "local-bare"));

        record.native_session_id = None;
        assert!(
            !adoption_unfinished(&record, "other-target"),
            "a record adopted onto another target is not this target's retry"
        );
    }

    #[test]
    fn recovery_container_scan_requires_both_managed_and_session_labels() {
        let template = TargetTemplate::LocalPodman {
            container: ConfigContainer {
                image: "ignored".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: BTreeMap::new(),
                workspace_storage: Default::default(),
            },
        };
        let json = serde_json::json!([
            {"Labels": {"dev.mj.managed": "true", "dev.mj.session": "0123456789abcdef0123456789abcdef"}},
            {"Labels": {"dev.mj.managed": "false", "dev.mj.session": "not-owned"}},
            {"configuration": {"labels": "dev.mj.managed=true,dev.mj.session=abcdef0123456789abcdef0123456789"}}
        ]);
        let candidates = candidates_from_container_json(
            "local",
            &template,
            serde_json::to_string(&json).unwrap().as_bytes(),
        )
        .unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].session_id, "0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn recovery_docker_scan_accepts_json_lines_and_builds_a_docker_locator() {
        let template = TargetTemplate::LocalDocker {
            container: ConfigContainer {
                image: "ignored".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: BTreeMap::new(),
                workspace_storage: Default::default(),
            },
        };
        let session = "0123456789abcdef0123456789abcdef";
        let output = format!(
            "{{\"Labels\":\"dev.mj.managed=true,dev.mj.session={session}\"}}\n{{\"Labels\":\"dev.mj.managed=false,dev.mj.session=ignored\"}}\n"
        );

        let candidates =
            candidates_from_container_json("docker", &template, output.as_bytes()).unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, session);
        assert!(matches!(
            &candidates[0].locator,
            TargetLocator::LocalDocker { container_id }
                if container_id == &hel_targets::resource_name(session).unwrap()
        ));
    }

    #[test]
    fn recovery_aws_scan_uses_exact_tagged_instance_and_address() {
        let json = serde_json::json!({"Reservations": [{"Instances": [{
            "InstanceId": "i-exact",
            "PrivateIpAddress": "10.0.0.7",
            "Tags": [
                {"Key": "dev.mj.managed", "Value": "true"},
                {"Key": "dev.mj.session", "Value": "0123456789abcdef0123456789abcdef"}
            ]
        }]}]});
        let candidates = candidates_from_aws_json(
            "aws",
            AwsAddressSource::PrivateIp,
            serde_json::to_string(&json).unwrap().as_bytes(),
        )
        .unwrap();
        assert!(matches!(
            &candidates[0].locator,
            TargetLocator::AwsEc2 { instance_id, address }
                if instance_id == "i-exact" && address.as_deref() == Some("10.0.0.7")
        ));
    }
}
