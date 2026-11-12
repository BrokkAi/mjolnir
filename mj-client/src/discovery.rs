//! Settings discovery from the machine owning configuration and credentials.
use crate::settings::RejectedRuntime;
use mj_core::config::Config;
use std::collections::BTreeMap;
/// Which section of the configuration a detection run fills in. Each scope
/// runs only the probes that section needs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Copy, PartialEq, Eq)]
pub enum DetectScope {
    Profiles,
    Runtimes,
}

/// What one detection run found: entries for the scope it was asked about,
/// and the runtimes it turned down. A runtime detection also reports what
/// each machine in the draft can run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SetupDetection {
    pub scope: DetectScope,
    #[serde(deserialize_with = "source_config")]
    pub config: Config,
    pub profile_readiness: BTreeMap<String, Option<String>>,
    pub rejected_runtimes: Vec<RejectedRuntime>,
    pub machines: Vec<crate::availability::MachineRuntimeProbe>,
}

fn source_config<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Config, D::Error> {
    mj_core::config::with_environment_sources_only(|| serde::Deserialize::deserialize(d))
}
