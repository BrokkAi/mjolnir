//! Cross-process shared cache ("fact") for provider usage probes.
//!
//! Several mj processes often run at once (multiple TUIs across
//! worktrees, `mj server`, headless runs), and each used to spawn its
//! own `claude -p /usage` probe — a full Claude Code process. This
//! store keeps one global fact per provider in a small sqlite database
//! under the mj config dir, plus a checkout lease so exactly one
//! process refreshes a stale fact while the others wait and read the
//! shared result.
//!
//! This is deliberately a separate database from the remote-control
//! server's `sessions.sqlite3`: that schema is owned and migrated by
//! `mj server` and lives next to its certificates and tokens, while
//! every mj client needs the usage fact even when no server runs.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

/// Default location of the shared usage-fact database:
/// `$XDG_CONFIG_HOME/mj/usage.sqlite3`.
pub fn default_store_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("mj")
        .join("usage.sqlite3")
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFact {
    pub payload: String,
    pub fetched_at: i64,
}

#[derive(Debug, Clone)]
pub struct UsageFactStore {
    path: PathBuf,
}

impl UsageFactStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn connect(&self) -> rusqlite::Result<Connection> {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(&self.path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        // WAL keeps concurrent readers from blocking the writer holding
        // the checkout lease.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS usage_facts (
                provider TEXT PRIMARY KEY,
                payload TEXT,
                fetched_at INTEGER NOT NULL DEFAULT 0,
                lease_owner TEXT,
                lease_expires_at INTEGER
            )",
        )?;
        Ok(conn)
    }

    /// The current fact for `provider`, if one has ever been published.
    pub fn read(&self, provider: &str) -> rusqlite::Result<Option<StoredFact>> {
        self.connect()?
            .query_row(
                "SELECT payload, fetched_at FROM usage_facts
                 WHERE provider = ?1 AND payload IS NOT NULL",
                params![provider],
                |row| {
                    Ok(StoredFact {
                        payload: row.get(0)?,
                        fetched_at: row.get(1)?,
                    })
                },
            )
            .optional()
    }

    /// Atomically claim the refresh lease for `provider`. Returns true
    /// when this owner now holds the lease and should run the probe.
    /// Fails when another live owner holds an unexpired lease; expired
    /// leases (crashed or hung holders) are taken over.
    pub fn try_checkout(
        &self,
        provider: &str,
        owner: &str,
        lease: Duration,
        now: i64,
    ) -> rusqlite::Result<bool> {
        let expires = now.saturating_add(lease.as_secs() as i64);
        let changed = self.connect()?.execute(
            "INSERT INTO usage_facts (provider, lease_owner, lease_expires_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(provider) DO UPDATE SET lease_owner = ?2, lease_expires_at = ?3
             WHERE usage_facts.lease_owner IS NULL
                OR usage_facts.lease_owner = ?2
                OR usage_facts.lease_expires_at <= ?4",
            params![provider, owner, expires, now],
        )?;
        Ok(changed > 0)
    }

    /// Store a fresh fact so waiters read it. The lease is cleared only
    /// while `owner` still holds it: a probe that finishes after its
    /// lease expired and was taken over must not unlock the newer
    /// holder's lease and trigger a redundant probe.
    pub fn publish(
        &self,
        provider: &str,
        payload: &str,
        owner: &str,
        now: i64,
    ) -> rusqlite::Result<()> {
        self.connect()?.execute(
            "INSERT INTO usage_facts (provider, payload, fetched_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(provider) DO UPDATE SET
                payload = ?2,
                fetched_at = ?3,
                lease_owner = CASE
                    WHEN usage_facts.lease_owner = ?4 THEN NULL
                    ELSE usage_facts.lease_owner
                END,
                lease_expires_at = CASE
                    WHEN usage_facts.lease_owner = ?4 THEN NULL
                    ELSE usage_facts.lease_expires_at
                END",
            params![provider, payload, now, owner],
        )?;
        Ok(())
    }

    /// Give the lease back without publishing (probe machinery failed
    /// before producing a result worth caching).
    pub fn release(&self, provider: &str, owner: &str) -> rusqlite::Result<()> {
        self.connect()?.execute(
            "UPDATE usage_facts SET lease_owner = NULL, lease_expires_at = NULL
             WHERE provider = ?1 AND lease_owner = ?2",
            params![provider, owner],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, UsageFactStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = UsageFactStore::new(dir.path().join("usage.sqlite3"));
        (dir, store)
    }

    #[test]
    fn checkout_excludes_other_owners_until_lease_expires() {
        let (_dir, store) = store();
        assert!(
            store
                .try_checkout("claude", "a", Duration::from_secs(30), 1000)
                .expect("checkout a")
        );
        assert!(
            !store
                .try_checkout("claude", "b", Duration::from_secs(30), 1010)
                .expect("checkout b while leased")
        );
        // The same owner may renew its own lease.
        assert!(
            store
                .try_checkout("claude", "a", Duration::from_secs(30), 1010)
                .expect("renew a")
        );
        // After expiry another owner takes the lease over.
        assert!(
            store
                .try_checkout("claude", "b", Duration::from_secs(30), 1041)
                .expect("checkout b after expiry")
        );
    }

    #[test]
    fn publish_clears_lease_and_serves_readers() {
        let (_dir, store) = store();
        assert!(
            store
                .try_checkout("claude", "a", Duration::from_secs(30), 1000)
                .expect("checkout")
        );
        assert_eq!(store.read("claude").expect("read"), None);
        store
            .publish("claude", "{\"ok\":true}", "a", 1005)
            .expect("publish");
        assert_eq!(
            store.read("claude").expect("read"),
            Some(StoredFact {
                payload: "{\"ok\":true}".to_string(),
                fetched_at: 1005,
            })
        );
        // The lease is free again once the fact is published.
        assert!(
            store
                .try_checkout("claude", "b", Duration::from_secs(30), 1006)
                .expect("checkout after publish")
        );
    }

    #[test]
    fn release_only_clears_the_matching_owner() {
        let (_dir, store) = store();
        assert!(
            store
                .try_checkout("claude", "a", Duration::from_secs(30), 1000)
                .expect("checkout")
        );
        store.release("claude", "b").expect("release wrong owner");
        assert!(
            !store
                .try_checkout("claude", "b", Duration::from_secs(30), 1001)
                .expect("still leased")
        );
        store.release("claude", "a").expect("release");
        assert!(
            store
                .try_checkout("claude", "b", Duration::from_secs(30), 1002)
                .expect("checkout after release")
        );
    }

    #[test]
    fn publish_from_a_stale_owner_keeps_the_current_lease() {
        let (_dir, store) = store();
        assert!(
            store
                .try_checkout("claude", "a", Duration::from_secs(30), 1000)
                .expect("checkout a")
        );
        // a's lease expires and b takes over the refresh.
        assert!(
            store
                .try_checkout("claude", "b", Duration::from_secs(30), 1031)
                .expect("checkout b after expiry")
        );
        // a's slow probe still lands its data, but b keeps the lease.
        store
            .publish("claude", "late-fact", "a", 1032)
            .expect("publish from stale owner");
        let fact = store.read("claude").expect("read").expect("fact");
        assert_eq!(fact.payload, "late-fact");
        assert!(
            !store
                .try_checkout("claude", "c", Duration::from_secs(30), 1033)
                .expect("b's lease survives the stale publish")
        );
    }

    #[test]
    fn facts_are_isolated_per_provider() {
        let (_dir, store) = store();
        store
            .publish("claude", "claude-fact", "a", 1000)
            .expect("publish");
        assert_eq!(store.read("codex").expect("read"), None);
        assert!(
            store
                .try_checkout("codex", "a", Duration::from_secs(30), 1000)
                .expect("codex checkout unaffected")
        );
        let fact = store.read("claude").expect("read").expect("fact");
        assert_eq!(fact.payload, "claude-fact");
    }
}
