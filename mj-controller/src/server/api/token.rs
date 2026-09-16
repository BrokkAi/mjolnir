use super::*;

/// Where the bearer token lives. It is a file rather than an environment
/// variable so it survives daemon restarts and so deleting it is the explicit
/// revoke gesture.
pub fn api_token_path() -> PathBuf {
    mj_core::config::data_dir().join(API_TOKEN_FILE)
}

/// Read the API bearer token, minting one on first use.
///
/// A missing file is ordinary first use. An unreadable or too-short one is
/// replaced loudly: refusing to start the daemon over a damaged token file
/// would be a worse answer than asking the caller to re-read the file.
pub fn load_or_create_api_token(path: &std::path::Path) -> AnyResult<String> {
    match std::fs::read_to_string(path) {
        Ok(token) if token.trim().len() >= 32 => return Ok(token.trim().to_owned()),
        Ok(token) => tracing::warn!(
            path = %path.display(),
            bytes = token.trim().len(),
            "Mjolnir API token is too short; generating a new one revokes the old token"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            path = %path.display(),
            "could not read the Mjolnir API token ({error}); generating a new one revokes the old token"
        ),
    }
    let mut bytes = [0_u8; API_TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("generate Mjolnir API token: {error}"))?;
    let token = hex_lower(&bytes);
    mj_core::config::atomic_write(path, token.as_bytes())
        .with_context(|| format!("persist Mjolnir API token {}", path.display()))?;
    Ok(token)
}

pub(super) fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

// ---------------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------------
