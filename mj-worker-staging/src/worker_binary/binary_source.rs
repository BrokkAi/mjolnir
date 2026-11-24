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
pub struct WorkerBinarySourceSnapshot {
    cache_root: PathBuf,
    pub entries: HashMap<
        (String, WorkerBinaryRequirement),
        std::result::Result<WorkerBinaryAvailability, String>,
    >,
}

pub static PINNED_WORKER_BINARY_SOURCES: OnceLock<WorkerBinarySourceSnapshot> = OnceLock::new();

pub fn packaged_worker_binary_path(directory: &Path, triple: &str) -> PathBuf {
    directory.join(format!("mj-worker-{triple}"))
}

/// Linux exposes an unlinked running executable through `/proc` with a
/// ` (deleted)` suffix. `current_exe` preserves that suffix, but it is not
/// part of the executable's real file name and must not leak into sibling
/// lookup after `cargo` or a package upgrade replaces the controller.
pub fn running_executable_file_name(controller: &Path) -> Option<std::ffi::OsString> {
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
pub fn worker_sibling_names(controller: &Path) -> Vec<std::ffi::OsString> {
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
pub fn select_native_worker(
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
pub fn select_sibling_worker(
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
    if triple.ends_with("-apple-darwin") {
        candidates.push((
            packaged_worker_binary_path(directory, "universal-apple-darwin"),
            "universal Darwin worker beside mj",
        ));
    }
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
        .filter(|_| !triple.ends_with("-apple-darwin"))
        .filter(|name| Some(name.as_os_str()) != controller_name.as_deref())
    {
        candidates.push((directory.join(name), "beside the running executable"));
    }
    candidates.into_iter().find(|(path, _)| is_file(path))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkerBinaryRequirement {
    PortableLinux,
    Darwin,
    LocalHost,
}

impl WorkerBinaryRequirement {
    pub fn for_os(os: targets::TargetOs) -> Self {
        match os {
            targets::TargetOs::Linux => Self::PortableLinux,
            targets::TargetOs::Darwin => Self::Darwin,
        }
    }

    pub fn triple(self, arch: &str) -> String {
        match self {
            Self::Darwin => format!("{arch}-apple-darwin"),
            Self::LocalHost if cfg!(target_os = "macos") => format!("{arch}-apple-darwin"),
            _ => format!("{arch}-unknown-linux-musl"),
        }
    }
}

impl WorkerBinarySourceSnapshot {
    /// Resolve and pin every source the daemon may use.
    ///
    /// Resolution runs per source on its own thread, and each distinct local
    /// file is pinned on its own thread, so one slow disk read does not queue
    /// the others behind it. `resolve` may therefore be called concurrently.
    pub fn capture<F>(cache_root: &Path, resolve: F) -> Self
    where
        F: Fn(&str, WorkerBinaryRequirement) -> Result<WorkerBinaryAvailability> + Sync,
    {
        let architectures = [
            (std::env::consts::ARCH, WorkerBinaryRequirement::LocalHost),
            ("x86_64", WorkerBinaryRequirement::PortableLinux),
            ("aarch64", WorkerBinaryRequirement::PortableLinux),
            ("x86_64", WorkerBinaryRequirement::Darwin),
            ("aarch64", WorkerBinaryRequirement::Darwin),
        ];
        let resolved: Vec<_> = std::thread::scope(|scope| {
            let resolve = &resolve;
            let handles: Vec<_> = architectures
                .iter()
                .map(|&(arch, requirement)| {
                    scope.spawn(move || {
                        let started = Instant::now();
                        (resolve(arch, requirement), started.elapsed())
                    })
                })
                .collect();
            handles
                .into_iter()
                .zip(architectures)
                .map(|(handle, (arch, requirement))| {
                    handle.join().unwrap_or_else(|_| {
                        (
                            Err(anyhow::anyhow!(
                                "resolving the worker source for {arch} ({requirement:?}) panicked"
                            )),
                            Duration::ZERO,
                        )
                    })
                })
                .collect()
        });

        let mut unique_paths: Vec<&Path> = Vec::new();
        for (result, _) in &resolved {
            if let Ok(WorkerBinaryAvailability::Local { path, .. }) = result
                && !unique_paths.contains(&path.as_path())
            {
                unique_paths.push(path);
            }
        }
        let pinned_paths: HashMap<PathBuf, (Result<PathBuf>, Duration)> =
            std::thread::scope(|scope| {
                let handles: Vec<_> = unique_paths
                    .iter()
                    .map(|&path| {
                        scope.spawn(move || {
                            let started = Instant::now();
                            (
                                copy_worker_source_to_cache(path, cache_root),
                                started.elapsed(),
                            )
                        })
                    })
                    .collect();
                unique_paths
                    .iter()
                    .zip(handles)
                    .map(|(&path, handle)| {
                        let outcome = handle.join().unwrap_or_else(|_| {
                            (
                                Err(anyhow::anyhow!("pinning {} panicked", path.display())),
                                Duration::ZERO,
                            )
                        });
                        (path.to_path_buf(), outcome)
                    })
                    .collect()
            });

        let mut entries = HashMap::new();
        for ((arch, requirement), (result, resolve_elapsed)) in
            architectures.into_iter().zip(resolved)
        {
            let pinned = match result {
                Ok(WorkerBinaryAvailability::Local { path, source }) => {
                    let (outcome, pin_elapsed) = &pinned_paths[&path];
                    match outcome {
                        Ok(cached) => {
                            record_build_pin(
                                cache_root,
                                worker_build_id(),
                                arch,
                                requirement,
                                cached,
                            );
                            tracing::info!(
                                triple = requirement.triple(arch),
                                source = source.as_str(),
                                path = %path.display(),
                                pinned = %cached.display(),
                                build = worker_build_id(),
                                resolve_ms = resolve_elapsed.as_millis(),
                                pin_ms = pin_elapsed.as_millis(),
                                "worker source selected"
                            );
                            Ok(WorkerBinaryAvailability::Local {
                                path: cached.clone(),
                                source,
                            })
                        }
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
                }) => {
                    tracing::info!(
                        triple = triple.as_str(),
                        source = "MJ_WORKER_URL",
                        url = url.as_str(),
                        sha256 = sha256.as_str(),
                        build = worker_build_id(),
                        "worker source selected"
                    );
                    Ok(WorkerBinaryAvailability::Remote {
                        url,
                        sha256,
                        triple,
                    })
                }
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

        Self {
            cache_root: cache_root.to_path_buf(),
            entries,
        }
    }

    pub fn resolve(
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
            Ok(availability) => {
                if let WorkerBinaryAvailability::Local { path, .. } = availability {
                    verify_worker_build_indexed(&self.cache_root, path).inspect_err(|error| {
                        tracing::warn!(path = %path.display(), error = %error, "rejecting pinned worker source");
                    })?;
                }
                Ok(availability.clone())
            }
            Err(error) => bail!(
                "worker source for {arch} ({requirement:?}) was unavailable when the daemon started: {error}"
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
    let started = std::time::Instant::now();
    let snapshot = capture_worker_binary_sources()?;
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis(),
        "worker sources pinned"
    );
    // The daemon boot path calls this once. If a second caller races it, keep
    // the first complete snapshot and never replace paths it may already use.
    let _ = PINNED_WORKER_BINARY_SOURCES.set(snapshot);
    Ok(())
}

/// Verify and pin this build's worker sources into the shared cache without
/// keeping the snapshot, so the daemon this build starts next finds every
/// source already indexed.
///
/// A client replacing the daemon runs this while the old daemon drains: the
/// copying and hashing a new build needs then happens beside the handoff
/// instead of on the new daemon's startup path. The cache is content-addressed
/// and published atomically without replacing anything, so the old daemon's
/// pins are unaffected. Startup still captures the sources itself and stays
/// the only owner of the pinned snapshot.
pub fn warm_worker_binary_sources() -> Result<()> {
    capture_worker_binary_sources().map(drop)
}

fn capture_worker_binary_sources() -> Result<WorkerBinarySourceSnapshot> {
    let current = std::env::current_exe().context("resolve Mjolnir controller binary")?;
    let cache_root = data_dir().join("workers").join("pinned");
    Ok(WorkerBinarySourceSnapshot::capture(
        &cache_root,
        |arch, requirement| {
            worker_binary_prerequisite_with_cache(
                arch,
                requirement,
                &current,
                &|path| path.is_file(),
                &|path| verify_worker_build_indexed(&cache_root, path),
                Some(&cache_root),
            )
        },
    ))
}

fn build_pin_entry(
    cache_root: &Path,
    build_id: &str,
    arch: &str,
    requirement: WorkerBinaryRequirement,
) -> PathBuf {
    let mut key = Sha256::new();
    key.update(b"by-build\0");
    key.update(build_id.as_bytes());
    key.update([0]);
    key.update(arch.as_bytes());
    key.update([0]);
    let requirement_key = match requirement {
        WorkerBinaryRequirement::LocalHost => {
            key.update(b"local-host\0");
            std::env::consts::OS
        }
        WorkerBinaryRequirement::PortableLinux => "portable-linux",
        WorkerBinaryRequirement::Darwin => "darwin",
    };
    key.update(requirement_key.as_bytes());
    cache_root.join("index").join(lower_hex(key.finalize()))
}

fn record_build_pin(
    cache_root: &Path,
    build_id: &str,
    arch: &str,
    requirement: WorkerBinaryRequirement,
    pinned: &Path,
) {
    let write = || -> Result<()> {
        let digest = pinned
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .context("pinned worker has no digest directory")?;
        let entry = build_pin_entry(cache_root, build_id, arch, requirement);
        let directory = entry.parent().expect("index entry parent");
        std::fs::create_dir_all(directory)?;
        let mut staged = tempfile::NamedTempFile::new_in(directory)?;
        staged.write_all(digest.as_bytes())?;
        staged.persist(entry).map_err(|error| error.error)?;
        Ok(())
    };
    if let Err(error) = write() {
        tracing::warn!(%error, "could not record build-target worker pin");
    }
}

pub(super) fn lookup_build_pinned_worker(
    cache_root: &Path,
    build_id: &str,
    arch: &str,
    requirement: WorkerBinaryRequirement,
    is_file: &dyn Fn(&Path) -> bool,
    verify: &dyn Fn(&Path) -> Result<()>,
) -> Option<PathBuf> {
    let lookup = || -> Result<Option<PathBuf>> {
        let digest =
            match std::fs::read_to_string(build_pin_entry(cache_root, build_id, arch, requirement))
            {
                Ok(digest) => digest,
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
        let digest = digest.trim();
        ensure!(
            digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid build-target worker digest"
        );
        let pinned = cache_root.join(digest).join("hel");
        ensure!(
            is_file(&pinned),
            "pinned worker is missing: {}",
            pinned.display()
        );
        verify(&pinned)?;
        ensure!(
            mj_core::worker_launch::worker_executable_digest(&pinned)? == digest,
            "pinned worker checksum mismatch: {}",
            pinned.display()
        );
        Ok(Some(pinned))
    };
    match lookup() {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(%error, build_id, arch, ?requirement, "could not use build-target worker pin");
            None
        }
    }
}

/// Cached record of a source file this daemon already verified and pinned.
///
/// The key covers the build id and the source's path, size, mtime, inode and
/// ctime, so a rebuilt or replaced file misses. The value is the digest of the
/// pinned copy. The index only saves work: deleting it makes the next start
/// read, verify and hash every source again.
fn index_entry_path(cache_root: &Path, source: &Path) -> Option<PathBuf> {
    let metadata = std::fs::metadata(source).ok()?;
    let mut key = Sha256::new();
    key.update(worker_build_id().as_bytes());
    key.update([0]);
    key.update(source.as_os_str().as_encoded_bytes());
    key.update(metadata.len().to_le_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        for value in [
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        ] {
            key.update(value.to_le_bytes());
        }
        key.update(metadata.ino().to_le_bytes());
    }
    #[cfg(not(unix))]
    {
        let modified = metadata.modified().ok()?;
        key.update(format!("{modified:?}").as_bytes());
    }
    Some(cache_root.join("index").join(lower_hex(key.finalize())))
}

/// The pinned copy of `source` if the index says this exact file was already
/// pinned and the copy is still present with the same length.
pub fn indexed_pinned_worker(cache_root: &Path, source: &Path) -> Option<PathBuf> {
    let entry = index_entry_path(cache_root, source)?;
    let digest = std::fs::read_to_string(entry).ok()?;
    let digest = digest.trim();
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let pinned = cache_root.join(digest).join("hel");
    let source_len = std::fs::metadata(source).ok()?.len();
    (std::fs::metadata(&pinned).ok()?.len() == source_len).then_some(pinned)
}

fn record_indexed_pin(cache_root: &Path, source: &Path, digest: &str) {
    let Some(entry) = index_entry_path(cache_root, source) else {
        return;
    };
    // Best effort: a missing entry only costs a re-hash on the next start.
    let write = || -> std::io::Result<()> {
        let directory = entry.parent().expect("index entries have a parent");
        std::fs::create_dir_all(directory)?;
        let mut staged = tempfile::NamedTempFile::new_in(directory)?;
        staged.write_all(digest.as_bytes())?;
        staged
            .persist(&entry)
            .map(drop)
            .map_err(|error| error.error)
    };
    if let Err(error) = write() {
        tracing::debug!(source = %source.display(), %error, "could not index a pinned worker");
    }
}

/// `verify_worker_build`, skipped for a file the index already knows: the same
/// path, size, mtime, inode and ctime were verified against this build and
/// pinned before. Anything else is read and checked in full.
pub fn verify_worker_build_indexed(cache_root: &Path, path: &Path) -> Result<()> {
    if indexed_pinned_worker(cache_root, path).is_some() {
        return Ok(());
    }
    verify_worker_build(path)
}

pub fn copy_worker_source_to_cache(source: &Path, cache_root: &Path) -> Result<PathBuf> {
    if let Some(pinned) = indexed_pinned_worker(cache_root, source) {
        return Ok(pinned);
    }
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
    let pinned = publish_cached_worker(temporary, cache_root, &digest)?;
    record_indexed_pin(cache_root, source, &digest);
    // The daemon resolves this verified copy repeatedly. Index its own file
    // identity too, so an unchanged pin needs no executable scan while a
    // removed or modified copy still misses and is verified again.
    record_indexed_pin(cache_root, &pinned, &digest);
    Ok(pinned)
}

/// Publish one immutable cache artifact. persist_noclobber makes the final
/// publication atomic and never replaces an artifact another daemon may have
/// already captured.
pub fn publish_cached_worker(
    temporary: tempfile::NamedTempFile,
    cache_root: &Path,
    digest: &str,
) -> Result<PathBuf> {
    mj_core::worker_build::verify_unpublished_worker_build(temporary.path())?;
    let directory = cache_root.join(digest);
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create pinned worker cache {}", directory.display()))?;
    let destination = directory.join("hel");
    if destination.is_file() {
        verify_cached_worker(&destination, digest)?;
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
                verify_cached_worker(&destination, digest)?;
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

fn verify_cached_worker(path: &Path, digest: &str) -> Result<()> {
    verify_worker_build(path)?;
    ensure!(
        mj_core::worker_launch::worker_executable_digest(path)? == digest,
        "content-addressed worker cache {} does not match {digest} checksum",
        path.display()
    );
    Ok(())
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

pub fn worker_binary_for_arch(
    arch: &str,
    requirement: WorkerBinaryRequirement,
) -> Result<WorkerBinaryAvailability> {
    if let Some(snapshot) = PINNED_WORKER_BINARY_SOURCES.get() {
        let pinned = snapshot.resolve(arch, requirement);
        if pinned_source_is_usable(&pinned, &|path| path.is_file()) {
            return pinned;
        }
        // Either nothing resolved when the daemon started, or the file the pin
        // named has been taken away since. Both used to fail every session on
        // this daemon until someone restarted it, which is #1068: a build
        // directory that a cache reaper removed took every later session with
        // it. Look again instead.
        return resolve_worker_source_again(arch, requirement, pinned.err());
    }
    let current = std::env::current_exe().context("resolve Mjolnir controller binary")?;
    let cache_root = data_dir().join("workers").join("pinned");
    worker_binary_prerequisite_with_cache(
        arch,
        requirement,
        &current,
        &|path| path.is_file(),
        &verify_worker_build,
        Some(&cache_root),
    )
}

/// Whether a pinned worker source can still be used as it stands.
///
/// A remote source is a URL and stays usable. A local one is a path, and a
/// path can stop being a file after the daemon pinned it: a build directory a
/// cache reaper removed is exactly #1068.
pub fn pinned_source_is_usable(
    pinned: &Result<WorkerBinaryAvailability>,
    is_file: &dyn Fn(&Path) -> bool,
) -> bool {
    match pinned {
        Ok(WorkerBinaryAvailability::Local { path, .. }) => is_file(path),
        Ok(WorkerBinaryAvailability::Remote { .. }) => true,
        Err(_) => false,
    }
}

/// Resolve a worker source now, after the pinned one turned out to be unusable.
///
/// A resolved local binary is copied into the daemon's own pinned cache, so
/// whatever removed the first one cannot remove this one too.
fn resolve_worker_source_again(
    arch: &str,
    requirement: WorkerBinaryRequirement,
    pinned_error: Option<anyhow::Error>,
) -> Result<WorkerBinaryAvailability> {
    let current = std::env::current_exe().context("resolve Mjolnir controller binary")?;
    let cache_root = data_dir().join("workers").join("pinned");
    let resolved = worker_binary_prerequisite_with_cache(
        arch,
        requirement,
        &current,
        &|path| path.is_file(),
        &verify_worker_build,
        Some(&cache_root),
    );
    match resolved {
        Ok(WorkerBinaryAvailability::Local { path, source }) => {
            let path = copy_worker_source_to_cache(&path, &cache_root)
                .context("pin the re-resolved worker source")?;
            record_build_pin(&cache_root, worker_build_id(), arch, requirement, &path);
            tracing::info!(
                arch,
                requirement = ?requirement,
                source = %source,
                "re-resolved a worker source the daemon could not pin at startup"
            );
            Ok(WorkerBinaryAvailability::Local { path, source })
        }
        Ok(remote) => Ok(remote),
        // Report what the daemon found at startup as well: it may name a
        // different, more useful absence than this attempt does.
        Err(error) => Err(match pinned_error {
            Some(pinned) => error.context(format!("{pinned:#}")),
            None => error,
        }),
    }
}

/// The lookup itself, with the controller's own path and the file probe passed
/// in so both can be exercised without the machine they describe.
pub fn worker_binary_prerequisite_for_current(
    arch: &str,
    requirement: WorkerBinaryRequirement,
    current: &Path,
    is_file: &dyn Fn(&Path) -> bool,
) -> Result<WorkerBinaryAvailability> {
    worker_binary_prerequisite_with_verifier(
        arch,
        requirement,
        current,
        is_file,
        &verify_worker_build,
    )
}

/// The lookup with the build-stamp check passed in. Startup pinning passes a
/// check that skips files it already verified; other callers use the
/// process-local executable memo.
pub fn worker_binary_prerequisite_with_verifier(
    arch: &str,
    requirement: WorkerBinaryRequirement,
    current: &Path,
    is_file: &dyn Fn(&Path) -> bool,
    verify: &dyn Fn(&Path) -> Result<()>,
) -> Result<WorkerBinaryAvailability> {
    worker_binary_prerequisite_with_cache(arch, requirement, current, is_file, verify, None)
}

pub(super) fn worker_binary_prerequisite_with_cache(
    arch: &str,
    requirement: WorkerBinaryRequirement,
    current: &Path,
    is_file: &dyn Fn(&Path) -> bool,
    verify: &dyn Fn(&Path) -> Result<()>,
    cache_root: Option<&Path>,
) -> Result<WorkerBinaryAvailability> {
    let triple = requirement.triple(arch);
    let rejected = std::cell::RefCell::new(Vec::new());
    let matches_build = |path: &Path| {
        if !is_file(path) {
            return false;
        }
        match verify(path) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "skipping incompatible worker source");
                rejected.borrow_mut().push(format!("{error:#}"));
                false
            }
        }
    };
    if let Some(path) = mj_core::config::env_override_os("WORKER_BINARY").map(PathBuf::from) {
        if !is_file(&path) {
            bail!("MJ_WORKER_BINARY is not a file: {}", path.display());
        }
        // An explicit override names the one worker to use. Installing a
        // different one instead would hide the mismatch.
        verify(&path).context(
            "MJ_WORKER_BINARY does not match the running mj; rebuild it from the same commit or unset MJ_WORKER_BINARY",
        )?;
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
        if triple.ends_with("-apple-darwin") {
            candidates.push((
                packaged_worker_binary_path(&directory, "universal-apple-darwin"),
                "MJ_WORKER_DIR",
            ));
        }
    }
    if let Some((path, source)) = candidates.into_iter().find(|(path, _)| matches_build(path)) {
        return Ok(WorkerBinaryAvailability::Local {
            path,
            source: source.into(),
        });
    }
    if requirement == WorkerBinaryRequirement::LocalHost
        && let Some((path, source)) = select_native_worker(current, matches_build)
    {
        return Ok(WorkerBinaryAvailability::Local {
            path,
            source: source.into(),
        });
    }
    if !controller_replaced
        && let Some((path, source)) = select_sibling_worker(current, &triple, matches_build)
    {
        return Ok(WorkerBinaryAvailability::Local {
            path,
            source: source.into(),
        });
    }
    if let Some(path) = cache_root.and_then(|root| {
        lookup_build_pinned_worker(root, worker_build_id(), arch, requirement, is_file, verify)
    }) {
        return Ok(WorkerBinaryAvailability::Local {
            path,
            source: "pinned worker cache".into(),
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
    let rejected = rejected.into_inner();
    ensure!(
        rejected.is_empty(),
        "no worker matching controller worker inputs {build} for {triple}; rejected sources:\n{}\nInstall the worker from the same mj release, or rebuild it from the current workspace inputs with `cargo build --target {triple} -p brokk-mj-worker --bin mj-worker` and set MJ_WORKER_BINARY to that file (local native: `cargo build -p brokk-mj-worker --bin mj-worker`).",
        rejected.join("\n"),
        build = worker_build_id()
    );
    // Telling someone to install a worker beside a binary that is no longer
    // there sends them looking in the wrong place.
    ensure!(
        !controller_replaced,
        "the running mj binary was replaced or removed on disk ({}); restart the Mjolnir daemon so it runs the current build, then retry",
        display_path(current)
    );
    bail!(
        "no worker for {triple}; install mj-worker-{triple} beside mj, set MJ_WORKER_DIR/MJ_WORKER_BINARY, or configure MJ_WORKER_URL and MJ_WORKER_SHA256"
    )
}

#[cfg(test)]
mod build_pin_tests {
    use super::*;

    #[test]
    fn build_pin_survives_install_replacement_and_rejects_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        let controller = directory.path().join("mj");
        let worker = directory.path().join("mj-worker");
        std::fs::write(&controller, b"controller").unwrap();
        let stamp = mj_core::worker_build::worker_build_stamp();
        let a = format!("worker A{stamp}");
        let b = format!("worker B{stamp}");
        std::fs::write(&worker, &a).unwrap();
        let arch = std::env::consts::ARCH;
        let requirement = WorkerBinaryRequirement::LocalHost;
        let snapshot = WorkerBinarySourceSnapshot::capture(&cache, |_, req| {
            ensure!(req == requirement, "unavailable target");
            Ok(WorkerBinaryAvailability::Local {
                path: worker.clone(),
                source: "installed".into(),
            })
        });
        let Ok(WorkerBinaryAvailability::Local { path: pinned, .. }) =
            &snapshot.entries[&(arch.to_owned(), requirement)]
        else {
            panic!("worker not pinned")
        };
        std::fs::write(&worker, &b).unwrap();
        let verify_a = |path: &Path| -> Result<()> {
            ensure!(std::fs::read(path)? == a.as_bytes(), "different build");
            Ok(())
        };
        let lookup = || {
            worker_binary_prerequisite_with_cache(
                arch,
                requirement,
                &controller,
                &|p| p.is_file(),
                &verify_a,
                Some(&cache),
            )
        };
        assert_eq!(
            lookup().unwrap(),
            WorkerBinaryAvailability::Local {
                path: pinned.clone(),
                source: "pinned worker cache".into()
            }
        );
        let fresh = WorkerBinarySourceSnapshot::capture(&cache, |candidate_arch, req| {
            worker_binary_prerequisite_with_cache(
                candidate_arch,
                req,
                &controller,
                &|p| p.is_file(),
                &verify_a,
                Some(&cache),
            )
        });
        assert_eq!(
            fresh.entries[&(arch.to_owned(), requirement)],
            Ok(WorkerBinaryAvailability::Local {
                path: pinned.clone(),
                source: "pinned worker cache".into()
            })
        );
        // A daemon with B's verifier prefers the installed B even while A is pinned.
        let verify_b = |path: &Path| -> Result<()> {
            ensure!(std::fs::read(path)? == b.as_bytes(), "different build");
            Ok(())
        };
        let newer = worker_binary_prerequisite_with_cache(
            arch,
            requirement,
            &controller,
            &|p| p.is_file(),
            &verify_b,
            Some(&cache),
        )
        .unwrap();
        assert!(matches!(newer, WorkerBinaryAvailability::Local { path, .. } if path == worker));
        assert!(
            lookup_build_pinned_worker(
                &cache,
                "different-build",
                arch,
                requirement,
                &|p| p.is_file(),
                &verify_a
            )
            .is_none()
        );
        assert!(
            lookup_build_pinned_worker(
                &cache,
                worker_build_id(),
                arch,
                WorkerBinaryRequirement::PortableLinux,
                &|p| p.is_file(),
                &verify_a
            )
            .is_none()
        );
        let portable_cache = directory.path().join("portable-cache");
        WorkerBinarySourceSnapshot::capture(&portable_cache, |_, req| {
            ensure!(
                req == WorkerBinaryRequirement::PortableLinux,
                "unavailable target"
            );
            Ok(WorkerBinaryAvailability::Local {
                path: worker.clone(),
                source: "installed portable".into(),
            })
        });
        assert!(
            lookup_build_pinned_worker(
                &portable_cache,
                worker_build_id(),
                arch,
                requirement,
                &|p| p.is_file(),
                &verify_b
            )
            .is_none()
        );
        assert!(
            lookup_build_pinned_worker(
                &portable_cache,
                worker_build_id(),
                arch,
                WorkerBinaryRequirement::PortableLinux,
                &|p| p.is_file(),
                &verify_b
            )
            .is_some()
        );
        let portable_pin = lookup_build_pinned_worker(
            &portable_cache,
            worker_build_id(),
            arch,
            WorkerBinaryRequirement::PortableLinux,
            &|p| p.is_file(),
            &verify_b,
        )
        .unwrap();
        std::fs::remove_file(&controller).unwrap();
        std::fs::write(&worker, &a).unwrap();
        assert_eq!(
            worker_binary_prerequisite_with_cache(
                arch,
                WorkerBinaryRequirement::PortableLinux,
                &controller,
                &|p| p.is_file(),
                &verify_b,
                Some(&portable_cache),
            )
            .unwrap(),
            WorkerBinaryAvailability::Local {
                path: portable_pin,
                source: "pinned worker cache".into()
            },
        );
        // A stamp verifier can still succeed after unrelated bytes are corrupted.
        std::fs::write(pinned, format!("corrupted{stamp}")).unwrap();
        assert!(
            lookup_build_pinned_worker(
                &cache,
                worker_build_id(),
                arch,
                requirement,
                &|p| p.is_file(),
                &verify_worker_build
            )
            .is_none()
        );
    }
}
