//! What a container host answers about its mbx, asked once.
//!
//! An SSH host used to answer each read-only question in its own round trip:
//! platform, mbx version, cache directory, where the configuration lives, and
//! the configuration itself. Here they are framed probes in one script
//! ([`targets::PROBE_HELPER_SH`]). The script only reads and orders the probes
//! the questions depend on. Every decision, including which answers an error
//! is, stays in Rust and reads the same answers a separate command would have
//! given. A local host runs no ssh, so it keeps asking each question itself.

use std::collections::BTreeMap;

use super::*;
use crate::cache_host::{PLATFORM_PURPOSE, parse_platform_output};

/// Which questions the caller will ask, so a probe nobody reads is not run.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Scope {
    /// Everything an inspection reads, platform and cache directory included.
    Inspection,
    /// The mbx and configuration a launching session stages.
    Staging,
}

/// Probes the host's answers, framed. `$1` is [`NATIVE_VERSION_SCRIPT`], `$2`
/// is [`configuration::READ_FRAMED_SCRIPT`], and `$3` is
/// [`configuration::DIRECTORY_SCRIPT`]; they travel as arguments so each stays
/// the one text its separate command runs.
fn script(scope: Scope) -> String {
    let platform = if scope == Scope::Inspection {
        r#"probe platform uname -sm || exit 0
case "$out" in
    Linux*) ;;
    Darwin*) probe native sh -c "$1" hel-mbx; exit 0 ;;
    *) exit 0 ;;
esac
"#
    } else {
        ""
    };
    let cache_directory = if scope == Scope::Inspection {
        "    probe cache_dir \"$program\" cache dir --json\n"
    } else {
        ""
    };
    format!(
        r#"{helper}nl='
'
{platform}if probe native sh -c "$1" hel-mbx; then
    program=${{out%%"$nl"*}}
{cache_directory}fi
if probe config_dir sh -c "$3" hel-mbx; then
    case "$out" in
        /*) probe config sh -c "$2" hel-mbx "$out/config.toml" ;;
    esac
fi
exit 0
"#,
        helper = targets::PROBE_HELPER_SH,
    )
}

/// A host's answers: batched when it is reached over SSH, asked one by one
/// when it is this machine.
pub(super) struct HostFacts<'a, E> {
    host: &'a CacheHost,
    executor: &'a E,
    batch: Option<BTreeMap<String, CommandOutput>>,
}

impl<'a, E: CommandExecutor> HostFacts<'a, E> {
    /// A host that answers each question with its own command.
    pub(super) fn direct(host: &'a CacheHost, executor: &'a E) -> Self {
        Self {
            host,
            executor,
            batch: None,
        }
    }

    /// Ask an SSH host everything `scope` reads in one round trip.
    pub(super) fn gather(host: &'a CacheHost, scope: Scope, executor: &'a E) -> Result<Self> {
        let mut facts = Self::direct(host, executor);
        if host.ssh().is_none() {
            return Ok(facts);
        }
        let purpose = match scope {
            Scope::Inspection => "discover the container host's build cache",
            Scope::Staging => "read the container host's mbx and its configuration",
        };
        let command = host.shell_command(
            &script(scope),
            "hel-mbx-discover",
            [
                NATIVE_VERSION_SCRIPT.to_owned(),
                configuration::READ_FRAMED_SCRIPT.to_owned(),
                configuration::DIRECTORY_SCRIPT.to_owned(),
            ],
            purpose,
        );
        let output = executor.execute(&command)?;
        let probes = targets::parse_framed_probes(&output.stdout, targets::DEFAULT_PROBE_FRAMES);
        // The script exits 0 whatever its probes did, so another status means
        // the shell or the connection failed before the probes could speak.
        if output.status != 0 || probes.is_empty() {
            let reported = String::from_utf8_lossy(&output.stderr);
            bail!(
                "{purpose} failed with status {}: {}",
                output.status,
                reported.trim()
            );
        }
        facts.batch = Some(probes);
        Ok(facts)
    }

    /// A probe's answer from the batch. A probe the script's own ordering
    /// skipped is one no caller asks for, so a missing one is a failure.
    fn answer(&self, batch: &BTreeMap<String, CommandOutput>, name: &str) -> Result<CommandOutput> {
        batch.get(name).cloned().with_context(|| {
            format!(
                "the host discovery for {} returned no `{name}` answer",
                self.host.key()
            )
        })
    }

    pub(super) fn platform(&self) -> Result<HostPlatform> {
        match &self.batch {
            Some(batch) => {
                parse_platform_output(&self.answer(batch, "platform").context(PLATFORM_PURPOSE)?)
            }
            None => self.host.platform(self.executor),
        }
    }

    pub(super) fn native(&self) -> Result<Option<NativeMbx>> {
        match &self.batch {
            Some(batch) => native_from_probe(&self.answer(batch, "native")?),
            None => probe_native_version(self.host, self.executor),
        }
    }

    pub(super) fn cache_directory(&self, native: &NativeMbx) -> Result<PathBuf> {
        match &self.batch {
            Some(batch) => cache_directory_from_probe(self.answer(batch, "cache_dir")?),
            None => native_cache_directory(self.host, native, self.executor),
        }
    }

    /// The host's `config.toml`, or `None` when it has none.
    pub(super) fn config(&self) -> Result<Option<String>> {
        match &self.batch {
            Some(batch) => {
                // The directory is read first, so an unusable one is reported
                // before the file it would have held.
                configuration::directory_from_probe(self.answer(batch, "config_dir")?)?;
                configuration::file_from_framed_probe(self.answer(batch, "config")?)
            }
            None => host_config_file(self.host, self.executor),
        }
    }
}
