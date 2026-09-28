//! Durable receiver fencing for randomly identified reviewer generations.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const FILE: &str = "reviewer-generations.json";
const MAX_GENERATIONS: usize = 4096;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Generations {
    pub(super) selected: Option<u64>,
    // False is admitted, true is retired. Identities are random nonces, not a
    // numerical sequence. Never evict a fence while a delayed request exists.
    identities: BTreeMap<u64, bool>,
}

impl Generations {
    pub(super) fn load(root: &Path) -> Result<Self> {
        match std::fs::read(root.join(FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("read reviewer generation fences"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Upgrade the one generation known before receiver fencing.
                let selected =
                    match std::fs::read_to_string(root.join(super::ROLE_GENERATION_MARKER)) {
                        Ok(identity) => Some(
                            identity
                                .split(':')
                                .next()
                                .unwrap_or("")
                                .parse()
                                .context("read legacy reviewer generation")?,
                        ),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => return Err(error).context("read legacy reviewer identity"),
                    };
                Ok(Self {
                    selected,
                    identities: selected
                        .map(|generation| (generation, false))
                        .into_iter()
                        .collect(),
                })
            }
            Err(error) => Err(error).context("read reviewer generation fences"),
        }
    }

    pub(super) fn ensure_active(&self, generation: u64) -> Result<()> {
        anyhow::ensure!(
            self.identities.get(&generation) != Some(&true),
            "reviewer generation {generation} is retired; its delayed work was not admitted"
        );
        Ok(())
    }

    fn reserve_identity(&self, generation: u64) -> Result<()> {
        anyhow::ensure!(
            self.identities.contains_key(&generation) || self.identities.len() < MAX_GENERATIONS,
            "reviewer generation fence capacity reached; new generations cannot be admitted"
        );
        Ok(())
    }

    pub(super) fn select(mut self, root: &Path, generation: u64) -> Result<()> {
        self.ensure_active(generation)?;
        self.reserve_identity(generation)?;
        if let Some(previous) = self.selected
            && previous != generation
        {
            self.identities.insert(previous, true);
        }
        self.identities.insert(generation, false);
        self.selected = Some(generation);
        self.save(root)
    }

    pub(super) fn retire(mut self, root: &Path, generation: u64) -> Result<()> {
        self.reserve_identity(generation)?;
        self.identities.insert(generation, true);
        self.save(root)
    }

    fn save(&self, root: &Path) -> Result<()> {
        mj_core::config::atomic_write(&root.join(FILE), &serde_json::to_vec(self)?)
            .context("persist reviewer generation fence")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retirement_before_start_survives_reopen_without_ordering_random_nonces() {
        let root = tempfile::tempdir().unwrap();
        Generations::load(root.path())
            .unwrap()
            .retire(root.path(), 900)
            .unwrap();
        assert!(
            Generations::load(root.path())
                .unwrap()
                .select(root.path(), 900)
                .is_err()
        );
        Generations::load(root.path())
            .unwrap()
            .select(root.path(), 7)
            .unwrap();
        Generations::load(root.path())
            .unwrap()
            .select(root.path(), 3)
            .unwrap();
        assert!(
            Generations::load(root.path())
                .unwrap()
                .select(root.path(), 7)
                .is_err()
        );
        let current = Generations::load(root.path()).unwrap();
        assert_eq!(current.selected, Some(3));
        current.select(root.path(), 3).unwrap();
    }
}
