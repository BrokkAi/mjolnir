//! Controller host migration support.

pub mod preflight;
pub mod rewrite;

use anyhow::{Context, Result, ensure};
use mj_core::config::{Config, DataLayout};
use mj_core::project_memory::ProjectMemoryIdentity;
use mj_core::state::Checkout;
use mj_store::database::{
    StoreFenceRecord, load_state_with, read_pinned_memory_key_with, read_store_fence,
};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::collections::BTreeMap;
use std::path::Path;

/// Capture legacy identities from the final fenced source snapshot.
pub fn capture_fenced_pinned_memory_keys(
    source_db_path: &Path,
    expected_fence: &StoreFenceRecord,
    config: &Config,
) -> Result<BTreeMap<String, String>> {
    let mut connection = Connection::open_with_flags(
        source_db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let fence = read_store_fence(&tx)?.context("source store is not fenced")?;
    ensure!(
        fence.matches_snapshot(expected_fence),
        "source store fence does not match migration source"
    );
    let root = source_db_path
        .parent()
        .context("source database has no parent directory")?;
    let state = load_state_with(&tx, &DataLayout::at(root).sessions_dir())?;
    let mut keys = BTreeMap::new();
    for session in state
        .sessions
        .values()
        .filter(|session| session.project.is_none())
    {
        let owner = state.project_identity_session(session);
        let pinned = read_pinned_memory_key_with(&tx, &session.id)?;
        let pinned = if pinned.is_none() && owner.id != session.id {
            read_pinned_memory_key_with(&tx, &owner.id)?
        } else {
            pinned
        };
        if let Some(key) = pinned {
            keys.insert(session.id.clone(), key);
            continue;
        }
        let checkout = owner.checkout();
        let bundle = checkout
            .project_directory()
            .is_none()
            .then(|| owner.project_bundle(config))
            .flatten();
        let has_worktree = matches!(checkout.effective(), Checkout::ManagedWorktree { .. })
            || owner.managed_worktree.is_some();
        if !has_worktree && bundle.is_none() && checkout.project_directory().is_none() {
            continue;
        }
        let identity = ProjectMemoryIdentity::for_legacy_session(
            session,
            &checkout,
            bundle,
            owner.managed_worktree.as_deref(),
        )?;
        keys.insert(session.id.clone(), identity.key()?);
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::config::{MachineId, SshConnection};
    use mj_core::state::TargetLocator;
    use mj_store::database::{
        MigrationRewriteSpec, apply_store_migration, pin_session_memory_key_with, save_session_to,
        set_store_fence,
    };

    #[test]
    fn fenced_memory_capture_uses_final_snapshot_and_skips_missing_identity() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        std::fs::create_dir_all(data.join("sessions")).unwrap();
        let path = data.join("mj.sqlite3");
        let fence = StoreFenceRecord {
            destination: "destination".into(),
            fenced_at: "now".into(),
            fenced_host: "source".into(),
            fenced_data_dir: data.clone(),
            note: None,
        };
        let mut gone = mj_store::database::test_session("gone", "removed-bundle");
        gone.checkpoint = None;
        gone.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: DataLayout::at(&data).worker_root("gone"),
        }));
        gone.project_directory = Some(root.path().join("project-gone"));
        save_session_to(&path, &gone).unwrap();
        assert!(capture_fenced_pinned_memory_keys(&path, &fence, &Config::default()).is_err());
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys=ON; DELETE FROM sessions WHERE session_id='gone';")
            .unwrap();
        drop(connection);
        let mut fresh = gone.clone();
        fresh.id = "fresh".into();
        fresh.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: DataLayout::at(&data).worker_root("fresh"),
        }));
        fresh.project_directory = Some(root.path().join("project-fresh"));
        save_session_to(&path, &fresh).unwrap();
        let mut pinned = fresh.clone();
        pinned.id = "pinned".into();
        pinned.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: DataLayout::at(&data).worker_root("pinned"),
        }));
        save_session_to(&path, &pinned).unwrap();
        let mut missing = fresh.clone();
        missing.id = "missing".into();
        missing.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: DataLayout::at(&data).worker_root("missing"),
        }));
        missing.project_directory = None;
        save_session_to(&path, &missing).unwrap();
        let connection = Connection::open(&path).unwrap();
        pin_session_memory_key_with(&connection, "pinned", &"b".repeat(64)).unwrap();
        set_store_fence(&connection, &fence).unwrap();
        let mut mismatch = fence.clone();
        mismatch.fenced_host = "wrong".into();
        assert!(capture_fenced_pinned_memory_keys(&path, &mismatch, &Config::default()).is_err());
        let keys = capture_fenced_pinned_memory_keys(&path, &fence, &Config::default()).unwrap();
        assert_eq!(
            keys.keys().map(String::as_str).collect::<Vec<_>>(),
            ["fresh", "pinned"]
        );
        let expected =
            ProjectMemoryIdentity::for_legacy_session(&fresh, &fresh.checkout(), None, None)
                .unwrap()
                .key()
                .unwrap();
        assert_eq!(keys["fresh"], expected);
        assert_eq!(keys["pinned"], "b".repeat(64));
        drop(connection);
        let spec = MigrationRewriteSpec {
            expected_fence: fence,
            old_data_root: data.clone(),
            new_data_root: root.path().join("new-data"),
            old_config_root: root.path().join("old-config"),
            new_config_root: root.path().join("new-config"),
            old_home: root.path().join("old-home"),
            new_home: root.path().join("new-home"),
            remote_template_ids: Default::default(),
            source_ssh: SshConnection {
                host: "source".into(),
                user: None,
                identity_file: None,
                extra_args: vec![],
            },
            source_machine_id: MachineId::new("source").unwrap(),
            source_templates: BTreeMap::from([(
                "local".into(),
                BTreeMap::from([("local-bare".into(), "source-bare".into())]),
            )]),
            pinned_memory_keys: keys,
            staged_sessions_dir: data.join("sessions"),
        };
        let summary = apply_store_migration(&path, &spec).unwrap();
        assert_eq!(summary.pinned_memory_keys, 2);
    }
}
