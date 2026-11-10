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
    redact_token_runs(text, |_| "[redacted]".into())
}

/// Request-local labels preserve identifier equality without exposing contents.
fn redact_secrets_distinct(
    text: &str,
    labels: &mut std::collections::BTreeMap<String, usize>,
) -> String {
    redact_token_runs(text, |token| {
        let next = labels.len();
        let label = labels.entry(token.to_owned()).or_insert(next);
        format!("[redacted token {label}]")
    })
}

fn redact_token_runs(text: &str, mut replacement: impl FnMut(&str) -> String) -> String {
    fn is_token_char(ch: char) -> bool {
        ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '+')
    }
    fn flush(
        run: &mut String,
        redacted: &mut String,
        replacement: &mut impl FnMut(&str) -> String,
    ) {
        if run.chars().count() >= SECRET_RUN_CHARS {
            redacted.push_str(&replacement(run));
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
            flush(&mut run, &mut redacted, &mut replacement);
            redacted.push(ch);
        }
    }
    flush(&mut run, &mut redacted, &mut replacement);
    redacted
}

/// Redact every string value in serialized evidence, at any depth, so a field
/// added later is covered without being named here. Values under `id` or a
/// `*_id` key are Mjolnir's own identifiers, which the receiver matches
/// against each other, so they are kept.
pub fn redact_json_strings(value: &mut Value) {
    redact_json_strings_with(value, true, &mut |text| redact_secrets(text));
}

/// Redact string values before serialization so escapes cannot alter identity.
pub fn redact_json_strings_distinct(
    value: &mut Value,
    labels: &mut std::collections::BTreeMap<String, usize>,
) {
    redact_json_strings_with(value, false, &mut |text| {
        redact_secrets_distinct(text, labels)
    });
}

fn redact_json_strings_with(
    value: &mut Value,
    keep_ids: bool,
    redactor: &mut impl FnMut(&str) -> String,
) {
    match value {
        Value::String(text) => *text = redactor(text),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|value| redact_json_strings_with(value, keep_ids, redactor)),
        Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                if keep_ids && (key == "id" || key.ends_with("_id")) {
                    continue;
                }
                redact_json_strings_with(field, keep_ids, redactor);
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
