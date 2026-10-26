use super::*;
use crate::targets::{CommandOutput, CommandSpec};
use mj_core::config::DEFAULT_WORKSPACE_PREFIX;

fn ssh_machine(host: &str) -> Machine {
    Machine::Ssh {
        ssh: SshConnection {
            host: host.to_owned(),
            user: Some("dev".to_owned()),
            identity_file: None,
            extra_args: vec![],
        },
        workspace_prefix: DEFAULT_WORKSPACE_PREFIX.into(),
        build_cache: None,
    }
}

fn found(kind: RuntimeKind, status: CheckStatus) -> RuntimeProbe {
    RuntimeProbe {
        kind,
        status,
        detail: String::new(),
        remediation: None,
    }
}

/// What a probe of `machine` found: a worker platform and Docker, no Podman.
fn answered(machine_id: &str, machine: &Machine) -> MachineProbe {
    MachineProbe {
        machine_id: machine_id.to_owned(),
        machine: machine.clone(),
        outcome: MachineProbeOutcome::Answered(vec![
            found(RuntimeKind::Bare, CheckStatus::Ready),
            found(RuntimeKind::Podman, CheckStatus::Fixable),
            found(RuntimeKind::Docker, CheckStatus::Ready),
        ]),
    }
}

/// The probe runs outside the configuration lock and the decision inside it,
/// so what a probe found reaches the file only for a machine that is still
/// the host that was probed, only for the kinds it does not run yet, and
/// only once however many clients probe it.
#[test]
fn probed_runtimes_are_added_once_and_only_to_the_machine_that_was_probed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let builder = ssh_machine("builder.example.test");
    let moved = ssh_machine("moved.example.test");
    let busy = ssh_machine("busy.example.test");
    Config::update_to(&path, |config| {
        config.machines.insert("builder".into(), builder.clone());
        config.machines.insert("moved".into(), moved.clone());
        config.machines.insert("busy".into(), busy.clone());
        // An existing runtime takes the id the bare runtime would like.
        config
            .targets
            .insert("builder".into(), TargetTemplate::LocalBare);
        Ok(())
    })
    .unwrap();
    let probes = [
        answered("builder", &builder),
        answered("moved", &moved),
        answered("busy", &busy),
    ];

    // While they were probed, one machine moved to another host and another
    // gained its Podman runtime from a different client, which it keeps.
    Config::update_to(&path, |config| {
        config
            .machines
            .insert("moved".into(), ssh_machine("elsewhere.example.test"));
        let (id, podman) = machine_runtime("busy", &busy, RuntimeKind::Podman).unwrap();
        config.targets.insert(id, podman);
        Ok(())
    })
    .unwrap();

    let added = add_machine_runtimes(&path, &probes, &|| false).unwrap();
    assert_eq!(
        added,
        BTreeMap::from([
            (
                "builder".to_owned(),
                vec!["builder-2".to_owned(), "builder-docker".to_owned()]
            ),
            (
                "busy".to_owned(),
                vec!["busy".to_owned(), "busy-docker".to_owned()]
            ),
        ])
    );
    let text = std::fs::read_to_string(&path).unwrap();
    for (id, kind) in [("builder-2", "bare"), ("builder-docker", "docker")] {
        let table = text
            .split(&format!("[targets.{id}]\n"))
            .nth(1)
            .unwrap_or_else(|| panic!("{id} is written:\n{text}"));
        let table = table.split("\n[").next().unwrap();
        assert!(table.contains(&format!("kind = \"{kind}\"")), "{table}");
        assert!(table.contains("machine = \"builder\""), "{table}");
    }
    let config = Config::load_from(&path).unwrap();
    assert_eq!(config.machine_runtimes("moved").count(), 0);
    assert_eq!(
        config
            .machine_runtimes("busy")
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        ["busy", "busy-docker", "busy-podman"]
    );

    // A second client's probe of the same machine adds nothing and leaves
    // the file as it is.
    assert!(
        add_machine_runtimes(&path, &probes, &|| false)
            .unwrap()
            .is_empty()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
}

/// A remote host played by this machine's own `sh`: the SSH command's remote
/// command line runs locally with stub programs first on `PATH`, so the real
/// batch script, its quoting and its framing are what is tested.
#[cfg(unix)]
struct ShellHost {
    stubs: tempfile::TempDir,
    ssh_calls: std::cell::Cell<usize>,
}

#[cfg(unix)]
impl ShellHost {
    fn new(stubs: &[(&str, &str)]) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        for (name, body) in stubs {
            let path = directory.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self {
            stubs: directory,
            ssh_calls: Default::default(),
        }
    }
}

#[cfg(unix)]
impl CommandExecutor for ShellHost {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        assert_eq!(command.program, "ssh");
        self.ssh_calls.set(self.ssh_calls.get() + 1);
        let remote = command.args.last().unwrap();
        let mut local = CommandSpec::new("/bin/sh", ["-c", remote.as_str()]);
        local.env.insert(
            "PATH".into(),
            format!("{}:/usr/bin:/bin", self.stubs.path().display()),
        );
        crate::targets::ProcessExecutor.execute(&local)
    }
}

/// A machine is probed with one SSH call whose answers every runtime check
/// reads, even when one probe's output is far larger than a pipe buffer: the
/// probes after it are still framed and judged.
#[cfg(unix)]
#[test]
fn one_ssh_call_answers_every_runtime_check_of_an_apple_silicon_mac() {
    let mac = ShellHost::new(&[
        ("uname", "echo 'Darwin arm64'"),
        // Podman that fails with 100 KiB of complaint.
        (
            "podman",
            "i=0; while [ $i -lt 2048 ]; do printf '%050d\\n' $i >&2; i=$((i+1)); done; exit 1",
        ),
        ("docker", "echo '27.3.1 linux'"),
        ("sw_vers", "echo 26.1"),
        (
            "container",
            "case \"$1\" in --version) echo 'container 1.2.3';; system) exit 0;; esac",
        ),
    ]);
    let probe = probe_machine("mac", &ssh_machine("mac.example.test"), &mac).unwrap();
    assert_eq!(mac.ssh_calls.get(), 1, "the machine is asked once");
    let status = |kind| {
        probe
            .usable()
            .chain(probe.rejected())
            .find(|runtime| runtime.kind == kind)
            .map(|runtime| (runtime.status, runtime.detail.clone()))
            .unwrap()
    };
    assert_eq!(status(RuntimeKind::Bare).0, CheckStatus::Ready);
    assert_eq!(status(RuntimeKind::Podman).0, CheckStatus::Fixable);
    let docker = status(RuntimeKind::Docker);
    assert_eq!(
        docker,
        (CheckStatus::Ready, "Docker 27.3.1 is available.".into())
    );
    let apple = status(RuntimeKind::AppleContainer);
    assert_eq!(apple.0, CheckStatus::Ready, "{}", apple.1);
    assert!(
        apple.1.contains("1.2.3") && apple.1.contains("26.1"),
        "{}",
        apple.1
    );
    let (id, target) = probe
        .runtimes()
        .into_iter()
        .find(|(_, target)| matches!(target, TargetTemplate::SshAppleContainer { .. }))
        .expect("the runtime resolves to a target on that machine");
    assert_eq!(id, "mac-apple-container");
    assert_eq!(
        mj_core::config::runtime_kind_name(&target),
        "apple-container"
    );
}

/// A Linux host is told why it cannot run Apple container, and is never
/// asked for a `container` command it cannot have.
#[cfg(unix)]
#[test]
fn a_linux_machine_is_never_asked_for_apple_container() {
    let asked = tempfile::tempdir().unwrap();
    let marker = asked.path().join("asked");
    let linux = ShellHost::new(&[
        ("uname", "echo 'Linux x86_64'"),
        ("container", &format!("touch '{}'", marker.display())),
        ("sw_vers", &format!("touch '{}'", marker.display())),
    ]);
    let probe = probe_machine("builder", &ssh_machine("builder.example.test"), &linux).unwrap();
    let rejected = probe
        .rejected()
        .find(|runtime| runtime.kind == RuntimeKind::AppleContainer)
        .expect("a Linux host is told why it cannot run Apple container");
    assert_eq!(rejected.status, CheckStatus::Unsupported);
    assert!(
        rejected.detail.contains("Apple-silicon"),
        "{}",
        rejected.detail
    );
    assert!(!marker.exists(), "a Linux host ran a container command");
}

/// When SSH itself fails, the one call's failure is classified as the
/// connectivity check always classified it.
#[test]
fn a_machine_whose_ssh_fails_is_unreachable_with_the_ssh_error() {
    struct Refused;
    impl CommandExecutor for Refused {
        fn execute(&self, _: &CommandSpec) -> Result<CommandOutput> {
            Ok(CommandOutput {
                status: 255,
                stdout: vec![],
                stderr: b"ssh: connect to host nas port 22: Connection refused\n".to_vec(),
            })
        }
    }
    let probe = probe_machine("nas", &ssh_machine("nas"), &Refused).unwrap();
    let MachineProbeOutcome::Unreachable { detail, .. } = probe.outcome else {
        panic!("{:?}", probe.outcome);
    };
    assert!(detail.contains("Connection refused"), "{detail}");
}
