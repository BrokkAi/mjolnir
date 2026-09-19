//! The skills Mjolnir itself authors and installs into every session it owns
//! a profile home for.
//!
//! The text lives in `mj-core/assets/skills/<name>/SKILL.md` and is embedded so
//! a published crate carries it; the only thing that varies between harnesses
//! is the synced directory the entries are written under.

use super::SkillsEntry;
use crate::config::HarnessKind;

/// One managed skill: the directory it occupies and its `SKILL.md`.
const MANAGED: &[(&str, &str)] = &[("mj", include_str!("../../assets/skills/mj/SKILL.md"))];

/// The skills Mjolnir installs into every session-owned profile home.
///
/// Paths are home-relative, under the harness's first synced skills directory,
/// and sorted, so the result can be merged into a collected archive directly.
pub fn managed_skills(kind: HarnessKind) -> Vec<SkillsEntry> {
    let prefix = kind
        .synced_skill_dirs()
        .first()
        .copied()
        .expect("every harness syncs at least one skills directory");
    MANAGED
        .iter()
        .map(|(name, body)| SkillsEntry {
            path: format!("{prefix}/{name}/SKILL.md"),
            bytes: body.as_bytes().to_vec(),
        })
        .collect()
}
