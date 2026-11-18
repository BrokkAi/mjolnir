//! Process-wide key material: the rustls provider every executable installs,
//! and the signing key that phone cookies and AWS credential scopes derive from.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Select the process-wide rustls provider before any TLS configuration is built.
///
/// Dependency feature unification can enable both rustls providers. Rustls
/// deliberately refuses to guess in that case, so each executable that links
/// the controller installs the ring provider at process startup. A provider
/// installed even earlier is already sufficient and remains in place.
pub fn install_rustls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub(crate) const COOKIE_KEY_BYTES: usize = 32;
const COOKIE_KEY_FILE: &str = "phone-cookie-key";

pub fn cookie_key_path() -> PathBuf {
    mj_core::config::data_dir().join(COOKIE_KEY_FILE)
}

/// Load the phone cookie signing key, creating it on first use.
///
/// This signing key keeps a signed-in phone signed in across daemon restarts.
/// Logged-out identities are tracked separately. Deleting the key is
/// therefore the explicit sign-everyone-out gesture: the next start writes a
/// new key and every outstanding cookie stops validating. A missing file is
/// ordinary first use; an unreadable or too-short one is replaced loudly,
/// because refusing to start would be a worse answer than asking phones to
/// enter the viewer code again.
///
/// The key has one creator: a file lock beside it serializes every caller in
/// every process, and the key is re-read under the lock, so concurrent first
/// uses (the phone server starting while sessions launch, parallel renewals)
/// all end with the key the first of them wrote. The key file itself is
/// replaced atomically, so the unlocked read below never sees a partial key.
pub fn load_or_create_cookie_key(path: &std::path::Path) -> Result<Vec<u8>> {
    if let Ok(key) = std::fs::read(path)
        && key.len() >= COOKIE_KEY_BYTES
    {
        return Ok(key);
    }
    let _creating = lock_cookie_key(path)?;
    match std::fs::read(path) {
        Ok(key) if key.len() >= COOKIE_KEY_BYTES => return Ok(key),
        Ok(key) => tracing::warn!(
            path = %path.display(),
            bytes = key.len(),
            "phone cookie key is shorter than {COOKIE_KEY_BYTES} bytes; generating a new key signs every phone out"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            path = %path.display(),
            "could not read the phone cookie key ({error}); generating a new key signs every phone out"
        ),
    }
    let key = generate_cookie_key()?;
    mj_core::config::atomic_write(path, &key)
        .with_context(|| format!("persist Mjolnir phone cookie key {}", path.display()))?;
    Ok(key.to_vec())
}

/// The lock that makes one caller the creator of the key at `path`.
fn lock_cookie_key(path: &std::path::Path) -> Result<mj_core::file_lock::FileLock> {
    let mut lock_name = path.as_os_str().to_owned();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);
    if let Some(parent) = lock_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create phone cookie key directory {}", parent.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options
        .open(&lock_path)
        .with_context(|| format!("open phone cookie key lock {}", lock_path.display()))?;
    file.lock()
        .with_context(|| format!("lock phone cookie key {}", path.display()))?;
    Ok(mj_core::file_lock::FileLock::new(file))
}

pub(crate) fn generate_cookie_key() -> Result<[u8; COOKIE_KEY_BYTES]> {
    let mut key = [0_u8; COOKIE_KEY_BYTES];
    getrandom::fill(&mut key)
        .map_err(|error| anyhow::anyhow!("generate Mjolnir cookie key: {error}"))?;
    Ok(key)
}
