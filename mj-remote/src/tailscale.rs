//! Tailscale integration for `mj server`.
//!
//! Discovers the local tailscale CLI, reads the node's HTTPS certificate
//! domain from `tailscale status --json`, and mints a publicly trusted
//! Let's Encrypt certificate via `tailscale cert`. Tailscale answers the
//! ACME DNS-01 challenge on its own `ts.net` zone, so this works even
//! though the machine is unreachable from the public internet — and the
//! resulting certificate produces no browser warning on any device.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

/// A running tailscale daemon that can issue certificates for this node.
#[derive(Debug, Clone)]
pub struct Tailscale {
    binary: PathBuf,
    /// Fully qualified `ts.net` name certificates can be issued for,
    /// e.g. `mybox.tail1234.ts.net` (no trailing dot).
    pub cert_domain: String,
}

impl Tailscale {
    /// Locate the tailscale CLI and confirm the node can mint certificates.
    pub fn discover() -> Result<Self> {
        let binary = find_binary()
            .ok_or_else(|| anyhow!("tailscale CLI not found in PATH or the macOS app bundle"))?;
        Self::discover_with_binary(binary)
    }

    fn discover_with_binary(binary: PathBuf) -> Result<Self> {
        let output = Command::new(&binary)
            .args(["status", "--json"])
            .output()
            .with_context(|| format!("run `{} status --json`", binary.display()))?;
        if !output.status.success() {
            bail!(
                "`tailscale status` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let status: Status = serde_json::from_slice(&output.stdout)
            .context("parse `tailscale status --json` output")?;
        let cert_domain = cert_domain(&status)?;
        Ok(Self {
            binary,
            cert_domain,
        })
    }

    /// Write a certificate chain and private key for `cert_domain` to the
    /// given paths. Tailscale caches issued certificates, so this only talks
    /// to Let's Encrypt on first run or when renewal is due; re-running it
    /// periodically is the supported renewal mechanism.
    pub fn mint_cert(&self, cert_path: &Path, key_path: &Path) -> Result<()> {
        let output = Command::new(&self.binary)
            .arg("cert")
            .arg("--cert-file")
            .arg(cert_path)
            .arg("--key-file")
            .arg(key_path)
            .arg(&self.cert_domain)
            .output()
            .with_context(|| format!("run `{} cert`", self.binary.display()))?;
        if !output.status.success() {
            bail!(
                "`tailscale cert {}` failed: {}",
                self.cert_domain,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

fn find_binary() -> Option<PathBuf> {
    if let Some(path_var) = std::env::var_os("PATH")
        && let Some(binary) = find_binary_in_path(&path_var)
    {
        return Some(binary);
    }
    find_bundled_binary()
}

fn find_binary_in_path(path_var: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path_var).find_map(|dir| {
        tailscale_binary_names()
            .iter()
            .map(|name| dir.join(name))
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
    // The macOS app (App Store and standalone) does not put the CLI on PATH.
    let app = PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
    if app.is_file() {
        return Some(app);
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn find_bundled_binary() -> Option<PathBuf> {
    None
}

/// Minimal slice of `tailscale status --json`.
#[derive(Debug, Deserialize)]
struct Status {
    #[serde(rename = "BackendState")]
    backend_state: String,
    /// Domains this node may request certificates for. Empty or absent when
    /// the tailnet has not enabled MagicDNS + HTTPS Certificates.
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
        .map(|domain| domain.trim_end_matches('.').to_string())
        .ok_or_else(|| {
            anyhow!(
                "this tailnet has no HTTPS certificate domains; enable MagicDNS and \
                 HTTPS Certificates under the DNS tab of the Tailscale admin console \
                 (https://login.tailscale.com/admin/dns), then retry"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn fake_tailscale(script: &str) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let binary = dir.path().join("tailscale");
        std::fs::write(&binary, script).expect("write fake tailscale");
        let mut permissions = std::fs::metadata(&binary).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&binary, permissions).expect("make fake tailscale executable");
        (dir, binary)
    }

    /// Executing a just-written script races with every concurrent test that
    /// forks: a child forked while the script's write descriptor was open
    /// still holds it until its own exec, and executing the script in that
    /// window fails with `ETXTBSY`. The window closes on its own, so retry
    /// exactly that error and let every other outcome through.
    #[cfg(unix)]
    fn retry_text_file_busy<T>(mut run: impl FnMut() -> Result<T>) -> Result<T> {
        const ETXTBSY: i32 = 26; // Same value on Linux and macOS.
        for _ in 0..50 {
            let result = run();
            let text_file_busy = result.as_ref().is_err_and(|error| {
                error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io_error| io_error.raw_os_error() == Some(ETXTBSY))
                })
            });
            if !text_file_busy {
                return result;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        run()
    }

    fn status(backend_state: &str, cert_domains: Option<Vec<&str>>) -> Status {
        Status {
            backend_state: backend_state.to_string(),
            cert_domains: cert_domains
                .map(|domains| domains.into_iter().map(str::to_string).collect()),
        }
    }

    #[test]
    fn cert_domain_prefers_first_entry_and_strips_trailing_dot() {
        let status = status("Running", Some(vec!["mybox.tail1234.ts.net."]));
        assert_eq!(
            cert_domain(&status).expect("domain"),
            "mybox.tail1234.ts.net"
        );
    }

    #[test]
    fn cert_domain_rejects_stopped_daemon() {
        let status = status("Stopped", Some(vec!["mybox.tail1234.ts.net"]));
        let error = cert_domain(&status).expect_err("stopped");
        assert!(error.to_string().contains("tailscale up"), "{error}");
    }

    #[test]
    fn cert_domain_requires_https_certificates_enabled() {
        for cert_domains in [None, Some(vec![])] {
            let status = status("Running", cert_domains);
            let error = cert_domain(&status).expect_err("no domains");
            assert!(error.to_string().contains("HTTPS Certificates"), "{error}");
        }
    }

    #[test]
    fn status_parses_real_output_shape() {
        let json = r#"{
            "Version": "1.80.0",
            "BackendState": "Running",
            "CertDomains": ["mybox.tail1234.ts.net"],
            "Self": {"DNSName": "mybox.tail1234.ts.net.", "Online": true},
            "MagicDNSSuffix": "tail1234.ts.net"
        }"#;
        let status: Status = serde_json::from_str(json).expect("parse");
        assert_eq!(status.backend_state, "Running");
        assert_eq!(
            cert_domain(&status).expect("domain"),
            "mybox.tail1234.ts.net"
        );
    }

    #[test]
    fn status_tolerates_missing_cert_domains() {
        let json = r#"{"BackendState": "NeedsLogin"}"#;
        let status: Status = serde_json::from_str(json).expect("parse");
        assert!(cert_domain(&status).is_err());
    }

    #[test]
    fn find_binary_in_path_finds_tailscale_without_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binary = dir.path().join("tailscale");
        std::fs::write(&binary, "").expect("write binary");
        let path = std::env::join_paths([dir.path()]).expect("join path");
        assert_eq!(find_binary_in_path(&path), Some(binary));
    }

    #[cfg(unix)]
    #[test]
    fn discover_reads_certificate_domain_from_cli() {
        let (_dir, binary) = fake_tailscale(
            r#"#!/bin/sh
if [ "$1" = "status" ] && [ "$2" = "--json" ]; then
  printf '%s' '{"BackendState":"Running","CertDomains":["node.tail1234.ts.net."]}'
  exit 0
fi
printf '%s' 'unexpected arguments' >&2
exit 9
"#,
        );

        let tailscale = retry_text_file_busy(|| Tailscale::discover_with_binary(binary.clone()))
            .expect("discover");
        assert_eq!(tailscale.binary, binary);
        assert_eq!(tailscale.cert_domain, "node.tail1234.ts.net");
    }

    #[cfg(unix)]
    #[test]
    fn discover_reports_status_command_failure() {
        let (_dir, binary) = fake_tailscale(
            r#"#!/bin/sh
printf '%s' 'daemon unavailable' >&2
exit 4
"#,
        );

        let error = retry_text_file_busy(|| Tailscale::discover_with_binary(binary.clone()))
            .expect_err("status failure");
        let message = error.to_string();
        assert!(message.contains("tailscale status"), "{message}");
        assert!(message.contains("daemon unavailable"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn discover_reports_malformed_status_json() {
        let (_dir, binary) = fake_tailscale(
            r#"#!/bin/sh
printf '%s' 'not json'
"#,
        );

        let error = retry_text_file_busy(|| Tailscale::discover_with_binary(binary.clone()))
            .expect_err("malformed status");
        assert!(
            error
                .to_string()
                .contains("parse `tailscale status --json` output"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn mint_cert_passes_paths_and_domain_to_cli() {
        let (_dir, binary) = fake_tailscale(
            r#"#!/bin/sh
if [ "$1" != "cert" ] || [ "$2" != "--cert-file" ] || [ "$4" != "--key-file" ] || [ "$6" != "node.tail1234.ts.net" ]; then
  printf '%s' 'unexpected arguments' >&2
  exit 9
fi
printf '%s' 'certificate' > "$3"
printf '%s' 'private key' > "$5"
"#,
        );
        let output = tempfile::tempdir().expect("output tempdir");
        let cert_path = output.path().join("node.crt");
        let key_path = output.path().join("node.key");
        let tailscale = Tailscale {
            binary,
            cert_domain: "node.tail1234.ts.net".to_string(),
        };

        retry_text_file_busy(|| tailscale.mint_cert(&cert_path, &key_path))
            .expect("mint certificate");
        assert_eq!(std::fs::read_to_string(cert_path).unwrap(), "certificate");
        assert_eq!(std::fs::read_to_string(key_path).unwrap(), "private key");
    }

    #[cfg(unix)]
    #[test]
    fn mint_cert_reports_cli_failure() {
        let (_dir, binary) = fake_tailscale(
            r#"#!/bin/sh
printf '%s' 'certificate permission denied' >&2
exit 5
"#,
        );
        let output = tempfile::tempdir().expect("output tempdir");
        let tailscale = Tailscale {
            binary,
            cert_domain: "node.tail1234.ts.net".to_string(),
        };

        let error = retry_text_file_busy(|| {
            tailscale.mint_cert(
                &output.path().join("node.crt"),
                &output.path().join("node.key"),
            )
        })
        .expect_err("certificate failure");
        let message = error.to_string();
        assert!(message.contains("node.tail1234.ts.net"), "{message}");
        assert!(
            message.contains("certificate permission denied"),
            "{message}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn find_binary_in_path_finds_windows_exe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binary = dir.path().join("tailscale.exe");
        std::fs::write(&binary, "").expect("write binary");
        let path = std::env::join_paths([dir.path()]).expect("join path");
        assert_eq!(find_binary_in_path(&path), Some(binary));
    }
}
