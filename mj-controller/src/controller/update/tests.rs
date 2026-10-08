use super::*;

fn fake_update_executable(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).expect("executable directory");
    std::fs::write(path, b"test executable").expect("executable file");
    path.to_path_buf()
}

fn update_command_text(command: &Command) -> String {
    std::iter::once(command.get_program().to_string_lossy().into_owned())
        .chain(
            command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned()),
        )
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn golden_install_method_detection_uses_cargo_and_package_manager_formats() {
    let root = tempfile::tempdir().expect("install roots");
    let version = env!("CARGO_PKG_VERSION");
    let source =
        format!("brokk-mjolnir {version} (registry+https://github.com/rust-lang/crates.io-index)");
    let voice_source = format!(
        "brokk-mj-voice-worker {version} (registry+https://github.com/rust-lang/crates.io-index)"
    );

    // Cargo's documented v1 install record shape, as written to .crates2.json.
    let cargo_json = root.path().join("cargo-json");
    std::fs::create_dir_all(cargo_json.join("bin")).expect("Cargo JSON bin directory");
    std::fs::write(
        cargo_json.join(".crates2.json"),
        serde_json::json!({
            "v1": 1,
            "installs": {
                (source.clone()): {"version": version, "bins": ["mj"], "features": []},
                (voice_source.clone()): {"version": version, "bins": ["mj-voice-worker"], "features": []}
            }
        })
        .to_string(),
    )
    .expect("Cargo JSON install record");
    let cargo_json_exe = fake_update_executable(&cargo_json.join("bin/mj"));

    // Cargo's compatibility v1 TOML install record, including a stale version
    // that must not be mistaken for the executable at the current version.
    let cargo_toml = root.path().join("cargo-toml");
    std::fs::create_dir_all(cargo_toml.join("bin")).expect("Cargo TOML bin directory");
    std::fs::write(
        cargo_toml.join(".crates.toml"),
        format!(
            "[v1]\n\"{source}\" = [\"mj\"]\n\"{voice_source}\" = [\"mj-voice-worker\"]\n\"brokk-mjolnir 0.0.1 (registry+https://github.com/rust-lang/crates.io-index)\" = [\"mj\"]\n"
        ),
    )
    .expect("Cargo TOML install record");
    let cargo_toml_exe = fake_update_executable(&cargo_toml.join("bin/mj"));

    let homebrew_exe =
        fake_update_executable(&root.path().join("homebrew/Cellar/mjolnir/2.4.0/bin/mj"));
    let npm_exe =
        fake_update_executable(&root.path().join("npm/node_modules/@brokkai/mjolnir/bin/mj"));
    let direct_exe = fake_update_executable(&root.path().join("direct/mj"));

    let variables = |entries: &[(&str, &str)]| {
        entries
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect::<std::collections::HashMap<_, _>>()
    };
    let cases = [
        (
            "npx marker takes precedence",
            direct_exe.clone(),
            variables(&[
                (NPX_MANAGED_ENV, "1"),
                (NPM_MANAGED_ENV, "1"),
                (HOMEBREW_MANAGED_ENV, "1"),
            ]),
        ),
        (
            "npm marker",
            direct_exe.clone(),
            variables(&[(NPM_MANAGED_ENV, "1")]),
        ),
        (
            "Homebrew marker",
            direct_exe.clone(),
            variables(&[(HOMEBREW_MANAGED_ENV, "1")]),
        ),
        ("Homebrew Cellar layout", homebrew_exe, variables(&[])),
        ("npm bundle layout", npm_exe, variables(&[])),
        ("Cargo .crates2.json", cargo_json_exe, variables(&[])),
        ("Cargo .crates.toml", cargo_toml_exe, variables(&[])),
        ("unrecorded executable", direct_exe, variables(&[])),
    ];

    let mut rendered = String::new();
    for (label, executable, environment) in cases {
        let method =
            InstallMethod::detect(|name| environment.get(name).cloned(), Some(&executable));
        rendered.push_str(&format!(
            "=== {label} ===\nmethod: {method:?}\nupgrade: {}\n\n",
            method
                .update_command()
                .unwrap_or_else(|| "(none)".to_owned())
        ));
    }
    rendered.push_str("=== package-manager commands ===\n");
    rendered.push_str(&format!(
        "npm: {}\n",
        update_command_text(&npm_upgrade_command())
    ));
    rendered.push_str(&format!(
        "brew update: {}\n",
        update_command_text(&brew_update_command())
    ));
    rendered.push_str(&format!(
        "brew upgrade: {}",
        update_command_text(&brew_upgrade_command())
    ));
    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "install-method-detection",
        &rendered,
    );
}

#[test]
fn cargo_index_uses_latest_non_yanked_version() {
    let index = concat!(
        r#"{"vers":"2.4.0","yanked":false}"#,
        "\n",
        r#"{"vers":"2.5.0","yanked":true}"#,
        "\n",
        r#"{"vers":"2.4.2","yanked":false}"#,
        "\n",
    );
    assert_eq!(
        parse_cargo_index_version(index).expect("version"),
        Version::parse("2.4.2").expect("semver")
    );
}

fn asset(name: &str) -> ReleaseAsset {
    ReleaseAsset {
        name: name.to_string(),
        browser_download_url: format!("https://example.com/{name}"),
    }
}

fn linux_x64() -> Platform {
    Platform {
        os_family: "linux",
        arch: "x86_64",
        rust_target: "x86_64-unknown-linux-gnu".to_string(),
    }
}

fn macos(arch: &'static str) -> Platform {
    Platform {
        os_family: "macos",
        arch,
        rust_target: format!("{arch}-apple-darwin"),
    }
}

#[test]
fn macos_update_selects_the_running_architecture_even_with_a_universal_asset() {
    // Releases still carrying a universal archive must not win over the
    // running architecture's own archive; the universal asset is only a
    // legacy worker-name fallback, not an update payload.
    let assets = [
        asset("brokk-mjolnir-v2.37.0-universal-apple-darwin.tar.gz"),
        asset("brokk-mjolnir-v2.37.0-x86_64-apple-darwin.tar.gz"),
        asset("brokk-mjolnir-v2.37.0-aarch64-apple-darwin.tar.gz"),
    ];

    assert_eq!(
        select_mj_asset(&assets, &macos("aarch64"))
            .expect("Apple silicon asset")
            .name,
        "brokk-mjolnir-v2.37.0-aarch64-apple-darwin.tar.gz"
    );
    assert_eq!(
        select_mj_asset(&assets, &macos("x86_64"))
            .expect("Intel asset")
            .name,
        "brokk-mjolnir-v2.37.0-x86_64-apple-darwin.tar.gz"
    );
}

#[test]
fn release_newer_than_current_requires_checksum_asset() {
    let release = GitHubRelease {
        tag_name: "v2.5.0".to_string(),
        assets: vec![asset(
            "brokk-mjolnir-v2.5.0-x86_64-unknown-linux-gnu.tar.gz",
        )],
    };

    let error = update_info_from_release(
        &release,
        &Version::parse("2.4.0").expect("version"),
        &linux_x64(),
    )
    .expect_err("missing checksum should fail");

    assert!(error
        .to_string()
        .contains("missing required checksum asset brokk-mjolnir-v2.5.0-x86_64-unknown-linux-gnu.tar.gz.sha256"));
}

#[test]
fn prompt_eof_declines_but_enter_accepts() {
    assert!(!read_update_answer(&mut Cursor::new(b"")).expect("read EOF"));
    assert!(read_update_answer(&mut Cursor::new(b"\n")).expect("read Enter"));
    assert!(read_update_answer(&mut Cursor::new(b"yes\n")).expect("read yes"));
    assert!(!read_update_answer(&mut Cursor::new(b"n\n")).expect("read no"));
}

#[test]
fn golden_mj_update_output() {
    let version = Version::parse("2.5.0").expect("version");
    let mut output = String::new();
    for (label, method) in [
        ("Homebrew", InstallMethod::Homebrew),
        (
            "Cargo",
            InstallMethod::Cargo {
                voice_worker: false,
            },
        ),
    ] {
        let notice = managed_update_notice(&version, &method, "2.4.0").expect("update notice");
        output.push_str(&format!(
            "=== {label} update notice (stdout) ===\n{notice}\n\n"
        ));
    }
    output.pop();
    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "mj-update-output", &output);
}

#[test]
fn managed_restart_retargets_homebrew_to_its_wrapper() {
    // npm must save its installation path before the old bundle is removed.
    // Homebrew must re-resolve the wrapper or the
    // restart would relaunch the old Cellar version.
    let exe = Path::new("/opt/homebrew/Cellar/mjolnir/2.4.0/libexec/mj");
    assert_eq!(
        managed_restart_target(&InstallMethod::Npm, exe).expect("target"),
        RestartTarget::SameExe(exe.to_path_buf())
    );
    assert_eq!(
        managed_restart_target(&InstallMethod::Homebrew, exe).expect("target"),
        RestartTarget::Wrapper
    );
    assert!(managed_restart_target(&InstallMethod::Npx, exe).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn npm_upgrade_restarts_after_the_running_package_is_removed() {
    const FIXTURE_ENV: &str = "MJ_UPDATE_RESTART_FIXTURE";
    if std::env::var_os(FIXTURE_ENV).is_some() {
        let target = run_managed_upgrade(
            &Version::parse("9.9.9").expect("version"),
            &InstallMethod::Npm,
        )
        .expect("fake npm upgrade");
        restart_current_process(target).expect("restart updated binary");
        unreachable!("exec does not return on success");
    }

    // The fixture lives beside the test binary so the package entry below can
    // be a hard link to it. Copying the binary and then exec'ing the copy is
    // the ETXTBSY race described on `install_fake_command`: the copy is long,
    // and any other test thread that forks during it inherits the open write
    // descriptor and keeps the new file busy. A hard link opens nothing for
    // writing, and because the kernel records the path used by `execve`, the
    // child still sees `current_exe()` inside the package, which is what the
    // npm restart target is resolved from.
    let binary = std::env::current_exe().expect("test binary");
    let root = tempfile::tempdir_in(binary.parent().expect("test binary directory"))
        .expect("fixture directory");
    let package_bin = root.path().join("package/bin");
    let manager_bin = root.path().join("manager");
    std::fs::create_dir_all(&package_bin).expect("package bin");
    std::fs::create_dir(&manager_bin).expect("manager bin");
    let executable = package_bin.join("mj");
    std::fs::hard_link(&binary, &executable).expect("link the test binary into the package");
    let replacement = root.path().join("replacement");
    std::fs::write(&replacement, "#!/bin/sh\necho UPDATED_MJ_RESTARTED\n")
        .expect("replacement script");
    // `npm` and `replacement` are only ever read: the fake npm script is run
    // through the shared dispatcher, and `replacement` is copied by that
    // script before anything execs the copy.
    mj_core::test_hooks::install_fake_command(
        &manager_bin,
        "npm",
        r#"#!/bin/sh
set -eu
mv "$MJ_UPDATE_RESTART_FIXTURE/package" "$MJ_UPDATE_RESTART_FIXTURE/retired"
mkdir -p "$MJ_UPDATE_RESTART_FIXTURE/package/bin"
cp "$MJ_UPDATE_RESTART_FIXTURE/replacement" "$MJ_UPDATE_RESTART_FIXTURE/package/bin/mj"
rm "$MJ_UPDATE_RESTART_FIXTURE/retired/bin/mj"
"#,
    );
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o755))
        .expect("executable script");
    let mut paths = vec![manager_bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let mut child = Command::new(executable);
    child.args([
        "--exact",
        "controller::update::tests::npm_upgrade_restarts_after_the_running_package_is_removed",
        "--nocapture",
    ]);
    child.env(FIXTURE_ENV, root.path());
    child.env("PATH", std::env::join_paths(paths).expect("fixture PATH"));
    let output =
        mj_core::subprocess::run_with_input(&mut child, b"").expect("run copied test executable");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("UPDATED_MJ_RESTARTED"));
}

#[cfg(unix)]
fn release_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut archive = tar::Builder::new(gz);
    for (name, bytes) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, name, *bytes)
            .expect("tar entry");
    }
    archive
        .into_inner()
        .expect("tar archive")
        .finish()
        .expect("gzip archive")
}

#[cfg(unix)]
#[test]
fn release_upgrade_replaces_every_packaged_binary_and_preserves_unrelated_files() {
    let root = tempfile::tempdir().expect("install directory");
    let executable = root.path().join("custom-mj-name");
    std::fs::write(&executable, b"old controller").expect("old controller");
    std::fs::write(root.path().join("README.md"), b"user notes").expect("unrelated file");
    let new_binary = vec![b'n'; 128 * 1024];
    let entries: Vec<(&str, &[u8])> = [
        "release/mj",
        "release/mj-desktop",
        "release/mj-voice-worker",
        "release/mj-worker",
        "release/mj-worker-x86_64-unknown-linux-musl",
        "release/mj-worker-aarch64-unknown-linux-musl",
    ]
    .into_iter()
    .map(|name| (name, new_binary.as_slice()))
    .collect();
    for (name, _) in &entries[1..] {
        std::fs::write(
            root.path().join(Path::new(name).file_name().unwrap()),
            b"old helper",
        )
        .expect("old helper");
    }
    let mut archive_entries = entries.clone();
    archive_entries.push(("release/README.md", b"release notes"));
    let archive = release_tar(&archive_entries);
    let installed = install_release_archive(&executable, "release.tar.gz", &archive)
        .expect("install complete release");
    assert_eq!(
        installed,
        executable.canonicalize().expect("resolved controller")
    );
    assert_eq!(std::fs::read(&executable).unwrap(), new_binary);
    use std::os::unix::fs::PermissionsExt;
    for (name, _) in &entries[1..] {
        let helper = root.path().join(Path::new(name).file_name().unwrap());
        assert_eq!(std::fs::read(&helper).unwrap(), new_binary);
        assert_eq!(
            std::fs::metadata(&helper).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
    assert_eq!(
        std::fs::read(root.path().join("README.md")).unwrap(),
        b"user notes"
    );
    assert_eq!(
        std::fs::read_dir(root.path()).unwrap().count(),
        entries.len() + 1
    );
}

#[cfg(unix)]
#[test]
fn malformed_release_leaves_installed_binaries_unchanged() {
    let root = tempfile::tempdir().expect("install directory");
    let executable = root.path().join("mj");
    let worker = root.path().join("mj-worker");
    for archive in [
        release_tar(&[("mj", b"new controller"), ("mj-worker", b"")]),
        release_tar(&[("mj", b"new controller"), ("mj", b"duplicate")]),
        release_tar(&[("mj-worker", b"new worker")]),
    ] {
        std::fs::write(&executable, b"old controller").unwrap();
        std::fs::write(&worker, b"old worker").unwrap();
        assert!(install_release_archive(&executable, "release.tar.gz", &archive).is_err());
        assert_eq!(std::fs::read(&executable).unwrap(), b"old controller");
        assert_eq!(std::fs::read(&worker).unwrap(), b"old worker");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
    }
}

#[cfg(unix)]
#[test]
fn release_upgrade_allows_retired_companions() {
    let root = tempfile::tempdir().expect("install directory");
    let executable = root.path().join("mj");
    std::fs::write(&executable, b"old controller").unwrap();
    let archive = release_tar(&[("mj", b"new controller")]);
    install_release_archive(&executable, "release.tar.gz", &archive).expect("main-only release");
    assert_eq!(std::fs::read(&executable).unwrap(), b"new controller");
}

#[test]
fn zip_release_stages_application_binaries_without_extracting_documents() {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    for name in [
        "release/mj.exe",
        "release/mj-worker.exe",
        "release/mj-desktop.exe",
        "release/LICENSE",
    ] {
        archive.start_file(name, options).expect("zip entry");
        archive.write_all(b"binary contents").expect("zip contents");
    }
    let bytes = archive.finish().expect("zip archive").into_inner();
    let root = tempfile::tempdir().expect("staging directory");
    let binaries = stage_release_archive("release.zip", &bytes, root.path()).expect("stage zip");
    assert_eq!(binaries.len(), 3);
    for name in ["mj.exe", "mj-worker.exe", "mj-desktop.exe"] {
        assert_eq!(
            std::fs::read(root.path().join(name)).unwrap(),
            b"binary contents"
        );
    }
    assert!(!root.path().join("LICENSE").exists());
}

/// Serves canned bodies per path prefix from a loopback port and returns
/// sources pointed at it, so channel fetches never leave the machine.
async fn serve_update_sources(
    routes: Vec<(&'static str, &'static str)>,
) -> (UpdateSources, std::net::SocketAddr) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = vec![0u8; 4096];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let path = request.split_whitespace().nth(1).unwrap_or_default();
            let matched = routes
                .iter()
                .find(|(route, _)| path.starts_with(route))
                .map(|(_, body)| *body);
            let (status, body) = match matched {
                Some(body) => ("200 OK", body),
                None => ("404 Not Found", ""),
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });

    let base = format!("http://{addr}");
    (
        UpdateSources {
            latest_release: format!("{base}/release"),
            npm_latest: format!("{base}/npm"),
            homebrew_formula: format!("{base}/formula"),
            cargo_index: format!("{base}/index"),
        },
        addr,
    )
}

#[tokio::test]
async fn npm_registry_update_is_offered_when_newer() {
    let (sources, _) = serve_update_sources(vec![("/npm", r#"{"version":"9.9.9"}"#)]).await;

    let update = latest_update(&sources, &InstallMethod::Npm)
        .await
        .expect("update check");

    assert_eq!(
        update,
        Some(AvailableUpdate::Managed {
            version: Version::parse("9.9.9").expect("version"),
            method: InstallMethod::Npm,
        })
    );
}

#[tokio::test]
async fn up_to_date_channel_offers_nothing() {
    let (sources, _) = serve_update_sources(vec![("/npm", r#"{"version":"0.0.1"}"#)]).await;

    let update = latest_update(&sources, &InstallMethod::Npm)
        .await
        .expect("update check");

    assert_eq!(update, None);
}

#[tokio::test]
async fn homebrew_formula_update_is_offered_when_newer() {
    let formula = "class Mjolnir < Formula\n  version \"9.9.9\"\nend\n";
    let (sources, _) = serve_update_sources(vec![("/formula", formula)]).await;

    let update = latest_update(&sources, &InstallMethod::Homebrew)
        .await
        .expect("update check");

    assert_eq!(
        update,
        Some(AvailableUpdate::Managed {
            version: Version::parse("9.9.9").expect("version"),
            method: InstallMethod::Homebrew,
        })
    );
}

#[tokio::test]
async fn failed_channel_fetch_is_reported_as_an_error() {
    // No route matches /npm, so the stub answers 404.
    let (sources, _) = serve_update_sources(vec![("/formula", "unused")]).await;

    let error = latest_update(&sources, &InstallMethod::Npm)
        .await
        .expect_err("404 should fail the check");

    assert!(format!("{error:#}").contains("404"));
}

#[tokio::test]
async fn direct_installs_read_the_release_endpoint() {
    if std::env::consts::OS != "linux" || std::env::consts::ARCH != "x86_64" {
        return;
    }

    let release = concat!(
        r#"{"tag_name":"v9.9.9","assets":["#,
        r#"{"name":"brokk-mjolnir-v9.9.9-x86_64-unknown-linux-gnu.tar.gz","#,
        r#""browser_download_url":"https://example.com/mj.tar.gz"},"#,
        r#"{"name":"brokk-mjolnir-v9.9.9-x86_64-unknown-linux-gnu.tar.gz.sha256","#,
        r#""browser_download_url":"https://example.com/mj.tar.gz.sha256"}]}"#,
    );
    let (sources, _) = serve_update_sources(vec![("/release", release)]).await;

    let update = latest_update(&sources, &InstallMethod::Direct)
        .await
        .expect("release response")
        .expect("newer release");

    match update {
        AvailableUpdate::Direct(info) => {
            assert_eq!(info.version, Version::parse("9.9.9").expect("version"));
            assert_eq!(info.tag, "v9.9.9");
            assert_eq!(
                info.asset.name,
                "brokk-mjolnir-v9.9.9-x86_64-unknown-linux-gnu.tar.gz"
            );
            assert_eq!(
                info.checksum_asset.name,
                "brokk-mjolnir-v9.9.9-x86_64-unknown-linux-gnu.tar.gz.sha256"
            );
        }
        other => panic!("expected a direct update, got {other:?}"),
    }
}
