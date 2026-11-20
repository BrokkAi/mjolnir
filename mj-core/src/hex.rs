//! Lowercase hexadecimal encoding and file checksums.
//!
//! Digest crates dropped `LowerHex` on their output arrays, so every digest and
//! random-token rendering in the workspace goes through this one function
//! instead of a per-crate copy.

/// Encode `bytes` as lowercase hexadecimal, two characters per byte.
pub fn lower_hex(bytes: impl AsRef<[u8]>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

/// Read a file once to compute its SHA-256 checksum in lowercase hexadecimal.
pub fn file_sha256(path: &std::path::Path) -> anyhow::Result<String> {
    use anyhow::Context;
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open file for checksum {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("hash file {}", path.display()))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(lower_hex(digest.finalize()))
}
