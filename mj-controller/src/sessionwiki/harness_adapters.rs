//! SessionWiki adapters for the harnesses SessionWiki itself has no adapter
//! for: Kimi Code, Grok Build and Muse.
//!
//! Mjolnir can start and import sessions for these harnesses, so their native
//! sessions belong in the same search index as a Claude Code or Codex one.
//! Each adapter covers one enabled profile home.
//!
//! These are shared-store adapters even though the sessions do live on disk.
//! Kimi and Grok keep one *directory* per session, and SessionWiki's
//! file-per-session path stats the discovered path and indexes on its
//! modification time; a directory's mtime does not move when a transcript
//! inside it grows. Enumerating by key lets the change token be the
//! transcript's own modification time, which does move.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use mj_core::config::HarnessKind;
use sessionwiki::adapters::{Adapter, Discovered, Store};
use sessionwiki::model::{Role, Session};

use crate::import::{list_native_session_sources, native_session_title, read_native_transcript};

use super::projected_messages;

/// One enabled profile home of a harness SessionWiki does not know.
pub(super) struct HarnessAdapter {
    kind: HarnessKind,
    /// The tool name this harness publishes under, resolved once so that
    /// `name` never has to account for a kind the constructor rejected.
    tool: &'static str,
    home: PathBuf,
}

/// The SessionWiki tool name for a harness, or `None` for a harness whose
/// sessions SessionWiki already indexes itself.
fn tool_name(kind: HarnessKind) -> Option<&'static str> {
    match kind {
        HarnessKind::Kimi => Some("kimi-code"),
        HarnessKind::Grok => Some("grok-build"),
        HarnessKind::Muse => Some("muse"),
        HarnessKind::Codex | HarnessKind::Claude => None,
    }
}

impl HarnessAdapter {
    /// An adapter for one profile home, or `None` when the harness is not one
    /// of Kimi Code, Grok Build and Muse.
    pub(super) fn in_home(kind: HarnessKind, home: PathBuf) -> Option<Self> {
        Some(Self {
            kind,
            tool: tool_name(kind)?,
            home,
        })
    }

    /// The directory this home keeps its sessions in.
    ///
    /// Canonicalized when it exists: Kimi's own listing canonicalizes every
    /// session path, so a root that still held a symlink would not be a prefix
    /// of the keys it produces, and [`Adapter::reconcile_scope`] would cover
    /// none of this adapter's rows.
    fn sessions_root(&self) -> Result<PathBuf> {
        let root = match self.kind {
            HarnessKind::Muse => mj_checkpoint::native::muse_sessions_root(&self.home)?,
            _ => self.home.join("sessions"),
        };
        Ok(root.canonicalize().unwrap_or(root))
    }
}

impl Adapter for HarnessAdapter {
    fn name(&self) -> &'static str {
        self.tool
    }

    fn root(&self) -> Option<PathBuf> {
        self.sessions_root().ok()
    }

    /// Unused: this is a shared-store adapter, so the indexer enumerates
    /// sessions through [`Adapter::store`] instead of walking files.
    fn discover(&self) -> Discovered {
        Discovered {
            files: Vec::new(),
            had_error: false,
        }
    }

    fn parse(&self, _path: &Path) -> Result<Session> {
        anyhow::bail!("{} sessions are parsed by key, not by file", self.tool)
    }

    fn store(&self) -> Option<Store> {
        let root = self.sessions_root().ok();
        // A home that has never run this harness is not a failure, and must
        // not set `had_error`: that would stop reconciliation for good.
        if !root.is_some_and(|root| root.is_dir()) {
            return Some(Store {
                keys: Vec::new(),
                files: Vec::new(),
                had_error: false,
            });
        }
        let sources = match list_native_session_sources(self.kind, &self.home) {
            Ok(sources) => sources,
            Err(error) => {
                tracing::debug!(
                    tool = self.tool,
                    home = %self.home.display(),
                    %error,
                    "could not list native sessions for SessionWiki"
                );
                // A partial or failed listing must not archive this home's
                // rows, so report it instead of returning an empty store.
                return Some(Store {
                    keys: Vec::new(),
                    files: Vec::new(),
                    had_error: true,
                });
            }
        };
        let mut keys = Vec::with_capacity(sources.len());
        let mut files = Vec::with_capacity(sources.len());
        for source in sources {
            let token = source
                .modified_at
                .duration_since(std::time::UNIX_EPOCH)
                .map(|age| age.as_secs() as i64)
                .unwrap_or_default();
            keys.push((source.source_path.display().to_string(), token));
            files.push(source.source_path);
        }
        Some(Store {
            keys,
            files,
            had_error: false,
        })
    }

    /// Two profiles of one kind publish under one tool name, so each adapter
    /// speaks only for the keys under its own sessions root. Without the
    /// scope, syncing one profile would archive the other's rows.
    fn reconcile_scope(&self) -> Option<String> {
        let root = self.sessions_root().ok()?;
        Some(format!("{}{}", root.display(), std::path::MAIN_SEPARATOR))
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let path = Path::new(key);
        anyhow::ensure!(!key.is_empty(), "no session path in key {key:?}");
        let transcript = read_native_transcript(self.kind, path)
            .with_context(|| format!("read the {} session {key}", self.tool))?;
        let session_id = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        let messages =
            projected_messages(&mj_transcript::projection::imported_materialized_session(
                &session_id,
                &transcript.events,
            ));
        let title = native_session_title(self.kind, path).unwrap_or_else(|| {
            messages
                .iter()
                .find(|message| message.role == Role::User)
                .map(|message| message.text.chars().take(80).collect())
                .unwrap_or_default()
        });
        Ok(Session {
            // The same derivation SessionWiki's own file adapters use, so an
            // id stays stable for as long as the session stays where it is.
            id: sessionwiki::util::short_id(key),
            tool: self.tool,
            path: path.to_path_buf(),
            project: transcript.cwd.display().to_string(),
            started: messages.first().and_then(|message| message.ts),
            ended: messages.last().and_then(|message| message.ts),
            title,
            subagent: false,
            messages,
            touched: transcript
                .edited_paths
                .iter()
                .map(|edited| edited.display().to_string())
                .collect(),
            edits: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use crate::import::test_fixtures::{MUSE_ID, grok_session, kimi_session, write_muse_session};

    const KIMI_ID: &str = "session_90c30a64-54f7-4261-90f1-e75b1c14311c";
    const KIMI_WIRE: &str = concat!(
        r#"{"type":"turn.prompt","origin":{"kind":"user"},"input":[{"type":"text","text":"first prompt"}]}"#,
        "\n",
        r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"first reply"}}}"#,
        "\n",
    );
    const GROK_HISTORY: &str = concat!(
        r#"{"type":"user","content":[{"type":"text","text":"first prompt"}],"prompt_index":0}"#,
        "\n",
        r#"{"type":"assistant","content":"first reply","model_id":"grok-4.6"}"#,
        "\n",
    );

    /// A temporary directory whose path holds no symlinks, so a key built from
    /// a canonicalized session path still starts with the adapter's root.
    fn profile_home(directory: &tempfile::TempDir, name: &str) -> PathBuf {
        let home = fs::canonicalize(directory.path()).unwrap().join(name);
        fs::create_dir_all(&home).unwrap();
        home
    }

    fn stored(adapter: &HarnessAdapter) -> Vec<(String, i64)> {
        let store = adapter.store().expect("a shared store");
        assert!(!store.had_error, "the listing must be complete");
        store.keys
    }

    fn only_key(adapter: &HarnessAdapter) -> String {
        let keys = stored(adapter);
        assert_eq!(keys.len(), 1, "one indexed session: {keys:?}");
        assert!(keys[0].1 > 0, "a session carries a change token: {keys:?}");
        let scope = adapter.reconcile_scope().expect("a per-home scope");
        assert!(
            keys[0].0.starts_with(&scope),
            "the scope {scope:?} must cover the key {:?}",
            keys[0].0
        );
        keys[0].0.clone()
    }

    fn texts(session: &Session, role: Role) -> Vec<String> {
        session
            .messages
            .iter()
            .filter(|message| message.role == role)
            .map(|message| message.text.clone())
            .collect()
    }

    #[test]
    fn kimi_sessions_are_indexed_from_their_profile_home() {
        let directory = tempfile::tempdir().unwrap();
        let home = profile_home(&directory, "kimi");
        let session_path = kimi_session(&home, KIMI_ID, "/work/app", "Native title", KIMI_WIRE);

        let adapter = HarnessAdapter::in_home(HarnessKind::Kimi, home.clone()).unwrap();
        assert_eq!(adapter.name(), "kimi-code");
        assert_eq!(adapter.root(), Some(home.join("sessions")));

        let key = only_key(&adapter);
        assert_eq!(key, session_path.display().to_string());

        let session = adapter.parse_key(&key).unwrap();
        assert_eq!(session.tool, "kimi-code");
        assert_eq!(session.project, "/work/app");
        // Kimi records a title of its own, so the conversation is not consulted.
        assert_eq!(session.title, "Native title");
        assert_eq!(texts(&session, Role::User), vec!["first prompt"]);
        assert_eq!(texts(&session, Role::Assistant), vec!["first reply"]);
    }

    #[test]
    fn grok_sessions_are_indexed_from_their_profile_home() {
        let directory = tempfile::tempdir().unwrap();
        let home = profile_home(&directory, "grok");
        let session_path = grok_session(&home, "/work/app", GROK_HISTORY);

        let adapter = HarnessAdapter::in_home(HarnessKind::Grok, home.clone()).unwrap();
        assert_eq!(adapter.name(), "grok-build");
        assert_eq!(adapter.root(), Some(home.join("sessions")));

        let key = only_key(&adapter);
        assert_eq!(key, session_path.display().to_string());

        let session = adapter.parse_key(&key).unwrap();
        assert_eq!(session.tool, "grok-build");
        assert_eq!(session.project, "/work/app");
        // This session has no summary of its own, so the first prompt names it.
        assert_eq!(session.title, "first prompt");
        assert_eq!(texts(&session, Role::User), vec!["first prompt"]);
        assert_eq!(texts(&session, Role::Assistant), vec!["first reply"]);
    }

    #[test]
    fn muse_sessions_are_indexed_from_their_profile_home() {
        let directory = tempfile::tempdir().unwrap();
        let home = profile_home(&directory, "muse");
        let cwd = profile_home(&directory, "app");
        let session_path = home
            .join(".data/muse/sessions/2026/09/08")
            .join(MUSE_ID)
            .join("session.jsonl");
        fs::create_dir_all(session_path.parent().unwrap()).unwrap();
        write_muse_session(&session_path, MUSE_ID, &cwd);

        let adapter = HarnessAdapter::in_home(HarnessKind::Muse, home.clone()).unwrap();
        assert_eq!(adapter.name(), "muse");
        assert_eq!(adapter.root(), Some(home.join(".data/muse/sessions")));

        let key = only_key(&adapter);
        assert_eq!(key, session_path.display().to_string());

        let session = adapter.parse_key(&key).unwrap();
        assert_eq!(session.tool, "muse");
        assert_eq!(session.project, cwd.display().to_string());
        assert_eq!(session.title, "remember Muse import");
        assert_eq!(texts(&session, Role::User), vec!["remember Muse import"]);
        assert_eq!(texts(&session, Role::Assistant), vec!["hello back"]);
    }

    #[test]
    fn a_change_token_moves_when_the_transcript_grows() {
        let directory = tempfile::tempdir().unwrap();
        let home = profile_home(&directory, "kimi");
        let session_path = kimi_session(&home, KIMI_ID, "/work/app", "Native title", KIMI_WIRE);
        let adapter = HarnessAdapter::in_home(HarnessKind::Kimi, home).unwrap();
        let before = stored(&adapter);

        let wire = session_path.join("agents/main/wire.jsonl");
        let mut appended = fs::read_to_string(&wire).unwrap();
        appended.push_str(
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"second reply"}}}"#,
        );
        appended.push('\n');
        fs::write(&wire, appended).unwrap();
        // A same-second rewrite leaves the whole-second token where it was, so
        // the new time is set explicitly rather than inferred from the write.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
        fs::File::options()
            .write(true)
            .open(&wire)
            .unwrap()
            .set_modified(later)
            .unwrap();

        let after = stored(&adapter);
        assert_eq!(before.len(), 1);
        assert_eq!(after.len(), 1);
        assert_eq!(before[0].0, after[0].0, "the key is stable");
        assert!(
            after[0].1 > before[0].1,
            "an appended transcript moves the token: {before:?} -> {after:?}"
        );
    }
}
