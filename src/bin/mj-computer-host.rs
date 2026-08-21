//! The macOS-only Mjolnir Computer app executable.
//!
//! It is intentionally a separate app-bundle executable rather than a hidden
//! `mj` subcommand: macOS must attribute Screen Recording and Accessibility to
//! Mjolnir Computer, never the terminal that launched Mjolnir.

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        fs::{self, OpenOptions},
        io::Read as _,
        os::unix::fs::{OpenOptionsExt as _, PermissionsExt},
        path::{Path, PathBuf},
        sync::Arc,
    };

    use anyhow::{Context, Result, bail};
    use clap::Parser;
    use mj_core::{
        computer_host::{HostLaunchDescriptor, HostService, serve_connection},
        computer_macos::MacosComputerBackend,
    };
    use tokio::net::UnixListener;

    const AUTHENTICATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    #[derive(Debug, Parser)]
    #[command(name = "mj-computer-host", disable_version_flag = true)]
    struct Args {
        /// A mode-0600 descriptor made by Mjolnir for one authenticated ACP
        /// session. The descriptor is deleted immediately after it is read.
        #[arg(long)]
        launch_descriptor: PathBuf,

        /// Deliberately opt into a local developer build outside the signed
        /// Mjolnir Computer.app bundle. Mjolnir itself never supplies this.
        #[arg(long)]
        development_host: bool,
    }

    struct SocketGuard(PathBuf);

    impl Drop for SocketGuard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    pub async fn run() -> Result<()> {
        let args = Args::parse();
        if !args.development_host {
            require_mjolnir_computer_bundle()?;
        } else {
            eprintln!(
                "mj-computer-host development mode: macOS will attribute permissions to this local build, not a production Mjolnir Computer app"
            );
        }
        let descriptor = read_private_descriptor(&args.launch_descriptor)?;
        fs::remove_file(&args.launch_descriptor).with_context(|| {
            format!(
                "remove consumed computer host descriptor {}",
                args.launch_descriptor.display()
            )
        })?;
        let listener = bind_private_socket(&descriptor.socket_path)?;
        let _socket_guard = SocketGuard(descriptor.socket_path);
        let service = Arc::new(
            HostService::new(
                MacosComputerBackend::default(),
                descriptor.session_id,
                descriptor.capability,
            )
            .context("validate computer host launch descriptor")?,
        );
        let revocation = service.revocation_token();
        let authenticated = service.authentication_token();
        let authentication_timeout = tokio::time::sleep(AUTHENTICATION_TIMEOUT);
        tokio::pin!(authentication_timeout);

        loop {
            tokio::select! {
                _ = revocation.cancelled() => return Ok(()),
                _ = &mut authentication_timeout, if !authenticated.is_cancelled() => {
                    // `mj` failed before it established the capability
                    // handshake. Do not leave an inaccessible LSUIElement app
                    // holding macOS automation permissions.
                    service.revoke().await;
                    return Ok(());
                }
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.context("accept Mjolnir computer host connection")?;
                    let service = service.clone();
                    tokio::spawn(async move {
                        if let Err(error) = serve_connection(&service, &mut stream).await {
                            tracing::debug!(%error, "computer host connection ended");
                        }
                    });
                }
            }
        }
    }

    fn require_mjolnir_computer_bundle() -> Result<()> {
        let executable = std::env::current_exe().context("locate computer host executable")?;
        validate_mjolnir_computer_bundle(&executable)
    }

    fn validate_mjolnir_computer_bundle(executable: &Path) -> Result<()> {
        let Some(macos) = executable.parent() else {
            bail!("Mjolnir Computer host executable has no parent directory");
        };
        let Some(contents) = macos.parent() else {
            bail!("Mjolnir Computer host must run from Mjolnir Computer.app");
        };
        let Some(bundle) = contents.parent() else {
            bail!("Mjolnir Computer host must run from Mjolnir Computer.app");
        };
        if macos.file_name().is_none_or(|name| name != "MacOS")
            || contents.file_name().is_none_or(|name| name != "Contents")
            || bundle
                .extension()
                .is_none_or(|extension| extension != "app")
            || !bundle.join("Contents/Info.plist").is_file()
        {
            bail!(
                "Mjolnir Computer host refuses to run outside Mjolnir Computer.app; use --development-host only for explicit local development"
            );
        }
        Ok(())
    }

    fn read_private_descriptor(path: &Path) -> Result<HostLaunchDescriptor> {
        let parent = path
            .parent()
            .context("computer host descriptor has no parent")?;
        let parent_metadata = fs::metadata(parent).with_context(|| {
            format!(
                "inspect computer host descriptor directory {}",
                parent.display()
            )
        })?;
        if parent_metadata.permissions().mode() & 0o077 != 0 {
            bail!(
                "computer host descriptor directory must not be accessible to group or other users"
            );
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("open computer host descriptor {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("inspect computer host descriptor {}", path.display()))?;
        if !metadata.file_type().is_file() {
            bail!("computer host descriptor must be a regular file");
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!(
                "computer host descriptor must not be accessible to group or other users (expected mode 0600)"
            );
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .with_context(|| format!("read computer host descriptor {}", path.display()))?;
        serde_json::from_slice(&bytes).context("parse computer host descriptor")
    }

    fn bind_private_socket(path: &Path) -> Result<UnixListener> {
        if path.exists() {
            bail!(
                "computer host socket path already exists: {}; refusing to replace it",
                path.display()
            );
        }
        let parent = path
            .parent()
            .context("computer host socket path has no parent")?;
        let metadata = fs::metadata(parent).with_context(|| {
            format!(
                "inspect computer host socket directory {}",
                parent.display()
            )
        })?;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("computer host socket directory must not be accessible to group or other users");
        }
        let listener = UnixListener::bind(path)
            .with_context(|| format!("bind computer host socket {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("protect computer host socket {}", path.display()))?;
        Ok(listener)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use mj_core::computer_host::{HostCapability, HostSessionId};

        #[test]
        fn bundle_check_rejects_an_ordinary_binary_path() {
            let temporary = tempfile::tempdir().unwrap();
            let binary = temporary.path().join("mj-computer-host");
            fs::write(&binary, []).unwrap();
            assert!(validate_mjolnir_computer_bundle(&binary).is_err());
        }

        #[test]
        fn bundle_check_accepts_the_expected_app_structure() {
            let temporary = tempfile::tempdir().unwrap();
            let executable = temporary
                .path()
                .join("Mjolnir Computer.app/Contents/MacOS/mj-computer-host");
            fs::create_dir_all(executable.parent().unwrap()).unwrap();
            fs::write(
                executable.ancestors().nth(2).unwrap().join("Info.plist"),
                [],
            )
            .unwrap();
            fs::write(&executable, []).unwrap();
            assert!(validate_mjolnir_computer_bundle(&executable).is_ok());
        }

        #[test]
        fn launch_descriptor_must_stay_in_a_private_directory_and_file() {
            let temporary = tempfile::tempdir().unwrap();
            fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let path = temporary.path().join("launch.json");
            let descriptor = HostLaunchDescriptor {
                socket_path: temporary.path().join("host.sock"),
                session_id: HostSessionId("session".to_string()),
                capability: HostCapability::generate().unwrap(),
            };
            fs::write(&path, serde_json::to_vec(&descriptor).unwrap()).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(
                read_private_descriptor(&path).unwrap().session_id,
                HostSessionId("session".to_string())
            );

            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            assert!(read_private_descriptor(&path).is_err());
        }
    }
}

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    macos::run().await
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mj-computer-host is available only in the macOS Mjolnir Computer.app bundle");
    std::process::exit(2);
}
