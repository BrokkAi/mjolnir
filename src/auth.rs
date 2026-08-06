//! Vendor-owned account discovery and login command selection.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthVendor {
    OpenAi,
}

impl AuthVendor {
    pub const ALL: [Self; 1] = [Self::OpenAi];

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI / ChatGPT",
        }
    }

    pub fn enables(self) -> &'static str {
        match self {
            Self::OpenAi => "Codex",
        }
    }

    /// Stable wire identifier used by the remote-control API.
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|vendor| vendor.id() == id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    Environment(&'static str),
    File(PathBuf),
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginOutcome {
    SignedIn(String),
    Cancelled(String),
}

impl LoginOutcome {
    pub fn succeeded(&self) -> bool {
        matches!(self, Self::SignedIn(_))
    }

    pub fn into_message(self) -> String {
        match self {
            Self::SignedIn(message) | Self::Cancelled(message) => message,
        }
    }
}

impl CredentialSource {
    pub fn available(&self) -> bool {
        !matches!(self, Self::Missing)
    }

    pub fn status(&self) -> String {
        match self {
            Self::Environment(name) => format!("signed in via {name}"),
            Self::File(_) => "signed in".to_string(),
            Self::Missing => "sign in".to_string(),
        }
    }
}

pub fn detect(vendor: AuthVendor) -> CredentialSource {
    match vendor {
        AuthVendor::OpenAi => detect_openai(),
    }
}

pub fn executable(vendor: AuthVendor) -> Option<PathBuf> {
    let name = match vendor {
        AuthVendor::OpenAi => "codex",
    };
    find_on_path(name)
}

pub fn install_hint(vendor: AuthVendor) -> &'static str {
    match vendor {
        AuthVendor::OpenAi => "npm install -g @openai/codex",
    }
}

fn detect_openai() -> CredentialSource {
    let root = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")));
    detect_openai_with(
        nonempty_env("CODEX_API_KEY"),
        nonempty_env("OPENAI_API_KEY"),
        root,
    )
}

fn detect_openai_with(
    has_codex_api_key: bool,
    has_openai_api_key: bool,
    root: Option<PathBuf>,
) -> CredentialSource {
    if has_codex_api_key {
        return CredentialSource::Environment("CODEX_API_KEY");
    }
    if has_openai_api_key {
        return CredentialSource::Environment("OPENAI_API_KEY");
    }
    detect_file(
        root.map(|root| root.join("auth.json")),
        &[
            "/OPENAI_API_KEY",
            "/tokens/access_token",
            "/tokens/refresh_token",
        ],
    )
}

fn detect_file(path: Option<PathBuf>, pointers: &[&str]) -> CredentialSource {
    let Some(path) = path else {
        return CredentialSource::Missing;
    };
    if credential_file_has_any(&path, pointers) {
        CredentialSource::File(path)
    } else {
        CredentialSource::Missing
    }
}

fn nonempty_env(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.to_string_lossy().trim().is_empty())
}

fn credential_file_has_any(path: &Path, pointers: &[&str]) -> bool {
    let Ok(contents) = std::fs::read(path) else {
        return false;
    };
    let Ok(document) = serde_json::from_slice::<serde_json::Value>(&contents) else {
        return false;
    };
    pointers.iter().any(|pointer| {
        document
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    })
}

pub fn find_on_path(name: &str) -> Option<PathBuf> {
    find_in_path(name, std::env::var_os("PATH").as_deref())
}

fn find_in_path(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let path = path?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        for extension in ["exe", "cmd", "bat"] {
            let candidate = directory.join(format!("{name}.{extension}"));
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Login invocation for contexts without an interactive terminal (the remote
/// viewer's sign-in runs the command server-side and streams its output to the
/// browser). OpenAI always uses the device-auth flow there: `codex login`
/// without it wants to open a local browser, which a headless server can't.
pub async fn headless_login_invocation(vendor: AuthVendor) -> Result<(PathBuf, Vec<String>)> {
    let command = executable(vendor).with_context(|| {
        format!(
            "{} CLI is not installed; run `{}`",
            vendor.label(),
            install_hint(vendor)
        )
    })?;
    let args = match vendor {
        AuthVendor::OpenAi => vec!["login".to_string(), "--device-auth".to_string()],
    };
    Ok((command, args))
}

pub async fn run_login(vendor: AuthVendor) -> Result<LoginOutcome> {
    let command = executable(vendor).with_context(|| {
        format!(
            "{} CLI is not installed; run `{}`",
            vendor.label(),
            install_hint(vendor)
        )
    })?;
    let args = match vendor {
        AuthVendor::OpenAi => {
            let options = [
                crate::menu::MenuOption {
                    label: "Browser",
                    hint: "codex login".to_string(),
                    shortcuts: &['b'],
                },
                crate::menu::MenuOption {
                    label: "Device code",
                    hint: "codex login --device-auth".to_string(),
                    shortcuts: &['d'],
                },
            ];
            let Some(selected) = crate::menu::select_inline_cancelable(
                "OpenAI / ChatGPT sign-in",
                "Enter confirms · Esc cancels",
                &options,
                0,
            )?
            else {
                return Ok(LoginOutcome::Cancelled(
                    "OpenAI / ChatGPT sign-in cancelled".to_string(),
                ));
            };
            if selected == 1 {
                vec!["login", "--device-auth"]
            } else {
                vec!["login"]
            }
        }
    };
    println!(
        "Signing in to {}. Mjolnir will return when it finishes.\n",
        vendor.label()
    );
    let _interrupt_guard = crate::termination::suppress_interrupts();
    let status = tokio::process::Command::new(&command)
        .args(&args)
        .status()
        .await
        .with_context(|| format!("run {} login", vendor.label()))?;
    if !status.success() {
        bail!("{} login exited with {status}", vendor.label());
    }
    if !detect(vendor).available() {
        bail!(
            "{} login finished but no supported credential was found",
            vendor.label()
        );
    }
    // Credentials can change the models advertised by both the vendor-native
    // bridge and any configured custom ACP servers. Clear every composite
    // launch key plus the process-local OnceCells instead of trying to remove
    // simple source IDs that do not match the persisted cache keys.
    crate::roster::invalidate_model_cache()
        .context("signed in, but failed to clear the model capability cache")?;
    Ok(LoginOutcome::SignedIn(format!(
        "Signed in to {}; models refresh on /new or /clear",
        vendor.label()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendors_report_labels_capabilities_and_install_hints() {
        assert_eq!(AuthVendor::ALL, [AuthVendor::OpenAi]);
        assert_eq!(AuthVendor::OpenAi.label(), "OpenAI / ChatGPT");
        assert_eq!(AuthVendor::OpenAi.enables(), "Codex");
        assert_eq!(
            install_hint(AuthVendor::OpenAi),
            "npm install -g @openai/codex"
        );
    }

    #[test]
    fn credential_source_reports_availability_and_status() {
        let environment = CredentialSource::Environment("TEST_API_KEY");
        assert!(environment.available());
        assert_eq!(environment.status(), "signed in via TEST_API_KEY");

        let file = CredentialSource::File(PathBuf::from("credentials.json"));
        assert!(file.available());
        assert_eq!(file.status(), "signed in");

        assert!(!CredentialSource::Missing.available());
        assert_eq!(CredentialSource::Missing.status(), "sign in");
    }

    #[test]
    fn login_outcome_distinguishes_success_from_cancellation() {
        let signed_in = LoginOutcome::SignedIn("connected".to_string());
        assert!(signed_in.succeeded());
        assert_eq!(signed_in.into_message(), "connected");

        let cancelled = LoginOutcome::Cancelled("cancelled".to_string());
        assert!(!cancelled.succeeded());
        assert_eq!(cancelled.into_message(), "cancelled");
    }

    #[test]
    fn credential_files_require_nonempty_strings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, r#"{"tokens":{"access_token":"token"}}"#).unwrap();
        assert!(credential_file_has_any(&path, &["/tokens/access_token"]));
        std::fs::write(&path, r#"{"tokens":{"access_token":"  "}}"#).unwrap();
        assert!(!credential_file_has_any(&path, &["/tokens/access_token"]));
    }

    #[test]
    fn credential_files_reject_missing_malformed_and_non_string_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        assert!(!credential_file_has_any(&path, &["/access_token"]));

        std::fs::write(&path, b"not json").unwrap();
        assert!(!credential_file_has_any(&path, &["/access_token"]));

        std::fs::write(&path, r#"{"access_token":42,"refresh_token":"token"}"#).unwrap();
        assert!(!credential_file_has_any(&path, &["/access_token"]));
        assert!(credential_file_has_any(
            &path,
            &["/access_token", "/refresh_token"]
        ));
    }

    #[test]
    fn openai_detection_prefers_environment_then_falls_back_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let path = root.join("auth.json");
        std::fs::write(&path, r#"{"tokens":{"refresh_token":"refresh"}}"#).unwrap();

        assert_eq!(
            detect_openai_with(true, true, Some(root.clone())),
            CredentialSource::Environment("CODEX_API_KEY")
        );
        assert_eq!(
            detect_openai_with(false, true, Some(root.clone())),
            CredentialSource::Environment("OPENAI_API_KEY")
        );
        assert_eq!(
            detect_openai_with(false, false, Some(root)),
            CredentialSource::File(path)
        );
        assert_eq!(
            detect_openai_with(false, false, None),
            CredentialSource::Missing
        );
    }

    #[test]
    fn path_lookup_skips_non_files_and_uses_first_matching_file() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::create_dir(first.path().join("helper")).unwrap();
        let executable = second.path().join("helper");
        std::fs::write(&executable, b"placeholder").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths([first.path(), second.path()]).unwrap();

        assert_eq!(find_in_path("helper", Some(&path)), Some(executable));
        assert_eq!(find_in_path("missing", Some(&path)), None);
        assert_eq!(find_in_path("helper", None), None);
    }

    #[cfg(unix)]
    #[test]
    fn path_lookup_skips_non_executable_files() {
        use std::os::unix::fs::PermissionsExt;

        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::write(first.path().join("helper"), b"not executable").unwrap();
        let executable = second.path().join("helper");
        std::fs::write(&executable, b"executable").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([first.path(), second.path()]).unwrap();

        assert_eq!(find_in_path("helper", Some(&path)), Some(executable));
    }

    #[test]
    fn public_detection_and_executable_lookup_cover_each_vendor() {
        for vendor in AuthVendor::ALL {
            let source = detect(vendor);
            assert_eq!(
                source.available(),
                !matches!(source, CredentialSource::Missing)
            );

            if let Some(path) = executable(vendor) {
                assert!(path.is_file());
            }
        }
    }
}
