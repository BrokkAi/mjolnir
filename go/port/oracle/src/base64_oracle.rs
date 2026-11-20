//! Oracle module `base64`. Requests are objects with an `"op"` member; see the
//! Go tests that use this module (`go/crates/base64/oracle_test.go`) for the
//! operations it serves.
//!
//! Bytes travel as lower-case hex (`"hex"`), so input that is not UTF-8 can
//! be decoded too. The engine is `"engine"`: one of the four
//! `general_purpose` constants (`standard`, `standard_no_pad`, `url_safe`,
//! `url_safe_no_pad`), or `custom`, built from `"alphabet"` (`standard`,
//! `url_safe`, `crypt`, `bcrypt`, `imap_mutf7`, `bin_hex`),
//! `"encode_padding"`, `"decode_allow_trailing_bits"` and
//! `"decode_padding_mode"` (`indifferent`, `canonical`, `none`).
//!
//! - `encode`: `encode`, `encode_string` onto a prefix, and `encode_slice`
//!   into a buffer of `"out_len"` bytes.
//! - `decode`: `decode`, `decode_vec` onto a prefix, `decode_slice` and
//!   `decode_slice_unchecked` into a buffer of `"out_len"` bytes; each as the
//!   bytes or the error's Display and Debug text (a panic of
//!   `decode_slice_unchecked` is caught and reported).
//! - `alphabet`: `Alphabet::new(text)`.
//! - `lengths`: `encoded_len` and `decoded_len_estimate` of `"len"`.

use base64::alphabet::{self, Alphabet};
use base64::engine::general_purpose::{self, GeneralPurpose, GeneralPurposeConfig};
use base64::engine::{DecodePaddingMode, Engine};
use serde_json::{Value, json};

pub fn handle(request: &Value) -> Value {
    match crate::text(request, "op") {
        "encode" => encode(request),
        "decode" => decode(request),
        "alphabet" => alphabet(request),
        "lengths" => lengths(request),
        _ => crate::unknown(request),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|index| u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex"))
        .collect()
}

fn alphabet_named(name: &str) -> &'static Alphabet {
    match name {
        "url_safe" => &alphabet::URL_SAFE,
        "crypt" => &alphabet::CRYPT,
        "bcrypt" => &alphabet::BCRYPT,
        "imap_mutf7" => &alphabet::IMAP_MUTF7,
        "bin_hex" => &alphabet::BIN_HEX,
        _ => &alphabet::STANDARD,
    }
}

fn engine(request: &Value) -> GeneralPurpose {
    match crate::text(request, "engine") {
        "standard" => general_purpose::STANDARD,
        "standard_no_pad" => general_purpose::STANDARD_NO_PAD,
        "url_safe" => general_purpose::URL_SAFE,
        "url_safe_no_pad" => general_purpose::URL_SAFE_NO_PAD,
        _ => {
            let flag = |key: &str| request.get(key).and_then(Value::as_bool).unwrap_or_default();
            let mode = match crate::text(request, "decode_padding_mode") {
                "indifferent" => DecodePaddingMode::Indifferent,
                "none" => DecodePaddingMode::RequireNone,
                _ => DecodePaddingMode::RequireCanonical,
            };
            let config = GeneralPurposeConfig::new()
                .with_encode_padding(flag("encode_padding"))
                .with_decode_allow_trailing_bits(flag("decode_allow_trailing_bits"))
                .with_decode_padding_mode(mode);
            GeneralPurpose::new(alphabet_named(crate::text(request, "alphabet")), config)
        }
    }
}

fn out_len(request: &Value) -> usize {
    request.get("out_len").and_then(Value::as_u64).unwrap_or_default() as usize
}

fn encode(request: &Value) -> Value {
    let engine = engine(request);
    let input = unhex(crate::text(request, "hex"));
    let mut string = String::from("prefix:");
    engine.encode_string(&input, &mut string);
    let mut buffer = vec![b'#'; out_len(request)];
    let slice = match engine.encode_slice(&input, &mut buffer) {
        Ok(written) => json!({"ok": written, "buffer": String::from_utf8_lossy(&buffer)}),
        Err(error) => json!({"err": error.to_string(), "err_debug": format!("{error:?}")}),
    };
    json!({"text": engine.encode(&input), "string": string, "slice": slice})
}

fn decode(request: &Value) -> Value {
    let engine = engine(request);
    let input = unhex(crate::text(request, "hex"));
    let decoded = match engine.decode(&input) {
        Ok(bytes) => json!({"ok": hex(&bytes)}),
        Err(error) => json!({"err": error.to_string(), "err_debug": format!("{error:?}")}),
    };
    let mut vec = b"prefix".to_vec();
    let vec = match engine.decode_vec(&input, &mut vec) {
        Ok(()) => json!({"ok": hex(&vec)}),
        Err(error) => json!({"err": error.to_string(), "err_debug": format!("{error:?}"), "buffer": hex(&vec)}),
    };
    let mut buffer = vec![0xEE_u8; out_len(request)];
    let slice = match engine.decode_slice(&input, &mut buffer) {
        Ok(written) => json!({"ok": written, "buffer": hex(&buffer)}),
        Err(error) => json!({"err": error.to_string(), "err_debug": format!("{error:?}")}),
    };
    let mut buffer = vec![0xEE_u8; out_len(request)];
    let unchecked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine.decode_slice_unchecked(&input, &mut buffer)
    }));
    let unchecked = match unchecked {
        Ok(Ok(written)) => json!({"ok": written, "buffer": hex(&buffer)}),
        Ok(Err(error)) => json!({"err": error.to_string(), "err_debug": format!("{error:?}")}),
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            json!({"panic": message})
        }
    };
    json!({"decode": decoded, "vec": vec, "slice": slice, "unchecked": unchecked})
}

fn alphabet(request: &Value) -> Value {
    match Alphabet::new(crate::text(request, "text")) {
        Ok(alphabet) => json!({"ok": alphabet.as_str()}),
        Err(error) => json!({"err": error.to_string(), "err_debug": format!("{error:?}")}),
    }
}

fn lengths(request: &Value) -> Value {
    let len = request.get("len").and_then(Value::as_u64).unwrap_or_default() as usize;
    json!({
        "padded": base64::encoded_len(len, true),
        "unpadded": base64::encoded_len(len, false),
        "estimate": base64::decoded_len_estimate(len),
    })
}
