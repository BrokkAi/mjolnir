use super::*;

/// Where the current worker build is staged for control commands on a target:
/// `control/hel` in the target's worker store, beside the session roots like
/// every store entry (`controller::worker_binary::store_root`). The store is on
/// the target's own filesystem, inside the container for container targets.
pub fn control_worker_path(worker_root: &str) -> String {
    let parent = worker_root
        .trim_end_matches('/')
        .rsplit_once('/')
        .map_or(".", |(parent, _)| parent);
    format!("{parent}/worker-binaries/control/hel")
}

/// Exit status of a control script whose staged worker is missing. Nothing ran.
/// Callers stage it first (`controller::worker_binary::ensure_control_worker`),
/// so this reports a staging that was skipped or removed.
pub const WORKER_CONTROL_UNSTAGED_STATUS: i32 = 78;

/// Shell that runs `hel worker <subcommand> --root <root>` from the target's
/// staged current worker ([`control_worker_path`]), never from the root's own
/// `hel`. A root keeps the build that last started a worker there, which can
/// predate these subcommands; the lock they inspect (`<root>/worker.lock`, which
/// no shell can test portably) and the startup record are older than they are,
/// so the current build judges and stops an older worker correctly. A root
/// without a `hel` never had a worker started from it, so `when_no_binary`
/// answers for it.
fn installed_worker_script(worker_root: &str, subcommand: &str, when_no_binary: &str) -> String {
    format!(
        r#"hel_root={root}
hel_control={control}
if [ -e "$hel_root/hel" ]; then
    if [ ! -x "$hel_control" ]; then
        echo "the current Mjolnir worker is not staged at $hel_control" >&2
        exit {unstaged}
    fi
    "$hel_control" worker {subcommand} --root "$hel_root" || exit $?
else
    {when_no_binary}
fi
"#,
        root = posix_quote(worker_root),
        control = posix_quote(&control_worker_path(worker_root)),
        unstaged = WORKER_CONTROL_UNSTAGED_STATUS,
    )
}

/// A plan whose commands may run the staged control worker. Running it stages
/// that worker first (`controller::worker_binary::ensure_control_worker`), so no
/// caller has to remember to. [`Self::declare`] decides by looking for the
/// control path in the commands, so a builder cannot embed it without the plan
/// requiring staging.
#[derive(Debug, Clone)]
pub struct WorkerControlPlan {
    pub description: String,
    pub commands: Vec<CommandSpec>,
    control: Option<(TargetLocator, String)>,
}

impl WorkerControlPlan {
    pub fn declare(plan: CommandPlan, locator: &TargetLocator, session_id: &str) -> Result<Self> {
        let path = control_worker_path(&worker_root(locator, session_id)?);
        let quoted = posix_quote(&path);
        let embeds = plan.commands.iter().any(|command| {
            std::iter::once(&command.program)
                .chain(&command.args)
                .any(|part| part.contains(&path) || part.contains(&quoted))
        });
        Ok(Self {
            description: plan.description,
            commands: plan.commands,
            control: embeds.then(|| (locator.clone(), session_id.to_owned())),
        })
    }

    /// Whether running this plan stages the control worker first.
    pub fn stages_control_worker(&self) -> bool {
        self.control.is_some()
    }

    /// Stage, then run every command with `executor`.
    pub fn execute(&self, executor: &impl CommandExecutor) -> Result<Vec<CommandOutput>> {
        self.execute_staging_with(executor, executor)
    }

    /// Stage with `staging`, then run the commands with `executor`: a caller
    /// that bounds the commands with a timeout keeps staging outside it.
    pub fn execute_staging_with(
        &self,
        staging: &impl CommandExecutor,
        executor: &impl CommandExecutor,
    ) -> Result<Vec<CommandOutput>> {
        if let Some((locator, session_id)) = &self.control {
            crate::controller::ensure_control_worker(staging, locator, session_id)?;
        }
        CommandPlan {
            description: self.description.clone(),
            commands: self.commands.clone(),
        }
        .execute(executor)
    }
}

/// Print `absent` when the root never ran a worker (no `hel`), `present` when
/// the staged control worker is this build, and `missing` otherwise.
pub fn control_worker_state_script(worker_root: &str) -> String {
    format!(
        r#"hel_root={root}
hel_control={control}
if [ ! -e "$hel_root/hel" ]; then printf 'absent\n'
elif [ -x "$hel_control" ] && [ "$("$hel_control" worker build-id 2>/dev/null)" = {build} ]; then printf 'present\n'
else printf 'missing\n'
fi
"#,
        root = posix_quote(worker_root),
        control = posix_quote(&control_worker_path(worker_root)),
        build = posix_quote(mj_core::worker_build::WORKER_BUILD_ID),
    )
}

/// Report whether the exact session worker is alive without signaling it.
/// A successful probe prints one of `alive`, `starting` or `dead`; transport or
/// shell failures stay distinguishable from a confirmed absent worker.
pub fn worker_daemon_liveness_script(worker_root: &str) -> String {
    installed_worker_script(worker_root, "liveness", "printf 'dead\\n'")
}

/// The pid of the worker that holds the root's lock, printed alone, or nothing.
pub fn worker_daemon_holder_script(worker_root: &str) -> String {
    installed_worker_script(worker_root, "holder", ":")
}

/// Stop the detached worker daemon rooted at `worker_root`: TERM, then KILL
/// after two seconds. Use this when the session continues from the root, so
/// the worker and its harness can shut down in order. Returns once the root's
/// lock is free and nothing the worker started is left.
pub fn stop_worker_daemon_script(worker_root: &str) -> String {
    installed_worker_script(worker_root, "stop", ":")
}

/// Kill a retired worker daemon rooted at `worker_root`, and its process tree,
/// at once. Only for retirement: the caller removes the root next, and a sealed
/// checkpoint (or a discard) means nothing in it needs an orderly shutdown.
pub fn kill_worker_daemon_script(worker_root: &str) -> String {
    installed_worker_script(worker_root, "stop --kill", ":")
}

/// Stop a leaked worker and delete the durable relay state under its root.
///
/// A resume seeds fresh relay state into the same root a closed session used.
/// Leftover state wins over that seed at startup, so it has to go, and
/// whatever might still be writing it has to go first. Container and instance
/// targets are rebuilt from scratch on resume, so they need nothing here.
///
/// A staged home that is a link to a profile home, left for a session an
/// earlier release started there (`controller::local_profile_homes`), is
/// unlinked too. The restore that follows writes the native session into the
/// staged home while the worker files are installed beside it, and it must
/// land in a directory of the session's own, not in the profile home.
pub fn clear_relay_state_plan(
    locator: &TargetLocator,
    session_id: &str,
) -> Result<Option<WorkerControlPlan>> {
    verify_locator(locator, session_id)?;
    let session_worker_root = worker_root(locator, session_id)?;
    let staged_home = posix_quote(&format!("{session_worker_root}/profile"));
    let script = format!(
        "{}\nrm -rf -- {} {}\nif [ -L {staged_home} ]; then rm -f -- {staged_home}; fi\n",
        stop_worker_daemon_script(&session_worker_root),
        posix_quote(&format!(
            "{session_worker_root}/{}",
            mj_core::relay::RELAY_STATE_FILE
        )),
        posix_quote(&format!(
            "{session_worker_root}/{}",
            mj_core::relay::RELAY_JOURNAL_DIR
        )),
    );
    let command = match locator {
        TargetLocator::LocalBare { .. } => Some(
            CommandSpec::new("sh", ["-c", script.as_str()])
                .purpose("stop a leaked local Mjolnir worker and clear its relay state"),
        ),
        TargetLocator::SshBare { ssh, .. } => Some(
            ssh_command(ssh, ["sh", "-c", script.as_str()])
                .purpose("stop a leaked remote Mjolnir worker and clear its relay state"),
        ),
        TargetLocator::LocalPodman { .. }
        | TargetLocator::LocalDocker { .. }
        | TargetLocator::AppleContainer { .. }
        | TargetLocator::SshPodman { .. }
        | TargetLocator::SshDocker { .. }
        | TargetLocator::SshAppleContainer { .. }
        | TargetLocator::SshNitro { .. }
        | TargetLocator::AwsEc2 { .. } => None,
    };
    command
        .map(|command| {
            WorkerControlPlan::declare(
                CommandPlan {
                    description: "clear a leaked worker's relay state".into(),
                    commands: vec![command],
                },
                locator,
                session_id,
            )
        })
        .transpose()
}

/// Stop the session's worker with the staged control worker. The plan stages it
/// first when it runs.
pub fn stop_worker_plan(
    locator: &TargetLocator,
    session_id: &str,
    purpose: &str,
) -> Result<WorkerControlPlan> {
    let script = stop_worker_daemon_script(&worker_root(locator, session_id)?);
    WorkerControlPlan::declare(
        CommandPlan {
            description: purpose.to_owned(),
            commands: vec![command_on_locator(
                locator,
                session_id,
                vec!["sh".into(), "-c".into(), script],
                purpose,
            )?],
        },
        locator,
        session_id,
    )
}

/// Everything an in-place harness replacement must remove before the new
/// profile is staged: the running daemon, relay state, the installed worker
/// files, and the previous per-session profile root. Runs inside the target on
/// every locator, unlike [`clear_relay_state_plan`], because the environment
/// survives the swap whether or not it is a container.
///
/// The worker files are unlinked rather than overwritten: a mapped `hel` cannot
/// be overwritten while the daemon holds it, and a stale `ownership.json` must
/// not survive a crash and let a later reader believe the old profile is still
/// installed. The worker root itself is recreated, so the caller can install
/// straight afterwards.
pub fn in_place_worker_reset_plan(
    locator: &TargetLocator,
    session_id: &str,
    previous_profile_root: &str,
) -> Result<WorkerControlPlan> {
    worker_reset_plan(locator, session_id, Some(previous_profile_root))
}

/// Reset only the relay for a native conversation that stays in its home.
/// Stop its processes first; keep the executable, native files and caches.
pub fn retained_native_worker_reset_plan(
    locator: &TargetLocator,
    session_id: &str,
) -> Result<WorkerControlPlan> {
    worker_reset_plan(locator, session_id, None)
}

fn worker_reset_plan(
    locator: &TargetLocator,
    session_id: &str,
    previous_profile_root: Option<&str>,
) -> Result<WorkerControlPlan> {
    verify_locator(locator, session_id)?;
    let session_worker_root = worker_root(locator, session_id)?;
    // A root that is a link left for a session an earlier release started
    // (`controller::local_profile_homes`) is unlinked, never followed: the path
    // carries no trailing slash, so `rm` removes the link itself.
    let mut script = format!(
        "{}\nrm -rf -- {} {}\n",
        stop_worker_daemon_script(&session_worker_root),
        posix_quote(&format!(
            "{session_worker_root}/{}",
            mj_core::relay::RELAY_STATE_FILE
        )),
        posix_quote(&format!(
            "{session_worker_root}/{}",
            mj_core::relay::RELAY_JOURNAL_DIR
        )),
    );
    let mut metadata = vec!["launch.json", "ownership.json"];
    if previous_profile_root.is_some() {
        metadata.insert(0, "hel");
    } else {
        metadata.extend([
            mj_core::relay::NATIVE_SESSION_IDENTITY_FILE,
            mj_core::relay::RESTORED_RELAY_SEED_FILE,
        ]);
    }
    script.push_str(&format!(
        "rm -f -- {}\n",
        metadata
            .into_iter()
            .map(|name| posix_quote(&format!("{session_worker_root}/{name}")))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    if let Some(profile_root) = previous_profile_root {
        script.push_str(&format!(
            "rm -rf -- {}\nmkdir -p -- {}\n",
            posix_quote(profile_root.trim_end_matches('/')),
            posix_quote(&session_worker_root),
        ));
    }
    let command = locator_command(locator, vec!["sh".into(), "-c".into(), script])
        .purpose("reset the worker root for an in-place harness replacement")
        .stage(ProvisionStage::Syncing);
    WorkerControlPlan::declare(
        CommandPlan {
            description: "reset the worker root".into(),
            commands: vec![command],
        },
        locator,
        session_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_script(script: &str) -> (i32, String) {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .expect("run the probe script");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )
    }

    // Hard-won: c29b1379c: an in-place Move's relay reset made review rescan the checkout and refuse it.
    #[test]
    #[cfg(unix)]
    fn in_place_reset_preserves_the_review_untracked_record() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("0123456789abcdef");
        std::fs::create_dir_all(root.join("profile")).unwrap();
        let record = root.join(mj_core::relay::REVIEW_UNTRACKED_FILE);
        std::fs::write(&record, "original review record").unwrap();
        std::fs::write(root.join(mj_core::relay::RELAY_STATE_FILE), "{}").unwrap();
        let plan = in_place_worker_reset_plan(
            &TargetLocator::LocalBare {
                worker_root: root.to_string_lossy().into_owned(),
            },
            "0123456789abcdef",
            &root.join("profile").to_string_lossy(),
        )
        .unwrap();
        let (status, stdout) = run_script(&plan.commands[0].args[1]);
        assert_eq!(status, 0, "{stdout}");
        assert_eq!(
            std::fs::read_to_string(record).unwrap(),
            "original review record"
        );
        assert!(!root.join(mj_core::relay::RELAY_STATE_FILE).exists());
        assert!(!root.join("profile").exists());
    }
}
