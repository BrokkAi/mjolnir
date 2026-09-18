//! Looking one native session up by the id the user named.
//!
//! Every harness listing is a resume picker: it hides sessions the user is
//! unlikely to want back, and it skips anything it cannot read. Neither
//! applies once the user names a session by id, so each importer falls back to
//! a lookup that goes straight to the path the id points at.
//!
//! What that lookup refuses is the same for all four harnesses, because the
//! archive step behind them is the same: `collect_native_tree` stores only
//! regular files and real directories inside the harness home, so anything
//! reached through a symlink would either be copied from outside the home or
//! archived as nothing at all. This module holds that judgement and its
//! wording once, so a refusal reads the same whichever importer produced it.

use super::*;

/// What a lookup by session id found where the id pointed.
pub(super) enum NamedEntry {
    /// Nothing is there. The lookup keeps searching and may end up reporting
    /// the session as not found.
    Absent,
    /// The entry can be imported, with the `symlink_metadata` already read.
    Importable(fs::Metadata),
    /// Something is there that cannot be imported, with the reason to report
    /// in place of "not found".
    Rejected(String),
}

/// How one harness stores a session, so every importer refuses the same things
/// in the same words.
#[derive(Clone, Copy)]
pub(super) struct NamedSessionStore {
    /// The harness name as error text uses it, such as `Grok Build`.
    harness: &'static str,
    /// What the harness keeps per session, named in the plural: `transcripts`
    /// or `session directories`.
    stored: &'static str,
}

pub(super) const CLAUDE_STORE: NamedSessionStore = NamedSessionStore {
    harness: "Claude",
    stored: "transcripts",
};
pub(super) const CODEX_STORE: NamedSessionStore = NamedSessionStore {
    harness: "Codex",
    stored: "rollouts",
};
pub(super) const KIMI_STORE: NamedSessionStore = NamedSessionStore {
    harness: "Kimi",
    stored: "session directories",
};
pub(super) const GROK_STORE: NamedSessionStore = NamedSessionStore {
    harness: "Grok Build",
    stored: "session directories",
};

impl NamedSessionStore {
    /// Classify the file a named session id points at.
    pub fn file(self, path: &Path) -> NamedEntry {
        self.classify(path, false)
    }

    /// Classify the directory a named session id points at.
    pub fn directory(self, path: &Path) -> NamedEntry {
        self.classify(path, true)
    }

    fn classify(self, path: &Path, want_directory: bool) -> NamedEntry {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return NamedEntry::Absent;
            }
            Err(error) => {
                return NamedEntry::Rejected(format!("{} cannot be read: {error}", path.display()));
            }
        };
        if metadata.file_type().is_symlink() {
            return NamedEntry::Rejected(format!(
                "{} is a symlink; {}",
                path.display(),
                self.only_stored_directly()
            ));
        }
        if want_directory {
            if !metadata.is_dir() {
                return NamedEntry::Rejected(format!("{} is not a directory", path.display()));
            }
        } else if !metadata.is_file() {
            return NamedEntry::Rejected(format!("{} is not a regular file", path.display()));
        }
        NamedEntry::Importable(metadata)
    }

    /// The refusal for a symlinked directory the named session sits inside,
    /// such as a Claude project directory or a Grok working directory.
    /// `described_as` is how the message names that directory.
    pub fn symlinked_container(self, path: &Path, described_as: &str) -> String {
        format!(
            "{} is a symlinked {described_as}; {}",
            path.display(),
            self.only_stored_directly()
        )
    }

    /// The refusal for a session whose own records never say which directory
    /// it ran in. The archive is collected relative to that directory, so
    /// there is nothing to import without it.
    pub fn no_cwd(self, path: &Path) -> String {
        format!("{} session {} has no cwd", self.harness, path.display())
    }

    /// The error a lookup reports when everything at the named path was
    /// refused. It names the reasons instead of saying the session is missing.
    pub fn cannot_import(self, native_session_id: &str, rejected: &[String]) -> anyhow::Error {
        anyhow::anyhow!(
            "{} session {native_session_id:?} cannot be imported: {}",
            self.harness,
            rejected.join("; ")
        )
    }

    fn only_stored_directly(self) -> String {
        format!(
            "Mjolnir imports only {} stored directly in the {} home",
            self.stored, self.harness
        )
    }
}
