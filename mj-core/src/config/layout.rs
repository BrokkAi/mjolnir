//! The one place that builds paths under the data and configuration
//! directories, and records what `mj migrate` does with each of them.
//!
//! `clippy.toml` bans `data_dir()` and `config_dir()` outside the two
//! `current()` constructors below, so a new file under either directory has to
//! get an accessor here, and with it an entry in the action tables. The tables
//! and the accessors share one name constant per entry, so they cannot drift.
//!
//! Directory layout changes are not made here: this module names what exists.

use std::path::{Component, Path, PathBuf};

use super::loading::{config_dir, data_dir};

/// What `mj migrate` does with a data or configuration entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrateAction {
    /// Copy to the new host (store, keys, checkpoints, config, secrets).
    Copy,
    /// Leave on the old host beside the fenced store while local sessions live.
    KeepWithLocalWorkers,
    /// Do not copy; the new host creates its own (host-bound certificates, ControlMaster dir).
    Recreate,
    /// Do not copy (locks, endpoints, sockets, temporary files, caches).
    Skip,
    /// Copy if the user asks (logs, diagnostics, decision logs).
    CopyOptional,
}

/// What a relative path under a data or configuration directory is.
///
/// `Unknown` is any name no code in this tree writes today: leftovers from
/// older releases, editor backups such as `config.toml~`, and hand-made copies.
/// Migrate's rule for `Unknown` is to copy it and report it. It never drops a
/// name silently, because a user's own files are worse to lose than extras are
/// to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    Known(MigrateAction),
    /// The `instances` directory or one of its children. Each child is a
    /// complete store with its own data or configuration layout.
    NestedStore,
    Unknown,
}

// Data directory names.
const DATABASE: &str = "mj.sqlite3";
const DATABASE_WAL: &str = "mj.sqlite3-wal";
const DATABASE_SHM: &str = "mj.sqlite3-shm";
const TOOL_DETAILS: &str = "tool-details";
const SESSIONS: &str = "sessions";
const PROJECTS: &str = "projects";
const CONFIG_RENAME_JOURNAL: &str = "config-rename.json";
const INSTANCE_IDENTITY: &str = "instance-identity";
const API_TOKEN: &str = "api-token";
const PHONE_COOKIE_KEY: &str = "phone-cookie-key";
const PHONE_COOKIE_REVOCATIONS: &str = "phone-cookie-revocations.json";
const WEB_PUSH_VAPID: &str = "web-push-vapid.pk8";
const SESSIONWIKI: &str = "sessionwiki";
const WORKERS: &str = "workers";
const PINNED_WORKERS: &str = "pinned";
const WORKER_BINARIES: &str = "worker-binaries";
const PROFILES: &str = "profiles";
const VIEWER: &str = "viewer";
const SSH: &str = "ssh";
const CONTROLLER_LOCK: &str = "controller.lock";
const DAEMON_START_LOCK: &str = "daemon-start.lock";
const SETUP_LOCK: &str = "setup.lock";
const SETUP_STATE: &str = "setup-state";
const DAEMON_JSON: &str = "daemon.json";
const API_ENDPOINT: &str = "api-endpoint.json";
const DAEMON_CONTAINER: &str = "daemon-container.json";
const DAEMON_EXITS: &str = "daemon-exits";
const IMAGE_PULLS: &str = "image-pulls";
const TERMINAL_UPGRADES: &str = "terminal-upgrades";
const UPLOADS: &str = "uploads";
const FETCH_SPOOL: &str = "fetch-spool";
const SNAPSHOT_CACHE: &str = "snapshot-cache";
const GITHUB_STUDENT: &str = "github-student";
const GITHUB_STUDENT_NATIVE: &str = "github-student-native";
const MODELS: &str = "models";
const MBX: &str = "mbx";
const MBX_DOWNLOAD: &str = "mbx-download";
const MBX_RELEASE_FAILURES: &str = "mbx-release-failures.jsonl";
const POWER_RANKING_CACHE: &str = "power-ranking-cache.json";
const SESSIONWIKI_BUILT: &str = "sessionwiki-built";
const DAEMON_LOG: &str = "daemon.log";
const LOGS: &str = "logs";
const DIAGNOSTICS: &str = "diagnostics";
const JEV_DECISIONS: &str = "jev-decisions";
const INSTANCES: &str = super::loading::INSTANCE_DIR;

// Configuration directory names.
const CONFIG_TOML: &str = "config.toml";
const GO_JSON: &str = "go.json";
const CONFIG_TOML_LOCK: &str = "config.toml.lock";
const GO_JSON_LOCK: &str = "go.json.lock";
const SECRETS_TOML: &str = super::secrets::SECRETS_FILE;
const EC2: &str = "ec2";
const CLIENT_HINTS: &str = "client-hints.json";

/// Name of the checkpoint archive files `sessions/` holds.
const CHECKPOINT_ARCHIVE_SUFFIX: &str = ".hel.zip";
/// Prefix of the temporary files checkpointing writes beside the archives.
const CHECKPOINT_TEMPORARY_PREFIX: &str = ".hel-checkpoint-";

/// Top-level data entries whose action does not depend on a deeper name.
const DATA_ENTRIES: &[(&str, MigrateAction)] = &[
    (DATABASE, MigrateAction::Copy),
    (DATABASE_WAL, MigrateAction::Copy),
    (DATABASE_SHM, MigrateAction::Skip),
    (TOOL_DETAILS, MigrateAction::Copy),
    (PROJECTS, MigrateAction::Copy),
    (CONFIG_RENAME_JOURNAL, MigrateAction::Copy),
    (INSTANCE_IDENTITY, MigrateAction::Copy),
    (API_TOKEN, MigrateAction::Copy),
    (PHONE_COOKIE_KEY, MigrateAction::Copy),
    (PHONE_COOKIE_REVOCATIONS, MigrateAction::Copy),
    (WEB_PUSH_VAPID, MigrateAction::Copy),
    // Contains `index.db`, a SQLite database: copy it only while stopped.
    (SESSIONWIKI, MigrateAction::Copy),
    (PROFILES, MigrateAction::KeepWithLocalWorkers),
    (VIEWER, MigrateAction::Recreate),
    (SSH, MigrateAction::Recreate),
    (CONTROLLER_LOCK, MigrateAction::Skip),
    (DAEMON_START_LOCK, MigrateAction::Skip),
    (SETUP_LOCK, MigrateAction::Skip),
    (SETUP_STATE, MigrateAction::Skip),
    (DAEMON_JSON, MigrateAction::Skip),
    (API_ENDPOINT, MigrateAction::Skip),
    (DAEMON_CONTAINER, MigrateAction::Skip),
    (DAEMON_EXITS, MigrateAction::Skip),
    (IMAGE_PULLS, MigrateAction::Skip),
    (TERMINAL_UPGRADES, MigrateAction::Skip),
    (UPLOADS, MigrateAction::Skip),
    (FETCH_SPOOL, MigrateAction::Skip),
    (SNAPSHOT_CACHE, MigrateAction::Skip),
    (GITHUB_STUDENT, MigrateAction::Skip),
    (GITHUB_STUDENT_NATIVE, MigrateAction::Skip),
    (MODELS, MigrateAction::Skip),
    (MBX, MigrateAction::Skip),
    (MBX_DOWNLOAD, MigrateAction::Skip),
    (MBX_RELEASE_FAILURES, MigrateAction::CopyOptional),
    (POWER_RANKING_CACHE, MigrateAction::Skip),
    (SESSIONWIKI_BUILT, MigrateAction::Skip),
    (DAEMON_LOG, MigrateAction::CopyOptional),
    (LOGS, MigrateAction::CopyOptional),
    (DIAGNOSTICS, MigrateAction::CopyOptional),
    (JEV_DECISIONS, MigrateAction::CopyOptional),
    // Legacy leftovers; current code no longer writes these diagnostics.
    ("backups", MigrateAction::CopyOptional),
    ("moves", MigrateAction::CopyOptional),
    // Legacy caches and log, no longer written by current code.
    ("git-brokers", MigrateAction::Skip),
    ("git-seeds", MigrateAction::Skip),
    ("update-check.json", MigrateAction::Skip),
    ("node.log", MigrateAction::Skip),
];

/// Top-level configuration entries.
const CONFIG_ENTRIES: &[(&str, MigrateAction)] = &[
    (CONFIG_TOML, MigrateAction::Copy),
    (GO_JSON, MigrateAction::Copy),
    (CONFIG_TOML_LOCK, MigrateAction::Skip),
    (GO_JSON_LOCK, MigrateAction::Skip),
    (SECRETS_TOML, MigrateAction::Copy),
    (PROFILES, MigrateAction::Copy),
    (EC2, MigrateAction::Copy),
    (CLIENT_HINTS, MigrateAction::Copy),
];

/// The first one or two normal components of `relative`. `None` when the path
/// is empty, absolute, climbs out of the directory, or is not UTF-8.
fn leading_names(relative: &Path) -> Option<(&str, Option<&str>)> {
    let mut names = relative.components().map(|component| match component {
        Component::Normal(name) => name.to_str(),
        _ => None,
    });
    let first = names.next()??;
    let second = match names.next() {
        None => None,
        Some(name) => Some(name?),
    };
    Some((first, second))
}

fn lookup(table: &[(&str, MigrateAction)], name: &str) -> Classification {
    table
        .iter()
        .find(|(entry, _)| *entry == name)
        .map_or(Classification::Unknown, |(_, action)| {
            Classification::Known(*action)
        })
}

/// Classify a path relative to a data directory.
///
/// `workers/`, `sessions/` and `instances/` mix classes, so the second path
/// component decides. The bare directory name returns what most of its
/// children are: `workers` and `sessions` classify as their default child, and
/// a caller that cares walks the children. Unknown names are `Unknown`; see
/// [`Classification`] for migrate's rule.
pub fn classify_data_entry(relative: &Path) -> Classification {
    let Some((first, second)) = leading_names(relative) else {
        return Classification::Unknown;
    };
    match (first, second) {
        (INSTANCES, _) => Classification::NestedStore,
        (WORKERS, Some(PINNED_WORKERS | WORKER_BINARIES)) => {
            Classification::Known(MigrateAction::Skip)
        }
        // Everything else under `workers/` is one session's worker root.
        (WORKERS, _) => Classification::Known(MigrateAction::KeepWithLocalWorkers),
        (SESSIONS, None) => Classification::Known(MigrateAction::Copy),
        (SESSIONS, Some(name)) if name.starts_with(CHECKPOINT_TEMPORARY_PREFIX) => {
            Classification::Known(MigrateAction::Skip)
        }
        (SESSIONS, Some(name)) if name.ends_with(CHECKPOINT_ARCHIVE_SUFFIX) => {
            Classification::Known(MigrateAction::Copy)
        }
        // The tombstone prevents a late upload from recreating deleted attachments.
        (SESSIONS, Some(_)) => {
            let attachments = relative
                .components()
                .nth(2)
                .and_then(|component| match component {
                    Component::Normal(name) => name.to_str(),
                    _ => None,
                });
            match attachments {
                None | Some(crate::attachment::ATTACHMENT_DIR | "attachments.deleted") => {
                    Classification::Known(MigrateAction::Copy)
                }
                Some("attachments.lock") => Classification::Known(MigrateAction::Skip),
                Some(_) => Classification::Unknown,
            }
        }
        (name, _) => lookup(DATA_ENTRIES, name),
    }
}

/// Classify a path relative to a configuration directory. See
/// [`classify_data_entry`].
pub fn classify_config_entry(relative: &Path) -> Classification {
    let Some((first, _)) = leading_names(relative) else {
        return Classification::Unknown;
    };
    match first {
        INSTANCES => Classification::NestedStore,
        name => lookup(CONFIG_ENTRIES, name),
    }
}

#[cfg(test)]
mod migration_classification_tests {
    use super::*;

    #[test]
    fn attachment_and_legacy_entries_keep_their_migration_semantics() {
        for (path, expected) in [
            (
                "sessions/id/attachments.lock",
                Classification::Known(MigrateAction::Skip),
            ),
            (
                "sessions/id/attachments.deleted",
                Classification::Known(MigrateAction::Copy),
            ),
            (
                "backups/old",
                Classification::Known(MigrateAction::CopyOptional),
            ),
            (
                "moves/old",
                Classification::Known(MigrateAction::CopyOptional),
            ),
            ("git-brokers/x", Classification::Known(MigrateAction::Skip)),
            ("git-seeds/x", Classification::Known(MigrateAction::Skip)),
            (
                "update-check.json",
                Classification::Known(MigrateAction::Skip),
            ),
            ("node.log", Classification::Known(MigrateAction::Skip)),
            ("sessions/id/user-file", Classification::Unknown),
            ("user-file", Classification::Unknown),
        ] {
            assert_eq!(classify_data_entry(Path::new(path)), expected, "{path}");
        }
    }
}

/// Every path under one data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataLayout {
    root: PathBuf,
}

impl DataLayout {
    /// The data directory this process uses: `MJ_DATA_DIR`, else the
    /// instance's or the default location.
    #[allow(
        clippy::disallowed_methods,
        reason = "the one constructor from data_dir"
    )]
    pub fn current() -> Self {
        Self { root: data_dir() }
    }

    /// A layout rooted at `root`, for a store at a path other than this
    /// process's own.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn entry(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub fn database(&self) -> PathBuf {
        self.entry(DATABASE)
    }
    pub fn tool_details_dir(&self) -> PathBuf {
        self.entry(TOOL_DETAILS)
    }
    /// Checkpoint archives, and one directory per session for attachments.
    pub fn sessions_dir(&self) -> PathBuf {
        self.entry(SESSIONS)
    }
    pub fn session_attachments_dir(&self, session_id: &str) -> PathBuf {
        self.sessions_dir()
            .join(session_id)
            .join(crate::attachment::ATTACHMENT_DIR)
    }
    pub fn projects_dir(&self) -> PathBuf {
        self.entry(PROJECTS)
    }
    pub fn config_rename_journal(&self) -> PathBuf {
        self.entry(CONFIG_RENAME_JOURNAL)
    }
    pub fn instance_identity(&self) -> PathBuf {
        self.entry(INSTANCE_IDENTITY)
    }
    pub fn api_token(&self) -> PathBuf {
        self.entry(API_TOKEN)
    }
    pub fn phone_cookie_key(&self) -> PathBuf {
        self.entry(PHONE_COOKIE_KEY)
    }
    pub fn phone_cookie_revocations(&self) -> PathBuf {
        self.entry(PHONE_COOKIE_REVOCATIONS)
    }
    pub fn web_push_vapid(&self) -> PathBuf {
        self.entry(WEB_PUSH_VAPID)
    }
    pub fn sessionwiki_dir(&self) -> PathBuf {
        self.entry(SESSIONWIKI)
    }
    /// Per-session worker roots (`<workers>/<session-id>`) and the pinned
    /// worker binary cache.
    pub fn workers_dir(&self) -> PathBuf {
        self.entry(WORKERS)
    }
    pub fn pinned_workers_dir(&self) -> PathBuf {
        self.workers_dir().join(PINNED_WORKERS)
    }
    /// One session's local worker root.
    pub fn worker_root(&self, session_id: &str) -> PathBuf {
        self.workers_dir().join(session_id)
    }
    /// Per-session nested harness homes of local workers.
    pub fn profiles_dir(&self) -> PathBuf {
        self.entry(PROFILES)
    }
    pub fn viewer_dir(&self) -> PathBuf {
        self.entry(VIEWER)
    }
    pub fn ssh_dir(&self) -> PathBuf {
        self.entry(SSH)
    }
    pub fn controller_lock(&self) -> PathBuf {
        self.entry(CONTROLLER_LOCK)
    }
    pub fn daemon_start_lock(&self) -> PathBuf {
        self.entry(DAEMON_START_LOCK)
    }
    pub fn setup_lock(&self) -> PathBuf {
        self.entry(SETUP_LOCK)
    }
    pub fn setup_state(&self) -> PathBuf {
        self.entry(SETUP_STATE)
    }
    pub fn daemon_json(&self) -> PathBuf {
        self.entry(DAEMON_JSON)
    }
    pub fn api_endpoint(&self) -> PathBuf {
        self.entry(API_ENDPOINT)
    }
    pub fn daemon_container(&self) -> PathBuf {
        self.entry(DAEMON_CONTAINER)
    }
    pub fn daemon_exits_dir(&self) -> PathBuf {
        self.entry(DAEMON_EXITS)
    }
    pub fn image_pulls_dir(&self) -> PathBuf {
        self.entry(IMAGE_PULLS)
    }
    pub fn terminal_upgrades_dir(&self) -> PathBuf {
        self.entry(TERMINAL_UPGRADES)
    }
    pub fn uploads_dir(&self) -> PathBuf {
        self.entry(UPLOADS)
    }
    pub fn fetch_spool_dir(&self) -> PathBuf {
        self.entry(FETCH_SPOOL)
    }
    pub fn snapshot_cache_dir(&self) -> PathBuf {
        self.entry(SNAPSHOT_CACHE)
    }
    pub fn github_student_dir(&self) -> PathBuf {
        self.entry(GITHUB_STUDENT)
    }
    pub fn github_student_native_dir(&self) -> PathBuf {
        self.entry(GITHUB_STUDENT_NATIVE)
    }
    pub fn models_dir(&self) -> PathBuf {
        self.entry(MODELS)
    }
    pub fn mbx_dir(&self) -> PathBuf {
        self.entry(MBX)
    }
    pub fn mbx_download_dir(&self) -> PathBuf {
        self.entry(MBX_DOWNLOAD)
    }
    pub fn mbx_release_failures(&self) -> PathBuf {
        self.entry(MBX_RELEASE_FAILURES)
    }
    pub fn power_ranking_cache(&self) -> PathBuf {
        self.entry(POWER_RANKING_CACHE)
    }
    pub fn sessionwiki_built(&self) -> PathBuf {
        self.entry(SESSIONWIKI_BUILT)
    }
    pub fn daemon_log(&self) -> PathBuf {
        self.entry(DAEMON_LOG)
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.entry(LOGS)
    }
    pub fn diagnostics_dir(&self) -> PathBuf {
        self.entry(DIAGNOSTICS)
    }
    pub fn jev_decisions_dir(&self) -> PathBuf {
        self.entry(JEV_DECISIONS)
    }
    /// Named instances under the default data directory; each child is a
    /// complete store.
    pub fn instances_dir(&self) -> PathBuf {
        self.entry(INSTANCES)
    }
}

/// Every path under one configuration directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigLayout {
    root: PathBuf,
}

impl ConfigLayout {
    /// The configuration directory this process uses: `MJ_CONFIG_DIR`, else
    /// the instance's or the default location.
    #[allow(
        clippy::disallowed_methods,
        reason = "the one constructor from config_dir"
    )]
    pub fn current() -> Self {
        Self { root: config_dir() }
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn entry(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub fn config_toml(&self) -> PathBuf {
        self.entry(CONFIG_TOML)
    }
    pub fn go_json(&self) -> PathBuf {
        self.entry(GO_JSON)
    }
    pub fn secrets_toml(&self) -> PathBuf {
        self.entry(SECRETS_TOML)
    }
    /// Managed harness profile homes, one per profile id.
    pub fn profiles_dir(&self) -> PathBuf {
        self.entry(PROFILES)
    }
    pub fn ec2_dir(&self) -> PathBuf {
        self.entry(EC2)
    }
    pub fn client_hints(&self) -> PathBuf {
        self.entry(CLIENT_HINTS)
    }
    /// Named instances under the default configuration directory.
    pub fn instances_dir(&self) -> PathBuf {
        self.entry(INSTANCES)
    }
}
