//! One native-runtime mechanism on every install: official pinned downloads,
//! verified and atomically cached by the same preparation owner as the model.
use super::{
    PreparationFailure, StudentFailureKind,
    assets::{self, Artifact, Staging},
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::OnceLock,
};
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
struct Manifest {
    version: String,
    platforms: BTreeMap<String, Platform>,
}
#[derive(Clone, Deserialize)]
struct Platform {
    library: String,
    archives: Vec<Archive>,
}
#[derive(Clone, Deserialize)]
struct Archive {
    url: String,
    bytes: u64,
    sha256: String,
    files: Vec<File>,
}
#[derive(Clone, Deserialize)]
struct File {
    source: String,
    name: String,
    bytes: u64,
    sha256: String,
}
fn manifest() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        serde_json::from_str(include_str!("runtime.json")).expect("checked-in runtime manifest")
    })
}
fn target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        (os, arch) => Err(PreparationFailure {
            kind: StudentFailureKind::Load,
            cause: format!("local GitHub scoring is unavailable on {os}/{arch}"),
            retry_after: None,
            notice: Some(super::StudentNotice::PlatformUnsupported),
        }
        .into()),
    }
}
pub(super) fn validate_platform() -> Result<()> {
    target().map(|_| ())
}
fn files(platform: &Platform) -> Vec<Artifact> {
    platform
        .archives
        .iter()
        .flat_map(|a| &a.files)
        .map(|f| Artifact {
            name: f.name.clone(),
            bytes: f.bytes,
            sha256: f.sha256.clone(),
        })
        .collect()
}
struct Selection {
    target: String,
    missing: Vec<String>,
    cuda: bool,
    directml: bool,
}
static SELECTION: OnceLock<Selection> = OnceLock::new();
pub(super) fn directml_selected() -> bool {
    SELECTION.get().is_some_and(|s| s.directml)
}
pub(super) fn gpu_requires_restart() -> bool {
    SELECTION.get().is_some_and(|s| {
        (cfg!(windows) && !s.directml)
            || (cfg!(all(target_os = "linux", target_arch = "x86_64")) && !s.cuda)
    })
}
pub(super) fn cuda_selected() -> bool {
    SELECTION.get().is_some_and(|s| s.cuda)
}
fn select(use_gpu: bool) -> Result<Selection> {
    let target = target()?.to_string();
    // Only Linux and Windows probes mutate the native-library selection.
    #[allow(unused_mut)]
    let mut selected = Selection {
        target,
        missing: Vec::new(),
        cuda: false,
        directml: false,
    };
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if use_gpu {
        // These exact non-system dependencies come from ldd of the pinned
        // ORT1.23.0 CUDA provider (not guessed CUDA package names).
        const SONAMES: &[&str] = &[
            "libcublasLt.so.12",
            "libcublas.so.12",
            "libcurand.so.10",
            "libcufft.so.11",
            "libcudart.so.12",
            "libcudnn.so.9",
        ];
        // SAFETY: probes use only documented system loader sonames and retain
        // handles until all dependencies have been checked. No symbols called.
        if let Ok(driver) = unsafe { libloading::Library::new("libcuda.so.1") } {
            let mut libraries = vec![driver];
            for name in SONAMES {
                // SAFETY: same system loader probe as above.
                match unsafe { libloading::Library::new(*name) } {
                    Ok(library) => libraries.push(library),
                    Err(_) => selected.missing.push((*name).into()),
                }
            }
            if selected.missing.is_empty() {
                selected.target.push_str("-cuda");
                selected.cuda = true;
            }
        }
    }
    #[cfg(windows)]
    if use_gpu && directx12_available() {
        selected.target.push_str("-directml");
        selected.directml = true;
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    let _ = use_gpu;
    Ok(selected)
}
#[cfg(windows)]
fn directx12_available() -> bool {
    use libloading::os::windows::{LOAD_LIBRARY_SEARCH_SYSTEM32, Library};
    #[repr(C)]
    struct Guid {
        a: u32,
        b: u16,
        c: u16,
        d: [u8; 8],
    }
    const DEVICE: Guid = Guid {
        a: 0x189819f1,
        b: 0x1db6,
        c: 0x4b57,
        d: [0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7],
    };
    type Create = unsafe extern "system" fn(
        *mut std::ffi::c_void,
        u32,
        *const Guid,
        *mut *mut std::ffi::c_void,
    ) -> i32;
    // SAFETY: system32-only library, documented D3D12CreateDevice ABI/IID.
    // Null adapter selects the default DXGI adapter; null output validates
    // support without creating a device (S_FALSE on success). Feature level
    // D3D_FEATURE_LEVEL_11_0 is DirectML's documented minimum for DX12 devices.
    unsafe {
        let Ok(library) = Library::load_with_flags("d3d12.dll", LOAD_LIBRARY_SEARCH_SYSTEM32)
        else {
            return false;
        };
        let Ok(create) = library.get::<Create>(b"D3D12CreateDevice\0") else {
            return false;
        };
        create(std::ptr::null_mut(), 0xb000, &DEVICE, std::ptr::null_mut()) >= 0
    }
}
pub(super) async fn prepare_selected(
    root: PathBuf,
    use_gpu: bool,
    notice: &super::Notice,
    client: &reqwest::Client,
    cancellation: &CancellationToken,
) -> Result<PathBuf> {
    // The owner chooses once before loading ORT. A later preference change
    // cannot replace a process-global native library underneath live sessions.
    if SELECTION.get().is_none() {
        let choice = tokio::task::spawn_blocking(move || select(use_gpu)).await??;
        if SELECTION.set(choice).is_ok()
            && let Some(choice) = SELECTION.get()
            && !choice.missing.is_empty()
        {
            notice(super::StudentNotice::CudaLibrariesMissing {
                missing: choice.missing.clone(),
            });
        }
    }
    let target = &SELECTION.get().context("runtime selection missing")?.target;
    let platform = manifest()
        .platforms
        .get(target)
        .context("unsupported runtime target")?
        .clone();
    prepare_platform(root, target, platform, client, cancellation).await
}
async fn prepare_platform(
    root: PathBuf,
    target: &str,
    platform: Platform,
    client: &reqwest::Client,
    cancellation: &CancellationToken,
) -> Result<PathBuf> {
    // Include the entire pin inventory in the key: DirectML's independent
    // revision can change without overwriting a DLL used by another daemon.
    let key = format!(
        "{}-{}-{}",
        manifest().version,
        target,
        mj_core::hex::lower_hex(Sha256::digest(include_bytes!("runtime.json")))
    );
    let destination = root.join(key);
    let check = destination.clone();
    let expected = files(&platform);
    if tokio::task::spawn_blocking(move || assets::verified(&check, &expected))
        .await?
        .is_ok()
    {
        return tokio::task::spawn_blocking(move || {
            destination
                .join(&platform.library)
                .canonicalize()
                .context("resolve verified runtime path")
        })
        .await?;
    }
    let staging = Staging::new(root).await?;
    futures::future::try_join_all(platform.archives.iter().enumerate().map(|(i, archive)| {
        let path = staging.path().join(format!("download-{i}"));
        async move { assets::download(&archive.url, &path, client, cancellation).await }
    }))
    .await?;
    tokio::task::spawn_blocking(move || -> Result<PathBuf> {
        for (i, archive) in platform.archives.iter().enumerate() {
            let path = staging.path().join(format!("download-{i}"));
            assets::verify_file(&path, archive.bytes, &archive.sha256).map_err(|e| {
                PreparationFailure::error(StudentFailureKind::Verification, e, None)
            })?;
            extract(&path, archive, staging.path()).map_err(|e| {
                PreparationFailure::error(StudentFailureKind::Verification, e, None)
            })?;
            std::fs::remove_file(path)?;
        }
        let library = platform.library.clone();
        let directory = staging.publish(destination, &files(&platform))?;
        directory
            .join(library)
            .canonicalize()
            .context("resolve verified runtime path")
    })
    .await?
}
fn extract(path: &Path, archive: &Archive, directory: &Path) -> Result<()> {
    use std::io::Write;
    let write = |file: &File, reader: &mut dyn std::io::Read| -> Result<()> {
        let mut output = std::fs::File::create_new(directory.join(&file.name))?;
        std::io::copy(reader, &mut output)?;
        output.flush()?;
        output.sync_all()?;
        Ok(())
    };
    if archive.url.ends_with(".nupkg") || archive.url.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(path)?)?;
        for file in &archive.files {
            let mut entry = zip.by_name(&file.source)?;
            ensure!(
                entry.is_file() && !entry.is_symlink(),
                "runtime entry is not a regular file: {}",
                file.source
            );
            write(file, &mut entry)?;
        }
    } else {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(path)?));
        let mut extracted = 0;
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            if let Some(file) = archive.files.iter().find(|f| path == Path::new(&f.source)) {
                ensure!(
                    entry.header().entry_type().is_file(),
                    "runtime entry is not a regular file: {}",
                    file.source
                );
                write(file, &mut entry)?;
                extracted += 1;
            }
        }
        ensure!(
            extracted == archive.files.len(),
            "runtime archive is missing pinned files"
        );
    }
    Ok(())
}
/// Only called after every native payload and its notices have been verified.
pub(super) fn environment(path: &Path) -> Result<ort::environment::Environment> {
    #[cfg(windows)]
    {
        use libloading::os::windows::{LOAD_LIBRARY_SEARCH_SYSTEM32, Library};
        let mut missing = Vec::new();
        for name in [
            "VCRUNTIME140.dll",
            "VCRUNTIME140_1.dll",
            "MSVCP140.dll",
            "MSVCP140_1.dll",
        ] {
            // SAFETY: inspect only the Microsoft runtime DLLs in System32;
            // documented loader API, no symbols invoked.
            if unsafe { Library::load_with_flags(name, LOAD_LIBRARY_SEARCH_SYSTEM32) }.is_err() {
                missing.push(name.to_string());
            }
        }
        if !missing.is_empty() {
            return Err(PreparationFailure {
                kind: StudentFailureKind::WindowsRuntimeMissing,
                cause: format!(
                    "Microsoft Visual C++ Redistributable x64 DLLs missing: {}",
                    missing.join(", ")
                ),
                retry_after: None,
                notice: Some(super::StudentNotice::WindowsRuntimeMissing { missing }),
            }
            .into());
        }
    }
    // ort permits exactly one environment per process, even after its last
    // session drops. Keep that small API owner alive across model retries.
    static ENVIRONMENT: std::sync::Mutex<Option<ort::environment::Environment>> =
        std::sync::Mutex::new(None);
    let mut environment = ENVIRONMENT.lock().expect("runtime environment poisoned");
    if let Some(environment) = environment.as_ref() {
        return Ok(environment.clone());
    }
    #[cfg(windows)]
    {
        use libloading::os::windows::{
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, Library,
        };
        static LIBRARY: std::sync::Mutex<Vec<Library>> = std::sync::Mutex::new(Vec::new());
        let mut library = LIBRARY.lock().expect("runtime loader poisoned");
        if library.is_empty() {
            // SAFETY: the owner verified the main DLL and every bundled
            // dependency. Absolute DLL-directory + System32 search excludes
            // PATH/current-directory copies, and the handle lives for the
            // process-global ORT API's entire lifetime.
            let mut names: Vec<_> = manifest()
                .platforms
                .get(&SELECTION.get().context("runtime selection missing")?.target)
                .context("unsupported runtime target")?
                .archives
                .iter()
                .flat_map(|a| &a.files)
                .filter(|f| f.name.ends_with(".dll"))
                .map(|f| f.name.clone())
                .collect();
            names.sort_by_key(|name| name == "onnxruntime.dll");
            let root = path.parent().context("runtime has no directory")?;
            let mut loaded = Vec::new();
            for name in names {
                // SAFETY: same verified cache and restricted search as above;
                // preload delayed dependencies by absolute path as well.
                loaded.push(unsafe {
                    Library::load_with_flags(
                        root.join(name),
                        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
                    )
                }?);
            }
            *library = loaded;
        }
    }
    let loaded = ort::init_from(path)?.build()?;
    *environment = Some(loaded.clone());
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "downloads the official pinned native archives for all supported platforms"]
    async fn native_archives_publish_only_verified_libraries_and_notices() {
        crate::server::install_rustls_crypto_provider();
        let root = tempfile::Builder::new()
            .prefix("github-student-native-integration-")
            .tempdir()
            .unwrap();
        let client = reqwest::Client::new();
        let cancellation = CancellationToken::new();
        for (target, platform) in &manifest().platforms {
            let library = prepare_platform(
                root.path().into(),
                target,
                platform.clone(),
                &client,
                &cancellation,
            )
            .await
            .unwrap_or_else(|e| panic!("{target}: {e:#}"));
            let directory = library.parent().unwrap();
            assets::verified(directory, &files(platform)).unwrap();
            let actual: std::collections::BTreeSet<_> = std::fs::read_dir(directory)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            let expected: std::collections::BTreeSet<_> = files(platform)
                .into_iter()
                .map(|f| std::ffi::OsString::from(f.name))
                .collect();
            assert_eq!(
                actual, expected,
                "{target}: downloaded archives and unselected payloads must not remain in the published cache"
            );
        }
    }
}
