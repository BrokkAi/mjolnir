use super::*;
use mj_core::targets::SshTrafficClass;

/// Where the staged control worker lives: one stable name in the session's
/// own worker root, a hard link to a worker-store entry. It exists only when
/// the root's own `hel` cannot serve a control subcommand, and it is staged
/// only on demand (`controller::worker_binary::run_control_command`). Any
/// build that passes the control scripts' `--help` probe serves, so the name
/// does not encode the daemon's build and daemons of every build share the
/// file. The root is its one owner: the link keeps the store entry alive under
/// the collector's ordinary link-count rule and goes with the root; installing
/// a `hel` that carries the interface removes it.
pub fn control_worker_path(worker_root: &str) -> String {
    mj_core::target_path::text(&mj_core::target_path::join(
        std::path::Path::new(worker_root),
        std::path::Path::new(CONTROL_WORKER_NAME),
    ))
}

/// File name of the staged control worker. Also the prefix the control scripts
/// glob (`hel-control*`), which keeps files staged under the hashed names of
/// earlier releases usable.
pub const CONTROL_WORKER_NAME: &str = "hel-control";

/// Exit status of a control script when nothing in the root serves its
/// subcommand. Nothing ran. The caller stages the control worker and reruns the
/// command once (`controller::worker_binary::run_control_command`); a
/// read-only diagnosis reports it as a finding.
pub const WORKER_CONTROL_UNSTAGED_STATUS: i32 = 78;

/// Shell that runs `hel worker <subcommand> <args> --root <root>` from the
/// first binary in the root that proves it serves the subcommand: the root's
/// own `hel`, then any `hel-control*`. A binary proves it by exiting 0 for
/// `worker <subcommand> --help`; every other result (status 2 from a build
/// that predates the subcommand, 126 or 127, a crash, a wrong-architecture
/// file) counts as not proven. When none proves it the script exits
/// [`WORKER_CONTROL_UNSTAGED_STATUS`] before doing anything. The decision is
/// made on the target against the file about to run, so the daemon keeps no
/// record of which build can do what. A root without a `hel` never had a
/// worker started from it, so `when_no_binary` answers for it. The contract the
/// subcommands keep is in `mj-worker/src/worker_stop.rs`.
fn installed_worker_script(
    worker_root: &str,
    subcommand: &str,
    args: &str,
    when_no_binary: &str,
) -> ControlScript {
    ControlScript::new(
        worker_root,
        format!(
            r#"hel_root={root}
if [ -e "$hel_root/hel" ]; then
    hel_bin=
    for hel_candidate in "$hel_root/hel" "$hel_root"/{control_name}*; do
        if [ -f "$hel_candidate" ] && [ -x "$hel_candidate" ] \
            && "$hel_candidate" worker {subcommand} --help >/dev/null 2>&1; then
            hel_bin=$hel_candidate
            break
        fi
    done
    if [ -z "$hel_bin" ]; then
        echo "no worker in $hel_root serves 'worker {subcommand}'" >&2
        exit {unstaged}
    fi
    "$hel_bin" worker {subcommand}{args} --root "$hel_root" || exit $?
else
    {when_no_binary}
fi
"#,
            root = posix_quote(worker_root),
            control_name = CONTROL_WORKER_NAME,
            unstaged = WORKER_CONTROL_UNSTAGED_STATUS,
        ),
    )
}

/// Installs the control worker into a worker root. The code that owns worker
/// binaries lives above this module and implements it; a plan's `execute`
/// takes one, so no caller can run a control command without the means to
/// stage what it needs.
pub trait ControlWorkerStager {
    fn stage_control_worker(
        &self,
        caller: &impl CommandExecutor,
        locator: &TargetLocator,
        session_id: &str,
        root: &str,
    ) -> Result<()>;
}

/// Run one control command, staging the control worker and rerunning it once
/// when the script reports (status 78) that the root has nothing to serve it.
/// `controller::worker_binary::run_control_command` documents the protocol.
pub fn run_staged_control_command(
    caller: &impl CommandExecutor,
    stager: &impl ControlWorkerStager,
    locator: &TargetLocator,
    session_id: &str,
    command: &CommandSpec,
) -> Result<CommandOutput> {
    let first = caller.execute(command)?;
    if first.status != WORKER_CONTROL_UNSTAGED_STATUS {
        return Ok(first);
    }
    let root = worker_root(locator, session_id)?;
    // Publishing a build waits behind other uploads of it and can take
    // minutes. A resumable operation does not hold daemon admission for that:
    // a handoff cancels the wait and the next daemon starts it again.
    let yielded = caller.yield_admission_if_resumable();
    let staged = stager.stage_control_worker(caller, locator, session_id, &root);
    if yielded && !caller.cancellation_requested() {
        caller.end_resumable_work("control worker install")?;
    }
    staged?;
    let second = caller.execute(command)?;
    ensure!(
        second.status != WORKER_CONTROL_UNSTAGED_STATUS,
        "the control worker staged in {root} does not serve {}: {}",
        command.purpose,
        String::from_utf8_lossy(&second.stderr).trim()
    );
    Ok(second)
}

/// A shell script that carries the root whose control worker it invokes.
/// Composition and transport wrapping preserve the requirement without parsing shell text.
#[derive(Debug, Clone)]
pub struct ControlScript {
    root: Option<PathBuf>,
    script: String,
}

impl ControlScript {
    fn new(root: &str, script: String) -> Self {
        Self {
            root: Some(PathBuf::from(root)),
            script,
        }
    }

    pub fn without_control(script: String) -> Self {
        Self { root: None, script }
    }

    pub fn map_script(mut self, compose: impl FnOnce(String) -> String) -> Self {
        self.script = compose(self.script);
        self
    }

    pub fn into_command(self, wrap: impl FnOnce(String) -> CommandSpec) -> ControlCommand {
        ControlCommand {
            root: self.root,
            command: wrap(self.script),
        }
    }

    /// Explicit escape for a caller that handles status 78 itself, and for read-only diagnoses.
    pub fn unstaged_script(&self) -> &str {
        &self.script
    }
}

/// A command whose staging requirement survives SSH and container wrapping.
#[derive(Debug, Clone)]
pub struct ControlCommand {
    root: Option<PathBuf>,
    command: CommandSpec,
}

impl ControlCommand {
    /// Only for a caller that handles status 78 itself or reports it as a diagnosis.
    pub fn unstaged_command(self) -> CommandSpec {
        self.command
    }
}

impl From<CommandSpec> for ControlCommand {
    fn from(command: CommandSpec) -> Self {
        Self {
            root: None,
            command,
        }
    }
}

/// Commands are private: execution stages the requirement carried by their builders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerControlPlan {
    description: String,
    commands: Vec<CommandSpec>,
    control: Option<(TargetLocator, String, PathBuf)>,
}

impl WorkerControlPlan {
    pub fn new(
        description: impl Into<String>,
        commands: Vec<ControlCommand>,
        locator: &TargetLocator,
        session_id: &str,
    ) -> Result<Self> {
        let mut root = None;
        for command in &commands {
            if let Some(required) = &command.root {
                ensure!(
                    root.as_ref().is_none_or(|root| root == required),
                    "a control plan cannot stage different worker roots"
                );
                root = Some(required.clone());
            }
        }
        Ok(Self {
            description: description.into(),
            commands: commands
                .into_iter()
                .map(|command| command.command)
                .collect(),
            control: root.map(|root| (locator.clone(), session_id.to_owned(), root)),
        })
    }

    /// A plan whose commands never run the control worker.
    #[doc(hidden)]
    pub fn without_control(description: &str, commands: Vec<CommandSpec>) -> Self {
        Self {
            description: description.to_owned(),
            commands,
            control: None,
        }
    }

    /// Whether running this plan stages the control worker first.
    pub fn stages_control_worker(&self) -> bool {
        self.control.is_some()
    }

    /// Run every command with `executor`. A command whose script finds nothing
    /// in the root to serve its subcommand (status 78) has the control worker
    /// staged and is rerun once ([`run_staged_control_command`]).
    pub fn execute(
        &self,
        executor: &impl CommandExecutor,
        stager: &impl ControlWorkerStager,
    ) -> Result<Vec<CommandOutput>> {
        let mut outputs = Vec::with_capacity(self.commands.len());
        for command in &self.commands {
            let output = match &self.control {
                Some((locator, session_id, root)) => {
                    ensure!(
                        Path::new(&worker_root(locator, session_id)?) == root,
                        "control script root differs from the staging target"
                    );
                    run_staged_control_command(executor, stager, locator, session_id, command)?
                }
                None => executor.execute(command)?,
            };
            outputs.push(mj_core::targets::checked_command_output(command, output)?);
        }
        Ok(outputs)
    }

    /// The commands of a plan that does not run the control worker, for a
    /// caller that composes them itself. A plan that does run it is refused:
    /// running its scripts would skip staging.
    pub fn commands_without_staging(&self) -> Result<&[CommandSpec]> {
        ensure!(
            self.control.is_none(),
            "{} runs the staged control worker, so it must run through `execute`",
            self.description
        );
        Ok(&self.commands)
    }

    /// The scripts of any plan, for a test that inspects them. They skip staging.
    #[doc(hidden)]
    pub fn scripts(&self) -> &[CommandSpec] {
        &self.commands
    }
}

#[doc(hidden)]
impl From<CommandSpec> for WorkerControlPlan {
    fn from(command: CommandSpec) -> Self {
        Self::without_control(&command.purpose.clone(), vec![command])
    }
}

#[doc(hidden)]
impl From<CommandPlan> for WorkerControlPlan {
    fn from(plan: CommandPlan) -> Self {
        Self::without_control(&plan.description, plan.commands)
    }
}

/// Report whether the exact session worker is alive without signaling it.
/// A successful probe prints one of `alive`, `starting` or `dead`; transport or
/// shell failures stay distinguishable from a confirmed absent worker.
pub fn worker_daemon_liveness_script(worker_root: &str) -> ControlScript {
    installed_worker_script(worker_root, "liveness", "", "printf 'dead\\n'")
}

/// The pid of the worker that holds the root's lock, printed alone, or nothing.
pub fn worker_daemon_holder_script(worker_root: &str) -> ControlScript {
    installed_worker_script(worker_root, "holder", "", ":")
}

/// Stop the detached worker daemon rooted at `worker_root`: TERM, then KILL
/// after two seconds. Use this when the session continues from the root, so
/// the worker and its harness can shut down in order. Returns once the root's
/// lock is free and nothing the worker started is left.
pub fn stop_worker_daemon_script(worker_root: &str) -> ControlScript {
    installed_worker_script(worker_root, "stop", "", ":")
}

/// Kill a retired worker daemon rooted at `worker_root`, and its process tree,
/// at once. Only for retirement: the caller removes the root next, and a sealed
/// checkpoint (or a discard) means nothing in it needs an orderly shutdown.
pub fn kill_worker_daemon_script(worker_root: &str) -> ControlScript {
    installed_worker_script(worker_root, "stop", " --kill", ":")
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
    let script = stop_worker_daemon_script(&session_worker_root).map_script(|stop| {
        format!(
            "{stop}\nrm -rf -- {} {}\nif [ -L {staged_home} ]; then rm -f -- {staged_home}; fi\n",
            posix_quote(&format!(
                "{session_worker_root}/{}",
                mj_core::relay::RELAY_STATE_FILE
            )),
            posix_quote(&format!(
                "{session_worker_root}/{}",
                mj_core::relay::RELAY_JOURNAL_DIR
            )),
        )
    });
    let command = match locator {
        TargetLocator::LocalBare { .. } => Some(script.into_command(|script| {
            CommandSpec::new("sh", ["-c", script.as_str()])
                .purpose("stop a leaked local Mjolnir worker and clear its relay state")
        })),
        TargetLocator::SshBare { ssh, .. } | TargetLocator::SshHostBare { ssh, .. } => {
            Some(script.into_command(|script| {
                ssh_command(SshTrafficClass::Control, ssh, ["sh", "-c", script.as_str()])
                    .purpose("stop a leaked remote Mjolnir worker and clear its relay state")
            }))
        }
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
            WorkerControlPlan::new(
                "clear a leaked worker's relay state",
                vec![command],
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
    WorkerControlPlan::new(
        purpose,
        vec![script.into_command(|script| {
            locator_command(
                SshTrafficClass::Control,
                locator,
                vec!["sh".into(), "-c".into(), script],
            )
            .purpose(purpose)
        })],
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
    let script = stop_worker_daemon_script(&session_worker_root).map_script(|stop| {
        let mut script = format!(
            "{stop}\nrm -rf -- {} {}\n",
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
        script
    });
    let command = script.into_command(|script| {
        locator_command(
            SshTrafficClass::Control,
            locator,
            vec!["sh".into(), "-c".into(), script],
        )
        .purpose("reset the worker root for an in-place harness replacement")
        .stage(ProvisionStage::Syncing)
    });
    WorkerControlPlan::new("reset the worker root", vec![command], locator, session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hard-won: #1550: an SSH root with an apostrophe hid the control path from the text scan, so staging was skipped
    #[test]
    fn ssh_control_plans_preserve_quoted_root_staging() {
        const SESSION: &str = "0123456789abcdef";
        let locator = TargetLocator::SshBare {
            worker_id: None,
            ssh: SshTarget {
                destination: "example.invalid".into(),
                ssh_args: vec![],
            },
            workspace: format!(".local/share/hel/workspaces/{SESSION}"),
        };
        // SSH locators currently derive a restricted root from the session ID.
        // Exercise arbitrary root text at the script/transport boundary instead.
        let root = Path::new(".local/share/hel/worker's space").join(SESSION);
        let root_text = root.to_str().unwrap();
        let close = kill_worker_daemon_script(root_text)
            .map_script(|kill| format!("{kill}\nrm -rf -- {}\n", posix_quote(root_text)));
        for script in [
            stop_worker_daemon_script(root_text),
            close,
            worker_daemon_liveness_script(root_text),
        ] {
            let TargetLocator::SshBare { ssh, .. } = &locator else {
                unreachable!()
            };
            let command = script.into_command(|script| {
                ssh_command_owned(
                    SshTrafficClass::Control,
                    ssh,
                    vec!["sh".into(), "-c".into(), script],
                )
            });
            let plan =
                WorkerControlPlan::new("quoted SSH control", vec![command], &locator, SESSION)
                    .unwrap();
            assert_eq!(plan.commands[0].program, "ssh");
            assert!(plan.stages_control_worker());
            assert_eq!(plan.control.as_ref().map(|(_, _, root)| root), Some(&root));
            assert!(plan.commands_without_staging().is_err());
        }
        // Actual locator plans also retain the builder's requirement.
        let expected = PathBuf::from(worker_root(&locator, SESSION).unwrap());
        for plan in [
            stop_worker_plan(&locator, SESSION, "stop").unwrap(),
            close_plan(&locator, SESSION).unwrap(),
            WorkerControlPlan::new(
                "liveness",
                vec![
                    worker_daemon_liveness_script(expected.to_str().unwrap()).into_command(
                        |script| {
                            locator_command(
                                SshTrafficClass::Control,
                                &locator,
                                vec!["sh".into(), "-c".into(), script],
                            )
                        },
                    ),
                ],
                &locator,
                SESSION,
            )
            .unwrap(),
        ] {
            assert_eq!(
                plan.control.as_ref().map(|(_, _, root)| root),
                Some(&expected)
            );
        }
    }

    // Only the Unix case below runs a probe script.
    #[cfg(unix)]
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
