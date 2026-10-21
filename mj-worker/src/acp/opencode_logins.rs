//! The logins an OpenCode bridge imported when it started.
//!
//! Staging and the credential sync keep the profile's logins in the session
//! home as the export `opencode auth import` reads
//! ([`HarnessKind::credential_file_name`]), and every launch imports it
//! ([`mj_core::harness_runtime::opencode_acp_script`]). The session's own
//! database copies those logins, and OpenCode refreshes an OAuth login in its
//! copy in place. Before each bridge starts, the copy is brought to the staged
//! export without losing such a refresh ([`reconcile_database_logins`]), and an
//! idle session whose staged export has changed reloads on a fresh bridge,
//! which imports it. A running OpenCode reloads its providers the same way when
//! a login changes through its own server, which Mjolnir cannot reach.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use mj_core::config::HarnessKind;
use serde_json::Value;

/// How often an idle OpenCode session looks for changed logins.
#[cfg(not(test))]
pub(super) const CHECK_INTERVAL: Duration = Duration::from_secs(5);
#[cfg(test)]
pub(super) const CHECK_INTERVAL: Duration = Duration::from_millis(20);

/// The staged export a bridge's launch imported, as it read it.
#[derive(Debug, Clone)]
pub struct ImportedLogins {
    path: PathBuf,
    entries: Vec<Value>,
}

impl ImportedLogins {
    /// Before an OpenCode bridge starts in `environment`: bring the session
    /// database's logins to the staged export, and record the export the
    /// launch's import reads. Recorded before the bridge reads it, so a change
    /// made in between is seen as a change.
    pub(super) fn prepare(environment: &BTreeMap<String, String>) -> Result<Self> {
        let kind = HarnessKind::OpenCode;
        let variable = |name: &str| {
            environment
                .get(name)
                .map(PathBuf::from)
                .with_context(|| format!("the OpenCode launch sets no {name}"))
        };
        let path = kind
            .home_from_environment(variable(kind.home_env())?)
            .join(kind.credential_file_name());
        let database = variable("XDG_DATA_HOME")?
            .join(kind.id())
            .join("opencode.db");
        let entries = read_entries(&path)?;
        reconcile_database_logins(&database, &entries)?;
        Ok(Self { path, entries })
    }

    /// Whether the staged export now differs from the one this bridge
    /// imported. A missing export is the same as one without logins.
    pub(super) async fn changed(&self) -> Result<bool> {
        let path = self.path.clone();
        let entries = tokio::task::spawn_blocking(move || read_entries(&path))
            .await
            .context("read the staged OpenCode logins")??;
        Ok(entries != self.entries)
    }
}

fn read_entries(path: &Path) -> Result<Vec<Value>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("decode the staged OpenCode logins {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error)
            .with_context(|| format!("read the staged OpenCode logins {}", path.display())),
    }
}

/// One staged login, as `auth export` writes it.
struct Staged<'a> {
    integration: &'a str,
    active: bool,
    value: &'a Value,
}

/// Bring the logins of the session database at `path` to the staged
/// `entries`, keeping what the session refreshed since. The launch's
/// `auth import` then adds every staged login the database lacks.
///
/// `auth import` adds only logins whose id is new, activates a new login only
/// for an integration the database lacks, and pins an integration's newest
/// remaining login active when every one of its logins is inactive. So an
/// integration is kept only while its selected login is: the session's copy
/// matches the staged one, or is a later refresh of the same OAuth login.
/// Then its other logins stay where they still match and the staged selection
/// is set; otherwise the integration is emptied and the import rebuilds it,
/// selecting as staged. A login the profile no longer has is removed.
///
/// The previous bridge has been stopped before this one is prepared, and its
/// server child ends with it. A database reached through a link is refused,
/// as a credential file is, so a session home cannot point this at another
/// database.
fn reconcile_database_logins(path: &Path, entries: &[Value]) -> Result<()> {
    for checked in [path.parent(), Some(path)].into_iter().flatten() {
        match std::fs::symlink_metadata(checked) {
            Ok(metadata) if metadata.file_type().is_symlink() => anyhow::bail!(
                "refusing to change OpenCode logins through the link {}",
                checked.display()
            ),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", checked.display()));
            }
        }
    }
    let staged: BTreeMap<&str, Staged<'_>> = entries
        .iter()
        .filter_map(|entry| {
            Some((
                entry.get("id")?.as_str()?,
                Staged {
                    integration: entry.get("integrationID")?.as_str()?,
                    active: entry.get("active")?.as_bool()?,
                    value: entry.get("value")?,
                },
            ))
        })
        .collect();
    let reconcile = || -> Result<()> {
        let mut database = rusqlite::Connection::open(path)?;
        database.busy_timeout(Duration::from_secs(5))?;
        let exists: bool = database.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'credential')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(());
        }
        let transaction = database.transaction()?;
        let rows = {
            let mut statement =
                transaction.prepare("SELECT id, integration_id, value FROM credential")?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let current = |id: &str, value: &str| {
            staged.get(id).is_some_and(|staged| {
                serde_json::from_str::<Value>(value)
                    .is_ok_and(|session| holds(&session, staged.value))
            })
        };
        let kept_integrations: std::collections::BTreeSet<&str> = staged
            .iter()
            .filter(|(_, staged)| staged.active)
            .filter(|(id, _)| {
                rows.iter()
                    .any(|(row, _, value)| row == *id && current(row, value))
            })
            .map(|(_, staged)| staged.integration)
            .collect();
        for (id, integration, value) in &rows {
            let keep = integration
                .as_deref()
                .is_some_and(|integration| kept_integrations.contains(integration))
                && current(id, value);
            if keep {
                transaction.execute(
                    "UPDATE credential SET active = ?2 WHERE id = ?1",
                    rusqlite::params![id, staged[id.as_str()].active],
                )?;
            } else {
                transaction.execute("DELETE FROM credential WHERE id = ?1", [id])?;
            }
        }
        transaction.commit()?;
        Ok(())
    };
    reconcile().with_context(|| {
        format!(
            "reconcile the OpenCode session's logins in {}",
            path.display()
        )
    })
}

/// Whether the session's copy of a login still holds the staged one: the same
/// login, or a later refresh of the same OAuth login.
fn holds(session: &Value, staged: &Value) -> bool {
    let expiry = |value: &Value| {
        (value.get("type")?.as_str()? == "oauth")
            .then(|| value.get("expires")?.as_u64())
            .flatten()
    };
    session == staged
        || matches!((expiry(session), expiry(staged)), (Some(session), Some(staged)) if session > staged)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session home for OpenCode under `root`, with `export` staged, and the
    /// launch environment that points OpenCode at it.
    fn session_home(root: &Path, export: &Value) -> BTreeMap<String, String> {
        let home = root.join("profile/opencode");
        let staged = home.join(HarnessKind::OpenCode.credential_file_name());
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, serde_json::to_vec(export).unwrap()).unwrap();
        BTreeMap::from([
            (
                "XDG_CONFIG_HOME".to_owned(),
                root.join("profile").to_string_lossy().into_owned(),
            ),
            (
                "XDG_DATA_HOME".to_owned(),
                home.join(".data").to_string_lossy().into_owned(),
            ),
        ])
    }

    fn login(id: &str, integration: &str, active: bool, value: Value) -> Value {
        serde_json::json!({
            "id": id, "integrationID": integration, "label": id, "active": active, "value": value,
        })
    }

    /// A reload brings the session's logins to the profile's through
    /// OpenCode's own import, without losing an OAuth refresh the session made:
    /// the refreshed login stays, a provider's newly selected login takes
    /// over, and a login the profile dropped is gone.
    #[test]
    #[ignore = "requires MJ_OPENCODE_TEST_BINARY naming an OpenCode 2 executable"]
    fn live_opencode_reload_keeps_session_refreshes_and_the_profiles_selection() {
        let opencode = PathBuf::from(
            std::env::var_os("MJ_OPENCODE_TEST_BINARY").expect("set an OpenCode 2 executable"),
        );
        let root = tempfile::tempdir().unwrap();
        let oauth = |refresh: &str, expires: u64| {
            serde_json::json!({"type": "oauth", "methodID": "chatgpt-browser", "refresh": refresh,
                               "access": "access", "expires": expires})
        };
        let key = |key: &str| serde_json::json!({"type": "key", "key": key});
        let import = |environment: &BTreeMap<String, String>| {
            let staged = HarnessKind::OpenCode
                .home_from_environment(&environment["XDG_CONFIG_HOME"])
                .join(HarnessKind::OpenCode.credential_file_name());
            let status = std::process::Command::new(&opencode)
                .args(["auth", "import", "--standalone"])
                .arg(staged)
                .envs(environment)
                .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
                .status()
                .unwrap();
            assert!(status.success(), "import: {status}");
        };
        let first = serde_json::json!([
            login("cred_openai", "openai", true, oauth("refresh-1", 100)),
            login("cred_deepseek_old", "deepseek", true, key("sk-old")),
            login("cred_xai", "xai", true, key("sk-xai")),
        ]);
        let environment = session_home(root.path(), &first);
        ImportedLogins::prepare(&environment).unwrap();
        import(&environment);
        let database = PathBuf::from(&environment["XDG_DATA_HOME"]).join("opencode/opencode.db");
        // The session refreshes its OpenAI login in place, as OpenCode does.
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute(
                "UPDATE credential SET value = ?1 WHERE id = 'cred_openai'",
                [oauth("refresh-2", 200).to_string()],
            )
            .unwrap();

        let second = serde_json::json!([
            login("cred_openai", "openai", true, oauth("refresh-1", 100)),
            login("cred_deepseek_old", "deepseek", false, key("sk-old")),
            login("cred_deepseek_new", "deepseek", true, key("sk-new")),
        ]);
        let environment = session_home(root.path(), &second);
        ImportedLogins::prepare(&environment).unwrap();
        import(&environment);

        let connection = rusqlite::Connection::open(&database).unwrap();
        let mut statement = connection
            .prepare("SELECT id, active, value FROM credential ORDER BY id")
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<bool>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let rows = rows
            .into_iter()
            .map(|(id, active, value)| {
                let value: Value = serde_json::from_str(&value).unwrap();
                let secret = value.get("refresh").or(value.get("key")).cloned();
                (id, active == Some(true), secret)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                ("cred_deepseek_new".to_owned(), true, Some("sk-new".into())),
                ("cred_deepseek_old".to_owned(), false, Some("sk-old".into())),
                ("cred_openai".to_owned(), true, Some("refresh-2".into())),
            ]
        );
    }

    /// A session home whose database is a link to another database is
    /// refused, and the other database keeps its logins.
    #[cfg(unix)]
    #[test]
    fn a_linked_session_database_is_refused_and_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let other = root.path().join("other.db");
        let connection = rusqlite::Connection::open(&other).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE credential (id text, integration_id text, value text);
                 INSERT INTO credential VALUES ('cred', 'deepseek', '{}');",
            )
            .unwrap();
        let environment = session_home(root.path(), &serde_json::json!([]));
        let data = PathBuf::from(&environment["XDG_DATA_HOME"]).join("opencode");
        std::fs::create_dir_all(&data).unwrap();
        std::os::unix::fs::symlink(&other, data.join("opencode.db")).unwrap();

        let error = ImportedLogins::prepare(&environment).unwrap_err();

        assert!(
            format!("{error:#}").contains("through the link"),
            "{error:#}"
        );
        let rows: i64 = connection
            .query_row("SELECT count(*) FROM credential", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }
}
