//! One entry point for completing a path on the machine that owns it.
//!
//! Every path field in every surface asks the same question: given what the
//! user typed and the host the path belongs to, what could it become? The
//! host mapping, the `~` round trip, and the candidate limit live here so no
//! screen has to repeat them.

use std::path::Path;

use anyhow::{Context, Result};
use mj_core::path_completion::{
    CompletionHost, CompletionKind, MAX_CANDIDATES, PathCompletion, common_insert,
    local_completions, ssh_completions,
};

use super::Controller;
use super::cache_host::CacheHost;
use crate::targets::CommandExecutor;

impl Controller {
    /// Complete `prefix` on `host`. Call only from background work: it may run
    /// a command on a remote machine.
    pub fn complete_path(
        &self,
        host: &CompletionHost,
        prefix: &str,
        kind: CompletionKind,
        executor: &impl CommandExecutor,
    ) -> Result<PathCompletion> {
        if prefix.is_empty() {
            return Ok(PathCompletion::default());
        }
        let host = match host {
            CompletionHost::Local => CacheHost::Local,
            CompletionHost::Target(target_id) => {
                let target = self
                    .config
                    .targets
                    .get(target_id)
                    .with_context(|| format!("unknown target template {target_id:?}"))?;
                CacheHost::for_path_target(target)?
            }
            CompletionHost::Machine(machine) => CacheHost::for_path_machine(machine)?,
        };
        let home = if mj_core::path_input::needs_home(Path::new(prefix))? {
            Some(host.home(executor)?)
        } else {
            None
        };
        let expanded = mj_core::path_input::expand_home(Path::new(prefix), home.as_deref())?;
        let mut lookup = expanded.to_string_lossy().into_owned();
        // Completion is a text protocol: a trailing separator requests children.
        if prefix.ends_with('/') && !lookup.ends_with('/') {
            lookup.push('/');
        }
        let candidates = match &host {
            CacheHost::Local => local_completions(&lookup, kind),
            CacheHost::Ssh(ssh) => ssh_completions(ssh, &lookup, kind, executor)?,
        };
        let mut candidates = candidates
            .into_iter()
            .map(|candidate| fold_home(candidate, home.as_deref()))
            .collect::<Result<Vec<_>>>()?;
        // The shared prefix belongs to every match, not to the first
        // `MAX_CANDIDATES` the popup can show. Reading it from the shown ones
        // would push the field into a subset of a large directory: fifty
        // dotfiles at the head of `/tmp` would insert `.` and leave the other
        // sixteen thousand entries unreachable without deleting what the
        // completion itself typed.
        let insert = common_insert(prefix, &candidates);
        let truncated = candidates.len() > MAX_CANDIDATES;
        candidates.truncate(MAX_CANDIDATES);
        Ok(PathCompletion {
            candidates,
            insert,
            truncated,
        })
    }
}

/// Return a candidate in the shape the user typed: a path under a home that
/// was expanded for the host goes back to `~/...`.
fn fold_home(candidate: String, home: Option<&Path>) -> Result<String> {
    let Some(home) = home else {
        return Ok(candidate);
    };
    let suffix = Path::new(&candidate)
        .strip_prefix(home)
        .context("Completed path is outside the requested home")?;
    let mut value = Path::new("~").join(suffix).to_string_lossy().into_owned();
    if candidate.ends_with('/') && !value.ends_with('/') {
        value.push('/');
    }
    Ok(value)
}
