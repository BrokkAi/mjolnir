//! Harness skills whitelist and its deterministic wire archive.
//!
//! A profile's skills tree is user-managed content, not a rotating secret, so
//! unlike credentials it syncs in one direction only: the controller-side
//! canonical home is authoritative and every live session converges to it.
//! This module owns the whole interpretation — which directories each harness
//! syncs, how a tree becomes a fingerprinted archive, and how an archive is
//! installed — with no relay, async, or process dependencies, so every rule
//! is testable in isolation.

use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::config::HarnessKind;

/// Skills archives travel base64-encoded inside an 8 MiB relay frame. The cap
/// keeps the encoded payload, envelope, and a credential payload comfortably
/// inside one frame each way.
pub const MAX_SKILLS_ARCHIVE_BYTES: usize = 4 * 1024 * 1024;
/// A single skill file above this is almost certainly a checked-in binary,
/// not a skill.
pub const MAX_SKILLS_FILE_BYTES: u64 = 1024 * 1024;
pub const MAX_SKILLS_FILES: usize = 1024;

const ARCHIVE_MAGIC: &[u8; 8] = b"HELSKIL1";

/// The harness-home-relative directories Hel keeps in sync for a profile.
///
/// Every harness resolves user skills from a `skills/` directory under its
/// home (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `KIMI_CODE_HOME`, `GROK_HOME`),
/// matching the provisioning allowlist in `stage_profile`.
pub fn synced_skill_dirs(kind: HarnessKind) -> &'static [&'static str] {
    match kind {
        HarnessKind::Codex
        | HarnessKind::Claude
        | HarnessKind::Kimi
        | HarnessKind::Grok
        | HarnessKind::Deepseek
        | HarnessKind::Muse => &["skills"],
    }
}

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
        format!("{:x}", Sha256::digest(self.encode()))
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

/// Snapshot the synced skills trees of a controller-side profile home. A home
/// without any synced directory collects as an empty archive, which compares
/// equal to a session in the same state. Symlinks are skipped, matching the
/// provisioning allowlist copy.
pub fn collect_skills(kind: HarnessKind, home: &Path) -> Result<SkillsArchive> {
    let mut entries = Vec::new();
    for dir in synced_skill_dirs(kind) {
        let root = home.join(dir);
        if !root.exists() {
            continue;
        }
        collect_tree(&root, dir, &mut entries)
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

fn collect_tree(root: &Path, prefix: &str, entries: &mut Vec<SkillsEntry>) -> Result<()> {
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
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_tree(&path, &relative, entries)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        if metadata.len() > MAX_SKILLS_FILE_BYTES {
            bail!(
                "skills file {} is {} bytes, above the {MAX_SKILLS_FILE_BYTES} byte limit",
                path.display(),
                metadata.len()
            );
        }
        entries.push(SkillsEntry {
            path: relative,
            bytes: std::fs::read(&path)?,
        });
    }
    Ok(())
}

/// Replace a session home's synced skills trees with an archive's contents.
///
/// Each synced directory is built beside the destination and swapped in, so a
/// failure mid-install never leaves a half-written tree. A symlinked
/// destination is refused rather than followed, and entry paths outside the
/// harness's synced directories are rejected outright.
pub fn install_skills(kind: HarnessKind, home: &Path, archive: &SkillsArchive) -> Result<()> {
    for dir in synced_skill_dirs(kind) {
        let entries = archive
            .entries
            .iter()
            .filter(|entry| entry.path == *dir || entry.path.starts_with(&format!("{dir}/")))
            .collect::<Vec<_>>();
        install_tree(home, dir, &entries)
            .with_context(|| format!("install skills into {}", home.join(dir).display()))?;
    }
    Ok(())
}

fn install_tree(home: &Path, dir: &str, entries: &[&SkillsEntry]) -> Result<()> {
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
    if !entries.is_empty() {
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
    }
    if destination.exists() {
        std::fs::rename(&destination, &retired)?;
    }
    if entries.is_empty() {
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
    if let Err(error) = std::fs::rename(&incoming, &destination) {
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
            assert_eq!(synced_skill_dirs(kind), &["skills"]);
        }
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
