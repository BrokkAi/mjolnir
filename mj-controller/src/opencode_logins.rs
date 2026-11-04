//! OpenCode 2's logins, read from its own database.
//!
//! OpenCode 2 keeps every login in the `credential` table of `opencode.db` in
//! its data directory, the same file that holds every session it ran there.
//! Staging carries only the logins into a session home, as the JSON array
//! `opencode auth export` prints and `opencode auth import` reads; each launch
//! imports it ([`mj_core::harness_runtime::opencode_acp_script`]).
//!
//! The database is read here rather than through `opencode auth export`
//! because the daemon cannot rely on finding the person's own `opencode`, and
//! Mjolnir's pinned one would migrate a database the person's OpenCode owns.
//! A read-only query changes nothing, and a schema it does not expect fails
//! the read with SQLite's error rather than staging a session without logins.
//!
//! OpenCode 1 kept logins in `auth.json` instead. OpenCode 2 imports that file
//! when it upgrades the database beside it, and ignores it once it has; a
//! login Mjolnir finds in a directory OpenCode 2 has not taken over yet is
//! carried the way that upgrade would carry it.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use mj_core::config::{HarnessKind, HarnessProfile};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Value, json};

/// The database file name of every released OpenCode channel.
const DATABASE: &str = "opencode.db";

/// OpenCode 1's login file, in the data directory.
const LEGACY_LOGINS: &str = "auth.json";

/// The migration through which OpenCode 2 takes over an OpenCode 1 data
/// directory, importing its [`LEGACY_LOGINS`]. OpenCode 2 also records it in
/// every database it creates, where it imports nothing.
const LEGACY_IMPORT_MIGRATION: &str = "20260805200742_import_legacy_credentials";

/// One login, keyed by the fields of an `auth export` entry.
struct Login {
    id: String,
    integration: String,
    label: String,
    value: Value,
    /// How OpenCode orders the logins of one integration to select one:
    /// the active flag (set, clear, then never set), then the newest.
    rank: (u8, i64, String),
}

/// The logins OpenCode keeps for `profile`, in `auth export` form: those in
/// its database, then OpenCode 1 logins for integrations the database lacks,
/// from the data directory and from the place Mjolnir kept them for an
/// OpenCode 1 profile. `None` when there are none.
pub fn profile_logins(profile: &HarnessProfile) -> Result<Option<Vec<u8>>> {
    home_logins(&profile.home)
}

/// [`profile_logins`] for the profile whose home is `home`.
pub fn home_logins(home: &Path) -> Result<Option<Vec<u8>>> {
    let data = HarnessKind::OpenCode
        .nested_data_dir(home)
        .expect("OpenCode has an XDG home");
    let mut logins = stored_logins(&data)?;
    let mut legacy = vec![data.clone()];
    // Mjolnir pointed OpenCode 1 at `<home>/.data` even for the person's own
    // home, so a login made through `mj login` may be there instead.
    let mjolnir = home.join(".data").join(HarnessKind::OpenCode.id());
    if mjolnir != data {
        legacy.push(mjolnir);
    }
    for directory in legacy {
        legacy_logins(&directory, &mut logins)?;
    }
    if logins.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::to_vec_pretty(&export(logins))?))
}

/// The `credential` rows of the database in `data`, oldest first.
fn stored_logins(data: &Path) -> Result<Vec<Login>> {
    let path = data.join(DATABASE);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let read = || -> Result<Vec<Login>> {
        let connection = open(&path)?;
        let mut statement = connection.prepare(
            "SELECT id, integration_id, label, active, value, time_created FROM credential \
             WHERE integration_id IS NOT NULL ORDER BY time_created, id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<bool>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        rows.map(|row| {
            let (id, integration, label, active, value, created) = row?;
            let value = serde_json::from_str(&value)
                .with_context(|| format!("decode stored login {id}"))?;
            let flag = match active {
                Some(true) => 2,
                Some(false) => 1,
                None => 0,
            };
            Ok(Login {
                rank: (flag, created, id.clone()),
                id,
                integration,
                label,
                value,
            })
        })
        .collect()
    };
    read().with_context(|| format!("read OpenCode logins from {}", path.display()))
}

/// Add the OpenCode 1 logins of `directory` for integrations `logins` lacks,
/// converted as OpenCode 2's own upgrade converts them. Nothing when OpenCode
/// 2 has taken the directory over: it decided about that file itself, and a
/// login removed since must not come back.
fn legacy_logins(directory: &Path, logins: &mut Vec<Login>) -> Result<()> {
    let path = directory.join(LEGACY_LOGINS);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    if taken_over_by_opencode_2(directory)? {
        return Ok(());
    }
    let entries: BTreeMap<String, Value> = serde_json::from_str(&text)
        .with_context(|| format!("{} must hold a JSON object of logins", path.display()))?;
    for (id, entry) in entries {
        let integration = id.trim_end_matches('/');
        if integration.is_empty() || logins.iter().any(|login| login.integration == integration) {
            continue;
        }
        let Some((label, value)) = legacy_value(integration, &entry) else {
            continue;
        };
        logins.push(Login {
            // Stable, so a relaunch that imports it again adds nothing.
            id: format!(
                "cred_mjolnir_{}",
                mj_core::hex::lower_hex(integration.as_bytes())
            ),
            integration: integration.to_owned(),
            label: label.to_owned(),
            value,
            rank: (0, 0, String::new()),
        });
    }
    Ok(())
}

/// One OpenCode 1 login as OpenCode 2's upgrade stores it, with its label;
/// `None` for an entry that upgrade skips because it is not a login it knows.
fn legacy_value(integration: &str, entry: &Value) -> Option<(&'static str, Value)> {
    let text = |name: &str| entry.get(name)?.as_str();
    match entry.get("type")?.as_str()? {
        "api" => {
            let mut value = json!({"type": "key", "key": text("key")?});
            if let Some(metadata) = entry.get("metadata") {
                let metadata = metadata.as_object()?;
                if !metadata.values().all(Value::is_string) {
                    return None;
                }
                value["metadata"] = Value::Object(metadata.clone());
            }
            Some(("API key", value))
        }
        "wellknown" => {
            text("key")?;
            Some(("API key", json!({"type": "key", "key": text("token")?})))
        }
        "oauth" => {
            let method = match integration {
                "openai" => "chatgpt-browser",
                "github-copilot" | "opencode" | "xai" => "device",
                _ => "oauth",
            };
            let mut value = json!({
                "type": "oauth",
                "methodID": method,
                "refresh": text("refresh")?,
                "access": text("access")?,
                "expires": entry.get("expires")?.as_u64()?,
            });
            let mut metadata = serde_json::Map::new();
            if let Some(account) = text("accountId").filter(|account| !account.is_empty()) {
                metadata.insert("accountID".into(), account.into());
            }
            if let Some(url) = text("enterpriseUrl").filter(|url| !url.is_empty()) {
                metadata.insert("enterpriseUrl".into(), url.into());
            }
            if !metadata.is_empty() {
                value["metadata"] = Value::Object(metadata);
            }
            Some(("OAuth", value))
        }
        _ => None,
    }
}

/// Whether OpenCode 2 has opened the database in `directory`.
fn taken_over_by_opencode_2(directory: &Path) -> Result<bool> {
    let path = directory.join(DATABASE);
    if !path.is_file() {
        return Ok(false);
    }
    let read = || -> Result<bool> {
        let connection = open(&path)?;
        let journal: Option<String> = connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'migration'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if journal.is_none() {
            return Ok(false);
        }
        Ok(connection
            .query_row(
                "SELECT 1 FROM migration WHERE id = ?1",
                [LEGACY_IMPORT_MIGRATION],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    };
    read().with_context(|| format!("read OpenCode migrations from {}", path.display()))
}

fn open(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // The person's own OpenCode may be writing; a reader waits out its brief
    // write locks instead of failing the launch.
    connection.busy_timeout(Duration::from_secs(5))?;
    Ok(connection)
}

/// The `auth export` entries for `logins`. Each integration's selected login,
/// the one OpenCode uses, is the active one, so a fresh session selects it
/// too; `auth import` activates the first login of an integration it lacks
/// whatever its flag says.
fn export(logins: Vec<Login>) -> Vec<Value> {
    let mut selected = BTreeMap::new();
    for (index, login) in logins.iter().enumerate() {
        let best = selected.entry(login.integration.as_str()).or_insert(index);
        if login.rank > logins[*best].rank {
            *best = index;
        }
    }
    let selected = selected.into_values().collect::<Vec<_>>();
    logins
        .iter()
        .enumerate()
        .map(|(index, login)| {
            json!({
                "id": login.id,
                "integrationID": login.integration,
                "label": login.label,
                "active": selected.contains(&index),
                "value": login.value,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::controller::test_support::{IsolatedTest, test_name};

    /// Moving to OpenCode 2 keeps every login the person had and revives none
    /// they removed. In the person's own home, the database's logins come with
    /// the one OpenCode selects for each integration; an OpenCode 1 login that
    /// `mj login` kept under the home is carried as OpenCode 2's own upgrade
    /// carries it; and the OpenCode 1 file OpenCode 2 already took over brings
    /// nothing back.
    #[test]
    fn moving_to_opencode_2_keeps_every_login_and_revives_none() {
        const CHILD: &str = "MJ_TEST_OPENCODE_2_LOGINS";
        let Some(root) = std::env::var_os(CHILD).map(PathBuf::from) else {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "moving_to_opencode_2_keeps_every_login_and_revives_none",
            ))
            .env(CHILD, root.path())
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .run();
            return;
        };
        let home = root.join("config/opencode");
        let data = root.join("data/opencode");
        std::fs::create_dir_all(&data).unwrap();
        // OpenCode 2 took the person's store over and imported its OpenCode 1
        // file, leaving two DeepSeek keys without an active flag. The person
        // has since logged out of Anthropic.
        Connection::open(data.join(DATABASE))
            .unwrap()
            .execute_batch(
                r#"CREATE TABLE migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL);
                   INSERT INTO migration VALUES ('20260805200742_import_legacy_credentials', 1);
                   CREATE TABLE credential (id text PRIMARY KEY, integration_id text, label text NOT NULL,
                       value text NOT NULL, connector_id text, method_id text, active integer,
                       time_created integer NOT NULL, time_updated integer NOT NULL);
                   INSERT INTO credential VALUES ('cred_old', 'deepseek', 'API key',
                       '{"type":"key","key":"sk-old"}', NULL, NULL, NULL, 1, 1);
                   INSERT INTO credential VALUES ('cred_new', 'deepseek', 'API key',
                       '{"type":"key","key":"sk-new"}', NULL, NULL, NULL, 2, 2);"#,
            )
            .unwrap();
        std::fs::write(
            data.join(LEGACY_LOGINS),
            r#"{"anthropic":{"type":"oauth","refresh":"r","access":"a","expires":1}}"#,
        )
        .unwrap();
        // OpenCode 1 under Mjolnir kept these under the home itself.
        let mjolnir = home.join(".data/opencode");
        std::fs::create_dir_all(&mjolnir).unwrap();
        std::fs::write(
            mjolnir.join(LEGACY_LOGINS),
            r#"{"broken":{"type":"api"},
                "deepseek":{"type":"api","key":"sk-mjolnir"},
                "openai/":{"type":"oauth","refresh":"r","access":"a","expires":5,"accountId":"acct"}}"#,
        )
        .unwrap();
        let profile = HarnessProfile {
            enabled: true,
            kind: HarnessKind::OpenCode,
            home,
            environment: Default::default(),
            context_window_bytes: None,
            guardian_review_model: None,
            codex_profile: None,
            muse_auto_review: None,
            legacy_subagents: None,
        };

        let logins = profile_logins(&profile).unwrap().unwrap();

        assert_eq!(
            profile_logins(&profile).unwrap().unwrap(),
            logins,
            "a relaunch stages the same logins, so its import adds nothing"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&logins).unwrap(),
            json!([
                {"id": "cred_old", "integrationID": "deepseek", "label": "API key", "active": false,
                 "value": {"type": "key", "key": "sk-old"}},
                {"id": "cred_new", "integrationID": "deepseek", "label": "API key", "active": true,
                 "value": {"type": "key", "key": "sk-new"}},
                {"id": "cred_mjolnir_6f70656e6169", "integrationID": "openai", "label": "OAuth",
                 "active": true,
                 "value": {"type": "oauth", "methodID": "chatgpt-browser", "refresh": "r",
                           "access": "a", "expires": 5, "metadata": {"accountID": "acct"}}},
            ])
        );
    }
}
