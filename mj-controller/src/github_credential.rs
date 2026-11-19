//! The GitHub credential of the person running the daemon: a token from the
//! environment, or the GitHub CLI's login.

use std::time::Duration;
/// Where the controller's GitHub credential came from, so an error about a
/// rejected token can name the setting to fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubTokenSource {
    GhTokenEnv,
    GithubTokenEnv,
    GhCli,
}

impl std::fmt::Display for GithubTokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::GhTokenEnv => "GH_TOKEN",
            Self::GithubTokenEnv => "GITHUB_TOKEN",
            Self::GhCli => "the GitHub CLI login",
        })
    }
}

pub struct GithubCredential {
    pub(crate) token: String,
    pub(crate) source: GithubTokenSource,
}

impl std::fmt::Debug for GithubCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubCredential")
            .field("token", &"<redacted>")
            .field("source", &self.source)
            .finish()
    }
}

/// `gh auth token` reads local state; a keychain prompt or a wedged credential
/// helper must not hold the caller, and the bounded runner kills it on expiry.
pub const GH_AUTH_TOKEN_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn controller_github_token() -> Option<String> {
    controller_github_credential()
        .await
        .map(|credential| credential.token)
}

pub async fn controller_github_credential() -> Option<GithubCredential> {
    for (name, source) in [
        ("GH_TOKEN", GithubTokenSource::GhTokenEnv),
        ("GITHUB_TOKEN", GithubTokenSource::GithubTokenEnv),
    ] {
        if let Ok(token) = std::env::var(name)
            && let Some(token) = usable_github_token(&token)
        {
            return Some(GithubCredential {
                token: token.to_owned(),
                source,
            });
        }
    }
    let mut command = tokio::process::Command::new("gh");
    command.args(["auth", "token", "--hostname", "github.com"]);
    let output = match mj_core::subprocess::run_bounded(
        &mut command,
        64 * 1024,
        GH_AUTH_TOKEN_TIMEOUT,
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            tracing::debug!(
                error = format!("{error:#}"),
                "could not query the GitHub CLI for a token"
            );
            return None;
        }
    };
    if !output.status.success() {
        tracing::debug!(status = ?output.status, "GitHub CLI did not return an authenticated token");
        return None;
    }
    let token = match std::str::from_utf8(&output.stdout) {
        Ok(token) => token,
        Err(error) => {
            tracing::debug!(%error, "GitHub CLI returned a non-UTF-8 token");
            return None;
        }
    };
    let Some(token) = usable_github_token(token) else {
        tracing::debug!("GitHub CLI returned an empty or invalid token");
        return None;
    };
    Some(GithubCredential {
        token: token.to_owned(),
        source: GithubTokenSource::GhCli,
    })
}

/// A token travels in an HTTP header and a git credential line, so only
/// visible ASCII qualifies; a stray control or non-ASCII byte is no token.
pub fn usable_github_token(token: &str) -> Option<&str> {
    let token = token.trim();
    (!token.is_empty() && token.bytes().all(|byte| byte.is_ascii_graphic())).then_some(token)
}
