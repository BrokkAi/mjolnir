//! Secret redaction for text that leaves the process: provider diagnostics in
//! logs and the evidence Mjolnir sends to the Jev classifier.

use serde_json::Value;

/// Shorter runs of token characters are left alone so that ordinary words,
/// paths and numbers stay readable.
const SECRET_RUN_CHARS: usize = 24;

/// Replace every long run of token characters. Access, refresh and id tokens,
/// API keys and account ids are all long runs of base64url, hex or dotted JWT
/// segments, and a diagnostic has no use for them.
pub fn redact_secrets(text: &str) -> String {
    fn is_token_char(ch: char) -> bool {
        ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '+')
    }
    fn flush(run: &mut String, redacted: &mut String) {
        if run.chars().count() >= SECRET_RUN_CHARS {
            redacted.push_str("[redacted]");
        } else {
            redacted.push_str(run);
        }
        run.clear();
    }
    let mut redacted = String::with_capacity(text.len());
    let mut run = String::new();
    for ch in text.chars() {
        if is_token_char(ch) {
            run.push(ch);
        } else {
            flush(&mut run, &mut redacted);
            redacted.push(ch);
        }
    }
    flush(&mut run, &mut redacted);
    redacted
}

/// Redact every string value in serialized evidence, at any depth, so a field
/// added later is covered without being named here. Values under `id` or a
/// `*_id` key are Mjolnir's own identifiers, which the receiver matches
/// against each other, so they are kept.
pub fn redact_json_strings(value: &mut Value) {
    match value {
        Value::String(text) => *text = redact_secrets(text),
        Value::Array(items) => items.iter_mut().for_each(redact_json_strings),
        Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                if key == "id" || key.ends_with("_id") {
                    continue;
                }
                redact_json_strings(field);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_removes_token_runs_and_keeps_ordinary_text() {
        assert_eq!(
            redact_secrets(
                "Bearer eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiIxMjM0In0.c2lnbmF0dXJl, key sk-proj-ABCDEFGHIJKLMNOPQRSTUVWX"
            ),
            "Bearer [redacted], key [redacted]"
        );
        let ordinary = "2026-10-05T12:00:00Z WARN codex_login::auth: POST https://auth.openai.com/oauth/token returned 401";
        assert_eq!(redact_secrets(ordinary), ordinary);
    }
}
