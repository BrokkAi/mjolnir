//! Optional trusted HTTPS for the daemon-owned web viewer.
//!
//! Tailscale owns ACME issuance for its `ts.net` names. Hel only discovers the
//! local node, asks the CLI for that certificate, and stores the resulting
//! pair in its private data directory.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use hel::hel_config::atomic_write;
use hel::hel_targets::{CommandExecutor, CommandSpec};

const CERT_FILE: &str = "tailscale-cert.pem";
const KEY_FILE: &str = "tailscale-key.pem";

#[derive(Debug, Clone)]
struct Tailscale {
    binary: PathBuf,
    cert_domain: String,
}

/// A Tailscale identity and its persisted certificate pair.
#[derive(Debug, Clone)]
pub struct TailscaleTls {
    tailscale: Tailscale,
    cert_path: PathBuf,
    key_path: PathBuf,
}

impl TailscaleTls {
    pub fn cert_domain(&self) -> &str {
        &self.tailscale.cert_domain
    }

    pub fn cert_path(&self) -> &Path {
        &self.cert_path
    }

    pub fn key_path(&self) -> &Path {
        &self.key_path
    }

    /// Refresh the persisted pair. Callers keep serving their currently
    /// loaded certificate until they have successfully reloaded these files.
    pub fn renew(&self, executor: &impl CommandExecutor) -> Result<()> {
        mint_certificate(&self.tailscale, &self.cert_path, &self.key_path, executor)
    }
}

/// Discover a certificate-capable local Tailscale node and mint its initial
/// certificate. Discovery failures are intentionally returned to the caller,
/// which can safely fall back to a loopback-only listener.
pub fn prepare_tailscale_tls(root: &Path, executor: &impl CommandExecutor) -> Result<TailscaleTls> {
    let binary = find_binary()
        .ok_or_else(|| anyhow!("tailscale CLI not found in PATH or the macOS app bundle"))?;
    prepare_tailscale_tls_with_binary(root, binary, executor)
}

fn prepare_tailscale_tls_with_binary(
    root: &Path,
    binary: PathBuf,
    executor: &impl CommandExecutor,
) -> Result<TailscaleTls> {
    let tailscale = discover_with_binary(binary, executor)?;
    fs::create_dir_all(root)
        .with_context(|| format!("create web viewer TLS directory {}", root.display()))?;
    let tls = TailscaleTls {
        tailscale,
        cert_path: root.join(CERT_FILE),
        key_path: root.join(KEY_FILE),
    };
    tls.renew(executor)?;
    Ok(tls)
}

fn discover_with_binary(binary: PathBuf, executor: &impl CommandExecutor) -> Result<Tailscale> {
    let command = CommandSpec::new(
        binary.to_string_lossy(),
        ["status".to_owned(), "--json".to_owned()],
    )
    .purpose("inspect local Tailscale status");
    let output = executor.execute(&command)?;
    if output.status != 0 {
        bail!(
            "`tailscale status` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let status: Status =
        serde_json::from_slice(&output.stdout).context("parse `tailscale status --json` output")?;
    let cert_domain = cert_domain(&status)?;
    Ok(Tailscale {
        binary,
        cert_domain,
    })
}

fn mint_certificate(
    tailscale: &Tailscale,
    cert_path: &Path,
    key_path: &Path,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let parent = cert_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("create web viewer TLS directory {}", parent.display()))?;
    let suffix = temporary_suffix()?;
    let staged_cert = parent.join(format!(".{CERT_FILE}.{suffix}.tmp"));
    let staged_key = parent.join(format!(".{KEY_FILE}.{suffix}.tmp"));
    let result = (|| -> Result<()> {
        let command = CommandSpec::new(
            tailscale.binary.to_string_lossy(),
            [
                "cert".to_owned(),
                "--cert-file".to_owned(),
                staged_cert.to_string_lossy().into_owned(),
                "--key-file".to_owned(),
                staged_key.to_string_lossy().into_owned(),
                tailscale.cert_domain.clone(),
            ],
        )
        .purpose("obtain the web viewer Tailscale certificate");
        let output = executor.execute(&command)?;
        if output.status != 0 {
            bail!(
                "`tailscale cert {}` failed: {}",
                tailscale.cert_domain,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let cert = fs::read(&staged_cert)
            .with_context(|| format!("read issued certificate {}", staged_cert.display()))?;
        let key = fs::read(&staged_key)
            .with_context(|| format!("read issued private key {}", staged_key.display()))?;
        validate_pem(&cert, &key)?;
        atomic_write(cert_path, &cert)?;
        atomic_write(key_path, &key)?;
        Ok(())
    })();
    remove_staged_file(&staged_cert);
    remove_staged_file(&staged_key);
    result
}

fn temporary_suffix() -> Result<String> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow!("generate temporary certificate filename: {error}"))?;
    Ok(format!(
        "{}.{:016x}",
        std::process::id(),
        u64::from_le_bytes(random)
    ))
}

fn validate_pem(cert: &[u8], key: &[u8]) -> Result<()> {
    if !cert
        .windows(b"-----BEGIN CERTIFICATE-----".len())
        .any(|window| window == b"-----BEGIN CERTIFICATE-----")
    {
        bail!("tailscale returned a certificate file without a PEM certificate");
    }
    if !key
        .windows(b"PRIVATE KEY-----".len())
        .any(|window| window == b"PRIVATE KEY-----")
    {
        bail!("tailscale returned a key file without a PEM private key");
    }
    Ok(())
}

fn remove_staged_file(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), %error, "could not remove staged Tailscale certificate file");
    }
}

fn find_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH")
        && let Some(binary) = find_binary_in_path(&path)
    {
        return Some(binary);
    }
    find_bundled_binary()
}

fn find_binary_in_path(path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path).find_map(|directory| {
        tailscale_binary_names()
            .iter()
            .map(|name| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(windows)]
fn tailscale_binary_names() -> &'static [&'static str] {
    &["tailscale.exe", "tailscale"]
}

#[cfg(not(windows))]
fn tailscale_binary_names() -> &'static [&'static str] {
    &["tailscale"]
}

#[cfg(target_os = "macos")]
fn find_bundled_binary() -> Option<PathBuf> {
    let bundled = PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
    bundled.is_file().then_some(bundled)
}

#[cfg(not(target_os = "macos"))]
fn find_bundled_binary() -> Option<PathBuf> {
    None
}

#[derive(Debug, Deserialize)]
struct Status {
    #[serde(rename = "BackendState")]
    backend_state: String,
    #[serde(rename = "CertDomains")]
    cert_domains: Option<Vec<String>>,
}

fn cert_domain(status: &Status) -> Result<String> {
    if status.backend_state != "Running" {
        bail!(
            "tailscale is not running (state: {}); run `tailscale up` first",
            status.backend_state
        );
    }
    status
        .cert_domains
        .as_deref()
        .unwrap_or_default()
        .first()
        .map(|domain| domain.trim_end_matches('.').to_owned())
        .filter(|domain| !domain.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "this tailnet has no HTTPS certificate domains; enable MagicDNS and HTTPS Certificates under the DNS tab at https://login.tailscale.com/admin/dns, then run `mj daemon restart`"
            )
        })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use hel::hel_targets::CommandOutput;

    struct FakeExecutor {
        outputs: Mutex<VecDeque<CommandOutput>>,
        commands: Mutex<Vec<CommandSpec>>,
        issue_files: bool,
    }

    impl FakeExecutor {
        fn new(outputs: impl IntoIterator<Item = CommandOutput>, issue_files: bool) -> Self {
            Self {
                outputs: Mutex::new(outputs.into_iter().collect()),
                commands: Mutex::new(Vec::new()),
                issue_files,
            }
        }
    }

    impl CommandExecutor for FakeExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.lock().unwrap().push(command.clone());
            if self.issue_files && command.args.first().map(String::as_str) == Some("cert") {
                fs::write(&command.args[2], b"-----BEGIN CERTIFICATE-----\ncert\n")?;
                fs::write(&command.args[4], b"-----BEGIN PRIVATE KEY-----\nkey\n")?;
            }
            self.outputs
                .lock()
                .unwrap()
                .pop_front()
                .context("fake command output missing")
        }
    }

    fn output(status: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            status,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn discovers_domain_and_mints_expected_certificate() {
        let directory = tempfile::tempdir().unwrap();
        let executor = FakeExecutor::new(
            [
                output(
                    0,
                    r#"{"BackendState":"Running","CertDomains":["minas.tail.ts.net."]}"#,
                    "",
                ),
                output(0, "", ""),
            ],
            true,
        );

        let tls = prepare_tailscale_tls_with_binary(
            directory.path(),
            PathBuf::from("/usr/bin/tailscale"),
            &executor,
        )
        .unwrap();

        assert_eq!(tls.cert_domain(), "minas.tail.ts.net");
        assert!(tls.cert_path().is_file());
        assert!(tls.key_path().is_file());
        let commands = executor.commands.lock().unwrap();
        assert_eq!(commands[0].args, ["status", "--json"]);
        assert_eq!(commands[1].args[0], "cert");
        assert_eq!(commands[1].args[5], "minas.tail.ts.net");
    }

    #[test]
    fn missing_certificate_domains_has_actionable_guidance() {
        let executor = FakeExecutor::new([output(0, r#"{"BackendState":"Running"}"#, "")], false);
        let error = discover_with_binary(PathBuf::from("tailscale"), &executor).unwrap_err();

        assert!(error.to_string().contains("MagicDNS"));
        assert!(error.to_string().contains("mj daemon restart"));
    }

    #[test]
    fn stopped_or_malformed_status_is_rejected() {
        let stopped = FakeExecutor::new(
            [output(
                0,
                r#"{"BackendState":"Stopped","CertDomains":["host.ts.net"]}"#,
                "",
            )],
            false,
        );
        assert!(
            discover_with_binary(PathBuf::from("tailscale"), &stopped)
                .unwrap_err()
                .to_string()
                .contains("not running")
        );

        let malformed = FakeExecutor::new([output(0, "not-json", "")], false);
        assert!(
            discover_with_binary(PathBuf::from("tailscale"), &malformed)
                .unwrap_err()
                .to_string()
                .contains("parse")
        );
    }

    #[test]
    fn command_failure_does_not_replace_existing_pair() {
        let directory = tempfile::tempdir().unwrap();
        let cert_path = directory.path().join(CERT_FILE);
        let key_path = directory.path().join(KEY_FILE);
        fs::write(&cert_path, "old-cert").unwrap();
        fs::write(&key_path, "old-key").unwrap();
        let executor = FakeExecutor::new([output(1, "", "issuance failed")], false);
        let tailscale = Tailscale {
            binary: PathBuf::from("tailscale"),
            cert_domain: "host.ts.net".into(),
        };

        assert!(mint_certificate(&tailscale, &cert_path, &key_path, &executor).is_err());
        assert_eq!(fs::read_to_string(cert_path).unwrap(), "old-cert");
        assert_eq!(fs::read_to_string(key_path).unwrap(), "old-key");
    }

    #[cfg(unix)]
    #[test]
    fn persisted_private_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let executor = FakeExecutor::new(
            [
                output(
                    0,
                    r#"{"BackendState":"Running","CertDomains":["host.ts.net"]}"#,
                    "",
                ),
                output(0, "", ""),
            ],
            true,
        );
        let tls = prepare_tailscale_tls_with_binary(
            directory.path(),
            PathBuf::from("tailscale"),
            &executor,
        )
        .unwrap();

        let mode = fs::metadata(tls.key_path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
