//! The host CLI skill Mjolnir installs into localhost sessions.
//!
//! The text lives in `mj-core/assets/skills/<name>/SKILL.md` and is embedded so
//! a published crate carries it; the only thing that varies between harnesses
//! is the synced directory the entries are written under.

use super::SkillsEntry;
use crate::config::HarnessKind;

/// The managed skill and its supporting files, in path order.
const MANAGED: &[(&str, &str)] = &[
    ("SKILL.md", include_str!("../../assets/skills/mj/SKILL.md")),
    (
        "references/configuration.md",
        include_str!("../../assets/skills/mj/references/configuration.md"),
    ),
];

pub fn mj_skill_directory(kind: HarnessKind) -> String {
    let prefix = kind
        .synced_skill_dirs()
        .first()
        .expect("every harness syncs at least one skills directory");
    format!("{prefix}/mj")
}

/// The files Mjolnir installs into localhost session-owned profile homes.
///
/// Paths are home-relative, under the harness's first synced skills directory,
/// and sorted, so the result can be merged into a collected archive directly.
pub fn managed_skills(kind: HarnessKind) -> Vec<SkillsEntry> {
    let directory = mj_skill_directory(kind);
    MANAGED
        .iter()
        .map(|(path, body)| SkillsEntry {
            path: format!("{directory}/{path}"),
            bytes: body.as_bytes().to_vec(),
        })
        .collect()
}
