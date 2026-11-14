use super::*;

pub struct NativeImportRequest<'a> {
    pub harness: HarnessKind,
    pub harness_home: &'a Path,
    pub native_session_id: &'a str,
    pub source_path: &'a Path,
    pub transcript: &'a ClaudeTranscript,
    pub bundle_id: &'a str,
    pub profile_id: Option<&'a str>,
    pub title: Option<&'a str>,
    pub daemon_project_root: Option<&'a Path>,
    pub archive_directory: &'a Path,
}

/// Build, verify, and install a local archive for one already-located,
/// already-parsed native session, for any harness, then update the in-memory
/// state. The caller saves `state` only after this returns successfully.
pub fn import_native_session(
    config: &Config,
    state: &mut State,
    request: NativeImportRequest<'_>,
    control: Option<&ImportControl<'_>>,
) -> Result<ImportedClaudeSession> {
    let NativeImportRequest {
        harness,
        harness_home,
        native_session_id,
        source_path,
        transcript,
        bundle_id,
        profile_id,
        title,
        archive_directory,
        daemon_project_root,
    } = request;
    let bundle = config
        .bundles
        .get(bundle_id)
        .with_context(|| format!("unknown bundle {bundle_id:?}"))?;
    let session_title_override = title.map(str::to_owned);
    let title = match session_title_override.as_deref() {
        Some(title) if !title.trim().is_empty() => title.to_owned(),
        Some(_) => bail!("import title must not be empty"),
        None => harness_session_title(&transcript.events).unwrap_or_else(|| {
            format!(
                "Imported {} session {native_session_id}",
                harness.display_name()
            )
        }),
    };
    let targets = session_edit_targets(transcript, harness_home)?;
    let raw_project = raw_project_import(config, &targets);
    let recorded_root = import_project_root(transcript, &targets);
    let remap = daemon_project_root
        .filter(|path| *path != recorded_root.as_path())
        .map(|destination| ImportProjectRemap::new(&recorded_root, destination, &targets.git_roots))
        .transpose()?;
    let mut repositories =
        collect_local_repositories(bundle, &targets.git_roots, raw_project.is_none(), control)?;
    let native_artifacts =
        collect_import_native_artifacts(harness, harness_home, native_session_id, source_path)?;
    if harness == HarnessKind::Muse {
        // The preview may precede a user's confirmation by minutes. Never
        // pair its old transcript with a newer native conversation.
        if let Some(control) = control {
            control.check_cancelled()?;
        }
        let current = read_native_transcript(harness, source_path)?;
        ensure!(
            current.cwd == transcript.cwd
                && current.edited_paths == transcript.edited_paths
                && serde_json::to_value(&current.events)?
                    == serde_json::to_value(&transcript.events)?,
            "native session changed after it was selected; select it again"
        );
        ensure!(
            native_artifacts
                == collect_import_native_artifacts(
                    harness,
                    harness_home,
                    native_session_id,
                    source_path
                )?,
            "native session changed while being imported; stop its harness and retry"
        );
    }
    let session_id = new_session_id()?;
    let canonical_session =
        canonical_import_session(session_id.as_str(), &transcript.events, source_path)?;
    let timestamp = timestamp();
    let profile_id = import_profile_id(config, profile_id, harness, harness_home)?;
    let target_id = default_import_target_id(config);
    let mut project = crate::project_catalog::snapshot(
        bundle,
        &mj_core::targets::CancellableProcessExecutor::with_timeout(Duration::from_secs(15)),
        raw_project.is_none(),
    )?;
    let remapped_bundle = if let Some(remap) = remap {
        remap.apply(&mut project, &mut repositories)?;
        Some(project.bundle.clone())
    } else {
        None
    };
    let remapped_id = remapped_bundle
        .as_ref()
        .map(|_| format!("import-{session_id}"));
    let bundle_id = remapped_id.as_deref().unwrap_or(bundle_id);
    let archive_path = archive_directory.join(format!("{session_id}.hel.zip"));
    if let Some(control) = control {
        control.report(ImportArchiveProgress::WritingArchive)?;
    }
    let verified = write_archive_atomic(
        &archive_path,
        &ArchiveInput {
            session: mj_checkpoint::archive::SessionManifest {
                id: session_id.clone(),
                title: title.clone(),
                harness_kind: harness,
                profile_id: profile_id.clone(),
                native_session_id: native_session_id.to_owned(),
                created_at: timestamp.clone(),
                checkpointed_at: timestamp.clone(),
                hel_version: env!("CARGO_PKG_VERSION").into(),
                relay_version: env!("CARGO_PKG_VERSION").into(),
                adapter_version: "acp-v1".into(),
            },
            target: TargetManifest {
                template_id: target_id.clone(),
                target_kind: "import".into(),
                details: BTreeMap::from([("source".into(), format!("{}-import", harness.id()))]),
            },
            bundle: BundleManifest {
                id: bundle_id.to_owned(),
                primary_repository: bundle.primary_repo.clone(),
            },
            canonical_session,
            native_artifacts,
            repositories,
        },
    )?;
    if let Some(control) = control
        && let Err(error) = control.check_cancelled()
    {
        let _ = fs::remove_file(&archive_path);
        return Err(error);
    }
    let checkpoint = CheckpointMetadata {
        archive_path: archive_path.clone(),
        sha256: verified.archive_sha256,
        created_at: timestamp.clone(),
        event_frontier: transcript.events.last().map_or(0, |event| event.seq),
    };
    state.sessions.insert(
        session_id.clone(),
        SessionRecord {
            oracle_choice: None,
            oracle: true,
            notifications_ignored: false,
            project: Some(project.into()),
            target_runtime: None,
            launch_base: None,
            launch_branch: None,
            checkout: None,
            publication: None,
            build_cache: None,
            subagents: None,
            // An imported history is a new session: when it is resumed into a
            // container it gets its own workspace, like any session created
            // now.
            container_workspace: Some(mj_core::targets::new_container_workspace(&session_id)?),
            create_managed_worktree: None,
            workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: session_id.clone(),
            title,
            harness_kind: harness,
            last_profile: profile_id,
            bundle_id: bundle_id.to_owned(),
            project_directory: raw_project
                .as_ref()
                .map(|(directory, _)| daemon_project_root.unwrap_or(directory).to_path_buf()),
            managed_worktree: None,
            review: None,
            no_project_memory: false,
            no_github_events: false,
            no_github_credentials: !config.github.session_credentials,
            target_template_id: raw_project.map_or(target_id, |(_, raw_target_id)| raw_target_id),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            state: SessionState::Stopped,
            target: None,
            native_session_id: Some(native_session_id.to_owned()),
            acp_session_title: None,
            session_title_override,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            dismissed_failure: None,
            failure_generation: 0,
            last_checkpoint_error: None,
            checkpoint: Some(Box::new(checkpoint)),
        },
    );
    Ok(ImportedClaudeSession {
        session_id,
        native_session_id: native_session_id.to_owned(),
        source_jsonl: source_path.to_path_buf(),
        source_cwd: transcript.cwd.clone(),
        bundle_id: bundle_id.to_owned(),
        archive_path,
        remapped_bundle,
    })
}

pub(super) fn default_import_target_id(config: &Config) -> String {
    config
        .targets
        .get_key_value("podman")
        .map(|(id, _)| id)
        .or_else(|| {
            config.targets.iter().find_map(|(id, target)| {
                matches!(
                    target,
                    TargetTemplate::LocalPodman { .. }
                        | TargetTemplate::LocalDocker { .. }
                        | TargetTemplate::SshPodman { .. }
                        | TargetTemplate::SshDocker { .. }
                )
                .then_some(id)
            })
        })
        .or_else(|| config.targets.keys().next())
        .cloned()
        .unwrap_or_else(|| "import".into())
}

/// Target that hosts raw project sessions on this machine.
pub(super) fn raw_import_target_id(config: &Config) -> Option<String> {
    let local_bare = |template: &TargetTemplate| matches!(template, TargetTemplate::LocalBare);
    config
        .targets
        .get_key_value("localhost")
        .filter(|(_, template)| local_bare(template))
        .map(|(id, _)| id.clone())
        .or_else(|| {
            config
                .targets
                .iter()
                .find_map(|(id, template)| local_bare(template).then(|| id.clone()))
        })
}

/// A session that only wrote to its own repository can keep working in that
/// directory, so import it as a raw project session instead of a bundle
/// session. `session_edit_targets` always records the cwd root, so a single
/// durable root is that root.
pub fn raw_project_import(
    config: &Config,
    targets: &SessionEditTargets,
) -> Option<(PathBuf, String)> {
    let [cwd_root] = targets.git_roots.as_slice() else {
        return None;
    };
    Some((cwd_root.clone(), raw_import_target_id(config)?))
}

pub(super) fn collect_local_repositories(
    bundle: &ProjectBundle,
    detected_roots: &[PathBuf],
    isolated: bool,
    control: Option<&ImportControl<'_>>,
) -> Result<Vec<mj_checkpoint::archive::RepositorySnapshot>> {
    let detected = detected_roots
        .iter()
        .map(|root| Ok((root_identity(root)?, root.clone())))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let repository_paths = bundle
        .repositories
        .iter()
        .map(|repository| {
            // A local source remains identified by its configured path even
            // after its checkout gains a network origin. GitHub-origin
            // detection is still used for configured network sources.
            let path = if let Some(configured_path) = repository.local.as_ref() {
                let configured_path =
                    fs::canonicalize(configured_path).unwrap_or_else(|_| configured_path.clone());
                detected_roots
                    .iter()
                    .find(|root| {
                        fs::canonicalize(root).unwrap_or_else(|_| (*root).clone())
                            == configured_path
                    })
                    .cloned()
            } else {
                let identity = configured_repository_identity(repository)?.with_context(|| {
                    format!("repository {:?} has no usable source", repository.id)
                })?;
                detected.get(&identity).cloned()
            };
            let path = path.with_context(|| {
                format!(
                    "repository {:?} was not detected in the native session",
                    repository.id
                )
            })?;
            Ok((repository.id.clone(), path))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let git = SystemGit;
    let repository_count = bundle.repositories.len();
    bundle
        .repositories
        // Indexed parallel iteration keeps repository and manifest order
        // identical to the configured bundle.
        .par_iter()
        .enumerate()
        .map(|(index, repository)| {
            if let Some(control) = control {
                control.report(ImportArchiveProgress::Repository {
                    current: index + 1,
                    total: repository_count,
                    id: repository.id.clone(),
                })?;
            }
            let path = repository_paths
                .get(&repository.id)
                .expect("repository paths cover the validated bundle")
                .clone();
            ensure!(
                path.is_dir(),
                "local repository {:?} is missing at {}",
                repository.id,
                path.display()
            );
            let source = isolated
                .then(|| {
                    resolve_repository(repository, &ProcessExecutor)
                        .with_context(|| format!("resolve network source for {:?}", repository.id))
                })
                .transpose()?;
            // Isolated imports restore the native session's committed work on
            // top of the source's available network baseline. Raw imports
            // continue to use the live local checkout, including repositories
            // without any remote.
            let history = if isolated {
                GitHistoryMode::DeltaFrom(import_delta_base(
                    &path,
                    &source.as_ref().expect("isolated source resolved").fetch_url,
                )?)
            } else {
                GitHistoryMode::NoBundle
            };
            let origin_override = if let Some(source) = &source {
                Some(source.fetch_url.clone())
            } else {
                Some(path.to_string_lossy().into_owned())
            };
            let mut snapshot = collect_git_snapshot_with_progress(
                &git,
                &path,
                &GitCollectionSpec {
                    id: repository.id.clone(),
                    relative_destination: repository.destination.clone(),
                    history,
                    origin_override,
                },
                if control.is_none_or(|control| control.include_untracked) {
                    mj_checkpoint::archive::UntrackedFiles::Include
                } else {
                    mj_checkpoint::archive::UntrackedFiles::Exclude
                },
                &|progress| {
                    let Some(control) = control else {
                        return Ok(());
                    };
                    match progress {
                        GitSnapshotProgress::UntrackedFile {
                            current,
                            total,
                            path,
                        } => control.report(ImportArchiveProgress::UntrackedFile {
                            repository_id: repository.id.clone(),
                            current,
                            total,
                            path,
                        }),
                    }
                },
            )
            .with_context(|| format!("collect local repository {:?}", repository.id))?;
            if let Some(source) = source {
                snapshot.metadata.push_urls = source
                    .push_urls
                    .iter()
                    .map(|url| mj_checkpoint::archive::redact_origin_credentials(url))
                    .collect::<Result<Vec<_>>>()?;
                snapshot.metadata.remote_workspace = true;
            } else {
                snapshot.metadata.push_urls.clear();
            }
            Ok(snapshot)
        })
        .collect()
}

pub(super) fn canonical_import_session(
    session_id: &str,
    events: &[SequencedEvent],
    source_path: &Path,
) -> Result<mj_checkpoint::archive::CanonicalSessionSnapshot> {
    let mut events = events.to_vec();
    finalize_import_event_times(&mut events, source_path)?;
    let mut materialized =
        mj_transcript::projection::imported_materialized_session(session_id, &events);
    materialized.session_title = harness_session_title(&events);
    if let Some(last_activity_at_ms) = events.iter().filter_map(|event| event.recorded_at_ms).max()
    {
        materialized.last_activity_at_ms = Some(
            materialized
                .last_activity_at_ms
                .map_or(last_activity_at_ms, |current| {
                    current.max(last_activity_at_ms)
                }),
        );
    }
    canonical_session_from_materialized(&materialized)
}

pub(super) fn default_profile(config: &Config, harness: HarnessKind, home: &Path) -> String {
    let source = fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    config
        .enabled_profiles()
        .find(|(_, profile)| {
            profile.kind == harness
                && fs::canonicalize(&profile.home).unwrap_or_else(|_| profile.home.clone())
                    == source
        })
        .or_else(|| {
            config
                .enabled_profiles()
                .find(|(_, profile)| profile.kind == harness)
        })
        .map(|(id, _)| id.to_owned())
        .unwrap_or_else(|| format!("{}-import", harness.id()))
}

pub(super) fn import_profile_id(
    config: &Config,
    requested: Option<&str>,
    harness: HarnessKind,
    home: &Path,
) -> Result<String> {
    let Some(requested) = requested else {
        return Ok(default_profile(config, harness, home));
    };
    let profile = config
        .profiles
        .get(requested)
        .with_context(|| format!("unknown import profile {requested:?}"))?;
    ensure!(profile.enabled, "import profile {requested:?} is disabled");
    ensure!(
        profile.kind == harness,
        "import profile {requested:?} does not use {harness:?}"
    );
    Ok(requested.to_owned())
}

/// The upstream revision an imported repository deltas from. A repository
/// without remote-tracking refs cannot tell us which ancestry a newly
/// provisioned clone has, and Hel never bundles full history, so it fails here.
pub(super) fn import_delta_base(path: &Path, fetch_url: &str) -> Result<String> {
    let remotes = git_optional_text(path, ["remote"])?.unwrap_or_default();
    let upstream_ref = git_optional_text(
        path,
        [
            "rev-parse",
            "--symbolic-full-name",
            "--verify",
            "--quiet",
            "@{upstream}",
        ],
    )?;
    for remote in remotes.lines() {
        let Some(url) = git_optional_text(path, ["remote", "get-url", remote])? else {
            continue;
        };
        let same_source = url == fetch_url
            // `url` comes from `git remote get-url`, where `owner/repo` is a
            // relative path; `fetch_url` is a resolved source.
            || mj_core::state::ProjectSourceIdentity::git_remote(&url, mj_core::remote_git::Shorthand::Rejected).is_some_and(|identity| {
                Some(identity) == mj_core::state::ProjectSourceIdentity::git_remote(fetch_url, mj_core::remote_git::Shorthand::Allowed)
            });
        if !same_source {
            continue;
        }
        let prefix = format!("refs/remotes/{remote}/");
        let revision = upstream_ref
            .as_deref()
            .filter(|reference| reference.starts_with(&prefix))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{prefix}HEAD"));
        if let Some(base) =
            git_optional_text(path, ["rev-parse", "--verify", "--quiet", &revision])?
        {
            return Ok(base);
        }
    }
    bail!(
        "repository {} has no remote-tracking refs to import against for its selected network source; fetch its remote first",
        path.display()
    )
}

pub(super) fn git_optional_text<const N: usize>(
    cwd: &Path,
    arguments: [&str; N],
) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("start git in {}", cwd.display()))?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8(output.stdout).context("decode Git output")?;
    Ok((!text.trim().is_empty()).then(|| text.trim().to_owned()))
}

pub(super) fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn persist_imported_session_locally(session: &SessionRecord) -> Result<()> {
    let checkpoint = session
        .checkpoint
        .as_ref()
        .context("imported session has no checkpoint")?;
    let canonical = mj_checkpoint::archive::verify_archive_streaming(&checkpoint.archive_path)?
        .canonical_session;
    let materialized = mj_transcript::projection::materialized_session_from_canonical(
        session.id.clone(),
        &canonical,
    )?;
    crate::database::save_imported_session(session, &materialized)
}

/// The recorded project root used by both the path prompt and archive relocation.
pub fn import_project_root(transcript: &ClaudeTranscript, targets: &SessionEditTargets) -> PathBuf {
    targets
        .git_roots
        .iter()
        .find(|root| transcript.cwd.starts_with(root))
        .cloned()
        .unwrap_or_else(|| transcript.cwd.clone())
}

/// Relocate operational project metadata together; native history retains its provenance.
struct ImportProjectRemap<'a> {
    recorded: &'a Path,
    destination: &'a Path,
}

impl<'a> ImportProjectRemap<'a> {
    fn new(recorded: &'a Path, destination: &'a Path, roots: &[PathBuf]) -> Result<Self> {
        ensure!(
            mj_core::target_path::is_absolute_on_host_or_target(destination),
            "Enter an absolute project root on the daemon host, or keep the recorded root."
        );
        let outside = roots
            .iter()
            .filter(|root| !root.starts_with(recorded))
            .collect::<Vec<_>>();
        ensure!(
            outside.is_empty(),
            "cannot relocate repositories outside recorded project root {}: {}. Import one repository at a time, or record the session from their common parent and import again with --path pointing to that parent's daemon-host counterpart.",
            recorded.display(),
            outside
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        Ok(Self {
            recorded,
            destination,
        })
    }
    fn apply(
        &self,
        project: &mut mj_core::repository::ProjectBundleSnapshot,
        repositories: &mut [mj_checkpoint::archive::RepositorySnapshot],
    ) -> Result<()> {
        let Self {
            recorded,
            destination,
        } = *self;
        let remap = |path: &Path| {
            path.strip_prefix(recorded).ok().map(|relative| {
                if relative.as_os_str().is_empty() {
                    destination.to_path_buf()
                } else {
                    // Daemon project paths obey the POSIX session-target contract.
                    mj_core::target_path::join(destination, relative)
                }
            })
        };
        let remap_url = |url: &mut String| {
            if let Some(path) = remap(Path::new(url)) {
                *url = path.to_string_lossy().into_owned();
            }
        };
        for repository in &mut project.bundle.repositories {
            if let Some(path) = &mut repository.local {
                *path = remap(path).with_context(|| format!("repository {} at {} is outside recorded project root {}; import one repository at a time", repository.id, path.display(), recorded.display()))?;
            }
        }
        for identity in project.identities.values_mut() {
            if let mj_core::repository::RepositoryIdentity::Local(path) = identity
                && let Some(mapped) = remap(path)
            {
                *path = mapped;
            }
        }
        for source in project.network_sources.values_mut() {
            remap_url(&mut source.fetch_url);
            for url in &mut source.push_urls {
                remap_url(url);
            }
        }
        for repository in repositories {
            remap_url(&mut repository.metadata.origin);
            for url in &mut repository.metadata.push_urls {
                remap_url(url);
            }
        }
        Ok(())
    }
}
