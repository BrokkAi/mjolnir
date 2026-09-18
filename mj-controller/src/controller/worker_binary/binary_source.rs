use super::*;
use mj_core::hex::lower_hex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerBinaryAvailability {
    Local {
        path: PathBuf,
        source: String,
    },
    Remote {
        url: String,
        sha256: String,
        triple: String,
    },
}

/// Sources captured before the daemon starts its managers and coordinators.
///
/// Local sources are copied into an immutable, content-addressed cache during
/// capture. Remote sources retain only their URL, digest, and target triple;
/// the network fetch still happens when a target is provisioned.
#[derive(Debug)]
pub(super) struct WorkerBinarySourceSnapshot {
    pub(super) entries: HashMap<
        (String, WorkerBinaryRequirement),
        std::result::Result<WorkerBinaryAvailability, String>,
    >,
}

pub(super) static PINNED_WORKER_BINARY_SOURCES: OnceLock<WorkerBinarySourceSnapshot> =
    OnceLock::new();

pub(super) fn packaged_worker_binary_path(directory: &Path, triple: &str) -> PathBuf {
    directory.join(format!("mj-worker-{triple}"))
}

/// Linux exposes an unlinked running executable through `/proc` with a
/// ` (deleted)` suffix. `current_exe` preserves that suffix, but it is not
/// part of the executable's real file name and must not leak into sibling
/// lookup after `cargo` or a package upgrade replaces the controller.
pub(super) fn running_executable_file_name(controller: &Path) -> Option<std::ffi::OsString> {
    let name = controller.file_name()?;
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        if let Some(name) = name.as_bytes().strip_suffix(b" (deleted)") {
            return Some(std::ffi::OsString::from_vec(name.to_vec()));
        }
    }
    Some(name.to_os_string())
}

/// File names a worker binary may carry when it sits beside the controller or
/// in a development sibling directory. The controller's own file name comes
/// first (after the 2.0 rename that is `mj`), then the legacy `hel` name that
/// older packages shipped, so both resolve without hardcoding one.
pub(super) fn worker_sibling_names(controller: &Path) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    let mut names = Vec::new();
    if let Some(own) = running_executable_file_name(controller) {
        names.push(own);
    }
    let legacy = OsString::from("hel");
    if !names.contains(&legacy) {
        names.push(legacy);
    }
    names
}

/// A local-bare session runs on the controller host, so it may use the native
/// worker built or packaged beside `mj`. Managed targets never consider this
/// name because a macOS or glibc binary is not portable into Linux targets.
pub(super) fn select_native_worker(
    controller: &Path,
    is_file: impl Fn(&Path) -> bool,
) -> Option<(PathBuf, &'static str)> {
    let directory = controller.parent()?;
    if let (Some(profile), Some(target_dir)) = (directory.file_name(), directory.parent()) {
        let development_worker = target_dir.join("worker").join(profile).join("mj-worker");
        if is_file(&development_worker) {
            return Some((development_worker, "isolated native development worker"));
        }
    }
    let packaged_worker = directory.join("mj-worker");
    is_file(&packaged_worker).then_some((packaged_worker, "native worker beside mj"))
}

/// Choose a worker binary that ships beside the controller or in a development
/// musl sibling directory. `is_file` probes the filesystem; tests pass a
/// hand-written probe. The static musl sibling is probed before the worker in
/// the controller's own directory, because in a development checkout that
/// same-directory candidate resolves to the controller itself, whose glibc may
/// be newer than the target's.
pub(super) fn select_sibling_worker(
    controller: &Path,
    triple: &str,
    is_file: impl Fn(&Path) -> bool,
) -> Option<(PathBuf, &'static str)> {
    let directory = controller.parent()?;
    let names = worker_sibling_names(controller);
    let mut candidates: Vec<(PathBuf, &'static str)> = Vec::new();
    // Packaged worker beside the controller, named for the target triple.
    candidates.push((
        packaged_worker_binary_path(directory, triple),
        "beside the mj binary",
    ));
    // Development checkout: a controller at target/<profile>/<name> finds its
    // musl sibling at target/<triple>/<profile>/<name>. The static build is
    // preferred because the target's glibc may be older than the host's, so it
    // is probed before the same-directory worker (which is the controller
    // itself in a development checkout).
    if let (Some(profile), Some(target_dir)) = (directory.file_name(), directory.parent()) {
        candidates.push((
            target_dir
                .join("worker")
                .join(triple)
                .join(profile)
                .join("mj-worker"),
            "isolated development musl worker",
        ));
        candidates.push((
            target_dir.join(triple).join(profile).join("mj-worker"),
            "development musl worker",
        ));
        for name in &names {
            candidates.push((
                target_dir.join(triple).join(profile).join(name),
                "development musl sibling",
            ));
        }
    }
    // A legacy package may put an `hel`-named worker beside an `mj`
    // controller. Never select the controller's own same-directory path: on
    // glibc Linux that is not a portable worker, and after an upgrade it is
    // the replacement controller rather than the still-running executable.
    let controller_name = running_executable_file_name(controller);
    for name in names
        .iter()
        .filter(|name| Some(name.as_os_str()) != controller_name.as_deref())
    {
        candidates.push((directory.join(name), "beside the running executable"));
    }
    candidates.into_iter().find(|(path, _)| is_file(path))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum WorkerBinaryRequirement {
    PortableLinux,
    LocalHost,
}

impl WorkerBinarySourceSnapshot {
    pub(super) fn capture<F>(cache_root: &Path, resolve: F) -> Self
    where
        F: Fn(&str, WorkerBinaryRequirement) -> Result<WorkerBinaryAvailability>,
    {
        let mut entries = HashMap::new();
        let mut local_cache = HashMap::<PathBuf, PathBuf>::new();
        let architectures = [
            (std::env::consts::ARCH, WorkerBinaryRequirement::LocalHost),
            ("x86_64", WorkerBinaryRequirement::PortableLinux),
            ("aarch64", WorkerBinaryRequirement::PortableLinux),
        ];

        for (arch, requirement) in architectures {
            let pinned = match resolve(arch, requirement) {
                Ok(WorkerBinaryAvailability::Local { path, source }) => {
                    match local_cache.get(&path).cloned().map(Ok).unwrap_or_else(|| {
                        copy_worker_source_to_cache(&path, cache_root).inspect(|cached| {
                            local_cache.insert(path.clone(), cached.clone());
                        })
                    }) {
                        Ok(cached) => Ok(WorkerBinaryAvailability::Local {
                            path: cached,
                            source,
                        }),
                        Err(error) => {
                            let error = format!(
                                "pin worker source {} for {arch} ({requirement:?}): {error:#}",
                                path.display()
                            );
                            tracing::warn!(arch, requirement = ?requirement, error = %error);
                            Err(error)
                        }
                    }
                }
                Ok(WorkerBinaryAvailability::Remote {
                    url,
                    sha256,
                    triple,
                }) => Ok(WorkerBinaryAvailability::Remote {
                    url,
                    sha256,
                    triple,
                }),
                Err(error) => {
                    let error = format!("{error:#}");
                    tracing::debug!(
                        arch,
                        requirement = ?requirement,
                        error = %error,
                        "worker source was unavailable when the daemon started"
                    );
                    Err(error)
                }
            };
            entries.insert((arch.to_owned(), requirement), pinned);
        }

        Self { entries }
    }

    pub(super) fn resolve(
        &self,
        arch: &str,
        requirement: WorkerBinaryRequirement,
    ) -> Result<WorkerBinaryAvailability> {
        let Some(source) = self.entries.get(&(arch.to_owned(), requirement)) else {
            bail!(
                "worker source for {arch} ({requirement:?}) was not captured when the daemon started"
            );
        };
        match source {
            Ok(availability) => Ok(availability.clone()),
            Err(error) => bail!(
                "worker source for {arch} ({requirement:?}) was unavailable when the daemon started; install it and restart the daemon to retry: {error}"
            ),
        }
    }
}

/// Capture the worker sources used by this daemon before its asynchronous
/// managers start. Missing sources are retained as per-architecture errors so
/// an unused architecture does not prevent daemon startup.
pub fn pin_worker_binary_sources() -> Result<()> {
    if PINNED_WORKER_BINARY_SOURCES.get().is_some() {
        return Ok(());
    }
    let current = std::env::current_exe().context("resolve Mjolnir controller binary")?;
    let cache_root = data_dir().join("workers").join("pinned");
    let started = std::time::Instant::now();
    let snapshot = WorkerBinarySourceSnapshot::capture(&cache_root, |arch, requirement| {
        worker_binary_prerequisite_for_current(arch, requirement, &current, &|path| path.is_file())
    });
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis(),
        "worker sources pinned"
    );
    // The daemon boot path calls this once. If a second caller races it, keep
    // the first complete snapshot and never replace paths it may already use.
    let _ = PINNED_WORKER_BINARY_SOURCES.set(snapshot);
    Ok(())
}

pub(super) fn copy_worker_source_to_cache(source: &Path, cache_root: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(cache_root)
        .with_context(|| format!("create pinned worker cache {}", cache_root.display()))?;
    let mut input =
        File::open(source).with_context(|| format!("open worker source {}", source.display()))?;
    let metadata = input
        .metadata()
        .with_context(|| format!("stat worker source {}", source.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(cache_root)
        .with_context(|| format!("create pinned worker staging file {}", cache_root.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .with_context(|| format!("read worker source {}", source.display()))?;
        if count == 0 {
            break;
        }
        temporary
            .write_all(&buffer[..count])
            .with_context(|| format!("copy worker source {}", source.display()))?;
        digest.update(&buffer[..count]);
    }
    temporary
        .as_file_mut()
        .sync_all()
        .with_context(|| format!("flush pinned worker source {}", source.display()))?;
    std::fs::set_permissions(temporary.path(), metadata.permissions())
        .with_context(|| format!("preserve permissions for {}", source.display()))?;
    let digest = lower_hex(digest.finalize());
    publish_cached_worker(temporary, cache_root, &digest)
}

/// Publish one immutable cache artifact. persist_noclobber makes the final
/// publication atomic and never replaces an artifact another daemon may have
/// already captured.
pub(super) fn publish_cached_worker(
    temporary: tempfile::NamedTempFile,
    cache_root: &Path,
    digest: &str,
) -> Result<PathBuf> {
    let directory = cache_root.join(digest);
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create pinned worker cache {}", directory.display()))?;
    let destination = directory.join("hel");
    if destination.is_file() {
        return Ok(destination);
    }
    match temporary.persist_noclobber(&destination) {
        Ok(_) => {
            #[cfg(unix)]
            File::open(&directory)
                .and_then(|directory| directory.sync_all())
                .with_context(|| format!("flush pinned worker cache {}", directory.display()))?;
            Ok(destination)
        }
        Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {
            if destination.is_file() {
                Ok(destination)
            } else {
                Err(error.error).with_context(|| {
                    format!("publish pinned worker artifact {}", destination.display())
                })
            }
        }
        Err(error) => Err(error.error)
            .with_context(|| format!("publish pinned worker artifact {}", destination.display())),
    }
}

/// Find a worker source without downloading it.
///
/// Container provisioning resolves this after discovering the target
/// architecture. Doctor uses the same lookup with the selected container's
/// expected architecture, so it can recommend a fix without creating a
/// container or making a network request.
pub fn worker_binary_prerequisite_for_arch(arch: &str) -> Result<WorkerBinaryAvailability> {
    worker_binary_for_arch(arch, WorkerBinaryRequirement::PortableLinux)
}

/// The worker a `local-bare` session on this host would use.
///
/// A local session runs on the controller's own machine, so it may use the
/// native worker rather than the portable Linux one. `mj doctor` reports on it
/// separately for that reason: rebuilding only the portable worker leaves a
/// local session on old code, and the other way round.
pub fn native_worker_binary_prerequisite() -> Result<WorkerBinaryAvailability> {
    worker_binary_for_arch(std::env::consts::ARCH, WorkerBinaryRequirement::LocalHost)
}

pub(super) fn worker_binary_for_arch(
    arch: &str,
    requirement: WorkerBinaryRequirement,
) -> Result<WorkerBinaryAvailability> {
    if let Some(snapshot) = PINNED_WORKER_BINARY_SOURCES.get() {
        return snapshot.resolve(arch, requirement);
    }
    let current = std::env::current_exe().context("resolve Mjolnir controller binary")?;
    worker_binary_prerequisite_for_current(arch, requirement, &current, &|path| path.is_file())
}

/// The lookup itself, with the controller's own path and the file probe passed
/// in so both can be exercised without the machine they describe.
pub(super) fn worker_binary_prerequisite_for_current(
    arch: &str,
    requirement: WorkerBinaryRequirement,
    current: &Path,
    is_file: &dyn Fn(&Path) -> bool,
) -> Result<WorkerBinaryAvailability> {
    let triple = format!("{arch}-unknown-linux-musl");
    if let Some(path) = mj_core::config::env_override_os("WORKER_BINARY").map(PathBuf::from) {
        if !is_file(&path) {
            bail!("MJ_WORKER_BINARY is not a file: {}", path.display());
        }
        return Ok(WorkerBinaryAvailability::Local {
            path,
            source: "MJ_WORKER_BINARY".into(),
        });
    }
    // A rebuilt or renamed checkout leaves a running controller pointing at a
    // path that no longer holds a binary. Every lookup derived from that path
    // is meaningless, so remember the fact and skip those lookups.
    let controller_replaced = !is_file(current);
    let mut candidates = Vec::new();
    if let Some(directory) = mj_core::config::env_override_os("WORKER_DIR").map(PathBuf::from) {
        candidates.push((
            packaged_worker_binary_path(&directory, &triple),
            "MJ_WORKER_DIR",
        ));
        candidates.push((directory.join(&triple).join("hel"), "MJ_WORKER_DIR"));
    }
    if let Some((path, source)) = candidates.into_iter().find(|(path, _)| is_file(path)) {
        return Ok(WorkerBinaryAvailability::Local {
            path,
            source: source.into(),
        });
    }
    if requirement == WorkerBinaryRequirement::LocalHost
        && let Some((path, source)) = select_native_worker(current, is_file)
    {
        return Ok(WorkerBinaryAvailability::Local {
            path,
            source: source.into(),
        });
    }
    if !controller_replaced
        && let Some((path, source)) = select_sibling_worker(current, &triple, is_file)
    {
        return Ok(WorkerBinaryAvailability::Local {
            path,
            source: source.into(),
        });
    }
    if let Some(template) = mj_core::config::env_override("WORKER_URL") {
        let expected = mj_core::config::env_override("WORKER_SHA256")
            .context("MJ_WORKER_URL requires MJ_WORKER_SHA256")?;
        validate_worker_sha256(&expected)?;
        return Ok(WorkerBinaryAvailability::Remote {
            url: template.replace("{target}", &triple),
            sha256: expected,
            triple,
        });
    }
    // Telling someone to install a worker beside a binary that is no longer
    // there sends them looking in the wrong place.
    ensure!(
        !controller_replaced,
        "the running mj binary was replaced or removed on disk ({}); restart the Mjolnir daemon so it runs the current build, then retry",
        display_path(current)
    );
    bail!(
        "no Linux worker for {triple}; install mj-worker-{triple} beside mj, set MJ_WORKER_DIR/MJ_WORKER_BINARY, or configure MJ_WORKER_URL and MJ_WORKER_SHA256"
    )
}
