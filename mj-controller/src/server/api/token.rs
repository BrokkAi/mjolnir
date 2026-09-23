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
    let token = mj_core::hex::lower_hex(bytes);
    mj_core::config::atomic_write(path, token.as_bytes())
        .with_context(|| format!("persist Mjolnir API token {}", path.display()))?;
    Ok(token)
}

/// The pin a local API client checks the viewer's certificate against:
/// lowercase hex SHA-256 of the certificate's DER bytes.
pub fn certificate_der_sha256(der: &[u8]) -> String {
    use sha2::Digest;
    mj_core::hex::lower_hex(sha2::Sha256::digest(der))
}

/// Pin the first (leaf) certificate of a PEM chain the viewer serves.
///
/// The daemon publishes this with the viewer URL so the CLI can trust exactly
/// the certificate the operator configured, including a self-signed one that
/// no CA chain, and no `CA:FALSE` rule, would accept.
pub fn served_certificate_sha256(pem: &[u8]) -> AnyResult<String> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    let leaf = CertificateDer::pem_slice_iter(pem)
        .next()
        .context("the certificate file holds no PEM certificate")?
        .context("parse the PEM certificate")?;
    Ok(certificate_der_sha256(&leaf))
}

// ---------------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------------
