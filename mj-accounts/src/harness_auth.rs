//! Whether a harness profile can reach its service without a login first, and
//! Claude Code's login in the macOS Keychain. Discovery, quota probes and
//! login staging all ask here.

use std::path::Path;

use crate::targets::{CommandExecutor, CommandSpec};
use mj_core::config::{HarnessHost, HarnessKind, HarnessProfile};

pub fn harness_is_authenticated_with_executor(
    profile: &HarnessProfile,
    executor: &impl CommandExecutor,
) -> bool {
    let is_default_home = mj_core::path_input::user_home()
        .is_some_and(|user_home| profile.home == user_home.join(profile.kind.default_home_leaf()));
    harness_is_authenticated_with(profile, is_default_home, executor)
}

/// Whether this profile can talk to its service without a login first.
///
/// An API-key profile is proven by its harness configuration file, because its
/// key lives in the profile's `environment` rather than in a credential file;
/// [`HarnessProfile::authentication_marker`] already names the right file for
/// either case. A harness without a login or a configuration file, such as
/// DSH, is proven by its key alone.
pub fn harness_is_authenticated_with(
    profile: &HarnessProfile,
    is_default_home: bool,
    executor: &impl CommandExecutor,
) -> bool {
    let kind = profile.kind;
    let home = profile.home.as_path();
    // AWS credentials can come from a role or an external credential process;
    // there is no harness login file. Doctor probes the chain separately.
    if profile.auth_scheme() == mj_core::config::AuthScheme::AwsCredentialChain {
        return true;
    }
    // OpenCode keeps its logins in its own database, or takes a provider key
    // from its environment; doctor reports a database it cannot read.
    if profile.auth_scheme() == mj_core::config::AuthScheme::NativeLoginStore {
        return crate::opencode_logins::profile_logins(profile)
            .is_ok_and(|logins| logins.is_some())
            || !profile.opencode_environment_logins().is_empty();
    }
    // A harness without a login reads its key from its environment, so no
    // file can prove it; the key itself does.
    if let Some(env_key) = kind.api_key_variable() {
        return profile.supplies_api_key(env_key);
    }
    // On a Mac the Keychain item is the only Claude login. A
    // `.credentials.json` there (a profile home's, left by earlier releases,
    // or Claude Code's plaintext fallback) is not one, so the marker proves
    // nothing, and every profile shares the item: asking the CLI about a
    // scoped home would only report the default profile's state again.
    if matches!(
        kind.login_store(HarnessHost::current(), home),
        mj_core::config::HarnessLoginStore::MacKeychain
    ) {
        return claude_keychain_reports_authenticated(executor);
    }
    if profile.authentication_marker().is_file()
        || (kind == HarnessKind::Kimi && home.join("credentials").is_file())
    {
        return true;
    }
    if kind != HarnessKind::Claude {
        return false;
    }
    // The default home on a host without a Keychain has no marker, so there is
    // no login: setting `CLAUDE_CONFIG_DIR` for the default home would change
    // Claude's profile selection.
    if is_default_home {
        return false;
    }
    claude_cli_reports_authenticated(home, executor)
}

/// Ask Claude Code about a scoped profile. Setting `CLAUDE_CONFIG_DIR` for the
/// default home changes Claude's profile selection, so the default home is
/// never asked.
pub fn claude_cli_reports_authenticated(home: &Path, executor: &impl CommandExecutor) -> bool {
    let mut command = CommandSpec::new("claude", ["auth", "status", "--json"])
        .purpose("check Claude Code authentication");
    command.env.insert(
        HarnessKind::Claude.home_env().to_owned(),
        home.to_string_lossy().into_owned(),
    );
    let Ok(output) = executor.execute(&command) else {
        return false;
    };
    if output.status != 0 {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .ok()
        .and_then(|status| status.get("loggedIn").and_then(serde_json::Value::as_bool))
        == Some(true)
}

/// Whether the Keychain holds a Claude login. `security` is already
/// authorized for the item, so this does not raise a Keychain prompt; the
/// shared executor still bounds a wedged lookup.
pub fn claude_keychain_reports_authenticated(executor: &impl CommandExecutor) -> bool {
    claude_keychain_credentials(executor)
        .ok()
        .flatten()
        .is_some_and(|credentials| claude_credentials_contain_login(&credentials))
}

/// What the macOS Keychain said about Claude Code's login.
pub enum KeychainLogin {
    /// No such item (`security` exit 44).
    Missing,
    /// The item could not be read: the Keychain is locked (exit 36) or
    /// `security` failed another way. Claude Code reads that as no login, but
    /// nobody should act on it as if the login were gone.
    Unreadable { status: i32 },
    /// The login, which the caller owns and must not log.
    Present(Vec<u8>),
}

/// The account of Claude Code's Keychain item, by Claude Code's own rule. The
/// reader and the writer ([`crate::claude_login::write_claude_keychain_login`]) share it, so both
/// address the item Claude Code uses.
pub fn claude_keychain_account() -> String {
    // Claude Code takes `$USER || os.userInfo().username`.
    let user = std::env::var("USER")
        .ok()
        .filter(|user| !user.is_empty())
        .or_else(|| mj_harness_install::login_environment::account_name().ok());
    mj_core::config::claude_keychain_account(user.as_deref())
}

/// How long a `security` lookup of the Keychain login may take before it is
/// treated as wedged (it can wait on a Keychain prompt). The same bound as the
/// other host credential lookups (the AWS export).
pub const KEYCHAIN_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The `security` exit status for "item not found".
const SECURITY_ITEM_NOT_FOUND: i32 = 44;

/// Read Claude Code's login from the macOS Keychain item every profile
/// shares ([`mj_core::config::CLAUDE_KEYCHAIN_SERVICE`]). This is the one
/// reader; the callers differ only in what an unreadable Keychain means to
/// them. It fails only when `security` cannot run or the item holds no
/// usable login.
pub fn read_claude_keychain_login(
    executor: &(impl CommandExecutor + ?Sized),
) -> anyhow::Result<KeychainLogin> {
    let mut command = CommandSpec::new(
        "security",
        [
            "find-generic-password",
            "-a",
            &claude_keychain_account(),
            "-s",
            mj_core::config::CLAUDE_KEYCHAIN_SERVICE,
            "-w",
        ],
    )
    .purpose("read Claude Code authentication from the macOS Keychain");
    command.sensitive_output = true;
    let output = executor
        .execute(&command)
        .map_err(|error| error.context("read Claude Code's login from the macOS Keychain"))?;
    if output.status == SECURITY_ITEM_NOT_FOUND {
        return Ok(KeychainLogin::Missing);
    }
    if output.status != 0 {
        return Ok(KeychainLogin::Unreadable {
            status: output.status,
        });
    }
    if !claude_credentials_contain_login(&output.stdout) {
        anyhow::bail!(
            "the macOS Keychain item \"{}\" does not contain a usable Claude login",
            mj_core::config::CLAUDE_KEYCHAIN_SERVICE
        );
    }
    Ok(KeychainLogin::Present(output.stdout))
}

/// Claude Code's Keychain login for a reader that, like Claude Code, treats
/// an unreadable Keychain as no login: staging a copy and the authentication
/// check. The caller owns the returned copy and must not log it.
pub fn claude_keychain_credentials(
    executor: &impl CommandExecutor,
) -> anyhow::Result<Option<Vec<u8>>> {
    Ok(match read_claude_keychain_login(executor)? {
        KeychainLogin::Present(credentials) => Some(credentials),
        KeychainLogin::Missing => None,
        KeychainLogin::Unreadable { status } => {
            tracing::warn!(
                status,
                "the macOS Keychain could not be read; Claude Code's login is treated as absent"
            );
            None
        }
    })
}

pub fn claude_credentials_contain_login(credentials: &[u8]) -> bool {
    let Ok(document) = serde_json::from_slice::<serde_json::Value>(credentials) else {
        return false;
    };
    [
        "/claudeAiOauth/accessToken",
        "/claudeAiOauth/refreshToken",
        "/oauth/accessToken",
        "/apiKey",
    ]
    .into_iter()
    .any(|pointer| {
        document
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    })
}
