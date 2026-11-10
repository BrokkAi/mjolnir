//! Profile registration against SessionWiki's real adapters, index and search.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use mj_core::config::{Config, HarnessKind, HarnessProfile};
use serde_json::json;
use sessionwiki::adapters::Adapter;

use crate::import::test_fixtures::{MUSE_ID, grok_session, kimi_session, write_muse_session};

use super::{
    WikiContinuation, harness_tools, native_adapters, query_rows_from, tags, top_level,
    wiki_continuation,
};

fn write_session(kind: HarnessKind, home: &Path, cwd: &Path) -> PathBuf {
    match kind {
        HarnessKind::Kimi => kimi_session(
            home,
            "session_90c30a64-54f7-4261-90f1-e75b1c14311c",
            &cwd.to_string_lossy(),
            "Native title",
            concat!(
                r#"{"type":"turn.prompt","origin":{"kind":"user"},"input":[{"type":"text","text":"remember native indexing"}]}"#,
                "\n",
                r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"hello back"}}}"#,
                "\n",
            ),
        ),
        HarnessKind::Grok => grok_session(
            home,
            &cwd.to_string_lossy(),
            concat!(
                r#"{"type":"user","content":[{"type":"text","text":"remember native indexing"}],"prompt_index":0}"#,
                "\n",
                r#"{"type":"assistant","content":"hello back","model_id":"grok-4.6"}"#,
                "\n",
            ),
        ),
        HarnessKind::Muse => {
            let path = home
                .join(".data/muse/sessions/2026/09/08")
                .join(MUSE_ID)
                .join("session.jsonl");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_muse_session(&path, MUSE_ID, cwd);
            path
        }
        HarnessKind::Dsh => {
            let path = home.join("sessions/90c30a64-54f7-4261-90f1-e75b1c14311c.json");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                &path,
                json!({
                    "id": "90c30a64-54f7-4261-90f1-e75b1c14311c",
                    "cwd": cwd,
                    "title": "Native title",
                    "created_at": "2026-11-10T00:00:00Z",
                    "updated_at": "2026-11-10T00:01:00Z",
                    "config": {"model": "deepseek-chat"},
                    "messages": [
                        {"role": "user", "content": "remember native indexing"},
                        {"role": "assistant", "content": "hello back"}
                    ]
                })
                .to_string(),
            )
            .unwrap();
            path
        }
        _ => unreachable!("fixture only covers the extracted native adapters"),
    }
}

/// Seed the former mj adapter's key and searchable transcript in the real
/// schema, so an adapter switch must update that row rather than duplicate it.
fn seed_legacy_row(connection: &rusqlite::Connection, path: &Path, tool: &str) -> String {
    let key = path.to_str().unwrap();
    let id = sessionwiki::util::short_id(key);
    tags::testing::index_row(connection, &id, tool);
    connection
        .execute(
            "UPDATE files SET path = ?1 WHERE session_id = ?2",
            [key, &id],
        )
        .unwrap();
    let text = "remember legacy native history";
    connection
        .execute(
            "INSERT INTO messages(session_id, role, text) VALUES (?1, 'user', ?2)",
            [id.as_str(), text],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO msgs(rowid, text) VALUES (?1, ?2)",
            rusqlite::params![connection.last_insert_rowid(), text],
        )
        .unwrap();
    id
}

#[test]
fn muse_native_search_and_import_agree_on_the_xdg_root() {
    let native_config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| mj_core::path_input::user_home().unwrap().join(".config"))
        .join("muse");
    let import_root = mj_checkpoint::native::muse_sessions_root(&native_config).unwrap();
    let index_root = sessionwiki::adapters::Muse::in_home(native_config)
        .keep_configured_paths()
        .root()
        .unwrap();
    let canonical =
        |path: PathBuf| sessionwiki::util::canonicalize_as_recorded(&path).unwrap_or(path);
    assert_eq!(
        canonical(index_root),
        canonical(import_root),
        "native search and import must agree on Muse's XDG data directory on every OS"
    );
}

// Hard-won: cff4a06e6: canonical Grok/Muse roots changed existing keys beneath symlinked homes.
#[test]
fn native_profiles_search_and_reconcile_only_their_own_home() {
    let _held = tags::testing::lock();
    let (_index, mut connection) = tags::testing::isolated_index();
    let directory = tempfile::tempdir().unwrap();
    let root = sessionwiki::util::canonicalize_as_recorded(directory.path()).unwrap();
    for kind in [
        HarnessKind::Kimi,
        HarnessKind::Grok,
        HarnessKind::Muse,
        HarnessKind::Dsh,
    ] {
        let tool = harness_tools::tool_name(kind);
        let mut config = Config::default();
        config.profiles.clear();
        let mut paths = Vec::new();
        for name in ["first", "second", "disabled"] {
            let home = root.join(tool).join(name);
            #[cfg(unix)]
            let home = if matches!(kind, HarnessKind::Grok | HarnessKind::Muse) && name == "first" {
                fs::create_dir_all(&home).unwrap();
                let linked = root.join(tool).join("first-link");
                std::os::unix::fs::symlink(&home, &linked).unwrap();
                linked
            } else {
                home
            };
            let cwd = home.join("workspace");
            fs::create_dir_all(&cwd).unwrap();
            let path = write_session(kind, &home, &cwd);
            // Kimi retains canonical directory keys, including Windows'
            // extended prefix. Grok/Muse retain the configured home spelling.
            paths.push(match kind {
                HarnessKind::Kimi => path.canonicalize().unwrap(),
                _ => path,
            });
            let mut profile = HarnessProfile::new(kind, home);
            profile.enabled = name != "disabled";
            config.profiles.insert(name.to_owned(), profile);
        }
        config
            .profiles
            .insert("duplicate".into(), config.profiles["first"].clone());
        let adapters: Vec<_> = native_adapters(&config)
            .into_iter()
            .filter(|adapter| adapter.name() == tool)
            .collect();
        assert_eq!(adapters.len(), 2, "only distinct enabled {tool} homes");
        let legacy_id = matches!(kind, HarnessKind::Grok | HarnessKind::Muse)
            .then(|| seed_legacy_row(&connection, &paths[0], tool));
        sessionwiki::index::sync_with(&mut connection, &adapters, None).unwrap();

        let hits =
            sessionwiki::index::search(&connection, "remember", 10, Some(tool), None).unwrap();
        let indexed: BTreeSet<_> = hits.iter().map(|hit| hit.row.path.as_str()).collect();
        let expected: BTreeSet<_> = paths[..2]
            .iter()
            .map(|path| path.to_str().unwrap())
            .collect();
        assert_eq!(indexed, expected, "stable path keys for {tool}");
        assert!(hits.iter().all(|hit| !hit.row.archived));
        if let Some(id) = &legacy_id {
            assert!(
                hits.iter()
                    .any(|hit| hit.row.session_id.as_str() == id.as_str()),
                "the {tool} row indexed before the adapter switch keeps its ID"
            );
        }
        for hit in &hits {
            let path = Path::new(&hit.row.path);
            if kind != HarnessKind::Dsh {
                // Muse stores its ID above session.jsonl; import must still
                // resolve the indexed path through its harness-specific reader.
                let native_session_id = if kind == HarnessKind::Muse {
                    path.parent().unwrap().file_name().unwrap()
                } else {
                    path.file_name().unwrap()
                };
                assert_eq!(
                    wiki_continuation(&hit.row.session_id, tool, path, false).unwrap(),
                    WikiContinuation::Import {
                        harness: kind,
                        native_session_id: native_session_id.to_str().unwrap().to_owned(),
                    }
                );
                let home = if hit.row.path == paths[0].to_str().unwrap() {
                    &config.profiles["first"].home
                } else {
                    &config.profiles["second"].home
                };
                let sources = crate::import::list_native_session_sources(kind, home).unwrap();
                assert!(
                    sources.iter().any(|source| {
                        source.source_path.canonicalize().unwrap() == path.canonicalize().unwrap()
                    }),
                    "the native import reader must locate the indexed {tool} session"
                );
            }
        }

        let rows = query_rows_from(
            &connection,
            "remember",
            100,
            &BTreeSet::new(),
            false,
            &top_level::Snapshot::default(),
            &crate::import::NativeScanCache::new(),
        )
        .unwrap();
        assert_eq!(rows.iter().filter(|row| row.tool == tool).count(), 2);

        // A standalone writer uses canonical discovery, but the shared index
        // must retain the key and ID established by the former mj adapter.
        if let Some(id) = &legacy_id {
            let home = &config.profiles["first"].home;
            let canonical: Box<dyn Adapter> = match kind {
                HarnessKind::Grok => Box::new(sessionwiki::adapters::GrokBuild::in_home(home)),
                HarnessKind::Muse => Box::new(sessionwiki::adapters::Muse::in_home(home)),
                _ => unreachable!(),
            };
            sessionwiki::index::sync_with(&mut connection, &[canonical], None).unwrap();
            let hits =
                sessionwiki::index::search(&connection, "remember", 10, Some(tool), None).unwrap();
            assert_eq!(
                hits.iter()
                    .map(|hit| hit.row.path.as_str())
                    .collect::<BTreeSet<_>>(),
                expected,
                "alternate writers must reuse existing {tool} keys"
            );
            assert!(hits.iter().all(|hit| !hit.row.archived));
            assert!(
                hits.iter()
                    .any(|hit| hit.row.session_id.as_str() == id.as_str())
            );
        }

        if paths[0].is_dir() {
            fs::remove_dir_all(&paths[0]).unwrap();
        } else {
            fs::remove_file(&paths[0]).unwrap();
        }
        // Reconcile just the emptied home. Its scope must not archive the
        // other enabled home's row, even though both publish the same tool.
        let first = adapters
            .into_iter()
            .find(|adapter| {
                paths[0]
                    .to_str()
                    .unwrap()
                    .starts_with(&adapter.reconcile_scope().expect("per-home scope"))
            })
            .unwrap();
        sessionwiki::index::sync_with(&mut connection, &[first], None).unwrap();
        let hits =
            sessionwiki::index::search(&connection, "remember", 10, Some(tool), None).unwrap();
        assert_eq!(hits.len(), 2, "deleted native sessions remain searchable");
        for hit in hits {
            assert_eq!(
                hit.row.archived,
                hit.row.path == paths[0].to_str().unwrap(),
                "{tool} reconciliation must affect only its own home"
            );
        }
    }
}
