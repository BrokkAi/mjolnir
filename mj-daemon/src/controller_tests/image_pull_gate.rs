//! Tests of `mj_controller::image_pull_gate` whose checks reach into the daemon layer,
//! so they live in mj-daemon.

use anyhow::Result;
use mj_controller::image_pull_gate::*;
use mj_core::targets::{CommandExecutor, ImageHost, TargetTemplate};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// A Create issued while the daemon is downloading the image waits for
/// that download instead of starting a second one, and says so once.
// Hard-won: 0703826: EC2 warming bypassed the gate used by session launches.
#[test]
fn a_create_waits_for_the_in_flight_pull_of_its_image() {
    use mj_core::config::ImagePullPolicy;
    use mj_core::targets::{CommandOutput, CommandSpec, ContainerTemplate};
    use std::sync::mpsc;

    struct Download {
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        ready: Arc<AtomicBool>,
    }
    impl CommandExecutor for Download {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            if command.args.first().is_some_and(|arg| arg == "pull") {
                self.started.send(())?;
                self.release.recv()?;
                self.ready.store(true, Ordering::Release);
            }
            Ok(CommandOutput {
                status: 0,
                stdout: if self.ready.load(Ordering::Acquire) {
                    b"sha256:preloaded\n".to_vec()
                } else {
                    Vec::new()
                },
                stderr: Vec::new(),
            })
        }
    }
    struct Launch {
        release: Mutex<Option<mpsc::Sender<()>>>,
        waits: AtomicUsize,
    }
    impl CommandExecutor for Launch {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, _: &CommandSpec) -> Result<CommandOutput> {
            unreachable!("the launch must reach its image gate before running commands")
        }
        fn notify_notice(&self, notice: &str) {
            assert!(notice.contains("Waiting for image"));
            self.waits.fetch_add(1, Ordering::Release);
            if let Some(release) = self.release.lock().unwrap().take() {
                release.send(()).unwrap();
            }
        }
    }

    let image = "ghcr.io/example/preload-coordination:1";
    let (started, began) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let ready = Arc::new(AtomicBool::new(false));
    let executor = Download {
        started,
        release: released,
        ready: ready.clone(),
    };
    let refresh = mj_core::targets::image_refresh(
        ImageHost::LocalPodman,
        image,
        None,
        ImagePullPolicy::Missing,
    )
    .unwrap();
    let downloader = std::thread::spawn(move || {
        crate::pollers::refresh_host_image(&refresh, &executor, &|_| {})
    });
    began.recv().unwrap();
    let launch = Launch {
        release: Mutex::new(Some(release)),
        waits: AtomicUsize::new(0),
    };
    let target = TargetTemplate::LocalPodman(ContainerTemplate {
        image: image.into(),
        pull_policy: ImagePullPolicy::Missing,
        extra_run_args: Vec::new(),
        workspace_storage: Default::default(),
        build_cache: None,
    });
    let result = with_image_ready(&target, &launch, || Ok(ready.load(Ordering::Acquire)));
    // Release the fixture even if the gate regresses and lets work start
    // early, so a failed assertion never leaves a blocked downloader.
    if let Some(release) = launch.release.lock().unwrap().take() {
        release.send(()).unwrap();
    }
    downloader.join().unwrap().unwrap();
    assert!(
        result.unwrap(),
        "the launch ran before the preload finished"
    );
    assert_eq!(launch.waits.load(Ordering::Acquire), 1);
}
