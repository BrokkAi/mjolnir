//! The session GitHub token a person configured for their sessions: reading
//! it on the controller's machine and installing it beside a worker.

use super::*;
use mj_core::config::GithubConfig;
use mj_core::targets::SshTrafficClass;
use std::fs;

/// Read the controller-side override afresh; missing is an error when configured.
pub fn configured_session_github_token(github: &GithubConfig) -> Result<Option<String>> {
    use std::io::Read;

    let Some(path) = &github.session_token_path else {
        return Ok(None);
    };
    let path =
        mj_core::path_input::expand_local(path).context("resolve [github] session_token_path")?;
    let read = || -> Result<String> {
        ensure!(
            fs::metadata(&path)?.is_file(),
            "session token must be a regular file"
        );
        let file = fs::File::open(&path)?;
        let mut bytes = Vec::new();
        file.take(mj_core::credentials::MAX_GITHUB_TOKEN_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok(mj_core::credentials::validate_github_token(&bytes)?.to_owned())
    };
    read()
        .map(Some)
        .with_context(|| format!("read [github] session_token_path {}", path.display()))
}

/// Refresh the override before a worker starts, including retained workers.
/// Lifecycle ownership excludes periodic reconciliation while this runs.
pub fn install_configured_session_github_token(
    github: &GithubConfig,
    executor: &impl CommandExecutor,
    locator: &mj_core::targets::TargetLocator,
    worker_root: &str,
) -> Result<()> {
    let Some(token) = configured_session_github_token(github)? else {
        return Ok(());
    };
    write_session_github_token(&token, executor, locator, worker_root)
}

/// Write a GitHub token beside a worker whose root must already exist.
///
/// The caller owns where the token came from; a retained session with no live
/// actor still receives a renewed App token this way.
pub fn write_session_github_token(
    token: &str,
    executor: &impl CommandExecutor,
    locator: &mj_core::targets::TargetLocator,
    worker_root: &str,
) -> Result<()> {
    const INSTALL: &str = r#"set -eu
umask 077
root=$1
path=$root/github-token
if [ -L "$path" ] || { [ -e "$path" ] && [ ! -f "$path" ]; }; then
    echo 'session GitHub token is not a regular file' >&2; exit 1
fi
[ -d "$root" ] || { echo 'session worker root is missing' >&2; exit 1; }
temporary=$(mktemp "$root/github-token.XXXXXXXX")
trap 'rm -f -- "$temporary"' EXIT
cat > "$temporary"
mv -f -- "$temporary" "$path"
"#;
    let mut bytes = token.as_bytes().to_vec();
    bytes.push(b'\n');
    // A token is a few bytes and a session waits on it, so it must not queue
    // behind bulk transfers to the same target.
    let command = targets::locator_command(
        SshTrafficClass::Control,
        locator,
        vec![
            "sh".into(),
            "-c".into(),
            INSTALL.into(),
            "mj-session-github-token".into(),
            worker_root.into(),
        ],
    )
    .purpose("install configured session GitHub token")
    .with_sensitive_stdin(bytes);
    execute_checked(executor, command)?;
    Ok(())
}
