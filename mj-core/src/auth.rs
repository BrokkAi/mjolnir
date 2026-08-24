//! Vendor-owned account discovery and login command selection.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthVendor {
    OpenAi,
    Anthropic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebLoginMode {
    OpenAiDevice,
    ClaudeSubscription,
    AnthropicConsole,
}

impl WebLoginMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenAiDevice => "device",
            Self::ClaudeSubscription => "subscription",
            Self::AnthropicConsole => "console",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAiDevice => "Sign in",
            Self::ClaudeSubscription => "Claude subscription",
            Self::AnthropicConsole => "Anthropic Console",
        }
    }
}

impl AuthVendor {
    pub const ALL: [Self; 2] = [Self::OpenAi, Self::Anthropic];

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI / ChatGPT",
            Self::Anthropic => "Anthropic / Claude",
        }
    }

    pub fn enables(self) -> &'static str {
        match self {
            Self::OpenAi => "Codex",
            Self::Anthropic => "Claude",
        }
    }

    pub fn acp_source(self) -> &'static str {
        match self {
            Self::OpenAi => "codex-acp",
            Self::Anthropic => "claude-acp",
        }
    }

    /// Stable wire identifier used by the remote-control API.
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|vendor| vendor.id() == id)
    }

    /// Whether the remote viewer can complete this vendor's login. OpenAI uses
    /// device auth; Claude streams its authorization prompt to the viewer and
    /// accepts the pasted code there.
    pub fn supports_web_login(self) -> bool {
        !self.web_login_modes().is_empty()
    }

    /// Claude's authorization command waits for the code produced by its
    /// browser flow. OpenAI device auth only needs captured output.
    pub fn web_login_accepts_input(self) -> bool {
        matches!(self, Self::Anthropic)
    }

    pub fn web_login_modes(self) -> &'static [WebLoginMode] {
        match self {
            Self::OpenAi => &[WebLoginMode::OpenAiDevice],
            Self::Anthropic => &[
                WebLoginMode::ClaudeSubscription,
                WebLoginMode::AnthropicConsole,
            ],
        }
    }

    pub fn web_login_mode(self, id: Option<&str>) -> Option<WebLoginMode> {
        match id {
            Some(id) => self
                .web_login_modes()
                .iter()
                .copied()
                .find(|mode| mode.id() == id),
            None => self.web_login_modes().first().copied(),
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
        AuthVendor::Anthropic => detect_anthropic(),
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

fn detect_anthropic() -> CredentialSource {
    let configured = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    let home = dirs::home_dir();
    let root = configured
        .clone()
        .or_else(|| home.as_ref().map(|home| home.join(".claude")));
    detect_anthropic_with(
        nonempty_env("CLAUDE_CODE_OAUTH_TOKEN"),
        nonempty_env("ANTHROPIC_API_KEY"),
        root,
        configured
            .map(|root| root.join(".claude.json"))
            .or_else(|| home.map(|home| home.join(".claude.json"))),
    )
}

fn detect_anthropic_with(
    has_oauth_token: bool,
    has_api_key: bool,
    root: Option<PathBuf>,
    legacy_config: Option<PathBuf>,
) -> CredentialSource {
    if has_oauth_token {
        return CredentialSource::Environment("CLAUDE_CODE_OAUTH_TOKEN");
    }
    if has_api_key {
        return CredentialSource::Environment("ANTHROPIC_API_KEY");
    }
    if let Some(root) = root {
        let credentials = root.join(".credentials.json");
        if credential_file_has_any(
            &credentials,
            &[
                "/claudeAiOauth/accessToken",
                "/claudeAiOauth/refreshToken",
                "/oauth/accessToken",
                "/apiKey",
            ],
        ) {
            return CredentialSource::File(credentials);
        }
        let config = root.join(".config.json");
        if credential_file_has_any(
            &config,
            &[
                "/oauthAccount/accountUuid",
                "/oauthAccount/organizationUuid",
            ],
        ) {
            return CredentialSource::File(config);
        }
    }
    detect_file(
        legacy_config,
        &[
            "/oauthAccount/accountUuid",
            "/oauthAccount/organizationUuid",
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

/// Login invocation for the remote viewer. The command runs server-side and
/// streams its output to the browser; Claude additionally receives its pasted
/// authorization code through the viewer.
pub struct LoginInvocation {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: std::collections::HashMap<String, String>,
}

pub async fn web_login_invocation(
    vendor: AuthVendor,
    mode: WebLoginMode,
) -> Result<LoginInvocation> {
    let mut invocation = bundled_invocation(vendor).await?;
    configure_web_login_invocation(vendor, mode, &mut invocation)?;
    Ok(invocation)
}

fn configure_web_login_invocation(
    vendor: AuthVendor,
    mode: WebLoginMode,
    invocation: &mut LoginInvocation,
) -> Result<()> {
    if !vendor.web_login_modes().contains(&mode) {
        bail!(
            "{} does not support {} web sign-in",
            vendor.label(),
            mode.id()
        );
    }
    append_login_args(invocation, web_login_args(mode));
    if vendor == AuthVendor::Anthropic {
        // The browser is on the viewer's machine, so a localhost OAuth
        // callback would return to the wrong host. Force Claude's manual-code
        // flow and send the pasted code through the protected viewer API.
        invocation
            .env
            .insert("NO_BROWSER".to_string(), "1".to_string());
    }
    Ok(())
}

pub fn login_args_for_selection(selected: usize) -> &'static [&'static str] {
    if selected == 1 {
        &["login", "--device-auth"]
    } else {
        &["login"]
    }
}

pub fn append_login_args(invocation: &mut LoginInvocation, args: &[&str]) {
    invocation
        .args
        .extend(args.iter().map(|arg| arg.to_string()));
}

fn web_login_args(mode: WebLoginMode) -> &'static [&'static str] {
    match mode {
        WebLoginMode::OpenAiDevice => &["login", "--device-auth"],
        WebLoginMode::ClaudeSubscription => &["auth", "login", "--claudeai"],
        WebLoginMode::AnthropicConsole => &["auth", "login", "--console"],
    }
}

/// Extra guidance printed before handing the terminal to the vendor CLI. The
/// Claude CLI reads the authorization code without echoing it, which looks
/// like a frozen prompt after a paste.
pub fn login_terminal_hint(vendor: AuthVendor) -> Option<&'static str> {
    match vendor {
        AuthVendor::OpenAi => None,
        AuthVendor::Anthropic => Some(
            "Note: the authorization code will not appear when you paste it — paste and press Enter.",
        ),
    }
}

pub fn anthropic_login_args(use_console: bool) -> &'static [&'static str] {
    if use_console {
        &["auth", "login", "--console"]
    } else {
        &["auth", "login", "--claudeai"]
    }
}

pub fn login_outcome_from_status(
    vendor: AuthVendor,
    success: bool,
    status: &str,
    credentials_available: bool,
) -> Result<LoginOutcome> {
    if !success {
        bail!("{} login exited with {status}", vendor.label());
    }
    if !credentials_available {
        bail!(
            "{} login finished but no supported credential was found",
            vendor.label()
        );
    }
    Ok(LoginOutcome::SignedIn(format!(
        "Signed in to {}; adapters reprobe on /new or /clear",
        vendor.label()
    )))
}

pub async fn bundled_invocation(vendor: AuthVendor) -> Result<LoginInvocation> {
    let provider = bundled_provider(vendor);
    let prepared = crate::acp::prepare_provider_cli(provider, &Default::default())
        .await
        .with_context(|| format!("prepare bundled {} CLI", vendor.label()))?;
    Ok(login_invocation_from_prepared(prepared))
}

fn bundled_provider(vendor: AuthVendor) -> crate::acp::ProviderCli {
    match vendor {
        AuthVendor::OpenAi => crate::acp::ProviderCli::Codex,
        AuthVendor::Anthropic => crate::acp::ProviderCli::Claude,
    }
}

fn login_invocation_from_prepared(prepared: crate::acp::PreparedProviderCli) -> LoginInvocation {
    LoginInvocation {
        command: prepared.command,
        args: prepared.args,
        env: prepared.env,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendors_report_labels_and_capabilities() {
        assert_eq!(AuthVendor::ALL, [AuthVendor::OpenAi, AuthVendor::Anthropic]);
        assert_eq!(AuthVendor::OpenAi.label(), "OpenAI / ChatGPT");
        assert_eq!(AuthVendor::OpenAi.enables(), "Codex");
        assert_eq!(AuthVendor::OpenAi.id(), "openai");
        assert_eq!(AuthVendor::OpenAi.acp_source(), "codex-acp");
        assert_eq!(AuthVendor::from_id("openai"), Some(AuthVendor::OpenAi));
        assert_eq!(AuthVendor::Anthropic.label(), "Anthropic / Claude");
        assert_eq!(AuthVendor::Anthropic.enables(), "Claude");
        assert_eq!(AuthVendor::Anthropic.id(), "anthropic");
        assert_eq!(AuthVendor::Anthropic.acp_source(), "claude-acp");
        assert!(AuthVendor::OpenAi.supports_web_login());
        assert!(AuthVendor::Anthropic.supports_web_login());
        assert!(!AuthVendor::OpenAi.web_login_accepts_input());
        assert!(AuthVendor::Anthropic.web_login_accepts_input());
        assert_eq!(
            AuthVendor::OpenAi.web_login_mode(None),
            Some(WebLoginMode::OpenAiDevice)
        );
        assert_eq!(
            AuthVendor::Anthropic.web_login_mode(Some("console")),
            Some(WebLoginMode::AnthropicConsole)
        );
        assert_eq!(AuthVendor::Anthropic.web_login_mode(Some("unknown")), None);
        assert_eq!(
            AuthVendor::from_id("anthropic"),
            Some(AuthVendor::Anthropic)
        );
        assert_eq!(
            bundled_provider(AuthVendor::Anthropic),
            crate::acp::ProviderCli::Claude
        );
        assert_eq!(AuthVendor::from_id("unknown"), None);
    }

    #[test]
    fn vendors_select_their_supported_login_commands() {
        assert_eq!(
            web_login_args(WebLoginMode::OpenAiDevice),
            ["login", "--device-auth"]
        );
        assert_eq!(
            web_login_args(WebLoginMode::ClaudeSubscription),
            ["auth", "login", "--claudeai"]
        );
        assert_eq!(
            web_login_args(WebLoginMode::AnthropicConsole),
            ["auth", "login", "--console"]
        );
        assert_eq!(anthropic_login_args(false), ["auth", "login", "--claudeai"]);
        assert_eq!(anthropic_login_args(true), ["auth", "login", "--console"]);
    }

    #[test]
    fn claude_login_warns_that_the_pasted_code_will_not_echo() {
        assert!(login_terminal_hint(AuthVendor::OpenAi).is_none());
        let hint = login_terminal_hint(AuthVendor::Anthropic).expect("Claude login hint");
        assert!(hint.contains("will not appear"), "{hint}");
        assert!(hint.contains("press Enter"), "{hint}");
    }

    #[test]
    fn login_arguments_preserve_bundled_launcher_arguments() {
        let prepared = crate::acp::PreparedProviderCli {
            command: PathBuf::from("npx"),
            args: vec!["--package=codex-acp".to_string(), "codex".to_string()],
            env: [("NPM_CONFIG_CACHE".to_string(), "/tmp/npm".to_string())]
                .into_iter()
                .collect(),
        };
        let mut invocation = login_invocation_from_prepared(prepared);

        append_login_args(&mut invocation, login_args_for_selection(1));

        assert_eq!(invocation.command, PathBuf::from("npx"));
        assert_eq!(
            invocation.args,
            ["--package=codex-acp", "codex", "login", "--device-auth"]
        );
        assert_eq!(invocation.env["NPM_CONFIG_CACHE"], "/tmp/npm");

        let mut browser = LoginInvocation {
            command: PathBuf::from("npx"),
            args: Vec::new(),
            env: Default::default(),
        };
        append_login_args(&mut browser, login_args_for_selection(0));
        assert_eq!(browser.args, ["login"]);
        assert_eq!(
            bundled_provider(AuthVendor::OpenAi),
            crate::acp::ProviderCli::Codex
        );

        let mut claude = LoginInvocation {
            command: PathBuf::from("npx"),
            args: vec!["--cli".to_string()],
            env: Default::default(),
        };
        configure_web_login_invocation(
            AuthVendor::Anthropic,
            WebLoginMode::ClaudeSubscription,
            &mut claude,
        )
        .expect("configure Claude web login");
        assert_eq!(claude.args, ["--cli", "auth", "login", "--claudeai"]);
        assert_eq!(claude.env["NO_BROWSER"], "1");
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
        assert!(matches!(&signed_in, LoginOutcome::SignedIn(_)));
        assert_eq!(signed_in.into_message(), "connected");

        let cancelled = LoginOutcome::Cancelled("cancelled".to_string());
        assert!(matches!(&cancelled, LoginOutcome::Cancelled(_)));
        assert_eq!(cancelled.into_message(), "cancelled");

        let failed = login_outcome_from_status(AuthVendor::OpenAi, false, "exit status: 1", false)
            .unwrap_err();
        assert!(failed.to_string().contains("login exited"));
        let missing =
            login_outcome_from_status(AuthVendor::OpenAi, true, "success", false).unwrap_err();
        assert!(missing.to_string().contains("no supported credential"));
        assert_eq!(
            login_outcome_from_status(AuthVendor::OpenAi, true, "success", true).unwrap(),
            LoginOutcome::SignedIn(
                "Signed in to OpenAI / ChatGPT; adapters reprobe on /new or /clear".to_string()
            )
        );
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
        assert_eq!(
            detect_openai_with(false, false, Some(dir.path().join("missing"))),
            CredentialSource::Missing
        );
    }

    #[test]
    fn anthropic_detection_prefers_environment_then_current_and_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".claude");
        std::fs::create_dir(&root).unwrap();
        let credentials = root.join(".credentials.json");
        std::fs::write(
            &credentials,
            r#"{"claudeAiOauth":{"refreshToken":"refresh"}}"#,
        )
        .unwrap();
        let legacy = dir.path().join(".claude.json");
        std::fs::write(&legacy, r#"{"oauthAccount":{"accountUuid":"account"}}"#).unwrap();

        assert_eq!(
            detect_anthropic_with(true, true, Some(root.clone()), Some(legacy.clone())),
            CredentialSource::Environment("CLAUDE_CODE_OAUTH_TOKEN")
        );
        assert_eq!(
            detect_anthropic_with(false, true, Some(root.clone()), Some(legacy.clone())),
            CredentialSource::Environment("ANTHROPIC_API_KEY")
        );
        assert_eq!(
            detect_anthropic_with(false, false, Some(root.clone()), Some(legacy.clone())),
            CredentialSource::File(credentials)
        );

        std::fs::remove_file(root.join(".credentials.json")).unwrap();
        let scoped_config = root.join(".config.json");
        std::fs::write(
            &scoped_config,
            r#"{"oauthAccount":{"organizationUuid":"organization"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_anthropic_with(false, false, Some(root.clone()), Some(legacy.clone())),
            CredentialSource::File(scoped_config.clone())
        );

        std::fs::remove_file(scoped_config).unwrap();
        assert_eq!(
            detect_anthropic_with(false, false, Some(root), Some(legacy.clone())),
            CredentialSource::File(legacy)
        );
        assert_eq!(
            detect_anthropic_with(false, false, None, None),
            CredentialSource::Missing
        );
    }

    #[test]
    fn public_detection_covers_each_vendor() {
        for vendor in AuthVendor::ALL {
            let source = detect(vendor);
            assert_eq!(
                source.available(),
                !matches!(source, CredentialSource::Missing)
            );
        }
    }
}
