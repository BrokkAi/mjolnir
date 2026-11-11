//! The small subset of Go templates that `--format` needs.
//!
//! The controller's Podman scripts use `{{.State.Running}}`, `{{.Id}}`,
//! `{{json .Config.Labels}}` and `{{index .Labels "key"}}`. Anything else is
//! refused by name rather than ignored.

use anyhow::{Result, anyhow, bail};
use serde_json::Value;

/// Render `template` against `value`, followed by a newline like Podman.
pub fn render(template: &str, value: &Value) -> Result<String> {
    let mut output = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| anyhow!("unsupported --format template {template:?}: unterminated action"))?;
        output.push_str(&action(after[..end].trim(), value).map_err(|error| {
            anyhow!("unsupported --format template {template:?}: {error}")
        })?);
        rest = &after[end + 2..];
    }
    output.push_str(rest);
    output.push('\n');
    Ok(output)
}

fn action(text: &str, root: &Value) -> Result<String> {
    let words = split_words(text)?;
    match words.as_slice() {
        [path] if path.starts_with('.') => scalar(lookup(root, path)?),
        [function, path] if function == "json" && path.starts_with('.') => {
            Ok(serde_json::to_string(lookup(root, path)?)?)
        }
        [function, path, key] if function == "index" && path.starts_with('.') => {
            let table = lookup(root, path)?;
            match table.get(key.as_str()) {
                Some(value) => scalar(value),
                // A map of strings yields its zero value for a missing key.
                None if table.is_object() || table.is_null() => Ok(String::new()),
                None => bail!("{path} is not a map"),
            }
        }
        _ => bail!("action {{{{{text}}}}} is not supported"),
    }
}

fn split_words(text: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' {
            chars.next();
            let mut word = String::new();
            loop {
                match chars.next() {
                    Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some(escaped) => word.push(escaped),
                        None => bail!("unterminated string"),
                    },
                    Some(other) => word.push(other),
                    None => bail!("unterminated string"),
                }
            }
            words.push(word);
        } else {
            let mut word = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                word.push(c);
                chars.next();
            }
            words.push(word);
        }
    }
    Ok(words)
}

fn lookup<'a>(root: &'a Value, path: &str) -> Result<&'a Value> {
    let mut current = root;
    for part in path.split('.').filter(|part| !part.is_empty()) {
        current = current
            .get(part)
            .ok_or_else(|| anyhow!("{path} names a field that does not exist ({part})"))?;
    }
    Ok(current)
}

fn scalar(value: &Value) -> Result<String> {
    Ok(match value {
        Value::String(text) => text.clone(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Null => "<nil>".to_owned(),
        Value::Array(_) | Value::Object(_) => bail!("the value is not a scalar; use json"),
    })
}
