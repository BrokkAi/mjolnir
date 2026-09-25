//! Harness skills whitelist and its deterministic wire archive.
//!
//! A profile's skills tree is user-managed content, not a rotating secret, so
//! unlike credentials it syncs in one direction only: the controller-side
//! canonical home is authoritative and every live session converges to it.
//! This module owns the whole interpretation — which directories each harness
//! syncs, how a tree becomes a fingerprinted archive, and how an archive is
//! installed — with no relay, async, or process dependencies, so every rule
//! is testable in isolation.

use crate::hex::lower_hex;
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::config::HarnessKind;

mod managed;

pub use managed::managed_skills;

/// Skills archives travel base64-encoded inside an 8 MiB relay frame. The cap
/// keeps the encoded payload, envelope, and a credential payload comfortably
/// inside one frame each way.
pub const MAX_SKILLS_ARCHIVE_BYTES: usize = 4 * 1024 * 1024;
/// A single skill file above this is almost certainly a checked-in binary,
/// not a skill.
pub const MAX_SKILLS_FILE_BYTES: u64 = 1024 * 1024;
pub const MAX_SKILLS_FILES: usize = 1024;

const ARCHIVE_MAGIC: &[u8; 8] = b"HELSKIL1";

/// Non-secret metadata about one copy of a skills tree. Fingerprints compare
/// trees; there is no freshness concept because the controller copy always
/// wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillsSyncState {
    pub present: bool,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillsEntry {
    /// Home-relative, `/`-separated path, always inside a synced directory.
    pub path: String,
    pub bytes: Vec<u8>,
}

/// A deterministic, fingerprintable snapshot of every synced skills tree in
/// one harness home.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillsArchive {
    entries: Vec<SkillsEntry>,
}

impl SkillsArchive {
    pub fn entries(&self) -> &[SkillsEntry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn state(&self) -> SkillsSyncState {
        SkillsSyncState {
            present: !self.entries.is_empty(),
            fingerprint: self.fingerprint(),
        }
    }

    /// SHA-256 over the canonical encoding. Collection sorts entries, so two
    /// homes holding the same tree fingerprint identically.
    pub fn fingerprint(&self) -> String {
        lower_hex(Sha256::digest(self.encode()))
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ARCHIVE_MAGIC.len() + 4);
        out.extend_from_slice(ARCHIVE_MAGIC);
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for entry in &self.entries {
            out.extend_from_slice(&(entry.path.len() as u32).to_le_bytes());
            out.extend_from_slice(entry.path.as_bytes());
            out.extend_from_slice(&(entry.bytes.len() as u64).to_le_bytes());
            out.extend_from_slice(&entry.bytes);
        }
        out
    }

    /// Parse an untrusted archive. Every rule that keeps an install inside
    /// the whitelist is enforced here and again at install time: relative
    /// `/`-separated paths, no traversal, sorted and unique, within caps.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_SKILLS_ARCHIVE_BYTES {
            bail!(
                "skills archive is {} bytes, above the {MAX_SKILLS_ARCHIVE_BYTES} byte limit",
                bytes.len()
            );
        }
        let mut cursor = Cursor(bytes);
        if cursor.take(ARCHIVE_MAGIC.len())? != ARCHIVE_MAGIC {
            bail!("skills archive has a bad magic header");
        }
        let count = cursor.u32()? as usize;
        if count > MAX_SKILLS_FILES {
            bail!("skills archive holds {count} files, above the {MAX_SKILLS_FILES} file limit");
        }
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let path_len = cursor.u32()? as usize;
            let path = std::str::from_utf8(cursor.take(path_len)?)
                .context("skills archive path is not valid UTF-8")?
                .to_owned();
            validate_archive_path(&path)?;
            if entries
                .last()
                .is_some_and(|last: &SkillsEntry| last.path >= path)
            {
                bail!("skills archive paths are not sorted and unique");
            }
            let data_len = usize::try_from(cursor.u64()?)
                .map_err(|_| anyhow::anyhow!("skills archive entry length overflows usize"))?;
            if data_len as u64 > MAX_SKILLS_FILE_BYTES {
                bail!(
                    "skills archive entry {path} is {data_len} bytes, above the {MAX_SKILLS_FILE_BYTES} byte limit"
                );
            }
            let bytes = cursor.take(data_len)?.to_vec();
            entries.push(SkillsEntry { path, bytes });
        }
        if !cursor.rest().is_empty() {
            bail!("skills archive has trailing bytes");
        }
        Ok(Self { entries })
    }
}

/// How a collection treats a symbolic link inside a skills tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Links {
    /// Leave it out. A session's own home holds only the regular files
    /// staging and installs wrote, and its worker never reads outside them.
    Skip,
    /// Read through it, as launch staging copies a profile home.
    Follow,
}

/// Snapshot the synced skills trees of one home. A home without any synced
/// directory collects as an empty archive, which compares equal to a session
/// in the same state. Symlinks inside the tree are skipped: this is how a
/// worker reads its session's home, and how the sync reads a home a session
/// runs out of directly. [`collect_profile_skills`] reads a home that launch
/// staging copies instead.
///
/// Both leave out the paths the harness maintains itself
/// ([`HarnessKind::harness_owned_skill_paths`]), such as the skills Claude
/// Code syncs from the user's claude.ai account: the harness keeps its own
/// copy current in every home, so Mjolnir neither compares nor copies it.
pub fn collect_skills(kind: HarnessKind, home: &Path) -> Result<SkillsArchive> {
    collect(kind, home, Links::Skip)
}

/// Snapshot a profile home's synced skills trees the way launch staging copies
/// them: through symbolic links, skipping a link whose target is missing and a
/// directory that links back into itself. A session whose home was staged
/// from this profile then fingerprints the same as this archive.
pub fn collect_profile_skills(kind: HarnessKind, home: &Path) -> Result<SkillsArchive> {
    collect(kind, home, Links::Follow)
}

fn collect(kind: HarnessKind, home: &Path, links: Links) -> Result<SkillsArchive> {
    let mut entries = Vec::new();
    for dir in kind.synced_skill_dirs() {
        let root = home.join(dir);
        if !root.exists() {
            continue;
        }
        let walk = Walk {
            links,
            harness_owned: kind.harness_owned_skill_paths(),
        };
        collect_tree(&root, dir, &mut entries, walk, &[])
            .with_context(|| format!("collect skills from {}", root.display()))?;
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let archive = SkillsArchive { entries };
    let encoded = archive.encode();
    if encoded.len() > MAX_SKILLS_ARCHIVE_BYTES {
        bail!(
            "skills tree under {} encodes to {} bytes, above the {MAX_SKILLS_ARCHIVE_BYTES} byte limit",
            home.display(),
            encoded.len()
        );
    }
    Ok(archive)
}

/// The skills tree a session gets: the user's own skills from `home`, plus the
/// skills Mjolnir manages.
///
/// A managed entry replaces a user entry at the same path, so the session
/// always runs Mjolnir's copy of a managed skill. Launch staging and the
/// credential-sync push both compute the tree this way; if they disagreed, the
/// first reconciliation after launch would wipe whatever the other installed.
pub fn session_skills(kind: HarnessKind, home: &Path) -> Result<SkillsArchive> {
    let collected = collect_profile_skills(kind, home)?;
    let mut entries = collected.entries;
    for entry in managed_skills(kind) {
        match entries.binary_search_by(|existing| existing.path.cmp(&entry.path)) {
            Ok(index) => {
                tracing::warn!(
                    path = %entry.path,
                    home = %home.display(),
                    "a user skill has the path of a Mjolnir-managed skill; the managed skill replaces it"
                );
                entries[index] = entry;
            }
            Err(index) => entries.insert(index, entry),
        }
    }
    if entries.len() > MAX_SKILLS_FILES {
        bail!("skills tree has more than {MAX_SKILLS_FILES} files");
    }
    let archive = SkillsArchive { entries };
    let encoded = archive.encode();
    if encoded.len() > MAX_SKILLS_ARCHIVE_BYTES {
        bail!(
            "skills tree under {} encodes to {} bytes with managed skills, above the {MAX_SKILLS_ARCHIVE_BYTES} byte limit",
            home.display(),
            encoded.len()
        );
    }
    Ok(archive)
}

/// How one collection walks a skills tree.
#[derive(Debug, Clone, Copy)]
struct Walk {
    links: Links,
    /// Home-relative paths the harness maintains itself; the walk leaves them
    /// out ([`HarnessKind::harness_owned_skill_paths`]).
    harness_owned: &'static [&'static str],
}

/// `entered` holds the resolved directories already entered on this branch of
/// the walk, which stops a followed link that points back at an ancestor.
fn collect_tree(
    root: &Path,
    prefix: &str,
    entries: &mut Vec<SkillsEntry>,
    walk: Walk,
    entered: &[std::path::PathBuf],
) -> Result<()> {
    let links = walk.links;
    let mut entered = entered.to_vec();
    if links == Links::Follow {
        let resolved = std::fs::canonicalize(root)
            .with_context(|| format!("resolve skills directory {}", root.display()))?;
        if entered.contains(&resolved) {
            return Ok(());
        }
        entered.push(resolved);
    }
    let mut children = std::fs::read_dir(root)?.collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(|child| child.file_name());
    for child in children {
        if entries.len() >= MAX_SKILLS_FILES {
            bail!("skills tree has more than {MAX_SKILLS_FILES} files");
        }
        let path = child.path();
        let name = child.file_name();
        let Some(name) = name.to_str() else {
            bail!("skills file name {} is not valid UTF-8", path.display());
        };
        let relative = format!("{prefix}/{name}");
        if walk.harness_owned.contains(&relative.as_str()) {
            continue;
        }
        let mut metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            if links == Links::Skip {
                continue;
            }
            metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                // Staging skips a link whose target is gone, so the
                // canonical tree leaves it out as well.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("read skills link {}", path.display()));
                }
            };
        }
        if metadata.is_dir() {
            collect_tree(&path, &relative, entries, walk, &entered)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        // One file that cannot travel must not stop the rest of the tree from
        // syncing. Both sides of the sync skip it the same way, so their
        // fingerprints still agree. A session keeps the copy staging gave it
        // until a later push rebuilds the tree from the archive without it.
        if metadata.len() > MAX_SKILLS_FILE_BYTES {
            skip_skill_file(
                &path,
                &format!(
                    "is {} bytes, above the {MAX_SKILLS_FILE_BYTES} byte limit",
                    metadata.len()
                ),
            );
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                skip_skill_file(&path, &format!("could not be read: {error}"));
                continue;
            }
        };
        entries.push(SkillsEntry {
            path: relative,
            bytes,
        });
    }
    Ok(())
}

/// Leaves one skill file out of a collection, saying so the first time.
fn skip_skill_file(path: &Path, problem: &str) {
    if first_report_of_skipped_skill(path) {
        tracing::warn!(
            path = %path.display(),
            "skills file {problem}; leaving it out of skills sync"
        );
    } else {
        tracing::debug!(
            path = %path.display(),
            "skills file {problem}; leaving it out of skills sync"
        );
    }
}

/// Whether this process has not yet reported skipping `path`. Collection runs
/// on every sync poll, so a file that stays too large would otherwise be
/// reported once a minute for the life of the worker.
fn first_report_of_skipped_skill(path: &Path) -> bool {
    static REPORTED: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeSet<std::path::PathBuf>>,
    > = std::sync::OnceLock::new();
    REPORTED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(path.to_path_buf())
}

/// Replace a session home's synced skills trees with an archive's contents.
///
/// Each synced directory is built beside the destination and swapped in, so a
/// failure mid-install never leaves a half-written tree. A symlinked
/// destination is refused rather than followed, and entry paths outside the
/// harness's synced directories are rejected outright.
///
/// The paths the harness maintains itself
/// ([`HarnessKind::harness_owned_skill_paths`]) are not Mjolnir's to replace:
/// an archive entry inside one is ignored like any other entry outside the
/// whitelist, and whatever the session home holds there moves into the new
/// tree unchanged.
pub fn install_skills(kind: HarnessKind, home: &Path, archive: &SkillsArchive) -> Result<()> {
    let harness_owned = kind.harness_owned_skill_paths();
    for dir in kind.synced_skill_dirs() {
        let entries = archive
            .entries
            .iter()
            .filter(|entry| within(&entry.path, dir))
            .filter(|entry| !harness_owned.iter().any(|owned| within(&entry.path, owned)))
            .collect::<Vec<_>>();
        let kept = harness_owned
            .iter()
            .filter_map(|owned| owned.strip_prefix(&format!("{dir}/")))
            .collect::<Vec<_>>();
        install_tree(home, dir, &entries, &kept)
            .with_context(|| format!("install skills into {}", home.join(dir).display()))?;
    }
    Ok(())
}

/// Whether a `/`-separated path is `directory` or lies beneath it.
fn within(path: &str, directory: &str) -> bool {
    path.strip_prefix(directory)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Swap `entries` in as the tree at `dir`, carrying the destination's `kept`
/// children (names directly under `dir`) into the new tree as they are.
fn install_tree(home: &Path, dir: &str, entries: &[&SkillsEntry], kept: &[&str]) -> Result<()> {
    let destination = home.join(dir);
    let incoming = home.join(format!("{dir}.hel-incoming"));
    let retired = home.join(format!("{dir}.hel-retired"));
    if std::fs::symlink_metadata(&destination)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!(
            "skills destination {} is a symbolic link",
            destination.display()
        );
    }
    for path in [&incoming, &retired] {
        if let Err(error) = std::fs::remove_dir_all(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %path.display(),
                %error,
                "could not remove stale skills staging tree"
            );
        }
    }
    let kept = kept
        .iter()
        .copied()
        .filter(|name| std::fs::symlink_metadata(destination.join(name)).is_ok())
        .collect::<Vec<_>>();
    for entry in entries {
        let relative = entry
            .path
            .strip_prefix(&format!("{dir}/"))
            .context("skills entry escaped its synced directory")?;
        let target = incoming.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, &entry.bytes)?;
    }
    if !kept.is_empty() {
        std::fs::create_dir_all(&incoming)?;
    }
    if destination.exists() {
        std::fs::rename(&destination, &retired)?;
    }
    if entries.is_empty() && kept.is_empty() {
        if let Err(error) = std::fs::remove_dir_all(&retired)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %retired.display(),
                %error,
                "could not remove retired skills tree"
            );
        }
        return Ok(());
    }
    let swapped = move_children(&retired, &incoming, &kept).and_then(|()| {
        std::fs::rename(&incoming, &destination).map_err(|error| {
            if let Err(return_error) = move_children(&incoming, &retired, &kept) {
                tracing::error!(
                    retired = %retired.display(),
                    error = %format!("{return_error:#}"),
                    "could not return the harness's own skills to the previous tree after a failed swap"
                );
            }
            anyhow::Error::new(error)
        })
    });
    if let Err(error) = swapped {
        // Restore the previous tree so a failed swap never strands a session
        // without skills it had before.
        if let Err(restore_error) = std::fs::rename(&retired, &destination) {
            tracing::error!(
                destination = %destination.display(),
                error = %restore_error,
                "could not restore the previous skills tree after a failed swap"
            );
            return Err(error).context(format!(
                "swap refreshed skills tree into place; restoring the previous tree also failed: {restore_error}"
            ));
        }
        return Err(error).context("swap refreshed skills tree into place");
    }
    if let Err(error) = std::fs::remove_dir_all(&retired)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %retired.display(),
            %error,
            "could not remove retired skills tree after a successful swap"
        );
    }
    Ok(())
}

/// Move the named children of `from` into `to`. A failure moves the children
/// already moved back, so the harness's own directories stay in one tree.
fn move_children(from: &Path, to: &Path, names: &[&str]) -> Result<()> {
    for (index, name) in names.iter().enumerate() {
        if let Err(error) = std::fs::rename(from.join(name), to.join(name)) {
            for moved in &names[..index] {
                if let Err(back_error) = std::fs::rename(to.join(moved), from.join(moved)) {
                    tracing::error!(
                        path = %to.join(moved).display(),
                        error = %back_error,
                        "could not move a harness-owned skills directory back"
                    );
                }
            }
            return Err(error).with_context(|| {
                format!(
                    "move {} to {}",
                    from.join(name).display(),
                    to.join(name).display()
                )
            });
        }
    }
    Ok(())
}

fn validate_archive_path(path: &str) -> Result<()> {
    if path.is_empty() {
        bail!("skills archive entry has an empty path");
    }
    if path.contains('\\') {
        bail!("skills archive path {path:?} uses backslash separators");
    }
    // Validate the raw string rather than `Path::components`, which would
    // silently normalize away `.` and repeated separators.
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.contains('\0') {
            bail!("skills archive path {path:?} is not a safe relative path");
        }
    }
    if Path::new(path).is_absolute() {
        bail!("skills archive path {path:?} is not a safe relative path");
    }
    Ok(())
}

/// Byte cursor with checked bounds so decode failures are errors, not panics.
struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        if self.0.len() < count {
            bail!("skills archive is truncated");
        }
        let (taken, rest) = self.0.split_at(count);
        self.0 = rest;
        Ok(taken)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }

    fn rest(&self) -> &[u8] {
        self.0
    }
}

/// Home-relative join used only by tests and diagnostics; the install path
/// never joins an unvalidated archive path onto a home.
#[cfg(test)]
fn entry_path(home: &Path, entry: &SkillsEntry) -> std::path::PathBuf {
    home.join(&entry.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(home: &Path, relative: &str, bytes: &[u8]) {
        let path = home.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn archive(entries: &[(&str, &[u8])]) -> SkillsArchive {
        let mut entries = entries
            .iter()
            .map(|(path, bytes)| SkillsEntry {
                path: (*path).to_owned(),
                bytes: bytes.to_vec(),
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        SkillsArchive { entries }
    }

    #[test]
    fn every_harness_syncs_a_skills_directory() {
        for kind in HarnessKind::ALL {
            assert_eq!(kind.synced_skill_dirs(), &["skills"]);
            // Install carries a harness-owned path across by moving it, which
            // works for a direct child of a synced directory only.
            for owned in kind.harness_owned_skill_paths() {
                assert!(
                    kind.synced_skill_dirs().iter().any(|dir| owned
                        .strip_prefix(&format!("{dir}/"))
                        .is_some_and(|name| !name.is_empty() && !name.contains('/'))),
                    "{kind:?} {owned}"
                );
            }
        }
        assert_eq!(
            HarnessKind::Claude.harness_owned_skill_paths(),
            &["skills/synced", "skills/.trash"]
        );
    }

    #[test]
    fn a_home_without_skills_collects_an_empty_archive() {
        let home = tempfile::tempdir().unwrap();
        let archive = collect_skills(HarnessKind::Claude, home.path()).unwrap();
        assert!(archive.is_empty());
        assert!(!archive.state().present);
    }

    #[test]
    fn collection_is_sorted_deterministic_and_skips_symlinks() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), "skills/review/SKILL.md", b"review");
        write(home.path(), "skills/review/checklist.md", b"check");
        write(home.path(), "skills/audit/SKILL.md", b"audit");
        write(
            home.path(),
            "skills/.DS_Store",
            b"junk but kept: it is a real file",
        );
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            home.path().join("skills/review"),
            home.path().join("skills/linked"),
        )
        .unwrap();

        let first = collect_skills(HarnessKind::Codex, home.path()).unwrap();
        let second = collect_skills(HarnessKind::Codex, home.path()).unwrap();
        assert_eq!(first, second);
        let paths = first
            .entries()
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                "skills/.DS_Store",
                "skills/audit/SKILL.md",
                "skills/review/SKILL.md",
                "skills/review/checklist.md",
            ]
        );
        assert!(first.state().present);
        assert_eq!(first.fingerprint(), second.fingerprint());
    }

    #[test]
    fn managed_skills_are_installable_archive_entries() {
        for kind in HarnessKind::ALL {
            let entries = managed_skills(kind);
            assert_eq!(entries.len(), 1, "{kind:?}");
            let prefix = kind.synced_skill_dirs()[0];
            for entry in &entries {
                validate_archive_path(&entry.path).expect(&entry.path);
                assert!(entry.path.starts_with(&format!("{prefix}/")), "{entry:?}");
                assert!(entry.bytes.len() as u64 <= MAX_SKILLS_FILE_BYTES);
                assert!(
                    entry.bytes.starts_with(b"---\nname: "),
                    "{} needs skill frontmatter",
                    entry.path
                );
            }
            // The install path only writes what `decode` accepts, so the
            // managed set has to survive a round trip on its own.
            let archive = SkillsArchive {
                entries: entries.clone(),
            };
            let encoded = archive.encode();
            assert!(encoded.len() <= MAX_SKILLS_ARCHIVE_BYTES);
            assert_eq!(SkillsArchive::decode(&encoded).unwrap(), archive);
        }
    }

    #[test]
    fn session_skills_of_an_empty_home_is_the_managed_set() {
        let home = tempfile::tempdir().unwrap();
        let archive = session_skills(HarnessKind::Claude, home.path()).unwrap();
        assert_eq!(archive.entries(), managed_skills(HarnessKind::Claude));
        assert!(archive.state().present);
        // Collection is unchanged: it still reports the user tree alone.
        assert!(
            collect_skills(HarnessKind::Claude, home.path())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn user_recall_and_provenance_skills_remain_user_owned() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), "skills/recall/SKILL.md", b"user recall");
        write(
            home.path(),
            "skills/provenance/SKILL.md",
            b"user provenance",
        );
        let archive = session_skills(HarnessKind::Codex, home.path()).unwrap();
        for name in ["recall", "provenance"] {
            let entry = archive
                .entries()
                .iter()
                .find(|entry| entry.path == format!("skills/{name}/SKILL.md"))
                .unwrap();
            assert_eq!(entry.bytes, format!("user {name}").as_bytes());
        }
    }

    #[test]
    fn a_managed_skill_replaces_a_user_skill_with_the_same_path() {
        let home = tempfile::tempdir().unwrap();
        write(
            home.path(),
            "skills/mj/SKILL.md",
            b"the user's own mj skill",
        );
        write(home.path(), "skills/review/SKILL.md", b"review");

        let archive = session_skills(HarnessKind::Codex, home.path()).unwrap();
        let managed = managed_skills(HarnessKind::Codex);
        let mine = archive
            .entries()
            .iter()
            .find(|entry| entry.path == "skills/mj/SKILL.md")
            .unwrap();
        assert_eq!(mine, &managed[0]);
        // The user's unrelated skill is kept, and nothing is duplicated.
        assert!(
            archive
                .entries()
                .iter()
                .any(|entry| entry.path == "skills/review/SKILL.md" && entry.bytes == b"review")
        );
        assert_eq!(archive.entries().len(), managed.len() + 1);
        let mut sorted = archive.entries().to_vec();
        sorted.sort_by(|left, right| left.path.cmp(&right.path));
        assert_eq!(sorted, archive.entries());
    }

    #[test]
    fn encoding_roundtrips_and_rejects_tampering() {
        let original = archive(&[
            ("skills/review/SKILL.md", b"review"),
            ("skills/audit/SKILL.md", b"audit"),
        ]);
        let decoded = SkillsArchive::decode(&original.encode()).unwrap();
        assert_eq!(original, decoded);
        assert_eq!(original.fingerprint(), decoded.fingerprint());

        assert!(SkillsArchive::decode(b"NOTSKILL").is_err());
        let mut truncated = original.encode();
        truncated.pop();
        assert!(SkillsArchive::decode(&truncated).is_err());
        let mut trailing = original.encode();
        trailing.push(0);
        assert!(SkillsArchive::decode(&trailing).is_err());
    }

    #[test]
    fn decode_rejects_unsafe_and_unsorted_paths() {
        for path in [
            "../escape",
            "skills/../escape",
            "/absolute",
            "skills\\windows",
            "skills//double",
            "skills/",
            "",
        ] {
            let hostile = archive(&[]);
            let encoded = {
                let mut out = Vec::new();
                out.extend_from_slice(ARCHIVE_MAGIC);
                out.extend_from_slice(&1u32.to_le_bytes());
                out.extend_from_slice(&(path.len() as u32).to_le_bytes());
                out.extend_from_slice(path.as_bytes());
                out.extend_from_slice(&0u64.to_le_bytes());
                let _ = hostile;
                out
            };
            assert!(
                SkillsArchive::decode(&encoded).is_err(),
                "path {path:?} must be rejected"
            );
        }

        let unsorted = {
            let mut out = Vec::new();
            out.extend_from_slice(ARCHIVE_MAGIC);
            out.extend_from_slice(&2u32.to_le_bytes());
            for path in ["skills/b", "skills/a"] {
                out.extend_from_slice(&(path.len() as u32).to_le_bytes());
                out.extend_from_slice(path.as_bytes());
                out.extend_from_slice(&0u64.to_le_bytes());
            }
            out
        };
        assert!(SkillsArchive::decode(&unsorted).is_err());
    }

    #[test]
    fn decode_rejects_oversized_archives() {
        let oversized = vec![b'x'; MAX_SKILLS_ARCHIVE_BYTES + 1];
        assert!(SkillsArchive::decode(&oversized).is_err());
    }

    /// Launch finding R4-8: one 2.2 MB demo file under a skill made every
    /// worker fail `skills_state` once a minute. The file is skipped and the
    /// rest of the tree still collects.
    #[test]
    fn an_oversized_skill_file_is_skipped_rather_than_failing_the_tree() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), "skills/viz/SKILL.md", b"viz");
        write(
            home.path(),
            "skills/viz/demos/large.html",
            &vec![b'x'; usize::try_from(MAX_SKILLS_FILE_BYTES).unwrap() + 1],
        );

        let archive = collect_skills(HarnessKind::Claude, home.path()).unwrap();

        let paths = archive
            .entries()
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["skills/viz/SKILL.md"]);
        // The next poll collects the same tree, so the fingerprint is stable.
        assert_eq!(
            collect_skills(HarnessKind::Claude, home.path())
                .unwrap()
                .fingerprint(),
            archive.fingerprint()
        );
    }

    #[test]
    fn a_skipped_skill_file_is_reported_once() {
        let path = Path::new("/nonexistent/skills-report-once/large.html");
        assert!(first_report_of_skipped_skill(path));
        assert!(!first_report_of_skipped_skill(path));
        assert!(first_report_of_skipped_skill(Path::new(
            "/nonexistent/skills-report-once/other.html"
        )));
    }

    /// A profile home that links a skill from elsewhere is staged with the
    /// link's contents, so the canonical tree the sync compares against has to
    /// read through the link too. Otherwise the first successful sync removes
    /// the linked skill from every session.
    #[cfg(unix)]
    #[test]
    fn profile_collection_follows_links_as_staging_does() {
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "viz/SKILL.md", b"linked viz");
        let home = tempfile::tempdir().unwrap();
        write(home.path(), "skills/own/SKILL.md", b"own");
        std::os::unix::fs::symlink(outside.path().join("viz"), home.path().join("skills/viz"))
            .unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("missing"),
            home.path().join("skills/gone"),
        )
        .unwrap();
        // A link back to an ancestor is entered once, not forever.
        std::os::unix::fs::symlink(
            home.path().join("skills"),
            home.path().join("skills/own/loop"),
        )
        .unwrap();

        let archive = collect_profile_skills(HarnessKind::Claude, home.path()).unwrap();

        let paths = archive
            .entries()
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["skills/own/SKILL.md", "skills/viz/SKILL.md"]);
        // The session side still ignores links: a worker never reads outside
        // the tree it was given.
        assert_eq!(
            collect_skills(HarnessKind::Claude, home.path())
                .unwrap()
                .entries()
                .len(),
            1
        );
    }

    /// Claude Code's own synced skills, as it lays them out under a Claude
    /// home: a `.bucket-…` marker and an `<org>_<user>` directory holding a
    /// manifest and one directory per skill, plus the `.trash` directory it
    /// moves removed skills into.
    fn write_claude_code_synced_skills(home: &Path) {
        write(home, "skills/synced/.bucket-org_user", b"");
        write(home, "skills/synced/org_user/manifest.json", b"{}");
        write(home, "skills/synced/org_user/docx/SKILL.md", b"docx");
        write(
            home,
            "skills/synced/org_user/docx/ooxml/schema.xsd",
            b"schema",
        );
        write(home, "skills/.trash/1789646711611/pdf/SKILL.md", b"old pdf");
    }

    /// Claude Code provisions `skills/synced/` from the user's claude.ai
    /// account and re-syncs it on its own; on the launch host its Office
    /// schemas alone were 4.2 MB, enough to push the tree over the archive
    /// limit. Mjolnir leaves it, and Claude Code's `.trash`, out of every
    /// collection of a Claude home.
    #[test]
    fn claude_codes_own_synced_skills_are_left_out_of_collection() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), "skills/review/SKILL.md", b"review");
        write_claude_code_synced_skills(home.path());

        let user = [SkillsEntry {
            path: "skills/review/SKILL.md".into(),
            bytes: b"review".to_vec(),
        }];
        assert_eq!(
            collect_skills(HarnessKind::Claude, home.path())
                .unwrap()
                .entries(),
            user
        );
        assert_eq!(
            collect_profile_skills(HarnessKind::Claude, home.path())
                .unwrap()
                .entries(),
            user
        );
        let mut session = user.to_vec();
        session.extend(managed_skills(HarnessKind::Claude));
        session.sort_by(|left, right| left.path.cmp(&right.path));
        assert_eq!(
            session_skills(HarnessKind::Claude, home.path())
                .unwrap()
                .entries(),
            session
        );

        // The directories are Claude Code's, not a rule about the name: in
        // another harness's home a skill called `synced` is the user's.
        let codex = collect_skills(HarnessKind::Codex, home.path()).unwrap();
        assert!(
            codex
                .entries()
                .iter()
                .any(|entry| entry.path == "skills/synced/org_user/docx/SKILL.md"),
            "{codex:?}"
        );
    }

    /// A session's Claude Code keeps its own synced skills under the session
    /// home, and a copy that an earlier sync or launch put there cannot be
    /// told apart from it. An install replaces only what Mjolnir owns, and
    /// never writes into Claude Code's directories.
    #[test]
    fn install_leaves_claude_codes_synced_skills_in_place() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), "skills/old/SKILL.md", b"old");
        write_claude_code_synced_skills(home.path());

        let pushed = archive(&[
            ("skills/review/SKILL.md", b"review"),
            ("skills/synced/org_user/docx/SKILL.md", b"pushed over"),
        ]);
        install_skills(HarnessKind::Claude, home.path(), &pushed).unwrap();

        assert_eq!(
            std::fs::read(home.path().join("skills/review/SKILL.md")).unwrap(),
            b"review"
        );
        assert!(!home.path().join("skills/old").exists());
        for (relative, bytes) in [
            ("skills/synced/.bucket-org_user", &b""[..]),
            ("skills/synced/org_user/manifest.json", b"{}"),
            ("skills/synced/org_user/docx/SKILL.md", b"docx"),
            ("skills/synced/org_user/docx/ooxml/schema.xsd", b"schema"),
            ("skills/.trash/1789646711611/pdf/SKILL.md", b"old pdf"),
        ] {
            assert_eq!(
                std::fs::read(home.path().join(relative)).unwrap(),
                bytes,
                "{relative}"
            );
        }
        assert!(!home.path().join("skills.hel-incoming").exists());
        assert!(!home.path().join("skills.hel-retired").exists());
        // The session reports what Mjolnir pushed, so the next reconcile
        // finds nothing to do.
        assert_eq!(
            collect_skills(HarnessKind::Claude, home.path())
                .unwrap()
                .entries(),
            [SkillsEntry {
                path: "skills/review/SKILL.md".into(),
                bytes: b"review".to_vec(),
            }]
        );

        // Removing every Mjolnir skill still leaves Claude Code's own.
        install_skills(HarnessKind::Claude, home.path(), &SkillsArchive::default()).unwrap();
        assert!(!home.path().join("skills/review").exists());
        assert_eq!(
            std::fs::read(home.path().join("skills/synced/org_user/docx/SKILL.md")).unwrap(),
            b"docx"
        );
        assert!(home.path().join("skills/.trash").exists());
    }

    #[test]
    fn collection_enforces_the_file_count_cap() {
        let home = tempfile::tempdir().unwrap();
        for index in 0..MAX_SKILLS_FILES + 1 {
            write(home.path(), &format!("skills/skill-{index}"), b"x");
        }
        assert!(collect_skills(HarnessKind::Kimi, home.path()).is_err());
    }

    #[test]
    fn install_creates_replaces_and_removes_trees() {
        let home = tempfile::tempdir().unwrap();
        let first = archive(&[
            ("skills/review/SKILL.md", b"v1"),
            ("skills/audit/SKILL.md", b"audit"),
        ]);
        install_skills(HarnessKind::Claude, home.path(), &first).unwrap();
        assert_eq!(
            std::fs::read(home.path().join("skills/review/SKILL.md")).unwrap(),
            b"v1"
        );

        let second = archive(&[("skills/review/SKILL.md", b"v2")]);
        install_skills(HarnessKind::Claude, home.path(), &second).unwrap();
        assert_eq!(
            std::fs::read(home.path().join("skills/review/SKILL.md")).unwrap(),
            b"v2"
        );
        // Removed from the canonical tree, so removed from the session.
        assert!(!home.path().join("skills/audit").exists());
        assert!(!home.path().join("skills.hel-incoming").exists());
        assert!(!home.path().join("skills.hel-retired").exists());

        install_skills(HarnessKind::Claude, home.path(), &SkillsArchive::default()).unwrap();
        assert!(!home.path().join("skills").exists());
    }

    #[cfg(unix)]
    #[test]
    fn install_refuses_a_symlinked_destination() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), home.path().join("skills")).unwrap();
        let archive = archive(&[("skills/evil", b"payload")]);
        let error = install_skills(HarnessKind::Codex, home.path(), &archive).unwrap_err();
        let chain = format!("{error:#}");
        assert!(chain.contains("symbolic link"), "{chain}");
        assert!(!elsewhere.path().join("evil").exists());
    }

    #[test]
    fn install_rejects_entries_outside_the_whitelist() {
        let home = tempfile::tempdir().unwrap();
        let hostile = SkillsArchive {
            entries: vec![
                SkillsEntry {
                    path: "skills/ok".into(),
                    bytes: b"ok".to_vec(),
                },
                SkillsEntry {
                    path: "plugins/not-skills".into(),
                    bytes: b"evil".to_vec(),
                },
            ],
        };
        // Whitelist filtering keeps the foreign entry out of every synced
        // directory, so it is ignored rather than written.
        install_skills(HarnessKind::Kimi, home.path(), &hostile).unwrap();
        assert!(home.path().join("skills/ok").exists());
        assert!(!home.path().join("plugins").exists());
    }

    #[test]
    fn collect_then_install_reproduces_the_tree_byte_for_byte() {
        let canonical = tempfile::tempdir().unwrap();
        write(canonical.path(), "skills/review/SKILL.md", b"review");
        write(canonical.path(), "skills/review/nested/deep.md", b"deep");
        let session = tempfile::tempdir().unwrap();

        let archive = collect_skills(HarnessKind::Claude, canonical.path()).unwrap();
        let wire = archive.encode();
        let received = SkillsArchive::decode(&wire).unwrap();
        install_skills(HarnessKind::Claude, session.path(), &received).unwrap();

        let installed = collect_skills(HarnessKind::Claude, session.path()).unwrap();
        assert_eq!(archive.fingerprint(), installed.fingerprint());
        assert_eq!(archive, installed);
        for entry in installed.entries() {
            assert_eq!(
                std::fs::read(entry_path(session.path(), entry)).unwrap(),
                entry.bytes
            );
        }
    }
}
