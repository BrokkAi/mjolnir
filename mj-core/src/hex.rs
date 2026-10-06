//! Lowercase hexadecimal encoding.
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
