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

#[cfg(test)]
mod tests {
    use super::lower_hex;

    #[test]
    fn lower_hex_pads_every_byte_to_two_lowercase_digits() {
        assert_eq!(lower_hex([0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(lower_hex([] as [u8; 0]), "");
    }
}
