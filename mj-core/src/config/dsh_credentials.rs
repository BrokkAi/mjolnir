//! DSH's own credential store, read the way DeepSeek Harness reads it.
//!
//! DSH keeps the API key a person saves in it under `refs` in
//! `<DSH home>/.credentials.yaml` (`@deepseek-ai/dsh-credentials-local`), and
//! reads `<DSH home>/.env` as its lowest fallback. A DSH profile's home is
//! that DSH home, so Mjolnir finds the same key DSH would use.
//!
//! A DeepSeek account sign-in is a different credential: a token DSH sends in
//! `x-dsh-auth-token` to its Messages endpoint. dsh-acp speaks the
//! OpenAI-compatible API with a bearer key only, so the sign-in is recognized
//! and reported, never sent.

use std::path::Path;

use anyhow::{Context, Result, bail};
use yaml_rust2::{Yaml, YamlLoader};

/// The managed store, beside the `.env` fallback in the DSH home.
pub(super) const CREDENTIALS_FILE: &str = ".credentials.yaml";
pub(super) const DOTENV_FILE: &str = ".env";
/// The record DSH's browser sign-in writes (`deepseek-account-platform`).
const ACCOUNT_RECORD: &str = "deepseek-account-platform/default";
/// The only layout DSH reads, besides the flat one it migrates from.
const DOCUMENT_VERSION: i64 = 1;

/// What DSH's credential store holds for one key.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct StoredCredentials {
    /// The key saved under `refs`, when there is one.
    pub api_key: Option<String>,
    /// Whether DSH is signed in to a DeepSeek account.
    pub account_signed_in: bool,
}

/// Read `name` from the store in `home`. A missing file holds nothing; a file
/// DSH itself would refuse to load is an error; the caller decides whether
/// another source makes it harmless.
pub(super) fn stored(home: &Path, name: &str) -> Result<StoredCredentials> {
    let path = home.join(CREDENTIALS_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StoredCredentials::default());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read DSH's credentials {}", path.display()));
        }
    };
    // DSH refuses a credential file any other user can read.
    if let Some(problem) = super::secrets::secrets_permission_problem(&path)? {
        bail!(
            "{problem}, so DSH refuses it; run `chmod 600 {}`",
            path.display()
        );
    }
    parse(&text, name).with_context(|| format!("read DSH's credentials {}", path.display()))
}

fn parse(text: &str, name: &str) -> Result<StoredCredentials> {
    // Messages name keys and positions only: every value here is a secret.
    let documents = YamlLoader::load_from_str(text)
        .map_err(|error| anyhow::anyhow!("not valid YAML at line {}", error.marker().line()))?;
    let root = match documents.as_slice() {
        [] => return Ok(StoredCredentials::default()),
        [root] => root,
        _ => bail!("holds more than one YAML document"),
    };
    let fields = match root {
        Yaml::Null => return Ok(StoredCredentials::default()),
        Yaml::Hash(fields) => fields,
        _ => bail!("its root is not a mapping"),
    };
    if fields.is_empty() {
        return Ok(StoredCredentials::default());
    }
    let field = |key: &str| fields.get(&Yaml::String(key.to_owned()));
    let Some(version) = field("version") else {
        // The pre-release flat layout: refs at the top level. DSH migrates it
        // in place on its next start without changing a value.
        return Ok(StoredCredentials {
            api_key: reference(root, name)?,
            account_signed_in: false,
        });
    };
    // DSH compares with `!==`, so `1.0` is version 1 too.
    let is_current = match version {
        Yaml::Integer(version) => *version == DOCUMENT_VERSION,
        Yaml::Real(version) => version.parse::<f64>() == Ok(DOCUMENT_VERSION as f64),
        _ => false,
    };
    if !is_current {
        bail!("declares a layout version other than {DOCUMENT_VERSION}, the one Mjolnir reads");
    }
    let api_key = match field("refs") {
        None | Some(Yaml::Null) => None,
        Some(refs) => reference(refs, name)?,
    };
    let account_signed_in = match field("records") {
        None | Some(Yaml::Null) => false,
        Some(Yaml::Hash(records)) => records.contains_key(&Yaml::String(ACCOUNT_RECORD.to_owned())),
        Some(_) => bail!("`records` is not a mapping"),
    };
    Ok(StoredCredentials {
        api_key,
        account_signed_in,
    })
}

fn reference(section: &Yaml, name: &str) -> Result<Option<String>> {
    let Yaml::Hash(refs) = section else {
        bail!("`refs` is not a mapping");
    };
    match refs.get(&Yaml::String(name.to_owned())) {
        None => Ok(None),
        Some(Yaml::String(value)) if value.is_empty() => {
            bail!("the value for `{name}` is empty; remove the key instead")
        }
        Some(Yaml::String(value)) => Ok(Some(value.clone())),
        Some(_) => bail!("the value for `{name}` must be a string"),
    }
}

/// `name` from the `.env` in `home`, DSH's lowest-ranked layer. DSH loads it
/// with Node's `util.parseEnv`; this reads the same syntax for one variable.
pub(super) fn dotenv(home: &Path, name: &str) -> Result<Option<String>> {
    let path = home.join(DOTENV_FILE);
    // Node decodes the file as UTF-8, replacing what is not.
    match std::fs::read(&path) {
        Ok(bytes) => Ok(parse_dotenv(&String::from_utf8_lossy(&bytes), name)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read DSH's {}", path.display())),
    }
}

/// Node's dotenv grammar: `[export ]NAME=value` lines, `#` comments, and
/// single, double or backtick quotes that may span lines; double quotes
/// expand `\n`. A later assignment replaces an earlier one.
fn parse_dotenv(text: &str, name: &str) -> Option<String> {
    let text = text.replace("\r\n", "\n");
    let mut rest = text.as_str();
    let mut found = None;
    while !rest.is_empty() {
        let (line, after) = rest.split_once('\n').unwrap_or((rest, ""));
        rest = after;
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let key = key.strip_prefix("export ").map_or(key, str::trim_start);
        let value = value.trim_start();
        let quote = value
            .chars()
            .next()
            .filter(|c| matches!(c, '"' | '\'' | '`'));
        let parsed = match quote {
            Some(quote) => {
                // The closing quote may be on a later line; the value then
                // continues through it.
                let body = &value[1..];
                let quoted = if let Some(end) = body.find(quote) {
                    Some(body[..end].to_owned())
                } else if let Some(end) = rest.find(quote) {
                    let value = format!("{body}\n{}", &rest[..end]);
                    rest = rest[end + 1..]
                        .split_once('\n')
                        .map_or("", |(_, after)| after);
                    Some(value)
                } else {
                    None
                };
                match quoted {
                    // Only a closed double-quoted value expands `\n`.
                    Some(value) if quote == '"' => Some(value.replace("\\n", "\n")),
                    Some(value) => Some(value),
                    // An unclosed quote is read as written up to the line end.
                    None => Some(value.trim_end().to_owned()),
                }
            }
            None => Some(
                value
                    .split_once('#')
                    .map_or(value, |(value, _)| value)
                    .trim_end()
                    .to_owned(),
            ),
        };
        if key == name {
            found = parsed;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::parse_dotenv;

    /// DSH loads `.env` with Node's `util.parseEnv`; each expectation is what
    /// Node 24 returns for the same text.
    #[test]
    fn dotenv_values_match_node_parse_env() {
        let cases: &[(&str, Option<&str>)] = &[
            ("A=plain # c", Some("plain")),
            ("export A=sk-1", Some("sk-1")),
            ("A=\"multi\nline\"", Some("multi\nline")),
            ("A=\"esc\\nx\"", Some("esc\nx")),
            ("A='single # x'", Some("single # x")),
            ("A=`back`", Some("back")),
            ("A=\"unclosed", Some("\"unclosed")),
            ("A=\"unc\\nx", Some("\"unc\\nx")),
            ("garbage line\nA=sk-x", Some("sk-x")),
            ("\tA=\tx\t", Some("x")),
            ("A=x\ty", Some("x\ty")),
            ("  A =  spaced  ", Some("spaced")),
            ("A=1\nA=2", Some("2")),
            ("A=\"x\" trailing", Some("x")),
            ("A=\"a\nb\" \nB=2", Some("a\nb")),
            ("A=\"a\r\nb\"\r\n", Some("a\nb")),
            ("#A=no", None),
            ("A=", Some("")),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_dotenv(text, "A").as_deref(), *expected, "{text:?}");
        }
    }
}
