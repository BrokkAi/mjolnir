//! Vendor-owned account discovery and login command selection.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthVendor {
    OpenAi,
    Kimi,
}

impl AuthVendor {
    pub const ALL: [Self; 2] = [Self::OpenAi, Self::Kimi];

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI / ChatGPT",
            Self::Kimi => "Kimi",
        }
    }

    pub fn enables(self) -> &'static str {
        match self {
            Self::OpenAi => "Codex, Anvil",
            Self::Kimi => "Kimi Code, Anvil",
        }
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
        AuthVendor::Kimi => detect_kimi(),
    }
}

pub fn executable(vendor: AuthVendor) -> Option<PathBuf> {
    let name = match vendor {
        AuthVendor::OpenAi => "codex",
        AuthVendor::Kimi => "kimi",
    };
    find_on_path(name)
}

pub fn install_hint(vendor: AuthVendor) -> &'static str {
    match vendor {
        AuthVendor::OpenAi => "npm install -g @openai/codex",
        AuthVendor::Kimi => "install Kimi Code from the ACP registry",
    }
}

fn detect_openai() -> CredentialSource {
    for name in ["CODEX_API_KEY", "OPENAI_API_KEY"] {
        if nonempty_env(name) {
            return CredentialSource::Environment(name);
        }
    }
    let root = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")));
    detect_file(
        root.map(|root| root.join("auth.json")),
        &[
            "/OPENAI_API_KEY",
            "/tokens/access_token",
            "/tokens/refresh_token",
        ],
    )
}

fn detect_kimi() -> CredentialSource {
    if nonempty_env("KIMI_API_KEY") {
        return CredentialSource::Environment("KIMI_API_KEY");
    }
    let root = std::env::var_os("KIMI_CODE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".kimi-code")));
    detect_file(
        root.map(|root| root.join("credentials").join("kimi-code.json")),
        &["/access_token", "/refresh_token"],
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
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        for extension in ["exe", "cmd", "bat"] {
            let candidate = directory.join(format!("{name}.{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

pub async fn run_login(vendor: AuthVendor) -> Result<LoginOutcome> {
    let command = match vendor {
        AuthVendor::Kimi => match executable(vendor) {
            Some(command) => command,
            None => {
                println!("Preparing the official Kimi Code login helper...");
                crate::kimi::wait_until_ready().await?
            }
        },
        _ => executable(vendor).with_context(|| {
            format!(
                "{} CLI is not installed; run `{}`",
                vendor.label(),
                install_hint(vendor)
            )
        })?,
    };
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
        AuthVendor::Kimi => vec!["login"],
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
    // bridge and shared routes such as Anvil. Clear every composite launch key
    // plus the process-local OnceCells instead of trying to remove simple
    // source IDs that do not match the persisted cache keys.
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
}
