//! Environment values that name a secret instead of holding it.
//!
//! A profile's or container's `environment` table accepts three forms per
//! entry. A plain string is passed through as written. `{ from_env = "NAME" }`
//! reads `NAME` from the process that loads the configuration. `{ from_secret
//! = "NAME" }` reads `NAME` from `secrets.toml` beside `config.toml`, a flat
//! table of string values that is never copied with the configuration.
//!
//! References are resolved while the configuration is read, so every consumer
//! sees plain strings, and they are serialized as written, so a save keeps
//! the reference rather than the value it stood for.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::de;
use serde::{Deserialize, Serialize};

/// The secrets file's name, beside `config.toml`.
pub const SECRETS_FILE: &str = "secrets.toml";

/// This instance's secrets file.
pub fn secrets_path() -> PathBuf {
    super::config_dir().join(SECRETS_FILE)
}

/// The secrets file beside a configuration file.
pub fn secrets_path_beside(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(SECRETS_FILE)
}

/// One environment entry as the configuration spells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentValue {
    Literal(String),
    /// Read from the loading process's environment.
    FromEnv(String),
    /// Read from the secrets file.
    FromSecret(String),
}

impl EnvironmentValue {
    pub const fn is_reference(&self) -> bool {
        !matches!(self, Self::Literal(_))
    }
}

impl Serialize for EnvironmentValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::Literal(value) => serializer.serialize_str(value),
            Self::FromEnv(name) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("from_env", name)?;
                map.end()
            }
            Self::FromSecret(name) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("from_secret", name)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for EnvironmentValue {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ValueVisitor;

        impl<'de> de::Visitor<'de> for ValueVisitor {
            type Value = EnvironmentValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(
                    "a string, `{ from_env = \"NAME\" }`, or `{ from_secret = \"NAME\" }`",
                )
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(EnvironmentValue::Literal(value.to_owned()))
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(EnvironmentValue::Literal(value))
            }

            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut found = None;
                while let Some(key) = map.next_key::<String>()? {
                    let name: String = map.next_value()?;
                    let value = match key.as_str() {
                        "from_env" => EnvironmentValue::FromEnv(name),
                        "from_secret" => EnvironmentValue::FromSecret(name),
                        other => {
                            return Err(de::Error::unknown_field(
                                other,
                                &["from_env", "from_secret"],
                            ));
                        }
                    };
                    if found.is_some() {
                        return Err(de::Error::custom(
                            "an environment reference names one of from_env or from_secret, not both",
                        ));
                    }
                    if value.name().trim().is_empty() {
                        return Err(de::Error::custom(format!("{key} names nothing")));
                    }
                    found = Some(value);
                }
                found.ok_or_else(|| {
                    de::Error::custom("an environment reference needs from_env or from_secret")
                })
            }
        }

        deserializer.deserialize_any(ValueVisitor)
    }
}

impl EnvironmentValue {
    fn name(&self) -> &str {
        match self {
            Self::Literal(value) | Self::FromEnv(value) | Self::FromSecret(value) => value,
        }
    }
}

/// An environment table: the entries as written, and the values they stand
/// for. Reading gives the resolved values; serializing gives the entries as
/// written.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Environment {
    sources: BTreeMap<String, EnvironmentValue>,
    resolved: BTreeMap<String, String>,
}

impl Environment {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve every entry with the active [`SecretResolver`].
    pub fn from_sources(sources: BTreeMap<String, EnvironmentValue>) -> Result<Self> {
        let resolved = sources
            .iter()
            .map(|(key, value)| Ok((key.clone(), resolve_active(key, value)?)))
            .collect::<Result<_>>()?;
        Ok(Self { sources, resolved })
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// The entries as the configuration spells them.
    pub fn sources(&self) -> &BTreeMap<String, EnvironmentValue> {
        &self.sources
    }

    /// The values the entries stand for.
    pub fn resolved(&self) -> &BTreeMap<String, String> {
        &self.resolved
    }

    pub fn into_resolved(self) -> BTreeMap<String, String> {
        self.resolved
    }

    /// Set a literal entry.
    pub fn insert(&mut self, key: String, value: String) -> Option<String> {
        self.sources
            .insert(key.clone(), EnvironmentValue::Literal(value.clone()));
        self.resolved.insert(key, value)
    }

    pub fn remove(&mut self, key: &str) -> Option<String> {
        self.sources.remove(key);
        self.resolved.remove(key)
    }

    pub fn clear(&mut self) {
        self.sources.clear();
        self.resolved.clear();
    }
}

impl Deref for Environment {
    type Target = BTreeMap<String, String>;

    fn deref(&self) -> &Self::Target {
        &self.resolved
    }
}

impl From<BTreeMap<String, String>> for Environment {
    fn from(resolved: BTreeMap<String, String>) -> Self {
        let sources = resolved
            .iter()
            .map(|(key, value)| (key.clone(), EnvironmentValue::Literal(value.clone())))
            .collect();
        Self { sources, resolved }
    }
}

impl FromIterator<(String, String)> for Environment {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(iter: I) -> Self {
        iter.into_iter().collect::<BTreeMap<_, _>>().into()
    }
}

impl IntoIterator for Environment {
    type Item = (String, String);
    type IntoIter = std::collections::btree_map::IntoIter<String, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.resolved.into_iter()
    }
}

impl<'a> IntoIterator for &'a Environment {
    type Item = (&'a String, &'a String);
    type IntoIter = std::collections::btree_map::Iter<'a, String, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.resolved.iter()
    }
}

impl Serialize for Environment {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.sources.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Environment {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let sources = BTreeMap::<String, EnvironmentValue>::deserialize(deserializer)?;
        if SOURCES_ONLY.get() {
            return Ok(Self {
                sources,
                resolved: BTreeMap::new(),
            });
        }
        Self::from_sources(sources).map_err(|error| {
            let message = format!("{error:#}");
            // The TOML parser wraps this in a parse error with a source
            // excerpt, which buries the message. The loader reads it from here.
            FAILURE.with(|failure| *failure.borrow_mut() = Some(message.clone()));
            de::Error::custom(message)
        })
    }
}

/// Where secret values are read from.
#[derive(Debug, Clone)]
pub enum SecretStore {
    File(PathBuf),
    Values(BTreeMap<String, String>),
}

/// Resolves environment references for one configuration read.
#[derive(Debug, Clone)]
pub struct SecretResolver {
    /// `None` reads the real process environment.
    process: Option<BTreeMap<String, String>>,
    secrets: SecretStore,
}

impl SecretResolver {
    /// Secrets from the file beside `config_path`, environment from the process.
    pub fn beside(config_path: &Path) -> Self {
        Self {
            process: None,
            secrets: SecretStore::File(secrets_path_beside(config_path)),
        }
    }

    /// Secrets from this instance's file, environment from the process.
    pub fn for_instance() -> Self {
        Self {
            process: None,
            secrets: SecretStore::File(secrets_path()),
        }
    }

    /// Fixed values for both sources.
    pub fn fixed(process: BTreeMap<String, String>, secrets: BTreeMap<String, String>) -> Self {
        Self {
            process: Some(process),
            secrets: SecretStore::Values(secrets),
        }
    }

    fn resolve(&self, key: &str, value: &EnvironmentValue) -> Result<String> {
        match value {
            EnvironmentValue::Literal(value) => Ok(value.clone()),
            EnvironmentValue::FromEnv(name) => {
                let found = match &self.process {
                    Some(process) => process.get(name).cloned(),
                    None => std::env::var(name).ok(),
                };
                match found {
                    Some(found) if !found.trim().is_empty() => Ok(found),
                    _ => bail!(
                        "{key} = {{ from_env = {name:?} }} needs {name} set, and not empty, in the environment of the process reading the configuration"
                    ),
                }
            }
            EnvironmentValue::FromSecret(name) => match &self.secrets {
                SecretStore::Values(values) => values.get(name).cloned().with_context(|| {
                    format!(
                        "{key} = {{ from_secret = {name:?} }} names a secret that is not defined"
                    )
                }),
                SecretStore::File(path) => {
                    let secrets = load_secrets(path).with_context(|| {
                        format!(
                            "{key} = {{ from_secret = {name:?} }} needs {}",
                            path.display()
                        )
                    })?;
                    match secrets.get(name) {
                        Some(found) if !found.trim().is_empty() => Ok(found.clone()),
                        _ => bail!(
                            "{key} = {{ from_secret = {name:?} }} needs `{name} = \"...\"` in {}",
                            path.display()
                        ),
                    }
                }
            },
        }
    }
}

thread_local! {
    static FAILURE: RefCell<Option<String>> = const { RefCell::new(None) };
    static ACTIVE: RefCell<Option<SecretResolver>> = const { RefCell::new(None) };
    static SOURCES_ONLY: Cell<bool> = const { Cell::new(false) };
}

/// Deserialize a configuration projection without reading credentials or
/// secret files. Its environments retain their configured sources, but have
/// no resolved values: serialize them for background resolution before use.
pub fn with_environment_sources_only<T>(read: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            SOURCES_ONLY.set(self.0);
        }
    }
    let _restore = Restore(SOURCES_ONLY.replace(true));
    read()
}

/// Run `read` with `resolver` answering every environment reference it meets.
/// Without one, references resolve against this instance's secrets file and
/// the process environment.
pub fn with_secret_resolver<T>(resolver: SecretResolver, read: impl FnOnce() -> T) -> T {
    let previous = ACTIVE.with(|active| active.replace(Some(resolver)));
    let value = read();
    ACTIVE.with(|active| *active.borrow_mut() = previous);
    value
}

/// The message of the last environment reference this thread failed to
/// resolve, cleared by the call. A caller that reads a configuration takes it
/// after a failed read, to report the reference instead of the parser's
/// excerpt of the file.
pub fn take_environment_failure() -> Option<String> {
    FAILURE.with(|failure| failure.borrow_mut().take())
}

fn resolve_active(key: &str, value: &EnvironmentValue) -> Result<String> {
    ACTIVE.with(|active| match active.borrow().as_ref() {
        Some(resolver) => resolver.resolve(key, value),
        None => SecretResolver::for_instance().resolve(key, value),
    })
}

/// Read a secrets file: a flat table of string values.
pub fn load_secrets(path: &Path) -> Result<BTreeMap<String, String>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("read secrets file {}", path.display()))?;
    let table: toml::Table = contents
        .parse()
        .with_context(|| format!("parse secrets file {}", path.display()))?;
    table
        .into_iter()
        .map(|(name, value)| match value {
            toml::Value::String(value) => Ok((name, value)),
            _ => bail!(
                "{}: `{name}` must be a string; the secrets file holds only `NAME = \"value\"` lines",
                path.display()
            ),
        })
        .collect()
}

/// Why a secrets file is not private to its owner, if it is not.
pub fn secrets_permission_problem(path: &Path) -> Result<Option<String>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("inspect {}", path.display()))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Ok(Some(format!(
                "{} is readable by other users (mode {mode:03o})",
                path.display()
            )));
        }
    }
    let _ = path;
    Ok(None)
}

/// Whether an environment variable's name suggests a credential, so a literal
/// value under it belongs in the secrets file.
pub fn looks_like_credential(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    ["API_KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
        .iter()
        .any(|word| name.contains(word))
        || name.ends_with("_KEY")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed() -> SecretResolver {
        SecretResolver::fixed(
            BTreeMap::from([("FROM_PROCESS".to_owned(), "process-value".to_owned())]),
            BTreeMap::from([("STORED".to_owned(), "stored-value".to_owned())]),
        )
    }

    #[test]
    fn every_reference_form_resolves_and_serializes_as_written() {
        let text = "LITERAL = \"plain\"\nA = { from_env = \"FROM_PROCESS\" }\nB = { from_secret = \"STORED\" }\n";
        let environment: Environment =
            with_secret_resolver(fixed(), || toml::from_str(text)).unwrap();
        assert_eq!(
            environment.get("LITERAL").map(String::as_str),
            Some("plain")
        );
        assert_eq!(
            environment.get("A").map(String::as_str),
            Some("process-value")
        );
        assert_eq!(
            environment.get("B").map(String::as_str),
            Some("stored-value")
        );
        assert_eq!(
            environment.sources()["A"],
            EnvironmentValue::FromEnv("FROM_PROCESS".into())
        );
        let written = toml::to_string(&environment).unwrap();
        assert!(written.contains("from_env = \"FROM_PROCESS\""), "{written}");
        assert!(written.contains("from_secret = \"STORED\""), "{written}");
        assert!(!written.contains("process-value"), "{written}");
        assert!(!written.contains("stored-value"), "{written}");
        let json = serde_json::to_value(&environment).unwrap();
        assert_eq!(json["B"]["from_secret"], "STORED");
        let back: Environment =
            with_secret_resolver(fixed(), || serde_json::from_value(json)).unwrap();
        assert_eq!(back, environment);
    }

    #[test]
    fn source_projections_need_no_credentials_and_preserve_references_for_background_resolution() {
        let json = serde_json::json!({
            "A": {"from_env": "UNDEFINED"},
            "B": {"from_secret": "UNDEFINED"},
            "C": "literal"
        });
        with_secret_resolver(
            SecretResolver::fixed(BTreeMap::new(), BTreeMap::new()),
            || {
                let projected: Environment =
                    with_environment_sources_only(|| serde_json::from_value(json.clone())).unwrap();
                assert!(projected.resolved().is_empty());
                assert_eq!(serde_json::to_value(projected).unwrap(), json);
                let resolved: Result<Environment, _> = serde_json::from_value(json);
                assert!(
                    resolved.is_err(),
                    "normal reads still require real credentials"
                );
                let malformed: Result<Environment, _> = with_environment_sources_only(|| {
                    serde_json::from_value(serde_json::json!({"A": {"from_secret": ""}}))
                });
                assert!(
                    malformed.is_err(),
                    "projections still validate source syntax"
                );
            },
        );
    }

    #[test]
    fn a_missing_secret_or_variable_names_what_to_set() {
        let missing_secret: Result<Environment, _> = with_secret_resolver(fixed(), || {
            toml::from_str("KEY = { from_secret = \"ABSENT\" }")
        });
        let message = missing_secret.unwrap_err().to_string();
        assert!(
            message.contains("ABSENT") && message.contains("KEY"),
            "{message}"
        );
        let missing_variable: Result<Environment, _> = with_secret_resolver(fixed(), || {
            toml::from_str("KEY = { from_env = \"ABSENT\" }")
        });
        let message = missing_variable.unwrap_err().to_string();
        assert!(message.contains("ABSENT set"), "{message}");
    }

    #[test]
    fn malformed_references_are_rejected() {
        for text in [
            "KEY = { from_env = \"A\", from_secret = \"B\" }",
            "KEY = { other = \"A\" }",
            "KEY = {}",
            "KEY = { from_env = \"\" }",
            "KEY = 3",
        ] {
            let parsed: Result<Environment, _> =
                with_secret_resolver(fixed(), || toml::from_str(text));
            assert!(parsed.is_err(), "{text}");
        }
    }

    #[test]
    fn the_secrets_file_is_a_flat_table_of_strings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(SECRETS_FILE);
        std::fs::write(&path, "A = \"1\"\nB = \"2\"\n").unwrap();
        assert_eq!(
            load_secrets(&path).unwrap(),
            BTreeMap::from([
                ("A".to_owned(), "1".to_owned()),
                ("B".to_owned(), "2".to_owned())
            ])
        );
        std::fs::write(&path, "A = 1\n").unwrap();
        assert!(
            load_secrets(&path)
                .unwrap_err()
                .to_string()
                .contains("`A` must be a string")
        );
        std::fs::write(&path, "[nested]\nA = \"1\"\n").unwrap();
        assert!(load_secrets(&path).is_err());
        let config = directory.path().join("config.toml");
        std::fs::write(&path, "STORED = \"from-file\"\n").unwrap();
        let environment: Environment =
            with_secret_resolver(SecretResolver::beside(&config), || {
                toml::from_str("KEY = { from_secret = \"STORED\" }")
            })
            .unwrap();
        assert_eq!(environment["KEY"], "from-file");
    }

    #[cfg(unix)]
    #[test]
    fn a_shared_secrets_file_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(SECRETS_FILE);
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(secrets_permission_problem(&path).unwrap().is_some());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(secrets_permission_problem(&path).unwrap().is_none());
    }

    #[test]
    fn credential_names_are_recognized() {
        for name in [
            "DEEPSEEK_API_KEY",
            "GITHUB_TOKEN",
            "ZAI_API_KEY",
            "db_password",
            "MY_KEY",
        ] {
            assert!(looks_like_credential(name), "{name}");
        }
        for name in ["PATH", "OPENAI_BASE_URL", "KIMI_CODE_BASE_URL", "HOME"] {
            assert!(!looks_like_credential(name), "{name}");
        }
    }
}
