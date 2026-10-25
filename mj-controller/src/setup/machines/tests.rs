use super::*;
use crate::targets::{CommandOutput, CommandSpec};
use mj_core::config::DEFAULT_WORKSPACE_PREFIX;
use mj_core::config::runtime_kind_name;

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
/// so what a probe found reaches the file only for a machine that still has
/// no runtime and is still the host that was probed, and only once however
/// many clients probe it.
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
    // gained a runtime from a different client.
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
        BTreeMap::from([(
            "builder".to_owned(),
            vec!["builder-2".to_owned(), "builder-docker".to_owned()]
        )])
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
        ["busy-podman"]
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

/// Answers each probe by the remote command it runs, so a machine probe's
/// many unrelated checks do not depend on their order.
struct RemoteHostExecutor {
    platform: &'static str,
    apple_container: Option<&'static str>,
    commands: std::cell::RefCell<Vec<CommandSpec>>,
}

impl CommandExecutor for RemoteHostExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.commands.borrow_mut().push(command.clone());
        let remote = command.args.last().cloned().unwrap_or_default();
        let output = |status, stdout: &str| CommandOutput {
            status,
            stdout: stdout.as_bytes().to_vec(),
            stderr: vec![],
        };
        Ok(if remote.contains("'uname' '-sm'") {
            output(0, self.platform)
        } else if remote.contains("sw_vers") {
            output(0, "26.1")
        } else if remote.contains("container' '--version") {
            output(0, self.apple_container.unwrap_or_default())
        } else if remote.contains("'container' 'system' 'status'") {
            output(i32::from(self.apple_container.is_none()), "")
        } else if remote == "'true'" {
            output(0, "")
        } else {
            output(1, "")
        })
    }
}

/// An SSH machine is offered the Apple container runtime exactly when it
/// reports Apple-silicon macOS; a Linux host is never asked for a `container`
/// command it cannot have.
#[test]
fn a_machine_offers_apple_container_only_on_apple_silicon_macos() {
    let machine = ssh_machine("mac.example.test");
    let mac = RemoteHostExecutor {
        platform: "Darwin arm64",
        apple_container: Some("container 1.2.3"),
        commands: Default::default(),
    };
    let probe = probe_machine("mac", &machine, &mac).unwrap();
    let apple = probe
        .usable()
        .find(|runtime| runtime.kind == RuntimeKind::AppleContainer)
        .expect("the Mac is offered Apple container");
    assert!(apple.detail.contains("1.2.3"), "{}", apple.detail);
    let (id, target) = probe
        .runtimes()
        .into_iter()
        .find(|(_, target)| matches!(target, TargetTemplate::SshAppleContainer { .. }))
        .expect("the runtime resolves to a target on that machine");
    assert_eq!(id, "mac-apple-container");
    assert_eq!(runtime_kind_name(&target), "apple-container");

    let linux = RemoteHostExecutor {
        platform: "Linux x86_64",
        apple_container: Some("container 1.2.3"),
        commands: Default::default(),
    };
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
    assert!(
        !linux.commands.borrow().iter().any(|command| command
            .args
            .last()
            .is_some_and(|arg| arg.contains("container"))),
        "a Linux host is never asked for a container command"
    );
}
